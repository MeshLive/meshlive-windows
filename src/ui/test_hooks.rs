//! Test-only, compiled only with `--features test-hooks` (never in the
//! release app): lets an unattended phone run make the app place or answer
//! a call through its normal UI path, so call traces (MESHLIVE_TRACE) can be
//! collected without clicks.
//!
//! - `MESHLIVE_TEST_DIAL=<fp12>`: once, call the verified contact whose
//!   fingerprint starts with these 12 hex digits (resolved by the contact
//!   backend); video unless `MESHLIVE_TEST_VOICE=1`.
//! - `MESHLIVE_TEST_AUTOANSWER=1`: accept an incoming call 2 s after it rings
//!   (video unless the caller offered voice); `MESHLIVE_TEST_AUTOANSWER_DELAY_S`
//!   (0..=70) changes the 2 s, e.g. 38 to repeat a long ring.
//! - `MESHLIVE_TEST_BLOCK=<fp12>`: block the verified contact whose
//!   fingerprint starts with these 12 hex digits at start (a call from it then
//!   gets the silent decline).
//! - `MESHLIVE_TEST_UNBLOCK=<fp12>`: unblock that contact at start (the block
//!   list is the real one, so a test undoes its own block with this).
//! - `MESHLIVE_TEST_BLOCK_UNKNOWN=1`: "Block unknown callers" for this run
//!   only (never saved), so a stranger's call gets the silent decline.
//! - `MESHLIVE_TEST_DIAL_STRANGER=<rdi12>`: open Calls > Nearby and call the
//!   nearby phone whose current rotating ID is these 12 hex digits, once, through
//!   the People list (video unless `MESHLIVE_TEST_VOICE=1`).
//! - `MESHLIVE_TEST_MINIMIZE=1`: start minimised (main.rs), so an incoming
//!   call shows the taskbar flash and the toast (traced toast_shown/failed).
//! - `MESHLIVE_TEST_HANGUP_S=<s>`: hang up that long after the call is answered
//!   (default 90, at most 115: keeps a test camera on for under 2 minutes).
//! - `MESHLIVE_TEST_EXIT=1`: close the app 3 s after the call ended.
//! - `MESHLIVE_TEST_PREVIEW=incoming|voice|incall|ended|recent|home|golive|nearby|contacts|meet`: for UI
//!   screenshots without a radio (use with `--offline`): shows that call
//!   screen (or Calls > Recent) with fake data; nothing is sent anywhere.
use super::{Phase, Ui};
use std::time::{Duration, Instant};

pub struct State {
    dial: Option<(super::model::Handle, Instant)>,
    voice: bool,
    autoanswer: bool,
    answer_after: Duration,
    hangup: Duration,
    exit: bool,
    ringing_since: Option<Instant>,
    answered_at: Option<Instant>,
    ended_at: Option<Instant>,
    /// Calls > Nearby was opened for MESHLIVE_TEST_DIAL_STRANGER.
    opened_nearby: bool,
    /// MESHLIVE_TEST_PREVIEW was applied (once), and its fake picture.
    previewed: bool,
    preview_picture: Option<egui::TextureHandle>,
}
fn flag(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}
impl Default for State {
    fn default() -> Self {
        let hangup = std::env::var("MESHLIVE_TEST_HANGUP_S").ok().and_then(|v| v.parse().ok()).unwrap_or(90u64).min(115);
        Self {
            dial: None,
            voice: flag("MESHLIVE_TEST_VOICE"),
            autoanswer: flag("MESHLIVE_TEST_AUTOANSWER"),
            answer_after: Duration::from_secs(
                std::env::var("MESHLIVE_TEST_AUTOANSWER_DELAY_S").ok().and_then(|v| v.parse().ok()).unwrap_or(2u64).min(70),
            ),
            hangup: Duration::from_secs(hangup),
            exit: flag("MESHLIVE_TEST_EXIT"),
            ringing_since: None,
            answered_at: None,
            ended_at: None,
            opened_nearby: false,
            previewed: false,
            preview_picture: None,
        }
    }
}

fn hex12(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_ascii_lowercase()).filter(|v| v.len() == 12 && v.chars().all(|c| c.is_ascii_hexdigit()))
}
/// The contact to block at start (12 hex digits of its fingerprint).
pub fn block_target() -> Option<String> {
    hex12("MESHLIVE_TEST_BLOCK")
}
/// `MESHLIVE_TEST_BLOCK_UNKNOWN=1`: "Block unknown callers" for this run
/// only (the Bluetooth owner applies it; the saved settings never change).
pub fn block_unknown() -> bool {
    flag("MESHLIVE_TEST_BLOCK_UNKNOWN")
}
/// The contact to unblock at start (undoes a test's own block).
pub fn unblock_target() -> Option<String> {
    hex12("MESHLIVE_TEST_UNBLOCK")
}
/// The stranger to dial (12 hex digits of its current rotating ID).
pub fn stranger_target() -> Option<String> {
    hex12("MESHLIVE_TEST_DIAL_STRANGER")
}

/// The 12-hex-digit fingerprint prefix to dial, if any.
pub fn dial_target() -> Option<String> {
    std::env::var("MESHLIVE_TEST_DIAL").ok().map(|v| v.trim().to_ascii_lowercase()).filter(|v| v.len() == 12)
}

impl Ui {
    pub(super) fn test_dial(&mut self, contact: super::model::Handle) {
        crate::radio_backend::trace("test_hook", &format!("dial contact={contact} voice={}", self.test.voice));
        self.test.dial = Some((contact, Instant::now()));
    }
    /// MESHLIVE_TEST_PREVIEW: a call screen (or Recent) with fake data.
    fn preview(&mut self, ctx: &egui::Context) {
        use super::backend::Update;
        use meshlive::call::Signal;
        if self.test.previewed {
            return;
        }
        self.test.previewed = true;
        let Ok(kind) = std::env::var("MESHLIVE_TEST_PREVIEW") else { return };
        self.bluetooth = Some(meshlive::bluetooth::Capabilities {
            radio: meshlive::bluetooth::Radio::On,
            low_energy: Some(true),
            central: Some(true),
            peripheral: Some(true),
            extended_advertising: None,
            max_advertisement_bytes: None,
            error: None,
        });
        self.peripheral_proof = meshlive::bluetooth::gate::PeripheralProof::Started;
        self.switches.receive_calls = true;
        self.switches.onboarded = true;
        if matches!(kind.as_str(), "home" | "golive" | "nearby" | "contacts" | "meet") {
            // The sample people (Settings > Show sample people), on that screen.
            self.set_demo(true);
            self.page = match kind.as_str() {
                "contacts" => super::Page::Contacts,
                "home" | "golive" => super::Page::Home,
                _ => super::Page::Calls,
            };
            if kind == "golive" {
                self.open_go_live();
            }
            self.recent_tab = false;
            if kind == "meet" {
                self.open_meet();
                if let Some(meet) = self.meet.as_mut() {
                    meet.own_code = Some("BVMID2.PREVIEW-ONLY-NOT-A-REAL-CODE-0123456789ABCDEFGHJKMNPQRSTVWXYZ".into());
                }
            }
            return;
        }
        if kind == "recent" {
            self.page = super::Page::Calls;
            self.recent_tab = true;
            for (peer, voice, outgoing, result) in [
                ("Asha", false, true, "Call ended"),
                ("Blue Otter", true, false, "Declined"),
                ("Ravi", true, true, "Busy"),
            ] {
                self.recent.push(super::RecentCall {
                    peer: peer.into(),
                    voice,
                    outgoing,
                    result: result.into(),
                    contact: None,
                    fingerprint: (!outgoing).then_some([7; 32]),
                    blocked: false,
                });
            }
            return;
        }
        let id = [0x5a; 16];
        let remote = super::model::remote;
        self.apply(Update::IncomingCall { peer: "Asha".into(), verified: true, invite: remote(id, Signal::Invite), fingerprint: None });
        if kind == "voice" {
            self.apply(Update::CallSignal(remote(id, Signal::CameraOff)));
        }
        if matches!(kind.as_str(), "incall" | "ended") {
            self.with_call(|c| {
                let _ = c.accept(false);
            });
            self.apply(Update::CallMediaReady);
            self.apply(Update::CallMic(None));
            self.apply(Update::CallCamera(None));
            // A neutral grey ramp stands in for the peer's picture.
            let (w, h) = (640usize, 360usize);
            let pixels = (0..w * h)
                .map(|i| {
                    let v = 40 + ((i % w) * 60 / w + (i / w) * 40 / h) as u8;
                    egui::Color32::from_gray(v)
                })
                .collect();
            let image = egui::ColorImage::new([w, h], pixels);
            let texture = ctx.load_texture("preview-peer", image, egui::TextureOptions::LINEAR);
            self.video = Some(texture.id());
            self.peer_size = [w as u32, h as u32];
            self.test.preview_picture = Some(texture);
        }
        if kind == "ended" {
            self.with_call(|c| {
                let _ = c.end();
            });
        }
    }
    pub(super) fn test_hooks_tick(&mut self, ctx: Option<&egui::Context>) {
        if let Some(ctx) = ctx {
            ctx.request_repaint_after(Duration::from_millis(200));
            self.preview(ctx);
        }
        // The People list scans only while it is on screen.
        if !self.test.opened_nearby && stranger_target().is_some() {
            self.test.opened_nearby = true;
            self.page = super::Page::Calls;
            self.recent_tab = false;
            crate::radio_backend::trace("test_hook", "open Calls > Nearby for a stranger dial");
        }
        if let Some((contact, since)) = self.test.dial {
            if self.call.is_some() || self.place_call(contact, self.test.voice) {
                self.test.dial = None;
            } else if since.elapsed() > Duration::from_secs(60) {
                crate::radio_backend::trace("test_hook", "dial gave up: calling stayed unavailable for 60 s");
                self.test.dial = None;
            }
        }
        let phase = self.call.as_ref().map(|c| c.phase());
        match phase {
            Some(Phase::IncomingRinging) if self.test.autoanswer => {
                let since = *self.test.ringing_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= self.test.answer_after {
                    let voice = self.call.as_ref().is_some_and(|c| c.voice);
                    crate::radio_backend::trace("test_hook", &format!("answer voice={voice} after_ms={}", since.elapsed().as_millis()));
                    self.with_call(|c| {
                        let _ = c.accept(voice);
                    });
                    self.test.ringing_since = None;
                }
            }
            Some(Phase::Active | Phase::NegotiatingMedia) => {
                let at = *self.test.answered_at.get_or_insert_with(Instant::now);
                if at.elapsed() >= self.test.hangup {
                    crate::radio_backend::trace("test_hook", &format!("hang up after {} s", self.test.hangup.as_secs()));
                    self.with_call(|c| {
                        let _ = c.end();
                    });
                }
            }
            Some(Phase::Ended) => {
                self.test.answered_at = None;
                let at = *self.test.ended_at.get_or_insert_with(Instant::now);
                if self.test.exit && at.elapsed() >= Duration::from_secs(3) {
                    crate::radio_backend::trace("test_hook", "exit");
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    }
}
