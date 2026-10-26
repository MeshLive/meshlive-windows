//! Video receive path for watching and video calls: admitted video packets are
//! timed at arrival on the shared playout clock, planned by the Android
//! decoder-format tracker (newest configuration, size/configuration rebuilds,
//! discontinuity resets) and queued in order for the decoder. Only video that
//! cannot be decoded is skipped: before the first keyframe and the queue
//! before a discontinuity or rebuild keyframe. A full queue is backpressure
//! (stop reading the link), never a silent drop. Decoded frames carry their
//! render time from the clock; the display path presents them without copies
//! (the GPU texture path is `video::surface`).
use crate::{
    media::{self, AUDIO, CONFIG, KEY_FRAME, VideoHeader},
    playout::Clock,
    video::{FormatError, FormatTracker, VideoFormat},
};
use std::{collections::VecDeque, sync::Arc};

/// Sized for the whole adaptive range (Auto up to 2 s at 30 FPS, with margin).
pub const MAX_QUEUED_FRAMES: usize = 240;
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_CONFIGURATIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The queue is full: stop reading the link until the decoder catches up.
    Full,
    Format(FormatError),
    Decoder,
}

/// The decoder seen by this path (Media Foundation in production, fakes in tests).
pub trait FrameDecoder {
    /// Create or recreate the decoder for this format (initial or rebuild).
    fn configure(&mut self, format: &VideoFormat) -> Result<(), ()>;
    /// `Ok(false)` is backpressure: the same input is offered again later.
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()>;
    /// The next decoded frame's presentation timestamp, if one is ready.
    fn pull(&mut self) -> Result<Option<i64>, ()>;
}

impl<T: FrameDecoder + ?Sized> FrameDecoder for Box<T> {
    fn configure(&mut self, format: &VideoFormat) -> Result<(), ()> {
        (**self).configure(format)
    }
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()> {
        (**self).push(payload, pts_us, key, config)
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        (**self).pull()
    }
}

enum Step {
    Configure(Arc<VideoFormat>),
    Input {
        payload: Arc<[u8]>,
        pts_us: i64,
        key: bool,
        config: bool,
    },
}

pub struct VideoReceiver {
    header: VideoHeader,
    pending: Vec<Vec<u8>>,
    tracker: Option<FormatTracker>,
    queue: VecDeque<Step>,
    frames: usize,
    bytes: usize,
    skipped: u64,
    /// After an overflow: drop input until the next keyframe.
    waiting_key: bool,
    /// CF-MEDIA-66: late video may grow the shared clock only this far.
    growth_limit_ms: Option<i64>,
}

/// A decoder that accepts everything and reports each input as one frame:
/// for an owner that does not show the peer's video (no surface yet), so the
/// receive queue never fills and the link keeps being read.
#[derive(Default)]
pub struct DiscardFrames {
    ready: VecDeque<i64>,
    pub inputs: u64,
}
impl FrameDecoder for DiscardFrames {
    fn configure(&mut self, _: &crate::video::VideoFormat) -> Result<(), ()> {
        Ok(())
    }
    fn push(&mut self, _: &[u8], pts_us: i64, _: bool, config: bool) -> Result<bool, ()> {
        self.inputs += 1;
        if !config {
            self.ready.push_back(pts_us);
        }
        Ok(true)
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        Ok(self.ready.pop_front())
    }
}

impl VideoReceiver {
    /// CF-MEDIA-66: limit how far late video grows the shared clock.
    pub fn set_growth_limit_ms(&mut self, limit: Option<i64>) {
        self.growth_limit_ms = limit;
    }
    pub fn new(header: VideoHeader) -> Self {
        Self {
            header,
            pending: Vec::new(),
            tracker: None,
            queue: VecDeque::new(),
            frames: 0,
            bytes: 0,
            skipped: 0,
            waiting_key: false,
            growth_limit_ms: None,
        }
    }
    /// The queue overflowed (the owner is not keeping up): queued input is
    /// dropped and input is skipped until the next keyframe, so the decoder
    /// never sees a broken reference chain. The owner asks the peer for one.
    pub fn overflow(&mut self) {
        self.reset_queue();
        self.waiting_key = true;
    }
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting_key
    }
    /// Video arrived but nothing can be decoded until a keyframe: after an
    /// overflow or a recovery, or inter frames before the first keyframe.
    pub fn awaiting_keyframe(&self) -> bool {
        self.waiting_key || (self.tracker.is_none() && self.skipped > 0)
    }
    /// The decoder failed (CF-MEDIA-59): queued input is dropped, input waits
    /// for the next keyframe, and the decoder is recreated (`configure`) from
    /// the newest format before it.
    pub fn recover(&mut self) -> Result<(), Error> {
        self.reset_queue();
        self.waiting_key = true;
        if let Some(tracker) = &self.tracker {
            let format = tracker.recovery().map_err(Error::Format)?;
            self.push_configure(format);
        }
        Ok(())
    }
    /// Check before reading the next record from the link.
    pub fn can_accept(&self) -> bool {
        self.frames < MAX_QUEUED_FRAMES && self.bytes < MAX_QUEUED_BYTES
    }
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
    pub fn queued_frames(&self) -> usize {
        self.frames
    }
    fn push_input(&mut self, packet: &media::Packet<'_>) {
        self.frames += 1;
        self.bytes += packet.payload.len();
        self.queue.push_back(Step::Input {
            payload: Arc::from(packet.payload),
            pts_us: packet.pts_us,
            key: packet.flags & KEY_FRAME != 0,
            config: packet.flags & CONFIG != 0,
        });
    }
    fn reset_queue(&mut self) {
        let dropped = self
            .queue
            .iter()
            .filter(|s| matches!(s, Step::Input { config: false, .. }))
            .count();
        self.skipped += dropped as u64;
        // A configuration not yet applied must survive: only input is dropped.
        let configure = self.queue.iter().rev().find_map(|s| match s {
            Step::Configure(format) => Some(format.clone()),
            Step::Input { .. } => None,
        });
        self.queue.clear();
        self.frames = 0;
        self.bytes = 0;
        if let Some(format) = configure {
            self.queue.push_back(Step::Configure(format));
        }
    }
    fn push_configure(&mut self, format: Arc<VideoFormat>) {
        // A newer format replaces one that was never applied.
        if matches!(self.queue.back(), Some(Step::Configure(_))) {
            self.queue.pop_back();
        }
        self.queue.push_back(Step::Configure(format));
    }
    /// One admitted video packet at arrival.
    pub fn on_packet(
        &mut self,
        packet: media::Packet<'_>,
        now_ns: i64,
        clock: &mut Clock,
    ) -> Result<(), Error> {
        if packet.flags & AUDIO != 0 {
            return Ok(());
        }
        if !self.can_accept() {
            return Err(Error::Full);
        }
        let config = packet.flags & CONFIG != 0;
        if !config {
            // Configuration is untimed; only media anchors and grows the shared clock.
            clock.timing_limited(packet.pts_us, now_ns, self.growth_limit_ms);
        }
        let Some(tracker) = self.tracker.as_mut() else {
            if config {
                if self.pending.len() == MAX_PENDING_CONFIGURATIONS {
                    return Err(Error::Format(FormatError::Budget));
                }
                if !self.pending.iter().any(|p| p == packet.payload) {
                    self.pending.push(packet.payload.to_vec());
                }
                return Ok(());
            }
            if packet.flags & KEY_FRAME == 0 {
                self.skipped += 1;
                return Ok(());
            }
            let configs: Vec<&[u8]> = self.pending.iter().map(Vec::as_slice).collect();
            let tracker = FormatTracker::new(self.header, &configs, Some(packet.payload))
                .map_err(Error::Format)?;
            let format = tracker.recovery().map_err(Error::Format)?;
            self.tracker = Some(tracker);
            self.pending.clear();
            self.push_configure(format);
            self.push_input(&packet);
            return Ok(());
        };
        if self.waiting_key && !config {
            if packet.flags & KEY_FRAME == 0 {
                self.skipped += 1;
                return Ok(());
            }
            self.waiting_key = false;
        }
        let plan = tracker.plan(packet).map_err(Error::Format)?;
        if plan.reset_queue {
            self.reset_queue();
        }
        if let Some(rebuild) = plan.rebuild {
            self.push_configure(rebuild.format);
        }
        if plan.queue {
            self.push_input(&packet);
        }
        Ok(())
    }
    /// Feeds queued input in order until the decoder pushes back. Returns the
    /// number of inputs accepted.
    pub fn feed(&mut self, decoder: &mut impl FrameDecoder) -> Result<usize, Error> {
        self.feed_until(decoder, |_| true)
    }
    /// Feeds only input whose render target is within `lead_ns` (configuration
    /// and reconfiguration always), so a one-output-per-input decoder produces
    /// each picture close to its render time, as Android releases output at
    /// its timestamp. Late frames are fed at once, never dropped.
    pub fn feed_due(
        &mut self,
        decoder: &mut impl FrameDecoder,
        now_ns: i64,
        clock: &Clock,
        lead_ns: i64,
    ) -> Result<usize, Error> {
        self.feed_until(decoder, |pts| {
            clock.target_for(pts, now_ns).target_ns <= now_ns + lead_ns
        })
    }
    fn feed_until(
        &mut self,
        decoder: &mut impl FrameDecoder,
        due: impl Fn(i64) -> bool,
    ) -> Result<usize, Error> {
        let mut fed = 0;
        while let Some(step) = self.queue.front() {
            if let Step::Input {
                pts_us,
                config: false,
                ..
            } = step
            {
                if !due(*pts_us) {
                    break;
                }
            }
            match step {
                Step::Configure(format) => {
                    decoder.configure(format).map_err(|_| Error::Decoder)?;
                }
                Step::Input {
                    payload,
                    pts_us,
                    key,
                    config,
                } => {
                    if !decoder
                        .push(payload, *pts_us, *key, *config)
                        .map_err(|_| Error::Decoder)?
                    {
                        break;
                    }
                    self.frames -= 1;
                    self.bytes -= payload.len();
                    fed += 1;
                }
            }
            self.queue.pop_front();
        }
        Ok(fed)
    }
    /// Pulls every decoded frame with its render time (never in the past).
    /// Late frames are still shown (no frame loss by default).
    pub fn drain(
        &mut self,
        decoder: &mut impl FrameDecoder,
        now_ns: i64,
        clock: &Clock,
    ) -> Result<Vec<(i64, i64)>, Error> {
        let mut out = Vec::new();
        while let Some(pts) = decoder.pull().map_err(|_| Error::Decoder)? {
            out.push((pts, clock.render_time_ns(pts, now_ns)));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- decode thread

/// What the decode thread drives: a `FrameDecoder` configured from the
/// newest configuration record alone (a `VideoFormat` stays on the caller).
pub trait ThreadDecoder {
    fn configure_with(&mut self, newest: Option<&[u8]>) -> Result<(), ()>;
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()>;
    fn pull(&mut self) -> Result<Option<i64>, ()>;
    fn last_error(&self) -> Option<String>;
}
impl<P: FnMut(i64, &crate::video::DecodedFrame<'_>)> ThreadDecoder for crate::video::MfFrames<'_, P> {
    fn configure_with(&mut self, newest: Option<&[u8]>) -> Result<(), ()> {
        crate::video::MfFrames::configure_with(self, newest)
    }
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()> {
        FrameDecoder::push(self, payload, pts_us, key, config)
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        FrameDecoder::pull(self)
    }
    fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }
}

pub enum DecodeJob {
    Configure(Option<Vec<u8>>, std::sync::mpsc::SyncSender<Result<(), String>>),
    Push { payload: Vec<u8>, pts_us: i64, key: bool, config: bool },
}
/// Units handed to the decode thread and not yet accepted by the decoder.
pub const MAX_DECODE_IN_FLIGHT: usize = 4;
/// How long the thread offers one unit to a decoder that reports backpressure.
const PUSH_PATIENCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Runs the decoder on the decode thread until the caller goes away: every
/// unit in order (never dropped), backpressure absorbed here by pulling,
/// each decoded picture presented (and read back) on this thread.
pub fn serve_decoder<D: ThreadDecoder>(
    decoder: &mut D,
    jobs: &std::sync::mpsc::Receiver<DecodeJob>,
    decoded: &std::sync::mpsc::Sender<Result<i64, String>>,
    in_flight: &std::sync::atomic::AtomicUsize,
) {
    use std::sync::{atomic::Ordering, mpsc::RecvTimeoutError};
    let error = |d: &D, what: &str| d.last_error().unwrap_or_else(|| what.to_owned());
    loop {
        let job = match jobs.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(job) => Some(job),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut failed = false;
        match job {
            Some(DecodeJob::Configure(newest, reply)) => {
                let result = decoder.configure_with(newest.as_deref()).map_err(|()| error(decoder, "configure"));
                let _ = reply.send(result);
            }
            Some(DecodeJob::Push { payload, pts_us, key, config }) => {
                let until = std::time::Instant::now() + PUSH_PATIENCE;
                loop {
                    match decoder.push(&payload, pts_us, key, config) {
                        Ok(true) => break,
                        Ok(false) if std::time::Instant::now() < until => {
                            // The decoder is full: take its pictures, then offer again.
                            while let Ok(Some(pts)) = decoder.pull() {
                                if decoded.send(Ok(pts)).is_err() {
                                    return;
                                }
                            }
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Ok(false) => {
                            failed = decoded.send(Err(format!("push {} bytes: decoder full for {} ms", payload.len(), PUSH_PATIENCE.as_millis()))).is_err();
                            break;
                        }
                        Err(()) => {
                            failed = decoded.send(Err(error(decoder, "push"))).is_err();
                            break;
                        }
                    }
                }
                in_flight.fetch_sub(1, Ordering::AcqRel);
            }
            None => {}
        }
        if failed {
            return;
        }
        loop {
            match decoder.pull() {
                Ok(Some(pts)) => {
                    if decoded.send(Ok(pts)).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(()) => {
                    if decoded.send(Err(error(decoder, "pull"))).is_err() {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

/// A `FrameDecoder` whose decoder (and the presenter's readback) runs on its
/// own thread with its own graphics device, so the Bluetooth worker never
/// waits on decode. `push` hands the unit over (at most
/// `MAX_DECODE_IN_FLIGHT` waiting, then backpressure as from the decoder
/// itself); `pull` returns pictures the thread finished; a thread-side
/// failure is reported at the next `push` or `pull`, and `configure`
/// (initial or rebuild) waits for the thread's answer and clears it.
pub struct DecodeThread {
    jobs: Option<std::sync::mpsc::Sender<DecodeJob>>,
    decoded: std::sync::mpsc::Receiver<Result<i64, String>>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
    failed: bool,
    pub last_error: Option<String>,
}
impl DecodeThread {
    /// Starts `body`, which builds the decoder on the new thread and calls
    /// `serve_decoder` with the three channels it is given.
    pub fn spawn<F>(body: F) -> Self
    where
        F: FnOnce(std::sync::mpsc::Receiver<DecodeJob>, std::sync::mpsc::Sender<Result<i64, String>>, Arc<std::sync::atomic::AtomicUsize>) + Send + 'static,
    {
        let (jobs, inbox) = std::sync::mpsc::channel();
        let (out, decoded) = std::sync::mpsc::channel();
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = in_flight.clone();
        let thread = std::thread::Builder::new().name("meshlive-video-decode".into()).spawn(move || body(inbox, out, counter)).ok();
        Self { jobs: Some(jobs), decoded, in_flight, thread, failed: false, last_error: None }
    }
    /// Media Foundation on the thread: its own D3D11 device; `present`
    /// receives each picture there (do the NV12 readback in it).
    pub fn media_foundation<P>(header: VideoHeader, present: P) -> Self
    where
        P: FnMut(i64, &crate::video::DecodedFrame<'_>) + Send + 'static,
    {
        Self::spawn(move |jobs, decoded, in_flight| {
            let owner = match crate::video::device(false, false).and_then(|(gpu, _, _)| crate::video::DecodeDevice::new(&gpu)) {
                Ok(owner) => owner,
                Err(error) => {
                    // Answer every request with the reason until the caller goes away.
                    let reason = format!("decode device: {error:?}");
                    for job in jobs {
                        match job {
                            DecodeJob::Configure(_, reply) => {
                                let _ = reply.send(Err(reason.clone()));
                            }
                            DecodeJob::Push { .. } => {
                                in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                                let _ = decoded.send(Err(reason.clone()));
                            }
                        }
                    }
                    return;
                }
            };
            let mut frames = crate::video::MfFrames::new(&owner, header, present);
            serve_decoder(&mut frames, &jobs, &decoded, &in_flight);
        })
    }
    fn fail(&mut self, reason: String) {
        self.failed = true;
        self.last_error = Some(reason);
    }
}
impl FrameDecoder for DecodeThread {
    fn configure(&mut self, format: &VideoFormat) -> Result<(), ()> {
        self.configure_newest(format.configurations().last().map(|c| c.to_vec()))
    }
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()> {
        self.push_unit(payload, pts_us, key, config)
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        self.pull_decoded()
    }
}
impl DecodeThread {
    /// `configure` from the newest configuration record.
    pub fn configure_newest(&mut self, newest: Option<Vec<u8>>) -> Result<(), ()> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let sent = self.jobs.as_ref().is_some_and(|jobs| jobs.send(DecodeJob::Configure(newest, reply)).is_ok());
        let result = if sent { answer.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or_else(|_| Err("configure: no answer in 3 s".into())) } else { Err("decode thread ended".into()) };
        // Pictures and failures of the old decoder are history now.
        while self.decoded.try_recv().is_ok() {}
        match result {
            Ok(()) => {
                self.failed = false;
                Ok(())
            }
            Err(reason) => {
                self.fail(reason);
                Err(())
            }
        }
    }
    fn push_unit(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()> {
        use std::sync::atomic::Ordering;
        if self.failed {
            return Err(());
        }
        if self.in_flight.load(Ordering::Acquire) >= MAX_DECODE_IN_FLIGHT {
            return Ok(false);
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let job = DecodeJob::Push { payload: payload.to_vec(), pts_us, key, config };
        if self.jobs.as_ref().is_none_or(|jobs| jobs.send(job).is_err()) {
            self.fail("decode thread ended".into());
            return Err(());
        }
        Ok(true)
    }
    fn pull_decoded(&mut self) -> Result<Option<i64>, ()> {
        if self.failed {
            return Err(());
        }
        match self.decoded.try_recv() {
            Ok(Ok(pts)) => Ok(Some(pts)),
            Ok(Err(reason)) => {
                self.fail(reason);
                Err(())
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.fail("decode thread ended".into());
                Err(())
            }
        }
    }
}
impl Drop for DecodeThread {
    fn drop(&mut self) {
        // Closing the job channel ends the thread after its current unit.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{media::DISCONTINUITY, playout::Profile};
    // ---- decode thread

    struct ThreadFake {
        gate: Option<std::sync::mpsc::Receiver<()>>,
        ready: VecDeque<i64>,
        refuse_first: usize,
        fail_pts: Option<i64>,
        configured: Arc<std::sync::Mutex<Vec<Option<Vec<u8>>>>>,
    }
    impl ThreadDecoder for ThreadFake {
        fn configure_with(&mut self, newest: Option<&[u8]>) -> Result<(), ()> {
            self.configured.lock().unwrap().push(newest.map(<[u8]>::to_vec));
            Ok(())
        }
        fn push(&mut self, _: &[u8], pts_us: i64, _: bool, _: bool) -> Result<bool, ()> {
            if let Some(gate) = &self.gate {
                let _ = gate.recv();
            }
            if self.fail_pts == Some(pts_us) {
                return Err(());
            }
            if self.refuse_first > 0 {
                self.refuse_first -= 1;
                return Ok(false);
            }
            self.ready.push_back(pts_us);
            Ok(true)
        }
        fn pull(&mut self) -> Result<Option<i64>, ()> {
            Ok(self.ready.pop_front())
        }
        fn last_error(&self) -> Option<String> {
            Some("fake failure".into())
        }
    }
    fn thread_fake(gate: Option<std::sync::mpsc::Receiver<()>>, refuse_first: usize, fail_pts: Option<i64>) -> (DecodeThread, Arc<std::sync::Mutex<Vec<Option<Vec<u8>>>>>) {
        let configured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = configured.clone();
        let thread = DecodeThread::spawn(move |jobs, decoded, in_flight| {
            let mut fake = ThreadFake { gate, ready: VecDeque::new(), refuse_first, fail_pts, configured: inner };
            serve_decoder(&mut fake, &jobs, &decoded, &in_flight);
        });
        (thread, configured)
    }
    fn pulled(thread: &mut DecodeThread, n: usize) -> Vec<Result<Option<i64>, ()>> {
        let mut out = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while out.len() < n && std::time::Instant::now() < until {
            match thread.pull() {
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
                other => out.push(other),
            }
        }
        out
    }

    #[test]
    fn the_decode_thread_decodes_every_unit_in_order_through_decoder_backpressure() {
        let (mut thread, _) = thread_fake(None, 3, None);
        for pts in [0, 66, 133] {
            assert_eq!(thread.push(&[1, 2], pts, pts == 0, false), Ok(true));
        }
        assert_eq!(pulled(&mut thread, 3), vec![Ok(Some(0)), Ok(Some(66)), Ok(Some(133))]);
        assert_eq!(thread.pull(), Ok(None));
    }

    #[test]
    fn a_busy_decode_thread_is_backpressure_and_the_worker_never_waits() {
        let (release, gate) = std::sync::mpsc::channel();
        let (mut thread, _) = thread_fake(Some(gate), 0, None);
        let started = std::time::Instant::now();
        let accepted: Vec<bool> = (0..6).map(|i| thread.push(&[0], i * 66, i == 0, false).unwrap()).collect();
        assert!(started.elapsed() < std::time::Duration::from_millis(100), "pushing never waits on the codec");
        assert_eq!(accepted, [true, true, true, true, false, false], "at most {MAX_DECODE_IN_FLIGHT} in flight");
        for _ in 0..4 {
            release.send(()).unwrap();
        }
        assert_eq!(pulled(&mut thread, 4).len(), 4);
        // The refused units are offered again later, as with any backpressure.
        release.send(()).unwrap();
        assert_eq!(thread.push(&[0], 264, false, false), Ok(true));
        assert_eq!(pulled(&mut thread, 1), vec![Ok(Some(264))]);
    }

    #[test]
    fn a_decode_failure_reaches_the_pipeline_and_a_rebuild_clears_it() {
        let (mut thread, configured) = thread_fake(None, 0, Some(66));
        assert_eq!(thread.push(&[0], 66, true, false), Ok(true));
        assert_eq!(pulled(&mut thread, 1), vec![Err(())]);
        assert_eq!(thread.last_error.as_deref(), Some("fake failure"));
        assert_eq!(thread.push(&[0], 133, false, false), Err(()), "failed until reconfigured");
        assert_eq!(thread.configure_newest(Some(vec![7])), Ok(()));
        assert_eq!(*configured.lock().unwrap(), vec![Some(vec![7])]);
        assert_eq!(thread.push(&[0], 200, true, false), Ok(true));
        assert_eq!(pulled(&mut thread, 1), vec![Ok(Some(200))]);
        drop(thread);
    }
    const MS: i64 = 1_000_000;
    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        capacity: usize,
        out: VecDeque<i64>,
    }
    impl FrameDecoder for Fake {
        fn configure(&mut self, format: &VideoFormat) -> Result<(), ()> {
            self.log.push(format!(
                "configure {}x{} csd={}",
                format.width,
                format.height,
                format.configurations().len()
            ));
            Ok(())
        }
        fn push(&mut self, payload: &[u8], pts: i64, key: bool, config: bool) -> Result<bool, ()> {
            if self.capacity == 0 {
                return Ok(false);
            }
            self.capacity -= 1;
            self.log.push(format!(
                "push {} {pts} key={key} config={config}",
                payload[0]
            ));
            if !config {
                self.out.push_back(pts);
            }
            Ok(true)
        }
        fn pull(&mut self) -> Result<Option<i64>, ()> {
            Ok(self.out.pop_front())
        }
    }
    fn header() -> VideoHeader {
        VideoHeader {
            width: 640,
            height: 480,
            fps: 15,
            codec: 2,
            quarter_turns: 0,
            opus: true,
        }
    }
    fn p(flags: u8, pts_us: i64, byte: u8) -> media::Packet<'static> {
        let payload: &'static [u8] = Box::leak(vec![byte; 32].into_boxed_slice());
        media::Packet {
            flags,
            pts_us,
            payload,
        }
    }
    #[test]
    fn video_starts_at_the_first_keyframe_with_its_configuration_and_keeps_order() {
        let mut clock = Profile::Auto.clock();
        let mut rx = VideoReceiver::new(header());
        rx.on_packet(p(0, 0, 1), 0, &mut clock).unwrap();
        rx.on_packet(p(CONFIG, 0, 2), 0, &mut clock).unwrap();
        rx.on_packet(p(KEY_FRAME, 66_000, 3), 66 * MS, &mut clock)
            .unwrap();
        rx.on_packet(p(0, 133_000, 4), 133 * MS, &mut clock)
            .unwrap();
        rx.on_packet(p(AUDIO, 140_000, 9), 140 * MS, &mut clock)
            .unwrap();
        assert_eq!(rx.skipped(), 1);
        assert_eq!(rx.queued_frames(), 2);
        let mut decoder = Fake {
            capacity: 1,
            ..Fake::default()
        };
        assert_eq!(rx.feed(&mut decoder).unwrap(), 1);
        decoder.capacity = 5;
        assert_eq!(rx.feed(&mut decoder).unwrap(), 1);
        assert_eq!(
            decoder.log,
            vec![
                // The tracker cannot read a size from this synthetic payload: BTV3 maximum.
                "configure 640x480 csd=1",
                "push 3 66000 key=true config=false",
                "push 4 133000 key=false config=false",
            ]
        );
        // Frames render on the shared clock: first media at 66 ms + 250 ms.
        let frames = rx.drain(&mut decoder, 100 * MS, &clock).unwrap();
        assert_eq!(frames, vec![(66_000, 316 * MS), (133_000, 383 * MS)]);
    }
    #[test]
    fn a_discontinuity_keyframe_resets_the_queue_and_new_configuration_rebuilds() {
        let mut clock = Profile::Auto.clock();
        let mut rx = VideoReceiver::new(header());
        rx.on_packet(p(KEY_FRAME, 0, 1), 0, &mut clock).unwrap();
        rx.on_packet(p(0, 66_000, 2), 66 * MS, &mut clock).unwrap();
        rx.on_packet(
            p(KEY_FRAME | DISCONTINUITY, 133_000, 3),
            133 * MS,
            &mut clock,
        )
        .unwrap();
        assert_eq!(
            rx.skipped(),
            2,
            "queued frames before the discontinuity are dropped"
        );
        // The initial configuration that was never applied survives the reset.
        let mut probe = Fake {
            capacity: 0,
            ..Fake::default()
        };
        rx.feed(&mut probe).unwrap();
        assert_eq!(probe.log, vec!["configure 640x480 csd=0"]);
        // New configuration is held, then the next keyframe rebuilds with it.
        rx.on_packet(p(CONFIG, 0, 7), 200 * MS, &mut clock).unwrap();
        rx.on_packet(p(KEY_FRAME, 266_000, 4), 266 * MS, &mut clock)
            .unwrap();
        let mut decoder = Fake {
            capacity: 10,
            ..Fake::default()
        };
        rx.feed(&mut decoder).unwrap();
        assert_eq!(
            decoder.log,
            vec![
                "configure 640x480 csd=1",
                "push 4 266000 key=true config=false"
            ]
        );
    }
    #[test]
    fn frames_are_fed_close_to_their_render_time_and_late_ones_at_once() {
        let mut clock = Profile::Auto.clock();
        let mut rx = VideoReceiver::new(header());
        rx.on_packet(p(KEY_FRAME, 0, 1), 0, &mut clock).unwrap();
        rx.on_packet(p(0, 66_000, 2), 0, &mut clock).unwrap();
        rx.on_packet(p(0, 133_000, 3), 0, &mut clock).unwrap();
        let mut decoder = Fake {
            capacity: 10,
            ..Fake::default()
        };
        // Targets 250, 316 and 383 ms; with a 20 ms lead only the first is due at 230 ms.
        assert_eq!(
            rx.feed_due(&mut decoder, 230 * MS, &clock, 20 * MS)
                .unwrap(),
            1
        );
        assert_eq!(
            rx.feed_due(&mut decoder, 296 * MS, &clock, 20 * MS)
                .unwrap(),
            1
        );
        assert_eq!(
            rx.feed_due(&mut decoder, 10_000 * MS, &clock, 20 * MS)
                .unwrap(),
            1
        );
        assert_eq!(decoder.log.len(), 4);
    }
    #[test]
    fn a_full_queue_is_backpressure_not_loss() {
        let mut clock = Profile::Auto.clock();
        let mut rx = VideoReceiver::new(header());
        rx.on_packet(p(KEY_FRAME, 0, 1), 0, &mut clock).unwrap();
        for i in 1..MAX_QUEUED_FRAMES as i64 {
            rx.on_packet(p(0, i * 66_000, 2), i * 66 * MS, &mut clock)
                .unwrap();
        }
        assert!(!rx.can_accept());
        assert_eq!(rx.on_packet(p(0, 0, 2), 0, &mut clock), Err(Error::Full));
        let mut decoder = Fake {
            capacity: 1,
            ..Fake::default()
        };
        rx.feed(&mut decoder).unwrap();
        assert!(rx.can_accept());
        assert_eq!(rx.skipped(), 0);
    }
}
