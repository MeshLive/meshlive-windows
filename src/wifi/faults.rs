//! The phone-wide local-fault pause and the process-wide Firewall/profile
//! flags. Peer failures never call `record`; an unknown or non-public profile
//! is not a fault either.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Instant,
};
use crate::wifi::limits::{LOCAL_FAULT_LIMIT, LOCAL_FAULT_PAUSE_MS, LOCAL_FAULT_WINDOW_MS};

#[derive(Default)]
pub struct Faults {
    times_ms: Vec<i64>,
    pause_until_ms: Option<i64>,
}

impl Faults {
    pub fn new() -> Self {
        Self::default()
    }

    /// A LOCAL platform failure (start refused, Wi-Fi off during setup, listener
    /// bind failed, address missing). Never a peer or unknown-profile failure.
    pub fn record(&mut self, now_ms: i64) {
        self.times_ms.push(now_ms);
    }

    /// True while the three oldest faults fall inside the window, until the
    /// third-oldest + `LOCAL_FAULT_PAUSE_MS`.
    pub fn paused(&mut self, now_ms: i64) -> bool {
        if let Some(until) = self.pause_until_ms {
            if now_ms < until {
                return true;
            }
            self.pause_until_ms = None;
        }
        let window_start = now_ms - LOCAL_FAULT_WINDOW_MS;
        self.times_ms.retain(|at| *at >= window_start);
        if self.times_ms.len() < LOCAL_FAULT_LIMIT {
            return false;
        }
        let third_oldest = self.times_ms[LOCAL_FAULT_LIMIT - 1];
        let until = third_oldest + LOCAL_FAULT_PAUSE_MS;
        if now_ms < until {
            self.pause_until_ms = Some(until);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn recorded(&self) -> usize {
        self.times_ms.len()
    }
}

fn global_faults() -> &'static Mutex<Faults> {
    static FAULTS: OnceLock<Mutex<Faults>> = OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(Faults::new()))
}

pub fn global() -> &'static Mutex<Faults> {
    global_faults()
}

/// Monotonic ms since the first call in this process.
pub fn clock_ms() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as i64
}

static GROUP_NOT_PUBLIC: AtomicBool = AtomicBool::new(false);
static PROFILE_UNKNOWN: AtomicBool = AtomicBool::new(false);
static ZERO_ACCEPT: AtomicU32 = AtomicU32::new(0);

pub fn set_group_not_public() {
    GROUP_NOT_PUBLIC.store(true, Ordering::Release);
}
pub fn group_not_public() -> bool {
    GROUP_NOT_PUBLIC.load(Ordering::Acquire)
}
/// The group's profile could not be read: NO_PENALTY; bit 2 is withheld until
/// MeshLive restarts; never a fault.
pub fn set_profile_unknown() {
    PROFILE_UNKNOWN.store(true, Ordering::Release);
}
pub fn profile_unknown() -> bool {
    PROFILE_UNKNOWN.load(Ordering::Acquire)
}
pub fn note_zero_accept() {
    ZERO_ACCEPT.fetch_add(1, Ordering::AcqRel);
}
pub fn clear_zero_accept() {
    ZERO_ACCEPT.store(0, Ordering::Release);
}
/// True after two zero-accept expiries in a row.
pub fn firewall_suspected() -> bool {
    ZERO_ACCEPT.load(Ordering::Acquire) >= 2
}

#[cfg(test)]
pub fn reset_for_tests() {
    GROUP_NOT_PUBLIC.store(false, Ordering::Release);
    PROFILE_UNKNOWN.store(false, Ordering::Release);
    ZERO_ACCEPT.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    #[test]
    fn three_local_faults_in_an_hour_pause_for_an_hour() {
        let mut faults = Faults::new();
        faults.record(0);
        faults.record(10 * MIN);
        faults.record(20 * MIN);
        assert!(faults.paused(21 * MIN));
        assert!(faults.paused(79 * MIN));
        assert!(!faults.paused(81 * MIN));
    }

    #[test]
    fn two_faults_do_not_pause() {
        let mut faults = Faults::new();
        faults.record(0);
        faults.record(10 * MIN);
        assert!(!faults.paused(21 * MIN));
    }

    #[test]
    fn faults_older_than_an_hour_are_forgotten() {
        let mut faults = Faults::new();
        faults.record(0);
        faults.record(30 * MIN);
        faults.record(61 * MIN);
        assert!(!faults.paused(62 * MIN));
    }

    #[test]
    fn two_zero_accept_expiries_in_a_row_suspect_the_firewall_and_an_accept_clears_it() {
        reset_for_tests();
        assert!(!firewall_suspected());
        note_zero_accept();
        assert!(!firewall_suspected());
        note_zero_accept();
        assert!(firewall_suspected());
        clear_zero_accept();
        assert!(!firewall_suspected());
        reset_for_tests();
    }
}
