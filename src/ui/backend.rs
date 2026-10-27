//! The seam between the screens and the core. The UI sends `Command`s and polls
//! `Update`s; whatever implements `Backend` (the real Bluetooth/session owner,
//! the offline default, or the scripted fake below) is the only thing that
//! touches a radio, camera, microphone or audio endpoint.
use super::model::*;
use meshlive::call::Message;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Search again (Android people_empty_action).
    Rescan,
    /// The People (Nearby) list is on screen or not: the radio owner scans
    /// continuously while it is (CF-DISC-29).
    PeopleVisible(bool),
    StreamsVisible(bool),
    Watch(Handle),
    StopWatching,
    /// Mute/unmute what is being listened to or watched (audio output only).
    MuteWatch(bool),
    Call { person: Handle, voice: bool },
    /// A signal for the active call, already produced by call::Controller.
    CallSignal(Message),
    GoLive { kind: LiveKind, title: String, initial: meshlive::stream_source::InitialMedia },
    ConfigureLive(meshlive::stream_source::Settings),
    LiveExposureConsent,
    PauseLive(bool),
    MuteLive(bool),
    LiveCamera(bool),
    StopLive,
    /// Settings changed; the backend applies or persists them.
    Switches(Switches),
    /// A new label for a verified contact (only this PC sees it).
    RenameContact { contact: Handle, name: String },
    /// Forget a verified contact (they must be verified again in person).
    ForgetContact(Handle),
    /// Block or unblock calls from a verified contact (full fingerprint).
    SetBlocked { contact: Handle, blocked: bool },
    /// Block or unblock a caller from Recent calls (its full fingerprint).
    BlockCaller { fingerprint: [u8; 32], blocked: bool },
    /// Forget every verified contact (the way out of a damaged store).
    ResetContacts,
    /// Call a verified contact by its recognised rotating ID (node ID =
    /// fingerprint[0..16]); `name` is for "Looking for X…".
    CallContact { node: [u8; 16], name: String, voice: bool },
    /// Scan a typed or pasted BVMID2 code (Add contact).
    ScanCode(String),
    /// The answer to "Is this X's screen you scanned?" and the name to save.
    ConfirmScan { yes: bool, name: String },
    /// Leave the Add contact screen (ends a scan in progress).
    CancelScan,
    /// Show this PC's own BVMID2 code.
    ShowOwnCode,
    /// The 60-digit safety code shared with this contact (read aloud to compare).
    ShowSafetyCode(Handle),
    /// The emergency wipe (confirmed by the user typing DELETE).
    Wipe,
    /// Write the diagnostics report (Settings > About) to Documents.
    ExportDiagnostics,
    ShareCrashReport,
    /// Save this PC's name (empty: remove it).
    SetOwnName(String),
    /// Add contact's "Scan their code" camera: true while that step is
    /// open, false when it closes (also closed by the backend when a code
    /// is read, a scan starts, Add contact is left or the app wipes). A read
    /// code arrives as `Update::Meet(Searching)` and goes into the same scan
    /// as a typed one; a camera that can't be used as `Meet(Failed(reason))`.
    QrCamera(bool),
    /// CF-PRES-07 "Reset presence key": delete a damaged presence store (only
    /// when it is latched damaged; shown with the Visibility note that says so).
    /// Handled by the radio backend; the button is on the Calls page note.
    ResetPresenceKey,
    /// "Not now" on the crash prompt: forget the pending crash report.
    DismissCrashReport,
    /// Open Windows Settings > Bluetooth (the backend launches
    /// ms-settings:bluetooth; MeshLive never switches the radio itself).
    OpenBluetoothSettings,
}

/// Why a call ended, when the backend knows more than the call signals
/// (ring timeouts). Android wording. Sent by the integrator's ring timers.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallEndReason {
    /// The caller gave up after 60 s of ringing.
    NoAnswer,
    /// This PC did not answer: the caller gave up (60 s) or the safety stop (65 s).
    Missed,
    Declined,
    Busy,
    Ended,
    /// The caller of a pseudonym<->pseudonym call did not prove who it is in time.
    Unconfirmed,
}
impl CallEndReason {
    pub fn text(self) -> &'static str {
        match self {
            CallEndReason::NoAnswer => "No answer",
            CallEndReason::Missed => "Missed call",
            CallEndReason::Declined => "Declined",
            CallEndReason::Busy => "Busy",
            CallEndReason::Ended => "Call ended",
            CallEndReason::Unconfirmed => "Couldn't confirm who this is",
        }
    }
}

/// The peer's decoded pictures for the active call (compared by identity).
#[derive(Clone)]
pub struct PictureHandle(pub std::sync::Arc<meshlive::video::PictureSlot>);
impl PartialEq for PictureHandle {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl std::fmt::Debug for PictureHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PictureHandle(published={})", self.0.published())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    Searching(bool),
    Streams(Vec<StreamEntry>),
    WatchAvailable(bool),
    WatchNote(Option<String>),
    WatchRoute(u8),
    WatchTitle(String),
    LiveNote(Option<String>),
    People(Vec<Person>),
    Contacts(Vec<Contact>),
    /// The verified-contact store cannot be read (None: it reads normally).
    ContactsProblem(Option<StoreProblem>),
    Renamed { contact: Handle, outcome: RenameOutcome },
    Watch(WatchEvent),
    /// A signal from the peer of the active call.
    CallSignal(Message),
    CallSearchSuperseded,
    /// The first record of a new incoming call (Design QW: a phone called
    /// this PC's announcement).
    IncomingCall {
        peer: String,
        verified: bool,
        invite: Message,
        /// The caller's full fingerprint when it presented its real key
        /// (Block from Recent calls); None for a pseudonymous caller.
        fingerprint: Option<[u8; 32]>,
    },
    /// Media for the active call is flowing (call::Controller::media_ready).
    CallMediaReady,
    /// Test-only (emitted only by `test-hooks` builds): call this contact.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    TestDial(Handle),
    /// CF-OUT-07: what this call really uses or why it fell back (shown on
    /// the call screen, never only in a trace): ("sending"|"receiving", text).
    CallMediaInfo { direction: &'static str, text: String },
    /// The diagnostics report was saved (its path) or why not.
    DiagnosticsSaved(Result<String, String>),
    /// Where the peer's decoded pictures appear during this call.
    CallPictures(PictureHandle),
    /// The peer's verified name label (CF-REC-43); shown for an unverified peer.
    CallPeerName(String),
    /// The call's microphone: None = on (capturing for this call); Some =
    /// unusable, with the reason the user reads.
    CallMic(Option<String>),
    /// Fresh capture is pending; readiness follows the current restart ack.
    CallCaptureStarting,
    CallCaptureReady,
    /// The call's camera: None = on (capturing for this call); Some =
    /// unusable, with the reason the user reads.
    CallCamera(Option<String>),
    /// Windows is paired with the phone of the active call at the Bluetooth
    /// level, so the link identifies this PC even in a private call.
    CallBonded,
    CallLinkLost,
    /// The call never got past setup (CF-CALL-21): the text the user reads,
    /// e.g. "Not found nearby" or "Couldn't connect the call: ...".
    CallSetupFailed(String),
    /// Bluetooth went off or keeps failing (CF-LIFE-13/15): the text the user
    /// reads; None when it is back.
    BluetoothNote(Option<String>),
    /// Contacts seen nearby by their recognised rotating ID, and contacts
    /// whose lease this PC holds (node IDs = fingerprint[0..16]).
    ContactsNearby { nearby: Vec<[u8; 16]>, leased: Vec<[u8; 16]> },
    /// The verified-contact store was written by the Bluetooth owner (a scan).
    ContactsChanged,
    /// What the call screen says while the call is being set up
    /// ("Looking for Asha…"); None returns to the normal status.
    CallStatus(Option<String>),
    Meet(MeetEvent),
    /// This PC's own BVMID2 code text (None: not available).
    OwnCode(Option<String>),
    /// The safety code with a contact (CF-HS-48).
    SafetyCode { contact: Handle, code: String },
    /// The wipe ran (or resumed at launch): what it did.
    Wiped(meshlive::wipe::Report),
    /// This PC's saved name, or why the typed one was refused.
    OwnName { name: Option<String>, error: Option<String> },
    /// What happened to the shown code: a phone scanned it, the reuse
    /// alert, presence exchanged (the text the user reads).
    CodeScanned(String),
    Live(LiveEvent),
    /// Design QW: this PC is announced to phones (`visible`), or why not.
    Visibility { visible: bool, note: Option<String> },
    /// What a real announcement start proved (the Receive calls gate).
    AdvertisingProof(Result<(), meshlive::bluetooth::gate::Refusal>),
    /// The last run ended in a crash and its report is waiting: the shell
    #[allow(dead_code)] // Sent by the crash reporter (integrator).
    /// asks once whether to share it.
    CrashReportPending,
    CrashReportShared(Result<(), String>),
    #[allow(dead_code)] // Sent by the integrator's ring timers.
    /// The active call ended for this reason (sent by the backend when its
    /// ring timers fire, or to name an ending the signals do not carry).
    /// The UI ends the call locally if it is still open and shows the text.
    CallEnded { reason: CallEndReason },
    /// This PC's ID as Android shows it (`names::handle` of the identity
    /// fingerprint's first 6 bytes, `SSSSS-SSSSS`); None when there is none.
    OwnId(Option<String>),
    /// The made-up name phones nearby show for this PC (Android `StrangerNames`
    /// of the role-0 rotating ID on air; names-v2 shared vectors). It changes
    /// with the ID at every epoch boundary; None while this PC is not on air.
    OwnGeneratedName(Option<String>),
}

pub trait Backend {
    fn command(&mut self, command: Command);
    fn poll(&mut self) -> Vec<Update>;
    /// A fresh call ID. The real backend returns CSPRNG bytes.
    fn new_call_id(&mut self) -> [u8; 16];
    /// How long until the next update may be due; None when idle.
    fn next_due(&self) -> Option<Duration> {
        None
    }
    /// False when nothing is searching or connecting (no Bluetooth owner yet).
    fn connected(&self) -> bool {
        true
    }
    /// Whether Watch is usable here. Source availability is separate.
    fn streams(&self) -> bool {
        true
    }
    fn can_go_live(&self) -> bool { self.streams() }
}

/// No radio, no media: lists stay empty, commands are recorded and ignored.
/// This is what the app uses until the Bluetooth owner is connected.
#[derive(Default)]
pub struct OfflineBackend {
    pub commands: Vec<Command>,
    counter: u64,
}
impl Backend for OfflineBackend {
    fn connected(&self) -> bool {
        false
    }
    fn streams(&self) -> bool {
        false
    }
    fn command(&mut self, command: Command) {
        self.commands.push(command);
    }
    fn poll(&mut self) -> Vec<Update> {
        vec![]
    }
    fn new_call_id(&mut self) -> [u8; 16] {
        self.counter += 1;
        let mut id = [0u8; 16];
        id[8..].copy_from_slice(&self.counter.to_be_bytes());
        id
    }
}

/// Scripted fake core: reacts to commands the way a healthy peer would, after
/// `delay` per step. Used by the UI tests and by `--demo`; it opens nothing.
pub struct ScriptedBackend {
    pub delay: Duration,
    pub commands: Vec<Command>,
    queue: VecDeque<(Instant, Update)>,
    streams: Vec<StreamEntry>,
    people: Vec<Person>,
    call_id: [u8; 16],
    counter: u64,
    /// Steps a test can switch off to exercise failures.
    pub fail_go_live: bool,
    pub answer_calls: bool,
    pub drop_watch_after_connect: bool,
}
impl ScriptedBackend {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            commands: vec![],
            queue: VecDeque::new(),
            streams: vec![],
            people: vec![],
            call_id: [0; 16],
            counter: 0,
            fail_go_live: false,
            answer_calls: true,
            drop_watch_after_connect: false,
        }
    }
    /// The people and streams of the demo.
    pub fn demo(delay: Duration) -> Self {
        let mut b = Self::new(delay);
        b.streams = vec![
            StreamEntry {
                id: 1,
                from: "Priya".into(),
                title: None,
                audio_only: false,
                reach: Reach::Direct,
                paused: false,
                full: false,
                confirmed: true,
                unavailable: None,
            },
            StreamEntry {
                id: 2,
                from: String::new(),
                title: Some("Neighbourhood radio, evening updates".into()),
                audio_only: true,
                reach: Reach::Via(2),
                paused: false,
                full: false,
                confirmed: false,
                unavailable: None,
            },
        ];
        b.people = vec![
            Person {
                id: 10,
                name: "Priya".into(),
                reach: Reach::Direct,
                verified: true,
                copied: false,
            },
            Person {
                id: 11,
                name: "Blue Otter".into(),
                reach: Reach::Via(1),
                verified: false,
                copied: false,
            },
        ];
        b.push(Update::Streams(b.streams.clone()));
        b.push(Update::People(b.people.clone()));
        b.push(Update::Contacts(vec![Contact {
            id: 10,
            name: "Priya".into(),
            by_qr: true,
            nearby: true,
            lease: true,
            blocked: false,
        }]));
        b
    }
    pub fn push(&mut self, update: Update) {
        self.queue.push_back((Instant::now() + self.delay, update));
    }
}
impl Backend for ScriptedBackend {
    fn command(&mut self, command: Command) {
        use meshlive::call::Signal;
        match &command {
            Command::Watch(_) => {
                self.push(Update::Watch(WatchEvent::Connected));
                if self.drop_watch_after_connect {
                    self.push(Update::Watch(WatchEvent::Lost));
                }
            }
            Command::Call { .. } => {
                self.call_id = self.new_call_id();
            }
            Command::CallSignal(m) => match m.signal {
                Signal::Invite if self.answer_calls => {
                    let id = m.call_id;
                    self.push(Update::CallSignal(remote(id, Signal::Ringing)));
                    self.push(Update::CallSignal(remote(id, Signal::Accept)));
                    self.push(Update::CallMediaReady);
                }
                Signal::Accept => self.push(Update::CallMediaReady),
                _ => {}
            },
            Command::GoLive { .. } => {
                if self.fail_go_live {
                    self.push(Update::Live(LiveEvent::Failed));
                } else {
                    self.push(Update::Live(LiveEvent::Started));
                    self.push(Update::Live(LiveEvent::Viewers(1)));
                }
            }
            Command::Rescan => self.push(Update::Streams(self.streams.clone())),
            Command::CallContact { .. } => {
                self.call_id = self.new_call_id();
            }
            Command::ScanCode(_) => {
                self.push(Update::Meet(MeetEvent::Searching));
                self.push(Update::Meet(MeetEvent::Ask { name: "Green Heron".into() }));
            }
            Command::ConfirmScan { yes: true, name } => {
                let name = name.clone();
                self.push(Update::Meet(MeetEvent::Done(format!("{name} is now a verified contact."))));
            }
            Command::ConfirmScan { yes: false, .. } => self.push(Update::Meet(MeetEvent::Failed("Nothing was saved.".into()))),
            Command::ShowOwnCode => self.push(Update::OwnCode(Some("BVMID2.DEMO".into()))),
            _ => {}
        }
        self.commands.push(command);
    }
    fn poll(&mut self) -> Vec<Update> {
        let now = Instant::now();
        let mut out = vec![];
        while self.queue.front().is_some_and(|(due, _)| *due <= now) {
            out.push(self.queue.pop_front().unwrap().1);
        }
        out
    }
    fn new_call_id(&mut self) -> [u8; 16] {
        self.counter += 1;
        let mut id = [0xA5u8; 16];
        id[8..].copy_from_slice(&self.counter.to_be_bytes());
        id
    }
    fn next_due(&self) -> Option<Duration> {
        self.queue
            .front()
            .map(|(due, _)| due.saturating_duration_since(Instant::now()))
    }
}
