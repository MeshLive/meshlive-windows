//! MeshLive Windows interface: egui on Direct3D 11 with AccessKit. The screens
//! follow Android/macOS (Home, Calls, Contacts, Settings, with Watch, Call and
//! Go live taking over the window). All state lives in `model`; the core is
//! reached only through a `Backend` (no radio, camera, microphone or audio
//! endpoint is opened from here).
pub mod backend;
pub mod components;
mod android_icons;
mod icons;
pub mod model;
mod screens;
mod settings;
mod vector;
pub mod theme;
#[cfg(feature = "test-hooks")]
pub(crate) mod test_hooks;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod native_call_check;

use backend::{Backend, CallEndReason, Command, OfflineBackend, Update};
#[allow(unused_imports)]
use components::{Kind, Leading, ListRow, Pill, ToggleRow, Tone, Trailing, divider, live_badge};
use egui::{Color32, FontFamily, RichText};
use icons::{Icon, draw_icon};
use meshlive::{
    bluetooth::gate::{Availability, Feature},
    call::Phase,
};
use model::*;

// Older names of the theme tokens, kept for the screens.
const SECONDARY: Color32 = theme::TEXT_SECONDARY;
#[allow(dead_code)]
const CARD: Color32 = theme::SURFACE;
const LINE: Color32 = theme::DIVIDER;
const RED: Color32 = theme::LIVE;
/// While the adapter capabilities are still being read.
const CHECKING_BLUETOOTH: &str = "Checking Bluetooth\u{2026}";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Home,
    Calls,
    Contacts,
    Settings,
}
impl Page {
    #[cfg(test)]
    pub const ALL: [Page; 4] = [Page::Home, Page::Calls, Page::Contacts, Page::Settings];
    pub fn label(self) -> &'static str {
        match self {
            Page::Home => "Live",
            Page::Calls => "Calls",
            Page::Contacts => "Contacts",
            Page::Settings => "Settings",
        }
    }
}

/// What "Ready" means for calls: Windows stops announcing this PC while it
/// sleeps, so phones reach it only while it is awake.
/// Settings > Your name while no name is set.
/// Who gets this PC's name (what the PC does today; GAPS: an audience choice).
pub(crate) const NAME_AUDIENCE: &str = "Your name is never broadcast. It's sent, encrypted, to people you call and to contacts who call you. Others see a generated name.";
/// The empty Calls list (until Find PCs is on by default on Android).
pub(crate) const FIND_PCS_HINT: &str = "Both phones need a current MeshLive. On the phone: Settings \u{203a} Advanced \u{203a} Connection \u{203a} turn on Find PCs.";
pub(crate) const NAME_NOT_SET: &str = "Not set. People nearby see a random name that changes every 15 minutes.";
/// Why the generated name (the (i) after "You appear as").
pub(crate) const APPEAR_AS_INFO: &str = "People nearby who haven't added you see this made-up name instead of an ID. It comes from this PC's private Bluetooth ID, which changes every 15 minutes, so nobody nearby can follow this PC over time. Your contacts see your name.";
pub(crate) const AWAKE_NOTE: &str = "Reachable while this PC is awake";
/// The words in the backend's damaged-keys note that make the Calls page
/// offer "Reset" (radio_backend `PRESENCE_DAMAGED_NOTE`).
pub(crate) const KEYS_DAMAGED: &str = "contact keys are damaged";

/// The Settings categories (a wide window lists them on the left, like
/// Windows Settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    Profile,
    Calls,
    Privacy,
    Bluetooth,
    About,
}
impl SettingsSection {
    pub const ALL: [SettingsSection; 5] =
        [SettingsSection::Profile, SettingsSection::Calls, SettingsSection::Privacy, SettingsSection::Bluetooth, SettingsSection::About];
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Profile => "Profile",
            SettingsSection::Calls => "Calls",
            SettingsSection::Privacy => "Privacy",
            SettingsSection::Bluetooth => "Bluetooth",
            SettingsSection::About => "About",
        }
    }
    pub fn icon(self) -> Icon {
        match self {
            SettingsSection::Profile => Icon::Person,
            SettingsSection::Calls => Icon::Phone,
            SettingsSection::Privacy => Icon::Lock,
            SettingsSection::Bluetooth => Icon::Bluetooth,
            SettingsSection::About => Icon::Info,
        }
    }
}

/// A finished call in the Recent list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentCall {
    pub peer: String,
    pub voice: bool,
    pub outgoing: bool,
    pub result: String,
    /// A saved contact can be called back (a stranger's rotating ID is gone).
    pub contact: Option<Handle>,
    /// A caller that presented its real key can be blocked (CF-CALL-24).
    pub fingerprint: Option<[u8; 32]>,
    pub blocked: bool,
}

/// The Add contact screen: paste the code the other phone shows.
#[derive(Default)]
pub struct Meet {
    pub code: String,
    pub state: Option<MeetEvent>,
    /// The name to save for the person whose screen was scanned.
    pub name: String,
    pub own_code: Option<String>,
    /// The latest event of the shown code ("Asha scanned your code").
    pub code_note: Option<String>,
    /// The QR picture of `own_code` (encoded once per code).
    pub qr: Option<(String, meshlive::qr_encode::QrCode)>,
    /// "Scan with camera" is on (Command::QrCamera(true) was sent).
    pub camera: bool,
}

pub struct Ui {
    pub bluetooth: Option<meshlive::bluetooth::Capabilities>,
    /// Set only by an explicit advertisement start; never by a background retry.
    pub peripheral_proof: meshlive::bluetooth::gate::PeripheralProof,
    pub page: Page,
    pub texture_visible: bool,
    /// The decoded video texture the integrator publishes while playing.
    pub video: Option<egui::TextureId>,
    /// The peer's pictures in a call: the slot, the last shown sequence, the
    /// texture, the picture size and rotation.
    peer_pictures: Option<backend::PictureHandle>,
    #[cfg(feature = "test-hooks")]
    test: test_hooks::State,
    peer_seen: u64,
    peer_texture: Option<egui::TextureHandle>,
    pub peer_size: [u32; 2],
    pub peer_turns: u8,
    backend: Box<dyn Backend>,
    #[cfg(test)]
    native_check: Option<native_call_check::State>,
    demo: bool,
    searching: bool,
    /// Why nothing can be found or called right now (Bluetooth off).
    bluetooth_note: Option<String>,
    /// The current call's microphone (None: not opened yet; Some(None): on;
    /// Some(Some(reason)): unusable).
    pub call_mic: Option<Option<String>>,
    call_mic_restarting: bool,
    hold_call_talk: bool,
    /// CF-OUT-07: the effective sending and receiving media of this call.
    pub call_sending: Option<String>,
    pub call_receiving: Option<String>,
    /// Hide shared video-header metadata while that direction's camera is off.
    accurate_call_media: bool,
    /// The wipe confirmation being typed (Some while the dialog is open).
    pub wipe_confirm: Option<String>,
    /// What the wipe did (the app closes after showing it).
    pub wiped: Option<meshlive::wipe::Report>,
    /// Safety codes the user asked to see, per contact.
    pub safety_codes: std::collections::HashMap<Handle, String>,
    /// The current call's camera, as `call_mic`.
    pub call_camera: Option<Option<String>>,
    /// Design QW: phones nearby can call this PC now; else why not (None: no word yet).
    pub pc_visible: bool,
    pub visibility_note: Option<String>,
    streams: Vec<StreamEntry>,
    people: Vec<Person>,
    contacts: Vec<Contact>,
    recent: Vec<RecentCall>,
    recent_tab: bool,
    /// CF-PRIV-22: a call to a copied ID waits for "Call anyway".
    pub copied_confirm: Option<(Handle, bool)>,
    /// An incoming call rang: (peer, video) for the taskbar flash and toast.
    incoming_alert: Option<(String, bool)>,
    /// Last People visibility sent to the backend.
    people_visible_sent: bool,
    watch: Option<WatchModel>,
    watch_note: Option<String>,
    streams_visible_sent: bool,
    call: Option<CallModel>,
    call_recorded: bool,
    /// The contact the current call is to (for Recent call-back).
    call_contact: Option<Handle>,
    /// The incoming caller's full fingerprint (real key only), for Recent.
    call_fingerprint: Option<[u8; 32]>,
    meet: Option<Meet>,
    golive: GoLiveModel,
    golive_open: bool,
    audio_disclosure: bool,
    live_exposure: bool,
    live_consent_required: bool,
    live_settings: meshlive::stream_source::Settings,
    live_note: Option<String>,
    /// This PC's saved name (Settings > Profile).
    pub own_name: Option<String>,
    /// This PC's ID (`SSSSS-SSSSS`), once the backend has read it.
    pub own_id: Option<String>,
    /// The generated name phones nearby see for this PC now (on air only).
    pub own_generated: Option<String>,
    /// The generated name the caller of the ringing call saw (a stranger's call).
    pub answer_as: Option<String>,
    /// The person or contact selected in a wide window's list.
    pub selected_person: Option<Handle>,
    pub selected_contact: Option<Handle>,
    /// The Settings category shown in a wide window.
    pub settings_section: SettingsSection,
    /// Experimental, off by default (`MESHLIVE_UI_SPLIT_PANES=1`): list +
    /// detail panes on Calls and Contacts and a category list in Settings on
    /// wide windows. Replaced by single scaling columns after design review
    /// (2026-10-21: two panes made every page look divided).
    pub split_panes: bool,
    /// The name being typed, and why the last one was refused.
    pub name_edit: Option<String>,
    pub name_error: Option<String>,
    /// The first-run welcome's name field.
    welcome_name: String,
    pub switches: Switches,
    dialog: Option<(String, String)>,
    contacts_problem: Option<StoreProblem>,
    /// Rename dialog: contact, text being edited, last refusal.
    rename: Option<(Handle, String, Option<&'static str>)>,
    forget: Option<Handle>,
    reset_contacts: bool,
    /// Focus the primary action once when a new screen appears.
    focus_primary: bool,
    /// The last run crashed and its report waits (the one-line prompt).
    pub crash_prompt: bool,
    /// Why the current call ended, when the backend said (ring timeouts).
    pub call_end: Option<CallEndReason>,
}

impl Default for Ui {
    fn default() -> Self {
        Self::with_backend(Box::new(OfflineBackend::default()))
    }
}

impl Ui {
    pub fn with_backend(backend: Box<dyn Backend>) -> Self {
        Self {
            bluetooth: None,
            peripheral_proof: Default::default(),
            page: Page::Home,
            texture_visible: false,
            peer_pictures: None,
            #[cfg(feature = "test-hooks")]
            test: Default::default(),
            peer_seen: 0,
            peer_texture: None,
            peer_size: [16, 9],
            peer_turns: 0,
            video: None,
            backend,
            #[cfg(test)]
            native_check: (std::env::var("MESHLIVE_NATIVE_UI_CHECK").as_deref() == Ok("1")).then(native_call_check::State::new),
            demo: false,
            searching: true,
            bluetooth_note: None,
            call_mic: None,
            call_mic_restarting: false,
            hold_call_talk: std::env::var("MESHLIVE_CALL_TALK_HOLD").as_deref() == Ok("1"),
            call_sending: None,
            call_receiving: None,
            accurate_call_media: std::env::var("MESHLIVE_CALL_MEDIA_FACTS").as_deref() == Ok("1"),
            safety_codes: Default::default(),
            wipe_confirm: None,
            wiped: None,
            call_camera: None,
            pc_visible: false,
            visibility_note: None,
            streams: vec![],
            people: vec![],
            contacts: vec![],
            recent: vec![],
            recent_tab: false,
            people_visible_sent: false,
            incoming_alert: None,
            copied_confirm: None,
            watch: None,
            watch_note: None,
            streams_visible_sent: false,
            call: None,
            call_recorded: false,
            call_contact: None,
            call_fingerprint: None,
            meet: None,
            golive: GoLiveModel::default(),
            golive_open: false,
            audio_disclosure: false,
            live_exposure: false,
            live_consent_required: meshlive::stream_source::enabled(),
            live_settings: Default::default(),
            live_note: None,
            own_name: None,
            own_id: None,
            own_generated: None,
            answer_as: None,
            selected_person: None,
            selected_contact: None,
            settings_section: SettingsSection::Profile,
            split_panes: std::env::var("MESHLIVE_UI_SPLIT_PANES").ok().as_deref() == Some("1"),
            name_edit: None,
            name_error: None,
            welcome_name: String::new(),
            switches: Switches::default(),
            dialog: None,
            contacts_problem: None,
            rename: None,
            forget: None,
            reset_contacts: false,
            focus_primary: false,
            crash_prompt: false,
            call_end: None,
        }
    }
    /// Sample people and streams from the scripted fake core (Experimental,
    /// off by default; `--demo` or Settings > Preview and diagnostics).
    pub fn set_demo(&mut self, on: bool) {
        if on == self.demo {
            return;
        }
        self.demo = on;
        self.backend = if on {
            Box::new(backend::ScriptedBackend::demo(std::time::Duration::from_millis(600)))
        } else {
            Box::new(OfflineBackend::default())
        };
        self.streams.clear();
        self.people.clear();
        self.contacts.clear();
        self.watch = None;
        self.call = None;
    }

    // ---- capability gate -------------------------------------------------

    pub fn availability(&self, feature: Feature) -> Option<Availability> {
        self.bluetooth.map(|caps| {
            meshlive::bluetooth::gate::availability(&caps, self.peripheral_proof, feature)
        })
    }
    /// True when the feature can be used now (or only needs its first
    /// advertising start, which happens when the user turns it on).
    pub fn usable(&self, feature: Feature) -> bool {
        matches!(
            self.availability(feature),
            Some(Availability::Available | Availability::NeedsAdvertisingCheck)
        )
    }
    /// The exact reason text, when the feature is not usable.
    pub fn unavailable_reason(&self, feature: Feature) -> Option<&'static str> {
        match self.availability(feature) {
            Some(Availability::Unavailable(reason)) => Some(reason),
            None => Some(CHECKING_BLUETOOTH),
            _ => None,
        }
    }

    // ---- core events -----------------------------------------------------

    /// Applies every update the backend has ready. Called once per frame.
    pub fn pump(&mut self) {
        for update in self.backend.poll() {
            self.apply(update);
        }
        self.flush_call();
        #[cfg(test)]
        if let Some(mut check) = self.native_check.take() {
            check.tick(self);
            self.native_check = Some(check);
        }
    }
    #[cfg(test)]
    pub(crate) fn native_check_done(&self) -> bool {
        self.native_check.as_ref().is_some_and(native_call_check::State::done)
    }
    #[cfg(test)]
    pub(crate) fn native_check_armed(&self) -> bool { self.native_check.is_some() }
    #[cfg(test)]
    pub(crate) fn native_check_take_focus_request(&mut self) -> bool {
        self.native_check.as_mut().is_some_and(native_call_check::State::take_focus_request)
    }
    #[cfg(test)]
    pub(crate) fn native_check_input(&mut self, input: &egui::RawInput) -> Vec<egui::Event> {
        self.native_check.as_mut().map_or_else(Vec::new, |check| check.input(input))
    }
    #[cfg(test)]
    pub(crate) fn native_check_result(&self) -> Result<(), String> {
        self.native_check.as_ref().ok_or("Native UI check was not armed")?.finish()
    }
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Searching(on) => self.searching = on,
            Update::BluetoothNote(note) => self.bluetooth_note = note,
            Update::Streams(list) => {
                self.streams = list;
                self.searching = false;
            }
            Update::People(list) => self.people = list,
            Update::WatchAvailable(_) => {},
            Update::WatchNote(note) => self.watch_note = note,
            Update::WatchRoute(hops) => if let Some(w) = self.watch.as_mut() { w.route = if hops == 0 { Reach::Direct } else { Reach::Via(hops) }; },
            Update::WatchTitle(title) => if let Some(w) = self.watch.as_mut() { w.stream.title = Some(title); },
            Update::Contacts(list) => self.contacts = list,
            Update::ContactsProblem(problem) => self.contacts_problem = problem,
            Update::Renamed { contact, outcome } => {
                if self.rename.as_ref().is_some_and(|r| r.0 == contact) {
                    match outcome.message() {
                        None => self.rename = None,
                        Some(message) => self.rename.as_mut().unwrap().2 = Some(message),
                    }
                }
            }
            Update::Watch(event) => {
                if let Some(watch) = self.watch.as_mut() {
                    watch.on(event);
                }
            }
            Update::CallSignal(message) => {
                if let Some(call) = self.call.as_mut() {
                    if call.on_remote(&message).is_ok() && self.accurate_call_media
                        && matches!(message.signal, meshlive::call::Signal::CameraOff | meshlive::call::Signal::CameraOn) {
                        // Ringing produces a voice-offer action rather than
                        // PeerCamera; keep the displayed camera state in sync.
                        call.peer_camera = message.signal == meshlive::call::Signal::CameraOn;
                    }
                }
            }
            Update::CallSearchSuperseded => {
                if self.call.as_ref().is_some_and(|c| c.outgoing && c.phase() == Phase::OutgoingInvite) {
                    self.call = None;
                    self.call_end = None;
                    self.call_contact = None;
                    self.call_recorded = false;
                }
            }
            Update::CallBonded => {
                if let Some(call) = self.call.as_mut() {
                    call.bonded = true;
                }
            }
            Update::CallMic(state) => self.call_mic = Some(state),
            Update::CallCaptureStarting => { self.call_mic = None; self.call_mic_restarting = true; }
            Update::CallCaptureReady => self.call_mic_restarting = false,
            Update::CallMediaInfo { direction, text } => {
                if direction == "sending" {
                    self.call_sending = Some(text);
                } else {
                    self.call_receiving = Some(text);
                }
            }
            Update::TestDial(contact) => {
                #[cfg(feature = "test-hooks")]
                self.test_dial(contact);
                #[cfg(not(feature = "test-hooks"))]
                let _ = contact;
            }
            Update::DiagnosticsSaved(result) => {
                self.dialog = Some(match result {
                    Ok(path) if crate::diagnostics::private_export_enabled() => ("Diagnostics saved".into(), format!("The report is in {path}. It holds versions, settings and scrubbed crash text if available. Device descriptions and technical trace lines are omitted. Attach it to your problem report.")),
                    Ok(path) => ("Diagnostics saved".into(), format!("The report is in {path}. It holds versions, Bluetooth adapter facts, your settings and recent technical log lines; no names, keys, codes or contacts. Attach it to your problem report.")),
                    Err(reason) => ("Diagnostics not saved".into(), reason),
                });
            }
            Update::CallPictures(handle) => {
                self.peer_seen = 0;
                self.peer_pictures = Some(handle);
            }
            Update::CallPeerName(name) => {
                if let Some(call) = self.call.as_mut() {
                    if !call.verified {
                        call.peer = name;
                    }
                }
            }
            Update::OwnName { name, error } => {
                if error.is_none() {
                    self.name_edit = None;
                }
                self.own_name = name;
                self.name_error = error;
            }
            Update::Wiped(report) => {
                self.own_name = None;
                self.wipe_confirm = None;
                self.wiped = Some(report);
            }
            Update::SafetyCode { contact, code } => {
                self.safety_codes.insert(contact, code);
            }
            Update::CallCamera(state) => self.call_camera = Some(state),
            Update::CallMediaReady => {
                if let Some(call) = self.call.as_mut() {
                    let _ = call.media_ready();
                }
            }
            Update::CallLinkLost => {
                if let Some(call) = self.call.as_mut() {
                    call.link_lost();
                }
            }
            Update::CallSetupFailed(text) => {
                if let Some(call) = self.call.as_mut() {
                    call.setup_failed(text);
                }
            }
            Update::CallStatus(text) => {
                if let Some(call) = self.call.as_mut() {
                    call.looking = text;
                }
            }
            Update::Meet(event) => {
                if let Some(meet) = self.meet.as_mut() {
                    // The camera closes when a code was read or it cannot be used.
                    if matches!(&event, MeetEvent::Searching) || matches!(&event, MeetEvent::Failed(t) if !t.contains("isn't a MeshLive code")) {
                        meet.camera = false;
                    }
                    if let MeetEvent::Ask { name } = &event {
                        if meet.name.is_empty() {
                            meet.name = name.clone();
                        }
                    }
                    meet.state = Some(event);
                }
            }
            Update::OwnCode(code) => {
                if let Some(meet) = self.meet.as_mut() {
                    meet.own_code = code;
                }
            }
            Update::CodeScanned(text) => {
                if let Some(meet) = self.meet.as_mut() {
                    meet.code_note = Some(text);
                }
            }
            // Folded into Contacts and People by the contact layer.
            Update::ContactsNearby { .. } | Update::ContactsChanged => {}
            Update::IncomingCall { peer, verified, invite, fingerprint } => {
                if self.call.is_none() {
                    self.call_fingerprint = fingerprint;
                }
                self.incoming(&peer, verified, &invite)
            }
            Update::Live(event) => self.golive.on(event),
            Update::LiveNote(note) => self.live_note = note,
            Update::Visibility { visible, note } => {
                self.pc_visible = visible;
                self.visibility_note = note;
            }
            Update::AdvertisingProof(outcome) => self.peripheral_proof.record(outcome),
            Update::CrashReportPending => self.crash_prompt = true,
            Update::CrashReportShared(result) => match result {
                Ok(()) => self.dialog = Some(("Crash report".into(), "Your email app has the text report. Choose who to send it to, or close it. Native dumps stay on this PC.".into())),
                Err(error) => {
                    self.crash_prompt = true;
                    self.dialog = Some(("Crash report".into(), error));
                }
            },
            Update::OwnId(id) => self.own_id = id,
            Update::OwnGeneratedName(name) => self.own_generated = name,
            Update::CallEnded { reason } => {
                if let Some(call) = self.call.as_mut() {
                    self.call_end = Some(reason);
                    // A remote Cancel already ended and recorded this call;
                    // the ring timer can then identify it as a missed call.
                    if self.call_recorded {
                        if let Some(recent) = self.recent.first_mut() {
                            recent.result = reason.text().to_owned();
                        }
                    }
                    if call.phase() != Phase::Ended && call.end().is_err() {
                        call.link_lost();
                    }
                }
            }
        }
        self.flush_call();
    }
    fn incoming(&mut self, peer: &str, verified: bool, invite: &meshlive::call::Message) {
        if !self.usable(Feature::ReceiveCalls) || !self.switches.receive_calls {
            return;
        }
        let Ok(mut model) = CallModel::incoming(peer, verified, invite) else {
            return;
        };
        if self.call.as_ref().is_some_and(|c| c.phase() != Phase::Ended) {
            // CF-CALL-04: a second caller is never shown and never sent a
            // signal (no Ringing, no BUSY); the Bluetooth owner, which stops
            // announcing during a call, closes such a link itself.
            drop(model.take_outbox());
            return;
        }
        let video = !model.voice;
        // A stranger found this PC by its generated name; a contact knows it by name.
        self.answer_as = if verified { None } else { self.appear_as().map(str::to_owned) };
        self.call = Some(model);
        self.call_mic = None;
        self.call_mic_restarting = false;
        self.call_camera = None;
        self.call_sending = None;
        self.call_receiving = None;
        self.call_recorded = false;
        self.call_end = None;
        self.focus_primary = true;
        self.incoming_alert = Some((peer.to_owned(), video));
    }
    /// "You appear as ...": this PC's generated name while it is on air, with
    /// Friendly names on (as Android's nameplate line).
    pub fn appear_as(&self) -> Option<&str> {
        if !self.switches.generated_names {
            return None;
        }
        self.own_generated.as_deref()
    }
    /// The incoming call to announce outside the window (taken once).
    pub fn take_incoming_alert(&mut self) -> Option<(String, bool)> {
        self.incoming_alert.take()
    }
    /// Sends what the call controller produced and records a finished call.
    fn flush_call(&mut self) {
        let Some(call) = self.call.as_mut() else {
            return;
        };
        for message in call.take_outbox() {
            self.backend.command(Command::CallSignal(message));
        }
        if call.phase() == Phase::Ended && !self.call_recorded {
            self.call_recorded = true;
            self.recent.insert(
                0,
                RecentCall {
                    peer: call.peer.clone(),
                    voice: call.voice,
                    outgoing: call.outgoing,
                    result: self.call_end.map_or_else(|| call.status(), |r| r.text().to_owned()),
                    contact: self.call_contact,
                    fingerprint: self.call_fingerprint,
                    blocked: false,
                },
            );
            self.recent.truncate(50);
        }
    }

    // ---- user actions (also driven directly by the tests) ----------------

    pub fn start_watch(&mut self, id: Handle) -> bool {
        if !self.usable(Feature::Watch) {
            return false;
        }
        let Some(entry) = self.streams.iter().find(|s| s.id == id).cloned() else {
            return false;
        };
        if entry.full || entry.unavailable.is_some() || self.call.as_ref().is_some_and(|c| c.phase() != Phase::Ended) || self.golive.running() {
            return false;
        }
        self.call = None;
        self.watch = Some(WatchModel::new(entry));
        self.watch_note = None;
        self.peer_pictures = None;
        self.peer_texture = None;
        self.video = None;
        self.backend.command(Command::Watch(id));
        self.focus_primary = true;
        true
    }
    pub fn leave_watch(&mut self) {
        if let Some(mut watch) = self.watch.take() {
            watch.stop();
            self.backend.command(Command::StopWatching);
        }
    }
    pub fn place_call(&mut self, person: Handle, voice: bool) -> bool {
        if !self.usable(Feature::CallOut) || self.call.is_some() || self.watch.is_some() || self.golive.running() {
            return false;
        }
        if voice && !self.switches.voice_calls {
            return false;
        }
        self.call_contact = self.contacts.iter().any(|c| c.id == person).then_some(person);
        self.call_fingerprint = None;
        let (name, verified) = match self.people.iter().find(|p| p.id == person) {
            Some(p) => (p.name.clone(), p.verified),
            None => match self.contacts.iter().find(|c| c.id == person) {
                Some(c) => (c.name.clone(), true),
                None => return false,
            },
        };
        let id = self.backend.new_call_id();
        let Ok(model) = CallModel::outgoing(&name, verified, voice, id) else {
            return false;
        };
        self.call = Some(model);
        self.call_mic = None;
        self.call_mic_restarting = false;
        self.call_camera = None;
        self.call_sending = None;
        self.call_receiving = None;
        self.call_recorded = false;
        self.call_end = None;
        self.focus_primary = true;
        self.backend.command(Command::Call { person, voice });
        self.flush_call();
        true
    }
    /// What the call screen says: the backend's ending (No answer, Missed
    /// call) once the call is over, else the call's own status.
    #[allow(dead_code)] // For the call screen (screens.rs).
    pub fn call_status(&self) -> Option<String> {
        let call = self.call.as_ref()?;
        Some(match self.call_end {
            Some(reason) if call.phase() == Phase::Ended => reason.text().to_owned(),
            _ if call.phase() == Phase::Active && self.call_mic_restarting && call.sending_audio() => "Starting microphone\u{2026}".into(),
            _ => call.status(),
        })
    }
    fn call_media_lines(&self) -> Vec<&str> {
        let (sending, receiving) = if self.accurate_call_media {
            self.call.as_ref().filter(|call| call.phase() == Phase::Active)
                .map_or((false, false), |call| (call.camera, call.peer_camera))
        } else { (true, true) };
        [self.call_sending.as_deref().filter(|_| sending), self.call_receiving.as_deref().filter(|_| receiving)]
            .into_iter().flatten().collect()
    }
    /// "Open Bluetooth settings" (Windows Settings > Bluetooth).
    pub fn open_bluetooth_settings(&mut self) {
        self.backend.command(Command::OpenBluetoothSettings);
    }
    /// The button every Bluetooth note carries; screens put it under their note.
    #[allow(dead_code)] // For the screens' Bluetooth notes (Nearby empty state).
    pub(crate) fn bluetooth_settings_button(&mut self, ui: &mut egui::Ui) {
        if Pill::new("Open Bluetooth settings").kind(Kind::Secondary).small().show(ui).clicked() {
            self.open_bluetooth_settings();
        }
    }
    pub fn with_call(&mut self, action: impl FnOnce(&mut CallModel)) {
        if let Some(call) = self.call.as_mut() {
            action(call);
        }
        self.flush_call();
    }
    /// Uploads the newest decoded peer picture (if any) and traces each one
    /// shown, so a run can count shown frames against decoded ones.
    fn show_peer_picture(&mut self, ctx: &egui::Context) {
        if self.call.is_none() && self.watch.is_none() {
            if self.peer_pictures.take().is_some() {
                self.peer_texture = None;
                self.video = None;
                self.peer_seen = 0;
            }
            return;
        }
        let Some(handle) = self.peer_pictures.clone() else { return };
        // While a call shows video the screen repaints at the picture rate.
        ctx.request_repaint_after(std::time::Duration::from_millis(15));
        let Some(picture) = handle.0.take_newer(self.peer_seen) else { return };
        let skipped = picture.seq.saturating_sub(self.peer_seen + 1);
        self.peer_seen = picture.seq;
        let image = egui::ColorImage::from_rgba_unmultiplied([picture.width as usize, picture.height as usize], &picture.rgba);
        match self.peer_texture.as_mut() {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => self.peer_texture = Some(ctx.load_texture("peer-video", image, egui::TextureOptions::LINEAR)),
        }
        self.video = self.peer_texture.as_ref().map(|t| t.id());
        self.peer_size = [picture.width, picture.height];
        self.peer_turns = handle.0.turns();
        crate::radio_backend::trace(
            "peer_frame_shown",
            &format!(
                "seq={} pts_us={} {}x{} turns={} age_ms={} skipped={skipped} skipped_span_ms={} read_failed={}",
                picture.seq,
                picture.pts_us,
                picture.width,
                picture.height,
                self.peer_turns,
                picture.decoded_at.elapsed().as_millis(),
                picture.decoded_at.duration_since(picture.first_unshown_at).as_millis(),
                handle.0.failures()
            ),
        );
    }
    /// A call exists (the main loop keeps the model ticking for it).
    /// The navigation's pages: Live only where this PC has live streams.
    pub fn pages(&self) -> Vec<Page> {
        let mut pages = vec![Page::Calls, Page::Contacts, Page::Settings];
        if self.backend.streams() {
            pages.insert(0, Page::Home);
        }
        pages
    }
    pub fn in_call(&self) -> bool {
        self.call.is_some()
    }
    /// The model's work without painting: backend updates, queued call
    /// signals and (test builds) the test hooks.
    pub fn background_tick(&mut self) {
        self.pump();
        #[cfg(feature = "test-hooks")]
        self.test_hooks_tick(None);
    }
    pub fn close_call(&mut self) {
        if self.call.as_ref().is_some_and(|c| c.phase() == Phase::Ended) {
            self.call = None;
        }
    }
    pub fn start_live(&mut self) -> bool {
        if self.live_consent_required {
            if !self.usable(Feature::GoLive) || !self.golive.can_start() { return false; }
            self.live_exposure = true;
            return false;
        }
        self.begin_live()
    }
    fn begin_live(&mut self) -> bool {
        if !self.backend.can_go_live() || !self.usable(Feature::GoLive) || self.watch.is_some()
            || self.call.as_ref().is_some_and(|c| c.phase() != Phase::Ended) || !self.live_settings.valid() || !self.golive.start() {
            return false;
        }
        self.live_note = None;
        if self.live_consent_required {
            self.backend.command(Command::ConfigureLive(self.live_settings));
            self.backend.command(Command::LiveExposureConsent);
        }
        self.backend.command(Command::GoLive {
            kind: self.golive.kind,
            title: self.golive.title.clone(),
            initial: meshlive::stream_source::InitialMedia { muted: self.golive.muted, camera_off: self.golive.camera_off },
        });
        true
    }
    pub fn open_go_live(&mut self) {
        self.golive_open = true;
        self.focus_primary = true;
    }
    pub fn stop_live(&mut self) {
        if self.golive.running() {
            self.golive.stop();
            self.backend.command(Command::StopLive);
        }
    }
    pub fn switches_changed(&mut self) {
        self.backend.command(Command::Switches(self.switches.clone()));
    }
}

/// Fonts, colours and spacing (see `theme`).
pub fn configure(ctx: &egui::Context) {
    theme::apply(ctx);
}

impl Ui {
    pub fn show(&mut self, ctx: &egui::Context, texture: egui::TextureId, renderer: &str) {
        self.pump();
        self.show_peer_picture(ctx);
        let streams_visible = meshlive::stream_watch::enabled() && self.backend.streams() && self.page == Page::Home && self.call.is_none() && self.watch.is_none() && !self.golive_open;
        if streams_visible != self.streams_visible_sent {
            self.streams_visible_sent = streams_visible;
            self.backend.command(Command::StreamsVisible(streams_visible));
        }
        let people_visible = self.page == Page::Calls && !self.recent_tab && self.call.is_none() && self.watch.is_none();
        if people_visible != self.people_visible_sent {
            self.people_visible_sent = people_visible;
            self.backend.command(Command::PeopleVisible(people_visible));
        }
        #[cfg(feature = "test-hooks")]
        self.test_hooks_tick(Some(ctx));
        if let Some(due) = self.backend.next_due() {
            ctx.request_repaint_after(due.max(std::time::Duration::from_millis(16)));
        }
        // Hide Live until the backend can actually watch on this platform.
        if self.page == Page::Home && !self.backend.streams() {
            self.page = Page::Calls;
        }
        let in_call = self.call.is_some();
        let takeover = in_call || self.watch.is_some() || self.golive_open;
        if !in_call {
            let keys = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4];
            for (key, page) in keys.into_iter().zip(self.pages()) {
                if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, key)) {
                    self.leave_screens();
                    self.page = page;
                }
            }
        }
        let compact = ctx.content_rect().width() < theme::COMPACT_BELOW;
        self.sidebar(ctx, compact);
        let gutter = theme::gutter(ctx.content_rect().width()) as i8;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(egui::Margin { left: gutter, right: gutter, top: 8, bottom: 20 }))
            .show(ctx, |ui| {
                let _ = takeover;
                if self.call.is_some() {
                    self.call_screen(ui);
                } else if self.watch.is_some() {
                    self.watch_screen(ui);
                } else if self.golive_open {
                    self.golive_screen(ui);
                } else if self.meet.is_some() {
                    self.meet_screen(ui);
                } else {
                    components::content_column(ui, |ui| {
                        let (word, tone) = self.status();
                        let mut status = false;
                        components::top_bar(ui, self.page.label(), None, |ui| {
                            status = components::status_chip(ui, word, tone).clicked();
                        });
                        if status {
                            self.dialog = Some(("Status".into(), self.status_detail()));
                        }
                        // Windows cannot announce this PC while it sleeps:
                        // say so under "Ready" whenever calls are allowed.
                        if word == "Ready" && self.calls_allowed() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                ui.label(theme::caption(AWAKE_NOTE));
                            });
                        }
                        self.shell_notices(ui);
                        ui.add_space(8.0);
                        // The page's pinned action sits at the bottom (laid out
                        // first); the page scrolls in the space above it.
                        let full = ui.available_rect_before_wrap();
                        let mut footer = ui.new_child(
                            egui::UiBuilder::new().max_rect(full).layout(egui::Layout::bottom_up(egui::Align::Min)),
                        );
                        self.page_footer(&mut footer);
                        let footer_height = footer.min_rect().height();
                        let bottom = if footer_height > 0.5 { full.bottom() - footer_height - 16.0 } else { full.bottom() };
                        let mut body = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(egui::Rect::from_min_max(full.min, egui::pos2(full.right(), bottom)))
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        let ui = &mut body;
                        match self.page {
                            Page::Home => self.home(ui),
                            Page::Calls => egui::ScrollArea::vertical()
                                .id_salt("calls")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.calls(ui))
                                .inner,
                            Page::Contacts => egui::ScrollArea::vertical()
                                .id_salt("contacts")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.contacts(ui))
                                .inner,
                            Page::Settings => egui::ScrollArea::vertical()
                                .id_salt("settings")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.settings(ui, renderer))
                                .inner,
                        }
                    });
                }
            });
        self.dialogs(ctx);
        if self.texture_visible {
            egui::Window::new("Graphics diagnostics")
                .open(&mut self.texture_visible)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label(renderer);
                    ui.label("Synthetic fixture \u{b7} D3D11 texture on the UI device");
                    let width = ui.available_width().min(640.0);
                    ui.image((texture, egui::vec2(width, width * 9.0 / 16.0)));
                });
        }
    }

    /// Receive calls is on and this PC can take calls.
    pub(crate) fn calls_allowed(&self) -> bool {
        self.switches.receive_calls && self.usable(Feature::ReceiveCalls)
    }

    /// The one status word at the end of every title bar: fine, on air, or
    /// the one problem to fix.
    fn status(&self) -> (&'static str, Tone) {
        use meshlive::bluetooth::Radio;
        if self.golive.running() {
            return ("On air", Tone::Live);
        }
        match self.bluetooth.map(|caps| caps.radio) {
            None => ("Checking\u{2026}", Tone::Ok),
            Some(Radio::On) => ("Ready", Tone::Ok),
            Some(Radio::Off | Radio::Disabled) => ("Bluetooth off", Tone::Problem),
            Some(Radio::Missing) => ("No Bluetooth", Tone::Problem),
            Some(Radio::Denied) => ("Permission needed", Tone::Problem),
            Some(Radio::Unknown | Radio::Unavailable) => ("Bluetooth unavailable", Tone::Problem),
        }
    }
    fn status_detail(&self) -> String {
        let mut lines = vec![self.bluetooth.map_or(CHECKING_BLUETOOTH, |caps| caps.status()).to_owned()];
        lines.push(match self.unavailable_reason(Feature::ReceiveCalls) {
            Some(reason) => format!("Phones can't call this PC. {reason}"),
            None if self.switches.receive_calls => format!("Phones nearby can call this PC. {AWAKE_NOTE}."),
            None => "Allow calls is off.".into(),
        });
        if self.golive.running() {
            lines.push("You are live.".into());
        }
        lines.join("\n\n")
    }

    /// The tab page's one pinned action, at the bottom of the window where
    /// it stays reachable however long the list (spec: Contacts pins "Add
    /// contact"; Live pins its own Go live). Pages without one leave it empty.
    fn page_footer(&mut self, ui: &mut egui::Ui) {
        if self.page == Page::Contacts {
            if Pill::new("Add contact").icon(Icon::PersonAdd).fill_width().large().show(ui).clicked() {
                self.open_meet();
            }
        }
    }

    /// One-line notices above every tab page: the crash prompt (once) and a
    /// Bluetooth problem with the way to fix it.
    fn shell_notices(&mut self, ui: &mut egui::Ui) {
        if self.crash_prompt {
            match components::notice_actions(ui, CRASH_PROMPT, &["Share", "Not now"]) {
                Some(0) => {
                    self.crash_prompt = false;
                    self.backend.command(Command::ShareCrashReport);
                }
                Some(_) => {
                    self.crash_prompt = false;
                    self.backend.command(Command::DismissCrashReport);
                }
                None => {}
            }
            ui.add_space(12.0);
        }
        if let Some(text) = self.bluetooth_problem() {
            if components::notice(ui, text, Some("Open Bluetooth settings")) {
                self.open_bluetooth_settings();
            }
            ui.add_space(12.0);
        }
    }
    /// The radio's problem the user can fix in Windows Settings (None: fine,
    /// still checking, or no adapter at all).
    fn bluetooth_problem(&self) -> Option<&'static str> {
        use meshlive::bluetooth::Radio;
        let caps = self.bluetooth?;
        matches!(caps.radio, Radio::Off | Radio::Disabled | Radio::Denied).then(|| caps.status())
    }

    /// Leaves Watch and Go live screens (a running broadcast keeps running).
    fn leave_screens(&mut self) {
        self.leave_watch();
        self.golive_open = false;
        self.close_meet();
    }
    /// Add contact: scan the code another phone shows (typed or pasted).
    pub fn open_meet(&mut self) {
        self.meet = Some(Meet::default());
        self.backend.command(Command::ShowOwnCode);
        self.focus_primary = true;
    }
    pub fn meet_scan(&mut self) -> bool {
        if self.meet.as_ref().is_some_and(|m| m.camera) {
            self.meet_camera(false);
        }
        let Some(meet) = self.meet.as_mut() else { return false };
        let code = meet.code.trim().to_owned();
        if code.is_empty() || matches!(meet.state, Some(MeetEvent::Searching) | Some(MeetEvent::Ask { .. })) {
            return false;
        }
        meet.state = Some(MeetEvent::Searching);
        meet.name.clear();
        self.backend.command(Command::ScanCode(code));
        true
    }
    pub fn meet_answer(&mut self, yes: bool) {
        let Some(meet) = self.meet.as_mut() else { return };
        if !matches!(meet.state, Some(MeetEvent::Ask { .. })) {
            return;
        }
        let name = meet.name.trim().to_owned();
        meet.state = Some(MeetEvent::Searching);
        self.backend.command(Command::ConfirmScan { yes, name });
    }
    /// "Scan with camera" on or off (the backend opens the camera only then).
    pub fn meet_camera(&mut self, on: bool) {
        let Some(meet) = self.meet.as_mut() else { return };
        if meet.camera != on {
            meet.camera = on;
            if on {
                meet.state = None;
            }
            self.backend.command(Command::QrCamera(on));
        }
    }
    pub fn close_meet(&mut self) {
        if self.meet.as_ref().is_some_and(|m| m.camera) {
            self.backend.command(Command::QrCamera(false));
        }
        if self.meet.take().is_some() {
            self.backend.command(Command::CancelScan);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        use components::{dialog, dialog_buttons, dialog_text, dialog_title};
        if let Some(report) = self.wiped.clone() {
            dialog(ctx, "wiped", |ui| {
                if report.complete {
                    dialog_title(ui, "Everything was deleted");
                    dialog_text(ui, "Your MeshLive ID, contacts and settings are deleted from this PC. MeshLive closes now and makes a new ID the next time it starts.");
                } else {
                    dialog_title(ui, "Not everything was deleted");
                    dialog_text(ui, "MeshLive closes now and finishes deleting when it starts again.");
                    for item in report.failed.iter() {
                        ui.label(theme::caption(item.as_str()));
                    }
                    if let Some(error) = &report.key_error {
                        ui.label(theme::caption(format!("identity key: {error}")));
                    }
                }
                let (close, _) = dialog_buttons(ui, "Close MeshLive", Kind::Primary, true, None);
                if close {
                    #[cfg(not(test))]
                    std::process::exit(0);
                }
            });
            return;
        }
        // First run: what MeshLive is, an optional name, what it needs.
        if !self.switches.onboarded && self.call.is_none() {
            let mut done = false;
            dialog(ctx, "welcome", |ui| {
                ui.add_space(4.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 24.0, theme::RAISED);
                draw_icon(ui.painter(), rect.shrink(12.0), Icon::Broadcast, theme::TEXT);
                ui.add_space(6.0);
                ui.label(theme::title("Welcome to MeshLive"));
                dialog_text(ui, "Voice and video calls with people nearby over Bluetooth, encrypted end to end. No internet or account needed.");
                ui.add_space(6.0);
                let field = components::text_field(ui, &mut self.welcome_name, Some("Your name (optional)"), "Name", Some(32));
                if self.focus_primary {
                    field.request_focus();
                    self.focus_primary = false;
                }
                ui.label(theme::caption("Only verified contacts see it, during calls. You can change it in Settings."));
                ui.add_space(4.0);
                let (bluetooth, ready) = match &self.bluetooth {
                    None => ("Checking Bluetooth\u{2026}", false),
                    Some(_) if self.usable(Feature::CallOut) => ("Bluetooth is ready.", true),
                    Some(_) => ("Turn on Bluetooth in Windows Settings to find people and call.", false),
                };
                icon_line(ui, if ready { Icon::Check } else { Icon::Bluetooth }, bluetooth);
                icon_line(ui, Icon::Mic, "Windows asks for the microphone and camera the first time a call needs them.");
                ui.add_space(8.0);
                let start = Pill::new("Start").fill_width().show(ui).clicked();
                // Enter starts from anywhere in the dialog (the default button).
                done = start || ui.input(|i| i.key_pressed(egui::Key::Enter)) || (field.lost_focus() && ui.input(|i| i.key_down(egui::Key::Enter)));
            });
            if done {
                let name = self.welcome_name.trim().to_owned();
                if !name.is_empty() {
                    self.backend.command(Command::SetOwnName(name));
                }
                self.switches.onboarded = true;
                self.switches_changed();
            }
        }
        if self.name_edit.is_some() {
            let mut cancel = false;
            let mut save = false;
            dialog(ctx, "own-name", |ui| {
                dialog_title(ui, "Your name");
                dialog_text(ui, "Only your verified contacts see it, during calls. Leave it empty to send no name.");
                if let Some(text) = self.name_edit.as_mut() {
                    let field = components::text_field(ui, text, None, "Name", Some(32));
                    if self.focus_primary {
                        field.request_focus();
                        self.focus_primary = false;
                    }
                    save = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                }
                if let Some(error) = self.name_error.as_deref() {
                    ui.label(theme::body(error).color(RED));
                }
                let (ok, no) = dialog_buttons(ui, "Save", Kind::Primary, true, Some("Cancel"));
                save |= ok;
                cancel = no;
            });
            if cancel {
                self.name_edit = None;
                self.name_error = None;
            }
            if save {
                let text = self.name_edit.clone().unwrap_or_default();
                self.backend.command(Command::SetOwnName(text));
            }
        }
        if let Some((person, voice)) = self.copied_confirm {
            let mut cancel = false;
            let mut call = false;
            dialog(ctx, "copied-id", |ui| {
                dialog_title(ui, "More than one phone is using this ID");
                dialog_text(ui, "Another phone may be using the same ID, so this may not be who you think. The call still checks who answers before it shows their name.");
                (call, cancel) = dialog_buttons(ui, "Call anyway", Kind::Secondary, true, Some("Cancel"));
            });
            if cancel {
                self.copied_confirm = None;
            }
            if call {
                self.copied_confirm = None;
                self.place_call(person, voice);
            }
        }
        if self.wipe_confirm.is_some() {
            let mut cancel = false;
            let mut confirm = false;
            dialog(ctx, "wipe-confirm", |ui| {
                dialog_title(ui, "Delete everything?");
                dialog_text(ui, "This deletes your MeshLive ID, contacts and settings from this PC. Your contacts won't recognise this PC any more. This can't be undone.");
                ui.add_space(4.0);
                if let Some(text) = self.wipe_confirm.as_mut() {
                    components::text_field(ui, text, Some("Type DELETE to confirm"), "DELETE", None);
                }
                let typed = self.wipe_confirm.as_deref().is_some_and(|t| t.trim() == "DELETE");
                let (wipe, no) = dialog_buttons(ui, "Delete", Kind::Destructive, typed, Some("Cancel"));
                confirm = typed && wipe;
                cancel = no;
            });
            if cancel {
                self.wipe_confirm = None;
            }
            if confirm {
                self.backend.command(Command::Wipe);
            }
        }
        if self.audio_disclosure {
            let mut confirm = false;
            let mut cancel = false;
            dialog(ctx, "audio-disclosure", |ui| {
                dialog_title(ui, "Before you go live");
                dialog_text(ui, AUDIO_DISCLOSURE);
                ui.add_space(4.0);
                let go = Pill::new("Go live with audio").kind(Kind::Live).icon(Icon::Broadcast).fill_width().show(ui);
                if self.focus_primary {
                    go.request_focus();
                    self.focus_primary = false;
                }
                confirm = go.clicked();
                cancel = Pill::new("Cancel").kind(Kind::Secondary).fill_width().show(ui).clicked()
                    || ui.input(|input| input.key_pressed(egui::Key::Escape));
            });
            if confirm {
                self.golive.disclosed = true;
                self.audio_disclosure = false;
                self.start_live();
            } else if cancel {
                self.audio_disclosure = false;
            }
        }
        self.contact_dialogs(ctx);
        if self.live_exposure {
            let mut confirm = false; let mut cancel = false;
            dialog(ctx, "live-exposure", |ui| {
                dialog_title(ui, "Before you go live");
                dialog_text(ui, meshlive::stream_source::EXPOSURE);
                let (go, no) = dialog_buttons(ui, "Go live", Kind::Live, true, Some("Cancel"));
                confirm = go; cancel = no || ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
            if confirm { self.live_exposure = false; self.begin_live(); }
            else if cancel { self.live_exposure = false; }
        }
        if let Some((title, body)) = self.dialog.clone() {
            dialog(ctx, "info-dialog", |ui| {
                dialog_title(ui, &title);
                dialog_text(ui, &body);
                let (ok, _) = dialog_buttons(ui, "Got it", Kind::Primary, true, None);
                if ok || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    self.dialog = None;
                }
            });
        }
    }

    fn contact_dialogs(&mut self, ctx: &egui::Context) {
        use components::{dialog, dialog_buttons, dialog_text, dialog_title};
        if let Some((contact, mut text, error)) = self.rename.clone() {
            let (mut save, mut cancel) = (false, false);
            dialog(ctx, "rename-contact", |ui| {
                dialog_title(ui, "Rename contact");
                dialog_text(ui, "Only you see this name.");
                let edit = components::text_field(ui, &mut text, None, "Name", None);
                if self.focus_primary {
                    edit.request_focus();
                    self.focus_primary = false;
                }
                if let Some(error) = error {
                    ui.label(theme::body(error).color(RED));
                }
                let (ok, no) = dialog_buttons(ui, "Save", Kind::Primary, true, Some("Cancel"));
                save = ok || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = no;
            });
            if cancel {
                self.rename = None;
            } else {
                self.rename = Some((contact, text.clone(), error));
                if save {
                    self.backend.command(Command::RenameContact { contact, name: text });
                }
            }
        }
        if let Some(contact) = self.forget {
            let name = self.contacts.iter().find(|c| c.id == contact).map(|c| c.name.clone()).unwrap_or_default();
            let (mut confirm, mut cancel) = (false, false);
            dialog(ctx, "forget-contact", |ui| {
                dialog_title(ui, &format!("Forget {name}?"));
                dialog_text(ui, screens::FORGET_MESSAGE);
                (confirm, cancel) = dialog_buttons(ui, "Forget", Kind::Destructive, true, Some("Cancel"));
            });
            if confirm {
                self.forget = None;
                self.backend.command(Command::ForgetContact(contact));
            } else if cancel {
                self.forget = None;
            }
        }
        if self.reset_contacts {
            let (mut confirm, mut cancel) = (false, false);
            dialog(ctx, "reset-contacts", |ui| {
                dialog_title(ui, "Reset verified people");
                dialog_text(ui, screens::RESET_MESSAGE);
                (confirm, cancel) = dialog_buttons(ui, "Forget everyone", Kind::Destructive, true, Some("Cancel"));
            });
            if confirm {
                self.reset_contacts = false;
                self.backend.command(Command::ResetContacts);
            } else if cancel {
                self.reset_contacts = false;
            }
        }
    }

    /// The navigation rail: wordmark, the four pages (Ctrl+1 to 4), and
    /// nothing else. Collapses to icons on a narrow window.
    fn sidebar(&mut self, ctx: &egui::Context, compact: bool) {
        egui::SidePanel::left("navigation")
            .exact_width(if compact { theme::NAV_WIDTH_COMPACT } else { theme::NAV_WIDTH })
            .resizable(false)
            .show_separator_line(true)
            .frame(egui::Frame::NONE.fill(theme::BG).inner_margin(egui::Margin { left: 12, right: 12, top: 18, bottom: 16 }))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let (mark, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
                let icon_center = if compact { mark.center() } else { egui::pos2(mark.left() + 22.0, mark.center().y) };
                draw_icon(ui.painter(), egui::Rect::from_center_size(icon_center, egui::vec2(24.0, 24.0)), Icon::Broadcast, theme::TEXT);
                if !compact {
                    ui.painter().text(
                        egui::pos2(mark.left() + 44.0, mark.center().y),
                        egui::Align2::LEFT_CENTER,
                        "MeshLive",
                        egui::FontId::new(19.0, theme::bold_family()),
                        theme::TEXT,
                    );
                }
                ui.add_space(24.0);
                let locked = self.call.is_some();
                for (index, page) in self.pages().into_iter().enumerate() {
                    let icon = match page {
                        Page::Home => Icon::Broadcast,
                        Page::Calls => Icon::Phone,
                        Page::Contacts => Icon::People,
                        Page::Settings => Icon::Settings,
                    };
                    let selected = self.page == page && self.call.is_none() && self.watch.is_none() && !self.golive_open;
                    let response = ui.add_enabled_ui(!locked, |ui| components::nav_item(ui, page.label(), icon, selected, compact));
                    let response = response.inner.on_hover_text(format!("{}  (Ctrl+{})", page.label(), index + 1));
                    if response.clicked() {
                        self.leave_screens();
                        self.page = page;
                    }
                }
            });
    }
}

/// A small grey glyph and a wrapped line (dialog facts).
fn icon_line(ui: &mut egui::Ui, icon: Icon, text: &str) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 20.0), egui::Sense::hover());
        draw_icon(ui.painter(), rect.shrink2(egui::vec2(0.0, 1.0)), icon, theme::TEXT_SECONDARY);
        ui.add(egui::Label::new(theme::secondary(text)).wrap());
    });
}

const CRASH_PROMPT: &str = "MeshLive closed unexpectedly. Share a crash report?";

const AUDIO_DISCLOSURE: &str = "Anyone nearby with MeshLive can listen, including phones that pass it on. Titles from people you have not verified show as unconfirmed.\n\nListeners can tell it is your phone: until streams use a throwaway identity, everyone who joins learns your phone's long-term identity key and can link broadcasts from different days to it.";

fn emphasis(text: &str) -> RichText {
    RichText::new(text).family(FontFamily::Name("Heading".into()))
}

fn support(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "Supported",
        Some(false) => "Not supported",
        None => "Unknown",
    }
}

// ------------------------------------------------------------------------
// Older helpers the screens use, now drawn with the shared components (see
// `components` for the full set). New code should call `components` directly.

fn empty(ui: &mut egui::Ui, icon: Icon, title: &str, subtitle: &str) {
    components::empty_state(ui, icon, title, subtitle);
}

/// A small pill; the selected one is white.
fn chip(ui: &mut egui::Ui, label: &str, selected: bool, enabled: bool) -> egui::Response {
    let response = Pill::new(label)
        .kind(if selected { Kind::Primary } else { Kind::Secondary })
        .small()
        .enabled(enabled)
        .show(ui);
    let on = response.enabled();
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, on, selected, label));
    response
}

/// A 16:9 picture area. Shows `texture` when given, otherwise the status text.
fn picture(ui: &mut egui::Ui, name: &str, status: &str, texture: Option<egui::TextureId>, max_height: f32) {
    let width = ui.available_width();
    let height = (width * 9.0 / 16.0).min(max_height).max(120.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, format!("{name}. {status}")));
    let corner = egui::CornerRadius::same(theme::SHEET_RADIUS);
    ui.painter().rect_filled(rect, corner, Color32::from_gray(10));
    match texture {
        Some(id) => {
            egui::Image::new((id, rect.size())).corner_radius(corner).paint_at(ui, rect);
        }
        None => {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, status, theme::Type::Body.font(), SECONDARY);
        }
    }
}

/// Screen readers and voice input set a text field's whole value through UI
/// Automation (`ValuePattern.SetValue`, AccessKit `SetValue`); egui 0.33's
/// `TextEdit` ignores that request, so every app text field applies it here
/// (cut to `limit` characters, as the field's own `char_limit`).
pub(crate) fn accessible_set_value(ui: &egui::Ui, field: &egui::Response, text: &mut String, limit: Option<usize>) -> bool {
    use egui::accesskit::{Action, ActionData};
    let mut changed = false;
    ui.input(|input| {
        for request in input.accesskit_action_requests(field.id, Action::SetValue) {
            if let Some(ActionData::Value(value)) = &request.data {
                *text = match limit {
                    Some(n) => value.chars().take(n).collect(),
                    None => value.to_string(),
                };
                changed = true;
            }
        }
    });
    ui.ctx().accesskit_node_builder(field.id, |node| node.add_action(Action::SetValue));
    if changed {
        ui.ctx().request_repaint();
    }
    changed
}

