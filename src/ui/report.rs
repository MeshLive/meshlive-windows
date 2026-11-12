//! Report and Contact us (Android `CallBlockingUi.report`, `LocalReportPolicy`,
//! `ContactEmailUi`, `strings_reporting.xml`). Report blocks a person on this
//! PC without asking (the same path as Block) and then offers "Tell MeshLive",
//! an email draft the user reviews and sends; Contact us and Report a bug open
//! the same kind of draft; when no email app takes one, the address is shown
//! to copy. A draft never carries a name, ID, key or code
//! (`meshlive::reporting` takes none). MeshLive sends nothing itself.
use super::*;
use backend::Command;
use meshlive::reporting::{ADDRESS, Draft};

// Android strings_reporting.xml and strings.xml ("phone" read as "PC").
pub(super) const REPORT: &str = "Report";
pub(super) const REPORT_DONE: &str = "Blocked and hidden on this PC.";
pub(super) const TELL_MESHLIVE: &str = "Tell MeshLive";
pub(super) const CONTACT_US: &str = "Contact us";
pub(super) const CONTACT_US_SUBTITLE: &str = "Questions, problems or safety concerns";
pub(super) const REPORT_BUG: &str = "Report a bug";
pub(super) const COPIED: &str = "Copied";

/// Android's dialog buttons with a neutral one (Material: the neutral on the
/// left, Cancel and the main action on the right), all text buttons as
/// `components::dialog_buttons`. Returns (neutral, cancel, main). Escape is
/// the caller's.
pub(super) fn buttons_with_neutral(ui: &mut egui::Ui, neutral: &str, cancel: Option<&str>, main: &str) -> (bool, bool, bool) {
    ui.add_space(8.0);
    let (mut n, mut c, mut m) = (false, false, false);
    ui.horizontal(|ui| {
        n = Pill::new(neutral).kind(Kind::Quiet).min_width(64.0).height(48.0).show(ui).clicked();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            m = Pill::new(main).kind(Kind::Quiet).min_width(64.0).height(48.0).show(ui).clicked();
            if let Some(cancel) = cancel {
                c = Pill::new(cancel).kind(Kind::Quiet).min_width(64.0).height(48.0).show(ui).clicked();
            }
        });
    });
    (n, c, m)
}

impl Ui {
    /// Report someone known by their full fingerprint (the ring screen, the
    /// listener and viewer, Recent): blocks without asking (Android
    /// `CallBlockingUi.report`); the dialog follows the backend's answer.
    pub(super) fn report_person(&mut self, fingerprint: [u8; 32]) {
        self.send_block(fingerprint, true, super::blocked::BlockFrom::Report);
    }
    /// Report a verified contact (Android `contactRowReport`).
    pub(super) fn report_contact(&mut self, contact: Handle) {
        self.block_from = Some(super::blocked::BlockFrom::Report);
        self.backend.command(Command::SetBlocked { contact, blocked: true });
    }
    /// Contact us, Report a bug and Tell MeshLive: the backend opens the draft.
    pub(super) fn open_mail_draft(&mut self, draft: Draft) {
        self.backend.command(Command::OpenMailDraft(draft));
    }

    /// "Blocked and hidden on this PC." with Tell MeshLive and Close (Android
    /// `report_done`), and the address to copy when no email app opened a
    /// draft (Android `ContactEmailUi`'s fallback).
    pub(super) fn report_dialogs(&mut self, ctx: &egui::Context) {
        use components::{dialog, dialog_buttons, dialog_text, dialog_title};
        if self.report_done {
            let (mut tell, mut close) = (false, false);
            dialog(ctx, "report-done", |ui| {
                dialog_text(ui, REPORT_DONE);
                (tell, _, close) = buttons_with_neutral(ui, TELL_MESHLIVE, None, "Close");
                close |= ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
            if tell {
                self.report_done = false;
                self.open_mail_draft(Draft::Concern);
            } else if close {
                self.report_done = false;
            }
        }
        if self.mail_fallback {
            let (mut copy, mut close) = (false, false);
            dialog(ctx, "contact-us-address", |ui| {
                dialog_title(ui, CONTACT_US);
                dialog_text(ui, ADDRESS);
                (copy, close) = dialog_buttons(ui, "Copy", Kind::Primary, true, Some("Close"));
            });
            if copy {
                self.mail_fallback = false;
                // The window puts it on the clipboard (as Add contact's Copy code).
                self.clipboard_request = Some(ADDRESS.to_owned());
                self.block_note = Some(COPIED.to_owned());
            } else if close {
                self.mail_fallback = false;
            }
        }
    }
}
