//! Signed peer labels (CF-REC-40..44, Android `SignedLabel`): a person name,
//! stream title or call subject signed by the key the endpoint handshake
//! authenticated, bound to one call ID or stream ID.
//! - `BNL1`: "BNL1" | version 1 | kind | spkiLen u16 | SPKI | sequence u32 |
//!   bindingType | binding (16) | textLen u8 | UTF-8 | sigLen u16 | DER
//!   signature over "BluetoothVideoMesh/Label/1" || 0x00 || every byte before
//!   sigLen.
//! - `BLB1`: "BLB1" | count (0..3) | count x (u16 length | BNL1), no trailing bytes.
//! - `Inbox`: the receiver's checks and limits; a label never fails a session.
use crate::{bmh2::SigningIdentity, names::LabelKind};
use std::collections::{HashMap, VecDeque};

/// End-to-end hello transport bit 5: accepts and sends LABELS.
pub const CAPABILITY_BIT: u8 = 0x20;
const MAGIC: &[u8; 4] = b"BNL1";
const CONTAINER: &[u8; 4] = b"BLB1";
const DOMAIN: &[u8] = b"BluetoothVideoMesh/Label/1\0";
pub const KIND_NAME: u8 = 1;
pub const KIND_TITLE: u8 = 2;
pub const KIND_SUBJECT: u8 = 3;
pub const BINDING_CALL: u8 = 1;
pub const BINDING_STREAM: u8 = 2;

/// LABELS are on (alternative off-switch: `MESHLIVE_LABELS=0`).
pub fn enabled() -> bool {
    std::env::var("MESHLIVE_LABELS").ok().as_deref() != Some("0")
}

static OWN_NAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// This PC's saved name, set by the app from its profile (tools send none).
pub fn set_own_name(name: Option<String>) {
    *OWN_NAME.lock().unwrap_or_else(|e| e.into_inner()) = name;
}
pub fn own_name() -> Option<String> {
    OWN_NAME.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
/// The container with this PC's name for one call (None: no name saved).
pub fn own_call_container(identity: &dyn SigningIdentity, call_id: &[u8; 16]) -> Option<Vec<u8>> {
    let name = own_name()?;
    let label = sign(identity, KIND_NAME, 0, BINDING_CALL, call_id, &name).ok()?;
    Some(container(&[label]))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub kind: u8,
    pub spki: Vec<u8>,
    pub sequence: u32,
    pub binding_type: u8,
    pub binding: [u8; 16],
    pub text: String,
    pub signature: Vec<u8>,
    /// Everything before sigLen (what the signature covers, after the domain).
    signed: Vec<u8>,
}

fn kind_of(kind: u8) -> Option<LabelKind> {
    match kind {
        KIND_NAME => Some(LabelKind::PersonName),
        KIND_TITLE => Some(LabelKind::StreamTitle),
        KIND_SUBJECT => Some(LabelKind::CallSubject),
        _ => None,
    }
}

/// The signed part (CF-REC-41 vector labels-002).
pub fn signed_part(kind: u8, spki: &[u8], sequence: u32, binding_type: u8, binding: &[u8; 16], text: &str) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(1);
    out.push(kind);
    out.extend_from_slice(&(spki.len() as u16).to_be_bytes());
    out.extend_from_slice(spki);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.push(binding_type);
    out.extend_from_slice(binding);
    out.push(text.len() as u8);
    out.extend_from_slice(text.as_bytes());
    out
}

/// Signs a label with `identity` (the key the endpoint handshake authenticated).
pub fn sign(identity: &dyn SigningIdentity, kind: u8, sequence: u32, binding_type: u8, binding: &[u8; 16], text: &str) -> Result<Vec<u8>, &'static str> {
    let kind_rules = kind_of(kind).ok_or("kind")?;
    if !crate::names::is_canonical(kind_rules, text) {
        return Err("text not canonical");
    }
    let spki = identity.public_key().map_err(|_| "key")?;
    let mut bytes = signed_part(kind, &spki, sequence, binding_type, binding, text);
    let signature = identity.sign(&[DOMAIN, bytes.as_slice()].concat()).map_err(|_| "sign")?;
    bytes.extend_from_slice(&(signature.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&signature);
    Ok(bytes)
}

/// Structural decode (magic, version, lengths, UTF-8, no trailing bytes).
pub fn decode(bytes: &[u8]) -> Result<Label, &'static str> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], &'static str> {
        let part = bytes.get(at..at + n).ok_or("truncated")?;
        at += n;
        Ok(part)
    };
    if take(4)? != MAGIC {
        return Err("magic");
    }
    if take(1)?[0] != 1 {
        return Err("version");
    }
    let kind = take(1)?[0];
    let spki_len = u16::from_be_bytes(take(2)?.try_into().unwrap()) as usize;
    if !(1..=512).contains(&spki_len) {
        return Err("spki length");
    }
    let spki = take(spki_len)?.to_vec();
    let sequence = u32::from_be_bytes(take(4)?.try_into().unwrap());
    let binding_type = take(1)?[0];
    let binding: [u8; 16] = take(16)?.try_into().unwrap();
    let text_len = take(1)?[0] as usize;
    if !(1..=64).contains(&text_len) {
        return Err("text length");
    }
    let text = std::str::from_utf8(take(text_len)?).map_err(|_| "utf-8")?.to_owned();
    let signed_len = 4 + 1 + 1 + 2 + spki_len + 4 + 1 + 16 + 1 + text_len;
    let sig_len = u16::from_be_bytes(take(2)?.try_into().unwrap()) as usize;
    if !(1..=256).contains(&sig_len) {
        return Err("signature length");
    }
    let signature = take(sig_len)?.to_vec();
    if at != bytes.len() {
        return Err("trailing bytes");
    }
    Ok(Label { kind, spki, sequence, binding_type, binding, text, signature, signed: bytes[..signed_len].to_vec() })
}

/// The receiver checks of one decoded label (CF-REC-43), in order.
pub fn verify(label: &Label, expected_signer: &[u8; 16], binding_type: u8, binding: &[u8; 16]) -> Result<(), &'static str> {
    let node: [u8; 16] = crate::crypto::sha256(&label.spki).map_err(|_| "signer")?[..16].try_into().unwrap();
    if &node != expected_signer {
        return Err("signer");
    }
    crate::crypto::verify_p256(&label.spki, &[DOMAIN, label.signed.as_slice()].concat(), &label.signature).map_err(|_| "signature")?;
    if label.binding_type != binding_type || &label.binding != binding {
        return Err("binding");
    }
    let kind = kind_of(label.kind).ok_or("kind")?;
    if !crate::names::is_canonical(kind, &label.text) {
        return Err("text not canonical");
    }
    Ok(())
}

pub fn container(labels: &[Vec<u8>]) -> Vec<u8> {
    let mut out = CONTAINER.to_vec();
    out.push(labels.len() as u8);
    for label in labels {
        out.extend_from_slice(&(label.len() as u16).to_be_bytes());
        out.extend_from_slice(label);
    }
    out
}

pub fn parse_container(bytes: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    if bytes.len() < 5 || &bytes[..4] != CONTAINER {
        return Err("container magic");
    }
    let count = bytes[4] as usize;
    if count > 3 {
        return Err("container count");
    }
    let mut at = 5;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = u16::from_be_bytes(bytes.get(at..at + 2).ok_or("container truncated")?.try_into().unwrap()) as usize;
        at += 2;
        out.push(bytes.get(at..at + len).ok_or("container truncated")?);
        at += len;
    }
    if at != bytes.len() {
        return Err("container trailing bytes");
    }
    Ok(out)
}

/// One session's received labels (CF-REC-43): rate limits, checks, and per
/// kind the label with the highest sequence. Nothing outlives the session.
#[derive(Default)]
pub struct Inbox {
    accepted_at: VecDeque<u64>,
    held: HashMap<u8, Label>,
    pub log: Vec<String>,
}
impl Inbox {
    /// A received container; returns the kinds whose held label changed.
    pub fn accept(&mut self, bytes: &[u8], now_ms: u64, signer: &[u8; 16], binding_type: u8, binding: &[u8; 16]) -> Vec<u8> {
        while self.accepted_at.front().is_some_and(|t| now_ms.saturating_sub(*t) >= 60_000) {
            self.accepted_at.pop_front();
        }
        if self.accepted_at.back().is_some_and(|t| now_ms.saturating_sub(*t) < 2_000) || self.accepted_at.len() >= 8 {
            self.log.push("labels ignored reason=rate".into());
            return Vec::new();
        }
        let parts = match parse_container(bytes) {
            Ok(parts) => parts,
            Err(reason) => {
                self.log.push(format!("SECURITY LABEL REJECTED kind=- reason={reason}"));
                return Vec::new();
            }
        };
        self.accepted_at.push_back(now_ms);
        let mut changed = Vec::new();
        for part in parts {
            let label = match decode(part) {
                Ok(label) => label,
                Err(reason) => {
                    self.log.push(format!("SECURITY LABEL REJECTED kind=- reason={reason}"));
                    continue;
                }
            };
            if kind_of(label.kind).is_none() {
                continue; // unknown kinds are skipped silently
            }
            if let Err(reason) = verify(&label, signer, binding_type, binding) {
                self.log.push(format!("SECURITY LABEL REJECTED kind={} reason={reason}", label.kind));
                continue;
            }
            if self.held.get(&label.kind).is_some_and(|held| label.sequence <= held.sequence) {
                continue;
            }
            changed.push(label.kind);
            self.held.insert(label.kind, label);
        }
        changed
    }
    pub fn text(&self, kind: u8) -> Option<&str> {
        self.held.get(&kind).map(|l| l.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexb(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }
    fn vectors() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/vectors/conformance/labels.json")).unwrap()
    }
    fn vector(id: &str) -> serde_json::Value {
        vectors()["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone()
    }

    #[test]
    fn the_signed_part_and_a_complete_label_match_android() {
        let v = vector("labels-002");
        let i = &v["inputs"];
        let binding: [u8; 16] = hexb(i["binding"].as_str().unwrap()).try_into().unwrap();
        let part = signed_part(3, &hexb("3001"), 0, 1, &binding, "Hi");
        assert_eq!(part, hexb(v["expected"]["signed_part"].as_str().unwrap()));
        // labels-005: Android's signed label verifies here and decodes field by field.
        let v = vector("labels-005");
        let i = &v["inputs"];
        let bytes = hexb(v["expected"]["label"].as_str().unwrap());
        let label = decode(&bytes).unwrap();
        assert_eq!((label.kind, label.text.as_str(), label.sequence), (1, "Asha", 0));
        let signer: [u8; 16] = hexb(i["identity_node_id"].as_str().unwrap()).try_into().unwrap();
        let binding: [u8; 16] = hexb(i["binding"].as_str().unwrap()).try_into().unwrap();
        assert_eq!(verify(&label, &signer, 1, &binding), Ok(()));
        assert_eq!([DOMAIN, label.signed.as_slice()].concat(), hexb(i["signing_input"].as_str().unwrap()));
        // labels-006: wrong signer, another call.
        assert_eq!(verify(&label, &[0; 16], 1, &binding), Err("signer"));
        assert_eq!(verify(&label, &signer, 1, &[0x11; 16]), Err("binding"));
        // A flipped signature byte fails the signature check.
        let mut forged = bytes.clone();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        assert!(verify(&decode(&forged).unwrap(), &signer, 1, &binding).is_err());
        // labels-004: a container of four is refused.
        assert_eq!(parse_container(&hexb("424c423104")).unwrap_err(), "container count");
        assert_eq!(parse_container(&hexb("424c423100")).unwrap(), Vec::<&[u8]>::new());
    }

    #[test]
    fn the_inbox_keeps_the_higher_sequence_and_rate_limits() {
        let v = vector("labels-007");
        let i = &v["inputs"];
        let signer: [u8; 16] = hexb(i["identity_node_id"].as_str().unwrap()).try_into().unwrap();
        let binding: [u8; 16] = hexb(i["binding"].as_str().unwrap()).try_into().unwrap();
        let steps = i["steps"].as_array().unwrap();
        let mut inbox = Inbox::default();
        let first = hexb(steps[0]["container"].as_str().unwrap());
        assert_eq!(inbox.accept(&first, 0, &signer, 1, &binding), vec![1]);
        assert_eq!(inbox.text(1), Some("Asha"));
        // Sequence 0 after 1, three seconds later: ignored; the name stays.
        let second = hexb(steps[1]["container"].as_str().unwrap());
        assert!(inbox.accept(&second, 3_000, &signer, 1, &binding).is_empty());
        assert_eq!(inbox.text(1), Some("Asha"));
        // A container inside 2 s of an accepted one is ignored.
        assert!(inbox.accept(&first, 4_000, &signer, 1, &binding).is_empty());
        assert!(inbox.log.iter().any(|l| l == "labels ignored reason=rate"));
    }

    #[test]
    fn a_label_signed_here_verifies_for_this_identity() {
        let identity = crate::identity::Identity::ephemeral().unwrap();
        let spki = SigningIdentity::public_key(&identity).unwrap();
        let node: [u8; 16] = crate::crypto::sha256(&spki).unwrap()[..16].try_into().unwrap();
        let call = [7u8; 16];
        let bytes = sign(&identity, KIND_NAME, 1, BINDING_CALL, &call, "Asha").unwrap();
        let label = decode(&bytes).unwrap();
        assert_eq!(verify(&label, &node, BINDING_CALL, &call), Ok(()));
        assert!(sign(&identity, KIND_NAME, 1, BINDING_CALL, &call, "  two  spaces ").is_err(), "only canonical text is signed");
        let mut inbox = Inbox::default();
        assert_eq!(inbox.accept(&container(&[bytes]), 0, &node, BINDING_CALL, &call), vec![KIND_NAME]);
        assert_eq!(inbox.text(KIND_NAME), Some("Asha"));
    }
}
