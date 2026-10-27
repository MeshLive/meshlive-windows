//! Design QW, the callee side shared by the app's Bluetooth owner and the
//! `answer_phone` tool: this PC's rotating call offer, the platform policy,
//! the announcement (`Listening`: start, follow the rotating ID with the
//! boundary silence and a new provider, report a subscribing phone) and the
//! answer (responder handshakes, the caller's Invite, the call session with
//! RV15 OPENING first or the REVEAL on Accept). Every step is traced with a
//! named phase; the caller decides how to ring and when to accept.
use crate::{
    bluetooth::{
        self,
        peripheral::{Form, Peripheral, status_name},
    },
    bmh2::SigningIdentity,
    bmh3::{guard::Guards, handshake::TagCheck, reveal},
    call::{Incoming, Message},
    call_session::{CallSession, CalleeReveal},
    crypto,
    dial::{self, Dialed, Link},
    identity::Identity,
    incoming::{self, Listener, TagOwner},
    opus, pc_announcement, presence, qw_policy,
    rotating::{self, OpeningEntry},
    verify_scan::Carrier,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub type Trace<'a> = &'a mut dyn FnMut(&str, &str);

/// This PC's call offer for one epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnOffer {
    pub epoch: u32,
    pub rdi: [u8; 6],
    pub payload: [u8; pc_announcement::LEN],
    /// Test hooks only (Design QV W1/W2): the kind 1 or 2 PC announcement
    /// of the same epoch, announced on its own provider next to the offer.
    pub extra: Option<[u8; pc_announcement::STREAM_LEN]>,
}

/// This PC's node ID (its persistent identity never changes in a process).
pub fn node_id() -> Option<[u8; 16]> {
    static NODE: std::sync::OnceLock<Option<[u8; 16]>> = std::sync::OnceLock::new();
    *NODE.get_or_init(|| {
        let (identity, _) = Identity::open_or_create().ok()?;
        let spki = SigningIdentity::public_key(&identity).ok()?;
        crypto::sha256(&spki).ok()?[..16].try_into().ok()
    })
}

/// The call offer from the presence keys (role 0 of `epoch`).
pub fn own_offer(state: &presence::State, epoch: u32) -> Option<OwnOffer> {
    let rdi = presence::own_rdi(&state.keys, &node_id()?, epoch, rotating::ROLE_CALL_OFFER)?;
    let caps = incoming::CAPABILITIES;
    let payload = pc_announcement::CallOffer { rdi, relay: false, consent: false, codecs: caps.encode_codecs & caps.decode_codecs & 7, cost: 0 }
        .encode()
        .ok()?;
    let extra = test_pc_kind().and_then(|kind| test_pc_payload(state, epoch, kind));
    Some(OwnOffer { epoch, rdi, payload, extra })
}

/// Test hooks only: `MESHLIVE_TEST_PC_ANNOUNCE=1|2` also announces a kind 1
/// (viewer request) or kind 2 (stream offer) PC announcement (Design QV
/// gates W1/W2: bytes on air and connectability; no media, no admission).
pub fn test_pc_kind() -> Option<u8> {
    #[cfg(feature = "test-hooks")]
    {
        match std::env::var("MESHLIVE_TEST_PC_ANNOUNCE").ok().as_deref().map(str::trim) {
            Some("1") => Some(pc_announcement::KIND_VIEWER_REQUEST),
            Some("2") => Some(pc_announcement::KIND_STREAM_OFFER),
            _ => None,
        }
    }
    #[cfg(not(feature = "test-hooks"))]
    None
}

/// Test hooks only: `MESHLIVE_TEST_PC_ANNOUNCE_RESTART_S=<10..900>` restarts
/// the kind 1/2 provider that often (a new provider, so a new address) to
/// observe address rotation without waiting for an epoch boundary.
pub fn test_pc_restart_every() -> Option<Duration> {
    #[cfg(feature = "test-hooks")]
    {
        let seconds: u64 = std::env::var("MESHLIVE_TEST_PC_ANNOUNCE_RESTART_S").ok()?.trim().parse().ok()?;
        Some(Duration::from_secs(seconds.clamp(10, 900)))
    }
    #[cfg(not(feature = "test-hooks"))]
    None
}

/// Test hooks only: `MESHLIVE_TEST_PC_ANNOUNCE_THIRD=1` keeps the first W1
/// arm (a third provider next to the call offer). Measured: that provider is
/// Aborted for its whole life on this adapter; kept as a test arm only.
pub fn test_pc_third_provider() -> bool {
    #[cfg(feature = "test-hooks")]
    {
        std::env::var("MESHLIVE_TEST_PC_ANNOUNCE_THIRD").ok().as_deref() == Some("1")
    }
    #[cfg(not(feature = "test-hooks"))]
    false
}

/// The fixed logical stream token of the W1/W2 test adverts (1..2^31-1).
const TEST_LOGICAL_TOKEN: u32 = 0x0051_7e57;

/// The kind 1 or 2 test payload for `epoch`: this PC's role-2 (request) or
/// role-1 (offer) RDI and a version 5 announcement that passes PROTOCOL's
/// strict decoding. The route token is `logical XOR mask(RDI)` (bit 31 set).
pub fn test_pc_payload(state: &presence::State, epoch: u32, kind: u8) -> Option<[u8; pc_announcement::STREAM_LEN]> {
    use crate::discovery::{ACCEPTING, Announcement, GATT, STREAM};
    let request = kind == pc_announcement::KIND_VIEWER_REQUEST;
    let role = if request { rotating::ROLE_VIEWER_REQUEST } else { rotating::ROLE_STREAM_OFFER };
    let rdi = presence::own_rdi(&state.keys, &node_id()?, epoch, role)?;
    let token = bluetooth::advertise::route_token(TEST_LOGICAL_TOKEN, &rdi).ok()?;
    let bitrate = bluetooth::advertise::rounded_bitrate(500);
    let inner = if request {
        Announcement { version: 5, prefix: rdi, flags: ACCEPTING | GATT, summary: 0, token, cost: 0, sequence: 0, width: 0, height: 0, fps: 0, bitrate, recovery: None }
    } else {
        // VP9 (codec bit 1), 640x360 at 15 FPS: an Android settings row.
        Announcement { version: 5, prefix: rdi, flags: STREAM | GATT, summary: 0x02, token, cost: 0, sequence: 0, width: 640, height: 360, fps: 15, bitrate, recovery: None }
    };
    pc_announcement::wrap(kind, &inner).ok()
}

/// The platform verdict (qw_policy), with the adapter and build traced.
pub fn policy(trace: Trace) -> Result<&'static str, &'static str> {
    let id = bluetooth::adapter_device_id();
    let verdict = qw_policy::current(id.as_deref());
    trace(
        "qw_policy",
        &format!("verdict={verdict:?} adapter={:?} build={:?}", id, qw_policy::os_build()),
    );
    verdict
}

/// The text after `MAX_DROPS` unexpected drops within `DROP_WINDOW`.
pub const DROPPED_TOO_OFTEN: &str = "Windows keeps stopping this PC's announcement, so phones can't call it. Turn Receive calls off and on to try again.";

/// What a `Listening::poll` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Waiting,
    /// A phone subscribed: call `answer`.
    Client,
    /// The announcement could not go back on air.
    Failed(String),
}

/// This PC announced and waiting for a phone.
pub struct Listening {
    pub peripheral: Peripheral,
    pub epoch: u32,
    pub rdi: [u8; 6],
    pub cancelled: Arc<AtomicBool>,
    /// This epoch's throwaway, presented to strangers.
    pub pseudonym: Identity,
    silent_until: Option<Instant>,
    checked: Instant,
    /// CF-DISC-20: the previous role-0 ID (epoch, RDI) and when it left the
    /// air; openable for `PREVIOUS_GRACE` after that, never longer.
    previous: Option<(u32, [u8; 6], Instant)>,
    /// Unexpected off-air drops restarted recently (`restart_after_drop`).
    drops: Vec<Instant>,
    /// When the announcement was first read off air (debounced, `confirm_off_air`).
    off_air_since: Option<Instant>,
    /// Test hooks only: when the kind 1/2 provider last started.
    extra_started: Option<Instant>,
    /// One announcement per PC, held for this listener's life.
    _lease: AnnouncementLease,
}

/// CF-DISC-20 `PREVIOUS_GRACE_MS`.
pub const PREVIOUS_GRACE: Duration = Duration::from_millis(60_000);

/// The epochs whose role-0 entries may be opened now: the one on air and,
/// within the grace after it left the air, the previous one (at most two).
pub fn openable_epochs(current: u32, previous: Option<(u32, Instant)>, now: Instant) -> Vec<u32> {
    let mut epochs = vec![current];
    if let Some((epoch, off_air)) = previous {
        if epoch != current && now.saturating_duration_since(off_air) < PREVIOUS_GRACE {
            epochs.push(epoch);
        }
    }
    epochs
}

/// What the user reads when the announcement can't start or stay on air
/// (pure; never a debug dump). `BluetoothError` values per the WinRT enum
/// (1 RadioNotAvailable, 2 ResourceInUse, 5 DisabledByPolicy,
/// 6 NotSupported, 7 DisabledByUser, 8 ConsentRequired).
pub fn user_text(error: &bluetooth::Error) -> String {
    use bluetooth::{Error, Radio};
    match error {
        Error::AdvertisingAborted | Error::AdvertisingIncomplete => bluetooth::gate::NO_PERIPHERAL.to_owned(),
        Error::Radio(Radio::Off) | Error::Bluetooth(1) => "Bluetooth is off. Turn it on in Windows settings so phones can call this PC.".to_owned(),
        Error::Radio(Radio::Missing) => "No Bluetooth adapter was found, so phones can't call this PC.".to_owned(),
        Error::Radio(Radio::Disabled) | Error::Bluetooth(5) => "Bluetooth is turned off on this PC by a policy, so phones can't call it.".to_owned(),
        Error::Radio(Radio::Denied) | Error::Bluetooth(7) | Error::Bluetooth(8) | Error::Native(ACCESS_DENIED) => {
            "Windows doesn't let this app use Bluetooth. Allow it in Windows privacy settings so phones can call this PC.".to_owned()
        }
        Error::Bluetooth(2) => "Another app is using this PC's Bluetooth announcement. Close it so phones can call this PC.".to_owned(),
        Error::Bluetooth(6) | Error::Unsupported => "This PC's Bluetooth can't announce itself to phones. Calling a phone still works.".to_owned(),
        Error::Timeout => "Windows didn't put this PC's announcement on air in time.".to_owned(),
        Error::Cancelled => "The announcement was stopped.".to_owned(),
        Error::Radio(_) => "Bluetooth status is unavailable, so phones can't call this PC right now.".to_owned(),
        Error::Bluetooth(code) | Error::Gatt(code) | Error::Native(code) | Error::ScanStopped(code) => {
            format!("This PC couldn't announce itself to phones (error {code:#x}).")
        }
        Error::BondMismatch => "This PC couldn't announce itself to phones.".to_owned(),
    }
}
const ACCESS_DENIED: i32 = 0x80070005u32 as i32;

/// The text when another MeshLive process already announces this PC.
pub const ANNOUNCED_ELSEWHERE: &str = "Another MeshLive window or tool is already announcing this PC, so this window can't receive calls. Close the other one; this window tries again every 30 seconds.";

/// One announcement per PC (the Windows session): a named mutex held while
/// a `Listening` lives, so a second app window or the answer_phone tool
/// never puts a second provider (a second address with the same rotating
/// ID) on air. Released on drop, and by Windows if the process dies.
pub struct AnnouncementLease(windows::Win32::Foundation::HANDLE);
// SAFETY: a mutex handle is a kernel object reference, usable from any thread.
unsafe impl Send for AnnouncementLease {}
impl AnnouncementLease {
    /// None when another holder exists (in this process or another).
    pub fn acquire() -> Option<Self> {
        use windows::Win32::{
            Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
        };
        // SAFETY: plain Win32 calls with a static name; the handle is owned here.
        unsafe {
            let handle = CreateMutexW(None, false, windows::core::w!("Local\\MeshLive.PcAnnouncement")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }
}
impl Drop for AnnouncementLease {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `acquire` and is closed once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// The announcement is on air (Started; an extended advert may report
/// StartedWithoutAllAdvertisementData with the full payload, gate 1).
pub fn on_air(status: i32) -> bool {
    status == 2 || status == 4
}

/// Windows itself took a started announcement off air (Stopped or Aborted).
/// Only these restart it: -1 (no provider: this app's own state) and
/// Created never do, so a restart cannot loop on a misread status (the
/// compact-only run restarted every second on -1 and changed address).
pub fn dropped(status: i32) -> bool {
    status == 1 || status == 3
}

/// Unexpected drops (Windows stopped or aborted the announcement outside a
/// boundary, e.g. after sleep or an adapter reset) are restarted at most
/// `MAX_DROPS` times within `DROP_WINDOW`; then the owner is told.
pub const DROP_WINDOW: Duration = Duration::from_secs(600);
pub const MAX_DROPS: usize = 3;

/// Records a drop at `now` and says whether to restart (pure).
/// A provider must read off air this long before it counts as dropped. The
/// adapter keeps one announcing provider on air; any other provider
/// starting or stopping (this app's or another's) flips the one on air to
/// Aborted for 10-40 ms (measured 2026-10-21, provider_capacity, 6 runs).
pub const OFF_AIR_CONFIRM: Duration = Duration::from_secs(1);

/// True once an off-air status first seen at `since` has lasted
/// `OFF_AIR_CONFIRM` (pure; the first sighting only starts the clock).
pub fn confirm_off_air(since: &mut Option<Instant>, now: Instant) -> bool {
    match since {
        None => {
            *since = Some(now);
            false
        }
        Some(at) => now.saturating_duration_since(*at) >= OFF_AIR_CONFIRM,
    }
}

/// The announcement this PC puts on air for `offer`: the call offer, or
/// (test hooks, Design QV W1/W2) the kind 1/2 form folded into the one
/// provider, the call offer paused, because this adapter keeps only one
/// announcing provider on air (a second one is Aborted for its whole life).
pub fn main_form(offer: &OwnOffer) -> Form {
    match offer.extra {
        Some(payload) if !test_pc_third_provider() => Form::PcTest(payload),
        _ => Form::PcAnnouncement(offer.payload),
    }
}

pub fn restart_after_drop(drops: &mut Vec<Instant>, now: Instant) -> bool {
    drops.retain(|at| now.saturating_duration_since(*at) < DROP_WINDOW);
    drops.push(now);
    drops.len() <= MAX_DROPS
}

impl Listening {
    /// Starts the data service and the announcement for `offer`.
    pub fn open(offer: &OwnOffer, trace: Trace) -> Result<Self, (Option<bluetooth::Error>, String)> {
        let Some(lease) = AnnouncementLease::acquire() else {
            trace("announce_refused", "reason=announced_elsewhere");
            return Err((None, ANNOUNCED_ELSEWHERE.to_owned()));
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut peripheral = Peripheral::create_pc(offer.payload, cancelled.clone()).map_err(|e| (Some(e), user_text(&e)))?;
        peripheral.set_form(main_form(offer));
        let started = peripheral.start();
        Self::trace_events(&peripheral, trace);
        started.map_err(|e| (Some(e), user_text(&e)))?;
        let (data, announcement) = peripheral.last_advertisement_status();
        Self::trace_alias(&peripheral, trace);
        trace(
            "announce_start",
            &format!(
                "epoch={} rdi={} payload={} data_status={data}({}) announcement_status={announcement}({}) qw_form={} connectable=true",
                offer.epoch,
                hex(&offer.rdi),
                hex(&offer.payload),
                status_name(data),
                status_name(announcement),
                if crate::bluetooth::advertise::qw_form_enabled() { "extended_1m+compact" } else { "off(compact_only)" }
            ),
        );
        let pseudonym = Identity::ephemeral().map_err(|e| (None, format!("identity {e:?}")))?;
        Ok(Self {
            peripheral,
            epoch: offer.epoch,
            rdi: offer.rdi,
            cancelled,
            pseudonym,
            silent_until: None,
            checked: Instant::now(),
            previous: None,
            drops: Vec::new(),
            off_air_since: None,
            extra_started: None,
            _lease: lease,
        })
        .map(|mut listening: Self| {
            listening.start_extra(offer, "open", trace);
            listening
        })
    }
    /// Test hooks only (Design QV W1/W2): (re)starts the kind 1/2 provider
    /// for `offer` and traces what Windows reports for every provider.
    /// WinRT exposes no advertising address: the raw observer records it.
    fn start_extra(&mut self, offer: &OwnOffer, reason: &str, trace: Trace) {
        let Some(payload) = offer.extra else { return };
        let third = test_pc_third_provider();
        let started = if third {
            self.peripheral.start_test_extra(payload)
        } else if reason == "periodic" {
            // Folded: the one provider restarts (a new address).
            self.peripheral.start_announcement(Form::PcTest(payload)).map(|()| self.peripheral.announcement_status())
        } else {
            // Folded: already started as the main form.
            Ok(self.peripheral.announcement_status())
        };
        Self::trace_events(&self.peripheral, trace);
        let common = format!(
            "reason={reason} mode={} call_offer={} kind={} epoch={} rdi={} len={} payload={} extended_1m=true connectable=true discoverable=false address=not_readable_on_windows",
            if third { "third_provider" } else { "fold" },
            if third { "up" } else { "paused" },
            payload[0] & 15,
            offer.epoch,
            hex(&payload[2..8]),
            payload.len(),
            hex(&payload)
        );
        match started {
            Ok(status) => trace("pc_test_announce", &format!("{common} status={status}({}) providers=[{}]", status_name(status), self.peripheral.provider_statuses())),
            Err(error) => trace("pc_test_announce_failed", &format!("{common} error={error:?} providers=[{}]", self.peripheral.provider_statuses())),
        }
        self.extra_started = Some(Instant::now());
    }
    /// The compact alias of the last start (MESHLIVE_COMPACT_OFFER).
    fn trace_alias(peripheral: &Peripheral, trace: Trace) {
        match peripheral.alias_result() {
            Some((uuid, Ok(status))) => trace(
                "announce_start",
                &format!(
                    "form=compact uuid={} status={status}({}) legacy=1 connectable=true discoverable={}",
                    bluetooth::advertise::uuid_text(&uuid),
                    status_name(status),
                    bluetooth::advertise::compact_discoverable()
                ),
            ),
            Some((uuid, Err(error))) => trace("announce_compact_failed", &format!("uuid={} error={error:?}", bluetooth::advertise::uuid_text(&uuid))),
            None => trace("announce_compact", "off"),
        }
    }
    fn trace_events(peripheral: &Peripheral, trace: Trace) {
        for (status, error) in peripheral.take_status_events() {
            trace("announce_status", &format!("status={status}({}) error={error}", status_name(status)));
        }
        for (status, error) in peripheral.take_test_events() {
            trace("pc_test_status", &format!("status={status}({}) error={error}", status_name(status)));
        }
        for line in peripheral.take_gatt_events() {
            trace("gatt_event", &line);
        }
    }
    /// Logs events, follows the rotating ID (`current`: this PC's offer
    /// now) and reports a subscribed phone.
    pub fn poll(&mut self, current: &mut dyn FnMut() -> Option<OwnOffer>, trace: Trace) -> Poll {
        Self::trace_events(&self.peripheral, trace);
        if self.peripheral.has_client() {
            return Poll::Client;
        }
        if let Some(until) = self.silent_until {
            if Instant::now() < until {
                return Poll::Waiting;
            }
        } else if self.checked.elapsed() < Duration::from_secs(1) {
            return Poll::Waiting;
        }
        self.checked = Instant::now();
        let Some(offer) = current() else { return Poll::Waiting };
        if self.silent_until.is_none() && offer.rdi == self.rdi {
            if let Some(every) = test_pc_restart_every() {
                if self.extra_started.is_some_and(|at| at.elapsed() >= every) {
                    self.start_extra(&offer, "periodic", trace);
                }
            }
        }
        if self.silent_until.is_none() {
            let status = self.peripheral.announcement_status();
            let off = offer.rdi == self.rdi && dropped(status);
            if !off {
                self.off_air_since = None;
            } else if !confirm_off_air(&mut self.off_air_since, Instant::now()) {
                // Measured 2026-10-21 (provider_capacity): a provider
                // starting or stopping anywhere on the adapter flips this
                // one to Aborted for 10-40 ms. Only a status that stays
                // off air is a drop; a blip never restarts the call offer.
                trace("announce_off_air_seen", &format!("status={status}({}) confirming", status_name(status)));
                return Poll::Waiting;
            }
            if off {
                self.off_air_since = None;
                // Windows took the announcement off air on its own (sleep,
                // adapter reset): a new provider for the same ID, a few times.
                if !restart_after_drop(&mut self.drops, Instant::now()) {
                    trace("announce_dropped", &format!("status={status}({}) drops={} giving_up", status_name(status), self.drops.len()));
                    return Poll::Failed(DROPPED_TOO_OFTEN.to_owned());
                }
                trace("announce_dropped", &format!("status={status}({}) drops={} restarting", status_name(status), self.drops.len()));
                let restarted = self.peripheral.start_announcement(main_form(&offer));
                Self::trace_events(&self.peripheral, trace);
                return match restarted {
                    Ok(()) => {
                        Self::trace_alias(&self.peripheral, trace);
                        trace("announce_restart", &format!("epoch={} rdi={} reason=dropped", offer.epoch, hex(&offer.rdi)));
                        self.start_extra(&offer, "dropped", trace);
                        Poll::Waiting
                    }
                    Err(error) => {
                        trace("announce_restart_failed", &format!("{error:?}"));
                        Poll::Failed(user_text(&error))
                    }
                };
            }
            if offer.rdi != self.rdi {
                // Off air for the boundary silence; the new ID then goes on
                // air from a new provider (a new address, gate 2).
                self.peripheral.stop_announcement();
                self.previous = Some((self.epoch, self.rdi, Instant::now()));
                let random = crypto::random_nonce().map_or(0, |n| u32::from_be_bytes([n[0], n[1], n[2], n[3]]));
                let silence = bluetooth::advertise::boundary_silence_ms(random);
                self.silent_until = Some(Instant::now() + Duration::from_millis(silence));
                trace("announce_boundary", &format!("silence_ms={silence} epoch={}->{} rdi={}->{}", self.epoch, offer.epoch, hex(&self.rdi), hex(&offer.rdi)));
            }
            return Poll::Waiting;
        }
        self.silent_until = None;
        let restarted = self.peripheral.start_announcement(main_form(&offer));
        Self::trace_events(&self.peripheral, trace);
        match restarted {
            Ok(()) => {
                self.epoch = offer.epoch;
                self.rdi = offer.rdi;
                if let Ok(pseudonym) = Identity::ephemeral() {
                    self.pseudonym = pseudonym;
                }
                let status = self.peripheral.announcement_status();
                Self::trace_alias(&self.peripheral, trace);
                trace("announce_restart", &format!("epoch={} rdi={} payload={} status={status}({})", offer.epoch, hex(&offer.rdi), hex(&offer.payload), status_name(status)));
                self.start_extra(&offer, "boundary", trace);
                Poll::Waiting
            }
            Err(error) => {
                trace("announce_restart_failed", &format!("{error:?}"));
                Poll::Failed(user_text(&error))
            }
        }
    }
    /// Stops the announcement and the data service.
    pub fn stop(mut self, trace: Trace) {
        self.cancelled.store(true, Ordering::Release);
        self.peripheral.close();
        Self::trace_events(&self.peripheral, trace);
        trace("announce_stop", &format!("epoch={}", self.epoch));
    }
}

/// An answered link: the session rings (Ringing queued) until the owner
/// accepts or declines.
pub struct Answer {
    pub dialed: Dialed,
    pub invite: Message,
    /// The caller proved its real key (a contact tag); else a stranger.
    pub verified: bool,
    pub peer_fingerprint: [u8; 32],
    /// The caller's verified name, when its labels came before the Invite.
    pub peer_name: Option<String>,
    /// The owner's screen declined this caller (CF-CALL-24): nothing of
    /// this PC's was queued; decline it silently.
    pub blocked: bool,
    pub busy: bool,
}

#[derive(Clone, Copy)]
pub struct Search {
    /// A rotating advert alone cannot prove the target's real identity.
    pub real_prefix: Option<[u8; 6]>,
}
impl Search {
    pub fn busy_for(&self, real: Option<[u8; 16]>) -> bool {
        !self.real_prefix.zip(real).is_some_and(|(target, node)| node[..6] == target)
    }
}

/// This PC's name goes at Ringing only to a real-key caller met with this
/// PC's real key, and never to a blocked caller (CF-REC-40, CF-CALL-24).
pub fn name_at_ringing(blocked: bool, own_pseudonymous: bool, peer_pseudonymous: bool) -> bool {
    !blocked && !own_pseudonymous && !peer_pseudonymous
}

/// What a subscribing phone turned out to be.
pub enum Inbound {
    Call(Answer),
    /// It scanned the code this PC shows (BVMID2 displayer side): the
    /// code is committed; run `verify_scan::ScanSession::displayer`.
    Scan {
        peripheral: Peripheral,
        reached: crate::verify_scan::Reached,
    },
}

/// Both responder handshakes, then for a call the caller's Invite and the
/// call session. `state`: the presence store (own day keys, lease
/// deliveries). `display`: the code this PC shows (its tag is accepted under
/// the single-use reservation). `calls`: Receive calls is on; without it only
/// a scan of the shown code is answered (every other link is closed before
/// any ring).
pub fn answer(
    listen: Listening,
    state: &presence::State,
    guards: &mut Guards,
    display: Option<&mut crate::qr_display::Display>,
    calls: bool,
    screen: &mut dyn FnMut(Option<[u8; 32]>) -> bool,
    trace: Trace,
) -> Result<Inbound, String> {
    answer_with_search(listen, state, guards, display, calls, None, screen, trace)
}

pub fn answer_with_search(
    listen: Listening, state: &presence::State, guards: &mut Guards,
    display: Option<&mut crate::qr_display::Display>, calls: bool,
    search: Option<Search>, screen: &mut dyn FnMut(Option<[u8; 32]>) -> bool, trace: Trace,
) -> Result<Inbound, String> {
    let (identity, _) = Identity::open_or_create().map_err(|e| format!("identity {e:?}"))?;
    answer_with_identity(listen, state, guards, display, calls, search, screen, identity, trace)
}

/// Same incoming path with an explicit identity. Native checks supply an
/// ephemeral identity and in-memory presence, never the installed app's key.
pub fn answer_with_identity(
    mut listen: Listening, state: &presence::State, guards: &mut Guards,
    mut display: Option<&mut crate::qr_display::Display>, calls: bool,
    search: Option<Search>, screen: &mut dyn FnMut(Option<[u8; 32]>) -> bool, identity: Identity, trace: Trace,
) -> Result<Inbound, String> {
    let started = Instant::now();
    Listening::trace_events(&listen.peripheral, trace);
    trace("incoming_subscriber", &format!("epoch={} rdi={}", listen.epoch, hex(&listen.rdi)));
    let day_key = *state.keys.get(&presence::day_of(listen.epoch)).ok_or("no day key")?;
    let public = identity.public_key().map_err(|e| format!("identity {e:?}"))?;
    let node = crypto::sha256(&public).map_err(|e| format!("identity {e:?}"))?[..16].try_into().unwrap();
    let entry = OpeningEntry::own(&day_key, &node, listen.epoch, rotating::ROLE_CALL_OFFER).map_err(|e| format!("{e:?}"))?;
    let delivered = |who: &[u8; 16]| state.contacts.iter().any(|c| &c.node_id == who && c.delivered_generation == state.generation);
    let (mut tags, mut owners) = (Vec::new(), Vec::new());
    let gate = display.as_deref_mut().map(crate::qr_display::Display::new_gate);
    if let Some((key, binding)) = display.as_deref_mut().and_then(crate::qr_display::Display::tag_check) {
        tags.push(TagCheck::new(key, binding));
        owners.push(TagOwner::Code);
    }
    if calls && state.contacts.iter().any(|c| c.delivered_generation == state.generation) {
        let key = rotating::contact_tag_key(&day_key).map_err(|e| format!("{e:?}"))?;
        tags.push(TagCheck::new(*key, rotating::advert_binding(&listen.rdi, rotating::ROLE_CALL_OFFER, None)));
        owners.push(TagOwner::Leased);
    }
    // CF-DISC-20: the previous ID too, while it is inside its 60 s grace
    // (a caller that sighted it just before the boundary). Pinned now, at
    // accept time; a contact that sighted it can use its tag as well.
    let mut entries = vec![entry];
    let previous = listen.previous.filter(|(epoch, _, at)| openable_epochs(listen.epoch, Some((*epoch, *at)), Instant::now()).len() == 2);
    if let Some((epoch, rdi, _)) = previous {
        if let Some(key) = state.keys.get(&presence::day_of(epoch)) {
            if let Ok(old) = OpeningEntry::own(key, &node, epoch, rotating::ROLE_CALL_OFFER) {
                entries.push(old);
                trace("incoming_previous_id", &format!("epoch={epoch} rdi={}", hex(&rdi)));
                if calls && state.contacts.iter().any(|c| c.delivered_generation == state.generation) {
                    if let Ok(old_key) = rotating::contact_tag_key(key) {
                        tags.push(TagCheck::new(*old_key, rotating::advert_binding(&rdi, rotating::ROLE_CALL_OFFER, None)));
                        owners.push(TagOwner::Leased);
                    }
                }
            }
        }
    }
    let listener = Listener { real: &identity, pseudonym: &listen.pseudonym, tags, owners, entries: entries.clone(), epoch: listen.epoch };
    let shown = match (display.as_deref_mut(), gate) {
        (Some(d), Some(g)) => Some((d, g)),
        _ => None,
    };
    let answered = incoming::respond(&mut listen.peripheral, &listener, guards, shown, &delivered, &listen.cancelled, &mut *trace);
    Listening::trace_events(&listen.peripheral, trace);
    let answered = match answered {
        Ok(answered) => answered,
        Err(e) => {
            // A failed connection releases the code (the attempt stays counted).
            if let (Some(d), Some(g)) = (display.as_deref_mut(), gate) {
                d.fail(g);
            }
            return Err(format!("handshake {e} after_ms={}", started.elapsed().as_millis()));
        }
    };
    trace(
        "incoming_handshake",
        &format!(
            "ms={} tagged={:?} own_pseudonym={} peer_pseudonym={} negotiated={}",
            started.elapsed().as_millis(),
            answered.tag,
            answered.own_pseudonymous,
            answered.peer_pseudonymous,
            answered.negotiated
        ),
    );
    if answered.tag == Some(TagOwner::Code) {
        trace("qr_committed", &format!("ms={} negotiated={}", started.elapsed().as_millis(), answered.negotiated));
        return Ok(Inbound::Scan {
            peripheral: listen.peripheral,
            reached: crate::verify_scan::Reached {
                link: answered.link,
                peer_fingerprint: answered.peer_fingerprint,
                peer_node: answered.peer_node,
                negotiated: answered.negotiated,
                peer_capabilities: answered.peer_capabilities,
                buffer: answered.buffer,
            },
        });
    }
    if !calls {
        return Err("not a scan of the shown code, and Receive calls is off: closed".into());
    }
    // The caller's Invite opens the call (heartbeats may come first).
    let mut link = answered.link;
    let mut buffer = answered.buffer;
    let mut early_labels: Option<Vec<u8>> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let invite = loop {
        let mut input = buffer.as_slice();
        let read = if crate::call::media_gate_enabled() {
            link.read_in_phase(&mut input, None)
        } else { link.read(&mut input) };
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        match read {
            Ok(Some(Incoming::Signal(message))) => break message,
            // The caller's labels come immediately before its Invite: held (one) and checked against it.
            Ok(Some(Incoming::Labels(bytes))) => early_labels = Some(bytes),
            Ok(Some(other)) => trace("incoming_before_invite", &format!("{:?}", std::mem::discriminant(&other))),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err("no invite within 10 s".into());
                }
                listen.peripheral.receive(&mut buffer, Duration::from_millis(50))?;
            }
            Err(error) => return Err(format!("before invite {error:?}")),
        }
    };
    trace("incoming_invite", &format!("call_id={} ms={}", hex(&invite.call_id), started.elapsed().as_millis()));
    let (header, video_plan) = crate::call_video::header_for(answered.peer_decode_codecs, &dial::encoders());
    let blocked = screen((!answered.peer_pseudonymous).then_some(answered.peer_fingerprint));
    let busy = !blocked && search.is_some_and(|s| s.busy_for((!answered.peer_pseudonymous).then_some(answered.peer_node)));
    let mut session = if busy {
        CallSession::<opus::Decoder>::incoming_busy(link, &invite, header, |c| opus::Decoder::new(c, opus::Profile::StreamOrCall), 0)
    } else {
        CallSession::<opus::Decoder>::incoming(link, &invite, header, |c| opus::Decoder::new(c, opus::Profile::StreamOrCall), 0).map(|(s, _)| s)
    }.map_err(|e| format!("invite {e:?}"))?;
    session.check_header_codecs(crate::video::decode_mask(), answered.peer_capabilities.encode_codecs);
    let mut pending_labels = Vec::new();
    // CF-CALL-24: the block decision comes before anything of this PC's is
    // queued (a blocked caller gets Ringing, then a Decline, and no label).
    if crate::labels::enabled() && answered.peer_capabilities.transport & crate::labels::CAPABILITY_BIT != 0 {
        session.accept_labels(answered.peer_node);
        if let Some(container) = early_labels.take() {
            let notices = session.on_labels(&container);
            trace("incoming_labels", &format!("before_invite kinds={:?}", notices.iter().map(|n| match n {
                crate::call_session::Notice::PeerLabel { kind, .. } => *kind,
                _ => 0,
            }).collect::<Vec<_>>()));
            pending_labels.extend(notices);
        }
        // A verified contact on both sides: this PC's name goes at Ringing.
        if !busy && name_at_ringing(blocked, answered.own_pseudonymous, answered.peer_pseudonymous) {
            if let Some(container) = crate::labels::own_call_container(&identity, &invite.call_id) {
                session.send_labels(container);
                trace("labels_sent", "kinds=[1] position=ringing");
            }
        }
    }
    if answered.own_pseudonymous {
        // A stranger met this PC's throwaway: the REVEAL goes out with Accept.
        let own = SigningIdentity::public_key(&listen.pseudonym).map_err(|e| format!("{e:?}"))?;
        let real = SigningIdentity::public_key(&identity).map_err(|e| format!("{e:?}"))?;
        let reveal = reveal::RevealSession::new(reveal::ROLE_CALLEE, &answered.exporter, own, answered.peer, true, answered.peer_pseudonymous, vec![own, real])
            .map_err(|e| format!("reveal {e:?}"))?;
        // Caller-reveal deadline: a direct link is 0 hops whatever the Invite says.
        let relay_hops = if answered.relayed { invite.relay_hops } else { 0 };
        session.reveal_on_accept(CalleeReveal { session: reveal, real: Box::new(identity), entries, relay_hops });
    } else {
        // RV15: a real-key callee opens the selected ID first.
        let body = rotating::encode_body(&entries).map_err(|e| format!("{e:?}"))?;
        session.send_discovery_first([b"BDX1".as_slice(), &[1], &body].concat());
    }
    let peer_real = (!answered.peer_pseudonymous).then_some((answered.peer_node, answered.peer_fingerprint));
    // CF-PLAT-05 on the callee side: a phone Windows is paired with is told
    // to the user (the call screen's bonded note), never paired by this app.
    let paired = listen.peripheral.client_paired() == Some(true);
    trace("incoming_bond", &format!("paired={paired}"));
    Ok(Inbound::Call(Answer {
        dialed: Dialed {
            link: Link::Peripheral(listen.peripheral),
            buffer,
            session,
            start: Instant::now(),
            peer_discovery: answered.negotiated,
            paired,
            header,
            video_plan,
            peer_real,
        },
        invite,
        verified: !answered.peer_pseudonymous,
        peer_fingerprint: answered.peer_fingerprint,
        blocked,
        busy,
        peer_name: pending_labels.into_iter().find_map(|n| match n {
            crate::call_session::Notice::PeerLabel { kind: crate::labels::KIND_NAME, text } => Some(text),
            _ => None,
        }),
    }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_authenticated_real_target_can_replace_a_pending_search() {
        let node = [7; 16];
        let search = Search { real_prefix: Some([7; 6]) };
        assert!(!search.busy_for(Some(node)));
        assert!(search.busy_for(Some([8; 16])));
        assert!(search.busy_for(None), "a throwaway never matches");
        assert!(Search { real_prefix: None }.busy_for(Some(node)), "a rotating ID alone proves no real target");
    }

    /// Regression (W1, 2026-10-21): another provider starting or stopping
    /// flips the call offer to Aborted for 10-40 ms; that blip restarted the
    /// call offer (new address) and three of them latched "dropped too often".
    #[test]
    fn a_short_off_air_blip_never_restarts_the_call_offer() {
        let t0 = Instant::now();
        let mut since = None;
        assert!(!confirm_off_air(&mut since, t0), "the first sighting only starts the clock");
        assert!(!confirm_off_air(&mut since, t0 + Duration::from_millis(40)));
        assert!(!confirm_off_air(&mut since, t0 + Duration::from_millis(999)));
        assert!(confirm_off_air(&mut since, t0 + OFF_AIR_CONFIRM), "still off air after a second: a real drop");
        // Back on air in between: the clock starts again.
        let mut since = None;
        assert!(!confirm_off_air(&mut since, t0));
        since = None;
        assert!(!confirm_off_air(&mut since, t0 + Duration::from_secs(5)));
    }

    /// The call offer is what goes on air unless a test-hooks build asked for
    /// a kind 1/2 test advert; a release build never builds one.
    #[test]
    fn the_call_offer_is_the_announcement_unless_a_test_advert_was_asked_for() {
        let offer = OwnOffer { epoch: 7, rdi: [1; 6], payload: [0x10, 1, 1, 1, 1, 1, 1, 1, 3], extra: None };
        assert_eq!(main_form(&offer), Form::PcAnnouncement(offer.payload));
        #[cfg(not(feature = "test-hooks"))]
        assert_eq!(test_pc_kind(), None, "no test advert outside test-hooks builds");
    }

    #[test]
    fn only_started_counts_as_on_air() {
        assert!(on_air(2) && on_air(4));
        for status in [-1, 0, 1, 3] {
            assert!(!on_air(status), "{status}");
        }
    }

    #[test]
    fn only_a_windows_stop_or_abort_restarts_the_announcement() {
        assert!(dropped(1) && dropped(3));
        for status in [-1, 0, 2, 4] {
            assert!(!dropped(status), "{status}: never restarted (address stays for the epoch)");
        }
    }

    #[test]
    fn unexpected_drops_are_restarted_a_few_times_per_window() {
        let start = Instant::now();
        let mut drops = Vec::new();
        for i in 0..MAX_DROPS {
            assert!(restart_after_drop(&mut drops, start + Duration::from_secs(i as u64)));
        }
        assert!(!restart_after_drop(&mut drops, start + Duration::from_secs(10)), "the fourth drop inside the window gives up");
        let mut drops = vec![start];
        assert!(restart_after_drop(&mut drops, start + DROP_WINDOW));
        assert_eq!(drops.len(), 1, "drops older than the window are forgotten");
    }

    #[test]
    fn announcement_failures_read_as_plain_text() {
        use bluetooth::{Error, Radio};
        assert_eq!(user_text(&Error::AdvertisingAborted), bluetooth::gate::NO_PERIPHERAL);
        assert!(user_text(&Error::Radio(Radio::Off)).contains("Bluetooth is off"));
        assert!(user_text(&Error::Bluetooth(1)).contains("Bluetooth is off"));
        assert!(user_text(&Error::Native(0x80070005u32 as i32)).contains("privacy settings"));
        assert!(user_text(&Error::Bluetooth(8)).contains("privacy settings"));
        assert!(user_text(&Error::Bluetooth(2)).contains("Another app"));
        assert!(user_text(&Error::Native(-2147023436)).contains("error 0x"));
        for error in [Error::Timeout, Error::Cancelled, Error::Unsupported, Error::Radio(Radio::Missing), Error::Gatt(3), Error::BondMismatch] {
            let text = user_text(&error);
            assert!(!text.contains('(') || text.contains("error 0x"), "no debug dump: {text}");
            assert!(text.ends_with('.'), "{text}");
        }
    }

    #[test]
    fn one_announcement_lease_per_pc() {
        // Skipped while a MeshLive app on this PC holds the real lease.
        let Some(first) = AnnouncementLease::acquire() else { return };
        assert!(AnnouncementLease::acquire().is_none(), "a second holder is refused");
        drop(first);
        assert!(AnnouncementLease::acquire().is_some(), "released on drop");
    }

    #[test]
    fn the_previous_id_is_openable_only_inside_its_grace() {
        let now = Instant::now();
        assert_eq!(openable_epochs(10, None, now), vec![10]);
        assert_eq!(openable_epochs(10, Some((9, now - Duration::from_secs(59))), now), vec![10, 9]);
        assert_eq!(openable_epochs(10, Some((9, now - Duration::from_secs(60))), now), vec![10]);
        assert_eq!(openable_epochs(10, Some((10, now)), now), vec![10], "never the same entry twice");
    }

        #[test]
    fn a_blocked_caller_never_gets_this_pcs_name() {
        assert!(name_at_ringing(false, false, false));
        assert!(!name_at_ringing(true, false, false), "blocked: no label at all");
        assert!(!name_at_ringing(false, true, false));
        assert!(!name_at_ringing(false, false, true));
        assert!(!name_at_ringing(true, true, true));
    }
}
