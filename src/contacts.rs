//! Verified contacts, matching Android `VerifiedContactStore`
//! (IdentityVerification.kt): one sealed file `identity-verification/
//! verified-contacts.bin` in the app's private folder, plaintext
//! `"BVC1" | u16 count | count x (fingerprint[32] | i64 verifiedAtMs |
//! u8 method | u8 nameLen | name UTF-8)`, big-endian, no trailing bytes, at
//! most 1024 entries, one per fingerprint and one per 6-byte node prefix.
//! A contact is verified only when the confirmed fingerprint (QR or safety
//! code) equals the fingerprint the current end-to-end handshake proved.
//! An unreadable store is reported and never overwritten implicitly: a corrupt
//! one is latched until reset, a transient failure retries with backoff
//! (2 s doubling to 60 s). Nothing here is ever sent on the wire.
use crate::sealed::{self, Sealer};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_CONTACTS: usize = 1024;
pub const MAX_NAME_BYTES: usize = 64;
const MAGIC: &[u8; 4] = b"BVC1";
/// DPAPI entropy label for this store (never reused by another store).
pub const SEAL_LABEL: &str = "io.github.meshlive.app/store/verified-contacts/1";

/// SHA-256 of the identity's DER SubjectPublicKeyInfo; [0..16] is the node
/// ID and [0..6] the announcement prefix.
pub type Fingerprint = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    QrScan = 1,
    SafetyCode = 2,
}
impl Method {
    fn from_wire(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::QrScan),
            2 => Some(Self::SafetyCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedContact {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub verified_at_ms: i64,
    pub method: Method,
}
impl VerifiedContact {
    pub fn node_id(&self) -> [u8; 16] {
        self.fingerprint[..16].try_into().unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// The bytes opened but are not a valid store, or did not open at all.
    Corrupt,
    /// Reading or opening failed for a reason that may pass (I/O, sealing).
    Transient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Verified,
    NoAuthenticatedSession,
    FingerprintMismatch,
    /// Another verified contact has the same 6-byte prefix with another key:
    /// it must be forgotten first.
    PrefixConflict,
    Full,
    InvalidName,
    Unreadable(Unreadable),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rename {
    Renamed,
    NotFound,
    InvalidName,
    Unreadable(Unreadable),
}

/// Android IdentityQrCodec.normalizeName: NFC, not blank, at most 64 UTF-8
/// bytes, no control, format, private-use, surrogate, unassigned or
/// line/paragraph-separator characters.
pub fn normalize_name(name: &str) -> Option<String> {
    let normalized = crate::names::nfc(name)?;
    if normalized.trim().is_empty() || normalized.len() > MAX_NAME_BYTES {
        return None;
    }
    if normalized.chars().any(crate::names::rejected_type) {
        return None;
    }
    Some(normalized)
}

fn encode(contacts: &[VerifiedContact]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend((contacts.len() as u16).to_be_bytes());
    for c in contacts {
        out.extend(c.fingerprint);
        out.extend(c.verified_at_ms.to_be_bytes());
        out.push(c.method as u8);
        out.push(c.name.len() as u8);
        out.extend(c.name.as_bytes());
    }
    out
}
fn decode(mut b: &[u8]) -> Option<Vec<VerifiedContact>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (b.get(..n)?, b.get(n..)?);
        *b = rest;
        Some(head)
    }
    if take(&mut b, 4)? != MAGIC {
        return None;
    }
    let count = u16::from_be_bytes(take(&mut b, 2)?.try_into().ok()?) as usize;
    if count > MAX_CONTACTS {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let fingerprint: Fingerprint = take(&mut b, 32)?.try_into().ok()?;
        let verified_at_ms = i64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let method = Method::from_wire(take(&mut b, 1)?[0])?;
        let len = take(&mut b, 1)?[0] as usize;
        let name = String::from_utf8(take(&mut b, len)?.to_vec()).ok()?;
        out.push(VerifiedContact {
            fingerprint,
            name,
            verified_at_ms,
            method,
        });
    }
    b.is_empty().then_some(out)
}

pub struct ContactStore {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
    cache: Option<Vec<VerifiedContact>>,
    latched: bool,
    failures: u32,
    retry_at: Option<Instant>,
}
impl ContactStore {
    /// The sealed file exists (an empty list next to an existing file is not
    /// a consistent read, CF-PRES-09).
    pub fn file_exists(&self) -> bool {
        self.path.exists()
    }
    /// The store under `dir` (the app's private folder, `sealed::default_dir`).
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self {
            path: dir.join("identity-verification").join("verified-contacts.bin"),
            sealer,
            cache: None,
            latched: false,
            failures: 0,
            retry_at: None,
        }
    }
    /// DPAPI-sealed store in the app's folder.
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(sealed::Dpapi::new(SEAL_LABEL)))
    }
    fn load(&mut self) -> Result<&Vec<VerifiedContact>, Unreadable> {
        if self.cache.is_some() {
            return Ok(self.cache.as_ref().unwrap());
        }
        if self.latched {
            return Err(Unreadable::Corrupt);
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return Err(Unreadable::Transient);
        }
        match sealed::read(&self.path, self.sealer.as_ref()) {
            Ok(None) => self.cache = Some(Vec::new()),
            Ok(Some(plain)) => match decode(&plain) {
                Some(list) => self.cache = Some(list),
                None => {
                    self.latched = true;
                    return Err(Unreadable::Corrupt);
                }
            },
            // Bytes that do not open with this user's key are damaged.
            Err(sealed::Error::Unseal(_)) => {
                self.latched = true;
                return Err(Unreadable::Corrupt);
            }
            Err(_) => {
                self.failures += 1;
                let wait = (2u64 << (self.failures - 1).min(5)).min(60);
                self.retry_at = Some(Instant::now() + Duration::from_secs(wait));
                return Err(Unreadable::Transient);
            }
        }
        self.failures = 0;
        self.retry_at = None;
        Ok(self.cache.as_ref().unwrap())
    }
    fn save(&mut self, list: Vec<VerifiedContact>) -> Result<(), Unreadable> {
        sealed::write(&self.path, self.sealer.as_ref(), &encode(&list)).map_err(|_| Unreadable::Transient)?;
        self.cache = Some(list);
        Ok(())
    }
    /// Forgets the cached list: the next read sees changes another owner of
    /// the same file (the Bluetooth worker's scan) wrote.
    pub fn reload(&mut self) {
        self.cache = None;
    }
    /// All contacts in stored order; Err when the store cannot be read.
    pub fn list(&mut self) -> Result<Vec<VerifiedContact>, Unreadable> {
        self.load().cloned()
    }
    pub fn get(&mut self, fingerprint: &Fingerprint) -> Result<Option<VerifiedContact>, Unreadable> {
        Ok(self.load()?.iter().find(|c| &c.fingerprint == fingerprint).cloned())
    }
    /// The contact whose node ID (fingerprint[0..16]) this is.
    pub fn by_node_id(&mut self, node_id: &[u8; 16]) -> Result<Option<VerifiedContact>, Unreadable> {
        Ok(self.load()?.iter().find(|c| &c.node_id() == node_id).cloned())
    }
    /// Marks `confirmed` verified. `authenticated` is the fingerprint the
    /// current end-to-end handshake proved (None without a session). A blank
    /// name keeps the label the contact already has.
    pub fn mark_verified(
        &mut self,
        authenticated: Option<&Fingerprint>,
        confirmed: &Fingerprint,
        name: &str,
        method: Method,
        now_ms: i64,
    ) -> Verify {
        let Some(authenticated) = authenticated else {
            return Verify::NoAuthenticatedSession;
        };
        if authenticated != confirmed {
            return Verify::FingerprintMismatch;
        }
        let current = match self.load() {
            Ok(list) => list.clone(),
            Err(e) => return Verify::Unreadable(e),
        };
        let name = if name.trim().is_empty() {
            current
                .iter()
                .find(|c| &c.fingerprint == confirmed)
                .map(|c| c.name.clone())
                .unwrap_or_default()
        } else {
            match normalize_name(name) {
                Some(n) => n,
                None => return Verify::InvalidName,
            }
        };
        if current
            .iter()
            .any(|c| &c.fingerprint != confirmed && c.fingerprint[..6] == confirmed[..6])
        {
            return Verify::PrefixConflict;
        }
        let mut others: Vec<_> = current.into_iter().filter(|c| &c.fingerprint != confirmed).collect();
        if others.len() >= MAX_CONTACTS {
            return Verify::Full;
        }
        others.push(VerifiedContact {
            fingerprint: *confirmed,
            name,
            verified_at_ms: now_ms,
            method,
        });
        match self.save(others) {
            Ok(()) => Verify::Verified,
            Err(e) => Verify::Unreadable(e),
        }
    }
    /// A new label; the fingerprint, time and method stay. Blank is invalid
    /// (to drop a name, forget the contact and verify again).
    pub fn rename(&mut self, fingerprint: &Fingerprint, name: &str) -> Rename {
        // A canonical person name (LabelText PERSON_NAME).
        let Ok(name) = crate::names::normalize(crate::names::LabelKind::PersonName, name) else {
            return Rename::InvalidName;
        };
        let current = match self.load() {
            Ok(list) => list.clone(),
            Err(e) => return Rename::Unreadable(e),
        };
        if !current.iter().any(|c| &c.fingerprint == fingerprint) {
            return Rename::NotFound;
        }
        let list = current
            .into_iter()
            .map(|mut c| {
                if &c.fingerprint == fingerprint {
                    c.name = name.clone();
                }
                c
            })
            .collect();
        match self.save(list) {
            Ok(()) => Rename::Renamed,
            Err(e) => Rename::Unreadable(e),
        }
    }
    /// Removes one contact; false when absent or the store is unreadable.
    pub fn forget(&mut self, fingerprint: &Fingerprint) -> bool {
        let Ok(current) = self.load().cloned() else {
            return false;
        };
        let remaining: Vec<_> = current.iter().filter(|c| &c.fingerprint != fingerprint).cloned().collect();
        if remaining.len() == current.len() {
            return false;
        }
        self.save(remaining).is_ok()
    }
    /// Deletes the store (also the only way out of a latched corrupt store).
    pub fn reset(&mut self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.cache = Some(Vec::new());
        self.latched = false;
        self.failures = 0;
        self.retry_at = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::tests::{Plain, scratch};

    fn fp(first: u8, rest: u8) -> Fingerprint {
        let mut f = [rest; 32];
        f[0] = first;
        f
    }

    #[test]
    fn verification_needs_the_authenticated_fingerprint_and_survives_reopen() {
        let dir = scratch("contacts");
        let mut store = ContactStore::new(&dir, Box::new(Plain));
        let a = fp(1, 0xa1);
        assert_eq!(store.mark_verified(None, &a, "Priya", Method::QrScan, 5), Verify::NoAuthenticatedSession);
        assert_eq!(
            store.mark_verified(Some(&fp(2, 0)), &a, "Priya", Method::QrScan, 5),
            Verify::FingerprintMismatch
        );
        assert_eq!(store.mark_verified(Some(&a), &a, "Priya", Method::QrScan, 5), Verify::Verified);
        // Re-verify with a blank name keeps the label; time and method update.
        assert_eq!(store.mark_verified(Some(&a), &a, " ", Method::SafetyCode, 9), Verify::Verified);
        let mut reopened = ContactStore::new(&dir, Box::new(Plain));
        let list = reopened.list().unwrap();
        assert_eq!(
            list,
            vec![VerifiedContact {
                fingerprint: a,
                name: "Priya".into(),
                verified_at_ms: 9,
                method: Method::SafetyCode
            }]
        );
        assert_eq!(reopened.by_node_id(&a[..16].try_into().unwrap()).unwrap().unwrap().name, "Priya");
        // Same 6-byte prefix, different key: refused until the old one is forgotten.
        let mut b = a;
        b[31] ^= 1;
        assert_eq!(reopened.mark_verified(Some(&b), &b, "Imposter", Method::QrScan, 10), Verify::PrefixConflict);
        assert!(reopened.forget(&a));
        assert!(!reopened.forget(&a));
        assert_eq!(reopened.mark_verified(Some(&b), &b, "New phone", Method::QrScan, 11), Verify::Verified);
        assert_eq!(reopened.rename(&b, "Priya"), Rename::Renamed);
        assert_eq!(reopened.rename(&b, ""), Rename::InvalidName);
        assert_eq!(reopened.rename(&a, "X"), Rename::NotFound);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_format_is_android_bvc1_and_damage_latches_until_reset() {
        let c = VerifiedContact {
            fingerprint: fp(7, 7),
            name: "Ana".into(),
            verified_at_ms: 0x0102030405060708,
            method: Method::QrScan,
        };
        let bytes = encode(std::slice::from_ref(&c));
        let mut expected = b"BVC1\x00\x01".to_vec();
        expected.extend(fp(7, 7));
        expected.extend([1, 2, 3, 4, 5, 6, 7, 8, 1, 3]);
        expected.extend(b"Ana");
        assert_eq!(bytes, expected);
        assert_eq!(decode(&bytes), Some(vec![c]));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), None);
        let mut bad_method = bytes.clone();
        bad_method[4 + 2 + 32 + 8] = 3;
        assert_eq!(decode(&bad_method), None);

        let dir = scratch("contacts-corrupt");
        let path = dir.join("identity-verification").join("verified-contacts.bin");
        crate::sealed::write(&path, &Plain, b"BVC1\x00\x05").unwrap();
        let mut store = ContactStore::new(&dir, Box::new(Plain));
        assert_eq!(store.list(), Err(Unreadable::Corrupt));
        let a = fp(1, 1);
        assert_eq!(
            store.mark_verified(Some(&a), &a, "Priya", Method::QrScan, 1),
            Verify::Unreadable(Unreadable::Corrupt)
        );
        // Never overwritten implicitly.
        assert_eq!(crate::sealed::read(&path, &Plain).unwrap().unwrap(), b"BVC1\x00\x05");
        store.reset().unwrap();
        assert_eq!(store.list(), Ok(vec![]));
        assert_eq!(store.mark_verified(Some(&a), &a, "Priya", Method::QrScan, 1), Verify::Verified);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_follow_the_android_rules() {
        assert_eq!(normalize_name("Cafe\u{301}").as_deref(), Some("Caf\u{e9}"));
        assert_eq!(normalize_name("  "), None);
        assert_eq!(normalize_name("a\u{202e}b"), None);
        assert_eq!(normalize_name("a\u{7}b"), None);
        assert_eq!(normalize_name("\u{e000}"), None);
        assert_eq!(normalize_name("a\u{378}"), None);
        assert_eq!(normalize_name(&"x".repeat(64)).map(|s| s.len()), Some(64));
        assert_eq!(normalize_name(&"x".repeat(65)), None);
    }

    #[test]
    fn a_dpapi_sealed_store_reads_back() {
        let dir = scratch("contacts-dpapi");
        let a = fp(9, 9);
        let mut store = ContactStore::new(&dir, Box::new(sealed::Dpapi::new("io.github.meshlive.app/test/contacts/1")));
        assert_eq!(store.mark_verified(Some(&a), &a, "Ana", Method::QrScan, 3), Verify::Verified);
        let raw = std::fs::read(dir.join("identity-verification").join("verified-contacts.bin")).unwrap();
        assert!(!raw.windows(4).any(|w| w == MAGIC));
        let mut reopened = ContactStore::new(&dir, Box::new(sealed::Dpapi::new("io.github.meshlive.app/test/contacts/1")));
        assert_eq!(reopened.list().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
