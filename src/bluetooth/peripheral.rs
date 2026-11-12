//! One pending GATT subscriber per endpoint. Carrier ownership does not grant application admission.
use super::{
    Apartment, Error,
    carrier::{self, LinkIo, Pipe, Writer},
    central::{NOTIFY, SERVICE, WRITE, buffer, connection_params, connection_phy, read_buffer},
    probe, wait,
};
use crate::discovery::Announcement;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Devices::Bluetooth::{BluetoothError, BluetoothLEDevice, GenericAttributeProfile::*},
    Foundation::TypedEventHandler,
    core::{GUID, HSTRING, IInspectable},
};

#[derive(Default)]
struct Admission {
    owner: Option<HSTRING>,
    /// Values written before any phone was admitted, in arrival order, with
    /// the writer's session: Windows can raise a phone's first write before
    /// its subscription (2026-10-28 S24 call: the same millisecond, the
    /// write first). They go to the pipe if that phone is admitted, else
    /// they are dropped. Bounded by `HELD_MAX_VALUES` / `HELD_MAX_BYTES`.
    held: Vec<(HSTRING, Vec<u8>, Instant)>,
}
const HELD_MAX_VALUES: usize = 64;
const HELD_MAX_BYTES: usize = 32 * 1024;
/// Only values written this recently before the admission count: the race is
/// milliseconds, while a phone's late write after the last call ended (the
/// 2026-10-28 00:37:56 first_write) must never reach the next call's pipe.
const HELD_MAX_AGE: Duration = Duration::from_millis(1_500);
/// What happens to one written value.
#[derive(Debug, PartialEq, Eq)]
enum Written {
    /// The admitted phone's: into the pipe now.
    Deliver,
    /// No phone admitted yet: kept until one is.
    Held,
    /// Another phone's, or too much before admission.
    Reject,
}
struct RequestSlot(Arc<AtomicUsize>);
impl RequestSlot {
    fn reserve(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |value| {
                (value < 4096).then_some(value + 1)
            })
            .ok()?;
        Some(Self(count.clone()))
    }
}
impl Drop for RequestSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Admission {
    fn claim(&mut self, peer: &HSTRING) -> bool {
        if let Some(owner) = &self.owner {
            return owner == peer;
        }
        self.owner = Some(peer.clone());
        true
    }
    fn owns(&self, peer: &HSTRING) -> bool {
        self.owner.as_ref() == Some(peer)
    }
    /// A write may go on to be read (no one admitted yet, or its owner).
    fn may_write(&self, peer: &HSTRING) -> bool {
        self.owner.as_ref().is_none_or(|owner| owner == peer)
    }
    /// Decides one value from `peer` (keeping it when no one is admitted yet).
    fn on_value(&mut self, peer: &HSTRING, value: &[u8], now: Instant) -> Written {
        match &self.owner {
            Some(owner) if owner == peer => Written::Deliver,
            Some(_) => Written::Reject,
            None => {
                self.held.retain(|(_, _, at)| now.saturating_duration_since(*at) <= HELD_MAX_AGE);
                let bytes: usize = self.held.iter().map(|(_, v, _)| v.len()).sum();
                if self.held.len() >= HELD_MAX_VALUES || bytes + value.len() > HELD_MAX_BYTES {
                    return Written::Reject;
                }
                self.held.push((peer.clone(), value.to_vec(), now));
                Written::Held
            }
        }
    }
    /// `claim`, returning the admitted phone's held values in order (the
    /// rest are dropped).
    fn admit(&mut self, peer: &HSTRING, now: Instant) -> Option<Vec<Vec<u8>>> {
        if !self.claim(peer) {
            return None;
        }
        let held = std::mem::take(&mut self.held);
        Some(
            held.into_iter()
                .filter(|(writer, _, at)| writer == peer && now.saturating_duration_since(*at) <= HELD_MAX_AGE)
                .map(|(_, v, _)| v)
                .collect(),
        )
    }
}
struct State {
    admission: Admission,
    client: Option<GattSubscribedClient>,
    session: Option<(GattSession, i64)>,
    pipe: Pipe,
}
struct PeripheralIo {
    notify: GattLocalCharacteristic,
    state: Arc<Mutex<State>>,
    cancelled: Arc<AtomicBool>,
    native_error: Option<Error>,
}
/// How this PC announces itself to phones (Design QW).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Experimental, never shown to reach a phone: the compact call UUID as
    /// the service UUID of an empty provider (Design QW option A).
    CompactAlias([u8; 16]),
    /// Design QW A2 v1 (gates 1 and 2 passed, 2026-10-20): the fixed
    /// announcement UUID carrying the 9-byte call offer as service data,
    /// connectable (Windows aborts a non-connectable one) and extended
    /// (LE 1M secondary PHY; a legacy advert drops the service data).
    PcAnnouncement([u8; crate::pc_announcement::LEN]),
    /// Test hooks only (Design QV gates W1/W2, `MESHLIVE_TEST_PC_ANNOUNCE`):
    /// a kind 1 or kind 2 PC announcement (23 bytes) on its own provider,
    /// next to the call offer, as the same extended connectable form.
    PcTest([u8; crate::pc_announcement::STREAM_LEN]),
    /// Design QV F1: the only announcing provider while watching or live.
    PcStream([u8; crate::pc_announcement::STREAM_LEN]),
    /// Design SR in-call link renewal: a connectable legacy set carrying only
    /// the per-attempt derived 128-bit UUID (no service data, nothing readable).
    Renewal([u8; 16]),
}

/// One announcement provider; dropping it stops it and removes its handlers.
struct Announcer {
    provider: GattServiceProvider,
    status_token: i64,
    write: Option<(GattLocalCharacteristic, i64)>,
    notify: Option<(GattLocalCharacteristic, i64)>,
}
impl Drop for Announcer {
    fn drop(&mut self) {
        let _ = self.provider.StopAdvertising();
        let _ = self.provider.RemoveAdvertisementStatusChanged(self.status_token);
        if let Some((write, token)) = &self.write {
            let _ = write.RemoveWriteRequested(*token);
        }
        if let Some((notify, token)) = &self.notify {
            let _ = notify.RemoveSubscribedClientsChanged(*token);
        }
    }
}

pub struct Peripheral {
    writer: Writer,
    io: PeripheralIo,
    data: GattServiceProvider,
    form: Form,
    /// The current announcement provider (replaced at every rotating-ID change).
    offer: Arc<Mutex<Option<Announcer>>>,
    /// The compact call-offer alias of the same offer (`advertise::pc_compact_alias`,
    /// Experimental MESHLIVE_COMPACT_OFFER): a second provider whose UUID is
    /// the compact v2 call offer, so phones without extended scanning see this
    /// PC in a legacy UUID-list advert. Started and stopped with `offer`.
    alias: Arc<Mutex<Option<Announcer>>>,
    /// The alias of the last announcement start: (UUID, settled status or error).
    alias_last: Option<([u8; 16], Result<i32, Error>)>,
    /// Test hooks only: the kind 1/2 provider (`Form::PcTest`) and its status events.
    test_extra: Arc<Mutex<Option<Announcer>>>,
    test_events: Arc<Mutex<Vec<(i32, i32)>>>,
    /// (status, Bluetooth error) of every announcement status event, for the owner's log.
    status_events: Arc<Mutex<Vec<(i32, i32)>>>,
    /// What connecting phones did on this PC's GATT services (WinRT exposes
    /// no service-discovery event: subscriptions and writes per
    /// characteristic are the evidence of what a phone found).
    gatt_events: Arc<Mutex<Vec<String>>>,
    write: GattLocalCharacteristic,
    write_token: i64,
    subscribe_token: i64,
    started: bool,
    /// The data service's start outlived its wait: its thread stops it once
    /// Windows returns; this owner never calls it again.
    data_stuck: bool,
    /// The subscribed phone's device, opened once for the link-state trace.
    peer_device: Option<BluetoothLEDevice>,
    /// Last observed (data service, offer alias) advertisement status, for diagnostics.
    last_status: (i32, i32),
    _apartment: Apartment,
}

/// The WinRT step the advertising start on this PC is in now (one
/// announcement per PC), so a start that never returns can be named by the
/// thread that gave up waiting for it (2026-11-06: `peripheral.start()`
/// never returned on an Intel AX USB adapter).
static STEP: Mutex<&'static str> = Mutex::new("");
fn step(name: &'static str) {
    *STEP.lock().unwrap_or_else(|e| e.into_inner()) = name;
    if std::env::var_os("MESHLIVE_TRACE").is_some() {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        eprintln!("t={ms} bluetooth phase=peripheral_step {name}");
    }
}
/// The last step `start` entered (empty before any).
pub fn current_step() -> &'static str {
    *STEP.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long one `StartAdvertising` call may take. The call is synchronous and
/// normally returns in milliseconds (Windows reports the outcome later through
/// `AdvertisementStatus`, see `settle`). Measured 2026-11-06 on this project's
/// Intel AX USB laptop (Windows build 26200, 19 days since boot): every
/// `GattServiceProvider.StartAdvertising` on the PC never returned, for any
/// service UUID and any parameters, in MeshLive and in a separate PowerShell
/// process alike, so the radio worker waited forever.
pub const START_CALL_LIMIT: Duration = Duration::from_secs(5);

/// Starts still inside Windows after their wait ended.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// True while a start that outlived its wait is still inside Windows. A new
/// start then fails at once (no second stuck thread); the owner tries again
/// once this is false (Windows answered, e.g. after Bluetooth off and on).
pub fn start_pending() -> bool {
    PENDING.load(Ordering::Acquire) > 0
}

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const ABANDONED: u8 = 2;

/// Runs `call` on its own thread (its own MTA apartment) and waits at most
/// `limit`. When the wait ends first: `Error::NotResponding`, and the call is
/// left on its thread, which runs `late` if and when Windows returns (to stop
/// what that start put on air). The caller never touches that provider again.
fn bounded<T: Send + 'static>(
    limit: Duration,
    call: impl FnOnce() -> Result<T, Error> + Send + 'static,
    late: impl FnOnce() + Send + 'static,
) -> Result<T, Error> {
    if start_pending() {
        return Err(Error::NotResponding);
    }
    let state = Arc::new(std::sync::atomic::AtomicU8::new(RUNNING));
    let shared = state.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("meshlive-adv-start".into()).spawn(move || {
        let apartment = Apartment::new().ok();
        let result = call();
        if shared.compare_exchange(RUNNING, DONE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = sender.send(result);
            drop(late);
        } else {
            drop(result);
            late();
            PENDING.fetch_sub(1, Ordering::AcqRel);
        }
        drop(apartment);
    });
    if spawned.is_err() {
        return Err(Error::Native(0x8007000Eu32 as i32));
    }
    match receiver.recv_timeout(limit) {
        Ok(result) => result,
        Err(_) => {
            PENDING.fetch_add(1, Ordering::AcqRel);
            if state.compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Err(Error::NotResponding);
            }
            // It returned just now: its result is on the way.
            PENDING.fetch_sub(1, Ordering::AcqRel);
            receiver.recv().unwrap_or(Err(Error::NotResponding))
        }
    }
}

/// What a start that outlived its wait leaves to its thread.
#[derive(Default)]
struct Leftover {
    returned: bool,
    announcer: Option<Announcer>,
}

/// `StartAdvertising` of the data service, bounded. On `NotResponding` the
/// provider is stopped by the thread still inside Windows once it returns.
fn start_data(provider: &GattServiceProvider, params: GattServiceProviderAdvertisingParameters) -> Result<(), Error> {
    let (call, stop) = (provider.clone(), provider.clone());
    bounded(START_CALL_LIMIT, move || call.StartAdvertisingWithParameters(&params).map_err(Error::from), move || {
        let _ = stop.StopAdvertising();
    })
}

/// `StartAdvertising` of the announcer in `slot`, bounded. On `NotResponding`
/// the announcer leaves `slot` for the thread still inside Windows, which
/// stops and drops it (handlers removed) once Windows returns.
fn start_announcer(slot: &Arc<Mutex<Option<Announcer>>>, provider: &GattServiceProvider, params: GattServiceProviderAdvertisingParameters) -> Result<(), Error> {
    let leftover: Arc<Mutex<Leftover>> = Arc::default();
    let (call, stop, parked) = (provider.clone(), provider.clone(), leftover.clone());
    let result = bounded(START_CALL_LIMIT, move || call.StartAdvertisingWithParameters(&params).map_err(Error::from), move || {
        let announcer = {
            let mut parked = parked.lock().unwrap_or_else(|e| e.into_inner());
            parked.returned = true;
            parked.announcer.take()
        };
        let _ = stop.StopAdvertising();
        drop(announcer);
    });
    if result == Err(Error::NotResponding) {
        let taken = slot.lock().unwrap().take();
        let mut parked = leftover.lock().unwrap_or_else(|e| e.into_inner());
        if parked.returned {
            // Windows answered in the meantime: stopping here no longer waits.
            drop(parked);
            drop(taken);
        } else {
            parked.announcer = taken;
        }
    }
    result
}

fn parameters(connectable: bool) -> Result<GattServiceProviderAdvertisingParameters, Error> {
    let value = GattServiceProviderAdvertisingParameters::new()?;
    value.SetIsDiscoverable(false)?; // Never opt into broadcasting the Windows friendly name.
    value.SetIsConnectable(connectable)?;
    Ok(value)
}

/// GattServiceProviderAdvertisementStatus by name (logs carry both).
pub fn status_name(status: i32) -> &'static str {
    match status {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAllAdvertisementData",
        _ => "None",
    }
}

/// Waits for the announcement to settle. Measured: Windows reports Aborted
/// for about 20 ms, then StartedWithoutAllAdvertisementData (status 4),
/// which for the extended PC form is the state that puts the whole payload
/// on air (observed by two phones). Aborted is final only after 3 s.
fn settle(provider: &GattServiceProvider, extended: bool, deadline: Instant, cancelled: &AtomicBool) -> Result<i32, Error> {
    let mut aborted_since: Option<Instant> = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let status = provider.AdvertisementStatus()?;
        match status {
            GattServiceProviderAdvertisementStatus::Started => return Ok(status.0),
            GattServiceProviderAdvertisementStatus::StartedWithoutAllAdvertisementData if extended => return Ok(status.0),
            GattServiceProviderAdvertisementStatus::StartedWithoutAllAdvertisementData => return Err(Error::AdvertisingIncomplete),
            GattServiceProviderAdvertisementStatus::Aborted => {
                if aborted_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(3) {
                    return Err(Error::AdvertisingAborted);
                }
            }
            _ => aborted_since = None,
        }
        if Instant::now() >= deadline {
            return Err(if aborted_since.is_some() { Error::AdvertisingAborted } else { Error::Timeout });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One announcement provider for `form`. The PC form mirrors the measured
/// provider exactly (write/notify characteristics; every write refused).
/// A short, stable tag for a connecting device in the logs (never its address).
fn peer_tag(id: &HSTRING) -> String {
    crate::crypto::sha256(id.to_string().as_bytes()).map_or_else(|_| "?".into(), |h| h[..4].iter().map(|b| format!("{b:02x}")).collect())
}

fn announcer(form: Form, events: Arc<Mutex<Vec<(i32, i32)>>>, gatt: Arc<Mutex<Vec<String>>>, deadline: Instant, cancelled: &AtomicBool) -> Result<Announcer, Error> {
    let uuid = match form {
        Form::CompactAlias(compact) | Form::Renewal(compact) => GUID::from_u128(u128::from_be_bytes(compact)),
        Form::PcAnnouncement(_) | Form::PcTest(_) | Form::PcStream(_) => GUID::from_u128(crate::pc_announcement::UUID),
    };
    step("announcer_create");
    let created = wait(GattServiceProvider::CreateAsync(uuid)?, deadline, cancelled)?;
    step("announcer_created");
    if created.Error()? != BluetoothError::Success {
        return Err(Error::Bluetooth(created.Error()?.0));
    }
    let provider = created.ServiceProvider()?;
    let mut write = None;
    let mut notify = None;
    if matches!(form, Form::PcAnnouncement(_) | Form::PcTest(_) | Form::PcStream(_)) {
        let kind = match form {
            Form::PcTest(payload) => format!(" pc_test_kind={}", payload[0] & 15),
            _ => String::new(),
        };
        let kind_tx = kind.clone();
        let rx = GattLocalCharacteristicParameters::new()?;
        rx.SetCharacteristicProperties(GattCharacteristicProperties::Write | GattCharacteristicProperties::WriteWithoutResponse)?;
        rx.SetWriteProtectionLevel(GattProtectionLevel::Plain)?;
        let tx = GattLocalCharacteristicParameters::new()?;
        tx.SetCharacteristicProperties(GattCharacteristicProperties::Notify)?;
        tx.SetReadProtectionLevel(GattProtectionLevel::Plain)?;
        let service = provider.Service()?;
        let a = wait(service.CreateCharacteristicAsync(WRITE, &rx)?, deadline, cancelled)?;
        let b = wait(service.CreateCharacteristicAsync(NOTIFY, &tx)?, deadline, cancelled)?;
        for status in [a.Error()?, b.Error()?] {
            if status != BluetoothError::Success {
                return Err(Error::Bluetooth(status.0));
            }
        }
        let characteristic = a.Characteristic()?;
        // Calls use the data service; a stray write here is completed and
        // ignored (never left pending), as in the measured gate provider.
        let log = gatt.clone();
        let token = characteristic.WriteRequested(&TypedEventHandler::<GattLocalCharacteristic, GattWriteRequestedEventArgs>::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let peer = args.Session().and_then(|s| s.DeviceId()).and_then(|d| d.Id()).map_or_else(|_| "?".into(), |id| peer_tag(&id));
                log.lock().unwrap().push(format!("write service=c8d5a10a char=c8d5a103 peer={peer}{kind} (announcement service: calls use c8d5a105)"));
                args.GetDeferral()?.Complete()?;
            }
            Ok(())
        }))?;
        write = Some((characteristic, token));
        let tx = b.Characteristic()?;
        let log = gatt.clone();
        let token = tx.SubscribedClientsChanged(&TypedEventHandler::<GattLocalCharacteristic, IInspectable>::new(move |sender, _| {
            if let Some(sender) = sender.as_ref() {
                let clients = sender.SubscribedClients()?.Size()?;
                log.lock().unwrap().push(format!("subscribed service=c8d5a10a char=c8d5a104 clients={clients}{kind_tx} (announcement service: calls use c8d5a105)"));
            }
            Ok(())
        }))?;
        notify = Some((tx, token));
    }
    step("announcer_status_handler");
    let status_token = provider.AdvertisementStatusChanged(&TypedEventHandler::<
        GattServiceProvider,
        GattServiceProviderAdvertisementStatusChangedEventArgs,
    >::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            events.lock().unwrap().push((args.Status()?.0, args.Error()?.0));
        }
        Ok(())
    }))?;
    Ok(Announcer { provider, status_token, write, notify })
}

impl Peripheral {
    /// Only registers services; advertising is a separate explicit action
    /// after owner setup. Experimental form (`Form::CompactAlias`).
    pub fn create(announcement: &Announcement, cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        if !announcement.is_gatt() {
            return Err(Error::Unsupported);
        }
        let compact = announcement.compact().map_err(|_| Error::Unsupported)?;
        Self::build(Form::CompactAlias(compact), cancelled)
    }
    /// Design QW A2 v1: the 9-byte PC call-offer announcement.
    pub fn create_pc(payload: [u8; crate::pc_announcement::LEN], cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        crate::pc_announcement::CallOffer::decode(&payload).map_err(|_| Error::Unsupported)?;
        Self::build(Form::PcAnnouncement(payload), cancelled)
    }
    pub fn create_stream(payload: [u8; crate::pc_announcement::STREAM_LEN], cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        match crate::pc_announcement::decode(&payload) {
            Ok(crate::pc_announcement::Pc::Request(_) | crate::pc_announcement::Pc::Offer(_)) => Self::build(Form::PcStream(payload), cancelled),
            _ => Err(Error::Unsupported),
        }
    }
    /// Design SR: the armed side's renewal server and its per-attempt advert
    /// (`uuid`). Registers the data service only; `start` puts it on air.
    pub fn create_renewal(uuid: [u8; 16], cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        Self::build(Form::Renewal(uuid), cancelled)
    }
    fn build(form: Form, cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let apartment = Apartment::new()?;
        let deadline = Instant::now() + Duration::from_secs(12);
        probe().require(true)?;
        let result = wait(
            GattServiceProvider::CreateAsync(SERVICE)?,
            deadline,
            &cancelled,
        )?;
        if result.Error()? != BluetoothError::Success {
            return Err(Error::Bluetooth(result.Error()?.0));
        }
        let data = result.ServiceProvider()?;
        let rx = GattLocalCharacteristicParameters::new()?;
        rx.SetCharacteristicProperties(
            GattCharacteristicProperties::Write
                | GattCharacteristicProperties::WriteWithoutResponse,
        )?;
        rx.SetWriteProtectionLevel(GattProtectionLevel::Plain)?;
        let tx = GattLocalCharacteristicParameters::new()?;
        tx.SetCharacteristicProperties(GattCharacteristicProperties::Notify)?;
        tx.SetReadProtectionLevel(GattProtectionLevel::Plain)?;
        let rx = wait(
            data.Service()?.CreateCharacteristicAsync(WRITE, &rx)?,
            deadline,
            &cancelled,
        )?;
        let tx = wait(
            data.Service()?.CreateCharacteristicAsync(NOTIFY, &tx)?,
            deadline,
            &cancelled,
        )?;
        for status in [rx.Error()?, tx.Error()?] {
            if status != BluetoothError::Success {
                return Err(Error::Bluetooth(status.0));
            }
        }
        let write = rx.Characteristic()?;
        let notify = tx.Characteristic()?;
        let offer: Arc<Mutex<Option<Announcer>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(State {
            admission: Admission::default(),
            client: None,
            session: None,
            pipe: Pipe::for_link("server"),
        }));
        let gatt_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let write_log = gatt_events.clone();
        let first_write = Arc::new(AtomicBool::new(true));
        let shared = state.clone();
        let stop = cancelled.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let write_token = write.WriteRequested(&TypedEventHandler::<
            GattLocalCharacteristic,
            GattWriteRequestedEventArgs,
        >::new(move |_, args| {
            let args = args.ok()?;
            let deferral = args.GetDeferral()?;
            let session = match args
                .Session()
                .and_then(|s| s.DeviceId())
                .and_then(|d| d.Id())
            {
                Ok(id) => id,
                Err(error) => {
                    let _ = deferral.Complete();
                    return Err(error);
                }
            };
            if first_write.swap(false, Ordering::AcqRel) {
                let owner = shared.lock().unwrap().admission.owns(&session);
                write_log.lock().unwrap().push(format!("first_write service=c8d5a105 char=c8d5a103 peer={} admitted_subscriber={owner}", peer_tag(&session)));
            }
            // Reject other centrals before allocating a deferred-request slot or reading their value
            // (a phone not yet admitted may write first; its values are held until it is).
            if stop.load(Ordering::Acquire) || !shared.lock().unwrap().admission.may_write(&session) {
                let _ = deferral.Complete();
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070005u32 as i32,
                )));
            }
            let Some(slot) = RequestSlot::reserve(&requests) else {
                let state = shared.lock().unwrap();
                if state.admission.owns(&session) {
                    stop.store(true, Ordering::Release);
                    state.pipe.abort(carrier::Error::Overflow);
                }
                let _ = deferral.Complete();
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x8007000eu32 as i32,
                )));
            };
            let operation = match args.GetRequestAsync() {
                Ok(op) => op,
                Err(error) => {
                    let _ = deferral.Complete();
                    return Err(error);
                }
            };
            let shared = shared.clone();
            let stop = stop.clone();
            let completion = deferral.clone();
            let registered = operation.when(move |result| {
                let _slot = slot;
                let own = shared.lock().unwrap().admission.may_write(&session);
                let process = || -> windows::core::Result<()> {
                    let request = result?;
                    let mut state = shared.lock().unwrap();
                    let valid = own && !stop.load(Ordering::Acquire) && request.Offset()? == 0;
                    // Decided under the lock, in completion order: the admitted
                    // phone's value goes to the pipe; before any admission it is
                    // held (the subscription can follow the first write).
                    let valid = valid
                        && read_buffer(&request.Value()?, 512)
                            .map(|value| match state.admission.on_value(&session, &value, Instant::now()) {
                                Written::Deliver => state.pipe.offer(&value).is_ok(),
                                Written::Held => true,
                                Written::Reject => false,
                            })
                            .unwrap_or(false);
                    let own = state.admission.owns(&session);
                    if own && !valid {
                        stop.store(true, Ordering::Release);
                        state.pipe.abort(carrier::Error::InvalidValue);
                    }
                    if request.Option()? == GattWriteOption::WriteWithResponse {
                        if valid {
                            request.Respond()?;
                        } else {
                            request
                                .RespondWithProtocolError(GattProtocolError::WriteNotPermitted()?)?;
                        }
                    }
                    Ok(())
                };
                if process().is_err() && shared.lock().unwrap().admission.owns(&session) {
                    stop.store(true, Ordering::Release);
                    shared.lock().unwrap().pipe.abort(carrier::Error::Backend);
                }
                let _ = completion.Complete();
            });
            if let Err(error) = registered {
                let _ = deferral.Complete();
                return Err(error);
            }
            Ok(())
        }))?;
        let shared = state.clone();
        let stop = cancelled.clone();
        let offer_copy = offer.clone();
        let alias: Arc<Mutex<Option<Announcer>>> = Arc::new(Mutex::new(None));
        let alias_copy = alias.clone();
        let subscribe_log = gatt_events.clone();
        let subscribed = notify.SubscribedClientsChanged(&TypedEventHandler::<
            GattLocalCharacteristic,
            IInspectable,
        >::new(move |sender, _| {
            let process = || -> windows::core::Result<()> {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let clients = sender.ok()?.SubscribedClients()?;
                let first = if clients.Size()? > 0 {
                    let client = clients.GetAt(0)?;
                    let session = client.Session()?;
                    format!(" peer={} max_pdu={}", peer_tag(&session.DeviceId()?.Id()?), session.MaxPduSize().unwrap_or(0))
                } else {
                    String::new()
                };
                subscribe_log.lock().unwrap().push(format!("subscribed service=c8d5a105 char=c8d5a104 clients={}{first}", clients.Size()?));
                let mut state = shared.lock().unwrap();
                if let Some(owner) = &state.admission.owner {
                    let mut present = false;
                    for client in &clients {
                        if client.Session()?.DeviceId()?.Id()? == *owner {
                            present = true;
                            break;
                        }
                    }
                    if !present {
                        stop.store(true, Ordering::Release);
                        state.pipe.abort(carrier::Error::Closed);
                    }
                    return Ok(());
                }
                if clients.Size()? == 0 {
                    return Ok(());
                }
                let client = clients.GetAt(0)?;
                let session = client.Session()?;
                let Some(held) = state.admission.admit(&session.DeviceId()?.Id()?, Instant::now()) else {
                    return Ok(());
                };
                // What this phone wrote before its subscription arrived, in order.
                for value in held {
                    if state.pipe.offer(&value).is_err() {
                        stop.store(true, Ordering::Release);
                        state.pipe.abort(carrier::Error::InvalidValue);
                        return Ok(());
                    }
                }
                let pipe = state.pipe.clone();
                let stopped = stop.clone();
                let token =
                    session.SessionStatusChanged(&TypedEventHandler::<
                        GattSession,
                        GattSessionStatusChangedEventArgs,
                    >::new(move |_, args| {
                        if args.ok()?.Status()? == GattSessionStatus::Closed {
                            stopped.store(true, Ordering::Release);
                            pipe.abort(carrier::Error::Closed);
                        }
                        Ok(())
                    }))?;
                state.session = Some((session, token));
                state.client = Some(client);
                // One caller: the announcement goes off air. The data service
                // stays published, since the admitted phone talks through it.
                // Both forms: one caller, whichever advert it found.
                for announcers in [&offer_copy, &alias_copy] {
                    if let Some(announcer) = announcers.lock().unwrap().as_ref() {
                        let _ = announcer.provider.StopAdvertising();
                    }
                }
                Ok(())
            };
            let result = process();
            if result.is_err() {
                stop.store(true, Ordering::Release);
                shared.lock().unwrap().pipe.abort(carrier::Error::Backend);
            }
            result
        }));
        let subscribe_token = match subscribed {
            Ok(token) => token,
            Err(error) => {
                let _ = write.RemoveWriteRequested(write_token);
                return Err(error.into());
            }
        };
        Ok(Self {
            writer: Writer::default(),
            io: PeripheralIo {
                notify,
                state,
                cancelled,
                native_error: None,
            },
            data,
            form,
            offer,
            alias,
            alias_last: None,
            test_extra: Arc::new(Mutex::new(None)),
            test_events: Arc::new(Mutex::new(Vec::new())),
            peer_device: None,
            status_events: Arc::new(Mutex::new(Vec::new())),
            gatt_events,
            write,
            write_token,
            subscribe_token,
            started: false,
            data_stuck: false,
            last_status: (-1, -1),
            _apartment: apartment,
        })
    }
    /// Starts the data service and the announcement. Never substitutes a
    /// name-bearing advertisement or a new wire format when it fails.
    pub fn start(&mut self) -> Result<(), Error> {
        self.start_mode(true)
    }
    /// Reopen the data slot while the announcement's RDI boundary stays silent.
    pub fn start_silent(&mut self) -> Result<(), Error> {
        self.start_mode(false)
    }
    fn start_mode(&mut self, announce: bool) -> Result<(), Error> {
        if self.started || self.io.cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if start_pending() {
            return Err(Error::NotResponding);
        }
        step("probe");
        probe().require(true)?;
        self.started = true;
        let pc = matches!(self.form, Form::PcAnnouncement(_) | Form::PcStream(_) | Form::Renewal(_));
        let result = (|| {
            // Measured (gate 1): the data provider connectable and without
            // data stays Created while the announcement is on air.
            step("data_start_advertising");
            let started = start_data(&self.data, parameters(pc)?);
            self.data_stuck = started == Err(Error::NotResponding);
            started?;
            if announce { self.start_announcement(self.form) } else { Ok(()) }
        })();
        // A start still inside Windows keeps its step for the trace.
        if result != Err(Error::NotResponding) {
            step(if result.is_ok() { "started" } else { "start_failed" });
        }
        if result.is_err() {
            self.io.abort();
            self.stop_announcement();
            if !self.data_stuck {
                let _ = self.data.StopAdvertising();
            }
        }
        result
    }
    /// A new announcement provider for `form` (the previous one is stopped
    /// first: one address never carries two payloads, gate 2), waited until
    /// on air. The data service and an admitted caller are not touched.
    pub fn start_announcement(&mut self, form: Form) -> Result<(), Error> {
        if self.io.cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if start_pending() {
            return Err(Error::NotResponding);
        }
        self.stop_announcement();
        self.form = form;
        // MESHLIVE_QW_FORM=0: only the compact alias goes on air (one address).
        if let Form::PcAnnouncement(payload) = form {
            if !super::advertise::qw_form_enabled() {
                self.start_alias(&payload);
                let alias = self.alias_last.map(|(_, r)| r).unwrap_or(Err(Error::Unsupported));
                self.last_status = (self.data.AdvertisementStatus().map_or(-1, |s| s.0), *alias.as_ref().unwrap_or(&-1));
                return alias.map(|_| ());
            }
        }
        // The renewal must be on air inside the protocol's 3 s registration bound.
        let deadline = Instant::now() + Duration::from_secs(if matches!(form, Form::Renewal(_)) { 3 } else { 8 });
        let announcer = announcer(form, self.status_events.clone(), self.gatt_events.clone(), deadline, &self.io.cancelled)?;
        let provider = announcer.provider.clone();
        *self.offer.lock().unwrap() = Some(announcer);
        let params = parameters(true)?;
        let service_data: Option<&[u8]> = match &form {
            Form::PcAnnouncement(payload) => Some(payload),
            Form::PcTest(payload) | Form::PcStream(payload) => Some(payload),
            Form::CompactAlias(_) | Form::Renewal(_) => None,
        };
        if let Some(payload) = service_data {
            params.SetUseLowEnergyUncoded1MPhyAsSecondaryPhy(true)?;
            let writer = windows::Storage::Streams::DataWriter::new()?;
            writer.WriteBytes(payload)?;
            params.SetServiceData(&writer.DetachBuffer()?)?;
        }
        step("offer_start_advertising");
        start_announcer(&self.offer, &provider, params)?;
        step("offer_settle");
        let settled = settle(&provider, service_data.is_some(), deadline, &self.io.cancelled);
        self.last_status = (self.data.AdvertisementStatus().map_or(-1, |s| s.0), settled.as_ref().map_or_else(|_| provider.AdvertisementStatus().map_or(-1, |s| s.0), |s| *s));
        if settled.is_err() {
            self.stop_announcement();
            return settled.map(|_| ());
        }
        if let Form::PcAnnouncement(payload) = form {
            if super::advertise::compact_offer_enabled() {
                self.start_alias(&payload);
            }
        }
        settled.map(|_| ())
    }
    /// The compact alias of `payload` (after the main form is on air, the same
    /// RDI). A failure is recorded in `alias_last` and never takes the main
    /// announcement down.
    fn start_alias(&mut self, payload: &[u8; crate::pc_announcement::LEN]) {
        let Ok(uuid) = super::advertise::pc_compact_alias(payload) else {
            self.alias_last = None;
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        let result = (|| -> Result<i32, Error> {
            let announcer = announcer(Form::CompactAlias(uuid), self.status_events.clone(), self.gatt_events.clone(), deadline, &self.io.cancelled)?;
            let provider = announcer.provider.clone();
            *self.alias.lock().unwrap() = Some(announcer);
            let params = parameters(true)?;
            let discoverable = super::advertise::compact_discoverable();
            if discoverable {
                // Experimental, off by default: see `compact_discoverable`.
                // Discoverable puts the service UUID into the advert without
                // service data (Microsoft "Bluetooth GATT Server"), so this arm
                // carries NO service data: the 128-bit UUID list fits one
                // legacy advert, which should leave a single advertising set
                // (one address). The name may be on air: measure before use.
                params.SetIsDiscoverable(true)?;
            } else {
                // Measured (Design QW gate 1): a non-discoverable provider
                // starts only with service data; a 128-bit UUID list plus
                // 128-bit service data does not fit a legacy advert, so Windows
                // reports StartedWithoutAllAdvertisementData. Scanners that read
                // extended adverts (A15s, 2026-10-20) then hear this RDI from a
                // second address too: a legacy UUID-list set and an extended
                // set (to be confirmed by a raw observer).
                let writer = windows::Storage::Streams::DataWriter::new()?;
                writer.WriteByte(0)?;
                params.SetServiceData(&writer.DetachBuffer()?)?;
            }
            step("alias_start_advertising");
            start_announcer(&self.alias, &provider, params)?;
            step("alias_settle");
            settle(&provider, true, deadline, &self.io.cancelled)
        })();
        if result.is_err() {
            step("alias_stop");
            self.alias.lock().unwrap().take();
        }
        self.alias_last = Some((uuid, result));
    }
    /// Test hooks only (Design QV W1/W2): a new provider carrying the kind 1
    /// or 2 `payload` as service data, connectable, non-discoverable and
    /// extended (LE 1M secondary), next to the call offer and the data
    /// service. A new provider each time (a new address). Returns the
    /// settled status; a failure leaves the other providers untouched.
    pub fn start_test_extra(&mut self, payload: [u8; crate::pc_announcement::STREAM_LEN]) -> Result<i32, Error> {
        self.test_extra.lock().unwrap().take();
        let deadline = Instant::now() + Duration::from_secs(8);
        let result = (|| -> Result<i32, Error> {
            let announcer = announcer(Form::PcTest(payload), self.test_events.clone(), self.gatt_events.clone(), deadline, &self.io.cancelled)?;
            let provider = announcer.provider.clone();
            *self.test_extra.lock().unwrap() = Some(announcer);
            let params = parameters(true)?;
            params.SetUseLowEnergyUncoded1MPhyAsSecondaryPhy(true)?;
            let writer = windows::Storage::Streams::DataWriter::new()?;
            writer.WriteBytes(&payload)?;
            params.SetServiceData(&writer.DetachBuffer()?)?;
            start_announcer(&self.test_extra, &provider, params)?;
            settle(&provider, true, deadline, &self.io.cancelled)
        })();
        if result.is_err() {
            self.test_extra.lock().unwrap().take();
        }
        result
    }
    /// Status events of the test provider since the last call: (status, error).
    pub fn take_test_events(&self) -> Vec<(i32, i32)> {
        std::mem::take(&mut *self.test_events.lock().unwrap())
    }
    /// Every provider's advertisement status now (-1: none), for the W1 trace.
    pub fn provider_statuses(&self) -> String {
        let status = |slot: &Arc<Mutex<Option<Announcer>>>| slot.lock().unwrap().as_ref().map_or(-1, |a| a.provider.AdvertisementStatus().map_or(-1, |s| s.0));
        format!(
            "data={} call_offer={} compact_alias={} pc_test={}",
            self.data.AdvertisementStatus().map_or(-1, |s| s.0),
            status(&self.offer),
            status(&self.alias),
            status(&self.test_extra)
        )
    }
    /// The compact alias started with the last announcement: (UUID, status or error).
    pub fn alias_result(&self) -> Option<([u8; 16], Result<i32, Error>)> {
        self.alias_last
    }
    /// Takes the announcement off air (boundary silence, a call, Bluetooth off).
    pub fn stop_announcement(&mut self) {
        self.test_extra.lock().unwrap().take();
        self.alias.lock().unwrap().take();
        self.offer.lock().unwrap().take();
    }
    /// The current form (its payload).
    pub fn form(&self) -> Form {
        self.form
    }
    /// The form `start` puts on air (test hooks: the folded kind 1/2 form).
    pub fn set_form(&mut self, form: Form) {
        self.form = form;
    }
    /// The announcement status now: the c8d5a10a provider's, or the compact
    /// alias's when it is the only form on air (MESHLIVE_QW_FORM=0); -1: none.
    pub fn announcement_status(&self) -> i32 {
        let status = |a: &Announcer| a.provider.AdvertisementStatus().map_or(-1, |s| s.0);
        if let Some(main) = self.offer.lock().unwrap().as_ref() {
            return status(main);
        }
        self.alias.lock().unwrap().as_ref().map_or(-1, status)
    }
    /// Announcement status events since the last call: (status, error).
    /// GATT activity lines since the last call (subscriptions, first writes).
    pub fn take_gatt_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.gatt_events.lock().unwrap())
    }
    pub fn take_status_events(&self) -> Vec<(i32, i32)> {
        std::mem::take(&mut *self.status_events.lock().unwrap())
    }
    /// A phone subscribed (admitted); non-blocking.
    pub fn has_client(&self) -> bool {
        self.io.state.lock().unwrap().client.is_some()
    }
    /// Whether Windows holds a pairing (bond) with the admitted phone
    /// (CF-PLAT-05: the link is then identity-bound even in a stranger
    /// call). Read-only: never pairs, never asks the user. Blocks up to 2 s;
    /// None when unknown. Worker thread only.
    pub fn client_paired(&self) -> Option<bool> {
        let id = self.io.state.lock().unwrap().session.as_ref()?.0.DeviceId().ok()?.Id().ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let device = wait(windows::Devices::Bluetooth::BluetoothLEDevice::FromIdAsync(&id).ok()?, deadline, &self.io.cancelled).ok()?;
        device.DeviceInformation().ok()?.Pairing().ok()?.IsPaired().ok()
    }
    /// The last (data service, announcement) GattServiceProviderAdvertisementStatus
    /// values seen while starting: 0 Created, 1 Stopped, 2 Started, 3 Aborted,
    /// 4 StartedWithoutAllAdvertisementData; -1 before any start.
    pub fn last_advertisement_status(&self) -> (i32, i32) {
        self.last_status
    }
    pub fn wait_for_client(&mut self, deadline: Instant) -> Result<(), Error> {
        let result = (|| {
            loop {
                if self.io.cancelled.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                if self.io.state.lock().unwrap().client.is_some() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Write-request arrival times and sizes since the last call (CF-MEDIA-23).
    pub fn take_arrivals(&self) -> Vec<(Instant, usize)> {
        self.io.state.lock().unwrap().pipe.take_arrivals()
    }
    pub fn read(&self, bytes: &mut [u8], deadline: Instant) -> Result<usize, carrier::Error> {
        let pipe = self.io.state.lock().unwrap().pipe.clone();
        pipe.read(bytes, deadline, &self.io.cancelled)
    }
    /// WP6NH: the call's peer is blocked: no more GR1 resends.
    pub fn fence_recovery(&mut self) {
        self.writer.fence_recovery(&self.io);
    }
    pub fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), carrier::Error> {
        let cancelled = self.io.cancelled.clone();
        self.writer.write(bytes, &mut self.io, deadline, &cancelled)
    }
    pub fn native_error(&self) -> Option<Error> {
        self.io.native_error
    }
    /// GR1 on this link while the data writer is idle (no-op when off).
    pub fn service(&mut self) -> Result<(), carrier::Error> {
        let cancelled = self.io.cancelled.clone();
        self.writer.service(&mut self.io, Instant::now() + Duration::from_secs(8), &cancelled)
    }
    /// `gatt_recovery` and `gatt_debug_drop` lines since the last call.
    pub fn take_notes(&self) -> Vec<(&'static str, String)> {
        self.io.state.lock().unwrap().pipe.take_notes()
    }
    /// What Windows reports for the subscribed phone's link now: connection
    /// interval, latency, supervision timeout, PHY per direction and the ATT
    /// MTU. WinRT exposes no LE data length (DLE) and no ACL segmentation.
    pub fn link_state(&mut self) -> String {
        let session = self.io.state.lock().unwrap().client.as_ref().and_then(|c| c.Session().ok());
        let Some(session) = session else { return "link=unsubscribed".into() };
        let max_pdu = session.MaxPduSize().unwrap_or(0);
        if self.peer_device.is_none() {
            let cancelled = AtomicBool::new(false);
            self.peer_device = session
                .DeviceId()
                .and_then(|d| d.Id())
                .ok()
                .and_then(|id| BluetoothLEDevice::FromIdAsync(&id).ok())
                .and_then(|op| wait(op, Instant::now() + Duration::from_secs(2), &cancelled).ok());
        }
        match &self.peer_device {
            Some(device) => format!("{} {} max_pdu={max_pdu}", connection_params(device), connection_phy(device)),
            None => format!("device=unavailable max_pdu={max_pdu}"),
        }
    }
    /// Native-check routing hint for this subscribed client.
    /// Kept in memory only; never logged or persisted by the product.
    pub fn diagnostic_peer_route(&mut self) -> Option<(u64, i32)> {
        if self.peer_device.is_none() { let _ = self.link_state(); }
        let device = self.peer_device.as_ref()?;
        Some((device.BluetoothAddress().ok()?, device.BluetoothAddressType().ok()?.0))
    }
    pub fn close(&mut self) {
        self.io.cancelled.store(true, Ordering::Release);
        self.stop_announcement();
        if !self.data_stuck {
            let _ = self.data.StopAdvertising();
        }
        self.io.disconnect();
    }
}
impl LinkIo for PeripheralIo {
    fn recovery_pipe(&self) -> Option<Pipe> {
        Some(self.state.lock().unwrap().pipe.clone())
    }
    fn capacity(&self) -> Result<usize, carrier::Error> {
        let state = self.state.lock().unwrap();
        let client = state.client.as_ref().ok_or(carrier::Error::Closed)?;
        Ok(usize::from(
            client
                .MaxNotificationSize()
                .map_err(|_| carrier::Error::Backend)?,
        )
        .min(512)
        .min(carrier::value_max()))
    }
    fn send(
        &mut self,
        value: &[u8],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), carrier::Error> {
        if value.len() > self.capacity()? {
            return Err(carrier::Error::InvalidValue);
        }
        let send = || -> Result<(), Error> {
            let client = self
                .state
                .lock()
                .unwrap()
                .client
                .clone()
                .ok_or(Error::Unsupported)?;
            let result = wait(
                self.notify
                    .NotifyValueForSubscribedClientAsync(&buffer(value)?, &client)?,
                deadline.min(Instant::now() + Duration::from_secs(8)),
                cancelled,
            )?;
            if result.Status()? != GattCommunicationStatus::Success {
                return Err(Error::Gatt(result.Status()?.0));
            }
            Ok(())
        };
        match send() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.native_error = Some(error);
                Err(match error {
                    Error::Timeout => carrier::Error::Timeout,
                    Error::Cancelled => carrier::Error::Cancelled,
                    _ => carrier::Error::Backend,
                })
            }
        }
    }
    fn abort(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.state
            .lock()
            .unwrap()
            .pipe
            .abort(carrier::Error::Backend);
        self.disconnect();
    }
}
impl PeripheralIo {
    fn disconnect(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let session = {
            let mut state = self.state.lock().unwrap();
            state.pipe.close();
            state.client = None;
            state.session.take()
        };
        if let Some((session, token)) = session {
            let _ = session.RemoveSessionStatusChanged(token);
            let _ = session.Close();
        }
    }
}
impl Drop for Peripheral {
    fn drop(&mut self) {
        self.close();
        let _ = self.write.RemoveWriteRequested(self.write_token);
        let _ = self
            .io
            .notify
            .RemoveSubscribedClientsChanged(self.subscribe_token);
        self.io.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_start_that_never_returns_fails_in_time_and_is_stopped_when_windows_answers() {
        // A fake StartAdvertising that blocks until released (as on the
        // 2026-11-06 laptop), then a working one.
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let (stopped_tx, stopped) = std::sync::mpsc::channel::<&str>();
        let begun = Instant::now();
        let result = bounded(Duration::from_millis(200), move || { let _ = parked.recv(); Ok(()) }, move || { let _ = stopped_tx.send("stopped"); });
        assert_eq!(result, Err(Error::NotResponding));
        assert!(begun.elapsed() < Duration::from_secs(2), "the caller got control back");
        assert!(start_pending(), "the stuck start is still inside Windows");
        // While it is stuck, a new start fails at once and never runs.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert_eq!(bounded(Duration::from_secs(5), move || { flag.store(true, Ordering::Release); Ok(()) }, || {}), Err(Error::NotResponding));
        assert!(!ran.load(Ordering::Acquire));
        // Windows answers: the late start is stopped on its own thread.
        release.send(()).unwrap();
        assert_eq!(stopped.recv_timeout(Duration::from_secs(5)), Ok("stopped"));
        let wait = Instant::now();
        while start_pending() && wait.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!start_pending());
        // The next start runs and keeps its own outcome; nothing is stopped.
        let (late_tx, late) = std::sync::mpsc::channel::<()>();
        assert_eq!(bounded(Duration::from_secs(5), || Ok(7), move || { let _ = late_tx.send(()); }), Ok(7));
        assert_eq!(bounded::<()>(Duration::from_secs(5), || Err(Error::Bluetooth(2)), || {}), Err(Error::Bluetooth(2)));
        assert!(late.recv_timeout(Duration::from_millis(100)).is_err(), "a start that returned in time is not stopped");
    }
    #[test]
    fn deferred_request_ownership_is_bounded_and_released_on_every_drop() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut held: Vec<_> = (0..4096)
            .map(|_| RequestSlot::reserve(&count).unwrap())
            .collect();
        assert!(RequestSlot::reserve(&count).is_none());
        held.pop();
        assert!(RequestSlot::reserve(&count).is_some());
        drop(held);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn first_subscriber_owns_writes_and_foreign_requests_cannot_replace_it() {
        let mut admission = Admission::default();
        let first = HSTRING::from("peer-a");
        let other = HSTRING::from("peer-b");
        assert!(!admission.owns(&first));
        assert!(admission.claim(&first));
        assert!(admission.owns(&first));
        assert!(!admission.owns(&other));
        assert!(!admission.claim(&other));
        assert!(admission.owns(&first));
    }
    #[test]
    fn a_write_before_the_subscription_is_held_and_delivered_in_order_on_admission() {
        // 2026-10-28 S24 call: Windows raised the phone's first write in the
        // same millisecond as, and before, its subscription; it was refused
        // and the call setup stalled.
        let mut admission = Admission::default();
        let phone = HSTRING::from("phone");
        let stranger = HSTRING::from("stranger");
        assert!(admission.may_write(&phone) && admission.may_write(&stranger), "no one admitted yet");
        let t = Instant::now();
        assert_eq!(admission.on_value(&phone, b"hello", t), Written::Held);
        assert_eq!(admission.on_value(&stranger, b"noise", t), Written::Held);
        assert_eq!(admission.on_value(&phone, b"msg1", t), Written::Held);
        let held = admission.admit(&phone, t + Duration::from_millis(5)).unwrap();
        assert_eq!(held, vec![b"hello".to_vec(), b"msg1".to_vec()], "the phone's values, in order; the stranger's dropped");
        assert_eq!(admission.on_value(&phone, b"next", t), Written::Deliver);
        assert_eq!(admission.on_value(&stranger, b"x", t), Written::Reject);
        assert!(!admission.may_write(&stranger));
        assert!(admission.admit(&stranger, t).is_none(), "the first subscriber keeps the link");
        assert!(admission.admit(&phone, t).unwrap().is_empty(), "held values go out once");
        // Bounded before admission.
        let mut flood = Admission::default();
        let big = vec![0u8; 512];
        let held = (0..200).filter(|_| flood.on_value(&stranger, &big, t) == Written::Held).count();
        assert_eq!(held, HELD_MAX_BYTES / 512);
        assert_eq!(flood.on_value(&stranger, &big, t), Written::Reject);
        // A late write from the phone after its last call ended is stale by
        // the time it subscribes for the next call: never replayed.
        let mut next = Admission::default();
        assert_eq!(next.on_value(&phone, b"old call's last write", t), Written::Held);
        let later = t + Duration::from_secs(9);
        assert_eq!(next.on_value(&phone, b"new hello", later), Written::Held);
        assert_eq!(next.admit(&phone, later + Duration::from_millis(3)).unwrap(), vec![b"new hello".to_vec()]);
    }
    #[test]
    fn native_advertising_parameters_never_enable_friendly_name_discovery() {
        let _apartment = Apartment::new().unwrap();
        for connectable in [false, true] {
            let plan = parameters(connectable).unwrap();
            assert!(!plan.IsDiscoverable().unwrap());
            assert_eq!(plan.IsConnectable().unwrap(), connectable);
        }
    }
}
