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
        // Go live is on by default, so it asks about exposure every session;
        // that consent has its own tests (they set this back to true).
        ui.live_consent_required = false;
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
            Contact { id: 10, name: "Priya".into(), by_qr: true, nearby: true, lease: true, blocked: false, ..Default::default() },
            Contact { id: 12, name: "Ravi".into(), by_qr: false, nearby: false, lease: true, blocked: false, ..Default::default() },
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
    /// Text this frame painted, including a ListRow's subtitle (AccessKit
    /// names do not carry it).
    fn painted(&mut self, text: &str) -> bool {
        let output = self.frame(vec![]);
        output.shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Text(shape) => shape.galley.text().contains(text),
            _ => false,
        })
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
    assert!(rig.has("Live"), "Live stays in the navigation");
    assert_eq!(rig.ui.page, Page::Home);
    assert!(!rig.has("Go live"));
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

/// Enter is the welcome's main button on every page, wherever the focus is:
/// four presses pass the four pages (Allow on Start with Windows).
#[test]
fn enter_starts_from_the_welcome() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    let _ = rig.frame(vec![]);
    for _ in 0..4 {
        assert!(!rig.ui.switches.onboarded);
        let _ = rig.frame(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }]);
        let _ = rig.frame(vec![]);
    }
    assert!(rig.ui.switches.onboarded);
    assert!(rig.ui.switches.start_with_windows, "Enter on Start with Windows is Allow");
}

/// Add contact stays pinned in view however long the contact list is.
#[test]
fn add_contact_is_pinned_at_the_bottom_of_a_long_contact_list() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Contacts;
    rig.send(Update::Contacts(
        (0..40).map(|i| Contact { id: 100 + i, name: format!("Person {i}"), by_qr: true, nearby: false, lease: true, blocked: false, ..Default::default() }).collect(),
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
    rig.click("Hidden");
    assert!(rig.contains(&format!("Phones can't call this PC. {NO_PERIPHERAL}")), "the status popover says why");
    let _ = rig.frame(vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
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
    assert!(rig.log().iter().any(|c| matches!(c, Command::Watch { id: 1, .. })));
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
    assert!(rig.has("Unmute"));
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
        rig.ui.call_details_open = true;
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
            rig.ui.call_details_open = true;
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
    assert!(!rig.contains(super::screens::BONDED_SHORT));
    rig.send(Update::CallBonded);
    assert!(rig.contains(super::screens::BONDED_SHORT), "one compact line");
    assert!(!rig.contains(super::screens::BONDED_NOTE));
    rig.click("Learn more");
    assert!(rig.contains(super::screens::BONDED_NOTE), "the whole explanation on request");
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

/// Voice calls have no switch (Android, 2026-10-17): an off saved by an older
/// Windows build no longer stops a voice call.
#[test]
fn a_voice_call_works_even_after_an_old_saved_voice_calls_off() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.switches = crate::settings_store::decode("voice_calls=0\nonboarded=1\n");
    rig.ui.page = Page::Calls;
    assert!(!rig.disabled("Voice call Priya"));
    assert!(rig.ui.place_call(10, true));
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
    rig.ui.switches.receive_calls = true;
    rig.send(Update::Visibility { visible: true, note: None });
    rig.ui.pump();
    assert!(rig.has("Status: ready"));
    let why = meshlive::qw_policy::NOT_LISTED;
    rig.send(Update::Visibility { visible: false, note: Some(why.into()) });
    rig.ui.pump();
    assert!(rig.has("Hidden"));
    rig.click("Hidden");
    assert!(rig.contains("hasn't been checked"), "the popover says why");
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
    // A working microphone is shown by the Mute button alone (no status words).
    assert!(rig.has("Mute"));
    assert!(!rig.contains("Microphone on"));
    let why = "Microphone access is off for desktop apps in Windows privacy settings, so the other person can't hear you.";
    rig.send(Update::CallMic(Some(why.into())));
    assert!(rig.contains("Microphone access is off"));
    assert!(rig.has("Open microphone settings"));
    assert!(!rig.contains("Microphone on"));
}

#[test]
fn holding_mute_talks_through_readiness_and_letting_go_or_losing_focus_mutes_again() {
    for hold in [false, true] {
        let mut rig = Rig::full(); rig.ui.hold_call_talk = hold; rig.time = Some(0.0);
        rig.send(Update::IncomingCall { peer: "Test peer".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
        rig.ui.pump(); rig.click("Accept");
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOff)));
        rig.send(Update::CallMediaReady); rig.send(Update::CallMic(None));
        rig.click("Mute");
        let pos = rig.bounds("Unmute", None).center();
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        assert!(!rig.ui.call.as_ref().unwrap().sending_audio(), "a press alone is not a hold");
        rig.time = Some(0.5); let _ = rig.frame(vec![]);
        assert!(rig.ui.call.as_ref().unwrap().sending_audio(), "held past the hold time: talking");
        rig.send(Update::CallCaptureStarting);
        rig.time = Some(1.0); let _ = rig.frame(vec![]);
        rig.time = Some(2.0); rig.send(Update::CallMic(None)); rig.send(Update::CallCaptureReady);
        let _ = rig.frame(vec![]);
        assert!(rig.ui.call.as_ref().unwrap().talking && rig.ui.call.as_ref().unwrap().sending_audio(), "past the click limit and through readiness");
        let _ = rig.frame(vec![egui::Event::PointerMoved(egui::Pos2::ZERO)]);
        assert_eq!(rig.ui.call.as_ref().unwrap().talking, !hold, "only the Experimental arm ends the hold when the pointer leaves Mute");
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos)]);
        let _ = rig.frame(vec![button(false)]);
        let call = rig.ui.call.as_ref().unwrap();
        assert!(!call.talking && !call.sending_audio() && call.muted, "letting go mutes again; the click is not Unmute");
        assert_eq!(rig.signals().last(), Some(&Signal::MicrophoneMuted));
        rig.time = Some(4.0); let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        rig.time = Some(4.5); let _ = rig.frame(vec![]);
        assert!(rig.ui.call.as_ref().unwrap().sending_audio());
        rig.focused = false;
        let _ = rig.frame(vec![egui::Event::WindowFocused(false)]);
        assert!(!rig.ui.call.as_ref().unwrap().sending_audio(), "losing focus ends the hold");
    }
}

#[test]
fn a_short_click_on_mute_unmutes_and_holding_while_unmuted_does_nothing() {
    let mut rig = Rig::full(); rig.time = Some(0.0);
    rig.populate(); rig.ui.pump();
    assert!(rig.ui.place_call(10, true));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    rig.send(Update::CallMediaReady);
    let pos = rig.bounds("Mute", None).center();
    let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
    rig.time = Some(1.0); let _ = rig.frame(vec![]);
    assert!(!rig.ui.call.as_ref().unwrap().talking, "not muted: holding is not talking");
    let _ = rig.frame(vec![button(false)]);
    rig.click("Mute");
    assert!(rig.ui.call.as_ref().unwrap().muted);
    rig.click("Unmute");
    assert!(!rig.ui.call.as_ref().unwrap().muted, "a short click still unmutes");
}

#[test]
fn the_call_row_is_mute_camera_end_flip_speaker_with_end_in_the_middle_in_every_state() {
    let mut rig = Rig::full();
    rig.populate(); rig.ui.pump();
    assert!(rig.ui.place_call(10, true));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    rig.send(Update::CallMediaReady);
    let centre = |rig: &mut Rig, name: &str| rig.bounds(name, None).center();
    let end = centre(&mut rig, "End call");
    let xs: Vec<f32> = ["Mute", "Add video", "End call", "Switch camera", "Audio output"].iter().map(|n| centre(&mut rig, n).x).collect();
    assert!(xs.windows(2).all(|w| w[0] < w[1]), "Android's order left to right: {xs:?}");
    assert!((xs[2] - (xs[0] + xs[4]) / 2.0).abs() < 1.0 && (xs[1] - xs[0] - (xs[4] - xs[3])).abs() < 1.0, "End in the middle, equal columns: {xs:?}");
    assert!(rig.disabled("Switch camera") && rig.disabled("Audio output"), "nothing to switch to yet: the slots stay, disabled");
    rig.click("Mute");
    assert_eq!(centre(&mut rig, "End call"), end, "muting moves nothing");
    rig.click("Add video");
    assert_eq!(centre(&mut rig, "End call"), end, "video on moves nothing");
    rig.click("End call");
    assert_eq!(rig.signals().last(), Some(&Signal::HangUp));
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
        let pos = rig.bounds("Unmute", None).center();
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        rig.time = Some(10.0);
        let _ = rig.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
        rig.time = Some(10.5);
        let _ = rig.frame(vec![]);
        rig.send(Update::CallCaptureStarting);
        assert!(rig.has("Starting microphone\u{2026}"));
        assert!(!rig.contains("Microphone on"));
        rig.send(Update::CallMic(None));
        assert!(rig.has("Starting microphone\u{2026}"), "an open notification alone is not the current restart acknowledgement");
        rig.send(Update::CallCaptureReady);
        assert!(rig.has("Talking\u{2026}"));
        assert!(!rig.contains("Microphone on"), "no technical status words");
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
    rig.send(Update::Contacts(vec![Contact { id: 20, name: "Asha".into(), by_qr: true, nearby: false, lease: true, blocked: false, ..Default::default() }]));
    rig.ui.pump();
    rig.ui.contact_more = None;
    rig.click("More for Asha");
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
    // Holding Mute (the pointer down on it) sends unmuted; release sends muted.
    let talk = rig.nodes().into_iter().find(|n| n.text == "Unmute").unwrap().center.unwrap();
    let button = |pressed| egui::Event::PointerButton { pos: talk, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
    rig.time = Some(0.0);
    let _ = rig.frame(vec![egui::Event::PointerMoved(talk)]);
    let _ = rig.frame(vec![button(true)]);
    rig.time = Some(0.5);
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
    ui.live_consent_required = false;
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
    // (Live on screen also tells the core to look for streams: StreamsVisible.)
    let started: Vec<_> = rig.log().into_iter().filter(|c| !matches!(c, Command::StreamsVisible(_))).collect();
    assert_eq!(started, [Command::GoLive { kind: LiveKind::Video, title: "Market".into(), initial: Default::default() }]);
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
    rig.click("Stop broadcast");
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
    rig.click("Stop broadcast");
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
    assert!(rig.contains(CHECKED_BY_CODE));
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
    rig.ui.contact_more = None;
    rig.click("More for Ravi");
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
    rig.ui.contact_more = None;
    rig.click("More for Priya");
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
    for name in ["Allow calls", "Start with Windows"] {
        assert!(rig.has(name), "missing switch {name}");
    }
    for removed in [
        "Friendly names for nearby phones",
        "Show my name before they answer",
        "Voice calls",
        "Play ringing tone",
        "Show my stream title to everyone nearby",
        "Relay video for others",
    ] {
        assert!(!rig.contains(removed), "{removed} has no Settings row");
    }
    // As Android: Block unknown callers is on the Privacy page.
    rig.click("Privacy");
    assert!(rig.has("Block unknown callers"));
    assert!(!rig.ui.switches.block_unknown, "off by default, as Android");
    rig.click("Block unknown callers");
    assert!(rig.ui.switches.block_unknown);
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert!(s.block_unknown && s.receive_calls),
        other => panic!("{other:?}"),
    }
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
    assert!(rig.contains(super::screens::BONDED_SHORT));
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
        Contact { id: 20, name: "Asha".into(), by_qr: true, nearby: false, lease: false, blocked: false, ..Default::default() },
        Contact { id: 21, name: "Ravi".into(), by_qr: true, nearby: false, lease: true, blocked: false, ..Default::default() },
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
    rig.click(super::screens::SEG_SHOW);
    assert!(rig.contains("Getting your code ready"));
    let code = format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789");
    rig.send(Update::Visibility { visible: true, note: None });
    rig.send(Update::OwnCode(Some(code.clone())));
    assert!(rig.has("Your code as a QR code"), "the code is drawn as a QR picture");
    assert!(rig.contains("Visible to people nearby. On the other phone, open Add contact and scan this code."));
    assert!(!rig.contains(&code), "the code text is neither drawn nor read out");
    rig.send(Update::CodeScanned("Asha scanned your code. Now scan theirs (above), so you can find each other.".into()));
    assert!(rig.contains("Asha scanned your code"));
    // Not on air: the reason replaces the visibility line.
    rig.send(Update::Visibility { visible: false, note: Some(meshlive::qw_policy::NOT_LISTED.into()) });
    assert!(rig.contains("hasn't been checked"));
}

#[test]
fn add_contact_says_when_windows_bluetooth_stopped_answering_and_recovers() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    rig.click(super::screens::SEG_SHOW);
    assert!(rig.contains("Getting your code ready"));
    // The worker's announcement start never returned (2026-11-06).
    let guidance = meshlive::bluetooth::radio_watch::RESTART_GUIDANCE;
    rig.send(Update::BluetoothNotResponding(true));
    rig.send(Update::Visibility { visible: false, note: Some(guidance.into()) });
    assert!(!rig.contains("Getting your code ready"), "no endless wait");
    assert!(rig.contains("Turn Bluetooth off and on"));
    // Every page's status says so and offers the way to fix it.
    rig.click("Back");
    assert!(rig.has("Bluetooth unavailable"));
    rig.click("Open Bluetooth settings");
    assert!(rig.log().contains(&Command::OpenBluetoothSettings));
    // Bluetooth answers again: the status is fine and Add contact shows the code.
    rig.send(Update::BluetoothNotResponding(false));
    rig.send(Update::Visibility { visible: true, note: None });
    assert!(rig.has("Status: ready"));
    rig.click("Add contact");
    rig.click(super::screens::SEG_SHOW);
    rig.send(Update::OwnCode(Some("BVMID2.ABC".into())));
    assert!(rig.has("Your code as a QR code"));
    // Swapping to sample people leaves the stuck radio's note behind.
    rig.click("Back");
    rig.send(Update::BluetoothNotResponding(true));
    rig.send(Update::Visibility { visible: false, note: Some(guidance.into()) });
    assert!(rig.has("Bluetooth unavailable"));
    rig.ui.set_demo(true);
    assert!(!rig.has("Bluetooth unavailable"));
    assert!(!rig.contains("Turn Bluetooth off and on"));
}

#[test]
fn add_contact_scans_a_pasted_code_and_asks_whose_screen_it_was() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    assert!(rig.log().contains(&Command::ShowOwnCode));
    rig.send(Update::OwnCode(Some("BVMID2.ABC".into())));
    assert!(!rig.contains("BVMID2.ABC"), "this PC's code is not printed");
    rig.ui.meet.as_mut().unwrap().code = "BVMID2.THEIRS".into();
    rig.click("Scan code");
    assert!(rig.log().contains(&Command::ScanCode("BVMID2.THEIRS".into())));
    assert!(rig.contains("Looking for the phone that shows this code"));
    rig.send(Update::Meet(MeetEvent::Ask { name: "Green Heron".into() }));
    assert!(rig.contains("Is this Green Heron's screen you scanned?"));
    rig.click("Yes, it is");
    // "Save contact as" never starts with an ID (Android); empty keeps it.
    assert!(rig.log().contains(&Command::ConfirmScan { yes: true, name: String::new() }));
    rig.send(Update::Meet(MeetEvent::Done("Green Heron is now a verified contact.".into())));
    assert!(rig.contains("now a verified contact"));
    rig.click("Back");
    assert!(rig.log().contains(&Command::CancelScan));
    assert!(rig.ui.meet.is_none());
}

/// F2: from the moment Add contact opens (before the code arrives) until it
/// closes, the window asks to stay out of captures.
#[test]
fn add_contact_keeps_the_window_out_of_captures_until_it_closes() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    assert!(!rig.ui.code_on_screen());
    rig.click("Add contact");
    assert!(rig.ui.code_on_screen(), "protected before the code arrives");
    rig.send(Update::OwnCode(Some("BVMID2.ABC".into())));
    rig.click(super::screens::SEG_SHOW);
    assert!(rig.has("Your code as a QR code"));
    assert!(rig.ui.code_on_screen());
    let _ = rig.frame(vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
    let _ = rig.frame(vec![]);
    assert!(rig.ui.meet.is_none(), "Escape leaves Add contact");
    assert!(!rig.ui.code_on_screen(), "captures are allowed again");
}

/// The code is copied, not printed; one press hands it to the
/// window once, and a failed copy can be tried again.
#[test]
fn copy_code_hands_this_pcs_code_to_the_window_once_per_press() {
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 1200.0);
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    rig.click(super::screens::SEG_SHOW);
    assert!(!rig.has("Copy code"), "nothing to copy before the code exists");
    let code = format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789");
    rig.send(Update::OwnCode(Some(code.clone())));
    assert_eq!(rig.ui.take_clipboard_request(), None);
    rig.click("Copy code");
    assert_eq!(rig.ui.take_clipboard_request(), Some(code.clone()));
    assert_eq!(rig.ui.take_clipboard_request(), None, "one copy per press");
    rig.ui.clipboard_done(false);
    rig.click("Copy code");
    assert_eq!(rig.ui.take_clipboard_request(), Some(code));
}

#[test]
fn a_caller_with_a_real_key_can_be_blocked_from_recent_calls() {
    use meshlive::blocking::BlockChange;
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.click("Block Priya");
    assert!(rig.ui.block_confirm.is_some(), "it asks first, as Android");
    rig.click("Cancel");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::BlockCaller { .. })), "Cancel blocks nobody");
    rig.click("Block Priya");
    rig.click("Block");
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: true }));
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Blocked });
    rig.ui.pump();
    assert!(rig.contains("Blocked: their calls are declined without ringing."));
    rig.click("Unblock Priya");
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: false }), "unblocking does not ask");
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

/// What a block did is said on the page until Close; a failure is never
/// shown as a success; Recent rows follow the block list.
#[test]
fn block_outcomes_are_said_and_recent_rows_follow_the_list() {
    use meshlive::blocking::BlockChange;
    use super::blocked::{BlockFrom, change_note};
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.send(Update::Blocked(BlockedView { people: vec![BlockedPerson { fingerprint: [5; 32], name: "Priya".into(), since_ms: 0 }], unreadable: false }));
    assert!(rig.has("Unblock Priya"), "the row follows the list");
    rig.send(Update::Blocked(BlockedView { people: vec![], unreadable: true }));
    assert!(rig.has("Unblock Priya"), "an unreadable list changes no row");
    rig.send(Update::Blocked(BlockedView::default()));
    assert!(rig.has("Block Priya"));
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Failed });
    let failed = change_note(BlockFrom::Confirmed, BlockChange::Failed, "Priya");
    assert!(rig.contains(&failed));
    assert!(!rig.contains(&change_note(BlockFrom::Confirmed, BlockChange::Blocked, "Priya")), "a failure never reads as blocked");
    rig.click("Close");
    assert!(!rig.contains(&failed));
}

/// A failed block is said as a failure and Recent still offers Block: the
/// row follows the saved outcome, not the tap.
#[test]
fn a_full_block_list_leaves_recent_offering_block() {
    use meshlive::blocking::BlockChange;
    use super::blocked::{BlockFrom, change_note};
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.click("Block Priya");
    rig.click("Block");
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Full });
    rig.ui.pump();
    assert!(rig.contains(&change_note(BlockFrom::Confirmed, BlockChange::Full, "Priya")));
    assert!(rig.has("Block Priya") && !rig.has("Unblock Priya"), "a full list blocks nobody");
}

/// Decline and block (Android `declineAndBlockButton`): declines, then
/// blocks the caller and says so; never offered for a one-time ID.
#[test]
fn decline_and_block_declines_and_blocks_only_a_caller_with_a_real_key() {
    use meshlive::blocking::BlockChange;
    use super::blocked::{BlockFrom, DECLINE_AND_BLOCK, change_note};
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click(DECLINE_AND_BLOCK);
    assert!(rig.signals().contains(&Signal::Decline), "the call is declined");
    assert_eq!(rig.log().last(), Some(&Command::BlockCaller { fingerprint: [5; 32], blocked: true }), "then the caller is blocked");
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::Ended);
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Blocked });
    rig.click("Back");
    assert!(rig.contains(&change_note(BlockFrom::DeclineAndBlock, BlockChange::Blocked, "Priya")));
    let other = [0x77; 16];
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(other, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(rig.has("Decline") && !rig.has(DECLINE_AND_BLOCK), "a one-time ID can't be blocked one by one");
}

/// A new call over an ended call's screen uses its own key: a one-time
/// caller offers no Decline and block, and a real-key caller blocks itself,
/// never the caller whose Ended screen it replaced.
#[test]
fn decline_and_block_after_an_ended_call_blocks_the_new_caller_not_the_previous_one() {
    use super::blocked::DECLINE_AND_BLOCK;
    let mut rig = Rig::full();
    // The Block unknown callers offer after a one-time caller's Decline has its own test.
    rig.ui.switches.block_unknown_offer_dismissed = true;
    let a = [0xaa; 32];
    rig.send(Update::IncomingCall { peer: "Asha".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some(a) });
    rig.ui.pump();
    rig.click("Decline");
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::Ended);
    let other = [0x77; 16];
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(other, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(!rig.has(DECLINE_AND_BLOCK), "a one-time caller cannot be blocked one by one");
    rig.click("Decline");
    let b = [0xbb; 32];
    rig.send(Update::IncomingCall { peer: "Ravi".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some(b) });
    rig.ui.pump();
    rig.click(DECLINE_AND_BLOCK);
    let log = rig.log();
    let last = log.iter().rev().find(|c| matches!(c, Command::BlockCaller { .. })).unwrap();
    assert_eq!(last, &Command::BlockCaller { fingerprint: b, blocked: true });
    assert!(!log.iter().any(|c| matches!(c, Command::BlockCaller { fingerprint, .. } if *fingerprint == a)), "the previous caller is not blocked");
}

/// Updates from a previous watch task cannot act on a new one: only the
/// current watch's id may set its source or close it.
#[test]
fn stale_watch_updates_cannot_act_on_a_new_watch() {
    use meshlive::stream_watch::StreamRoute;
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_watch(1));
    let first = rig.ui.watch_id.unwrap();
    assert!(rig.ui.start_watch(2));
    let second = rig.ui.watch_id.unwrap();
    assert_ne!(first, second);
    let route = StreamRoute { logical: 1, prefix: [8; 6] };
    rig.send(Update::WatchSource { id: first, fingerprint: [0x11; 32], route });
    rig.ui.pump();
    assert!(rig.ui.watch.is_some() && rig.ui.watch_source.is_none(), "the previous watch's source is ignored");
    rig.send(Update::WatchSource { id: second, fingerprint: [0x22; 32], route });
    rig.ui.pump();
    assert_eq!(rig.ui.watch_source, Some([0x22; 32]), "the current watch's source is used");
    rig.send(Update::WatchSourceBlocked { id: first });
    rig.ui.pump();
    assert!(rig.ui.watch.is_some(), "the previous watch cannot close the new one");
    rig.send(Update::WatchSourceBlocked { id: second });
    rig.ui.pump();
    assert!(rig.ui.watch.is_none(), "the current watch's end closes it");
}

/// Block this person (Android's listener; also the viewer): offered only
/// once the source's key is authenticated; it blocks, hides the stream and
/// leaves, and says so. A blocked source found by the backend leaves at once.
#[test]
fn block_this_person_shows_only_for_a_known_source_and_blocks_hides_and_leaves() {
    use meshlive::blocking::BlockChange;
    use super::blocked::{BLOCK_THIS_PERSON, BlockFrom, change_note};
    for id in [2, 1] {
        let mut rig = Rig::full();
        rig.populate();
        assert!(rig.ui.start_watch(id));
        rig.send(Update::Watch(WatchEvent::Connected));
        assert!(!rig.has(BLOCK_THIS_PERSON), "no authenticated source key yet");
        let route = meshlive::stream_watch::StreamRoute { logical: 1, prefix: [1; 6] };
        rig.send(Update::WatchSource { id: rig.ui.watch_id.unwrap(), fingerprint: [7; 32], route });
        rig.click(BLOCK_THIS_PERSON);
        let log = rig.log();
        let block_at = log.iter().position(|c| matches!(c, Command::BlockCaller { fingerprint, blocked: true } if *fingerprint == [7; 32])).unwrap();
        assert_eq!(&log[block_at + 1..block_at + 3], &[
            Command::HideWatchedStream { source: [7; 16], route },
            Command::StopWatching,
        ], "block, hide this route, then leave");
        assert!(rig.ui.watch.is_none(), "it leaves");
        rig.send(Update::BlockChanged { fingerprint: [7; 32], name: "Asha".into(), change: BlockChange::Blocked });
        assert!(rig.contains(&change_note(BlockFrom::Stream, BlockChange::Blocked, "Asha")));
    }
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_watch(1));
    rig.send(Update::WatchSourceBlocked { id: rig.ui.watch_id.unwrap() });
    rig.ui.pump();
    assert!(rig.ui.watch.is_none());
    assert!(rig.log().contains(&Command::StopWatching));
}

/// "Block this person" hides the route whose source this PC authenticated,
/// not the row that was tapped (a relayed row can join the direct route).
#[test]
fn block_this_person_hides_the_route_that_was_authenticated() {
    use meshlive::stream_watch::StreamRoute;
    use super::blocked::BLOCK_THIS_PERSON;
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_watch(1));
    let id = rig.ui.watch_id.unwrap();
    let direct = StreamRoute { logical: 7, prefix: [3; 6] };
    let relayed = StreamRoute { logical: 7, prefix: [2; 6] };
    rig.send(Update::WatchSource { id, fingerprint: [7; 32], route: direct });
    rig.ui.pump();
    rig.click(BLOCK_THIS_PERSON);
    assert!(rig.log().contains(&Command::HideWatchedStream { source: [7; 16], route: direct }), "the authenticated route is hidden");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::HideWatchedStream { route, .. } if *route == relayed)), "the tapped relayed row is not hidden by this action");
}

/// A contact's More > Block asks first (Android `confirmBlock`); Cancel
/// blocks nobody; Unblock does not ask.
#[test]
fn blocking_a_contact_asks_first_and_unblocking_does_not() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    rig.ui.contact_more = None;
    rig.click("More for Ravi");
    rig.click("Block Ravi");
    assert!(rig.ui.block_confirm.is_some(), "it asks first");
    rig.click("Cancel");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::SetBlocked { .. })), "Cancel blocks nobody");
    // Every sheet row closes the sheet first (Android): open it again.
    rig.click("More for Ravi");
    rig.click("Block Ravi");
    rig.click("Block");
    assert_eq!(rig.log().last(), Some(&Command::SetBlocked { contact: 12, blocked: true }));
    rig.send(Update::Contacts(vec![Contact { id: 12, name: "Ravi".into(), by_qr: false, nearby: false, lease: true, blocked: true, ..Default::default() }]));
    rig.click("More for Ravi");
    rig.click("Unblock Ravi");
    assert_eq!(rig.log().last(), Some(&Command::SetBlocked { contact: 12, blocked: false }));
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
    rig.ui.switches.receive_calls = true;
    rig.click("Hidden");
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
    rig.ui.switches.receive_calls = true;
    assert!(rig.has("Reset needed"));
    rig.click("Reset needed");
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
    for button in ["Next", "Next", "Not now", "Start"] {
        rig.click(button);
    }
    assert!(rig.log().contains(&Command::SetOwnName("Test PC".into())));
}

#[test]
fn the_first_run_welcome_sets_the_optional_name_and_is_shown_once() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    assert!(rig.contains(super::WELCOME_TITLE));
    rig.ui.welcome_name = "  Asha ".into();
    rig.click("Next");
    assert!(rig.contains("Bluetooth is ready."));
    for button in ["Next", "Not now", "Start"] {
        rig.click(button);
    }
    assert!(rig.log().contains(&Command::SetOwnName("Asha".into())));
    assert!(rig.log().iter().any(|c| matches!(c, Command::Switches(s) if s.onboarded)), "saved, so never shown again");
    assert!(!rig.contains(super::WELCOME_TITLE));
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
/// an earlier build: the line was missing there).
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

/// Android's status indicator: all well is the green dot alone ("Status:
/// ready" for a screen reader), a problem adds its word; a click opens the
/// Status sheet, whose Allow calls row replaces the old Reachable switch.
/// The Calls page itself carries no status card.
#[test]
fn the_status_indicator_is_a_dot_when_ready_and_names_the_problem() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.ui.switches.receive_calls = true;
    rig.ui.pc_visible = true;
    rig.ui.own_generated = Some("Gold Fox".into());
    assert!(rig.has("Status: ready"));
    assert!(!rig.has("Ready"), "all well: the dot alone, as Android");
    assert!(!rig.contains("Visible to people nearby"), "no card on the page");
    rig.click("Status: ready");
    assert!(rig.ui.status_open.is_some(), "the click opens the Status sheet");
    assert!(rig.has("Allow calls") && rig.has("Nearby you appear as"));
    assert!(!rig.has("Reachable"), "one name for one switch, and the switch lives in Settings");
    let _ = rig.frame(vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
    assert!(rig.ui.status_open.is_none(), "Escape closes it");
    // Allow calls off: phones can't call, Android's word.
    rig.ui.switches.receive_calls = false;
    assert!(rig.has("Hidden"));
    // A problem shows its word.
    rig.ui.bluetooth = Some(Capabilities { radio: Radio::Off, ..caps(true, true) });
    assert!(rig.has("Bluetooth off"));
}

/// Android's order: Bluetooth first (a live stream no longer hides it), then
/// watching or live (the backend stops announcing calls then), then Hidden.
#[test]
fn the_status_follows_androids_order() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.switches.receive_calls = true;
    rig.ui.pc_visible = true;
    assert_eq!(rig.ui.status_kind(), StatusKind::Ready);
    rig.ui.switches.receive_calls = false;
    assert_eq!(rig.ui.status_kind(), StatusKind::Hidden);
    rig.ui.switches.receive_calls = true;
    assert!(rig.ui.start_live());
    assert_eq!(rig.ui.status_kind(), StatusKind::Live, "not 'On air' any more");
    rig.send(Update::BluetoothNotResponding(true));
    rig.ui.pump();
    assert_eq!(rig.ui.status_kind(), StatusKind::BluetoothUnavailable, "a stuck Bluetooth stack comes first too");
    rig.send(Update::BluetoothNotResponding(false));
    rig.ui.pump();
    rig.ui.bluetooth = Some(Capabilities { radio: Radio::Off, ..caps(true, true) });
    assert_eq!(rig.ui.status_kind(), StatusKind::BluetoothOff, "Bluetooth comes first");
    rig.ui.bluetooth = Some(Capabilities { radio: Radio::Denied, ..caps(true, true) });
    assert_eq!(rig.ui.status_kind(), StatusKind::Permissions);
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_watch(1));
    assert_eq!(rig.ui.status_kind(), StatusKind::Watching);
}

/// A narrow window keeps the whole title and shows the dot alone; a wide one
/// shows the word too (Android `fitStatusChip`).
#[test]
fn a_narrow_window_keeps_the_whole_title_and_shows_the_dot() {
    use egui::accesskit::Role;
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.start_live());
    rig.ui.golive_open = false;
    rig.ui.page = Page::Settings;
    let word = StatusKind::Live.word();
    rig.size = egui::vec2(1400.0, 900.0);
    let wide_title = rig.bounds("Settings", Some(Role::Label));
    let wide_chip = rig.bounds(word, Some(Role::Button));
    rig.size = egui::vec2(360.0, 900.0);
    let narrow_title = rig.bounds("Settings", Some(Role::Label));
    let narrow_chip = rig.bounds(word, Some(Role::Button));
    assert!((narrow_title.width() - wide_title.width()).abs() < 0.5, "the title is never cut: {narrow_title:?} vs {wide_title:?}");
    assert!(wide_chip.width() > 100.0, "wide: the word shows: {wide_chip:?}");
    assert!(narrow_chip.width() < 48.0, "narrow: the dot alone: {narrow_chip:?}");
}

/// Every click opens the sheet (Android 2026-10-21); with Bluetooth off its
/// one action opens Windows Bluetooth settings. Add contact carries the
/// indicator, and Escape closes the sheet before it leaves the screen.
#[test]
fn add_contact_has_the_status_and_its_sheet_fixes_bluetooth() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    rig.ui.bluetooth = Some(Capabilities { radio: Radio::Off, ..caps(true, true) });
    rig.click("Bluetooth off");
    assert!(rig.ui.status_open.is_some(), "a Bluetooth problem opens the sheet too");
    let escape = || egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let _ = rig.frame(vec![escape()]);
    let _ = rig.frame(vec![]);
    assert!(rig.ui.status_open.is_none() && rig.ui.meet.is_some(), "Escape closed the sheet only");
    rig.click("Bluetooth off");
    rig.click("Open Bluetooth settings");
    assert!(rig.log().contains(&Command::OpenBluetoothSettings));
    assert!(rig.ui.status_open.is_none(), "the action closes the sheet");
    let _ = rig.frame(vec![escape()]);
    let _ = rig.frame(vec![]);
    assert!(rig.ui.meet.is_none(), "the next Escape leaves Add contact");
}

/// Go live carries the indicator too, and while live it says calls are paused.
#[test]
fn go_live_has_the_status_and_says_calls_are_paused_while_live() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.switches.receive_calls = true;
    rig.ui.pc_visible = true;
    rig.click("Go live");
    assert!(rig.ui.golive_open);
    assert!(rig.has("Status: ready"));
    assert!(rig.ui.start_live());
    rig.click(StatusKind::Live.word());
    assert!(rig.ui.status_open.is_some());
}

/// While calls are allowed, the sheet warns when Windows blocks MeshLive's
/// notifications (a call would show only in the window and the taskbar).
#[test]
fn the_status_sheet_warns_when_windows_blocks_call_notifications() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    rig.ui.switches.receive_calls = true;
    rig.ui.pc_visible = true;
    rig.ui.notifications_off = true;
    rig.click("Status: ready");
    assert!(rig.contains(super::NOTIFICATIONS_OFF));
    rig.ui.switches.receive_calls = false;
    assert!(!rig.contains(super::NOTIFICATIONS_OFF), "no calls, nothing to warn about");
    rig.ui.switches.receive_calls = true;
    rig.ui.notifications_off = false;
    assert!(!rig.contains(super::NOTIFICATIONS_OFF));
}

/// Settings > Your code opens Add contact with this PC's code (Android: the
/// row has no value).
#[test]
fn your_code_opens_add_contact() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.send(Update::OwnId(Some("7KQ2M-4ZD80".into())));
    rig.ui.pump();
    assert!(!rig.contains("7KQ2M-4ZD80"), "no value on the row");
    rig.click("Your code");
    assert!(rig.ui.meet.is_some(), "your code opens, as on Android");
    assert!(rig.ui.meet.as_ref().is_some_and(|m| m.show_code), "on Show my code (Android EXTRA_SHOW_CODE)");
}

/// 2026-11-12: a phone this PC calls can read its computer name over
/// Bluetooth. Settings > Privacy shows which name, says so, and selecting it
/// opens Windows' About page to rename the PC; without a name, no row.
#[test]
fn privacy_shows_the_computer_name_phones_can_read_and_opens_windows_to_rename_it() {
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.ui.page = Page::Settings;
    rig.ui.settings_page = SettingsPage::Privacy;
    assert!(!rig.contains("Computer name"), "Windows gave no name: no row");
    rig.send(Update::ComputerName(Some("ASHA-LAPTOP".into())));
    rig.ui.pump();
    assert!(rig.contains("ASHA-LAPTOP"), "the name phones can read is shown");
    assert!(rig.painted("can read this name over Bluetooth"), "and what it means");
    rig.click("Computer name, ASHA-LAPTOP. Opens Windows settings to rename this PC.");
    assert!(rig.log().contains(&Command::OpenAboutSettings), "selecting it opens Windows' About page");
    assert_eq!(rig.ui.settings_page, SettingsPage::Privacy, "MeshLive stays on Privacy");
}

/// Android's Settings tree: every page opens from its row, and Back or
/// Escape returns to the top page; the rail's Settings item does too.
#[test]
fn every_settings_page_opens_from_its_row_and_back_or_escape_returns() {
    let escape = || egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.ui.page = Page::Settings;
    for page in [SettingsPage::Privacy, SettingsPage::Connection, SettingsPage::Manual, SettingsPage::Experiments, SettingsPage::Help, SettingsPage::About] {
        rig.click(page.title());
        assert_eq!(rig.ui.settings_page, page);
        rig.click("Back");
        assert_eq!(rig.ui.settings_page, SettingsPage::Main, "Back from {page:?}");
        rig.click(page.title());
        let _ = rig.frame(vec![escape()]);
        let _ = rig.frame(vec![]);
        assert_eq!(rig.ui.settings_page, SettingsPage::Main, "Escape from {page:?}");
    }
    // The status indicator stays on a pushed page; Escape closes its sheet first.
    rig.click("Privacy");
    let chip = rig.ui.status_kind().a11y();
    rig.click(chip);
    assert!(rig.ui.status_open.is_some());
    let _ = rig.frame(vec![escape()]);
    let _ = rig.frame(vec![]);
    assert!(rig.ui.status_open.is_none() && rig.ui.settings_page == SettingsPage::Privacy, "Escape closed the sheet only");
    rig.click("Settings");
    assert_eq!(rig.ui.settings_page, SettingsPage::Main, "the rail item opens the top page");
}

/// Reset private ID asks first; Cancel sends nothing; Reset sends the
/// command and the page says what the backend answered.
#[test]
fn reset_private_id_asks_first_then_resets() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.click("Privacy");
    rig.click("Reset private ID");
    assert!(rig.ui.reset_id_confirm, "it asks first");
    rig.click("Cancel");
    assert!(!rig.ui.reset_id_confirm);
    assert!(!rig.log().contains(&Command::ResetPresenceKey), "Cancel resets nothing");
    rig.click("Reset private ID");
    rig.click("Reset");
    assert!(rig.log().contains(&Command::ResetPresenceKey));
    rig.send(Update::PrivateIdReset(true));
    assert!(rig.contains(super::settings::RESET_ID_DONE));
    rig.send(Update::PrivateIdReset(false));
    assert!(rig.contains(super::settings::RESET_ID_FAILED));
}

/// Settings > Privacy > Blocked people (Android `BlockedPeopleActivity`):
/// people by name or ID with when, Unblock after asking (Cancel unblocks
/// nobody), the empty and unreadable states, Reset after asking, and Back
/// or Escape to Privacy.
#[test]
fn blocked_people_lists_and_unblocks_after_asking_and_resets_an_unreadable_list() {
    use meshlive::blocking::BlockChange;
    use super::blocked::{BlockFrom, EMPTY_TITLE, change_note, since_text};
    let escape = || egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 1400.0);
    rig.ui.page = Page::Settings;
    rig.click("Privacy");
    rig.click("Blocked people");
    assert_eq!(rig.ui.settings_page, SettingsPage::Blocked);
    assert!(rig.contains(EMPTY_TITLE), "nobody blocked yet");
    let since = 1_793_923_200_000 + 43_200_000;
    let id = meshlive::names::handle(&[6; 6]);
    rig.send(Update::Blocked(BlockedView {
        people: vec![
            BlockedPerson { fingerprint: [5; 32], name: "Asha".into(), since_ms: since },
            BlockedPerson { fingerprint: [6; 32], name: id.clone(), since_ms: 0 },
        ],
        unreadable: false,
    }));
    assert!(rig.has("Asha, blocked") && rig.has(&format!("{id}, blocked")));
    assert!(rig.painted(&since_text(since).unwrap()), "when Asha was blocked");
    assert!(!rig.contains(EMPTY_TITLE));
    rig.click("Asha, blocked");
    assert!(rig.ui.unblock_confirm.is_some(), "it asks first");
    rig.click("Cancel");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::BlockCaller { .. })), "Cancel unblocks nobody");
    rig.click("Asha, blocked");
    rig.click("Unblock");
    assert_eq!(rig.log().last(), Some(&Command::BlockCaller { fingerprint: [5; 32], blocked: false }));
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Asha".into(), change: BlockChange::Unblocked });
    assert!(rig.contains(&change_note(BlockFrom::Unblock, BlockChange::Unblocked, "Asha")));
    rig.send(Update::Blocked(BlockedView { people: vec![], unreadable: true }));
    assert!(!rig.contains(EMPTY_TITLE), "an unreadable list is not an empty one");
    rig.click("Reset block list");
    assert!(rig.ui.reset_blocks_confirm, "it asks first");
    rig.click("Reset");
    assert_eq!(rig.log().last(), Some(&Command::ResetBlocked));
    rig.click("Back");
    assert_eq!(rig.ui.settings_page, SettingsPage::Privacy, "Back returns to Privacy");
    rig.click("Blocked people");
    let _ = rig.frame(vec![escape()]);
    let _ = rig.frame(vec![]);
    assert_eq!(rig.ui.settings_page, SettingsPage::Privacy, "Escape returns to Privacy");
}

/// The experiments live on their own page and act at once (this run only).
#[test]
fn the_experiments_page_turns_on_sample_people_and_split_panes() {
    let mut rig = Rig::full();
    // Tall enough for the whole top page (Video quality and Manual video added rows).
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.ui.page = Page::Settings;
    rig.click("Experiments");
    rig.click("Show sample people and streams (Experimental)");
    assert!(rig.ui.demo, "sample people are on");
    rig.click("Split panes on wide windows (Experimental)");
    assert!(rig.ui.split_panes);
    // A wide window then shows the top page beside the chosen page.
    rig.size = egui::vec2(2000.0, 1200.0);
    assert!(rig.has("Delete everything") && rig.has("Show GPU texture test"), "both panes");
    assert!(!rig.has("Back"), "no Back in the split layout");
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
            rig.ui.reset_id_confirm = false;
            rig.ui.block_confirm = None;
            rig.ui.unblock_confirm = None;
            rig.ui.reset_blocks_confirm = false;
            rig.ui.settings_page = SettingsPage::Main;
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
/// the backend (new name at every epoch); friendly names are always on.
#[test]
fn your_generated_name_is_shown_where_strangers_meet_this_pc() {
    let mut rig = Rig::full();
    rig.populate();
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
    // Off air: no line.
    rig.send(Update::OwnGeneratedName(None));
    assert!(!rig.contains("You appear as"));
    rig.send(Update::OwnGeneratedName(Some("Gold Fox".into())));
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
        // "Allow calls" adds a subtitle while it is on; off, it is a plain
        // short row, as "Voice calls" was when this check was written.
        rig.ui.switches.receive_calls = false;
        rig.ui.page = Page::Settings;
        rig.size = egui::vec2(1400.0 / scale, 6000.0);
        let row = rig.bounds("Allow calls", Some(Role::CheckBox));
        let info = rig.bounds("About Allow calls", Some(Role::Button));
        assert!(info.center().x < row.left() + 240.0, "{scale}: (i) right after the short label: {info:?} in {row:?}");
        assert!((info.center().y - row.center().y).abs() < 4.0, "{scale}: on the label's line");
        // Narrow window: a long label wraps; the (i) sits on its last line.
        rig.size = egui::vec2(320.0, 6000.0);
        rig.ui.settings_page = SettingsPage::Privacy;
        let label = "Block unknown callers";
        let row = rig.bounds(label, Some(Role::CheckBox));
        let info = rig.bounds(&format!("About {label}"), Some(Role::Button));
        assert!(row.height() > super::theme::ROW_HEIGHT + 1.0, "{scale}: the wrapped row grows: {row:?}");
        assert!(info.center().y > row.center().y, "{scale}: (i) on the last line: {info:?} in {row:?}");
        assert!(info.right() < row.right() - super::components::SWITCH_SIZE.x - 8.0, "{scale}: not on the switch");
    }
}

/// Every row in every Settings card has a divider above it except the first,
/// on the top page and the Experiments page, at every scale,
/// and never two lines between the same rows.
#[test]
fn every_settings_row_has_exactly_one_divider_from_its_neighbour_at_every_scale() {
    use egui::accesskit::Role;
    for scale in SCALES {
        let mut rig = Rig::full();
        rig.ctx.set_pixels_per_point(scale);
        rig.ui.page = Page::Settings;
        rig.size = egui::vec2(1000.0, 6000.0);
        rig.ui.settings_page = SettingsPage::Experiments;
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
        assert!(rows.iter().any(|r| r.0 == "Show GPU texture test"), "the Experiments page: {rows:?}");
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

/// Live streams can't run on this PC: Live stays first in the navigation
/// (like Android) and its page says plainly why, instead of disappearing.
#[test]
fn without_streams_live_stays_and_says_why() {
    let mut rig = Rig::full();
    rig.shared.borrow_mut().no_streams = true;
    rig.populate();
    let names = rig.texts();
    assert_eq!(rig.ui.page, Page::Home, "Live is still the first screen");
    for expected in ["Live", "Calls", "Contacts", "Settings"] {
        assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
    }
    assert!(rig.contains("Checking Bluetooth"), "before the first check");
    let reason = meshlive::stream_watch::unsupported_reason(
        true,
        &caps(true, false),
        Ok("intel"),
    )
    .unwrap();
    rig.send(Update::LiveSupport(Some(reason.clone())));
    rig.ui.pump();
    assert!(rig.has(super::screens::STREAMS_NOT_HERE));
    assert!(rig.contains("Windows 11 version 24H2"), "names what is needed");
    assert!(rig.has(&reason));
    assert!(!rig.has("Go live"), "no dead Go live button");
    rig.ui.page = Page::Settings;
    let _ = rig.frame(vec![ctrl(egui::Key::Num1)]);
    assert_eq!(rig.ui.page, Page::Home, "Ctrl+1 is Live");
    // With streams, Live has its list and Go live.
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.has("Live"));
    assert!(!rig.has(super::screens::STREAMS_NOT_HERE));
}

/// The switches no part of the PC reads are gone; the real
/// ones carry Android's names; Start with Windows is off and reaches the core.
#[test]
fn settings_show_only_wired_switches_with_android_names() {
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 4000.0);
    rig.ui.page = Page::Settings;
    for hidden in [
        "Visible to contacts",
        "Friendly names for nearby phones",
        "Show my name before they answer",
        "Codes that expire when you add a contact",
        "Accept older verification codes",
        "Visible by name",
        "Receive calls",
    ] {
        assert!(!rig.contains(hidden), "{hidden} is not shown");
    }
    assert!(rig.has("Allow calls"));
    assert!(rig.contains(super::NAME_AUDIENCE), "the name note is shown");
    assert!(!rig.ui.switches.start_with_windows, "off by default");
    rig.click("Start with Windows");
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert!(s.start_with_windows),
        other => panic!("{other:?}"),
    }
}

/// An empty Calls list says what a phone needs to show up here.
#[test]
fn the_empty_calls_list_says_what_a_phone_needs_to_show_up() {
    let mut rig = Rig::full();
    rig.ui.page = Page::Calls;
    assert!(rig.contains(super::NOBODY_NEARBY_HINT));
}

/// The welcome, the name dialog and Settings make the same statement about
/// who sees the name.
#[test]
fn every_place_that_asks_for_the_name_says_who_sees_it() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    assert!(rig.contains(super::NAME_WHO), "the welcome");
    rig.ui.switches.onboarded = true;
    rig.ui.page = Page::Settings;
    rig.size = egui::vec2(1000.0, 3600.0);
    assert!(rig.contains(super::NAME_AUDIENCE), "the Settings note");
    rig.click(&format!("Your name, {}", super::NAME_NOT_SET));
    assert!(rig.contains(super::NAME_WHO), "the name dialog");
}

/// The video call screen fits the window at any size and display scale (the
/// sizes are logical points: 1536x864 is 1920x1080 at 125 %): no part of the
/// picture runs past the window, the controls stay on screen and your own
/// camera's tile is visible inside the picture area. The other person's
/// portrait picture keeps its shape.
#[test]
fn the_video_call_screen_fits_the_window_with_controls_and_self_view() {
    let near = |a: f32, b: f32| (a - b).abs() < 1.0;
    for (w, h) in [(1536.0, 864.0), (1000.0, 720.0), (800.0, 450.0), (640.0, 480.0), (512.0, 384.0)] {
        let mut rig = Rig::full();
        rig.size = egui::vec2(w, h);
        rig.populate();
        rig.ui.pump();
        assert!(rig.ui.place_call(10, false));
        rig.send(Update::CallSignal(remote(ID, Signal::Ringing)));
        rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
        rig.send(Update::CallSignal(remote(ID, Signal::CameraOn)));
        rig.send(Update::CallMediaReady);
        rig.send(Update::CallCamera(None));
        rig.send(Update::CallBonded);
        rig.send(Update::CallMediaInfo { direction: "sending", text: "Sending AV1 video (hardware encoder)".into() });
        rig.send(Update::CallMediaInfo { direction: "receiving", text: "Receiving VP9 video, 352x288 at 15 fps".into() });
        // The S24's portrait picture (352x288, one clockwise quarter turn).
        let peer = std::sync::Arc::new(meshlive::video::PictureSlot::default());
        peer.publish(352, 288, vec![200; 352 * 288 * 4], 0);
        peer.set_turns(1);
        rig.send(Update::CallPictures(PictureHandle(peer)));
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, rig.size);
        assert!(rig.has("Your camera, starting"), "{w}x{h}: the tile shows before the first picture");
        // Your camera's first picture: 4:3, shown in its own shape.
        let own = std::sync::Arc::new(meshlive::video::PictureSlot::default());
        own.publish(160, 120, vec![90; 160 * 120 * 4], 0);
        rig.send(Update::CallSelfPictures(PictureHandle(own)));
        let tile = rig.bounds("Your camera", None);
        let picture_label = rig
            .texts()
            .into_iter()
            .find(|t| t.starts_with("Other person's video"))
            .expect("the picture area");
        let area = rig.bounds(&picture_label, None);
        assert!(screen.contains_rect(area), "{w}x{h}: picture area {area:?} inside the window");
        assert!(area.contains_rect(tile), "{w}x{h}: your camera {tile:?} inside the picture area {area:?}");
        assert!(near(tile.width() / tile.height() * 3.0, 4.0), "{w}x{h}: 4:3 tile {tile:?}");
        for control in ["Mute", "Video off", "End call"] {
            let b = rig.bounds(control, None);
            assert!(screen.contains_rect(b), "{w}x{h}: {control} {b:?} on screen");
            // The controls float over the bottom of the picture, under your camera.
            assert!(area.contains_rect(b), "{w}x{h}: {control} {b:?} over the picture {area:?}");
            assert!(b.top() >= tile.bottom(), "{w}x{h}: {control} below your camera's tile");
        }
        assert!(!rig.contains("Receiving VP9"), "{w}x{h}: codec lines only under Details");
        assert_eq!(rig.ui.peer_size, [288, 352], "{w}x{h}: uploaded upright (the sender's quarter turn applied)");
        let fitted = super::call_layout::fit(area, rig.ui.peer_size, 0).0;
        assert!(near(fitted.width() / fitted.height() * 352.0, 288.0), "{w}x{h}: portrait, not stretched");
        assert!(area.contains_rect(fitted.shrink(0.5)));
        // Camera off: a clear tile instead of nothing.
        rig.click("Video off");
        assert!(rig.has("Your camera is off"), "{w}x{h}");
    }
}


/// A stranger who calls this PC shows the made-up name of its throwaway key
/// (Android `strangerGeneratedName`), not "Someone nearby", until its name
/// label arrives.
#[test]
fn a_stranger_caller_shows_its_generated_name() {
    let mut rig = Rig::full();
    rig.ui.switches.receive_calls = true;
    rig.send(Update::IncomingCall { peer: "Someone nearby".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.send(Update::CallPeerGenerated("Gold Fox".into()));
    rig.ui.pump();
    assert!(rig.has("Gold Fox"));
    assert!(!rig.has("Someone nearby"));
    rig.send(Update::CallPeerName("Asha".into()));
    assert!(rig.has("Asha"), "a signed name label replaces it");
    rig.ui.call = None;
    // A verified caller keeps the contact's name.
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: None });
    rig.send(Update::CallPeerGenerated("Gold Fox".into()));
    assert!(rig.has("Priya") && !rig.has("Gold Fox"));
}

/// names-v2 of a throwaway node ID is byte-exact with Android (labels-063
/// pins the digest; the name comes from the same bundled lists).
#[test]
fn a_throwaway_node_id_names_like_android() {
    let node: Vec<u8> = (0u8..16).collect();
    let name = meshlive::names::stranger_name(meshlive::names::NameInput::Throwaway(&node)).expect("lists installed");
    assert_eq!(name.name.split(' ').count(), 2, "{}", name.name);
    let again = meshlive::names::stranger_name(meshlive::names::NameInput::Throwaway(&node)).unwrap();
    assert_eq!(name, again, "deterministic");
    let rdi_form = meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(&node[..6]));
    assert_ne!(rdi_form.map(|n| n.name), Some(name.name), "a throwaway ID is its own input");
}


/// Streams this PC can't watch (a phone with an older MeshLive) don't fill
/// Live with greyed-out rows: the watchable ones are listed, the rest
/// wait behind one plain line with what to ask, and "Show them" lists them.
#[test]
fn unwatchable_streams_fold_behind_one_plain_line() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.pump();
    rig.ui.page = Page::Home;
    let needs = meshlive::stream_watch::NEEDS_PRIVATE_ID;
    rig.ui.streams[1].unavailable = Some(needs);
    let first = rig.ui.streams[0].headline();
    let second = rig.ui.streams[1].headline();
    assert!(rig.contains(&first));
    assert!(!rig.contains(&second), "the unwatchable one is folded");
    assert!(rig.contains(&format!("1 stream nearby can't be watched from this PC. {needs}.")));
    rig.click("Show them: streams this PC can't watch");
    assert!(rig.contains(&second));
    // None watchable: one friendly empty state with the reason, no rows.
    rig.ui.show_unwatchable = false;
    rig.ui.streams[0].unavailable = Some(needs);
    assert!(rig.has("No streams you can watch yet"));
    assert!(rig.contains(&format!("2 streams nearby can't be watched from this PC. {needs}.")));
    assert!(!rig.contains(&first) && !rig.contains(&second));
    assert!(!rig.contains("rotating discovery"), "no jargon");
}


/// Go live's bitrate slider keeps one length at any value and window width,
/// and its value never sits on top of it (the value is on the line above).
#[test]
fn the_bitrate_slider_keeps_its_size_and_its_value_stays_off_it() {
    use egui::accesskit::Role;
    for width in [640.0, 1000.0, 1536.0] {
        let mut rig = Rig::full();
        rig.size = egui::vec2(width, 1400.0);
        // The slider is in Settings > Manual video since WP11 (Android).
        rig.ui.page = Page::Settings;
        rig.ui.settings_page = SettingsPage::Manual;
        rig.ui.manual_video = true;
        let mut slider_rects = vec![];
        for kbps in [100, 1500, 525] {
            rig.ui.live_settings.kbps = kbps;
            let slider = {
                let _ = rig.frame(vec![]);
                let output = rig.frame(vec![]);
                let update = output.platform_output.accesskit_update.expect("AccessKit");
                update.nodes.iter().find(|(_, n)| n.role() == Role::Slider).and_then(|(_, n)| n.bounds())
                    .map(|b| egui::Rect::from_min_max(egui::pos2(b.x0 as f32, b.y0 as f32), egui::pos2(b.x1 as f32, b.y1 as f32)))
                    .expect("the bitrate slider")
            };
            let value = rig.bounds(&format!("{kbps} kbps"), None);
            assert!(!value.intersects(slider), "{width}: {kbps}: value {value:?} over the slider {slider:?}");
            assert!(value.bottom() <= slider.top() + 0.5, "{width}: the value is on the line above");
            assert!(slider.right() <= width, "{width}: inside the window");
            slider_rects.push(slider);
        }
        for r in &slider_rects[1..] {
            assert!((r.width() - slider_rects[0].width()).abs() < 0.5, "{width}: one length at every value: {slider_rects:?}");
            assert!((r.left() - slider_rects[0].left()).abs() < 0.5, "{width}: it doesn't move");
        }
    }
}

/// Contact us (Settings, About and updates) and Report a bug (Help and
    /// feedback) each open one email draft to MeshLive (Android `contactUsRow`;
    /// Report a bug as the same draft, owner scope); neither is a concern draft.
    #[test]
    fn contact_us_and_report_a_bug_each_open_one_mail_draft() {
        use meshlive::reporting::Draft;
        use super::report::{CONTACT_US, CONTACT_US_SUBTITLE, REPORT_BUG};
        let drafts = |rig: &Rig| rig.log().iter().filter(|c| **c == Command::OpenMailDraft(Draft::Contact)).count();
        let mut rig = Rig::full();
        rig.size = egui::vec2(1000.0, 3000.0);
        rig.ui.page = Page::Settings;
        assert_eq!(drafts(&rig), 0);
        rig.click(&format!("{CONTACT_US}, {CONTACT_US_SUBTITLE}"));
        assert_eq!(drafts(&rig), 1, "one draft per press");
        rig.click(SettingsPage::Help.title());
        rig.click(REPORT_BUG);
        assert_eq!(drafts(&rig), 2);
        assert!(!rig.log().contains(&Command::OpenMailDraft(Draft::Concern)), "only a report drafts a concern");
    }

    /// No email app took the draft (Android `ContactEmailUi`): the address is
    /// shown; Copy puts it on the clipboard and says Copied; Close copies nothing.
    #[test]
    fn a_missing_email_app_shows_the_address_to_copy() {
        use meshlive::reporting::ADDRESS;
        use super::report::COPIED;
        let mut rig = Rig::full();
        rig.ui.page = Page::Contacts;
        rig.send(Update::MailDraftFailed);
        rig.ui.pump();
        assert!(rig.ui.mail_fallback);
        assert!(rig.has(ADDRESS), "the address is shown");
        rig.click("Copy");
        assert_eq!(rig.ui.take_clipboard_request(), Some(ADDRESS.to_owned()));
        assert!(!rig.ui.mail_fallback, "the dialog closes");
        assert_eq!(rig.ui.block_note.as_deref(), Some(COPIED));
        // The note has its own Close; clear it so only the dialog's is on screen.
        rig.ui.block_note = None;
        rig.send(Update::MailDraftFailed);
        rig.ui.pump();
        rig.click("Close");
        assert!(!rig.ui.mail_fallback);
        assert_eq!(rig.ui.take_clipboard_request(), None, "Close copies nothing");
    }

/// Report on a contact (Android `contactRowReport`): blocks without asking,
/// then "Blocked and hidden" offers Tell MeshLive (a concern draft) or Close
/// (no draft); an already blocked person gets the same; a failed block says
/// so and offers no draft.
#[test]
fn report_on_a_contact_blocks_without_asking_then_offers_tell_meshlive() {
    use meshlive::{blocking::BlockChange, reporting::Draft};
    use super::blocked::{BlockFrom, change_note};
    use super::report::{REPORT_DONE, TELL_MESHLIVE};
    let concern = |rig: &Rig| rig.log().iter().filter(|c| **c == Command::OpenMailDraft(Draft::Concern)).count();
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    rig.ui.contact_more = None;
    rig.click("More for Ravi");
    rig.click("Report Ravi");
    assert!(rig.ui.block_confirm.is_none(), "Report does not ask (Android)");
    assert_eq!(rig.log().last(), Some(&Command::SetBlocked { contact: 12, blocked: true }));
    rig.send(Update::BlockChanged { fingerprint: [12; 32], name: "Ravi".into(), change: BlockChange::Blocked });
    rig.ui.pump();
    assert!(rig.ui.report_done);
    assert!(rig.contains(REPORT_DONE));
    assert!(rig.ui.block_note.is_none(), "the dialog says it, not a second note");
    assert_eq!(concern(&rig), 0, "nothing is drafted until asked");
    rig.click(TELL_MESHLIVE);
    assert_eq!(concern(&rig), 1);
    assert!(!rig.ui.report_done, "the dialog closes");
    // Already blocked: the same dialog; Close drafts nothing.
    rig.click("More for Ravi");
    rig.click("Report Ravi");
    rig.send(Update::BlockChanged { fingerprint: [12; 32], name: "Ravi".into(), change: BlockChange::AlreadyBlocked });
    rig.ui.pump();
    assert!(rig.ui.report_done);
    rig.click("Close");
    assert!(!rig.ui.report_done);
    assert_eq!(concern(&rig), 1, "Close drafts nothing");
    // A failed block: no dialog, the failure is said.
    rig.click("More for Ravi");
    rig.click("Report Ravi");
    rig.send(Update::BlockChanged { fingerprint: [12; 32], name: "Ravi".into(), change: BlockChange::Failed });
    rig.ui.pump();
    assert!(!rig.ui.report_done, "a failure is never shown as reported");
    assert_eq!(rig.ui.block_note, Some(change_note(BlockFrom::Report, BlockChange::Failed, "Ravi")));
}

/// Report on the ring screen (Android `declineAndReportButton`): declines,
/// then blocks the caller without asking and offers Tell MeshLive; never
/// offered for a one-time ID.
#[test]
fn report_on_the_ring_screen_declines_then_blocks_and_offers_tell_meshlive() {
    use meshlive::{blocking::BlockChange, reporting::Draft};
    use super::report::{REPORT, TELL_MESHLIVE};
    let mut rig = Rig::full();
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click(REPORT);
    assert!(rig.signals().contains(&Signal::Decline), "the call is declined");
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: true }), "then the caller is blocked");
    assert!(rig.ui.block_confirm.is_none(), "without asking (Android)");
    assert_eq!(rig.ui.call.as_ref().unwrap().phase(), Phase::Ended);
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Blocked });
    rig.ui.pump();
    assert!(rig.ui.report_done);
    assert!(!rig.log().iter().any(|c| matches!(c, Command::OpenMailDraft(_))), "nothing is drafted until asked");
    rig.click(TELL_MESHLIVE);
    assert!(rig.log().contains(&Command::OpenMailDraft(Draft::Concern)));
    assert!(!rig.ui.report_done);
    rig.click("Back");
    let other = [0x77; 16];
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(other, Signal::Invite), fingerprint: None });
    rig.ui.pump();
    assert!(rig.has("Decline") && !rig.has(REPORT), "a one-time ID can't be reported one by one");
}

/// Report while listening or watching (Android `audioListenReport`; the
/// viewer too, as Block this person): offered only once the source's key is
/// authenticated; it blocks, hides this route and leaves, then offers Tell
/// MeshLive.
#[test]
fn report_while_listening_or_watching_blocks_hides_leaves_and_offers_tell_meshlive() {
    use meshlive::{blocking::BlockChange, reporting::Draft};
    use super::report::{REPORT, TELL_MESHLIVE};
    for id in [2, 1] {
        let mut rig = Rig::full();
        rig.populate();
        assert!(rig.ui.start_watch(id));
        rig.send(Update::Watch(WatchEvent::Connected));
        assert!(!rig.has(REPORT), "no authenticated source key yet");
        let route = meshlive::stream_watch::StreamRoute { logical: 1, prefix: [1; 6] };
        rig.send(Update::WatchSource { id: rig.ui.watch_id.unwrap(), fingerprint: [7; 32], route });
        rig.click(REPORT);
        let log = rig.log();
        let block_at = log.iter().position(|c| matches!(c, Command::BlockCaller { fingerprint, blocked: true } if *fingerprint == [7; 32])).unwrap();
        assert_eq!(&log[block_at + 1..block_at + 3], &[
            Command::HideWatchedStream { source: [7; 16], route },
            Command::StopWatching,
        ], "block, hide this route, then leave");
        assert!(rig.ui.watch.is_none(), "it leaves");
        rig.send(Update::BlockChanged { fingerprint: [7; 32], name: "Asha".into(), change: BlockChange::Blocked });
        rig.ui.pump();
        assert!(rig.ui.report_done);
        rig.click(TELL_MESHLIVE);
        assert!(rig.log().contains(&Command::OpenMailDraft(Draft::Concern)));
    }
}

/// "Block %s?" offers Report as Android's neutral button (`confirmBlock`):
/// from a contact and from Recent it reports (the dialog follows) instead
/// of a plain block; Cancel still blocks nobody.
#[test]
fn the_block_question_offers_report_from_a_contact_and_from_recent() {
    use meshlive::blocking::BlockChange;
    use super::report::REPORT;
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    rig.ui.contact_more = None;
    rig.click("More for Ravi");
    rig.click("Block Ravi");
    assert!(rig.ui.block_confirm.is_some());
    rig.click(REPORT);
    assert!(rig.ui.block_confirm.is_none(), "the question closes");
    assert_eq!(rig.log().last(), Some(&Command::SetBlocked { contact: 12, blocked: true }));
    rig.send(Update::BlockChanged { fingerprint: [12; 32], name: "Ravi".into(), change: BlockChange::Blocked });
    rig.ui.pump();
    assert!(rig.ui.report_done, "a report, not a plain block");
    rig.click("Close");
    rig.send(Update::IncomingCall { peer: "Priya".into(), verified: true, invite: remote(ID, Signal::Invite), fingerprint: Some([5; 32]) });
    rig.ui.pump();
    rig.click("Decline");
    rig.click("Back");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    rig.click("Block Priya");
    rig.click("Cancel");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::BlockCaller { .. })), "Cancel blocks nobody");
    rig.click("Block Priya");
    rig.click(REPORT);
    assert!(rig.log().contains(&Command::BlockCaller { fingerprint: [5; 32], blocked: true }));
    rig.send(Update::BlockChanged { fingerprint: [5; 32], name: "Priya".into(), change: BlockChange::Blocked });
    rig.ui.pump();
    assert!(rig.ui.report_done);
}

/// Android's Add contact segments: it opens on "Scan their code"; "Show my
/// code" shows only this PC's code and closes the camera; the window stays
/// out of captures on both segments while Add contact is open.
#[test]
fn add_contact_shows_one_segment_at_a_time() {
    use super::screens::{SEG_SCAN, SEG_SHOW};
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.click("Add contact");
    rig.send(Update::OwnCode(Some("BVMID2.ABC".into())));
    assert!(rig.has("Scan with camera") && !rig.has("Your code as a QR code"), "opens on Scan their code");
    rig.click("Scan with camera");
    assert!(rig.ui.meet.as_ref().unwrap().camera);
    rig.click(SEG_SHOW);
    assert!(rig.has("Your code as a QR code") && !rig.has("Scan with camera"));
    assert!(rig.log().contains(&Command::QrCamera(false)), "the camera closes with its segment");
    assert!(rig.ui.code_on_screen());
    rig.click(SEG_SCAN);
    assert!(!rig.has("Your code as a QR code"));
    assert!(rig.ui.code_on_screen(), "still out of captures while Add contact is open");
}

/// "Save contact as" (Android `ContactNames.suggestion`): your label for a
/// contact you scan again, else empty, never their ID; empty can be saved
/// and keeps their ID.
#[test]
fn save_contact_as_starts_with_your_label_or_empty_never_an_id() {
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    rig.send(Update::Contacts(vec![Contact { id: 20, name: "Asha".into(), lease: true, handle: "7KQ2M-4ZD80".into(), ..Default::default() }]));
    rig.click("Add contact");
    rig.ui.meet.as_mut().unwrap().code = "BVMID2.THEIRS".into();
    rig.click("Scan code");
    rig.send(Update::Meet(MeetEvent::Ask { name: "7KQ2M-4ZD80".into() }));
    assert!(rig.has(super::screens::SAVE_AS));
    assert_eq!(rig.ui.meet.as_ref().unwrap().name, "Asha", "your label for a contact you scan again");
    rig.click("No");
    rig.ui.close_meet();
    rig.click("Add contact");
    rig.ui.meet.as_mut().unwrap().code = "BVMID2.OTHER".into();
    rig.click("Scan code");
    rig.send(Update::Meet(MeetEvent::Ask { name: "9XW3P-1BC77".into() }));
    assert_eq!(rig.ui.meet.as_ref().unwrap().name, "", "never their ID");
    rig.click("Yes, it is");
    assert!(rig.log().contains(&Command::ConfirmScan { yes: true, name: String::new() }), "empty keeps their ID");
}

/// Android's contact sheet (`ContactActions`): the row or its info button
/// opens it; it has no Voice or Video rows (calls start from the row); a
/// contact checked by code only offers "Scan to confirm", which opens Add
/// contact on "Scan their code"; Escape closes the sheet.
#[test]
fn a_contact_opens_its_sheet_without_call_rows_and_a_code_only_contact_offers_scan_to_confirm() {
    use super::contact_sheet::{SCAN_CONFIRM, detail};
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    let row = format!("Priya, {}", rig.ui.contacts[0].subtitle());
    rig.click(&row);
    assert_eq!(rig.ui.contact_more, Some(10), "the row opens the sheet");
    for action in ["Rename Priya", "Safety code with Priya", "Block Priya", "Report Priya", "Forget Priya"] {
        assert!(rig.has(action), "{action}");
    }
    assert!(!rig.has("Voice call") && !rig.has("Video call"), "no call rows (Android hides them)");
    assert!(!rig.has(SCAN_CONFIRM), "a scanned contact needs no confirmation");
    let escape = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let _ = rig.frame(vec![escape]);
    let _ = rig.frame(vec![]);
    assert_eq!(rig.ui.contact_more, None, "Escape closes it");
    rig.click("More for Ravi");
    let ravi = detail(&rig.ui.contacts[1]);
    assert!(rig.contains(&ravi), "how they were verified");
    rig.click(SCAN_CONFIRM);
    assert_eq!(rig.ui.contact_more, None, "the row closes the sheet first");
    assert!(rig.ui.meet.as_ref().is_some_and(|m| !m.show_code), "Add contact on Scan their code");
    assert!(!rig.log().iter().any(|c| matches!(c, Command::SetBlocked { .. } | Command::RenameContact { .. } | Command::ForgetContact(_))), "nothing about the contact changed");
}

/// The claimed-name offer (Android `bindClaim`): a new name they call
/// themselves is only a note until "Use it as my label" (the contact layer
/// renames them and the page says so) or "Ignore"; it never verifies them.
#[test]
fn a_claimed_name_is_offered_on_the_sheet_and_used_only_when_chosen() {
    use super::contact_sheet::{IGNORE_CLAIM, SCAN_CONFIRM, USE_CLAIM, claim_text};
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    let ravi = Contact { id: 12, name: "Ravi".into(), lease: true, claim: Some(ClaimNote::Changed("Ravi K".into())), ..Default::default() };
    rig.send(Update::Contacts(vec![ravi.clone()]));
    rig.click("More for Ravi");
    assert!(rig.contains(&claim_text(&ravi).unwrap()));
    rig.click(IGNORE_CLAIM);
    assert_eq!(rig.log().last(), Some(&Command::ClaimedName { contact: 12, use_it: false }));
    assert_eq!(rig.ui.contact_more, None);
    rig.click("More for Ravi");
    rig.click(USE_CLAIM);
    assert_eq!(rig.log().last(), Some(&Command::ClaimedName { contact: 12, use_it: true }));
    assert!(!rig.log().iter().any(|c| matches!(c, Command::RenameContact { .. })), "the contact layer renames, from the stored claim");
    rig.send(Update::Renamed { contact: 12, outcome: RenameOutcome::Renamed });
    rig.ui.pump();
    assert!(rig.ui.block_note.as_deref().is_some_and(|note| note.contains("Ravi K")), "it says what it saved");
    // Seen: "They call themselves" only, no offer; still checked by code only.
    let seen = Contact { claim: Some(ClaimNote::CallsThemselves("Ravi K".into())), ..ravi };
    rig.send(Update::Contacts(vec![seen.clone()]));
    rig.click("More for Ravi");
    assert!(rig.contains(&claim_text(&seen).unwrap()));
    assert!(!rig.has(USE_CLAIM) && !rig.has(IGNORE_CLAIM));
    assert!(rig.has(SCAN_CONFIRM), "Scan to confirm stays until they are scanned");
}

/// A contact this PC can't recognise yet (Android `bindRecognitionHint`):
/// "Show my code" opens Add contact on this PC's code; after you scanned
/// each other, "Scan again" opens it on "Scan their code".
#[test]
fn a_contact_this_pc_cannot_recognise_offers_show_my_code_or_scan_again() {
    use super::contact_sheet::{SCAN_AGAIN, SHOW_MY_CODE};
    let mut rig = Rig::full();
    rig.ui.pump();
    rig.ui.page = Page::Contacts;
    let asha = Contact { id: 20, name: "Asha".into(), by_qr: true, lease: false, ..Default::default() };
    rig.send(Update::Contacts(vec![asha.clone()]));
    rig.click("More for Asha");
    assert!(!rig.has(SCAN_AGAIN));
    rig.click(SHOW_MY_CODE);
    assert!(rig.ui.meet.as_ref().is_some_and(|m| m.show_code), "Add contact on Show my code");
    assert!(rig.log().contains(&Command::ShowOwnCode));
    rig.ui.close_meet();
    rig.send(Update::Contacts(vec![Contact { both_ways: true, ..asha }]));
    rig.click("More for Asha");
    assert!(!rig.has(SHOW_MY_CODE));
    rig.click(SCAN_AGAIN);
    assert!(rig.ui.meet.as_ref().is_some_and(|m| !m.show_code), "Add contact on Scan their code");
}

/// An incoming call closes the open contact sheet (Android F15), so the
/// ring screen is never covered.
#[test]
fn an_incoming_call_closes_the_contact_sheet() {
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    rig.click("More for Ravi");
    assert_eq!(rig.ui.contact_more, Some(12));
    rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: None });
    let _ = rig.frame(vec![]);
    let _ = rig.frame(vec![]);
    assert_eq!(rig.ui.contact_more, None);
    assert!(rig.has("Accept"), "the ring screen is not covered");
}

#[test]
fn video_live_has_mute_camera_end_flip_pause_with_end_in_the_middle_and_a_timer() {
    let mut rig = Rig::full();
    rig.ui.open_go_live();
    rig.ui.golive.title = "Market".into();
    rig.click("Go live");
    rig.send(Update::Live(LiveEvent::Started));
    let centre = |rig: &mut Rig, name: &str| rig.bounds(name, None).center();
    let xs: Vec<f32> = ["Mute", "Camera off", "Stop broadcast", "Pause"].iter().map(|n| centre(&mut rig, n).x).collect();
    assert!(xs.windows(2).all(|w| w[0] < w[1]), "Android's order: {xs:?}");
    assert!(!rig.has("Switch camera"), "one camera: Flip's place stays empty");
    assert!((xs[2] - (xs[0] + xs[3]) / 2.0).abs() < 1.0, "End in the middle: {xs:?}");
    assert!(rig.painted(&live_timer(0)));
    rig.ui.clock_offset = std::time::Duration::from_secs(125);
    assert!(rig.painted(&live_timer(125)), "the timer runs from going live");
    rig.click("Pause");
    assert!(rig.disabled("Mute") && rig.disabled("Camera off"), "nothing is captured while paused");
    assert_eq!(centre(&mut rig, "Stop broadcast").x, xs[2], "End does not move");
    rig.click("Resume");
    rig.click("Stop broadcast");
    assert!(rig.log().contains(&Command::StopLive));
}

#[test]
fn audio_only_live_shows_the_level_listeners_and_timer_and_switches_to_video_with_the_same_title() {
    let mut rig = Rig::full();
    rig.ui.golive_open = true;
    rig.click("Audio only");
    rig.ui.golive.title = "Evening news".into();
    rig.click("Go live");
    rig.click("Go live with audio");
    rig.send(Update::Live(LiveEvent::Started));
    rig.send(Update::Live(LiveEvent::Viewers(2)));
    assert!(rig.has("Live. 2 listening"));
    assert!(rig.has("Microphone level"));
    rig.send(Update::LiveLevel(0.6));
    rig.ui.pump();
    assert_eq!(rig.ui.golive.level, 0.6);
    assert!(rig.painted(&live_timer(0)));
    let xs: Vec<f32> = ["Mute", "Stop broadcast", "Pause"].iter().map(|n| rig.bounds(n, None).center().x).collect();
    assert!(xs.windows(2).all(|w| w[0] < w[1]) && (xs[1] - (xs[0] + xs[2]) / 2.0).abs() < 1.0, "Mute, End, Pause: {xs:?}");
    rig.click("Switch to video");
    let log = rig.log();
    let stop = log.iter().position(|c| *c == Command::StopLive).expect("the radio broadcast ends");
    assert!(matches!(&log[stop + 1], Command::GoLive { kind: LiveKind::Video, title, .. } if title == "Evening news"),
        "then video starts with the same title: {:?}", &log[stop..]);
    rig.send(Update::Live(LiveEvent::Started));
    assert!(rig.has("Camera off") && !rig.has("Switch to video"));
}

#[test]
fn a_forgotten_audio_broadcast_reminds_then_stops_and_an_answer_keeps_it_going() {
    let short = ReminderLimits {
        no_listener: std::time::Duration::from_secs(30),
        total: std::time::Duration::from_secs(120),
        unanswered_stop: std::time::Duration::from_secs(5),
    };
    let mut rig = Rig::full();
    rig.ui.live_limits = short;
    rig.ui.golive_open = true;
    rig.click("Audio only");
    rig.click("Go live");
    rig.click("Go live with audio");
    rig.send(Update::Live(LiveEvent::Started));
    rig.ui.pump();
    rig.ui.page = Page::Calls;
    rig.ui.golive_open = false;
    let at = |rig: &mut Rig, secs: u64| { rig.ui.clock_offset = std::time::Duration::from_secs(secs); rig.ui.pump(); };
    at(&mut rig, 29);
    assert!(!rig.has("Keep broadcasting"));
    at(&mut rig, 31);
    assert!(rig.has("Keep broadcasting"), "30 (shortened) with nobody listening: the reminder, on any page");
    assert!(rig.ui.take_live_reminder_alert(), "the window is asked to announce it");
    assert!(!rig.ui.take_live_reminder_alert(), "once");
    rig.click("Keep broadcasting");
    assert!(rig.ui.golive.running() && !rig.has("Keep broadcasting"), "an answer keeps it going");
    at(&mut rig, 50);
    assert!(!rig.has("Keep broadcasting"), "the clocks started again at the answer");
    at(&mut rig, 62);
    assert!(rig.has("Keep broadcasting"));
    at(&mut rig, 68);
    assert!(!rig.ui.golive.running(), "5 (shortened) without an answer: stopped");
    assert!(rig.log().contains(&Command::StopLive));
    assert!(rig.contains(super::LIVE_REMINDER_STOPPED));
    assert!(!rig.has("Keep broadcasting"));
}

#[test]
fn a_heard_audio_broadcast_reminds_after_the_long_limit_and_stop_ends_it_but_video_never_reminds() {
    let short = ReminderLimits {
        no_listener: std::time::Duration::from_secs(30),
        total: std::time::Duration::from_secs(120),
        unanswered_stop: std::time::Duration::from_secs(5),
    };
    let mut rig = Rig::full();
    rig.ui.live_limits = short;
    rig.ui.golive_open = true;
    rig.click("Audio only");
    rig.click("Go live");
    rig.click("Go live with audio");
    rig.send(Update::Live(LiveEvent::Started));
    rig.send(Update::Live(LiveEvent::Viewers(1)));
    rig.ui.pump();
    rig.ui.clock_offset = std::time::Duration::from_secs(119);
    assert!(!rig.has("Keep broadcasting"), "someone is listening");
    rig.ui.clock_offset = std::time::Duration::from_secs(121);
    assert!(rig.has("Keep broadcasting"), "the long limit (shortened)");
    rig.click("Stop");
    assert!(!rig.ui.golive.running() && rig.log().contains(&Command::StopLive));
    let mut video = Rig::full();
    video.ui.live_limits = short;
    video.ui.open_go_live();
    video.click("Go live");
    video.send(Update::Live(LiveEvent::Started));
    video.ui.pump();
    video.ui.clock_offset = std::time::Duration::from_secs(600);
    assert!(!video.has("Keep broadcasting") && video.ui.golive.running(), "video has no reminder (Android)");
}

#[test]
fn speaker_opens_the_audio_output_picker_and_the_pick_goes_to_the_call() {
    let mut rig = Rig::full();
    rig.populate(); rig.ui.pump();
    assert!(rig.ui.place_call(10, true));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    rig.send(Update::CallMediaReady);
    let speakers = ("spk".to_owned(), "Speakers (Realtek)".to_owned());
    let headset = ("hs".to_owned(), "Headset (Jabra)".to_owned());
    rig.send(Update::CallOutputs { outputs: vec![speakers.clone()], current: Some("spk".into()) });
    assert!(rig.disabled("Audio output"), "one output: nothing to choose");
    rig.send(Update::CallOutputs { outputs: vec![speakers, headset], current: Some("spk".into()) });
    rig.click("Audio output");
    assert!(rig.has("Speakers (Realtek), Selected") && rig.has("Headset (Jabra)"));
    rig.click("Headset (Jabra)");
    assert_eq!(rig.log().last(), Some(&Command::CallOutput("hs".into())));
    assert!(!rig.has("Headset (Jabra)"), "a pick closes the sheet");
    rig.click("Audio output");
    assert!(rig.has("Headset (Jabra), Selected"));
    rig.click("Headset (Jabra), Selected");
    rig.click("End call");
    assert!(!rig.has("Audio output"), "the picker goes with the call");
}

#[test]
fn flip_switches_the_camera_only_with_two_cameras_and_the_camera_on() {
    let mut rig = Rig::full();
    rig.populate(); rig.ui.pump();
    assert!(rig.ui.place_call(10, false));
    rig.send(Update::CallSignal(remote(ID, Signal::Accept)));
    rig.send(Update::CallMediaReady);
    rig.send(Update::CallCamera(None));
    rig.send(Update::Cameras(1));
    assert!(rig.disabled("Switch camera"), "one camera: the slot stays, disabled");
    rig.send(Update::Cameras(2));
    rig.click("Switch camera");
    assert_eq!(rig.log().last(), Some(&Command::SwitchCamera));
    rig.click("Video off");
    assert!(rig.disabled("Switch camera"), "camera off: nothing to flip");
    let mut live = Rig::full();
    live.send(Update::Cameras(2));
    live.ui.pump();
    live.ui.open_go_live();
    live.click("Go live");
    live.send(Update::Live(LiveEvent::Started));
    live.click("Switch camera");
    assert_eq!(live.log().last(), Some(&Command::SwitchCamera));
    live.click("Camera off");
    assert!(live.disabled("Switch camera"));
}

#[test]
fn the_radio_listener_shows_the_sound_level_and_video_does_not() {
    let mut rig = Rig::full();
    rig.populate(); rig.ui.pump();
    assert!(rig.ui.start_watch(2));
    rig.send(Update::Watch(WatchEvent::Connected));
    rig.send(Update::WatchLevel(0.5));
    assert!(rig.has("Sound level"));
    assert_eq!(rig.ui.watch_level, 0.5);
    rig.ui.leave_watch();
    assert!(rig.ui.start_watch(1));
    assert_eq!(rig.ui.watch_level, 0.0, "a new watch starts silent");
    assert!(!rig.has("Sound level"), "video has no meter");
}

#[test]
fn declining_a_one_time_caller_offers_block_unknown_callers_until_not_now() {
    let ring = |rig: &mut Rig, fingerprint: Option<[u8; 32]>| {
        rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint });
        rig.ui.pump();
    };
    let mut rig = Rig::full();
    ring(&mut rig, None);
    rig.click("Decline");
    assert!(rig.signals().contains(&Signal::Decline));
    assert!(rig.contains(super::blocked::DECLINED_OFFER));
    rig.click("Not now");
    assert!(rig.ui.switches.block_unknown_offer_dismissed && !rig.ui.switches.block_unknown);
    assert!(matches!(rig.log().last(), Some(Command::Switches(s)) if s.block_unknown_offer_dismissed), "Not now is saved");
    rig.click("Back");
    ring(&mut rig, None);
    rig.click("Decline");
    assert!(!rig.contains(super::blocked::DECLINED_OFFER), "never asked again");
    let mut on = Rig::full();
    ring(&mut on, None);
    on.click("Decline");
    on.click(super::blocked::TURN_ON_UNKNOWN);
    assert!(on.ui.switches.block_unknown);
    assert!(matches!(on.log().last(), Some(Command::Switches(s)) if s.block_unknown), "Block unknown callers is turned on and saved");
    let mut real = Rig::full();
    ring(&mut real, Some([3; 32]));
    real.click("Decline");
    assert!(!real.contains(super::blocked::DECLINED_OFFER), "a caller with a real key can be blocked by itself");
}

// ------------------------------------------------------------- WP11 polish

/// Lists show the full verified check only for a contact scanned in person
/// (Android `DisplayIdentity`: the shield, or "Checked by code" for a contact
/// only checked by code), in Contacts and in Calls > Nearby alike.
#[test]
fn only_contacts_scanned_in_person_get_the_full_check_in_lists() {
    let mut rig = Rig::full();
    rig.populate();
    // Ravi (checked by code only) comes into range too.
    rig.send(Update::People(vec![
        Person { id: 10, name: "Priya".into(), reach: Reach::Direct, verified: true, copied: false },
        Person { id: 12, name: "Ravi".into(), reach: Reach::Direct, verified: true, copied: false },
        Person { id: 11, name: "Blue Otter".into(), reach: Reach::Via(2), verified: false, copied: false },
    ]));
    rig.ui.pump();
    assert!(rig.ui.scanned_in_person(10), "Priya was scanned in person: the full check");
    assert!(!rig.ui.scanned_in_person(12), "Ravi was only checked by code: no full check");
    assert!(!rig.ui.scanned_in_person(11), "a stranger is never marked");
    rig.ui.page = Page::Calls;
    assert!(rig.has(&format!("Ravi, Nearby \u{b7} {CHECKED_BY_CODE}")), "Calls > Nearby says how Ravi was checked");
    assert!(!rig.contains(&format!("Priya, Nearby \u{b7} {CHECKED_BY_CODE}")));
    rig.click("Video call Ravi");
    assert_eq!(rig.ui.call.as_ref().unwrap().peer, "Ravi", "a contact checked by code is still called as before");
    let mut rig = Rig::full();
    rig.populate();
    rig.ui.page = Page::Contacts;
    assert!(rig.has(&format!("Ravi, {CHECKED_BY_CODE}")), "the Contacts row says so too");
}

/// Types into the screen's only text field as a screen reader or voice
/// input does (UI Automation SetValue).
fn type_into_field(rig: &mut Rig, text: &str) {
    use egui::accesskit::{Action, ActionData, ActionRequest, Role};
    let _ = rig.frame(vec![]);
    let output = rig.frame(vec![]);
    let update = output.platform_output.accesskit_update.expect("AccessKit");
    let (id, _) = update.nodes.iter().find(|(_, n)| n.role() == Role::TextInput).expect("a text field");
    let request = ActionRequest { action: Action::SetValue, target: *id, data: Some(ActionData::Value(text.into())) };
    let _ = rig.frame(vec![egui::Event::AccessKitActionRequest(request)]);
    let _ = rig.frame(vec![]);
}

/// Calls > Nearby (Android `nearbyPeopleSearch`): with more than eight people
/// "Search people" appears and keeps only the people whose name contains what
/// was typed, contacts and strangers alike; with a few people there is none.
#[test]
fn calls_search_people_appears_for_a_long_list_and_keeps_matching_names() {
    use egui::accesskit::Role;
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.populate();
    rig.ui.page = Page::Calls;
    assert!(!rig.nodes().iter().any(|n| n.role == Role::TextInput), "two people: no search");
    let mut people = vec![Person { id: 10, name: "Priya".into(), reach: Reach::Direct, verified: true, copied: false }];
    for (i, name) in ["Blue Otter", "Gold Fox", "Red Kite", "Green Owl", "Grey Seal", "Pink Crab", "Tan Hare", "Jade Wren", "Prim Heron"].iter().enumerate() {
        people.push(Person { id: 20 + i as Handle, name: (*name).into(), reach: Reach::Direct, verified: false, copied: false });
    }
    rig.send(Update::People(people));
    rig.ui.pump();
    type_into_field(&mut rig, "PRI");
    assert_eq!(rig.ui.people_query, "PRI");
    assert!(rig.has("Video call Priya") && rig.has("Video call Prim Heron"), "any case, contacts and strangers");
    assert!(!rig.has("Video call Blue Otter") && !rig.has("Video call Gold Fox"), "the others are hidden");
    rig.click("Video call Prim Heron");
    assert_eq!(rig.ui.call.as_ref().unwrap().peer, "Prim Heron", "a found person is called as before");
    rig.ui.call = None;
    type_into_field(&mut rig, "");
    assert!(rig.has("Video call Blue Otter") && rig.has("Video call Priya"), "an empty search shows everyone");
}

/// Recent (Android `RecentCalls.recordBlocked`): a call "Block unknown
/// callers" declined is listed as a "Blocked call" with when it was declined,
/// naming nobody and offering neither call back nor Block; other calls keep
/// their actions.
#[test]
fn recent_lists_a_blocked_unknown_call_that_names_nobody() {
    use super::screens::BLOCKED_CALL;
    let mut rig = Rig::full();
    rig.populate();
    let ring = |rig: &mut Rig| {
        rig.send(Update::IncomingCall { peer: "Blue Otter".into(), verified: false, invite: remote(ID, Signal::Invite), fingerprint: Some([3; 32]) });
        rig.ui.pump();
    };
    ring(&mut rig);
    rig.click("Decline");
    rig.click("Back");
    rig.send(Update::CallBlockedUnknown);
    rig.ui.pump();
    assert!(rig.ui.recent[0].declined_at.is_some() && rig.ui.recent[1].declined_at.is_none(), "newest first");
    rig.ui.page = Page::Calls;
    rig.ui.recent_tab = true;
    assert!(rig.has(&format!("{BLOCKED_CALL}, Declined, just now")), "newest first, with when");
    let blocks: Vec<String> = rig.nodes().into_iter().filter(|n| n.is_control() && n.text.starts_with("Block ")).map(|n| n.text).collect();
    assert_eq!(blocks, ["Block Blue Otter"], "only the caller who showed a real key can be blocked; the blocked call offers no Block");
    assert!(!rig.contains("Call back"), "and no call back");
    rig.ui.clock_offset = std::time::Duration::from_secs(5 * 60);
    assert!(rig.has(&format!("{BLOCKED_CALL}, Declined, 5 minutes ago")));
}

/// Settings > Video quality (Android `settingsRowVideo`) is Standard until
/// Manual video settings is turned on in Settings > Manual video; Go live then
/// sends exactly the chosen size and frame rate (its own screen has no video
/// choices), and turning Manual off sends the default video again. Fill
/// screen with video is on and saved when turned off.
#[test]
fn video_quality_is_auto_until_manual_video_settings_are_chosen() {
    use meshlive::stream_source::Settings;
    let configured = |rig: &Rig| rig.log().iter().rev().find_map(|c| match c { Command::ConfigureLive(s) => Some(*s), _ => None });
    let go_live = |rig: &mut Rig| {
        rig.ui.page = Page::Home;
        rig.ui.open_go_live();
        rig.click("Go live");
        rig.click("Go live");
    };
    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.ui.live_consent_required = true;
    rig.ui.page = Page::Settings;
    rig.click("Video quality, Standard");
    assert_eq!(rig.ui.settings_page, SettingsPage::Video);
    assert!(rig.ui.switches.video_fill, "Fill screen with video is on by default");
    rig.click("Fill screen with video");
    assert!(matches!(rig.log().last(), Some(Command::Switches(s)) if !s.video_fill), "turned off and saved");
    rig.click("Back");
    rig.click("Manual video");
    assert!(!rig.has("640 \u{d7} 360"), "the choices show only with Manual video settings on");
    rig.click("Manual video settings");
    rig.click("640 \u{d7} 360");
    rig.click("24 fps");
    rig.click("Back");
    assert!(rig.has("Video quality, Manual"));
    go_live(&mut rig);
    assert!(rig.ui.golive.running());
    assert_eq!(configured(&rig), Some(Settings { width: 640, height: 360, fps: 24, ..Settings::default() }), "exactly what was chosen");
    rig.ui.stop_live();
    assert!(!rig.has("640 \u{d7} 360") && !rig.has("24 fps"), "Go live has no video choices of its own");
    rig.ui.golive_open = false;
    rig.ui.page = Page::Settings;
    rig.click("Manual video");
    rig.click("Manual video settings");
    rig.click("Back");
    assert!(rig.has("Video quality, Standard"));
    go_live(&mut rig);
    assert_eq!(configured(&rig), Some(Settings::default()), "Standard sends the default video again");
}

/// "Warn when video freezes" (Experimental, off; Android `ViewerFrameAge`):
/// off nothing is added; on, a fresh picture shows how long ago it came, a
/// picture that stopped for more than 1.5 s is dimmed as stalled until the
/// next one, a broadcaster's pause is no freeze, and a lost signal says when
/// the last picture came.
#[test]
fn warn_when_video_freezes_marks_a_stalled_picture_until_the_next_one() {
    use std::time::Duration;
    let publish = |slot: &meshlive::video::PictureSlot| slot.publish(64, 36, vec![120; 64 * 36 * 4], 0);
    let watch = |rig: &mut Rig| {
        let slot = std::sync::Arc::new(meshlive::video::PictureSlot::default());
        assert!(rig.ui.start_watch(1));
        rig.send(Update::Watch(WatchEvent::Connected));
        rig.send(Update::CallPictures(PictureHandle(slot.clone())));
        publish(&slot);
        rig.ui.pump();
        slot
    };
    let mut off = Rig::full();
    off.populate();
    let _slot = watch(&mut off);
    off.ui.clock_offset = Duration::from_secs(3);
    assert!(!off.painted(VIEWER_PICTURE_STALLED) && !off.painted("Last update"), "off by default: nothing added");

    let mut rig = Rig::full();
    rig.size = egui::vec2(1000.0, 3000.0);
    rig.populate();
    rig.ui.page = Page::Settings;
    rig.click("Video quality, Standard");
    rig.click("Warn when video freezes");
    assert!(matches!(rig.log().last(), Some(Command::Switches(s)) if s.frame_age), "turned on and saved");
    rig.size = egui::vec2(1000.0, 720.0);
    let slot = watch(&mut rig);
    assert!(rig.painted("Last update <1 s ago"), "a fresh picture says how long ago it came");
    rig.ui.clock_offset = Duration::from_secs(2);
    assert!(rig.painted(VIEWER_PICTURE_STALLED) && rig.painted("Last picture "), "stopped for 2 s: stalled");
    assert!(rig.has(WATCH_STATUS_STALLED), "and the status no longer says Live");
    publish(&slot);
    assert!(!rig.painted(VIEWER_PICTURE_STALLED) && rig.painted("Last update"), "the next picture clears it");
    rig.send(Update::Watch(WatchEvent::Paused(true)));
    rig.ui.pump();
    rig.ui.clock_offset = Duration::from_secs(10);
    assert!(!rig.painted(VIEWER_PICTURE_STALLED), "a pause is no freeze");
    rig.send(Update::Watch(WatchEvent::Paused(false)));
    rig.ui.pump();
    assert!(!rig.painted(VIEWER_PICTURE_STALLED), "the stall clock restarts after the pause");
    rig.send(Update::Watch(WatchEvent::Lost));
    rig.ui.pump();
    assert!(rig.painted(VIEWER_SIGNAL_LOST) && rig.painted("Last picture "), "a lost signal says when the last picture came");
}

/// The first run (Android `OnboardingActivity`): four pages with step dots;
/// Back (or Escape) returns a page; on Start with Windows "Not now" leaves it
/// off and "Allow" turns it on and saves it at once; Start finishes, once.
#[test]
fn the_first_run_pages_go_forward_and_back_and_offer_start_with_windows() {
    let escape = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = false;
    assert!(rig.has("Step 1 of 4"));
    assert!(!rig.has("Back"), "nothing before the first page");
    rig.click("Next");
    assert!(rig.has("Step 2 of 4") && rig.contains("Bluetooth is ready."));
    rig.click("Back");
    assert!(rig.has("Step 1 of 4"), "Back returns a page");
    rig.click("Next");
    rig.click("Next");
    assert!(rig.has("Step 3 of 4") && rig.has("Not now"));
    let _ = rig.frame(vec![escape]);
    let _ = rig.frame(vec![]);
    assert!(rig.has("Step 2 of 4"), "Escape returns a page");
    rig.click("Next");
    rig.click("Not now");
    assert!(rig.has("Step 4 of 4") && !rig.ui.switches.start_with_windows, "Not now leaves it off");
    rig.click("Back");
    rig.click("Allow");
    assert!(rig.ui.switches.start_with_windows);
    assert!(matches!(rig.log().last(), Some(Command::Switches(s)) if s.start_with_windows && !s.onboarded), "on and saved at once");
    rig.click("Back");
    assert!(rig.has("Next") && !rig.has("Allow"), "once on, the page only moves on");
    rig.click("Next");
    rig.click("Start");
    assert!(rig.ui.switches.onboarded);
    assert!(!rig.has("Step 4 of 4"), "shown once");
}

/// WP6: a call the core ended because the other person turned out to be
/// someone you blocked ends on the call screen and says why there and in Recent.
#[test]
fn a_call_ended_by_a_revealed_block_says_so_on_the_call_and_in_recent() {
    let mut rig = Rig::full();
    rig.populate();
    assert!(rig.ui.place_call(10, false));
    rig.send(Update::CallEnded { reason: CallEndReason::BlockedPerson });
    rig.ui.pump();
    assert_eq!(rig.ui.call.as_ref().map(|c| c.phase()), Some(Phase::Ended));
    assert_eq!(rig.ui.call_status().as_deref(), Some(CallEndReason::BlockedPerson.text()));
    assert_eq!(rig.ui.recent[0].result, CallEndReason::BlockedPerson.text(), "Recent says the same");
}

/// 2026-11 Windows -> S22: the phone stopped receiving this PC while Windows
/// Bluetooth here was not responding. The call ends saying so (on the call and
/// in Recent), and the page keeps the way to fix it.
#[test]
fn a_call_dropped_by_this_pcs_stuck_bluetooth_says_so_and_offers_the_fix() {
    let mut rig = Rig::full();
    rig.populate();
    rig.send(Update::BluetoothNotResponding(true));
    assert!(rig.ui.place_call(10, false));
    rig.send(Update::CallEnded { reason: CallEndReason::PcBluetoothNotResponding });
    rig.ui.pump();
    assert_eq!(rig.ui.call.as_ref().map(|c| c.phase()), Some(Phase::Ended));
    assert_eq!(rig.ui.call_status().as_deref(), Some(CallEndReason::PcBluetoothNotResponding.text()));
    assert_eq!(rig.ui.recent[0].result, CallEndReason::PcBluetoothNotResponding.text(), "Recent says the same");
    assert_ne!(CallEndReason::PcBluetoothNotResponding.text(), CallEndReason::Ended.text(), "not a plain ending");
    rig.ui.close_call();
    rig.click("Open Bluetooth settings");
    assert!(rig.log().contains(&Command::OpenBluetoothSettings));
}

/// WP4: "Who sees my name" shows only while a name is set; the current
/// choice is marked; a pick is saved and sent with the switches; Cancel
/// changes nothing.
#[test]
fn who_sees_my_name_shows_with_a_name_and_the_pick_goes_to_the_switches() {
    use super::settings::{NAMES_AUDIENCE_TITLE, audience_text};
    use meshlive::label_policy::NamesAudience;
    let row = |audience| format!("{NAMES_AUDIENCE_TITLE}, {}", audience_text(audience));
    let selected = |audience| format!("{}, Selected", audience_text(audience));
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = true;
    rig.ui.page = Page::Settings;
    rig.size = egui::vec2(1000.0, 3600.0);
    rig.ui.own_name = None;
    assert!(!rig.has(&row(NamesAudience::Everyone)), "no name: no choice");
    rig.ui.own_name = Some("Asha".into());
    rig.click(&row(NamesAudience::Everyone));
    assert!(rig.has(&selected(NamesAudience::Everyone)), "the current choice is marked");
    rig.click(audience_text(NamesAudience::VerifiedOnly));
    assert_eq!(rig.ui.switches.names_audience, NamesAudience::VerifiedOnly);
    match rig.log().last().unwrap() {
        Command::Switches(s) => assert_eq!(s.names_audience, NamesAudience::VerifiedOnly),
        other => panic!("{other:?}"),
    }
    // Cancel keeps the choice and closes the dialog.
    rig.click(&row(NamesAudience::VerifiedOnly));
    rig.click("Cancel");
    assert_eq!(rig.ui.switches.names_audience, NamesAudience::VerifiedOnly);
    assert!(!rig.has(&selected(NamesAudience::VerifiedOnly)), "the dialog closed");
    // Nobody is offered too.
    rig.click(&row(NamesAudience::VerifiedOnly));
    rig.click(audience_text(NamesAudience::Nobody));
    assert_eq!(rig.ui.switches.names_audience, NamesAudience::Nobody);
}

/// WP6NF finding 7: Settings says when a choice could not be saved, until a
/// save works again.
#[test]
fn settings_say_when_a_choice_could_not_be_saved() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = true;
    rig.ui.page = Page::Settings;
    rig.size = egui::vec2(1000.0, 3600.0);
    assert!(!rig.contains(super::settings::SETTINGS_NOT_SAVED));
    rig.send(Update::SettingsSaved(false));
    rig.ui.pump();
    assert!(rig.contains(super::settings::SETTINGS_NOT_SAVED));
    rig.send(Update::SettingsSaved(true));
    rig.ui.pump();
    assert!(!rig.contains(super::settings::SETTINGS_NOT_SAVED));
}

/// Design decision 2026-11-06: an unreadable block list is said on Privacy too, with
/// Reset; it blocks nobody until it is read again or reset (CF-CALL-24).
#[test]
fn privacy_says_when_the_block_list_cannot_be_read_and_offers_reset() {
    let mut rig = Rig::full();
    rig.ui.switches.onboarded = true;
    rig.ui.page = Page::Settings;
    rig.ui.settings_page = SettingsPage::Privacy;
    rig.size = egui::vec2(1000.0, 3600.0);
    assert!(!rig.contains(super::blocked::UNREADABLE));
    rig.send(Update::Blocked(BlockedView { people: vec![], unreadable: true }));
    rig.ui.pump();
    assert!(rig.contains(super::blocked::UNREADABLE), "said on Privacy");
    rig.click(super::blocked::RESET_ACTION);
    assert!(rig.ui.reset_blocks_confirm, "it asks first");
    rig.click("Reset");
    assert_eq!(rig.log().last(), Some(&Command::ResetBlocked));
}

/// SEC-11: Settings > Privacy says where this PC's identity key is kept once
/// the backend says so, and says nothing about it otherwise.
#[test]
fn privacy_says_where_the_identity_key_is_kept() {
    use super::backend::IdentityKey as KeyProvider;
    let mut rig = Rig::full();
    rig.ui.page = Page::Settings;
    rig.click("Privacy");
    assert!(!rig.contains(super::settings::IDENTITY_KEY_CHIP) && !rig.contains(super::settings::IDENTITY_KEY_WINDOWS));
    rig.send(Update::IdentityKey(Some(KeyProvider::Chip)));
    assert!(rig.contains(super::settings::IDENTITY_KEY_CHIP) && !rig.contains(super::settings::IDENTITY_KEY_WINDOWS));
    rig.send(Update::IdentityKey(Some(KeyProvider::Windows)));
    assert!(rig.contains(super::settings::IDENTITY_KEY_WINDOWS) && !rig.contains(super::settings::IDENTITY_KEY_CHIP));
    // A chip identity the chip can't open: said, with the way out.
    rig.send(Update::IdentityKey(Some(KeyProvider::ChipUnavailable)));
    assert!(rig.contains(super::settings::IDENTITY_KEY_UNAVAILABLE) && !rig.contains(super::settings::IDENTITY_KEY_CHIP));
    rig.send(Update::IdentityKey(None));
    assert!(!rig.contains(super::settings::IDENTITY_KEY_CHIP) && !rig.contains(super::settings::IDENTITY_KEY_WINDOWS)
        && !rig.contains(super::settings::IDENTITY_KEY_UNAVAILABLE));
}
