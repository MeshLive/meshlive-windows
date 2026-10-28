//! Design AD fixtures and measurement for the dav1d fallback. Owned,
//! generated pixels only (no camera, no third-party footage), encoded by the
//! bundled libaom at phone-like settings.
//!
//!   cargo run --release --example dav1d_fixtures -- write
//!       regenerates tests/fixtures/dav1d-bench-854x480.ivf and -640x360.ivf
//!       (then update their hashes in sources.lock.json).
//!   cargo run --release --example dav1d_fixtures -- bench
//!       decode speed at 640x480/30 and 1280x720/30 (3 s each): dav1d alone,
//!       and through the app's session path (NV12 texture upload included).
use meshlive::{
    generated::pattern_nv12,
    media::VideoHeader,
    video::{
        AomEncoder, DecoderPolicy, EncoderSettings, MfFrames,
        dav1d::{self, Native},
    },
    viewer::FrameDecoder,
};
use std::time::Instant;

/// The app's moving test pattern plus a panning texture, so the encoder
/// spends its bitrate the way camera detail makes it.
fn textured(width: usize, height: usize, frame: u32) -> Vec<u8> {
    let mut picture = pattern_nv12(width, height, frame);
    let pan = frame as usize * 3;
    for y in 0..height {
        for x in 0..width {
            let mut h = ((x + pan) as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
            h ^= h >> 15;
            let noise = (h & 31) as i32 - 16;
            let value = &mut picture[y * width + x];
            *value = (i32::from(*value) + noise).clamp(16, 235) as u8;
        }
    }
    picture
}

fn encode(width: u32, height: u32, fps: u32, kbps: u32, frames: u32) -> Vec<Vec<u8>> {
    let settings = EncoderSettings { codec: 3, width, height, fps, bitrate_kbps: kbps, keyframe_interval_s: 60 };
    let mut encoder = AomEncoder::open(settings).expect("libaom");
    let mut units = Vec::new();
    for i in 0..frames {
        let pts = i64::from(i) * 1_000_000 / i64::from(fps);
        for unit in encoder.encode(&textured(width as usize, height as usize, i), pts, i == 0).expect("encode") {
            units.push(unit.data);
        }
    }
    units.extend(encoder.finish().expect("finish").into_iter().map(|u| u.data));
    assert_eq!(units.len(), frames as usize, "one temporal unit per picture");
    units
}

fn ivf(width: u32, height: u32, fps: u32, units: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"AV01");
    out.extend_from_slice(&(width as u16).to_le_bytes());
    out.extend_from_slice(&(height as u16).to_le_bytes());
    out.extend_from_slice(&fps.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(units.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (i, unit) in units.iter().enumerate() {
        out.extend_from_slice(&(unit.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u64).to_le_bytes());
        out.extend_from_slice(unit);
    }
    out
}

fn write() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (width, height, fps, kbps) in [(854, 480, 30, 600), (640, 360, 15, 300)] {
        let units = encode(width, height, fps, kbps, fps);
        let path = root.join(format!("dav1d-bench-{width}x{height}.ivf"));
        let data = ivf(width, height, fps, &units);
        std::fs::write(&path, &data).unwrap();
        let hash: String = meshlive::crypto::sha256(&data).unwrap().iter().map(|b| format!("{b:02x}")).collect();
        println!("{} {} bytes sha256 {hash}", path.display(), data.len());
    }
}

fn bench() {
    println!("dav1d {} threads={} frame_delay={}", dav1d::version(), dav1d::THREADS, dav1d::FRAME_DELAY);
    let readiness = dav1d::readiness();
    println!("{}", readiness.line());
    let (gpu, _, _) = meshlive::video::device(false, false).expect("device");
    let owner = meshlive::video::DecodeDevice::new(&gpu).expect("decode device");
    for (width, height, fps, kbps) in [(640u32, 480u32, 30u32, 800u32), (1280, 720, 30, 1500)] {
        let seconds = 3;
        let units = encode(width, height, fps, kbps, fps * seconds);
        let bytes: usize = units.iter().map(Vec::len).sum();
        let media = f64::from(seconds);
        // dav1d alone (I420 in system memory).
        let mut native = Native::open(width * height).unwrap();
        let mut per_frame = Vec::new();
        let start = Instant::now();
        for (i, unit) in units.iter().enumerate() {
            let t = Instant::now();
            let n = dav1d::decode_all(&mut native, [(&unit[..], i as i64)], |_| {}).unwrap();
            assert_eq!(n, 1);
            per_frame.push(t.elapsed().as_micros() as u32);
        }
        let alone = start.elapsed().as_secs_f64();
        let (p50, p95) = dav1d::percentiles(&per_frame);
        // The app's session path: MfFrames forced to dav1d, NV12 texture out.
        let header = VideoHeader { width: width as u16, height: height as u16, fps: fps as u8, codec: 3, quarter_turns: 0, opus: false };
        let mut shown = 0u32;
        let mut frames = MfFrames::new(&owner, header, |_, _| shown += 1).with_policy(DecoderPolicy::Dav1dOnly);
        frames.configure_with(None).expect("dav1d session");
        let start = Instant::now();
        for (i, unit) in units.iter().enumerate() {
            let pts = i as i64 * 1_000_000 / i64::from(fps);
            while !FrameDecoder::push(&mut frames, unit, pts, i == 0, false).unwrap() {
                while FrameDecoder::pull(&mut frames).unwrap().is_some() {}
            }
            while FrameDecoder::pull(&mut frames).unwrap().is_some() {}
        }
        let session = start.elapsed().as_secs_f64();
        drop(frames);
        println!(
            "{width}x{height}/{fps} {kbps} kbps ({} KB, {} frames): dav1d alone {:.0} fps = {:.1}x real time, \
             decode ms p50 {p50:.2} p95 {p95:.2}; app session path {:.0} fps = {:.1}x real time, {shown} shown",
            bytes / 1024,
            units.len(),
            units.len() as f64 / alone,
            media / alone,
            units.len() as f64 / session,
            media / session,
        );
    }
    println!("{}", dav1d::summary());
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("write") => write(),
        Some("bench") => bench(),
        _ => eprintln!("usage: dav1d_fixtures write|bench"),
    }
}
