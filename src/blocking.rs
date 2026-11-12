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
//! fingerprint, never on a prefix, rotating ID or name; a stream source known
//! only by its 128-bit node ID (the first 16 bytes of a fingerprint) is
//! matched by that node ID (Android `PersonPolicy.checkNode`), and a fixed
//! stream offer by its 48-bit prefix (Android `hideByPrefix`).
//!
//! Stored DPAPI-sealed at `identity-verification/blocked-callers.bin` (on the
//! wipe allowlist) as `BBL2`: each fingerprint with the time it was blocked,
//! at most 512 (Android `BlockedPeopleStore`). A `BBL1` list (fingerprints
//! only, before 2026-11) still reads with unknown times (0); the next change
//! writes `BBL2`. Local only: no byte on the wire changes.
use crate::sealed::{self, Dpapi, Sealer};
use std::path::{Path, PathBuf};

pub const FILE: &str = "identity-verification/blocked-callers.bin";
const SEAL_LABEL: &str = "MeshLive blocked callers";
const MAGIC_V1: &[u8; 4] = b"BBL1";
const MAGIC: &[u8; 4] = b"BBL2";
/// Android `BlockedPeopleStore.MAX_BLOCKED`.
pub const MAX_BLOCKED: usize = 512;
pub const DELAY_MS: std::ops::RangeInclusive<u64> = 3_000..=7_000;

/// One blocked person (Android `BlockedPerson`): the full fingerprint and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocked {
    pub fingerprint: [u8; 32],
    /// Milliseconds since 1970 (UTC) when it was blocked; 0 when unknown (a `BBL1` list).
    pub since_ms: i64,
}

/// What a block or unblock did (Android `BlockResult`, and Unblocked).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockChange {
    Blocked,
    AlreadyBlocked,
    Unblocked,
    /// 512 people are blocked already.
    Full,
    /// The list can't be read or written (an unreadable list is never overwritten).
    Failed,
}

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
    /// Everyone blocked, in the order they were blocked (Err: unreadable; the
    /// caller fails open and never overwrites it).
    pub fn entries(&self) -> Result<Vec<Blocked>, ()> {
        let Some(bytes) = sealed::read(&self.path, self.sealer.as_ref()).map_err(|_| ())? else {
            return Ok(Vec::new());
        };
        decode(&bytes)
    }
    /// The blocked fingerprints (Err: unreadable; the caller fails open).
    pub fn list(&self) -> Result<Vec<[u8; 32]>, ()> {
        Ok(self.entries()?.into_iter().map(|b| b.fingerprint).collect())
    }
    pub fn contains(&self, fingerprint: &[u8; 32]) -> Result<bool, ()> {
        Ok(self.list()?.contains(fingerprint))
    }
    /// Android `add`: blocks `fingerprint` as of `now_ms`.
    pub fn block(&self, fingerprint: &[u8; 32], now_ms: i64) -> BlockChange {
        let Ok(mut list) = self.entries() else { return BlockChange::Failed };
        if list.iter().any(|b| &b.fingerprint == fingerprint) {
            return BlockChange::AlreadyBlocked;
        }
        if list.len() >= MAX_BLOCKED {
            return BlockChange::Full;
        }
        list.push(Blocked { fingerprint: *fingerprint, since_ms: now_ms });
        if self.save(&list).is_ok() { BlockChange::Blocked } else { BlockChange::Failed }
    }
    /// Android `remove`: someone not on the list is unblocked already.
    pub fn unblock(&self, fingerprint: &[u8; 32]) -> BlockChange {
        let Ok(list) = self.entries() else { return BlockChange::Failed };
        if !list.iter().any(|b| &b.fingerprint == fingerprint) {
            return BlockChange::Unblocked;
        }
        let rest: Vec<Blocked> = list.into_iter().filter(|b| &b.fingerprint != fingerprint).collect();
        if self.save(&rest).is_ok() { BlockChange::Unblocked } else { BlockChange::Failed }
    }
    /// Blocks or unblocks (the test hooks); an unreadable list is never overwritten.
    pub fn set(&self, fingerprint: &[u8; 32], blocked: bool) -> Result<(), ()> {
        let change = if blocked { self.block(fingerprint, now_ms()) } else { self.unblock(fingerprint) };
        match change {
            BlockChange::Blocked | BlockChange::AlreadyBlocked | BlockChange::Unblocked => Ok(()),
            BlockChange::Full | BlockChange::Failed => Err(()),
        }
    }
    /// Android `reset` (Blocked people > Reset block list): deletes the whole
    /// list, also an unreadable one.
    pub fn reset(&self) -> Result<(), ()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(()),
        }
    }
    fn save(&self, list: &[Blocked]) -> Result<(), ()> {
        sealed::write(&self.path, self.sealer.as_ref(), &encode(list)).map_err(|_| ())
    }
}

fn encode(list: &[Blocked]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(list.len() as u16).to_be_bytes());
    for b in list {
        bytes.extend_from_slice(&b.fingerprint);
        bytes.extend_from_slice(&b.since_ms.to_be_bytes());
    }
    bytes
}

fn decode(bytes: &[u8]) -> Result<Vec<Blocked>, ()> {
    if let Some(body) = bytes.strip_prefix(MAGIC_V1.as_slice()) {
        if body.len() % 32 != 0 || body.len() / 32 > MAX_BLOCKED {
            return Err(());
        }
        return Ok(body.chunks_exact(32).map(|c| Blocked { fingerprint: c.try_into().unwrap(), since_ms: 0 }).collect());
    }
    let body = bytes.strip_prefix(MAGIC.as_slice()).ok_or(())?;
    let (count, rest) = body.split_first_chunk::<2>().ok_or(())?;
    let count = usize::from(u16::from_be_bytes(*count));
    if count > MAX_BLOCKED || rest.len() != count * 40 {
        return Err(());
    }
    Ok(rest
        .chunks_exact(40)
        .map(|c| Blocked { fingerprint: c[..32].try_into().unwrap(), since_ms: i64::from_be_bytes(c[32..].try_into().unwrap()) })
        .collect())
}

/// A stream source known by its node ID only (a forwarded stream's `BFG1`
/// source, or a direct one) is a blocked person (Android `containsNode`).
pub fn node_blocked(list: &[[u8; 32]], node: &[u8; 16]) -> bool {
    list.iter().any(|f| f[..16] == node[..])
}

/// A fixed (not rotating) stream offer's 48-bit prefix is a blocked person's
/// (Android `hideByPrefix` on `nodeIdPrefix`).
pub fn prefix_blocked(list: &[[u8; 32]], prefix: &[u8; 6]) -> bool {
    list.iter().any(|f| f[..6] == prefix[..])
}

/// Milliseconds since 1970 (UTC) now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `utc_ms` in this PC's time zone (as Windows applies it now).
pub fn local_ms(utc_ms: i64) -> i64 {
    use windows::Win32::{Foundation::FILETIME, Storage::FileSystem::FileTimeToLocalFileTime};
    // FILETIME counts 100 ns ticks since 1601-01-01.
    const FROM_1601_MS: i64 = 11_644_473_600_000;
    let ticks = (utc_ms.saturating_add(FROM_1601_MS).max(0) as u64).saturating_mul(10_000);
    let utc = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut local = FILETIME::default();
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.is_err() {
        return utc_ms;
    }
    let local_ticks = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
    (local_ticks / 10_000) as i64 - FROM_1601_MS
}

/// The day of `local_ms` as Android's medium date reads in English ("Nov 6, 2026").
pub fn date_text(local_ms: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let year_length = |y: i64| if leap(y) { 366 } else { 365 };
    let mut days = local_ms.div_euclid(86_400_000);
    let mut year = 1970i64;
    while days < 0 {
        year -= 1;
        days += year_length(year);
    }
    while days >= year_length(year) {
        days -= year_length(year);
        year += 1;
    }
    let lengths = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while days >= lengths[month] {
        days -= lengths[month];
        month += 1;
    }
    format!("{} {}, {year}", MONTHS[month], days + 1)
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

    /// Android `add`: a second block says already; the list keeps the order
    /// and the time of each block; unblocking someone not on it is fine.
    #[test]
    fn blocking_twice_says_already_and_the_list_keeps_order_and_time() {
        let dir = scratch("blocking-order");
        let list = BlockList::new(&dir, Box::new(Plain));
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_eq!(list.block(&a, 1_000), BlockChange::Blocked);
        assert_eq!(list.block(&a, 9_000), BlockChange::AlreadyBlocked);
        assert_eq!(list.block(&b, 2_000), BlockChange::Blocked);
        assert_eq!(list.entries(), Ok(vec![Blocked { fingerprint: a, since_ms: 1_000 }, Blocked { fingerprint: b, since_ms: 2_000 }]));
        assert_eq!(list.unblock(&a), BlockChange::Unblocked);
        assert_eq!(list.unblock(&a), BlockChange::Unblocked, "not on the list: unblocked already");
        assert_eq!(list.entries(), Ok(vec![Blocked { fingerprint: b, since_ms: 2_000 }]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A full list refuses a new block until someone is unblocked; an
    /// unreadable list fails every change, is never overwritten, and Reset
    /// deletes it so blocking works again.
    #[test]
    fn a_full_list_refuses_and_an_unreadable_one_is_kept_until_reset() {
        let dir = scratch("blocking-full");
        let list = BlockList::new(&dir, Box::new(Plain));
        let many: Vec<Blocked> = (0..MAX_BLOCKED).map(|i| {
            let mut fingerprint = [0u8; 32];
            fingerprint[..8].copy_from_slice(&(i as u64).to_be_bytes());
            Blocked { fingerprint, since_ms: 1 }
        }).collect();
        sealed::write(&dir.join(FILE), &Plain, &encode(&many)).unwrap();
        assert_eq!(list.block(&[0xee; 32], 5), BlockChange::Full);
        assert_eq!(list.unblock(&many[0].fingerprint), BlockChange::Unblocked);
        assert_eq!(list.block(&[0xee; 32], 5), BlockChange::Blocked);

        let path = dir.join(FILE);
        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(list.entries(), Err(()));
        assert_eq!(list.block(&[0xee; 32], 6), BlockChange::Failed);
        assert_eq!(list.unblock(&[0xee; 32]), BlockChange::Failed);
        assert_eq!(list.set(&[0xee; 32], true), Err(()));
        assert_eq!(std::fs::read(&path).unwrap(), b"junk", "never overwritten");
        assert_eq!(list.reset(), Ok(()));
        assert_eq!(list.entries(), Ok(vec![]));
        assert_eq!(list.reset(), Ok(()), "nothing to delete is fine");
        assert_eq!(list.block(&[0xee; 32], 7), BlockChange::Blocked);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A list written before 2026-11 (BBL1, fingerprints only) still blocks
    /// the same people; the next change keeps them and writes BBL2 with times.
    #[test]
    fn an_older_list_still_reads_and_the_next_change_keeps_everyone() {
        let dir = scratch("blocking-v1");
        let list = BlockList::new(&dir, Box::new(Plain));
        let (a, b) = ([3u8; 32], [4u8; 32]);
        sealed::write(&dir.join(FILE), &Plain, &[b"BBL1".as_slice(), &a, &b].concat()).unwrap();
        assert_eq!(list.contains(&a), Ok(true));
        assert_eq!(list.entries(), Ok(vec![Blocked { fingerprint: a, since_ms: 0 }, Blocked { fingerprint: b, since_ms: 0 }]));
        assert_eq!(list.block(&[5; 32], 9), BlockChange::Blocked);
        assert_eq!(list.list(), Ok(vec![a, b, [5; 32]]));
        let plain = sealed::read(&dir.join(FILE), &Plain).unwrap().unwrap();
        assert!(plain.starts_with(b"BBL2"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A stream source by its node ID (the first 16 bytes of the
    /// fingerprint) and a fixed offer by its 48-bit prefix; nothing matches an
    /// empty list.
    #[test]
    fn a_stream_source_matches_by_node_id_and_a_fixed_offer_by_its_prefix() {
        let list = [[9u8; 32]];
        assert!(node_blocked(&list, &[9; 16]));
        let mut near = [9u8; 16];
        near[15] = 8;
        assert!(!node_blocked(&list, &near));
        assert!(prefix_blocked(&list, &[9; 6]));
        assert!(!prefix_blocked(&list, &[9, 9, 9, 9, 9, 8]));
        assert!(!node_blocked(&[], &[9; 16]) && !prefix_blocked(&[], &[9; 6]));
    }

    /// Dates as Android's medium date reads them; this PC's zone moves a time
    /// by less than a day.
    #[test]
    fn dates_read_like_android_and_the_zone_moves_less_than_a_day() {
        assert_eq!(date_text(0), "Jan 1, 1970");
        assert_eq!(date_text(1_793_923_200_000 + 86_399_999), "Nov 6, 2026");
        assert_eq!(date_text(1_709_164_800_000), "Feb 29, 2024");
        assert_eq!(date_text(978_220_800_000), "Dec 31, 2000");
        assert_eq!(date_text(-1), "Dec 31, 1969");
        let t = 1_793_923_200_000;
        assert!((local_ms(t) - t).abs() <= 14 * 3_600_000);
    }
}
