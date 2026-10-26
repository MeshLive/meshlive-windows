//! Contact claims (Android `ContactClaimsStore`, PROTOCOL "Local stores
//! added"): `identity-verification/contact-claims.bin`, DPAPI-sealed, plain
//! `"BCC1" | u16 count | count x (fingerprint (32) | i64 at ms | u8 flags
//! (bit 0 scanned both ways) | u8 len | claimed name | u8 len | acknowledged
//! name)`, at most 1024 entries, names at most 32 bytes, no trailing bytes.
//! Separate from the verified contacts, so losing it loses only the note and
//! the "verified each other" fact, never a contact or its shield. Never
//! overwritten while unreadable; Forget and Reset delete entries.
//!
//! "Scanned both ways" on Windows: Android records it when its own scan of
//! the peer's code and the peer's scan of its code happened in one visit.
//! This PC cannot be scanned yet (it cannot advertise), so it records the
//! fact when its own scan stored the shield and the same person's presence
//! lease (or a scan claim from them) arrived: a lease is sent only to a
//! contact that peer verified itself, so both sides verified each other.
use crate::{
    contacts::Fingerprint,
    sealed::{self, Sealer},
};
use std::path::{Path, PathBuf};

pub const MAX_CLAIMS: usize = 1024;
pub const MAX_NAME_BYTES: usize = 32;
const FLAG_BOTH_WAYS: u8 = 1;
const MAGIC: &[u8; 4] = b"BCC1";
/// DPAPI entropy label of this store.
pub const SEAL_LABEL: &str = "io.github.meshlive.app/store/contact-claims/1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub fingerprint: Fingerprint,
    pub claimed_name: String,
    pub claimed_at_ms: i64,
    pub acknowledged_name: String,
    pub scanned_both_ways: bool,
}

fn encode(claims: &[Claim]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend((claims.len() as u16).to_be_bytes());
    for c in claims {
        out.extend(c.fingerprint);
        out.extend(c.claimed_at_ms.to_be_bytes());
        out.push(if c.scanned_both_ways { FLAG_BOTH_WAYS } else { 0 });
        for name in [&c.claimed_name, &c.acknowledged_name] {
            out.push(name.len() as u8);
            out.extend(name.as_bytes());
        }
    }
    out
}
fn decode(mut b: &[u8]) -> Option<Vec<Claim>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (b.get(..n)?, b.get(n..)?);
        *b = rest;
        Some(head)
    }
    if take(&mut b, 4)? != MAGIC {
        return None;
    }
    let count = u16::from_be_bytes(take(&mut b, 2)?.try_into().ok()?) as usize;
    if count > MAX_CLAIMS {
        return None;
    }
    let mut claims = Vec::with_capacity(count);
    for _ in 0..count {
        let fingerprint: Fingerprint = take(&mut b, 32)?.try_into().ok()?;
        let at = i64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let flags = take(&mut b, 1)?[0];
        let mut name = || -> Option<String> {
            let n = take(&mut b, 1)?[0] as usize;
            if n > MAX_NAME_BYTES {
                return None;
            }
            String::from_utf8(take(&mut b, n)?.to_vec()).ok()
        };
        let (claimed, acknowledged) = (name()?, name()?);
        claims.push(Claim {
            fingerprint,
            claimed_name: claimed,
            claimed_at_ms: at,
            acknowledged_name: acknowledged,
            scanned_both_ways: flags & FLAG_BOTH_WAYS != 0,
        });
    }
    b.is_empty().then_some(claims)
}

pub struct ClaimsStore {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
    cache: Option<Vec<Claim>>,
    latched: bool,
}
impl ClaimsStore {
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self {
            path: dir.join("identity-verification").join("contact-claims.bin"),
            sealer,
            cache: None,
            latched: false,
        }
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(sealed::Dpapi::new(SEAL_LABEL)))
    }
    fn load(&mut self) -> Option<&Vec<Claim>> {
        if self.cache.is_none() && !self.latched {
            match sealed::read(&self.path, self.sealer.as_ref()) {
                Ok(None) => self.cache = Some(Vec::new()),
                Ok(Some(plain)) => match decode(&plain) {
                    Some(list) => self.cache = Some(list),
                    None => self.latched = true,
                },
                Err(_) => self.latched = true,
            }
        }
        self.cache.as_ref()
    }
    fn save(&mut self, claims: Vec<Claim>) -> bool {
        if sealed::write(&self.path, self.sealer.as_ref(), &encode(&claims)).is_err() {
            return false;
        }
        self.cache = Some(claims);
        true
    }
    pub fn is_unreadable(&mut self) -> bool {
        self.load().is_none()
    }
    pub fn get(&mut self, fingerprint: &Fingerprint) -> Option<Claim> {
        self.load()?.iter().find(|c| &c.fingerprint == fingerprint).cloned()
    }
    pub fn scanned_both_ways(&mut self, fingerprint: &Fingerprint) -> bool {
        self.get(fingerprint).is_some_and(|c| c.scanned_both_ways)
    }
    /// Records "scanned both ways" (an entry without a claimed name when needed).
    pub fn mark_scanned_both_ways(&mut self, fingerprint: &Fingerprint, now_ms: i64) -> bool {
        let Some(current) = self.load().cloned() else { return false };
        match current.iter().find(|c| &c.fingerprint == fingerprint) {
            Some(c) if c.scanned_both_ways => true,
            Some(_) => self.save(
                current
                    .into_iter()
                    .map(|mut c| {
                        if &c.fingerprint == fingerprint {
                            c.scanned_both_ways = true;
                        }
                        c
                    })
                    .collect(),
            ),
            None if current.len() >= MAX_CLAIMS => false,
            None => {
                let mut next = current;
                next.push(Claim {
                    fingerprint: *fingerprint,
                    claimed_name: String::new(),
                    claimed_at_ms: now_ms,
                    acknowledged_name: String::new(),
                    scanned_both_ways: true,
                });
                self.save(next)
            }
        }
    }
    /// Forget deletes this contact's entry.
    pub fn forget(&mut self, fingerprint: &Fingerprint) -> bool {
        let Some(current) = self.load().cloned() else { return false };
        let remaining: Vec<Claim> = current.iter().filter(|c| &c.fingerprint != fingerprint).cloned().collect();
        if remaining.len() == current.len() {
            return false;
        }
        if remaining.is_empty() {
            return self.reset();
        }
        self.save(remaining)
    }
    /// Deletes the file (also the recovery from an unreadable one).
    pub fn reset(&mut self) -> bool {
        let removed = match std::fs::remove_file(&self.path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        if removed {
            self.cache = Some(Vec::new());
            self.latched = false;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::tests::{Plain, scratch};

    #[test]
    fn scanned_both_ways_is_recorded_kept_and_forgotten_and_a_damaged_file_is_never_overwritten() {
        let dir = scratch("claims");
        let mut store = ClaimsStore::new(&dir, Box::new(Plain));
        let (a, b) = ([0xa1; 32], [0xb2; 32]);
        assert!(!store.scanned_both_ways(&a));
        assert!(store.mark_scanned_both_ways(&a, 5));
        assert!(store.mark_scanned_both_ways(&a, 6), "idempotent");
        assert!(store.mark_scanned_both_ways(&b, 7));
        let mut reopened = ClaimsStore::new(&dir, Box::new(Plain));
        assert!(reopened.scanned_both_ways(&a) && reopened.scanned_both_ways(&b));
        let claim = reopened.get(&a).unwrap();
        assert_eq!((claim.claimed_name.as_str(), claim.claimed_at_ms), ("", 5));
        assert!(reopened.forget(&a));
        assert!(!reopened.scanned_both_ways(&a) && reopened.scanned_both_ways(&b));
        // Codec: exact, strict.
        let bytes = encode(&[claim.clone()]);
        assert_eq!(&bytes[..6], b"BCC1\x00\x01");
        assert_eq!(decode(&bytes), Some(vec![claim]));
        assert!(decode(&[bytes.as_slice(), &[0]].concat()).is_none());
        // A damaged file latches and is not overwritten.
        let path = dir.join("identity-verification").join("contact-claims.bin");
        std::fs::write(&path, b"damaged").unwrap();
        let mut damaged = ClaimsStore::new(&dir, Box::new(Plain));
        assert!(damaged.is_unreadable());
        assert!(!damaged.mark_scanned_both_ways(&a, 9));
        assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
        assert!(damaged.reset() && !damaged.is_unreadable());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
