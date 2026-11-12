//! Settings as Android: a top page of rows and pushed pages (Privacy,
//! Connection, Experiments, Help and feedback, About this app) whose title
//! row has a Back arrow (`Ui::show`). Each switch says what it does in its
//! name; the longer explanation sits behind its (i) button. A switch that
//! cannot work says why under it.
use super::*;
use backend::Command;

/// Experiments page (Android `advanced_experiments_hint`; this PC has no
/// Manual video gate, and these choices are kept for this run only).
pub(super) const EXPERIMENTS_HINT: &str = "Tests that may not work well yet. They turn off when MeshLive closes.";
/// Reset private ID, asked first (Android `rotating_ids_reset_confirm`).
pub(super) const RESET_ID_CONFIRM: &str = "Reset nearby recognition? Your contacts' phones recognise this PC nearby again after your next call with each of them. Your contacts and their verification stay the same.";
pub(super) const RESET_ID_DONE: &str = "Private ID reset.";
pub(super) const RESET_ID_FAILED: &str = "The private ID couldn't be reset. Try again.";
/// Manual video settings on (Android `advancedSettingsDescriptionText`).
pub(super) const MANUAL_ON: &str = "On: Go live uses exactly what you choose below; nothing adapts automatically. Your choice lasts until MeshLive closes.";
/// Android `feat_viewer_frame_age_description` ("phone" read as "PC").
const FRAME_AGE_INFO: &str = "While you watch a stream, dim a frozen picture and say so when it stops updating or the connection to the sender is lost, instead of leaving it on screen as if it were live. A small label shows how long ago the picture last updated. That time is counted on this PC from the last picture it received; it is not the delay from the camera.";
/// CF-PRIV-23 (`notify::toast_text`): no switch on Windows (design decision).
pub(super) const LOCK_SCREEN_NOTE: &str = "While this PC is locked or you are presenting, a call notification shows no name.";
/// SEC-11: where this PC's identity key is kept (Settings > Privacy).
pub(super) const IDENTITY_KEY_CHIP: &str = "Identity key: protected by this PC's security chip";
pub(super) const IDENTITY_KEY_WINDOWS: &str = "Identity key: stored by Windows";
/// Under Settings > Privacy > Computer name (2026-11-12).
pub(super) const COMPUTER_NAME_NOTE: &str = "Phones this PC calls can read this name over Bluetooth, and some keep it. To change it, select it and rename this PC in Windows settings.";
pub(super) const IDENTITY_KEY_UNAVAILABLE: &str = "Identity key: this PC's security chip can't open it now. Calls can't start until it can. Delete everything starts a new ID.";
const ABOUT_NOTE: &str = "Free software under the GNU GPL, version 3 or later. This Windows build is a preview.";
/// Android `names_audience_title` and its choices (`names_audience_everyone`,
/// `names_audience_verified`, `names_audience_nobody`).
pub(super) const NAMES_AUDIENCE_TITLE: &str = "Who sees my name";
/// WP6NF: a choice that could not be saved.
pub(super) const SETTINGS_NOT_SAVED: &str = "Your settings couldn't be saved. They apply until MeshLive closes.";
pub(super) fn audience_text(audience: meshlive::label_policy::NamesAudience) -> &'static str {
    use meshlive::label_policy::NamesAudience;
    match audience {
        NamesAudience::Everyone => "People I call or who call me",
        NamesAudience::VerifiedOnly => "Only my contacts",
        NamesAudience::Nobody => "Nobody",
    }
}

impl Ui {
    pub(super) fn settings(&mut self, ui: &mut egui::Ui, renderer: &str) {
        ui.spacing_mut().item_spacing.y = 0.0;
        if self.settings_split(ui) {
            // Experimental (off): the top page on the left of a wide window,
            // the chosen page on the right.
            components::split(ui, 360.0, |left, right| {
                left.spacing_mut().item_spacing.y = 0.0;
                right.spacing_mut().item_spacing.y = 0.0;
                self.settings_main(left);
                right.set_max_width(right.available_width().min(960.0));
                let page = if self.settings_page == SettingsPage::Main { SettingsPage::Privacy } else { self.settings_page };
                components::settings_label(right, page.title());
                self.settings_page_body(right, page, renderer);
            });
        } else if self.settings_page == SettingsPage::Main {
            self.settings_main(ui);
        } else {
            self.settings_page_body(ui, self.settings_page, renderer);
        }
    }
    /// The experimental split layout is in use (wide window, switch on).
    pub(super) fn settings_split(&self, ui: &egui::Ui) -> bool {
        self.split_panes && ui.available_width() >= theme::WIDE_SPLIT
    }
    /// A dialog over the page owns Escape.
    pub(super) fn settings_dialog_open(&self) -> bool {
        self.dialog.is_some() || self.wipe_confirm.is_some() || self.name_edit.is_some() || self.reset_id_confirm
            || self.block_confirm.is_some() || self.unblock_confirm.is_some() || self.reset_blocks_confirm
            || self.report_done || self.mail_fallback || self.audience_sheet
    }

    /// "Who sees my name" (Android `refreshIdentityAudience`'s single-choice
    /// dialog): the three choices, the current one marked; a pick saves it,
    /// Cancel, Escape or a click outside change nothing.
    pub(super) fn names_audience_sheet(&mut self, ctx: &egui::Context) {
        if !self.audience_sheet {
            return;
        }
        if self.own_name.is_none() {
            self.audience_sheet = false;
            return;
        }
        let current = self.switches.names_audience;
        let mut picked = None;
        let mut cancel = false;
        let response = components::dialog(ctx, "names-audience", |ui| {
            components::dialog_title(ui, NAMES_AUDIENCE_TITLE);
            for audience in meshlive::label_policy::NamesAudience::ALL {
                let text = audience_text(audience);
                let mut row = ListRow::new(text);
                if audience == current {
                    row = row.trailing(Trailing::Value("Selected".into())).a11y(format!("{text}, Selected"));
                }
                if row.show(ui).clicked() {
                    picked = Some(audience);
                }
            }
            (cancel, _) = components::dialog_buttons(ui, "Cancel", Kind::Quiet, true, None);
        });
        if let Some(audience) = picked {
            self.audience_sheet = false;
            if audience != current {
                self.switches.names_audience = audience;
                self.switches_changed();
            }
        } else if cancel || response.should_close() {
            self.audience_sheet = false;
        }
    }

    /// The top page (Android `settingsPageMain`).
    fn settings_main(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        // WP6NF finding 7: a choice that could not be saved says so.
        if self.settings_unsaved {
            note(ui, SETTINGS_NOT_SAVED);
            ui.add_space(8.0);
        }
        components::settings_card(ui, |ui| {
            // As Android: Your code opens Add contact with this PC's code.
            if ListRow::new("Your code").icon(Icon::Qr).chevron().show(ui).clicked() {
                // Android: Settings > Your code opens on "Show my code".
                self.open_meet_on(true);
            }
            let shown = self.own_name.clone().unwrap_or_else(|| crate::ui::NAME_NOT_SET.into());
            if ListRow::new("Your name")
                .icon(Icon::Person)
                .trailing(Trailing::ValueChevron(shown.clone()))
                .a11y(format!("Your name, {shown}"))
                .show(ui)
                .clicked()
            {
                self.name_error = None;
                self.name_edit = Some(self.own_name.clone().unwrap_or_default());
            }
            // WP4: Android's "Who sees my name", only while a name is set.
            if self.own_name.is_some() {
                let value = audience_text(self.switches.names_audience);
                if ListRow::new(NAMES_AUDIENCE_TITLE)
                    .icon(Icon::People)
                    .trailing(Trailing::ValueChevron(value.to_owned()))
                    .a11y(format!("{NAMES_AUDIENCE_TITLE}, {value}"))
                    .show(ui)
                    .clicked()
                {
                    self.audience_sheet = true;
                }
            }
        });
        ui.add_space(8.0);
        note(ui, crate::ui::NAME_AUDIENCE);
        ui.add_space(theme::SECTION_GAP);

        components::settings_card(ui, |ui| {
            let receive_gate = self.unavailable_reason(Feature::ReceiveCalls);
            let mut receive = self.switches.receive_calls && receive_gate.is_none();
            let awake = if receive { Some(crate::ui::AWAKE_NOTE) } else { None };
            let mut row = ToggleRow::new("Allow calls").info("Let phones nearby call this PC. Phones can't reach it while it sleeps.");
            if let Some(note) = awake {
                row = row.subtitle(note);
            }
            if row.enabled(receive_gate.is_none()).show(ui, &mut receive).changed() {
                self.switches.receive_calls = receive;
                changed = true;
            }
            if let Some(reason) = receive_gate {
                gate_reason(ui, reason);
            }
            if ToggleRow::new("Start with Windows")
                .info("MeshLive starts in the notification area when you sign in, so phones can call this PC. Windows can also turn this off in Task Manager.")
                .show(ui, &mut self.switches.start_with_windows)
                .changed()
            {
                changed = true;
            }
        });
        ui.add_space(theme::SECTION_GAP);

        components::settings_card(ui, |ui| {
            // Android `settingsRowVideo` above Privacy: Standard, or Manual while
            // Manual video settings is on.
            let value = if self.manual_video { "Manual" } else { "Standard" };
            if ListRow::new("Video quality")
                .icon(SettingsPage::Video.icon())
                .trailing(Trailing::ValueChevron(value.into()))
                .a11y(format!("Video quality, {value}"))
                .show(ui)
                .clicked()
            {
                self.settings_page = SettingsPage::Video;
            }
            self.page_row(ui, SettingsPage::Privacy);
        });
        ui.add_space(theme::SECTION_GAP);

        components::settings_label(ui, "Advanced");
        components::settings_card(ui, |ui| {
            for page in [SettingsPage::Connection, SettingsPage::Manual, SettingsPage::Experiments, SettingsPage::Help] {
                self.page_row(ui, page);
            }
        });
        ui.add_space(theme::SECTION_GAP);

        components::settings_label(ui, "About and updates");
        components::settings_card(ui, |ui| {
            self.page_row(ui, SettingsPage::About);
            // Android `contactUsRow`: an email draft to MeshLive (no identifiers).
            if ListRow::new(super::report::CONTACT_US)
                .subtitle(super::report::CONTACT_US_SUBTITLE)
                .icon(Icon::People)
                .chevron()
                .show(ui)
                .clicked()
            {
                self.open_mail_draft(meshlive::reporting::Draft::Contact);
            }
        });
        ui.add_space(theme::SECTION_GAP);

        if Pill::new("Delete everything").kind(Kind::Destructive).icon(Icon::Warning).fill_width().show(ui).clicked() {
            self.wipe_confirm = Some(String::new());
        }
        ui.add_space(theme::SECTION_GAP);
        if changed {
            self.switches_changed();
        }
    }
    /// A row that opens a pushed page.
    fn page_row(&mut self, ui: &mut egui::Ui, page: SettingsPage) {
        if ListRow::new(page.title()).icon(page.icon()).chevron().show(ui).clicked() {
            self.settings_page = page;
        }
    }
    fn settings_page_body(&mut self, ui: &mut egui::Ui, page: SettingsPage, renderer: &str) {
        match page {
            SettingsPage::Main => self.settings_main(ui),
            SettingsPage::Video => self.settings_video(ui),
            SettingsPage::Privacy => self.settings_privacy(ui),
            SettingsPage::Connection => self.settings_connection(ui),
            SettingsPage::Manual => self.settings_manual(ui),
            SettingsPage::Experiments => self.settings_experiments(ui, renderer),
            SettingsPage::Help => self.settings_help(ui),
            SettingsPage::About => self.settings_about(ui),
            SettingsPage::Blocked => self.settings_blocked(ui),
        }
    }

    /// Video and calls (Android `settingsPageVideo`). This PC reads no
    /// temperature, so Android's "Show temperature" and "Lower quality when
    /// hot" are not here.
    fn settings_video(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        components::settings_card(ui, |ui| {
            if ToggleRow::new("Fill screen with video")
                .info("The other person's picture fills the video area in calls and while you watch; the sides that run past are cut. Off: the whole picture with black bars.")
                .show(ui, &mut self.switches.video_fill)
                .changed()
            {
                changed = true;
            }
            // Android `ViewerFeatures.frameAge` (Experimental, off).
            if ToggleRow::new("Warn when video freezes")
                .info(FRAME_AGE_INFO)
                .show(ui, &mut self.switches.frame_age)
                .changed()
            {
                changed = true;
            }
        });
        ui.add_space(theme::SECTION_GAP);
        if changed {
            self.switches_changed();
        }
    }

    /// Manual video (Android `settingsPageManual`): off (Standard), Go live
    /// sends the default video; on, exactly the encoder and quality chosen
    /// here. The radio quality stays on Go live (Android's audio-only screen).
    fn settings_manual(&mut self, ui: &mut egui::Ui) {
        components::settings_card(ui, |ui| {
            let _ = ToggleRow::new("Manual video settings").show(ui, &mut self.manual_video);
        });
        ui.add_space(8.0);
        note(ui, &manual_note(self.manual_video));
        if self.manual_video {
            let s = &mut self.live_settings;
            ui.add_space(theme::SECTION_GAP);
            components::settings_label(ui, "Encoder");
            let codecs = meshlive::video::codecs::Switches::current();
            // Two fixed rows: four chips in one row ran past a 640 px window.
            for row in [[2, 3], [5, 4]] {
                if !row.iter().any(|codec| codecs.codec_on(*codec)) {
                    continue;
                }
                ui.horizontal_wrapped(|ui| {
                    for codec in row {
                        if codecs.codec_on(codec) && chip(ui, codec_name(codec), s.codec == codec, true).clicked() {
                            s.codec = codec;
                        }
                    }
                });
            }
            ui.add_space(theme::SECTION_GAP);
            components::settings_label(ui, "Quality");
            ui.label(theme::secondary("Resolution"));
            ui.horizontal_wrapped(|ui| {
                for (w, h) in [(426, 240), (640, 360), (854, 480)] {
                    if chip(ui, &format!("{w} \u{d7} {h}"), (s.width, s.height) == (w, h), true).clicked() {
                        s.width = w;
                        s.height = h;
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(theme::secondary("Frame rate"));
            ui.horizontal_wrapped(|ui| {
                for fps in [15, 24, 30] {
                    if chip(ui, &format!("{fps} fps"), s.fps == fps, true).clicked() {
                        s.fps = fps;
                    }
                }
            });
            ui.add_space(12.0);
            components::value_slider(ui, "Video bitrate", &mut s.kbps, 100..=1500, 25, "kbps");
        }
        ui.add_space(theme::SECTION_GAP);
    }

    /// Privacy (the Android rows this PC has, in Android's order).
    fn settings_privacy(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        components::settings_card(ui, |ui| {
            if ToggleRow::new("Block unknown callers")
                .info("Calls from anyone who isn't a verified contact are declined without ringing. To them it looks like you didn't answer.")
                .show(ui, &mut self.switches.block_unknown)
                .changed()
            {
                changed = true;
            }
            // Android: Blocked people follows Block unknown callers.
            if ListRow::new("Blocked people").icon(Icon::Person).chevron().show(ui).clicked() {
                self.settings_page = SettingsPage::Blocked;
            }
            if ListRow::new("Reset private ID").icon(Icon::Shield).show(ui).clicked() {
                self.reset_id_confirm = true;
            }
            // What Windows tells a phone this PC calls when it asks for its
            // Bluetooth name: the computer name (Windows Settings > About).
            if let Some(name) = self.computer_name.clone() {
                if ListRow::new("Computer name")
                    .icon(Icon::Info)
                    .trailing(Trailing::ValueChevron(name.clone()))
                    .a11y(format!("Computer name, {name}. Opens Windows settings to rename this PC."))
                    .show(ui)
                    .clicked()
                {
                    self.open_about_settings();
                }
            }
        });
        if self.computer_name.is_some() {
            ui.add_space(8.0);
            note(ui, COMPUTER_NAME_NOTE);
        }
        // Design decision 2026-11-06: an unreadable block list is said where you look,
        // with Reset (it blocks nobody until it is read again or reset).
        if self.blocked_people.unreadable {
            ui.add_space(8.0);
            if components::notice(ui, super::blocked::UNREADABLE, Some(super::blocked::RESET_ACTION)) {
                self.reset_blocks_confirm = true;
            }
        }
        if let Some(done) = self.private_id_reset {
            ui.add_space(8.0);
            note(ui, if done { RESET_ID_DONE } else { RESET_ID_FAILED });
        }
        ui.add_space(8.0);
        note(ui, LOCK_SCREEN_NOTE);
        if let Some(key) = self.identity_key {
            ui.add_space(8.0);
            note(ui, match key {
                backend::IdentityKey::Chip => IDENTITY_KEY_CHIP,
                backend::IdentityKey::Windows => IDENTITY_KEY_WINDOWS,
                backend::IdentityKey::ChipUnavailable => IDENTITY_KEY_UNAVAILABLE,
            });
        }
        ui.add_space(theme::SECTION_GAP);
        if changed {
            self.switches_changed();
        }
    }

    /// Connection (Android's Advanced > Connection): this PC's Bluetooth and
    /// what it can do. The Wi-Fi Direct row joins this page later (W4).
    fn settings_connection(&mut self, ui: &mut egui::Ui) {
        let status = self.bluetooth.map_or(CHECKING_BLUETOOTH, |caps| caps.status());
        components::settings_card(ui, |ui| {
            let _ = ListRow::new("Bluetooth")
                .icon(Icon::Bluetooth)
                .trailing(Trailing::Value(status.into()))
                .clickable(false)
                .a11y(format!("Bluetooth, {status}"))
                .show(ui);
        });
        ui.add_space(8.0);
        ui.indent("bluetooth-details", |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if let Some(caps) = self.bluetooth {
                ui.label(theme::secondary(format!(
                    "Find phones: {} \u{b7} Be found by phones: {}",
                    support(caps.central),
                    support(caps.peripheral)
                )));
                for feature in Feature::ALL {
                    let text = match self.availability(feature) {
                        Some(Availability::Available) => "Available".to_owned(),
                        Some(Availability::NeedsAdvertisingCheck) => "Checked when you turn it on".to_owned(),
                        Some(Availability::Unavailable(reason)) => format!("Unavailable. {reason}"),
                        None => "Unknown".to_owned(),
                    };
                    ui.label(theme::secondary(format!("{}: {text}", feature.label())));
                }
            }
            ui.label(theme::secondary("MeshLive never turns Bluetooth on or off."));
        });
        ui.add_space(theme::SECTION_GAP);
    }

    /// Experiments (Android `settingsPageExperiments`): user-facing tests,
    /// kept for this run only. Developer arms stay environment variables.
    fn settings_experiments(&mut self, ui: &mut egui::Ui, renderer: &str) {
        note(ui, EXPERIMENTS_HINT);
        ui.add_space(8.0);
        components::settings_card(ui, |ui| {
            let mut demo = self.demo;
            if ToggleRow::new("Show sample people and streams (Experimental)").show(ui, &mut demo).changed() {
                self.set_demo(demo);
            }
            ToggleRow::new("Show GPU texture test").show(ui, &mut self.texture_visible);
            ToggleRow::new("Split panes on wide windows (Experimental)").show(ui, &mut self.split_panes);
        });
        ui.add_space(8.0);
        ui.indent("experiments-facts", |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(theme::secondary(renderer));
            ui.label(theme::secondary("Ctrl+plus/minus to change text size."));
        });
        ui.add_space(theme::SECTION_GAP);
    }

    /// Help and feedback: Export diagnostics and Report a bug (Android
    /// `reportBugButton`; here the same email draft as Contact us).
    fn settings_help(&mut self, ui: &mut egui::Ui) {
        components::settings_card(ui, |ui| {
            if ListRow::new("Export diagnostics")
                .icon(Icon::Search)
                .chevron()
                .a11y("Export diagnostics, A text report for a problem report")
                .show(ui)
                .clicked()
            {
                self.backend.command(Command::ExportDiagnostics);
            }
            if ListRow::new(super::report::REPORT_BUG).icon(Icon::Warning).chevron().show(ui).clicked() {
                self.open_mail_draft(meshlive::reporting::Draft::Contact);
            }
        });
        ui.add_space(theme::SECTION_GAP);
    }

    /// About this app (Android `activity_about`): the version and how this
    /// copy was installed.
    fn settings_about(&mut self, ui: &mut egui::Ui) {
        let installed = if crate::packaged() { "MSIX package" } else { "Zip folder" };
        components::settings_card(ui, |ui| {
            for (title, value) in [("Version", env!("CARGO_PKG_VERSION")), ("Installed as", installed)] {
                let _ = ListRow::new(title).trailing(Trailing::Value(value.into())).clickable(false).a11y(format!("{title}, {value}")).show(ui);
            }
        });
        ui.add_space(8.0);
        note(ui, ABOUT_NOTE);
        ui.add_space(theme::SECTION_GAP);
    }
}

/// The encoder's name as its chip shows it.
pub(super) fn codec_name(codec: u8) -> &'static str {
    match codec {
        3 => "AV1 (hardware)",
        4 => "H.264 (hardware)",
        5 => "HEVC (hardware)",
        _ => "VP9",
    }
}

/// What Manual video settings means now: on, exactly the choices; off
/// (Standard), the default video, said in full (this PC adapts nothing).
fn manual_note(on: bool) -> String {
    if on {
        return MANUAL_ON.to_owned();
    }
    let auto = meshlive::stream_source::Settings::default();
    format!(
        "Off (recommended): Go live uses {} \u{d7} {}, {} fps, {} kbps and {}.",
        auto.width,
        auto.height,
        auto.fps,
        auto.kbps,
        codec_name(auto.codec)
    )
}

/// Why a switch cannot be used, under it (wraps; screen readers read it).
fn gate_reason(ui: &mut egui::Ui, reason: &str) {
    ui.label(theme::secondary(reason));
    ui.add_space(10.0);
}

/// A quiet note under a card, aligned with the card's text.
fn note(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.add(egui::Label::new(theme::caption(text)).wrap());
    });
}
