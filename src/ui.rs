//! MeshLive Windows interface: egui on Direct3D 11 with AccessKit. The screens
//! follow Android/macOS (Home, Calls, Contacts, Settings, with Watch, Call and
//! Go live taking over the window). All state lives in `model`; the core is
//! reached only through a `Backend` (no radio, camera, microphone or audio
//! endpoint is opened from here).
pub mod backend;
pub mod components;
mod android_icons;
mod call_layout;
mod icons;
pub mod model;
mod screens;
mod settings;
mod blocked;
mod report;
mod contact_sheet;
mod control_row;
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
/// Who gets this PC's name (WP4, Android `identity_name_hint`): Settings >
/// Who sees my name decides (`call_policy`; the dial and answer paths apply
/// it). Shown in the welcome and the name dialog.
pub(crate) const NAME_WHO: &str = "It's sent, encrypted, only in calls, to the people you choose under Who sees my name.";
/// Settings > Your name, the note under the card (Android `identity_name_note`
/// without "and your QR code shows it": this PC never sends its name in a code
/// scan).
pub(crate) const NAME_AUDIENCE: &str = "Your name is never broadcast. It is sent, encrypted, only to the people you choose under Who sees my name. Without a name, others see a made-up name.";
/// The empty Calls list: this PC lists phones that announce calls (Allow calls
/// on). Calls need no Find PCs on the phone (the compact announcement is read by
/// Android's Find iPhones, on by default).
pub(crate) const NOBODY_NEARBY_HINT: &str = "People appear here while MeshLive runs on their phone with Allow calls on. Both of you need a current MeshLive.";
pub(crate) const NAME_NOT_SET: &str = "Not set. People nearby see a random name that changes every 15 minutes.";
/// Why the generated name (the (i) after "You appear as").
pub(crate) const APPEAR_AS_INFO: &str = "People nearby who haven't added you see this made-up name instead of an ID. It comes from this PC's private Bluetooth ID, which changes every 15 minutes, so nobody nearby can follow this PC over time. Your name, if you set one, is only sent encrypted to the people you choose (Settings > Who sees my name).";
pub(crate) const AWAKE_NOTE: &str = "Reachable while this PC is awake";
/// The words in the backend's damaged-keys note that make the Calls page
/// offer "Reset" (radio_backend `PRESENCE_DAMAGED_NOTE`).
pub(crate) const KEYS_DAMAGED: &str = "contact keys are damaged";

/// The Status sheet's warning while Windows blocks MeshLive's toasts (Android
/// `status_notifications_off`): `announce_incoming` still shows the window
/// and flashes the taskbar.
pub(crate) const NOTIFICATIONS_OFF: &str = "Windows notifications are off for MeshLive, so a call shows only in its window and as a flashing taskbar button.";

/// What the status indicator says (Android `StatusKind`), one at a time;
/// `Ui::status_kind` picks the first that applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    /// Windows-only: the first Bluetooth read hasn't come back.
    Checking,
    NoBluetooth,
    BluetoothOff,
    /// Windows refused this app the Bluetooth radio.
    Permissions,
    /// Windows-only: the radio's state couldn't be read.
    BluetoothUnavailable,
    /// The backend stops announcing calls while watching or live
    /// (radio_backend `start_watch`, `start_source`).
    Watching,
    Live,
    /// Phones can't call this PC now: Allow calls off, an adapter that can't
    /// announce, or the announcement failed.
    Hidden,
    /// Windows-only: the saved contact keys are damaged (the sheet offers Reset).
    ResetNeeded,
    Ready,
}
impl StatusKind {
    /// Android's words (strings_ui `status_*`, strings `status_calls_paused_*`).
    pub(crate) fn word(self) -> &'static str {
        match self {
            StatusKind::Checking => "Checking\u{2026}",
            StatusKind::NoBluetooth => "No Bluetooth",
            StatusKind::BluetoothOff => "Bluetooth off",
            StatusKind::Permissions => "Permissions needed",
            StatusKind::BluetoothUnavailable => "Bluetooth unavailable",
            StatusKind::Watching => "Calls paused while you watch",
            StatusKind::Live => "Calls paused while you're live",
            StatusKind::Hidden => "Hidden",
            StatusKind::ResetNeeded => "Reset needed",
            StatusKind::Ready => "Ready",
        }
    }
    pub(crate) fn tone(self) -> Tone {
        match self {
            StatusKind::Ready => Tone::Ok,
            StatusKind::NoBluetooth | StatusKind::BluetoothOff | StatusKind::Permissions | StatusKind::BluetoothUnavailable => Tone::Problem,
            _ => Tone::Attention,
        }
    }
    /// All well is the dot alone (Android); anything else adds its word when it fits.
    pub(crate) fn shows_word(self) -> bool {
        self != StatusKind::Ready
    }
    /// The indicator's accessible name (Android contentDescription).
    pub(crate) fn a11y(self) -> &'static str {
        if self == StatusKind::Ready { "Status: ready" } else { self.word() }
    }
}

/// One line of the Status sheet: what, and its value at the end (Android
/// `item_status_row`).
fn status_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(15.0).color(theme::TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(theme::secondary(value));
        });
    });
}

/// The Settings pages (Android: the top page and its pushed pages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPage {
    Main,
    /// Video and calls (the top page's "Video quality" row).
    Video,
    Privacy,
    Connection,
    /// Manual video (Advanced): the encoder and quality Go live uses.
    Manual,
    Experiments,
    Help,
    About,
    Blocked,
}
impl SettingsPage {
    pub fn title(self) -> &'static str {
        match self {
            SettingsPage::Main => "Settings",
            SettingsPage::Video => "Video and calls",
            SettingsPage::Privacy => "Privacy",
            SettingsPage::Connection => "Connection",
            SettingsPage::Manual => "Manual video",
            SettingsPage::Experiments => "Experiments",
            SettingsPage::Help => "Help and feedback",
            SettingsPage::About => "About this app",
            SettingsPage::Blocked => "Blocked people",
        }
    }
    pub fn icon(self) -> Icon {
        match self {
            SettingsPage::Main => Icon::Settings,
            SettingsPage::Video => Icon::Camera,
            SettingsPage::Privacy => Icon::Lock,
            SettingsPage::Connection => Icon::Bluetooth,
            SettingsPage::Manual => Icon::Settings,
            SettingsPage::Experiments => Icon::Settings,
            SettingsPage::Help => Icon::Search,
            SettingsPage::About => Icon::Info,
            SettingsPage::Blocked => Icon::Person,
        }
    }
    /// Where Back and Escape go: Blocked people opens from Privacy; every
    /// other page from the top page.
    pub fn parent(self) -> SettingsPage {
        match self {
            SettingsPage::Blocked => SettingsPage::Privacy,
            _ => SettingsPage::Main,
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
    /// Some: a call "Block unknown callers" declined without ringing, when
    /// (Android's "Blocked call"; nobody is named, nothing can be done).
    pub declined_at: Option<std::time::Instant>,
}

/// "Save contact as" starts with the label you gave the contact whose ID
/// (`handle`) was scanned (Android `ContactNames.suggestion`); a contact
/// without a label shows by its ID, and an ID is never suggested.
fn saved_label(contacts: &[Contact], handle: &str) -> Option<String> {
    contacts.iter().find(|c| c.handle == handle && c.name != c.handle).map(|c| c.name.clone())
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
    /// "Copy code" was pressed this frame (taken after the screen is drawn).
    pub copy_requested: bool,
    /// The last copy: Some(true) copied, Some(false) the clipboard was busy.
    pub copied: Option<bool>,
    /// Android's segments: "Show my code" is chosen (else "Scan their code").
    pub show_code: bool,
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
    /// This PC's own camera in a call (the self-view): the slot, the last
    /// shown sequence, the texture and its size.
    self_pictures: Option<backend::PictureHandle>,
    /// Why Watch and Go live can't run on this PC (shown on the Live page).
    pub live_unsupported: Option<String>,
    /// The call screen's technical media lines are shown (collapsed by default).
    pub call_details_open: bool,
    self_seen: u64,
    self_texture: Option<egui::TextureHandle>,
    pub self_size: [u32; 2],
    backend: Box<dyn Backend>,
    #[cfg(test)]
    native_check: Option<native_call_check::State>,
    demo: bool,
    searching: bool,
    /// Why nothing can be found or called right now (Bluetooth off).
    bluetooth_note: Option<String>,
    /// Windows Bluetooth stopped answering this app (`Update::BluetoothNotResponding`).
    bluetooth_stuck: bool,
    /// The current call's microphone (None: not opened yet; Some(None): on;
    /// Some(Some(reason)): unusable).
    pub call_mic: Option<Option<String>>,
    call_mic_restarting: bool,
    hold_call_talk: bool,
    /// Hold Mute to talk (Android `HoldToTalkCore`) for the current call.
    mute_hold: HoldToTalk,
    /// How many cameras this PC has (from the camera owner; 0 until a
    /// camera opened): Flip works with two or more.
    pub cameras: usize,
    /// The current call's audio outputs, (endpoint id, name); the Speaker
    /// button opens the picker when there are two or more.
    pub call_outputs: Vec<(String, String)>,
    /// The output the call plays through now (an id of `call_outputs`).
    call_output: Option<String>,
    /// The Speaker button's "Audio output" picker is open.
    output_sheet: bool,
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
    /// Calls > Nearby's "Search people" text (Android `nearbySearchQuery`).
    people_query: String,
    /// CF-PRIV-22: a call to a copied ID waits for "Call anyway".
    pub copied_confirm: Option<(Handle, bool)>,
    /// An incoming call rang: (peer, video) for the taskbar flash and toast.
    incoming_alert: Option<(String, bool)>,
    /// This PC's code for the window to put on the clipboard (taken once).
    clipboard_request: Option<String>,
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
    /// Settings > Manual video > "Manual video settings" (off: Standard). Like
    /// the choices it unlocks, it lasts until MeshLive closes.
    manual_video: bool,
    live_note: Option<String>,
    /// Added to the clock of the live timer and the audio reminder (tests
    /// move time with it; always zero in the app).
    clock_offset: std::time::Duration,
    /// The audio reminder's times (Android's; tests shorten them).
    live_limits: ReminderLimits,
    /// "Declined. Block all unknown callers?" is on screen.
    block_unknown_offer: bool,
    /// "Still broadcasting" is on screen, waiting for an answer.
    live_reminder: bool,
    /// The reminder to announce outside the window (taken once).
    live_reminder_alert: bool,
    /// This PC's saved name (Settings > Profile).
    pub own_name: Option<String>,
    /// This PC's ID (`SSSSS-SSSSS`), once the backend has read it.
    pub own_id: Option<String>,
    /// Where this PC's identity key is kept (SEC-11), when the backend says.
    pub identity_key: Option<backend::IdentityKey>,
    /// This PC's computer name (Settings > Privacy), from the backend at start.
    pub computer_name: Option<String>,
    /// The generated name phones nearby see for this PC now (on air only).
    pub own_generated: Option<String>,
    /// The generated name the caller of the ringing call saw (a stranger's call).
    pub answer_as: Option<String>,
    /// The person or contact selected in a wide window's list.
    pub selected_person: Option<Handle>,
    pub selected_contact: Option<Handle>,
    /// The contact whose sheet is open (Android `ContactActions`; the row or
    /// its "More" button opens it).
    pub contact_more: Option<Handle>,
    /// "Use it as my label" was chosen for this contact and name; its answer
    /// (`Update::Renamed`) is said in one line (Android's toast).
    claim_rename: Option<(Handle, String)>,
    /// The status popover is open under the indicator at this rectangle.
    pub status_open: Option<egui::Rect>,
    /// Windows won't show MeshLive's toasts (the window sets it on focus,
    /// `notify::toasts_blocked`); the Status sheet warns while calls are allowed.
    pub notifications_off: bool,
    /// The Live list also shows the streams this PC can't watch.
    pub show_unwatchable: bool,
    /// The Settings page shown (Main: the top page).
    pub settings_page: SettingsPage,
    /// Experimental, off by default (`MESHLIVE_UI_SPLIT_PANES=1`): list +
    /// detail panes on Calls and Contacts and Settings' top page beside its pages on
    /// wide windows. Replaced by single scaling columns after design review
    /// (2026-10-21: two panes made every page look divided).
    pub split_panes: bool,
    /// The name being typed, and why the last one was refused.
    pub name_edit: Option<String>,
    /// Settings > Who sees my name: the choice is open.
    pub audience_sheet: bool,
    /// WP6NF: the last settings save failed (Settings says so).
    pub settings_unsaved: bool,
    /// Settings > Privacy > Reset private ID: the question is open.
    pub reset_id_confirm: bool,
    /// The backend's answer to the last reset (shown on the Privacy page).
    pub private_id_reset: Option<bool>,
    /// Everyone blocked (Settings > Privacy > Blocked people).
    pub blocked_people: BlockedView,
    /// The watched stream's authenticated source key ("Block this person").
    watch_source: Option<[u8; 32]>,
    /// The route this PC authenticated for the watched stream (the route to hide).
    watch_route: Option<meshlive::stream_watch::StreamRoute>,
    /// The radio's sound level for the listener's meter, 0..1.
    watch_level: f32,
    /// The current watch's id (its task's updates carry it).
    watch_id: Option<u64>,
    /// The last watch id given to a task (a new watch increments it).
    watch_serial: u64,
    /// Where the last block or unblock was asked for (what its note says).
    block_from: Option<blocked::BlockFrom>,
    /// What the last block or unblock did (Android's toast), until Close or a page change.
    pub block_note: Option<String>,
    /// "Block %s?" is open (Recent, a contact).
    block_confirm: Option<(blocked::BlockTarget, String)>,
    /// "Unblock %s?" is open (Blocked people).
    unblock_confirm: Option<([u8; 32], String)>,
    /// "Reset the block list?" is open.
    reset_blocks_confirm: bool,
    /// "Blocked and hidden on this PC." with Tell MeshLive is open (after a Report).
    report_done: bool,
    /// No email app opened a draft: the address is shown to copy.
    mail_fallback: bool,
    pub name_error: Option<String>,
    /// The first-run welcome's name field.
    welcome_name: String,
    /// The first-run page shown (an index into `WelcomePage::ALL`).
    welcome_page: usize,
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
            self_pictures: None,
            live_unsupported: None,
            call_details_open: false,
            self_seen: 0,
            self_texture: None,
            self_size: [4, 3],
            video: None,
            backend,
            #[cfg(test)]
            native_check: (std::env::var("MESHLIVE_NATIVE_UI_CHECK").as_deref() == Ok("1")).then(native_call_check::State::new),
            demo: false,
            searching: true,
            bluetooth_note: None,
            bluetooth_stuck: false,
            call_mic: None,
            call_mic_restarting: false,
            hold_call_talk: std::env::var("MESHLIVE_CALL_TALK_HOLD").as_deref() == Ok("1"),
            mute_hold: HoldToTalk::default(),
            cameras: 0,
            call_outputs: Vec::new(),
            call_output: None,
            output_sheet: false,
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
            people_query: String::new(),
            people_visible_sent: false,
            incoming_alert: None,
            clipboard_request: None,
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
            manual_video: false,
            live_note: None,
            clock_offset: std::time::Duration::ZERO,
            live_limits: ReminderLimits::ANDROID,
            block_unknown_offer: false,
            live_reminder: false,
            live_reminder_alert: false,
            own_name: None,
            own_id: None,
            identity_key: None,
            computer_name: None,
            own_generated: None,
            answer_as: None,
            selected_person: None,
            selected_contact: None,
            contact_more: None,
            claim_rename: None,
            status_open: None,
            notifications_off: false,
            show_unwatchable: false,
            settings_page: SettingsPage::Main,
            split_panes: std::env::var("MESHLIVE_UI_SPLIT_PANES").ok().as_deref() == Some("1"),
            name_edit: None,
            audience_sheet: false,
            settings_unsaved: false,
            reset_id_confirm: false,
            private_id_reset: None,
            blocked_people: BlockedView::default(),
            watch_source: None,
            watch_route: None,
            watch_level: 0.0,
            watch_id: None,
            watch_serial: 0,
            block_from: None,
            block_note: None,
            block_confirm: None,
            unblock_confirm: None,
            reset_blocks_confirm: false,
            report_done: false,
            mail_fallback: false,
            name_error: None,
            welcome_name: String::new(),
            welcome_page: 0,
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
    /// off by default; `--demo` or Settings > Experiments).
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
        // The new backend reports its own Bluetooth state and visibility
        // (the old one's "stopped responding" note must not stay on screen).
        self.bluetooth_stuck = false;
        self.pc_visible = false;
        self.visibility_note = None;
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
        self.tick_live();
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
            Update::BluetoothNotResponding(stuck) => self.bluetooth_stuck = stuck,
            Update::Streams(list) => {
                self.streams = list;
                self.searching = false;
            }
            Update::People(list) => self.people = list,
            Update::WatchAvailable(_) => {},
            Update::LiveSupport(reason) => self.live_unsupported = reason,
            Update::WatchNote(note) => self.watch_note = note,
            Update::WatchRoute(hops) => if let Some(w) = self.watch.as_mut() { w.route = if hops == 0 { Reach::Direct } else { Reach::Via(hops) }; },
            Update::WatchTitle(title) => if let Some(w) = self.watch.as_mut() { w.stream.title = Some(title); },
            Update::WatchLevel(level) => if self.watch.is_some() { self.watch_level = level; },
            Update::Contacts(list) => self.contacts = list,
            Update::ContactsProblem(problem) => self.contacts_problem = problem,
            Update::Renamed { contact, outcome } => {
                if self.rename.as_ref().is_some_and(|r| r.0 == contact) {
                    match outcome.message() {
                        None => self.rename = None,
                        Some(message) => self.rename.as_mut().unwrap().2 = Some(message),
                    }
                }
                // "Use it as my label" (Android `renameTo`'s toast).
                if self.claim_rename.as_ref().is_some_and(|(id, _)| *id == contact) {
                    let (_, name) = self.claim_rename.take().unwrap_or_default();
                    self.block_note = Some(match outcome.message() {
                        None => format!("Saved as {name}"),
                        Some(message) => message.to_owned(),
                    });
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
            Update::Cameras(count) => self.cameras = count,
            Update::CallOutputs { outputs, current } => {
                if self.call.is_some() {
                    self.call_outputs = outputs;
                    self.call_output = current;
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
            Update::SettingsSaved(saved) => self.settings_unsaved = !saved,
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
            Update::CallSelfPictures(handle) => {
                self.self_seen = 0;
                self.self_pictures = Some(handle);
                // A new camera start: never show the previous start's last picture.
                self.self_texture = None;
            }
            Update::CallPeerGenerated(name) => {
                if let Some(call) = self.call.as_mut().filter(|c| !c.verified) {
                    call.peer = name;
                }
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
            Update::CallBlockedUnknown => {
                // Android `recordBlocked`: an incoming call, declined, nobody named.
                let at = self.now();
                self.recent.insert(
                    0,
                    RecentCall {
                        peer: String::new(),
                        voice: true,
                        outgoing: false,
                        result: "Declined".into(),
                        contact: None,
                        fingerprint: None,
                        blocked: false,
                        declined_at: Some(at),
                    },
                );
                self.recent.truncate(50);
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
                    // "Save contact as": your label for a contact scanned
                    // again, else empty (never their ID; empty keeps it).
                    if let MeetEvent::Ask { name } = &event {
                        if meet.name.is_empty() {
                            meet.name = saved_label(&self.contacts, name).unwrap_or_default();
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
            Update::ContactsNearby { .. } | Update::ContactsChanged | Update::PeerClaimedName { .. } => {}
            Update::IncomingCall { peer, verified, invite, fingerprint } => {
                self.incoming(&peer, verified, &invite, fingerprint)
            }
            Update::Live(event) => {
                let now = self.now();
                self.golive.on_at(event, now, self.live_limits);
            }
            Update::LiveLevel(level) => {
                if self.golive.running() {
                    self.golive.level = level;
                }
            }
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
            Update::IdentityKey(key) => self.identity_key = key,
            Update::ComputerName(name) => self.computer_name = name,
            Update::MailDraftFailed => self.mail_fallback = true,
            Update::PrivateIdReset(done) => self.private_id_reset = Some(done),
            Update::Blocked(view) => {
                // Recent rows follow the list (an unblock on Blocked people
                // shows Block again); an unreadable list changes nothing.
                if !view.unreadable {
                    for call in self.recent.iter_mut() {
                        if let Some(fingerprint) = call.fingerprint {
                            call.blocked = view.people.iter().any(|p| p.fingerprint == fingerprint);
                        }
                    }
                }
                self.blocked_people = view;
            }
            Update::BlockChanged { fingerprint, name, change } => {
                let from = self.block_from.take().unwrap_or(blocked::BlockFrom::Confirmed);
                // Report (Android `LocalReportPolicy.report`): a block that held
                // opens "Blocked and hidden" with Tell MeshLive; a failure is
                // said as for any block, with no draft offered.
                let held = matches!(change, meshlive::blocking::BlockChange::Blocked | meshlive::blocking::BlockChange::AlreadyBlocked);
                if from == blocked::BlockFrom::Report && held {
                    self.report_done = true;
                } else {
                    self.block_note = Some(blocked::change_note(from, change, &name));
                }
                // Recent follows what was saved: a Full or Failed block leaves
                // the row as it was (the caller still rings).
                let blocked = match change {
                    meshlive::blocking::BlockChange::Blocked | meshlive::blocking::BlockChange::AlreadyBlocked => Some(true),
                    meshlive::blocking::BlockChange::Unblocked => Some(false),
                    meshlive::blocking::BlockChange::Full | meshlive::blocking::BlockChange::Failed => None,
                };
                if let Some(blocked) = blocked {
                    for call in self.recent.iter_mut().filter(|c| c.fingerprint == Some(fingerprint)) {
                        call.blocked = blocked;
                    }
                }
            }
            Update::WatchSource { id, fingerprint, route } => {
                if self.watch_id == Some(id) && self.watch.is_some() {
                    self.watch_source = Some(fingerprint);
                    self.watch_route = Some(route);
                }
            }
            Update::WatchSourceBlocked { id } => {
                if self.watch_id == Some(id) {
                    self.leave_watch();
                }
            }
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
    fn incoming(&mut self, peer: &str, verified: bool, invite: &meshlive::call::Message, fingerprint: Option<[u8; 32]>) {
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
        self.call_fingerprint = fingerprint;
        self.call = Some(model);
        self.call_mic = None;
        self.call_mic_restarting = false;
        self.call_camera = None;
        self.mute_hold = HoldToTalk::default();
        self.call_outputs.clear();
        self.call_sending = None;
        self.call_receiving = None;
        self.call_recorded = false;
        self.call_end = None;
        self.focus_primary = true;
        self.incoming_alert = Some((peer.to_owned(), video));
    }
    /// "You appear as ...": this PC's generated name while it is on air (as
    /// Android's nameplate line; friendly names are always on).
    pub fn appear_as(&self) -> Option<&str> {
        self.own_generated.as_deref()
    }
    /// The incoming call to announce outside the window (taken once).
    pub fn take_incoming_alert(&mut self) -> Option<(String, bool)> {
        self.incoming_alert.take()
    }
    /// "Still broadcasting" to announce outside the window (taken once).
    pub fn take_live_reminder_alert(&mut self) -> bool {
        std::mem::take(&mut self.live_reminder_alert)
    }
    /// A broadcast is starting or on air (the window keeps its model ticking).
    pub fn live_running(&self) -> bool {
        self.golive.running()
    }
    /// The audio reminder (Android `AudioLiveController.tick`): "Still
    /// broadcasting" once it is due; the broadcast stops when nobody answered.
    fn tick_live(&mut self) {
        let now = self.now();
        let Some(reminder) = self.golive.reminder.as_mut() else {
            self.live_reminder = false;
            return;
        };
        match reminder.decide(now) {
            ReminderAction::None => {}
            ReminderAction::Remind => {
                self.live_reminder = true;
                self.live_reminder_alert = true;
            }
            ReminderAction::Stop => {
                self.live_reminder = false;
                self.stop_live();
                self.dialog = Some(("Audio broadcast stopped".into(), LIVE_REMINDER_STOPPED.into()));
            }
        }
    }
    /// F2 (Android FLAG_SECURE): Add contact is open, so this PC's code can be
    /// on screen. The window stays out of captures for the whole visit, so no
    /// frame with the code is presented unprotected (main.rs).
    pub fn code_on_screen(&self) -> bool {
        self.meet.is_some()
    }
    /// The code to copy, taken once by the window.
    pub fn take_clipboard_request(&mut self) -> Option<String> {
        self.clipboard_request.take()
    }
    /// The window's copy finished (`ok`: the code is on the clipboard).
    pub fn clipboard_done(&mut self, ok: bool) {
        if let Some(meet) = self.meet.as_mut() {
            meet.copied = Some(ok);
        }
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
                    declined_at: None,
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
        self.watch_source = None;
        self.watch_route = None;
        self.watch_level = 0.0;
        self.watch_serial = self.watch_serial.wrapping_add(1);
        self.watch_id = Some(self.watch_serial);
        self.peer_pictures = None;
        self.peer_texture = None;
        self.video = None;
        self.backend.command(Command::Watch { id, watch: self.watch_serial });
        self.focus_primary = true;
        true
    }
    pub fn leave_watch(&mut self) {
        self.watch_source = None;
        self.watch_route = None;
        self.watch_id = None;
        if let Some(mut watch) = self.watch.take() {
            watch.stop();
            self.backend.command(Command::StopWatching);
        }
    }
    pub fn place_call(&mut self, person: Handle, voice: bool) -> bool {
        if !self.usable(Feature::CallOut) || self.call.is_some() || self.watch.is_some() || self.golive.running() {
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
        self.mute_hold = HoldToTalk::default();
        self.call_outputs.clear();
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
    /// Windows Settings > System > About (rename this PC).
    pub fn open_about_settings(&mut self) {
        self.backend.command(Command::OpenAboutSettings);
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
    /// Uploads this PC's newest self-view picture (if any); dropped with the call.
    fn show_self_picture(&mut self, ctx: &egui::Context) {
        if self.call.is_none() {
            self.self_pictures = None;
            self.self_texture = None;
            return;
        }
        let Some(handle) = self.self_pictures.clone() else { return };
        let Some(picture) = handle.0.take_newer(self.self_seen) else { return };
        self.self_seen = picture.seq;
        let image = egui::ColorImage::from_rgba_unmultiplied([picture.width as usize, picture.height as usize], &picture.rgba);
        match self.self_texture.as_mut() {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => self.self_texture = Some(ctx.load_texture("self-video", image, egui::TextureOptions::LINEAR)),
        }
        self.self_size = [picture.width, picture.height];
    }
    /// The self-view texture while this call's camera is on.
    pub(crate) fn self_video(&self) -> Option<(egui::TextureId, [u32; 2])> {
        let on = self.call.as_ref().is_some_and(|c| c.camera) && matches!(self.call_camera, Some(None));
        self.self_texture.as_ref().filter(|_| on).map(|t| (t.id(), self.self_size))
    }
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
        // "Warn when video freezes" counts from the last picture shown.
        let now = self.now();
        if let Some(watch) = self.watch.as_mut() {
            watch.picture(now);
        }
        let skipped = picture.seq.saturating_sub(self.peer_seen + 1);
        self.peer_seen = picture.seq;
        // Turned upright here (the sender's clockwise quarter turns), so every
        // screen draws it as a plain picture with rounded corners.
        self.peer_turns = handle.0.turns();
        let (w, h, upright) = call_layout::rotate_rgba(&picture.rgba, picture.width as usize, picture.height as usize, self.peer_turns);
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &upright);
        match self.peer_texture.as_mut() {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => self.peer_texture = Some(ctx.load_texture("peer-video", image, egui::TextureOptions::LINEAR)),
        }
        self.video = self.peer_texture.as_ref().map(|t| t.id());
        self.peer_size = [w as u32, h as u32];
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
    /// The navigation's pages, always all four (like Android): where this PC
    /// can't run live streams, the Live page says why.
    pub fn pages(&self) -> Vec<Page> {
        Page::ALL.to_vec()
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
            || self.call.as_ref().is_some_and(|c| c.phase() != Phase::Ended) || !self.live_request().valid() || !self.golive.start() {
            return false;
        }
        self.live_note = None;
        if self.live_consent_required {
            self.backend.command(Command::ConfigureLive(self.live_request()));
            self.backend.command(Command::LiveExposureConsent);
        }
        self.backend.command(Command::GoLive {
            kind: self.golive.kind,
            title: self.golive.title.clone(),
            initial: meshlive::stream_source::InitialMedia { muted: self.golive.muted, camera_off: self.golive.camera_off },
        });
        true
    }
    /// What Go live sends: with Manual video settings off (Standard) the
    /// default video and the radio quality chosen on Go live; on, exactly the
    /// encoder and quality chosen in Settings > Manual video.
    fn live_request(&self) -> meshlive::stream_source::Settings {
        if self.manual_video {
            self.live_settings
        } else {
            meshlive::stream_source::Settings { radio_preset: self.live_settings.radio_preset, ..Default::default() }
        }
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
    /// Audio only's "Switch to video" (Android `switchAudioLiveToVideo`): the
    /// radio broadcast ends and a video broadcast starts with the same title
    /// (asking first, as every start does).
    pub fn switch_to_video(&mut self) {
        if self.golive.kind != LiveKind::AudioOnly || !matches!(self.golive.state, LiveState::Live | LiveState::Paused) {
            return;
        }
        self.stop_live();
        self.golive.kind = LiveKind::Video;
        self.start_live();
    }
    /// The clock of the live timer and the audio reminder.
    pub(crate) fn now(&self) -> std::time::Instant {
        std::time::Instant::now() + self.clock_offset
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
        if self.golive.running() {
            // The timer and the audio reminder tick once a second on any page.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        self.show_peer_picture(ctx);
        self.show_self_picture(ctx);
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
                        let pushed = (self.page == Page::Settings && self.settings_page != SettingsPage::Main && !self.settings_split(ui))
                            .then_some(self.settings_page);
                        if let Some(page) = pushed {
                            // Android: a pushed Settings page has the Back arrow and
                            // keeps the status indicator. Escape is a dialog's or the
                            // Status sheet's while one is open.
                            let back = self.title_with_status(ui, page.title(), Some("Back"));
                            let escape = self.status_open.is_none()
                                && !self.settings_dialog_open()
                                && ui.input(|i| i.key_pressed(egui::Key::Escape));
                            if back || escape {
                                self.settings_page = page.parent();
                            }
                        } else {
                            let _ = self.title_with_status(ui, self.page.label(), None);
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
        self.status_popover(ctx);
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

    /// The one status at the end of every title bar (Android `statusKind`):
    /// the first that applies, Bluetooth first, so a live stream never hides
    /// a Bluetooth problem.
    pub(crate) fn status_kind(&self) -> StatusKind {
        use meshlive::bluetooth::Radio;
        match self.bluetooth.map(|caps| caps.radio) {
            None => StatusKind::Checking,
            Some(Radio::Missing) => StatusKind::NoBluetooth,
            Some(Radio::Off | Radio::Disabled) => StatusKind::BluetoothOff,
            Some(Radio::Denied) => StatusKind::Permissions,
            Some(Radio::Unknown | Radio::Unavailable) => StatusKind::BluetoothUnavailable,
            // Windows Bluetooth stopped answering this app (a start call never
            // returned): a Bluetooth problem, so it comes before watching or live.
            Some(Radio::On) if self.bluetooth_stuck => StatusKind::BluetoothUnavailable,
            Some(Radio::On) if self.watch.is_some() => StatusKind::Watching,
            Some(Radio::On) if self.golive.running() => StatusKind::Live,
            Some(Radio::On) if !self.reachable_now() => StatusKind::Hidden,
            Some(Radio::On) if self.keys_damaged() => StatusKind::ResetNeeded,
            Some(Radio::On) => StatusKind::Ready,
        }
    }
    /// The title row of a tab, Add contact or Go live, with the status
    /// indicator at its end; a click on the indicator opens or closes the
    /// Status sheet. Returns true when Back was used.
    pub(crate) fn title_with_status(&mut self, ui: &mut egui::Ui, title: &str, back: Option<&str>) -> bool {
        let kind = self.status_kind();
        let word = kind.shows_word().then(|| kind.word());
        let (went_back, chip) = components::status_top_bar(ui, title, back, word, kind.a11y(), kind.tone());
        if let Some(chip) = chip.filter(|chip| chip.clicked()) {
            self.status_open = if self.status_open.is_some() { None } else { Some(chip.rect) };
        }
        went_back
    }
    /// Phones can ring this PC now (as far as this PC knows).
    fn reachable_now(&self) -> bool {
        self.calls_allowed() && (self.pc_visible || self.visibility_note.is_none())
    }
    /// The saved contact keys are damaged (the popover offers the reset).
    fn keys_damaged(&self) -> bool {
        self.visibility_note.as_deref().is_some_and(|n| n.contains(KEYS_DAMAGED))
    }
    /// The Status sheet under the indicator (Android `sheet_status`):
    /// Bluetooth, Access, Allow calls, Private Bluetooth ID, Nearby you appear
    /// as, the notifications warning and one action. Allow calls has no switch
    /// here (its home is Settings). Closes on Escape or a click outside.
    fn status_popover(&mut self, ctx: &egui::Context) {
        use meshlive::bluetooth::Radio;
        let Some(anchor) = self.status_open else { return };
        // The call and Watch screens have no indicator; Add contact and Go live do.
        if self.call.is_some() || self.watch.is_some() {
            self.status_open = None;
            return;
        }
        let radio = self.bluetooth.map(|caps| caps.radio);
        let bluetooth = match radio {
            None => "Checking\u{2026}",
            Some(Radio::On) if self.bluetooth_stuck => "Not responding",
            Some(Radio::On) => "On",
            Some(Radio::Off | Radio::Disabled) => "Off",
            Some(Radio::Denied) => "Needs permission",
            Some(Radio::Missing | Radio::Unknown | Radio::Unavailable) => "Not available",
        };
        // Windows asks for the microphone and camera when a call first needs
        // them, so only Bluetooth access is known here.
        let access = match radio {
            None => "Checking\u{2026}",
            Some(Radio::Denied) => "Bluetooth needs permission",
            Some(_) => "Bluetooth allowed",
        };
        let streaming = self.watch.is_some() || self.golive.running();
        let calls = if self.watch.is_some() {
            StatusKind::Watching.word()
        } else if self.golive.running() {
            StatusKind::Live.word()
        } else if self.reachable_now() {
            "On"
        } else {
            "Not available for nearby calls"
        };
        let fix_bluetooth = self.bluetooth_problem().is_some();
        let offer_reset = !fix_bluetooth && self.switches.receive_calls && self.keys_damaged();
        let width = 400.0f32.min(ctx.content_rect().width() - 32.0).max(200.0);
        let left = (anchor.right() - width).max(ctx.content_rect().left() + 16.0);
        let mut open_bluetooth = false;
        let mut reset = false;
        let area = egui::Area::new(egui::Id::new("status-popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(left, anchor.bottom() + 6.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(theme::RAISED)
                    .stroke(egui::Stroke::new(1.0, Color32::from_gray(60)))
                    .corner_radius(14)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(width - 28.0);
                        ui.spacing_mut().item_spacing.y = 8.0;
                        ui.label(RichText::new("Status").font(theme::Type::Headline.font()).color(theme::TEXT));
                        status_row(ui, "Bluetooth", bluetooth);
                        status_row(ui, "Access", access);
                        status_row(ui, "Allow calls", calls);
                        if let Some(reason) = self.unavailable_reason(Feature::ReceiveCalls) {
                            ui.add(egui::Label::new(theme::secondary(format!("Phones can't call this PC. {reason}"))).wrap());
                        } else if let Some(note) = self.visibility_note.clone().filter(|_| self.switches.receive_calls && !streaming) {
                            ui.add(egui::Label::new(theme::secondary(note)).wrap());
                        } else if self.reachable_now() && !streaming {
                            ui.label(theme::caption(AWAKE_NOTE));
                        }
                        status_row(ui, "Private Bluetooth ID", "On");
                        // Android's order: the notifications warning, then the name.
                        if self.notifications_off && self.calls_allowed() {
                            ui.add(egui::Label::new(theme::secondary(NOTIFICATIONS_OFF)).wrap());
                        }
                        if let Some(name) = self.appear_as() {
                            status_row(ui, "Nearby you appear as", &format!("{name} \u{b7} changes every 15 min"));
                        }
                        if fix_bluetooth {
                            ui.add_space(4.0);
                            open_bluetooth = Pill::new("Open Bluetooth settings").kind(Kind::Secondary).fill_width().show(ui).clicked();
                        } else if offer_reset {
                            ui.add_space(4.0);
                            reset = Pill::new("Reset").kind(Kind::Secondary).small().a11y("Reset this PC's contact keys").show(ui).clicked();
                        }
                    });
            });
        if open_bluetooth {
            self.open_bluetooth_settings();
            self.status_open = None;
        }
        if reset {
            self.backend.command(Command::ResetPresenceKey);
        }
        let outside = ctx.input(|i| {
            i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !anchor.contains(p))
        });
        if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.status_open = None;
        }
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
        // Android's toast after a block or unblock: one line until Close.
        if let Some(text) = self.block_note.clone() {
            let mut close = false;
            ui.horizontal_wrapped(|ui| {
                ui.label(theme::body(&text));
                close = Pill::new("Close").kind(Kind::Quiet).small().show(ui).clicked();
            });
            if close {
                self.block_note = None;
            }
            ui.add_space(12.0);
        }
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
        if caps.radio == Radio::On && self.bluetooth_stuck {
            return Some(meshlive::bluetooth::radio_watch::RESTART_GUIDANCE);
        }
        matches!(caps.radio, Radio::Off | Radio::Disabled | Radio::Denied).then(|| caps.status())
    }

    /// Leaves Watch and Go live screens (a running broadcast keeps running).
    fn leave_screens(&mut self) {
        self.leave_watch();
        self.golive_open = false;
        self.close_meet();
        self.block_note = None;
        // The Settings tab opens at its top page again.
        self.settings_page = SettingsPage::Main;
    }
    /// Add contact: scan the code another phone shows (typed or pasted).
    pub fn open_meet(&mut self) {
        self.meet = Some(Meet::default());
        self.backend.command(Command::ShowOwnCode);
        self.focus_primary = true;
    }
    /// Add contact on one segment (Android `EXTRA_SHOW_CODE`): Settings >
    /// Your code and the contact sheet's "Show my code" open on the code;
    /// everything else opens on "Scan their code".
    pub fn open_meet_on(&mut self, show_code: bool) {
        self.open_meet();
        if let Some(meet) = self.meet.as_mut() {
            meet.show_code = show_code;
        }
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
        // First run (Android `OnboardingActivity`): Welcome (with this PC's
        // optional name), Allow access, Start with Windows, You're all set;
        // the step dots, the main button (Enter), Back (Escape).
        if !self.switches.onboarded && self.call.is_none() {
            let index = self.welcome_page.min(WelcomePage::ALL.len() - 1);
            let page = WelcomePage::ALL[index];
            // Android's Background page: Allow, or Next once it is on.
            let offer_startup = page == WelcomePage::Startup && !self.switches.start_with_windows;
            let (mut next, mut not_now, mut back) = (false, false, false);
            dialog(ctx, "welcome", |ui| {
                ui.add_space(4.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 24.0, theme::RAISED);
                draw_icon(ui.painter(), rect.shrink(12.0), page.icon(), theme::TEXT);
                ui.add_space(6.0);
                ui.label(theme::title(page.title()));
                match page {
                    WelcomePage::Welcome => {
                        dialog_text(ui, "Calls and livestreams over Bluetooth. No internet or account needed.");
                        dialog_text(ui, "Nearby phones can help pass livestreams along, extending their reach.");
                        ui.add_space(6.0);
                        let field = components::text_field(ui, &mut self.welcome_name, Some("Your name (optional)"), "Name", Some(32));
                        if self.focus_primary {
                            field.request_focus();
                            self.focus_primary = false;
                        }
                        ui.label(theme::caption(format!("{NAME_WHO} You can change it in Settings.")));
                        // Enter in the name field moves on too (the field takes the key).
                        next |= field.lost_focus() && ui.input(|i| i.key_down(egui::Key::Enter));
                    }
                    WelcomePage::Access => {
                        dialog_text(ui, "You can change this later in Windows Settings.");
                        ui.add_space(4.0);
                        let (bluetooth, ready) = match &self.bluetooth {
                            None => ("Checking Bluetooth\u{2026}", false),
                            Some(_) if self.usable(Feature::CallOut) => ("Bluetooth is ready.", true),
                            Some(_) => ("Turn on Bluetooth in Windows Settings to find people and call.", false),
                        };
                        icon_line(ui, if ready { Icon::Check } else { Icon::Bluetooth }, bluetooth);
                        icon_line(ui, Icon::Mic, "Windows asks for the microphone and camera the first time a call needs them.");
                        if !ready && self.bluetooth.is_some() {
                            self.bluetooth_settings_button(ui);
                        }
                    }
                    WelcomePage::Startup => {
                        dialog_text(ui, "MeshLive starts in the notification area when you sign in, so phones can call this PC. Phones can't reach it while it sleeps.");
                        dialog_text(ui, "You can change this later in Settings.");
                    }
                    WelcomePage::Ready => {
                        dialog_text(ui, "To call: both open MeshLive, stay within about 20 m, and choose them in Calls.");
                    }
                }
                ui.add_space(8.0);
                step_dots(ui, index, WelcomePage::ALL.len());
                ui.add_space(8.0);
                let main = match page {
                    WelcomePage::Ready => "Start",
                    _ if offer_startup => "Allow",
                    _ => "Next",
                };
                next |= Pill::new(main).fill_width().show(ui).clicked();
                if offer_startup {
                    not_now = Pill::new("Not now").kind(Kind::Quiet).fill_width().show(ui).clicked();
                }
                if index > 0 {
                    back = Pill::new("Back").kind(Kind::Quiet).fill_width().show(ui).clicked();
                }
                // Enter is the main button wherever the focus is; Escape goes back.
                next |= ui.input(|i| i.key_pressed(egui::Key::Enter));
                back |= index > 0 && ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
            if offer_startup && next {
                // Android's Allow: on now (Settings > Start with Windows).
                self.switches.start_with_windows = true;
                self.switches_changed();
            }
            let done = page == WelcomePage::Ready && next;
            if back {
                self.welcome_page = index - 1;
            } else if (next || not_now) && !done {
                self.welcome_page = index + 1;
            }
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
                dialog_text(ui, &format!("{NAME_WHO} Leave it empty to send no name."));
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
        if self.reset_id_confirm {
            let (mut confirm, mut cancel) = (false, false);
            dialog(ctx, "reset-private-id", |ui| {
                dialog_title(ui, "Reset private ID?");
                dialog_text(ui, settings::RESET_ID_CONFIRM);
                let (go, no) = dialog_buttons(ui, "Reset", Kind::Primary, true, Some("Cancel"));
                confirm = go;
                cancel = no || ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
            if confirm {
                self.reset_id_confirm = false;
                self.private_id_reset = None;
                self.backend.command(Command::ResetPresenceKey);
            } else if cancel {
                self.reset_id_confirm = false;
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
        self.call_output_sheet(ctx);
        self.names_audience_sheet(ctx);
        self.blocking_dialogs(ctx);
        self.report_dialogs(ctx);
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
        if self.live_reminder {
            let (mut keep, mut stop) = (false, false);
            dialog(ctx, "live-reminder", |ui| {
                dialog_title(ui, "Still broadcasting");
                dialog_text(ui, LIVE_REMINDER);
                // Escape is Stop: a forgotten microphone is the risk.
                (keep, stop) = dialog_buttons(ui, "Keep broadcasting", Kind::Primary, true, Some("Stop"));
            });
            if keep {
                let now = self.now();
                if let Some(reminder) = self.golive.reminder.as_mut() {
                    reminder.on_user_present(now);
                }
                self.live_reminder = false;
            } else if stop {
                self.live_reminder = false;
                self.stop_live();
            }
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
        self.contact_sheet(ctx);
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

/// The first-run pages in order (Android `OnboardingActivity.Page`; Hide
/// your phone's name and full-screen calls have no PC counterpart).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WelcomePage {
    Welcome,
    Access,
    Startup,
    Ready,
}
impl WelcomePage {
    const ALL: [WelcomePage; 4] = [WelcomePage::Welcome, WelcomePage::Access, WelcomePage::Startup, WelcomePage::Ready];
    /// Android `ob_*_title` ("Start with Windows" stands for "Work with the screen off").
    fn title(self) -> &'static str {
        match self {
            WelcomePage::Welcome => WELCOME_TITLE,
            WelcomePage::Access => "Allow access",
            WelcomePage::Startup => "Start with Windows",
            WelcomePage::Ready => "You're all set",
        }
    }
    fn icon(self) -> Icon {
        match self {
            WelcomePage::Welcome => Icon::Broadcast,
            WelcomePage::Access => Icon::Bluetooth,
            WelcomePage::Startup => Icon::Settings,
            WelcomePage::Ready => Icon::Check,
        }
    }
}
/// The first page's title (Android `ob_welcome_title`).
pub(crate) const WELCOME_TITLE: &str = "Call and go live with people nearby";

/// The first-run dots (Android `renderDots`): the current page larger and
/// white; read as "Step 2 of 4" (`ob_page_of`).
fn step_dots(ui: &mut egui::Ui, index: usize, count: usize) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 12.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("Step {} of {count}", index + 1)));
    let gap = 16.0;
    let first = rect.center().x - gap * (count as f32 - 1.0) * 0.5;
    for i in 0..count {
        let (radius, color) = if i == index { (4.0, theme::TEXT) } else { (3.0, theme::TEXT_DISABLED) };
        ui.painter().circle_filled(egui::pos2(first + gap * i as f32, rect.center().y), radius, color);
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
/// Android `audio_live_reminder_text`, without "Tap to open it" (it is open).
pub(crate) const LIVE_REMINDER: &str = "Your audio broadcast is still on. Keep broadcasting, or stop it.";
/// Android `audio_live_reminder_stopped`.
pub(crate) const LIVE_REMINDER_STOPPED: &str = "Audio broadcast stopped: nobody answered the reminder.";

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

