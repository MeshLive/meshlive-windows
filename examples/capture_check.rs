//! Capture check (run only with `--approved`; camera covered or
//! pointed away, quiet room). Measures and keeps nothing but numbers:
//! 1. camera: N s of NV12 at 426x240 from the first video capture device:
//!    frames, achieved FPS, mean luma (pictures are dropped at once);
//! 2. microphone: N s of 48 kHz mono from the default communications
//!    endpoint: frames, RMS dBFS (samples are dropped at once);
//! 3. with `--loopback`: a short quiet 1 kHz tone (gain 0.01, about -40 dBFS)
//!    on the default communications render endpoint, captured by WASAPI
//!    loopback on that SAME endpoint (render pipeline delay, enhancers such
//!    as FxSound included); a hard 10 s deadline. Volume is never changed.
//! `--list` only prints every active endpoint (read only). `--speak SECONDS`
//! asks the operator to speak and reports, per capture endpoint, the peak
//! above the noise floor (20 dB or more = a voice was heard).
//!
//! cargo example: capture_check --approved [--seconds 3] [--no-camera] [--no-mic] [--loopback]
use meshlive::capture::{Camera, FRAME_SAMPLES, Microphone, mean_luma, onset, rms_dbfs};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn log(phase: &str, detail: &str) {
    println!("t={} phase={phase} {detail}", now_ms());
}
fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}
fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    if !flag("--approved") {
        eprintln!("capture_check opens the camera and microphone. Run only with --approved (camera covered or pointed away, quiet room).");
        std::process::exit(2);
    }
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(3);
    // Every active endpoint, read only (FxSound and other virtual devices show as virtual=true).
    let endpoints = meshlive::audio_devices::list();
    for e in &endpoints {
        log("stack_fact", &e.line());
    }
    if flag("--list") {
        log("done", "listed only; nothing was opened for capture");
        return;
    }
    if let Some(secs) = arg("--speak").and_then(|s| s.parse::<u64>().ok()) {
        speak(secs.clamp(1, 30), &endpoints);
        log("done", "no sample was stored");
        return;
    }
    if !flag("--no-camera") {
        match Camera::open_first(426, 240) {
            Ok(mut camera) => {
                let start = Instant::now();
                let (mut frames, mut luma) = (0u32, 0.0);
                while start.elapsed() < Duration::from_secs(seconds) {
                    match camera.next_frame(Instant::now() + Duration::from_secs(2)) {
                        Ok(picture) => {
                            frames += 1;
                            luma += mean_luma(&picture, 426, 240);
                        }
                        Err(e) => {
                            log("camera_error", &format!("{e:?}"));
                            break;
                        }
                    }
                }
                let s = start.elapsed().as_secs_f64();
                log(
                    "camera",
                    &format!(
                        "frames={frames} seconds={s:.2} fps={:.1} mean_luma={:.1} (nothing kept)",
                        f64::from(frames) / s,
                        if frames == 0 { 0.0 } else { luma / f64::from(frames) }
                    ),
                );
            }
            Err(e) => log("camera_unavailable", &format!("{e:?}")),
        }
    }
    if !flag("--no-mic") {
        for line in meshlive::capture::capture_endpoint_facts() {
            log("stack_fact", &line);
        }
        let privacy = meshlive::capture::privacy_facts();
        for line in &privacy {
            log("stack_fact", line);
        }
        let chosen = meshlive::audio_devices::open_best_microphone(Duration::from_secs(1), arg("--mic-device").as_deref());
        let chosen = match chosen {
            Ok((mic, tried)) => {
                for line in tried {
                    log("mic_probe", &line);
                }
                Ok(mic)
            }
            Err((verdict, tried)) => {
                for line in tried {
                    log("mic_probe", &line);
                }
                log("mic_verdict", &verdict);
                Err(verdict)
            }
        };
        match chosen {
            Ok(mut mic) => {
                let start = Instant::now();
                let mut frames = 0u32;
                let mut energy = Vec::with_capacity(FRAME_SAMPLES);
                let mut worst = -120.0f64;
                let mut meter = meshlive::capture::SignalMeter::default();
                while start.elapsed() < Duration::from_secs(seconds) {
                    match mic.next_frame(Instant::now() + Duration::from_secs(1)) {
                        Ok(frame) => {
                            frames += 1;
                            worst = worst.max(rms_dbfs(&frame));
                            meter.push(&frame);
                            energy.clear();
                            energy.extend_from_slice(&frame);
                        }
                        Err(e) => {
                            log("mic_error", &format!("{e:?}"));
                            break;
                        }
                    }
                }
                log(
                    "microphone",
                    &format!(
                        "frames={frames} seconds={:.2} loudest_frame_dbfs={worst:.1} last_frame_dbfs={:.1} {} (nothing kept)",
                        start.elapsed().as_secs_f64(),
                        rms_dbfs(&energy),
                        meter.summary()
                    ),
                );
                energy.fill(0);
                let endpoint_muted = meshlive::capture::capture_endpoint_facts()
                    .iter()
                    .any(|l| l.contains(&mic.endpoint_id) && l.contains("muted=true"));
                log(
                    "mic_verdict",
                    &format!(
                        "{} packets={} silent_packets={} endpoint={}",
                        meshlive::capture::mic_verdict(&meter, mic.packets, mic.silent_packets, &privacy, endpoint_muted),
                        mic.packets,
                        mic.silent_packets,
                        mic.endpoint_id
                    ),
                );
                if flag("--loopback") {
                    loopback(Some(&mut mic), &endpoints);
                }
            }
            Err(_) => {
                if flag("--loopback") {
                    // Without a microphone the render path is still measured by WASAPI loopback.
                    loopback(None, &endpoints);
                }
            }
        }
    }
    log("done", "no picture or sample was stored");
}

/// A 300 ms, 1 kHz tone at gain 0.01 on the default render endpoint while
/// the microphone listens for 1.5 s; onset delay = acoustic loopback estimate.
/// `--speak SECONDS`: every capture endpoint listens at the same time while
/// the operator speaks; per endpoint the noise floor (p10 frame level), the
/// peak and peak minus floor (a voice gives 20 dB or more). Nothing is kept.
fn speak(seconds: u64, endpoints: &[meshlive::audio_devices::Endpoint]) {
    use meshlive::audio_devices::Flow;
    let mut mics = Vec::new();
    for e in endpoints.iter().filter(|e| e.flow == Flow::Capture) {
        match Microphone::open(Some(&e.id), false) {
            Ok(m) => mics.push((e.name.clone(), e.is_virtual(), m, meshlive::capture::SignalMeter::default())),
            Err(error) => log("speak_open_failed", &format!("name=\"{}\" {error:?}", e.name)),
        }
    }
    println!("say a few words now ({seconds} s)");
    log("speak_start", &format!("seconds={seconds} endpoints={}", mics.len()));
    let until = Instant::now() + Duration::from_secs(seconds) + Duration::from_millis(200);
    while Instant::now() < until {
        for (_, _, mic, meter) in mics.iter_mut() {
            while let Ok(frame) = mic.next_frame(Instant::now() + Duration::from_millis(5)) {
                meter.push(&frame);
            }
        }
    }
    for (name, is_virtual, mic, meter) in &mics {
        let rise = meter.peak_dbfs() - meter.floor_dbfs();
        log(
            "speak",
            &format!(
                "name=\"{name}\" virtual={is_virtual} floor_dbfs={:.1} peak_dbfs={:.1} peak_minus_floor_db={rise:.1} voice_heard={} packets={} silent_packets={} {}",
                meter.floor_dbfs(),
                meter.peak_dbfs(),
                rise >= 20.0,
                mic.packets,
                mic.silent_packets,
                meter.summary()
            ),
        );
    }
}

fn loopback(mic: Option<&mut Microphone>, endpoints: &[meshlive::audio_devices::Endpoint]) {
    let render = endpoints
        .iter()
        .find(|e| e.flow == meshlive::audio_devices::Flow::Render && e.default_communications);
    if let Some(r) = render {
        log("loopback_render", &format!("name=\"{}\" virtual={} mix_format=\"{}\" device_period_ms={}", r.name, r.is_virtual(), r.mix_format, r.latency_ms.map_or("unknown".into(), |l| format!("{l:.1}"))));
    }
    // WASAPI loopback on the SAME endpoint the tone is rendered to: the
    // render pipeline's own delay (an enhancer like FxSound included), no air.
    let Some(id) = render.map(|r| r.id.clone()) else {
        log("loopback_unavailable", "no default communications render endpoint");
        return;
    };
    let mut tap = match Microphone::open(Some(&id), true) {
        Ok(t) => t,
        Err(e) => {
            log("loopback_unavailable", &format!("wasapi loopback {e:?}"));
            return;
        }
    };
    let _ = mic;
    let mic = &mut tap;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = match meshlive::playback::Wasapi::open_default() {
        Ok(output) => output,
        Err(error) => {
            log("loopback_unavailable", &format!("render endpoint {error:?}"));
            return;
        }
    };
    let tone: Vec<i16> = (0..14_400).map(|i| ((i as f64 * 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0).sin() * 327.0) as i16).collect();
    let mut heard = Vec::with_capacity(48_000 * 2);
    // Drop what the microphone already holds (bounded: frames keep arriving
    // in real time, so draining "until empty" never ends; that was the hang).
    let drain_until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < drain_until {
        let _ = mic.next_frame(Instant::now() + Duration::from_millis(30));
    }
    let never = std::sync::atomic::AtomicBool::new(false);
    let started = Instant::now();
    let mut played = 0;
    let mut first_write: Option<Instant> = None;
    let mut heard_at: Option<Instant> = None;
    while started.elapsed() < Duration::from_millis(1500) && Instant::now() < deadline {
        if played < tone.len() {
            let end = (played + 960).min(tone.len());
            if output.write(&tone[played..end], (Instant::now() + Duration::from_millis(100)).min(deadline), &never).is_ok() {
                first_write.get_or_insert_with(Instant::now);
                played = end;
            }
        }
        if let Ok(frame) = mic.next_frame((Instant::now() + Duration::from_millis(25)).min(deadline)) {
            if heard_at.is_none() && onset(&frame, 100).is_some() {
                heard_at = Some(Instant::now());
            }
            heard.extend_from_slice(&frame);
        }
    }
    match (first_write, heard_at) {
        (Some(w), Some(h)) => log(
            "loopback",
            &format!(
                "method=wasapi_loopback_same_endpoint tone_gain=0.01 render_to_tap_ms={:.1} heard_dbfs={:.1} (nothing kept)",
                h.saturating_duration_since(w).as_secs_f64() * 1000.0,
                rms_dbfs(&heard)
            ),
        ),
        _ => log(
            "loopback",
            &format!("tone not heard within 10 s (written={}) peak_dbfs={:.1}", first_write.is_some(), rms_dbfs(&heard)),
        ),
    }
    heard.fill(0);
}
