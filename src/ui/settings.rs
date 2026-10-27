//! Settings: plain rows on black grouped under short section labels. Each
//! switch says what it does in its name; the longer explanation sits behind
//! its (i) button. A switch that cannot work says why under it.
use super::*;
use backend::Command;

impl Ui {
    pub(super) fn settings(&mut self, ui: &mut egui::Ui, renderer: &str) {
        // Experimental (off): a category list on the left of a wide window.
        if self.split_panes && ui.available_width() >= theme::WIDE_SPLIT {
            components::split(ui, 260.0, |left, right| {
                for section in SettingsSection::ALL {
                    let selected = self.settings_section == section;
                    if components::nav_item(left, section.label(), section.icon(), selected, false).clicked() {
                        self.settings_section = section;
                    }
                }
                right.set_max_width(right.available_width().min(960.0));
                self.settings_body(right, renderer, Some(self.settings_section));
            });
        } else {
            self.settings_body(ui, renderer, None);
        }
    }
    fn settings_body(&mut self, ui: &mut egui::Ui, renderer: &str, only: Option<SettingsSection>) {
        let show = |section: SettingsSection| only.is_none_or(|o| o == section);
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut changed = false;

        if show(SettingsSection::Profile) {
        components::settings_label(ui, "Profile");
        components::settings_card(ui, |ui| {
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
        let id = self.own_id.clone().unwrap_or_else(|| "Not available".into());
        if ListRow::new("Your ID")
            .icon(Icon::Shield)
            .trailing(Trailing::ValueChevron(id.clone()))
            .a11y(format!("Your ID, {id}"))
            .show(ui)
            .clicked()
        {
            // As Android: your ID opens your code, for adding a contact.
            self.open_meet();
        }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.add(egui::Label::new(theme::caption(crate::ui::NAME_AUDIENCE)).wrap());
        });
        ui.add_space(theme::SECTION_GAP);
        }

        if show(SettingsSection::Calls) {
        components::settings_label(ui, "Calls");
        components::settings_card(ui, |ui| {
        let receive_gate = self.unavailable_reason(Feature::ReceiveCalls);
        let mut receive = self.switches.receive_calls && receive_gate.is_none();
        let awake = if receive { Some(crate::ui::AWAKE_NOTE) } else { None };
        let mut row = ToggleRow::new("Allow calls").info("Let phones nearby call this PC. Phones can't reach it while it sleeps.");
        if let Some(note) = awake {
            row = row.subtitle(note);
        }
        if row
            .enabled(receive_gate.is_none())
            .show(ui, &mut receive)
            .changed()
        {
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
        let streams = self.backend.streams();
        let s = &mut self.switches;
        for (value, title, info) in [
            (&mut s.voice_calls, "Voice calls", "Call someone without video, and add video during the call."),
            (
                &mut s.block_unknown,
                "Block unknown callers",
                "Calls from anyone who isn't a verified contact are declined without ringing. To them it looks like you didn't answer.",
            ),
            (
                &mut s.ringback,
                "Play ringing tone",
                "A quiet ringing tone while the other phone rings, and when someone calls this PC (not during Do not disturb or while presenting). Your volume is never changed.",
            ),
            (&mut s.relay, "Relay video for others", "Pass live video on to phones near you."),
        ] {
            if title == "Relay video for others" && !streams {
                continue;
            }
            changed |= ToggleRow::new(title).info(info).show(ui, value).changed();
        }
        });
        ui.add_space(theme::SECTION_GAP);
        }

        if show(SettingsSection::Privacy) {
        components::settings_label(ui, "Privacy");
        components::settings_card(ui, |ui| {
        let visible_gate = self.unavailable_reason(Feature::VisibleToContacts);
        let mut visible = self.switches.visible_to_contacts && visible_gate.is_none();
        if WIRED_VISIBLE_TO_CONTACTS && ToggleRow::new("Visible to contacts")
            .info("Contacts nearby can see that you are ready for calls.")
            .enabled(visible_gate.is_none())
            .show(ui, &mut visible)
            .changed()
        {
            self.switches.visible_to_contacts = visible;
            changed = true;
        }
        if let (true, Some(reason)) = (WIRED_VISIBLE_TO_CONTACTS, visible_gate) {
            gate_reason(ui, reason);
        }
        let streams_privacy = self.backend.streams();
        let Switches { generated_names, title_pages, names_before_answer, bvmid2, bvmid1_compat, visible_by_name, .. } =
            &mut self.switches;
        for (value, title, info) in [
            (
                generated_names,
                "Friendly names for nearby phones",
                "Shows phones you haven't verified under a made-up two-word name such as Blue Otter, made from the temporary ID they are using. It doesn't prove who they are. Off: the plain ID is shown.",
            ),
            (
                title_pages,
                "Show my stream title to everyone nearby",
                "When you go live with a title, anyone nearby can read it in their list before they join. A title is public. Off: viewers see the title only after they join.",
            ),
            (
                names_before_answer,
                "Show my name before they answer",
                "Your name goes to whoever you call, before the app has checked who they are. Only the name goes early, never the call subject. Turn this off to send it only after they answer and are checked.",
            ),
            (
                bvmid2,
                "Codes that expire when you add a contact",
                "Add contact shows a code that works once, for five minutes, and checks that the other phone really holds its key before you confirm. Both of you scan each other. Off: the earlier code that never expires.",
            ),
            (
                bvmid1_compat,
                "Accept older verification codes",
                "Lets a code from an older MeshLive version give the full check mark. Those codes never expire, so by default they only count as checked by code.",
            ),
            (
                visible_by_name,
                "Visible by name",
                "Adds a button to show a name you choose to everyone nearby for ten minutes, for a meeting or a camp. Off: no name is ever shown over Bluetooth.",
            ),
        ] {
            if title == "Show my stream title to everyone nearby" && !streams_privacy {
                continue;
            }
            if HIDDEN_UNTIL_WIRED.contains(&title) {
                continue;
            }
            changed |= ToggleRow::new(title).info(info).show(ui, value).changed();
        }
        });
        ui.add_space(theme::SECTION_GAP);
        }

        if show(SettingsSection::Bluetooth) {
        components::settings_label(ui, "Bluetooth");
        components::settings_card(ui, |ui| {
        let status = self.bluetooth.map_or(CHECKING_BLUETOOTH, |caps| caps.status());
        let details_id = ui.id().with("bluetooth-details");
        let mut open = ui.data(|d| d.get_temp::<bool>(details_id).unwrap_or(false));
        let (row, _) = ListRow::new("Bluetooth")
            .icon(Icon::Bluetooth)
            .trailing(Trailing::Value(status.into()))
            .a11y(format!("Bluetooth, {status}. {} details", if open { "Hide" } else { "Show" }))
            .show_with(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(theme::ICON_SMALL, theme::ICON_SMALL), egui::Sense::hover());
                draw_icon(ui.painter(), rect, if open { Icon::ChevronDown } else { Icon::Chevron }, theme::TEXT_SECONDARY);
            });
        if row.clicked() {
            open = !open;
            ui.data_mut(|d| d.insert_temp(details_id, open));
        }
        if open {
            ui.indent("bluetooth-details", |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.add_space(4.0);
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
                ui.add_space(8.0);
            });
        }
        });
        ui.add_space(theme::SECTION_GAP);
        }

        if show(SettingsSection::About) {
        components::settings_label(ui, "About");
        components::settings_card(ui, |ui| {
        let version = concat!("Version ", env!("CARGO_PKG_VERSION"));
        if ListRow::new("About MeshLive")
            .icon(Icon::Info)
            .trailing(Trailing::ValueChevron(version.into()))
            .a11y(concat!("About MeshLive, Version ", env!("CARGO_PKG_VERSION"), " \u{b7} Windows preview"))
            .show(ui)
            .clicked()
        {
            self.dialog = Some((
                "About MeshLive".into(),
                concat!(
                    "MeshLive ",
                    env!("CARGO_PKG_VERSION"),
                    "\nFree software: GPL-3.0-or-later.\n\nThis Windows build is a preview."
                )
                .into(),
            ));
        }
        if ListRow::new("Export diagnostics")
            .icon(Icon::Search)
            .chevron()
            .a11y("Export diagnostics, A text report for a problem report")
            .show(ui)
            .clicked()
        {
            self.backend.command(Command::ExportDiagnostics);
        }
        let preview_id = ui.id().with("preview-diagnostics");
        let mut preview = ui.data(|d| d.get_temp::<bool>(preview_id).unwrap_or(false));
        let (row, _) = ListRow::new("Preview and diagnostics")
            .icon(Icon::Settings)
            .a11y("Preview and diagnostics")
            .show_with(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(theme::ICON_SMALL, theme::ICON_SMALL), egui::Sense::hover());
                draw_icon(ui.painter(), rect, if preview { Icon::ChevronDown } else { Icon::Chevron }, theme::TEXT_SECONDARY);
            });
        if row.clicked() {
            preview = !preview;
            ui.data_mut(|d| d.insert_temp(preview_id, preview));
        }
        if preview {
            let mut demo = self.demo;
            if ToggleRow::new("Show sample people and streams (Experimental)").show(ui, &mut demo).changed() {
                self.set_demo(demo);
            }
            ToggleRow::new("Show GPU texture test").show(ui, &mut self.texture_visible);
            ui.add_space(4.0);
            ui.label(theme::secondary(renderer));
            ui.label(theme::secondary("Ctrl+plus/minus to change text size. Preview choices are never saved."));
        }
        });
        ui.add_space(theme::SECTION_GAP);
        if Pill::new("Delete everything").kind(Kind::Destructive).icon(Icon::Warning).fill_width().show(ui).clicked() {
            self.wipe_confirm = Some(String::new());
        }
        ui.add_space(theme::SECTION_GAP);
        }
        if changed {
            self.switches_changed();
        }
    }
}

/// Hidden until wired (2026-10-21): no part of the PC reads these switches
/// yet, so showing them would promise something that does not happen.
/// Their saved values stay. See the switch register (docs/dev/switches.md).
const HIDDEN_UNTIL_WIRED: [&str; 4] = [
    "Show my name before they answer",
    "Codes that expire when you add a contact",
    "Accept older verification codes",
    "Visible by name",
];
/// "Visible to contacts" (global) is hidden until nearby visibility is wired.
const WIRED_VISIBLE_TO_CONTACTS: bool = false;

/// Why a switch cannot be used, under it (wraps; screen readers read it).
fn gate_reason(ui: &mut egui::Ui, reason: &str) {

    ui.label(theme::secondary(reason));
    ui.add_space(10.0);
}
