//! Native WinRT central. Caller supplies an ephemeral address from its selected sighting.
use super::{
    Apartment, Error,
    carrier::{self, LinkIo, Pacer, Pipe, Writer},
    probe, wait_with,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
        BluetoothLEPreferredConnectionParameters, BluetoothLEPreferredConnectionParametersRequest,
        GenericAttributeProfile::*,
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter, IBuffer},
    core::{GUID, IInspectable},
};

pub const SERVICE: GUID = GUID::from_u128(0xc8d5a105_9f88_4f65_a03d_7f5a2fb921fa);
pub const WRITE: GUID = GUID::from_u128(0xc8d5a103_9f88_4f65_a03d_7f5a2fb921fa);
pub const NOTIFY: GUID = GUID::from_u128(0xc8d5a104_9f88_4f65_a03d_7f5a2fb921fa);

pub(crate) fn read_buffer(buffer: &IBuffer, maximum: usize) -> windows::core::Result<Vec<u8>> {
    let size = buffer.Length()? as usize;
    if size == 0 || size > maximum {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x80070057u32 as i32,
        )));
    }
    let reader = DataReader::FromBuffer(buffer)?;
    let mut bytes = vec![0; size];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn buffer(bytes: &[u8]) -> windows::core::Result<IBuffer> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(bytes)?;
    writer.DetachBuffer()
}

/// The whole connection (device, session, discovery, CCCD) must finish in
/// this: the session wait plus discovery plus a margin. The phone's own 20 s
/// deadline starts only at the CCCD write (subscription), not here.
const CONNECT_SECONDS: u64 = 20;
/// A short look for an already-active session; the first GATT request is
/// what opens the link (measured), so nothing longer is waited for.
const ACTIVE_GRACE_MS: u64 = 300;
const RETRY_MS: u64 = 500;

#[derive(Clone)]
struct ConnectCancel {
    requested: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}
impl ConnectCancel {
    fn new(requested: Arc<AtomicBool>, separate: bool) -> Self {
        let stopped = if separate { Arc::new(AtomicBool::new(false)) } else { requested.clone() };
        Self { requested, stopped }
    }
    fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire)
    }
    fn stop(&self) { self.stopped.store(true, Ordering::Release); }
    fn wait<T: windows::core::RuntimeType + 'static>(&self, op: windows_future::IAsyncOperation<T>, deadline: Instant) -> Result<T, Error> {
        wait_with(op, deadline, || self.is_cancelled())
    }
}

/// Connection-time diagnostics: WinRT events recorded with their time since
/// the connect began, drained into the caller's trace at each step. Handlers
/// are removed when the connect returns (success or failure).
struct Events {
    log: Arc<std::sync::Mutex<Vec<String>>>,
    begun: Instant,
    device: BluetoothLEDevice,
    session: Option<GattSession>,
    connection: Option<i64>,
    pdu: Option<i64>,
    status: Option<i64>,
    params: Option<i64>,
    phy: Option<i64>,
}
impl Events {
    fn new(device: &BluetoothLEDevice, begun: Instant) -> Self {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = log.clone();
        let connection = device
            .ConnectionStatusChanged(&TypedEventHandler::<BluetoothLEDevice, IInspectable>::new(
                move |device, _| {
                    let status = device.ok()?.ConnectionStatus()?.0;
                    if let Ok(mut log) = sink.lock() {
                        log.push(format!(
                            "event=ConnectionStatusChanged connection={status} ms={}",
                            begun.elapsed().as_millis()
                        ));
                    }
                    Ok(())
                },
            ))
            .ok();
        // Granted connection parameters and PHY as Windows reports them when
        // they change (Windows 11 APIs; absent on older builds: None).
        let sink = log.clone();
        let params = device
            .ConnectionParametersChanged(&TypedEventHandler::<BluetoothLEDevice, IInspectable>::new(
                move |device, _| {
                    let line = connection_params(&device.ok()?.clone());
                    if let Ok(mut log) = sink.lock() {
                        log.push(format!("event=ConnectionParametersChanged {line} ms={}", begun.elapsed().as_millis()));
                    }
                    Ok(())
                },
            ))
            .ok();
        let sink = log.clone();
        let phy = device
            .ConnectionPhyChanged(&TypedEventHandler::<BluetoothLEDevice, IInspectable>::new(
                move |device, _| {
                    let line = connection_phy(&device.ok()?.clone());
                    if let Ok(mut log) = sink.lock() {
                        log.push(format!("event=ConnectionPhyChanged {line} ms={}", begun.elapsed().as_millis()));
                    }
                    Ok(())
                },
            ))
            .ok();
        Self {
            log,
            begun,
            device: device.clone(),
            session: None,
            connection,
            pdu: None,
            status: None,
            params,
            phy,
        }
    }
    fn watch(&mut self, session: &GattSession) {
        let begun = self.begun;
        let sink = self.log.clone();
        self.pdu = session
            .MaxPduSizeChanged(&TypedEventHandler::<GattSession, IInspectable>::new(
                move |session, _| {
                    let pdu = session.ok()?.MaxPduSize()?;
                    if let Ok(mut log) = sink.lock() {
                        log.push(format!(
                            "event=MaxPduSizeChanged max_pdu={pdu} ms={}",
                            begun.elapsed().as_millis()
                        ));
                    }
                    Ok(())
                },
            ))
            .ok();
        let sink = self.log.clone();
        self.status = session
            .SessionStatusChanged(&TypedEventHandler::<
                GattSession,
                GattSessionStatusChangedEventArgs,
            >::new(move |_, args| {
                let args = args.ok()?;
                let (status, error) = (args.Status()?.0, args.Error()?.0);
                if let Ok(mut log) = sink.lock() {
                    log.push(format!(
                        "event=SessionStatusChanged status={status} error={error} ms={}",
                        begun.elapsed().as_millis()
                    ));
                }
                Ok(())
            }))
            .ok();
        self.session = Some(session.clone());
    }
    fn drain(&self, trace: &mut dyn FnMut(&str, &str)) {
        let lines = self.log.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default();
        for line in lines {
            trace("winrt_event", &line);
        }
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        if let Some(token) = self.connection.take() {
            let _ = self.device.RemoveConnectionStatusChanged(token);
        }
        if let Some(token) = self.params.take() {
            let _ = self.device.RemoveConnectionParametersChanged(token);
        }
        if let Some(token) = self.phy.take() {
            let _ = self.device.RemoveConnectionPhyChanged(token);
        }
        if let Some(session) = &self.session {
            if let Some(token) = self.pdu.take() {
                let _ = session.RemoveMaxPduSizeChanged(token);
            }
            if let Some(token) = self.status.take() {
                let _ = session.RemoveSessionStatusChanged(token);
            }
        }
    }
}

/// The connection parameters Windows reports as granted now.
pub(crate) fn connection_params(device: &BluetoothLEDevice) -> String {
    device
        .GetConnectionParameters()
        .map(|c| {
            format!(
                "interval_ms={:.2} latency={} timeout_ms={}",
                f64::from(c.ConnectionInterval().unwrap_or(0)) * 1.25,
                c.ConnectionLatency().unwrap_or(0),
                u32::from(c.LinkTimeout().unwrap_or(0)) * 10
            )
        })
        .unwrap_or_else(|_| "params=unavailable".into())
}
/// The PHY Windows reports for each direction now.
pub(crate) fn connection_phy(device: &BluetoothLEDevice) -> String {
    let side = |info: windows::core::Result<windows::Devices::Bluetooth::BluetoothLEConnectionPhyInfo>| {
        info.map(|i| {
            if i.IsUncoded2MPhy().unwrap_or(false) {
                "2M"
            } else if i.IsCodedPhy().unwrap_or(false) {
                "coded"
            } else if i.IsUncoded1MPhy().unwrap_or(false) {
                "1M"
            } else {
                "unknown"
            }
        })
        .unwrap_or("unavailable")
    };
    match device.GetConnectionPhy() {
        Ok(phy) => format!("tx_phy={} rx_phy={}", side(phy.TransmitInfo()), side(phy.ReceiveInfo())),
        Err(_) => "phy=unavailable".into(),
    }
}

/// Every primary service Windows can see (diagnostic, after a failed query).
fn all_services(device: &BluetoothLEDevice, deadline: Instant, cancelled: &ConnectCancel) -> String {
    let result = (|| -> Result<String, Error> {
        let services = cancelled.wait(
            device.GetGattServicesWithCacheModeAsync(BluetoothCacheMode::Uncached)?,
            deadline,
        )?;
        let status = services.Status()?.0;
        let list = services.Services()?;
        let mut uuids = Vec::new();
        for service in &list {
            uuids.push(format!("{:?}", service.Uuid()?));
            let _ = service.Close();
        }
        Ok(format!("status={status} count={} uuids=[{}]", uuids.len(), uuids.join(",")))
    })();
    result.unwrap_or_else(|error| format!("failed={error:?}"))
}

pub struct Central {
    writer: Writer,
    io: CentralIo,
    paired: Option<bool>,
    /// WinRT link events stay registered for the link's life (stack facts).
    events: Events,
    _apartment: Apartment,
}
struct CentralIo {
    device: BluetoothLEDevice,
    service: Option<GattDeviceService>,
    session: Option<GattSession>,
    write: Option<GattCharacteristic>,
    notify: Option<GattCharacteristic>,
    value_token: Option<i64>,
    status_token: Option<i64>,
    pipe: Pipe,
    cancelled: ConnectCancel,
    with_response: bool,
    started: Instant,
    pacer: Pacer,
    native_error: Option<Error>,
    /// Windows 11: the throughput-optimized interval request, held for the
    /// link's life (closing it withdraws the request).
    preferred: Option<BluetoothLEPreferredConnectionParametersRequest>,
    /// The preset last requested ("none" before any request).
    preset: &'static str,
    /// CF-LINK-23: when to read the granted parameters back, and whether
    /// the one re-request was already made.
    preset_check: Option<(Instant, bool)>,
}

impl Central {
    /// Explicit worker action only. No pairing, access request or protocol
    /// fallback. The only retry is service discovery while the link is still
    /// coming up (Unreachable), bounded by the connect budget.
    pub fn connect(route: &super::scan::Route, cancelled: Arc<AtomicBool>) -> Result<Self, Error> {
        Self::connect_traced(route, cancelled, &mut |_, _| {})
    }
    /// `connect` that gives up at `deadline` when it is sooner than the usual bound.
    pub fn connect_until(route: &super::scan::Route, cancelled: Arc<AtomicBool>, deadline: Instant) -> Result<Self, Error> {
        Self::connect_bounded(route, cancelled, &mut |_, _| {}, Some(deadline))
    }
    /// `connect`, reporting each WinRT step as `(step, detail)`: statuses and
    /// timings only, never an address.
    pub fn connect_traced(
        route: &super::scan::Route,
        cancelled: Arc<AtomicBool>,
        trace: &mut dyn FnMut(&str, &str),
    ) -> Result<Self, Error> {
        Self::connect_bounded(route, cancelled, trace, None)
    }
    fn connect_bounded(
        route: &super::scan::Route,
        cancelled: Arc<AtomicBool>,
        trace: &mut dyn FnMut(&str, &str),
        until: Option<Instant>,
    ) -> Result<Self, Error> {
        let separate = std::env::var("MESHLIVE_GATT_CANCEL_OWNERSHIP").as_deref() == Ok("1");
        let cancelled = ConnectCancel::new(cancelled, separate);
        if cancelled.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let apartment = Apartment::new()?;
        let begun = Instant::now();
        let deadline = (begun + Duration::from_secs(CONNECT_SECONDS)).min(until.unwrap_or(Instant::now() + Duration::from_secs(CONNECT_SECONDS)));
        let ms = move || begun.elapsed().as_millis();
        probe().require(false)?;
        let device = cancelled.wait(
            BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(
                route.address,
                route.address_type,
            )?,
            deadline,
        )?;
        let mut io = CentralIo {
            device,
            service: None,
            session: None,
            write: None,
            notify: None,
            value_token: None,
            status_token: None,
            pipe: Pipe::for_link("client"),
            cancelled,
            with_response: false,
            started: Instant::now(),
            pacer: Pacer::default(),
            native_error: None,
            preferred: None,
            preset: "none",
            preset_check: None,
        };
        let mut events = Events::new(&io.device, begun);
        let paired = io
            .device
            .DeviceInformation()
            .and_then(|d| d.Pairing())
            .and_then(|p| p.IsPaired())
            .ok();
        trace(
            "le_device",
            &format!(
                "call=FromBluetoothAddressWithBluetoothAddressTypeAsync type={} connection={} paired={} protection={} ms={}",
                route.address_type.0,
                io.device.ConnectionStatus()?.0,
                // A bonded phone is reached by its resolved identity and the
                // link is encrypted with the stored key (stale keys drop it).
                paired.map_or("unknown".to_owned(), |p| p.to_string()),
                io.device
                    .DeviceInformation()
                    .and_then(|d| d.Pairing())
                    .and_then(|p| p.ProtectionLevel())
                    .map(|p| p.0.to_string())
                    .unwrap_or_else(|_| "unknown".into()),
                ms()
            ),
        );
        if paired == Some(true) {
            trace(
                "bonded_peer",
                "warning=this PC is paired with the phone at the Bluetooth level; the link is identity-bound even in stranger mode",
            );
        }
        let session = io.cancelled.wait(
            GattSession::FromDeviceIdAsync(&io.device.BluetoothDeviceId()?)?,
            deadline,
        )?;
        // Keep the device and session alive and ask Windows to hold the link
        // open before any discovery (an Uncached query on a link that is
        // still coming up reports Unreachable).
        session.SetMaintainConnection(true)?;
        trace(
            "gatt_session",
            &format!(
                "call=GattSession.FromDeviceIdAsync maintain=true status={} max_pdu={} ms={}",
                session.SessionStatus()?.0,
                session.MaxPduSize()?,
                ms()
            ),
        );
        events.watch(&session);
        io.session = Some(session);
        // Measured on three phones (win-userflow-20261019): MaintainConnection
        // alone did not open the link; the session went Active only after
        // the first GATT request (A15s: at 14.3 s, right after the 12 s idle
        // wait ended; ASUS: the request at 12 s came too late for the budget).
        // The short control grace only notes an already-active session.
        // The eager arm issues discovery without waiting for that grace.
        let active_grace = if std::env::var("MESHLIVE_GATT_EAGER_QUERY").as_deref() == Ok("1") { 0 } else { ACTIVE_GRACE_MS };
        let active_by = Instant::now() + Duration::from_millis(active_grace);
        loop {
            let session = io.session.as_ref().unwrap();
            if session.SessionStatus()? == GattSessionStatus::Active {
                events.drain(trace);
                trace(
                    "session_active",
                    &format!(
                        "connection={} max_pdu={} ms={}",
                        io.device.ConnectionStatus()?.0,
                        session.MaxPduSize()?,
                        ms()
                    ),
                );
                break;
            }
            if io.cancelled.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= active_by {
                // The first GATT request below brings the link up.
                trace(
                    "session_not_active",
                    &format!("connection={} ms={} next=services_request", io.device.ConnectionStatus()?.0, ms()),
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let discover_by = deadline;
        let mut attempt = 0;
        let mut last_status: GattCommunicationStatus;
        let available = loop {
            attempt += 1;
            let services = io.cancelled.wait(
                io.device
                    .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)?,
                deadline,
            )?;
            let available = services.Services()?;
            let status = services.Status()?;
            last_status = status;
            let count = available.Size()?;
            events.drain(trace);
            trace(
                "services",
                &format!(
                    "call=GetGattServicesForUuidWithCacheModeAsync(c8d5a105,Uncached) attempt={attempt} status={} count={count} session={} connection={} ms={}",
                    status.0,
                    io.session.as_ref().unwrap().SessionStatus()?.0,
                    io.device.ConnectionStatus()?.0,
                    ms()
                ),
            );
            if status == GattCommunicationStatus::Success && count == 1 {
                break Some(available);
            }
            for item in &available {
                let _ = item.Close();
            }
            // Only "not reachable yet" or "no service yet" is retried, until
            // the connect budget ends; a protocol error or access denial is final.
            let retry = status == GattCommunicationStatus::Unreachable
                || (status == GattCommunicationStatus::Success && count == 0);
            if !retry || Instant::now() + Duration::from_millis(RETRY_MS) >= discover_by {
                break None;
            }
            let until = Instant::now() + Duration::from_millis(RETRY_MS);
            while Instant::now() < until {
                if io.cancelled.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        };
        let Some(available) = available else {
            // One bounded diagnostic of what Windows can see, then the failure.
            let diagnostic_by = Instant::now() + Duration::from_secs(3);
            trace(
                "all_services",
                &format!(
                    "call=GetGattServicesWithCacheModeAsync(Uncached) {} ms={}",
                    all_services(&io.device, diagnostic_by, &io.cancelled),
                    ms()
                ),
            );
            events.drain(trace);
            return Err(if paired == Some(true) && last_status == GattCommunicationStatus::Unreachable {
                trace("bond_mismatch", "paired=true services=unreachable for the whole budget");
                Error::BondMismatch
            } else if last_status != GattCommunicationStatus::Success {
                Error::Gatt(last_status.0)
            } else {
                Error::Unsupported
            });
        };
        // Windows 11 (build 22000+) lets the central ask for one of three
        // presets (measured on the S22: Balanced 30-60 ms / 4 s supervision
        // timeout, ThroughputOptimized 15 ms / 2 s, PowerOptimized 90-180 ms
        // / 6 s; no custom values). Setup and voice use Balanced (Opus at
        // 16-32 kbps fits easily, 4 s timeout); a video call switches to
        // ThroughputOptimized (`request_preset`), accepting its 2 s timeout.
        let requested = BluetoothLEPreferredConnectionParameters::Balanced()
            .and_then(|p| io.device.RequestPreferredConnectionParameters(&p));
        let params = |device: &BluetoothLEDevice| {
            device
                .GetConnectionParameters()
                .map(|c| {
                    format!(
                        "interval_ms={:.2} latency={} timeout_ms={}",
                        f64::from(c.ConnectionInterval().unwrap_or(0)) * 1.25,
                        c.ConnectionLatency().unwrap_or(0),
                        u32::from(c.LinkTimeout().unwrap_or(0)) * 10
                    )
                })
                .unwrap_or_else(|_| "params=unavailable".into())
        };
        // Log the presets themselves: WinRT offers only these three (no custom
        // values), so a short supervision timeout after the request is the
        // preset's, not the phone's.
        for (name, preset) in [
            ("Balanced", BluetoothLEPreferredConnectionParameters::Balanced()),
            ("ThroughputOptimized", BluetoothLEPreferredConnectionParameters::ThroughputOptimized()),
            ("PowerOptimized", BluetoothLEPreferredConnectionParameters::PowerOptimized()),
        ] {
            if let Ok(p) = preset {
                trace(
                    "stack_fact",
                    &format!(
                        "fact=preferred_preset name={name} interval_ms={:.2}-{:.2} latency={} timeout_ms={}",
                        f64::from(p.MinConnectionInterval().unwrap_or(0)) * 1.25,
                        f64::from(p.MaxConnectionInterval().unwrap_or(0)) * 1.25,
                        p.ConnectionLatency().unwrap_or(0),
                        u32::from(p.LinkTimeout().unwrap_or(0)) * 10
                    ),
                );
            }
        }
        match requested {
            Ok(request) => {
                trace(
                    "preferred_params",
                    &format!(
                        "call=RequestPreferredConnectionParameters(Balanced) why=setup_and_voice status={} {} ms={}",
                        request.Status().map(|s| s.0).unwrap_or(-1),
                        params(&io.device),
                        ms()
                    ),
                );
                io.preferred = Some(request);
                io.preset = "Balanced";
            }
            Err(error) => trace(
                "preferred_params",
                &format!("unavailable hresult={:#x} {} ms={}", error.code().0, params(&io.device), ms()),
            ),
        }
        io.service = Some(available.GetAt(0)?);
        let service = io.service.as_ref().unwrap();
        let writes = io.cancelled.wait(
            service
                .GetCharacteristicsForUuidWithCacheModeAsync(WRITE, BluetoothCacheMode::Uncached)?,
            deadline,
        )?;
        let notifies = io.cancelled.wait(
            service.GetCharacteristicsForUuidWithCacheModeAsync(
                NOTIFY,
                BluetoothCacheMode::Uncached,
            )?,
            deadline,
        )?;
        events.drain(trace);
        trace(
            "characteristics",
            &format!(
                "write_status={} write_count={} notify_status={} notify_count={} ms={}",
                writes.Status()?.0,
                writes.Characteristics()?.Size()?,
                notifies.Status()?.0,
                notifies.Characteristics()?.Size()?,
                ms()
            ),
        );
        for status in [writes.Status()?, notifies.Status()?] {
            if status != GattCommunicationStatus::Success {
                return Err(Error::Gatt(status.0));
            }
        }
        if writes.Characteristics()?.Size()? != 1 || notifies.Characteristics()?.Size()? != 1 {
            return Err(Error::Unsupported);
        }
        let write = writes.Characteristics()?.GetAt(0)?;
        let notify = notifies.Characteristics()?.GetAt(0)?;
        let properties = write.CharacteristicProperties()?;
        if (properties
            & (GattCharacteristicProperties::Write
                | GattCharacteristicProperties::WriteWithoutResponse))
            .0
            == 0
            || (notify.CharacteristicProperties()? & GattCharacteristicProperties::Notify).0 == 0
        {
            return Err(Error::Unsupported);
        }
        io.with_response = (properties & GattCharacteristicProperties::WriteWithoutResponse).0 == 0;
        io.write = Some(write);
        io.notify = Some(notify);
        let pipe = io.pipe.clone();
        let stop = io.cancelled.clone();
        io.value_token = Some(
            io.notify
                .as_ref()
                .unwrap()
                .ValueChanged(&TypedEventHandler::<
                    GattCharacteristic,
                    GattValueChangedEventArgs,
                >::new(move |_, args| {
                    if stop.is_cancelled() {
                        return Ok(());
                    }
                    let received = args
                        .ok()
                        .and_then(|args| read_buffer(&args.CharacteristicValue()?, 512));
                    if received
                        .map_err(|_| carrier::Error::InvalidValue)
                        .and_then(|value| pipe.offer(&value))
                        .is_err()
                    {
                        stop.stop();
                        pipe.abort(carrier::Error::Backend);
                    }
                    Ok(())
                }))?,
        );
        let pipe = io.pipe.clone();
        let stop = io.cancelled.clone();
        io.status_token =
            Some(io.device.ConnectionStatusChanged(&TypedEventHandler::<
                BluetoothLEDevice,
                IInspectable,
            >::new(move |device, _| {
                if device.ok()?.ConnectionStatus()? == BluetoothConnectionStatus::Disconnected {
                    stop.stop();
                    pipe.abort(carrier::Error::Closed);
                }
                Ok(())
            }))?);
        let status = io.cancelled.wait(
            io.notify
                .as_ref()
                .unwrap()
                .WriteClientCharacteristicConfigurationDescriptorAsync(
                    GattClientCharacteristicConfigurationDescriptorValue::Notify,
                )?,
            deadline,
        )?;
        events.drain(trace);
        trace(
            "cccd",
            &format!(
                "call=WriteClientCharacteristicConfigurationDescriptorAsync(Notify) status={} with_response={} max_pdu={} ms={}",
                status.0,
                io.with_response,
                io.session.as_ref().unwrap().MaxPduSize()?,
                ms()
            ),
        );
        if status != GattCommunicationStatus::Success {
            return Err(Error::Gatt(status.0));
        }
        let central = Self {
            writer: Writer::default(),
            io,
            paired,
            events,
            _apartment: apartment,
        };
        for line in central.stack_facts() {
            trace("stack_fact", &line);
        }
        Ok(central)
    }

    /// Measured, not assumed: what Windows reports for this link now, one
    /// line per fact (granted connection parameters, PHY, ATT PDU size, the
    /// GATT session state under MaintainConnection, link status, bond).
    pub fn stack_facts(&self) -> Vec<String> {
        let session = self.io.session.as_ref();
        vec![
            format!("fact=connection_params {}", connection_params(&self.io.device)),
            format!("fact=phy {}", connection_phy(&self.io.device)),
            format!(
                "fact=max_pdu bytes={}",
                session.and_then(|s| s.MaxPduSize().ok()).map_or(-1, i32::from)
            ),
            format!(
                "fact=gatt_session maintain_connection={} status={} (1 active) can_maintain={}",
                session.and_then(|s| s.MaintainConnection().ok()).map_or("unknown".into(), |m| m.to_string()),
                session.and_then(|s| s.SessionStatus().ok()).map_or(-1, |s| s.0),
                session.and_then(|s| s.CanMaintainConnection().ok()).map_or("unknown".into(), |m| m.to_string()),
            ),
            format!(
                "fact=link connection_status={} (1 connected) paired={}",
                self.io.device.ConnectionStatus().map_or(-1, |s| s.0),
                self.paired.map_or("unknown".into(), |p| p.to_string())
            ),
        ]
    }
    /// Source experiment: wait for the peer's ATT response on every value,
    /// instead of treating a write-without-response OS enqueue as delivery.
    pub fn acknowledged_writes(&mut self) -> Result<(), Error> {
        let properties = self.io.write.as_ref().ok_or(Error::Unsupported)?.CharacteristicProperties()?;
        if (properties & GattCharacteristicProperties::Write).0 == 0 { return Err(Error::Unsupported); }
        self.io.with_response = true;
        Ok(())
    }
    /// Asks Windows for `ThroughputOptimized` (video) or `Balanced` (voice);
    /// the previous request is withdrawn. Logged with the reason and the
    /// parameters granted right after.
    pub fn request_preset(&mut self, video: bool, why: &str, trace: &mut dyn FnMut(&str, &str)) {
        let name = if video { "ThroughputOptimized" } else { "Balanced" };
        if self.io.preset == name {
            return;
        }
        if self.send_preset(video, why, trace) {
            self.io.preset_check = Some((Instant::now() + Duration::from_millis(500), false));
        }
    }
    /// CF-LINK-23: 500 ms after a request, read the granted parameters; if
    /// the request failed or a video link's interval is still above 30 ms,
    /// request once more (traced either way).
    pub fn recheck_preset(&mut self, trace: &mut dyn FnMut(&str, &str)) {
        let Some((at, retried)) = self.io.preset_check else { return };
        if Instant::now() < at {
            return;
        }
        self.io.preset_check = None;
        let video = self.io.preset == "ThroughputOptimized";
        let granted = self
            .io
            .preferred
            .as_ref()
            .and_then(|r| r.Status().ok())
            .is_some_and(|s| s == windows::Devices::Bluetooth::BluetoothLEPreferredConnectionParametersRequestStatus::Success);
        let interval_ms = self.io.device.GetConnectionParameters().ok().and_then(|c| c.ConnectionInterval().ok()).map(|i| f64::from(i) * 1.25);
        let ok = granted && (!video || interval_ms.is_some_and(|ms| ms <= 30.0));
        trace(
            "preferred_params_check",
            &format!("preset={} granted={granted} ok={ok} retried={retried} {}", self.io.preset, connection_params(&self.io.device)),
        );
        if !ok && !retried && self.send_preset(video, "recheck", trace) {
            self.io.preset_check = Some((Instant::now() + Duration::from_millis(500), true));
        }
    }
    fn send_preset(&mut self, video: bool, why: &str, trace: &mut dyn FnMut(&str, &str)) -> bool {
        let name = if video { "ThroughputOptimized" } else { "Balanced" };
        let preset = if video {
            BluetoothLEPreferredConnectionParameters::ThroughputOptimized()
        } else {
            BluetoothLEPreferredConnectionParameters::Balanced()
        };
        match preset.and_then(|p| self.io.device.RequestPreferredConnectionParameters(&p)) {
            Ok(request) => {
                if let Some(old) = self.io.preferred.replace(request) {
                    let _ = old.Close();
                }
                self.io.preset = name;
                trace(
                    "preferred_params",
                    &format!(
                        "call=RequestPreferredConnectionParameters({name}) why={why} status={} {}",
                        self.io.preferred.as_ref().and_then(|r| r.Status().ok()).map_or(-1, |s| s.0),
                        connection_params(&self.io.device)
                    ),
                );
                true
            }
            Err(error) => {
                trace("preferred_params", &format!("call={name} why={why} unavailable hresult={:#x}", error.code().0));
                false
            }
        }
    }
    /// For a lost link: the preset in force and the parameters Windows last
    /// reported (a drop under ThroughputOptimized's 2 s timeout is the
    /// suspected supervision timeout; WinRT reports no disconnect reason).
    pub fn loss_context(&self) -> String {
        let suspected = if self.io.preset == "ThroughputOptimized" { "supervision_timeout_2s" } else { "unknown" };
        format!(
            "preset={} suspected={suspected} {} connection={}",
            self.io.preset,
            connection_params(&self.io.device),
            self.io.device.ConnectionStatus().map_or(-1, |s| s.0)
        )
    }
    /// WinRT link events since the last call (parameter/PHY/PDU/session
    /// changes), for the owner's trace during the call.
    pub fn drain_events(&self, trace: &mut dyn FnMut(&str, &str)) {
        self.events.drain(trace);
        for (phase, line) in self.io.pipe.take_notes() {
            trace(phase, &line);
        }
    }
    /// GR1 on this link while the data writer is idle (no-op when off).
    pub fn service(&mut self) -> Result<(), carrier::Error> {
        let cancelled = self.io.cancelled.requested.clone();
        self.writer.service(&mut self.io, Instant::now() + Duration::from_secs(8), &cancelled)
    }

    /// Windows holds a Bluetooth pairing (bond) with this phone: the link is
    /// made to its identity address and encrypted with the bond key, so the
    /// phone's Bluetooth stack knows this PC even when the app protocol runs
    /// as a stranger. None when Windows could not say.
    pub fn paired(&self) -> Option<bool> {
        self.paired
    }
    /// Notification arrival times and sizes since the last call (CF-MEDIA-23).
    pub fn take_arrivals(&self) -> Vec<(Instant, usize)> {
        self.io.pipe.take_arrivals()
    }
    pub fn read(&self, output: &mut [u8], deadline: Instant) -> Result<usize, carrier::Error> {
        self.io.pipe.read(output, deadline, &self.io.cancelled.requested)
    }
    pub fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), carrier::Error> {
        let cancelled = self.io.cancelled.requested.clone();
        self.writer.write(bytes, &mut self.io, deadline, &cancelled)
    }
    pub fn native_error(&self) -> Option<Error> {
        self.io.native_error
    }
}

impl LinkIo for CentralIo {
    fn recovery_pipe(&self) -> Option<Pipe> {
        Some(self.pipe.clone())
    }
    fn capacity(&self) -> Result<usize, carrier::Error> {
        let mtu = self
            .session
            .as_ref()
            .ok_or(carrier::Error::Closed)?
            .MaxPduSize()
            .map_err(|_| carrier::Error::Backend)?;
        carrier::value_capacity(mtu)
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
        let mut send = || -> Result<(), Error> {
            let until = deadline.min(Instant::now() + Duration::from_secs(8));
            if self.with_response {
                let now = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                let ready = self
                    .pacer
                    .reserve(now, value.len(), 24000)
                    .map_err(|_| Error::Unsupported)?;
                let target = self.started + Duration::from_nanos(ready);
                while Instant::now() < target {
                    if cancelled.load(Ordering::Acquire) || self.cancelled.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if Instant::now() >= until {
                        return Err(Error::Timeout);
                    }
                    std::thread::sleep(
                        target
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(5)),
                    );
                }
            }
            let option = if self.with_response {
                GattWriteOption::WriteWithResponse
            } else {
                GattWriteOption::WriteWithoutResponse
            };
            if cancelled.load(Ordering::Acquire) || self.cancelled.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= until {
                return Err(Error::Timeout);
            }
            if value.len() > self.capacity().map_err(|_| Error::Unsupported)? {
                return Err(Error::Unsupported);
            }
            let status = wait_with(
                self.write
                    .as_ref()
                    .ok_or(Error::Unsupported)?
                    .WriteValueWithOptionAsync(&buffer(value)?, option)?,
                until,
                || cancelled.load(Ordering::Acquire) || self.cancelled.is_cancelled(),
            )?;
            if status != GattCommunicationStatus::Success {
                return Err(Error::Gatt(status.0));
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
        self.cancelled.stop();
        self.pipe.abort(carrier::Error::Backend);
        self.disconnect();
    }
}

impl CentralIo {
    fn disconnect(&mut self) {
        self.cancelled.stop();
        self.pipe.close();
        if let (Some(notify), Some(token)) = (&self.notify, self.value_token.take()) {
            let _ = notify.RemoveValueChanged(token);
        }
        if let Some(token) = self.status_token.take() {
            let _ = self.device.RemoveConnectionStatusChanged(token);
        }
        if let Some(request) = self.preferred.take() {
            let _ = request.Close();
        }
        if let Some(session) = &self.session {
            let _ = session.SetMaintainConnection(false);
        }
        if let Some(service) = self.service.take() {
            let _ = service.Close();
        }
        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }
        let _ = self.device.Close();
    }
}
impl Drop for CentralIo {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_connection_does_not_cancel_a_retry_but_the_user_still_can() {
        for separate in [false, true] {
            let requested = Arc::new(AtomicBool::new(false));
            let failed = ConnectCancel::new(requested.clone(), separate);
            let pipe = Pipe::default();
            failed.stop();
            pipe.abort(carrier::Error::Closed);
            let mut byte = [0];
            let error = pipe.read(&mut byte, Instant::now() + Duration::from_secs(1), &requested).unwrap_err();
            assert_eq!(error, carrier::Error::Closed, "the carrier preserves its concrete failure");
            assert_eq!(requested.load(Ordering::Acquire), !separate,
                "only the retained arm mistakes transport cleanup for the user's Cancel");
            let error = crate::dial::Error { stage: crate::dial::Stage::Connect, detail: "Timeout".into() };
            assert_eq!(crate::dial::retry_plan(&error, Duration::from_secs(2), requested.load(Ordering::Acquire)).is_some(), separate);
            assert!(crate::dial::retry_plan(&error, Duration::from_secs(20), false).is_none(), "no retry beyond the existing budget");
            let retry = ConnectCancel::new(requested.clone(), separate);
            assert_eq!(retry.is_cancelled(), !separate);
            failed.stop(); // an old connection's late callback must not cancel its replacement
            assert_eq!(retry.is_cancelled(), !separate);
            requested.store(true, Ordering::Release);
            assert!(retry.is_cancelled() && failed.is_cancelled());
            assert!(crate::dial::retry_plan(&error, Duration::from_secs(2), requested.load(Ordering::Acquire)).is_none());
            assert_eq!(Pipe::default().read(&mut byte, Instant::now() + Duration::from_secs(1), &requested), Err(carrier::Error::Cancelled));
        }
    }
    #[test]
    fn native_buffer_round_trips_are_owned_and_bounded() {
        let _apartment = Apartment::new().unwrap();
        for length in [1, 20, 244, 512] {
            let input = vec![7; length];
            let value = buffer(&input).unwrap();
            let mut first = read_buffer(&value, 512).unwrap();
            first.fill(9);
            assert_eq!(read_buffer(&value, 512).unwrap(), input);
        }
        assert!(read_buffer(&buffer(&[]).unwrap(), 512).is_err());
        assert!(read_buffer(&buffer(&[1; 513]).unwrap(), 512).is_err());
    }
}
