//! How long a nearby caller row may go unheard (CF-DISC-39, Android
//! `CallPeerCadence`): the row goes after 3 missed sightings at the measured
//! cadence (the largest of the last 16 gaps), bounded to 4..12 s; with fewer
//! than 4 gaps, or after the offer left the air for over 12 s, the fixed 12 s.
use std::collections::{HashMap, VecDeque};

pub const STALE_MS: u64 = 12_000;
pub const MIN_STALE_MS: u64 = 4_000;
pub const MISSED_GAPS: u64 = 3;
pub const WINDOW: usize = 16;
pub const MIN_GAPS: usize = 4;

struct Track {
    last_ms: u64,
    gaps: VecDeque<u64>,
}

#[derive(Default)]
pub struct Cadence<K> {
    tracks: HashMap<K, Track>,
}
impl<K: std::hash::Hash + Eq + Clone> Cadence<K> {
    /// One sighting of `key` at `now_ms`.
    pub fn observe(&mut self, key: &K, now_ms: u64) {
        let Some(track) = self.tracks.get_mut(key) else {
            self.tracks.insert(key.clone(), Track { last_ms: now_ms, gaps: VecDeque::new() });
            return;
        };
        let gap = now_ms.saturating_sub(track.last_ms);
        track.last_ms = now_ms;
        if gap == 0 {
        } else if gap > STALE_MS {
            // It left the air and came back: the old measurement says nothing now.
            track.gaps.clear();
        } else {
            track.gaps.push_back(gap);
            while track.gaps.len() > WINDOW {
                track.gaps.pop_front();
            }
        }
    }
    pub fn stale_after_ms(&self, key: &K) -> u64 {
        match self.tracks.get(key) {
            Some(t) if t.gaps.len() >= MIN_GAPS => {
                (t.gaps.iter().max().unwrap() * MISSED_GAPS).clamp(MIN_STALE_MS, STALE_MS)
            }
            _ => STALE_MS,
        }
    }
    pub fn is_fresh(&self, key: &K, last_seen_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(last_seen_ms) <= self.stale_after_ms(key)
    }
    /// The last sighting time of `key`, if tracked.
    pub fn last_seen(&self, key: &K) -> Option<u64> {
        self.tracks.get(key).map(|t| t.last_ms)
    }
    /// The row was removed: a later sighting starts a new measurement.
    pub fn forget(&mut self, key: &K) {
        self.tracks.remove(key);
    }
    pub fn clear(&mut self) {
        self.tracks.clear();
    }
}

#[cfg(test)]
mod tests {
    //! Android CallPeerCadenceTest, same numbers.
    use super::*;
    const KEY: &str = "c79e03836d6a";

    #[test]
    fn a_phone_heard_often_leaves_the_list_after_three_missed_sightings_not_twelve_seconds() {
        let mut c = Cadence::default();
        let mut t = 0;
        for _ in 0..10 {
            c.observe(&KEY, t);
            t += 600;
        }
        let last = t - 600;
        assert_eq!(c.stale_after_ms(&KEY), MIN_STALE_MS);
        assert!(c.is_fresh(&KEY, last, last + 4_000));
        assert!(!c.is_fresh(&KEY, last, last + 4_001));
    }

    #[test]
    fn a_slow_scanner_keeps_its_longer_measured_window_up_to_the_fixed_one() {
        let mut c = Cadence::default();
        for t in [0, 2_000, 4_000, 7_000, 9_000] {
            c.observe(&KEY, t);
        }
        assert_eq!(c.stale_after_ms(&KEY), 9_000);
        for t in [13_000, 17_000] {
            c.observe(&KEY, t);
        }
        assert_eq!(c.stale_after_ms(&KEY), STALE_MS);
    }

    #[test]
    fn too_few_sightings_or_a_return_after_leaving_use_the_fixed_window() {
        let mut c = Cadence::default();
        assert_eq!(c.stale_after_ms(&"unknown"), STALE_MS);
        for t in [0, 500, 1_000] {
            c.observe(&KEY, t);
        }
        assert_eq!(c.stale_after_ms(&KEY), STALE_MS, "two gaps are not a measurement");
        for t in [1_500, 2_000] {
            c.observe(&KEY, t);
        }
        assert_eq!(c.stale_after_ms(&KEY), MIN_STALE_MS);
        c.observe(&KEY, 60_000);
        assert_eq!(c.stale_after_ms(&KEY), STALE_MS);
        c.forget(&KEY);
        assert_eq!(c.stale_after_ms(&KEY), STALE_MS);
    }
}
