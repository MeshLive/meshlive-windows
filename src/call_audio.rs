//! The app's call audio (CF-OUT-16/17/37): one dedicated thread owns the
//! WASAPI render endpoint and, only while a call's media runs, the
//! microphone (both need a single-threaded COM apartment, which the
//! Bluetooth worker's multithreaded one cannot host). The worker decodes and
//! encodes; PCM crosses in 20 ms frames.
//! - Render: the default communications endpoint, opened at the first sound;
//!   a failure is reported once and the call goes on silent.
//! - Ringback: a quiet 425 Hz tone, 1 s on / 4 s off, while the caller hears
//!   Ringing (setting "ringback").
//! - Capture: `audio_devices::open_best_microphone` (the communications
//!   default unless there is evidence against it). Never outside a call:
//!   the microphone opens at `start_capture` and closes at `stop_capture` or
//!   when this owner is dropped. A device change reopens it mid-call.
//! - Echo: capture and render are communications streams
//!   (AudioCategory_Communications), so Windows runs the endpoint's echo
//!   canceller and noise suppression where offered; the microphone log ends
//!   with one `audio_effects` line (aec=On/Off/Absent/Unknown). Both streams
//!   opt out of Windows' communications ducking, so other apps keep their
//!   level. Experimental `MESHLIVE_VOICE_DSP=1`: the Voice Capture DSP.
//! - Volume: never changed. `gain` (0..=1) only scales what this app plays
//!   (`MESHLIVE_PLAYBACK_GAIN`, for quiet automated tests; default 1).
use crate::{
    audio_devices::{self, DeviceWatch},
    capture::{FRAME_SAMPLES, Microphone, VoiceDsp},
    playback::{Playback, Wasapi},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

enum Command {
    Play(Vec<i16>),
    Ringback(bool),
    StartCapture(Option<String>),
    RestartCapture(u64),
    StopCapture,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 20 ms of microphone audio (48 kHz mono).
    Frame(Box<[i16; FRAME_SAMPLES]>),
    /// The microphone is open (the endpoint chosen and why).
    MicOn(Vec<String>),
    /// Source-only fence: the old microphone is closed; subsequent frames
    /// belong to the newly opened capture. Ordered with Frame on this channel.
    CaptureRestarted(u64),
    /// No usable microphone: the plain reason (privacy, mute, no device).
    MicUnavailable(String),
    /// Nothing can be played (the render endpoint failed).
    RenderUnavailable(String),
    /// A speaker write timed out; the speaker is reopened for the next chunk.
    RenderReopen,
    /// The speaker opened: path, period, buffer, latency (for the owner's log).
    RenderOpened(String),
    /// CF-MEDIA-68: the playback speed changed (for the owner's log).
    Speed { speed: f64, lag_ms: f64, baseline_ms: Option<f64> },
}

pub struct CallAudio {
    commands: Sender<Command>,
    events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
    end_fence: Option<Arc<AtomicBool>>,
}

impl CallAudio {
    pub fn start(gain: f32) -> Self {
        Self::with_end_fence(gain, false, None)
    }
    /// Private calls opt into immediate terminal cleanup with the media gate.
    /// Source retains its existing lifecycle and shutdown control arm.
    pub fn start_call(gain: f32) -> Self {
        Self::with_end_fence(gain, crate::call::media_gate_enabled(), None)
    }
    /// Stop queued Watch sound on Leave even while the transport is busy.
    pub fn start_watch(gain: f32, leave: Arc<AtomicBool>) -> Self {
        let enabled = std::env::var("MESHLIVE_WATCH_AUDIO_FENCE").as_deref() != Ok("0");
        Self::with_end_fence(gain, enabled, enabled.then_some(leave))
    }
    fn with_end_fence(gain: f32, enabled: bool, leave: Option<Arc<AtomicBool>>) -> Self {
        let (commands, inbox) = mpsc::channel();
        let (outbox, events) = mpsc::channel();
        let gain = gain.clamp(0.0, 1.0);
        let stopped = Arc::new(AtomicBool::new(false));
        let end_fence = enabled.then(|| stopped.clone());
        let thread = std::thread::Builder::new()
            .name("meshlive-call-audio".into())
            .spawn(move || Worker { inbox: Some(inbox), outbox: Some(outbox), gain, last_speed: 1.0, stopped, leave, ..Worker::default() }.run())
            .ok();
        Self { commands, events, thread, end_fence }
    }
    /// Bypass pending playback/capture commands when the call or Watch ends.
    /// False in the retained control arm. Already submitted device bytes
    /// cannot be recalled; the WASAPI write observes this cancellation flag.
    pub fn stop_now(&self) -> bool {
        let Some(stopped) = &self.end_fence else { return false };
        stopped.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Quit);
        true
    }
    pub fn stopped(&self) -> bool {
        self.end_fence.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire))
    }
    /// PCM to play, in order.
    pub fn play(&self, pcm: Vec<i16>) {
        let _ = self.commands.send(Command::Play(pcm));
    }
    pub fn ringback(&self, on: bool) {
        let _ = self.commands.send(Command::Ringback(on));
    }
    /// Opens the microphone (`device`: the user's pick, else the best).
    pub fn start_capture(&self, device: Option<String>) {
        let _ = self.commands.send(Command::StartCapture(device));
    }
    pub fn stop_capture(&self) {
        let _ = self.commands.send(Command::StopCapture);
    }
    /// Reopen the source microphone, acknowledging after any old queued frames.
    pub fn restart_capture(&self, fence: u64) {
        let _ = self.commands.send(Command::RestartCapture(fence));
    }
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}
impl Drop for CallAudio {
    fn drop(&mut self) {
        if !self.stop_now() { let _ = self.commands.send(Command::Quit); }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The playback gain from `MESHLIVE_PLAYBACK_GAIN` (0..=1; never above 1).
pub fn gain_from_env() -> f32 {
    std::env::var("MESHLIVE_PLAYBACK_GAIN").ok().and_then(|v| v.parse::<f32>().ok()).map_or(1.0, |g| g.clamp(0.0, 1.0))
}

/// One ringback period: 1 s of 425 Hz at about -24 dBFS, then 4 s of silence (pure).
pub fn ringback_sample(index: u64, gain: f32) -> i16 {
    let period = 5 * 48_000;
    let at = index % period;
    if at >= 48_000 {
        return 0;
    }
    let t = at as f64 / 48_000.0;
    let level = 2_000.0 * f64::from(gain);
    ((t * 425.0 * std::f64::consts::TAU).sin() * level) as i16
}

/// Scales PCM by `gain` (pure; 1.0 leaves it untouched).
pub fn scale(pcm: &mut [i16], gain: f32) {
    if gain < 1.0 {
        for s in pcm {
            *s = (f32::from(*s) * gain) as i16;
        }
    }
}

/// The call's microphone: the endpoint (communications category) or, with
/// the Experimental `MESHLIVE_VOICE_DSP=1`, the Voice Capture DSP.
enum Mic {
    Endpoint(Microphone),
    Dsp(VoiceDsp),
}
impl Mic {
    fn next_frame(&mut self, deadline: Instant) -> Result<[i16; FRAME_SAMPLES], crate::capture::Error> {
        match self {
            Mic::Endpoint(m) => m.next_frame(deadline),
            Mic::Dsp(d) => d.next_frame(deadline),
        }
    }
}

/// Whether to try the voice DSP (pure): only when switched on and the user
/// did not pick a microphone (the DSP opens the default devices only).
pub fn use_voice_dsp(requested: bool, chosen: Option<&str>) -> Result<(), &'static str> {
    match (requested, chosen) {
        (false, _) => Err("off"),
        (true, Some(_)) => Err("a microphone was chosen; the DSP uses the default devices only"),
        (true, None) => Ok(()),
    }
}

/// The thread's MMCSS "Audio" registration, reverted on drop.
struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);
impl Mmcss {
    fn audio() -> Self {
        let mut index = 0u32;
        let handle = unsafe { windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(windows::core::w!("Audio"), &mut index) };
        Self(handle.ok())
    }
}
impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

#[derive(Default)]
struct Worker {
    inbox: Option<Receiver<Command>>,
    outbox: Option<Sender<Event>>,
    gain: f32,
    render: Option<Playback<Wasapi>>,
    render_failed: bool,
    queue: VecDeque<i16>,
    ringback: Option<u64>,
    mic: Option<Mic>,
    watch: Option<DeviceWatch>,
    device: Option<String>,
    sync: crate::time_stretch::PlayoutSync,
    stretch: crate::time_stretch::Wsola,
    stretching: bool,
    last_speed: f64,
    stopped: Arc<AtomicBool>,
    leave: Option<Arc<AtomicBool>>,
}
// The channels are set at construction; Default only fills the rest.
impl Worker {
    fn stop_requested(&mut self) -> bool {
        if self.leave.as_ref().is_some_and(|leave| leave.load(Ordering::Acquire)) { self.stopped.store(true, Ordering::Release); }
        if !self.stopped.load(Ordering::Acquire) { return false; }
        self.queue.clear();
        self.ringback = None;
        self.mic = None;
        self.watch = None;
        self.render = None;
        true
    }
    fn send(&self, event: Event) {
        if let Some(out) = &self.outbox {
            let _ = out.send(event);
        }
    }
    /// The call's render endpoint: the one in the microphone's device
    /// (`audio_devices::call_render`), else None (the communications default).
    fn render_target(&self) -> Option<(String, &'static str)> {
        let mic = match &self.mic {
            Some(Mic::Endpoint(m)) => Some(m.endpoint_id.clone()),
            // The DSP captures from the default devices.
            Some(Mic::Dsp(_)) => None,
            None => self.device.clone(),
        };
        audio_devices::call_render(&audio_devices::list(), mic.as_deref()).map(|(id, reason)| (id, reason.word()))
    }
    /// After the microphone opened: an open speaker on another device than
    /// the rule now picks is closed and reopens at the next chunk.
    fn follow_mic(&mut self) {
        let Some(current) = self.render.as_ref().and_then(|r| r.device()).map(|d| d.endpoint_id().to_owned()) else { return };
        let wanted = self.render_target().map(|(id, _)| id);
        if wanted.is_some_and(|id| id != current) {
            self.render = None;
        }
    }
    fn open_mic(&mut self) {
        let mut dsp_log = Vec::new();
        match use_voice_dsp(crate::capture::voice_dsp_requested(), self.device.as_deref()) {
            Ok(()) => match VoiceDsp::open() {
                Ok(dsp) => {
                    let log = vec![dsp.effects.clone(), "mic_selected name=\"voice capture DSP\" reason=MESHLIVE_VOICE_DSP=1 (Experimental)".to_owned()];
                    self.mic = Some(Mic::Dsp(dsp));
                    self.send(Event::MicOn(log));
                    self.follow_mic();
                    return;
                }
                Err(error) => dsp_log.push(format!("voice_dsp fallback=endpoint error={error:?}")),
            },
            Err("off") => {}
            Err(reason) => dsp_log.push(format!("voice_dsp fallback=endpoint reason=\"{reason}\"")),
        }
        match audio_devices::open_best_microphone(Duration::from_secs(1), self.device.as_deref()) {
            Ok((mic, mut log)) => {
                // Echo: what the endpoint's communications processing reports.
                log.push(mic.effects.clone());
                log.extend(dsp_log);
                self.mic = Some(Mic::Endpoint(mic));
                self.follow_mic();
                self.send(Event::MicOn(log));
            }
            Err((verdict, _)) => {
                self.mic = None;
                self.send(Event::MicUnavailable(verdict));
            }
        }
    }
    fn run(&mut self) {
        let inbox = self.inbox.take().expect("inbox");
        // CF-OUT-17/18: the Multimedia Class Scheduler's "Audio" class, so a
        // busy PC does not starve render or capture (released on exit).
        let _mmcss = Mmcss::audio();
        let stopped = self.stopped.clone();
        loop {
            if self.stop_requested() { return; }
            // Commands: block briefly only when nothing is captured or played.
            let idle = self.mic.is_none() && self.queue.is_empty() && self.ringback.is_none();
            let command = if idle {
                match inbox.recv_timeout(Duration::from_millis(50)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match inbox.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            if self.stop_requested() { return; }
            match command {
                Some(Command::Quit) => return,
                Some(Command::Play(mut pcm)) => {
                    scale(&mut pcm, self.gain);
                    // CF-MEDIA-68: drift is corrected by playback speed (pitch
                    // kept), from the lag this queue holds; never by dropping.
                    let lag_ms = self.queue.len() as f64 / 48.0;
                    let speed = self.sync.on_chunk(lag_ms);
                    if speed != self.last_speed {
                        self.last_speed = speed;
                        self.send(Event::Speed { speed, lag_ms, baseline_ms: self.sync.baseline_ms() });
                    }
                    if speed == 1.0 {
                        if self.stretching {
                            self.stretching = false;
                            self.queue.extend(self.stretch.flush());
                        }
                        self.queue.extend(pcm);
                    } else {
                        self.stretching = true;
                        self.queue.extend(self.stretch.process(&pcm, speed));
                    }
                    // A safety bound only (2 s): the speed rule keeps the queue near its baseline.
                    while self.queue.len() > 48_000 * 2 {
                        self.queue.pop_front();
                    }
                }
                Some(Command::Ringback(on)) => self.ringback = if on { Some(self.ringback.unwrap_or(0)) } else { None },
                Some(Command::StartCapture(device)) => {
                    self.device = device;
                    if self.mic.is_none() {
                        self.open_mic();
                        self.watch = DeviceWatch::start().ok();
                    }
                }
                Some(Command::StopCapture) => {
                    self.mic = None;
                    self.watch = None;
                }
                Some(Command::RestartCapture(fence)) => {
                    self.mic = None;
                    self.watch = None;
                    self.open_mic();
                    self.watch = DeviceWatch::start().ok();
                    self.send(Event::CaptureRestarted(fence));
                }
                None => {}
            }
            if self.stop_requested() { return; }
            // A default changed or a device went away: reopen, the call goes on.
            if self.mic.is_some() && self.watch.as_ref().is_some_and(DeviceWatch::take) {
                self.mic = None;
                self.open_mic();
            }
            if let (Some(mic), Some(out)) = (self.mic.as_mut(), self.outbox.as_ref()) {
                while !stopped.load(Ordering::Acquire) {
                    let Ok(frame) = mic.next_frame(Instant::now() + Duration::from_millis(5)) else { break };
                    if stopped.load(Ordering::Acquire) { break; }
                    let _ = out.send(Event::Frame(Box::new(frame)));
                }
            }
            if self.stop_requested() { return; }
            // Ringback fills the queue 20 ms at a time while nothing else plays.
            if let Some(index) = self.ringback.as_mut() {
                if self.queue.len() < 960 * 2 {
                    for _ in 0..960 {
                        self.queue.push_back(ringback_sample(*index, self.gain));
                        *index += 1;
                    }
                }
            }
            if !self.queue.is_empty() && !self.render_failed {
                if self.render.is_none() {
                    let target = self.render_target();
                    match Wasapi::open_call(target.as_ref().map(|(id, reason)| (id.as_str(), *reason))) {
                        Ok(r) => {
                            if let Some(device) = r.device() {
                                self.send(Event::RenderOpened(device.describe()));
                            }
                            self.render = Some(r);
                        }
                        Err(error) => {
                            self.render_failed = true;
                            self.queue.clear();
                            self.send(Event::RenderUnavailable(format!("{error}")));
                        }
                    }
                }
                if let Some(render) = self.render.as_mut() {
                    let chunk: Vec<i16> = self.queue.drain(..self.queue.len().min(960)).collect();
                    if let Err(error) = render.write(&chunk, Instant::now() + Duration::from_millis(40), &stopped) {
                        // A write that timed out (the engine did not drain for
                        // 40 ms, e.g. this thread was descheduled) closed the
                        // device; it is reopened for the next chunk instead of
                        // ending the call's sound (measured: render_latency A/B,
                        // write#447 waited 108 ms). Other errors are final.
                        self.render = None;
                        if matches!(error, crate::playback::Error::Timeout) {
                            self.send(Event::RenderReopen);
                        } else {
                            self.render_failed = true;
                            self.send(Event::RenderUnavailable(format!("{error}")));
                        }
                    }
                }
            } else if self.render_failed {
                self.queue.clear();
            }
        }
    }
}

#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ending_a_call_discards_queued_sound_instead_of_waiting_behind_it() {
        for ended in [false, true] {
            let (commands, inbox) = mpsc::channel();
            commands.send(Command::Play(vec![12_000; 960])).unwrap();
            commands.send(Command::Ringback(true)).unwrap();
            commands.send(Command::Quit).unwrap();
            let mut worker = Worker {
                inbox: Some(inbox),
                queue: VecDeque::from(vec![8_000; 960]),
                stopped: Arc::new(AtomicBool::new(ended)),
                // No physical output endpoint in this deterministic queue flow.
                render_failed: true,
                ..Worker::default()
            };
            worker.run();
            assert!(worker.queue.is_empty());
            assert_eq!(worker.ringback.is_none(), ended,
                "terminal cancellation skips commands ahead of Quit; the retained arm processes them");
        }
    }

    #[test]
    fn terminal_audio_fence_stays_closed_to_late_playback_commands() {
        let audio = CallAudio::with_end_fence(0.0, true, None);
        assert!(!audio.stopped());
        assert!(audio.stop_now());
        audio.play(vec![12_000; 960]);
        audio.ringback(true);
        assert!(audio.stopped());
        assert!(audio.stop_now(), "repeated terminal cleanup is harmless");
        drop(audio); // joins the cancelled worker; no capture or playback was opened
    }

    #[test]
    fn leaving_watch_skips_pending_sound_without_cancelling_a_replacement_parent() {
        for fenced in [false, true] {
            let leave = Arc::new(AtomicBool::new(false));
            let (commands, inbox) = mpsc::channel();
            commands.send(Command::Play(vec![12_000; 960])).unwrap();
            commands.send(Command::Ringback(true)).unwrap();
            commands.send(Command::Quit).unwrap();
            let mut worker = Worker {
                inbox: Some(inbox), queue: VecDeque::from(vec![8_000; 960]),
                leave: fenced.then(|| leave.clone()), render_failed: true,
                ..Worker::default()
            };
            leave.store(true, Ordering::Release); // transport need not make progress
            worker.run();
            assert!(worker.queue.is_empty());
            assert_eq!(worker.ringback.is_none(), fenced);
            assert_eq!(worker.stopped.load(Ordering::Acquire), fenced);
        }
        let watching = Arc::new(AtomicBool::new(false));
        let old_parent = CallAudio::with_end_fence(0.0, true, Some(watching.clone()));
        assert!(old_parent.stop_now()); // withdrawal or loss ends only this owner
        drop(old_parent);
        assert!(!watching.load(Ordering::Acquire));
        let replacement = CallAudio::with_end_fence(0.0, true, Some(watching.clone()));
        watching.store(true, Ordering::Release);
        drop(replacement);
    }

    #[test]
    fn ringback_is_one_quiet_second_in_five_and_gain_only_lowers() {
        let on: Vec<i16> = (0..48_000).map(|i| ringback_sample(i, 1.0)).collect();
        let peak = on.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((1_900..=2_000).contains(&peak), "about -24 dBFS: {peak}");
        assert!((48_000..5 * 48_000).all(|i| ringback_sample(i, 1.0) == 0));
        assert_eq!(ringback_sample(5 * 48_000 + 100, 1.0), ringback_sample(100, 1.0));
        let quiet = (0..48_000).map(|i| ringback_sample(i, 0.01).unsigned_abs()).max().unwrap();
        assert!(quiet <= 20);
        let mut pcm = vec![10_000i16, -10_000];
        scale(&mut pcm, 1.0);
        assert_eq!(pcm, [10_000, -10_000]);
        scale(&mut pcm, 0.5);
        assert_eq!(pcm, [5_000, -5_000]);
    }
    #[test]
    fn the_voice_dsp_is_tried_only_when_switched_on_and_no_microphone_was_chosen() {
        assert_eq!(use_voice_dsp(false, None), Err("off"));
        assert_eq!(use_voice_dsp(false, Some("id")), Err("off"));
        assert!(use_voice_dsp(true, Some("id")).unwrap_err().contains("default devices"));
        assert_eq!(use_voice_dsp(true, None), Ok(()));
    }
}
