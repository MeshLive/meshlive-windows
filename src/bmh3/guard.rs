//! Shared across attempts: process-wide without pseudonyms, discovery-epoch
//! scoped with them. Never construct a new owner for each incoming connection.
use super::Error;
use crate::crypto;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

struct Window {
    seen: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
    origin: Instant,
    last_ms: u64,
    tokens: f64,
}
impl Default for Window {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            origin: Instant::now(),
            last_ms: 0,
            tokens: 8.0,
        }
    }
}
impl Window {
    pub(super) fn first_use(&mut self, point: &[u8; 65]) -> Result<(), Error> {
        let digest = crypto::sha256(point)?;
        if !self.seen.insert(digest) {
            return Err(Error::Replay);
        }
        self.order.push_back(digest);
        if self.order.len() > 4096 {
            self.seen.remove(&self.order.pop_front().unwrap());
        }
        Ok(())
    }
    fn reserve_at(&mut self, now_ms: u64) -> Result<Duration, Error> {
        self.tokens = (self.tokens + now_ms.saturating_sub(self.last_ms) as f64 / 4000.0).min(8.0);
        self.last_ms = self.last_ms.max(now_ms);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(Duration::ZERO);
        }
        // Match the Android bucket's floor + 1 ms rule and maximum reservation.
        let wait = ((1.0 - self.tokens) * 4000.0) as u64 + 1;
        if wait > 4000 {
            return Err(Error::Budget);
        }
        self.tokens -= 1.0;
        Ok(Duration::from_millis(wait))
    }
    /// Call only on the protocol worker. Waiting is bounded to four seconds;
    /// transport pending-connection/deadline guards must also remain in force.
    pub(super) fn spend(&mut self) -> Result<(), Error> {
        let now = self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let wait = self.reserve_at(now)?;
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Guards {
    process: Window,
    epochs: BTreeMap<u32, Window>,
    latest: Option<u32>,
}
impl Guards {
    pub(super) fn for_attempt(
        &mut self,
        pseudonyms: bool,
        epoch: Option<u32>,
    ) -> Result<AttemptGuards<'_>, Error> {
        if !pseudonyms {
            return Ok(AttemptGuards(&mut self.process));
        }
        let epoch = epoch.ok_or(Error::Policy)?;
        if self.latest.is_some_and(|latest| epoch < latest) {
            return Err(Error::Policy);
        }
        self.latest = Some(epoch);
        self.epochs.retain(|old, _| *old >= epoch.saturating_sub(1));
        Ok(AttemptGuards(self.epochs.entry(epoch).or_default()))
    }
}
pub(super) struct AttemptGuards<'a>(&'a mut Window);
impl AttemptGuards<'_> {
    pub(super) fn first_use(&mut self, point: &[u8; 65]) -> Result<(), Error> {
        self.0.first_use(point)
    }
    pub(super) fn spend(&mut self) -> Result<(), Error> {
        self.0.spend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signing_bucket_matches_burst_refill_wait_and_overcommit_limits() {
        let mut guards = Window::default();
        for _ in 0..8 {
            assert_eq!(guards.reserve_at(0), Ok(Duration::ZERO));
        }
        assert_eq!(guards.reserve_at(0), Err(Error::Budget));
        assert_eq!(guards.reserve_at(1), Ok(Duration::from_millis(4000)));
        assert_eq!(guards.reserve_at(1), Err(Error::Budget));
        assert_eq!(guards.reserve_at(8001), Ok(Duration::ZERO));
        // A backward time sample does not refill or create extra tokens.
        assert_eq!(guards.reserve_at(8000), Ok(Duration::from_millis(4000)));
        assert_eq!(guards.reserve_at(8000), Err(Error::Budget));
        assert_eq!(guards.reserve_at(100000), Ok(Duration::ZERO));
        for _ in 0..7 {
            assert_eq!(guards.reserve_at(100000), Ok(Duration::ZERO));
        }
        assert_eq!(guards.reserve_at(100000), Err(Error::Budget));
    }
    #[test]
    fn replay_memory_is_fifo_bounded_and_shared_across_attempts() {
        let mut guards = Window::default();
        let point = [7; 65];
        guards.first_use(&point).unwrap();
        assert_eq!(guards.first_use(&point), Err(Error::Replay));
        for index in 0..4096_u32 {
            let mut point = [0; 65];
            point[..4].copy_from_slice(&index.to_be_bytes());
            guards.first_use(&point).unwrap();
        }
        assert_eq!(guards.order.len(), 4096);
        assert_eq!(guards.seen.len(), 4096);
        guards.first_use(&point).unwrap();
    }
    #[test]
    fn pseudonym_replay_and_budget_state_cannot_link_discovery_epochs() {
        let mut guards = Guards::default();
        let point = [4; 65];
        assert!(matches!(guards.for_attempt(true, None), Err(Error::Policy)));
        guards
            .for_attempt(true, Some(10))
            .unwrap()
            .first_use(&point)
            .unwrap();
        assert_eq!(
            guards
                .for_attempt(true, Some(10))
                .unwrap()
                .first_use(&point),
            Err(Error::Replay)
        );
        guards.epochs.get_mut(&10).unwrap().tokens = 0.0;
        guards
            .for_attempt(true, Some(11))
            .unwrap()
            .first_use(&point)
            .unwrap();
        assert_eq!(guards.epochs[&11].tokens, 8.0);
        guards.for_attempt(true, Some(12)).unwrap();
        assert_eq!(guards.epochs.len(), 2);
        assert!(!guards.epochs.contains_key(&10));
        assert!(matches!(
            guards.for_attempt(true, Some(11)),
            Err(Error::Policy)
        ));
        guards
            .for_attempt(false, None)
            .unwrap()
            .first_use(&point)
            .unwrap();
        assert_eq!(
            guards
                .for_attempt(false, Some(20))
                .unwrap()
                .first_use(&point),
            Err(Error::Replay)
        );
    }
}
