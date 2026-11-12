//! Design SR in-call link renewal: the renewal key, BME1 type-12 records and
//! the end-to-end capability. No wire bytes are invented here: the layouts and
//! KDF labels are PROTOCOL "Experimental in-call link renewal (Design SR)".
use crate::crypto::{hkdf_sha256, hmac_sha256};
use std::sync::OnceLock;
use zeroize::Zeroizing;

pub const CAPABILITY_BIT: u8 = 0x10;
pub const CONTENT_TYPE: u8 = 12;

/// The switch: Experimental, off unless `MESHLIVE_CALL_RENEWAL=1`.
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(forced) = test_switch::forced() {
        return forced;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_CALL_RENEWAL").ok().as_deref() == Some("1"))
}

/// Tests: hold the switch on for the length of a test (tests run one at a time).
#[cfg(test)]
pub(crate) mod test_switch {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ON: AtomicBool = AtomicBool::new(false);
    pub fn forced() -> Option<bool> {
        ON.load(Ordering::SeqCst).then_some(true)
    }
    pub struct Held;
    pub fn on() -> Held {
        ON.store(true, Ordering::SeqCst);
        Held
    }
    impl Drop for Held {
        fn drop(&mut self) {
            ON.store(false, Ordering::SeqCst);
        }
    }
}

/// The endpoint hello transport byte with the renewal bit. Only the call hello
/// builders use it (never a BVMID2 verification hello).
pub fn transport(base: u8, renewal: bool) -> u8 {
    if renewal { base | CAPABILITY_BIT } else { base }
}
/// Renewal is negotiated only when this side enabled it and both signed
/// end-to-end hellos carry bit 4.
pub fn negotiated(local: bool, own: u8, peer: u8) -> bool {
    local && own & CAPABILITY_BIT != 0 && peer & CAPABILITY_BIT != 0
}

pub const ERROR_MALFORMED: &str = "malformed RENEW record";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Offer { ticket: [u8; 8], role: u8 },
    Armed { ticket: [u8; 8] },
    Refused { ticket: [u8; 8], reason: u8 },
    Bind { ticket: [u8; 8], next_sequence: u64 },
}

/// REFUSED reasons (PROTOCOL): the armed phone will not arm, or the offerer
/// kept the old link.
pub mod reason {
    pub const NOT_DIRECT: u8 = 1;
    pub const LIMIT: u8 = 2;
    pub const REGISTRATION: u8 = 3;
    pub const ADVERTISING: u8 = 4;
    pub const COLLISION: u8 = 5;
    pub const DISABLED: u8 = 6;
    pub const ABANDONED: u8 = 7;
}

/// `K_renew` from the two directional BME1 content keys; role-free.
pub fn k_renew(k1: &[u8; 32], k2: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, &'static str> {
    // Unsigned byte order: Rust array comparison is lexicographic on u8.
    let (low, high) = if k1 <= k2 { (k1, k2) } else { (k2, k1) };
    let mut ikm = Zeroizing::new([0u8; 64]);
    ikm[..32].copy_from_slice(low);
    ikm[32..].copy_from_slice(high);
    hkdf_sha256::<32>(&ikm[..], b"BluetoothVideoMesh/SR1", b"renew-key").map_err(|_| "hkdf")
}

/// The per-attempt advert service UUID (version 4, variant 10).
pub fn advert_uuid(key: &[u8; 32], call_id: &[u8; 16], ticket: &[u8; 8]) -> Result<[u8; 16], &'static str> {
    let mut input = Vec::with_capacity(7 + 16 + 8);
    input.extend_from_slice(b"SR1 srv");
    input.extend_from_slice(call_id);
    input.extend_from_slice(ticket);
    let mac = hmac_sha256(key, &input).map_err(|_| "hmac")?;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&mac[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(uuid)
}

pub fn encode(op: &Op) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push(match op {
        Op::Offer { .. } => 1,
        Op::Armed { .. } => 2,
        Op::Refused { .. } => 3,
        Op::Bind { .. } => 4,
    });
    match op {
        Op::Offer { ticket, role } => {
            out.extend_from_slice(ticket);
            out.push(*role);
        }
        Op::Armed { ticket } => out.extend_from_slice(ticket),
        Op::Refused { ticket, reason } => {
            out.extend_from_slice(ticket);
            out.push(*reason);
        }
        Op::Bind { ticket, next_sequence } => {
            out.extend_from_slice(ticket);
            out.extend_from_slice(&next_sequence.to_be_bytes());
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<Op, &'static str> {
    let mut ticket = [0u8; 8];
    match bytes {
        [1, rest @ ..] if rest.len() == 9 => {
            ticket.copy_from_slice(&rest[..8]);
            if ticket == [0; 8] || rest[8] > 1 {
                return Err(ERROR_MALFORMED);
            }
            Ok(Op::Offer { ticket, role: rest[8] })
        }
        [2, rest @ ..] if rest.len() == 8 => {
            ticket.copy_from_slice(rest);
            if ticket == [0; 8] {
                return Err(ERROR_MALFORMED);
            }
            Ok(Op::Armed { ticket })
        }
        [3, rest @ ..] if rest.len() == 9 => {
            ticket.copy_from_slice(&rest[..8]);
            if ticket == [0; 8] || !(1..=7).contains(&rest[8]) {
                return Err(ERROR_MALFORMED);
            }
            Ok(Op::Refused { ticket, reason: rest[8] })
        }
        [4, rest @ ..] if rest.len() == 16 => {
            ticket.copy_from_slice(&rest[..8]);
            let mut sequence = [0u8; 8];
            sequence.copy_from_slice(&rest[8..]);
            let next_sequence = u64::from_be_bytes(sequence);
            if ticket == [0; 8] || next_sequence == 0 {
                return Err(ERROR_MALFORMED);
            }
            Ok(Op::Bind { ticket, next_sequence })
        }
        _ => Err(ERROR_MALFORMED),
    }
}

/// The new link's hop keys: bytes 0-31 offerer to armed, 32-63 armed to offerer.
pub fn hop_keys(
    key: &[u8; 32],
    call_id: &[u8; 16],
    ticket: &[u8; 8],
    challenge: &[u8; 16],
    nonce: &[u8; 16],
) -> Result<Zeroizing<[u8; 64]>, &'static str> {
    let mut salt = [0u8; 32];
    salt[..16].copy_from_slice(challenge);
    salt[16..].copy_from_slice(nonce);
    let mut info = Vec::with_capacity(7 + 16 + 8);
    info.extend_from_slice(b"SR1 hop");
    info.extend_from_slice(call_id);
    info.extend_from_slice(ticket);
    hkdf_sha256::<64>(key, &salt, &info).map_err(|_| "hkdf")
}

// ---------------------------------------------------------------------------
// The renewal opening (new GATT link, before any BMS1 byte).
// ---------------------------------------------------------------------------

pub const CHALLENGE_LEN: usize = 20;
pub const PROOF_LEN: usize = 60;
pub const CONFIRM_LEN: usize = 36;
/// A connection without a valid proof this long after its challenge is dropped.
pub const PROOF_MS: u64 = 1_000;
/// At most this many connections are challenged per attempt.
pub const MAX_CHALLENGES: u8 = 8;

/// `"SR1C" | challenge (16)`.
pub fn challenge_frame(challenge: &[u8; 16]) -> [u8; CHALLENGE_LEN] {
    let mut frame = [0u8; CHALLENGE_LEN];
    frame[..4].copy_from_slice(b"SR1C");
    frame[4..].copy_from_slice(challenge);
    frame
}
/// The challenge of a CHALLENGE frame; None for any other bytes.
pub fn parse_challenge(frame: &[u8]) -> Option<[u8; 16]> {
    if frame.len() != CHALLENGE_LEN || &frame[..4] != b"SR1C" {
        return None;
    }
    let mut challenge = [0u8; 16];
    challenge.copy_from_slice(&frame[4..]);
    Some(challenge)
}

fn opening_mac(
    label: &[u8],
    key: &[u8; 32],
    call_id: &[u8; 16],
    ticket: &[u8; 8],
    challenge: &[u8; 16],
    nonce: &[u8; 16],
) -> Option<Zeroizing<[u8; 32]>> {
    let mut input = Vec::with_capacity(label.len() + 16 + 8 + 16 + 16);
    input.extend_from_slice(label);
    input.extend_from_slice(call_id);
    input.extend_from_slice(ticket);
    input.extend_from_slice(challenge);
    input.extend_from_slice(nonce);
    hmac_sha256(key, &input).ok()
}
fn same(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Armed,
    Offerer,
}

/// One side of the opening. The armed side challenges every connection afresh
/// (`next_connection`), the offerer answers the challenge it was sent.
pub struct Opening {
    side: Side,
    key: Zeroizing<[u8; 32]>,
    call_id: [u8; 16],
    ticket: [u8; 8],
    challenge: [u8; 16],
    nonce: [u8; 16],
    have_challenge: bool,
    /// Armed: a valid PROOF consumed the ticket. Offerer: a valid CONFIRM arrived.
    proved: bool,
    challenged: u8,
    challenged_at_ms: u64,
}
impl Opening {
    /// The armed side, its first connection challenged with `challenge`.
    pub fn armed(key: Zeroizing<[u8; 32]>, call_id: [u8; 16], ticket: [u8; 8], challenge: [u8; 16]) -> Self {
        Self {
            side: Side::Armed,
            key,
            call_id,
            ticket,
            challenge,
            nonce: [0; 16],
            have_challenge: true,
            proved: false,
            challenged: 1,
            challenged_at_ms: 0,
        }
    }
    /// The offerer with its fresh 16-byte nonce.
    pub fn offerer(key: Zeroizing<[u8; 32]>, call_id: [u8; 16], ticket: [u8; 8], nonce: [u8; 16]) -> Self {
        Self {
            side: Side::Offerer,
            key,
            call_id,
            ticket,
            challenge: [0; 16],
            nonce,
            have_challenge: false,
            proved: false,
            challenged: 0,
            challenged_at_ms: 0,
        }
    }
    /// The clock reading at which the first challenge went out.
    pub fn at(mut self, now_ms: u64) -> Self {
        self.challenged_at_ms = now_ms;
        self
    }
    pub fn challenge(&self) -> [u8; 16] {
        self.challenge
    }
    /// A new connection to the armed side: a fresh challenge. False once 8
    /// connections were challenged, or after the ticket was consumed.
    pub fn next_connection(&mut self, challenge: [u8; 16], now_ms: u64) -> bool {
        if self.side != Side::Armed || self.proved || self.challenged >= MAX_CHALLENGES {
            return false;
        }
        self.challenged += 1;
        self.challenge = challenge;
        self.challenged_at_ms = now_ms;
        true
    }
    pub fn connections_challenged(&self) -> u8 {
        self.challenged
    }
    /// The connection waited 1,000 ms without a valid PROOF.
    pub fn expired(&self, now_ms: u64) -> bool {
        self.side == Side::Armed && !self.proved && now_ms.saturating_sub(self.challenged_at_ms) >= PROOF_MS
    }
    /// Offerer: the PROOF frame answering `challenge`.
    pub fn proof(&mut self, challenge: &[u8; 16]) -> Option<[u8; PROOF_LEN]> {
        if self.side != Side::Offerer {
            return None;
        }
        self.challenge = *challenge;
        self.have_challenge = true;
        let mac = opening_mac(b"SR1 offer", &self.key, &self.call_id, &self.ticket, &self.challenge, &self.nonce)?;
        let mut frame = [0u8; PROOF_LEN];
        frame[..4].copy_from_slice(b"SR1P");
        frame[4..12].copy_from_slice(&self.ticket);
        frame[12..28].copy_from_slice(&self.nonce);
        frame[28..].copy_from_slice(&mac[..]);
        Some(frame)
    }
    /// Armed: a PROOF for this call, ticket and challenge. A valid proof
    /// consumes the ticket; nothing else changes any state.
    pub fn accept_proof(&mut self, frame: &[u8]) -> bool {
        if self.side != Side::Armed || self.proved || !self.have_challenge || frame.len() != PROOF_LEN {
            return false;
        }
        if &frame[..4] != b"SR1P" || !same(&frame[4..12], &self.ticket) {
            return false;
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&frame[12..28]);
        let Some(expected) = opening_mac(b"SR1 offer", &self.key, &self.call_id, &self.ticket, &self.challenge, &nonce)
        else {
            return false;
        };
        if !same(&frame[28..], &expected[..]) {
            return false;
        }
        self.nonce = nonce;
        self.proved = true;
        true
    }
    /// Armed: the CONFIRM frame, only after a valid proof.
    pub fn confirm(&self) -> Option<[u8; CONFIRM_LEN]> {
        if self.side != Side::Armed || !self.proved {
            return None;
        }
        let mac = opening_mac(b"SR1 armed", &self.key, &self.call_id, &self.ticket, &self.challenge, &self.nonce)?;
        let mut frame = [0u8; CONFIRM_LEN];
        frame[..4].copy_from_slice(b"SR1A");
        frame[4..].copy_from_slice(&mac[..]);
        Some(frame)
    }
    /// Offerer: the armed side's CONFIRM; a wrong one is an advert copied from the air.
    pub fn accept_confirm(&mut self, frame: &[u8]) -> bool {
        if self.side != Side::Offerer || !self.have_challenge || frame.len() != CONFIRM_LEN || &frame[..4] != b"SR1A" {
            return false;
        }
        let Some(expected) = opening_mac(b"SR1 armed", &self.key, &self.call_id, &self.ticket, &self.challenge, &self.nonce)
        else {
            return false;
        };
        if !same(&frame[4..], &expected[..]) {
            return false;
        }
        self.proved = true;
        true
    }
    pub fn proved(&self) -> bool {
        self.proved
    }
    pub fn ticket(&self) -> [u8; 8] {
        self.ticket
    }
    /// The hop keys once the opening succeeded on this side.
    pub fn hop_keys(&self) -> Option<Zeroizing<[u8; 64]>> {
        if !self.proved {
            return None;
        }
        hop_keys(&self.key, &self.call_id, &self.ticket, &self.challenge, &self.nonce).ok()
    }
}

// ---------------------------------------------------------------------------
// The offerer and armed state machines (no I/O: the owner performs `Action`s).
// ---------------------------------------------------------------------------

/// The call must have been ACTIVE this long.
pub const ACTIVE_MS: u64 = 10_000;
/// Soft state: no record for 3 s, then 1 s more while this side's writes complete.
pub const TRIGGER_SILENCE_MS: u64 = 4_000;
/// "Own writes still complete" = a Bluetooth write succeeded this recently (local).
pub const WRITE_FRESH_MS: u64 = 2_000;
pub const SCAN_MS: u64 = 6_000;
pub const ADVERT_MS: u64 = 7_000;
/// OFFER to BIND: after this the offerer ends the call link lost.
pub const BOUND_MS: u64 = 12_000;
/// Server registration and advert start, from OFFER.
pub const REGISTRATION_MS: u64 = 3_000;
/// No connect starts later than this after OFFER (local, so a connect can still finish).
pub const LAST_CONNECT_MS: u64 = 11_000;
pub const MAX_ATTEMPTS: u8 = 2;
pub const ATTEMPT_GAP_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Offerer: OFFER sent, scanning (the old link is kept until a sighting).
    Scanning,
    /// Offerer: the old link is closed, connecting and running the opening.
    Connecting,
    /// Armed side: registering the server and starting the advert.
    Arming,
    Armed,
    Bound,
    /// This side kept the old link (nothing found, refused, peer heard).
    Abandoned,
    Failed,
}

/// What the call looks like to the renewal, supplied by the session each tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub enabled: bool,
    pub relay_hops: u8,
    /// Checked: the link's authenticated node ID is the end-to-end node ID.
    pub direct: bool,
    /// The hop link is GATT (not CoC/Classic/Wi-Fi) and Bluetooth is not retired.
    pub gatt: bool,
    pub active: bool,
    pub verification: bool,
    /// A Wi-Fi attempt is in progress (RD12: refuse until it ends).
    pub wifi_attempt: bool,
    /// Another carrier (Wi-Fi) still carries the call.
    pub carrier_alive: bool,
    pub active_ms: u64,
    pub silence_ms: u64,
    /// Age of the last successful Bluetooth write (None: never).
    pub write_age_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A RENEW record on the old link (best effort).
    Send(Op),
    /// Scan for the derived service UUID.
    Scan { uuid: [u8; 16], until_ms: u64 },
    /// Register the renewal GATT server (answer `on_registered`).
    RegisterServer { uuid: [u8; 16], until_ms: u64 },
    /// Advertise the derived UUID (answer `on_advertising`).
    Advertise { uuid: [u8; 16], until_ms: u64 },
    /// The advert window is over; connections already made may finish.
    StopAdvert,
    /// Stop the old link's writer (joined) and close it.
    CloseOldLink,
    /// Connect to the sighting and run the opening.
    Connect { uuid: [u8; 16], until_ms: u64 },
    /// Stop scan, advert and server; the old link and the call are untouched.
    Stop,
    /// Re-assert media state on the new link.
    Reassert,
    EndCall(&'static str),
    Log { role: &'static str, phase: &'static str, reason: &'static str, attempt: u8 },
}

/// `rpa`, `static` or `other` from the two top bits of a 48-bit address.
pub fn address_kind(address: u64) -> &'static str {
    match (address >> 46) & 3 {
        1 => "rpa",
        3 => "static",
        _ => "other",
    }
}

/// One call's renewal state. Times are the owner's millisecond clock.
pub struct Renewal {
    call_id: [u8; 16],
    /// This side is the end-to-end caller (role 0).
    caller: bool,
    key: Option<Zeroizing<[u8; 32]>>,
    phase: Phase,
    offerer: bool,
    attempts: u8,
    last_attempt_ms: Option<u64>,
    /// Before this side's own pending offer was counted (for a yield).
    before_offer: (u8, Option<u64>),
    ticket: [u8; 8],
    started_ms: u64,
    scan_until_ms: u64,
    old_closed: bool,
    advert_stopped: bool,
    registered: bool,
    old_failed: bool,
}

/// A fresh 8-byte ticket, never all zero; None if the system RNG fails.
fn draw_ticket() -> Option<[u8; 8]> {
    for _ in 0..4 {
        if let Ok(random) = crate::crypto::random_nonce() {
            let mut ticket = [0u8; 8];
            ticket.copy_from_slice(&random[..8]);
            if ticket != [0; 8] {
                return Some(ticket);
            }
        }
    }
    None
}

impl Renewal {
    pub fn new(call_id: [u8; 16], caller: bool) -> Self {
        Self {
            call_id,
            caller,
            key: None,
            phase: Phase::Idle,
            offerer: false,
            attempts: 0,
            last_attempt_ms: None,
            before_offer: (0, None),
            ticket: [0; 8],
            started_ms: 0,
            scan_until_ms: 0,
            old_closed: false,
            advert_stopped: false,
            registered: false,
            old_failed: false,
        }
    }
    /// Renewal was negotiated: keep `K_renew` (memory only, zeroed on drop).
    pub fn set_key(&mut self, key: Zeroizing<[u8; 32]>) {
        self.key = Some(key);
    }
    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn attempts(&self) -> u8 {
        self.attempts
    }
    pub fn ticket(&self) -> [u8; 8] {
        self.ticket
    }
    /// The role the current attempt has (log name).
    pub fn role(&self) -> &'static str {
        if self.offerer { "offerer" } else { "armed" }
    }
    /// A renewal runs: it owns the call's liveness and no Wi-Fi attempt starts.
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Scanning | Phase::Connecting | Phase::Arming | Phase::Armed)
    }
    pub fn owns_liveness(&self) -> bool {
        self.running()
    }
    /// The old link is closed (the offerer broke it, or the armed side saw it fail).
    pub fn old_closed(&self) -> bool {
        self.old_closed
    }
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }
    fn uuid(&self) -> Option<[u8; 16]> {
        advert_uuid(self.key.as_ref()?, &self.call_id, &self.ticket).ok()
    }
    fn log(&self, phase: &'static str, reason: &'static str) -> Action {
        Action::Log { role: self.role(), phase, reason, attempt: self.attempts }
    }
    fn idle(&self) -> bool {
        !self.running()
    }
    fn since_last_ok(&self, now_ms: u64) -> bool {
        self.last_attempt_ms.is_none_or(|at| now_ms.saturating_sub(at) >= ATTEMPT_GAP_MS)
    }
    fn in_scope(facts: &Facts) -> bool {
        facts.direct && facts.relay_hops == 0 && facts.gatt && !facts.verification
    }

    /// Time and facts: starts an offer, enforces the scan, advert and call bounds.
    pub fn tick(&mut self, now_ms: u64, facts: &Facts) -> Vec<Action> {
        let mut out = Vec::new();
        let elapsed = now_ms.saturating_sub(self.started_ms);
        match self.phase {
            Phase::Scanning | Phase::Connecting => {
                if elapsed >= BOUND_MS {
                    self.fail_at_bound(facts, &mut out);
                } else if self.phase == Phase::Scanning && now_ms >= self.scan_until_ms {
                    self.scan_ended(now_ms, &mut out);
                }
            }
            Phase::Arming => {
                if elapsed >= REGISTRATION_MS {
                    let reason = if self.registered { reason::ADVERTISING } else { reason::REGISTRATION };
                    self.refuse_arming(reason, &mut out);
                }
            }
            Phase::Armed => {
                if elapsed >= ADVERT_MS && !self.advert_stopped {
                    self.advert_stopped = true;
                    out.push(Action::StopAdvert);
                }
                if elapsed >= BOUND_MS {
                    self.phase = Phase::Failed;
                    out.push(Action::Stop);
                    if self.old_failed && !facts.carrier_alive {
                        out.push(self.log("failed", "link_lost"));
                        out.push(Action::EndCall("link_lost"));
                    } else {
                        out.push(self.log("abandoned", "no_bind"));
                    }
                }
            }
            _ => {}
        }
        if self.idle() && self.offer_due(now_ms, facts) {
            self.start_offer(now_ms, &mut out);
        }
        out
    }

    fn offer_due(&self, now_ms: u64, facts: &Facts) -> bool {
        facts.enabled
            && self.has_key()
            && Self::in_scope(facts)
            && facts.active
            && !facts.wifi_attempt
            // A live Wi-Fi carrier is never put at risk by a renewal that blocks the worker.
            && !facts.carrier_alive
            && facts.active_ms >= ACTIVE_MS
            && facts.silence_ms >= TRIGGER_SILENCE_MS
            && facts.write_age_ms.is_some_and(|age| age < WRITE_FRESH_MS)
            && self.attempts < MAX_ATTEMPTS
            && self.since_last_ok(now_ms)
    }

    fn start_offer(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        let Some(_) = self.key else { return };
        let Some(ticket) = draw_ticket() else { return };
        self.ticket = ticket;
        self.before_offer = (self.attempts, self.last_attempt_ms);
        self.attempts += 1;
        self.last_attempt_ms = Some(now_ms);
        self.offerer = true;
        self.phase = Phase::Scanning;
        self.started_ms = now_ms;
        self.scan_until_ms = now_ms + SCAN_MS;
        self.old_closed = false;
        self.advert_stopped = false;
        self.registered = false;
        self.old_failed = false;
        let Some(uuid) = self.uuid() else {
            self.phase = Phase::Failed;
            return;
        };
        out.push(self.log("offer", ""));
        out.push(Action::Send(Op::Offer { ticket: self.ticket, role: if self.caller { 0 } else { 1 } }));
        out.push(Action::Scan { uuid, until_ms: self.scan_until_ms });
    }

    /// The scan window ended without a usable sighting.
    fn scan_ended(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        if !self.old_closed {
            // The old link was never broken: keep it, tell the armed phone.
            self.phase = Phase::Abandoned;
            out.push(Action::Stop);
            out.push(Action::Send(Op::Refused { ticket: self.ticket, reason: reason::ABANDONED }));
            out.push(self.log("abandoned", "not_found"));
        } else if now_ms.saturating_sub(self.started_ms) < LAST_CONNECT_MS {
            // The link is already gone: look again while time remains.
            let until = (now_ms + SCAN_MS).min(self.started_ms + LAST_CONNECT_MS);
            self.scan_until_ms = until;
            if let Some(uuid) = self.uuid() {
                out.push(Action::Scan { uuid, until_ms: until });
            }
        }
    }

    fn fail_at_bound(&mut self, facts: &Facts, out: &mut Vec<Action>) {
        self.phase = Phase::Failed;
        out.push(Action::Stop);
        if facts.carrier_alive {
            out.push(self.log("failed", "carrier_alive"));
        } else {
            out.push(self.log("failed", "link_lost"));
            out.push(Action::EndCall("link_lost"));
        }
    }

    fn refuse_arming(&mut self, why: u8, out: &mut Vec<Action>) {
        self.phase = Phase::Failed;
        out.push(Action::Stop);
        out.push(Action::Send(Op::Refused { ticket: self.ticket, reason: why }));
        out.push(self.log("refused", if why == reason::REGISTRATION { "registration" } else { "advertising" }));
    }

    fn refuse(&self, ticket: [u8; 8], why: u8, tag: &'static str) -> Vec<Action> {
        vec![Action::Send(Op::Refused { ticket, reason: why }), self.log("refused", tag)]
    }

    /// A RENEW record from the peer on the old link.
    pub fn on_record(&mut self, now_ms: u64, op: &Op, facts: &Facts) -> Vec<Action> {
        match *op {
            Op::Offer { ticket, .. } => self.on_offer(now_ms, ticket, facts),
            Op::Armed { ticket } => {
                if self.offerer && ticket == self.ticket && matches!(self.phase, Phase::Scanning | Phase::Connecting) {
                    vec![self.log("armed", "")]
                } else {
                    Vec::new()
                }
            }
            Op::Refused { ticket, reason: why } => {
                if ticket != self.ticket {
                    return Vec::new();
                }
                // The armed phone will not arm: this side keeps the old link.
                if self.offerer && self.phase == Phase::Scanning && !self.old_closed {
                    self.phase = Phase::Abandoned;
                    return vec![Action::Stop, self.log("refused", reason_name(why))];
                }
                // The offerer kept the old link: disarm.
                if !self.offerer && matches!(self.phase, Phase::Arming | Phase::Armed) && why == reason::ABANDONED {
                    self.phase = Phase::Abandoned;
                    return vec![Action::Stop, self.log("abandoned", "peer_kept_old_link")];
                }
                Vec::new()
            }
            // A BIND is only valid as the first record of a new link (never here).
            Op::Bind { .. } => vec![self.log("refused", "bind_on_old_link")],
        }
    }

    fn on_offer(&mut self, now_ms: u64, ticket: [u8; 8], facts: &Facts) -> Vec<Action> {
        if !facts.enabled || !self.has_key() {
            return self.refuse(ticket, reason::DISABLED, "disabled");
        }
        if !Self::in_scope(facts) {
            return self.refuse(ticket, reason::NOT_DIRECT, "not_direct");
        }
        let mut out = Vec::new();
        match self.phase {
            Phase::Scanning if self.offerer => {
                // Both sides offered: the caller (role 0) keeps its offer.
                if self.caller {
                    return self.refuse(ticket, reason::COLLISION, "collision");
                }
                // The callee yields: its offer does not count as an attempt.
                self.attempts = self.before_offer.0;
                self.last_attempt_ms = self.before_offer.1;
                self.phase = Phase::Idle;
                self.offerer = false;
                out.push(Action::Stop);
                out.push(self.log("yielded", "collision"));
            }
            Phase::Connecting | Phase::Arming | Phase::Armed => {
                return self.refuse(ticket, reason::LIMIT, "busy");
            }
            _ => {}
        }
        if !facts.active
            || facts.active_ms < ACTIVE_MS
            || facts.wifi_attempt
            || facts.carrier_alive
            || self.attempts >= MAX_ATTEMPTS
            || !self.since_last_ok(now_ms)
        {
            out.extend(self.refuse(ticket, reason::LIMIT, "limit"));
            return out;
        }
        self.ticket = ticket;
        self.offerer = false;
        self.attempts += 1;
        self.last_attempt_ms = Some(now_ms);
        self.phase = Phase::Arming;
        self.started_ms = now_ms;
        self.old_closed = false;
        self.old_failed = false;
        self.advert_stopped = false;
        self.registered = false;
        let Some(uuid) = self.uuid() else {
            self.phase = Phase::Failed;
            return self.refuse(ticket, reason::DISABLED, "disabled");
        };
        out.push(self.log("offer", ""));
        out.push(Action::RegisterServer { uuid, until_ms: now_ms + REGISTRATION_MS });
        out
    }

    /// The renewal GATT server was registered (or could not be).
    pub fn on_registered(&mut self, now_ms: u64, ok: bool) -> Vec<Action> {
        if self.phase != Phase::Arming {
            return vec![Action::Stop];
        }
        let late = now_ms.saturating_sub(self.started_ms) > REGISTRATION_MS;
        if !ok || late {
            let mut out = Vec::new();
            self.refuse_arming(reason::REGISTRATION, &mut out);
            return out;
        }
        self.registered = true;
        match self.uuid() {
            Some(uuid) => vec![Action::Advertise { uuid, until_ms: self.started_ms + ADVERT_MS }],
            None => {
                let mut out = Vec::new();
                self.refuse_arming(reason::ADVERTISING, &mut out);
                out
            }
        }
    }

    /// The advert started (or could not). ARMED goes on the old link, best effort.
    pub fn on_advertising(&mut self, now_ms: u64, ok: bool) -> Vec<Action> {
        if self.phase != Phase::Arming {
            return vec![Action::Stop];
        }
        let late = now_ms.saturating_sub(self.started_ms) > REGISTRATION_MS;
        if !ok || late {
            let mut out = Vec::new();
            self.refuse_arming(reason::ADVERTISING, &mut out);
            return out;
        }
        self.phase = Phase::Armed;
        vec![Action::Send(Op::Armed { ticket: self.ticket }), self.log("armed", "")]
    }

    /// The offerer heard a record other than RENEW from the peer. Before the
    /// break the old link works after all: abandon.
    pub fn on_peer_heard(&mut self, _now_ms: u64) -> Vec<Action> {
        if self.offerer && self.phase == Phase::Scanning && !self.old_closed {
            self.phase = Phase::Abandoned;
            return vec![
                Action::Stop,
                Action::Send(Op::Refused { ticket: self.ticket, reason: reason::ABANDONED }),
                self.log("abandoned", "peer_heard"),
            ];
        }
        Vec::new()
    }

    /// The scanner saw the advert. Break the old link and connect, or ignore it.
    pub fn on_found(&mut self, now_ms: u64) -> Vec<Action> {
        if !self.offerer || self.phase != Phase::Scanning {
            return Vec::new();
        }
        let elapsed = now_ms.saturating_sub(self.started_ms);
        if now_ms >= self.scan_until_ms || elapsed >= LAST_CONNECT_MS {
            // Too late to use: the scan window or the connect limit passed.
            return Vec::new();
        }
        let Some(uuid) = self.uuid() else { return Vec::new() };
        let mut out = vec![self.log("found", "")];
        if !self.old_closed {
            self.old_closed = true;
            out.push(Action::CloseOldLink);
        }
        self.phase = Phase::Connecting;
        out.push(Action::Connect { uuid, until_ms: self.started_ms + LAST_CONNECT_MS });
        out
    }

    /// The scan failed outright (no scan slot, radio error): same as nothing found.
    pub fn on_scan_failed(&mut self, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        if self.offerer && self.phase == Phase::Scanning {
            self.scan_until_ms = now_ms;
            self.scan_ended(now_ms, &mut out);
        }
        out
    }

    /// Connecting or the opening failed before CONFIRM: look again while time remains.
    pub fn on_connect_failed(&mut self, now_ms: u64) -> Vec<Action> {
        let mut out = vec![self.log("connect_timeout", "")];
        if self.offerer && self.phase == Phase::Connecting {
            self.phase = Phase::Scanning;
            self.scan_until_ms = now_ms;
            self.scan_ended(now_ms, &mut out);
        }
        out
    }

    /// May the offerer still send BIND (the 12 s bound is checked first)?
    pub fn bind_allowed(&self, now_ms: u64) -> bool {
        self.offerer
            && self.phase == Phase::Connecting
            && now_ms.saturating_sub(self.started_ms) < BOUND_MS
    }

    /// Armed side: may a BIND that arrived on the proved connection be accepted?
    pub fn accept_bind_allowed(&self, now_ms: u64, ticket: &[u8; 8]) -> bool {
        !self.offerer
            && self.phase == Phase::Armed
            && *ticket == self.ticket
            && now_ms.saturating_sub(self.started_ms) < BOUND_MS
    }

    /// The BIND was written (offerer) or accepted (armed): the new link carries the call.
    pub fn on_bound(&mut self, _now_ms: u64) -> Vec<Action> {
        let valid = if self.offerer {
            self.phase == Phase::Connecting
        } else {
            self.phase == Phase::Armed
        };
        if !valid {
            return Vec::new();
        }
        self.phase = Phase::Bound;
        vec![Action::Stop, self.log("bound", ""), Action::Reassert]
    }

    /// The attempt failed after the old link was given up on or the ticket was
    /// consumed (an invalid first record on the new link): nothing more runs.
    pub fn on_failed(&mut self, why: &'static str) -> Vec<Action> {
        if !self.running() {
            return Vec::new();
        }
        self.phase = Phase::Failed;
        vec![Action::Stop, self.log("failed", why)]
    }

    /// The old link failed or was closed under the armed side.
    pub fn on_old_link_failed(&mut self) {
        self.old_failed = true;
        // The offerer's own link is gone: it can no longer be "kept", so it keeps looking
        // until the connect limit instead of giving up when the scan window ends.
        if self.offerer {
            self.old_closed = true;
        }
    }

    /// The call ended or the session closed: nothing runs.
    pub fn end(&mut self) -> Vec<Action> {
        if self.running() {
            self.phase = Phase::Idle;
            return vec![Action::Stop];
        }
        Vec::new()
    }
}

fn reason_name(why: u8) -> &'static str {
    match why {
        reason::NOT_DIRECT => "not_direct",
        reason::LIMIT => "limit",
        reason::REGISTRATION => "registration",
        reason::ADVERTISING => "advertising",
        reason::COLLISION => "collision",
        reason::DISABLED => "disabled",
        reason::ABANDONED => "abandoned",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: [u8; 32] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f];
    const K2: [u8; 32] = [0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f];
    const CALL: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const TICKET: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn k_renew_is_role_free_and_binds_both_content_keys() {
        let key = k_renew(&K1, &K2).unwrap();
        assert_eq!(key.as_slice(), hex("b6871ec67a82939d30a541ae9ddcadd89c3ae8a793b963fa5e6df2108cc88eee").as_slice());
        assert_eq!(k_renew(&K2, &K1).unwrap().as_slice(), key.as_slice());
        assert_ne!(k_renew(&K1, &K1).unwrap().as_slice(), key.as_slice());
    }

    #[test]
    fn renew_records_have_their_exact_layouts() {
        for (op, bytes) in [
            (Op::Offer { ticket: TICKET, role: 0 }, "01010203040506070800"),
            (Op::Armed { ticket: TICKET }, "020102030405060708"),
            (Op::Refused { ticket: TICKET, reason: 7 }, "03010203040506070807"),
            (Op::Bind { ticket: TICKET, next_sequence: 9 }, "0401020304050607080000000000000009"),
        ] {
            assert_eq!(encode(&op), hex(bytes));
            assert_eq!(decode(&hex(bytes)).unwrap(), op);
        }
    }

    #[test]
    fn malformed_renew_records_fail_closed() {
        for bad in [
            // wrong lengths
            "01",
            "0101020304050607080000",
            "0201020304050607",
            "02010203040506070800",
            "04010203040506070800000000000000",
            // role above 1, reason outside 1..7
            "01010203040506070802",
            "03010203040506070800",
            "03010203040506070808",
            // an all-zero ticket
            "01000000000000000000",
            "020000000000000000",
            "03000000000000000001",
            "0400000000000000000000000000000001",
            // BIND with next sequence 0
            "0401020304050607080000000000000000",
            // unknown op
            "05",
        ] {
            assert!(decode(&hex(bad)).is_err(), "{bad}");
        }
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn the_advert_uuid_is_version_four_and_new_for_every_ticket() {
        let key = k_renew(&K1, &K2).unwrap();
        let uuid = advert_uuid(&key, &CALL, &TICKET).unwrap();
        assert_eq!(uuid.to_vec(), hex("1cbd9604253146e5971c16182e8a27d4"));
        assert_eq!(uuid[6] >> 4, 4, "version nibble");
        assert_eq!(uuid[8] >> 6, 2, "variant bits 10");
        assert_ne!(uuid, advert_uuid(&key, &CALL, &[9; 8]).unwrap());
        assert_ne!(uuid, advert_uuid(&key, &[9; 16], &TICKET).unwrap());
    }

    #[test]
    fn the_hop_keys_bind_ticket_challenge_and_nonce() {
        let key = k_renew(&K1, &K2).unwrap();
        let challenge = [0x10; 16];
        let nonce = [0x20; 16];
        let hop = hop_keys(&key, &CALL, &TICKET, &challenge, &nonce).unwrap();
        assert_eq!(hop.len(), 64);
        assert_eq!(
            hop.as_slice(),
            hex("35a81cb71a687b6c107f23142f3e17cab344bf6e35b6f30066e9f27c72b252598efd2347414e63ca46182f92099abb2d8d688c721c0315d850b2009d5fddf752").as_slice()
        );
        assert_ne!(hop[..32], hop[32..]);
        assert_ne!(hop, hop_keys(&key, &CALL, &TICKET, &[0x11; 16], &nonce).unwrap());
        assert_ne!(hop, hop_keys(&key, &CALL, &[9; 8], &challenge, &nonce).unwrap());
        assert_ne!(hop, hop_keys(&key, &CALL, &TICKET, &challenge, &[0x21; 16]).unwrap());
    }

    #[test]
    fn the_renewal_bit_is_end_to_end_and_both_sides_must_sign_it() {
        assert_eq!(transport(0x0c, false), 0x0c);
        assert_eq!(transport(0x0c, true), 0x1c);
        assert!(negotiated(true, 0x1c, 0x1c));
        assert!(!negotiated(true, 0x1c, 0x0c));
        assert!(!negotiated(true, 0x0c, 0x1c));
        assert!(!negotiated(false, 0x1c, 0x1c));
    }

    // ---- Step 2: the opening ----

    fn pair() -> (Opening, Opening) {
        let key = k_renew(&K1, &K2).unwrap();
        let armed = Opening::armed(key.clone(), CALL, TICKET, [0x10; 16]);
        let offerer = Opening::offerer(key, CALL, TICKET, [0x20; 16]);
        (armed, offerer)
    }

    #[test]
    fn the_opening_macs_and_hop_keys_match_the_known_answers() {
        let (mut armed, mut offerer) = pair();
        let proof = offerer.proof(&armed.challenge()).unwrap();
        assert_eq!(&proof[..4], b"SR1P");
        assert_eq!(&proof[4..12], &TICKET);
        assert_eq!(&proof[12..28], &[0x20; 16]);
        assert_eq!(&proof[28..], hex("3df605ea7666a5334b26b0e606c4c07f0ae2c96b55e5ff6abaabc41d88c7ea7c").as_slice());
        assert!(armed.accept_proof(&proof));
        let confirm = armed.confirm().unwrap();
        assert_eq!(&confirm[..4], b"SR1A");
        assert_eq!(&confirm[4..], hex("528621dee1790be580515e72d0afece0dc1a1bc562c582491d662794ab3fcd58").as_slice());
        assert_eq!(challenge_frame(&[0x10; 16]).len(), 20);
        assert_eq!(parse_challenge(&challenge_frame(&[0x10; 16])), Some([0x10; 16]));
        assert_eq!(parse_challenge(&[0u8; 20]), None);
    }

    #[test]
    fn the_production_opening_proves_the_key_both_ways() {
        let (mut armed, mut offerer) = pair();
        let challenge = armed.challenge();
        let proof = offerer.proof(&challenge).unwrap();
        assert!(armed.confirm().is_none(), "no confirm before a valid proof");
        assert!(armed.accept_proof(&proof));
        let confirm = armed.confirm().unwrap();
        assert!(offerer.accept_confirm(&confirm));
        assert_eq!(offerer.hop_keys().unwrap(), armed.hop_keys().unwrap());
        assert_eq!(
            offerer.hop_keys().unwrap().as_slice(),
            hex("35a81cb71a687b6c107f23142f3e17cab344bf6e35b6f30066e9f27c72b252598efd2347414e63ca46182f92099abb2d8d688c721c0315d850b2009d5fddf752").as_slice()
        );
    }

    #[test]
    fn an_advert_copied_from_the_air_cannot_confirm() {
        let (mut armed, _) = pair();
        // A phone that heard the advert but holds another key.
        let mut other = Opening::offerer(k_renew(&K1, &K1).unwrap(), CALL, TICKET, [0x20; 16]);
        let proof = other.proof(&armed.challenge()).unwrap();
        assert!(!armed.accept_proof(&proof));
        assert!(armed.confirm().is_none());
        assert!(armed.hop_keys().is_none());
        // A genuine offerer is not fooled by a fake armed side either.
        let (_, mut offerer) = pair();
        let _ = offerer.proof(&[0x10; 16]).unwrap();
        let mut fake = [0u8; CONFIRM_LEN];
        fake[..4].copy_from_slice(b"SR1A");
        assert!(!offerer.accept_confirm(&fake));
        assert!(offerer.hop_keys().is_none());
    }

    #[test]
    fn a_proof_for_another_call_or_ticket_is_refused() {
        let key = k_renew(&K1, &K2).unwrap();
        let mut armed = Opening::armed(key.clone(), CALL, TICKET, [0x10; 16]);
        let mut other_call = Opening::offerer(key.clone(), [9; 16], TICKET, [0x20; 16]);
        assert!(!armed.accept_proof(&other_call.proof(&armed.challenge()).unwrap()));
        let mut other_ticket = Opening::offerer(key, CALL, [9; 8], [0x20; 16]);
        assert!(!armed.accept_proof(&other_ticket.proof(&armed.challenge()).unwrap()));
        assert!(!armed.proved(), "a refused proof changes nothing");
    }

    #[test]
    fn a_proof_and_a_confirmation_are_not_interchangeable() {
        let (mut armed, mut offerer) = pair();
        let proof = offerer.proof(&armed.challenge()).unwrap();
        assert!(armed.accept_proof(&proof));
        let confirm = armed.confirm().unwrap();
        // The armed side's own CONFIRM mac presented as a PROOF to a fresh armed side.
        let key = k_renew(&K1, &K2).unwrap();
        let mut again = Opening::armed(key, CALL, TICKET, [0x10; 16]);
        let mut reflected = [0u8; PROOF_LEN];
        reflected[..4].copy_from_slice(b"SR1P");
        reflected[4..12].copy_from_slice(&TICKET);
        reflected[12..28].copy_from_slice(&[0x20; 16]);
        reflected[28..].copy_from_slice(&confirm[4..]);
        assert!(!again.accept_proof(&reflected), "a CONFIRM mac is not a PROOF mac");
        // The offerer's own PROOF mac presented to it as a CONFIRM.
        let (_, mut offerer) = pair();
        let proof = offerer.proof(&[0x10; 16]).unwrap();
        let mut echoed = [0u8; CONFIRM_LEN];
        echoed[..4].copy_from_slice(b"SR1A");
        echoed[4..].copy_from_slice(&proof[28..]);
        assert!(!offerer.accept_confirm(&echoed), "a PROOF mac is not a CONFIRM mac");
    }

    #[test]
    fn a_proof_replayed_on_a_new_connection_fails() {
        let (mut armed, mut offerer) = pair();
        let old_proof = offerer.proof(&armed.challenge()).unwrap();
        // A squatter's connection is dropped and the next one is challenged afresh.
        assert!(armed.next_connection([0x11; 16], 1_000));
        assert!(!armed.accept_proof(&old_proof), "the proof answered another challenge");
        assert!(armed.accept_proof(&offerer.proof(&armed.challenge()).unwrap()));
    }

    #[test]
    fn a_squatter_is_dropped_after_one_second_and_the_ticket_is_single_use() {
        let key = k_renew(&K1, &K2).unwrap();
        let mut armed = Opening::armed(key.clone(), CALL, TICKET, [0x10; 16]).at(5_000);
        assert!(!armed.accept_proof(&[0; PROOF_LEN]));
        assert_eq!(armed.connections_challenged(), 1);
        assert!(!armed.expired(5_999));
        assert!(armed.expired(6_000));
        for i in 2..=8u8 {
            assert!(armed.next_connection([i; 16], 6_000 + u64::from(i)));
            assert_eq!(armed.connections_challenged(), i);
        }
        assert!(!armed.next_connection([9; 16], 7_000), "a ninth connection is not challenged");
        let mut offerer = Opening::offerer(key, CALL, TICKET, [0x20; 16]);
        let proof = offerer.proof(&armed.challenge()).unwrap();
        assert!(armed.accept_proof(&proof));
        assert!(!armed.expired(60_000), "a proved connection does not expire");
        assert!(!armed.accept_proof(&proof), "ticket single use");
    }

    // ---- Step 3: the offerer and armed state machines ----

    fn facts() -> Facts {
        Facts {
            enabled: true,
            relay_hops: 0,
            direct: true,
            gatt: true,
            active: true,
            verification: false,
            wifi_attempt: false,
            carrier_alive: false,
            active_ms: 20_000,
            silence_ms: 4_000,
            write_age_ms: Some(100),
        }
    }
    fn renewal(caller: bool) -> Renewal {
        let mut renewal = Renewal::new(CALL, caller);
        renewal.set_key(k_renew(&K1, &K2).unwrap());
        renewal
    }
    fn sent(actions: &[Action]) -> Vec<Op> {
        actions.iter().filter_map(|a| if let Action::Send(op) = a { Some(*op) } else { None }).collect()
    }
    fn has(actions: &[Action], wanted: impl Fn(&Action) -> bool) -> bool {
        actions.iter().any(wanted)
    }
    fn closes(actions: &[Action]) -> bool {
        has(actions, |a| matches!(a, Action::CloseOldLink))
    }
    fn ends(actions: &[Action]) -> bool {
        has(actions, |a| matches!(a, Action::EndCall(_)))
    }
    /// The offerer's first offer at `now`: (ticket, derived UUID).
    fn start(renewal: &mut Renewal, now: u64) -> ([u8; 8], [u8; 16]) {
        let actions = renewal.tick(now, &facts());
        let ticket = match sent(&actions).first() {
            Some(Op::Offer { ticket, .. }) => *ticket,
            other => panic!("no OFFER: {other:?}"),
        };
        let uuid = actions
            .iter()
            .find_map(|a| if let Action::Scan { uuid, .. } = a { Some(*uuid) } else { None })
            .expect("scan");
        (ticket, uuid)
    }
    fn offer(ticket: [u8; 8]) -> Op {
        Op::Offer { ticket, role: 0 }
    }

    #[test]
    fn the_offerer_waits_one_second_of_soft_state_then_scans_and_breaks_only_after_found() {
        let mut renewal = renewal(true);
        let mut f = facts();
        f.silence_ms = 3_999;
        assert!(renewal.tick(1_000, &f).is_empty(), "3,999 ms of silence is not yet a stall");
        f.silence_ms = 4_000;
        let actions = renewal.tick(1_000, &f);
        let ticket = match sent(&actions)[..] {
            [Op::Offer { ticket, role: 0 }] => ticket,
            ref other => panic!("{other:?}"),
        };
        assert!(has(&actions, |a| matches!(a, Action::Scan { until_ms: 7_000, .. })));
        assert!(!closes(&actions), "the old link stays until the advert is found");
        assert_eq!(renewal.phase(), Phase::Scanning);
        assert!(renewal.owns_liveness());
        let uuid = advert_uuid(&k_renew(&K1, &K2).unwrap(), &CALL, &ticket).unwrap();
        let found = renewal.on_found(2_000);
        let close = found.iter().position(|a| matches!(a, Action::CloseOldLink)).expect("close after found");
        let connect = found.iter().position(|a| matches!(a, Action::Connect { uuid: u, .. } if *u == uuid)).expect("connect");
        assert!(close < connect, "close the old link, then connect");
    }

    #[test]
    fn an_offerer_that_finds_nothing_keeps_the_old_link_under_the_normal_rules() {
        let mut renewal = renewal(true);
        let (ticket, _) = start(&mut renewal, 1_000);
        assert!(renewal.tick(6_999, &facts()).is_empty());
        let actions = renewal.tick(7_000, &facts());
        assert!(!closes(&actions));
        assert!(!ends(&actions));
        assert_eq!(sent(&actions), vec![Op::Refused { ticket, reason: reason::ABANDONED }]);
        assert_eq!(renewal.phase(), Phase::Abandoned);
        assert!(!renewal.owns_liveness(), "liveness is handed back at once");
    }

    #[test]
    fn hearing_the_peer_before_the_break_abandons_the_renewal() {
        let mut renewal = renewal(true);
        let (ticket, _) = start(&mut renewal, 1_000);
        let actions = renewal.on_peer_heard(2_000);
        assert_eq!(sent(&actions), vec![Op::Refused { ticket, reason: reason::ABANDONED }]);
        assert!(!closes(&actions));
        assert_eq!(renewal.phase(), Phase::Abandoned);
        // A later sighting of the advert is ignored.
        assert!(renewal.on_found(2_500).is_empty());
        // After the break there is nothing to abandon.
        let mut broken = self::renewal(true);
        start(&mut broken, 1_000);
        broken.on_found(1_500);
        assert!(broken.on_peer_heard(1_600).is_empty());
        assert_eq!(broken.phase(), Phase::Connecting);
    }

    #[test]
    fn a_broken_old_link_ends_the_call_twelve_seconds_after_the_offer() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        assert!(closes(&renewal.on_found(2_000)));
        assert!(!ends(&renewal.tick(12_999, &facts())), "not before the bound");
        let actions = renewal.tick(13_000, &facts());
        assert!(has(&actions, |a| matches!(a, Action::EndCall("link_lost"))));
        assert_eq!(renewal.phase(), Phase::Failed);
        assert!(!ends(&renewal.tick(14_000, &facts())), "once");
        // A live Wi-Fi carrier keeps the call.
        let mut wifi = self::renewal(true);
        start(&mut wifi, 1_000);
        wifi.on_found(2_000);
        let mut f = facts();
        f.carrier_alive = true;
        assert!(!ends(&wifi.tick(13_000, &f)));
    }

    #[test]
    fn a_collision_keeps_the_callers_offer() {
        let mut caller = renewal(true);
        let (own, _) = start(&mut caller, 1_000);
        let actions = caller.on_record(1_100, &offer([9; 8]), &facts());
        assert_eq!(sent(&actions), vec![Op::Refused { ticket: [9; 8], reason: reason::COLLISION }]);
        assert_eq!(caller.phase(), Phase::Scanning);
        assert_eq!(caller.ticket(), own);
        // The callee abandons its offer and arms for the caller's.
        let mut callee = renewal(false);
        start(&mut callee, 1_000);
        let actions = callee.on_record(1_100, &offer([7; 8]), &facts());
        assert!(has(&actions, |a| matches!(a, Action::Stop)), "its scan stops");
        assert!(has(&actions, |a| matches!(a, Action::RegisterServer { .. })));
        assert_eq!(callee.phase(), Phase::Arming);
        assert_eq!(callee.ticket(), [7; 8]);
    }

    #[test]
    fn a_yielding_callee_does_not_count_an_attempt() {
        let mut callee = renewal(false);
        start(&mut callee, 1_000);
        assert_eq!(callee.attempts(), 1);
        callee.on_record(1_100, &offer([7; 8]), &facts());
        assert_eq!(callee.attempts(), 1, "its abandoned offer is not an attempt; the armed one is");
        // And it may still offer again after 60 s: only one attempt used.
        let mut f = facts();
        callee.on_registered(1_200, true);
        callee.on_advertising(1_300, true);
        callee.on_record(1_400, &Op::Refused { ticket: [7; 8], reason: reason::ABANDONED }, &f);
        f.active_ms = 90_000;
        assert!(sent(&callee.tick(62_000, &f)).iter().any(|op| matches!(op, Op::Offer { .. })));
    }

    #[test]
    fn an_out_of_scope_offer_is_not_armed() {
        let reason_of = |actions: Vec<Action>| match sent(&actions)[..] {
            [Op::Refused { reason, .. }] => reason,
            ref other => panic!("{other:?}"),
        };
        let mut f = facts();
        f.direct = false;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::NOT_DIRECT, "link peer is not the end-to-end peer");
        let mut f = facts();
        f.relay_hops = 1;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::NOT_DIRECT, "relayed");
        let mut f = facts();
        f.gatt = false;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::NOT_DIRECT, "not GATT");
        let mut f = facts();
        f.verification = true;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::NOT_DIRECT, "verification");
        let mut f = facts();
        f.active = false;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::LIMIT, "not ACTIVE");
        let mut f = facts();
        f.active_ms = 9_999;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::LIMIT, "ACTIVE under 10 s");
        let mut f = facts();
        f.wifi_attempt = true;
        assert_eq!(reason_of(renewal(false).on_record(0, &offer([5; 8]), &f)), reason::LIMIT, "a Wi-Fi attempt runs");
    }

    #[test]
    fn an_armed_side_without_a_key_refuses_disabled_and_disarms() {
        let mut keyless = Renewal::new(CALL, false);
        let actions = keyless.on_record(0, &offer([5; 8]), &facts());
        assert_eq!(sent(&actions), vec![Op::Refused { ticket: [5; 8], reason: reason::DISABLED }]);
        assert_eq!(keyless.phase(), Phase::Idle);
        assert!(!keyless.running());
        let mut switched_off = renewal(false);
        let mut f = facts();
        f.enabled = false;
        let actions = switched_off.on_record(0, &offer([5; 8]), &f);
        assert_eq!(sent(&actions), vec![Op::Refused { ticket: [5; 8], reason: reason::DISABLED }]);
        assert!(!switched_off.running());
        // Without a key nothing is ever offered either.
        assert!(sent(&keyless.tick(1_000, &facts())).is_empty());
    }

    #[test]
    fn a_registration_slower_than_three_seconds_is_refused() {
        let refused = |actions: Vec<Action>| match sent(&actions)[..] {
            [Op::Refused { reason, .. }] => reason,
            ref other => panic!("{other:?}"),
        };
        let mut late = renewal(false);
        late.on_record(10_000, &offer([5; 8]), &facts());
        assert_eq!(refused(late.on_registered(13_001, true)), reason::REGISTRATION);
        assert!(!late.running());
        let mut failed = renewal(false);
        failed.on_record(10_000, &offer([5; 8]), &facts());
        assert_eq!(refused(failed.on_registered(10_010, false)), reason::REGISTRATION);
        let mut slow_advert = renewal(false);
        slow_advert.on_record(10_000, &offer([5; 8]), &facts());
        assert!(has(&slow_advert.on_registered(10_100, true), |a| matches!(a, Action::Advertise { until_ms: 17_000, .. })));
        assert_eq!(refused(slow_advert.on_advertising(13_001, true)), reason::ADVERTISING);
        let mut never = renewal(false);
        never.on_record(10_000, &offer([5; 8]), &facts());
        assert_eq!(refused(never.tick(13_000, &facts())), reason::REGISTRATION);
        let mut ok = renewal(false);
        ok.on_record(10_000, &offer([5; 8]), &facts());
        ok.on_registered(10_100, true);
        let armed = ok.on_advertising(12_999, true);
        assert_eq!(sent(&armed), vec![Op::Armed { ticket: [5; 8] }]);
        assert_eq!(ok.phase(), Phase::Armed);
    }

    fn armed_at(now: u64) -> Renewal {
        let mut renewal = renewal(false);
        renewal.on_record(now, &offer([5; 8]), &facts());
        renewal.on_registered(now + 10, true);
        renewal.on_advertising(now + 20, true);
        assert_eq!(renewal.phase(), Phase::Armed);
        renewal
    }

    #[test]
    fn the_advert_stops_at_seven_seconds_and_the_bound_ends_only_a_call_whose_old_link_is_gone() {
        let mut broken = armed_at(10_000);
        assert!(broken.tick(16_999, &facts()).is_empty());
        assert!(has(&broken.tick(17_000, &facts()), |a| matches!(a, Action::StopAdvert)));
        assert!(!has(&broken.tick(17_500, &facts()), |a| matches!(a, Action::StopAdvert)), "once");
        broken.on_old_link_failed();
        assert!(!ends(&broken.tick(21_999, &facts())));
        let actions = broken.tick(22_000, &facts());
        assert!(has(&actions, |a| matches!(a, Action::EndCall("link_lost"))));
        // The offerer kept the old link: the armed phone only disarms.
        let mut healthy = armed_at(10_000);
        healthy.tick(17_000, &facts());
        let actions = healthy.tick(22_000, &facts());
        assert!(!ends(&actions));
        assert!(has(&actions, |a| matches!(a, Action::Stop)));
        assert!(!healthy.running());
        // A live Wi-Fi carrier keeps a call whose Bluetooth link died.
        let mut wifi = armed_at(10_000);
        wifi.on_old_link_failed();
        let mut f = facts();
        f.carrier_alive = true;
        assert!(!ends(&wifi.tick(22_000, &f)));
    }

    #[test]
    fn an_abandoned_offer_disarms_at_once() {
        let mut armed = armed_at(10_000);
        assert!(armed.owns_liveness());
        let actions = armed.on_record(11_000, &Op::Refused { ticket: [5; 8], reason: reason::ABANDONED }, &facts());
        assert!(has(&actions, |a| matches!(a, Action::Stop)));
        assert!(!armed.running());
        assert!(!armed.owns_liveness());
        // Another ticket's refusal is not for this attempt.
        let mut other = armed_at(10_000);
        assert!(other.on_record(11_000, &Op::Refused { ticket: [6; 8], reason: reason::ABANDONED }, &facts()).is_empty());
        assert!(other.running());
    }

    #[test]
    fn attempts_are_counted_per_call() {
        let mut renewal = renewal(true);
        start(&mut renewal, 0);
        renewal.tick(6_000, &facts());
        assert_eq!(renewal.phase(), Phase::Abandoned);
        assert!(sent(&renewal.tick(30_000, &facts())).is_empty(), "under 60 s apart");
        assert!(sent(&renewal.tick(59_999, &facts())).is_empty());
        start(&mut renewal, 60_000);
        assert_eq!(renewal.attempts(), 2);
        renewal.tick(66_000, &facts());
        assert!(sent(&renewal.tick(300_000, &facts())).is_empty(), "a third attempt is never made");
        // The armed side counts the offers it armed for toward the same limit.
        let mut armed = self::renewal(false);
        armed.on_record(20_000, &offer([1; 8]), &facts());
        armed.on_record(21_000, &Op::Refused { ticket: [1; 8], reason: reason::ABANDONED }, &facts());
        let early = armed.on_record(30_000, &offer([2; 8]), &facts());
        assert_eq!(sent(&early), vec![Op::Refused { ticket: [2; 8], reason: reason::LIMIT }]);
        assert!(has(&armed.on_record(80_000, &offer([3; 8]), &facts()), |a| matches!(a, Action::RegisterServer { .. })));
        armed.on_record(81_000, &Op::Refused { ticket: [3; 8], reason: reason::ABANDONED }, &facts());
        let third = armed.on_record(200_000, &offer([4; 8]), &facts());
        assert_eq!(sent(&third), vec![Op::Refused { ticket: [4; 8], reason: reason::LIMIT }]);
    }

    #[test]
    fn no_offer_while_own_writes_do_not_complete() {
        let mut renewal = renewal(true);
        let mut f = facts();
        f.write_age_ms = None;
        assert!(renewal.tick(1_000, &f).is_empty(), "no write has ever completed");
        f.write_age_ms = Some(2_000);
        assert!(renewal.tick(1_000, &f).is_empty(), "the last write is not fresh");
        f.write_age_ms = Some(1_999);
        assert!(!renewal.tick(1_000, &f).is_empty());
        // Nor before 10 s ACTIVE, off the switch, off GATT, relayed or verifying.
        for edit in [
            (|f: &mut Facts| f.active_ms = 9_999) as fn(&mut Facts),
            |f| f.enabled = false,
            |f| f.gatt = false,
            |f| f.relay_hops = 1,
            |f| f.direct = false,
            |f| f.verification = true,
            |f| f.wifi_attempt = true,
            |f| f.active = false,
        ] {
            let mut fresh = self::renewal(true);
            let mut f = facts();
            edit(&mut f);
            assert!(fresh.tick(1_000, &f).is_empty());
        }
    }

    #[test]
    fn scan_failure_and_registration_refusal_hand_liveness_back() {
        let mut offerer = renewal(true);
        let (ticket, _) = start(&mut offerer, 1_000);
        assert!(offerer.owns_liveness());
        let actions = offerer.on_scan_failed(1_500);
        assert!(!closes(&actions));
        assert_eq!(sent(&actions), vec![Op::Refused { ticket, reason: reason::ABANDONED }]);
        assert!(!offerer.owns_liveness());
        let mut armed = renewal(false);
        armed.on_record(1_000, &offer([5; 8]), &facts());
        assert!(armed.owns_liveness());
        armed.on_registered(1_100, false);
        assert!(!armed.owns_liveness());
        // The offerer refused by the armed phone keeps the old link and its liveness rules.
        let mut refused = renewal(true);
        let (ticket, _) = start(&mut refused, 1_000);
        let actions = refused.on_record(1_200, &Op::Refused { ticket, reason: reason::REGISTRATION }, &facts());
        assert!(has(&actions, |a| matches!(a, Action::Stop)));
        assert!(!refused.owns_liveness());
        assert!(!closes(&actions));
    }

    #[test]
    fn an_end_verdict_while_connecting_never_ends_a_renewal_that_binds() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        renewal.on_found(2_000);
        assert!(!ends(&renewal.tick(12_999, &facts())), "no end verdict inside the bound");
        assert!(renewal.bind_allowed(12_999));
        let bound = renewal.on_bound(12_999);
        assert!(has(&bound, |a| matches!(a, Action::Reassert)));
        assert_eq!(renewal.phase(), Phase::Bound);
        assert!(!renewal.owns_liveness());
        assert!(!ends(&renewal.tick(13_000, &facts())), "a bound renewal never ends the call");
        assert!(!ends(&renewal.tick(60_000, &facts())));
    }

    #[test]
    fn the_bound_is_checked_before_bind_is_sent() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        assert!(!renewal.bind_allowed(2_000), "not before the opening: the old link is still open");
        renewal.on_found(2_000);
        assert!(renewal.bind_allowed(12_999));
        assert!(!renewal.bind_allowed(13_000), "12 s after the offer is too late");
        // The armed side applies the same bound and the ticket.
        let armed = armed_at(10_000);
        assert!(armed.accept_bind_allowed(21_999, &[5; 8]));
        assert!(!armed.accept_bind_allowed(22_000, &[5; 8]));
        assert!(!armed.accept_bind_allowed(15_000, &[6; 8]), "another ticket");
    }

    #[test]
    fn a_sighting_after_six_seconds_is_ignored() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        assert!(renewal.on_found(7_000).is_empty());
        assert!(!closes(&renewal.on_found(7_001)));
        assert_eq!(renewal.phase(), Phase::Scanning);
        let mut fresh = self::renewal(true);
        start(&mut fresh, 1_000);
        assert!(closes(&fresh.on_found(6_999)));
    }

    #[test]
    fn no_connect_starts_after_eleven_seconds() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        renewal.on_found(2_000);
        // The first connection fails; a new sighting is looked for while time
        // remains, but never past 11 s after the offer.
        let actions = renewal.on_connect_failed(8_000);
        assert!(has(&actions, |a| matches!(a, Action::Scan { until_ms: 12_000, .. })), "{actions:?}");
        assert!(!closes(&actions), "the link is already closed");
        assert!(renewal.on_found(12_000).is_empty(), "11 s after the offer nothing connects");
        // A failure late in the window starts nothing at all.
        let mut late = self::renewal(true);
        start(&mut late, 1_000);
        late.on_found(2_000);
        let actions = late.on_connect_failed(12_100);
        assert!(actions.iter().all(|a| !matches!(a, Action::Scan { .. } | Action::Connect { .. })), "{actions:?}");
        // A sighting inside the window connects again without a second close.
        let mut retry = self::renewal(true);
        start(&mut retry, 1_000);
        retry.on_found(2_000);
        retry.on_connect_failed(3_000);
        let actions = retry.on_found(4_000);
        assert!(has(&actions, |a| matches!(a, Action::Connect { .. })));
        assert!(!closes(&actions));
    }

    #[test]
    fn only_a_scanning_offer_is_a_pending_collision() {
        // Past the break the offerer is busy, not colliding.
        let mut connecting = renewal(true);
        start(&mut connecting, 1_000);
        connecting.on_found(2_000);
        let actions = connecting.on_record(3_000, &offer([9; 8]), &facts());
        assert_eq!(sent(&actions), vec![Op::Refused { ticket: [9; 8], reason: reason::LIMIT }]);
        assert_eq!(connecting.phase(), Phase::Connecting);
        // A finished offer is no collision: the OFFER goes through the ordinary rules.
        let mut done = renewal(true);
        start(&mut done, 0);
        done.tick(6_000, &facts());
        assert_eq!(done.phase(), Phase::Abandoned);
        let actions = done.on_record(70_000, &offer([9; 8]), &facts());
        assert!(has(&actions, |a| matches!(a, Action::RegisterServer { .. })));
    }

    #[test]
    fn address_kinds_are_read_from_the_top_bits() {
        assert_eq!(address_kind(0x4000_0000_0000), "rpa");
        assert_eq!(address_kind(0x7fff_ffff_ffff), "rpa");
        assert_eq!(address_kind(0xc000_0000_0000), "static");
        assert_eq!(address_kind(0x0000_1234_5678), "other");
        assert_eq!(address_kind(0x8000_0000_0000), "other");
    }

    #[test]
    fn a_call_with_a_live_wifi_carrier_neither_offers_nor_arms() {
        let mut f = facts();
        f.carrier_alive = true;
        assert!(renewal(true).tick(1_000, &f).is_empty(), "no offer while another carrier carries the call");
        let actions = renewal(false).on_record(0, &offer([5; 8]), &f);
        assert_eq!(sent(&actions), vec![Op::Refused { ticket: [5; 8], reason: reason::LIMIT }]);
        assert!(!has(&actions, |a| matches!(a, Action::RegisterServer { .. })));
    }

    #[test]
    fn an_old_link_that_fails_while_scanning_keeps_the_offerer_looking_until_the_connect_limit() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        renewal.on_old_link_failed();
        // The six-second scan window ends: the link is already gone, so it is not "never broken".
        let actions = renewal.tick(7_000, &facts());
        assert!(sent(&actions).is_empty(), "no REFUSED abandoned: there is no old link to keep");
        assert_eq!(renewal.phase(), Phase::Scanning);
        assert!(has(&actions, |a| matches!(a, Action::Scan { until_ms: 12_000, .. })), "{actions:?}");
        let found = renewal.on_found(8_000);
        assert!(has(&found, |a| matches!(a, Action::Connect { .. })));
        assert!(!closes(&found));
        assert!(!ends(&renewal.tick(11_999, &facts())));
    }

    #[test]
    fn the_connect_budget_is_eleven_seconds_after_the_offer() {
        let mut renewal = renewal(true);
        start(&mut renewal, 1_000);
        let found = renewal.on_found(2_000);
        assert!(has(&found, |a| matches!(a, Action::Connect { until_ms: 12_000, .. })), "{found:?}");
    }
}
