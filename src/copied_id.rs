//! The copied-ID warning (CF-PRIV-22, Android `CopiedIdTracker`, Design G
//! review F4). Keyed by (rotating ID, route cost); it fires when two
//! different advertising addresses carry the same key within 2000 ms of each
//! other on at least 2 separate occasions (occasions more than 2000 ms
//! apart). One ID on a second address in turn (the stack's address timer)
//! and a relay's mirror at route cost + 1 are not copies. It disables
//! nothing: the screen asks for a second tap. Addresses stay in memory only.
//!
//! Overlap rule (as on Android, default on; `MESHLIVE_COPIED_ID_OVERLAP=0`
//! is the control arm): two addresses count only when their sightings
//! interleave, i.e. the other address was heard again after this one first
//! appeared. An advertiser that restarts its set with a new random address
//! (the old one stopping just before) is a handover, not a copy.
use std::collections::HashMap;

/// The overlap rule's switch (default on).
pub fn overlap_rule() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_COPIED_ID_OVERLAP").ok().as_deref() != Some("0"))
}

pub const SIMULTANEOUS_MS: u64 = 2_000;
pub const REPEATS: u32 = 2;
pub const STALE_MS: u64 = 20 * 60_000;
pub const MAX_KEYS: usize = 512;

type Key = ([u8; 6], u8);

pub struct CopiedIdTracker {
    /// Per key: (address, first heard, last heard).
    seen: HashMap<Key, Vec<(u64, u64, u64)>>,
    touched: HashMap<Key, u64>,
    coincidences: HashMap<Key, u32>,
    last_coincidence: HashMap<Key, u64>,
    require_overlap: bool,
}
impl Default for CopiedIdTracker {
    fn default() -> Self {
        Self::with_overlap(overlap_rule())
    }
}
impl CopiedIdTracker {
    pub fn with_overlap(require_overlap: bool) -> Self {
        Self {
            seen: HashMap::new(),
            touched: HashMap::new(),
            coincidences: HashMap::new(),
            last_coincidence: HashMap::new(),
            require_overlap,
        }
    }
    /// Records one advert; true when the warning is now due for this ID.
    pub fn observe(&mut self, rdi: [u8; 6], address: u64, cost: u8, now_ms: u64) -> bool {
        let key = (rdi, cost);
        self.touched.insert(key, now_ms);
        if self.touched.len() > MAX_KEYS {
            self.prune(now_ms);
        }
        let list = self.seen.entry(key).or_default();
        let own_first = match list.iter_mut().find(|(a, _, _)| *a == address) {
            Some(own) => {
                own.2 = now_ms;
                own.1
            }
            None => {
                list.push((address, now_ms, now_ms));
                now_ms
            }
        };
        let overlap = self.require_overlap;
        let other = list.iter().any(|(a, _, last)| {
            *a != address
                && now_ms.saturating_sub(*last) <= SIMULTANEOUS_MS
                // The other address was heard after this one appeared: both on
                // the air, not one replacing the other.
                && (!overlap || *last > own_first)
        });
        if other {
            let previous = self.last_coincidence.get(&key).copied();
            if previous.is_none_or(|p| now_ms.saturating_sub(p) > SIMULTANEOUS_MS) {
                *self.coincidences.entry(key).or_default() += 1;
                self.last_coincidence.insert(key, now_ms);
            }
        }
        self.coincidences.get(&key).copied().unwrap_or(0) >= REPEATS
    }
    /// The warning is due for this ID at any route cost.
    pub fn warned(&self, rdi: &[u8; 6]) -> bool {
        self.coincidences.iter().any(|((r, _), n)| r == rdi && *n >= REPEATS)
    }
    /// Drops every ID not heard for 20 minutes.
    pub fn prune(&mut self, now_ms: u64) {
        let stale: Vec<Key> = self.touched.iter().filter(|(_, t)| now_ms.saturating_sub(**t) > STALE_MS).map(|(k, _)| *k).collect();
        for key in stale {
            self.touched.remove(&key);
            self.seen.remove(&key);
            self.coincidences.remove(&key);
            self.last_coincidence.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: [u8; 6] = [1, 2, 3, 4, 5, 6];

    #[test]
    fn two_addresses_at_once_on_two_occasions_warn() {
        let mut t = CopiedIdTracker::default();
        assert!(!t.observe(ID, 1, 0, 0));
        assert!(!t.observe(ID, 2, 0, 500), "one coincidence is not enough");
        assert!(!t.observe(ID, 1, 0, 1_000), "the first interleaved occasion");
        assert!(!t.observe(ID, 1, 0, 3_200));
        assert!(t.observe(ID, 2, 0, 3_300), "a second occasion more than 2 s later");
        assert!(t.warned(&ID));
    }

    #[test]
    fn a_second_address_in_turn_or_a_relay_mirror_is_not_a_copy() {
        let mut t = CopiedIdTracker::default();
        // The stack's address timer: the old address stops, a new one follows.
        t.observe(ID, 1, 0, 0);
        t.observe(ID, 2, 0, 5_000);
        t.observe(ID, 2, 0, 9_000);
        assert!(!t.warned(&ID));
        // A relay mirrors the ID at cost + 1 from its own address.
        let mut t = CopiedIdTracker::default();
        for n in 0..10 {
            t.observe(ID, 1, 0, n * 3_000);
            t.observe(ID, 7, 1, n * 3_000 + 100);
        }
        assert!(!t.warned(&ID));
    }

    /// Android edge-case suite m31-a15s-redmi-1: an advertiser replaced its
    /// set (new random address) 31 ms after the old one stopped, three times.
    fn handovers(t: &mut CopiedIdTracker) -> bool {
        let mut warned = false;
        for (index, address) in [0x7935u64, 0x5b2a, 0x57e0, 0x4f9a].into_iter().enumerate() {
            let index = index as u64;
            let mut at = index * 10_000 + if index == 0 { 0 } else { 31 };
            while at < (index + 1) * 10_000 {
                warned |= t.observe(ID, address, 0, at);
                at += 100;
            }
        }
        warned
    }

    #[test]
    fn address_handovers_one_after_another_are_not_a_copy_even_within_two_seconds() {
        let mut t = CopiedIdTracker::with_overlap(true);
        assert!(!handovers(&mut t));
        assert!(!t.warned(&ID));
    }

    #[test]
    fn the_control_arm_keeps_the_two_second_rule_that_flagged_the_handovers() {
        assert!(handovers(&mut CopiedIdTracker::with_overlap(false)));
    }

    #[test]
    fn a_copy_that_starts_while_the_real_phone_keeps_advertising_is_still_found() {
        let mut t = CopiedIdTracker::with_overlap(true);
        let mut warned = false;
        let mut at = 0;
        while at < 5_000 {
            warned |= t.observe(ID, 0xaa01, 0, at);
            at += 100;
        }
        while at < 15_000 {
            warned |= t.observe(ID, 0xaa01, 0, at);
            warned |= t.observe(ID, 0xee66, 0, at + 50);
            at += 200;
        }
        assert!(warned && t.warned(&ID));
    }

    #[test]
    fn ids_not_heard_for_twenty_minutes_are_forgotten() {
        let mut t = CopiedIdTracker::default();
        t.observe(ID, 1, 0, 0);
        t.observe(ID, 2, 0, 100);
        t.observe(ID, 1, 0, 200);
        t.observe(ID, 1, 0, 3_000);
        t.observe(ID, 2, 0, 3_100);
        assert!(t.warned(&ID));
        t.prune(3_100 + STALE_MS + 1);
        assert!(!t.warned(&ID));
    }
}
