//! The identity reveal of a pseudonymous BMH3 end-to-end session (PROTOCOL
//! "Phase 2: pseudonymous stranger identities", `REVEAL`, BDX1 kind 4). Message
//! construction, signature input, verification and the per-session order. The
//! call owner decides when (after the user accepted) and still runs the
//! verified-contact gate and the discovery binding of the selected offer.
use super::Error;
use crate::{
    bmh2::SigningIdentity,
    crypto,
    rotating::{self, ENTRY_BYTES, MAX_ENTRIES, OpeningEntry},
};
use zeroize::Zeroizing;

pub const ROLE_CALLER: u8 = 1;
pub const ROLE_CALLEE: u8 = 2;
const DOMAIN: &[u8] = b"BluetoothVideoMesh/BMH3/Reveal/1";
const KEY_INFO: &[u8] = b"BluetoothVideoMesh/BMH3/Reveal/1 key";
/// "BDX1" | kind 4 | role | SPKI length (2) | SPKI (91) | count.
const FIXED_BYTES: usize = 4 + 1 + 1 + 2 + 91 + 1;

impl From<rotating::Error> for Error {
    fn from(error: rotating::Error) -> Self {
        match error {
            rotating::Error::Format => Self::Format,
            rotating::Error::Crypto(error) => Self::Crypto(error),
        }
    }
}

/// `"BDX1" | 4 | role | 0x005b | SPKI (91) | count | count x entry (43) | signature (64)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reveal {
    pub role: u8,
    pub spki: [u8; 91],
    pub entries: Vec<OpeningEntry>,
    pub signature: [u8; 64],
}
impl Reveal {
    fn validate(&self) -> Result<(), Error> {
        if !matches!(self.role, ROLE_CALLER | ROLE_CALLEE)
            || self.entries.len() > MAX_ENTRIES
            || self.role == ROLE_CALLER && !self.entries.is_empty()
        {
            return Err(Error::Format);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut output = Vec::with_capacity(FIXED_BYTES + self.entries.len() * ENTRY_BYTES + 64);
        output.extend_from_slice(b"BDX1");
        output.extend_from_slice(&[4, self.role, 0, 91]);
        output.extend_from_slice(&self.spki);
        output.extend_from_slice(&rotating::encode_body(&self.entries)?);
        output.extend_from_slice(&self.signature);
        Ok(output)
    }
    /// Strict: every other magic, kind, role, length or count is a protocol error.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < FIXED_BYTES + 64
            || &bytes[..4] != b"BDX1"
            || bytes[4] != 4
            || bytes[6..8] != [0, 91]
        {
            return Err(Error::Format);
        }
        let count = bytes[FIXED_BYTES - 1] as usize;
        if count > MAX_ENTRIES || bytes.len() != FIXED_BYTES + count * ENTRY_BYTES + 64 {
            return Err(Error::Format);
        }
        let body_end = FIXED_BYTES + count * ENTRY_BYTES;
        let reveal = Self {
            role: bytes[5],
            spki: bytes[8..99].try_into().unwrap(),
            entries: rotating::decode_body(&bytes[FIXED_BYTES - 1..body_end])?,
            signature: bytes[body_end..].try_into().unwrap(),
        };
        reveal.validate()?;
        Ok(reveal)
    }
}

/// `D_R "/" Role " signature" 0x00 TH_R`, with
/// `TH_R = SHA-256(D_R | 0x00 | role | K | SHA-256(own pseudonym SPKI) | SHA-256(peer pseudonym SPKI) | SPKI | prior | body)`
/// and `K = HKDF-Expand(EXP, ".../Reveal/1 key", 32)`; the exporter itself is never signed.
pub fn signing_input(
    role: u8,
    exporter: &[u8; 32],
    own_pseudonym: &[u8; 91],
    peer_pseudonym: &[u8; 91],
    real: &[u8; 91],
    entries: &[OpeningEntry],
    prior: Option<&[u8; 32]>,
) -> Result<Vec<u8>, Error> {
    let name: &[u8] = match role {
        ROLE_CALLER => b"/Caller signature",
        ROLE_CALLEE => b"/Callee signature",
        _ => return Err(Error::Format),
    };
    let key = crypto::hkdf_expand::<32>(exporter, KEY_INFO)?;
    let body = rotating::encode_body(entries)?;
    let hashed = Zeroizing::new(
        [
            DOMAIN,
            &[0, role],
            &key[..],
            &crypto::sha256(own_pseudonym)?,
            &crypto::sha256(peer_pseudonym)?,
            real,
            prior.map_or(&[][..], |p| &p[..]),
            &body,
        ]
        .concat(),
    );
    Ok([DOMAIN, name, &[0], &crypto::sha256(&hashed)?].concat())
}

/// Signs this phone's reveal with its real identity; the signature is checked
/// against the real key before it is returned.
pub fn build(
    role: u8,
    real: &dyn SigningIdentity,
    exporter: &[u8; 32],
    own_pseudonym: &[u8; 91],
    peer_pseudonym: &[u8; 91],
    entries: &[OpeningEntry],
    prior: Option<&[u8; 32]>,
) -> Result<Reveal, Error> {
    let spki = real.public_key()?;
    crypto::validate_agreement_public(&spki)?;
    let mut reveal = Reveal {
        role,
        spki,
        entries: entries.to_vec(),
        signature: [0; 64],
    };
    // Refuse an invalid role or a caller opening before anything is signed.
    reveal.validate()?;
    if (role == ROLE_CALLER) != prior.is_some() {
        return Err(Error::Format);
    }
    let input = signing_input(
        role,
        exporter,
        own_pseudonym,
        peer_pseudonym,
        &spki,
        entries,
        prior,
    )?;
    reveal.signature = crypto::der_to_raw(&real.sign(&input)?)?;
    crypto::verify_p256_raw(&spki, &input, &reveal.signature)?;
    Ok(reveal)
}

/// A reveal whose signature verified. Binding and contact checks remain the owner's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub spki: [u8; 91],
    pub node_id: [u8; 16],
    pub fingerprint: [u8; 32],
    pub entries: Vec<OpeningEntry>,
}

/// Role, canonical SPKI, not the peer's pseudonym or any of this phone's keys,
/// then the signature over this session's exporter and both pseudonyms.
pub fn verify(
    reveal: &Reveal,
    expected_role: u8,
    exporter: &[u8; 32],
    peer_pseudonym: &[u8; 91],
    own_pseudonym: &[u8; 91],
    own_keys: &[[u8; 91]],
    prior: Option<&[u8; 32]>,
) -> Result<Verified, Error> {
    reveal.validate()?;
    if reveal.role != expected_role {
        return Err(Error::Format);
    }
    crypto::validate_agreement_public(&reveal.spki)?;
    if reveal.spki == *peer_pseudonym
        || reveal.spki == *own_pseudonym
        || own_keys.contains(&reveal.spki)
    {
        return Err(Error::Policy);
    }
    let input = signing_input(
        expected_role,
        exporter,
        peer_pseudonym,
        own_pseudonym,
        &reveal.spki,
        &reveal.entries,
        prior,
    )?;
    crypto::verify_p256_raw(&reveal.spki, &input, &reveal.signature)
        .map_err(|_| Error::Signature)?;
    let fingerprint = crypto::sha256(&reveal.spki)?;
    Ok(Verified {
        spki: reveal.spki,
        node_id: fingerprint[..16].try_into().unwrap(),
        fingerprint,
        entries: reveal.entries.clone(),
    })
}

/// One session's reveal exchange: a reveal goes only to a peer that declared a
/// pseudonym, the callee's first, the caller's only after it verified the
/// callee's, at most one per direction. Any peer error latches the exchange.
pub struct RevealSession {
    local_role: u8,
    exporter: Zeroizing<[u8; 32]>,
    own_pseudonym: [u8; 91],
    peer_pseudonym: [u8; 91],
    own_keys: Vec<[u8; 91]>,
    own_pseudonymous: bool,
    peer_pseudonymous: bool,
    own_sent: bool,
    peer: Option<Verified>,
    callee_record: Option<[u8; 32]>,
    failed: bool,
}
impl RevealSession {
    /// `own_keys` must hold every key this phone could present (its real key too).
    pub fn new(
        local_role: u8,
        exporter: &[u8; 32],
        own_pseudonym: [u8; 91],
        peer_pseudonym: [u8; 91],
        own_pseudonymous: bool,
        peer_pseudonymous: bool,
        own_keys: Vec<[u8; 91]>,
    ) -> Result<Self, Error> {
        if !matches!(local_role, ROLE_CALLER | ROLE_CALLEE) {
            return Err(Error::Format);
        }
        Ok(Self {
            local_role,
            exporter: Zeroizing::new(*exporter),
            own_pseudonym,
            peer_pseudonym,
            own_keys,
            own_pseudonymous,
            peer_pseudonymous,
            own_sent: false,
            peer: None,
            callee_record: None,
            failed: false,
        })
    }
    fn peer_role(&self) -> u8 {
        if self.local_role == ROLE_CALLER {
            ROLE_CALLEE
        } else {
            ROLE_CALLER
        }
    }
    pub fn revealed(&self) -> Option<&Verified> {
        self.peer.as_ref()
    }
    /// A caller must hold a verified callee reveal before the call may proceed.
    pub fn peer_reveal_required(&self) -> bool {
        self.local_role == ROLE_CALLER && self.peer_pseudonymous && self.peer.is_none()
    }
    pub fn caller_may_reveal(&self) -> bool {
        self.local_role == ROLE_CALLER
            && self.own_pseudonymous
            && self.peer_pseudonymous
            && self.peer.is_some()
            && !self.own_sent
            && !self.failed
    }
    /// The callee passes its pinned opening entries; the caller none.
    pub fn send_own(
        &mut self,
        real: &dyn SigningIdentity,
        entries: &[OpeningEntry],
    ) -> Result<Vec<u8>, Error> {
        if !self.own_pseudonymous || !self.peer_pseudonymous || self.own_sent || self.failed {
            return Err(Error::State);
        }
        if self.local_role == ROLE_CALLER && (self.peer.is_none() || !entries.is_empty()) {
            return Err(Error::State);
        }
        let prior = if self.local_role == ROLE_CALLER {
            Some(self.callee_record.ok_or(Error::State)?)
        } else {
            None
        };
        let encoded = build(
            self.local_role,
            real,
            &self.exporter,
            &self.own_pseudonym,
            &self.peer_pseudonym,
            entries,
            prior.as_ref(),
        )?
        .encode()?;
        if self.local_role == ROLE_CALLEE {
            self.callee_record = Some(crypto::sha256(&encoded)?);
        }
        self.own_sent = true;
        Ok(encoded)
    }
    pub fn on_peer_reveal(&mut self, record: &[u8]) -> Result<&Verified, Error> {
        let result = self.on_peer_reveal_inner(record);
        if result.is_err() {
            self.failed = true;
        }
        result?;
        Ok(self.peer.as_ref().unwrap())
    }
    fn on_peer_reveal_inner(&mut self, record: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(Error::State);
        }
        if !self.peer_pseudonymous || self.peer.is_some() {
            return Err(Error::Policy);
        }
        if self.local_role == ROLE_CALLEE && !self.own_sent {
            return Err(Error::Policy);
        }
        let reveal = Reveal::decode(record)?;
        let prior = if self.local_role == ROLE_CALLEE {
            Some(self.callee_record.ok_or(Error::State)?)
        } else {
            None
        };
        let verified = verify(
            &reveal,
            self.peer_role(),
            &self.exporter,
            &self.peer_pseudonym,
            &self.own_pseudonym,
            &self.own_keys,
            prior.as_ref(),
        )?;
        if self.local_role == ROLE_CALLER {
            // The strict codec makes the received bytes the canonical encoding.
            self.callee_record = Some(crypto::sha256(record)?);
        }
        self.peer = Some(verified);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
