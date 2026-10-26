//! Explicit native audio ownership check; no samples or device facts saved.
use super::*;

#[derive(Default)]
struct Seen { mic: bool, render: bool, opens: usize, before_ack: usize, after_ack: usize, ack: bool }
fn poll(audio: &CallAudio, seen: &mut Seen) -> Result<(), &'static str> {
    for event in audio.events() {
        match event {
            Event::MicOn(_) => { seen.mic = true; seen.opens += 1; }
            Event::RenderOpened(_) => seen.render = true,
            Event::Frame(mut pcm) => {
                if seen.ack { seen.after_ack += 1; } else { seen.before_ack += 1; }
                pcm.fill(0);
            }
            Event::CaptureRestarted(1) => seen.ack = true,
            Event::MicUnavailable(_) => return Err("capture_unavailable"),
            Event::RenderUnavailable(_) => return Err("render_unavailable"),
            _ => {},
        }
    }
    Ok(())
}
fn wait(audio: &CallAudio, seen: &mut Seen, ready: impl Fn(&Seen) -> bool) -> Result<u128, &'static str> {
    let start = Instant::now();
    loop {
        poll(audio, seen)?;
        if ready(seen) { return Ok(start.elapsed().as_millis()); }
        if start.elapsed() >= Duration::from_secs(8) { return Err("audio_progress_timeout"); }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn opt_in_native_capture_restart_and_terminal_cleanup() {
    if std::env::var("MESHLIVE_NATIVE_AUDIO_CHECK").as_deref() != Ok("1") { return; }
    let _mta = crate::bluetooth::MtaUsage::for_process().expect("Process MTA");
    assert!(_mta.is_some(), "Native audio check requires MESHLIVE_MTA_LIFETIME=1");
    let audio = CallAudio::with_end_fence(0.0, true, None);
    let mut seen = Seen::default();
    let (mut open_ms, mut restart_ms, mut stop_ms) = (0, 0, 0);
    let mut finished = false;
    let result = (|| -> Result<(), &'static str> {
        audio.start_capture(None);
        audio.play(vec![0; FRAME_SAMPLES]);
        open_ms = wait(&audio, &mut seen, |s| s.mic && s.render && s.before_ack >= 3)?;
        seen.before_ack = 0;
        // Leave real captured frames queued before requesting the fresh owner.
        std::thread::sleep(Duration::from_millis(200));
        audio.restart_capture(1);
        restart_ms = wait(&audio, &mut seen, |s| s.ack && s.after_ack >= 3)?;
        if seen.before_ack == 0 { return Err("queued_capture_not_observed"); }
        // Both endpoints are already open; queue only generated digital silence.
        audio.play(vec![0; FRAME_SAMPLES * 100]);
        std::thread::sleep(Duration::from_millis(15));
        let stop = Instant::now();
        if !audio.stop_now() { return Err("terminal_fence_unavailable"); }
        while stop.elapsed() < Duration::from_secs(3) {
            if audio.thread.as_ref().is_some_and(JoinHandle::is_finished) { finished = true; break; }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop_ms = stop.elapsed().as_millis();
        if !finished { return Err("device_cleanup_timeout"); }
        // Drop any frames captured before terminal cancellation, without saving.
        poll(&audio, &mut seen)?;
        Ok(())
    })();
    audio.stop_now();
    drop(audio);
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".artifacts");
    std::fs::create_dir_all(&dir).unwrap();
    let report = format!("passed={}\nfailure={}\nmic_opened={}\nrender_opened={}\nmic_open_events={}\nqueued_frames_before_restart_ack={}\nframes_after_restart_ack={}\nrestart_ack={}\nworker_finished={}\nopen_ms={open_ms}\nrestart_ms={restart_ms}\nterminal_cleanup_ms={stop_ms}\nplayback_gain=0\n",
        result.is_ok(), result.err().unwrap_or("none"), seen.mic, seen.render, seen.opens,
        seen.before_ack, seen.after_ack, seen.ack, finished);
    std::fs::write(dir.join("native-audio-lifecycle.txt"), report).unwrap();
    assert!(result.is_ok(), "Native audio ownership check: {result:?}");
}
