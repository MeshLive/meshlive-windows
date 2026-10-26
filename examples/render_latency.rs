//! A/B of the call output path (Experimental `MESHLIVE_AUDIOCLIENT3`):
//! opens the default communications speaker in the standard path and the
//! IAudioClient3 low-latency path, in A B B A order, and plays SILENCE (all
//! zeros: nothing is heard, no volume is changed) in 20 ms chunks paced like
//! call audio. Per run it reports the engine period, buffer, stream latency
//! and the queued engine audio after each write, plus starvations (the engine
//! buffer found empty after start). Engine figures are proxies, not acoustic
//! latency.
//!
//! cargo example: render_latency [--seconds 10] [--rounds 2]
use meshlive::playback::Wasapi;
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn arg(name: &str, default: u64) -> u64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)?.parse().ok()).unwrap_or(default)
}

fn run(low: bool, seconds: u64) -> Result<String, String> {
    let device = Wasapi::open(low).map_err(|e| format!("open {e:?}"))?;
    let describe = device.describe();
    let mut playback = meshlive::playback::Playback::new(device);
    let never = AtomicBool::new(false);
    let silence = [0i16; 960];
    let start = Instant::now();
    let mut next = start;
    let mut queued_ms = Vec::new();
    let mut starved = 0;
    let mut late_writes = 0;
    let mut empty_before = 0;
    let mut errors: Vec<String> = Vec::new();
    while start.elapsed() < Duration::from_secs(seconds) {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += Duration::from_millis(20);
        let began = Instant::now();
        let before = playback.device().ok_or_else(|| format!("closed after {errors:?}"))?.queued_frames().map_err(|e| format!("{e:?}"))?;
        if before == 0 && queued_ms.len() > 10 {
            empty_before += 1;
        }
        if let Err(e) = playback.write(&silence, Instant::now() + Duration::from_millis(40), &never) {
            // Recorded, and the run goes on (to show whether one timeout closes the device).
            errors.push(format!("write#{} {e:?} at_ms={} waited_ms={}", queued_ms.len(), start.elapsed().as_millis(), began.elapsed().as_millis()));
            if errors.len() > 3 {
                return Err(format!("{errors:?}"));
            }
            queued_ms.push(0.0);
            continue;
        }
        if began.elapsed() > Duration::from_millis(5) {
            late_writes += 1;
        }
        let queued = playback.device().ok_or("closed")?.queued_frames().map_err(|e| format!("{e:?}"))?;
        // Just before this write the engine held queued - 960 frames.
        if queued <= 960 && start.elapsed() > Duration::from_millis(200) {
            starved += 1;
        }
        queued_ms.push(queued as f64 / 48.0);
    }
    queued_ms.sort_by(f64::total_cmp);
    let mean = queued_ms.iter().sum::<f64>() / queued_ms.len().max(1) as f64;
    let p95 = queued_ms.get(queued_ms.len() * 95 / 100).copied().unwrap_or(0.0);
    Ok(format!("{describe} writes={} queued_ms_mean={mean:.1} queued_ms_p95={p95:.1} low_before={starved} empty_before={empty_before} blocked_writes={late_writes} errors={errors:?}", queued_ms.len()))
}

fn main() {
    let seconds = arg("--seconds", 10);
    let rounds = arg("--rounds", 2);
    for round in 0..rounds {
        let order = if round % 2 == 0 { [false, true] } else { [true, false] };
        for low in order {
            let label = if low { "audioclient3" } else { "standard" };
            match run(low, seconds) {
                Ok(line) => println!("round={round} path={label} {line}"),
                Err(error) => println!("round={round} path={label} failed={error}"),
            }
        }
    }
}
