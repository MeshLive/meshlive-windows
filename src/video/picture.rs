//! The peer's decoded pictures for the call screen. The decoder (on the
//! Bluetooth owner's thread) reads each NV12 frame back once, converts it to
//! RGBA and publishes it in a one-picture slot; the UI takes the newest one
//! when it paints. Only the newest picture is kept: a slow UI skips stale
//! pictures, it never queues them (the decoder never waits for the UI).
use super::{Colour, Nv12Frame, Range};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicU8, Ordering},
};
use std::time::Instant;

pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Tight RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    pub pts_us: i64,
    /// 1, 2, 3, ... in decode order.
    pub seq: u64,
    pub decoded_at: Instant,
    /// When the oldest picture this one replaced was decoded (== decoded_at
    /// when none was replaced): a short span over several replaced pictures
    /// means they arrived in a burst, a long one that the screen was slow.
    pub first_unshown_at: Instant,
}

#[derive(Default)]
pub struct PictureSlot {
    newest: Mutex<Option<Picture>>,
    published: AtomicU64,
    failed: AtomicU64,
    /// The peer's display rotation in quarter turns (header, then flag-4 packets).
    turns: AtomicU8,
}
impl PictureSlot {
    pub fn publish(&self, width: u32, height: u32, rgba: Vec<u8>, pts_us: i64) {
        let seq = self.published.fetch_add(1, Ordering::AcqRel) + 1;
        let now = Instant::now();
        let mut newest = self.newest.lock().unwrap_or_else(|e| e.into_inner());
        let first_unshown_at = newest.as_ref().map_or(now, |old| old.first_unshown_at);
        *newest = Some(Picture { width, height, rgba, pts_us, seq, decoded_at: now, first_unshown_at });
    }
    /// The newest picture if it is newer than `seen` (it stays for no one else).
    pub fn take_newer(&self, seen: u64) -> Option<Picture> {
        if self.published.load(Ordering::Acquire) <= seen {
            return None;
        }
        self.newest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }
    pub fn read_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn failures(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
    pub fn set_turns(&self, turns: u8) {
        self.turns.store(turns % 4, Ordering::Relaxed);
    }
    pub fn turns(&self) -> u8 {
        self.turns.load(Ordering::Relaxed)
    }
}

/// YUV -> RGB coefficients (Kr/Kb matrices; limited range scales Y and C).
fn coefficients(colour: Colour) -> [f32; 4] {
    // [R from V, G from U, G from V, B from U]
    match colour {
        Colour::Bt709 => [1.5748, 0.1873, 0.4681, 1.8556],
        Colour::Bt601 => [1.402, 0.344_136, 0.714_136, 1.772],
    }
}

/// NV12 (tight Y plane, then interleaved UV) to RGBA.
pub fn nv12_to_rgba(nv12: &[u8], width: usize, height: usize, colour: Colour, range: Range) -> Vec<u8> {
    let [rv, gu, gv, bu] = coefficients(colour);
    let (y_scale, c_scale, y_off) = match range {
        Range::Limited => (255.0 / 219.0, 255.0 / 224.0, 16.0),
        Range::Full => (1.0, 1.0, 0.0),
    };
    let (luma, chroma) = nv12.split_at(width * height);
    let mut out = vec![255u8; width * height * 4];
    for y in 0..height {
        let row = &chroma[(y / 2) * width..];
        for x in 0..width {
            let l = (f32::from(luma[y * width + x]) - y_off) * y_scale;
            let u = (f32::from(row[x & !1]) - 128.0) * c_scale;
            let v = (f32::from(row[(x & !1) + 1]) - 128.0) * c_scale;
            let at = (y * width + x) * 4;
            out[at] = (l + rv * v).round().clamp(0.0, 255.0) as u8;
            out[at + 1] = (l - gu * u - gv * v).round().clamp(0.0, 255.0) as u8;
            out[at + 2] = (l + bu * u).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

impl Nv12Frame<'_> {
    /// One readback of the cropped picture as RGBA (for a CPU-uploaded UI
    /// texture). `colour`/`range` default to BT.601 below 720 lines, BT.709
    /// from 720, limited range.
    pub fn read_rgba(&self, colour: Option<Colour>, range: Option<Range>) -> windows::core::Result<(u32, u32, Vec<u8>)> {
        let (w, h) = (self.crop_size()[0] as usize & !1, self.crop_size()[1] as usize & !1);
        let nv12 = self.read_nv12_cropped(w, h)?;
        let colour = colour.unwrap_or(if h >= 720 { Colour::Bt709 } else { Colour::Bt601 });
        Ok((w as u32, h as u32, nv12_to_rgba(&nv12, w, h, colour, range.unwrap_or(Range::Limited))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_converts_to_the_expected_colours() {
        // 2x2: limited-range white, black, and a mid grey with neutral chroma; then pure red (BT.601).
        let grey = [235, 16, 126, 126, 128, 128];
        let rgba = nv12_to_rgba(&grey, 2, 2, Colour::Bt601, Range::Limited);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);
        assert_eq!(rgba[8], rgba[9]);
        let red = [81, 81, 81, 81, 90, 240];
        let rgba = nv12_to_rgba(&red, 2, 2, Colour::Bt601, Range::Limited);
        assert!(rgba[0] >= 250 && rgba[1] <= 5 && rgba[2] <= 5, "{:?}", &rgba[0..4]);
    }

    #[test]
    fn the_slot_keeps_only_the_newest_picture() {
        let slot = PictureSlot::default();
        assert!(slot.take_newer(0).is_none());
        slot.publish(2, 2, vec![0; 16], 10);
        slot.publish(2, 2, vec![1; 16], 20);
        let p = slot.take_newer(0).unwrap();
        assert_eq!((p.seq, p.pts_us), (2, 20));
        assert!(p.first_unshown_at <= p.decoded_at, "the replaced picture's time is kept");
        slot.publish(2, 2, vec![2; 16], 30);
        let q = slot.take_newer(2).unwrap();
        assert_eq!(q.first_unshown_at, q.decoded_at, "nothing replaced");
        assert!(slot.take_newer(2).is_none());
        slot.set_turns(5);
        assert_eq!(slot.turns(), 1);
    }
}
