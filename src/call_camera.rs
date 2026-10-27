//! The app's call camera: one dedicated thread owns the first camera
//! (Media Foundation needs a single-threaded COM apartment, which the
//! Bluetooth worker's multithreaded one cannot host) and hands the newest
//! NV12 picture at the call's size to the video sender. It opens only when a
//! video call's media runs with the camera on, and closes when the camera is
//! turned off or the call ends (this owner is dropped): never outside a call.
//! Windows privacy is respected: a denied camera is reported with the plain
//! reason, never worked around.
use crate::capture::{Camera, camera_privacy_facts};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The camera is on (capturing for this call).
    On,
    /// No camera can be used: the plain reason.
    Unavailable(String),
}

/// NV12 plane means (Y, U, V), 0..255.
pub fn plane_means(nv12: &[u8], width: usize, height: usize) -> (f64, f64, f64) {
    let luma = (width * height).min(nv12.len());
    let y = crate::capture::mean_luma(nv12, width, height);
    let chroma = &nv12[luma..];
    let (mut u, mut v, mut n) = (0f64, 0f64, 0f64);
    for pair in chroma.chunks_exact(2) {
        u += f64::from(pair[0]);
        v += f64::from(pair[1]);
        n += 1.0;
    }
    if n == 0.0 { (y, 128.0, 128.0) } else { (y, u / n, v / n) }
}

/// Android `LeadingBlankFrames` (same constants): drop the camera's leading
/// all-black buffers (Y below 1, U and V within 1 of 128) before the encoder,
/// at most 15 frames or 1 s; the first real picture ends the check.
/// Experimental, off (`MESHLIVE_DROP_LEADING_BLANK_FRAMES=1`), as on Android.
pub struct LeadingBlankFrames {
    done: bool,
    first: Option<Instant>,
    pub dropped: u32,
}
impl LeadingBlankFrames {
    pub const MAX_FRAMES: u32 = 15;
    pub const MAX_MS: u128 = 1_000;
    pub const LUMA_MAX: f64 = 1.0;
    pub const CHROMA_TOLERANCE: f64 = 1.0;
    pub fn new(enabled: bool) -> Self {
        Self { done: !enabled, first: None, dropped: 0 }
    }
    pub fn is_blank(y: f64, u: f64, v: f64) -> bool {
        y < Self::LUMA_MAX && (u - 128.0).abs() <= Self::CHROMA_TOLERANCE && (v - 128.0).abs() <= Self::CHROMA_TOLERANCE
    }
    /// True when this frame should be dropped.
    pub fn drop(&mut self, means: (f64, f64, f64), now: Instant) -> bool {
        if self.done {
            return false;
        }
        let first = *self.first.get_or_insert(now);
        if !Self::is_blank(means.0, means.1, means.2) || self.dropped >= Self::MAX_FRAMES || now.duration_since(first).as_millis() >= Self::MAX_MS {
            self.done = true;
            return false;
        }
        self.dropped += 1;
        true
    }
    pub fn checking(&self) -> bool {
        !self.done
    }
}
pub fn drop_leading_blank_frames() -> bool {
    std::env::var("MESHLIVE_DROP_LEADING_BLANK_FRAMES").ok().as_deref() == Some("1")
}

pub struct CallCamera {
    frames: Receiver<Vec<u8>>,
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the call screen says when the camera cannot be used (pure).
pub fn reason_text(native: Option<i32>, privacy: &[String]) -> String {
    let denied = privacy.iter().any(|l| l.contains("value=Deny"));
    if denied || native == Some(0x80070005u32 as i32) {
        "Camera access is off for desktop apps in Windows privacy settings, so the other person can't see you.".into()
    } else if native.is_none() {
        "No camera was found on this PC, so the other person can't see you.".into()
    } else {
        "The camera couldn't be started (another app may be using it), so the other person can't see you.".into()
    }
}

/// The self-view thumbnail: long side near this many pixels (Android
/// `rawSelfViewStep`'s 320) and at most this often (15 per second).
pub const SELF_VIEW_LONG_SIDE: usize = 320;
pub const SELF_VIEW_INTERVAL: Duration = Duration::from_millis(66);

/// The camera's NV12 picture as the call screen's self-view: subsampled and
/// mirrored (the way a mirror and the Android preview show you). Camera NV12
/// is BT.601 limited range, the same assumption the encoder's input makes.
pub fn self_view(nv12: &[u8], width: usize, height: usize) -> (usize, usize, Vec<u8>) {
    let step = crate::video::self_view_step(width, height, SELF_VIEW_LONG_SIDE);
    crate::video::nv12_to_rgba_thumbnail(nv12, width, height, step, true, crate::video::Colour::Bt601, crate::video::Range::Limited)
}

impl CallCamera {
    pub fn start(width: u32, height: u32) -> Self {
        Self::start_with_preview(width, height, None)
    }
    /// `start` that also publishes a small mirrored copy of each picture
    /// (at most every `SELF_VIEW_INTERVAL`) for the call screen's self-view.
    pub fn start_with_preview(width: u32, height: u32, preview: Option<Arc<crate::video::PictureSlot>>) -> Self {
        let (frame_tx, frames) = mpsc::sync_channel::<Vec<u8>>(2);
        let (event_tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("meshlive-call-camera".into())
            .spawn(move || run(width, height, frame_tx, event_tx, flag, preview))
            .ok();
        Self { frames, events, stop, thread }
    }
    /// The newest picture (older ones are dropped: never a backlog of video).
    pub fn latest(&self) -> Option<Vec<u8>> {
        self.frames.try_iter().last()
    }
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}
impl Drop for CallCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unblock a full channel so the thread sees the stop.
        while self.frames.try_recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    width: u32,
    height: u32,
    frames: SyncSender<Vec<u8>>,
    events: mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
    preview: Option<Arc<crate::video::PictureSlot>>,
) {
    let mut camera = match Camera::open_first(width, height) {
        Ok(camera) => camera,
        Err(error) => {
            let native = match error {
                crate::capture::Error::Native(code) => Some(code),
                _ => None,
            };
            let _ = events.send(Event::Unavailable(reason_text(native, &camera_privacy_facts())));
            return;
        }
    };
    let _ = events.send(Event::On);
    let mut blanks = LeadingBlankFrames::new(drop_leading_blank_frames());
    let mut shown_at: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        match camera.next_frame(Instant::now() + Duration::from_millis(200)) {
            Ok(picture)
                if blanks.checking()
                    && blanks.drop(plane_means(&picture, width as usize, height as usize), Instant::now()) => {}
            Ok(picture) => {
                if let Some(slot) = preview.as_ref().filter(|_| shown_at.is_none_or(|at| at.elapsed() >= SELF_VIEW_INTERVAL)) {
                    shown_at = Some(Instant::now());
                    let (w, h, rgba) = self_view(&picture, width as usize, height as usize);
                    if w > 0 && h > 0 {
                        slot.publish(w as u32, h as u32, rgba, 0);
                    }
                }
                match frames.try_send(picture) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Err(crate::capture::Error::Timeout) => {}
            Err(_) => {
                let _ = events.send(Event::Unavailable(reason_text(Some(0), &camera_privacy_facts())));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leading_blank_frames_are_dropped_up_to_the_first_picture_or_the_caps() {
        let now = Instant::now();
        let blank = (0.4, 128.9, 128.0);
        let mut gate = LeadingBlankFrames::new(true);
        assert!(gate.drop(blank, now) && gate.drop(blank, now));
        assert!(!gate.drop((90.0, 120.0, 130.0), now), "a real picture ends the check");
        assert!(!gate.drop(blank, now), "never again after the first picture");
        assert_eq!(gate.dropped, 2);
        let mut capped = LeadingBlankFrames::new(true);
        let dropped = (0..40).filter(|_| capped.drop(blank, now)).count();
        assert_eq!(dropped, 15);
        let mut timed = LeadingBlankFrames::new(true);
        assert!(timed.drop(blank, now));
        assert!(!timed.drop(blank, now + Duration::from_millis(1_000)), "1 s cap");
        assert!(!LeadingBlankFrames::new(false).drop(blank, now), "off by default");
        // NV12 2x2: Y 0, one UV pair 128/128.
        assert_eq!(plane_means(&[0, 0, 0, 0, 128, 128], 2, 2), (0.0, 128.0, 128.0));
    }
    #[test]
    fn the_reason_follows_privacy_and_the_native_error() {
        let deny = vec!["fact=camera_privacy scope=user_desktop_apps value=Deny".to_owned()];
        assert!(reason_text(Some(1), &deny).contains("privacy settings"));
        assert!(reason_text(Some(0x80070005u32 as i32), &[]).contains("privacy settings"));
        assert!(reason_text(None, &[]).contains("No camera"));
        assert!(reason_text(Some(-1), &[]).contains("another app"));
    }
}
