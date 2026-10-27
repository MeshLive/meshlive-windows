//! Screen state-machine tests against fake core sessions. No pixels, radio,
//! camera, microphone or audio endpoint is involved.
use super::{backend::*, model::*, *};
use meshlive::{
    bluetooth::{
        Capabilities, Radio,
        gate::{NO_PERIPHERAL, PeripheralProof},
    },
    call::{Message, Phase, Reason, Signal},
};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

#[test]
fn every_pc_broadcast_requires_fresh_exposure_consent_and_cancel_never_starts() {
    let mut r = Rig::full(); r.ui.live_consent_required = true; r.ui.open_go_live();
    r.click("Go live");
    assert!(r.contains(meshlive::stream_source::EXPOSURE));
    assert!(!r.log().iter().any(|c| matches!(c, Command::GoLive { .. })));
    r.click("Cancel");
    assert!(!r.ui.golive.running());
    assert!(!r.log().iter().any(|c| matches!(c, Command::LiveExposureConsent)));
    r.click("Go live"); r.click("Go live");
    assert_eq!(&r.log()[r.log().len() - 3..], &[
        Command::ConfigureLive(meshlive::stream_source::Settings::default()), Command::LiveExposureConsent,
        Command::GoLive { kind: LiveKind::Video, title: String::new(), initial: Default::default() }
    ]);
    r.ui.apply(Update::Live(LiveEvent::Started)); r.ui.stop_live();
    r.click("Go live");
    assert!(r.contains(meshlive::stream_source::EXPOSURE));
    assert_eq!(r.log().iter().filter(|c| matches!(c, Command::GoLive { .. })).count(), 1);
}

#[test]
fn stream_modes_hold_call_actions_until_the_user_leaves_or_stops() {
    let mut r = Rig::full(); r.populate(); assert!(r.ui.start_watch(1));
    assert!(!r.ui.place_call(10, false)); assert!(!r.ui.start_live());
    r.ui.leave_watch(); assert!(r.ui.place_call(10, false));
    let mut r = Rig::full(); r.populate(); assert!(r.ui.start_live());
    assert!(!r.ui.place_call(10, false) && !r.ui.start_watch(1));
    r.ui.apply(Update::Live(LiveEvent::Started)); r.ui.stop_live();
    assert!(r.ui.place_call(10, false));
}

#[derive(Default)]
struct Shared {
    log: Vec<Command>,
    inbox: VecDeque<Update>,
    /// The Windows Bluetooth owner: no live streams.
    no_streams: bool,
    no_go_live: bool,
}
/// A fake core whose traffic the test both reads and writes.
struct Fake(Rc<RefCell<Shared>>);
impl Backend for Fake {
    fn command(&mut self, command: Command) {
        self.0.borrow_mut().log.push(command);
    }
    fn poll(&mut self) -> Vec<Update> {
        self.0.borrow_mut().inbox.drain(..).collect()
    }
    fn new_call_id(&mut self) -> [u8; 16] {
        [0x42; 16]
    }
    fn streams(&self) -> bool {
        !self.0.borrow().no_streams
    }
    fn can_go_live(&self) -> bool { !self.0.borrow().no_go_live && self.streams() }
}

const ID: [u8; 16] = [0x42; 16];

fn caps(central: bool, peripheral: bool) -> Capabilities {
    Capabilities {
        radio: Radio::On,
        low_energy: Some(true),
        central: Some(central),
        peripheral: Some(peripheral),
        extended_advertising: None,
        max_advertisement_bytes: None,
        error: None,
    }
}
fn stream(id: Handle, audio_only: bool) -> StreamEntry {
    StreamEntry {
        id,
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
struct Rig {
    size: egui::Vec2,
    time: Option<f64>,
    focused: bool,
    ui: Ui,
    shared: Rc<RefCell<Shared>>,
    ctx: egui::Context,
}
impl Rig {
    /// A UI whose adapter can do everything.
    fn full() -> Self {
        let mut rig = Self::bare();
        rig.ui.bluetooth = Some(caps(true, true));
        rig.ui.peripheral_proof = PeripheralProof::Started;
        rig
    }
    /// The laptop case: central only, outgoing calls only.
    fn outgoing_only() -> Self {
        let mut rig = Self::bare();
        rig.ui.bluetooth = Some(caps(true, false));
        rig
    }
    fn bare() -> Self {
        let shared = Rc::new(RefCell::new(Shared::default()));
        let ctx = egui::Context::default();
        configure(&ctx);
        ctx.enable_accesskit();
        let mut ui = Ui::with_backend(Box::new(Fake(shared.clone())));
        // The first-run welcome is tested on its own (it would cover every screen).
        ui.switches.onboarded = true;
        Self { size: egui::vec2(1000.0, 720.0), time: None, focused: true, ui, shared, ctx }
    }
    fn send(&mut self, update: Update) {
        self.shared.borrow_mut().inbox.push_back(update);
    }
    fn log(&self) -> Vec<Command> {
        self.shared.borrow().log.clone()
    }
    fn signals(&self) -> Vec<Signal> {
        self.log()
            .into_iter()
            .filter_map(|c| match c {
                Command::CallSignal(m) => Some(m.signal),
                _ => None,
            })
            .collect()
    }
    fn populate(&mut self) {
        self.send(Update::Streams(vec![stream(1, false), stream(2, true)]));
        self.send(Update::People(vec![
            Person { id: 10, name: "Priya".into(), reach: Reach::Direct, verified: true, copied: false },
            Person { id: 11, name: "Blue Otter".into(), reach: Reach::Via(2), verified: false, copied: false },
        ]));
        self.send(Update::Contacts(vec![
            Contact { id: 10, name: "Priya".into(), by_qr: true, nearby: true, lease: true, blocked: false },
            Contact { id: 12, name: "Ravi".into(), by_qr: false, nearby: false, lease: true, blocked: false },
        ]));
        self.ui.pump();
    }
    fn frame(&mut self, events: Vec<egui::Event>) -> egui::FullOutput {
        let size = self.size;
        let ui = &mut self.ui;
        self.ctx.run(
            egui::RawInput {
                time: self.time,
                focused: self.focused,
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| ui.show(ctx, egui::TextureId::User(1), "WARP"),
        )
    }
    /// (role, label-or-value, enabled, bounds center) of every named node.
    fn nodes(&mut self) -> Vec<Seen> {
        let _ = self.frame(vec![]);
        let output = self.frame(vec![]);
        let update = output.platform_output.accesskit_update.expect("AccessKit");
        update
            .nodes
            .iter()
            .map(|(_, node)| Seen {
                role: node.role(),
                text: node.label().or(node.value()).unwrap_or("").to_owned(),
                disabled: node.is_disabled(),
                center: node.bounds().map(|b| egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)),
            })
            .collect()
    }
    /// The bounds of the first node with this exact label (and role, if given).
    fn bounds(&mut self, label: &str, role: Option<egui::accesskit::Role>) -> egui::Rect {
        let _ = self.frame(vec![]);
        let output = self.frame(vec![]);
        let update = output.platform_output.accesskit_update.expect("AccessKit");
        update
            .nodes
            .iter()
            .filter(|(_, n)| n.label().or(n.value()) == Some(label) && role.is_none_or(|r| n.role() == r))
            .find_map(|(_, n)| n.bounds())
            .map(|b| egui::Rect::from_min_max(egui::pos2(b.x0 as f32, b.y0 as f32), egui::pos2(b.x1 as f32, b.y1 as f32)))
            .unwrap_or_else(|| panic!("no node {label:?}"))
    }
    fn texts(&mut self) -> Vec<String> {
        self.nodes().into_iter().map(|n| n.text).collect()
    }
    fn has(&mut self, text: &str) -> bool {
        self.texts().iter().any(|t| t == text)
    }
    fn contains(&mut self, text: &str) -> bool {
        self.texts().iter().any(|t| t.contains(text))
    }
    /// A real pointer click on the control with this exact accessible name.
    fn click(&mut self, label: &str) {
        let node = self
            .nodes()
            .into_iter()
            .find(|n| n.text == label && n.center.is_some() && n.is_control())
            .unwrap_or_else(|| panic!("no control named {label:?}"));
        assert!(!node.disabled, "{label:?} is disabled");
        let pos = node.center.unwrap();
        let _ = self.frame(vec![egui::Event::PointerMoved(pos)]);
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        let _ = self.frame(vec![button(true)]);
        let _ = self.frame(vec![button(false)]);
        let _ = self.frame(vec![]);
    }
    fn disabled(&mut self, label: &str) -> bool {
        self.nodes()
            .into_iter()
            .find(|n| n.text == label && n.is_control())
            .unwrap_or_else(|| panic!("no control named {label:?}"))
            .disabled
    }
}
struct Seen {
    role: egui::accesskit::Role,
    text: String,
    disabled: bool,
    center: Option<egui::Pos2>,
}

impl Seen {
    fn is_control(&self) -> bool {
        use egui::accesskit::Role;
        matches!(self.role, Role::Button | Role::CheckBox | Role::Switch | Role::Link)
    }
}

fn ctrl(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    }
}

// ---------------------------------------------------------------- shell

#[test]
fn live_navigation_follows_watch_capability_and_unsupported_video_does_not_block_radio() {
    let mut rig = Rig::full(); rig.populate(); rig.ui.pump();
    rig.shared.borrow_mut().no_go_live = true;
    rig.ui.page = Page::Home;
    rig.ui.streams[0].unavailable = Some("This PC can't decode this stream");
    assert!(rig.has("Live"));
    assert!(!rig.has("Go live"));
    assert!(rig.contains("This PC can't decode this stream"));
    assert!(!rig.ui.start_watch(1));
    assert!(rig.ui.start_watch(2));
    rig.send(Update::WatchNote(Some("Couldn't join. Update MeshLive on that phone, and turn on Find PCs.".into())));
    assert!(rig.contains("turn on Find PCs"));
    rig.send(Update::Watch(WatchEvent::Connected));
    rig.send(Update::WatchNote(None));
    assert!(!rig.contains("turn on Find PCs"));
    rig.ui.leave_watch();
    rig.shared.borrow_mut().no_streams = true;
    rig.ui.page = Page::Home;
    assert!(!rig.has("Live"));
    assert_eq!(rig.ui.page, Page::Calls);
}

#[test]
fn every_page_has_named_navigation_and_every_control_is_labelled() {
    use egui::accesskit::Role;
    for page in Page::ALL {
        let mut rig = Rig::full();
        rig.populate();
        rig.ui.page = page;
        let nodes = rig.nodes();
        for expected in ["Live", "Calls", "Contacts", "Settings"] {
            assert!(nodes.iter().any(|n| n.text == expected), "Missing {expected} on {page:?}");
        }
        for node in &nodes {
            if matches!(node.role, Role::Button | Role::CheckBox | Role::Link | Role::Switch) {
                assert!(!node.text.is_empty(), "unlabelled {:?} on {page:?}", node.role);
            }
        }
        assert!(!nodes.iter().any(|n| n.text == "Show GPU texture test"), "diagnostics start collapsed");
    }
}

#[test]
fn keyboard_navigation_switches_pages_and_the_go_live_footer_stays_visible() {
    let mut rig = Rig::full();
    for (key, page) in [
        (egui::Key::Num2, Page::Calls),
        (egui::Key::Num3, Page::Contacts),
        (egui::Key::Num4, Page::Settings),
        (egui::Key::Num1, Page::Home),
    ] {
        let _ = rig.frame(vec![ctrl(key)]);
        assert_eq!(rig.ui.page, page);
    }
    let nodes = rig.nodes();
    let footer = nodes.iter().find(|n| n.text == "Go live").expect("Go live footer");
    assert!(footer.center.unwrap().y < 720.0);
}

/// Enter is the welcome's default button, wherever the focus is.
#[test]
fn enter_starts_from_the_welcome() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    let _ = rig.frame(vec![]);
    let _ = rig.frame(vec![egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    }]);
    assert!(rig.ui.switches.onboarded);
}

/// Add contact stays pinned in view however long the contact list is.
#[test]
fn add_contact_is_pinned_at_the_bottom_of_a_long_contact_list() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Contacts;
    rig.send(Update::Contacts(
        (0..40).map(|i| Contact { id: 100 + i, name: format!("Person {i}"), by_qr: true, nearby: false, lease: true, blocked: false }).collect(),
    ));
    rig.ui.pump();
    let nodes = rig.nodes();
    let add = nodes.iter().find(|n| n.text == "Add contact").expect("Add contact");
    let y = add.center.expect("on screen").y;
    assert!(y > 600.0 && y < 720.0, "pinned at the bottom, in view (y = {y})");
    rig.click("Add contact");
    assert!(rig.ui.meet.is_some(), "it opens Add contact");
}

#[test]
fn a_pending_crash_report_asks_once_and_share_or_not_now_tell_the_core() {
    for (button, expected) in [("Share", Command::ShareCrashReport), ("Not now", Command::DismissCrashReport)] {
        let mut rig = Rig::full();
        rig.send(Update::CrashReportPending);
        rig.ui.pump();
        assert!(rig.contains("MeshLive closed unexpectedly. Share a crash report?"));
        rig.click(button);
        assert!(rig.log().contains(&expected), "{button} sends {expected:?}");
        assert!(!rig.contains("closed unexpectedly"), "asked once");
    }
}

#[test]
fn bluetooth_off_offers_windows_bluetooth_settings_on_every_page() {
    for page in Page::ALL {
        let mut rig = Rig::full();
        rig.ui.bluetooth = Some(Capabilities { radio: Radio::Off, ..caps(true, true) });
        rig.ui.page = page;
        rig.click("Open Bluetooth settings");
        assert!(rig.log().contains(&Command::OpenBluetoothSettings), "on {page:?}");
    }
    let mut rig = Rig::full();
    assert!(!rig.has("Open Bluetooth settings"), "nothing to fix while Bluetooth is on");
}

#[test]
fn the_call_end_reason_from_the_core_names_how_the_call_ended() {
    for (reason, text) in [
        (CallEndReason::NoAnswer, "No answer"),
        (CallEndReason::Missed, "Missed call"),
        (CallEndReason::Declined, "Declined"),
        (CallEndReason::Busy, "Busy"),
        (CallEndReason::Ended, "Call ended"),
        (CallEndReason::Unconfirmed, "Couldn't confirm who this is"),
    ] {
        assert_eq!(reason.text(), text);
        let mut rig = Rig::full();
        rig.populate();
        assert!(rig.ui.place_call(10, false));
        rig.send(Update::CallEnded { reason });
        rig.ui.pump();
        assert_eq!(rig.ui.call.as_ref().map(|c| c.phase()), Some(Phase::Ended), "{reason:?} ends the call");
        assert_eq!(rig.ui.call_status().as_deref(), Some(text));
        assert_eq!(rig.ui.recent[0].result, text, "Recent says the same");
    }
}

// ----------------------------------------------------- capability gate

#[test]
fn an_adapter_that_cannot_advertise_keeps_outgoing_calls_and_shows_the_exact_reason() {
    let mut rig = Rig::outgoing_only();
    rig.populate();
    rig.ui.pump();
    // Home: Watch and Go live are unavailable with the repo's reason text, once.
    assert!(rig.contains(&format!("Unavailable. {NO_PERIPHERAL}")));
    assert!(rig.disabled("Go live"));
    assert!(!rig.ui.start_watch(1));
    assert!(!rig.has("Live from Priya"), "streams are not offered when watching is unavailable");
    // Calls: the call buttons work, receiving is explained.
    rig.ui.page = Page::Calls;
    assert!(!rig.disabled("Voice call Priya"));
    assert!(rig.contains(&format!("Unavailable. {NO_PERIPHERAL}")));
    rig.click("Voice call Priya");
    assert!(rig.ui.call.is_some(), "an outgoing call starts");
    // An incoming call is never shown (this PC cannot advertise).
    let mut rig = Rig::outgoing_only();
    rig.send(Update::IncomingCall { peer: "x".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(rig.ui.call.is_none());
    // Settings: the switch is disabled and carries the reason.
    rig.ui.page = Page::Settings;
    assert!(rig.disabled("Allow calls"));
    assert!(rig.contains(NO_PERIPHERAL));
    assert!(rig.ui.unavailable_reason(Feature::GoLive).is_some());
    assert!(rig.ui.unavailable_reason(Feature::CallOut).is_none());
}

#[test]
fn before_the_adapter_is_known_everything_waits_instead_of_guessing() {
    let mut rig = Rig::bare();
    assert_eq!(rig.ui.unavailable_reason(Feature::CallOut), Some("Checking Bluetooth\u{2026}"));
    assert!(rig.disabled("Go live"));
    assert!(!rig.ui.place_call(10, true));
}

// ------------------------------------------------------------- Home/Watch

#[test]
fn home_lists_streams_and_radio_and_watching_goes_through_its_states() {
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.has("Live from Priya, Nearby"));
    assert!(rig.contains("Radio from Priya"));
    rig.click("Live from Priya, Nearby");
    assert_eq!(rig.ui.watch.as_ref().unwrap().state, WatchState::Connecting);
    assert!(rig.log().contains(&Command::Watch(1)));
    assert!(rig.has("Connecting to live video"));
    rig.send(Update::Watch(WatchEvent::Connected));
    assert!(rig.contains("Live stream"));
    assert_eq!(rig.ui.watch.as_ref().unwrap().state, WatchState::Playing);
    rig.send(Update::Watch(WatchEvent::Lost));
    assert!(rig.contains("Signal lost \u{2014} reconnecting"));
    rig.send(Update::Watch(WatchEvent::Connected));
    rig.click("Mute");
    assert!(rig.log().contains(&Command::MuteWatch(true)));
    assert!(rig.has("Unmute"));
    rig.send(Update::Watch(WatchEvent::Ended));
    assert!(rig.contains("Ended"));
    rig.click("Back");
    assert!(rig.ui.watch.is_none());
    assert!(rig.log().contains(&Command::StopWatching));
    assert_eq!(rig.ui.page, Page::Home);
}

#[test]
fn watch_media_notices_resume_clear_and_cover_the_last_picture() {
    use meshlive::stream_watch::PlaybackState;
    let mut rig = Rig::full();
    let mut offer = stream(1, false); offer.paused = true;
    rig.send(Update::Streams(vec![offer])); rig.ui.pump();
    assert!(rig.ui.start_watch(1));
    rig.send(Update::Watch(WatchEvent::Connected));
    assert!(rig.has("Live"), "an authenticated join clears the untrusted paused hint");
    let texture = egui::TextureId::User(42);
    rig.ui.video = Some(texture);
    let visible = |rig: &mut Rig| {
        let output = rig.frame(vec![]);
        rig.ctx.tessellate(output.shapes, output.pixels_per_point).iter().any(|p| {
            matches!(&p.primitive, egui::epaint::Primitive::Mesh(m) if m.texture_id == texture)
        })
    };
    let state = |value| Update::Watch(WatchEvent::SourceState(PlaybackState::from_record(value).unwrap()));
    assert!(visible(&mut rig));
    rig.send(state(2)); assert!(rig.has("Paused by broadcaster"));
    rig.send(state(4)); assert!(rig.has("Microphone muted"));
    assert!(!rig.ui.watch.as_ref().unwrap().paused);
    assert!(visible(&mut rig));
    rig.click("Mute"); assert!(rig.has("Unmute"), "listener mute is independent");
    rig.send(state(5)); assert!(rig.has("Camera off")); assert!(!visible(&mut rig));
    rig.send(state(6)); assert!(rig.has("Camera off. Microphone muted")); assert!(!visible(&mut rig));
    rig.send(state(1)); assert!(rig.has("Live")); assert!(visible(&mut rig)); assert!(rig.has("Unmute"));
    rig.send(state(6)); rig.send(Update::Watch(WatchEvent::Lost));
    assert!(rig.contains("reconnecting")); assert!(!visible(&mut rig));
    rig.send(Update::Watch(WatchEvent::Connected));
    assert!(rig.has("Live")); assert_eq!(rig.ui.watch.as_ref().unwrap().source_state, PlaybackState::default());
    rig.send(state(6)); rig.send(Update::Watch(WatchEvent::Ended));
    assert!(rig.has("Ended")); assert!(!visible(&mut rig));
    rig.send(state(4)); assert!(rig.has("Ended"));
}

#[test]
fn listening_to_radio_shows_audio_wording_and_stop_leaves() {
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_watch(2));
    assert!(rig.has("Connecting\u{2026}"));
    rig.send(Update::Watch(WatchEvent::Connected));
    rig.click("Mute");
    assert!(rig.has("Live. Muted"));
    rig.send(Update::Watch(WatchEvent::Lost));
    assert!(rig.has("Reconnecting\u{2026}"));
    rig.click("Stop");
    assert!(rig.ui.watch.is_none());
    // A full stream cannot be joined.
    let mut full = stream(3, false);
    full.full = true;
    rig.send(Update::Streams(vec![full]));
    rig.ui.pump();
    assert!(!rig.ui.start_watch(3));
}

#[test]
fn escape_leaves_the_watch_screen_and_the_empty_home_says_what_is_happening() {
    let mut rig = Rig::full();
    assert!(rig.contains("No one is live nearby"));
    rig.send(Update::Streams(vec![stream(1, false)]));
    assert!(rig.ui.start_watch(1) || { rig.ui.pump(); rig.ui.start_watch(1) });
    let _ = rig.frame(vec![egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    }]);
    assert!(rig.ui.watch.is_none());
}

// ------------------------------------------------------------------ Calls

#[test]
fn an_authenticated_glare_winner_replaces_the_search_without_sending_cancel_to_it() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    assert!(rig.ui.place_call(10, true));
    let incoming_id = [9; 16];
    rig.send(Update::CallSearchSuperseded);
    rig.send(Update::IncomingCall {
        peer: "Nearby phone".into(), verified: true,
        invite: remote(incoming_id, Signal::Invite), fingerprint: Some([7; 32]),
    });
    rig.ui.pump();
    assert!(!rig.ui.call.as_ref().unwrap().outgoing);
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::IncomingRinging);
    assert!(!rig.signals().contains(&Signal::Cancel));
    assert!(rig.ui.recent.is_empty(), "the search did not become a missed call");
}

#[test]
fn a_late_remote_cancel_is_missed_on_the_call_screen_and_in_recent_without_a_reply() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall {
        peer: "Nearby phone".into(), verified: false,
        invite: remote(ID, Signal::Invite), fingerprint: None,
    });
    rig.ui.pump();
    let sent = rig.signals();
    rig.send(Update::CallSignal(remote(ID, Signal::Cancel)));
    rig.send(Update::CallEnded { reason: CallEndReason::Missed });
    assert!(rig.has("Missed call"));
    assert_eq!(rig.signals(), sent, "no Decline or other reply after the peer cancelled");
    assert_eq!(rig.ui.recent.len(), 1);
    assert_eq!(rig.ui.recent[0].result, "Missed call");
    assert!(!rig.ui.recent[0].outgoing);
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    assert!(rig.contains("Missed call"));
}

#[test]
fn an_outgoing_voice_call_runs_ringing_active_mute_talk_and_end_from_the_screen() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.page = Page::Calls;
    rig.click("Voice call Priya");
    assert_eq!(rig.signals(), [Signal::Invite, Signal::CameraOff]);
    assert!(rig.has("Calling\u{2026}"));
    assert!(rig.has("Cancel"), "while ringing out, End is Cancel");
    rig.send(Update::CallSignal(remote(ID, Signal::Ringing)));
    assert!(rig.has("Ringing\u{2026}"));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    assert!(rig.has("Connecting\u{2026}"));
    rig.send(Update::CallMediaReady);
    rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
    assert!(rig.contains("Voice call"));
    rig.click("Mute");
    assert!(rig.has("Unmute") && rig.has("Talk"));
    assert!(rig.signals().contains(&Signal::MicrophoneMuted));
    assert!(!rig.ui.call.as_ref().unwrap().sending_audio());
    rig.click("Unmute");
    assert!(rig.ui.call.as_ref().unwrap().sending_audio());
    rig.click("Add video");
    assert!(rig.signals().contains(&Signal::CameraOn));
    rig.click("End call");
    assert_eq!(rig.signals().last(), Some(&Signal::HangUp));
    assert!(rig.has("Call ended"));
    rig.click("Back");
    assert!(rig.ui.call.is_none());
    assert_eq!(rig.ui.recent[0].peer, "Priya");
    assert_eq!(rig.ui.recent[0].result, "Call ended");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    assert!(rig.contains("Voice call \u{b7} Outgoing"));
}

#[test]
fn voice_call_details_follow_each_camera_instead_of_the_shared_media_header() {
    for enabled in [false, true] {
        let mut rig = Rig::full();
        rig.ui.accurate_call_media = enabled;
        rig.populate(); rig.ui.pump();
        assert!(rig.ui.place_call(10, true));
        rig.send(Update::CallSignal(remote(ID, Signal::Ringing)));
        rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
        rig.send(Update::CallMediaReady);
        let receiving = "Receiving VP9 video, 640x360 at 15 fps";
        let sending = "Sending VP9 video, 426x240 at 15 fps";
        rig.send(Update::CallMediaInfo { direction: "receiving", text: receiving.into() });
        assert!(rig.contains("Voice call"));
        assert_eq!(rig.contains(receiving), !enabled, "a voice call's header is not receiving video");
        rig.click("Add video");
        rig.send(Update::CallMediaInfo { direction: "sending", text: sending.into() });
        assert!(rig.contains(sending));
        assert_eq!(rig.contains(receiving), !enabled, "local video does not imply peer video");
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOn)));
        assert!(rig.contains(receiving));
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
        assert_eq!(rig.contains(receiving), !enabled);
        rig.click("Video off");
        assert_eq!(rig.contains(sending), !enabled);
        rig.click("End call");
        assert!(rig.has("Call ended"));
        if enabled { assert!(rig.ui.call_media_lines().is_empty()); }
    }
}

#[test]
fn camera_off_while_ringing_keeps_voice_calls_out_of_the_video_layout() {
    for enabled in [false, true] {
        for incoming in [false, true] {
            let mut rig = Rig::full(); rig.ui.accurate_call_media = enabled;
            rig.populate(); rig.ui.pump();
            if incoming {
                rig.send(Update::IncomingCall { peer: "peer".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
            } else {
                assert!(rig.ui.place_call(10, true));
                rig.send(Update::CallSignal(remote(ID, Signal::Ringing)));
            }
            rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
            if incoming { rig.click("Accept"); } else { rig.send(Update::CallSignal(remote(ID, Signal::Accept))); }
            rig.send(Update::CallMediaReady);
            let receiving = "Receiving VP9 video, 640x360 at 15 fps";
            rig.send(Update::CallMediaInfo { direction: "receiving", text: receiving.into() });
            assert_eq!(rig.contains("Voice call"), enabled || !incoming,
                "the old incoming arm also mislabels a voice call as video");
            assert_eq!(rig.ui.call.as_ref().unwrap().peer_camera, incoming && !enabled,
                "only incoming ringing omits the old arm's PeerCamera action");
            assert_eq!(rig.contains(receiving), !enabled);
            let before = rig.ui.call.as_ref().unwrap().peer_camera;
            rig.send(Update::CallSignal(Message::simple([0x55; 16], Signal::CameraOn).unwrap()));
            assert_eq!(rig.ui.call.as_ref().unwrap().peer_camera, before, "an unrelated call cannot change the camera state");
        }
    }
}

#[test]
fn a_call_to_a_phone_paired_with_this_pc_says_the_link_is_identity_bound() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.page = Page::Calls;
    rig.click("Voice call Priya");
    assert!(!rig.contains(super::screens::BONDED_NOTE));
    rig.send(Update::CallBonded);
    assert!(rig.contains(super::screens::BONDED_NOTE));
    assert!(rig.ui.call.as_ref().unwrap().bonded);
}

#[test]
fn cancelling_while_it_rings_sends_cancel_and_a_busy_peer_ends_the_call() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    assert!(rig.ui.place_call(10, false));
    rig.click("Cancel");
    assert_eq!(rig.signals().last(), Some(&Signal::Cancel));
    rig.click("Back");
    assert!(rig.ui.place_call(10, false));
    rig.send(Update::CallSignal(Message::new(ID, Signal::Decline, Reason::Busy, 0).unwrap()));
    assert!(rig.has("Busy"));
    rig.send(Update::CallLinkLost);
    assert_eq!(rig.ui.recent[0].result, "Busy");
}

#[test]
fn a_voice_call_is_refused_when_voice_calls_are_switched_off() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.switches.voice_calls = false;
    rig.ui.page = Page::Calls;
    assert!(rig.disabled("Voice call Priya"));
    assert!(!rig.ui.place_call(10, true));
    assert!(rig.ui.place_call(10, false));
}

#[test]
fn an_incoming_call_rings_and_accept_or_decline_work_from_the_ring_screen() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
    rig.ui.pump();
    assert_eq!(rig.signals(), [Signal::Ringing]);
    assert!(rig.has("Incoming voice call"));
    assert!(rig.contains("Not verified"));
    assert!(rig.has("Accept") && rig.has("Decline"));
    // CF-CALL-04: a second caller meanwhile never appears and is sent nothing (no BUSY).
    let other = [0x99; 16];
    rig.send(Update::IncomingCall { peer: "Late".into(), verified: false, invite: remote(other, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(!rig.log().into_iter().any(|c| matches!(c, Command::CallSignal(m) if m.call_id == other)));
    assert_eq!(rig.ui.call.as_ref().unwrap().peer, "Blue Otter");
    rig.click("Accept");
    let sent = rig.signals();
    assert_eq!(sent[sent.len() - 2..], [Signal::Accept, Signal::CameraOff], "a voice offer is answered by voice");
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::NegotiatingMedia);
}

#[test]
fn the_calls_page_says_when_this_pc_is_visible_to_phones_and_why_not() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.send(Update::Visibility { visible: true, note: None });
    rig.ui.pump();
    assert!(rig.has("Visible to people nearby"));
    let why = meshlive::qw_policy::NOT_LISTED;
    rig.send(Update::Visibility { visible: false, note: Some(why.into()) });
    rig.ui.pump();
    assert!(!rig.has("Visible to people nearby"));
    assert!(rig.has("Not visible to people nearby") && rig.contains("hasn't been checked"));
    // A refused real start latches the Receive calls gate with its reason.
    rig.send(Update::AdvertisingProof(Err(meshlive::bluetooth::gate::Refusal::Aborted)));
    rig.ui.pump();
    assert_eq!(rig.ui.unavailable_reason(Feature::ReceiveCalls), Some(NO_PERIPHERAL));
}

#[test]
fn the_call_screen_says_whether_the_microphone_is_on_or_why_it_cannot_be_used() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    rig.click("Accept");
    rig.send(Update::CallMediaReady);
    rig.send(Update::CallMic(None));
    assert!(rig.contains("Microphone on"));
    let why = "Microphone access is off for desktop apps in Windows privacy settings, so the other person can't hear you.";
    rig.send(Update::CallMic(Some(why.into())));
    assert!(rig.contains("Microphone access is off"));
    assert!(rig.has("Open microphone settings"));
    assert!(!rig.contains("Microphone on"));
}

#[test]
fn holding_talk_past_the_click_limit_survives_readiness_but_leaving_or_releasing_mutes() {
    for hold in [false, true] {
        let mut rig = Rig::full(); rig.ui.hold_call_talk = hold; rig.time = Some(0.0);
        rig.send(Update::IncomingCall { peer: "Test peer".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
        rig.ui.pump(); rig.click("Accept");
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
        rig.send(Update::CallMediaReady); rig.send(Update::CallMic(None));
        rig.click("Mute");
        let pos = rig.bounds("Talk", None).center();
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        assert!(rig.ui.call.as_ref().unwrap().sending_audio());
        rig.send(Update::CallCaptureStarting);
        rig.time = Some(1.0); let _ = rig.frame(vec![]);
        assert_eq!(rig.ui.call.as_ref().unwrap().talking, hold, "the click-only control expires while the pointer remains down");
        rig.time = Some(2.0); rig.send(Update::CallMic(None)); rig.send(Update::CallCaptureReady);
        let _ = rig.frame(vec![]);
        assert_eq!(rig.ui.call.as_ref().unwrap().sending_audio(), hold);
        rig.time = Some(3.0); let _ = rig.frame(vec![]);
        assert_eq!(rig.ui.call.as_ref().unwrap().talking, hold);
        let _ = rig.frame(vec![egui::Event::PointerMoved(egui::Pos2::ZERO)]);
        assert!(!rig.ui.call.as_ref().unwrap().sending_audio(), "leaving Talk cancels the hold");
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos)]);
        assert_eq!(rig.ui.call.as_ref().unwrap().talking, hold);
        let _ = rig.frame(vec![button(false)]);
        assert!(!rig.ui.call.as_ref().unwrap().talking && !rig.ui.call.as_ref().unwrap().sending_audio());
        assert_eq!(rig.signals().last(), Some(&Signal::MicrophoneMuted));
        rig.time = Some(4.0); let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        rig.focused = false;
        let _ = rig.frame(vec![egui::Event::WindowFocused(false)]);
        assert_eq!(rig.ui.call.as_ref().unwrap().sending_audio(), !hold, "the new arm cancels Talk on focus loss; the old arm is retained");
    }
}

#[test]
fn talk_waits_for_fresh_microphone_readiness_and_overrides_latched_mute_wording() {
    for accurate in [false, true] {
        let mut rig = Rig::full(); rig.ui.accurate_call_media = accurate;
        rig.populate(); rig.ui.pump();
        assert!(rig.ui.place_call(10, true));
        rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
        rig.send(Update::CallMediaReady);
        rig.send(Update::CallMic(None));
        rig.click("Mute");
        let pos = rig.bounds("Talk", None).center();
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        rig.send(Update::CallCaptureStarting);
        assert!(rig.has("Starting microphone\u{2026}"));
        assert!(!rig.contains("Microphone on"));
        rig.send(Update::CallMic(None));
        assert!(rig.has("Starting microphone\u{2026}"), "an open notification alone is not the current restart acknowledgement");
        rig.send(Update::CallCaptureReady);
        assert!(rig.has("Talking\u{2026}"));
        assert_eq!(rig.contains("Microphone on \u{b7} muted"), !accurate);
        let _ = rig.frame(vec![button(false)]);
        assert!(rig.has("Muted"));
        rig.click("Unmute");
        rig.send(Update::CallCaptureStarting);
        assert!(rig.has("Starting microphone\u{2026}"));
        rig.send(Update::CallMic(Some("Microphone access is off.".into())));
        rig.send(Update::CallCaptureReady);
        assert!(rig.contains("Microphone access is off"));
        assert!(!rig.has("Starting microphone\u{2026}"));
        rig.click("End call");
        rig.send(Update::CallCaptureStarting);
        assert!(rig.has("Call ended"), "late microphone progress cannot replace the terminal status");
        rig.click("Back");
        assert!(rig.ui.place_call(10, true));
        assert!(!rig.ui.call_mic_restarting, "the next call starts with fresh capture state");
    }
}

#[test]
fn the_call_screen_says_why_the_camera_cannot_be_used() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    rig.click("Accept");
    rig.send(Update::CallMediaReady);
    let why = meshlive::call_camera::reason_text(Some(0x80070005u32 as i32), &[]);
    rig.send(Update::CallCamera(Some(why)));
    assert!(rig.contains("Camera access is off for desktop apps"));
    assert!(rig.has("Open camera settings"));
}

#[test]
fn a_contacts_safety_code_is_shown_on_request() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Contacts;
    rig.send(Update::Contacts(vec![Contact { id: 20, name: "Asha".into(), by_qr: true, nearby: false, lease: true, blocked: false }]));
    rig.ui.pump();
    rig.click("Safety code with Asha");
    assert!(rig.log().contains(&Command::ShowSafetyCode(20)));
    let code = "71883 11788 79268 78261 54534 68020 91662 04557 05960 21671 25144 22586";
    rig.send(Update::SafetyCode { contact: 20, code: code.into() });
    assert!(rig.contains(code));
    assert!(rig.contains("Both screens must show exactly the same numbers"));
}

#[test]
fn the_wipe_needs_wipe_typed_and_then_reports_and_closes() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    assert!(rig.has("Delete everything"), "the button is enabled at the end of Settings");
    // (Below the fold in the test viewport: open the dialog as the click does.)
    rig.ui.wipe_confirm = Some(String::new());
    assert!(rig.contains("Delete everything?"));
    assert!(rig.disabled("Delete"), "nothing happens until DELETE is typed");
    rig.ui.wipe_confirm = Some("DELETE".into());
    rig.click("Delete");
    assert!(rig.log().contains(&Command::Wipe));
    let report = meshlive::wipe::Report { key_deleted: true, complete: true, ..Default::default() };
    rig.send(Update::Wiped(report));
    assert!(rig.contains("Everything was deleted"));
    assert!(rig.has("Close MeshLive"));
}

#[test]
fn declining_an_incoming_video_call_sends_decline_and_records_it() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(rig.has("Incoming video call"));
    rig.click("Decline");
    assert!(rig.signals().contains(&Signal::Decline));
    assert!(rig.has("Declined"));
    rig.click("Back");
    assert_eq!(rig.ui.recent[0].result, "Declined");
    assert!(!rig.ui.recent[0].outgoing);
}

#[test]
fn keyboard_shortcuts_mute_and_end_a_call_and_holding_talk_opens_the_microphone() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    assert!(rig.ui.place_call(10, false));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    rig.send(Update::CallMediaReady);
    let _ = rig.frame(vec![ctrl(egui::Key::M)]);
    assert!(rig.ui.call.as_ref().unwrap().muted);
    // Holding Talk (the pointer down on it) sends unmuted; release sends muted.
    let talk = rig.nodes().into_iter().find(|n| n.text == "Talk").unwrap().center.unwrap();
    let button = |pressed| egui::Event::PointerButton { pos: talk, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
    let _ = rig.frame(vec![egui::Event::PointerMoved(talk)]);
    let _ = rig.frame(vec![button(true)]);
    let _ = rig.frame(vec![]);
    assert!(rig.ui.call.as_ref().unwrap().talking);
    assert!(rig.ui.call.as_ref().unwrap().sending_audio());
    let _ = rig.frame(vec![button(false)]);
    let _ = rig.frame(vec![]);
    assert!(!rig.ui.call.as_ref().unwrap().talking);
    let _ = rig.frame(vec![ctrl(egui::Key::E)]);
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::Ended);
}

#[test]
fn the_scripted_core_answers_an_outgoing_call_end_to_end() {
    let mut ui = Ui::with_backend(Box::new(ScriptedBackend::demo(std::time::Duration::ZERO)));
    ui.bluetooth = Some(caps(true, true));
    ui.peripheral_proof = PeripheralProof::Started;
    ui.pump();
    assert_eq!(ui.people.len(), 2);
    assert!(ui.place_call(10, true));
    ui.pump();
    ui.pump();
    assert_eq!(ui.call.as_ref().unwrap().phase(), Phase::Active);
    ui.with_call(|c| {
        c.end().unwrap();
    });
    assert_eq!(ui.recent.len(), 1);
    assert!(ui.start_watch(1));
    ui.pump();
    assert_eq!(ui.watch.as_ref().unwrap().state, WatchState::Playing);
    ui.leave_watch();
    ui.golive.title = "x".into();
    assert!(ui.start_live());
    ui.pump();
    assert_eq!(ui.golive.state, LiveState::Live);
    assert_eq!(ui.golive.viewers, 1);
}

// ------------------------------------------------------------------ Go live

#[test]
fn going_live_with_video_shows_state_and_viewer_count_and_stops() {
    let mut rig = Rig::full();
    rig.click("Go live");
    assert!(rig.ui.golive_open);
    assert!(rig.has("Video and audio") && rig.has("Audio only"));
    rig.ui.golive.title = "Market".into();
    rig.click("Go live");
    assert_eq!(rig.log(), [Command::GoLive { kind: LiveKind::Video, title: "Market".into(), initial: Default::default() }]);
    assert!(rig.has("Starting\u{2026}"));
    rig.send(Update::Live(LiveEvent::Started));
    rig.send(Update::Live(LiveEvent::Viewers(3)));
    assert!(rig.has("Live, 3 watching"));
    assert_eq!(rig.ui.golive.wire_state(), Some(meshlive::live::LIVE));
    rig.click("Pause");
    assert!(rig.has("Paused, 3 watching"));
    assert!(rig.log().contains(&Command::PauseLive(true)));
    rig.click("Resume");
    rig.click("Camera off");
    assert!(rig.log().contains(&Command::LiveCamera(false)));
    rig.click("Stop");
    assert!(rig.log().contains(&Command::StopLive));
    assert_eq!(rig.ui.golive.state, LiveState::Ended);
    rig.click("Back");
    assert!(!rig.ui.golive_open);
}

#[test]
fn restarting_or_retrying_a_broadcast_keeps_mute_and_camera_off_in_the_start_request() {
    use meshlive::stream_source::InitialMedia;
    let mut rig = Rig::full();
    rig.ui.open_go_live(); rig.click("Go live");
    rig.send(Update::Live(LiveEvent::Started));
    rig.click("Mute"); rig.click("Camera off");
    assert!(rig.has("Unmute") && rig.has("Camera on"));
    rig.click("Stop");
    rig.click("Go live");
    let expected = InitialMedia { muted: true, camera_off: true };
    assert!(matches!(rig.log().last(), Some(Command::GoLive { initial, .. }) if *initial == expected),
        "capture choices must travel in the start request, before any worker opens devices");
    rig.send(Update::Live(LiveEvent::Failed)); rig.ui.pump();
    rig.click("Go live");
    assert!(matches!(rig.log().last(), Some(Command::GoLive { initial, .. }) if *initial == expected));
    rig.send(Update::Live(LiveEvent::Started));
    assert!(rig.has("Unmute") && rig.has("Camera on"));
}

#[test]
fn audio_only_asks_before_going_live_and_a_failed_start_is_reported() {
    let mut rig = Rig::full();
    rig.ui.golive_open = true;
    rig.click("Audio only");
    rig.click("Go live");
    assert!(rig.ui.audio_disclosure, "the disclosure comes first");
    assert!(rig.log().is_empty());
    assert!(rig.contains("Anyone nearby with MeshLive can listen"));
    rig.click("Cancel");
    assert!(rig.log().is_empty());
    rig.click("Go live");
    rig.click("Go live with audio");
    assert!(matches!(rig.log()[0], Command::GoLive { kind: LiveKind::AudioOnly, .. }));
    rig.send(Update::Live(LiveEvent::Failed));
    assert!(rig.contains("Couldn't start. Nothing is being shared."));
    // After disclosure a second start does not ask again.
    rig.click("Go live");
    assert_eq!(rig.log().len(), 2);
    rig.send(Update::Live(LiveEvent::Started));
    assert!(rig.has("Live. Waiting for listeners"));
    rig.send(Update::Live(LiveEvent::Viewers(2)));
    assert!(rig.has("Live. 2 listening"));
    rig.click("Mute");
    assert!(rig.log().contains(&Command::MuteLive(true)));
}

#[test]
fn going_live_is_refused_without_the_peripheral_role_and_a_running_broadcast_survives_back() {
    let mut rig = Rig::outgoing_only();
    rig.ui.golive_open = true;
    assert!(rig.contains(&format!("Unavailable. {NO_PERIPHERAL}")));
    assert!(rig.disabled("Go live"));
    assert!(!rig.ui.start_live());
    let mut rig = Rig::full();
    assert!(rig.ui.start_live());
    rig.send(Update::Live(LiveEvent::Started));
    rig.ui.pump();
    rig.ui.golive_open = true;
    rig.click("Back");
    assert!(rig.ui.golive.running(), "Back only closes the screen");
    assert!(rig.has("Go live: back to your broadcast"));
}

// ----------------------------------------------------------------- Contacts

#[test]
fn contacts_show_verification_and_contacts_with_a_lease_can_be_called_even_when_not_listed() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    assert!(rig.contains("Verified by QR code"));
    assert!(rig.contains("Verified by safety code"));
    assert!(rig.contains("Ravi isn't nearby right now"));
    assert!(!rig.disabled("Voice call Ravi"));
    rig.click("Video call Priya");
    assert_eq!(rig.ui.call.as_ref().unwrap().peer, "Priya");
    assert!(rig.signals().contains(&Signal::Invite));
    let mut empty = Rig::full();
    empty.ui.page = Page::Contacts;
    assert!(empty.has("No contacts yet"));
}

#[test]
fn contacts_can_be_renamed_and_forgotten_with_confirmation_and_store_problems_show() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    rig.click("Rename Ravi");
    assert!(rig.has("Rename contact") && rig.has("Only you see this name."));
    rig.click("Save");
    assert_eq!(
        rig.log().last(),
        Some(&Command::RenameContact { contact: 12, name: "Ravi".into() })
    );
    rig.send(Update::Renamed { contact: 12, outcome: RenameOutcome::NameTaken });
    assert!(rig.contains("Another contact already has that name"));
    rig.send(Update::Renamed { contact: 12, outcome: RenameOutcome::Renamed });
    assert!(!rig.has("Rename contact"));
    rig.click("Forget Priya");
    assert!(rig.has("Forget Priya?"));
    assert!(rig.contains("To trust them again you need to verify them in person."));
    rig.click("Forget");
    assert_eq!(rig.log().last(), Some(&Command::ForgetContact(10)));
    rig.send(Update::ContactsProblem(Some(StoreProblem::Corrupt)));
    assert!(rig.contains("The saved list is damaged or its key was lost"));
    rig.click("Reset verified people");
    rig.click("Forget everyone");
    assert_eq!(rig.log().last(), Some(&Command::ResetContacts));
}

// ----------------------------------------------------------------- Settings

#[test]
fn settings_switches_follow_android_defaults_and_report_changes_to_the_core() {
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3600.0);
    rig.ui.page = Page::Settings;
    for name in [
        "Friendly names for nearby phones",
        "Show my stream title to everyone nearby",
        "Voice calls",
        "Play ringing tone",
        "Allow calls",
        "Start with Windows",
    ] {
        assert!(rig.has(name), "missing switch {name}");
    }
    rig.click("Play ringing tone");
    assert!(!rig.ui.switches.ringback);
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert!(!s.ringback && s.generated_names),
        other => panic!("{other:?}"),
    }
    rig.click("Friendly names for nearby phones");
    assert!(!rig.ui.switches.generated_names);
}

#[test]
fn the_sample_data_switch_swaps_in_the_scripted_core_and_back() {
    let mut rig = Rig::full();
    rig.ui.set_demo(true);
    rig.ui.pump();
    std::thread::sleep(std::time::Duration::from_millis(700));
    rig.ui.pump();
    assert_eq!(rig.ui.streams.len(), 2);
    rig.ui.set_demo(false);
    assert!(rig.ui.streams.is_empty());
}

#[test]
fn a_failed_setup_to_a_paired_phone_keeps_the_bonded_note_with_its_reason() {
    // CF-LINK-32: the note arrives while connecting and stays when the dial fails.
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.page = Page::Calls;
    rig.click("Voice call Priya");
    rig.send(Update::CallBonded);
    rig.send(Update::CallSetupFailed("Couldn't connect the call. Try again.".into()));
    assert!(rig.contains(super::screens::BONDED_NOTE));
    assert!(rig.contains("Couldn't connect the call. Try again."));
}

#[test]
fn bluetooth_off_says_so_on_the_calls_page_and_clears_when_it_is_back() {
    // CF-LIFE-15: a plain reason instead of a silent empty list.
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Calls;
    rig.send(Update::BluetoothNote(Some("Turn Bluetooth on in Windows settings to find people and call.".into())));
    assert!(rig.contains("Bluetooth is off"));
    assert!(rig.contains("Turn Bluetooth on in Windows settings"));
    rig.send(Update::BluetoothNote(None));
    assert!(!rig.contains("Bluetooth is off"));
}

#[test]
fn a_contact_without_a_lease_says_ask_them_to_scan_and_cannot_be_called() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.send(Update::Contacts(vec![
        Contact { id: 20, name: "Asha".into(), by_qr: true, nearby: false, lease: false, blocked: false },
        Contact { id: 21, name: "Ravi".into(), by_qr: true, nearby: false, lease: true, blocked: false },
    ]));
    assert!(rig.contains("Ask Asha to scan your code too"));
    assert!(rig.disabled("Video call Asha"));
    // With a lease a call looks for them even when they are not listed nearby.
    assert!(rig.contains("Ravi isn't nearby right now"));
    assert!(!rig.disabled("Video call Ravi"));
    rig.click("Video call Ravi");
    assert!(rig.log().contains(&Command::Call { person: 21, voice: false }));
    rig.send(Update::CallStatus(Some("Looking for Ravi\u{2026}".into())));
    assert!(rig.contains("Looking for Ravi"));
    rig.send(Update::CallSetupFailed("Ravi wasn't found nearby".into()));
    assert!(rig.contains("Ravi wasn't found nearby"));
    rig.click("Back");
    // Recent offers a call-back to the contact.
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.click("Call back (voice) Ravi");
    assert!(rig.log().contains(&Command::Call { person: 21, voice: true }));
}

#[test]
fn your_code_is_a_qr_picture_with_the_visibility_and_what_happened_to_it() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    assert!(rig.contains("Getting your code ready"));
    let code = format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789");
    rig.send(Update::Visibility { visible: true, note: None });
    rig.send(Update::OwnCode(Some(code.clone())));
    assert!(rig.has("Your code as a QR code"), "the code is drawn as a QR picture");
    assert!(rig.contains("Visible to people nearby. On the other phone, open Add contact and scan this code."));
    assert!(rig.contains(&code));
    rig.send(Update::CodeScanned("Asha scanned your code. Now scan theirs (above), so you can find each other.".into()));
    assert!(rig.contains("Asha scanned your code"));
    // Not on air: the reason replaces the visibility line.
    rig.send(Update::Visibility { visible: false, note: Some(meshlive::qw_policy::NOT_LISTED.into()) });
    assert!(rig.contains("hasn't been checked"));
}

#[test]
fn add_contact_scans_a_pasted_code_and_asks_whose_screen_it_was() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    assert!(rig.log().contains(&Command::ShowOwnCode));
    rig.send(Update::OwnCode(Some("BVMID2.ABC".into())));
    assert!(rig.contains("BVMID2.ABC"));
    rig.ui.meet.as_mut().unwrap().code = "BVMID2.THEIRS".into();
    rig.click("Scan code");
    assert!(rig.log().contains(&Command::ScanCode("BVMID2.THEIRS".into())));
    assert!(rig.contains("Looking for the phone that shows this code"));
    rig.send(Update::Meet(MeetEvent::Ask { name: "Green Heron".into() }));
    assert!(rig.contains("Is this Green Heron's screen you scanned?"));
    rig.click("Yes, it is");
    assert!(rig.log().contains(&Command::ConfirmScan { yes: true, name: "Green Heron".into() }));
    rig.send(Update::Meet(MeetEvent::Done("Green Heron is now a verified contact.".into())));
    assert!(rig.contains("now a verified contact"));
    rig.click("Back");
    assert!(rig.log().contains(&Command::CancelScan));
    assert!(rig.ui.meet.is_none());
}

#[test]
fn a_caller_with_a_real_key_can_be_blocked_from_recent_calls() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.click("Block Priya");
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: true }));
    assert!(rig.contains("Blocked: their calls are declined without ringing."));
    rig.click("Unblock Priya");
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: false }));
    // A pseudonymous caller has no fingerprint to block.
    let other = [0x77; 16];
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(other, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    assert!(!rig.has("Block Blue Otter"));
}

#[test]
fn the_people_list_on_screen_asks_for_a_continuous_scan_and_leaving_it_stops_it() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.ui.pump();
    let _ = rig.texts();
    assert!(rig.log().contains(&Command::PeopleVisible(true)));
    rig.ui.page = Page::Contacts;
    let _ = rig.texts();
    assert_eq!(rig.log().iter().filter(|c| matches!(c, Command::PeopleVisible(_))).last(), Some(&Command::PeopleVisible(false)));
}

#[test]
fn a_copied_id_is_marked_and_a_call_to_it_needs_call_anyway() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.send(Update::People(vec![Person { id: 11, name: "Blue Otter".into(), reach: Reach::Direct, verified: false, copied: true }]));
    assert!(rig.contains("More than one phone is using this ID"));
    rig.click("Video call Blue Otter");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::Call { .. })), "no call before the second tap");
    rig.click("Call anyway");
    assert!(rig.log().contains(&Command::Call { person: 11, voice: false }));
}

#[test]
fn the_effective_media_of_a_call_is_kept_for_the_call_screen_and_cleared_by_the_next_call() {
    let mut rig = Rig::full();
    rig.ui.apply(Update::CallMediaInfo { direction: "sending", text: "Voice only: this PC's video couldn't start.".into() });
    rig.ui.apply(Update::CallMediaInfo { direction: "receiving", text: "Receiving VP9 video, 640x360 at 15 fps".into() });
    assert_eq!(rig.ui.call_sending.as_deref(), Some("Voice only: this PC's video couldn't start."));
    assert_eq!(rig.ui.call_receiving.as_deref(), Some("Receiving VP9 video, 640x360 at 15 fps"));
    rig.send(Update::IncomingCall { peer: "x".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert_eq!((rig.ui.call_sending.clone(), rig.ui.call_receiving.clone()), (None, None));
}

#[test]
fn an_incoming_call_is_announced_once_outside_the_window() {
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert_eq!(rig.ui.take_incoming_alert().map(|(p, _)| p), Some("Blue Otter".to_owned()));
    assert_eq!(rig.ui.take_incoming_alert(), None, "announced once");
}


#[test]
fn add_contact_scans_with_the_camera_and_offers_the_privacy_page_when_it_is_blocked() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    rig.click("Scan with camera");
    assert!(rig.log().contains(&Command::QrCamera(true)));
    assert!(rig.contains("Point the camera at the code on their phone"));
    // Blocked by Windows privacy settings: the camera is off and the page is offered.
    rig.send(Update::Meet(MeetEvent::Failed(
        "Camera access is off for desktop apps in Windows privacy settings, so this PC can't scan their code. Turn it on, or paste their code instead.".into(),
    )));
    assert!(rig.has("Open camera settings"));
    assert!(!rig.ui.meet.as_ref().unwrap().camera);
    // Leaving Add contact with the camera on closes it.
    rig.click("Scan with camera");
    rig.ui.close_meet();
    assert_eq!(rig.log().iter().filter(|c| **c == Command::QrCamera(false)).count(), 1);
}

#[test]
fn a_damaged_presence_store_offers_reset_presence_key() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.send(Update::Visibility {
        visible: false,
        note: Some("This PC's saved contact keys are damaged, so phones can't call it. Reset fixes this.".into()),
    });
    rig.ui.pump();
    rig.click("Reset this PC's contact keys");
    assert!(rig.log().contains(&Command::ResetPresenceKey));
}

#[test]
fn a_damaged_presence_store_still_visible_on_ephemeral_ids_offers_the_reset() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.send(Update::Visibility {
        visible: true,
        note: Some("This PC's saved contact keys are damaged, so your contacts can't recognise this PC (they can still call it with its code). Reset fixes this; contacts recognise this PC again after a call.".into()),
    });
    rig.ui.pump();
    assert!(rig.contains("Visible to people nearby"));
    assert!(rig.contains("contacts can't recognise this PC"));
    rig.click("Reset this PC's contact keys");
    assert!(rig.log().contains(&Command::ResetPresenceKey));
    // A healthy store shows no reset.
    rig.send(Update::Visibility { visible: true, note: None });
    rig.ui.pump();
    assert!(!rig.has("Reset this PC's contact keys"));
}


/// Screen readers and voice input set a field's whole value (UI Automation
/// ValuePattern.SetValue): the welcome name field takes it.
#[test]
fn a_screen_reader_can_set_the_welcome_name() {
    use egui::accesskit::{Action, ActionData, ActionRequest, Role};
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    let _ = rig.frame(vec![]);
    let output = rig.frame(vec![]);
    let update = output.platform_output.accesskit_update.expect("AccessKit");
    let (id, node) = update.nodes.iter().find(|(_, n)| n.role() == Role::TextInput).expect("the name field");
    assert!(node.supports_action(Action::SetValue), "the field offers SetValue");
    let request = ActionRequest { action: Action::SetValue, target: *id, data: Some(ActionData::Value("Test PC".into())) };
    let _ = rig.frame(vec![egui::Event::AccessKitActionRequest(request)]);
    assert_eq!(rig.ui.welcome_name, "Test PC");
    rig.click("Start");
    assert!(rig.log().contains(&Command::SetOwnName("Test PC".into())));
}

#[test]
fn the_first_run_welcome_sets_the_optional_name_and_is_shown_once() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    assert!(rig.contains("Welcome to MeshLive"));
    assert!(rig.contains("Bluetooth is ready."));
    rig.ui.welcome_name = "  Asha ".into();
    rig.click("Start");
    assert!(rig.log().contains(&Command::SetOwnName("Asha".into())));
    assert!(rig.log().iter().any(|c| matches!(c, Command::Switches(s) if s.onboarded)), "saved, so never shown again");
    assert!(!rig.contains("Welcome to MeshLive"));
}

/// Android parity (activity_main.xml streamModeButton): Home's Go live is the
/// white Primary button with the red live mark, not a red button.
#[test]
fn the_home_go_live_is_primary_with_the_red_live_icon() {
    use super::components::Kind;
    use super::icons::Icon;
    assert_eq!(super::screens::home_go_live(false).style(), (Kind::Primary, Some(Icon::Broadcast), Some(super::theme::LIVE)));
    assert_eq!(super::screens::home_go_live(true).style().0, Kind::Primary);
}

/// Every pair of neighbouring switch rows in Settings has a divider between
/// them, including rows that both carry the (i) button (user report on
/// ae751be: the line was missing there).
#[test]
fn every_neighbouring_pair_of_settings_switches_has_a_divider() {
    use egui::accesskit::Role;
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.size = egui::vec2(1000.0, 4000.0);
    let _ = rig.frame(vec![]);
    let output = rig.frame(vec![]);
    let update = output.platform_output.accesskit_update.clone().expect("AccessKit");
    let mut rows: Vec<(String, f32, f32)> = update
        .nodes
        .iter()
        .filter(|(_, n)| n.role() == Role::CheckBox)
        .filter_map(|(_, n)| n.bounds().map(|b| (n.label().unwrap_or("").to_owned(), b.y0 as f32, b.y1 as f32)))
        .collect();
    rows.sort_by(|a, b| a.1.total_cmp(&b.1));
    let lines: Vec<f32> = output
        .shapes
        .iter()
        .filter_map(|c| match &c.shape {
            egui::Shape::LineSegment { points, stroke } if stroke.color == super::theme::DIVIDER && points[0].y == points[1].y => Some(points[0].y),
            _ => None,
        })
        .collect();
    let mut missing = Vec::new();
    for pair in rows.windows(2) {
        let (above, below) = (&pair[0], &pair[1]);
        // Neighbours only (not across a section gap).
        if below.1 - above.2 > 12.0 {
            continue;
        }
        if !lines.iter().any(|y| *y >= above.2 - 1.5 && *y <= below.1 + 1.5) {
            missing.push(format!("{} | {} ({:.1}..{:.1})", above.0, below.0, above.2, below.1));
        }
    }
    assert!(missing.is_empty(), "no divider between: {missing:#?}\nlines at {lines:?}");
}

/// Windows cannot announce this PC while it sleeps: when calls are allowed,
/// "Ready" says so; with Receive calls off it does not.
#[test]
fn ready_says_reachable_while_this_pc_is_awake_only_when_calls_are_allowed() {
    let mut rig = Rig::full();
    rig.ui.switches.receive_calls = true;
    assert!(rig.contains("Reachable while this PC is awake"));
    rig.ui.switches.receive_calls = false;
    assert!(!rig.contains("Reachable while this PC is awake"));
}


/// Settings "Your ID" shows this PC's real ID (Android's handle), never a
/// placeholder once the backend has read it; tapping it shows your code.
#[test]
fn your_id_shows_the_real_id_and_opens_your_code() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.send(Update::OwnId(Some("7KQ2M-4ZD80".into())));
    rig.ui.pump();
    assert!(rig.has("Your ID, 7KQ2M-4ZD80"));
    assert!(!rig.contains("Not created"));
    rig.click("Your ID, 7KQ2M-4ZD80");
    assert!(rig.ui.meet.is_some(), "your code opens, as on Android");
}

/// On the Windows Bluetooth owner (no live streams yet) the Live tab says
/// so plainly and no visible control sends a command it has no handler for
/// (Watch, Go live, stream switches).
#[test]
fn without_live_streams_no_control_sends_an_unhandled_command() {
    let mut rig = Rig::full();
    rig.shared.borrow_mut().no_streams = true;
    rig.populate();
    rig.ui.page = Page::Home;
    // The Live tab is hidden while Windows has no streams; nothing on it may send a stream command.
    assert!(!rig.has("Go live") && !rig.has("Search again"));
    let names: Vec<String> = rig.nodes().into_iter().filter(|n| n.is_control()).map(|n| n.text).collect();
    assert!(!names.iter().any(|n| n.starts_with("Watch")), "no stream rows to watch: {names:?}");
    rig.ui.page = Page::Settings;
    rig.size = egui::vec2(700.0, 4000.0);
    assert!(!rig.contains("Relay video for others"));
    assert!(!rig.contains("Show my stream title"));
    // Everything a page offers is handled by the real backend.
    let unhandled = [
        "Watch", "StopWatching", "MuteWatch", "GoLive", "StopLive", "PauseLive", "MuteLive", "LiveCamera",
    ];
    for page in [Page::Home, Page::Calls, Page::Contacts, Page::Settings] {
        rig.ui.page = page;
        let controls: Vec<egui::Pos2> = rig.nodes().into_iter().filter(|n| n.is_control() && !n.disabled).filter_map(|n| n.center).collect();
        for pos in controls {
            rig.ui.page = page;
            let _ = rig.frame(vec![egui::Event::PointerMoved(pos)]);
            let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            let _ = rig.frame(vec![button(true)]);
            let _ = rig.frame(vec![button(false)]);
            let _ = rig.frame(vec![]);
            // Back to the page (a click may open Add contact, a dialog or a call).
            rig.ui.meet = None;
            rig.ui.dialog = None;
            rig.ui.wipe_confirm = None;
            rig.ui.name_edit = None;
            rig.ui.copied_confirm = None;
            rig.ui.call = None;
        }
    }
    assert!(rig.log().iter().all(|c| !unhandled.iter().any(|u| format!("{c:?}").starts_with(u))), "{:?}", rig.log());
}

/// One column per page that grows with the window (no split panes): rows
/// stretch, the column is centred once capped, and no page leaves more than
/// about 35 % of a 2560-wide window empty.
#[test]
fn pages_use_a_wide_window_and_grow_with_it() {
    let mut rig = Rig::full();
    rig.populate();
    rig.send(Update::OwnId(Some("7KQ2M-4ZD80".into())));
    rig.ui.pump();
    assert!(!rig.ui.split_panes, "split panes are an experiment, off by default");
    let mut spans = Vec::new();
    for width in [1366.0 / 1.25, 1920.0, 2560.0, 2560.0 / 1.25, 3840.0 / 1.5] {
        rig.size = egui::vec2(width, 1000.0);
        for page in [Page::Home, Page::Calls, Page::Contacts, Page::Settings] {
            rig.ui.page = page;
            let nodes = rig.nodes();
            let (min, max) = nodes
                .iter()
                .filter_map(|n| n.center)
                .fold((f32::MAX, f32::MIN), |(lo, hi), c| (lo.min(c.x), hi.max(c.x)));
            spans.push((width, page, max - min));
            if width >= 2000.0 {
                assert!(max - min >= width * 0.65, "{page:?} at {width}: content spans {:.0} of {width}", max - min);
            }
        }
    }
    // Every page's content grows with the window.
    for page in [Page::Home, Page::Calls, Page::Contacts, Page::Settings] {
        let narrow = spans.iter().find(|(w, p, _)| *p == page && *w < 1100.0).unwrap().2;
        let wide = spans.iter().find(|(w, p, _)| *p == page && *w == 2560.0).unwrap().2;
        assert!(wide > narrow * 1.5, "{page:?}: {narrow:.0} -> {wide:.0}");
    }
    // Single column: the calls sit on each row, Settings shows every group.
    rig.size = egui::vec2(2560.0, 4000.0);
    rig.ui.page = Page::Settings;
    assert!(rig.has("Delete everything") && rig.contains("Allow calls"), "one page, all groups");
    rig.ui.page = Page::Calls;
    assert!(!rig.contains("Choose someone to call"), "no detail pane");
}

// ------------------------------------------------ generated name (Naming v2)

/// The made-up name strangers see for this PC: on the Calls page with an
/// (i), on the incoming call of a stranger and on Add contact; it follows
/// the backend (new name at every epoch) and the Friendly names switch.
#[test]
fn your_generated_name_is_shown_where_strangers_meet_this_pc() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.switches.generated_names = true;
    rig.ui.page = Page::Calls;
    assert!(!rig.contains("You appear as"), "nothing before this PC is on air");
    rig.send(Update::OwnGeneratedName(Some("Gold Fox".into())));
    assert!(rig.has("You appear as Gold Fox \u{b7} changes every 15 min"));
    assert!(!rig.contains("private Bluetooth ID"));
    rig.click("About your generated name");
    assert!(rig.contains("private Bluetooth ID, which changes every 15 minutes"));
    // The next epoch: a new name.
    rig.send(Update::OwnGeneratedName(Some("Quiet Heron".into())));
    assert!(rig.has("You appear as Quiet Heron \u{b7} changes every 15 min"));
    // Off air, or Friendly names off: no line.
    rig.send(Update::OwnGeneratedName(None));
    assert!(!rig.contains("You appear as"));
    rig.send(Update::OwnGeneratedName(Some("Gold Fox".into())));
    rig.ui.switches.generated_names = false;
    assert!(!rig.contains("You appear as"));
    rig.ui.switches.generated_names = true;
    // Add contact.
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    assert!(rig.has("You appear as Gold Fox"));
    rig.ui.meet = None;
    // A stranger's incoming call shows the name they saw; a contact's does not.
    rig.ui.switches.receive_calls = true;
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    assert!(rig.has("You appear as Gold Fox"));
    rig.ui.call = None;
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: None });
    assert!(!rig.contains("You appear as"));
}

/// Settings > Your name, unset, says what people nearby see instead.
#[test]
fn an_unset_name_says_people_nearby_see_a_random_name() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.ui.own_name = None;
    assert!(rig.contains("Not set. People nearby see a random name that changes every 15 minutes."));
    rig.ui.own_name = Some("Asha".into());
    assert!(rig.has("Your name, Asha"));
}

// ------------------------------------------------------------ row polish

const SCALES: [f32; 3] = [1.0, 1.25, 1.5];

/// The (i) follows the label's last word; it is never pinned beside the
/// switch. A label too long for one line wraps and the row grows.
#[test]
fn the_info_button_follows_the_label_at_every_scale() {
    use egui::accesskit::Role;
    for scale in SCALES {
        let mut rig = Rig::full();
        rig.ctx.set_pixels_per_point(scale);
        rig.ui.page = Page::Settings;
        rig.size = egui::vec2(1400.0 / scale, 6000.0);
        let row = rig.bounds("Voice calls", Some(Role::CheckBox));
        let info = rig.bounds("About Voice calls", Some(Role::Button));
        assert!(info.center().x < row.left() + 240.0, "{scale}: (i) right after the short label: {info:?} in {row:?}");
        assert!((info.center().y - row.center().y).abs() < 4.0, "{scale}: on the label's line");
        // Narrow window: a long label wraps; the (i) sits on its last line.
        rig.size = egui::vec2(400.0, 6000.0);
        let label = "Friendly names for nearby phones";
        let row = rig.bounds(label, Some(Role::CheckBox));
        let info = rig.bounds(&format!("About {label}"), Some(Role::Button));
        assert!(row.height() > super::theme::ROW_HEIGHT + 1.0, "{scale}: the wrapped row grows: {row:?}");
        assert!(info.center().y > row.center().y, "{scale}: (i) on the last line: {info:?} in {row:?}");
        assert!(info.right() < row.right() - super::components::SWITCH_SIZE.x - 8.0, "{scale}: not on the switch");
    }
}

/// Every row in every Settings card has a divider above it except the first,
/// nested groups included (Preview and diagnostics open), at every scale,
/// and never two lines between the same rows.
#[test]
fn every_settings_row_has_exactly_one_divider_from_its_neighbour_at_every_scale() {
    use egui::accesskit::Role;
    for scale in SCALES {
        let mut rig = Rig::full();
        rig.ctx.set_pixels_per_point(scale);
        rig.ui.page = Page::Settings;
        rig.size = egui::vec2(1000.0, 6000.0);
        rig.click("Preview and diagnostics");
        let _ = rig.frame(vec![]);
        let output = rig.frame(vec![]);
        let update = output.platform_output.accesskit_update.clone().expect("AccessKit");
        let mut rows: Vec<(String, f32, f32)> = update
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n.role(), Role::CheckBox | Role::Button))
            .filter_map(|(_, n)| n.bounds().map(|b| (n.label().unwrap_or("").to_owned(), b.y0 as f32, b.y1 as f32, (b.x1 - b.x0) as f32)))
            .filter(|(label, y0, y1, w)| *w > 400.0 && y1 - y0 >= 50.0 && label != "Delete everything")
            .map(|(label, y0, y1, _)| (label, y0, y1))
            .collect();
        rows.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert!(rows.iter().any(|r| r.0 == "Show GPU texture test"), "the nested group is open: {rows:?}");
        let lines: Vec<f32> = output
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::LineSegment { points, stroke } if stroke.color == super::theme::DIVIDER && points[0].y == points[1].y => Some(points[0].y),
                _ => None,
            })
            .collect();
        let mut problems = Vec::new();
        for pair in rows.windows(2) {
            let (above, below) = (&pair[0], &pair[1]);
            if below.1 - above.2 > 12.0 {
                continue;
            }
            let between = lines.iter().filter(|y| **y >= above.2 - 1.5 && **y <= below.1 + 1.5).count();
            if between != 1 {
                problems.push(format!("{} | {}: {between} lines", above.0, below.0));
            }
        }
        assert!(problems.is_empty(), "{scale}: {problems:#?}");
    }
}

/// A value too long to sit beside its title moves under it; a short one
/// stays at the end of the row.
#[test]
fn a_value_that_does_not_fit_moves_under_the_title() {
    use egui::accesskit::Role;
    for scale in SCALES {
        let mut rig = Rig::full();
        rig.ctx.set_pixels_per_point(scale);
        rig.ui.page = Page::Settings;
        rig.ui.own_name = None;
        rig.size = egui::vec2(900.0 / scale, 6000.0);
        let unset = rig.bounds(&format!("Your name, {}", super::NAME_NOT_SET), Some(Role::Button));
        assert!(unset.height() > super::theme::ROW_HEIGHT + 1.0, "{scale}: the long value goes under the title: {unset:?}");
        rig.ui.own_name = Some("Asha".into());
        let set = rig.bounds("Your name, Asha", Some(Role::Button));
        assert!((set.height() - super::theme::ROW_HEIGHT).abs() < 1.0, "{scale}: a short value stays beside it: {set:?}");
    }
}

// ------------------------------------------------------- audit 2026-10-21

/// No live streams on this PC: there is no Live page, Calls is the first
/// screen, and Ctrl+1 opens Calls (no dead-end first screen).
#[test]
fn without_streams_calls_is_the_first_screen_and_live_is_gone() {
    let mut rig = Rig::full();
    rig.shared.borrow_mut().no_streams = true;
    rig.populate();
    assert_eq!(rig.ui.page, Page::Home, "the default before the first frame");
    let names = rig.texts();
    assert_eq!(rig.ui.page, Page::Calls);
    assert!(!names.iter().any(|n| n == "Live"), "no Live in the navigation: {names:?}");
    for expected in ["Calls", "Contacts", "Settings"] {
        assert!(names.iter().any(|n| n == expected), "{expected}");
    }
    rig.ui.page = Page::Settings;
    let _ = rig.frame(vec![ctrl(egui::Key::Num1)]);
    assert_eq!(rig.ui.page, Page::Calls, "Ctrl+1 is the first page shown");
    // With streams, Live is first again.
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.has("Live"));
}

/// The switches no part of the PC reads are hidden (values kept); the real
/// ones carry Android's names; Start with Windows is off and reaches the core.
#[test]
fn settings_show_only_wired_switches_with_android_names() {
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 4000.0);
    rig.ui.page = Page::Settings;
    for hidden in [
        "Visible to contacts",
        "Show my name before they answer",
        "Codes that expire when you add a contact",
        "Accept older verification codes",
        "Visible by name",
        "Receive calls",
    ] {
        assert!(!rig.contains(hidden), "{hidden} is not shown");
    }
    assert!(rig.has("Allow calls"));
    assert!(rig.contains("Your name is never broadcast. It's sent, encrypted, to people you call and to contacts who call you."));
    assert!(!rig.ui.switches.start_with_windows, "off by default");
    rig.click("Start with Windows");
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert!(s.start_with_windows),
        other => panic!("{other:?}"),
    }
    // Hidden switches keep their saved values.
    rig.ui.switches.visible_by_name = true;
    rig.click("Play ringing tone");
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert!(s.visible_by_name),
        other => panic!("{other:?}"),
    }
}

/// An empty Calls list says what the phone needs to find this PC.
#[test]
fn the_empty_calls_list_says_how_a_phone_finds_this_pc() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    assert!(rig.contains("Both phones need a current MeshLive. On the phone: Settings \u{203a} Advanced \u{203a} Connection \u{203a} turn on Find PCs."));
}
