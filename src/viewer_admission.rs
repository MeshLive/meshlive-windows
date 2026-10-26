//! Android ViewerAdmissionBounds / PreAuthAdmissionGate, for the one-child QV source.
use crate::bluetooth::{Error, scan::Sighting};
use std::{collections::VecDeque, time::{Duration, Instant}};

type Key = (u64, [u8; 6]);
fn key(s: &Sighting) -> Key { (s.route.address_key(), s.announcement.prefix) }
const MAX_TRACKED: usize = 512;

/// BoundedExpiringMap: refresh a key, sweep at capacity, then evict soonest expiry.
#[derive(Default)]
struct Table(Vec<(Key, Instant, Instant)>);
impl Table {
    fn put(&mut self, key: Key, value: Instant, until: Instant, now: Instant) {
        self.0.retain(|(k, _, _)| *k != key);
        if self.0.len() >= MAX_TRACKED { self.0.retain(|(_, _, expiry)| *expiry > now); }
        if self.0.len() >= MAX_TRACKED {
            let oldest = self.0.iter().enumerate().min_by_key(|(_, e)| e.2).unwrap().0;
            self.0.remove(oldest);
        }
        self.0.push((key, value, until));
    }
    fn peek(&self, key: Key) -> Option<Instant> { self.0.iter().find(|e| e.0 == key).map(|e| e.1) }
}

#[derive(Default)]
pub struct Requests {
    pending: Vec<Sighting>,
    seen: Table,
    failures: Table,
    attempts: VecDeque<Instant>,
    pool_failures: u32,
    pool_until: Option<Instant>,
}
impl Requests {
    pub fn observe(&mut self, sighting: Sighting, now: Instant) {
        self.pending.retain(|s| now.saturating_duration_since(s.received) <= Duration::from_secs(12));
        let k = key(&sighting);
        let since = self.seen.peek(k).unwrap_or(now);
        self.seen.put(k, since, now + Duration::from_secs(30), now);
        if let Some(old) = self.pending.iter_mut().find(|s| key(s) == k) { *old = sighting; }
        else if self.pending.len() < MAX_TRACKED { self.pending.push(sighting); }
    }
    /// Deferred requests incur no capacity cooldown. Only attempts consume the window.
    pub fn next(&mut self, now: Instant) -> Option<Sighting> {
        self.pending.retain(|s| now.saturating_duration_since(s.received) <= Duration::from_secs(12));
        self.failures.0.retain(|e| e.2 > now);
        while self.attempts.front().is_some_and(|at| now.saturating_duration_since(*at) >= Duration::from_secs(10)) { self.attempts.pop_front(); }
        if self.attempts.len() >= 4 || self.pool_until.is_some_and(|until| now < until) { return None; }
        let index = self.pending.iter().enumerate().min_by_key(|(_, s)| {
            (self.failures.peek(key(s)).is_some(), self.seen.peek(key(s)).unwrap_or(now))
        })?.0;
        Some(self.pending.remove(index))
    }
    pub fn attempted(&mut self, now: Instant) { self.attempts.push_back(now); }
    pub fn failed(&mut self, s: &Sighting, now: Instant) { self.failures.put(key(s), now, now + Duration::from_secs(10), now); }
    /// Android PoolBackoff client policy: 1, 2, 4, 8, 16, 30 seconds;
    /// failures during a hold do not advance it; a successful connect resets it.
    /// WinRT has no Android registration status 257/128. Only explicit Windows
    /// allocation/system-resource exhaustion maps to a pool failure here.
    pub fn connected(&mut self, result: Result<(), Error>, now: Instant) {
        match result {
            Ok(()) => { self.pool_failures = 0; self.pool_until = None; }
            Err(Error::Native(code)) if matches!(code as u32, 0x8007000e | 0x800705aa) => {
                if self.pool_until.is_some_and(|until| now < until) { return; }
                self.pool_failures += 1;
                self.pool_until = Some(now + Duration::from_secs((1u64 << (self.pool_failures - 1).min(10)).min(30)));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bluetooth::scan::Route, discovery::{Announcement, ACCEPTING, GATT}};
    fn request(id: u64, now: Instant) -> Sighting {
        Sighting { route: Route { address: id, address_type: Default::default() },
            announcement: Announcement { version: 5, prefix: id.to_be_bytes()[2..].try_into().unwrap(), flags: ACCEPTING | GATT,
                summary: 0, token: 1, cost: 0, sequence: 0, width: 0, height: 0, fps: 0, bitrate: 0, recovery: None },
            rssi: 0, received: now, kind: (5, true, false) }
    }
    #[test]
    fn rotating_requests_cannot_starve_persistent_viewer() {
        let start = Instant::now(); let mut board = Requests::default(); let mut admitted = None;
        // Fifty fresh address/RDI pairs per minute; attempts fail before authentication.
        for tick in 0..=300 {
            let now = start + Duration::from_millis(tick * 200);
            if tick % 6 == 0 { board.observe(request(tick + 1, now), now); }
            // Join only after rotating requests have consumed all four slots.
            if tick >= 25 { board.observe(request(9999, now), now); }
            if let Some(s) = board.next(now) {
                board.attempted(now);
                if key(&s).0 == 9999 { admitted = Some(tick * 200); break; }
                board.failed(&s, now);
            }
        }
        assert!(admitted.is_some_and(|ms| (10_000..60_000).contains(&ms)));
    }
    #[test]
    fn window_bound_failure_order_expiry_and_bounded_tracking() {
        let now = Instant::now(); let mut board = Requests::default();
        for id in 0..4 { board.observe(request(id, now), now); let s = board.next(now).unwrap(); board.attempted(now); board.failed(&s, now); }
        board.observe(request(20, now), now);
        assert!(board.next(now + Duration::from_millis(9999)).is_none());
        assert_eq!(key(&board.next(now + Duration::from_secs(10)).unwrap()).0, 20);
        let mut board = Requests::default();
        let old = request(1, now); board.observe(old.clone(), now); board.failed(&old, now);
        board.observe(request(2, now + Duration::from_secs(1)), now + Duration::from_secs(1));
        assert_eq!(key(&board.next(now + Duration::from_secs(1)).unwrap()).0, 2);
        assert_eq!(key(&board.next(now + Duration::from_secs(10)).unwrap()).0, 1);
        for id in 0..2000 { let s = request(id, now); board.observe(s.clone(), now); board.failed(&s, now); }
        assert!(board.pending.len() <= 512 && board.seen.0.len() <= 512 && board.failures.0.len() <= 512);
        assert!(board.next(now + Duration::from_secs(13)).is_none());
    }
    #[test]
    fn pool_backoff_holds_all_rotated_routes_and_resets_only_on_success() {
        let mut board = Requests::default(); let mut now = Instant::now();
        for delay in [1, 2, 4, 8, 16, 30, 30] {
            board.connected(Err(Error::Native(0x800705aau32 as i32)), now);
            board.observe(request(delay, now), now);
            board.connected(Err(Error::Native(0x800705aau32 as i32)), now);
            assert!(board.next(now + Duration::from_millis(999)).is_none());
            assert_eq!(board.pool_until, Some(now + Duration::from_secs(delay)));
            now += Duration::from_secs(delay);
        }
        board.connected(Ok(()), now);
        board.observe(request(99, now), now);
        assert!(board.next(now).is_some());
        board.connected(Err(Error::Timeout), now);
        assert!(board.pool_until.is_none());
    }
}
