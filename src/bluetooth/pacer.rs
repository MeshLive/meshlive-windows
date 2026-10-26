//! Android `LinkPacer`: keeps the backlog in the app's ordered queue, where it is
//! measured and audio goes first, instead of hiding it inside the OS Bluetooth
//! stack. Writes are paced at the child-measured delivery rate times 1.2, with
//! only 50 ms of data allowed ahead of the link. Until the first measurement a
//! seed paces the link; without reports it halves every 2 s after a 4 s grace,
//! down to 150 kbps. On-time reports hold the seed and, while this side backs
//! up by at least 500 ms, raise it by 5/4 per report up to 1500 kbps (on by
//! default on Android since 2026-10-16). The Android experiments that are off
//! by default (join probe, drain guard, relay lead cap) are not ported.
//! The clock is injected; nothing here reads OS completion times as air time.

pub const BURST_NS: u64 = 50_000_000;
pub const PACE_GAIN: f64 = 1.2;
pub const SILENCE_GRACE_NS: u64 = 4_000_000_000;
pub const SILENCE_STEP_NS: u64 = 2_000_000_000;
pub const MIN_SEED_KBPS: u32 = 150;
pub const SEED_RAISE_BACKLOG_MS: u64 = 500;
pub const SEED_RAISE_CAP_KBPS: u32 = 1500;
/// Conservative GATT prior when this PC has no measurement of the transport
/// (Android `LinkPacerSeed.priorKbps`: 250 on a known 1M PHY, else 350).
pub const GATT_PRIOR_KBPS: u32 = 350;
pub const GATT_1M_PRIOR_KBPS: u32 = 250;

/// `(kbps * 1000 / 8 * 1.2).toLong()` exactly as Kotlin evaluates it.
fn paced_bytes_per_second(kbps: u32) -> u64 {
    ((u64::from(kbps) * 1000 / 8) as f64 * PACE_GAIN) as u64
}

/// Seed pace after `silence_ns` without a report: halved per step beyond the
/// grace, never below the floor (a seed already below it stays as it is).
pub fn decayed_seed(seed: u64, silence_ns: u64) -> u64 {
    if silence_ns <= SILENCE_GRACE_NS {
        return seed;
    }
    let steps = ((silence_ns - SILENCE_GRACE_NS) / SILENCE_STEP_NS + 1).min(16);
    let floor = paced_bytes_per_second(MIN_SEED_KBPS);
    seed.min(floor).max(seed >> steps)
}

/// The seed in link kbps: the measured transport rate, else the prior.
pub fn seed_kbps(measured_kbps: Option<u32>, le_2m_phy: Option<bool>) -> u32 {
    measured_kbps
        .filter(|k| *k > 0)
        .unwrap_or(if le_2m_phy == Some(false) {
            GATT_1M_PRIOR_KBPS
        } else {
            GATT_PRIOR_KBPS
        })
}

#[derive(Debug, Default)]
pub struct LinkPacer {
    burst_ns: u64,
    measured: u64,
    seed: u64,
    seeded_at_ns: u64,
    virtual_drain_ns: u64,
}
impl LinkPacer {
    pub fn new() -> Self {
        Self::with_burst(BURST_NS)
    }
    pub fn with_burst(burst_ns: u64) -> Self {
        Self {
            burst_ns,
            ..Self::default()
        }
    }
    fn rate(&self, now_ns: u64) -> u64 {
        if self.measured > 0 {
            self.measured
        } else if self.seed == 0 {
            0
        } else {
            decayed_seed(self.seed, now_ns.saturating_sub(self.seeded_at_ns))
        }
    }
    /// 0 = unpaced (nothing measured or seeded).
    pub fn bytes_per_second(&self, now_ns: u64) -> u64 {
        self.rate(now_ns)
    }
    pub fn measured(&self) -> bool {
        self.measured > 0
    }
    /// The undecayed seed in link kbps, for logs.
    pub fn seed_kbps(&self) -> u32 {
        ((self.seed * 8 / 1000) as f64 / PACE_GAIN) as u32
    }
    fn rescale(&mut self, now_ns: u64, previous: u64, next: u64) {
        if previous > 0 && next > 0 && self.virtual_drain_ns > now_ns {
            let ahead = u128::from(self.virtual_drain_ns - now_ns) * u128::from(previous)
                / u128::from(next);
            self.virtual_drain_ns = now_ns + ahead as u64;
        }
    }
    /// A child's arrival report. The first one replaces the seed: bytes already
    /// charged drain at the measured rate.
    pub fn set_measured_kbps(&mut self, now_ns: u64, kbps: u32) {
        let seeded = if self.measured == 0 {
            self.rate(now_ns)
        } else {
            0
        };
        let rate = paced_bytes_per_second(kbps);
        self.rescale(now_ns, seeded, rate);
        self.measured = rate;
    }
    /// Paces the link until the first measurement; ignored once measured or seeded.
    pub fn seed(&mut self, now_ns: u64, kbps: u32) -> bool {
        if kbps == 0 || self.measured > 0 || self.seed > 0 {
            return false;
        }
        self.seed = paced_bytes_per_second(kbps);
        self.seeded_at_ns = now_ns;
        true
    }
    /// An on-time report from an unmeasured child restarts the silence clock.
    /// `backlog_ms` is this side's oldest queued media age; `None` disables the raise.
    pub fn hold_seed(&mut self, now_ns: u64, backlog_ms: Option<u64>) -> bool {
        if self.measured > 0 || self.seed == 0 {
            return false;
        }
        if backlog_ms.is_some_and(|ms| ms >= SEED_RAISE_BACKLOG_MS) {
            let previous = self.rate(now_ns);
            let raised = paced_bytes_per_second(SEED_RAISE_CAP_KBPS).min(self.seed * 5 / 4);
            if raised > self.seed {
                self.seed = raised;
                self.rescale(now_ns, previous, raised);
            }
        }
        self.seeded_at_ns = now_ns;
        true
    }
    /// Nanoseconds to wait before the next write may start.
    pub fn wait_ns(&self, now_ns: u64) -> u64 {
        if self.rate(now_ns) == 0 {
            return 0;
        }
        self.virtual_drain_ns
            .saturating_sub(self.burst_ns)
            .saturating_sub(now_ns)
    }
    /// Charges `bytes` from the write's start: a blocking GATT write already
    /// spent its own transmission time.
    pub fn on_sent(&mut self, bytes: usize, write_started_ns: u64, now_ns: u64) {
        let rate = self.rate(now_ns);
        if rate == 0 {
            return;
        }
        let cost =
            (bytes as u128 * 1_000_000_000 / u128::from(rate)).min(u128::from(u64::MAX / 2)) as u64;
        self.virtual_drain_ns = self
            .virtual_drain_ns
            .max(write_started_ns)
            .saturating_add(cost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Ported from Android LinkSelfMeasurementTest (same numbers).
    #[test]
    fn pacing_allows_only_a_short_burst_ahead_of_the_measured_link() {
        let mut now = 1_000_000_000_u64;
        let mut pacer = LinkPacer::new();
        assert_eq!(pacer.wait_ns(now), 0);
        pacer.on_sent(100_000, now, now);
        assert_eq!(pacer.wait_ns(now), 0);
        pacer.set_measured_kbps(now, 800);
        pacer.on_sent(12_000, now, now);
        assert_eq!(pacer.wait_ns(now), 50_000_000);
        now += 60_000_000;
        assert_eq!(pacer.wait_ns(now), 0);
    }
    #[test]
    fn a_seeded_link_is_paced_from_the_first_write_and_the_first_report_replaces_the_seed() {
        let now = 1_000_000_000_u64;
        let mut pacer = LinkPacer::new();
        assert!(!pacer.measured());
        assert!(pacer.seed(now, 400));
        assert_eq!(pacer.bytes_per_second(now), 60_000);
        assert!(!pacer.seed(now, 900));
        pacer.on_sent(6_000, now, now);
        assert_eq!(pacer.wait_ns(now), 50_000_000);
        pacer.set_measured_kbps(now, 1_200);
        assert!(pacer.measured());
        assert_eq!(pacer.bytes_per_second(now), 180_000);
        assert_eq!(pacer.wait_ns(now), 0);
        assert!(!pacer.seed(now, 100));
    }
    #[test]
    fn a_seed_held_without_reports_decays_and_on_time_reports_hold_or_raise_it() {
        let floor = paced_bytes_per_second(MIN_SEED_KBPS);
        let mut now = 1_000_000_000_u64;
        let mut pacer = LinkPacer::new();
        pacer.seed(now, 800);
        now += 3_000_000_000;
        assert_eq!(pacer.bytes_per_second(now), 120_000);
        now += 2_000_000_000;
        assert_eq!(pacer.bytes_per_second(now), 60_000);
        now += 60_000_000_000;
        assert_eq!(pacer.bytes_per_second(now), floor);
        pacer.set_measured_kbps(now, 800);
        assert_eq!(pacer.bytes_per_second(now), 120_000);

        let mut pacer = LinkPacer::new();
        pacer.seed(now, 1_488);
        let seeded = pacer.bytes_per_second(now);
        for _ in 0..30 {
            now += 2_000_000_000;
            assert!(pacer.hold_seed(now, None));
            assert_eq!(pacer.bytes_per_second(now), seeded);
        }
        now += 60_000_000_000;
        assert_eq!(pacer.bytes_per_second(now), floor);
        assert!(pacer.hold_seed(now, None));
        assert_eq!(pacer.bytes_per_second(now), seeded);
        pacer.set_measured_kbps(now, 600);
        assert!(!pacer.hold_seed(now, None));
        assert_eq!(pacer.bytes_per_second(now), 90_000);

        let mut pacer = LinkPacer::new();
        pacer.seed(now, 350);
        assert!(pacer.hold_seed(now, None));
        assert!(pacer.hold_seed(now, Some(SEED_RAISE_BACKLOG_MS - 1)));
        assert_eq!(pacer.seed_kbps(), 350);
        assert!(pacer.hold_seed(now, Some(SEED_RAISE_BACKLOG_MS)));
        assert_eq!(pacer.seed_kbps(), 437);
        assert_eq!(
            pacer.bytes_per_second(now),
            (437.5 * 1_000.0 / 8.0 * PACE_GAIN) as u64
        );
        for _ in 0..20 {
            pacer.hold_seed(now, Some(10_000));
        }
        assert_eq!(pacer.seed_kbps(), SEED_RAISE_CAP_KBPS);
        now += 60_000_000_000;
        assert_eq!(pacer.bytes_per_second(now), floor);
        assert!(!LinkPacer::new().hold_seed(now, None));
        assert_eq!(decayed_seed(10_000, 100_000_000_000), 10_000);
    }
    #[test]
    fn a_raised_seed_drains_what_was_charged_and_blocking_writes_are_charged_from_their_start() {
        let mut now = 1_000_000_000_u64;
        let mut pacer = LinkPacer::with_burst(0);
        pacer.seed(now, 400);
        pacer.on_sent(60_000, now, now);
        assert_eq!(pacer.wait_ns(now), 1_000_000_000);
        pacer.hold_seed(now, Some(SEED_RAISE_BACKLOG_MS));
        assert_eq!(pacer.wait_ns(now), 800_000_000);

        let mut pacer = LinkPacer::new();
        pacer.set_measured_kbps(now, 800);
        let started = now;
        now += 1_000_000_000;
        pacer.on_sent(120_000, started, now);
        assert_eq!(pacer.wait_ns(now), 0);
        pacer.on_sent(12_000, now, now);
        assert_eq!(pacer.wait_ns(now), 50_000_000);
        assert_eq!(seed_kbps(Some(620), Some(true)), 620);
        assert_eq!(seed_kbps(None, None), 350);
        assert_eq!(seed_kbps(Some(0), Some(false)), 250);
    }
}
