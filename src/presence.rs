//! Presence leases (Design QL / rotating IDs phase 2), ported from Android
//! `PresenceLeases.kt`, `PresenceStore.kt` and PROTOCOL "Keys and epochs":
//! this PC's day keys `L_d` (independent random keys, window
//! `[dayOf(E-1), dayOf(E)+27]`), the BDX1 OFFER / PRESENCE_LEASE records, the
//! leases received from contacts (receiver rules, pruning, skew), the
//! recognition table that maps a contact's rotating ID back to the contact,
//! and the sealed `BPS1` version 2 store.
use crate::{
    crypto,
    rotating::{self, ROLE_CALL_OFFER, ROLE_STREAM_OFFER},
    sealed::{self, Sealer},
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

pub const EPOCHS_PER_DAY: u32 = 96;
pub const DAYS_AHEAD: u32 = 27;
pub const DAYS_PER_DELIVERY: u32 = 28;
pub const MAX_KEYS: usize = 64;
pub const MAX_DAYS_KEPT: usize = 32;
pub const MAX_SKEW_EPOCHS: i64 = 30 * EPOCHS_PER_DAY as i64;
pub const STALE_SKEW_MS: i64 = 14 * 24 * 3_600_000;
pub const NARROW_RADIUS: i64 = 1;
pub const WIDE_RADIUS: i64 = 8;
pub const MAX_CONTACTS: usize = 1024;
pub const LEASE_DOMAIN: &[u8] = b"BluetoothVideoMesh/Discovery/Lease/2\0";
/// DPAPI entropy label of the presence store.
pub const SEAL_LABEL: &str = "io.github.meshlive.app/store/presence-leases/2";

pub type Key = [u8; 32];

/// The day of an epoch (900 s epochs: `epoch / 96`).
pub fn day_of(epoch: u32) -> u32 {
    epoch / EPOCHS_PER_DAY
}

/// MIGRATION ONLY: the earlier `L_d = HMAC(P, "…/Lease/2\0" || u32 d)`.
pub fn legacy_lease_key(presence_key: &Key, day: u32) -> Result<Key, crypto::Error> {
    let mac = crypto::hmac_sha256(presence_key, &[LEASE_DOMAIN, &day.to_be_bytes()].concat())?;
    Ok(*mac)
}

// ---------------------------------------------------------------- own day keys

pub fn first_day(epoch: u32) -> u32 {
    day_of(epoch.saturating_sub(1))
}
pub fn last_day(epoch: u32) -> u32 {
    day_of(epoch) + DAYS_AHEAD
}
/// Android DayKeyWindow.roll: days before the window deleted, every missing
/// day from today to the last day created; existing keys never replaced.
/// CF-PRES-02: day keys are zeroed when dropped, revoked or rolled out.
fn wipe_keys<'a>(keys: impl IntoIterator<Item = &'a mut Key>) {
    use zeroize::Zeroize;
    for key in keys {
        key.zeroize();
    }
}
/// Replaces a key window, zeroing the old one first.
fn replace_keys(slot: &mut BTreeMap<u32, Key>, fresh: BTreeMap<u32, Key>) {
    wipe_keys(slot.values_mut());
    *slot = fresh;
}

pub fn roll(keys: &BTreeMap<u32, Key>, epoch: u32, new_key: &mut dyn FnMut() -> Key) -> BTreeMap<u32, Key> {
    let mut next: BTreeMap<u32, Key> = keys.range(first_day(epoch)..).map(|(d, k)| (*d, *k)).collect();
    for day in day_of(epoch)..=last_day(epoch) {
        next.entry(day).or_insert_with(&mut *new_key);
    }
    next
}
pub fn random_key() -> Key {
    let mut key = [0u8; 32];
    key.copy_from_slice(&crypto::random_nonce().expect("CSPRNG")[..32]);
    key
}

/// This PC's own role-`role` rotating ID at `epoch` under its day key.
pub fn own_rdi(keys: &BTreeMap<u32, Key>, node_id: &[u8; 16], epoch: u32, role: u8) -> Option<[u8; 6]> {
    let key = keys.get(&day_of(epoch))?;
    let salt = rotating::salt(key, epoch, role).ok()?;
    rotating::rdi(node_id, epoch, role, &salt).ok()
}

// ---------------------------------------------------------------- BDX1 OFFER / LEASE

pub const BDX1: &[u8; 4] = b"BDX1";
pub const KIND_OFFER: u8 = 2;
pub const KIND_LEASE: u8 = 3;
pub const LEASE_BYTES_FULL: usize = 914;

pub fn offer() -> [u8; 6] {
    [b'B', b'D', b'X', b'1', KIND_OFFER, 0]
}
pub fn is_offer(bytes: &[u8]) -> bool {
    bytes == offer()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub generation: u32,
    pub sender_epoch: u32,
    pub first_day: u32,
    pub keys: Vec<Key>,
}
impl Lease {
    /// Today and the next 27 days, or None when a day is missing.
    pub fn delivery(keys: &BTreeMap<u32, Key>, generation: u32, epoch: u32) -> Option<Self> {
        let first = day_of(epoch);
        let list: Option<Vec<Key>> = (0..DAYS_PER_DELIVERY).map(|i| keys.get(&(first + i)).copied()).collect();
        Some(Self {
            generation,
            sender_epoch: epoch,
            first_day: first,
            keys: list?,
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = BDX1.to_vec();
        out.push(KIND_LEASE);
        out.extend(self.generation.to_be_bytes());
        out.extend(self.sender_epoch.to_be_bytes());
        out.extend(self.first_day.to_be_bytes());
        out.push(self.keys.len() as u8);
        for key in &self.keys {
            out.extend(key);
        }
        out
    }
    /// Strict: exact length, generation >= 1, 1..28 keys.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 18 || &bytes[..4] != BDX1 || bytes[4] != KIND_LEASE {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
        let (generation, sender_epoch, first_day, count) = (u32_at(5), u32_at(9), u32_at(13), bytes[17] as usize);
        if generation == 0 || !(1..=DAYS_PER_DELIVERY as usize).contains(&count) || bytes.len() != 18 + 32 * count {
            return None;
        }
        let keys = bytes[18..].chunks(32).map(|c| c.try_into().unwrap()).collect();
        Some(Self {
            generation,
            sender_epoch,
            first_day,
            keys,
        })
    }
}

// ---------------------------------------------------------------- received leases

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub generation: u32,
    pub days: BTreeMap<u32, Key>,
    /// `sender epoch - my epoch` at receipt, clamped to +-30 days.
    pub skew: i64,
    pub received_at_ms: i64,
}
impl Drop for Received {
    fn drop(&mut self) {
        wipe_keys(self.days.values_mut());
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        wipe_keys(self.keys.iter_mut());
    }
}
impl Received {
    /// Android ContactPresence.receive: higher generation replaces, equal
    /// merges (dropping days before the new first day; replaces if a shared
    /// day differs), lower is ignored.
    pub fn receive(stored: Option<&Self>, lease: &Lease, my_epoch: u32, now_ms: i64) -> Self {
        if let Some(stored) = stored {
            if lease.generation < stored.generation {
                return stored.clone();
            }
        }
        let delivered: BTreeMap<u32, Key> =
            lease.keys.iter().enumerate().map(|(i, k)| (lease.first_day + i as u32, *k)).collect();
        let same_key = stored.is_some_and(|s| {
            s.generation == lease.generation && delivered.iter().all(|(d, k)| s.days.get(d).is_none_or(|old| old == k))
        });
        let days = match stored {
            Some(s) if same_key => {
                let mut merged: BTreeMap<u32, Key> = s.days.range(lease.first_day..).map(|(d, k)| (*d, *k)).collect();
                merged.extend(delivered);
                while merged.len() > MAX_DAYS_KEPT {
                    let first = *merged.keys().next().unwrap();
                    merged.remove(&first);
                }
                merged
            }
            _ => delivered,
        };
        let skew = (i64::from(lease.sender_epoch) - i64::from(my_epoch)).clamp(-MAX_SKEW_EPOCHS, MAX_SKEW_EPOCHS);
        Self {
            generation: lease.generation,
            days,
            skew,
            received_at_ms: now_ms,
        }
    }
    /// Days before `dayOf(myEpoch + skew) - 1` are deleted; None when nothing is left.
    pub fn pruned(&self, my_epoch: u32) -> Option<Self> {
        let centre = (i64::from(my_epoch) + self.skew).max(0) as u32;
        let keep_from = day_of(centre).saturating_sub(1);
        let days: BTreeMap<u32, Key> = self.days.range(keep_from..).map(|(d, k)| (*d, *k)).collect();
        (!days.is_empty()).then(|| Self { days, ..self.clone() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub contact: [u8; 16],
    pub epoch: u32,
    pub role: u8,
}

/// RDI -> contacts, for roles 0 and 1 around each contact's learned epoch.
#[derive(Default)]
pub struct RecognitionTable {
    table: HashMap<[u8; 6], Vec<Hit>>,
}
impl RecognitionTable {
    pub fn build<'a>(contacts: impl IntoIterator<Item = (&'a [u8; 16], &'a Received)>, my_epoch: u32, now_ms: i64) -> Self {
        let mut table: HashMap<[u8; 6], Vec<Hit>> = HashMap::new();
        for (node, lease) in contacts {
            let radius = if now_ms - lease.received_at_ms > STALE_SKEW_MS { WIDE_RADIUS } else { NARROW_RADIUS };
            let centre = i64::from(my_epoch) + lease.skew;
            for epoch in (centre - radius)..=(centre + radius) {
                let Ok(epoch) = u32::try_from(epoch) else { continue };
                let Some(key) = lease.days.get(&day_of(epoch)) else { continue };
                for role in [ROLE_CALL_OFFER, ROLE_STREAM_OFFER] {
                    let Ok(salt) = rotating::salt(key, epoch, role) else { continue };
                    let Ok(rdi) = rotating::rdi(node, epoch, role, &salt) else { continue };
                    table.entry(rdi).or_default().push(Hit { contact: *node, epoch, role });
                }
            }
        }
        Self { table }
    }
    pub fn len(&self) -> usize {
        self.table.values().map(Vec::len).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
    pub fn lookup(&self, rdi: &[u8; 6]) -> &[Hit] {
        self.table.get(rdi).map_or(&[], Vec::as_slice)
    }
}

// ---------------------------------------------------------------- state and BPS1 store

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactState {
    pub node_id: [u8; 16],
    /// "Can see when I'm nearby" (default on).
    pub visible_to_them: bool,
    /// The generation last delivered to this contact (0: never).
    pub delivered_generation: u32,
    pub lease: Option<Received>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub generation: u32,
    pub keys: BTreeMap<u32, Key>,
    pub contacts: Vec<ContactState>,
}
impl Drop for State {
    fn drop(&mut self) {
        wipe_keys(self.keys.values_mut());
    }
}
impl State {
    pub fn fresh(epoch: u32) -> Self {
        Self::fresh_at(1, epoch)
    }
    pub fn fresh_at(generation: u32, epoch: u32) -> Self {
        Self {
            generation: generation.max(1),
            keys: roll(&BTreeMap::new(), epoch, &mut random_key),
            contacts: Vec::new(),
        }
    }
    /// A whole new window under the next generation (forget after delivery,
    /// switch off after delivery, reset).
    pub fn new_generation(&mut self, epoch: u32) {
        self.generation = self.generation.wrapping_add(1).max(1);
        replace_keys(&mut self.keys, roll(&BTreeMap::new(), epoch, &mut random_key));
    }
    pub fn contact_mut(&mut self, node_id: &[u8; 16]) -> &mut ContactState {
        if let Some(i) = self.contacts.iter().position(|c| &c.node_id == node_id) {
            return &mut self.contacts[i];
        }
        self.contacts.push(ContactState {
            node_id: *node_id,
            visible_to_them: true,
            delivered_generation: 0,
            lease: None,
        });
        self.contacts.last_mut().unwrap()
    }
    /// Forgetting a contact: its record goes; if it ever got a lease, the
    /// window is replaced so the old leases stop matching (PROTOCOL QL).
    pub fn forget(&mut self, node_id: &[u8; 16], epoch: u32) {
        let delivered = self.contacts.iter().any(|c| &c.node_id == node_id && c.delivered_generation != 0);
        self.contacts.retain(|c| &c.node_id != node_id);
        if delivered {
            self.new_generation(epoch);
        }
    }
    pub fn recognition(&self, my_epoch: u32, now_ms: i64) -> RecognitionTable {
        RecognitionTable::build(
            self.contacts.iter().filter_map(|c| c.lease.as_ref().map(|l| (&c.node_id, l))),
            my_epoch,
            now_ms,
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"BPS1".to_vec();
        out.push(2);
        out.extend(self.generation.to_be_bytes());
        out.push(self.keys.len() as u8);
        for (day, key) in &self.keys {
            out.extend(day.to_be_bytes());
            out.extend(key);
        }
        out.extend((self.contacts.len() as u16).to_be_bytes());
        for c in &self.contacts {
            out.extend(c.node_id);
            out.push(u8::from(c.visible_to_them));
            out.extend(c.delivered_generation.to_be_bytes());
            match &c.lease {
                None => out.extend(0u32.to_be_bytes()),
                Some(l) => {
                    out.extend(l.generation.to_be_bytes());
                    out.extend((l.skew as i32).to_be_bytes());
                    out.extend(l.received_at_ms.to_be_bytes());
                    out.push(l.days.len() as u8);
                    for (day, key) in &l.days {
                        out.extend(day.to_be_bytes());
                        out.extend(key);
                    }
                }
            }
        }
        out
    }
    pub fn decode(mut b: &[u8]) -> Option<Self> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            let (head, rest) = (b.get(..n)?, b.get(n..)?);
            *b = rest;
            Some(head)
        }
        fn u32_(b: &mut &[u8]) -> Option<u32> {
            Some(u32::from_be_bytes(take(b, 4)?.try_into().ok()?))
        }
        if take(&mut b, 4)? != b"BPS1" || take(&mut b, 1)?[0] != 2 {
            return None;
        }
        let generation = u32_(&mut b)?;
        let count = take(&mut b, 1)?[0] as usize;
        if !(1..=MAX_KEYS).contains(&count) {
            return None;
        }
        let mut keys = BTreeMap::new();
        for _ in 0..count {
            let day = u32_(&mut b)?;
            if keys.insert(day, take(&mut b, 32)?.try_into().ok()?).is_some() {
                return None;
            }
        }
        let contacts_count = u16::from_be_bytes(take(&mut b, 2)?.try_into().ok()?) as usize;
        if contacts_count > MAX_CONTACTS {
            return None;
        }
        let mut contacts: Vec<ContactState> = Vec::with_capacity(contacts_count);
        for _ in 0..contacts_count {
            let node_id: [u8; 16] = take(&mut b, 16)?.try_into().ok()?;
            let flags = take(&mut b, 1)?[0];
            if flags & !1 != 0 || contacts.iter().any(|c| c.node_id == node_id) {
                return None;
            }
            let delivered_generation = u32_(&mut b)?;
            let lease_generation = u32_(&mut b)?;
            let lease = if lease_generation == 0 {
                None
            } else {
                let skew = i64::from(i32::from_be_bytes(take(&mut b, 4)?.try_into().ok()?));
                let received_at_ms = i64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
                let n = take(&mut b, 1)?[0] as usize;
                if !(1..=MAX_DAYS_KEPT).contains(&n) {
                    return None;
                }
                let mut days = BTreeMap::new();
                for _ in 0..n {
                    let day = u32_(&mut b)?;
                    if days.insert(day, take(&mut b, 32)?.try_into().ok()?).is_some() {
                        return None;
                    }
                }
                Some(Received {
                    generation: lease_generation,
                    days,
                    skew,
                    received_at_ms,
                })
            };
            contacts.push(ContactState {
                node_id,
                visible_to_them: flags & 1 != 0,
                delivered_generation,
                lease,
            });
        }
        b.is_empty().then_some(Self { generation, keys, contacts })
    }
}

/// The sealed file `identity-verification/presence-leases.bin`.
pub struct PresenceStore {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
}
impl PresenceStore {
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self {
            path: dir.join("identity-verification").join("presence-leases.bin"),
            sealer,
        }
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(sealed::Dpapi::new(SEAL_LABEL)))
    }
    /// Ok(None) when there is no store yet; Err when it cannot be read (never overwritten implicitly).
    pub fn load(&self) -> Result<Option<State>, sealed::Error> {
        match sealed::read(&self.path, self.sealer.as_ref())? {
            None => Ok(None),
            Some(plain) => State::decode(&plain).map(Some).ok_or(sealed::Error::Unseal(0)),
        }
    }
    pub fn save(&self, state: &State) -> Result<(), sealed::Error> {
        sealed::write(&self.path, self.sealer.as_ref(), &state.encode())
    }
    /// Deletes the file ("Reset presence key" of a damaged store; absent is fine).
    pub fn remove(&self) -> Result<(), sealed::Error> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
    /// Loads (or creates) the state and rolls its window to `epoch`, saving
    /// any new day key before it can be delivered.
    pub fn current(&self, epoch: u32) -> Result<State, sealed::Error> {
        let mut state = match self.load()? {
            Some(s) => s,
            None => State::fresh(epoch),
        };
        let rolled = roll(&state.keys, epoch, &mut random_key);
        let changed = rolled != state.keys;
        replace_keys(&mut state.keys, rolled);
        let mut pruned = false;
        for c in &mut state.contacts {
            if let Some(lease) = &c.lease {
                let next = lease.pruned(epoch);
                if next.as_ref() != Some(lease) {
                    c.lease = next;
                    pruned = true;
                }
            }
        }
        if changed || pruned || !self.path.exists() {
            self.save(&state)?;
        }
        Ok(state)
    }
}

// ---------------------------------------------------------------- generation floor

/// The highest presence generation ever used on this PC (Android keeps it in
/// preferences): it survives a lost, unreadable or reset store, so a restored
/// older store can never bring revoked keys back.
pub trait GenerationFloor: Send {
    fn get(&self) -> u32;
    fn raise_to(&mut self, value: u32);
}
/// A 4-byte big-endian file next to the store, replaced atomically. It holds
/// a counter, not a secret, so it is not sealed. An unreadable file counts as
/// 0 (the store's own generation then rules); a failed write is retried at
/// the next raise.
pub struct FileFloor {
    path: PathBuf,
    value: u32,
}
impl FileFloor {
    pub fn new(dir: &Path) -> Self {
        let path = dir.join("identity-verification").join("presence-generation-floor.bin");
        let value = std::fs::read(&path)
            .ok()
            .and_then(|b| <[u8; 4]>::try_from(b.as_slice()).ok())
            .map_or(0, u32::from_be_bytes);
        Self { path, value }
    }
}
impl GenerationFloor for FileFloor {
    fn get(&self) -> u32 {
        self.value
    }
    fn raise_to(&mut self, value: u32) {
        if value <= self.value {
            return;
        }
        self.value = value;
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut temporary = self.path.as_os_str().to_owned();
            temporary.push(".tmp");
            // CF-PRES-06: the floor is on disk before it is relied on.
            {
                use std::io::Write;
                let mut file = std::fs::File::create(&temporary)?;
                file.write_all(&value.to_be_bytes())?;
                file.sync_all()?;
            }
            std::fs::rename(&temporary, &self.path)
        };
        let _ = write();
    }
}
#[derive(Default)]
pub struct MemoryFloor(pub u32);
impl GenerationFloor for MemoryFloor {
    fn get(&self) -> u32 {
        self.0
    }
    fn raise_to(&mut self, value: u32) {
        self.0 = self.0.max(value);
    }
}

// ---------------------------------------------------------------- eligibility and the exchange

/// The verified-contact store's answer for a peer (Android VerificationState).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Verified,
    Unverified,
    Mismatch,
    StoreUnreadable,
}

/// Android PresenceEligibility.reason: the fixed reason a session may not
/// exchange leases with this peer, checked in this order; None when it may.
pub fn ineligible_reason(
    vault_present: bool,
    persistent: bool,
    peer: Option<PeerState>,
    visible_to_them: bool,
    code_only_unscanned: bool,
) -> Option<&'static str> {
    match peer {
        _ if !vault_present => Some("no_vault"),
        _ if !persistent => Some("vault_not_persistent"),
        None => Some("no_contact_store"),
        Some(PeerState::Unverified) => Some("peer_unverified"),
        Some(PeerState::Mismatch) => Some("peer_mismatch"),
        Some(PeerState::StoreUnreadable) => Some("peer_store_unreadable"),
        Some(PeerState::Verified) if !visible_to_them => Some("contact_switch_off"),
        Some(PeerState::Verified) if code_only_unscanned => Some("code_only_not_scanned"),
        Some(PeerState::Verified) => None,
    }
}

/// Android CodeOnlyLeaseRule (BVMID2 on): a contact only checked by code gets
/// no new leases until scanned both ways, unless it already holds one.
pub fn code_only_blocked(method: Option<crate::contacts::Method>, scanned_both_ways: bool, holds_lease: bool) -> bool {
    method == Some(crate::contacts::Method::SafetyCode) && !scanned_both_ways && !holds_lease
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendOffer,
    SendLease,
    StoreLease(Lease),
    Reject(&'static str),
}

/// One received BDX1 record of the exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Opening,
    Reveal,
    Offer,
    Lease(Lease),
}
impl Record {
    /// Strict for OFFER and LEASE (Android DiscoveryExchange.decode); None for
    /// anything malformed or unknown.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 6 || &bytes[..4] != BDX1 {
            return None;
        }
        match bytes[4] {
            1 => Some(Self::Opening),
            KIND_OFFER => is_offer(bytes).then_some(Self::Offer),
            KIND_LEASE => Lease::decode(bytes).map(Self::Lease),
            4 => Some(Self::Reveal),
            _ => None,
        }
    }
}

/// Android PresenceSession: the OFFER / LEASE rules of one session. A lease
/// is stored only after both sides offered and the peer is verified now.
pub struct Session {
    local_eligible: bool,
    started: bool,
    sent_offer: bool,
    received_offer: bool,
    sent_lease: bool,
    received_lease: bool,
    withheld: bool,
}
impl Session {
    pub fn new(local_eligible: bool) -> Self {
        Self {
            local_eligible,
            started: false,
            sent_offer: false,
            received_offer: false,
            sent_lease: false,
            received_lease: false,
            withheld: false,
        }
    }
    /// Nothing more can move in this session.
    pub fn settled(&self) -> bool {
        !self.local_eligible || self.withheld || (self.sent_lease && self.received_lease)
    }
    pub fn started(&self) -> bool {
        self.started
    }
    /// Once, at the "answered" moment (scanner: the user's yes with the shield stored).
    pub fn start(&mut self) -> Vec<Action> {
        if !self.local_eligible || self.started {
            return Vec::new();
        }
        self.started = true;
        self.sent_offer = true;
        if self.received_offer && !self.sent_lease {
            self.sent_lease = true;
            vec![Action::SendOffer, Action::SendLease]
        } else {
            vec![Action::SendOffer]
        }
    }
    /// The OFFER from `start` could not be sent: no lease leaves or is stored.
    pub fn offer_withheld(&mut self) {
        self.sent_offer = false;
        self.sent_lease = true;
        self.withheld = true;
    }
    pub fn on_record(&mut self, record: &Record, peer_verified_now: bool) -> Vec<Action> {
        match record {
            Record::Opening => vec![Action::Reject("opening repeated")],
            Record::Reveal => vec![Action::Reject("reveal outside the reveal exchange")],
            Record::Offer if self.received_offer => vec![Action::Reject("duplicate offer")],
            Record::Offer => {
                self.received_offer = true;
                if self.sent_offer && !self.sent_lease {
                    self.sent_lease = true;
                    vec![Action::SendLease]
                } else {
                    Vec::new()
                }
            }
            Record::Lease(_) if self.received_lease => vec![Action::Reject("duplicate lease")],
            Record::Lease(_) if !self.sent_offer || !self.received_offer => {
                vec![Action::Reject("lease without both offers")]
            }
            Record::Lease(_) if !peer_verified_now => vec![Action::Reject("peer is not a verified contact")],
            Record::Lease(lease) => {
                self.received_lease = true;
                vec![Action::StoreLease(lease.clone())]
            }
        }
    }
}

// ---------------------------------------------------------------- vault

/// Android PresenceVault over the sealed store and the generation floor.
/// While the store cannot be read the vault is not persistent: it delivers
/// and stores nothing and never writes over the unreadable file.
pub struct Vault {
    store: PresenceStore,
    floor: Box<dyn GenerationFloor>,
    state: Option<State>,
    /// CF-PRES-07: the file does not open or decode: never read again (or
    /// overwritten) until "Reset presence key" (`reset_damaged`).
    damaged: bool,
    /// Transient read or first-save failures in a row, and the next attempt
    /// (2 s doubling to 60 s, as the contact store; DPAPI needs no 5 s
    /// Keystore cold-start retry).
    failures: u32,
    retry_at: Option<std::time::Instant>,
    /// CF-PRES-07: random in-memory day keys announced while the store cannot
    /// be used (nobody recognises them; no contacts, so no OFFER or LEASE).
    /// Dropped, and so zeroed, as soon as the store loads.
    ephemeral: Option<State>,
    /// Bumped on every change of `state`; the cached table is keyed on it.
    revision: u64,
    /// CF-PRES-27: the recognition table, rebuilt in full on the first lookup
    /// after an epoch tick, any state change (lease stored, forget, reset,
    /// generation, reconcile) or a lease turning stale; dropped with the vault
    /// (the wipe).
    table: std::cell::RefCell<Option<CachedTable>>,
}

struct CachedTable {
    epoch: u32,
    revision: u64,
    stale: usize,
    table: RecognitionTable,
}

/// Why the presence store cannot be used now (CF-PRES-07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultProblem {
    /// Retried with backoff.
    Transient,
    /// Latched until "Reset presence key".
    Damaged,
}

impl Vault {
    pub fn new(store: PresenceStore, floor: Box<dyn GenerationFloor>) -> Self {
        Self { store, floor, state: None, damaged: false, failures: 0, retry_at: None, ephemeral: None, revision: 0, table: std::cell::RefCell::new(None) }
    }
    /// Why the store is not loaded (None: loaded, or not tried yet).
    pub fn problem(&self) -> Option<VaultProblem> {
        if self.state.is_some() {
            None
        } else if self.damaged {
            Some(VaultProblem::Damaged)
        } else if self.failures > 0 {
            Some(VaultProblem::Transient)
        } else {
            None
        }
    }
    /// "Reset presence key": deletes a damaged store so the next load starts
    /// a new generation (contacts' leases from it are gone; they recognise
    /// this PC again after the next verified call). False when the store is
    /// not damaged (nothing deleted) or the file can't be removed.
    pub fn reset_damaged(&mut self) -> bool {
        if !self.damaged || self.store.remove().is_err() {
            return false;
        }
        self.damaged = false;
        self.failures = 0;
        self.retry_at = None;
        true
    }
    fn transient_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        let wait = (2u64 << (self.failures - 1).min(5)).min(60);
        self.retry_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(wait));
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(PresenceStore::open_default(dir), Box::new(FileFloor::new(dir)))
    }
    pub fn persistent(&self) -> bool {
        self.state.is_some()
    }
    pub fn state(&self) -> Option<&State> {
        self.state.as_ref()
    }
    /// Loads (or creates) the state at `epoch`; true when persistent.
    pub fn ensure_loaded(&mut self, epoch: u32) -> bool {
        if self.state.is_some() {
            return true;
        }
        if self.damaged || self.retry_at.is_some_and(|at| std::time::Instant::now() < at) {
            return false;
        }
        match self.store.load() {
            Ok(Some(loaded)) if loaded.generation < self.floor.get() => {
                // The counter moved past this file (a restored older copy, or a
                // revocation that could not be stored): never reuse its keys.
                let mut next = State::fresh_at(self.floor.get().saturating_add(1), epoch);
                next.contacts = std::mem::take(&mut { loaded }.contacts);
                self.floor.raise_to(next.generation);
                if self.store.save(&next).is_ok() {
                    self.revision += 1;
                    self.state = Some(next);
                } else {
                    self.transient_failure();
                }
            }
            Ok(Some(loaded)) => {
                self.floor.raise_to(loaded.generation);
                self.revision += 1;
                self.state = Some(loaded);
            }
            Ok(None) => {
                let created = State::fresh_at(self.floor.get().saturating_add(1), epoch);
                self.floor.raise_to(created.generation);
                if self.store.save(&created).is_ok() {
                    self.revision += 1;
                    self.state = Some(created);
                } else {
                    self.transient_failure();
                }
            }
            // Bytes that do not open with this user's key, or do not decode.
            Err(sealed::Error::Unseal(_)) => self.damaged = true,
            Err(_) => self.transient_failure(),
        }
        if self.state.is_some() {
            self.failures = 0;
            self.retry_at = None;
            // Leaving the unreadable state discards the ephemeral keys.
            self.ephemeral = None;
        }
        self.state.is_some()
    }
    /// CF-PRES-07: the state to announce and answer with at `epoch`, and
    /// whether it is the persistent one. While the store cannot be used this
    /// is an ephemeral in-memory window (random day keys, no contacts), so
    /// the PC stays on the air under IDs nobody recognises, and the
    /// persistent file is neither read again early nor overwritten.
    pub fn announcing(&mut self, epoch: u32) -> (&State, bool) {
        if self.ensure_loaded(epoch) {
            return (self.state.as_ref().expect("loaded"), true);
        }
        let ephemeral = self.ephemeral.get_or_insert_with(|| State::fresh(epoch));
        let mut rolled = roll(&ephemeral.keys, epoch, &mut random_key);
        if rolled != ephemeral.keys {
            replace_keys(&mut ephemeral.keys, rolled);
        } else {
            wipe_keys(rolled.values_mut());
        }
        (ephemeral, false)
    }
    fn mutate(&mut self, change: impl FnOnce(&mut State)) -> bool {
        let Some(current) = &self.state else { return false };
        let mut next = current.clone();
        change(&mut next);
        if self.store.save(&next).is_err() {
            return false;
        }
        self.revision += 1;
        self.state = Some(next);
        true
    }
    /// Once-per-day maintenance: roll the window, prune contacts' days.
    pub fn maintain(&mut self, epoch: u32) -> bool {
        let Some(current) = &self.state else { return false };
        let mut next = current.clone();
        let rolled = roll(&current.keys, epoch, &mut random_key);
        replace_keys(&mut next.keys, rolled);
        for c in &mut next.contacts {
            c.lease = c.lease.as_ref().and_then(|l| l.pruned(epoch));
        }
        if &next == current || self.store.save(&next).is_err() {
            return false;
        }
        self.revision += 1;
        self.state = Some(next);
        true
    }
    pub fn contact(&self, node_id: &[u8; 16]) -> Option<&ContactState> {
        self.state.as_ref()?.contacts.iter().find(|c| &c.node_id == node_id)
    }
    pub fn visible_to_them(&self, node_id: &[u8; 16]) -> bool {
        self.contact(node_id).is_none_or(|c| c.visible_to_them)
    }
    /// Android deliverLease (RD1): checks, rolls and stores the window, stores
    /// the delivery durably, and only then returns the lease (keys and
    /// generation from one state). None: nothing delivered or recorded.
    pub fn deliver_lease(&mut self, node_id: &[u8; 16], epoch: u32, still_verified: impl FnOnce() -> bool) -> Option<Lease> {
        if self.state.is_none() || !self.visible_to_them(node_id) || !still_verified() {
            return None;
        }
        let rolled = roll(&self.state.as_ref()?.keys, epoch, &mut random_key);
        if rolled != self.state.as_ref()?.keys && !self.mutate(|s| replace_keys(&mut s.keys, rolled)) {
            return None;
        }
        let generation = self.state.as_ref()?.generation;
        if self.state.as_ref()?.contacts.len() >= MAX_CONTACTS && self.contact(node_id).is_none() {
            return None;
        }
        if !self.mutate(|s| s.contact_mut(node_id).delivered_generation = generation) {
            return None;
        }
        let state = self.state.as_ref()?;
        Lease::delivery(&state.keys, state.generation, epoch)
    }
    /// Stores `lease` from `node_id` by the receiver rules; false when nothing changed.
    pub fn store_lease(&mut self, node_id: &[u8; 16], lease: &Lease, my_epoch: u32, now_ms: i64) -> bool {
        let Some(state) = &self.state else { return false };
        let before = state.contacts.iter().find(|c| &c.node_id == node_id).and_then(|c| c.lease.clone());
        if before.is_none() && state.contacts.len() >= MAX_CONTACTS && !state.contacts.iter().any(|c| &c.node_id == node_id) {
            return false;
        }
        let received = Received::receive(before.as_ref(), lease, my_epoch, now_ms);
        if before.as_ref() == Some(&received) {
            return false;
        }
        let after = received.pruned(my_epoch);
        self.mutate(|s| s.contact_mut(node_id).lease = after)
    }
    /// A revocation: a new generation and a whole new window now. The floor
    /// is raised first; if the new state cannot be stored the vault stops
    /// being persistent and the next load replaces the stored keys.
    pub fn bump(&mut self, epoch: u32) -> bool {
        self.revision += 1;
        let Some(current) = self.state.take() else {
            self.floor.raise_to(self.floor.get().saturating_add(1));
            return true;
        };
        let generation = self.floor.get().max(current.generation).saturating_add(1);
        self.floor.raise_to(generation);
        let mut next = State::fresh_at(generation, epoch);
        next.contacts = std::mem::take(&mut { current }.contacts);
        if self.store.save(&next).is_ok() {
            self.revision += 1;
            self.state = Some(next);
        }
        true
    }
    /// Forgets `node_id`; revokes first when it got a lease of this generation.
    pub fn forget(&mut self, node_id: &[u8; 16], epoch: u32) -> bool {
        let Some(state) = &self.state else {
            self.floor.raise_to(self.floor.get().saturating_add(1));
            return true;
        };
        let Some(existing) = state.contacts.iter().find(|c| &c.node_id == node_id) else {
            return false;
        };
        let revoked = existing.delivered_generation == state.generation && self.bump(epoch);
        self.mutate(|s| s.contacts.retain(|c| &c.node_id != node_id));
        revoked
    }
    /// CF-PRES-09 start-up reconcile: drops the records of node IDs that are
    /// no longer verified contacts; a new generation first if any of them had
    /// a delivery of the current one. Returns (records dropped, revoked).
    pub fn reconcile(&mut self, verified: &[[u8; 16]], epoch: u32) -> (usize, bool) {
        let Some(state) = &self.state else { return (0, false) };
        let stale: Vec<[u8; 16]> = state.contacts.iter().map(|c| c.node_id).filter(|n| !verified.contains(n)).collect();
        if stale.is_empty() {
            return (0, false);
        }
        let revoke = state.contacts.iter().any(|c| stale.contains(&c.node_id) && c.delivered_generation == state.generation);
        let revoked = revoke && self.bump(epoch);
        self.mutate(|s| s.contacts.retain(|c| !stale.contains(&c.node_id)));
        (stale.len(), revoked)
    }
    /// Every contact record goes (the verified contacts were reset); a new
    /// generation if any lease of this one was delivered.
    pub fn reset_contacts(&mut self, epoch: u32) -> bool {
        let Some(state) = &self.state else {
            self.floor.raise_to(self.floor.get().saturating_add(1));
            return true;
        };
        let revoke = state.contacts.iter().any(|c| c.delivered_generation == state.generation);
        let revoked = revoke && self.bump(epoch);
        self.mutate(|s| s.contacts.clear());
        revoked
    }
    /// "Can see when I'm nearby"; turning it off after a delivery of this
    /// generation revokes (true).
    pub fn set_visible(&mut self, node_id: &[u8; 16], visible: bool, epoch: u32) -> bool {
        let Some(state) = &self.state else {
            if !visible {
                self.floor.raise_to(self.floor.get().saturating_add(1));
            }
            return !visible;
        };
        let existing = state.contacts.iter().find(|c| &c.node_id == node_id);
        if existing.is_none_or(|c| c.visible_to_them) == visible {
            return false;
        }
        let revoke = !visible && existing.is_some_and(|c| c.delivered_generation == state.generation);
        self.mutate(|s| s.contact_mut(node_id).visible_to_them = visible);
        revoke && self.bump(epoch)
    }
    /// The recognition table of every contact that gave this PC a lease.
    pub fn recognition(&self, my_epoch: u32, now_ms: i64) -> RecognitionTable {
        self.state.as_ref().map_or_else(RecognitionTable::default, |s| s.recognition(my_epoch, now_ms))
    }
    /// The hits for `rdi` from the cached table (CF-PRES-27), rebuilding it
    /// first when the epoch, the state or the set of stale leases changed.
    /// Only a persistent store is used: none recognises nobody.
    fn lookup(&self, rdi: &[u8; 6], my_epoch: u32, now_ms: i64) -> Vec<Hit> {
        let Some(state) = &self.state else { return Vec::new() };
        let stale = state.contacts.iter().filter_map(|c| c.lease.as_ref()).filter(|l| now_ms - l.received_at_ms > STALE_SKEW_MS).count();
        let mut cache = self.table.borrow_mut();
        let fresh = cache.as_ref().is_some_and(|c| c.epoch == my_epoch && c.revision == self.revision && c.stale == stale);
        if !fresh {
            *cache = Some(CachedTable { epoch: my_epoch, revision: self.revision, stale, table: state.recognition(my_epoch, now_ms) });
        }
        cache.as_ref().map_or_else(Vec::new, |c| c.table.lookup(rdi).to_vec())
    }
    /// The epoch and revision the cached table was built for (tests).
    #[cfg(test)]
    fn cached_revision(&self) -> Option<(u32, u64)> {
        self.table.borrow().as_ref().map(|c| (c.epoch, c.revision))
    }
    /// What calling the contact behind a sighted role-0 RDI needs: the
    /// contact's node ID and the day key of the hit's epoch from its lease
    /// (the contact tag key is derived from it). None without exactly one
    /// recognised contact (a collision of two contacts is not dialled).
    pub fn lease_for(&self, rdi: &[u8; 6], my_epoch: u32, now_ms: i64) -> Option<([u8; 16], Key)> {
        let found: Vec<Hit> = self.lookup(rdi, my_epoch, now_ms);
        let hits: Vec<&Hit> = found.iter().filter(|h| h.role == ROLE_CALL_OFFER).collect();
        let first = hits.first()?;
        if hits.iter().any(|h| h.contact != first.contact) {
            return None;
        }
        let key = *self.contact(&first.contact)?.lease.as_ref()?.days.get(&day_of(first.epoch))?;
        Some((first.contact, key))
    }
    /// The contact a sighted rotating call offer belongs to (role 0 hits only).
    pub fn recognise_call_offer(&self, rdi: &[u8; 6], my_epoch: u32, now_ms: i64) -> Vec<[u8; 16]> {
        let mut out: Vec<[u8; 16]> = Vec::new();
        for hit in &self.lookup(rdi, my_epoch, now_ms) {
            if hit.role == ROLE_CALL_OFFER && !out.contains(&hit.contact) {
                out.push(hit.contact);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn a_damaged_store_latches_until_reset_and_is_never_overwritten() {
        let dir = crate::sealed::tests::scratch("vault-damaged");
        let file = dir.join("identity-verification").join("presence-leases.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"not a sealed store").unwrap();
        let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(crate::sealed::tests::Plain)), Box::new(MemoryFloor(0)));
        assert!(!vault.ensure_loaded(EPOCH));
        assert_eq!(vault.problem(), Some(VaultProblem::Damaged));
        assert!(!vault.ensure_loaded(EPOCH), "latched: not read again");
        assert_eq!(std::fs::read(&file).unwrap(), b"not a sealed store", "never overwritten");
        assert!(vault.reset_damaged());
        assert!(!file.exists());
        assert!(vault.ensure_loaded(EPOCH), "a new store after the reset");
        assert_eq!(vault.problem(), None);
        assert!(!vault.reset_damaged(), "a healthy store is never deleted");
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_store_announces_an_ephemeral_window_that_the_store_replaces() {
        let dir = crate::sealed::tests::scratch("vault-ephemeral");
        let file = dir.join("identity-verification").join("presence-leases.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"not a sealed store").unwrap();
        let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(crate::sealed::tests::Plain)), Box::new(MemoryFloor(0)));
        let (state, persistent) = vault.announcing(EPOCH);
        assert!(!persistent);
        assert!(state.keys.contains_key(&day_of(EPOCH)), "a day key to announce with");
        assert!(state.contacts.is_empty(), "nobody is recognised and nothing is delivered");
        let first = state.keys.clone();
        assert_eq!(vault.announcing(EPOCH).0.keys, first, "the same window while unreadable");
        let later = EPOCH + 96 * 2;
        let rolled = vault.announcing(later).0.keys.clone();
        assert!(rolled.contains_key(&day_of(later)) && !rolled.contains_key(&day_of(EPOCH)), "rolled like a real window");
        assert!(vault.state().is_none() && !vault.persistent());
        assert_eq!(std::fs::read(&file).unwrap(), b"not a sealed store", "never overwritten");
        assert!(vault.reset_damaged());
        let (state, persistent) = vault.announcing(later);
        assert!(persistent);
        assert_ne!(state.keys, rolled, "the persistent window is new, not the ephemeral one");
        assert!(vault.ephemeral.is_none(), "ephemeral keys discarded on leaving");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_recognition_table_is_rebuilt_on_an_epoch_tick_a_change_or_a_stale_lease_only() {
        let dir = crate::sealed::tests::scratch("vault-table");
        let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(crate::sealed::tests::Plain)), Box::new(MemoryFloor(0)));
        assert!(vault.ensure_loaded(EPOCH));
        let node = [7u8; 16];
        let window = roll(&BTreeMap::new(), EPOCH, &mut random_key);
        let rdi = |epoch| own_rdi(&window, &node, epoch, ROLE_CALL_OFFER).unwrap();
        assert!(vault.recognise_call_offer(&rdi(EPOCH), EPOCH, 1_000).is_empty(), "no lease yet");
        let before = vault.cached_revision();
        assert!(vault.store_lease(&node, &Lease::delivery(&window, 1, EPOCH).unwrap(), EPOCH, 1_000));
        assert_eq!(vault.recognise_call_offer(&rdi(EPOCH), EPOCH, 1_000), vec![node], "a stored lease rebuilds");
        let built = vault.cached_revision();
        assert_ne!(built, before);
        assert_eq!(vault.recognise_call_offer(&rdi(EPOCH), EPOCH, 2_000), vec![node]);
        assert_eq!(vault.cached_revision(), built, "nothing changed: the cached table is used");
        assert_eq!(vault.recognise_call_offer(&rdi(EPOCH + 1), EPOCH + 1, 2_000), vec![node], "an epoch tick rebuilds");
        assert_eq!(vault.cached_revision().unwrap().0, EPOCH + 1);
        // Five epochs away is outside the narrow radius until the lease is stale.
        assert!(vault.recognise_call_offer(&rdi(EPOCH + 5), EPOCH, 2_000).is_empty());
        assert_eq!(vault.recognise_call_offer(&rdi(EPOCH + 5), EPOCH, 1_000 + STALE_SKEW_MS + 1), vec![node], "a stale lease widens the table");
        assert!(!vault.forget(&node, EPOCH), "no delivery, so no revocation");
        assert!(vault.recognise_call_offer(&rdi(EPOCH), EPOCH, 2_000).is_empty(), "forget rebuilds");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transient_failure_backs_off_instead_of_retrying_every_call() {
        let dir = crate::sealed::tests::scratch("vault-transient");
        // A directory where the file should be: reading fails with an I/O error.
        let file = dir.join("identity-verification").join("presence-leases.bin");
        std::fs::create_dir_all(&file).unwrap();
        let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(crate::sealed::tests::Plain)), Box::new(MemoryFloor(0)));
        assert!(!vault.ensure_loaded(EPOCH));
        assert_eq!(vault.problem(), Some(VaultProblem::Transient));
        std::fs::remove_dir_all(&file).unwrap();
        assert!(!vault.ensure_loaded(EPOCH), "inside the 2 s backoff nothing is read");
        assert!(!vault.reset_damaged(), "a transient failure is not reset by deleting");
        vault.retry_at = Some(std::time::Instant::now());
        assert!(vault.ensure_loaded(EPOCH));
        assert_eq!(vault.problem(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn vector(id: &str) -> Value {
        let json: Value = serde_json::from_str(include_str!("../tests/vectors/conformance/leases.json")).unwrap();
        json["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone()
    }
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
    fn arr<const N: usize>(v: &Value) -> [u8; N] {
        hex(v.as_str().unwrap()).try_into().unwrap()
    }
    const P: Key = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
    const NODE: [u8; 16] = [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f, 0x90];
    const EPOCH: u32 = 1_990_656;
    fn legacy_window(from: u32, to: u32) -> BTreeMap<u32, Key> {
        (from..=to).map(|d| (d, legacy_lease_key(&P, d).unwrap())).collect()
    }

    #[test]
    fn derivations_match_leases_001_to_005() {
        assert_eq!(LEASE_DOMAIN, hex(vector("leases-001")["expected"]["lease_domain"].as_str().unwrap()));
        let v = vector("leases-002");
        let days: Vec<u32> = v["inputs"]["epochs"].as_array().unwrap().iter().map(|e| day_of(e.as_u64().unwrap() as u32)).collect();
        assert_eq!(serde_json::to_value(days).unwrap(), v["expected"]["days"]);
        assert_eq!(legacy_lease_key(&P, 20736).unwrap(), arr::<32>(&vector("leases-003")["expected"]["lease_key"]));
        assert_eq!(legacy_lease_key(&P, 20737).unwrap(), arr::<32>(&vector("leases-004")["expected"]["lease_key"]));
        let v = vector("leases-005");
        let key: Key = arr(&v["inputs"]["salt_key"]);
        let salt = rotating::salt(&key, EPOCH, 0).unwrap();
        assert_eq!(salt, arr::<32>(&v["expected"]["salt"]));
        let keys: BTreeMap<u32, Key> = [(20736, key)].into();
        assert_eq!(own_rdi(&keys, &NODE, EPOCH, 0), Some(arr::<6>(&v["expected"]["rdi"])));
    }

    #[test]
    fn delivery_and_window_follow_leases_006_and_007() {
        let window = legacy_window(first_day(EPOCH), last_day(EPOCH));
        assert_eq!((first_day(EPOCH), last_day(EPOCH)), (20735, 20763));
        let lease = Lease::delivery(&window, 1, EPOCH).unwrap();
        assert_eq!(lease.keys.len(), 28);
        assert_eq!(lease.encode().len(), LEASE_BYTES_FULL);
        let mut missing = window.clone();
        missing.remove(&20750);
        assert!(Lease::delivery(&missing, 1, EPOCH).is_none());
        // 007: fresh, same day, next day adds day+28 and keeps yesterday, then drops it, a month off renews.
        let mut n = 0u8;
        let mut fresh = || {
            n = n.wrapping_add(1);
            [n; 32]
        };
        let span = |k: &BTreeMap<u32, Key>| (*k.keys().next().unwrap(), *k.keys().last().unwrap());
        let w0 = roll(&BTreeMap::new(), EPOCH, &mut fresh);
        assert_eq!(span(&w0), (20736, 20763));
        assert_eq!(span(&roll(&w0, EPOCH + 95, &mut fresh)), (20736, 20763));
        let w2 = roll(&w0, EPOCH + 96, &mut fresh);
        assert_eq!(span(&w2), (20736, 20764));
        assert_eq!(w2[&20740], w0[&20740], "existing keys are never replaced");
        assert_eq!(span(&roll(&w2, EPOCH + 97, &mut fresh)), (20737, 20764));
        assert_eq!(span(&roll(&w0, EPOCH + 40 * 96, &mut fresh)), (20776, 20803));
    }

    #[test]
    fn recognition_and_skew_follow_leases_008_and_009() {
        let v = vector("leases-008");
        let window = legacy_window(20735, 20763);
        let lease = Lease::delivery(&window, 1, EPOCH + 2).unwrap();
        let received = Received::receive(None, &lease, EPOCH, 0);
        assert_eq!(received.skew, 2);
        let table = RecognitionTable::build([(&NODE, &received)], EPOCH, 1_000);
        assert_eq!(table.len(), v["expected"]["table_size"].as_u64().unwrap() as usize);
        for (epoch, rdi) in v["inputs"]["hit_rdis"].as_object().unwrap() {
            let hits = table.lookup(&arr(rdi));
            assert_eq!(hits, [Hit { contact: NODE, epoch: epoch.parse().unwrap(), role: 0 }]);
        }
        for rdi in v["inputs"]["miss_rdis"].as_object().unwrap().values() {
            assert!(table.lookup(&arr(rdi)).is_empty());
        }
        let stale = RecognitionTable::build([(&NODE, &received)], EPOCH, STALE_SKEW_MS + 1);
        assert_eq!(stale.len(), v["expected"]["stale_skew_table_size"].as_u64().unwrap() as usize);
        let mut far = lease.clone();
        far.sender_epoch = EPOCH + 10_000;
        assert_eq!(Received::receive(None, &far, EPOCH, 0).skew, vector("leases-009")["expected"]["skew_epochs"].as_i64().unwrap());
    }

    #[test]
    fn receiver_rules_replace_merge_and_ignore() {
        let window = legacy_window(20730, 20800);
        let delivery = |generation, epoch| Lease::delivery(&window, generation, epoch).unwrap();
        let first = Received::receive(None, &delivery(2, EPOCH), EPOCH - 3, 1000);
        assert_eq!(first.skew, 3);
        let refreshed = Received::receive(Some(&first), &delivery(2, EPOCH + 5 * 96), EPOCH + 5 * 96, 2000);
        assert_eq!(*refreshed.days.keys().next().unwrap(), 20741);
        assert_eq!(*refreshed.days.keys().last().unwrap(), 20768);
        let lower = Received::receive(Some(&refreshed), &delivery(1, EPOCH), EPOCH, 3000);
        assert_eq!(lower, refreshed);
        // Same generation, different key on a shared day: replaced, not mixed.
        let other: BTreeMap<u32, Key> = (20730..20800).map(|d| (d, [9; 32])).collect();
        let swapped = Received::receive(Some(&refreshed), &Lease::delivery(&other, 2, EPOCH + 6 * 96).unwrap(), EPOCH, 4000);
        assert!(swapped.days.values().all(|k| *k == [9; 32]));
    }

    #[test]
    fn bdx1_records_round_trip_and_reject_like_android() {
        assert_eq!(offer().to_vec(), hex("424458310200"));
        let window = legacy_window(20736, 20763);
        let lease = Lease::delivery(&window, 1, EPOCH).unwrap();
        let bytes = lease.encode();
        assert_eq!(hex("424458310300000001001e6000000051001c"), bytes[..18]);
        assert_eq!(Lease::decode(&bytes), Some(lease.clone()));
        for bad in ["424458310400", "424458310201", "42445831020000", "4244583101"] {
            assert!(Lease::decode(&hex(bad)).is_none() && !is_offer(&hex(bad)), "{bad}");
        }
        assert!(Lease::decode(&bytes[..bytes.len() - 1]).is_none());
        let mut long = bytes.clone();
        long.push(0);
        assert!(Lease::decode(&long).is_none());
        let mut zero_gen = bytes.clone();
        zero_gen[5..9].copy_from_slice(&[0; 4]);
        assert!(Lease::decode(&zero_gen).is_none());
        let mut count29 = bytes.clone();
        count29[17] = 29;
        assert!(Lease::decode(&count29).is_none());
    }

    #[test]
    fn the_store_codec_matches_leases_010_and_rejects_011_to_015() {
        let v = vector("leases-010");
        let state = State {
            generation: 1,
            keys: [(20736, [7; 32])].into(),
            contacts: vec![],
        };
        assert_eq!(state.encode(), hex(v["expected"]["plaintext"].as_str().unwrap()));
        let full = State {
            generation: 3,
            keys: legacy_window(20736, 20763),
            contacts: vec![ContactState {
                node_id: NODE,
                visible_to_them: true,
                delivered_generation: 3,
                lease: Some(Received::receive(None, &Lease::delivery(&legacy_window(20736, 20763), 1, EPOCH).unwrap(), EPOCH, 5)),
            }],
        };
        let bytes = full.encode();
        assert_eq!(State::decode(&bytes), Some(full.clone()));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(State::decode(&trailing).is_none());
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(State::decode(&magic).is_none());
        let mut twice = full.clone();
        twice.contacts.push(twice.contacts[0].clone());
        assert!(State::decode(&twice.encode()).is_none());
        let mut same_day = state.encode();
        same_day[9] = 2; // two keys, the same day twice
        let entry = same_day[10..46].to_vec();
        same_day.splice(46..46, entry);
        assert!(State::decode(&same_day).is_none());
        let mut no_key = state.encode();
        no_key[9] = 0;
        no_key.drain(10..46);
        assert!(State::decode(&no_key).is_none());
    }

    #[test]
    fn the_sealed_store_rolls_and_forgetting_after_delivery_renews_the_window() {
        let dir = crate::sealed::tests::scratch("presence");
        let store = PresenceStore::new(&dir, Box::new(crate::sealed::tests::Plain));
        let mut state = store.current(EPOCH).unwrap();
        assert_eq!(state.generation, 1);
        assert_eq!(store.load().unwrap(), Some(state.clone()), "keys are saved before any delivery");
        state.contact_mut(&NODE).delivered_generation = 1;
        let before = state.keys.clone();
        state.forget(&NODE, EPOCH);
        assert_eq!(state.generation, 2);
        assert_ne!(state.keys, before);
        assert!(state.contacts.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
