//! Android `MediaPlayoutClock` (default arm): maps source audio/video
//! timestamps onto one receiver timeline. The delay starts at the profile
//! minimum, grows by the measured lateness plus 50 ms when media arrives more
//! than 20 ms late (never past the maximum), and after a calm period (60 s for
//! streams, 10 s for calls) shrinks by 1 ms per 100 ms. Only arrival (`timing`)
//! may move the anchor; later stages use `target_for`, so a stalled decoder is
//! not mistaken for late media. The experimental one-step burst growth and
//! time-based target (off by default on Android) are not ported.

const NS_PER_US: i64 = 1_000;
const NS_PER_MS: i64 = 1_000_000;
pub const STREAM_CALM_BEFORE_SHRINK_MS: i64 = 60_000;
pub const CALL_CALM_BEFORE_SHRINK_MS: i64 = 10_000;
const MAX_SCHEDULE_AHEAD_MS: i64 = 2_000;
const RESET_AFTER_GAP_US: i64 = 5_000_000;
const EARLY_REANCHOR_SLACK_MS: i64 = 150;
const LATE_TOLERANCE_NS: i64 = 20_000_000;
const RESERVE_RANGE_FRACTION: f64 = 0.75;
const GROWTH_MARGIN_NS: i64 = 50_000_000;
const SATURATED_LATE_WINDOW_NS: i64 = 2_000_000_000;
const SHRINK_DIVISOR: i64 = 100;

/// Android `PlaybackDelayProfile` start and maximum delays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Low,
    Balanced,
    Stable,
    /// Default for streams.
    Auto,
    /// Default for calls.
    Call,
    CallLow,
    /// Audio-only live.
    Radio,
}
impl Profile {
    pub fn delays_ms(self) -> (i64, i64) {
        match self {
            Self::Low => (100, 600),
            Self::Balanced => (250, 1_200),
            Self::Stable => (500, 2_500),
            Self::Auto => (250, 2_000),
            Self::Call => (250, 600),
            Self::CallLow => (30, 200),
            Self::Radio => (600, 3_000),
        }
    }
    pub fn calm_ms(self) -> i64 {
        match self {
            Self::Call | Self::CallLow => CALL_CALM_BEFORE_SHRINK_MS,
            _ => STREAM_CALM_BEFORE_SHRINK_MS,
        }
    }
    pub fn clock(self) -> Clock {
        let (start, max) = self.delays_ms();
        Clock::new(start, max, self.calm_ms())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub target_ns: i64,
    pub lateness_ms: i64,
}

pub struct Clock {
    delay_ms: i64,
    max_delay_ms: i64,
    calm_ns: i64,
    anchor_us: Option<i64>,
    anchor_ns: i64,
    latest_us: Option<i64>,
    extra_ns: i64,
    last_late_ns: Option<i64>,
    last_shrink_ns: Option<i64>,
    reserved_ns: i64,
    reanchors: u32,
    current_delay_ms: i64,
}
impl Clock {
    pub fn new(delay_ms: i64, max_delay_ms: i64, calm_ms: i64) -> Self {
        Self {
            delay_ms,
            max_delay_ms: max_delay_ms.max(delay_ms),
            calm_ns: calm_ms * NS_PER_MS,
            anchor_us: None,
            anchor_ns: 0,
            latest_us: None,
            extra_ns: 0,
            last_late_ns: None,
            last_shrink_ns: None,
            reserved_ns: 0,
            reanchors: 0,
            current_delay_ms: delay_ms,
        }
    }
    fn adaptive(&self) -> bool {
        self.max_delay_ms > self.delay_ms
    }
    pub fn current_delay_ms(&self) -> i64 {
        self.current_delay_ms
    }
    pub fn reanchors(&self) -> u32 {
        self.reanchors
    }
    /// Pinned at the maximum while media is still arriving late.
    pub fn is_saturated(&self, now_ns: i64) -> bool {
        self.adaptive()
            && self.current_delay_ms >= self.max_delay_ms
            && self
                .last_late_ns
                .is_some_and(|late| now_ns - late < SATURATED_LATE_WINDOW_NS)
    }
    /// Keeps at least `extra_ms` above the start delay (capped at 3/4 of the range).
    pub fn reserve(&mut self, extra_ms: i64) {
        if !self.adaptive() {
            return;
        }
        let range = (self.max_delay_ms - self.delay_ms) * NS_PER_MS;
        self.reserved_ns =
            (extra_ms * NS_PER_MS).clamp(0, (range as f64 * RESERVE_RANGE_FRACTION) as i64);
        if self.anchor_us.is_none() && self.extra_ns < self.reserved_ns {
            self.extra_ns = self.reserved_ns;
            self.current_delay_ms = self.delay_ms + self.extra_ns / NS_PER_MS;
        }
    }
    /// Network-arrival timing: anchors the timeline and grows the delay for late media.
    pub fn timing(&mut self, pts_us: i64, now_ns: i64) -> Timing {
        self.timing_limited(pts_us, now_ns, None)
    }
    /// `timing` where this arrival may grow the added delay only up to
    /// `growth_limit_ms` (CF-MEDIA-66, Android `MediaPlayoutClock.timing`
    /// `growthLimitMs`): lateness beyond it is still returned (and reported)
    /// but is played late, and it does not hold off the calm shrink of delay
    /// other media added above the limit.
    pub fn timing_limited(&mut self, pts_us: i64, now_ns: i64, growth_limit_ms: Option<i64>) -> Timing {
        let effective = self.delay_ms * NS_PER_MS + self.extra_ns;
        let reset = match (self.anchor_us, self.latest_us) {
            (None, _) => true,
            (_, Some(latest)) => {
                pts_us > latest + RESET_AFTER_GAP_US || pts_us < latest - RESET_AFTER_GAP_US
            }
            _ => false,
        };
        if reset {
            self.anchor_us = Some(pts_us);
            self.anchor_ns = now_ns + effective;
            self.latest_us = Some(pts_us);
        } else if self.latest_us.is_none_or(|latest| pts_us > latest) {
            self.latest_us = Some(pts_us);
        }
        if self.adaptive() {
            self.shrink_when_calm(now_ns);
        }
        let mut raw = self.anchor_ns + (pts_us - self.anchor_us.unwrap()) * NS_PER_US;
        let delay_now = self.delay_ms * NS_PER_MS + self.extra_ns;
        let slack = delay_now.max(EARLY_REANCHOR_SLACK_MS * NS_PER_MS);
        if raw - now_ns > delay_now + slack {
            self.anchor_us = Some(pts_us);
            self.anchor_ns = now_ns + delay_now;
            raw = self.anchor_ns;
            self.reanchors += 1;
        }
        let lateness = now_ns - raw;
        if self.adaptive() && lateness > LATE_TOLERANCE_NS {
            let max_extra = (self.max_delay_ms - self.delay_ms) * NS_PER_MS;
            let limit = growth_limit_ms.map_or(max_extra, |ms| max_extra.min(ms.max(0) * NS_PER_MS));
            let within_limit = self.extra_ns <= limit;
            let grown = (self.extra_ns + lateness + GROWTH_MARGIN_NS).min(limit);
            let delta = grown - self.extra_ns;
            if delta > 0 {
                self.extra_ns = grown;
                self.anchor_ns += delta;
                raw += delta;
            }
            if within_limit {
                self.last_late_ns = Some(now_ns);
                self.last_shrink_ns = Some(now_ns);
            }
        }
        self.current_delay_ms = self.delay_ms + self.extra_ns / NS_PER_MS;
        Timing {
            target_ns: raw.min(now_ns + MAX_SCHEDULE_AHEAD_MS * NS_PER_MS),
            lateness_ms: (now_ns - raw).max(0) / NS_PER_MS,
        }
    }
    fn shrink_when_calm(&mut self, now_ns: i64) {
        let floor = self.reserved_ns;
        let Some(late) = self.last_late_ns else {
            return;
        };
        if self.extra_ns <= floor || now_ns - late < self.calm_ns {
            return;
        }
        let since = self
            .last_shrink_ns
            .unwrap_or(i64::MIN)
            .max(late + self.calm_ns);
        let elapsed = now_ns - since;
        if elapsed <= 0 {
            return;
        }
        let shrink = (elapsed / SHRINK_DIVISOR).min(self.extra_ns - floor);
        if shrink <= 0 {
            return;
        }
        self.extra_ns -= shrink;
        self.anchor_ns -= shrink;
        self.last_shrink_ns = Some(now_ns);
    }
    /// Read-only lookup for post-arrival stages (decoder feed, release, audio write).
    pub fn target_for(&self, pts_us: i64, now_ns: i64) -> Timing {
        let raw = match self.anchor_us {
            None => now_ns + self.delay_ms * NS_PER_MS + self.extra_ns,
            Some(anchor) => self.anchor_ns + (pts_us - anchor) * NS_PER_US,
        };
        Timing {
            target_ns: raw.min(now_ns + MAX_SCHEDULE_AHEAD_MS * NS_PER_MS),
            lateness_ms: (now_ns - raw).max(0) / NS_PER_MS,
        }
    }
    pub fn render_time_ns(&self, pts_us: i64, now_ns: i64) -> i64 {
        self.target_for(pts_us, now_ns).target_ns.max(now_ns)
    }
    pub fn is_too_late(&self, pts_us: i64, maximum_late_ms: i64, now_ns: i64) -> bool {
        self.target_for(pts_us, now_ns).lateness_ms > maximum_late_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Ported from Android MediaPlayoutClockTest (same numbers).
    fn fixed(delay: i64) -> Clock {
        Clock::new(delay, delay, STREAM_CALM_BEFORE_SHRINK_MS)
    }
    #[test]
    fn late_video_grows_the_call_delay_by_at_most_150_ms_and_late_audio_keeps_the_range() {
        let (delay, max) = Profile::Call.delays_ms();
        let mut c = Profile::Call.clock();
        c.timing(0, 0);
        // Video 400 ms late: limited to +150 ms; its lateness is still reported.
        let late = c.timing_limited(100_000, (100 + delay + 400) * NS_PER_MS, Some(150));
        assert_eq!(c.current_delay_ms(), delay + 150);
        assert!(late.lateness_ms >= 240, "{late:?}");
        // Late audio (no limit) may grow it to the adaptive maximum.
        c.timing(200_000, (200 + delay + 2_000) * NS_PER_MS);
        assert_eq!(c.current_delay_ms(), max);
    }
        #[test]
    fn audio_and_video_share_one_long_running_timeline() {
        let mut c = fixed(60);
        assert_eq!(c.timing(1_000_000, 1_000_000_000).target_ns, 1_060_000_000);
        assert_eq!(c.timing(1_020_000, 1_020_000_000).target_ns, 1_080_000_000);
        assert_eq!(c.timing(1_033_333, 1_033_333_000).target_ns, 1_093_333_000);
        let mut c = fixed(60);
        for s in 1..=6 {
            c.timing(s * 1_000_000, s * 1_000_000_000);
        }
        assert_eq!(c.timing(7_000_000, 7_000_000_000).target_ns, 7_060_000_000);
        let mut c = fixed(60);
        c.timing(1_000_000, 1_000_000_000);
        assert_eq!(c.timing(980_000, 1_010_000_000).target_ns, 1_040_000_000);
        let mut c = fixed(60);
        c.timing(1_000_000, 1_000_000_000);
        assert!(!c.is_too_late(1_020_000, 90, 1_100_000_000));
        assert!(c.is_too_late(1_020_000, 90, 1_171_000_000));
        let mut c = fixed(60);
        c.timing(1_000_000, 1_000_000_000);
        assert_eq!(c.timing(7_000_001, 2_000_000_000).target_ns, 2_060_000_000);
    }
    #[test]
    fn a_late_first_keyframe_or_stale_anchor_is_replaced_not_kept_as_latency() {
        let mut c = fixed(250);
        assert_eq!(
            c.timing(1_000_000, 10_000_000_000).target_ns,
            10_250_000_000
        );
        assert_eq!(
            c.timing(1_800_000, 10_030_000_000).target_ns,
            10_280_000_000
        );
        assert_eq!(c.reanchors(), 1);
        assert_eq!(
            c.timing(1_866_000, 10_100_000_000).target_ns,
            10_346_000_000
        );
        assert_eq!(c.reanchors(), 1);
        let mut c = fixed(250);
        c.timing(1_000_000, 1_000_000_000);
        assert_eq!(c.timing(1_400_000, 1_200_000_000).target_ns, 1_650_000_000);
        assert_eq!(c.reanchors(), 0);
        assert_eq!(c.timing(2_000_000, 1_300_000_000).target_ns, 1_550_000_000);
        assert_eq!(c.reanchors(), 1);
        assert!(!c.is_too_late(2_020_000, 90, 1_320_000_000));
        // Untimed post-arrival lookups never move the anchor.
        let mut c = fixed(250);
        c.timing(20_000_000, 2_000_000_000);
        assert_eq!(c.timing(20_020_000, 2_101_000_000).target_ns, 2_270_000_000);
        assert_eq!(c.render_time_ns(20_040_000, 2_210_000_000), 2_290_000_000);
    }
    #[test]
    fn the_adaptive_delay_grows_on_late_media_and_shrinks_after_the_calm_period() {
        let mut c = Clock::new(250, 500, STREAM_CALM_BEFORE_SHRINK_MS);
        assert_eq!(c.timing(0, 1_000_000_000).target_ns, 1_250_000_000);
        assert_eq!(c.timing(1_000_000, 2_350_000_000).target_ns, 2_400_000_000);
        assert_eq!(c.current_delay_ms(), 400);
        assert!(!c.is_saturated(2_350_000_000));
        c.timing(2_000_000, 3_800_000_000);
        assert_eq!(c.current_delay_ms(), 500);
        assert!(c.is_saturated(3_800_000_000));
        assert!(!fixed(250).is_saturated(0));
        let mut c = Clock::new(250, 500, STREAM_CALM_BEFORE_SHRINK_MS);
        c.timing(0, 1_000_000_000);
        c.timing(1_000_000, 2_450_000_000);
        assert_eq!(c.current_delay_ms(), 500);
        c.timing(4_000_000, 5_000_000_000);
        c.timing(21_000_000, 22_450_000_000);
        assert_eq!(c.current_delay_ms(), 500);
        c.timing(61_000_000, 64_450_000_000);
        assert_eq!(c.current_delay_ms(), 480);
        // Calls shrink after ten calm seconds.
        let mut c = Profile::Call.clock();
        c.timing(0, 1_000_000_000);
        c.timing(1_000_000, 2_600_000_000);
        assert_eq!(c.current_delay_ms(), 600);
        c.timing(14_000_000, 15_000_000_000);
        assert!(c.current_delay_ms() < 600);
    }
    #[test]
    fn a_reserve_starts_later_and_bounds_the_calm_shrink() {
        let mut c = Profile::Auto.clock();
        c.reserve(10_000);
        assert_eq!(c.current_delay_ms(), 250 + 1_312);
        assert_eq!(c.timing(0, 0).target_ns, 1_562_500_000);
        let mut fixed_clock = fixed(250);
        fixed_clock.reserve(500);
        assert_eq!(fixed_clock.current_delay_ms(), 250);
        assert_eq!(Profile::Radio.delays_ms(), (600, 3_000));
    }
}
