//! Pure screen state machines. Each wraps the core type that owns the real
//! rules (call::Controller for calls, live:: wire states for going live) and
//! never touches a radio, camera, microphone or audio endpoint. A `Backend`
//! (ui/backend.rs) feeds it events and carries out its commands.
use meshlive::call::{self, Action, Controller, Message, Phase, Reason, Signal};
use std::time::{Duration, Instant};

/// Opaque handle a backend assigns to a stream, person or contact.
pub type Handle = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Direct,
    Via(u8),
}
impl Reach {
    /// Android people_reach_* / audio_live_route_* wording.
    pub fn label(self) -> String {
        match self {
            Reach::Direct => "Nearby".into(),
            Reach::Via(1) => "Via 1 phone".into(),
            Reach::Via(n) => format!("Via {n} phones"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: Handle,
    /// Display name of the source; empty when unknown.
    pub from: String,
    pub title: Option<String>,
    pub audio_only: bool,
    pub reach: Reach,
    pub paused: bool,
    pub full: bool,
    /// False for a source nobody has verified (titles then read as unconfirmed).
    pub confirmed: bool,
    pub unavailable: Option<&'static str>,
}
impl StreamEntry {
    pub fn headline(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            return title.to_owned();
        }
        match (self.audio_only, self.from.is_empty()) {
            (false, true) => "Live from a nearby phone".into(),
            (false, false) => format!("Live from {}", self.from),
            (true, true) => "Someone nearby is on the radio".into(),
            (true, false) => format!("Radio from {}", self.from),
        }
    }
    pub fn subtitle(&self) -> String {
        let mut parts = vec![];
        if let Some(reason) = self.unavailable { parts.push(reason.to_owned()); }
        if self.audio_only {
            parts.push("Audio only".to_owned());
        }
        parts.push(self.reach.label());
        if self.paused {
            parts.push("Paused".into());
        }
        if self.full {
            parts.push("Full, can't take more viewers".into());
        }
        if !self.confirmed {
            parts.push("unconfirmed".into());
        }
        parts.join(" \u{b7} ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub id: Handle,
    pub name: String,
    pub reach: Reach,
    pub verified: bool,
    /// CF-PRIV-22: more than one phone is using this rotating ID.
    pub copied: bool,
}
impl Person {
    pub fn subtitle(&self) -> String {
        if self.copied {
            format!("{} \u{b7} More than one phone is using this ID", self.reach.label())
        } else if self.verified {
            format!("{} \u{b7} Verified contact", self.reach.label())
        } else {
            format!("{} \u{b7} not checked", self.reach.label())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    pub id: Handle,
    pub name: String,
    pub by_qr: bool,
    pub nearby: bool,
    /// They gave this PC a presence lease, so it can find them by their
    /// rotating ID; without it: "Ask them to scan your code too".
    pub lease: bool,
    /// Calls from this contact are declined silently (CF-CALL-24).
    pub blocked: bool,
    /// Their ID (`SSSSS-SSSSS`): the contact sheet's "ID" line.
    pub handle: String,
    /// When they were verified, as this PC's date ("Nov 6, 2026"); empty when unknown.
    pub verified_on: String,
    /// Their full ID: the fingerprint as upper-case hex, four groups of four
    /// per line (Android `IdentityFormat.groupedHex`).
    pub full_id: String,
    /// What they call themselves in calls (Naming v2); never their title.
    pub claim: Option<ClaimNote>,
    /// You and they scanned each other's code (the claims store's fact).
    pub both_ways: bool,
}

/// What a verified contact calls themselves (Android `ContactClaim`), shown
/// on the contact sheet only. Your label stays their name either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimNote {
    /// "They call themselves %s".
    CallsThemselves(String),
    /// A newer name you have not seen: the one note, with "Use it as my
    /// label" and "Ignore".
    Changed(String),
}
/// A contact only checked by code, never scanned in person (Android
/// `DisplayIdentityText.CHECKED_BY_CODE`): lists say so and draw no full check.
pub const CHECKED_BY_CODE: &str = "Checked by code";

impl Contact {
    pub fn subtitle(&self) -> String {
        let how = if self.by_qr {
            "Verified by QR code"
        } else {
            CHECKED_BY_CODE
        };
        if self.nearby {
            format!("{how} \u{b7} Nearby now")
        } else {
            how.into()
        }
    }
}

/// One row of Settings > Privacy > Blocked people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedPerson {
    pub fingerprint: [u8; 32],
    /// Your label for a verified contact, else their ID (`SSSSS-SSSSS`).
    pub name: String,
    /// When it was blocked (ms since 1970, UTC); 0 when unknown.
    pub since_ms: i64,
}

/// The block list as the contact layer read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockedView {
    pub people: Vec<BlockedPerson>,
    /// The list can't be read: calls are not blocked until it is reset.
    pub unreadable: bool,
}

/// The Add contact (Meet) screen's scan of a typed or pasted code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeetEvent {
    /// Searching for the phone that shows the code.
    Searching,
    /// The phone answered with the code's key: "Is this X's screen you scanned?"
    Ask { name: String },
    /// Finished: what the user reads (e.g. verified, recognise each other).
    Done(String),
    /// Not done: the reason in plain words.
    Failed(String),
}

/// Android StoreProblem: the verified-contact store cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreProblem {
    Temporary,
    Corrupt,
}
/// Android RenameOutcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameOutcome {
    Renamed,
    InvalidName,
    NameTaken,
    NotFound,
    StoreUnreadable,
}
impl RenameOutcome {
    /// Android contact_name_* / verify_store_* wording; None when renamed.
    pub fn message(self) -> Option<&'static str> {
        match self {
            Self::Renamed => None,
            Self::InvalidName => Some("Use a short plain name (up to 32 bytes), not an ID or a mix of alphabets."),
            Self::NameTaken => Some("Another contact already has that name. Choose a different one."),
            Self::NotFound => Some("This contact is no longer saved."),
            Self::StoreUnreadable => Some("Your list of verified people can't be read right now. Try again in a moment."),
        }
    }
}

// ---------------------------------------------------------------- watching

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchState {
    Connecting,
    Playing,
    Reconnecting,
    Ended,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Connected,
    /// The signal dropped (viewer_signal_lost).
    Lost,
    /// Pause/resume from scripted backends. Native Watch supplies SourceState.
    #[cfg_attr(not(test), allow(dead_code))]
    Paused(bool),
    SourceState(meshlive::stream_watch::PlaybackState),
    #[cfg_attr(not(test), allow(dead_code))]
    PictureStalled(bool),
    #[cfg_attr(not(test), allow(dead_code))]
    Ended,
    /// The reconnect window passed without a source.
    #[cfg_attr(not(test), allow(dead_code))]
    GaveUp,
}

/// Android `viewer_picture_stalled` / `viewer_signal_lost`.
pub const VIEWER_PICTURE_STALLED: &str = "Picture stalled \u{2014} waiting";
pub const VIEWER_SIGNAL_LOST: &str = "Signal lost \u{2014} reconnecting";
/// The status under a stalled picture (Android `watch_status_stalled`).
pub const WATCH_STATUS_STALLED: &str = "Picture stalled";
/// No new picture for longer than this is a freeze (Android
/// `ViewerFrameAgeConfig.MIN_STALE_MS`: shorter gaps are Bluetooth delivery
/// working as designed).
pub const FROZEN_AFTER: Duration = Duration::from_millis(1_500);

/// What "Warn when video freezes" shows now (Android `ViewerStaleUiPlan`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freeze {
    /// Nothing to say: no picture yet, ended, paused or camera off.
    Waiting,
    /// Pictures keep coming; the last one came this long ago.
    Fresh(Duration),
    /// No new picture for longer than [`FROZEN_AFTER`] with the signal up.
    Stalled(Duration),
    /// The signal dropped; the last picture came this long ago.
    SignalLost(Duration),
}

/// "<1 s", "3.2 s", "1 min 05 s" (Android `ViewerFrameAgeText.age`; rounded
/// down to a tenth, so a rising age never reads ahead).
pub fn frame_age_text(age: Duration) -> String {
    let ms = age.as_millis() as u64;
    if ms < 1_000 {
        "<1 s".into()
    } else if ms < 60_000 {
        format!("{}.{} s", ms / 1_000, ms / 100 % 10)
    } else {
        format!("{} min {:02} s", ms / 60_000, ms / 1_000 % 60)
    }
}

/// Watching a stream, video or audio-only (radio). Ended is final.
#[derive(Clone, Debug)]
pub struct WatchModel {
    pub stream: StreamEntry,
    pub state: WatchState,
    pub muted: bool,
    pub paused: bool,
    pub source_state: meshlive::stream_watch::PlaybackState,
    pub stalled: bool,
    pub route: Reach,
    /// When this PC last showed a new picture of the stream (None: none yet).
    pub last_picture: Option<Instant>,
    /// The signal dropped and no picture came since (Android `lossEpoch`).
    lost: bool,
    /// A pause or camera off was seen: the stall clock restarts after it.
    quiet: bool,
    /// Stalls count from here (Android `referenceFloorNs`).
    stall_from: Option<Instant>,
}
impl WatchModel {
    pub fn new(stream: StreamEntry) -> Self {
        let paused = stream.paused;
        let route = stream.reach;
        Self {
            stream,
            state: WatchState::Connecting,
            muted: false,
            paused,
            source_state: Default::default(),
            stalled: false,
            route,
            last_picture: None,
            lost: false,
            quiet: false,
            stall_from: None,
        }
    }
    /// A new picture of this stream was shown (Android `onFramePresented`).
    pub fn picture(&mut self, now: Instant) {
        self.last_picture = Some(now);
        self.lost = false;
    }
    /// The picture now (Android `ViewerFrameAgeMonitor.snapshot`): a lost
    /// signal stays lost until the next picture; a pause or camera off is no
    /// freeze, and the stall clock restarts when it ends.
    pub fn freeze(&mut self, now: Instant) -> Freeze {
        let Some(last) = self.last_picture else { return Freeze::Waiting };
        let since = now.saturating_duration_since(last);
        if self.state == WatchState::Ended {
            return Freeze::Waiting;
        }
        if self.lost || self.state == WatchState::Reconnecting {
            return Freeze::SignalLost(since);
        }
        if self.paused || self.source_state.camera_off {
            self.quiet = true;
            return Freeze::Waiting;
        }
        if std::mem::take(&mut self.quiet) {
            self.stall_from = Some(now);
        }
        let counted = self.stall_from.map_or(since, |from| now.saturating_duration_since(from.max(last)));
        if counted > FROZEN_AFTER { Freeze::Stalled(since) } else { Freeze::Fresh(since) }
    }
    pub fn on(&mut self, event: WatchEvent) {
        use {WatchEvent as E, WatchState as S};
        self.state = match (self.state, event) {
            (S::Ended, _) => S::Ended,
            (_, E::Ended | E::GaveUp) => {
                self.source_state = Default::default(); self.paused = false;
                S::Ended
            }
            (S::Connecting | S::Reconnecting, E::Connected) => {
                self.stalled = false;
                self.source_state = Default::default(); self.paused = false;
                S::Playing
            }
            (S::Playing, E::Lost) => {
                self.source_state = Default::default(); self.paused = false;
                self.lost = true;
                S::Reconnecting
            }
            (s, E::Paused(p)) => {
                self.paused = p;
                self.source_state = meshlive::stream_watch::PlaybackState { paused: p, ..Default::default() };
                s
            }
            (s, E::SourceState(state)) => {
                self.paused = state.paused;
                self.source_state = state;
                s
            }
            (S::Playing, E::PictureStalled(v)) => {
                self.stalled = v;
                S::Playing
            }
            (s, _) => s,
        };
    }
    pub fn stop(&mut self) {
        self.state = WatchState::Ended;
    }
    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
    }
    /// Android audio_live_listen_state_* / watch_status_* wording.
    pub fn status(&self) -> String {
        let radio = self.stream.audio_only;
        match self.state {
            WatchState::Connecting if radio => "Connecting\u{2026}".into(),
            WatchState::Connecting => "Connecting to live video".into(),
            WatchState::Reconnecting if radio => "Reconnecting\u{2026}".into(),
            WatchState::Reconnecting => VIEWER_SIGNAL_LOST.into(),
            WatchState::Ended => "Ended".into(),
            WatchState::Playing if self.paused => "Paused by broadcaster".into(),
            WatchState::Playing if !radio && self.source_state.camera_off && self.source_state.source_muted => "Camera off. Microphone muted".into(),
            WatchState::Playing if !radio && self.source_state.camera_off => "Camera off".into(),
            WatchState::Playing if !radio && self.source_state.source_muted => "Microphone muted".into(),
            WatchState::Playing if self.stalled => VIEWER_PICTURE_STALLED.into(),
            WatchState::Playing if radio && self.muted => "Live. Muted".into(),
            WatchState::Playing => "Live".into(),
        }
    }
    pub fn route_label(&self) -> String {
        match self.route {
            Reach::Direct => "Watching directly".into(),
            Reach::Via(n) => format!("Watching via {n} phones"),
        }
    }
}

// ------------------------------------------------------------------- calls

/// One call screen: the core controller plus what the screen shows. Messages the
/// backend must send accumulate in the outbox.
pub struct CallModel {
    ctl: Controller,
    pub peer: String,
    pub verified: bool,
    pub outgoing: bool,
    /// Voice offer (caller's camera off) or voice answer.
    pub voice: bool,
    pub camera: bool,
    pub peer_camera: bool,
    pub talking: bool,
    pub muted: bool,
    pub ended: Option<Reason>,
    pub camera_started: bool,
    /// The link is to a phone Windows is paired with (identity-bound).
    pub bonded: bool,
    /// Why the call never got past setup (shown instead of "Call dropped: the connection was lost").
    pub setup_failure: Option<String>,
    /// While the call is being set up: "Looking for Asha...".
    pub looking: Option<String>,
    outbox: Vec<Message>,
}
impl CallModel {
    fn base(ctl: Controller, peer: &str, verified: bool, outgoing: bool) -> Self {
        Self {
            ctl,
            peer: peer.into(),
            verified,
            outgoing,
            voice: false,
            camera: true,
            peer_camera: true,
            talking: false,
            muted: false,
            ended: None,
            camera_started: false,
            bonded: false,
            setup_failure: None,
            looking: None,
            outbox: vec![],
        }
    }
    pub fn outgoing(
        peer: &str,
        verified: bool,
        voice: bool,
        call_id: [u8; 16],
    ) -> Result<Self, call::Error> {
        let (ctl, messages) = Controller::outgoing(call_id, voice, 0)?;
        let mut model = Self::base(ctl, peer, verified, true);
        model.voice = voice;
        model.camera = !voice;
        model.outbox = messages;
        Ok(model)
    }
    pub fn incoming(peer: &str, verified: bool, invite: &Message) -> Result<Self, call::Error> {
        let (ctl, actions) = Controller::incoming(invite)?;
        let mut model = Self::base(ctl, peer, verified, false);
        model.apply(actions);
        Ok(model)
    }
    pub fn phase(&self) -> Phase {
        self.ctl.phase()
    }
    pub fn take_outbox(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.outbox)
    }
    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(m) => self.outbox.push(m),
                Action::Ring { voice } => self.voice = voice,
                Action::StartMedia { camera } => {
                    self.camera = camera;
                    self.camera_started = camera;
                }
                Action::PeerCamera(on) => self.peer_camera = on,
                Action::PeerMicrophone(_) => {}
                Action::Ended(reason) => self.ended = Some(reason),
            }
        }
        if self.ctl.phase() == Phase::Ended && self.ended.is_none() {
            self.ended = Some(Reason::Normal);
        }
    }
    pub fn on_remote(&mut self, message: &Message) -> Result<(), call::Error> {
        let actions = self.ctl.on_message(message)?;
        self.apply(actions);
        Ok(())
    }
    pub fn accept(&mut self, voice: bool) -> Result<(), call::Error> {
        let actions = self.ctl.accept(voice)?;
        self.voice = voice;
        self.apply(actions);
        Ok(())
    }
    pub fn decline(&mut self) -> Result<(), call::Error> {
        self.decline_with(Reason::Rejected)
    }
    pub fn decline_with(&mut self, reason: Reason) -> Result<(), call::Error> {
        let m = self.ctl.decline(reason)?;
        self.outbox.push(m);
        self.ended = Some(reason);
        Ok(())
    }
    /// Cancel while ringing out, hang up otherwise (the End button).
    pub fn end(&mut self) -> Result<(), call::Error> {
        let m = match self.ctl.phase() {
            Phase::OutgoingInvite | Phase::OutgoingRinging => self.ctl.cancel()?,
            _ => self.ctl.hang_up(Reason::Normal)?,
        };
        self.outbox.push(m);
        self.ended = Some(Reason::Normal);
        Ok(())
    }
    pub fn media_ready(&mut self) -> Result<(), call::Error> {
        self.ctl.media_ready()
    }
    /// The call failed before it was set up; `text` is what the user reads.
    pub fn setup_failed(&mut self, text: String) {
        if self.ctl.link_lost().is_ok() {
            self.ended = Some(Reason::Error);
            self.setup_failure = Some(text);
        }
    }
    pub fn link_lost(&mut self) {
        if self.ctl.link_lost().is_ok() {
            self.ended = Some(Reason::Error);
        }
    }
    /// The Mute / Unmute button.
    pub fn toggle_mute(&mut self) -> Result<(), call::Error> {
        let mute = !self.muted;
        let m = self.ctl.push_to_talk(!mute)?;
        self.outbox.push(m);
        self.muted = mute;
        self.talking = false;
        Ok(())
    }
    /// Push-to-talk: while muted, holding Talk opens the microphone and releasing
    /// closes it again.
    pub fn talk(&mut self, pressed: bool) -> Result<(), call::Error> {
        if !self.muted || pressed == self.talking {
            return Ok(());
        }
        let m = self.ctl.push_to_talk(pressed)?;
        self.outbox.push(m);
        self.talking = pressed;
        Ok(())
    }
    pub fn toggle_camera(&mut self) -> Result<(), call::Error> {
        let on = !self.camera;
        let m = self.ctl.set_camera(on)?;
        self.outbox.push(m);
        self.camera = on;
        Ok(())
    }
    /// For the capture owner (not wired: capture is the user's decision).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sending_audio(&self) -> bool {
        self.ctl.should_send_audio()
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sending_video(&self) -> bool {
        self.ctl.should_send_video()
    }
    /// Android call_* wording.
    pub fn status(&self) -> String {
        match self.phase() {
            Phase::OutgoingInvite => self.looking.clone().unwrap_or_else(|| "Calling\u{2026}".into()),
            Phase::OutgoingRinging => "Ringing\u{2026}".into(),
            Phase::IncomingRinging if self.voice => "Incoming voice call".into(),
            Phase::IncomingRinging => "Incoming video call".into(),
            Phase::NegotiatingMedia => "Connecting\u{2026}".into(),
            Phase::Active if self.talking => "Talking\u{2026}".into(),
            Phase::Active if self.muted => "Muted".into(),
            Phase::Active if !self.camera && !self.peer_camera => "Voice call".into(),
            Phase::Active => "Video call".into(),
            Phase::Ended => match self.ended {
                Some(Reason::Rejected) => "Declined".into(),
                Some(Reason::Busy) => "Busy".into(),
                Some(Reason::Unsupported) => "Not supported".into(),
                Some(Reason::Error) => self.setup_failure.clone().unwrap_or_else(|| "Call dropped: the connection was lost".into()),
                _ => "Call ended".into(),
            },
        }
    }
}

/// A bare signal message, for backends and tests acting as the remote side.
pub fn remote(call_id: [u8; 16], signal: Signal) -> Message {
    Message::simple(call_id, signal).expect("simple signal")
}

// ----------------------------------------------------------------- go live

pub const TITLE_MAX_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveKind {
    Video,
    AudioOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveState {
    Setup,
    Starting,
    Live,
    Paused,
    Ended,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveEvent {
    Started,
    Viewers(u32),
    /// The source could not start (permission, hardware, radio).
    Failed,
}

#[derive(Clone, Debug)]
pub struct GoLiveModel {
    pub kind: LiveKind,
    pub title: String,
    pub state: LiveState,
    pub viewers: u32,
    pub muted: bool,
    pub camera_off: bool,
    /// Audio-only broadcasts show a disclosure first (audio_live_disclosure_*).
    pub disclosed: bool,
    pub error: Option<&'static str>,
    /// When the broadcast went live (the timer); None while not live.
    pub live_since: Option<Instant>,
    /// Audio only: the "Still broadcasting" reminder (Android
    /// `AudioLiveReminderPolicy`); None for video and while not live.
    pub reminder: Option<ReminderPolicy>,
    /// The microphone level for the meter, 0..1 (audio only).
    pub level: f32,
}
impl Default for GoLiveModel {
    fn default() -> Self {
        Self {
            kind: LiveKind::Video,
            title: String::new(),
            state: LiveState::Setup,
            viewers: 0,
            muted: false,
            camera_off: false,
            disclosed: false,
            error: None,
            live_since: None,
            reminder: None,
            level: 0.0,
        }
    }
}
impl GoLiveModel {
    pub fn title_valid(&self) -> bool {
        self.title.len() <= TITLE_MAX_BYTES && !self.title.chars().any(char::is_control)
    }
    /// True when Start may be pressed.
    pub fn can_start(&self) -> bool {
        matches!(self.state, LiveState::Setup | LiveState::Ended)
            && self.title_valid()
            && (self.kind == LiveKind::Video || self.disclosed)
    }
    pub fn start(&mut self) -> bool {
        if !self.can_start() {
            return false;
        }
        self.state = LiveState::Starting;
        self.viewers = 0;
        self.error = None;
        true
    }
    pub fn on(&mut self, event: LiveEvent) {
        match (self.state, event) {
            (LiveState::Starting, LiveEvent::Started) => self.state = LiveState::Live,
            (LiveState::Starting, LiveEvent::Failed) => {
                self.state = LiveState::Setup;
                self.error = Some("Couldn't start. Nothing is being shared.");
            }
            (LiveState::Live | LiveState::Paused, LiveEvent::Viewers(n)) => self.viewers = n,
            (LiveState::Live | LiveState::Paused, LiveEvent::Failed) => {
                self.state = LiveState::Ended
            }
            _ => {}
        }
    }
    /// `on` with the clock: the timer starts when the broadcast goes live,
    /// an audio-only broadcast starts its reminder then, and the listener
    /// count feeds the reminder. Anything that ends the broadcast clears both.
    pub fn on_at(&mut self, event: LiveEvent, now: Instant, limits: ReminderLimits) {
        let before = self.state;
        self.on(event);
        if before == LiveState::Starting && self.state == LiveState::Live {
            self.live_since = Some(now);
            self.reminder = (self.kind == LiveKind::AudioOnly).then(|| ReminderPolicy::new(now, limits));
        }
        if let (LiveEvent::Viewers(count), Some(reminder)) = (event, self.reminder.as_mut()) {
            reminder.on_listeners(count, now);
        }
        if !self.running() {
            self.live_since = None;
            self.reminder = None;
            self.level = 0.0;
        }
    }
    /// Whole seconds on air (the timer), None while not live.
    pub fn elapsed(&self, now: Instant) -> Option<u64> {
        self.live_since.map(|since| now.saturating_duration_since(since).as_secs())
    }
    pub fn toggle_pause(&mut self) {
        self.state = match self.state {
            LiveState::Live => LiveState::Paused,
            LiveState::Paused => LiveState::Live,
            s => s,
        };
    }
    pub fn stop(&mut self) {
        if matches!(
            self.state,
            LiveState::Starting | LiveState::Live | LiveState::Paused
        ) {
            self.state = LiveState::Ended;
            self.viewers = 0;
        }
        self.live_since = None;
        self.reminder = None;
        self.level = 0.0;
    }
    pub fn running(&self) -> bool {
        matches!(
            self.state,
            LiveState::Starting | LiveState::Live | LiveState::Paused
        )
    }
    /// The state byte a `live::Source` announces to viewers (for the going-live
    /// backend, not wired: this PC is not connectable).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn wire_state(&self) -> Option<u8> {
        match self.state {
            LiveState::Live => Some(meshlive::live::LIVE),
            LiveState::Paused => Some(meshlive::live::PAUSED),
            LiveState::Ended => Some(meshlive::live::ENDED),
            _ => None,
        }
    }
    /// Android live_status_* / audio_live_status_* wording.
    pub fn status(&self) -> String {
        let audio = self.kind == LiveKind::AudioOnly;
        match self.state {
            LiveState::Setup | LiveState::Ended => "Not live".into(),
            LiveState::Starting => "Starting\u{2026}".into(),
            LiveState::Live if audio && self.viewers == 0 => "Live. Waiting for listeners".into(),
            LiveState::Live if audio => format!("Live. {} listening", self.viewers),
            LiveState::Live => format!("Live, {} watching", self.viewers),
            LiveState::Paused => format!("Paused, {} watching", self.viewers),
        }
    }
}

/// The live timer as Android shows it: "4:07", "1:02:03".
pub fn live_timer(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// Android `AudioLiveReminderPolicy`'s times: remind after 30 min with no
/// listener or 2 h on air, stop 5 min after an unanswered reminder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReminderLimits {
    pub no_listener: Duration,
    pub total: Duration,
    pub unanswered_stop: Duration,
}
impl ReminderLimits {
    pub const ANDROID: Self = Self {
        no_listener: Duration::from_secs(30 * 60),
        total: Duration::from_secs(2 * 60 * 60),
        unanswered_stop: Duration::from_secs(5 * 60),
    };
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderAction {
    None,
    /// Show "Still broadcasting".
    Remind,
    /// Nobody answered the reminder: stop the broadcast.
    Stop,
}
/// A forgotten open microphone is a privacy risk: an audio broadcast reminds
/// its owner and stops itself when nobody answers (Android
/// `AudioLiveReminderPolicy`, the same rules).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderPolicy {
    limits: ReminderLimits,
    last_answered: Instant,
    empty_since: Option<Instant>,
    reminded_at: Option<Instant>,
}
impl ReminderPolicy {
    pub fn new(start: Instant, limits: ReminderLimits) -> Self {
        Self { limits, last_answered: start, empty_since: Some(start), reminded_at: None }
    }
    pub fn on_listeners(&mut self, count: u32, now: Instant) {
        if count > 0 {
            self.empty_since = None;
        } else if self.empty_since.is_none() {
            self.empty_since = Some(now);
        }
    }
    /// The user answered the reminder: the clocks start again.
    pub fn on_user_present(&mut self, now: Instant) {
        self.last_answered = now;
        self.reminded_at = None;
        if self.empty_since.is_some() {
            self.empty_since = Some(now);
        }
    }
    pub fn decide(&mut self, now: Instant) -> ReminderAction {
        if let Some(at) = self.reminded_at {
            return if now.saturating_duration_since(at) >= self.limits.unanswered_stop { ReminderAction::Stop } else { ReminderAction::None };
        }
        let total = now.saturating_duration_since(self.last_answered) >= self.limits.total;
        let empty = self.empty_since.is_some_and(|since| now.saturating_duration_since(since) >= self.limits.no_listener);
        if !(total || empty) {
            return ReminderAction::None;
        }
        self.reminded_at = Some(now);
        ReminderAction::Remind
    }
}

/// Android `HoldToTalkCore` for the call's Mute button: while muted, holding
/// Mute for `HOLD_SECONDS` opens the microphone until the button is let go;
/// a short press stays an ordinary click (Unmute). Times are the frame's
/// input time in seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HoldToTalk {
    pressed_at: Option<f64>,
    pub talking: bool,
}
/// What one frame of the hold means for the call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldStep {
    /// Some(true): start talking; Some(false): stop talking.
    pub talk: Option<bool>,
    /// This frame's click ended a hold: it is not a Mute toggle.
    pub swallow_click: bool,
}
impl HoldToTalk {
    pub const HOLD_SECONDS: f64 = 0.35;
    /// `down`: the pointer is held on Mute (and the window has focus);
    /// `can_start`: the call is muted.
    pub fn frame(&mut self, down: bool, can_start: bool, now: f64) -> HoldStep {
        if down {
            let since = *self.pressed_at.get_or_insert(now);
            if !self.talking && can_start && now - since >= Self::HOLD_SECONDS {
                self.talking = true;
                return HoldStep { talk: Some(true), swallow_click: false };
            }
            return HoldStep::default();
        }
        self.pressed_at = None;
        if self.talking {
            self.talking = false;
            return HoldStep { talk: Some(false), swallow_click: true };
        }
        HoldStep::default()
    }
    /// A hold is under way (the screen keeps repainting to time it).
    pub fn pressed(&self) -> bool {
        self.pressed_at.is_some()
    }
}

// ---------------------------------------------------------------- settings

/// Settings switches, with Android's defaults. These have no field, so a value
/// saved by an older build is ignored (settings_store): Friendly names and
/// "Show my name before they answer" (always on since 2026-11-04); Voice calls,
/// Play ringing tone and "Show my stream title to everyone nearby" (Android has
/// had no switch for them since 2026-10-17); "Relay video for others" (read by
/// nothing: this PC has no stream forwarder); the never-wired "Codes that
/// expire", "Accept older verification codes", "Visible by name" and "Visible to
/// contacts" (removed 2026-11-05, README "Switches").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switches {
    /// CF-CALL-24 "Block unknown callers" (default off).
    pub block_unknown: bool,
    /// Receive calls: needs the peripheral role (capability gate).
    pub receive_calls: bool,
    /// The first-run welcome was seen (saved; never shown again).
    pub onboarded: bool,
    /// Settings > Start with Windows (off by default): start in the tray at sign-in.
    pub start_with_windows: bool,
    /// The tray's one-time "keeps running" notice was shown (saved).
    pub tray_hint_shown: bool,
    /// "Not now" on "Declined. Block all unknown callers?" (Android
    /// `block_unknown_offer_dismissed`; saved, so it is never asked again).
    pub block_unknown_offer_dismissed: bool,
    /// Settings > Video quality > "Fill screen with video" (Android
    /// `videoFillSwitch`, on): the other person's picture fills the video area.
    pub video_fill: bool,
    /// Settings > Video quality > "Warn when video freezes" (Android
    /// `ViewerFeatures.frameAge`; Experimental, off).
    pub frame_age: bool,
    /// "Who sees my name" (Android `NamesAudience`, default everyone). Saved;
    /// read by the call code from WP4 part 2 (not shown before then).
    pub names_audience: meshlive::label_policy::NamesAudience,
}
impl Default for Switches {
    fn default() -> Self {
        Self {
            block_unknown: false,
            receive_calls: true,
            onboarded: false,
            start_with_windows: false,
            tray_hint_shown: false,
            block_unknown_offer_dismissed: false,
            video_fill: true,
            frame_age: false,
            names_audience: meshlive::label_policy::NamesAudience::Everyone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: [u8; 16] = [7; 16];

    fn stream(audio_only: bool) -> StreamEntry {
        StreamEntry {
            id: 1,
            from: "Priya".into(),
            title: None,
            audio_only,
            reach: Reach::Direct,
            paused: false,
            full: false,
            confirmed: true,
            unavailable: None,
        }
    }

    #[test]
    fn watching_goes_connecting_playing_reconnecting_playing_ended_and_ended_is_final() {
        let mut w = WatchModel::new(stream(false));
        assert_eq!(w.state, WatchState::Connecting);
        assert_eq!(w.status(), "Connecting to live video");
        w.on(WatchEvent::Lost);
        assert_eq!(w.state, WatchState::Connecting, "nothing to lose before it connected");
        w.on(WatchEvent::Connected);
        assert_eq!((w.state, w.status().as_str()), (WatchState::Playing, "Live"));
        w.on(WatchEvent::Paused(true));
        assert_eq!(w.status(), "Paused by broadcaster");
        w.on(WatchEvent::Paused(false));
        w.on(WatchEvent::PictureStalled(true));
        assert_eq!(w.status(), "Picture stalled \u{2014} waiting");
        w.on(WatchEvent::Lost);
        assert_eq!(
            (w.state, w.status().as_str()),
            (WatchState::Reconnecting, "Signal lost \u{2014} reconnecting")
        );
        w.on(WatchEvent::Connected);
        assert!(!w.stalled);
        assert_eq!(w.state, WatchState::Playing);
        w.on(WatchEvent::Ended);
        w.on(WatchEvent::Connected);
        assert_eq!((w.state, w.status().as_str()), (WatchState::Ended, "Ended"));
    }
    #[test]
    fn radio_uses_the_audio_wording_and_reconnect_can_give_up() {
        let mut w = WatchModel::new(stream(true));
        assert_eq!(w.status(), "Connecting\u{2026}");
        w.on(WatchEvent::Connected);
        w.toggle_mute();
        assert_eq!(w.status(), "Live. Muted");
        w.on(WatchEvent::Lost);
        assert_eq!(w.status(), "Reconnecting\u{2026}");
        w.on(WatchEvent::GaveUp);
        assert_eq!(w.state, WatchState::Ended);
        assert_eq!(stream(true).headline(), "Radio from Priya");
        assert_eq!(stream(false).headline(), "Live from Priya");
        let mut titled = stream(false);
        titled.title = Some("Evening".into());
        titled.confirmed = false;
        assert_eq!(titled.headline(), "Evening");
        assert!(titled.subtitle().ends_with("unconfirmed"));
    }

    #[test]
    fn an_outgoing_voice_call_rings_connects_mutes_talks_and_hangs_up() {
        let mut c = CallModel::outgoing("Ravi", true, true, ID).unwrap();
        assert_eq!(c.phase(), Phase::OutgoingInvite);
        let sent: Vec<_> = c.take_outbox().iter().map(|m| m.signal).collect();
        assert_eq!(sent, [Signal::Invite, Signal::CameraOff]);
        assert!(!c.camera);
        assert_eq!(c.status(), "Calling\u{2026}");
        c.on_remote(&remote(ID, Signal::Ringing)).unwrap();
        assert_eq!(c.status(), "Ringing\u{2026}");
        c.on_remote(&remote(ID, Signal::Accept)).unwrap();
        assert_eq!(c.phase(), Phase::NegotiatingMedia);
        assert_eq!(c.status(), "Connecting\u{2026}");
        assert!(!c.camera_started, "a voice call never starts the camera");
        c.media_ready().unwrap();
        c.on_remote(&remote(ID, Signal::CameraOff)).unwrap();
        assert_eq!(c.status(), "Voice call");
        assert!(c.sending_audio() && !c.sending_video());
        c.toggle_mute().unwrap();
        assert!(!c.sending_audio());
        assert_eq!(c.status(), "Muted");
        c.talk(true).unwrap();
        assert!(c.sending_audio());
        assert_eq!(c.status(), "Talking\u{2026}");
        c.talk(false).unwrap();
        assert!(!c.sending_audio());
        c.take_outbox();
        c.talk(false).unwrap();
        assert!(c.take_outbox().is_empty(), "releasing twice sends nothing");
        c.toggle_mute().unwrap();
        assert!(c.sending_audio());
        c.end().unwrap();
        assert_eq!(c.phase(), Phase::Ended);
        assert_eq!(c.status(), "Call ended");
        assert_eq!(c.take_outbox().last().unwrap().signal, Signal::HangUp);
        assert!(c.toggle_mute().is_err(), "controls do nothing after the call ended");
    }
    #[test]
    fn ringing_out_can_be_cancelled_and_a_decline_ends_the_call() {
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        c.take_outbox();
        c.end().unwrap();
        assert_eq!(c.take_outbox()[0].signal, Signal::Cancel);
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        let busy = Message::new(ID, Signal::Decline, Reason::Busy, 0).unwrap();
        c.on_remote(&busy).unwrap();
        assert_eq!(c.status(), "Busy");
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        c.link_lost();
        assert_eq!(c.status(), "Call dropped: the connection was lost");
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        c.setup_failed("Not found nearby".into());
        assert_eq!(c.status(), "Not found nearby");
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        assert!(c.on_remote(&remote([9; 16], Signal::Ringing)).is_err());
    }
    #[test]
    fn an_incoming_call_rings_and_accept_or_decline_follow_the_core_rules() {
        let invite = remote(ID, Signal::Invite);
        let mut c = CallModel::incoming("Priya", true, &invite).unwrap();
        assert_eq!(c.phase(), Phase::IncomingRinging);
        assert_eq!(c.take_outbox()[0].signal, Signal::Ringing);
        assert_eq!(c.status(), "Incoming video call");
        // The caller's Camera off while ringing makes it a voice offer.
        c.on_remote(&remote(ID, Signal::CameraOff)).unwrap();
        assert_eq!(c.status(), "Incoming voice call");
        c.accept(true).unwrap();
        let sent: Vec<_> = c.take_outbox().iter().map(|m| m.signal).collect();
        assert_eq!(sent, [Signal::Accept, Signal::CameraOff]);
        assert!(!c.camera_started);
        c.media_ready().unwrap();
        c.on_remote(&remote(ID, Signal::CameraOn)).unwrap();
        assert!(c.peer_camera);
        c.toggle_camera().unwrap();
        assert!(c.camera && c.sending_video());
        c.on_remote(&Message::new(ID, Signal::HangUp, Reason::Normal, 0).unwrap()).unwrap();
        assert_eq!(c.phase(), Phase::Ended);

        let mut d = CallModel::incoming("Priya", false, &invite).unwrap();
        d.decline().unwrap();
        assert_eq!(d.status(), "Declined");
        assert!(d.take_outbox().iter().any(|m| m.signal == Signal::Decline));
        let mut e = CallModel::incoming("Priya", false, &invite).unwrap();
        e.on_remote(&remote(ID, Signal::Cancel)).unwrap();
        assert_eq!(e.phase(), Phase::Ended);
        assert!(CallModel::incoming("x", false, &remote(ID, Signal::Accept)).is_err());
    }
    #[test]
    fn a_video_call_starts_the_camera_after_accept_only() {
        let mut c = CallModel::outgoing("Ravi", false, false, ID).unwrap();
        assert!(!c.camera_started);
        c.on_remote(&remote(ID, Signal::Accept)).unwrap();
        assert!(c.camera_started);
        c.media_ready().unwrap();
        assert_eq!(c.status(), "Video call");
        c.toggle_camera().unwrap();
        assert!(!c.sending_video());
    }

    #[test]
    fn going_live_needs_a_disclosure_for_audio_and_tracks_viewers_and_the_wire_state() {
        let mut g = GoLiveModel::default();
        assert_eq!((g.status().as_str(), g.can_start()), ("Not live", true));
        g.kind = LiveKind::AudioOnly;
        assert!(!g.can_start(), "audio-only asks before going live");
        g.disclosed = true;
        g.title = "x".repeat(TITLE_MAX_BYTES + 1);
        assert!(!g.can_start());
        g.title = "Evening updates".into();
        assert!(g.start());
        assert_eq!(g.state, LiveState::Starting);
        assert_eq!(g.wire_state(), None);
        g.on(LiveEvent::Viewers(3));
        assert_eq!(g.viewers, 0, "no viewers before it is live");
        g.on(LiveEvent::Started);
        assert_eq!(g.status(), "Live. Waiting for listeners");
        g.on(LiveEvent::Viewers(2));
        assert_eq!(g.status(), "Live. 2 listening");
        assert_eq!(g.wire_state(), Some(meshlive::live::LIVE));
        g.toggle_pause();
        assert_eq!(g.wire_state(), Some(meshlive::live::PAUSED));
        g.toggle_pause();
        g.stop();
        assert_eq!(
            (g.state, g.viewers, g.wire_state()),
            (LiveState::Ended, 0, Some(meshlive::live::ENDED))
        );
        assert!(g.can_start(), "a new broadcast can follow an ended one");
    }
    #[test]
    fn a_failed_start_returns_to_setup_with_a_plain_message() {
        let mut g = GoLiveModel::default();
        g.title = "bad\u{7}".into();
        assert!(!g.start());
        g.title.clear();
        assert!(g.start());
        g.on(LiveEvent::Failed);
        assert_eq!(g.state, LiveState::Setup);
        assert!(g.error.is_some());
        g.start();
        g.on(LiveEvent::Started);
        g.on(LiveEvent::Viewers(4));
        assert_eq!(g.status(), "Live, 4 watching");
        g.toggle_pause();
        assert_eq!(g.status(), "Paused, 4 watching");
    }
    #[test]
    fn an_audio_broadcast_reminds_after_30_minutes_alone_or_2_hours_and_stops_5_minutes_after_no_answer() {
        let start = Instant::now();
        let min = |m: u64| start + Duration::from_secs(m * 60);
        let mut p = ReminderPolicy::new(start, ReminderLimits::ANDROID);
        assert_eq!(p.decide(min(29)), ReminderAction::None);
        assert_eq!(p.decide(min(30)), ReminderAction::Remind, "30 min with nobody listening");
        assert_eq!(p.decide(min(34)), ReminderAction::None, "one reminder, then a wait");
        assert_eq!(p.decide(min(35)), ReminderAction::Stop, "5 min without an answer");
        let mut answered = ReminderPolicy::new(start, ReminderLimits::ANDROID);
        assert_eq!(answered.decide(min(30)), ReminderAction::Remind);
        answered.on_user_present(min(31));
        assert_eq!(answered.decide(min(40)), ReminderAction::None, "an answer starts the clocks again");
        assert_eq!(answered.decide(min(61)), ReminderAction::Remind);
        let mut heard = ReminderPolicy::new(start, ReminderLimits::ANDROID);
        heard.on_listeners(2, min(1));
        assert_eq!(heard.decide(min(119)), ReminderAction::None, "a listener keeps it going");
        assert_eq!(heard.decide(min(120)), ReminderAction::Remind, "2 h on air");
        let mut left = ReminderPolicy::new(start, ReminderLimits::ANDROID);
        left.on_listeners(1, min(1));
        left.on_listeners(0, min(50));
        assert_eq!(left.decide(min(79)), ReminderAction::None);
        assert_eq!(left.decide(min(80)), ReminderAction::Remind, "30 min after the last listener left");
    }
    #[test]
    fn holding_mute_talks_after_the_hold_time_and_a_short_press_stays_a_click() {
        let mut h = HoldToTalk::default();
        assert_eq!(h.frame(true, true, 0.0), HoldStep::default());
        assert_eq!(h.frame(true, true, 0.2), HoldStep::default());
        assert_eq!(h.frame(false, true, 0.25), HoldStep::default(), "a short press is an ordinary click");
        assert_eq!(h.frame(true, true, 1.0).talk, None);
        assert_eq!(h.frame(true, true, 1.35).talk, Some(true), "held long enough");
        assert!(h.talking);
        assert_eq!(h.frame(true, true, 5.0).talk, None, "talking until let go");
        assert_eq!(h.frame(false, true, 5.1), HoldStep { talk: Some(false), swallow_click: true });
        assert_eq!(h.frame(false, true, 5.2), HoldStep::default(), "the next click is a click again");
        assert_eq!(h.frame(true, false, 6.0).talk, None);
        assert_eq!(h.frame(true, false, 7.0).talk, None, "not muted: holding does nothing");
    }
    #[test]
    fn the_live_timer_and_the_reminder_start_when_an_audio_broadcast_goes_live() {
        let now = Instant::now();
        let mut g = GoLiveModel { kind: LiveKind::AudioOnly, disclosed: true, ..GoLiveModel::default() };
        assert!(g.start());
        assert_eq!(g.elapsed(now), None);
        g.on_at(LiveEvent::Started, now, ReminderLimits::ANDROID);
        assert_eq!(g.elapsed(now + Duration::from_secs(65)), Some(65));
        assert_eq!(live_timer(65), "1:05");
        assert_eq!(live_timer(3_723), "1:02:03");
        assert!(g.reminder.is_some());
        g.stop();
        assert_eq!((g.elapsed(now), g.reminder.is_some()), (None, false));
        let mut video = GoLiveModel::default();
        assert!(video.start());
        video.on_at(LiveEvent::Started, now, ReminderLimits::ANDROID);
        assert!(video.live_since.is_some() && video.reminder.is_none(), "video has a timer but no reminder");
    }
    #[test]
    fn switch_defaults_match_android() {
        let s = Switches::default();
        assert!(s.receive_calls, "Allow calls is on, as Android");
        assert!(!s.block_unknown && !s.start_with_windows);
    }
}
