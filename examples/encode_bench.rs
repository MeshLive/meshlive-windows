//! Generated-input encoder benchmark (`--generated [--seconds N]`): no camera,
//! microphone, audio output or radio. For each backend on this PC, 640x360 at
//! 15 and 30 FPS for N seconds (default 60) in real time (frames are paced to
//! their timestamps), CBR 400 kbps, one keyframe at the start. Measures, per
//! picture, the encode latency from submission until its encoded unit is back
//! (hardware MFTs are asynchronous, so this waits for the unit), and the
//! process CPU time over the run. Hardware encoders get GPU textures (RGBA ->
//! NV12 on their own adapter, no CPU copy) and, for comparison, system memory.
use meshlive::video::*;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::FILETIME,
    Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
    System::Threading::{GetCurrentProcess, GetProcessTimes},
};

const W: u32 = 640;
const H: u32 = 360;
const KBPS: u32 = 400;
const DISTINCT: u32 = 60;

fn cpu_seconds() -> f64 {
    let (mut a, mut b, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut kernel, &mut user) }.unwrap();
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)) as f64 / 1e7;
    t(kernel) + t(user)
}

/// The same moving textured picture as the encode tests (pattern + texture).
fn picture(frame: u32) -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut out = meshlive::generated::pattern_nv12(w, h, frame);
    let shift = frame as usize * 3;
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x + shift) as u32, (y + shift / 2) as u32);
            let mut k = u.wrapping_mul(0x9E37_79B1) ^ v.wrapping_mul(0x85EB_CA77);
            k ^= k >> 15;
            k = k.wrapping_mul(0x2C1B_3C6D);
            k ^= k >> 12;
            out[y * w + x] = (out[y * w + x] as i32 + (k & 15) as i32 - 8).clamp(16, 235) as u8;
        }
    }
    out
}

enum Input<'a> {
    Memory(&'a [Vec<u8>]),
    Texture(&'a ID3D11Device, &'a [ID3D11Texture2D], RgbaToNv12),
}

enum Backend<'a> {
    Hardware(Encoder<'a>),
    Software(VideoEncoder<'a>),
}

struct Result {
    name: String,
    fps: u32,
    frames: usize,
    units: usize,
    kbps: f64,
    p50: f64,
    p95: f64,
    max: f64,
    cpu_core_percent: f64,
    texture: bool,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn run(name: &str, mut backend: Backend<'_>, mut input: Input<'_>, fps: u32, seconds: u32) -> Result {
    let frames = (fps * seconds) as usize;
    let mut submitted: HashMap<i64, Instant> = HashMap::new();
    let mut latency = Vec::with_capacity(frames);
    let (mut units, mut bytes) = (0usize, 0usize);
    let texture = matches!(input, Input::Texture(..)) && matches!(&backend, Backend::Hardware(e) if e.texture_input());
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    let mut take = |got: Vec<EncodedUnit>, submitted: &mut HashMap<i64, Instant>| {
        for unit in got {
            if let Some(at) = submitted.remove(&unit.pts_us) {
                latency.push(at.elapsed().as_secs_f64() * 1000.0);
            }
            units += 1;
            bytes += unit.data.len();
        }
    };
    for frame in 0..frames {
        let due = start + Duration::from_secs_f64(frame as f64 / f64::from(fps));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let pts = frame as i64 * 1_000_000 / i64::from(fps);
        let deadline = Instant::now() + Duration::from_secs(2);
        let index = frame % DISTINCT as usize;
        submitted.insert(pts, Instant::now());
        let got = match (&mut backend, &mut input) {
            (Backend::Hardware(e), Input::Texture(_, rgba, converter)) => {
                let nv12 = converter.convert(&rgba[index]).unwrap();
                e.encode_texture(&nv12, 0, pts, frame == 0, deadline).unwrap()
            }
            (Backend::Hardware(e), Input::Memory(pictures)) => e.encode(&pictures[index], pts, frame == 0, deadline).unwrap(),
            (Backend::Software(e), Input::Memory(pictures)) => e.encode(&pictures[index], pts, frame == 0, deadline).unwrap(),
            (Backend::Software(e), Input::Texture(gpu, rgba, converter)) => {
                let nv12 = converter.convert(&rgba[index]).unwrap();
                e.encode_texture(gpu, &nv12, 0, pts, frame == 0, deadline).unwrap()
            }
        };
        take(got, &mut submitted);
        // Hardware is asynchronous: wait for this picture's unit (bounded) to
        // measure its latency; software returned it from the call.
        if let Backend::Hardware(e) = &mut backend {
            let until = Instant::now() + Duration::from_millis(500);
            while submitted.contains_key(&pts) && Instant::now() < until {
                let got = e.wait_output(until).unwrap();
                take(got, &mut submitted);
            }
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu_start;
    let tail = match &mut backend {
        Backend::Hardware(e) => e.finish(Instant::now() + Duration::from_secs(5)).unwrap(),
        Backend::Software(e) => e.finish(Instant::now() + Duration::from_secs(5)).unwrap(),
    };
    units += tail.len();
    bytes += tail.iter().map(|u| u.data.len()).sum::<usize>();
    latency.sort_by(f64::total_cmp);
    Result {
        name: name.to_owned(),
        fps,
        frames,
        units,
        kbps: bytes as f64 * 8.0 / (frames as f64 / f64::from(fps)) / 1000.0,
        p50: percentile(&latency, 0.5),
        p95: percentile(&latency, 0.95),
        max: *latency.last().unwrap(),
        cpu_core_percent: cpu / wall * 100.0,
        texture,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("--generated") {
        eprintln!("Use --generated [--seconds N] for an explicit local benchmark on generated input");
        std::process::exit(2);
    }
    let seconds: u32 = args.iter().position(|a| a == "--seconds").and_then(|i| args.get(i + 1)?.parse().ok()).unwrap_or(60);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("640x360, {KBPS} kbps CBR, {seconds} s per run, real-time paced; CPU in % of one logical core ({cores} cores)");
    let pictures: Vec<Vec<u8>> = (0..DISTINCT).map(picture).collect();
    let mut results = Vec::new();
    for fps in [15u32, 30] {
        let s = |codec| EncoderSettings { codec, width: W, height: H, fps, bitrate_kbps: KBPS, keyframe_interval_s: 60 };
        for (codec, candidates) in [(3u8, hardware_candidates(3).unwrap()), (2, hardware_candidates(2).unwrap())] {
            let mut seen = Vec::new();
            for candidate in candidates {
                if seen.contains(&candidate.name) {
                    continue; // duplicate registrations of the same encoder
                }
                let (gpu, _) = device_on(candidate.adapter_luid).unwrap();
                let owner = DecodeDevice::new(&gpu).unwrap();
                let Ok(encoder) = Encoder::open_candidate(&owner, s(codec), &candidate) else {
                    println!("{}: does not open on this PC", candidate.name);
                    continue;
                };
                seen.push(candidate.name.clone());
                let rgba: Vec<ID3D11Texture2D> = pictures
                    .iter()
                    .map(|p| upload_rgba(&gpu, W, H, &nv12_to_rgba(p, W as usize, H as usize)).unwrap())
                    .collect();
                let converter = RgbaToNv12::new(&gpu, W, H, fps, 8).unwrap();
                results.push(run(&candidate.name, Backend::Hardware(encoder), Input::Texture(&gpu, &rgba, converter), fps, seconds));
                let encoder = Encoder::open_candidate(&owner, s(codec), &candidate).unwrap();
                results.push(run(&format!("{} (system memory)", candidate.name), Backend::Hardware(encoder), Input::Memory(&pictures), fps, seconds));
            }
        }
        let (gpu, _, _) = device(false, false).unwrap();
        let owner = DecodeDevice::new(&gpu).unwrap();
        for (codec, backend) in [(3u8, meshlive::video::Backend::SoftwareAv1), (2, meshlive::video::Backend::SoftwareVp9)] {
            let encoder = VideoEncoder::open(&owner, s(codec), backend).unwrap();
            let name = encoder.name();
            results.push(run(&name, Backend::Software(encoder), Input::Memory(&pictures), fps, seconds));
        }
    }
    println!("{:<52} {:>4} {:>6} {:>6} {:>8} {:>7} {:>7} {:>7} {:>9} {:>8}", "encoder", "fps", "frames", "units", "kbps", "p50 ms", "p95 ms", "max ms", "CPU %core", "texture");
    for r in &results {
        println!(
            "{:<52} {:>4} {:>6} {:>6} {:>8.0} {:>7.2} {:>7.2} {:>7.2} {:>9.1} {:>8}",
            r.name, r.fps, r.frames, r.units, r.kbps, r.p50, r.p95, r.max, r.cpu_core_percent, r.texture
        );
    }
    println!("No camera, microphone, audio output or radio was used.");
}
