//! The queue-age quality rule (F-9): a second is slow while the carrier is the
//! send carrier and its oldest unsent record is older than 300 ms; five
//! consecutive slow seconds call the quality fallback. Pure, no I/O.

pub const QUALITY_SAMPLE_MS: i64 = 1_000;
pub const QUALITY_SLOW_AGE_MS: i64 = 300;
pub const QUALITY_SLOW_SECONDS: u32 = 5;
pub const QUALITY_GAP_RESET_MS: i64 = 3_000;

pub struct QualitySampler {
    last_sample_ms: i64,
    slow: u32,
}

impl QualitySampler {
    /// Seeded at hand-off.
    pub fn new(now_ms: i64) -> Self {
        Self {
            last_sample_ms: now_ms,
            slow: 0,
        }
    }

    /// Returns true exactly when the fifth consecutive slow second is reached.
    pub fn sample(
        &mut self,
        now_ms: i64,
        is_send_carrier: bool,
        oldest_age_ms: Option<i64>,
    ) -> bool {
        if now_ms - self.last_sample_ms < QUALITY_SAMPLE_MS {
            return false;
        }
        if now_ms - self.last_sample_ms > QUALITY_GAP_RESET_MS {
            self.slow = 0;
            self.last_sample_ms = now_ms;
            return false;
        }
        self.last_sample_ms = now_ms;
        let slow = is_send_carrier && oldest_age_ms.is_some_and(|age| age > QUALITY_SLOW_AGE_MS);
        if slow {
            self.slow += 1;
        } else {
            self.slow = 0;
        }
        if self.slow == QUALITY_SLOW_SECONDS {
            self.slow = 0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_carrier_falls_back_after_five_slow_seconds() {
        let mut sampler = QualitySampler::new(0);
        for second in 1..5 {
            assert!(!sampler.sample(second * 1_000, true, Some(400)));
        }
        assert!(sampler.sample(5_000, true, Some(400)));
    }

    #[test]
    fn a_keyframe_burst_under_300ms_never_counts() {
        let mut sampler = QualitySampler::new(0);
        for second in 1..=20 {
            assert!(!sampler.sample(second * 1_000, true, Some(250)));
        }
    }

    #[test]
    fn a_gap_over_three_seconds_resets_the_slow_count() {
        let mut sampler = QualitySampler::new(0);
        for second in 1..=4 {
            assert!(!sampler.sample(second * 1_000, true, Some(400)));
        }
        assert!(!sampler.sample(7_500, true, Some(400)));
        for step in 1..=4 {
            assert!(!sampler.sample(7_500 + step * 1_000, true, Some(400)));
        }
        assert!(sampler.sample(12_500, true, Some(400)));
    }

    #[test]
    fn the_sampler_keeps_counting_after_a_fallback_until_the_carrier_closes() {
        let mut sampler = QualitySampler::new(0);
        for second in 1..=5 {
            sampler.sample(second * 1_000, true, Some(400));
        }
        for second in 6..10 {
            assert!(!sampler.sample(second * 1_000, true, Some(400)));
        }
        assert!(sampler.sample(10_000, true, Some(400)));
    }

    #[test]
    fn a_second_that_is_not_the_send_carrier_is_never_slow() {
        let mut sampler = QualitySampler::new(0);
        for second in 1..=20 {
            assert!(!sampler.sample(second * 1_000, false, Some(10_000)));
        }
    }
}
