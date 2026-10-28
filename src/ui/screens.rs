//! The screens. Wording follows the Android strings. Every control is a
//! labelled egui widget (AccessKit) reachable with Tab; shortcuts are noted
//! beside the control they trigger.
use super::*;
use backend::Command;
use meshlive::call::Phase;

/// Shown in a call when Windows is paired with the other phone.
/// Android contacts_forget_message / verify_store_* / home_verified_store_*.
pub(super) const FORGET_MESSAGE: &str = "They will no longer show as verified, and their name is removed from this PC. To trust them again you need to verify them in person.";
pub(super) const RESET_MESSAGE: &str = "Forget every verified person? You will need to verify them again in person.";
const STORE_CORRUPT: &str = "Verified contacts couldn't be loaded. The saved list is damaged or its key was lost, so nobody shows as verified. Calls still work, and the list is kept as it is until you reset it.";
const STORE_TEMPORARY: &str = "Verified contacts couldn't be loaded. Calls still work, but nobody shows as verified for now. Trying again automatically.";

/// The compact form of `BONDED_NOTE` (the rest is under "Learn more").
pub(super) const BONDED_SHORT: &str = "This PC is paired with this phone, so the phone can tell it's this PC.";
pub(super) const NOT_VERIFIED: &str = "Not verified";
pub(super) const NOT_VERIFIED_HELP: &str = "Not verified. A new call from the same phone shows a new name.";
pub(super) const BONDED_NOTE: &str = "This PC is paired with this phone in Bluetooth settings, so the phone can tell it is this PC even in a private call. Remove the pairing in Windows Bluetooth settings to call privately.";

/// A takeover screen's title row: Back and the title (Esc is the screen's).
fn screen_title(ui: &mut egui::Ui, title: &str) -> bool {
    components::top_bar(ui, title, Some("Back"), |_| {})
}

impl Ui {
    // ------------------------------------------------------------- Home

    pub(super) fn home(&mut self, ui: &mut egui::Ui) {
        // Live streams can't run on this PC (old Windows, an adapter that
        // can't advertise, Bluetooth off): the page stays and says why.
        if !self.backend.streams() {
            match self.live_unsupported.as_deref() {
                Some(reason) => components::empty_state(ui, Icon::Broadcast, STREAMS_NOT_HERE, reason),
                None => components::empty_state(ui, Icon::Broadcast, "Live streams", CHECKING_BLUETOOTH),
            }
            return;
        }
        // Live: the streams nearby, with one pinned Go live below the list.
        let go_live_ready = self.usable(Feature::GoLive) && self.golive_can_open();
        let offer_go_live = self.backend.can_go_live();
        egui::ScrollArea::vertical()
            .id_salt("home")
            .max_height((ui.available_height() - if offer_go_live { theme::BUTTON_HEIGHT_LARGE + 20.0 } else { 0.0 }).max(0.0))
            .auto_shrink([false, false])
            .show(ui, |ui| self.home_content(ui));
        if !offer_go_live { return; }
        ui.add_space(16.0);
        let running = self.golive.running();
        let response = home_go_live(running)
            .fill_width()
            .large()
            .enabled(go_live_ready)
            .a11y(if running { "Go live: back to your broadcast" } else { "Go live" })
            .show(ui);
        if response.clicked() {
            self.open_go_live();
        }
    }
    fn golive_can_open(&self) -> bool {
        true
    }

    fn home_content(&mut self, ui: &mut egui::Ui) {
        self.gate_notes(ui, &[Feature::Watch, Feature::GoLive]);
        if !self.usable(Feature::Watch) {
            return;
        }
        if self.streams.is_empty() {
            self.empty_streams(ui);
            return;
        }
        // Streams this PC can watch come first; the ones it can't wait behind
        // one plain line (never a wall of greyed-out rows).
        let (joinable, blocked): (Vec<_>, Vec<_>) = self.streams.iter().cloned().partition(|s| s.unavailable.is_none());
        let shown = |s: &StreamEntry| s.unavailable.is_none() || self.show_unwatchable;
        let video: Vec<_> = self.streams.iter().filter(|s| !s.audio_only && shown(s)).cloned().collect();
        let radio: Vec<_> = self.streams.iter().filter(|s| s.audio_only && shown(s)).cloned().collect();
        if joinable.is_empty() && !self.show_unwatchable {
            components::empty_state(ui, Icon::Broadcast, "No streams you can watch yet", &unwatchable_line(&blocked));
        }
        let mut toggle = false;
        if !blocked.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if !joinable.is_empty() || self.show_unwatchable {
                    ui.label(theme::caption(unwatchable_line(&blocked)));
                }
                let label = if self.show_unwatchable { "Hide them" } else { "Show them" };
                toggle = Pill::new(label).kind(Kind::Quiet).small().a11y(format!("{label}: streams this PC can't watch")).show(ui).clicked();
            });
            ui.add_space(8.0);
        }
        if toggle {
            self.show_unwatchable = !self.show_unwatchable;
        }
        // A wide window shows the streams as a grid of cards.
        if self.split_panes && ui.available_width() >= GRID_FROM {
            for (heading, group) in [(None, &video), (Some("Radio"), &radio)] {
                if group.is_empty() {
                    continue;
                }
                if let Some(heading) = heading {
                    components::section_header(ui, heading);
                }
                if let Some(id) = self.stream_grid(ui, group) {
                    self.start_watch(id);
                }
                ui.add_space(theme::SECTION_GAP);
            }
            return;
        }
        for (index, entry) in video.iter().enumerate() {
            if self.stream_row(ui, entry, index + 1 == video.len()) {
                self.start_watch(entry.id);
            }
        }
        if !radio.is_empty() {
            if !video.is_empty() {
                ui.add_space(theme::SECTION_GAP);
            }
            components::section_header(ui, "Radio");
            for (index, entry) in radio.iter().enumerate() {
                if self.stream_row(ui, entry, index + 1 == radio.len()) {
                    self.start_watch(entry.id);
                }
            }
        }
    }
    fn empty_streams(&mut self, ui: &mut egui::Ui) {
        self.show_unwatchable = false;
        let text = if !self.backend.connected() {
            "Searching isn't connected in this build."
        } else if self.searching {
            "Looking\u{2026}"
        } else {
            "When someone nearby goes live, they appear here."
        };
        components::empty_state(ui, Icon::Broadcast, "No one is live nearby", text);
        ui.vertical_centered(|ui| {
            if Pill::new("Search again").kind(Kind::Secondary).show(ui).clicked() {
                self.searching = true;
                self.backend.command(Command::Rescan);
            }
        });
    }
    /// Streams as cards in as many columns as fit (wide windows).
    fn stream_grid(&self, ui: &mut egui::Ui, entries: &[StreamEntry]) -> Option<Handle> {
        let gap = 16.0;
        let width = ui.available_width();
        let columns = ((width + gap) / (CARD_MIN + gap)).floor().max(1.0) as usize;
        let card = (width - gap * (columns as f32 - 1.0)) / columns as f32;
        let mut chosen = None;
        for row in entries.chunks(columns) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for entry in row {
                    if self.stream_card(ui, entry, card) {
                        chosen = Some(entry.id);
                    }
                }
            });
            ui.add_space(gap);
        }
        chosen
    }
    /// One stream card: the live (or radio) mark, the title and who is live.
    fn stream_card(&self, ui: &mut egui::Ui, entry: &StreamEntry, width: f32) -> bool {
        let (headline, subtitle) = (entry.headline(), entry.subtitle());
        let enabled = !entry.full && entry.unavailable.is_none();
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 120.0), egui::Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, format!("{headline}, {subtitle}")));
        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            let fill = if enabled && response.hovered() { theme::RAISED } else { theme::SURFACE };
            painter.rect_filled(rect, 16.0, fill);
            let mark = egui::Rect::from_min_size(rect.min + egui::vec2(16.0, 16.0), egui::vec2(24.0, 24.0));
            let (icon, tint) = if entry.audio_only { (Icon::Radio, theme::TEXT_SECONDARY) } else { (Icon::Broadcast, theme::LIVE) };
            draw_icon(painter, mark, icon, tint);
            let ink = if enabled { theme::TEXT } else { theme::TEXT_DISABLED };
            let title = components::single_line(ui, &headline, theme::Type::RowTitle.font(), ink, width - 32.0);
            let sub = components::single_line(ui, &subtitle, theme::Type::Subtitle.font(), theme::TEXT_SECONDARY, width - 32.0);
            painter.galley(egui::pos2(rect.left() + 16.0, rect.top() + 56.0), title, ink);
            painter.galley(egui::pos2(rect.left() + 16.0, rect.top() + 84.0), sub, theme::TEXT_SECONDARY);
        }
        enabled && response.clicked()
    }
    /// A stream: no thumbnail (nothing can be previewed before joining), the
    /// red live mark for video, the radio mark for audio only.
    fn stream_row(&self, ui: &mut egui::Ui, entry: &StreamEntry, last: bool) -> bool {
        let (headline, subtitle) = (entry.headline(), entry.subtitle());
        ui.add_enabled_ui(!entry.full && entry.unavailable.is_none(), |ui| {
            ListRow::new(&headline)
                .subtitle(&subtitle)
                .leading(if entry.audio_only { Leading::Icon(Icon::Radio) } else { Leading::Live })
                .chevron()
                .divider(!last)
                .show(ui)
                .clicked()
        })
        .inner
    }
    /// One note per distinct reason, naming the features it blocks.
    fn gate_notes(&self, ui: &mut egui::Ui, features: &[Feature]) {
        let mut groups: Vec<(&'static str, Vec<&'static str>)> = vec![];
        for &feature in features {
            if let Some(reason) = self.unavailable_reason(feature) {
                match groups.iter_mut().find(|(r, _)| *r == reason) {
                    Some((_, names)) => names.push(feature.label()),
                    None => groups.push((reason, vec![feature.label()])),
                }
            }
        }
        for (reason, names) in groups {
            ui.label(theme::strong(names.join(", ")));
            components::notice(ui, &format!("Unavailable. {reason}"), None);
            ui.add_space(16.0);
        }
    }

    // ------------------------------------------------------------ Watch

    pub(super) fn watch_screen(&mut self, ui: &mut egui::Ui) {
        if let Some(note) = self.watch_note.as_ref() { components::notice(ui, note, None); }
        let Some(watch) = self.watch.as_ref() else {
            return;
        };
        let (headline, radio) = (watch.stream.headline(), watch.stream.audio_only);
        let (state, status, route) = (watch.state, watch.status(), watch.route_label());
        let (muted, paused) = (watch.muted, watch.paused);
        let info = watch.stream.subtitle();
        let playing = state == WatchState::Playing && !paused;
        // Top bar: back, title and the red live badge while it plays.
        let mut leave = components::top_bar(ui, &headline, Some("Back"), |ui| {
            let kind = if radio { "Radio" } else { "Live stream" };
            if playing {
                live_badge(ui, kind);
            } else {
                ui.label(theme::secondary(kind));
            }
        });
        let mut toggle_mute = false;
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            leave = true;
        }
        ui.add_space(8.0);
        let bar = theme::ROUND_CONTROL + 40.0;
        if radio {
            // Audio only: the radio mark, its state and where it comes from.
            ui.add_space(((ui.available_height() - bar - 240.0) * 0.3).max(12.0));
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 120.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 60.0, theme::RAISED);
                draw_icon(ui.painter(), egui::Rect::from_center_size(rect.center(), egui::vec2(48.0, 48.0)), Icon::Radio, theme::TEXT);
                ui.add_space(16.0);
                ui.label(theme::headline(&status));
                ui.label(theme::secondary(&route));
                ui.label(theme::caption(&info));
                ui.add_space(8.0);
                ui.set_max_width(520.0);
                ui.label(theme::caption(
                    "Anyone nearby with MeshLive can listen, including phones that pass it on. Titles from people you have not verified are unconfirmed. Volume is your PC's volume.",
                ));
            });
        } else {
            let height = (ui.available_height() - bar - 52.0).max(120.0);
            let texture = if state != WatchState::Playing || watch.source_state.camera_off { None } else { self.video };
            // The same video area as a call: upright, never stretched to 16:9.
            video_stage(ui, egui::vec2(ui.available_width(), height), "Live video", &status, texture.map(|id| (id, self.peer_size)));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(theme::body(&status));
                ui.label(theme::secondary(&route));
            });
        }
        ui.add_space((ui.available_height() - bar).max(12.0));
        let live = state != WatchState::Ended;
        let count = if live { 2.0 } else { 1.0 };
        centered_row(ui, count * theme::ROUND_CONTROL + (count - 1.0) * 28.0, |ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            let (label, kind, icon) = if muted { ("Unmute", Kind::Primary, Icon::MicOff) } else { ("Mute", Kind::Secondary, Icon::Speaker) };
            let r = ui.add_enabled_ui(live, |ui| components::round_button(ui, icon, label, kind)).inner;
            if self.focus_primary {
                r.request_focus();
                self.focus_primary = false;
            }
            toggle_mute |= r.clicked();
            if live {
                leave |= components::round_button(ui, Icon::Stop, "Stop", Kind::Live).clicked();
            }
        });
        if toggle_mute {
            if let Some(w) = self.watch.as_mut() {
                w.toggle_mute();
                let muted = w.muted;
                self.backend.command(Command::MuteWatch(muted));
            }
        }
        if leave {
            self.leave_watch();
        }
    }

    // ------------------------------------------------------------ Calls

    pub(super) fn calls(&mut self, ui: &mut egui::Ui) {
        if self.split_panes && ui.available_width() >= theme::WIDE_SPLIT {
            let list = components::split_width(ui.available_width());
            components::split(ui, list, |left, right| {
                self.calls_list(left, true);
                self.calls_detail(right);
            });
        } else {
            self.calls_list(ui, false);
        }
    }
    /// The selected person (wide window): who they are and the two calls.
    fn calls_detail(&mut self, ui: &mut egui::Ui) {
        let person = self.selected_person.and_then(|id| self.people.iter().find(|p| p.id == id).cloned());
        let Some(person) = person else {
            components::empty_state(ui, Icon::Phone, "Choose someone to call", "People nearby appear in the list.");
            return;
        };
        let can_call = self.usable(Feature::CallOut);
        if let Some(voice) = self.person_detail(ui, &person.name, &person.subtitle(), person.verified, can_call) {
            if person.copied {
                self.copied_confirm = Some((person.id, voice));
            } else {
                self.place_call(person.id, voice);
            }
        }
        if person.copied {
            ui.add_space(12.0);
            components::notice(ui, "More than one phone is using this ID", None);
        }
    }
    /// A person's detail pane: avatar, name, details and the two calls.
    fn person_detail(&self, ui: &mut egui::Ui, name: &str, details: &str, verified: bool, enabled: bool) -> Option<bool> {
        let mut result = None;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            components::avatar(ui, if verified { name } else { "" }, 72.0);
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.add_space(8.0);
                ui.label(theme::title(name));
                ui.label(theme::secondary(details));
            });
        });
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            if Pill::new("Video call").icon(Icon::Camera).min_width(180.0).enabled(enabled).a11y(format!("Video call {name}")).show(ui).clicked() {
                result = Some(false);
            }
            let voice_ok = enabled && self.switches.voice_calls;
            if Pill::new("Voice call").kind(Kind::Secondary).icon(Icon::Phone).min_width(180.0).enabled(voice_ok).a11y(format!("Voice call {name}")).show(ui).clicked() {
                result = Some(true);
            }
        });
        result
    }
    fn calls_list(&mut self, ui: &mut egui::Ui, wide: bool) {
        let mut tab = usize::from(self.recent_tab);
        if components::segmented(ui, &["Nearby", "Recent"], &mut tab) {
            self.recent_tab = tab == 1;
        }
        ui.add_space(16.0);
        if self.recent_tab {
            self.recent_calls(ui);
            return;
        }
        if let Some(reason) = self.unavailable_reason(Feature::CallOut) {
            components::notice(ui, &format!("Unavailable. {reason}"), None);
            ui.add_space(8.0);
        }
        // Whether phones can ring this PC (and why not) and the Reachable
        // switch live in the status indicator's popover (top right), as on
        // Android; Calls > Nearby keeps one quiet line with the made-up name
        // strangers see (Android OwnNearbyName), so two people can match up.
        if let Some(name) = self.appear_as().map(str::to_owned) {
            components::line_with_info(ui, "appear-as-calls", &format!("You appear as {name} \u{b7} changes every 15 min"), "About your generated name", crate::ui::APPEAR_AS_INFO);
            ui.add_space(8.0);
        }
        let people = self.people.clone();
        if people.is_empty() {
            let note = self.bluetooth_note.clone();
            let (title, text) = if let Some(note) = note.as_deref() {
                ("Bluetooth is off", note)
            } else if !self.backend.connected() {
                ("No one found yet", "Searching isn't connected in this build.")
            } else {
                ("No one found yet", crate::ui::FIND_PCS_HINT)
            };
            empty(ui, Icon::Phone, title, text);
            ui.add_space(12.0);
            if note.is_some() {
                ui.vertical_centered(|ui| self.bluetooth_settings_button(ui));
                ui.add_space(8.0);
            }
            ui.vertical_centered(|ui| {
                if Pill::new("Search again").kind(Kind::Secondary).show(ui).clicked() {
                    self.searching = true;
                    self.backend.command(Command::Rescan);
                }
            });
            return;
        }
        let can_call = self.usable(Feature::CallOut);
        for (heading, verified) in [("Contacts", true), ("Nearby", false)] {
            let group: Vec<_> = people.iter().filter(|p| p.verified == verified).collect();
            if group.is_empty() {
                continue;
            }
            components::section_header(ui, heading);
            let count = group.len();
            for (index, person) in group.into_iter().enumerate() {
                let (call, picked, _) = self.person_card(ui, person.id, &person.name, &person.subtitle(), can_call, verified, index + 1 == count, wide, None);
                if picked {
                    self.selected_person = Some(person.id);
                }
                if let Some(voice) = call {
                    // CF-PRIV-22: a copied ID asks for a second tap first.
                    if person.copied {
                        self.copied_confirm = Some((person.id, voice));
                    } else {
                        self.place_call(person.id, voice);
                    }
                }
            }
            ui.add_space(theme::SECTION_GAP);
        }
    }
    /// A person row: avatar, name (+ verified), details and the two round
    /// call buttons (voice grey, video white). Returns Some(voice) when pressed.
    /// In a wide window the row also selects the person (second value).
    #[allow(clippy::too_many_arguments)]
    /// A person's row with the two calls; `more` (Some(open)) adds a quiet
    /// "More" toggle for a contact's other actions, returned as the third value.
    #[allow(clippy::too_many_arguments)]
    fn person_card(&self, ui: &mut egui::Ui, _id: Handle, name: &str, details: &str, enabled: bool, saved: bool, last: bool, selectable: bool, more: Option<bool>) -> (Option<bool>, bool, bool) {
        let voice_ok = enabled && self.switches.voice_calls;
        let mut toggled = false;
        let (row, call) = ListRow::new(name)
            .subtitle(details)
            .leading(Leading::Avatar(if saved { name.to_owned() } else { String::new() }))
            .verified(saved)
            .clickable(selectable)
            .divider(!last)
            .show_with(ui, |ui| {
                let mut result = None;
                // Right to left: video (white), then voice (grey).
                let video = ui.add_enabled_ui(enabled, |ui| components::icon_button_sized(ui, Icon::Camera, &format!("Video call {name}"), Kind::Primary, theme::ICON_BUTTON)).inner;
                if video.clicked() {
                    result = Some(false);
                }
                let voice = ui.add_enabled_ui(voice_ok, |ui| components::icon_button_sized(ui, Icon::Phone, &format!("Voice call {name}"), Kind::Secondary, theme::ICON_BUTTON)).inner;
                if voice.clicked() {
                    result = Some(true);
                }
                if let Some(open) = more {
                    let icon = if open { Icon::ChevronDown } else { Icon::Chevron };
                    toggled = components::icon_button_sized(ui, icon, &format!("More for {name}"), Kind::Quiet, theme::ICON_BUTTON).clicked();
                }
                result
            });
        (call, selectable && row.clicked(), toggled)
    }
    fn recent_calls(&mut self, ui: &mut egui::Ui) {
        if self.recent.is_empty() {
            components::empty_state(ui, Icon::Phone, "No recent calls", "Your calls will appear here.");
            return;
        }
        let recent = self.recent.clone();
        let can_call = self.usable(Feature::CallOut);
        let mut back = None;
        let mut block = None;
        for (index, call) in recent.iter().enumerate() {
            let kind = if call.voice { "Voice call" } else { "Video call" };
            let direction = if call.outgoing { "Outgoing" } else { "Incoming" };
            let contact = call.contact.filter(|c| self.contacts.iter().any(|k| k.id == *c && k.lease));
            // Callers that presented their real key can be blocked here (CF-CALL-24).
            let blockable = call.fingerprint.filter(|_| !call.outgoing);
            let subtitle = format!("{kind} \u{b7} {direction} \u{b7} {}", call.result);
            let last = index + 1 == recent.len();
            ListRow::new(&call.peer)
                .subtitle(&subtitle)
                .icon(if call.voice { Icon::Phone } else { Icon::Camera })
                .clickable(false)
                .divider(!last && !call.blocked)
                .show_with(ui, |ui| {
                    if let Some(contact) = contact {
                        for (label, voice, icon, kind) in [
                            ("Call back (video)", false, Icon::Camera, Kind::Primary),
                            ("Call back (voice)", true, Icon::Phone, Kind::Secondary),
                        ] {
                            let ok = can_call && (!voice || self.switches.voice_calls);
                            let name = format!("{label} {}", call.peer);
                            let b = ui.add_enabled_ui(ok, |ui| components::icon_button_sized(ui, icon, &name, kind, theme::ICON_BUTTON)).inner;
                            if b.clicked() {
                                back = Some((contact, voice));
                            }
                        }
                    }
                    if let Some(fingerprint) = blockable {
                        let label = if call.blocked { "Unblock" } else { "Block" };
                        if text_action(ui, label, &format!("{label} {}", call.peer)).clicked() {
                            block = Some((fingerprint, !call.blocked));
                        }
                    }
                });
            if call.blocked {
                ui.horizontal(|ui| {
                    ui.add_space(theme::ROW_TEXT_INSET);
                    ui.label(theme::caption("Blocked: their calls are declined without ringing."));
                });
                ui.add_space(6.0);
                if !last {
                    divider(ui, theme::ROW_TEXT_INSET);
                }
            }
        }
        if let Some((contact, voice)) = back {
            self.place_call(contact, voice);
        }
        if let Some((fingerprint, blocked)) = block {
            self.backend.command(Command::BlockCaller { fingerprint, blocked });
            for call in self.recent.iter_mut().filter(|c| c.fingerprint == Some(fingerprint)) {
                call.blocked = blocked;
            }
        }
    }

    /// The call takes over the window: ring screen, in-call controls, summary.
    /// A field-tool layout: name and state at the top, the picture (or the
    /// person's mark) in the middle, round controls in one bottom bar.
    pub(super) fn call_screen(&mut self, ui: &mut egui::Ui) {
        let Some(call) = self.call.as_ref() else {
            return;
        };
        let phase = call.phase();
        // The end reason ("No answer", "Missed call", "Declined", "Busy") once it ended.
        let status = self.call_status().unwrap_or_else(|| call.status());
        let peer = call.peer.clone();
        let verified = call.verified;
        let (muted, talking, camera, voice) = (call.muted, call.talking, call.camera, call.voice);
        let mic_state = if self.call_mic_restarting { None } else { self.call_mic.clone() };
        let camera_state = self.call_camera.clone();
        let peer_camera = call.peer_camera;
        let outgoing = call.outgoing;
        let bonded = call.bonded;
        let focus = std::mem::take(&mut self.focus_primary);

        // The call clock runs from the moment the call is active.
        let since_id = egui::Id::new("call-active-since");
        let elapsed = if phase == Phase::Active {
            let since = ui.ctx().data_mut(|d| *d.get_temp_mut_or_insert_with(since_id, std::time::Instant::now));
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            Some(since.elapsed().as_secs())
        } else {
            if phase != Phase::Ended {
                ui.ctx().data_mut(|d| d.remove::<std::time::Instant>(since_id));
            }
            None
        };
        let active = matches!(phase, Phase::Active | Phase::NegotiatingMedia);
        let video = active && (peer_camera || camera);
        let ringing_out = matches!(phase, Phase::OutgoingInvite | Phase::OutgoingRinging);
        let name = peer.clone();
        // Android shows an unverified caller's name with a small quiet tag.
        let unverified = !verified && !outgoing;
        let encrypted = phase == Phase::Active;
        let bar_height = match phase {
            Phase::IncomingRinging => 112.0,
            Phase::Ended => 64.0,
            _ => 100.0,
        };

        // ---- name, state and the plain facts about this call (Android's call header)
        let header = |ui: &mut egui::Ui, centred: bool| {
            let layout = if centred { egui::Layout::top_down(egui::Align::Center) } else { egui::Layout::top_down(egui::Align::Min) };
            ui.with_layout(layout, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let title = ui.label(
                    RichText::new(&name)
                        .size(if centred { 30.0 } else { 22.0 })
                        .family(egui::FontFamily::Name("Heading".into()))
                        .color(Color32::WHITE),
                );
                if verified {
                    verified_mark(ui.painter(), egui::pos2(title.rect.right() + 16.0, title.rect.center().y), 18.0);
                } else if unverified {
                    // A small, quiet tag (the details are one hover away).
                    let font = egui::FontId::proportional(12.0);
                    let galley = ui.painter().layout_no_wrap(NOT_VERIFIED.to_owned(), font, SECONDARY);
                    let tag = egui::Rect::from_min_size(
                        egui::pos2(title.rect.right() + 10.0, title.rect.center().y - galley.size().y * 0.5 - 3.0),
                        galley.size() + egui::vec2(14.0, 6.0),
                    );
                    if !centred {
                        ui.painter().rect_stroke(tag, 8, egui::Stroke::new(1.0, Color32::from_gray(70)), egui::StrokeKind::Inside);
                        ui.painter().galley(tag.min + egui::vec2(7.0, 3.0), galley, SECONDARY);
                        ui.interact(tag, ui.id().with("not-verified"), egui::Sense::hover())
                            .on_hover_text(NOT_VERIFIED_HELP)
                            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, NOT_VERIFIED));
                    }
                }
                if centred && unverified {
                    ui.label(RichText::new(NOT_VERIFIED).size(13.0).color(SECONDARY));
                }
                ui.horizontal(|ui| {
                    let state_width = ui.painter().layout_no_wrap(status.clone(), egui::FontId::proportional(17.0), SECONDARY).size().x
                        + elapsed.map_or(0.0, |_| 64.0)
                        + if encrypted { 22.0 } else { 0.0 };
                    if centred {
                        // Centre the state line under the name.
                        ui.add_space(((ui.available_width() - state_width) / 2.0).max(0.0));
                    }
                    ui.label(RichText::new(&status).size(17.0).color(SECONDARY));
                    if let Some(seconds) = elapsed {
                        ui.label(RichText::new(clock(seconds)).size(17.0).color(Color32::WHITE).monospace());
                    }
                    if encrypted {
                        // A small lock: end-to-end encrypted.
                        let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), egui::Sense::hover());
                        draw_icon(ui.painter(), rect, Icon::Lock, SECONDARY);
                        response
                            .on_hover_text(if verified { "Encrypted, direct" } else { "Encrypted" })
                            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, if verified { "Encrypted, direct" } else { "Encrypted" }));
                    }
                });
            });
        };
        // A paired phone: one compact line, the whole explanation on "Learn more".
        let facts = |ui: &mut egui::Ui, centred: bool| {
            if !bonded {
                return;
            }
            let more_id = egui::Id::new("call-bonded-more");
            let mut more = ui.data(|d| d.get_temp::<bool>(more_id).unwrap_or(false));
            let layout = if centred { egui::Layout::top_down(egui::Align::Center) } else { egui::Layout::top_down(egui::Align::Min) };
            ui.with_layout(layout, |ui| {
                ui.set_max_width(ui.available_width().min(640.0));
                ui.horizontal_wrapped(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), egui::Sense::hover());
                    draw_icon(ui.painter(), rect, Icon::Info, SECONDARY);
                    ui.label(RichText::new(BONDED_SHORT).size(13.0).color(SECONDARY));
                    if Pill::new(if more { "Less" } else { "Learn more" }).kind(Kind::Quiet).small().show(ui).clicked() {
                        more = !more;
                    }
                });
                if more {
                    ui.add(egui::Label::new(RichText::new(BONDED_NOTE).size(13.0).color(SECONDARY)).wrap());
                }
            });
            ui.data_mut(|d| d.insert_temp(more_id, more));
        };

        let mut open_settings = false;
        let mut open_camera_settings = false;
        let problems = |ui: &mut egui::Ui, open_settings: &mut bool, open_camera_settings: &mut bool| {
            if !(active || ringing_out) {
                return;
            }
            // The microphone or camera can't be used: why, and the way to fix it.
            if let Some(Some(reason)) = &mic_state {
                *open_settings = components::notice(ui, reason, Some("Open microphone settings"));
            }
            if let Some(Some(reason)) = &camera_state {
                *open_camera_settings = components::notice(ui, reason, Some("Open camera settings"));
            }
        };

        // The control bar's place is reserved first, so it is on screen in any
        // window size; the name, facts and picture share the space above it
        // (clipped there, never pushing the controls out of the window). In a
        // video call the picture runs to the bottom and the controls float
        // over it, as on Android.
        let full = ui.available_rect_before_wrap();
        let reserve = if video { 0.0 } else { bar_height };
        let top_rect = egui::Rect::from_min_max(full.min, egui::pos2(full.right(), (full.bottom() - reserve).max(full.top())));
        let mut top = ui.new_child(egui::UiBuilder::new().max_rect(top_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        top.set_clip_rect(top_rect.intersect(ui.clip_rect()));
        {
            let ui = &mut top;
            if video {
                // ---- video: a compact header, the picture as the hero, your camera in the corner
                let media: Vec<String> = self.call_media_lines().into_iter().map(str::to_owned).collect();
                let mut toggle_details = false;
                let details_open = self.call_details_open;
                let has_details = media.iter().any(|l| !plain_media_line(l));
                ui.horizontal(|ui| {
                    // One line, so the picture gets the height: name, a quiet
                    // tag, state, timer and lock; Details at the end.
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        RichText::new(&name)
                            .size(20.0)
                            .family(egui::FontFamily::Name("Heading".into()))
                            .color(Color32::WHITE),
                    );
                    if verified {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                        verified_mark(ui.painter(), rect.center(), 18.0);
                    } else if unverified {
                        components::tag(ui, NOT_VERIFIED).on_hover_text(NOT_VERIFIED_HELP);
                    }
                    ui.label(RichText::new(&status).size(15.0).color(SECONDARY));
                    if let Some(seconds) = elapsed {
                        ui.label(RichText::new(clock(seconds)).size(15.0).color(Color32::WHITE).monospace());
                    }
                    if encrypted {
                        let label = if verified { "Encrypted, direct" } else { "Encrypted" };
                        let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), egui::Sense::hover());
                        draw_icon(ui.painter(), rect, Icon::Lock, SECONDARY);
                        response.on_hover_text(label).widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
                    }
                    if has_details {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            toggle_details = Pill::new(if details_open { "Hide details" } else { "Details" })
                                .kind(Kind::Quiet)
                                .small()
                                .show(ui)
                                .clicked();
                        });
                    }
                });
                if toggle_details {
                    self.call_details_open = !self.call_details_open;
                }
                facts(ui, false);
                problems(ui, &mut open_settings, &mut open_camera_settings);
                // CF-OUT-07: a fallback is always said; the codec and encoder in use only under Details.
                self.call_detail_lines(ui, media);
                ui.add_space(8.0);
                let height = ui.available_height().max(48.0);
                let texture = if phase == Phase::Active && peer_camera { self.video } else { None };
                let area = video_stage(
                    ui,
                    egui::vec2(ui.available_width(), height),
                    "Other person's video",
                    if peer_camera { "Connecting\u{2026}" } else { "Video off" },
                    texture.map(|id| (id, self.peer_size)),
                );
                // The controls float over the bottom of the picture: a soft shade keeps them readable.
                if texture.is_some() {
                    // A soft shade from clear to dark keeps the floating controls readable.
                    let top = (area.bottom() - bar_height - 40.0).max(area.top());
                    let mut shade = egui::Mesh::default();
                    let (clear, dark) = (Color32::TRANSPARENT, Color32::from_black_alpha(150));
                    shade.colored_vertex(egui::pos2(area.left(), top), clear);
                    shade.colored_vertex(egui::pos2(area.right(), top), clear);
                    shade.colored_vertex(egui::pos2(area.right(), area.bottom()), dark);
                    shade.colored_vertex(egui::pos2(area.left(), area.bottom()), dark);
                    shade.add_triangle(0, 1, 2);
                    shade.add_triangle(0, 2, 3);
                    ui.painter().add(egui::Shape::mesh(shade));
                }
                // Your own camera rests bottom right above the controls (like
                // Android's picture-in-picture); with the camera off, a clear tile says so.
                let own = if camera && matches!(camera_state, Some(None)) {
                    SelfView::Picture(self.self_video())
                } else {
                    SelfView::Off
                };
                let pip_area = egui::Rect::from_min_max(area.min, egui::pos2(area.right(), (area.bottom() - bar_height).max(area.top())));
                self_view_tile(ui, pip_area, own);
            } else {
                // ---- no picture: the person's mark, name and state, centred
                let free = ui.available_height();
                ui.add_space((free * 0.16).max(12.0));
                ui.vertical_centered(|ui| {
                    let rect = components::avatar(ui, if verified || outgoing { &peer } else { "" }, 120.0).rect;
                    if phase == Phase::IncomingRinging {
                        // A quiet ring around the mark while it rings.
                        ui.painter().circle_stroke(rect.center(), 70.0, egui::Stroke::new(1.5, Color32::from_gray(60)));
                    }
                });
                ui.add_space(24.0);
                header(ui, true);
                ui.add_space(6.0);
                facts(ui, true);
                if phase == Phase::IncomingRinging {
                    if let Some(name) = self.answer_as.clone() {
                        ui.vertical_centered(|ui| ui.label(RichText::new(format!("You appear as {name}")).size(14.0).color(SECONDARY)));
                    }
                }
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(560.0);
                    problems(ui, &mut open_settings, &mut open_camera_settings);
                    // The shared header also exists on voice calls; it does not prove video is flowing.
                    let media: Vec<String> = self.call_media_lines().into_iter().map(str::to_owned).collect();
                    self.call_details(ui, media);
                });
            }
        }
        if open_settings {
            open_microphone_settings();
        }
        if open_camera_settings {
            open_settings_page("ms-settings:privacy-webcam");
        }
        // ---- the bottom control bar
        ui.add_space((ui.available_height() - bar_height).max(12.0));
        match phase {
            Phase::IncomingRinging => {
                let mut accept = false;
                let mut decline = false;
                centered_row(ui, theme::ROUND_CONTROL_LARGE * 2.0 + 120.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 120.0;
                    let large = theme::ROUND_CONTROL_LARGE;
                    decline = components::round_button_sized(ui, Icon::HangUp, "Decline", Kind::Destructive, large).clicked();
                    let a = components::round_button_sized(ui, if voice { Icon::Phone } else { Icon::Camera }, "Accept", Kind::Positive, large);
                    if focus {
                        a.request_focus();
                    }
                    accept = a.on_hover_text(if voice { "Accept voice call" } else { "Accept video call" }).clicked();
                });
                if accept {
                    self.with_call(|c| {
                        let _ = c.accept(voice);
                    });
                }
                if decline {
                    self.with_call(|c| {
                        let _ = c.decline();
                    });
                }
            }
            Phase::OutgoingInvite | Phase::OutgoingRinging | Phase::NegotiatingMedia | Phase::Active => {
                let mut mute = false;
                let mut press = false;
                let mut cam = false;
                let mut end = false;
                let count = if ringing_out { 1.0 } else if muted { 4.0 } else { 3.0 };
                let size = theme::ROUND_CONTROL;
                centered_row(ui, count * size + (count - 1.0) * 20.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 20.0;
                    if !ringing_out {
                        // Kind::Primary (white) shows a control that is on.
                        let (label, kind, icon) =
                            if muted { ("Unmute", Kind::Primary, Icon::MicOff) } else { ("Mute", Kind::Secondary, Icon::Mic) };
                        mute = components::round_button_sized(ui, icon, label, kind, size).on_hover_text(format!("{label} (Ctrl+M)")).clicked();
                        if muted {
                            let talk = components::round_button_sized(ui, Icon::Mic, "Talk", if talking { Kind::Primary } else { Kind::Secondary }, size);
                            let talk = if self.hold_call_talk { talk.interact(egui::Sense::click_and_drag()) } else { talk };
                            #[cfg(test)]
                            if let Some(check) = self.native_check.as_mut() { check.talk_position(talk.id, talk.rect.center()); }
                            press = talk.is_pointer_button_down_on() || (talk.has_focus() && ui.input(|i| i.key_down(egui::Key::Space)));
                            if self.hold_call_talk {
                                press &= ui.input(|i| i.focused) && (talk.contains_pointer() || (talk.has_focus() && ui.input(|i| i.key_down(egui::Key::Space))));
                            }
                            talk.on_hover_text("Hold to talk. Release to mute again.");
                        }
                        let video_label = if camera {
                            "Video off"
                        } else if voice || !peer_camera {
                            "Add video"
                        } else {
                            "Video on"
                        };
                        let (kind, icon) = if camera { (Kind::Primary, Icon::Camera) } else { (Kind::Secondary, Icon::VideoOff) };
                        cam = components::round_button_sized(ui, icon, video_label, kind, size).on_hover_text(video_label).clicked();
                    }
                    let name = if ringing_out { "Cancel" } else { "End call" };
                    let e = components::round_button_sized(ui, Icon::HangUp, name, Kind::Live, size);
                    if focus {
                        e.request_focus();
                    }
                    end = e.on_hover_text(format!("{name} (Ctrl+E)")).clicked();
                });
                let (k_mute, k_end) = ui.input_mut(|i| {
                    (
                        i.consume_key(egui::Modifiers::CTRL, egui::Key::M),
                        i.consume_key(egui::Modifiers::CTRL, egui::Key::E),
                    )
                });
                let (mute, end) = (mute || (k_mute && !ringing_out), end || k_end);
                if mute {
                    self.with_call(|c| {
                        let _ = c.toggle_mute();
                    });
                }
                if cam {
                    self.with_call(|c| {
                        let _ = c.toggle_camera();
                    });
                }
                if muted && press != talking {
                    self.with_call(|c| {
                        let _ = c.talk(press);
                    });
                }
                if end {
                    self.with_call(|c| {
                        let _ = c.end();
                    });
                }
            }
            Phase::Ended => {
                let mut close = false;
                ui.vertical_centered(|ui| {
                    let r = Pill::new("Back").kind(Kind::Secondary).icon(Icon::Back).min_width(220.0).large().show(ui);
                    if focus {
                        r.request_focus();
                    }
                    close = r.clicked();
                });
                if close || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.close_call();
                }
            }
        }
    }

    // --------------------------------------------------------- Contacts

    pub(super) fn contacts(&mut self, ui: &mut egui::Ui) {
        if self.split_panes && ui.available_width() >= theme::WIDE_SPLIT && !self.contacts.is_empty() {
            let list = components::split_width(ui.available_width());
            components::split(ui, list, |left, right| {
                self.contacts_list(left, true);
                self.contacts_detail(right);
            });
        } else {
            self.contacts_list(ui, false);
        }
    }
    /// The selected contact (wide window): calls, then the quiet actions.
    fn contacts_detail(&mut self, ui: &mut egui::Ui) {
        let contact = self.selected_contact.and_then(|id| self.contacts.iter().find(|c| c.id == id).cloned());
        let Some(contact) = contact else {
            components::empty_state(ui, Icon::People, "Choose a contact", "Your contacts appear in the list.");
            return;
        };
        let enabled = self.usable(Feature::CallOut) && contact.lease;
        if let Some(voice) = self.person_detail(ui, &contact.name, &contact.subtitle(), true, enabled) {
            self.place_call(contact.id, voice);
        }
        ui.add_space(16.0);
        self.contact_extras(ui, &contact, 0.0, true);
    }
    fn contacts_list(&mut self, ui: &mut egui::Ui, wide: bool) {
        // "Add contact" is pinned at the bottom by the shell (Ui::page_footer).
        if let Some(reason) = self.unavailable_reason(Feature::CallOut) {
            components::notice(ui, &format!("Unavailable. {reason}"), None);
            ui.add_space(12.0);
        }
        match self.contacts_problem {
            Some(StoreProblem::Corrupt) => {
                components::notice(ui, &format!("Unavailable. {STORE_CORRUPT}"), None);
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    if text_action(ui, "Reset verified people", "Reset verified people").clicked() {
                        self.reset_contacts = true;
                    }
                });
                ui.add_space(12.0);
            }
            Some(StoreProblem::Temporary) => {
                components::notice(ui, &format!("Unavailable. {STORE_TEMPORARY}"), None);
                ui.add_space(12.0);
            }
            None => {}
        }
        if self.contacts.is_empty() {
            empty(ui, Icon::People, "No contacts yet", "Meet in person and scan each other's code.");
            return;
        }
        components::section_header(ui, "Your contacts");
        let contacts = self.contacts.clone();
        let can_call = self.usable(Feature::CallOut);
        let count = contacts.len();
        for (index, contact) in contacts.into_iter().enumerate() {
            // A lease lets this PC find them by their rotating ID; a call then
            // looks for them for up to 10 s even when the list has not seen them.
            let enabled = can_call && contact.lease;
            // Rename, Safety code, Block and Forget wait behind "More" (Android's
            // contact sheet), so the list is names and calls only.
            let open = self.contact_more == Some(contact.id);
            // The row draws no divider of its own here: the one after its
            // notes below separates contacts (one line, never two).
            let (call, picked, toggled) = self.person_card(ui, contact.id, &contact.name, &contact.subtitle(), enabled, true, true, wide, (!wide).then_some(open));
            if picked {
                self.selected_contact = Some(contact.id);
            }
            if toggled {
                self.contact_more = if open { None } else { Some(contact.id) };
            }
            if let Some(voice) = call {
                self.place_call(contact.id, voice);
            }
            if wide {
                continue;
            }
            self.contact_extras(ui, &contact, theme::ROW_TEXT_INSET, self.contact_more == Some(contact.id));
            ui.add_space(6.0);
            if index + 1 < count {
                divider(ui, theme::ROW_TEXT_INSET);
            }
        }
    }
    /// A contact's quiet actions and notes (under the row, or in the detail pane).
    fn contact_extras(&mut self, ui: &mut egui::Ui, contact: &Contact, inset: f32, actions: bool) {
        {
            if actions { ui.horizontal(|ui| {
                ui.add_space((inset - 10.0).max(0.0));
                ui.spacing_mut().item_spacing.x = 2.0;
                if text_action(ui, "Rename", &format!("Rename {}", contact.name)).clicked() {
                    self.rename = Some((contact.id, contact.name.clone(), None));
                    self.focus_primary = true;
                }
                if text_action(ui, "Safety code", &format!("Safety code with {}", contact.name)).clicked() {
                    self.backend.command(Command::ShowSafetyCode(contact.id));
                }
                let label = if contact.blocked { "Unblock" } else { "Block" };
                if text_action(ui, label, &format!("{label} {}", contact.name)).clicked() {
                    self.backend.command(Command::SetBlocked { contact: contact.id, blocked: !contact.blocked });
                }
                if text_action(ui, "Forget", &format!("Forget {}", contact.name)).clicked() {
                    self.forget = Some(contact.id);
                }
            }); }
            // Notes line up under the contact's name (the row's text inset).
            let caption = |ui: &mut egui::Ui, text: String| {
                ui.horizontal_top(|ui| {
                    ui.add_space(inset);
                    ui.vertical(|ui| {
                        ui.set_max_width(theme::READABLE_WIDTH);
                        ui.add(egui::Label::new(RichText::new(text).size(13.0).color(SECONDARY)).wrap());
                    });
                });
            };
            if contact.blocked {
                caption(ui, format!("Blocked. Calls from {} are declined without ringing.", contact.name));
            }
            if let Some(code) = self.safety_codes.get(&contact.id) {
                ui.horizontal(|ui| {
                    ui.add_space(inset);
                    ui.label(RichText::new(code.as_str()).monospace().size(15.0).color(Color32::WHITE));
                });
                caption(ui, format!("Read these 60 digits with {} in person. Both screens must show exactly the same numbers.", contact.name));
            }
            if !contact.lease {
                caption(ui, format!("Ask {} to scan your code too. Until then this PC can't find them nearby.", contact.name));
            } else if !contact.nearby {
                caption(ui, format!("{} isn't nearby right now. Calling looks for them for up to 10 seconds.", contact.name));
            }
        }
    }

    // --------------------------------------------------------- Add contact (Meet)

    pub(super) fn meet_screen(&mut self, ui: &mut egui::Ui) {
        let focus = std::mem::take(&mut self.focus_primary);
        let mut back = screen_title(ui, "Add contact");
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            back = true;
        }
        if let Some(name) = self.appear_as().map(str::to_owned) {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("You appear as {name}")).size(14.0).color(SECONDARY));
        }
        ui.add_space(16.0);
        let mut scan = false;
        let mut answer = None;
        let mut camera: Option<bool> = None;
        let visible = self.pc_visible;
        let visibility_note = self.visibility_note.clone();
        egui::ScrollArea::vertical().id_salt("meet").auto_shrink([false, false]).show(ui, |ui| {
            let Some(meet) = self.meet.as_mut() else { return };
            let busy = matches!(meet.state, Some(MeetEvent::Searching) | Some(MeetEvent::Ask { .. }));
            // Scan their code: the camera, or the code pasted.
            let mut scan_column = |ui: &mut egui::Ui, meet: &mut Meet| {
                ui.set_max_width(MEET_COLUMN_MAX);
                ui.label(RichText::new("Scan their code").size(20.0).family(egui::FontFamily::Name("Heading".into())));
                ui.label(RichText::new("Stay close to each other.").size(14.0).color(SECONDARY));
                ui.add_space(12.0);
                // Their code by the webcam (the camera is on only while this is shown).
                if meet.camera {
                    ui.horizontal_top(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                        draw_icon(ui.painter(), rect, Icon::Qr, theme::TEXT);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(emphasis("Point the camera at the code on their phone"));
                            ui.label(RichText::new("Hold their screen still, about 20 to 40 cm from the camera.").size(13.0).color(SECONDARY));
                        });
                    });
                    ui.add_space(8.0);
                    if Pill::new("Stop camera").kind(Kind::Secondary).fill_width().show(ui).clicked() {
                        camera = Some(false);
                    }
                } else {
                    let start = Pill::new("Scan with camera").icon(Icon::Camera).fill_width().large().enabled(!busy).show(ui);
                    if start.clicked() {
                        camera = Some(true);
                    }
                }
                ui.add_space(18.0);
                let label = ui.label(RichText::new("Or paste their code").size(14.0).color(SECONDARY));
                let edit = ui
                    .add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut meet.code)
                            .hint_text("BVMID2.\u{2026}")
                            .margin(egui::vec2(14.0, 12.0)).background_color(theme::RAISED)
                            .desired_width(f32::INFINITY),
                    )
                    .labelled_by(label.id);
                if !busy {
                    crate::ui::accessible_set_value(ui, &edit, &mut meet.code, None);
                }
                if focus {
                    edit.request_focus();
                }
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.add_space(8.0);
                let ready = !busy && !meet.code.trim().is_empty();
                let s = Pill::new("Scan code").kind(Kind::Secondary).fill_width().enabled(ready).show(ui);
                scan = ready && (s.clicked() || enter);
                ui.add_space(14.0);
                match &meet.state {
                    None => {}
                    Some(MeetEvent::Searching) => {
                        ui.label(RichText::new("Looking for the phone that shows this code\u{2026}").color(SECONDARY));
                    }
                    Some(MeetEvent::Ask { name }) => {
                        ui.label(emphasis(&format!("Is this {name}'s screen you scanned?")).size(18.0));
                        ui.label(RichText::new("Say yes only if you can see the other phone yourself.").size(13.0).color(SECONDARY));
                        ui.add_space(6.0);
                        let label = ui.label(RichText::new("Name on this PC").size(14.0).color(SECONDARY));
                        let field = ui
                            .add(egui::TextEdit::singleline(&mut meet.name).margin(egui::vec2(14.0, 12.0)).background_color(theme::RAISED).desired_width(f32::INFINITY))
                            .labelled_by(label.id);
                        crate::ui::accessible_set_value(ui, &field, &mut meet.name, None);
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let named = !meet.name.trim().is_empty();
                            let yes = Pill::new("Yes, it is").min_width(140.0).enabled(named).show(ui);
                            if yes.clicked() {
                                answer = Some(true);
                            }
                            if Pill::new("No").kind(Kind::Secondary).show(ui).clicked() {
                                answer = Some(false);
                            }
                        });
                    }
                    Some(MeetEvent::Done(text)) => {
                        ui.horizontal_top(|ui| {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 22.0), egui::Sense::hover());
                            verified_mark(ui.painter(), rect.center(), 18.0);
                            ui.add(egui::Label::new(emphasis(text)).wrap());
                        });
                    }
                    Some(MeetEvent::Failed(text)) => {
                        components::notice(ui, text, None);
                        // The camera is blocked by Windows privacy settings: the way to fix it.
                        if text.contains("privacy settings") && text_action(ui, "Open camera settings", "Open camera settings").clicked() {
                            open_settings_page("ms-settings:privacy-webcam");
                        }
                    }
                }
            };
            // Show my code: the only place this PC's code and QR appear.
            let code_column = |ui: &mut egui::Ui, meet: &mut Meet| {
                ui.set_max_width(MEET_COLUMN_MAX);
                ui.label(RichText::new("Your code").size(20.0).family(egui::FontFamily::Name("Heading".into())));
                ui.label(RichText::new("Let them scan it with MeshLive.").size(14.0).color(SECONDARY));
                ui.add_space(12.0);
                match meet.own_code.clone() {
                    Some(code) => {
                        if meet.qr.as_ref().is_none_or(|(text, _)| *text != code) {
                            meet.qr = meshlive::qr_encode::QrCode::encode(&code).ok().map(|qr| (code.clone(), qr));
                        }
                        if let Some((_, qr)) = &meet.qr {
                            qr_picture(ui, qr);
                        }
                        ui.add_space(10.0);
                        let status = if visible {
                            "Visible to people nearby. On the other phone, open Add contact and scan this code.".to_owned()
                        } else {
                            visibility_note.clone().unwrap_or_else(|| "Getting ready to be visible\u{2026}".into())
                        };
                        ui.add(egui::Label::new(RichText::new(status).size(13.0).color(SECONDARY)).wrap());
                        if let Some(note) = &meet.code_note {
                            ui.label(emphasis(note));
                        }
                        ui.add(egui::Label::new(RichText::new(code).monospace().size(11.0).color(Color32::from_gray(110))).wrap());
                    }
                    None => {
                        let text = visibility_note.clone().unwrap_or_else(|| "Getting your code ready\u{2026}".into());
                        ui.label(RichText::new(text).size(14.0).color(SECONDARY));
                    }
                }
            };
            if ui.available_width() >= 760.0 {
                // Wide window: scan on the left, your code on the right.
                let column = ((ui.available_width() - 64.0) / 2.0).min(MEET_COLUMN_MAX);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(column);
                        scan_column(ui, meet);
                    });
                    ui.add_space(64.0);
                    ui.vertical(|ui| {
                        ui.set_width(column);
                        code_column(ui, meet);
                    });
                });
            } else {
                scan_column(ui, meet);
                ui.add_space(28.0);
                code_column(ui, meet);
            }
        });
        if let Some(on) = camera {
            self.meet_camera(on);
        }
        if scan {
            self.meet_scan();
        }
        if let Some(yes) = answer {
            self.meet_answer(yes);
        }
        if back {
            self.close_meet();
        }
    }

    // --------------------------------------------------------- Go live

    pub(super) fn golive_screen(&mut self, ui: &mut egui::Ui) {
        let focus = std::mem::take(&mut self.focus_primary);
        let mut back = screen_title(ui, "Go live");
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !self.audio_disclosure && !self.live_exposure {
            back = true;
        }
        ui.add_space(12.0);
        let running = self.golive.running();
        let gate = self.unavailable_reason(Feature::GoLive);
        if let Some(reason) = gate {
            components::notice(ui, &format!("Unavailable. {reason}"), None);
            ui.add_space(12.0);
        }
        if let Some(note) = self.live_note.as_deref() { components::notice(ui, note, None); ui.add_space(8.0); }
        // Before going live, the Go live button is pinned under the settings
        // (they scroll), so it is on screen at any window height.
        let pinned = if running { 0.0 } else { theme::BUTTON_HEIGHT_LARGE + 24.0 };
        let mut start_clicked = false;
        let ready = gate.is_none() && self.golive.title_valid();
        egui::ScrollArea::vertical()
            .id_salt("golive")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - pinned).max(0.0))
            .show(ui, |ui| {
            if !running {
                ui.horizontal(|ui| {
                    for (label, kind) in [("Video and audio", LiveKind::Video), ("Audio only", LiveKind::AudioOnly)] {
                        let r = chip(ui, label, self.golive.kind == kind, true);
                        if r.clicked() {
                            self.golive.kind = kind;
                        }
                    }
                });
                ui.add_space(4.0);
                ui.label(theme::caption("Audio only reaches further and keeps playing with the screen off."));
                ui.add_space(20.0);
                components::text_field(
                    ui,
                    &mut self.golive.title,
                    Some("Stream title (optional)"),
                    if self.live_consent_required { "Viewers see the title only after they join" } else { "Public: anyone nearby can read it" },
                    Some(TITLE_MAX_BYTES),
                );
                if !self.golive.title_valid() {
                    ui.label(RichText::new("Use a short plain title without control characters.").small().color(SECONDARY));
                }
                if self.live_consent_required {
                    ui.add_space(12.0);
                    if self.golive.kind == LiveKind::Video {
                        ui.horizontal_wrapped(|ui| { for (w, h) in [(426, 240), (640, 360), (854, 480)] {
                            if chip(ui, &format!("{w} × {h}"), (self.live_settings.width, self.live_settings.height) == (w, h), true).clicked() { self.live_settings.width = w; self.live_settings.height = h; }
                        }});
                        ui.horizontal_wrapped(|ui| { for fps in [15, 24, 30] { if chip(ui, &format!("{fps} fps"), self.live_settings.fps == fps, true).clicked() { self.live_settings.fps = fps; } }});
                        let switches = meshlive::video::codecs::Switches::current();
                        // Two fixed rows: in this scroll area horizontal_wrapped did not
                        // wrap, so four codec chips ran past a 640 px window and widened
                        // the page (and the bitrate slider) beyond it.
                        for row in [[("VP9", 2), ("AV1 (hardware)", 3)], [("HEVC (hardware)", 5), ("H.264 (hardware)", 4)]] {
                            if !row.iter().any(|(_, codec)| switches.codec_on(*codec)) { continue; }
                            ui.horizontal_wrapped(|ui| { for (label, codec) in row {
                                if !switches.codec_on(codec) { continue; }
                                if chip(ui, label, self.live_settings.codec == codec, true).clicked() { self.live_settings.codec = codec; }
                            }});
                        }
                        ui.add_space(8.0);
                        components::value_slider(ui, "Video bitrate", &mut self.live_settings.kbps, 100..=1500, 25, "kbps");
                    } else {
                        ui.horizontal_wrapped(|ui| { for (label, preset) in [("Low data · 16 kbps", 1), ("Voice · 24 kbps", 2), ("Music · 48 kbps", 3)] {
                            if chip(ui, label, self.live_settings.radio_preset == preset, true).clicked() { self.live_settings.radio_preset = preset; }
                        }});
                    }
                }
                if !self.switches.title_pages {
                    ui.label(
                        RichText::new("Viewers see the title only after they join.")
                            .small()
                            .color(SECONDARY),
                    );
                }
                if let Some(error) = self.golive.error {
                    ui.add_space(8.0);
                    ui.label(error);
                }
            } else {
                live_badge(ui, &self.golive.status());
                ui.add_space(4.0);
                if self.golive.title.is_empty() {
                    ui.label(theme::secondary(if self.golive.kind == LiveKind::Video { "Video and audio" } else { "Audio only" }));
                } else {
                    ui.label(theme::secondary(self.golive.title.clone()));
                }
                ui.add_space(28.0);
                let starting = self.golive.state == LiveState::Starting;
                let video = self.golive.kind == LiveKind::Video;
                let count = if video { 4.0 } else { 3.0 };
                centered_row(ui, count * theme::ROUND_CONTROL + (count - 1.0) * 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 28.0;
                    let paused = self.golive.state == LiveState::Paused;
                    // Kind::Primary (white) marks a control that is on (paused, muted, camera off).
                    let (label, kind) = if paused { ("Resume", Kind::Primary) } else { ("Pause", Kind::Secondary) };
                    if ui.add_enabled_ui(!starting, |ui| components::round_button(ui, Icon::Broadcast, label, kind)).inner.clicked() {
                        self.golive.toggle_pause();
                        let p = self.golive.state == LiveState::Paused;
                        self.backend.command(Command::PauseLive(p));
                    }
                    let (label, kind, icon) =
                        if self.golive.muted { ("Unmute", Kind::Primary, Icon::MicOff) } else { ("Mute", Kind::Secondary, Icon::Mic) };
                    if ui.add_enabled_ui(!starting, |ui| components::round_button(ui, icon, label, kind)).inner.clicked() {
                        self.golive.muted = !self.golive.muted;
                        let m = self.golive.muted;
                        self.backend.command(Command::MuteLive(m));
                    }
                    if video {
                        let (label, kind, icon) = if self.golive.camera_off {
                            ("Camera on", Kind::Primary, Icon::VideoOff)
                        } else {
                            ("Camera off", Kind::Secondary, Icon::Camera)
                        };
                        if ui.add_enabled_ui(!starting, |ui| components::round_button(ui, icon, label, kind)).inner.clicked() {
                            self.golive.camera_off = !self.golive.camera_off;
                            let off = self.golive.camera_off;
                            self.backend.command(Command::LiveCamera(!off));
                        }
                    }
                    let stop = components::round_button(ui, Icon::Stop, "Stop", Kind::Live);
                    if focus {
                        stop.request_focus();
                    }
                    if stop.clicked() {
                        self.stop_live();
                    }
                });
                if self.golive.kind == LiveKind::AudioOnly {
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(520.0);
                        ui.label(theme::caption(
                            "Mute keeps the microphone open and sends silence. Pause stops the microphone. Audio that was already sent may still play on other phones.",
                        ));
                    });
                }
            }
            if self.golive.state == LiveState::Ended {
                ui.add_space(8.0);
                ui.label(RichText::new("Not live").color(SECONDARY));
            }
        });
        if !running {
            ui.add_space(12.0);
            let start = components::live_button(ui, "Go live", ready);
            if focus && ready {
                start.request_focus();
            }
            start_clicked = start.clicked();
        }
        if start_clicked {
            if self.golive.kind == LiveKind::AudioOnly && !self.golive.disclosed {
                self.audio_disclosure = true;
                self.focus_primary = true;
            } else {
                self.start_live();
            }
        }
        if back {
            // A running broadcast keeps running; Back only closes this screen.
            self.golive_open = false;
        }
    }
}

/// Home's "Go live" as Android's streamModeButton: Widget.App.Button.Primary
/// (white, black text) with the live mark in red (iconTint live_red).
pub(super) fn home_go_live(running: bool) -> Pill<'static> {
    Pill::new(if running { "Your broadcast" } else { "Go live" })
        .kind(Kind::Primary)
        .icon(Icon::Broadcast)
        .icon_tint(theme::LIVE)
}

/// Add contact's two columns grow with the window up to this.
const MEET_COLUMN_MAX: f32 = 720.0;
/// From this width the Live page shows streams as a grid of cards.
const GRID_FROM: f32 = 900.0;
/// A stream card is at least this wide.
const CARD_MIN: f32 = 320.0;
/// The Live page on a backend without live streams.
pub(super) const STREAMS_NOT_HERE: &str = "Live streams aren't available on this PC";

/// The shown BVMID2 code as a QR picture: dark modules on white with the
/// four-module quiet zone, sized to scan from a phone held near the screen.
fn qr_picture(ui: &mut egui::Ui, qr: &meshlive::qr_encode::QrCode) {
    let quiet = 4;
    let cells = qr.size + 2 * quiet;
    // Bigger on a big window (easier to scan from a phone), never tiny.
    let side = ui.available_width().clamp(160.0, 420.0);
    let cell = (side / cells as f32).floor().max(2.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(cell * cells as f32, cell * cells as f32), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, "Your code as a QR code"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 16.0, egui::Color32::WHITE);
    for y in 0..qr.size {
        for x in 0..qr.size {
            if qr.get(x, y) {
                let min = rect.min + egui::vec2((x + quiet) as f32 * cell, (y + quiet) as f32 * cell);
                painter.rect_filled(egui::Rect::from_min_size(min, egui::vec2(cell, cell)), 0.0, egui::Color32::BLACK);
            }
        }
    }
}

/// Windows Settings > Privacy & security > Microphone (the user decides;
/// this app never changes a privacy setting).
fn open_microphone_settings() {
    open_settings_page("ms-settings:privacy-microphone");
}
/// Opens a Windows Settings page by its ms-settings URI.
fn open_settings_page(page: &str) {
    #[cfg(not(test))]
    let _ = std::process::Command::new("explorer.exe").arg(page).spawn();
    #[cfg(test)]
    let _ = page;
}

/// A media line a person needs to read ("Voice only: ..."); the rest
/// (codec, encoder, size, frame rate) is technical and waits under Details.
fn plain_media_line(line: &str) -> bool {
    line.starts_with("Voice only")
}

impl Ui {
    /// CF-OUT-07 on the call screen in plain words: a fallback ("Voice only")
    /// is always shown; the codec and encoder in use sit under a collapsed
    /// "Details" (never only in a trace).
    fn call_details(&mut self, ui: &mut egui::Ui, lines: Vec<String>) {
        let (plain, technical): (Vec<_>, Vec<_>) = lines.into_iter().partition(|l| plain_media_line(l));
        for line in plain {
            ui.label(RichText::new(line).size(13.0).color(SECONDARY));
        }
        if technical.is_empty() {
            return;
        }
        let label = if self.call_details_open { "Hide details" } else { "Details" };
        if Pill::new(label).kind(Kind::Quiet).small().show(ui).clicked() {
            self.call_details_open = !self.call_details_open;
        }
        if self.call_details_open {
            for line in technical {
                ui.label(RichText::new(line).size(12.0).color(SECONDARY));
            }
        }
    }
    /// `call_details` without its own toggle (the video header carries it).
    fn call_detail_lines(&mut self, ui: &mut egui::Ui, lines: Vec<String>) {
        for line in lines {
            if plain_media_line(&line) {
                ui.label(RichText::new(line).size(13.0).color(SECONDARY));
            } else if self.call_details_open {
                ui.label(RichText::new(line).size(12.0).color(SECONDARY));
            }
        }
    }
}

/// The video area: the picture as large as fits (FIT, never stretched,
/// already upright), with rounded corners, on plain black (no framed box).
/// Without a picture, a quiet panel with the state.
pub(super) fn video_stage(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    name: &str,
    status: &str,
    texture: Option<(egui::TextureId, [u32; 2])>,
) -> egui::Rect {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size.x.max(0.0), size.y.max(0.0)), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, format!("{name}. {status}")));
    let radius = 16;
    match texture {
        Some((id, picture)) => {
            ui.painter().rect_filled(rect, radius, Color32::BLACK);
            let (shown, _) = call_layout::fit(rect, picture, 0);
            egui::Image::new((id, shown.size())).corner_radius(radius).paint_at(ui, shown);
        }
        None => {
            ui.painter().rect_filled(rect, radius, Color32::from_gray(14));
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, status, egui::FontId::proportional(18.0), SECONDARY);
        }
    }
    rect
}

/// One line for the streams this PC can't watch: how many, and the one
/// thing to ask when they share a reason.
fn unwatchable_line(blocked: &[StreamEntry]) -> String {
    let count = blocked.len();
    let what = if count == 1 { "1 stream nearby can't be watched from this PC".to_owned() } else { format!("{count} streams nearby can't be watched from this PC") };
    match blocked.first().and_then(|s| s.unavailable) {
        Some(reason) if blocked.iter().all(|s| s.unavailable == Some(reason)) => format!("{what}. {reason}."),
        _ => format!("{what}."),
    }
}

/// What the self-view tile shows.
enum SelfView {
    /// The camera is on: its newest picture, or None until the first arrives.
    Picture(Option<(egui::TextureId, [u32; 2])>),
    /// The camera is off (or can't be used: the notice above says why).
    Off,
}

/// Your own camera: a small tile bottom right inside the call picture's area.
fn self_view_tile(ui: &mut egui::Ui, area: egui::Rect, view: SelfView) {
    let aspect = match &view {
        SelfView::Picture(Some((_, [w, h]))) => *w as f32 / (*h).max(1) as f32,
        _ => 4.0 / 3.0,
    };
    let tile = call_layout::pip(area, aspect);
    if tile.width() < 24.0 || tile.height() < 18.0 {
        return;
    }
    let label = match &view {
        SelfView::Picture(Some(_)) => "Your camera",
        SelfView::Picture(None) => "Your camera, starting",
        SelfView::Off => "Your camera is off",
    };
    ui.interact(tile, ui.id().with("self-view"), egui::Sense::hover())
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, label));
    let painter = ui.painter().with_clip_rect(area);
    let radius = 10;
    painter.rect_filled(tile, radius, theme::RAISED);
    match view {
        SelfView::Picture(Some((id, size))) => {
            egui::Image::new((id, egui::vec2(size[0] as f32, size[1] as f32))).corner_radius(radius).paint_at(ui, tile);
        }
        SelfView::Picture(None) | SelfView::Off => {
            let off = matches!(view, SelfView::Off);
            let (icon, text) = if off { (Icon::VideoOff, "Camera off") } else { (Icon::Camera, "Starting\u{2026}") };
            let small = tile.height() < 64.0;
            let icon_center = if small { tile.center() } else { tile.center() - egui::vec2(0.0, 9.0) };
            draw_icon(&painter, egui::Rect::from_center_size(icon_center, egui::vec2(20.0, 20.0)), icon, SECONDARY);
            if !small {
                painter.text(tile.center() + egui::vec2(0.0, 15.0), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.0), SECONDARY);
            }
        }
    }
    painter.rect_stroke(tile, radius, egui::Stroke::new(1.0, Color32::from_gray(70)), egui::StrokeKind::Inside);
}

// ------------------------------------------------------------------ parts
// Small screen-local pieces on top of `components` and `theme`.

/// A quiet text action (Rename, Forget, Block...): `label` shown, `name` read.
fn text_action(ui: &mut egui::Ui, label: &str, name: &str) -> egui::Response {
    Pill::new(label).kind(Kind::Quiet).small().a11y(name).show(ui)
}

/// The green verified mark beside a name (green only for verified/answer).
fn verified_mark(painter: &egui::Painter, center: egui::Pos2, size: f32) {
    draw_icon(painter, egui::Rect::from_center_size(center, egui::vec2(size, size)), Icon::Verified, theme::POSITIVE);
}

/// "02:13" (or "1:02:13").
fn clock(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

/// Lays out controls centred in the available width.
fn centered_row(ui: &mut egui::Ui, total_width: f32, f: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - total_width) / 2.0).max(0.0));
        f(ui);
    });
}

