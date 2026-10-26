//! Verification code BVMID2 (PROTOCOL "Verification code BVMID2"), the
//! scanner's side: the code text, its parser and the BMH3 tag key. This PC
//! cannot advertise, so it never shows a code; it accepts a typed or pasted
//! one and connects to the phone that shows it.
//!
//! `BVMID2.` + 64 hex fingerprint + `.` + 64 hex K_qr + `.` + 12 hex RDI
//! (149 characters, one case only), K_qr never all zeros. The handshake tag key
//! is `HKDF-Expand(K_qr, "BluetoothVideoMesh/BMH3/QrTag/1", 32)`.
use crate::{
    bmh3::{self, handshake::{ContactTag, Peer}},
    discovery::Announcement,
    rotating::{self, ROLE_CALL_OFFER},
};
use zeroize::Zeroizing;

pub const PREFIX: &str = "BVMID2";
pub const TEXT_LENGTH: usize = 149;
/// A displayed code lives this long (monotonic) on the phone that shows it.
pub const LIFETIME_MS: u64 = 300_000;
/// The shower's reservation for one scanner attempt.
pub const RESERVATION_MS: u64 = 45_000;
/// Attempts the shower allows per code.
pub const MAX_ATTEMPTS: u32 = 3;
/// A verification session lasts at most this long.
pub const MAX_SESSION_MS: u64 = 60_000;

/// Why a text is not a BVMID2 code (Android Bvmid2FormatException.reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    OtherVersion,
    OtherMagic,
    FieldCount,
    Length,
    Alphabet,
    MixedCase,
    ZeroSecret,
}
impl FormatError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::OtherVersion => "other version",
            Self::OtherMagic => "other magic",
            Self::FieldCount => "field count",
            Self::Length => "length",
            Self::Alphabet => "alphabet",
            Self::MixedCase => "mixed case",
            Self::ZeroSecret => "zero secret",
        }
    }
}

pub struct Code {
    pub fingerprint: [u8; 32],
    pub k_qr: Zeroizing<[u8; 32]>,
    pub rdi: [u8; 6],
}
impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret.
        f.debug_struct("Code").field("rdi", &self.rdi).finish_non_exhaustive()
    }
}
impl Code {
    /// The node ID the shower must prove (fingerprint[0..16]).
    pub fn node_id(&self) -> [u8; 16] {
        self.fingerprint[..16].try_into().unwrap()
    }
    pub fn tag_key(&self) -> Result<Zeroizing<[u8; 32]>, rotating::Error> {
        rotating::qr_tag_key(&self.k_qr)
    }
    pub fn encode(&self) -> String {
        format!(
            "{PREFIX}.{}.{}.{}",
            upper_hex(&self.fingerprint),
            upper_hex(self.k_qr.as_slice()),
            upper_hex(&self.rdi)
        )
    }
    /// Parses a typed or pasted code (surrounding whitespace is ignored).
    pub fn parse(text: &str) -> Result<Self, FormatError> {
        let text = text.trim();
        let fields: Vec<&str> = text.split('.').collect();
        match fields[0] {
            PREFIX => {}
            other if other.starts_with("BVMID") => return Err(FormatError::OtherVersion),
            _ => return Err(FormatError::OtherMagic),
        }
        if fields.len() != 4 {
            return Err(FormatError::FieldCount);
        }
        if fields[1].len() != 64 || fields[2].len() != 64 || fields[3].len() != 12 {
            return Err(FormatError::Length);
        }
        let hex = &text[PREFIX.len() + 1..];
        if !hex.bytes().all(|b| b == b'.' || b.is_ascii_hexdigit()) {
            return Err(FormatError::Alphabet);
        }
        if hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(FormatError::MixedCase);
        }
        let fingerprint: [u8; 32] = decode_hex(fields[1]).try_into().unwrap();
        let k_qr: [u8; 32] = decode_hex(fields[2]).try_into().unwrap();
        let rdi: [u8; 6] = decode_hex(fields[3]).try_into().unwrap();
        if k_qr.iter().all(|b| *b == 0) {
            return Err(FormatError::ZeroSecret);
        }
        Ok(Self {
            fingerprint,
            k_qr: Zeroizing::new(k_qr),
            rdi,
        })
    }
}
/// True for text that begins with the version 2 magic (the screen can say
/// "unreadable code" instead of "not a code").
pub fn looks_like_bvmid2(text: &str) -> bool {
    text.trim().starts_with("BVMID2.")
}

// ---------------------------------------------------------------- scanner policy (Android QrScanPolicy)

/// The scanner searches this long for the code's RDI.
pub const SEARCH_MS: u64 = 10_000;

/// The tag toward the displayer at both layers: `K_tag` bound to the code's
/// RDI (role 0, no wall epoch) and the full node ID the answerer must have.
pub fn tag(code: &Code) -> Result<ContactTag, rotating::Error> {
    let key = code.tag_key()?;
    Ok(ContactTag::new(*key, rotating::advert_binding(&code.rdi, ROLE_CALL_OFFER, None), code.node_id()))
}

/// Before this PC presents its real key: a pseudonym answer is refused and
/// the handshake key's fingerprint must equal the code's.
pub fn check_responder(code: &Code, peer: &Peer) -> Result<(), bmh3::Error> {
    if peer.extensions().pseudonym {
        return Err(bmh3::Error::Policy);
    }
    let same: bool = subtle::ConstantTimeEq::ct_eq(peer.fingerprint().as_slice(), code.fingerprint.as_slice()).into();
    if !same {
        return Err(bmh3::Error::Policy);
    }
    Ok(())
}

/// Only rotating (version 5) adverts with the code's RDI at route cost 0
/// are tried, every distinct address, in the order seen (a copy of the
/// advert may answer first and fail).
pub fn addresses_to_try<A: PartialEq + Clone>(code: &Code, seen: &[(A, Announcement)]) -> Vec<A> {
    let mut out: Vec<A> = Vec::new();
    for (address, ad) in seen {
        if ad.version == 5 && ad.cost == 0 && ad.prefix == code.rdi && !out.contains(address) {
            out.push(address.clone());
        }
    }
    out
}

// ---------------------------------------------------------------- verify-only scope (Android QrSessionScope)

/// Hop record types a verification session accepts: the end-to-end
/// handshake (4), end-to-end content (5), link heartbeats (6), BDO1 (13).
pub const LINK_RECORDS: [u8; 4] = [4, 5, 6, 13];
/// BME1 content type LABELS (the scanner's optional name).
pub const CONTENT_LABELS: u8 = 10;
pub const BDX1_OFFER: u8 = 2;
pub const BDX1_LEASE: u8 = 3;

/// The exact scope of a session opened with a code: verification only, plus
/// (Design QL) one BDX1 OFFER and one LEASE per direction when both endpoint
/// hellos carried bit 3. Every violation is counted and closes the session.
pub struct Scope {
    start_ms: u64,
    violations: u32,
    kinds_seen: [bool; 2],
}
impl Scope {
    pub const MAX_SESSION_MS: u64 = 60_000;
    /// After the verification event or a settled exchange the session lingers this long.
    pub const CLOSE_GRACE_MS: u64 = 2_000;
    /// The wait for this user's answer (scanner) before ending without a lease.
    pub const CONFIRM_WAIT_MS: u64 = 20_000;

    pub fn new(start_ms: u64) -> Self {
        Self { start_ms, violations: 0, kinds_seen: [false; 2] }
    }
    pub fn violations(&self) -> u32 {
        self.violations
    }
    pub fn allows_link_record(kind: u8) -> bool {
        LINK_RECORDS.contains(&kind)
    }
    pub fn allows_content(kind: u8) -> bool {
        kind == CONTENT_LABELS
    }
    /// True when refused (counted).
    pub fn refuse_link_record(&mut self, kind: u8) -> bool {
        let refused = !Self::allows_link_record(kind);
        self.violations += u32::from(refused);
        refused
    }
    /// True when refused (counted).
    pub fn refuse_content(&mut self, kind: u8) -> bool {
        let refused = !Self::allows_content(kind);
        self.violations += u32::from(refused);
        refused
    }
    /// A BDX1 record passes only as the first OFFER or the first LEASE from
    /// the peer, and only when `negotiated`. True when refused (counted).
    pub fn refuse_discovery(&mut self, payload: &[u8], negotiated: bool) -> bool {
        let kind = (payload.len() > 4 && &payload[..4] == b"BDX1").then(|| payload[4]);
        let slot = match kind {
            Some(BDX1_OFFER) => Some(0),
            Some(BDX1_LEASE) => Some(1),
            _ => None,
        };
        let allowed = negotiated && slot.is_some_and(|i| !std::mem::replace(&mut self.kinds_seen[i], true));
        self.violations += u32::from(!allowed);
        !allowed
    }
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.start_ms) >= Self::MAX_SESSION_MS
    }
}

/// The question the scanner asks before the shield is set.
pub fn confirmation_text(claimed_or_handle: &str) -> String {
    format!("Is this {claimed_or_handle}'s screen you scanned?")
}

fn upper_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}
fn decode_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn vectors() -> Vec<Value> {
        let text = include_str!("../tests/vectors/conformance/bvmid2.json");
        let json: Value = serde_json::from_str(text).unwrap();
        json["vectors"].as_array().unwrap().clone()
    }
    fn vector(id: &str) -> Value {
        vectors().into_iter().find(|v| v["id"] == id).unwrap()
    }
    fn hex(v: &Value) -> Vec<u8> {
        decode_hex(v.as_str().unwrap())
    }

    #[test]
    fn the_tag_key_matches_bvmid2_011() {
        let v = vector("bvmid2-011");
        let k_qr: [u8; 32] = hex(&v["inputs"]["k_qr"]).try_into().unwrap();
        let key = rotating::qr_tag_key(&k_qr).unwrap();
        assert_eq!(key.to_vec(), hex(&v["expected"]["k_tag"]));
    }

    #[test]
    fn the_text_round_trips_as_bvmid2_012() {
        let v = vector("bvmid2-012");
        let code = Code {
            fingerprint: hex(&v["inputs"]["fingerprint"]).try_into().unwrap(),
            k_qr: Zeroizing::new(hex(&v["inputs"]["k_qr"]).try_into().unwrap()),
            rdi: hex(&v["inputs"]["rdi"]).try_into().unwrap(),
        };
        let text = code.encode();
        assert_eq!(text, v["expected"]["text"].as_str().unwrap());
        assert_eq!(text.len(), TEXT_LENGTH);
        for form in [&v["expected"]["text"], &v["expected"]["lowercase_form_also_parses"]] {
            let parsed = Code::parse(form.as_str().unwrap()).unwrap();
            assert_eq!(parsed.fingerprint, code.fingerprint);
            assert_eq!(*parsed.k_qr, *code.k_qr);
            assert_eq!(parsed.rdi, code.rdi);
            assert_eq!(parsed.node_id(), code.fingerprint[..16]);
        }
        // Mixed case is refused with Android's reason; any other BVMID version is "other version".
        let mixed = text.replacen("BA78", "ba78", 1);
        assert_eq!(Code::parse(&mixed).err().map(FormatError::reason), Some("mixed case"));
        assert_eq!(Code::parse(&text.replacen("BVMID2", "BVMID3", 1)).err(), Some(FormatError::OtherVersion));
        assert!(looks_like_bvmid2(&text) && !looks_like_bvmid2("BVMID1.abc"));
    }

    #[test]
    fn every_malformed_text_of_bvmid2_013_is_rejected_with_its_reason() {
        let v = vector("bvmid2-013");
        for case in v["inputs"]["cases"].as_array().unwrap() {
            let got = Code::parse(case["text"].as_str().unwrap()).err().map(FormatError::reason);
            assert_eq!(got, case["reason"].as_str(), "{}", case["description"]);
        }
    }

    #[test]
    fn limits_match_bvmid2_014() {
        let v = vector("bvmid2-014");
        let e = &v["expected"];
        assert_eq!(e["lifetime_ms"], LIFETIME_MS);
        assert_eq!(e["reservation_ms"], RESERVATION_MS);
        assert_eq!(e["max_attempts"], MAX_ATTEMPTS);
        assert_eq!(e["max_session_ms"], MAX_SESSION_MS);
    }
}
