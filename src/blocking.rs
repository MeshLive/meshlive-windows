//! Call blocking (CF-CALL-24, Android `CallBlocking`). The callee decides at
//! Invite time, before any UI, from the caller's authenticated identity:
//! - a caller presenting its real key whose full fingerprint is blocked is declined;
//! - with "Block unknown callers" on (default off), a pseudonymous caller or a
//!   real key that is not a verified contact is declined;
//! - an unreadable block list or contact store rings (fail open).
//!
//! A declined call looks normal on the wire: Ringing at once (as for any
//! call), then Decline(Rejected) after a uniform 3,000-7,000 ms, with no ring,
//! notification or window on this PC. Blocks key only on the full 32-byte
//! fingerprint, never on a prefix, rotating ID or name. Stored DPAPI-sealed at
//! `identity-verification/blocked-callers.bin` (on the wipe allowlist).
use crate::sealed::{self, Dpapi, Sealer};
use std::path::{Path, PathBuf};

pub const FILE: &str = "identity-verification/blocked-callers.bin";
const SEAL_LABEL: &str = "MeshLive blocked callers";
const MAGIC: &[u8; 4] = b"BBL1";
pub const DELAY_MS: std::ops::RangeInclusive<u64> = 3_000..=7_000;

pub struct BlockList {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
}
impl BlockList {
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self { path: dir.join(FILE), sealer }
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(Dpapi::new(SEAL_LABEL)))
    }
    /// The blocked fingerprints (Err: unreadable; the caller fails open).
    pub fn list(&self) -> Result<Vec<[u8; 32]>, ()> {
        let Some(bytes) = sealed::read(&self.path, self.sealer.as_ref()).map_err(|_| ())? else {
            return Ok(Vec::new());
        };
        let body = bytes.strip_prefix(MAGIC.as_slice()).ok_or(())?;
        if body.len() % 32 != 0 {
            return Err(());
        }
        Ok(body.chunks_exact(32).map(|c| c.try_into().unwrap()).collect())
    }
    pub fn contains(&self, fingerprint: &[u8; 32]) -> Result<bool, ()> {
        Ok(self.list()?.contains(fingerprint))
    }
    /// Blocks or unblocks; an unreadable list is never overwritten.
    pub fn set(&self, fingerprint: &[u8; 32], blocked: bool) -> Result<(), ()> {
        let mut list = self.list()?;
        list.retain(|f| f != fingerprint);
        if blocked {
            list.push(*fingerprint);
        }
        let mut bytes = MAGIC.to_vec();
        for f in &list {
            bytes.extend_from_slice(f);
        }
        sealed::write(&self.path, self.sealer.as_ref(), &bytes).map_err(|_| ())
    }
}

/// The caller as the callee authenticated it.
pub struct Caller {
    /// Some: the caller presented its real key (its full fingerprint).
    pub real: Option<[u8; 32]>,
    /// Some(true): a verified contact; None: the contact store is unreadable.
    pub contact: Option<bool>,
}

/// Whether to decline this caller silently. `blocked`: the block-list lookup
/// (None when unreadable).
pub fn decline(caller: &Caller, blocked: Option<bool>, block_unknown: bool) -> bool {
    if caller.real.is_some() && blocked == Some(true) {
        return true;
    }
    if block_unknown {
        return match caller.real {
            None => true,
            Some(_) => caller.contact == Some(false),
        };
    }
    false
}

/// A uniform delay in 3,000..=7,000 ms from the system RNG.
pub fn delay_ms() -> u64 {
    let span = DELAY_MS.end() - DELAY_MS.start() + 1;
    let random = crate::crypto::random_nonce().map(|n| u64::from_be_bytes(n[..8].try_into().unwrap())).unwrap_or(0);
    DELAY_MS.start() + random % span
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::tests::{Plain, scratch};

    #[test]
    fn the_policy_table_matches_android() {
        let fp = [9u8; 32];
        let real_contact = Caller { real: Some(fp), contact: Some(true) };
        let real_stranger = Caller { real: Some(fp), contact: Some(false) };
        let pseudonym = Caller { real: None, contact: None };
        let unreadable = Caller { real: Some(fp), contact: None };
        // Blocked fingerprint: declined whatever the switch.
        assert!(decline(&real_contact, Some(true), false));
        assert!(decline(&real_stranger, Some(true), true));
        // A pseudonymous caller cannot be on the list (no full fingerprint).
        assert!(!decline(&pseudonym, Some(true), false));
        // Block unknown callers.
        assert!(!decline(&real_stranger, Some(false), false));
        assert!(decline(&real_stranger, Some(false), true));
        assert!(decline(&pseudonym, Some(false), true));
        assert!(!decline(&real_contact, Some(false), true));
        // Fail open: unreadable block list or contact store rings.
        assert!(!decline(&real_contact, None, false));
        assert!(!decline(&unreadable, Some(false), true));
    }

    #[test]
    fn the_delay_is_uniform_in_range_and_the_list_round_trips() {
        for _ in 0..200 {
            assert!(DELAY_MS.contains(&delay_ms()));
        }
        let dir = scratch("blocking");
        let list = BlockList::new(&dir, Box::new(Plain));
        assert_eq!(list.list(), Ok(vec![]));
        list.set(&[1; 32], true).unwrap();
        list.set(&[2; 32], true).unwrap();
        list.set(&[1; 32], false).unwrap();
        assert_eq!(list.list(), Ok(vec![[2; 32]]));
        assert_eq!(list.contains(&[2; 32]), Ok(true));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
