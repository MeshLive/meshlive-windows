//! Blocking on this PC (Android `CallBlockingUi`, `BlockedPeopleActivity`,
//! `audioListenBlock`, `declineAndBlockButton`): the words, what a change
//! says, and the one place the screens send a block. Local only: blocking
//! changes no byte on the wire and no admission rule.
use super::*;
use backend::Command;
use meshlive::blocking::BlockChange;

// Android strings_blocking.xml and strings_audio_live.xml ("phone" read as "PC"
// where it means this device; no touch words).
pub(super) const INTRO: &str = "Blocked calls won't ring, and blocked livestreams are hidden on this PC. People aren't notified when you block them.";
pub(super) const EMPTY_TITLE: &str = "No one is blocked";
pub(super) const EMPTY_TEXT: &str = "Block someone from Recent, from a contact, or with Decline and block on an incoming call.";
/// Design decision 2026-11-06: said on Privacy and Blocked people (fail open, CF-CALL-24).
pub(super) const UNREADABLE: &str = "Block list can't be read. People you blocked aren't blocked until it can be read again or you reset it.";
pub(super) const RESET_ACTION: &str = "Reset block list";
pub(super) const RESET_TITLE: &str = "Reset the block list?";
pub(super) const RESET_MESSAGE: &str = "Everyone is unblocked and you can block people again.";
pub(super) const UNBLOCK_MESSAGE: &str = "Their calls will ring again and their livestreams will show in Live.";
pub(super) const BLOCK_MESSAGE: &str = "Their livestreams will be hidden. Calls will be blocked when MeshLive can recognise their phone. Calls using a temporary ID may still ring; turn on Block unknown callers to stop those too. They won't be notified.";
pub(super) const BLOCK_FULL: &str = "The block list is full. Unblock someone first.";
pub(super) const BLOCK_FAILED: &str = "Could not save the block. The block list cannot be read right now.";
pub(super) const STREAM_BLOCKED: &str = "Blocked. Their broadcasts are hidden.";
pub(super) const DECLINE_AND_BLOCK: &str = "Decline and block";
pub(super) const BLOCK_THIS_PERSON: &str = "Block this person";
pub(super) const DECLINED_OFFER: &str = "Declined. Block all unknown callers?";
pub(super) const TURN_ON_UNKNOWN: &str = "Block all unknown callers";
pub(super) const UNKNOWN_TURNED_ON: &str = "Block unknown callers is on";

/// Where a block or unblock was asked for (it decides what the note says).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockFrom {
    /// "Block" after "Block %s?" (Recent, a contact).
    Confirmed,
    DeclineAndBlock,
    /// "Block this person" while watching or listening.
    Stream,
    Unblock,
    /// Report (contact, ring screen, listener, viewer, Block question): a
    /// block that held opens "Blocked and hidden on this PC.".
    Report,
}

/// Who "Block %s?" is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockTarget {
    Caller([u8; 32]),
    Contact(Handle),
}

/// What the screen says after a change (Android's toasts).
pub(super) fn change_note(from: BlockFrom, change: BlockChange, name: &str) -> String {
    match (from, change) {
        (_, BlockChange::Full) => BLOCK_FULL.to_owned(),
        (_, BlockChange::Failed) => BLOCK_FAILED.to_owned(),
        (BlockFrom::Unblock, _) => format!("{name} unblocked"),
        (BlockFrom::DeclineAndBlock, _) => format!("Declined and blocked {name}"),
        (BlockFrom::Stream, _) => STREAM_BLOCKED.to_owned(),
        (BlockFrom::Report, _) => super::report::REPORT_DONE.to_owned(),
        (BlockFrom::Confirmed, BlockChange::AlreadyBlocked) => format!("{name} is already blocked"),
        (BlockFrom::Confirmed, _) => format!("{name} blocked"),
    }
}

/// A Blocked people row's second line ("Blocked Nov 6, 2026"); None when the
/// time is unknown (a list from before 2026-11).
pub(super) fn since_text(since_ms: i64) -> Option<String> {
    (since_ms > 0).then(|| format!("Blocked {}", meshlive::blocking::date_text(meshlive::blocking::local_ms(since_ms))))
}

impl Ui {
    /// Blocks or unblocks a person by fingerprint; Recent rows follow the
    /// saved outcome (`Update::BlockChanged`), not the tap.
    pub(super) fn send_block(&mut self, fingerprint: [u8; 32], blocked: bool, from: BlockFrom) {
        self.block_from = Some(from);
        self.backend.command(Command::BlockCaller { fingerprint, blocked });
    }
    /// Blocks or unblocks a verified contact.
    pub(super) fn send_contact_block(&mut self, contact: Handle, blocked: bool) {
        self.block_from = Some(if blocked { BlockFrom::Confirmed } else { BlockFrom::Unblock });
        self.backend.command(Command::SetBlocked { contact, blocked });
    }
}

impl Ui {
    /// Settings > Privacy > Blocked people (Android `BlockedPeopleActivity`):
    /// everyone blocked on this PC by your label for a contact or their ID,
    /// with when; a row asks, then unblocks. An unreadable list offers Reset.
    pub(super) fn settings_blocked(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::Label::new(theme::caption(INTRO)).wrap());
        ui.add_space(8.0);
        if self.blocked_people.unreadable {
            if components::notice(ui, UNREADABLE, Some(RESET_ACTION)) {
                self.reset_blocks_confirm = true;
            }
            ui.add_space(theme::SECTION_GAP);
            return;
        }
        if self.blocked_people.people.is_empty() {
            components::empty_state(ui, Icon::Person, EMPTY_TITLE, EMPTY_TEXT);
            return;
        }
        let people = self.blocked_people.people.clone();
        let mut chosen = None;
        components::settings_card(ui, |ui| {
            for person in &people {
                let since = since_text(person.since_ms);
                let mut row = ListRow::new(&person.name).icon(Icon::Person).a11y(format!("{}, blocked", person.name));
                if let Some(since) = since.as_deref() {
                    row = row.subtitle(since);
                }
                if row.show(ui).clicked() {
                    chosen = Some((person.fingerprint, person.name.clone()));
                }
            }
        });
        if let Some(choice) = chosen {
            self.unblock_confirm = Some(choice);
        }
        ui.add_space(theme::SECTION_GAP);
    }

    /// Block %s? (Android `confirmBlock`), Unblock %s? and Reset the block list?
    pub(super) fn blocking_dialogs(&mut self, ctx: &egui::Context) {
        use components::{dialog, dialog_buttons, dialog_text, dialog_title};
        if self.block_unknown_offer {
            let (mut turn_on, mut not_now) = (false, false);
            dialog(ctx, "block-unknown-offer", |ui| {
                dialog_text(ui, DECLINED_OFFER);
                (turn_on, not_now) = dialog_buttons(ui, TURN_ON_UNKNOWN, Kind::Primary, true, Some("Not now"));
            });
            if turn_on {
                self.block_unknown_offer = false;
                self.switches.block_unknown = true;
                self.switches_changed();
                self.block_note = Some(UNKNOWN_TURNED_ON.into());
            } else if not_now {
                // Remembered (saved): never asked again.
                self.block_unknown_offer = false;
                self.switches.block_unknown_offer_dismissed = true;
                self.switches_changed();
            }
        }
        if let Some((target, name)) = self.block_confirm.clone() {
            let (mut go, mut cancel, mut report) = (false, false, false);
            dialog(ctx, "block-person", |ui| {
                dialog_title(ui, &format!("Block {name}?"));
                dialog_text(ui, BLOCK_MESSAGE);
                // Android `confirmBlock`: Report is the neutral button (left).
                (report, cancel, go) = super::report::buttons_with_neutral(ui, super::report::REPORT, Some("Cancel"), "Block");
                cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
            if go {
                self.block_confirm = None;
                match target {
                    BlockTarget::Caller(fingerprint) => self.send_block(fingerprint, true, BlockFrom::Confirmed),
                    BlockTarget::Contact(contact) => self.send_contact_block(contact, true),
                }
            } else if report {
                self.block_confirm = None;
                match target {
                    BlockTarget::Caller(fingerprint) => self.report_person(fingerprint),
                    BlockTarget::Contact(contact) => self.report_contact(contact),
                }
            } else if cancel {
                self.block_confirm = None;
            }
        }
        if let Some((fingerprint, name)) = self.unblock_confirm.clone() {
            let (mut go, mut cancel) = (false, false);
            dialog(ctx, "unblock-person", |ui| {
                dialog_title(ui, &format!("Unblock {name}?"));
                dialog_text(ui, UNBLOCK_MESSAGE);
                (go, cancel) = dialog_buttons(ui, "Unblock", Kind::Primary, true, Some("Cancel"));
            });
            if go {
                self.unblock_confirm = None;
                self.send_block(fingerprint, false, BlockFrom::Unblock);
            } else if cancel {
                self.unblock_confirm = None;
            }
        }
        if self.reset_blocks_confirm {
            let (mut go, mut cancel) = (false, false);
            dialog(ctx, "reset-block-list", |ui| {
                dialog_title(ui, RESET_TITLE);
                dialog_text(ui, RESET_MESSAGE);
                (go, cancel) = dialog_buttons(ui, "Reset", Kind::Primary, true, Some("Cancel"));
            });
            if go {
                self.reset_blocks_confirm = false;
                self.backend.command(Command::ResetBlocked);
            } else if cancel {
                self.reset_blocks_confirm = false;
            }
        }
    }
}
