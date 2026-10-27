//! Experimental, off by default (`--bluetooth`): the real Bluetooth owner
//! behind the UI's `Backend` seam. Incoming calls (Design QW): with Receive
//! calls on, on an allowlisted platform (`qw_policy`), this PC announces its
//! rotating call offer (`pc_announcement`, connectable extended advertising),
//! restarts the announcement at every rotating-ID change after the boundary
//! silence, answers a subscribing phone with the responder handshakes
//! (`incoming::respond`), rings on its Invite and runs the same call loop.
//! While a call exists the announcement is off (a second caller cannot reach
//! this PC: Busy before any handshake). Outgoing calls go over the same
//! path as the `call_phone` tool (`meshlive::dial`): a rescan lists nearby
//! version 5 (rotating ID) call offers; placing a call connects to the one
//! selected, speaks BMH3 as a stranger and runs a `CallSession` whose Accept
//! counts only after the callee's verified REVEAL.
//!
//! The call session is the authority; the UI's `CallModel` mirrors it. UI
//! signals are translated into session actions, and every authenticated peer
//! signal the session applied is forwarded to the UI.
//!
//! Call audio (CF-OUT-16/17/37): received audio is decoded on the playout
//! clock and played on the default communications endpoint by the call-audio
//! thread (`call_audio`), with a ringback while the callee rings; the
//! microphone opens only when a call's media starts and closes with it. A video call
//! (or Add video) sends the first camera's pictures (`call_camera`, opened
//! with the video and closed with it). The default-off QV workers use these
//! capture/output owners for explicit Watch and Go live sessions too.
use crate::ui::{
    backend::{Backend, Command, Update},
    model::{Handle, MeetEvent, Person, Reach},
};
use meshlive::{
    bluetooth::scan::{ScanOptions, Scanner, Sighting},
    bvmid2::Code,
    call_presence::CallPresence,
    contacts::ContactStore,
    presence::Vault,
    verify_scan::{self, ScanSession, StoreHooks},
    call::{Message, Phase, Reason, Signal},
    call_session::Notice,
    call_video::CallVideo,
    crypto,
    dial::{self, Dialed},
    video::DecodeDevice,
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long one Rescan listens.
const SCAN_SECONDS: u64 = 10;
/// CF-DISC-29: a visible People list scans continuously (renewed hourly).
const CONTINUOUS_SCAN_SECONDS: u64 = 3_600;
/// CF-DISC-30 (Android `DISCOVERY_RESTART_MS`, `SCAN_ERROR_RETRY_MS`).
const SCAN_RESTART_MS: u64 = 500;
const SCAN_ERROR_RETRY_MS: u64 = 30_000;
/// CF-DISC-32 (Android `ScanRecoveryBackoff`): `min(3 s x n, 30 s)` for the
/// n-th consecutive scan failure; any result resets it.
const SCAN_BACKOFF_STEP_MS: u64 = 3_000;
/// Consecutive scan failures that no user setting explains before the screen
/// says only a Bluetooth restart helps (the back-off has reached its cap).
const SCAN_RESTART_AFTER: u32 = 10;

/// A failure that only turning Bluetooth off and on fixes: the back-off has
/// reached its cap on errors no user setting explains.
fn scan_needs_restart(error: &meshlive::bluetooth::Error, failures: u32) -> bool {
    failures >= SCAN_RESTART_AFTER && scan_blocker(error).is_none() && scan_retry_ms(error, failures).is_some()
}

/// CF-DISC-32: when to retry after the n-th consecutive scan failure. None:
/// not at all (this adapter can't scan; the note says so). A failure the user
/// must fix (policy, privacy, radio off) keeps the screen's 30 s retry so a
/// fix is picked up; every other failure backs off `min(3 s x n, 30 s)`.
fn scan_retry_ms(error: &meshlive::bluetooth::Error, failures: u32) -> Option<u64> {
    use meshlive::bluetooth::Error;
    match error {
        Error::ScanStopped(6) | Error::Bluetooth(6) | Error::Unsupported => None,
        _ if scan_blocker(error).is_some() => Some(SCAN_ERROR_RETRY_MS),
        _ => Some((SCAN_BACKOFF_STEP_MS * u64::from(failures.max(1))).min(SCAN_ERROR_RETRY_MS)),
    }
}
/// An unanswered outgoing call is cancelled after this when the callee never
/// said Ringing (setup cap, as call_phone).
const RING_SECONDS: u64 = 90;
/// PROTOCOL "Ring timeout" (design decision 2026-10-21, as Android): the caller
/// sends Cancel 60 s after the callee's Ringing; the callee shows "Missed
/// call" when that Cancel arrives, and its own safety stop sends
/// Decline/Rejected at 65 s (just after the caller's limit, for a caller
/// whose Cancel never arrives).
const CALLER_RING_SECONDS: u64 = 60;
const CALLEE_RING_SECONDS: u64 = 65;

/// Experimental parity completion; the existing 60/65 s limits stay active.
fn ring_parity() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_CALL_RING_PARITY").ok().as_deref() == Some("1"))
}

fn ring_timeout(phase: Phase, placed: Duration, ringing: Option<Duration>) -> Option<Signal> {
    match phase {
        Phase::IncomingRinging if placed >= Duration::from_secs(CALLEE_RING_SECONDS) => Some(Signal::Decline),
        Phase::OutgoingInvite | Phase::OutgoingRinging
            if ringing.is_some_and(|elapsed| elapsed >= Duration::from_secs(CALLER_RING_SECONDS))
                || placed >= Duration::from_secs(RING_SECONDS) => Some(Signal::Cancel),
        _ => None,
    }
}

fn missed_cancel(incoming: bool, media: bool, elapsed: Duration, message: &Message) -> bool {
    incoming && !media && message.signal == Signal::Cancel && elapsed >= Duration::from_secs(55)
}
/// Time for the last records (hang-up) to leave before the link is dropped.
const LINGER_MS: u64 = 300;

enum Job {
    Ui(Command),
    WipeBarrier(Sender<()>),
    /// Command::Call with the CSPRNG call ID the UI's model already uses and
    /// the flag that aborts the connection while it is being set up.
    Dial {
        person: Handle,
        voice: bool,
        call_id: [u8; 16],
        cancelled: Arc<AtomicBool>,
    },
    /// Command::CallContact: a verified contact by its recognised rotating ID.
    DialContact {
        node: [u8; 16],
        name: String,
        voice: bool,
        call_id: [u8; 16],
        cancelled: Arc<AtomicBool>,
    },
    Quit,
}

pub struct RadioBackend {
    jobs: Sender<Job>,
    updates: Receiver<Update>,
    worker: Option<JoinHandle<()>>,
    /// The ID handed to the UI by the last `new_call_id`.
    pending_id: Option<[u8; 16]>,
    /// Aborts the dial in progress (a Cancel/Hang up from the UI).
    dialing: Option<Arc<AtomicBool>>,
    /// Set by the worker once the dialled call link is up: from then on a
    /// Cancel/Hang up must reach the peer as a signal, so the dial's abort
    /// flag (which also stops every link write) is no longer raised
    /// (app call s22-app-video-1: the Hang up failed "Call: Cancelled" and
    /// the phone waited 2.5 s for the link to drop).
    link_up: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    /// Updates decided here without the worker.
    local: Vec<Update>,
    /// Add contact's "Scan their code" camera (Command::QrCamera).
    qr: Option<meshlive::qr_scan::QrScan>,
    watch_available: bool,
}

impl RadioBackend {
    /// No stream owner may recreate a file after the wipe deletes it.
    fn wipe_barrier(&mut self) {
        if let Some(qr) = self.qr.take() { qr.close_and_wait(); }
        if let Some(flag) = self.dialing.take() { flag.store(true, Ordering::Release); }
        let (ack, done) = mpsc::channel();
        if self.jobs.send(Job::WipeBarrier(ack)).is_err() || done.recv().is_err() {
            // A failed worker can drop its inbox before its stream fields.
            // Join its cleanup before allowing the decorator to delete files.
            if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        }
        self.watch_available = false;
    }
    pub fn start() -> Self {
        Self::start_with(meshlive::sealed::default_dir())
    }
    /// `dir`: the app's private folder with the verified contacts and the
    /// presence store (None: no contacts, nothing written; offline tests).
    pub fn start_with(dir: Option<std::path::PathBuf>) -> Self {
        // CF-HS-50: handshakes compare real keys with the app's verified contacts.
        meshlive::identity_gate::set_store_dir(dir.clone());
        let (jobs, inbox) = mpsc::channel();
        let (outbox, updates) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let flag = busy.clone();
        let link_up = Arc::new(AtomicBool::new(false));
        let up = link_up.clone();
        let worker = std::thread::Builder::new()
            .name("meshlive-bluetooth".into())
            .spawn(move || {
                let mut worker = Worker::new(inbox, outbox, flag, dir.as_deref().map(Stores::open));
                worker.link_up = up;
                worker.run()
            })
            .ok();
        Self {
            jobs,
            updates,
            worker,
            pending_id: None,
            dialing: None,
            busy,
            link_up,
            local: Vec::new(),
            qr: None,
            watch_available: false,
        }
    }
}
impl Drop for RadioBackend {
    fn drop(&mut self) {
        if let Some(qr) = self.qr.take() {
            qr.close_and_wait();
        }
        if let Some(flag) = &self.dialing {
            flag.store(true, Ordering::Release);
        }
        let _ = self.jobs.send(Job::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Backend for RadioBackend {
    /// Live navigation follows the default-off QV platform/policy gate.
    fn streams(&self) -> bool {
        self.watch_available
    }
    fn can_go_live(&self) -> bool { self.watch_available && meshlive::stream_source::enabled() }
    fn command(&mut self, command: Command) {
        let job = match command {
            Command::Wipe if meshlive::stream_watch::enabled() || meshlive::stream_source::enabled() => { self.wipe_barrier(); return; }
            Command::Call { person, voice } => {
                let Some(call_id) = self.pending_id.take() else {
                    // No CSPRNG ID was issued: never dial with a guessable one.
                    self.local.push(Update::CallLinkLost);
                    return;
                };
                let cancelled = Arc::new(AtomicBool::new(false));
                self.dialing = Some(cancelled.clone());
                Job::Dial {
                    person,
                    voice,
                    call_id,
                    cancelled,
                }
            }
            Command::CallContact { node, name, voice } => {
                let Some(call_id) = self.pending_id.take() else {
                    self.local.push(Update::CallLinkLost);
                    return;
                };
                let cancelled = Arc::new(AtomicBool::new(false));
                self.dialing = Some(cancelled.clone());
                Job::DialContact {
                    node,
                    name,
                    voice,
                    call_id,
                    cancelled,
                }
            }
            Command::CallSignal(message) => {
                if matches!(
                    message.signal,
                    Signal::Cancel | Signal::HangUp | Signal::Decline
                ) {
                    if let Some(flag) = self.dialing.take() {
                        if !self.link_up.load(Ordering::Acquire) {
                            flag.store(true, Ordering::Release);
                        }
                    }
                }
                Job::Ui(Command::CallSignal(message))
            }
            Command::QrCamera(on) => {
                if !on {
                    self.qr = None;
                } else if self.qr.is_none() {
                    trace("qr_camera", "open");
                    self.qr = Some(meshlive::qr_scan::QrScan::start(meshlive::qr_scan::default_decoder()));
                }
                return;
            }
            other => {
                if matches!(other, Command::ScanCode(_) | Command::CancelScan | Command::Wipe) && self.qr.take().is_some() {
                    trace("qr_camera", "closed by the scan step");
                }
                Job::Ui(other)
            }
        };
        let _ = self.jobs.send(job);
    }
    fn poll(&mut self) -> Vec<Update> {
        let events = self.qr.as_ref().map(|qr| qr.events()).unwrap_or_default();
        for event in events {
            let (update, scan, close) = qr_outcome(event);
            if close {
                self.qr = None;
            }
            self.local.extend(update);
            if let Some(text) = scan {
                // The same path as a typed or pasted code.
                let _ = self.jobs.send(Job::Ui(Command::ScanCode(text)));
            }
        }
        let mut out = std::mem::take(&mut self.local);
        out.extend(self.updates.try_iter());
        for update in &out { if let Update::WatchAvailable(ready) = update { self.watch_available = *ready; } }
        out
    }
    fn new_call_id(&mut self) -> [u8; 16] {
        match crypto::random_nonce() {
            Ok(bytes) => {
                let mut id = [0u8; 16];
                id.copy_from_slice(&bytes[..16]);
                self.pending_id = Some(id);
                id
            }
            Err(_) => {
                // The UI still gets an ID, but Command::Call will not dial.
                self.pending_id = None;
                [0u8; 16]
            }
        }
    }
    fn next_due(&self) -> Option<Duration> {
        // Updates come from the worker thread; poll often while it is busy.
        if self.busy.load(Ordering::Acquire) || self.qr.is_some() {
            Some(Duration::from_millis(50))
        } else {
            Some(Duration::from_millis(250))
        }
    }
}

/// What decides whether this PC is on air (Design QW).
#[derive(Clone, Copy, Debug, Default)]
struct Holds {
    receive_calls: bool,
    showing_code: bool,
    radio_off: bool,
    /// A call in any phase, or a blocked caller being declined. CF-CALL-04:
    /// a second caller then finds no announcement, and the admitted link is
    /// the only one the data service serves, so it is refused before any
    /// handshake and never sent BUSY.
    busy: bool,
    /// A contact verification (either side) holds the radio.
    verifying: bool,
}
/// Ok: announce; Err: stay off air with the note the user reads (pure).
fn announce_wanted(h: Holds) -> Result<(), Option<&'static str>> {
    if h.radio_off {
        Err(Some("Bluetooth is off, so phones can't call this PC."))
    } else if !h.receive_calls && !h.showing_code {
        Err(Some("Allow calls is off."))
    } else if h.busy || h.verifying {
        Err(None)
    } else {
        Ok(())
    }
}

/// CF-DISC-32: the text for a scan error only the user can fix (pure;
/// WinRT BluetoothError 5 DisabledByPolicy, 6 NotSupported, 7 DisabledByUser,
/// 8 ConsentRequired). Bluetooth off is the radio watcher's note; anything
/// else is retried.
fn scan_blocker(error: &meshlive::bluetooth::Error) -> Option<&'static str> {
    use meshlive::bluetooth::{Error, Radio};
    match error {
        Error::ScanStopped(5) | Error::Bluetooth(5) | Error::Radio(Radio::Disabled) => {
            Some("Bluetooth scanning is turned off on this PC by a policy, so people nearby can't be found.")
        }
        Error::ScanStopped(6) | Error::Bluetooth(6) | Error::Unsupported => Some("This PC's Bluetooth can't search for phones nearby."),
        Error::ScanStopped(7 | 8) | Error::Bluetooth(7 | 8) | Error::Radio(Radio::Denied) | Error::Native(-2147024891) => {
            Some("Windows doesn't let this app use Bluetooth. Allow it in Windows privacy settings to find people nearby.")
        }
        Error::Radio(Radio::Missing) => Some("No Bluetooth adapter was found, so people nearby can't be found."),
        _ => None,
    }
}

/// What a QR scan event does (pure): the update for the Add contact
/// screen, the code to scan, and whether the camera is now closed.
fn qr_outcome(event: meshlive::qr_scan::Event) -> (Option<Update>, Option<String>, bool) {
    use meshlive::qr_scan::Event;
    match event {
        Event::On => (None, None, false),
        Event::Unavailable { reason, privacy } => {
            trace("qr_camera", &format!("unavailable privacy={privacy}"));
            (Some(Update::Meet(MeetEvent::Failed(reason))), None, true)
        }
        Event::NotACode => (
            Some(Update::Meet(MeetEvent::Failed(
                "That QR code isn't a MeshLive code. Point the camera at the code on their Add contact screen.".into(),
            ))),
            None,
            false,
        ),
        Event::Code(text) => {
            trace("qr_camera", "code read; closed");
            (Some(Update::Meet(MeetEvent::Searching)), Some(text), true)
        }
    }
}

/// Diagnostic line with UTC milliseconds (user-flow timings: discovery,
/// connection, answer), only with MESHLIVE_TRACE set.
/// The trace sink: stderr with MESHLIVE_TRACE set, and the file
/// `trace.log` in the app folder when MESHLIVE_TRACE is set or a file named
/// `trace.on` exists there (a packaged app gets no environment variables from
/// the shell; its app folder is redirected into the package's LocalCache).
/// Native device diagnostics can include identifying descriptions. The private
/// diagnostics export omits raw trace lines; this local log is opt-in.
fn trace_sink() -> &'static (bool, bool, Option<std::sync::Mutex<std::fs::File>>) {
    static SINK: std::sync::OnceLock<(bool, bool, Option<std::sync::Mutex<std::fs::File>>)> = std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        let env = std::env::var_os("MESHLIVE_TRACE").is_some();
        let setup_only = std::env::var("MESHLIVE_SETUP_TIMINGS").as_deref() == Ok("1");
        // Unit tests never write into the user's real app folder.
        let file = meshlive::sealed::default_dir()
            .filter(|_| !cfg!(test))
            .filter(|dir| env || setup_only || dir.join("trace.on").exists())
            .and_then(|dir| {
                std::fs::create_dir_all(&dir).ok()?;
                std::fs::OpenOptions::new().create(true).append(true).open(dir.join("trace.log")).ok()
            })
            .map(std::sync::Mutex::new);
        (env, setup_only, file)
    })
}
fn trace_line(utc: u128, phase: &str, detail: &str, setup_only: bool) -> Option<String> {
    if setup_only {
        match phase {
            "dial_requested" | "dial_route_found" | "provider_stopping" | "provider_stopped" |
            "dial" | "resighted" | "gatt_connected" | "link_msg1" | "link_msg2" | "link_done" |
            "endpoint_msg1" | "endpoint_done" | "connected" | "ringing" | "dial_retry" |
            "dial_failed" | "accepted" | "answered" | "call_end" | "cancel" | "link_lost" |
            "le_device" | "gatt_session" | "session_active" | "session_not_active" |
            "services" | "characteristics" | "cccd" | "bond_mismatch" => {},
            _ => return None,
        }
        Some(format!("t={utc} bluetooth phase={phase}"))
    } else { Some(format!("t={utc} bluetooth phase={phase} {detail}")) }
}
pub(crate) fn trace(phase: &str, detail: &str) {
    let (env, setup_only, file) = trace_sink();
    if !*env && file.is_none() {
        return;
    }
    let utc = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let Some(line) = trace_line(utc, phase, detail, *setup_only) else { return };
    if *env {
        eprintln!("{line}");
    }
    if let Some(file) = file {
        use std::io::Write;
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

struct Offer {
    rdi: [u8; 6],
    sighting: Sighting,
}

/// How long a tap waits for a fresh sighting of the selected offer.
const RESIGHT_SECONDS: u64 = 10;

/// The same offer seen again: same 6 bytes and version, still a call offer.
fn same_offer(selected: &meshlive::discovery::Announcement, seen: &meshlive::discovery::Announcement) -> bool {
    seen.prefix == selected.prefix && seen.version == selected.version && dial::is_call_offer(seen)
}

/// A fresh sighting of `offer` within `RESIGHT_SECONDS`, or None.
use dial::{retry_plan, setup_failure_text};

/// The central's trace, also telling the UI once that this PC is paired
/// with the phone (the central's `bonded_peer` phase).
fn bonded_forwarder<'a>(outbox: &'a Sender<Update>, told: &'a mut bool) -> impl FnMut(&str, &str) + 'a {
    move |phase, detail| {
        trace(phase, detail);
        if phase == "bonded_peer" && !*told {
            *told = true;
            let _ = outbox.send(Update::CallBonded);
        }
    }
}


fn resight(offer: &Offer, cancelled: &AtomicBool) -> Option<Sighting> {
    resight_for(offer, cancelled, Duration::from_secs(RESIGHT_SECONDS))
}
fn resight_for(offer: &Offer, cancelled: &AtomicBool, wait: Duration) -> Option<Sighting> {
    let scanner = match Scanner::start(ScanOptions { rotating: true, compact: true }) {
        Ok(s) => s,
        Err(error) => {
            trace("resight_scan_failed", &format!("{error:?}"));
            return None;
        }
    };
    let until = Instant::now() + wait;
    loop {
        match scanner.receive(until, cancelled) {
            Ok(s) if same_offer(&offer.sighting.announcement, &s.announcement) => return Some(s),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// One visit of Add contact's "Your code" (BVMID2 displayer): the shown
/// code and its single-use reservation, the scanners this PC's user
/// confirmed during this visit (its own scans: yes with the shield stored),
/// and the reuse alert, which withholds every lease for the rest of the visit.
struct Visit {
    display: meshlive::qr_display::Display,
    fingerprint: [u8; 32],
    shown_rdi: Option<[u8; 6]>,
    confirmed: std::collections::HashSet<[u8; 32]>,
    reuse_alert: bool,
}
impl Visit {
    fn new(fingerprint: [u8; 32]) -> Self {
        let clock = Instant::now();
        // The global signing budget for code attempts within one visit.
        let mut spent = 0u32;
        Self {
            display: meshlive::qr_display::Display::new(
                Box::new(move || clock.elapsed().as_millis() as u64),
                Box::new(move || {
                    spent += 1;
                    spent <= 30
                }),
            ),
            fingerprint,
            shown_rdi: None,
            confirmed: Default::default(),
            reuse_alert: false,
        }
    }
}

/// A phone's scan of this PC's code, committed: the displayer session.
struct ShowScan {
    peripheral: meshlive::bluetooth::peripheral::Peripheral,
    session: verify_scan::ScanSession,
    buffer: Vec<u8>,
    clock: Instant,
    logged: usize,
    name: String,
    answering: bool,
}

fn call_capture_fence() -> Option<meshlive::stream_source::CaptureFence> {
    (std::env::var("MESHLIVE_CALL_CAPTURE_FENCE").as_deref() != Ok("0")).then(Default::default)
}

struct ActiveCall {
    dialed: Dialed,
    /// A phone called this PC (Design QW).
    incoming: bool,
    call_id: [u8; 16],
    /// Render, ringback and (only while media runs) the microphone.
    audio: meshlive::call_audio::CallAudio,
    /// Opus encoder for the microphone, from media start.
    encoder: Option<meshlive::opus::Encoder>,
    /// Microphone frames captured since media start (the audio clock).
    mic_frames: u32,
    /// The user muted the microphone (frames are dropped, the clock runs).
    mic_muted: bool,
    /// Optional fresh-device acknowledgement across Unmute; the control arm
    /// keeps the existing continuous capture behavior.
    capture_fence: Option<meshlive::stream_source::CaptureFence>,
    /// Play the ringback while the callee rings (setting).
    ringback: bool,
    /// When the callee's Ringing arrived (outgoing calls).
    ringing_at: Option<Instant>,
    /// CF-CALL-15: the call shows ACTIVE only once media started and the
    /// peer's stream header arrived (not at Accept alone).
    peer_header: bool,
    active_shown: bool,
    /// The camera, only while this call sends video (shared with the
    /// sender's picture source; dropped with the video or the call).
    camera: Option<std::rc::Rc<meshlive::call_camera::CallCamera>>,
    placed: Instant,
    media: bool,
    /// The local camera is on (StartMedia or Add video): send pictures.
    want_video: bool,
    video: Option<CallVideo<'static>>,
    /// The peer's video: discarded until its stream header arrives, then
    /// decoded by Media Foundation into `pictures` for the call screen.
    peer_video: Box<dyn meshlive::viewer::FrameDecoder>,
    /// The newest decoded peer picture, shared with the UI.
    pictures: std::sync::Arc<meshlive::video::PictureSlot>,
    /// Set once a final record (hang up, cancel, decline) is queued or the
    /// peer ended: the link is dropped after `LINGER_MS`.
    closing: Option<Instant>,
    /// The PC stays awake while the call exists (released with it).
    _awake: Option<meshlive::keep_awake::KeepAwake>,
    /// Phase 2 presence leases after the answer, judged on the real identity.
    presence: CallPresence,
    /// CF-MEDIA-52: one monotonic clock for this call's audio and video
    /// timestamps (set when media starts); audio is anchored at its first
    /// captured frame, video when its encoder opens.
    media_epoch: Option<Instant>,
    audio_base_us: Option<i64>,
    /// CF-PRES-19: presence inputs waiting for the task, the task itself,
    /// and whether the answer was handed to presence already.
    presence_inputs: Vec<PresenceInput>,
    presence_task: Option<PresenceTask>,
    presence_answered: bool,
    /// When `le_link` was last traced (peripheral calls).
    link_traced: Option<Instant>,
}

impl ActiveCall {
    fn stop_media(&mut self) {
        if self.audio.stop_now() {
            self.want_video = false;
            self.video = None;
            self.camera = None;
        }
    }
    /// Media starts (Accept sent or received): the ringback stops, the
    /// microphone opens (only now: never outside a call) and the Opus config
    /// goes first. False when the session refused.
    fn start_media(&mut self, camera: bool) -> bool {
        if self.dialed.session.media_ready().is_err() {
            return false;
        }
        trace(
            "answered",
            &format!("ring_ms={} camera={camera} gatt_value_max={}", self.placed.elapsed().as_millis(), meshlive::bluetooth::carrier::value_max()),
        );
        self.media = true;
        self.media_epoch.get_or_insert_with(Instant::now);
        self.want_video = camera;
        self.audio.ringback(false);
        self.audio.start_capture(None);
        match meshlive::opus::Encoder::new() {
            Ok(encoder) => {
                let config = encoder.config();
                let packet = meshlive::media::Packet { flags: meshlive::media::AUDIO | meshlive::media::CONFIG, pts_us: 0, payload: &config }.encode();
                if let Ok(packet) = packet {
                    let _ = self.dialed.session.send(packet);
                }
                self.encoder = Some(encoder);
            }
            Err(error) => trace("opus_encoder_failed", &format!("{error:?}")),
        }
        true
    }
    /// Plays what is due and sends what the microphone captured.
    fn pump_audio(&mut self, now: i64) -> Vec<Update> {
        if self.audio.stopped() { return Vec::new(); }
        let mut updates = Vec::new();
        let mut scratch = vec![0i16; meshlive::opus::Decoder::MAX_SAMPLES];
        let mut chunks: Vec<Vec<i16>> = Vec::new();
        if let Err(error) = self.dialed.session.audio().pump(now, 0, &mut scratch, |_, pcm| chunks.push(pcm.to_vec())) {
            trace("audio_receive_failed", &format!("{error:?}"));
        }
        for chunk in chunks {
            self.audio.play(chunk);
        }
        for event in self.audio.events() {
            match event {
                meshlive::call_audio::Event::Frame(mut frame) => {
                    let epoch = *self.media_epoch.get_or_insert_with(Instant::now);
                    let base = *self.audio_base_us.get_or_insert_with(|| epoch.elapsed().as_micros() as i64);
                    let pts = base + i64::from(self.mic_frames) * 20_000;
                    self.mic_frames += 1;
                    if self.mic_muted {
                        continue;
                    }
                    if let Some(fence) = self.capture_fence.as_ref() { fence.scrub(false, &mut frame[..]); }
                    if let Some(encoder) = self.encoder.as_mut() {
                        if let Ok(payload) = encoder.encode(&frame) {
                            if let Ok(packet) = (meshlive::media::Packet { flags: meshlive::media::AUDIO, pts_us: pts, payload: &payload }).encode() {
                                let _ = self.dialed.session.send(packet);
                            }
                        }
                    }
                }
                meshlive::call_audio::Event::CaptureRestarted(generation) => {
                    if let Some(fence) = self.capture_fence.as_mut() {
                        fence.acknowledged(generation);
                        if fence.ready() { updates.push(Update::CallCaptureReady); }
                    }
                }
                meshlive::call_audio::Event::MicOn(log) => {
                    for line in log {
                        trace("mic_probe", &line);
                    }
                    updates.push(Update::CallMic(None));
                }
                meshlive::call_audio::Event::MicUnavailable(reason) => {
                    trace("mic_unavailable", &reason);
                    updates.push(Update::CallMic(Some(mic_reason_text(&reason))));
                }
                meshlive::call_audio::Event::RenderUnavailable(reason) => trace("speaker_unavailable", &reason),
                meshlive::call_audio::Event::RenderOpened(detail) => trace("speaker_open", &detail),
                meshlive::call_audio::Event::RenderReopen => trace("speaker_reopen", "reason=write_timeout"),
                meshlive::call_audio::Event::Speed { speed, lag_ms, baseline_ms } => trace(
                    "speed",
                    &format!(
                        "speed={speed:.2} lag_ms={lag_ms:.0} baseline_ms={} error_ms={}",
                        baseline_ms.map_or("learning".into(), |b| format!("{b:.0}")),
                        baseline_ms.map_or("-".into(), |b| format!("{:.0}", lag_ms - b))
                    ),
                ),
            }
        }
        updates
    }
}

/// What the call screen says when no microphone can be used.
fn mic_reason_text(verdict: &str) -> String {
    if verdict.contains("Deny") {
        "Microphone access is off for desktop apps in Windows privacy settings, so the other person can't hear you.".into()
    } else if verdict.contains("muted") {
        "This PC's microphone is muted in Windows, so the other person can't hear you.".into()
    } else {
        "No microphone could be used, so the other person can't hear you.".into()
    }
}

/// The app's verified contacts and presence vault, as the Bluetooth owner
/// sees them (the contact layer has its own reader of the same file).
/// A blocked caller's call (CF-CALL-24): Ringing went out like any call; the
/// Decline follows after a random 3-7 s; nothing reaches the UI.
struct SilentDecline {
    dialed: dial::Dialed,
    decline_at: Instant,
    closing: Option<Instant>,
}

/// CF-PRES-19: presence work that may touch DPAPI and the disk (lease
/// delivery and storage) runs on its own thread, never on the worker that
/// reads and writes the call link. The inputs it consumes, in order:
enum PresenceInput {
    Identified([u8; 16], [u8; 32]),
    Record(Vec<u8>),
    Answered,
}
/// The stores and the call's presence state are moved into the task and
/// come back with the records to send.
type PresenceTask = std::thread::JoinHandle<(Stores, CallPresence, Vec<Vec<u8>>, Duration)>;
// The task needs both to be Send.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Stores>();
    send::<CallPresence>();
};

struct Stores {
    dir: std::path::PathBuf,
    blocks: meshlive::blocking::BlockList,
    contacts: ContactStore,
    vault: Vault,
    claims: meshlive::claims::ClaimsStore,
    /// The persisted monotonic epoch E (day keys follow it, never the raw wall clock).
    epoch: meshlive::epoch_store::EpochOwner,
}
impl Stores {
    fn open(dir: &std::path::Path) -> Self {
        Self {
            dir: dir.to_owned(),
            blocks: meshlive::blocking::BlockList::open_default(dir),
            contacts: ContactStore::open_default(dir),
            vault: Vault::open_default(dir),
            claims: meshlive::claims::ClaimsStore::open_default(dir),
            epoch: meshlive::epoch_store::EpochOwner::open(dir, meshlive::epoch_store::system_clocks().1),
        }
    }
    /// E from the persisted epoch clock; a forward-jump recovery starts a new
    /// presence generation (Android adoptGeneration).
    fn epoch(&mut self) -> u32 {
        let (elapsed, _) = meshlive::epoch_store::system_clocks();
        match self.epoch.read(wall_ms().max(0) as u64, elapsed) {
            Some(reading) => {
                if reading.new_generation && self.vault.ensure_loaded(reading.epoch) {
                    self.vault.bump(reading.epoch);
                    trace("epoch_new_generation", &format!("generation={}", self.epoch.generation()));
                }
                reading.epoch
            }
            None => dial::wall_epoch().unwrap_or(0),
        }
    }
    /// CF-PRES-09 (at start, before any presence work): records of node IDs
    /// that are no longer verified contacts go; skipped unless the verified
    /// store reads consistently.
    fn reconcile(&mut self) {
        let list = match self.contacts.list() {
            Ok(list) => list,
            Err(_) => {
                trace("presence_reconcile", "skipped=contacts_unreadable");
                return;
            }
        };
        if list.is_empty() && self.contacts.file_exists() {
            trace("presence_reconcile", "skipped=empty_list_with_file");
            return;
        }
        let epoch = self.epoch();
        if !self.vault.ensure_loaded(epoch) {
            trace("presence_reconcile", "skipped=presence_unreadable");
            return;
        }
        let verified: Vec<[u8; 16]> = list.iter().map(|c| c.node_id()).collect();
        let (dropped, revoked) = self.vault.reconcile(&verified, epoch);
        if dropped > 0 {
            trace("presence_reconcile", &format!("dropped={dropped} revoked={revoked}"));
        }
    }
    /// CF-PRES-04/25: once per epoch, roll the window and prune leases (one
    /// write, only when something changed).
    fn maintain(&mut self) -> Option<u32> {
        let epoch = self.epoch();
        if !self.vault.ensure_loaded(epoch) {
            return None;
        }
        let changed = self.vault.maintain(epoch);
        trace("presence_maintain", &format!("epoch={epoch} changed={changed}"));
        Some(epoch)
    }
    fn hooks(&mut self) -> StoreHooks<'_> {
        let epoch = self.epoch();
        self.vault.ensure_loaded(epoch);
        StoreHooks {
            contacts: &mut self.contacts,
            vault: &mut self.vault,
            claims: Some(&mut self.claims),
            epoch,
            now_ms: wall_ms(),
        }
    }
    /// The contact a sighted version 5 call offer belongs to.
    fn recognise(&mut self, rdi: &[u8; 6]) -> Option<[u8; 16]> {
        let epoch = self.epoch();
        if !self.vault.ensure_loaded(epoch) {
            return None;
        }
        match self.vault.recognise_call_offer(rdi, epoch, wall_ms()).as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }
    /// Contacts whose lease this PC holds.
    fn leased(&mut self) -> Vec<[u8; 16]> {
        let epoch = self.epoch();
        if !self.vault.ensure_loaded(epoch) {
            return Vec::new();
        }
        self.vault
            .state()
            .map(|s| s.contacts.iter().filter(|c| c.lease.is_some()).map(|c| c.node_id).collect())
            .unwrap_or_default()
    }
}

fn codec_name(codec: u8) -> &'static str {
    match codec {
        2 => "VP9",
        3 => "AV1",
        _ => "unknown",
    }
}
/// The encoder as a user reads it: hardware, or software (slower; a15s-r6
/// measured software AV1 freezing where hardware did not).
fn encoder_label(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.contains("libaom") || lower.contains("libvpx") || lower.contains("software") {
        format!("software encoder {name}")
    } else {
        format!("hardware encoder {name}")
    }
}

fn spawn_presence(mut stores: Stores, mut presence: CallPresence, inputs: Vec<PresenceInput>) -> PresenceTask {
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut out = Vec::new();
        for input in inputs {
            let mut hooks = stores.hooks();
            match input {
                PresenceInput::Identified(node, fingerprint) => out.extend(presence.identified(node, fingerprint, &mut hooks)),
                PresenceInput::Record(record) => out.extend(presence.on_record(&record, &mut hooks)),
                PresenceInput::Answered => out.extend(presence.answered(&mut hooks)),
            }
        }
        (stores, presence, out, started.elapsed())
    })
}

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Who a dial is for.
enum DialTarget {
    Stranger,
    Contact { node: [u8; 16] },
}

fn search_busy_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_SEARCH_BUSY").ok().as_deref() == Some("1"))
}

struct PendingDial {
    scanner: Scanner,
    target: DialTarget,
    rdi: Option<([u8; 6], u8)>,
    voice: bool,
    call_id: [u8; 16],
    cancelled: Arc<AtomicBool>,
    until: Instant,
}
impl PendingDial {
    fn admission(&self) -> meshlive::callee::Search {
        meshlive::callee::Search { real_prefix: match self.target {
            DialTarget::Contact { node } => Some(node[..6].try_into().unwrap()),
            DialTarget::Stranger => None,
        } }
    }
}

/// A BVMID2 scan in progress on the Add contact screen.
struct MeetScan {
    central: meshlive::bluetooth::central::Central,
    session: ScanSession,
    buffer: Vec<u8>,
    code: Code,
    clock: Instant,
    asked: String,
    answered: Option<(bool, String)>,
    logged: usize,
}

struct Worker {
    inbox: Receiver<Job>,
    outbox: Sender<Update>,
    busy: Arc<AtomicBool>,
    /// Shared with the front end: the dialled call's link is up.
    link_up: Arc<AtomicBool>,
    offers: HashMap<Handle, Offer>,
    next_handle: Handle,
    scan: Option<(Scanner, Instant)>,
    /// CF-DISC-29: the People list is on screen, so the scan runs
    /// continuously (restarted 500 ms after it ends, 30 s after an error,
    /// CF-DISC-30); hidden, it stops.
    people_visible: bool,
    scan_restart_at: Option<Instant>,
    /// CF-DISC-32: consecutive scan failures (reset by any result).
    scan_failures: u32,
    /// CF-PRIV-22: two addresses carrying one rotating ID at once.
    copied: meshlive::copied_id::CopiedIdTracker,
    /// Test-only (`test-hooks`): the stranger's rotating ID to dial once.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    test_stranger: Option<String>,
    /// CF-PRES-04: the epoch the presence store was last maintained for.
    maintained_epoch: Option<u32>,
    next_maintenance: Option<Instant>,
    /// When the current search started and whether it found anything yet.
    scan_started: Option<Instant>,
    call: Option<ActiveCall>,
    pending_dial: Option<PendingDial>,
    watching: Option<crate::stream_backend::Task>,
    source: Option<crate::source_backend::Task>,
    source_settings: meshlive::stream_source::Settings,
    source_consent: bool,
    stream_offers: meshlive::stream_watch::Offers,
    streams_visible: bool,
    watch_mask: u8,
    /// The encoder device, created on the first video call and kept for the
    /// worker's life (encoders borrow it).
    owner: Option<&'static DecodeDevice>,
    /// Bluetooth off/on (CF-LIFE-13/14): the radio's state edges.
    radio: Option<meshlive::bluetooth::radio_watch::RadioWatch>,
    epoch: meshlive::bluetooth::radio_watch::Epoch,
    born: Instant,
    /// A search was running when Bluetooth went off: restart it when it is back.
    scan_interrupted: bool,
    stores: Option<Stores>,
    /// Recognised contacts' current call offers, one per person (keyed by
    /// identity, so a rotating ID never duplicates a row).
    contact_offers: HashMap<[u8; 16], Offer>,
    meet: Option<MeetScan>,
    /// CF-DISC-39: rows go after 3 missed sightings at the measured cadence.
    cadence: meshlive::cadence::Cadence<[u8; 6]>,
    contact_cadence: meshlive::cadence::Cadence<[u8; 16]>,
    /// Design QW: announced and waiting for a phone (None: off air).
    listen: Option<meshlive::callee::Listening>,
    /// The Receive calls switch (UI default on).
    receive_calls: bool,
    /// The ringback setting (UI default on).
    ringback: bool,
    /// Bluetooth is off (CF-LIFE-13): nothing is announced.
    radio_off: bool,
    /// After a failed start, the next attempt.
    listen_retry_at: Option<Instant>,
    /// CF-PLAT-17: an announcement that failed to start (or a platform the
    /// policy refuses) stays off until the user asks again (Receive calls
    /// switched, Add contact opened, Bluetooth back on); no background retry.
    listen_latched: bool,
    /// "Block unknown callers" (CF-CALL-24, default off).
    block_unknown: bool,
    /// A blocked caller being declined silently (a call state: busy).
    silent: Option<SilentDecline>,
    /// What the UI was last told: (visible, note).
    visibility: Option<(bool, Option<String>)>,
    /// The generated name of the role-0 ID on air (last sent to the UI).
    own_generated: Option<String>,
    /// Responder replay guards (BMH3), for this process's life.
    guards: meshlive::bmh3::guard::Guards,
    /// Add contact's "Your code" is on screen.
    visit: Option<Visit>,
    /// CF-DISC-32: a scan error's note is shown (cleared by the next start).
    scan_note: bool,
    /// A phone's scan of the shown code, in its verification session.
    show: Option<ShowScan>,
}

impl Worker {
    fn refresh_watch_gate(&mut self) {
        if !meshlive::stream_watch::enabled() || self.stores.is_none() { return; }
        let caps = meshlive::bluetooth::probe();
        let ready = caps.require(true).is_ok() && caps.central == Some(true) && caps.extended_advertising == Some(true)
            && meshlive::callee::policy(&mut |_, _| {}).is_ok();
        if ready { self.watch_mask = meshlive::stream_watch::decode_mask(); }
        self.send(Update::WatchAvailable(ready));
    }

    fn start_watch(&mut self, id: Handle) {
        self.finish_presence_task();
        let selected = self.stream_offers.best(id, self.watch_mask).cloned();
        let allowed = meshlive::stream_watch::enabled() && !self.radio_off && self.call.is_none()
            && self.pending_dial.is_none() && self.watching.is_none() && self.source.is_none() && self.silent.is_none() && self.meet.is_none() && self.show.is_none();
        let selected = selected.filter(|s| s.received.elapsed() <= Duration::from_secs(10)
            && s.announcement.sequence & 0x80 == 0 && meshlive::stream_watch::unavailable(&s.announcement, self.watch_mask).is_none());
        let Some((selected, dir)) = selected.zip(self.stores.as_ref().map(|s| s.dir.clone())).filter(|_| allowed) else {
            self.send(Update::WatchNote(Some("This stream isn't available to watch right now.".into())));
            self.send(Update::Watch(crate::ui::model::WatchEvent::GaveUp));
            return;
        };
        self.stop_scan();
        if let Some(listen) = self.listen.take() { listen.stop(&mut |p, d| trace(p, d)); }
        self.set_visible(false, Some("Calls are paused while watching.".into()));
        self.watching = Some(crate::stream_backend::Task::watch(dir, selected, self.outbox.clone()));
    }

    fn start_source(&mut self, kind: crate::ui::model::LiveKind, title: String, initial: meshlive::stream_source::InitialMedia) {
        self.finish_presence_task();
        let consent = std::mem::take(&mut self.source_consent);
        let allowed = meshlive::stream_source::enabled() && meshlive::stream_watch::enabled() && consent && !self.radio_off
            && self.call.is_none() && self.pending_dial.is_none() && self.watching.is_none() && self.source.is_none()
            && self.silent.is_none() && self.meet.is_none() && self.show.is_none() && self.visit.is_none();
        let Some(dir) = self.stores.as_ref().map(|s| s.dir.clone()).filter(|_| allowed) else {
            self.send(Update::LiveNote(Some("Couldn't start. Close the current call or activity and try again.".into())));
            self.send(Update::Live(crate::ui::model::LiveEvent::Failed)); return;
        };
        self.stop_scan(); if let Some(listen) = self.listen.take() { listen.stop(&mut |p, d| trace(p, d)); }
        self.set_visible(false, Some("Calls are paused while live.".into()));
        self.source = Some(crate::source_backend::Task::start(dir, kind, title, self.source_settings, initial, self.outbox.clone()));
    }

    fn reload_stream_stores(&mut self) {
        if let Some(stores) = self.stores.as_ref() { self.stores = Some(Stores::open(&stores.dir)); }
        self.maintained_epoch = None;
        self.listen_retry_at = None;
    }

    fn new(inbox: Receiver<Job>, outbox: Sender<Update>, busy: Arc<AtomicBool>, stores: Option<Stores>) -> Self {
        Self {
            inbox,
            outbox,
            busy,
            link_up: Arc::new(AtomicBool::new(false)),
            offers: HashMap::new(),
            next_handle: 1,
            scan: None,
            people_visible: false,
            scan_restart_at: None,
            scan_failures: 0,
            copied: Default::default(),
            #[cfg(feature = "test-hooks")]
            test_stranger: crate::ui::test_hooks::stranger_target(),
            #[cfg(not(feature = "test-hooks"))]
            test_stranger: None,
            maintained_epoch: None,
            next_maintenance: None,
            scan_started: None,
            call: None,
            pending_dial: None,
            watching: None,
            source: None,
            source_settings: Default::default(),
            source_consent: false,
            stream_offers: Default::default(),
            streams_visible: false,
            watch_mask: 0,
            owner: None,
            radio: None,
            epoch: Default::default(),
            born: Instant::now(),
            scan_interrupted: false,
            stores,
            contact_offers: HashMap::new(),
            meet: None,
            cadence: Default::default(),
            contact_cadence: Default::default(),
            listen: None,
            receive_calls: true,
            ringback: true,
            radio_off: false,
            listen_retry_at: None,
            listen_latched: false,
            block_unknown: {
                #[cfg(feature = "test-hooks")]
                {
                    crate::ui::test_hooks::block_unknown()
                }
                #[cfg(not(feature = "test-hooks"))]
                {
                    false
                }
            },
            silent: None,
            visibility: None,
            own_generated: None,
            guards: Default::default(),
            visit: None,
            show: None,
            scan_note: false,
        }
    }
    /// Tells the contact layer who is nearby and whose lease this PC holds.
    fn publish_contacts(&mut self) {
        if self.stores.is_none() {
            return;
        }
        let nearby: Vec<[u8; 16]> = self.contact_offers.keys().copied().collect();
        let leased = self.stores.as_mut().map(Stores::leased).unwrap_or_default();
        self.send(Update::ContactsNearby { nearby, leased });
    }
    /// CF-LIFE-13/14/15: at off, stop the search (restarted when Bluetooth is
    /// back) and end a call with a plain reason; at the scheduled on, restore
    /// the search; three offs within 60 s ask the user to restart Bluetooth.
    /// The app never turns Bluetooth on or off itself.
    fn tick_radio(&mut self) {
        use meshlive::bluetooth::radio_watch::{CRASH_LOOP_GUIDANCE, Event};
        let now = self.born.elapsed().as_millis() as u64;
        let states: Vec<bool> = self.radio.as_ref().map_or_else(Vec::new, |r| r.states.try_iter().collect());
        let mut events = Vec::new();
        for on in states {
            let jitter = u64::from(meshlive::crypto::random_nonce().map_or(0, |n| n[0])) % 251;
            events.extend(self.epoch.on_state(on, now, jitter));
        }
        events.extend(self.epoch.poll(now));
        for event in events {
            match event {
                Event::Off => {
                    trace("bluetooth_off", "");
                    self.radio_off = true;
                    if meshlive::stream_watch::enabled() { self.send(Update::WatchAvailable(false)); }
                    if self.scan.is_some() {
                        self.scan_interrupted = true;
                        self.stop_scan();
                    }
                    if self.call.is_some() {
                        self.end_local(|s| s.hang_up(Reason::Error));
                        self.send(Update::CallSetupFailed("Call ended: Bluetooth was turned off".into()));
                    }
                    let note = if self.epoch.crash_loop(now) { CRASH_LOOP_GUIDANCE } else { "Turn Bluetooth on in Windows settings to find people and call." };
                    self.send(Update::BluetoothNote(Some(note.into())));
                }
                Event::On(epoch) => {
                    trace("bluetooth_on", &format!("epoch={epoch}"));
                    self.radio_off = false;
                    self.refresh_watch_gate();
                    self.listen_retry_at = None;
                    self.listen_latched = false;
                    self.send(Update::BluetoothNote(None));
                    if std::mem::take(&mut self.scan_interrupted) {
                        self.start_scan();
                    }
                }
            }
        }
    }
    fn send(&self, update: Update) {
        let _ = self.outbox.send(update);
    }
    fn run(mut self) {
        if let Some(stores) = self.stores.as_mut() {
            stores.reconcile();
        }
        if self.stores.is_some() {
            match meshlive::bluetooth::radio_watch::RadioWatch::start() {
                Ok(watch) => self.radio = Some(watch),
                Err(error) => trace("radio_watch_failed", &format!("{error:?}")),
            }
        }
        self.publish_contacts();
        self.refresh_watch_gate();
        loop {
            let active = self.call.is_some() || self.pending_dial.is_some() || self.scan.is_some() || self.meet.is_some() || self.show.is_some() || self.watching.is_some() || self.source.is_some();
            self.busy.store(active, Ordering::Release);
            let wait = if self.call.is_some() || self.silent.is_some() || self.meet.is_some() || self.show.is_some() {
                Duration::from_millis(1)
            } else if self.scan.is_some() || self.listen.is_some() || self.pending_dial.is_some() || self.watching.is_some() || self.source.is_some() {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(500)
            };
            match self.inbox.recv_timeout(wait) {
                Ok(Job::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(job) => self.on_job(job),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick_radio();
            if self.source.as_ref().is_some_and(|s| s.finished()) { self.source = None; self.reload_stream_stores(); }
            self.tick_maintenance();
            self.tick_scan();
            self.tick_call();
            self.tick_silent();
            self.tick_meet();
            self.tick_show();
            self.tick_listen();
            self.tick_pending_dial();
        }
        if let Some(call) = self.call.as_mut() {
            if call.closing.is_none() && call.dialed.session.phase() != Phase::Ended {
                let _ = call.dialed.session.hang_up(Reason::Normal);
                let _ = call.dialed.flush();
                std::thread::sleep(Duration::from_millis(LINGER_MS));
            }
        }
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::WipeBarrier(ack) => { self.on_job(Job::Ui(Command::Wipe)); let _ = ack.send(()); }
            Job::Ui(Command::StreamsVisible(visible)) => {
                self.streams_visible = visible;
                if visible { self.scan_restart_at = Some(Instant::now()); }
                else if !self.people_visible { self.stop_scan(); }
            }
            Job::Ui(Command::Watch(id)) => self.start_watch(id),
            Job::Ui(Command::StopWatching) => { self.watching = None; self.reload_stream_stores(); },
            Job::Ui(Command::MuteWatch(muted)) => if let Some(task) = self.watching.as_ref() { task.mute(muted); },
            Job::Ui(Command::ConfigureLive(settings)) => if self.source.is_none() && settings.valid() { self.source_settings = settings; },
            Job::Ui(Command::LiveExposureConsent) => self.source_consent = meshlive::stream_source::enabled(),
            Job::Ui(Command::GoLive { kind, title, initial }) => self.start_source(kind, title, initial),
            Job::Ui(Command::StopLive) => { self.source_consent = false; self.source = None; self.reload_stream_stores(); },
            Job::Ui(Command::PauseLive(value)) => if let Some(task) = self.source.as_ref() { task.control(crate::source_backend::Control::Pause(value)); },
            Job::Ui(Command::MuteLive(value)) => if let Some(task) = self.source.as_ref() { task.control(crate::source_backend::Control::Mute(value)); },
            Job::Ui(Command::LiveCamera(value)) => if let Some(task) = self.source.as_ref() { task.control(crate::source_backend::Control::Camera(value)); },
            Job::Ui(Command::Rescan) => self.start_scan(),
            Job::Ui(Command::PeopleVisible(visible)) => {
                self.people_visible = visible;
                if visible {
                    if self.scan.is_none() {
                        self.scan_restart_at = Some(Instant::now());
                    } else if let Some((_, until)) = self.scan.as_mut() {
                        *until = Instant::now() + Duration::from_secs(CONTINUOUS_SCAN_SECONDS);
                    }
                } else if !self.streams_visible {
                    self.scan_restart_at = None;
                    self.stop_scan();
                }
            }
            Job::Ui(Command::CallSignal(message)) => self.on_ui_signal(message),
            Job::Dial {
                person,
                voice,
                call_id,
                cancelled,
            } => self.dial(person, voice, call_id, cancelled),
            Job::DialContact {
                node,
                name,
                voice,
                call_id,
                cancelled,
            } => self.dial_contact(node, name, voice, call_id, cancelled),
            Job::Ui(Command::ScanCode(text)) => self.start_meet(&text),
            Job::Ui(Command::ConfirmScan { yes, name }) => {
                if let Some(meet) = self.meet.as_mut() {
                    meet.answered = Some((yes, name));
                }
            }
            Job::Ui(Command::CancelScan) => {
                // Leaving Add contact ends the visit: the code leaves the screen.
                self.meet = None;
                if let Some(mut visit) = self.visit.take() {
                    visit.display.leave();
                    trace("code_visit_end", "");
                }
                self.show = None;
            }
            Job::Ui(Command::Switches(switches)) => {
                self.receive_calls = switches.receive_calls;
                self.ringback = switches.ringback;
                self.block_unknown = switches.block_unknown;
                // Test-only: "Block unknown callers" for this run only (never saved).
                #[cfg(feature = "test-hooks")]
                if crate::ui::test_hooks::block_unknown() {
                    self.block_unknown = true;
                }
                self.listen_retry_at = None;
                self.listen_latched = false;
            }
            Job::Ui(Command::ShowOwnCode) => self.open_visit(),
            Job::Ui(Command::ResetPresenceKey) => {
                let reset = self.stores.as_mut().is_some_and(|s| s.vault.reset_damaged());
                trace("presence_reset", &format!("deleted={reset}"));
                if reset {
                    // The announcement tries again with a new generation.
                    self.listen_retry_at = None;
                    self.listen_latched = false;
                }
            }
            Job::Ui(Command::Wipe) => {
                // CF-PRIV-26: end everything and write nothing more.
                trace("wipe", "ending calls, scans and announcements; dropping stores");
                // The stores come back from a running presence task first.
                self.finish_presence_task();
                if let Some(call) = self.call.as_mut() {
                    let _ = call.dialed.session.hang_up(Reason::Normal);
                    let _ = call.dialed.flush();
                }
                self.call = None;
                self.watching = None;
                self.source = None;
                self.source_consent = false;
                self.pending_dial = None;
                self.meet = None;
                self.show = None;
                self.visit = None;
                if let Some(listen) = self.listen.take() {
                    listen.stop(&mut |p, d| trace(p, d));
                }
                self.stop_scan();
                self.receive_calls = false;
                self.stores = None;
            }
            // Remaining commands belong to the other backend decorators.
            Job::Ui(_) | Job::Quit => {}
        }
    }

    fn start_scan(&mut self) {
        if self.call.is_some() || self.watching.is_some() || self.source.is_some() {
            return;
        }
        self.scan = None;
        match Scanner::start(ScanOptions {
            rotating: true,
            compact: true,
        }) {
            Ok(scanner) => {
                self.offers.clear();
                self.contact_offers.clear();
                self.cadence.clear();
                self.contact_cadence.clear();
                self.publish_contacts();
                self.send(Update::People(Vec::new()));
                self.send(Update::Searching(true));
                // While People is on screen the scan is continuous (CF-DISC-29).
                let seconds = if self.people_visible || self.streams_visible { CONTINUOUS_SCAN_SECONDS } else { SCAN_SECONDS };
                self.scan = Some((scanner, Instant::now() + Duration::from_secs(seconds)));
                self.scan_started = Some(Instant::now());
                if std::mem::take(&mut self.scan_note) && !self.radio_off {
                    self.send(Update::BluetoothNote(None));
                }
                trace("search_start", "");
            }
            Err(error) => {
                self.scan_failures = self.scan_failures.saturating_add(1);
                let retry = scan_retry_ms(&error, self.scan_failures);
                trace("scan_failed", &format!("{error:?} failures={} retry_ms={retry:?}", self.scan_failures));
                self.note_scan_blocker(&error);
                self.send(Update::Searching(false));
                if self.people_visible {
                    self.scan_restart_at = retry.map(|ms| Instant::now() + Duration::from_millis(ms));
                }
            }
        }
    }
    fn stop_scan(&mut self) {
        if let Some((scanner, _)) = self.scan.as_ref() {
            trace("stack_fact", &scanner.facts());
        }
        if self.scan.take().is_some() {
            self.send(Update::Searching(false));
        }
    }
    fn tick_scan(&mut self) {
        // CF-DISC-29/30: keep the visible People list scanning.
        if self.scan.is_none()
            && (self.people_visible || self.streams_visible)
            && self.watching.is_none()
            && self.call.is_none()
            && self.silent.is_none()
            && !self.radio_off
            && self.scan_restart_at.is_some_and(|at| Instant::now() >= at)
        {
            self.scan_restart_at = None;
            trace("scan_restart", "people_visible=true");
            self.start_scan();
        }
        let Some((scanner, until)) = self.scan.as_ref() else {
            return;
        };
        let until = *until;
        let soon = (Instant::now() + Duration::from_millis(20)).min(until);
        let never = AtomicBool::new(false);
        let mut seen = Vec::new();
        let mut stopped = false;
        let mut failure = None;
        loop {
            match scanner.receive(soon, &never) {
                Ok(sighting) => {
                    // CF-DISC-32: any result resets the back-off.
                    self.scan_failures = 0;
                    if dial::is_call_offer(&sighting.announcement) {
                        seen.push(sighting);
                    } else if meshlive::stream_watch::enabled() && sighting.announcement.flags & meshlive::discovery::STREAM != 0 {
                        self.stream_offers.observe(sighting);
                    }
                }
                Err(meshlive::bluetooth::Error::Timeout) => break,
                Err(error) => {
                    trace("scan_stopped", &format!("{error:?}"));
                    failure = Some(error);
                    stopped = true;
                    break;
                }
            }
        }
        let mut changed = false;
        if self.streams_visible {
            self.stream_offers.prune(Instant::now());
            let rows = self.stream_offers.rows(self.watch_mask).into_iter().map(|(id, s)| {
                let a = &s.announcement;
                crate::ui::model::StreamEntry { id, from: own_generated_name(&a.prefix).unwrap_or_default(), title: None,
                    audio_only: a.summary == 0x40, reach: if a.cost == 0 { Reach::Direct } else { Reach::Via(a.cost) },
                    paused: a.flags & meshlive::discovery::PAUSED != 0, full: a.sequence & 0x80 != 0, confirmed: false,
                    unavailable: meshlive::stream_watch::unavailable(a, self.watch_mask) }
            }).collect();
            self.send(Update::Streams(rows));
        }
        let mut contacts_changed = false;
        let now_ms = self.born.elapsed().as_millis() as u64;
        for sighting in seen {
            self.cadence.observe(&sighting.announcement.prefix, now_ms);
            // CF-PRIV-22: before any per-address dedup; rotating IDs only.
            if sighting.announcement.version == 5 {
                let rdi = sighting.announcement.prefix;
                let was = self.copied.warned(&rdi);
                if self.copied.observe(rdi, sighting.route.address_key(), sighting.announcement.cost, now_ms) && !was {
                    trace("copied_id", "warning=on");
                    changed = true;
                }
            }
            // A recognised contact is one row keyed by identity, never a stranger row.
            let contact = if sighting.announcement.version == 5 {
                self.stores.as_mut().and_then(|s| s.recognise(&sighting.announcement.prefix))
            } else {
                None
            };
            match contact {
                Some(node) => {
                    self.contact_cadence.observe(&node, now_ms);
                    let rdi = sighting.announcement.prefix;
                    contacts_changed |= self.contact_offers.insert(node, Offer { rdi, sighting }).is_none();
                }
                None => changed |= self.on_offer(sighting),
            }
        }
        // Rows not heard for 3 measured gaps (4..12 s) leave the list.
        let stale: Vec<Handle> = self
            .offers
            .iter()
            .filter(|(_, o)| {
                let last = self.cadence.last_seen(&o.rdi).unwrap_or(0);
                !self.cadence.is_fresh(&o.rdi, last, now_ms)
            })
            .map(|(h, _)| *h)
            .collect();
        for handle in stale {
            if let Some(offer) = self.offers.remove(&handle) {
                trace("row_expired", &format!("window_ms={}", self.cadence.stale_after_ms(&offer.rdi)));
                self.cadence.forget(&offer.rdi);
                changed = true;
            }
        }
        let gone: Vec<[u8; 16]> = self
            .contact_offers
            .keys()
            .filter(|n| {
                let last = self.contact_cadence.last_seen(n).unwrap_or(0);
                !self.contact_cadence.is_fresh(n, last, now_ms)
            })
            .copied()
            .collect();
        for node in gone {
            self.contact_offers.remove(&node);
            trace("contact_row_expired", &format!("window_ms={}", self.contact_cadence.stale_after_ms(&node)));
            self.contact_cadence.forget(&node);
            contacts_changed = true;
        }
        if changed {
            self.send(Update::People(self.people()));
            #[cfg(feature = "test-hooks")]
            if let Some(target) = self.test_stranger.clone() {
                let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
                if let Some(handle) = self.offers.iter().find(|(_, o)| hex(&o.rdi) == target).map(|(h, _)| *h) {
                    self.test_stranger = None;
                    trace("test_hook", &format!("dial stranger rdi={target} person={handle}"));
                    self.send(Update::TestDial(handle));
                }
            }
        }
        if contacts_changed {
            self.publish_contacts();
        }
        let failure_kind = failure.clone();
        if let Some(error) = failure {
            self.note_scan_blocker(&error);
        }
        if stopped || Instant::now() >= until {
            self.stop_scan();
            if self.people_visible {
                // CF-DISC-30: 500 ms after a finished scan; after an error the
                // CF-DISC-32 back-off (or no retry on an adapter that can't scan).
                let wait = match &failure_kind {
                    None => Some(SCAN_RESTART_MS),
                    Some(error) => {
                        self.scan_failures = self.scan_failures.saturating_add(1);
                        scan_retry_ms(error, self.scan_failures)
                    }
                };
                self.scan_restart_at = wait.map(|ms| Instant::now() + Duration::from_millis(ms));
            }
        }
    }
    /// CF-DISC-32: a scan error the user must fix is shown (the retry stays
    /// at CF-DISC-30's 30 s, so a fix is picked up); others are only traced.
    fn note_scan_blocker(&mut self, error: &meshlive::bluetooth::Error) {
        if let Some(text) = scan_blocker(error) {
            trace("scan_blocked", text);
            self.scan_note = true;
            self.send(Update::BluetoothNote(Some(text.into())));
        } else if scan_needs_restart(error, self.scan_failures) {
            // Not an empty list with no reason: say a restart helps.
            trace("scan_blocked", &format!("restart failures={}", self.scan_failures));
            self.scan_note = true;
            self.send(Update::BluetoothNote(Some(meshlive::bluetooth::radio_watch::RESTART_GUIDANCE.into())));
        }
    }
    /// Records or refreshes a call offer; true when the list changed.
    fn on_offer(&mut self, sighting: Sighting) -> bool {
        let rdi = sighting.announcement.prefix;
        if let Some(offer) = self.offers.values_mut().find(|o| o.rdi == rdi) {
            offer.sighting = sighting;
            return false;
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.offers.insert(handle, Offer { rdi, sighting });
        if let Some(started) = self.scan_started.take() {
            trace("discovered", &format!("first_offer_ms={}", started.elapsed().as_millis()));
        }
        true
    }
    fn people(&self) -> Vec<Person> {
        let mut offers: Vec<(&Handle, &Offer)> = self.offers.iter().collect();
        offers.sort_by_key(|(handle, _)| **handle);
        // CF-PRIV-21/22: the generated name of each row's rotating ID, with
        // ID-derived suffixes when names collide.
        let named: Vec<(Vec<u8>, meshlive::names::StrangerName)> = offers
            .iter()
            .filter_map(|(_, offer)| {
                meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(&offer.rdi)).map(|n| (offer.rdi.to_vec(), n))
            })
            .collect();
        let labels = if named.len() == offers.len() { meshlive::names::disambiguate(&named) } else { Vec::new() };
        offers
            .iter()
            .enumerate()
            .map(|(i, (handle, offer))| Person {
                id: **handle,
                name: labels
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("Nearby phone {:02X}{:02X}", offer.rdi[0], offer.rdi[1])),
                reach: match offer.sighting.announcement.cost {
                    0 => Reach::Direct,
                    n => Reach::Via(n),
                },
                verified: false,
                copied: offer.sighting.announcement.version == 5 && self.copied.warned(&offer.rdi),
            })
            .collect()
    }

    fn dial(&mut self, person: Handle, voice: bool, call_id: [u8; 16], cancelled: Arc<AtomicBool>) {
        trace("dial_requested", "");
        self.stop_scan();
        if self.call.is_some() || self.watching.is_some() || self.source.is_some() {
            return;
        }
        if !search_busy_enabled() { self.stop_availability_for_dial(); }
        self.link_up.store(false, Ordering::Release);
        let Some(offer) = self.offers.remove(&person) else {
            trace("dial_failed", "person not in the last scan");
            self.send(Update::CallLinkLost);
            return;
        };
        if search_busy_enabled() {
            self.begin_pending_dial(DialTarget::Stranger, Some((offer.rdi, offer.sighting.announcement.version)), voice, call_id, cancelled);
        } else {
            self.dial_offer(offer, DialTarget::Stranger, voice, call_id, cancelled, false);
        }
    }

    /// CF-CALL-03: dialing first closes this PC's own availability (the
    /// announcement and its GATT server), so no caller can connect while the
    /// PC dials (CF-CALL-05 glare resolves by timing). `tick_listen` brings it
    /// back once no call exists.
    fn stop_availability_for_dial(&mut self) {
        if let Some(listen) = self.listen.take() {
            trace("announce_stop_reason", "dialing");
            trace("provider_stopping", "");
            listen.stop(&mut |p, d| trace(p, d));
            trace("provider_stopped", "");
            self.set_visible(false, Some("Not reachable while you are calling.".into()));
        }
    }

    /// CF-DISC-40 for contacts: the contact's offer from the current list, or
    /// a search of up to 10 s for an offer recognised as that contact
    /// ("Looking for X…" on the call screen; Cancel stops it).
    fn dial_contact(&mut self, node: [u8; 16], name: String, voice: bool, call_id: [u8; 16], cancelled: Arc<AtomicBool>) {
        trace("dial_requested", "");
        self.stop_scan();
        if self.call.is_some() || self.watching.is_some() || self.source.is_some() {
            return;
        }
        if search_busy_enabled() {
            self.send(Update::CallStatus(Some(format!("Looking for {name}\u{2026}"))));
            self.begin_pending_dial(DialTarget::Contact { node }, None, voice, call_id, cancelled);
            return;
        }
        self.stop_availability_for_dial();
        self.link_up.store(false, Ordering::Release);
        self.busy.store(true, Ordering::Release);
        let started = Instant::now();
        let offer = match self.contact_offers.remove(&node) {
            Some(offer) => Some(offer),
            None => {
                self.send(Update::CallStatus(Some(format!("Looking for {name}\u{2026}"))));
                self.seek_contact(&node, &cancelled)
            }
        };
        let Some(offer) = offer else {
            trace("dial_failed", &format!("contact not found nearby wait_ms={}", started.elapsed().as_millis()));
            if !cancelled.load(Ordering::Acquire) {
                self.send(Update::CallSetupFailed(format!(
                    "{name} wasn't found nearby. They may be out of range, in another call, or have MeshLive closed."
                )));
            }
            return;
        };
        trace("contact_found", &format!("wait_ms={}", started.elapsed().as_millis()));
        self.dial_offer(offer, DialTarget::Contact { node }, voice, call_id, cancelled, false);
    }
    fn seek_contact(&mut self, node: &[u8; 16], cancelled: &AtomicBool) -> Option<Offer> {
        let scanner = Scanner::start(ScanOptions { rotating: true, compact: true }).ok()?;
        let until = Instant::now() + Duration::from_secs(RESIGHT_SECONDS);
        loop {
            let s = scanner.receive(until, cancelled).ok()?;
            if s.announcement.version == 5
                && dial::is_call_offer(&s.announcement)
                && self.stores.as_mut().and_then(|st| st.recognise(&s.announcement.prefix)) == Some(*node)
            {
                return Some(Offer { rdi: s.announcement.prefix, sighting: s });
            }
        }
    }

    fn begin_pending_dial(&mut self, target: DialTarget, rdi: Option<([u8; 6], u8)>, voice: bool, call_id: [u8; 16], cancelled: Arc<AtomicBool>) {
        self.link_up.store(false, Ordering::Release);
        match Scanner::start(ScanOptions { rotating: true, compact: true }) {
            Ok(scanner) => self.pending_dial = Some(PendingDial {
                scanner, target, rdi, voice, call_id, cancelled,
                until: Instant::now() + Duration::from_secs(RESIGHT_SECONDS),
            }),
            Err(_) => self.send(Update::CallSetupFailed("Couldn't search for that phone. Check Bluetooth and try again.".into())),
        }
    }

    fn tick_pending_dial(&mut self) {
        let Some(pending) = self.pending_dial.take() else { return };
        if pending.cancelled.load(Ordering::Acquire) { return; }
        if self.radio_off || Instant::now() >= pending.until {
            self.send(Update::CallSetupFailed("That phone wasn't found nearby. Try again when it is ready.".into()));
            return;
        }
        match pending.scanner.receive(Instant::now() + Duration::from_millis(1), &pending.cancelled) {
            Ok(sighting) => {
                let matches = dial::is_call_offer(&sighting.announcement) && match pending.target {
                    DialTarget::Stranger => pending.rdi == Some((sighting.announcement.prefix, sighting.announcement.version)),
                    DialTarget::Contact { node } => sighting.announcement.version == 5
                        && self.stores.as_mut().and_then(|s| s.recognise(&sighting.announcement.prefix)) == Some(node),
                };
                if matches {
                    trace("dial_route_found", "");
                    self.stop_availability_for_dial();
                    let offer = Offer { rdi: sighting.announcement.prefix, sighting };
                    drop(pending.scanner);
                    self.dial_offer(offer, pending.target, pending.voice, pending.call_id, pending.cancelled, true);
                } else { self.pending_dial = Some(pending); }
            }
            Err(meshlive::bluetooth::Error::Timeout) => self.pending_dial = Some(pending),
            Err(_) => self.send(Update::CallSetupFailed("The search stopped. Check Bluetooth and try again.".into())),
        }
    }

    fn dial_offer(&mut self, offer: Offer, target: DialTarget, voice: bool, call_id: [u8; 16], cancelled: Arc<AtomicBool>, fresh: bool) {
        self.busy.store(true, Ordering::Release);
        trace("dial", &format!("voice={voice} contact={}", matches!(target, DialTarget::Contact { .. })));
        let dial_started = Instant::now();
        // CF-DISC-40: the listed address may be minutes old (a dead private
        // address, or a set the phone replaced). Wait up to 10 s for a fresh
        // sighting of the same RDI and version and dial that route.
        let sighting = if fresh { Some(offer.sighting) } else { resight(&offer, &cancelled) };
        let Some(sighting) = sighting else {
            trace("dial_failed", &format!("not found nearby wait_ms={}", dial_started.elapsed().as_millis()));
            if !cancelled.load(Ordering::Acquire) {
                self.send(Update::CallSetupFailed(setup_failure_text(None)));
            }
            return;
        };
        trace("resighted", &format!("wait_ms={}", dial_started.elapsed().as_millis()));
        let mut offer = Offer { rdi: offer.rdi, sighting };
        let first_dial = Instant::now();
        // CF-LINK-32: Windows knows it is paired with the phone as soon as it
        // resolves the device, before the GATT session: the call screen shows
        // the note while connecting, so a stale bond that drops the link at
        // encryption is explained even when the dial then fails.
        let outbox = self.outbox.clone();
        // The lease of a contact for the sighted ID (recomputed per attempt).
        let mut lease_for = |rdi: &[u8; 6]| -> Option<dial::ContactDial> {
            let DialTarget::Contact { node, .. } = &target else { return None };
            let stores = self.stores.as_mut()?;
            let epoch = stores.epoch();
            stores.vault.ensure_loaded(epoch);
            match stores.vault.lease_for(rdi, epoch, wall_ms()) {
                Some((contact, lease_key)) if contact == *node => Some(dial::ContactDial { contact, lease_key }),
                _ => None,
            }
        };
        let mut attempt = |offer: &Offer, cancelled: &Arc<AtomicBool>| {
            let mut told = false;
            let mut on_trace = bonded_forwarder(&outbox, &mut told);
            // Android's reveal rules: the caller reveals its persistent real
            // identity (after a stranger callee's REVEAL, or at once under a
            // contact tag), so contacts recognise this PC and leases can move.
            let identity = match meshlive::identity::Identity::open_or_create() {
                Ok((identity, _)) => identity,
                Err(error) => {
                    return Err(dial::Error { stage: dial::Stage::Link, detail: format!("identity {error:?}") });
                }
            };
            if offer.sighting.announcement.cost > 0 {
                let who = match &target {
                    DialTarget::Contact { .. } => Some(lease_for(&offer.rdi).ok_or(dial::Error { stage: dial::Stage::Connect, detail: "no lease for this ID".into() })?),
                    DialTarget::Stranger => None,
                };
                return dial::dial_relay(&offer.sighting.route, &offer.sighting.announcement, identity, who.as_ref(), call_id, voice, cancelled.clone(), &mut on_trace);
            }
            match &target {
                DialTarget::Stranger if dial::needs_bmh2(&offer.sighting.announcement) => dial::dial_bmh2(
                    &offer.sighting.route,
                    offer.rdi,
                    &identity,
                    call_id,
                    voice,
                    cancelled.clone(),
                    &mut on_trace,
                ),
                DialTarget::Stranger => dial::dial_selected(
                    &offer.sighting.route,
                    offer.rdi,
                    offer.sighting.announcement.version == 5,
                    Some(identity),
                    call_id,
                    voice,
                    cancelled.clone(),
                    &mut on_trace,
                ),
                DialTarget::Contact { .. } => match lease_for(&offer.rdi) {
                    Some(who) => dial::dial_contact(
                        &offer.sighting.route,
                        offer.rdi,
                        &who,
                        &identity,
                        call_id,
                        voice,
                        cancelled.clone(),
                        &mut on_trace,
                    ),
                    None => Err(dial::Error { stage: dial::Stage::Connect, detail: "no lease for this ID".into() }),
                },
            }
        };
        let mut result = attempt(&offer, &cancelled);
        if let Err(error) = &result
            && let Some(wait) = retry_plan(error, first_dial.elapsed(), cancelled.load(Ordering::Acquire))
        {
            trace("dial_retry", &format!("after={} rescan_ms={}", error, wait.as_millis()));
            // A brand-new link with fresh handshakes on a fresh sighting; the
            // failed attempt wrote no Invite (it fails before the call link).
            if let Some(sighting) = resight_for(&offer, &cancelled, wait) {
                offer = Offer { rdi: offer.rdi, sighting };
                result = attempt(&offer, &cancelled);
            }
        }
        match result {
            Ok(dialed) => {
                trace("connected", &format!("dial_ms={}", dial_started.elapsed().as_millis()));
                self.link_up.store(true, Ordering::Release);
                if dialed.paired {
                    // Usually already sent while connecting; the UI flag is idempotent.
                    self.send(Update::CallBonded);
                }
                self.call = Some(ActiveCall {
                    dialed,
                    incoming: false,
                    call_id,
                    audio: meshlive::call_audio::CallAudio::start_call(meshlive::call_audio::gain_from_env()),
                    encoder: None,
                    mic_frames: 0,
                    mic_muted: false,
                    capture_fence: call_capture_fence(),
                    ringback: self.ringback,
                    ringing_at: None,
                    peer_header: false,
                    active_shown: false,
                    camera: None,
                    placed: Instant::now(),
                    media: false,
                    want_video: false,
                    video: None,
                    peer_video: Box::new(meshlive::viewer::DiscardFrames::default()),
            pictures: Default::default(),
                    closing: None,
                    _awake: match meshlive::keep_awake::KeepAwake::new(meshlive::keep_awake::REASON) {
                        Ok(awake) => Some(awake),
                        Err(error) => {
                            trace("keep_awake_failed", &format!("{error:?}"));
                            None
                        }
                    },
                    presence: CallPresence::new(),
            media_epoch: None,
            audio_base_us: None,
            presence_inputs: Vec::new(),
            presence_task: None,
            presence_answered: false,
            link_traced: None,
                });
                self.send(Update::CallStatus(None));
                // A real-key peer is judged now; a pseudonym at its REVEAL.
                if let (Some(call), Some(stores)) = (self.call.as_mut(), self.stores.as_mut()) {
                    if let Some((node, fingerprint)) = call.dialed.peer_real {
                        for record in call.presence.identified(node, fingerprint, &mut stores.hooks()) {
                            call.dialed.session.send_discovery(record);
                        }
                    }
                }
                if cancelled.load(Ordering::Acquire) {
                    self.end_local(|s| s.cancel());
                }
            }
            Err(error) => {
                trace("dial_failed", &error.to_string());
                if !cancelled.load(Ordering::Acquire) {
                    self.send(Update::CallSetupFailed(setup_failure_text(Some(&error))));
                }
            }
        }
    }

    fn end_local(
        &mut self,
        action: impl FnOnce(&mut meshlive::call_session::CallSession<meshlive::opus::Decoder>) -> Result<(), meshlive::call_session::Error>,
    ) {
        if let Some(call) = self.call.as_mut() {
            if call.closing.is_none() {
                if let Err(error) = action(&mut call.dialed.session) {
                    trace("end_rejected", &format!("{error:?}"));
                }
                call.closing = Some(Instant::now());
                call.stop_media();
            }
        }
    }

    /// A signal the UI's call model produced.
    fn on_ui_signal(&mut self, message: Message) {
        let Some(call) = self.call.as_ref() else {
            return; // Invite/Camera off during the dial are already queued by the session.
        };
        if message.call_id != call.call_id {
            return;
        }
        let reason = message.reason;
        match message.signal {
            Signal::Cancel => return self.end_local(|s| s.cancel()),
            Signal::HangUp => return self.end_local(|s| s.hang_up(reason)),
            Signal::Decline => return self.end_local(|s| s.decline(reason)),
            _ => {}
        }
        let Some(call) = self.call.as_mut() else {
            return;
        };
        // Video added to a voice call: the link moves to ThroughputOptimized now.
        if message.signal == Signal::CameraOn {
            call.dialed.link.request_preset(true, "video_added", &mut |p, d| trace(p, d));
        }
        if message.signal == Signal::Accept && call.incoming {
            // The user answered: the session sends Accept (after this PC's
            // REVEAL in a call it answered with its throwaway).
            let voice = call.dialed.session.voice_offer();
            match call.dialed.session.accept(voice) {
                Ok(notices) => {
                    trace("accepted", &format!("voice={voice} ring_ms={}", call.placed.elapsed().as_millis()));
                    for notice in notices {
                        if let Notice::StartMedia { camera } = notice {
                            if call.start_media(camera) && call.peer_header && !call.active_shown {
                                call.active_shown = true;
                                let _ = self.outbox.send(Update::CallMediaReady);
                            }
                        }
                    }
                }
                Err(error) => trace("accept_rejected", &format!("{error:?}")),
            }
            return;
        }
        let session = &mut call.dialed.session;
        let result = match message.signal {
            // The session queued its own Invite (and Camera off) or Ringing
            // on creation; an incoming Accept is handled above.
            Signal::Invite | Signal::Ringing | Signal::Accept => Ok(()),
            Signal::CameraOff => {
                call.want_video = false;
                call.video = None;
                call.camera = None;
                session.set_camera(false)
            }
            Signal::CameraOn => {
                call.want_video = true;
                session.set_camera(true)
            }
            Signal::MicrophoneMuted => {
                call.mic_muted = true;
                if call.capture_fence.is_some() { session.discard_queued_audio(); }
                session.push_to_talk(false)
            }
            Signal::MicrophoneUnmuted => {
                if call.mic_muted {
                    if let Some(fence) = call.capture_fence.as_mut() {
                        let _ = self.outbox.send(Update::CallCaptureStarting);
                        call.audio.restart_capture(fence.restart());
                    }
                }
                call.mic_muted = false;
                session.push_to_talk(true)
            }
            Signal::Cancel | Signal::HangUp | Signal::Decline => Ok(()),
        };
        if let Err(error) = result {
            trace("ui_signal_rejected", &format!("{:?} {error:?}", message.signal));
        }
    }

    /// The silent decline: Ringing is already queued; the Decline goes out
    /// when due if the caller is still waiting; the caller's own Cancel or a
    /// lost link ends it at once.
    fn tick_silent(&mut self) {
        let Some(silent) = self.silent.as_mut() else { return };
        let mut done = false;
        if silent.dialed.flush().is_err() {
            done = true;
        }
        if let Some(at) = silent.closing {
            done |= at.elapsed() >= Duration::from_millis(LINGER_MS);
        } else if !done {
            match silent.dialed.pump_input(Duration::from_millis(5)) {
                Ok(notices) => done |= notices.iter().any(|n| matches!(n, Notice::Ended(_))),
                Err(_) => done = true,
            }
            if !done && Instant::now() >= silent.decline_at {
                if silent.dialed.session.phase() == Phase::IncomingRinging && silent.dialed.session.decline(Reason::Rejected).is_ok() {
                    let _ = silent.dialed.flush();
                    trace("incoming_blocked_declined", "reason=Rejected");
                    silent.closing = Some(Instant::now());
                } else {
                    done = true;
                }
            }
        }
        if done {
            trace("incoming_blocked_end", "");
            self.silent = None;
        }
    }
    /// Waits for a running presence task and restores the stores (call end,
    /// wipe); its records are dropped with the call.
    fn finish_presence_task(&mut self) {
        let Some(call) = self.call.as_mut() else { return };
        if let Some(task) = call.presence_task.take() {
            if let Ok((stores, presence, _, _)) = task.join() {
                self.stores = Some(stores);
                for line in &presence.log {
                    trace("presence", line);
                }
                call.presence = presence;
            }
        }
    }
    /// CF-PRES-04: a 10 s timer (also after sleep or Modern Standby) runs the
    /// presence maintenance once per epoch, never during a call (its disk
    /// work stays off the link worker, CF-PRES-19; it runs after the call).
    fn tick_maintenance(&mut self) {
        if self.call.is_some() || self.watching.is_some() || self.source.is_some() || self.next_maintenance.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.next_maintenance = Some(Instant::now() + Duration::from_secs(10));
        let Some(stores) = self.stores.as_mut() else { return };
        let epoch = stores.epoch();
        if self.maintained_epoch != Some(epoch) {
            self.maintained_epoch = stores.maintain();
        }
    }
    fn tick_call(&mut self) {
        let Some(call) = self.call.as_mut() else {
            return;
        };
        // Stage timing: a tick of 100 ms or more is traced, so a burst of
        // late pictures can be told apart from a stalled worker.
        let tick_started = Instant::now();
        let mut stage = [Duration::ZERO; 3];
        let mut updates = Vec::new();
        let mut lost = false;
        let mut ended = false;
        if let Err(error) = call.dialed.flush() {
            // The liveness close: say whether the Hang-up/Error went out first.
            if call.dialed.session.liveness_hung_up() {
                trace("liveness_hang_up", "written=true");
            }
            trace("write_failed", &error.to_string());
            lost = true;
        }
        if !lost {
            if let Err(error) = call.dialed.link.service() {
                trace("write_failed", &format!("gatt_recovery {error}"));
                lost = true;
            }
        }
        // A pseudonym<->pseudonym caller that did not reveal in time: the
        // session wrote Hang-up/Error and dropped the call's media.
        if call.dialed.session.unconfirmed() && call.closing.is_none() {
            trace("caller_reveal_timeout", "hang_up=error");
            call.closing = Some(Instant::now());
            updates.push(Update::CallEnded { reason: crate::ui::backend::CallEndReason::Unconfirmed });
            updates.push(Update::CallLinkLost);
        }
        call.dialed.link.drain_events(&mut |p, d| trace(p, d));
        call.dialed.link.recheck_preset(&mut |p, d| trace(p, d));
        // As peripheral the phone sets the link: trace what Windows reports
        // every 5 s while media runs (diagnosis of PC -> phone loss).
        if call.media && call.link_traced.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
            call.link_traced = Some(Instant::now());
            if let Some(state) = call.dialed.link.link_state() {
                trace("le_link", &state);
            }
        }
        if !lost && call.closing.is_none() {
            match call.dialed.pump_input(Duration::from_millis(5)) {
                Ok(notices) => {
                    // CF-PRES-19: queued for the presence task, in order.
                    for notice in &notices {
                        if let Notice::PeerRevealed { node_id, fingerprint } = notice {
                            call.presence_inputs.push(PresenceInput::Identified(*node_id, *fingerprint));
                        }
                    }
                    for record in call.dialed.session.take_presence_records() {
                        call.presence_inputs.push(PresenceInput::Record(record));
                    }
                    if call.dialed.session.answered() && !call.presence_answered {
                        call.presence_answered = true;
                        call.presence_inputs.push(PresenceInput::Answered);
                    }
                    // A finished task returns the stores and its records.
                    if call.presence_task.as_ref().is_some_and(|t| t.is_finished()) {
                        let task = call.presence_task.take().unwrap();
                        match task.join() {
                            Ok((stores, presence, out, took)) => {
                                self.stores = Some(stores);
                                call.presence = presence;
                                if took >= Duration::from_millis(50) {
                                    trace("presence_task", &format!("ms={}", took.as_millis()));
                                }
                                for record in out {
                                    call.dialed.session.send_discovery(record);
                                }
                                for line in call.presence.log.drain(..) {
                                    trace("presence", &line);
                                }
                                if std::mem::take(&mut call.presence.lease_stored) {
                                    updates.push(Update::ContactsChanged);
                                }
                            }
                            Err(_) => trace("presence_task_failed", "panicked; stores unavailable until restart"),
                        }
                    }
                    if call.presence_task.is_none() && !call.presence_inputs.is_empty() {
                        if let Some(stores) = self.stores.take() {
                            let inputs = std::mem::take(&mut call.presence_inputs);
                            let presence = std::mem::take(&mut call.presence);
                            call.presence_task = Some(spawn_presence(stores, presence, inputs));
                        }
                    }
                    // Which received signal ends the call (named in `call_end`).
                    let mut peer_end = None;
                    for message in call.dialed.session.take_remote() {
                        if message.signal == Signal::Ringing && !call.incoming && call.ringback && !call.media {
                            call.audio.ringback(true);
                        }
                        if message.signal == Signal::Ringing && !call.incoming {
                            trace("ringing", "");
                            call.ringing_at.get_or_insert_with(Instant::now);
                        }
                        if matches!(message.signal, Signal::HangUp | Signal::Decline | Signal::Cancel) {
                            peer_end = Some(message.signal);
                        }
                        updates.push(Update::CallSignal(message));
                        if ring_parity() && missed_cancel(call.incoming, call.media, call.placed.elapsed(), &message) {
                            updates.push(Update::CallEnded { reason: crate::ui::backend::CallEndReason::Missed });
                        }
                    }
                    for notice in notices {
                        trace("notice", &format!("{notice:?}"));
                        if let Notice::Ended(reason) = &notice {
                            // The cause of an end: the peer's own signal and
                            // reason (a phone's liveness hang-up says Error).
                            let source = match peer_end {
                                Some(signal) => format!("peer_signal signal={signal:?}"),
                                None => "local".to_owned(),
                            };
                            trace(
                                "call_end",
                                &format!(
                                    "source={source} reason={reason:?} media={} since_media_ms={}",
                                    call.media,
                                    call.media_epoch.map_or(-1, |at| at.elapsed().as_millis() as i64)
                                ),
                            );
                        }
                        match notice {
                            Notice::StartMedia { camera } => {
                                if call.start_media(camera) && call.peer_header && !call.active_shown {
                                    call.active_shown = true;
                                    updates.push(Update::CallMediaReady);
                                }
                            }
                            Notice::PeerHeader(header) => {
                                trace("peer_header", &format!("codec={} {}x{} fps={}", header.codec, header.width, header.height, header.fps));
                                updates.push(Update::CallMediaInfo {
                                    direction: "receiving",
                                    text: format!("Receiving {} video, {}x{} at {} fps", codec_name(header.codec), header.width, header.height, header.fps),
                                });
                                call.peer_header = true;
                                // The peer's pictures are decoded and shown (CF-OUT: the call screen).
                                if self.owner.is_none() {
                                    self.owner = meshlive::video::device(false, false)
                                        .ok()
                                        .and_then(|(gpu, _, _)| DecodeDevice::new(&gpu).ok())
                                        .map(|d| &*Box::leak(Box::new(d)));
                                }
                                call.pictures.set_turns(header.quarter_turns);
                                let slot = call.pictures.clone();
                                let present = move |pts_us: i64, frame: &meshlive::video::DecodedFrame<'_>| {
                                    let read = frame
                                        .nv12()
                                        .and_then(|nv12| nv12.read_rgba(frame.format.colour, frame.format.range));
                                    match read {
                                        Ok((w, h, rgba)) => slot.publish(w, h, rgba, pts_us),
                                        Err(_) => slot.read_failed(),
                                    }
                                };
                                // The peer's video is decoded (and read back) on its own thread, off
                                // the link worker (default); MESHLIVE_CALL_DECODE_THREAD=0 keeps the
                                // earlier inline decoder on this worker.
                                if std::env::var("MESHLIVE_CALL_DECODE_THREAD").ok().as_deref() == Some("0") {
                                    match self.owner {
                                        Some(owner) => {
                                            call.peer_video = Box::new(meshlive::video::MfFrames::new(owner, header, present));
                                            updates.push(Update::CallPictures(crate::ui::backend::PictureHandle(call.pictures.clone())));
                                        }
                                        None => trace("peer_video_unavailable", "no decode device"),
                                    }
                                } else {
                                    call.peer_video = Box::new(meshlive::viewer::DecodeThread::media_foundation(header, present));
                                    trace("peer_video_decoder", "thread=true");
                                    updates.push(Update::CallPictures(crate::ui::backend::PictureHandle(call.pictures.clone())));
                                }
                                if call.media && !call.active_shown {
                                    call.active_shown = true;
                                    updates.push(Update::CallMediaReady);
                                }
                            }
                            Notice::KeyFrameRequested => {
                                let now = call.dialed.now_ns();
                                if let Some(video) = call.video.as_mut() {
                                    video.keyframe_requested(now);
                                }
                            }
                            Notice::Ended(_) => { ended = true; call.stop_media(); }
                            Notice::PeerRotation(turns) => call.pictures.set_turns(turns),
                            Notice::PeerLabel { kind: meshlive::labels::KIND_NAME, text } => {
                                updates.push(Update::CallPeerName(text));
                            }
                            _ => {}
                        }
                    }
                }
                Err(error) => {
                    trace("read_failed", &error.to_string());
                    // Signals applied before the failure still reach the UI.
                    for message in call.dialed.session.take_remote() {
                        updates.push(Update::CallSignal(message));
                    }
                    lost = true;
                }
            }
            stage[0] = tick_started.elapsed();
            if lost { call.stop_media(); }
            if call.media && call.want_video {
                if call.video.is_none() {
                    if self.owner.is_none() {
                        self.owner = meshlive::video::device(false, false)
                            .ok()
                            .and_then(|(gpu, _, _)| DecodeDevice::new(&gpu).ok())
                            .map(|d| &*Box::leak(Box::new(d)));
                    }
                    match self.owner {
                        Some(owner) => match CallVideo::open_for_call(owner, &call.dialed.header, &call.dialed.video_plan,
                            if dial::relay_calls_enabled() { call.dialed.session.relay_hops() } else { 0 }) {
                            Ok((mut video, name)) => {
                                // CF-MEDIA-52: video timestamps on the call's media clock.
                                if let Some(epoch) = call.media_epoch {
                                    video.align_to(epoch);
                                }
                                trace("video_start", &format!("encoder=\"{name}\" source=camera"));
                                updates.push(Update::CallMediaInfo {
                                    direction: "sending",
                                    text: format!("Sending {} video ({})", codec_name(call.dialed.header.codec), encoder_label(&name)),
                                });
                                // The real camera, opened now and closed with the video.
                                let header = &call.dialed.header;
                                let camera = std::rc::Rc::new(meshlive::call_camera::CallCamera::start(u32::from(header.width), u32::from(header.height)));
                                let source = camera.clone();
                                video.use_frames(Box::new(move || source.latest()));
                                call.camera = Some(camera);
                                call.video = Some(video);
                            }
                            Err(error) => {
                                trace("video_unavailable", &format!("{error:?}"));
                                call.want_video = false;
                                let text = if call.dialed.video_plan.is_empty() {
                                    "Voice only: this PC has no video encoder the other phone can decode."
                                } else {
                                    "Voice only: this PC's video couldn't start."
                                };
                                updates.push(Update::CallMediaInfo { direction: "sending", text: text.into() });
                            }
                        },
                        None => {
                            trace("video_unavailable", "no encoder device");
                            call.want_video = false;
                            updates.push(Update::CallMediaInfo { direction: "sending", text: "Voice only: no graphics device for video.".into() });
                        }
                    }
                }
                if let Some(video) = call.video.as_mut() {
                    if let Err(error) = video.tick(&mut call.dialed.session) {
                        trace("video_failed", &format!("{error:?}"));
                        call.video = None;
                        call.want_video = false;
                    } else if let Some(line) = video.rate_tick(&call.dialed.session) {
                        trace("rate", &line);
                    }
                }
            }
            // The camera's state for the call screen; an unusable camera turns video off.
            let camera_events = call.camera.as_ref().map(|c| c.events()).unwrap_or_default();
            for event in camera_events {
                match event {
                    meshlive::call_camera::Event::On => updates.push(Update::CallCamera(None)),
                    meshlive::call_camera::Event::Unavailable(reason) => {
                        trace("camera_unavailable", &reason);
                        call.video = None;
                        call.camera = None;
                        call.want_video = false;
                        let _ = call.dialed.session.set_camera(false);
                        updates.push(Update::CallCamera(Some(reason)));
                    }
                }
            }
            stage[1] = tick_started.elapsed();
            let now = call.dialed.now_ns() as i64;
            if let Err(error) = call.dialed.session.pump_video(&mut call.peer_video, now, 20_000_000) {
                trace("peer_video_failed", &format!("{error:?}"));
            }
            stage[2] = tick_started.elapsed();
            updates.extend(call.pump_audio(now));
            let timeout = (!call.media).then(|| ring_timeout(
                call.dialed.session.phase(), call.placed.elapsed(), call.ringing_at.map(|at| at.elapsed()),
            )).flatten();
            if call.incoming
                && timeout == Some(Signal::Decline)
                && call.dialed.session.decline(Reason::Rejected).is_ok()
            {
                trace("decline", &format!("not answered ring_s={}", call.placed.elapsed().as_secs()));
                call.closing = Some(Instant::now());
                updates.push(Update::CallEnded { reason: crate::ui::backend::CallEndReason::Missed });
                updates.push(Update::CallLinkLost);
            }
            if !call.incoming
                && timeout == Some(Signal::Cancel)
                && call.dialed.session.cancel().is_ok()
            {
                trace("cancel", &format!("not answered rang={}", call.ringing_at.is_some()));
                call.closing = Some(Instant::now());
                updates.push(Update::CallEnded { reason: crate::ui::backend::CallEndReason::NoAnswer });
                updates.push(Update::CallLinkLost);
            }
        }
        if ended && call.closing.is_none() {
            call.closing = Some(Instant::now());
        }
        if lost || call.closing.is_some() { call.stop_media(); }
        if lost {
            trace("link_lost", &call.dialed.link.loss_context());
        }
        let total = tick_started.elapsed();
        if total >= Duration::from_millis(100) {
            trace(
                "worker_slow",
                &format!(
                    "total_ms={} input_ms={} encode_ms={} decode_ms={} rest_ms={}",
                    total.as_millis(),
                    stage[0].as_millis(),
                    stage[1].saturating_sub(stage[0]).as_millis(),
                    stage[2].saturating_sub(stage[1]).as_millis(),
                    total.saturating_sub(stage[2]).as_millis()
                ),
            );
        }
        let linger_done = call
            .closing
            .is_some_and(|at| at.elapsed() >= Duration::from_millis(LINGER_MS));
        for update in updates {
            self.send(update);
        }
        if lost {
            self.send(Update::CallLinkLost);
        }
        if lost || linger_done {
            self.finish_presence_task();
            if let Some(call) = self.call.as_ref() {
                // Decoded pictures (published to the screen) for counting against peer_frame_shown.
                trace("peer_video_summary", &format!("decoded={} read_failed={}", call.pictures.published(), call.pictures.failures()));
            }
            self.call = None;
        }
    }
}

/// The made-up name phones show for `rdi` (Android `StrangerNames.derive` of
/// the advertised ID; names-v2 lists and shared vectors).
fn own_generated_name(rdi: &[u8; 6]) -> Option<String> {
    meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(rdi)).map(|n| n.name)
}

impl Worker {
    /// Tells the UI the generated name of the call ID on air (None: off air).
    fn set_own_name(&mut self, rdi: Option<[u8; 6]>) {
        let name = rdi.and_then(|rdi| own_generated_name(&rdi));
        if self.own_generated != name {
            self.own_generated = name.clone();
            self.send(Update::OwnGeneratedName(name));
        }
    }
    fn set_visible(&mut self, visible: bool, note: Option<String>) {
        if !visible {
            self.set_own_name(None);
        }
        let next = Some((visible, note.clone()));
        if self.visibility != next {
            trace("pc_visible", &format!("visible={visible} note={:?}", note));
            self.visibility = next;
            self.send(Update::Visibility { visible, note });
        }
    }
    /// This PC's call offer now (role 0 of its presence keys).
    fn own_offer(&mut self) -> Option<meshlive::callee::OwnOffer> {
        let stores = self.stores.as_mut()?;
        let epoch = stores.epoch();
        // CF-PRES-07: an unusable store announces an ephemeral window.
        let (state, _) = stores.vault.announcing(epoch);
        meshlive::callee::own_offer(state, epoch)
    }
    /// The note shown while this PC is on the air: a damaged presence store
    /// (announcing ephemeral IDs) offers its reset.
    fn presence_note(&self) -> Option<String> {
        let damaged = self.stores.as_ref().and_then(|s| s.vault.problem()) == Some(meshlive::presence::VaultProblem::Damaged);
        damaged.then(|| PRESENCE_DAMAGED_NOTE.to_owned())
    }
    /// Design QW: announce while Receive calls is on and nothing else holds
    /// the radio; follow the rotating ID; answer a phone that subscribed.
    fn tick_listen(&mut self) {
        use meshlive::callee::{Listening, Poll};
        // Without the presence store there is no rotating ID to announce (offline).
        if self.stores.is_none() {
            return;
        }
        // The shown code needs this PC on air too (it then accepts only that
        // code's scan when Receive calls is off).
        let want = announce_wanted(Holds {
            receive_calls: self.receive_calls,
            showing_code: self.visit.is_some(),
            radio_off: self.radio_off,
            busy: self.call.is_some() || self.silent.is_some() || self.watching.is_some() || self.source.is_some(),
            verifying: self.meet.is_some() || self.show.is_some(),
        });
        if let Err(note) = want {
            if let Some(listen) = self.listen.take() {
                trace("announce_stop_reason", &format!("receive_calls={} radio_off={} in_call={}", self.receive_calls, self.radio_off, self.call.is_some()));
                listen.stop(&mut |p, d| trace(p, d));
            }
            self.set_visible(false, note.map(str::to_owned));
            return;
        }
        if self.listen.is_none() {
            if self.listen_latched || self.listen_retry_at.is_some_and(|at| Instant::now() < at) {
                return;
            }
            if let Err(reason) = meshlive::callee::policy(&mut |p, d| trace(p, d)) {
                self.listen_latched = true;
                trace("announce_latched", "reason=policy");
                self.set_visible(false, Some(reason.into()));
                return;
            }
            let Some(offer) = self.own_offer() else {
                // No node ID or day key yet (an unusable presence store
                // announces ephemeral keys instead, CF-PRES-07): retry.
                self.listen_retry_at = Some(Instant::now() + Duration::from_secs(60));
                self.set_visible(false, Some("This PC's presence keys aren't ready.".into()));
                return;
            };
            match Listening::open(&offer, &mut |p, d| trace(p, d)) {
                Ok(listen) => {
                    let persistent = self.stores.as_ref().is_some_and(|s| s.vault.persistent());
                    trace("announce_keys", if persistent { "persistent" } else { "ephemeral" });
                    self.set_own_name(Some(listen.rdi));
                    self.listen = Some(listen);
                    self.send(Update::AdvertisingProof(Ok(())));
                    let note = self.presence_note();
                    self.set_visible(true, note);
                }
                Err((None, text)) if text == meshlive::callee::ANNOUNCED_ELSEWHERE => {
                    // Another window holds the PC's one announcement: try
                    // again later without latching (it may close).
                    trace("announce_failed", "announced_elsewhere");
                    self.listen_retry_at = Some(Instant::now() + Duration::from_secs(30));
                    self.set_visible(false, Some(text));
                }
                Err((error, text)) => {
                    trace("announce_failed", &format!("{error:?}"));
                    if let Some(refusal) = error.and_then(|e| e.refusal()) {
                        self.send(Update::AdvertisingProof(Err(refusal)));
                    }
                    // Not capable until the user asks again (CF-PLAT-17).
                    self.listen_latched = true;
                    trace("announce_latched", "reason=start_failed");
                    self.set_visible(false, Some(format!("{text} Turn Receive calls off and on to try again.")));
                }
            }
            return;
        }
        let mut listen = self.listen.take().unwrap();
        let polled = listen.poll(&mut || self.own_offer(), &mut |p, d| trace(p, d));
        match polled {
            Poll::Waiting => {
                self.refresh_code(listen.rdi);
                self.set_own_name(Some(listen.rdi));
                self.listen = Some(listen);
                let note = self.presence_note();
                self.set_visible(true, note);
            }
            Poll::Client => self.answer(listen),
            Poll::Failed(text) => {
                listen.stop(&mut |p, d| trace(p, d));
                if text == meshlive::callee::DROPPED_TOO_OFTEN {
                    // Windows keeps stopping the announcement: a Bluetooth
                    // restart is what helps (shown with Open Bluetooth settings).
                    self.send(Update::BluetoothNote(Some(meshlive::bluetooth::radio_watch::RESTART_GUIDANCE.into())));
                    // Restarted MAX_DROPS times already: wait for the user
                    // (CF-PLAT-17: no endless background retry).
                    self.listen_latched = true;
                    trace("announce_latched", "reason=dropped_too_often");
                } else {
                    self.listen_retry_at = Some(Instant::now() + Duration::from_secs(5));
                }
                self.set_visible(false, Some(text));
            }
        }
    }
    /// A phone subscribed: both responder handshakes, then its Invite rings.
    fn answer(&mut self, listen: meshlive::callee::Listening) {
        let Some(stores) = self.stores.as_mut() else { return };
        // CF-PRES-07: the ephemeral window when the store is unusable (no
        // contacts, so no tag owner, lease or recognition).
        let state = stores.vault.announcing(listen.epoch).0.clone();
        let display = self.visit.as_mut().map(|v| &mut v.display);
        // CF-CALL-24: decided at Invite time inside the callee, before this
        // PC queues anything of its own (no name label to a blocked caller).
        let block_unknown = self.block_unknown;
        let stores_ref = &mut self.stores;
        let mut block_reason = None;
        let mut screen = |real: Option<[u8; 32]>| -> bool {
            let Some(stores) = stores_ref.as_mut() else { return false };
            let caller = meshlive::blocking::Caller {
                real,
                contact: match real {
                    Some(fp) => stores.contacts.get(&fp).ok().map(|c| c.is_some()),
                    None => Some(false),
                },
            };
            let blocked = match real {
                Some(fp) => stores.blocks.contains(&fp).ok(),
                None => Some(false),
            };
            let decline = meshlive::blocking::decline(&caller, blocked, block_unknown);
            if decline {
                block_reason = Some(if blocked == Some(true) { "blocked" } else { "unknown" });
            }
            decline
        };
        let search = self.pending_dial.as_ref().map(PendingDial::admission);
        let inbound = meshlive::callee::answer_with_search(listen, &state, &mut self.guards, display, self.receive_calls, search, &mut screen, &mut |p, d| trace(p, d));
        drop(screen);
        let answer = match inbound {
            Ok(meshlive::callee::Inbound::Call(answer)) => answer,
            Ok(meshlive::callee::Inbound::Scan { peripheral, reached }) => {
                self.start_show(peripheral, reached);
                return;
            }
            Err(error) => {
                trace("incoming_failed", &error);
                return;
            }
        };
        if answer.busy {
            let mut dialed = answer.dialed;
            let _ = dialed.flush();
            return;
        }
        if answer.blocked {
            let delay = meshlive::blocking::delay_ms();
            trace(
                "incoming_blocked",
                &format!("call_id={} reason={} delay_ms={delay}", hex(&answer.invite.call_id), block_reason.unwrap_or("blocked")),
            );
            self.set_visible(false, None);
            self.silent = Some(SilentDecline { dialed: answer.dialed, decline_at: Instant::now() + Duration::from_millis(delay), closing: None });
            return;
        }
        if let Some(pending) = self.pending_dial.take() {
            pending.cancelled.store(true, Ordering::Release);
            self.send(Update::CallSearchSuperseded);
        }
        trace("incoming_ring", &format!("call_id={} verified={}", hex(&answer.invite.call_id), answer.verified));
        let label_name = answer.peer_name.clone();
        let peer_name = if answer.verified {
            self.stores
                .as_mut()
                .and_then(|s| s.contacts.get(&answer.peer_fingerprint).ok().flatten())
                .map(|c| c.name)
                .unwrap_or_else(|| "A verified contact".to_owned())
        } else {
            // An unverified caller: its signed name label, else a neutral text.
            label_name.unwrap_or_else(|| "Someone nearby".to_owned())
        };
        let invite = answer.invite.clone();
        self.call = Some(ActiveCall {
            dialed: answer.dialed,
            incoming: true,
            call_id: invite.call_id,
            audio: meshlive::call_audio::CallAudio::start_call(meshlive::call_audio::gain_from_env()),
            encoder: None,
            mic_frames: 0,
            mic_muted: false,
            capture_fence: call_capture_fence(),
            ringback: false,
            ringing_at: None,
            peer_header: false,
            active_shown: false,
            camera: None,
            placed: Instant::now(),
            media: false,
            want_video: false,
            video: None,
            peer_video: Box::new(meshlive::viewer::DiscardFrames::default()),
            pictures: Default::default(),
            closing: None,
            _awake: meshlive::keep_awake::KeepAwake::new(meshlive::keep_awake::REASON).ok(),
            presence: CallPresence::new(),
            media_epoch: None,
            audio_base_us: None,
            presence_inputs: Vec::new(),
            presence_task: None,
            presence_answered: false,
            link_traced: None,
        });
        if let (Some(call), Some(stores)) = (self.call.as_mut(), self.stores.as_mut()) {
            if let Some((node, fingerprint)) = call.dialed.peer_real {
                for record in call.presence.identified(node, fingerprint, &mut stores.hooks()) {
                    call.dialed.session.send_discovery(record);
                }
            }
        }
        self.set_visible(false, None);
        let fingerprint = answer.verified.then_some(answer.peer_fingerprint);
        // CF-CALL-25: the ring (the quiet ringing tone, never above the user's
        // volume) unless switched off or Windows is in do-not-disturb or
        // presenting; it stops at Accept (start_media), Decline or the end.
        let attention = crate::notify::attention();
        if self.ringback && crate::notify::ring_allowed(attention) {
            if let Some(call) = self.call.as_mut() {
                call.audio.ringback(true);
            }
            trace("incoming_ring_sound", &format!("on attention={attention:?}"));
        } else {
            trace("incoming_ring_sound", &format!("off switch={} attention={attention:?}", self.ringback));
        }
        self.send(Update::IncomingCall { peer: peer_name, verified: answer.verified, invite, fingerprint });
        // After IncomingCall: the UI marks the call it now has.
        if self.call.as_ref().is_some_and(|c| c.dialed.paired) {
            self.send(Update::CallBonded);
        }
    }

    /// Add contact opened: a new visit; the code appears once this PC is on
    /// air (its RDI is the announced one).
    fn open_visit(&mut self) {
        if self.watching.is_some() || self.source.is_some() { self.send(Update::OwnCode(None)); return; }
        let Some(node) = meshlive::callee::node_id() else {
            self.send(Update::OwnCode(None));
            return;
        };
        let Some(fingerprint) = meshlive::identity::Identity::open_or_create()
            .ok()
            .and_then(|(i, _)| meshlive::bmh2::SigningIdentity::public_key(&i).ok())
            .and_then(|spki| crypto::sha256(&spki).ok())
        else {
            self.send(Update::OwnCode(None));
            return;
        };
        let _ = node;
        self.visit = Some(Visit::new(fingerprint));
        self.listen_retry_at = None;
        trace("code_visit_start", "");
    }
    /// Keeps the shown code on the RDI on air (a new code after it changed,
    /// expired, was used or hit its attempt cap) and reports its events.
    fn refresh_code(&mut self, rdi: [u8; 6]) {
        let Some(visit) = self.visit.as_mut() else { return };
        let mut updates = Vec::new();
        for event in std::mem::take(&mut visit.display.events) {
            match event {
                meshlive::qr_display::Event::ReuseAlert => {
                    visit.reuse_alert = true;
                    trace("code_reuse_alert", "");
                    updates.push(Update::CodeScanned(
                        "Two phones used your code. The first one may not be the person in front of you. Start again.".into(),
                    ));
                }
                meshlive::qr_display::Event::Ended(why) => trace("code_ended", &format!("reason={why}")),
                meshlive::qr_display::Event::ScanCompleted(_) => {}
            }
        }
        if visit.shown_rdi != Some(rdi) || !visit.display.has_code() {
            visit.display.on_rdi_changed(&rdi);
            match visit.display.show(visit.fingerprint, rdi, meshlive::qr_display::Display::fresh_secret()) {
                Ok(text) => {
                    visit.shown_rdi = Some(rdi);
                    trace("code_shown", "");
                    updates.push(Update::OwnCode(Some(text)));
                }
                Err(error) => trace("code_failed", &format!("{error:?}")),
            }
        }
        for update in updates {
            self.send(update);
        }
    }
    /// A phone completed its scan of the shown code: the displayer session.
    fn start_show(&mut self, peripheral: meshlive::bluetooth::peripheral::Peripheral, reached: verify_scan::Reached) {
        let scanner = reached.peer_fingerprint;
        let Some(visit) = self.visit.as_mut() else { return };
        visit.display.events.retain(|e| !matches!(e, meshlive::qr_display::Event::ScanCompleted(_)));
        let confirmed = visit.confirmed.contains(&scanner);
        let answering = confirmed && !visit.reuse_alert;
        let known = self.stores.as_mut().and_then(|s| s.contacts.get(&scanner).ok().flatten()).map(|c| c.name);
        let name = known.clone().unwrap_or_else(|| "A phone".to_owned());
        if confirmed {
            // Both K_qr events in this visit: they scanned this PC, this PC scanned them.
            if let Some(stores) = self.stores.as_mut() {
                stores.claims.mark_scanned_both_ways(&scanner, wall_ms());
            }
        }
        trace("code_scanned", &format!("known={} confirmed_in_visit={confirmed} answering={answering}", known.is_some()));
        let text = if answering {
            format!("{name} scanned your code. Exchanging presence\u{2026}")
        } else if visit.reuse_alert {
            format!("{name} scanned your code. Nothing more is shared on this visit.")
        } else {
            format!("{name} scanned your code. Now scan theirs (above), so you can find each other.")
        };
        self.send(Update::CodeScanned(text));
        let (session, buffer) = verify_scan::ScanSession::displayer(reached, 0, answering);
        self.show = Some(ShowScan { peripheral, session, buffer, clock: Instant::now(), logged: 0, name, answering });
    }
    fn tick_show(&mut self) {
        use meshlive::verify_scan::Carrier;
        let (Some(show), Some(stores)) = (self.show.as_mut(), self.stores.as_mut()) else {
            return;
        };
        let now = show.clock.elapsed().as_millis() as u64;
        {
            let mut hooks = stores.hooks();
            let mut input = show.buffer.as_slice();
            show.session.on_input(&mut input, now, &mut hooks);
            let used = show.buffer.len() - input.len();
            show.buffer.drain(..used);
        }
        for line in &show.session.log[show.logged..] {
            trace("code_exchange", line);
        }
        show.logged = show.session.log.len();
        for record in show.session.take_writes() {
            if show.peripheral.send(&record, Instant::now() + Duration::from_secs(8)).is_err() {
                show.session.link_lost();
            }
        }
        let Some(end) = show.session.poll(now) else {
            if show.peripheral.receive(&mut show.buffer, Duration::from_millis(5)).is_err() {
                show.session.link_lost();
            }
            return;
        };
        for record in show.session.take_writes() {
            let _ = show.peripheral.send(&record, Instant::now() + Duration::from_secs(2));
        }
        trace("code_session_ended", &format!("reason={end:?} lease_stored={} answering={}", show.session.lease_stored(), show.answering));
        let name = show.name.clone();
        let stored = show.session.lease_stored();
        let show = self.show.take();
        if let Some(mut show) = show {
            show.peripheral.close();
        }
        if stored {
            self.send(Update::CodeScanned(format!("You and {name} can now find each other nearby, and call from Contacts.")));
            self.send(Update::ContactsChanged);
            self.publish_contacts();
        }
    }

    /// Scans a pasted BVMID2 code: search (10 s, every address), both
    /// tagged handshakes with the persistent identity, then the question.
    fn start_meet(&mut self, text: &str) {
        if self.watching.is_some() || self.source.is_some() { self.send(Update::Meet(MeetEvent::Failed("Leave the stream before adding a contact.".into()))); return; }
        self.stop_scan();
        self.meet = None;
        let code = match Code::parse(text) {
            Ok(code) => code,
            Err(reason) => {
                let why = match reason {
                    meshlive::bvmid2::FormatError::OtherVersion => "This code is from an older MeshLive. Ask them to update the app.",
                    _ => "That isn't a MeshLive code. Copy the whole code, starting with BVMID2.",
                };
                self.send(Update::Meet(MeetEvent::Failed(why.into())));
                return;
            }
        };
        let identity = match meshlive::identity::Identity::open_or_create() {
            Ok((identity, _)) => identity,
            Err(_) => {
                self.send(Update::Meet(MeetEvent::Failed("This PC's identity key can't be opened.".into())));
                return;
            }
        };
        let Some(epoch) = dial::wall_epoch() else { return };
        self.busy.store(true, Ordering::Release);
        let started = Instant::now();
        let cancelled = Arc::new(AtomicBool::new(false));
        match verify_scan::find_and_reach(&code, &identity, epoch, cancelled, &mut |p, d| trace(p, d)) {
            Ok((central, reached)) => {
                trace("qr_reached", &format!("ms={}", started.elapsed().as_millis()));
                let clock = Instant::now();
                let (session, buffer) = ScanSession::new(reached, 0);
                // The phone's claimed name arrives only in a label; ask by its handle.
                let asked = meshlive::names::handle(&code.fingerprint[..6].try_into().unwrap());
                self.send(Update::Meet(MeetEvent::Ask { name: asked.clone() }));
                self.meet = Some(MeetScan { central, session, buffer, code, clock, asked, answered: None, logged: 0 });
            }
            Err(error) => {
                trace("qr_failed", &error.to_string());
                let text = match error.stage {
                    dial::Stage::Connect => "The phone with this code was not found nearby. Keep its Add contact screen open and try again.",
                    _ => "The phone didn't prove it's the one showing this code. Ask them to show a new code and try again.",
                };
                self.send(Update::Meet(MeetEvent::Failed(text.into())));
            }
        }
    }

    fn tick_meet(&mut self) {
        let (Some(meet), Some(stores)) = (self.meet.as_mut(), self.stores.as_mut()) else {
            return;
        };
        let now = meet.clock.elapsed().as_millis() as u64;
        let mut updates = Vec::new();
        {
            let mut hooks = stores.hooks();
            let mut input = meet.buffer.as_slice();
            meet.session.on_input(&mut input, now, &mut hooks);
            let used = meet.buffer.len() - input.len();
            meet.buffer.drain(..used);
            if let Some((yes, name)) = meet.answered.take() {
                let stored = yes && verify_scan::store_shield(hooks.contacts, &meet.session, &meet.code, &name, wall_ms());
                trace("qr_answer", &format!("yes={yes} shield_stored={stored}"));
                meet.session.confirm(yes, stored, now, &mut hooks);
                if stored {
                    meet.asked = name;
                    if let Some(visit) = self.visit.as_mut() {
                        visit.confirmed.insert(meet.session.peer_fingerprint);
                    }
                    updates.push(Update::ContactsChanged);
                } else if yes {
                    updates.push(Update::Meet(MeetEvent::Failed("Couldn't save the contact on this PC.".into())));
                }
            }
        }
        for line in &meet.session.log[meet.logged..] {
            trace("qr_exchange", line);
        }
        meet.logged = meet.session.log.len();
        for record in meet.session.take_writes() {
            if meet.central.write(&record, Instant::now() + Duration::from_secs(8)).is_err() {
                meet.session.link_lost();
            }
        }
        let ended = meet.session.poll(now);
        if ended.is_none() {
            let mut chunk = [0u8; 4096];
            match meet.central.read(&mut chunk, Instant::now() + Duration::from_millis(5)) {
                Ok(0) => meet.session.link_lost(),
                Ok(n) => meet.buffer.extend_from_slice(&chunk[..n]),
                Err(meshlive::bluetooth::carrier::Error::Timeout) => {}
                Err(_) => meet.session.link_lost(),
            }
        }
        let verified = stores.contacts.get(&meet.code.fingerprint).ok().flatten().is_some();
        if let Some(end) = meet.session.ended() {
            trace("qr_ended", &format!("reason={end:?} lease_stored={}", meet.session.lease_stored()));
            let name = meet.asked.clone();
            let text = match end {
                verify_scan::End::Declined if !verified => MeetEvent::Failed("Nothing was saved.".into()),
                _ if meet.session.lease_stored() => MeetEvent::Done(format!(
                    "{name} is now a verified contact. Your devices recognise each other, so you can call {name} from Contacts."
                )),
                _ if verified => MeetEvent::Done(format!(
                    "{name} is now a verified contact. Ask {name} to scan your code too, so this PC can find them nearby."
                )),
                verify_scan::End::NoConfirmation => MeetEvent::Failed("No answer in time. Nothing was saved.".into()),
                _ => MeetEvent::Failed("The connection ended before the check finished. Nothing was saved.".into()),
            };
            updates.push(Update::Meet(text));
            self.meet = None;
            updates.push(Update::ContactsChanged);
        }
        for update in updates {
            self.send(update);
        }
        if self.meet.is_none() {
            self.publish_contacts();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Offline only: nothing here searches or dials (no Rescan, no known person).
    use super::*;

    #[test]
    fn setup_timing_flow_retains_stage_times_without_free_form_details() {
        let private = "PRIVATE-CONTENT\nphase=forged";
        let mut output = String::new();
        for (time, stage) in [(10, "dial_requested"), (20, "dial_route_found"), (30, "provider_stopped"),
            (900, "gatt_connected"), (1_200, "link_done"), (1_500, "endpoint_done"), (1_600, "ringing"), (1_700, "mic_probe")] {
            if let Some(line) = trace_line(time, stage, private, true) { output.push_str(&line); output.push('\n'); }
        }
        assert!(output.starts_with("t=10 bluetooth phase=dial_requested\n"));
        assert!(output.ends_with("t=1600 bluetooth phase=ringing\n"));
        assert!(!output.contains("PRIVATE-CONTENT") && !output.contains("forged") && !output.contains("mic_probe"));
        assert!(trace_line(1_800, private, "", true).is_none(), "unknown or injected stage names are omitted");
        assert!(trace_line(1_800, "mic_probe", private, false).unwrap().contains(private), "the original opt-in trace arm is retained");
    }

    #[test]
    fn wipe_waits_for_stream_cleanup_even_when_the_worker_drops_its_acknowledgement() {
        for dropped_ack in [false, true] {
            let stopped = Arc::new(AtomicBool::new(false)); let finished = stopped.clone();
            let (jobs, inbox) = mpsc::channel(); let (_, updates) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let Job::WipeBarrier(ack) = inbox.recv().unwrap() else { panic!("expected wipe barrier") };
                let ack = if dropped_ack { drop(ack); None } else { Some(ack) };
                // Simulate a native stream releasing capture/provider owners.
                std::thread::sleep(Duration::from_millis(30)); finished.store(true, Ordering::Release);
                if let Some(ack) = ack { let _ = ack.send(()); }
            });
            let mut backend = RadioBackend { jobs, updates, worker: Some(worker), pending_id: None, dialing: None,
                link_up: Arc::new(AtomicBool::new(false)), busy: Arc::new(AtomicBool::new(false)), local: Vec::new(), qr: None, watch_available: true };
            backend.wipe_barrier();
            assert!(stopped.load(Ordering::Acquire)); assert!(!backend.watch_available);
        }
    }

    #[test]
    fn unanswered_calls_send_cancel_at_sixty_seconds_and_decline_at_sixty_five() {
        use meshlive::call::{Action, Controller};
        let seconds = Duration::from_secs;
        let (mut caller, opening) = Controller::outgoing([7; 16], true, 0).unwrap();
        let (mut callee, actions) = Controller::incoming(&opening[0]).unwrap();
        let Action::Send(ringing) = actions[0] else { panic!("Ringing") };
        caller.on_message(&ringing).unwrap();
        assert_eq!(ring_timeout(caller.phase(), seconds(59), Some(seconds(59))), None);
        assert_eq!(ring_timeout(caller.phase(), seconds(60), Some(seconds(60))), Some(Signal::Cancel));
        let cancel = Message::decode(&caller.cancel().unwrap().encode().unwrap()).unwrap();
        assert!(missed_cancel(true, false, seconds(60), &cancel));
        assert_eq!(callee.on_message(&cancel).unwrap(), [Action::Ended(Reason::None)]);
        assert_eq!(ring_timeout(callee.phase(), seconds(65), None), None, "the Cancel already ended it");

        // The Cancel was lost: the callee's independent timer releases it.
        let (mut callee, _) = Controller::incoming(&opening[0]).unwrap();
        assert_eq!(ring_timeout(callee.phase(), seconds(64), None), None);
        assert_eq!(ring_timeout(callee.phase(), seconds(65), None), Some(Signal::Decline));
        let decline = Message::decode(&callee.decline(Reason::Rejected).unwrap().encode().unwrap()).unwrap();
        assert_eq!((decline.signal, decline.reason), (Signal::Decline, Reason::Rejected));
        assert_eq!(callee.phase(), Phase::Ended);
    }

    #[test]
    fn answering_just_before_timeout_prevents_it_and_an_early_cancel_is_not_missed() {
        use meshlive::call::Controller;
        let seconds = Duration::from_secs;
        let (mut caller, opening) = Controller::outgoing([7; 16], false, 0).unwrap();
        let (mut callee, _) = Controller::incoming(&opening[0]).unwrap();
        let actions = callee.accept(false).unwrap();
        let meshlive::call::Action::Send(accept) = actions[0] else { panic!("Accept") };
        caller.on_message(&accept).unwrap();
        for phase in [caller.phase(), callee.phase(), Phase::Active, Phase::Ended] {
            assert_eq!(ring_timeout(phase, seconds(100), Some(seconds(100))), None);
        }
        let cancel = Message::simple([7; 16], Signal::Cancel).unwrap();
        assert!(!missed_cancel(true, false, seconds(54), &cancel));
        assert!(missed_cancel(true, false, seconds(55), &cancel));
        assert!(!missed_cancel(false, false, seconds(60), &cancel));
        assert!(!missed_cancel(true, true, seconds(60), &cancel));
        let hang_up = Message::new([7; 16], Signal::HangUp, Reason::Error, 0).unwrap();
        assert!(!missed_cancel(true, false, seconds(60), &hang_up));
        assert_eq!(ring_timeout(Phase::OutgoingInvite, seconds(89), None), None);
        assert_eq!(ring_timeout(Phase::OutgoingInvite, seconds(90), None), Some(Signal::Cancel));
    }

    /// This PC's generated name is the one phones derive from the call ID it
    /// advertises (Android StrangerNames, shared vectors labels-059/062).
    #[test]
    fn the_own_generated_name_matches_the_shared_vectors() {
        let file: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/conformance/labels.json")).unwrap();
        for id in ["labels-059", "labels-062"] {
            let v = file["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap();
            let text = v["inputs"]["input_rdi"].as_str().unwrap();
            let rdi: [u8; 6] = (0..12).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap();
            assert_eq!(own_generated_name(&rdi).as_deref(), v["expected"]["name"].as_str(), "{id}");
        }
    }

    #[test]
    fn the_pc_is_off_air_during_any_call_and_says_why_otherwise() {
        let on = Holds { receive_calls: true, ..Default::default() };
        assert_eq!(announce_wanted(on), Ok(()));
        assert_eq!(announce_wanted(Holds { busy: true, ..on }), Err(None), "CF-CALL-04: a second caller finds nothing");
        assert_eq!(announce_wanted(Holds { verifying: true, ..on }), Err(None));
        assert!(announce_wanted(Holds { radio_off: true, ..on }).unwrap_err().unwrap().contains("Bluetooth is off"));
        assert_eq!(announce_wanted(Holds::default()), Err(Some("Allow calls is off.")));
        assert_eq!(announce_wanted(Holds { showing_code: true, ..Default::default() }), Ok(()), "the shown code needs the PC on air");
        assert!(announce_wanted(Holds { showing_code: true, radio_off: true, ..Default::default() }).is_err());
    }

    #[test]
    fn only_scan_errors_the_user_must_fix_are_shown() {
        use meshlive::bluetooth::{Error, Radio};
        assert!(scan_blocker(&Error::ScanStopped(7)).unwrap().contains("privacy settings"));
        assert!(scan_blocker(&Error::Native(0x80070005u32 as i32)).unwrap().contains("privacy settings"));
        assert!(scan_blocker(&Error::ScanStopped(5)).unwrap().contains("policy"));
        assert!(scan_blocker(&Error::Radio(Radio::Missing)).is_some());
        // Ring timeouts (design decision 2026-10-21): the caller gives up at
        // 60 s, the callee's own safety stop follows at 65 s.
        assert_eq!((super::CALLER_RING_SECONDS, super::CALLEE_RING_SECONDS), (60, 65));
        // CF-DISC-32: back-off 3, 6, 9 ... capped at 30 s; user-fixable keeps 30 s; unsupported stops.
        assert_eq!(super::scan_retry_ms(&Error::ScanStopped(4), 1), Some(3_000));
        assert_eq!(super::scan_retry_ms(&Error::ScanStopped(4), 3), Some(9_000));
        assert_eq!(super::scan_retry_ms(&Error::Native(-1), 50), Some(30_000));
        assert_eq!(super::scan_retry_ms(&Error::ScanStopped(7), 1), Some(30_000));
        assert_eq!(super::scan_retry_ms(&Error::ScanStopped(6), 1), None);
        assert_eq!(super::scan_retry_ms(&Error::Unsupported, 2), None);
        // Only a Bluetooth restart helps once the back-off is capped on unexplained errors.
        assert!(!super::scan_needs_restart(&Error::ScanStopped(4), 9));
        assert!(super::scan_needs_restart(&Error::ScanStopped(4), 10));
        assert!(!super::scan_needs_restart(&Error::ScanStopped(7), 20), "a privacy block has its own note");
        assert!(!super::scan_needs_restart(&Error::Unsupported, 20), "an adapter that can't scan");
        for retried in [Error::ScanStopped(4), Error::ScanStopped(1), Error::Radio(Radio::Off), Error::Native(-1), Error::Cancelled] {
            assert_eq!(scan_blocker(&retried), None, "{retried:?}");
        }
    }

    #[test]
    fn a_read_code_goes_into_the_typed_scan_path_and_closes_the_camera() {
        use meshlive::qr_scan::Event;
        let code = format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789");
        assert_eq!(qr_outcome(Event::Code(code.clone())), (Some(Update::Meet(MeetEvent::Searching)), Some(code), true));
        assert_eq!(qr_outcome(Event::On), (None, None, false));
        let (update, scan, close) = qr_outcome(Event::Unavailable { reason: "denied".into(), privacy: true });
        assert_eq!((update, scan, close), (Some(Update::Meet(MeetEvent::Failed("denied".into()))), None, true));
        let (update, scan, close) = qr_outcome(Event::NotACode);
        assert!(matches!(update, Some(Update::Meet(MeetEvent::Failed(_)))) && scan.is_none() && !close);
    }

    #[test]
    fn closing_the_scan_step_needs_no_camera_and_sends_nothing_to_the_worker() {
        let mut backend = RadioBackend::start_with(None);
        // Closing a camera that was never opened is a no-op (no device touched).
        backend.command(Command::QrCamera(false));
        assert!(backend.qr.is_none());
        assert!(backend.poll().is_empty());
    }

    fn wait_for(backend: &mut RadioBackend) -> Vec<Update> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let got = backend.poll();
            if !got.is_empty() || Instant::now() >= until {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_bonded_note_reaches_the_ui_while_connecting_once() {
        let (tx, rx) = mpsc::channel();
        let mut told = false;
        {
            let mut forward = bonded_forwarder(&tx, &mut told);
            forward("le_device", "paired=true");
            forward("bonded_peer", "warning");
            forward("bonded_peer", "warning");
        }
        assert!(told);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Update::CallBonded]);
    }

    #[test]
    fn only_a_setup_failure_inside_15_s_is_retried_once_and_failures_read_differently() {
        let error = |stage| dial::Error { stage, detail: String::new() };
        let s = Duration::from_secs;
        assert_eq!(retry_plan(&error(dial::Stage::Connect), s(2), false), Some(s(8)));
        assert_eq!(retry_plan(&error(dial::Stage::Link), s(10), false), Some(s(5)));
        assert_eq!(retry_plan(&error(dial::Stage::Endpoint), s(14), false), Some(s(1)));
        assert_eq!(retry_plan(&error(dial::Stage::Connect), s(15), false), None);
        assert_eq!(retry_plan(&error(dial::Stage::Call), s(1), false), None, "after the handshake: never");
        assert_eq!(retry_plan(&error(dial::Stage::Connect), s(1), true), None, "cancelled");
        let texts = [
            setup_failure_text(None),
            setup_failure_text(Some(&error(dial::Stage::Connect))),
            setup_failure_text(Some(&error(dial::Stage::Link))),
        ];
        assert_eq!(texts[0], "Not found nearby");
        assert!(texts[1].starts_with("Couldn't connect the call") && texts[2].starts_with("Couldn't connect the call"));
        assert_ne!(texts[1], texts[2]);
        // CF-CALL-04: a callee in another call closes the link before the handshake: no retry, a clear reason.
        let closed = dial::Error { stage: dial::Stage::Link, detail: "link closed".into() };
        assert_eq!(retry_plan(&closed, s(1), false), None);
        assert!(setup_failure_text(Some(&closed)).contains("They may be in another call"));
    }

    #[test]
    fn a_tap_redials_only_a_fresh_sighting_of_the_same_rdi_and_version() {
        // CF-DISC-40 on the shared vector announcements-004 (a v5 call offer).
        let all: serde_json::Value =
            serde_json::from_str(include_str!("../tests/vectors/conformance/announcements.json")).unwrap();
        let hex = all["vectors"].as_array().unwrap().iter().find(|v| v["id"] == "announcements-004").unwrap()
            ["expected"]["bytes"]
            .as_str()
            .unwrap()
            .to_string();
        let raw: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let mut selected = meshlive::discovery::Announcement::decode(&raw, true).unwrap();
        selected.flags |= meshlive::discovery::GATT;
        assert!(same_offer(&selected, &selected.clone()));
        let mut other_id = selected.clone();
        other_id.prefix[0] ^= 1;
        assert!(!same_offer(&selected, &other_id));
        let mut other_version = selected.clone();
        other_version.version = 2;
        assert!(!same_offer(&selected, &other_version));
        let mut not_a_call = selected.clone();
        not_a_call.flags &= !meshlive::discovery::CALL;
        assert!(!same_offer(&selected, &not_a_call));
    }

    #[test]
    fn call_ids_are_random_and_bound_to_the_next_call() {
        let mut backend = RadioBackend::start_with(None);
        let a = backend.new_call_id();
        let b = backend.new_call_id();
        assert_ne!(a, b);
        assert_ne!(a, [0; 16]);
        assert_eq!(backend.pending_id, Some(b));
    }

    #[test]
    fn a_call_without_an_issued_id_never_dials() {
        let mut backend = RadioBackend::start_with(None);
        backend.command(Command::Call {
            person: 1,
            voice: true,
        });
        assert_eq!(backend.poll(), vec![Update::CallLinkLost]);
        assert!(backend.dialing.is_none());
    }

    #[test]
    fn calling_someone_not_found_by_a_search_reports_the_link_lost() {
        let mut backend = RadioBackend::start_with(None);
        let id = backend.new_call_id();
        backend.command(Command::Call {
            person: 7,
            voice: true,
        });
        assert_eq!(wait_for(&mut backend), vec![Update::CallLinkLost]);
        // Later UI signals for that call are ignored without a session.
        backend.command(Command::CallSignal(
            Message::simple(id, Signal::Cancel).unwrap(),
        ));
        assert!(backend.dialing.is_none());
        std::thread::sleep(Duration::from_millis(50));
        assert!(backend.poll().is_empty());
    }
}

/// CF-PRES-07: shown (with "Reset presence key") while a damaged presence
/// store makes this PC announce ephemeral IDs.
const PRESENCE_DAMAGED_NOTE: &str = "This PC's saved contact keys are damaged, so your contacts can't recognise this PC (they can still call it with its code). Reset fixes this; contacts recognise this PC again after a call.";
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
