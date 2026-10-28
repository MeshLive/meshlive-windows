//! Outgoing call video from generated input (no camera): Android's call
//! defaults (426x240, 15 FPS, 200 kbps) encoded once by `SourceEncoder` with
//! the codec the call header announced, paced by wall time, and handed to the
//! `CallSession` (which orders, fragments and seals). Started only when the
//! session reports `StartMedia { camera: true }` (after Accept, as Android).
//! Joins (the first picture) are forced keyframes; the peer's keyframe
//! requests go through the recovery backoff.
//! The encoder runs on its own thread (`EncodeThread`, with its own graphics
//! device): `worker_slow` showed 95-120 ms encode ticks on the Bluetooth
//! worker, which then did not read the link. Pictures go in newest-only
//! (one waiting slot), packets come back in order and are queued on the
//! session by the worker, so rate control and keyframe recovery are unchanged.
use crate::{
    call_session::{self, CallSession},
    generated::pattern_nv12,
    listen::UnitDecoder,
    media::VideoHeader,
    video::{
        Availability, Backend, DecodeDevice, EncodeError, EncoderSettings, MASK_AV1, MASK_VP9, SourceEncoder,
        SourceError, VideoEncoder,
    },
};
use std::time::{Duration, Instant};

/// Android's default call/stream picture (ARCHITECTURE "Defaults").
pub const WIDTH: u16 = 426;
pub const HEIGHT: u16 = 240;
pub const FPS: u8 = 15;
pub const BITRATE_KBPS: u32 = 200;

/// The codec the call header announces: the first encoder of the one choice
/// operation for this direction (`codecs::for_call`: today's Android order
/// with both Design HC masters off), or None when there is no common codec
/// (the call stays voice-only: the header then names the peer's first
/// decodable codec and no picture is ever sent).
pub fn header_for(peer_decode: u8, available: &Availability) -> (VideoHeader, Vec<(u8, Backend)>) {
    header_with(peer_decode, crate::video::advertised_encode_mask(), available, crate::video::codecs::Switches::current())
}
/// `BENCHMARK encoder_choice` for this direction (Design HC labels).
pub fn choice_line(peer_decode: u8, available: &Availability) -> String {
    let switches = crate::video::codecs::Switches::current();
    crate::video::codecs::for_call(peer_decode, available, switches).line(Some(peer_decode), switches)
}
/// `own_encode`: the encode mask this PC's end-to-end hello advertised; the
/// peer refuses a header codec outside it ("did not authenticate use of its
/// call codec"), so entries outside it are dropped.
pub fn header_with(peer_decode: u8, own_encode: u8, available: &Availability, switches: crate::video::codecs::Switches) -> (VideoHeader, Vec<(u8, Backend)>) {
    let plan: Vec<(u8, Backend)> = crate::video::codecs::for_call(peer_decode, available, switches)
        .list
        .into_iter()
        .filter(|(codec, _)| own_encode & crate::video::codecs::hello_bit(*codec) != 0)
        .collect();
    let codec = match plan.first() {
        Some((codec, _)) => *codec,
        None if peer_decode & MASK_VP9 != 0 => 2,
        None if peer_decode & MASK_AV1 != 0 => 3,
        None => 2,
    };
    // Only entries for the announced codec may be used later.
    let plan = plan.into_iter().filter(|(c, _)| *c == codec).collect();
    (
        VideoHeader {
            width: WIDTH,
            height: HEIGHT,
            fps: FPS,
            codec,
            quarter_turns: 0,
            opus: true,
        },
        plan,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No encoder for the announced codec opened.
    Encoder(EncodeError),
    Source(SourceError),
    Session(call_session::Error),
}

/// Alternative switch: `MESHLIVE_CALL_ENCODE_THREAD=0` encodes on the
/// Bluetooth worker as before (default: the encode thread).
pub const ENCODE_THREAD_SWITCH: &str = "MESHLIVE_CALL_ENCODE_THREAD";
pub fn encode_thread_enabled() -> bool {
    std::env::var(ENCODE_THREAD_SWITCH).ok().as_deref() != Some("0")
}

/// What the call needs from an encoder (the real `SourceEncoder`, or a test fake).
pub trait FrameEncoder {
    fn encode(&mut self, nv12: &[u8], pts_us: i64, deadline: Instant) -> Result<Vec<Vec<u8>>, SourceError>;
    fn request_keyframe(&mut self, now_ns: u64, force: bool);
    /// False when the encoder refused the rate.
    fn set_bitrate(&mut self, kbps: u32) -> bool;
}
impl FrameEncoder for SourceEncoder<'_> {
    fn encode(&mut self, nv12: &[u8], pts_us: i64, deadline: Instant) -> Result<Vec<Vec<u8>>, SourceError> {
        SourceEncoder::encode(self, nv12, pts_us, deadline)
    }
    fn request_keyframe(&mut self, now_ns: u64, force: bool) {
        SourceEncoder::request_keyframe(self, now_ns, force);
    }
    fn set_bitrate(&mut self, kbps: u32) -> bool {
        self.encoder_mut().set_bitrate(kbps).is_ok()
    }
}

/// The encode thread's inbox: one picture slot (newest only: a picture not
/// yet started is replaced by a newer one; nothing encoded is ever dropped
/// here, so the reference chain stays whole), and every control in order.
#[derive(Default)]
struct Inbox {
    picture: Option<(Vec<u8>, i64)>,
    keyframes: Vec<(u64, bool)>,
    bitrate: Option<u32>,
    quit: bool,
}
#[derive(Default)]
pub struct Shared {
    inbox: std::sync::Mutex<Inbox>,
    wake: std::sync::Condvar,
}
#[derive(Debug, PartialEq)]
pub enum Output {
    /// One picture's media packets, in order, and the encode time.
    Packets { packets: Vec<Vec<u8>>, encode_us: u64 },
    BitrateRefused(u32),
    /// The encoder failed; the thread has ended.
    Failed(SourceError),
}

/// Serves the inbox until quit or an encode failure (runs on the encode thread).
pub fn serve<E: FrameEncoder>(encoder: &mut E, shared: &Shared, out: &std::sync::mpsc::Sender<Output>) {
    loop {
        let (picture, keyframes, bitrate) = {
            let mut inbox = shared.inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while inbox.picture.is_none() && inbox.keyframes.is_empty() && inbox.bitrate.is_none() && !inbox.quit {
                inbox = shared.wake.wait(inbox).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if inbox.quit {
                return;
            }
            (inbox.picture.take(), std::mem::take(&mut inbox.keyframes), inbox.bitrate.take())
        };
        // Controls first: a request made before a picture applies to it.
        for (now_ns, force) in keyframes {
            encoder.request_keyframe(now_ns, force);
        }
        if let Some(kbps) = bitrate {
            if !encoder.set_bitrate(kbps) && out.send(Output::BitrateRefused(kbps)).is_err() {
                return;
            }
        }
        if let Some((nv12, pts_us)) = picture {
            let started = Instant::now();
            let result = encoder.encode(&nv12, pts_us, started + Duration::from_millis(200));
            let output = match result {
                Ok(packets) => Output::Packets { packets, encode_us: started.elapsed().as_micros() as u64 },
                Err(error) => Output::Failed(error),
            };
            let failed = matches!(output, Output::Failed(_));
            if out.send(output).is_err() || failed {
                return;
            }
        }
    }
}

/// The call's video encoder on its own thread (with its own graphics
/// device), so the Bluetooth worker never waits on the codec: pictures go in
/// through a newest-only slot, packets come back in order.
pub struct EncodeThread {
    shared: std::sync::Arc<Shared>,
    output: std::sync::mpsc::Receiver<Output>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Pictures replaced in the slot before the encoder took them.
    pub replaced: u64,
}
impl EncodeThread {
    /// Starts `body` on a new thread; it reports ready (the encoder's name)
    /// or an error through its first argument, then serves.
    pub fn spawn<F>(body: F) -> Result<(Self, String), Error>
    where
        F: FnOnce(std::sync::mpsc::SyncSender<Result<String, Error>>, std::sync::Arc<Shared>, std::sync::mpsc::Sender<Output>) + Send + 'static,
    {
        let shared = std::sync::Arc::new(Shared::default());
        let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
        let (out, output) = std::sync::mpsc::channel();
        let inner = shared.clone();
        let thread = std::thread::Builder::new()
            .name("meshlive-call-encode".into())
            .spawn(move || body(ready_tx, inner, out))
            .map_err(|_| Error::Encoder(EncodeError::Closed))?;
        let mut this = Self { shared, output, thread: Some(thread), replaced: 0 };
        match ready.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(name)) => Ok((this, name)),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                this.stop();
                Err(Error::Encoder(EncodeError::Timeout))
            }
        }
    }
    /// The real encoder: its own D3D11 device and Media Foundation runtime.
    fn open_real(settings: EncoderSettings, plan: Vec<(u8, Backend)>) -> Result<(Self, String), Error> {
        Self::spawn(move |ready, shared, out| {
            let owner = match crate::video::device(false, false).and_then(|(gpu, _, _)| DecodeDevice::new(&gpu)) {
                Ok(owner) => owner,
                Err(error) => {
                    let _ = ready.send(Err(Error::Encoder(error.into())));
                    return;
                }
            };
            let mut source = match VideoEncoder::open_planned(&owner, settings, &plan) {
                Ok((encoder, _skipped)) => {
                    let _ = ready.send(Ok(encoder.name()));
                    SourceEncoder::new(encoder, 60)
                }
                Err(error) => {
                    let _ = ready.send(Err(Error::Encoder(error)));
                    return;
                }
            };
            source.request_keyframe(0, true);
            serve(&mut source, &shared, &out);
        })
    }
    fn with_inbox(&self, change: impl FnOnce(&mut Inbox)) {
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        change(&mut inbox);
        self.shared.wake.notify_one();
    }
    /// Offers a picture (replacing one the encoder has not started).
    pub fn submit(&mut self, nv12: Vec<u8>, pts_us: i64) {
        let mut replaced = false;
        self.with_inbox(|inbox| replaced = inbox.picture.replace((nv12, pts_us)).is_some());
        self.replaced += u64::from(replaced);
    }
    pub fn request_keyframe(&self, now_ns: u64, force: bool) {
        self.with_inbox(|inbox| inbox.keyframes.push((now_ns, force)));
    }
    pub fn set_bitrate(&self, kbps: u32) {
        self.with_inbox(|inbox| inbox.bitrate = Some(kbps));
    }
    /// Everything finished since the last call, in order.
    pub fn drain(&self) -> Vec<Output> {
        self.output.try_iter().collect()
    }
    /// The next finished output, waiting at most `timeout`.
    pub fn next(&self, timeout: Duration) -> Option<Output> {
        self.output.recv_timeout(timeout).ok()
    }
    fn stop(&mut self) {
        self.with_inbox(|inbox| inbox.quit = true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for EncodeThread {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Engine<'a> {
    /// The encoder on the Bluetooth worker (`MESHLIVE_CALL_ENCODE_THREAD=0`).
    Inline(SourceEncoder<'a>),
    Thread(EncodeThread),
}

pub struct CallVideo<'a> {
    engine: Engine<'a>,
    settings: EncoderSettings,
    name: String,
    started: Instant,
    frame: u32,
    pub sent: u64,
    /// Packets the session refused (its own queue full).
    pub refused: u64,
    /// Inter frames not queued while waiting for a keyframe after a refusal.
    pub dropped: u64,
    /// Encode thread: pictures replaced before encoding, rate changes the
    /// encoder refused, and the slowest encode since the last rate line.
    pub replaced: u64,
    pub bitrate_refused: u64,
    max_encode_us: u64,
    /// A packet was refused: send nothing until the next keyframe.
    awaiting_key: bool,
    /// Real pictures (camera capture): NV12 at the header's size, or None
    /// when no picture is ready; without it the generated pattern is sent.
    frames: Option<Box<dyn FnMut() -> Option<Vec<u8>> + 'a>>,
    /// CF-MEDIA-33..36 rate control, sampled once a second.
    rate: crate::call_rate::RateController,
    /// The frame rate in use (the controller lowers it below the floor).
    fps: u32,
    /// The next picture's time (us since start) and pts.
    next_due_us: u64,
    next_sample: Option<Instant>,
    /// Encoded bytes produced, and the counters at the last sample.
    offered_bytes: u64,
    keyframe: bool,
    last: (u64, u64, u64),
}

/// The audio rate the controller reserves (Opus voice).
const AUDIO_KBPS: u32 = 32;
fn call_settings(header: &VideoHeader, relay_hops: u8) -> EncoderSettings {
    EncoderSettings { codec: header.codec, width: u32::from(header.width), height: u32::from(header.height),
        fps: if relay_hops > 0 { u32::from(header.fps).min(15) } else { u32::from(header.fps) },
        bitrate_kbps: if relay_hops > 0 { 110 } else { BITRATE_KBPS }, keyframe_interval_s: 60 }
}
impl<'a> CallVideo<'a> {
    /// Opens the first encoder of `plan` that works; returns its name too.
    /// By default on the encode thread (its own device); if that thread
    /// cannot start, on this thread with `owner`, as before.
    pub fn open(owner: &'a DecodeDevice, header: &VideoHeader, plan: &[(u8, Backend)]) -> Result<(Self, String), Error> {
        Self::open_for_call(owner, header, plan, 0)
    }
    /// Android's ordinary relayed-call cap; no measured-cap experiment.
    pub fn open_for_call(owner: &'a DecodeDevice, header: &VideoHeader, plan: &[(u8, Backend)], relay_hops: u8) -> Result<(Self, String), Error> {
        let settings = call_settings(header, relay_hops);
        let threaded = if encode_thread_enabled() { EncodeThread::open_real(settings, plan.to_vec()).ok() } else { None };
        let (engine, name) = match threaded {
            Some((thread, name)) => (Engine::Thread(thread), name),
            None => {
                let (encoder, _skipped) = VideoEncoder::open_planned(owner, settings, plan).map_err(Error::Encoder)?;
                let name = encoder.name();
                let mut source = SourceEncoder::new(encoder, 60);
                source.request_keyframe(0, true);
                (Engine::Inline(source), name)
            }
        };
        Ok((Self::with_engine(engine, settings, name.clone(), header), name))
    }
    fn with_engine(engine: Engine<'a>, settings: EncoderSettings, name: String, header: &VideoHeader) -> Self {
        Self {
            engine,
            settings,
            name,
            started: Instant::now(),
            frame: 0,
            sent: 0,
            refused: 0,
            dropped: 0,
            replaced: 0,
            bitrate_refused: 0,
            max_encode_us: 0,
            awaiting_key: false,
            frames: None,
            rate: crate::call_rate::RateController::new(settings.bitrate_kbps, settings.fps, u32::from(header.width), u32::from(header.height), AUDIO_KBPS),
            fps: settings.fps.max(1),
            next_due_us: 0,
            next_sample: None,
            offered_bytes: 0,
            keyframe: false,
            last: (0, 0, 0),
        }
    }
    /// True when the encoder runs on its own thread.
    pub fn threaded(&self) -> bool {
        matches!(self.engine, Engine::Thread(_))
    }
    /// CF-MEDIA-52: timestamps from `epoch` (the call's media clock shared
    /// with audio), the first picture due now.
    pub fn align_to(&mut self, epoch: Instant) {
        self.started = epoch;
        self.next_due_us = epoch.elapsed().as_micros() as u64;
    }
    /// Sends real pictures from `frames` instead of the generated pattern.
    pub fn use_frames(&mut self, frames: Box<dyn FnMut() -> Option<Vec<u8>> + 'a>) {
        self.frames = Some(frames);
    }
    fn request_keyframe(&mut self, now_ns: u64, force: bool) {
        match &mut self.engine {
            Engine::Inline(source) => source.request_keyframe(now_ns, force),
            Engine::Thread(thread) => thread.request_keyframe(now_ns, force),
        }
    }
    /// The peer asked for a keyframe (recovery: backoff applies).
    pub fn keyframe_requested(&mut self, now_ns: u64) {
        self.request_keyframe(now_ns, false);
    }
    /// Encodes the picture due now (at most one per call, so a stall never
    /// bursts) and queues its packets on the session. With the encode
    /// thread, the picture is handed over and the packets finished since the
    /// last tick are queued; this call never waits on the codec.
    pub fn tick<D: UnitDecoder>(&mut self, session: &mut CallSession<D>) -> Result<usize, Error> {
        let mut count = 0;
        if let Engine::Thread(thread) = &self.engine {
            for output in thread.drain() {
                match output {
                    Output::Packets { packets, encode_us } => {
                        self.max_encode_us = self.max_encode_us.max(encode_us);
                        count += self.queue_packets(packets, session)?;
                    }
                    Output::BitrateRefused(_) => self.bitrate_refused += 1,
                    Output::Failed(error) => return Err(Error::Source(error)),
                }
            }
        }
        let settings = self.settings;
        // Pictures at the controller's frame rate (CF-MEDIA-35), never in a burst.
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        if elapsed_us < self.next_due_us {
            return Ok(count);
        }
        let pts_us = self.next_due_us as i64;
        self.next_due_us = (self.next_due_us + 1_000_000 / u64::from(self.fps)).max(elapsed_us.saturating_sub(1_000_000 / u64::from(self.fps)));
        let picture = match self.frames.as_mut() {
            Some(next) => match next() {
                Some(picture) if picture.len() == (settings.width * settings.height * 3 / 2) as usize => picture,
                // No real picture yet: nothing is sent (never a fake one).
                _ => return Ok(count),
            },
            None => pattern_nv12(settings.width as usize, settings.height as usize, self.frame),
        };
        self.frame += 1;
        match &mut self.engine {
            Engine::Thread(thread) => {
                thread.submit(picture, pts_us);
                self.replaced = thread.replaced;
                Ok(count)
            }
            Engine::Inline(source) => {
                let packets = source.encode(&picture, pts_us, Instant::now() + Duration::from_millis(200)).map_err(Error::Source)?;
                Ok(count + self.queue_packets(packets, session)?)
            }
        }
    }
    /// Encode thread: waits up to `timeout` for the next finished picture and
    /// queues its packets (tests, and a caller that must not lose the last
    /// picture). Inline: nothing is pending, returns 0.
    pub fn wait_encoded<D: UnitDecoder>(&mut self, session: &mut CallSession<D>, timeout: Duration) -> Result<usize, Error> {
        let Engine::Thread(thread) = &self.engine else {
            return Ok(0);
        };
        match thread.next(timeout) {
            Some(Output::Packets { packets, encode_us }) => {
                self.max_encode_us = self.max_encode_us.max(encode_us);
                self.queue_packets(packets, session)
            }
            Some(Output::BitrateRefused(_)) => {
                self.bitrate_refused += 1;
                Ok(0)
            }
            Some(Output::Failed(error)) => Err(Error::Source(error)),
            None => Ok(0),
        }
    }
    /// Queues one picture's packets on the session (keyframe recovery after
    /// a refusal, CF-MEDIA-07 discontinuity flag).
    fn queue_packets<D: UnitDecoder>(&mut self, packets: Vec<Vec<u8>>, session: &mut CallSession<D>) -> Result<usize, Error> {
        let count = packets.len();
        let now_ns = self.started.elapsed().as_nanos() as u64;
        for mut packet in packets {
            let decoded = crate::media::Packet::decode(&packet, crate::media::MAX_PAYLOAD).ok();
            let key = decoded.as_ref().is_some_and(|p| p.flags & crate::media::KEY_FRAME != 0);
            self.offered_bytes += decoded.as_ref().map_or(0, |p| p.payload.len() as u64);
            self.keyframe |= key;
            if self.awaiting_key && !key {
                // Overload: inter frames after a lost one cannot be decoded.
                self.dropped += 1;
                continue;
            }
            // CF-MEDIA-07: the recovery keyframe says frames were skipped.
            let flagged = if self.awaiting_key {
                decoded.as_ref().and_then(|p| {
                    crate::media::Packet { flags: p.flags | crate::media::DISCONTINUITY, pts_us: p.pts_us, payload: p.payload }.encode().ok()
                })
            } else {
                None
            };
            if let Some(flagged) = flagged {
                packet = flagged;
            }
            match session.send(packet) {
                Ok(true) => {
                    self.sent += 1;
                    self.awaiting_key = false;
                }
                // Camera off or not accepted yet: nothing to recover.
                Ok(false) => {}
                // The send queue is full (link slower than the encoder): drop
                // this picture and everything up to a new keyframe; never fail
                // the call and never touch audio.
                Err(call_session::Error::Backlog) => {
                    self.refused += 1;
                    if !self.awaiting_key {
                        self.awaiting_key = true;
                        // Forced: the queue was purged, recovery cannot wait.
                        self.request_keyframe(now_ns, true);
                    }
                }
                Err(error) => return Err(Error::Session(error)),
            }
        }
        Ok(count)
    }
    /// Once a second: one rate-control sample (CF-MEDIA-33..35); a change is
    /// applied to the encoder at once and described in the returned line.
    pub fn rate_tick<D: UnitDecoder>(&mut self, session: &CallSession<D>) -> Option<String> {
        let now = Instant::now();
        let next = *self.next_sample.get_or_insert(now + Duration::from_secs(1));
        if now < next {
            return None;
        }
        self.next_sample = Some(next + Duration::from_secs(1));
        let stats = session.stats();
        let written = session.delivery().written();
        let shed = self.refused + self.dropped;
        let (last_written, last_offered, last_shed) = self.last;
        self.last = (written, self.offered_bytes, shed);
        let sample = crate::call_rate::Sample {
            now_ns: self.started.elapsed().as_nanos() as u64,
            // The backlog's drain time at the target rate (bytes, not frames:
            // one queued frame is not 66 ms of congestion).
            app_backlog_ms: (stats.video_queued_bytes as u64 * 8 / u64::from(self.rate.target_kbps().max(1))) as i32,
            peer_queue_ms: session.delivery().queue_ms(),
            delivered_kbps: session.delivery().delivered_kbps(),
            carried_kbps: (((written - last_written) * 8 / 1000) as u32).saturating_sub(AUDIO_KBPS),
            offered_kbps: ((self.offered_bytes - last_offered) * 8 / 1000) as u32,
            peer_late: session.peer_late(),
            shed: shed > last_shed,
            keyframe: std::mem::take(&mut self.keyframe),
        };
        let before = self.rate.target_kbps();
        let decision = self.rate.on_sample(&sample);
        let target = self.rate.target_kbps();
        if target == before {
            return None;
        }
        // The encode thread applies the rate before its next picture; a
        // refusal there is counted in `bitrate_refused`.
        let applied = match &mut self.engine {
            Engine::Inline(source) => source.encoder_mut().set_bitrate(target).is_ok(),
            Engine::Thread(thread) => {
                thread.set_bitrate(target);
                true
            }
        };
        self.fps = self.rate.fps();
        Some(format!(
            "decision={decision:?} target_kbps={target} fps={} applied={applied} queue_ms={} peer_queue_ms={:?} delivered_kbps={:?} carried_kbps={} offered_kbps={} peer_late={} shed={} encode_thread={} replaced={} bitrate_refused={} max_encode_ms={}",
            self.fps,
            sample.app_backlog_ms,
            sample.peer_queue_ms,
            sample.delivered_kbps,
            sample.carried_kbps,
            sample.offered_kbps,
            sample.peer_late,
            sample.shed,
            self.threaded(),
            self.replaced,
            self.bitrate_refused,
            std::mem::take(&mut self.max_encode_us) / 1000
        ))
    }
    pub fn encoder_name(&self) -> String {
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_video_recovers_from_congestion_without_exceeding_androids_rate_cap() {
        let mut header = header_for(MASK_VP9, &available(false, false)).0; header.fps = 30;
        let settings = call_settings(&header, 4);
        let (thread, _) = fake_thread(None, None, false);
        let mut video = CallVideo::with_engine(Engine::Thread(thread), settings, "generated".into(), &header);
        assert_eq!((video.settings.bitrate_kbps, video.fps), (110, 15));
        let mut sample = crate::call_rate::Sample { now_ns: 1_000_000_000, app_backlog_ms: 600, carried_kbps: 90,
            offered_kbps: 110, ..Default::default() };
        assert_eq!(video.rate.on_sample(&sample), crate::call_rate::Decision::Cut);
        sample.app_backlog_ms = 0; sample.delivered_kbps = Some(800); sample.peer_queue_ms = Some(0); sample.carried_kbps = 110;
        for tick in 2..=30 {
            sample.now_ns = tick * 1_000_000_000; video.rate.on_sample(&sample);
            assert!(video.rate.target_kbps() <= 110); assert!(video.rate.fps() <= 15);
        }
        assert_eq!(video.rate.target_kbps(), 110, "clean delivery recovers only to the existing relay cap");
    }

    fn available(av1_hw: bool, vp9_hw: bool) -> Availability {
        Availability {
            av1_hardware: if av1_hw { vec!["av1".into()] } else { vec![] },
            vp9_hardware: if vp9_hw { vec!["vp9".into()] } else { vec![] },
            av1_software: "libaom".into(),
            vp9_software: "libvpx".into(),
            ..Availability::default()
        }
    }

    #[test]
    fn with_a_master_on_the_header_announces_hevc_from_the_one_choice() {
        use crate::video::codecs::{HELLO_HEVC, Switches};
        let on = Switches { hevc: true, ..Switches::LEGACY };
        let a = Availability { hevc_hardware: vec!["hevc".into()], ..available(false, false) };
        let advertised = MASK_VP9 | MASK_AV1 | HELLO_HEVC;
        let (h, plan) = header_with(MASK_VP9 | MASK_AV1 | HELLO_HEVC, advertised, &a, on);
        assert_eq!((h.codec, plan), (5, vec![(5, Backend::Hardware)]));
        // An old peer (decode mask 0x03) falls back along the same list.
        let (h, plan) = header_with(MASK_VP9 | MASK_AV1, advertised, &a, on);
        assert_eq!((h.codec, plan), (3, vec![(3, Backend::SoftwareAv1)]));
        // Masters off: today's header, whatever the peer advertises.
        assert_eq!(header_with(MASK_VP9 | HELLO_HEVC, MASK_VP9 | MASK_AV1, &a, Switches::LEGACY).0.codec, 2);
        // The PC never announces a codec its own hello did not advertise
        // (phone run 2026-10-28: a 0x03 hello, then an HEVC header, and the
        // phone ended the call): HEVC is skipped, VP9 is next.
        let (h, plan) = header_with(MASK_VP9 | MASK_AV1 | HELLO_HEVC, MASK_VP9 | MASK_AV1, &a, on);
        assert_eq!((h.codec, plan), (3, vec![(3, Backend::SoftwareAv1)]));
        let (h, _) = header_with(MASK_VP9 | HELLO_HEVC, MASK_VP9 | MASK_AV1, &a, on);
        assert_eq!(h.codec, 2);
    }

    #[test]
    fn the_header_announces_the_first_encoder_in_android_order_for_the_peer() {
        let (h, plan) = header_for(MASK_VP9 | MASK_AV1, &available(true, true));
        assert_eq!((h.codec, h.width, h.height, h.fps), (3, 426, 240, 15));
        assert_eq!(plan, vec![(3, Backend::Hardware), (3, Backend::SoftwareAv1)]);
        let (h, plan) = header_for(MASK_VP9, &available(true, true));
        assert_eq!((h.codec, plan), (2, vec![(2, Backend::Hardware), (2, Backend::SoftwareVp9)]));
        let (h, plan) = header_for(MASK_AV1, &available(false, false));
        assert_eq!((h.codec, plan), (3, vec![(3, Backend::SoftwareAv1)]));
    }

    #[test]
    fn a_peer_that_prefers_vp9_gets_vp9_even_when_av1_hardware_exists() {
        let mask = MASK_VP9 | MASK_AV1 | crate::video::DECODE_PREFERS_VP9;
        let (h, plan) = header_for(mask, &available(true, true));
        assert_eq!((h.codec, plan), (2, vec![(2, Backend::Hardware), (2, Backend::SoftwareVp9)]));
        let (h, _) = header_for(mask, &available(true, false));
        assert_eq!(h.codec, 2, "software VP9 still beats hardware AV1");
        // Bit 7 without the VP9 bit changes nothing.
        let (h, _) = header_for(MASK_AV1 | crate::video::DECODE_PREFERS_VP9, &available(true, true));
        assert_eq!(h.codec, 3);
    }

    /// A fake encoder: logs every call; `encode` waits for `gate` when set.
    struct Fake {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
        fail_at: Option<i64>,
        refuse_rate: bool,
    }
    impl FrameEncoder for Fake {
        fn encode(&mut self, nv12: &[u8], pts_us: i64, _: Instant) -> Result<Vec<Vec<u8>>, SourceError> {
            if let Some(gate) = &self.gate {
                let _ = gate.recv();
            }
            self.log.lock().unwrap().push(format!("encode {pts_us}"));
            if self.fail_at == Some(pts_us) {
                return Err(SourceError::Encode(EncodeError::Timeout));
            }
            Ok(vec![nv12.to_vec(), vec![pts_us as u8]])
        }
        fn request_keyframe(&mut self, now_ns: u64, force: bool) {
            self.log.lock().unwrap().push(format!("key {now_ns} {force}"));
        }
        fn set_bitrate(&mut self, kbps: u32) -> bool {
            self.log.lock().unwrap().push(format!("rate {kbps}"));
            !self.refuse_rate
        }
    }
    fn fake_thread(gate: Option<std::sync::mpsc::Receiver<()>>, fail_at: Option<i64>, refuse_rate: bool) -> (EncodeThread, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = log.clone();
        let (thread, name) = EncodeThread::spawn(move |ready, shared, out| {
            let mut fake = Fake { log: inner, gate, fail_at, refuse_rate };
            let _ = ready.send(Ok("fake".into()));
            serve(&mut fake, &shared, &out);
        })
        .unwrap();
        assert_eq!(name, "fake");
        (thread, log)
    }
    fn wait_outputs(thread: &EncodeThread, n: usize) -> Vec<Output> {
        let mut all = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while all.len() < n && Instant::now() < until {
            all.extend(thread.drain());
            std::thread::sleep(Duration::from_millis(1));
        }
        all
    }

    #[test]
    fn the_encode_thread_returns_every_picture_in_order_and_controls_come_first() {
        let (mut thread, log) = fake_thread(None, None, false);
        thread.request_keyframe(5, true);
        thread.set_bitrate(150);
        thread.submit(vec![1], 0);
        let out = wait_outputs(&thread, 1);
        assert!(matches!(&out[0], Output::Packets { packets, .. } if *packets == vec![vec![1], vec![0]]), "{out:?}");
        thread.submit(vec![2], 66);
        let out = wait_outputs(&thread, 1);
        assert!(matches!(&out[0], Output::Packets { packets, .. } if *packets == vec![vec![2], vec![66]]));
        assert_eq!(*log.lock().unwrap(), ["key 5 true", "rate 150", "encode 0", "encode 66"]);
        assert_eq!(thread.replaced, 0);
    }

    #[test]
    fn a_busy_encoder_gets_only_the_newest_waiting_picture_and_the_caller_never_waits() {
        let (release, gate) = std::sync::mpsc::channel();
        let (mut thread, log) = fake_thread(Some(gate), None, false);
        thread.submit(vec![1], 0);
        // Wait until the encoder took picture 0 (it now blocks on the gate).
        let until = Instant::now() + Duration::from_secs(5);
        while thread.shared.inbox.lock().unwrap().picture.is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        let started = Instant::now();
        for pts in [66, 133, 200] {
            thread.submit(vec![2], pts);
        }
        thread.request_keyframe(9, false);
        assert!(started.elapsed() < Duration::from_millis(100), "submitting never waits on the codec");
        assert_eq!(thread.replaced, 2, "66 and 133 were replaced before encoding");
        release.send(()).unwrap();
        release.send(()).unwrap();
        let out = wait_outputs(&thread, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(*log.lock().unwrap(), ["encode 0", "key 9 false", "encode 200"]);
    }

    #[test]
    fn an_encode_failure_or_a_refused_rate_is_reported_and_drop_joins_the_thread() {
        let (mut thread, _) = fake_thread(None, Some(66), true);
        thread.set_bitrate(90);
        thread.submit(vec![1], 0);
        let out = wait_outputs(&thread, 2);
        assert_eq!(out[0], Output::BitrateRefused(90));
        thread.submit(vec![1], 66);
        let out = wait_outputs(&thread, 1);
        assert_eq!(out, vec![Output::Failed(SourceError::Encode(EncodeError::Timeout))]);
        drop(thread);
        // A body that fails to open reports the error and no thread lingers.
        let error = EncodeThread::spawn(|ready, _, _| {
            let _ = ready.send(Err(Error::Encoder(EncodeError::Unsupported)));
        });
        assert!(matches!(error, Err(Error::Encoder(EncodeError::Unsupported))));
        assert!(encode_thread_enabled() || std::env::var(ENCODE_THREAD_SWITCH).is_ok());
    }

    #[test]
    fn a_vp9_only_peer_without_hardware_vp9_gets_the_software_vp9_encoder() {
        let (h, plan) = header_for(MASK_VP9, &available(true, false));
        assert_eq!((h.codec, plan), (2, vec![(2, Backend::SoftwareVp9)]));
        // No common codec at all keeps the call voice-only.
        let (_, plan) = header_for(0, &available(true, true));
        assert!(plan.is_empty());
    }
}
