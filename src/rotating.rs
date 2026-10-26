//! Rotating discovery ID derivation, opening entries and the selection check
//! (PROTOCOL "Experimental rotating discovery IDs"), plus the BMH3 tag keys and
//! advert binding that use the same lease keys. Pure functions: day-key storage,
//! pins, the monotonic epoch clock and advertising stay with their owners.
use crate::crypto;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub const EPOCH_SECONDS: u64 = 900;
/// RD4: an opening entry must lie within this many epochs of the verifier's wall epoch.
pub const EPOCH_TOLERANCE: u32 = 2;
pub const ENTRY_BYTES: usize = 43;
pub const MAX_ENTRIES: usize = 2;
pub const ROLE_CALL_OFFER: u8 = 0;
pub const ROLE_STREAM_OFFER: u8 = 1;
pub const ROLE_VIEWER_REQUEST: u8 = 2;

const SALT_DOMAIN: &[u8] = b"BluetoothVideoMesh/Discovery/Salt/1\0";
const ID_DOMAIN: &[u8] = b"BluetoothVideoMesh/Discovery/Id/1\0";
const CONTACT_TAG_INFO: &[u8] = b"BluetoothVideoMesh/BMH3/ContactTag/1";
const QR_TAG_INFO: &[u8] = b"BluetoothVideoMesh/BMH3/QrTag/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Format,
    Crypto(crypto::Error),
}
impl From<crypto::Error> for Error {
    fn from(error: crypto::Error) -> Self {
        Self::Crypto(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rotating discovery error: {self:?}")
    }
}
impl std::error::Error for Error {}

/// `floor(unix_seconds / 900)`; a value beyond u32 is not a protocol epoch.
pub fn wall_epoch(unix_seconds: u64) -> Option<u32> {
    u32::try_from(unix_seconds / EPOCH_SECONDS).ok()
}

/// `salt = HMAC-SHA256(L_d, ".../Salt/1" || 0x00 || u32 e || u8 r)`.
pub fn salt(day_key: &[u8; 32], epoch: u32, role: u8) -> Result<[u8; 32], Error> {
    let input = [SALT_DOMAIN, &epoch.to_be_bytes(), &[role]].concat();
    Ok(*crypto::hmac_sha256(day_key, &input)?)
}

/// `RDI = SHA-256(".../Id/1" || 0x00 || nodeId || u32 e || u8 r || salt)[0:6]`.
pub fn rdi(node_id: &[u8; 16], epoch: u32, role: u8, salt: &[u8; 32]) -> Result<[u8; 6], Error> {
    let input = [ID_DOMAIN, node_id, &epoch.to_be_bytes(), &[role], salt].concat();
    Ok(crypto::sha256(&input)?[..6].try_into().unwrap())
}

/// `K_tag = HKDF-Expand(L_d, "BluetoothVideoMesh/BMH3/ContactTag/1", 32)`.
pub fn contact_tag_key(lease_key: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    Ok(crypto::hkdf_expand(lease_key, CONTACT_TAG_INFO)?)
}

/// BVMID2: `K_tag = HKDF-Expand(K_qr, "BluetoothVideoMesh/BMH3/QrTag/1", 32)`, never `K_qr` itself.
pub fn qr_tag_key(k_qr: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    Ok(crypto::hkdf_expand(k_qr, QR_TAG_INFO)?)
}

/// The 11-byte contact-tag binding `selected (6) | role (1) | epoch (4)`: the
/// initiator's wall epoch for a version 2 advert, 0 for version 5 (`None`).
pub fn advert_binding(selected: &[u8; 6], role: u8, wall_epoch: Option<u32>) -> [u8; 11] {
    let mut binding = [0; 11];
    binding[..6].copy_from_slice(selected);
    binding[6] = role;
    binding[7..].copy_from_slice(&wall_epoch.unwrap_or(0).to_be_bytes());
    binding
}

/// `RDI (6) | epoch (4) | role (1) | salt (32)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpeningEntry {
    pub rdi: [u8; 6],
    pub epoch: u32,
    pub role: u8,
    pub salt: [u8; 32],
}
impl OpeningEntry {
    /// The entry this phone publishes for `(epoch, role)`, salted by that day's key.
    pub fn own(
        day_key: &[u8; 32],
        node_id: &[u8; 16],
        epoch: u32,
        role: u8,
    ) -> Result<Self, Error> {
        let salt = salt(day_key, epoch, role)?;
        Ok(Self {
            rdi: rdi(node_id, epoch, role, &salt)?,
            epoch,
            role,
            salt,
        })
    }
    fn write(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.rdi);
        output.extend_from_slice(&self.epoch.to_be_bytes());
        output.push(self.role);
        output.extend_from_slice(&self.salt);
    }
    fn read(bytes: &[u8]) -> Self {
        Self {
            rdi: bytes[..6].try_into().unwrap(),
            epoch: u32::from_be_bytes(bytes[6..10].try_into().unwrap()),
            role: bytes[10],
            salt: bytes[11..43].try_into().unwrap(),
        }
    }
    /// The entry opens `selected` for `node_id`: recomputed, never trusted. Both
    /// comparisons are public values; the second is constant time anyway.
    pub fn opens(&self, selected: &[u8; 6], node_id: &[u8; 16], role: u8) -> Result<bool, Error> {
        if self.role != role || self.rdi != *selected {
            return Ok(false);
        }
        let computed = rdi(node_id, self.epoch, self.role, &self.salt)?;
        Ok(bool::from(computed.ct_eq(selected)))
    }
}

/// `count (1, 0..2) | count x entry (43)`: the BDO1 body after its version byte,
/// the BMH3 `OPENING` extension value and the BDX1 OPENING body.
pub fn encode_body(entries: &[OpeningEntry]) -> Result<Vec<u8>, Error> {
    if entries.len() > MAX_ENTRIES {
        return Err(Error::Format);
    }
    let mut output = Vec::with_capacity(1 + entries.len() * ENTRY_BYTES);
    output.push(entries.len() as u8);
    for entry in entries {
        entry.write(&mut output);
    }
    Ok(output)
}
pub fn decode_body(bytes: &[u8]) -> Result<Vec<OpeningEntry>, Error> {
    let count = *bytes.first().ok_or(Error::Format)? as usize;
    if count > MAX_ENTRIES || bytes.len() != 1 + count * ENTRY_BYTES {
        return Err(Error::Format);
    }
    Ok(bytes[1..]
        .chunks_exact(ENTRY_BYTES)
        .map(OpeningEntry::read)
        .collect())
}

/// Hop type 13 `"BDO1" | version 1 | body`.
pub fn encode_bdo1(entries: &[OpeningEntry]) -> Result<Vec<u8>, Error> {
    Ok([b"BDO1".as_slice(), &[1], &encode_body(entries)?].concat())
}
pub fn decode_bdo1(bytes: &[u8]) -> Result<Vec<OpeningEntry>, Error> {
    if bytes.len() < 6 || &bytes[..4] != b"BDO1" || bytes[4] != 1 {
        return Err(Error::Format);
    }
    decode_body(&bytes[5..])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Pass,
    PrefixMismatch,
    OpeningMissing,
    OpeningMismatch,
    OpeningStale,
}
/// RV15: the check follows the advert the caller selected, never the peer's
/// hello. Version 2 compares the announced prefix; version 5 (or compact v2)
/// needs an entry that opens the selected bytes within the epoch window.
pub fn check_selection(
    selected: &[u8; 6],
    rotating: bool,
    role: u8,
    peer_node_id: &[u8; 16],
    entries: Option<&[OpeningEntry]>,
    verifier_wall_epoch: u32,
) -> Result<Selection, Error> {
    if !rotating {
        return Ok(if peer_node_id[..6] == selected[..] {
            Selection::Pass
        } else {
            Selection::PrefixMismatch
        });
    }
    let Some(entries) = entries else {
        return Ok(Selection::OpeningMissing);
    };
    let mut opened = false;
    for entry in entries {
        if entry.opens(selected, peer_node_id, role)? {
            opened = true;
            if entry.epoch.abs_diff(verifier_wall_epoch) <= EPOCH_TOLERANCE {
                return Ok(Selection::Pass);
            }
        }
    }
    Ok(if opened {
        Selection::OpeningStale
    } else {
        Selection::OpeningMismatch
    })
}

/// One epoch plus a minute, so a normal wall boundary always ticks first.
pub const ELAPSED_TICK_MS: u64 = (EPOCH_SECONDS + 60) * 1000;
/// A lead of E over the wall above the opening tolerance starts the recovery clock.
pub const MAX_AHEAD_EPOCHS: u32 = EPOCH_TOLERANCE;
pub const AHEAD_RECOVERY_MS: u64 = 3_600_000;
pub const CONSISTENT_WALL_TOLERANCE_MS: i64 = 120_000;
/// An ID stays openable for this long after its set left the air.
pub const PREVIOUS_GRACE_MS: u64 = 60_000;

/// Persisted monotonic-epoch state (RV4). `boot` identifies the elapsed-time base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochState {
    pub last_epoch: Option<u32>,
    pub tick_wall_epoch: Option<u32>,
    pub tick_elapsed_ms: u64,
    pub boot: u64,
    pub generation: u32,
}
impl Default for EpochState {
    fn default() -> Self {
        Self {
            last_epoch: None,
            tick_wall_epoch: None,
            tick_elapsed_ms: 0,
            boot: 0,
            generation: 1,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochReading {
    pub epoch: u32,
    pub ticked: bool,
    /// Once per forward-jump recovery: the owner must start a fresh day-key window.
    pub new_generation: bool,
}
/// Android `DiscoveryEpochClock`: a tick happens when the wall epoch changes or
/// 960 s of elapsed time pass; at a tick `E = max(wall, last + 1)`; E never goes
/// down; a lead above two epochs held for an hour of consistent wall time starts
/// generation + 1 at the wall epoch. The owner persists `state` after each read.
pub struct EpochClock {
    pub state: EpochState,
    boot: u64,
    ahead_since_ms: Option<u64>,
    ahead_offset_ms: i64,
}
impl EpochClock {
    pub fn new(state: EpochState, boot: u64) -> Self {
        Self {
            state,
            boot,
            ahead_since_ms: None,
            ahead_offset_ms: 0,
        }
    }
    pub fn read(&mut self, wall_ms: u64, elapsed_ms: u64) -> Option<EpochReading> {
        let wall = wall_epoch(wall_ms / 1000)?;
        let s = &self.state;
        let tick = match s.last_epoch {
            None => true,
            Some(_) if s.boot != self.boot => Some(wall) != s.tick_wall_epoch,
            Some(_) => {
                Some(wall) != s.tick_wall_epoch
                    || elapsed_ms.saturating_sub(s.tick_elapsed_ms) >= ELAPSED_TICK_MS
            }
        };
        let advertised = match (tick, s.last_epoch) {
            (true, Some(last)) => wall.max(last.checked_add(1)?),
            (true, None) => wall,
            (false, last) => last?,
        };
        if s.last_epoch.is_some() && advertised.saturating_sub(wall) > MAX_AHEAD_EPOCHS {
            let offset = wall_ms as i64 - elapsed_ms as i64;
            match self.ahead_since_ms {
                Some(since)
                    if (offset - self.ahead_offset_ms).abs() <= CONSISTENT_WALL_TOLERANCE_MS =>
                {
                    if elapsed_ms.saturating_sub(since) >= AHEAD_RECOVERY_MS {
                        self.state = EpochState {
                            last_epoch: Some(wall),
                            tick_wall_epoch: Some(wall),
                            tick_elapsed_ms: elapsed_ms,
                            boot: self.boot,
                            generation: self.state.generation.saturating_add(1),
                        };
                        self.ahead_since_ms = None;
                        return Some(EpochReading {
                            epoch: wall,
                            ticked: true,
                            new_generation: true,
                        });
                    }
                }
                _ => {
                    self.ahead_since_ms = Some(elapsed_ms);
                    self.ahead_offset_ms = offset;
                }
            }
        } else {
            self.ahead_since_ms = None;
        }
        if !tick {
            if self.state.boot != self.boot {
                self.state.tick_elapsed_ms = elapsed_ms;
                self.state.boot = self.boot;
            }
            return Some(EpochReading {
                epoch: advertised,
                ticked: false,
                new_generation: false,
            });
        }
        self.state.last_epoch = Some(advertised);
        self.state.tick_wall_epoch = Some(wall);
        self.state.tick_elapsed_ms = elapsed_ms;
        self.state.boot = self.boot;
        Some(EpochReading {
            epoch: advertised,
            ticked: true,
            new_generation: false,
        })
    }
}

/// Section 6 pin rule: only the ID on air, or one that left the air less than
/// 60 s ago, is ever opened; an arbitrary or older epoch never is.
#[derive(Default)]
pub struct OpeningPins {
    current: Option<OpeningEntry>,
    current_off_air_ms: Option<u64>,
    previous: Option<(OpeningEntry, u64)>,
}
impl OpeningPins {
    pub fn on_advertised(&mut self, entry: OpeningEntry, elapsed_ms: u64) {
        if self.current.as_ref() == Some(&entry) {
            self.current_off_air_ms = None;
            return;
        }
        if let Some(old) = self.current.take() {
            self.previous = Some((old, self.current_off_air_ms.unwrap_or(elapsed_ms)));
        }
        self.current = Some(entry);
        self.current_off_air_ms = None;
    }
    pub fn off_air(&mut self, elapsed_ms: u64) {
        if self.current.is_some() && self.current_off_air_ms.is_none() {
            self.current_off_air_ms = Some(elapsed_ms);
        }
    }
    /// Evaluated when a connection is accepted (or a BDO1 is sent).
    pub fn pinned(&self, elapsed_ms: u64) -> Vec<OpeningEntry> {
        let fresh = |off: u64| elapsed_ms.saturating_sub(off) < PREVIOUS_GRACE_MS;
        let mut out = Vec::with_capacity(2);
        if let Some(entry) = &self.current {
            if self.current_off_air_ms.is_none_or(fresh) {
                out.push(entry.clone());
            }
        }
        if let Some((entry, off)) = &self.previous {
            if fresh(*off) {
                out.push(entry.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    const E: u32 = 1_990_656;
    fn at(epoch: u32, plus_s: u64) -> u64 {
        (u64::from(epoch) * EPOCH_SECONDS + plus_s) * 1000
    }
    #[test]
    fn epoch_clock_ticks_on_boundaries_never_goes_down_and_recovers_a_forward_jump() {
        let mut clock = EpochClock::new(EpochState::default(), 7);
        let r = clock.read(at(E, 10), 0).unwrap();
        assert_eq!((r.epoch, r.ticked), (E, true));
        // Restarts inside the epoch reuse E.
        assert_eq!(
            clock.read(at(E, 500), 490_000).unwrap(),
            EpochReading {
                epoch: E,
                ticked: false,
                new_generation: false
            }
        );
        // A boundary ticks once.
        let r = clock.read(at(E + 1, 1), 892_000).unwrap();
        assert_eq!((r.epoch, r.ticked), (E + 1, true));
        // A stuck wall still rotates after 960 s of elapsed time, and E never goes down.
        let base = 892_000 + ELAPSED_TICK_MS;
        let r = clock.read(at(E + 1, 2), base).unwrap();
        assert_eq!((r.epoch, r.ticked), (E + 2, true));
        // A backward wall step leaves a lead above the tolerance; after an hour of
        // consistent wall time a new generation restarts at the wall epoch.
        let back = at(E - 10, 0);
        let r = clock.read(back, base + 1_000).unwrap();
        assert!(r.ticked && r.epoch == E + 3 && !r.new_generation);
        let r = clock.read(back + 1_800_000, base + 1_801_000).unwrap();
        assert!(!r.new_generation);
        let r = clock.read(back + 3_600_000, base + 3_601_000).unwrap();
        assert!(r.new_generation);
        assert_eq!(r.epoch, E - 6);
        assert_eq!(clock.state.generation, 2);
        assert_eq!(clock.state.last_epoch, Some(r.epoch));
        // A lead of two epochs or less starts nothing.
        let mut clock = EpochClock::new(EpochState::default(), 1);
        clock.read(at(E, 0), 0).unwrap();
        let r = clock.read(at(E - 1, 0), 1_000).unwrap();
        assert_eq!(r.epoch, E + 1);
        assert!(
            !clock
                .read(at(E - 1, 0) + 4_000_000, 4_001_000)
                .unwrap()
                .new_generation
        );
    }
    #[test]
    fn after_a_reboot_e_is_reused_only_while_the_wall_shows_the_same_epoch() {
        let mut clock = EpochClock::new(EpochState::default(), 1);
        clock.read(at(E, 0), 5_000_000).unwrap();
        let mut rebooted = EpochClock::new(clock.state.clone(), 2);
        let r = rebooted.read(at(E, 100), 10).unwrap();
        assert_eq!((r.epoch, r.ticked), (E, false));
        assert_eq!(rebooted.state.boot, 2);
        let r = rebooted.read(at(E + 1, 0), 900_000).unwrap();
        assert_eq!((r.epoch, r.ticked), (E + 1, true));
    }
    #[test]
    fn pins_open_only_the_id_on_air_or_one_that_left_less_than_a_minute_ago() {
        let key = [3; 32];
        let node = [9; 16];
        let a = OpeningEntry::own(&key, &node, E, 0).unwrap();
        let b = OpeningEntry::own(&key, &node, E + 1, 0).unwrap();
        let mut pins = OpeningPins::default();
        assert!(pins.pinned(0).is_empty());
        pins.on_advertised(a.clone(), 0);
        assert_eq!(pins.pinned(1), vec![a.clone()]);
        pins.off_air(1_000);
        assert_eq!(pins.pinned(60_999), vec![a.clone()]);
        assert!(pins.pinned(61_000).is_empty());
        pins.on_advertised(b.clone(), 62_000);
        assert_eq!(pins.pinned(62_000), vec![b.clone()]);
        let mut pins = OpeningPins::default();
        pins.on_advertised(a.clone(), 0);
        pins.on_advertised(b.clone(), 900_000);
        assert_eq!(pins.pinned(900_001), vec![b.clone(), a.clone()]);
        assert_eq!(pins.pinned(960_000), vec![b]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    // PROTOCOL "Experimental rotating discovery IDs", Vectors (RotatingDiscoveryTest):
    // salt key 000102..1f, node a1b2..8f90, epoch of 2026-10-10T00:00:00Z.
    const NODE: [u8; 16] = [
        0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f,
        0x90,
    ];
    const EPOCH: u32 = 1_990_656;
    fn key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }
    #[test]
    fn android_salt_rdi_and_opening_vectors_match_byte_for_byte() {
        assert_eq!(wall_epoch(1_791_590_400), Some(EPOCH));
        assert_eq!(EPOCH, 0x001e_6000);
        for (role, epoch, salt_hex, rdi_hex) in [
            (
                0,
                EPOCH,
                "d8cbe82b78824c1ba08d0c4eed26502189e19ccccd180e46c4826c5ba47e7167",
                "44e69c0a7a7d",
            ),
            (
                1,
                EPOCH,
                "dcd9349e228e00ceb4a7ec2e7e023fc0c34c507f88e99934897105ca4fae84c0",
                "bb2d061f6480",
            ),
            (
                2,
                EPOCH,
                "e59970c8523f23a6b1b16a858daf75320e3291d6c86d0329df89a62899804335",
                "974ae410ea91",
            ),
            (
                0,
                EPOCH + 1,
                "7ece98afad0378297e68c9e2b59c1a1f88448a0c70b86d91f192f65d263a763b",
                "67aba670e65a",
            ),
        ] {
            let entry = OpeningEntry::own(&key(), &NODE, epoch, role).unwrap();
            assert_eq!(entry.salt.as_slice(), hex(salt_hex));
            assert_eq!(entry.rdi.as_slice(), hex(rdi_hex));
            assert!(entry.opens(&entry.rdi, &NODE, role).unwrap());
        }
        let entry = OpeningEntry::own(&key(), &NODE, EPOCH, 0).unwrap();
        let one = hex(
            "42444f31010144e69c0a7a7d001e600000d8cbe82b78824c1ba08d0c4eed26502189e19ccccd180e46c4826c5ba47e7167",
        );
        assert_eq!(encode_bdo1(std::slice::from_ref(&entry)).unwrap(), one);
        assert_eq!(decode_bdo1(&one).unwrap(), vec![entry.clone()]);
        assert_eq!(encode_bdo1(&[]).unwrap(), hex("42444f310100"));
        assert!(decode_bdo1(&hex("42444f310100")).unwrap().is_empty());
        // The BDX1 OPENING record is "BDX1" | kind 1 | the same body.
        let bdx1 = [b"BDX1".as_slice(), &[1], &encode_body(&[entry]).unwrap()].concat();
        assert_eq!(
            bdx1,
            hex(
                "42445831010144e69c0a7a7d001e600000d8cbe82b78824c1ba08d0c4eed26502189e19ccccd180e46c4826c5ba47e7167"
            )
        );
    }
    #[test]
    fn contact_and_qr_tag_keys_and_binding_match_the_bmh3_fixture_constants() {
        let lease: [u8; 32] = std::array::from_fn(|i| 0x40 + i as u8);
        assert_eq!(
            contact_tag_key(&lease).unwrap().as_slice(),
            hex("6bba08d6e64fcddca5d2663c383c599ec6c3d44990efa6bb3a0f7532fc0270ae")
        );
        let k_qr: [u8; 32] = std::array::from_fn(|i| 0x80 + i as u8);
        assert_eq!(
            qr_tag_key(&k_qr).unwrap().as_slice(),
            hex("1789e2ace57290b7957dc1fb19fc6de1a6c3d85afe9e0c2f223bee52391c431a")
        );
        let rdi: [u8; 6] = hex("44e69c0a7a7d").try_into().unwrap();
        assert_eq!(
            advert_binding(&rdi, 0, None).as_slice(),
            hex("44e69c0a7a7d0000000000")
        );
        assert_eq!(
            advert_binding(&rdi, 0, Some(EPOCH)).as_slice(),
            hex("44e69c0a7a7d00001e6000")
        );
    }
    #[test]
    fn selection_check_binds_role_node_epoch_window_and_rejects_malformed_openings() {
        let entry = OpeningEntry::own(&key(), &NODE, EPOCH, 0).unwrap();
        let entries = [entry.clone()];
        let check = |selected: &[u8; 6], role, node: &[u8; 16], wall| {
            check_selection(selected, true, role, node, Some(&entries), wall).unwrap()
        };
        for wall in [EPOCH - 2, EPOCH, EPOCH + 2] {
            assert_eq!(check(&entry.rdi, 0, &NODE, wall), Selection::Pass);
        }
        for wall in [EPOCH - 3, EPOCH + 3, 0, u32::MAX] {
            assert_eq!(check(&entry.rdi, 0, &NODE, wall), Selection::OpeningStale);
        }
        let mut other = NODE;
        other[15] ^= 1;
        assert_eq!(
            check(&entry.rdi, 0, &other, EPOCH),
            Selection::OpeningMismatch
        );
        assert_eq!(
            check(&entry.rdi, 1, &NODE, EPOCH),
            Selection::OpeningMismatch
        );
        let mut wrong = entry.rdi;
        wrong[0] ^= 1;
        assert_eq!(check(&wrong, 0, &NODE, EPOCH), Selection::OpeningMismatch);
        // A forged entry whose RDI equals the selection but whose salt does not recompute.
        let mut forged = entry.clone();
        forged.salt[0] ^= 1;
        assert_eq!(
            check_selection(&entry.rdi, true, 0, &NODE, Some(&[forged]), EPOCH).unwrap(),
            Selection::OpeningMismatch
        );
        assert_eq!(
            check_selection(&entry.rdi, true, 0, &NODE, None, EPOCH).unwrap(),
            Selection::OpeningMissing
        );
        assert_eq!(
            check_selection(&entry.rdi, true, 0, &NODE, Some(&[]), EPOCH).unwrap(),
            Selection::OpeningMismatch
        );
        // Version 2: the prefix decides, whatever opening arrives.
        let prefix: [u8; 6] = NODE[..6].try_into().unwrap();
        assert_eq!(
            check_selection(&prefix, false, 0, &NODE, None, 0).unwrap(),
            Selection::Pass
        );
        assert_eq!(
            check_selection(&entry.rdi, false, 0, &NODE, Some(&entries), EPOCH).unwrap(),
            Selection::PrefixMismatch
        );
        let one = encode_bdo1(&entries).unwrap();
        for cut in 0..one.len() {
            assert!(decode_bdo1(&one[..cut]).is_err(), "cut {cut}");
        }
        for (index, value) in [(0, b'X'), (4, 2), (5, 0), (5, 2), (5, 3)] {
            let mut changed = one.clone();
            changed[index] = value;
            assert!(decode_bdo1(&changed).is_err(), "byte {index}");
        }
        assert!(decode_bdo1(&[one.as_slice(), &[0]].concat()).is_err());
        let three = vec![entry.clone(), entry.clone(), entry];
        assert!(encode_body(&three).is_err());
        assert!(decode_body(&[]).is_err());
    }
}

/// GAPS P3: rotating-ids 008-011 (role 3 salt and ID) and 031 (a different
/// route token gives a different recovery hint).
#[cfg(test)]
mod shared_vectors {
    use serde_json::Value;
    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }
    fn vector(id: &str) -> Value {
        let doc: Value = serde_json::from_str(include_str!("../tests/vectors/conformance/rotating-ids.json")).unwrap();
        doc["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone()
    }
    #[test]
    fn role_3_salts_and_ids_match_the_shared_vectors() {
        for id in ["rotating-ids-008", "rotating-ids-009", "rotating-ids-010", "rotating-ids-011"] {
            let v = vector(id);
            let key: [u8; 32] = hex(v["inputs"]["salt_key"].as_str().unwrap()).try_into().unwrap();
            let node: [u8; 16] = hex(v["inputs"]["node_id"].as_str().unwrap()).try_into().unwrap();
            let epoch = v["inputs"]["epoch"].as_u64().unwrap() as u32;
            let role = v["inputs"]["role"].as_u64().unwrap() as u8;
            let salt = super::salt(&key, epoch, role).unwrap();
            assert_eq!(salt.to_vec(), hex(v["expected"]["salt"].as_str().unwrap()), "{id}");
            assert_eq!(super::rdi(&node, epoch, role, &salt).unwrap().to_vec(), hex(v["expected"]["rdi"].as_str().unwrap()), "{id}");
        }
    }
    #[test]
    fn a_different_route_token_gives_a_different_recovery_hint() {
        let v = vector("rotating-ids-031");
        let i = &v["inputs"];
        let key: [u8; 32] = hex(i["hint_key"].as_str().unwrap()).try_into().unwrap();
        let node: [u8; 16] = hex(i["failed_node_id"].as_str().unwrap()).try_into().unwrap();
        let viewer: [u8; 6] = hex(i["viewer_rdi"].as_str().unwrap()).try_into().unwrap();
        let logical = u32::from_str_radix(i["logical_token_hex"].as_str().unwrap(), 16).unwrap();
        let route = u32::from_str_radix(i["route_token_hex"].as_str().unwrap(), 16).unwrap();
        let hint = crate::bluetooth::advertise::recovery_hint(&key, &node, logical, &viewer, route).unwrap();
        assert_eq!(hint.to_vec(), hex(v["expected"]["hint"].as_str().unwrap()));
        assert_ne!(hint.to_vec(), hex(v["expected"]["differs_from"].as_str().unwrap()));
    }
}
