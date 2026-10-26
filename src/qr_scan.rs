//! Add contact's "Scan their code" by webcam: the first camera is opened
//! only while that step asks for it (`QrScan` lives exactly as long; drop it
//! when the step closes, a code is read or a scan starts), its pictures are
//! decoded as QR codes, and the first BVMID2 text found goes into the same
//! path as a typed or pasted code (the scan, the verification and "Is this
//! X's screen you scanned?"). Pictures are never kept: each one is dropped
//! right after its decode attempt, nothing is written or traced but the
//! outcome. A camera Windows privacy settings deny is reported with the
//! plain reason (the screen offers the camera settings page), never worked
//! around.
//!
//! The QR decoder is behind `Decoder`: `Rqrr` (the pure-Rust `rqrr` crate,
//! MIT OR Apache-2.0 AND ISC, a port of quirc). Typing or pasting the code
//! stays available.
use crate::capture::{Camera, Error as CameraError, camera_privacy_facts};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The capture size asked of the camera: a phone's v6 code (41 modules)
/// held 15-40 cm away spans 150-500 pixels at this size.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// At most this often a picture is decoded (the CPU stays free for the UI).
pub const DECODE_INTERVAL: Duration = Duration::from_millis(150);

/// Finds QR codes in an 8-bit greyscale picture (row-major, `width` per row).
pub trait Decoder: Send {
    /// The text of every QR code read in the picture.
    fn decode(&mut self, luma: &[u8], width: usize, height: usize) -> Vec<String>;
}

/// Finds nothing (no decoder crate yet).
#[derive(Default)]
pub struct NoDecoder;
impl Decoder for NoDecoder {
    fn decode(&mut self, _: &[u8], _: usize, _: usize) -> Vec<String> {
        Vec::new()
    }
}

/// `rqrr`: finds and decodes every QR code in the picture.
#[derive(Default)]
pub struct Rqrr;
impl Decoder for Rqrr {
    fn decode(&mut self, luma: &[u8], width: usize, height: usize) -> Vec<String> {
        if width == 0 || luma.len() < width * height {
            return Vec::new();
        }
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| luma[y * width + x]);
        image.detect_grids().into_iter().filter_map(|grid| grid.decode().ok().map(|(_, text)| text)).collect()
    }
}

/// The decoder the app uses.
pub fn default_decoder() -> Box<dyn Decoder> {
    Box::new(Rqrr)
}

/// What the scanner reports to the Add contact screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The camera is on and looking for a code.
    On,
    /// No camera can be used: the plain reason (`privacy` says whether the
    /// camera privacy settings page is the way to fix it).
    Unavailable { reason: String, privacy: bool },
    /// A QR code was read but it is not a MeshLive code (reported once per
    /// distinct text; scanning continues).
    NotACode,
    /// A MeshLive code (BVMID2 text, already parsed once). The camera is closed.
    Code(String),
}

/// The text the user reads when the camera cannot be used, and whether the
/// privacy settings page is the fix (pure).
pub fn reason(native: Option<i32>, privacy: &[String]) -> (String, bool) {
    let denied = privacy.iter().any(|l| l.contains("value=Deny"));
    if denied || native == Some(0x80070005u32 as i32) {
        ("Camera access is off for desktop apps in Windows privacy settings, so this PC can't scan their code. Turn it on, or paste their code instead.".into(), true)
    } else if native.is_none() {
        ("No camera was found on this PC. Paste their code instead.".into(), false)
    } else {
        ("The camera couldn't be started (another app may be using it). Close that app, or paste their code instead.".into(), false)
    }
}

/// The text to scan, if `text` is a MeshLive code (pure; the same parser as
/// a typed code, so a code the scan path refuses is never reported).
pub fn code_text(text: &str) -> Option<String> {
    let text = text.trim();
    crate::bvmid2::Code::parse(text).ok().map(|_| text.to_owned())
}

/// The luma plane of a packed NV12 picture (pure; None when too short).
pub fn luma(nv12: &[u8], width: usize, height: usize) -> Option<&[u8]> {
    nv12.get(..width.checked_mul(height)?)
}

/// `luma` mirrored left to right (some webcam drivers deliver mirrored
/// pictures, and a mirrored QR code never decodes).
pub fn mirrored(luma: &[u8], width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(luma.len());
    for row in luma.chunks_exact(width) {
        out.extend(row.iter().rev());
    }
    out
}

/// One decode attempt: tries the picture as delivered and, every other
/// attempt, mirrored. Returns the first MeshLive code and whether some other
/// QR code was seen.
pub fn attempt(decoder: &mut dyn Decoder, luma: &[u8], width: usize, height: usize, mirror: bool) -> (Option<String>, Option<String>) {
    let texts = if mirror { decoder.decode(&mirrored(luma, width), width, height) } else { decoder.decode(luma, width, height) };
    let mut other = None;
    for text in texts {
        if let Some(code) = code_text(&text) {
            return (Some(code), None);
        }
        other.get_or_insert(text);
    }
    (None, other)
}

/// The scanner thread of the last closed `QrScan`s: a new scan waits for
/// them so the camera is free before it is opened again (closing never
/// blocks the UI).
static CLOSING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// The webcam open for Add contact's "Scan their code".
pub struct QrScan {
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl QrScan {
    pub fn start(decoder: Box<dyn Decoder>) -> Self {
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let previous: Vec<JoinHandle<()>> = std::mem::take(&mut *CLOSING.lock().unwrap());
        let thread = std::thread::Builder::new()
            .name("meshlive-qr-scan".into())
            .spawn(move || {
                for handle in previous {
                    let _ = handle.join();
                }
                run(decoder, tx, flag)
            })
            .ok();
        Self { events, stop, thread }
    }
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
    /// Closes the camera and waits until it is closed (app exit).
    pub fn close_and_wait(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for QrScan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let mut closing = CLOSING.lock().unwrap();
            closing.retain(|t| !t.is_finished());
            closing.push(thread);
        }
    }
}

fn run(mut decoder: Box<dyn Decoder>, events: Sender<Event>, stop: Arc<AtomicBool>) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    let mut camera = match Camera::open_first(WIDTH, HEIGHT) {
        Ok(camera) => camera,
        Err(error) => {
            let native = match error {
                CameraError::Native(code) => Some(code),
                _ => None,
            };
            let (reason, privacy) = reason(native, &camera_privacy_facts());
            let _ = events.send(Event::Unavailable { reason, privacy });
            return;
        }
    };
    let _ = events.send(Event::On);
    let (width, height) = (camera.width as usize, camera.height as usize);
    let mut mirror = false;
    let mut last_other: Option<String> = None;
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let picture = match camera.next_frame(Instant::now() + Duration::from_millis(200)) {
            Ok(picture) => picture,
            Err(CameraError::Timeout) => continue,
            Err(_) => {
                let (reason, privacy) = reason(Some(0), &camera_privacy_facts());
                let _ = events.send(Event::Unavailable { reason, privacy });
                return;
            }
        };
        if Instant::now() < next {
            continue; // dropped unread
        }
        next = Instant::now() + DECODE_INTERVAL;
        let Some(plane) = luma(&picture, width, height) else { continue };
        let (code, other) = attempt(decoder.as_mut(), plane, width, height, mirror);
        drop(picture);
        mirror = !mirror;
        if let Some(code) = code {
            let _ = events.send(Event::Code(code));
            return; // the camera closes here
        }
        if let Some(other) = other {
            if last_other.as_deref() != Some(other.as_str()) {
                let _ = events.send(Event::NotACode);
                last_other = Some(other);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qr_encode::QrCode;

    /// A picture of `qr` as a phone screen shows it: `scale` pixels per
    /// module, a 4-module white quiet zone, centred in a grey frame.
    pub fn render(qr: &QrCode, scale: usize, width: usize, height: usize) -> Vec<u8> {
        let side = (qr.size + 8) * scale;
        assert!(side <= width && side <= height);
        let (ox, oy) = ((width - side) / 2, (height - side) / 2);
        let mut luma = vec![128u8; width * height];
        for y in 0..side {
            for x in 0..side {
                let (mx, my) = ((x / scale) as isize - 4, (y / scale) as isize - 4);
                let dark = mx >= 0 && my >= 0 && (mx as usize) < qr.size && (my as usize) < qr.size && qr.get(mx as usize, my as usize);
                luma[(oy + y) * width + ox + x] = if dark { 20 } else { 235 };
            }
        }
        luma
    }

    fn sample_code() -> String {
        format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789")
    }

    /// Returns the given texts for every picture, recording the pictures' first pixel.
    struct Fixed(Vec<String>, Vec<u8>);
    impl Decoder for Fixed {
        fn decode(&mut self, luma: &[u8], _: usize, _: usize) -> Vec<String> {
            self.1.push(luma[0]);
            self.0.clone()
        }
    }

    #[test]
    fn only_a_parsable_bvmid2_code_is_scanned() {
        assert_eq!(code_text(&format!("  {}\n", sample_code())), Some(sample_code()));
        assert_eq!(code_text("https://example.com"), None);
        assert_eq!(code_text("BVMID2.ABC"), None);
        assert_eq!(code_text(&sample_code().replace("BVMID2", "BVMID3")), None);
        // An all-zero secret is refused by the scan path, so never reported.
        assert_eq!(code_text(&format!("BVMID2.{}.{}.{}", "AB".repeat(32), "00".repeat(32), "EF0123456789")), None);
    }

    #[test]
    fn the_first_meshlive_code_wins_and_other_codes_are_reported() {
        let mut decoder = Fixed(vec!["WIFI:S:x;;".into(), sample_code()], vec![]);
        assert_eq!(attempt(&mut decoder, &[1, 2, 3, 4], 2, 2, false), (Some(sample_code()), None));
        let mut decoder = Fixed(vec!["WIFI:S:x;;".into()], vec![]);
        assert_eq!(attempt(&mut decoder, &[1, 2, 3, 4], 2, 2, false), (None, Some("WIFI:S:x;;".into())));
        let mut decoder = NoDecoder;
        assert_eq!(attempt(&mut decoder, &[1, 2, 3, 4], 2, 2, false), (None, None));
    }

    #[test]
    fn a_mirrored_attempt_flips_each_row() {
        assert_eq!(mirrored(&[1, 2, 3, 4, 5, 6], 3), vec![3, 2, 1, 6, 5, 4]);
        let mut decoder = Fixed(vec![], vec![]);
        attempt(&mut decoder, &[1, 2, 3, 4], 2, 2, true);
        attempt(&mut decoder, &[1, 2, 3, 4], 2, 2, false);
        assert_eq!(decoder.1, vec![2, 1]);
    }

    #[test]
    fn the_decoder_finds_nothing_in_a_blank_or_short_picture() {
        let mut decoder = Rqrr;
        assert!(decoder.decode(&vec![200u8; 640 * 480], 640, 480).is_empty());
        assert!(decoder.decode(&[0u8; 10], 640, 480).is_empty());
    }

    #[test]
    fn the_luma_plane_is_the_start_of_nv12() {
        let nv12: Vec<u8> = (0..24).collect();
        assert_eq!(luma(&nv12, 4, 4), Some(&nv12[..16]));
        assert_eq!(luma(&nv12[..10], 4, 4), None);
    }

    #[test]
    fn the_reason_follows_privacy_and_the_native_error() {
        let deny = vec!["fact=camera_privacy scope=user_desktop_apps value=Deny".to_owned()];
        let (text, privacy) = reason(Some(1), &deny);
        assert!(text.contains("privacy settings") && privacy);
        assert!(reason(Some(0x80070005u32 as i32), &[]).1);
        let (text, privacy) = reason(None, &[]);
        assert!(text.contains("No camera") && !privacy);
        let (text, privacy) = reason(Some(-1), &[]);
        assert!(text.contains("another app") && !privacy);
        for native in [None, Some(-1), Some(0x80070005u32 as i32)] {
            assert!(reason(native, &[]).0.to_lowercase().contains("paste their code"));
        }
    }

    #[test]
    fn the_test_picture_holds_the_encoders_modules() {
        let qr = QrCode::encode(&sample_code()).unwrap();
        let luma = render(&qr, 4, 640, 480);
        let side = (qr.size + 8) * 4;
        let (ox, oy) = ((640 - side) / 2, (480 - side) / 2);
        // The top-left finder's corner module is dark; the quiet zone is light.
        assert_eq!(luma[(oy + 16) * 640 + ox + 16], 20);
        assert_eq!(luma[(oy + 2) * 640 + ox + 2], 235);
    }

    /// The repo's own encoder output read back through the app's decoder,
    /// as delivered and mirrored (needs the decoder crate).
    #[test]
    fn the_decoder_reads_this_pcs_own_qr_code() {
        let code = sample_code();
        let qr = QrCode::encode(&code).unwrap();
        for scale in [3, 5, 8] {
            let luma = render(&qr, scale, 1280, 720);
            let mut decoder = default_decoder();
            assert_eq!(attempt(decoder.as_mut(), &luma, 1280, 720, false).0, Some(code.clone()), "scale {scale}");
            let flipped = mirrored(&luma, 1280);
            assert_eq!(attempt(decoder.as_mut(), &flipped, 1280, 720, true).0, Some(code.clone()), "mirrored, scale {scale}");
        }
    }
}
