//! Call video rate control (Android `LinkRateController`, CONFORMANCE
//! CF-MEDIA-25 and CF-MEDIA-33..36), in two pure parts:
//! - `DeliveryEstimate`: from this side's written media bytes and the
//!   peer's v3 delivery counters, the delivered rate and the queue hidden
//!   below the app (one-way delay of the last reported byte minus its
//!   2-minute rolling minimum). Counters going backwards, or more received
//!   than written by over 1 MiB, make both unusable until fresh data.
//! - `RateController`: one 1 s sample at a time. Overuse cuts to
//!   `clamp(0.85 x link - audio, lowest, request)` at most once per 3 s; three
//!   loaded clean windows climb by `request / 10`; below the geometry's
//!   bitrate floor the call keeps bits per frame and lowers the frame rate.
use std::collections::VecDeque;

const MS: u64 = 1_000_000;
const MIB: u64 = 1 << 20;

/// CF-MEDIA-23 on Windows: the link rate measured only from back-to-back
/// arrivals. The stack pushes each GATT notification (or write request)
/// with its arrival time (`carrier::Pipe::offer`). A train starts at a
/// notification after an idle gap; that first notification's bytes are
/// excluded (they may have waited in the stack). The train continues while
/// notifications follow within `max_gap` (about 2.5 connection intervals at
/// 15 ms: back to back means the sender had more queued). A train counts when
/// at least 8000 bytes arrived after its start and it lasted at least 60 ms;
/// rate = bytes x 8 / duration. The report carries the median (lower middle)
/// of the samples in the last 10 s. `MESHLIVE_TRAIN_GAP_MS` (Experimental)
/// changes the gap for measurement runs.
#[derive(Debug)]
pub struct ArrivalMeter {
    trains: u64,
    max_gap_ns: u64,
    /// The current train: (start ns, last arrival ns, bytes after the start).
    train: Option<(u64, u64, u64)>,
    /// (end ns, kbps) of counted trains, last 10 s.
    samples: VecDeque<(u64, u32)>,
}
impl Default for ArrivalMeter {
    fn default() -> Self {
        let gap_ms = std::env::var("MESHLIVE_TRAIN_GAP_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(40).clamp(5, 500);
        Self::with_gap(gap_ms * MS)
    }
}
impl ArrivalMeter {
    pub const MIN_BYTES: u64 = 8_000;
    pub const MIN_TRAIN_NS: u64 = 60 * MS;
    pub const WINDOW_NS: u64 = 10_000 * MS;
    pub fn with_gap(max_gap_ns: u64) -> Self {
        Self { trains: 0, max_gap_ns, train: None, samples: VecDeque::new() }
    }
    fn close(&mut self) {
        if let Some((start, last, bytes)) = self.train.take() {
            let ns = last.saturating_sub(start);
            if bytes >= Self::MIN_BYTES && ns >= Self::MIN_TRAIN_NS {
                self.trains += 1;
                self.samples.push_back((last, (bytes * 8 * 1_000_000 / ns) as u32));
            }
        }
    }
    pub fn on_arrival(&mut self, at_ns: u64, bytes: usize) {
        match self.train {
            Some((_, last, _)) if at_ns.saturating_sub(last) <= self.max_gap_ns => {
                let t = self.train.as_mut().unwrap();
                t.1 = at_ns;
                t.2 += bytes as u64;
            }
            _ => {
                self.close();
                // The first notification of a train is excluded (it may have been buffered).
                self.train = Some((at_ns, at_ns, 0));
            }
        }
        while self.samples.front().is_some_and(|(end, _)| at_ns.saturating_sub(*end) > Self::WINDOW_NS) {
            self.samples.pop_front();
        }
    }
    /// The median (lower middle) of the last 10 s; 0 when nothing was measured.
    pub fn kbps(&mut self, now_ns: u64) -> u16 {
        // A train that went quiet closes (and counts if it qualifies).
        if self.train.is_some_and(|(_, last, _)| now_ns.saturating_sub(last) > self.max_gap_ns) {
            self.close();
        }
        while self.samples.front().is_some_and(|(end, _)| now_ns.saturating_sub(*end) > Self::WINDOW_NS) {
            self.samples.pop_front();
        }
        let mut rates: Vec<u32> = self.samples.iter().map(|(_, k)| *k).collect();
        if rates.is_empty() {
            return 0;
        }
        rates.sort_unstable();
        rates[(rates.len() - 1) / 2].min(65_535) as u16
    }
    pub fn samples(&self) -> usize {
        self.samples.len()
    }
    pub fn trains(&self) -> u64 { self.trains }
}

#[derive(Debug, Default)]
pub struct DeliveryEstimate {
    written: u64,
    /// (cumulative written bytes, time written ns), oldest first.
    log: VecDeque<(u64, u64)>,
    last_raw: Option<(u32, u32)>,
    /// Unwrapped peer counters and the history used for the rate.
    received: u64,
    peer_clock_ms: u64,
    history: VecDeque<(u64, u64)>,
    /// 10 s buckets of the minimum one-way delay (bucket start ns, min ms).
    minimum: VecDeque<(u64, i64)>,
    delivered_kbps: Option<u32>,
    queue_ms: Option<i32>,
}

impl DeliveryEstimate {
    pub fn on_written(&mut self, bytes: usize, now_ns: u64) {
        self.written += bytes as u64;
        self.log.push_back((self.written, now_ns));
        while self.log.len() > 8192 {
            self.log.pop_front();
        }
    }
    fn reset(&mut self) {
        self.history.clear();
        self.minimum.clear();
        self.delivered_kbps = None;
        self.queue_ms = None;
    }
    pub fn on_report(&mut self, received_bytes: u32, clock_ms: u32, now_ns: u64) {
        match self.last_raw {
            Some((bytes, clock)) if received_bytes < bytes || clock_ms < clock => {
                // A peer reset: never read as a wrap.
                self.reset();
                self.received = 0;
                self.peer_clock_ms = 0;
                self.last_raw = Some((received_bytes, clock_ms));
                return;
            }
            Some((bytes, clock)) => {
                self.received += u64::from(received_bytes - bytes);
                self.peer_clock_ms += u64::from(clock_ms - clock);
            }
            None => {
                self.received = u64::from(received_bytes);
                self.peer_clock_ms = u64::from(clock_ms);
            }
        }
        self.last_raw = Some((received_bytes, clock_ms));
        if self.received > self.written + MIB {
            self.reset();
            return;
        }
        // Delivered rate: the window of 800..2200 ms, or the last interval of >= 500 ms.
        self.history.push_back((self.received, self.peer_clock_ms));
        while self.history.len() > 64 {
            self.history.pop_front();
        }
        let rate = |a: &(u64, u64), b: &(u64, u64)| -> Option<u32> {
            let ms = b.1.checked_sub(a.1)?;
            (ms > 0).then(|| ((b.0.saturating_sub(a.0)) * 8 / ms) as u32)
        };
        let last = *self.history.back().unwrap();
        let window = self
            .history
            .iter()
            .rev()
            .find(|h| (800..=2_200).contains(&(last.1 - h.1)))
            .and_then(|h| rate(h, &last));
        let interval = self
            .history
            .iter()
            .rev()
            .nth(1)
            .filter(|h| last.1 - h.1 >= 500)
            .and_then(|h| rate(h, &last));
        if let Some(kbps) = window.into_iter().chain(interval).max() {
            self.delivered_kbps = Some(kbps);
        }
        // Queue: the one-way delay of the last reported byte minus its rolling minimum.
        let Some(&(_, sent_ns)) = self.log.iter().find(|(cumulative, _)| *cumulative >= self.received) else {
            return;
        };
        let delay = self.peer_clock_ms as i64 - (sent_ns / MS) as i64;
        let bucket = now_ns / (10_000 * MS);
        match self.minimum.back_mut() {
            Some((b, m)) if *b == bucket => *m = (*m).min(delay),
            _ => self.minimum.push_back((bucket, delay)),
        }
        while self.minimum.front().is_some_and(|(b, _)| bucket.saturating_sub(*b) >= 12) {
            self.minimum.pop_front();
        }
        let floor = self.minimum.iter().map(|(_, m)| *m).min().unwrap_or(delay);
        self.queue_ms = Some((delay - floor).clamp(0, 60_000) as i32);
    }
    /// Delivered kbps (held between reports), when measured.
    pub fn delivered_kbps(&self) -> Option<u32> {
        self.delivered_kbps
    }
    /// The queue below the app in ms, when measured.
    pub fn queue_ms(&self) -> Option<i32> {
        self.queue_ms
    }
    pub fn written(&self) -> u64 {
        self.written
    }
}

/// One 1 s sample of the call's send side.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub now_ns: u64,
    /// Queued in the app (send queue), ms.
    pub app_backlog_ms: i32,
    /// The peer-measured queue (v3), when known.
    pub peer_queue_ms: Option<i32>,
    pub delivered_kbps: Option<u32>,
    /// Video actually written in this sample.
    pub carried_kbps: u32,
    /// Video the encoder produced in this sample.
    pub offered_kbps: u32,
    /// The peer reported its playout clock pinned late.
    pub peer_late: bool,
    /// Frames were shed (refused or dropped) in this sample.
    pub shed: bool,
    /// A keyframe went out in this sample (large-frame grace).
    pub keyframe: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateController {
    pub request_kbps: u32,
    pub request_fps: u32,
    pub audio_kbps: u32,
    floor_kbps: u32,
    min_fps: u32,
    target_kbps: u32,
    last_cut_ns: Option<u64>,
    clean: u32,
    last_queue_ms: Option<i32>,
}

/// What a sample decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    Cut,
    Climb,
}

impl RateController {
    /// CF-MEDIA-36: an unknown peer starts at `min(request, 400 - audio)`.
    pub fn new(request_kbps: u32, request_fps: u32, width: u32, height: u32, audio_kbps: u32) -> Self {
        let geometry = (u64::from(width) * u64::from(height) * u64::from(request_fps) / 10 / 1000) as u32;
        let floor_kbps = geometry.min(request_kbps * 2 / 5).max(1);
        let min_fps = (request_fps / 3).max(5).min(request_fps.max(1));
        Self {
            request_kbps,
            request_fps,
            audio_kbps,
            floor_kbps,
            min_fps,
            target_kbps: request_kbps.min(400u32.saturating_sub(audio_kbps)).max(1),
            last_cut_ns: None,
            clean: 0,
            last_queue_ms: None,
        }
    }
    /// The lowest rate: the floor at the minimum frame rate.
    pub fn lowest_kbps(&self) -> u32 {
        (self.floor_kbps * self.min_fps / self.request_fps.max(1)).max(1)
    }
    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }
    /// CF-MEDIA-35: full frame rate at or above the floor, else the same
    /// bits per frame at a proportionally lower frame rate.
    pub fn fps(&self) -> u32 {
        if self.target_kbps >= self.floor_kbps {
            self.request_fps
        } else {
            (self.request_fps * self.target_kbps / self.floor_kbps).max(self.min_fps)
        }
    }
    /// The sender did not fill the target in this window (generated or easy
    /// content, camera off): its delivery is not a capacity sample (as GCC
    /// and BBR treat app-limited windows).
    pub fn app_limited(&self, s: &Sample) -> bool {
        s.offered_kbps * 10 < self.target_kbps * 9 && s.app_backlog_ms < 100
    }
    pub fn on_sample(&mut self, s: &Sample) -> Decision {
        let queue = s.app_backlog_ms + s.peer_queue_ms.unwrap_or(0);
        let measured = s.peer_queue_ms.is_some() || s.delivered_kbps.is_some();
        let app_limited = self.app_limited(s);
        // Queue growth is a congestion signal only while the sender fills the link.
        let grew = !app_limited && self.last_queue_ms.is_some_and(|q| queue - q >= 50);
        let draining = self.last_queue_ms.is_some_and(|q| q - queue >= 50);
        self.last_queue_ms = Some(queue);
        let late = s.peer_late && (!measured || (queue >= 150 && !draining));
        let overuse = queue >= 400 || (grew && !s.keyframe && (measured || s.carried_kbps < s.offered_kbps)) || late || s.shed;
        if overuse {
            self.clean = 0;
            if self.last_cut_ns.is_none_or(|t| s.now_ns.saturating_sub(t) >= 3_000 * MS) {
                // App-limited: delivery says nothing about capacity, so a real
                // congestion signal cuts from the target itself.
                let link = if app_limited { self.target_kbps } else { s.delivered_kbps.unwrap_or(s.carried_kbps) };
                let wanted = (link * 85 / 100).saturating_sub(self.audio_kbps).clamp(self.lowest_kbps(), self.request_kbps);
                if wanted < self.target_kbps {
                    self.target_kbps = wanted;
                    self.last_cut_ns = Some(s.now_ns);
                    return Decision::Cut;
                }
            }
            return Decision::Hold;
        }
        let clean = !s.peer_late && queue < 150 && !s.shed;
        let loaded = (s.offered_kbps + self.audio_kbps) * 10 >= (self.target_kbps + self.audio_kbps) * 6;
        if clean && loaded {
            self.clean += 1;
            if self.clean >= 3 {
                self.clean = 0;
                let raised = (self.target_kbps + self.request_kbps / 10).min(self.request_kbps);
                if raised > self.target_kbps {
                    self.target_kbps = raised;
                    return Decision::Climb;
                }
            }
        } else if !clean {
            // A draining backlog: neither clean nor overuse.
            self.clean = 0;
        }
        Decision::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now_s: u64) -> Sample {
        Sample { now_ns: now_s * 1_000 * MS, carried_kbps: 200, offered_kbps: 200, ..Default::default() }
    }

    #[test]
    fn floors_trade_frame_rate_for_a_call() {
        // Android call default 426x240 at 15 FPS, 200 kbps request.
        let c = RateController::new(200, 15, 426, 240, 32);
        assert_eq!(c.target_kbps(), 200, "start: min(request, 400 - audio)");
        assert_eq!(c.floor_kbps, 80, "min(426*240*15*0.1 = 153 kbps, 0.4 x 200)");
        assert_eq!(c.min_fps, 5);
        assert_eq!(c.lowest_kbps(), 26);
        let mut low = c.clone();
        low.target_kbps = 40;
        assert_eq!(low.fps(), 7, "half the floor: half the frame rate");
        low.target_kbps = 10;
        assert_eq!(low.fps(), 5);
    }

    #[test]
    fn one_cut_per_overuse_then_a_climb_after_three_loaded_clean_windows() {
        let mut c = RateController::new(400, 15, 426, 240, 32);
        assert_eq!(c.target_kbps(), 368);
        // A 450 ms queue: one cut to 0.85 x delivered - audio.
        let over = Sample { app_backlog_ms: 450, delivered_kbps: Some(300), ..sample(10) };
        assert_eq!(c.on_sample(&over), Decision::Cut);
        assert_eq!(c.target_kbps(), 300 * 85 / 100 - 32);
        // Still overused one second later: no cascade inside 3 s.
        assert_eq!(c.on_sample(&Sample { now_ns: 11_000 * MS, ..over }), Decision::Hold);
        let cut = c.target_kbps();
        // Three loaded clean windows: climb by request / 10.
        let clean = Sample { offered_kbps: cut, carried_kbps: cut, ..sample(20) };
        assert_eq!(c.on_sample(&clean), Decision::Hold);
        assert_eq!(c.on_sample(&clean), Decision::Hold);
        assert_eq!(c.on_sample(&clean), Decision::Climb);
        assert_eq!(c.target_kbps(), cut + 40);
        // Unloaded clean windows (camera off) never climb.
        let idle = Sample { offered_kbps: 10, carried_kbps: 10, ..sample(30) };
        for _ in 0..6 {
            assert_eq!(c.on_sample(&idle), Decision::Hold);
        }
        assert_eq!(c.target_kbps(), cut + 40);
        // Shedding frames is overuse.
        assert_eq!(c.on_sample(&Sample { shed: true, ..sample(40) }), Decision::Cut);
    }

    #[test]
    fn arrival_trains_count_only_back_to_back_bytes_after_the_first_notification() {
        let mut m = ArrivalMeter::with_gap(40 * MS);
        // A saturated 15 ms link: 3 x 244 bytes per connection event for 300 ms.
        let mut t = 1_000 * MS;
        for _ in 0..20 {
            for _ in 0..3 {
                m.on_arrival(t, 244);
            }
            t += 15 * MS;
        }
        // Idle 200 ms: the train closes. 19 events x 732 bytes after the first
        // notification (2 x 244 in its event) over 285 ms.
        assert_eq!(m.kbps(t + 200 * MS), ((19 * 732 + 2 * 244) * 8 / 285) as u16, "404 kbps");
        assert_eq!(m.samples(), 1);
        // A short burst (under 60 ms or 8000 bytes) never counts.
        let mut n = ArrivalMeter::with_gap(40 * MS);
        for i in 0..3 {
            n.on_arrival(i * 15 * MS, 3_000);
        }
        assert_eq!(n.kbps(500 * MS), 0);
        // Samples older than 10 s leave the median.
        assert_eq!(m.kbps(t + 11_000 * MS), 0);
    }

    #[test]
    fn an_app_limited_sender_never_reads_low_delivery_as_a_link_limit() {
        // Xiaomi video-call run: the generated pattern offered 6..78 kbps
        // of a 200 kbps target; delivered followed it down; one queued frame
        // read as 66 ms "growth". None of that is congestion.
        let mut c = RateController::new(200, 15, 426, 240, 32);
        let mut s = Sample { now_ns: 10_000 * MS, offered_kbps: 78, carried_kbps: 70, delivered_kbps: Some(117), peer_queue_ms: Some(0), ..Default::default() };
        assert_eq!(c.on_sample(&s), Decision::Hold);
        s.now_ns += 1_000 * MS;
        s.app_backlog_ms = 66;
        s.delivered_kbps = Some(52);
        assert!(c.app_limited(&s));
        assert_eq!(c.on_sample(&s), Decision::Hold, "growth while app-limited is not overuse");
        for i in 0..10 {
            s.now_ns += 1_000 * MS;
            s.app_backlog_ms = if i % 2 == 0 { 0 } else { 80 };
            s.offered_kbps = 13;
            s.carried_kbps = 0;
            assert_eq!(c.on_sample(&s), Decision::Hold);
        }
        assert_eq!(c.target_kbps(), 200, "the target holds");
        // A real congestion signal still cuts, from the target, not from delivery.
        let shed = Sample { shed: true, ..s };
        assert_eq!(c.on_sample(&Sample { now_ns: s.now_ns + 1_000 * MS, ..shed }), Decision::Cut);
        assert_eq!(c.target_kbps(), 200 * 85 / 100 - 32);
        // A sender that fills the target still cuts to 0.85 x delivered.
        let mut d = RateController::new(200, 15, 426, 240, 32);
        let full = Sample { now_ns: 50_000 * MS, offered_kbps: 200, carried_kbps: 150, delivered_kbps: Some(150), app_backlog_ms: 450, ..Default::default() };
        assert_eq!(d.on_sample(&full), Decision::Cut);
        assert_eq!(d.target_kbps(), 150 * 85 / 100 - 32);
    }

    #[test]
    fn the_delivery_estimate_reads_rate_and_hidden_queue_and_resets_on_backwards_counters() {
        let mut d = DeliveryEstimate::default();
        // 25 kB written every 1 s (200 kbps), each byte arriving 30 ms after its write...
        for s in 0..4u64 {
            d.on_written(25_000, s * 1_000 * MS);
            d.on_report((25_000 * (s + 1)) as u32, (s * 1_000 + 30) as u32, s * 1_000 * MS + 30 * MS);
        }
        assert_eq!(d.delivered_kbps(), Some(200));
        assert_eq!(d.queue_ms(), Some(0), "a constant delay is no queue");
        // ...then a byte arriving 250 ms after its write: 220 ms of hidden queue.
        d.on_written(25_000, 4_000 * MS);
        d.on_report(125_000, 4_250, 4_250 * MS);
        assert_eq!(d.queue_ms(), Some(220));
        // Counters going backwards: a peer reset, both unusable.
        d.on_report(10, 5, 5_000 * MS);
        assert_eq!((d.delivered_kbps(), d.queue_ms()), (None, None));
        // More received than written by over 1 MiB: unusable too.
        let mut e = DeliveryEstimate::default();
        e.on_written(10, 0);
        e.on_report(2_000_000, 100, MS);
        assert_eq!(e.queue_ms(), None);
    }
}
