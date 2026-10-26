//! When the one encoder of a source makes a keyframe. Ports Android's rules
//! (`CameraVideoEncoder`): the first frame and every frame after an encoder
//! (re)start is a keyframe; a safety keyframe follows the configured interval
//! (Android: 60 s, because a scheduled keyframe on detailed content is the
//! biggest single burst on a Bluetooth link); requests are coalesced, so any
//! number of requests before the next frame make one keyframe; unforced
//! requests (loss recovery) follow `KeyFrameRequestBackoff` (1 s apart,
//! doubling to 8 s while they keep arriving back to back, reset after 10 calm
//! seconds); forced requests (a new viewer joining) are never delayed.
//! Pure state, no codec: the caller passes its monotonic clock.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeBackoff {
    min_ns: u64,
    max_ns: u64,
    calm_ns: u64,
    last_ns: Option<u64>,
    interval_ns: u64,
}
impl Default for KeyframeBackoff {
    fn default() -> Self {
        Self::new(1_000_000_000, 8_000_000_000, 10_000_000_000)
    }
}
impl KeyframeBackoff {
    pub fn new(min_ns: u64, max_ns: u64, calm_ns: u64) -> Self {
        Self {
            min_ns,
            max_ns,
            calm_ns,
            last_ns: None,
            interval_ns: min_ns,
        }
    }
    /// Android `KeyFrameRequestBackoff.shouldHonor`, the same arithmetic.
    pub fn should_honor(&mut self, now_ns: u64, force: bool) -> bool {
        if let (false, Some(last)) = (force, self.last_ns) {
            let since = now_ns.saturating_sub(last);
            if since < self.interval_ns {
                return false;
            }
            self.interval_ns = if since >= self.calm_ns {
                self.min_ns
            } else if since < self.interval_ns.saturating_mul(2) {
                self.interval_ns.saturating_mul(2).min(self.max_ns)
            } else {
                self.interval_ns
            };
        }
        self.last_ns = Some(now_ns);
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyframePolicy {
    interval_us: i64,
    pending: bool,
    last_key_pts_us: Option<i64>,
    backoff: KeyframeBackoff,
    /// Requests honoured / coalesced into an already pending keyframe / deferred by the backoff.
    pub honoured: u64,
    pub coalesced: u64,
    pub deferred: u64,
}
impl KeyframePolicy {
    /// `interval_s` 0 disables the safety keyframe (requests only).
    pub fn new(interval_s: u32) -> Self {
        Self {
            interval_us: i64::from(interval_s) * 1_000_000,
            pending: true,
            last_key_pts_us: None,
            backoff: KeyframeBackoff::default(),
            honoured: 0,
            coalesced: 0,
            deferred: 0,
        }
    }
    /// A viewer's request (`force` for a joining viewer, else loss recovery).
    /// Returns true when a keyframe is now pending.
    pub fn request(&mut self, now_ns: u64, force: bool) -> bool {
        if self.pending {
            self.coalesced += 1;
            return true;
        }
        if self.backoff.should_honor(now_ns, force) {
            self.pending = true;
            self.honoured += 1;
        } else {
            self.deferred += 1;
        }
        self.pending
    }
    /// An encoder restart (new size, new encoder): its first frame is a keyframe.
    pub fn restart(&mut self) {
        self.pending = true;
        self.last_key_pts_us = None;
    }
    /// Whether the frame at `pts_us` must be forced as a keyframe; clears the request.
    pub fn force_for(&mut self, pts_us: i64) -> bool {
        let due = match self.last_key_pts_us {
            None => true,
            Some(last) => self.interval_us > 0 && pts_us - last >= self.interval_us,
        };
        let force = std::mem::take(&mut self.pending) || due;
        if force {
            // Asynchronous (hardware) encoders return the keyframe later; the
            // interval counts from the forced input, not from its output.
            self.last_key_pts_us = Some(pts_us);
        }
        force
    }
    /// The encoder's actual output: any keyframe (forced or the encoder's own) restarts the interval.
    pub fn produced(&mut self, pts_us: i64, key_frame: bool) {
        if key_frame {
            self.last_key_pts_us = Some(pts_us);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;

    #[test]
    fn backoff_matches_android_spacing_doubling_and_calm_reset() {
        let mut b = KeyframeBackoff::default();
        assert!(b.should_honor(0, false));
        assert!(!b.should_honor(S / 2, false));
        assert!(b.should_honor(S, false)); // 1 s apart; back to back -> 2 s
        assert!(!b.should_honor(2 * S, false));
        assert!(b.should_honor(3 * S, false)); // -> 4 s
        assert!(!b.should_honor(6 * S, false));
        assert!(b.should_honor(7 * S, false)); // -> 8 s cap
        assert!(!b.should_honor(14 * S, false));
        assert!(b.should_honor(15 * S, false)); // stays 8 s
        assert!(b.should_honor(15 * S + 1, true)); // forced is never delayed
        assert!(b.should_honor(26 * S, false)); // 11 s calm: back to 1 s
        assert!(b.should_honor(27 * S, false));
    }

    #[test]
    fn first_frame_interval_and_coalesced_requests() {
        let mut p = KeyframePolicy::new(60);
        assert!(p.force_for(0));
        p.produced(0, true);
        assert!(!p.force_for(66_666));
        p.produced(66_666, false);
        // An asynchronous encoder has not returned frame 0 yet: no second force.
        let mut slow = KeyframePolicy::new(60);
        assert!(slow.force_for(0));
        assert!(!slow.force_for(66_666));
        // Three viewers ask before the next frame: one keyframe.
        assert!(p.request(2 * S, true));
        assert!(p.request(2 * S, true));
        assert!(p.request(2 * S, false));
        assert_eq!((p.honoured, p.coalesced), (1, 2));
        assert!(p.force_for(2_000_000));
        p.produced(2_000_000, true);
        assert!(!p.force_for(2_066_666));
        // Unforced recovery 0.5 s after the forced one waits for the backoff.
        assert!(!p.request(2 * S + S / 2, false));
        assert_eq!(p.deferred, 1);
        assert!(!p.force_for(2_500_000));
        // Safety keyframe 60 s after the last keyframe.
        assert!(!p.force_for(61_999_999));
        assert!(p.force_for(62_000_000));
        // An encoder's own keyframe restarts the interval too.
        p.produced(62_000_000, true);
        assert!(!p.force_for(100_000_000));
        p.restart();
        assert!(p.force_for(100_066_666));
        let mut off = KeyframePolicy::new(0);
        assert!(off.force_for(0));
        off.produced(0, true);
        assert!(!off.force_for(i64::MAX / 2));
    }
}
