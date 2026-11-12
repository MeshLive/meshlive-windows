//! A saved contact's sheet (Android `ContactActions`, `sheet_contact.xml`):
//! the name, how and when they were verified, their ID and full ID, what
//! they call themselves (with the one change note), "Scan to confirm" for a
//! contact checked by code only (or "Show my code" / "Scan again" while this
//! PC can't recognise them), Rename, Safety code (Windows), Block or
//! Unblock, Report and Forget. Android hides the Voice and Video rows (calls
//! start from the contact's row), so this sheet has none. Every row closes
//! the sheet first, as Android's do; Safety code keeps it open to show the
//! digits. A claimed name is never the title and never verifies anyone.
use super::*;
use backend::Command;

// Android strings.xml and strings_ui.xml ("phone" read as "PC" where it
// means this device; "tap" read as "choose").
pub(super) const SCAN_CONFIRM: &str = "Scan to confirm";
pub(super) const SHOW_MY_CODE: &str = "Show my code";
pub(super) const SCAN_AGAIN: &str = "Scan again";
pub(super) const USE_CLAIM: &str = "Use it as my label";
pub(super) const IGNORE_CLAIM: &str = "Ignore";
pub(super) const SCANNED_BOTH_WAYS: &str = "You and this person scanned each other.";

/// Whether this PC can find the contact nearby (Android `ContactReach.Hint`;
/// Windows always has the expiring-code Add contact, so there is no
/// "call once from Nearby" case).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Recognition {
    /// This PC holds their presence lease.
    Recognisable,
    /// No lease, and you have not scanned each other's code.
    AskToScan,
    /// No lease although you scanned each other's code.
    ScanAgain,
}
pub(super) fn recognition(contact: &Contact) -> Recognition {
    if contact.lease {
        Recognition::Recognisable
    } else if contact.both_ways {
        Recognition::ScanAgain
    } else {
        Recognition::AskToScan
    }
}

/// What a row of the sheet asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SheetAction {
    UseClaim,
    IgnoreClaim,
    /// Add contact, on "Show my code" (true) or "Scan their code".
    AddContact { show_code: bool },
    Rename,
    SafetyCode,
    Block,
    Unblock,
    Report,
    Forget,
}

/// Android `contacts_detail` ("%1$s\n\nID %2$s\n\nFull ID\n%3$s"), then
/// "You and this person scanned each other." and the recognition note.
pub(super) fn detail(contact: &Contact) -> String {
    let how = if contact.by_qr { "Verified by QR code" } else { "Verified by safety code" };
    let first = if contact.verified_on.is_empty() { how.to_owned() } else { format!("{how}, {}", contact.verified_on) };
    let mut text = format!("{first}\n\nID {}\n\nFull ID\n{}", contact.handle, contact.full_id);
    if contact.both_ways {
        text.push('\n');
        text.push_str(SCANNED_BOTH_WAYS);
    }
    match recognition(contact) {
        Recognition::AskToScan => text.push_str(&format!("\nAsk {} to scan your code too", contact.name)),
        Recognition::ScanAgain => text.push_str(&format!("\nYour PC does not recognise {} yet", contact.name)),
        Recognition::Recognisable => {}
    }
    text
}

/// Android `contact_claim_calls_themselves` / `contact_claim_changed`.
pub(super) fn claim_text(contact: &Contact) -> Option<String> {
    match &contact.claim {
        None => None,
        Some(ClaimNote::CallsThemselves(name)) => Some(format!("They call themselves {name}")),
        Some(ClaimNote::Changed(name)) => {
            Some(format!("{} now calls themselves {name}. Nothing changes unless you choose Use it.", contact.name))
        }
    }
}

/// The sheet's scan row (Android binds "Scan to confirm", then the
/// recognition hint over the same row): its title and whether Add contact
/// opens on "Show my code".
pub(super) fn scan_row(contact: &Contact) -> Option<(&'static str, bool)> {
    match recognition(contact) {
        Recognition::AskToScan => Some((SHOW_MY_CODE, true)),
        Recognition::ScanAgain => Some((SCAN_AGAIN, false)),
        Recognition::Recognisable if !contact.by_qr => Some((SCAN_CONFIRM, false)),
        Recognition::Recognisable => None,
    }
}

fn sheet_row(ui: &mut egui::Ui, row: ListRow<'_>, chosen: SheetAction, action: &mut Option<SheetAction>) {
    if row.show(ui).clicked() {
        *action = Some(chosen);
    }
}

impl Ui {
    /// Android `ContactActions.show`: the sheet, over the Contacts page only
    /// (a call, Add contact, a stream or another page closes it).
    pub(super) fn contact_sheet(&mut self, ctx: &egui::Context) {
        let Some(id) = self.contact_more else { return };
        let shown = self.page == Page::Contacts
            && self.call.is_none()
            && self.watch.is_none()
            && !self.golive_open
            && self.meet.is_none();
        let Some(contact) = self.contacts.iter().find(|c| c.id == id).cloned().filter(|_| shown) else {
            self.contact_more = None;
            return;
        };
        let mut action = None;
        let response = components::dialog(ctx, "contact-sheet", |ui| {
            action = self.contact_sheet_body(ui, &contact, true);
        });
        if let Some(action) = action {
            self.contact_sheet_action(&contact, action);
        } else if response.should_close() {
            self.contact_more = None;
        }
    }

    /// The sheet's text and rows, in Android's order (also the wide window's
    /// detail pane, which shows the name itself). Returns the chosen row.
    pub(super) fn contact_sheet_body(&self, ui: &mut egui::Ui, contact: &Contact, title: bool) -> Option<SheetAction> {
        let mut action = None;
        if title {
            components::dialog_title(ui, &contact.name);
        }
        ui.add(egui::Label::new(theme::body(detail(contact))).wrap().selectable(true));
        if let Some(code) = self.safety_codes.get(&contact.id) {
            ui.label(RichText::new(code.as_str()).monospace().size(15.0).color(Color32::WHITE));
            ui.add(egui::Label::new(theme::caption(format!("Read these 60 digits with {} in person. Both screens must show exactly the same numbers.", contact.name))).wrap());
        }
        if let Some(text) = claim_text(contact) {
            ui.add(egui::Label::new(theme::body(text)).wrap());
        }
        ui.add_space(4.0);
        if matches!(contact.claim, Some(ClaimNote::Changed(_))) {
            sheet_row(ui, ListRow::new(USE_CLAIM), SheetAction::UseClaim, &mut action);
            sheet_row(ui, ListRow::new(IGNORE_CLAIM), SheetAction::IgnoreClaim, &mut action);
        }
        if let Some((title, show_code)) = scan_row(contact) {
            sheet_row(ui, ListRow::new(title), SheetAction::AddContact { show_code }, &mut action);
        }
        let name = &contact.name;
        sheet_row(ui, ListRow::new("Rename").icon(Icon::Person).a11y(format!("Rename {name}")), SheetAction::Rename, &mut action);
        sheet_row(ui, ListRow::new("Safety code").icon(Icon::Lock).a11y(format!("Safety code with {name}")), SheetAction::SafetyCode, &mut action);
        let (block, chosen) = if contact.blocked { ("Unblock", SheetAction::Unblock) } else { ("Block", SheetAction::Block) };
        sheet_row(ui, ListRow::new(block).icon(Icon::Warning).a11y(format!("{block} {name}")), chosen, &mut action);
        sheet_row(ui, ListRow::new(super::report::REPORT).a11y(format!("Report {name}")), SheetAction::Report, &mut action);
        sheet_row(ui, ListRow::new("Forget").icon(Icon::Close).a11y(format!("Forget {name}")), SheetAction::Forget, &mut action);
        action
    }

    /// A row's action. Android dismisses the sheet before every row's
    /// action; Safety code (Windows only) keeps it open to show the digits.
    pub(super) fn contact_sheet_action(&mut self, contact: &Contact, action: SheetAction) {
        if action != SheetAction::SafetyCode {
            self.contact_more = None;
        }
        match action {
            SheetAction::UseClaim => {
                if let Some(ClaimNote::Changed(name)) = &contact.claim {
                    self.claim_rename = Some((contact.id, name.clone()));
                }
                self.backend.command(Command::ClaimedName { contact: contact.id, use_it: true });
            }
            SheetAction::IgnoreClaim => self.backend.command(Command::ClaimedName { contact: contact.id, use_it: false }),
            SheetAction::AddContact { show_code } => self.open_meet_on(show_code),
            SheetAction::Rename => {
                self.rename = Some((contact.id, contact.name.clone(), None));
                self.focus_primary = true;
            }
            SheetAction::SafetyCode => self.backend.command(Command::ShowSafetyCode(contact.id)),
            // Android asks first (`confirmBlock`); Unblock does not ask.
            SheetAction::Block => {
                self.block_confirm = Some((super::blocked::BlockTarget::Contact(contact.id), contact.name.clone()));
            }
            SheetAction::Unblock => self.send_contact_block(contact.id, false),
            // Android `contactRowReport`: blocks without asking.
            SheetAction::Report => self.report_contact(contact.id),
            SheetAction::Forget => self.forget = Some(contact.id),
        }
    }
}
