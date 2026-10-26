//! BMH2 hello, verification and directional key agreement. BMH3 negotiation and
//! transport/session orchestration belong to their respective state machines.
use crate::{
    crypto::{self, Ephemeral},
    framing::MAX_HANDSHAKE_BYTES,
};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Format,
    Role,
    Identity,
    PeerRejected,
    Crypto(crypto::Error),
}
impl From<crypto::Error> for Error {
    fn from(error: crypto::Error) -> Self {
        Self::Crypto(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BMH2 error: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Initiator = 1,
    Responder = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Link,
    Endpoint,
}
impl Purpose {
    fn signature_context(self) -> &'static [u8] {
        match self {
            Self::Link => b"BluetoothVideoMesh/LinkHandshake/1",
            Self::Endpoint => b"BluetoothVideoMesh/EndToEndHandshake/1",
        }
    }
    fn key_context(self) -> &'static [u8] {
        match self {
            Self::Link => b"BluetoothVideoMesh/LinkSession/1",
            Self::Endpoint => b"BluetoothVideoMesh/EndToEndSession/1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub transport: u8,
    pub encode_codecs: u8,
    pub decode_codecs: u8,
    pub maximum_bitrate_kbps: u16,
    pub maximum_children: u8,
}
impl Capabilities {
    pub fn encode(self) -> [u8; 7] {
        let rate = self.maximum_bitrate_kbps.to_be_bytes();
        [
            1,
            self.transport,
            self.encode_codecs,
            self.decode_codecs,
            rate[0],
            rate[1],
            self.maximum_children,
        ]
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 7 || bytes[0] != 1 {
            return Err(Error::Format);
        }
        Ok(Self {
            transport: bytes[1],
            encode_codecs: bytes[2],
            decode_codecs: bytes[3],
            maximum_bitrate_kbps: u16::from_be_bytes([bytes[4], bytes[5]]),
            maximum_children: bytes[6],
        })
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], Error> {
        let result = self.0.get(..size).ok_or(Error::Format)?;
        self.0 = &self.0[size..];
        Ok(result)
    }
    fn block(&mut self, maximum: usize) -> Result<Vec<u8>, Error> {
        let size = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        if !(1..=maximum).contains(&size) {
            return Err(Error::Format);
        }
        Ok(self.take(size)?.to_vec())
    }
}
fn write_block(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u16).to_be_bytes());
    output.extend_from_slice(value);
}

/// Parsed metadata is untrusted until verification; fields cannot be modified
/// after parsing or after they have been included in the signed transcript.
#[derive(Clone)]
pub struct Hello {
    role: Role,
    node_id: [u8; 16],
    identity_key: Vec<u8>,
    ephemeral_key: Vec<u8>,
    nonce: [u8; 32],
    capabilities: Capabilities,
    signature: Vec<u8>,
}
impl Hello {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > MAX_HANDSHAKE_BYTES {
            return Err(Error::Format);
        }
        let mut reader = Reader(bytes);
        if reader.take(4)? != b"BMH2" {
            return Err(Error::Format);
        }
        let role = match reader.take(1)?[0] {
            1 => Role::Initiator,
            2 => Role::Responder,
            _ => return Err(Error::Role),
        };
        let node_id = reader.take(16)?.try_into().unwrap();
        let identity_key = reader.block(512)?;
        let ephemeral_key = reader.block(512)?;
        let nonce = reader.take(32)?.try_into().unwrap();
        let capabilities = Capabilities::decode(reader.take(7)?)?;
        let signature = reader.block(256)?;
        if !reader.0.is_empty() {
            return Err(Error::Format);
        }
        Ok(Self {
            role,
            node_id,
            identity_key,
            ephemeral_key,
            nonce,
            capabilities,
            signature,
        })
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn node_id(&self) -> &[u8; 16] {
        &self.node_id
    }
    pub fn identity_key(&self) -> &[u8] {
        &self.identity_key
    }
    pub fn ephemeral_key(&self) -> &[u8] {
        &self.ephemeral_key
    }
    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
    pub fn unsigned_encoding(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(248);
        output.extend_from_slice(b"BMH2");
        output.push(self.role as u8);
        output.extend_from_slice(&self.node_id);
        write_block(&mut output, &self.identity_key);
        write_block(&mut output, &self.ephemeral_key);
        output.extend_from_slice(&self.nonce);
        output.extend_from_slice(&self.capabilities.encode());
        output
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut output = self.unsigned_encoding();
        write_block(&mut output, &self.signature);
        output
    }
    pub fn verify(&self, purpose: Purpose) -> Result<AuthenticatedPeer, Error> {
        let fingerprint = crypto::sha256(&self.identity_key)?;
        if fingerprint[..16] != self.node_id {
            return Err(Error::Identity);
        }
        let signed = [purpose.signature_context(), &self.unsigned_encoding()].concat();
        crypto::verify_p256(&self.identity_key, &signed, &self.signature)?;
        crypto::validate_agreement_public(&self.ephemeral_key)?;
        let mut capabilities = self.capabilities;
        let mut build_tag = None;
        if purpose == Purpose::Link && capabilities.transport & 0x80 != 0 {
            let tag = u16::from_be_bytes(self.nonce[..2].try_into().unwrap());
            if tag & 0x7fff != 0 {
                build_tag = Some(BuildTag {
                    release: tag & 0x7fff,
                    project_key: tag & 0x8000 != 0,
                });
            }
            capabilities.transport &= 0x7f;
        }
        Ok(AuthenticatedPeer {
            node_id: self.node_id,
            fingerprint,
            capabilities,
            build_tag,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildTag {
    pub release: u16,
    pub project_key: bool,
}
/// Signature-authenticated identity only: discovery binding/contact decisions
/// must still be enforced by the session owner before accepting keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    pub node_id: [u8; 16],
    pub fingerprint: [u8; 32],
    pub capabilities: Capabilities,
    pub build_tag: Option<BuildTag>,
}

/// Implemented by the persistent CNG identity owner. No volatile identity is
/// created implicitly, and no signing private key is passed to this module.
pub trait SigningIdentity {
    fn public_key(&self) -> Result<[u8; 91], crypto::Error>;
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, crypto::Error>;
}

pub struct SessionKeys {
    send: Zeroizing<[u8; 32]>,
    receive: Zeroizing<[u8; 32]>,
    peer: AuthenticatedPeer,
}
impl SessionKeys {
    pub fn send_key(&self) -> &[u8; 32] {
        &self.send
    }
    pub fn receive_key(&self) -> &[u8; 32] {
        &self.receive
    }
    pub fn peer(&self) -> &AuthenticatedPeer {
        &self.peer
    }
}

pub struct Pending {
    hello: Hello,
    ephemeral: Ephemeral,
    purpose: Purpose,
}
impl Pending {
    pub fn start(
        identity: &impl SigningIdentity,
        role: Role,
        capabilities: Capabilities,
        purpose: Purpose,
    ) -> Result<Self, Error> {
        // Link-hello bit 7 says nonce bytes 0-1 are a build tag. This build sends
        // no tag, so a stray bit would sign a random release claim.
        if purpose == Purpose::Link && capabilities.transport & 0x80 != 0 {
            return Err(Error::Format);
        }
        let ephemeral = Ephemeral::generate()?;
        let identity_key = identity.public_key()?;
        let mut hello = Hello {
            role,
            node_id: crypto::sha256(&identity_key)?[..16].try_into().unwrap(),
            identity_key: identity_key.to_vec(),
            ephemeral_key: ephemeral.public_key()?.to_vec(),
            nonce: crypto::random_nonce()?,
            capabilities,
            signature: Vec::new(),
        };
        hello.signature =
            identity.sign(&[purpose.signature_context(), &hello.unsigned_encoding()].concat())?;
        if !(1..=256).contains(&hello.signature.len()) {
            return Err(Error::Format);
        }
        hello.verify(purpose)?;
        Ok(Self {
            hello,
            ephemeral,
            purpose,
        })
    }
    pub fn hello(&self) -> &Hello {
        &self.hello
    }
    /// The mandatory owner check must bind the selected advert and enforce
    /// verified-contact mismatch policy. No key material is derived/returned if
    /// it refuses. BMH2 is never selected as an automatic fallback from BMH3.
    /// Consuming self destroys the ephemeral even if any check fails.
    pub fn finish(
        self,
        remote: &Hello,
        accept_peer: impl FnOnce(&AuthenticatedPeer) -> Result<(), Error>,
    ) -> Result<SessionKeys, Error> {
        if self.hello.role == remote.role {
            return Err(Error::Role);
        }
        let peer = remote.verify(self.purpose)?;
        accept_peer(&peer)?;
        let (initiator, responder) = if self.hello.role == Role::Initiator {
            (&self.hello, remote)
        } else {
            (remote, &self.hello)
        };
        let salt =
            crypto::sha256(&[initiator.nonce.as_slice(), responder.nonce.as_slice()].concat())?;
        let transcript = crypto::sha256(&[initiator.encode(), responder.encode()].concat())?;
        let info = [self.purpose.key_context(), &transcript].concat();
        let secret = self.ephemeral.agree(&remote.ephemeral_key)?;
        let material = crypto::hkdf_sha256::<64>(&secret[..], &salt, &info)?;
        let mut send = Zeroizing::new([0; 32]);
        let mut receive = Zeroizing::new([0; 32]);
        if self.hello.role == Role::Initiator {
            send.copy_from_slice(&material[..32]);
            receive.copy_from_slice(&material[32..]);
        } else {
            receive.copy_from_slice(&material[..32]);
            send.copy_from_slice(&material[32..]);
        }
        Ok(SessionKeys {
            send,
            receive,
            peer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::tests::{FixtureIdentity, fixture_ephemeral},
        records::{Layer, Opener, Policy, Sealer},
    };
    use serde_json::Value;
    use std::cell::Cell;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap()
    }
    fn bytes(v: &Value, field: &str) -> Vec<u8> {
        v[field]
            .as_str()
            .unwrap()
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    fn hello(v: &Value, name: &str) -> Hello {
        Hello::decode(&bytes(&v[name], "encoded")).unwrap()
    }
    fn fixture_pending(v: &Value, role: Role, purpose: Purpose) -> Pending {
        let (name, key) = if role == Role::Initiator {
            ("initiatorHello", "initiatorEphemeralPrivate")
        } else {
            ("responderHello", "responderEphemeralPrivate")
        };
        let hello = hello(v, name);
        let ephemeral = fixture_ephemeral(&hello.ephemeral_key, &bytes(v, key));
        Pending {
            hello,
            ephemeral,
            purpose,
        }
    }
    fn identity(v: &Value, role: Role) -> FixtureIdentity {
        let (name, key) = if role == Role::Initiator {
            ("initiatorHello", "initiatorIdentityPrivate")
        } else {
            ("responderHello", "responderIdentityPrivate")
        };
        FixtureIdentity::new(&bytes(&v[name], "identityKey"), &bytes(v, key))
    }
    fn resign(hello: &mut Hello, identity: &FixtureIdentity, purpose: Purpose) {
        hello.signature = identity
            .sign(&[purpose.signature_context(), &hello.unsigned_encoding()].concat())
            .unwrap();
    }
    fn pinned(expected: &AuthenticatedPeer, actual: &AuthenticatedPeer) -> Result<(), Error> {
        if expected.fingerprint == actual.fingerprint && expected.node_id == actual.node_id {
            Ok(())
        } else {
            Err(Error::PeerRejected)
        }
    }

    #[test]
    fn all_reference_hellos_round_trip_and_bind_each_signed_field() {
        let v = vectors();
        for (domain, purpose) in [("link", Purpose::Link), ("endpoint", Purpose::Endpoint)] {
            for (name, role) in [
                ("initiatorHello", Role::Initiator),
                ("responderHello", Role::Responder),
            ] {
                let entry = &v[domain][name];
                let h = hello(&v[domain], name);
                assert_eq!(h.encode(), bytes(entry, "encoded"));
                assert_eq!(h.unsigned_encoding(), bytes(entry, "unsigned"));
                assert_eq!(h.role(), role);
                assert_eq!(h.node_id().as_slice(), bytes(entry, "nodeId"));
                assert_eq!(h.identity_key(), bytes(entry, "identityKey"));
                assert_eq!(h.ephemeral_key(), bytes(entry, "ephemeralKey"));
                assert_eq!(h.nonce().as_slice(), bytes(entry, "nonce"));
                assert_eq!(
                    h.capabilities().encode().as_slice(),
                    bytes(entry, "capabilities")
                );
                assert_eq!(h.signature(), bytes(entry, "signature"));
                assert_eq!(h.verify(purpose).unwrap().node_id, h.node_id);
                let wrong = if purpose == Purpose::Link {
                    Purpose::Endpoint
                } else {
                    Purpose::Link
                };
                assert!(h.verify(wrong).is_err());
            }
        }
    }
    #[test]
    fn both_roles_derive_exact_reference_keys_after_the_owner_check() {
        let v = vectors();
        for (domain, purpose, layer) in [
            ("link", Purpose::Link, Layer::Hop),
            ("endpoint", Purpose::Endpoint, Layer::Endpoint),
        ] {
            let v = &v[domain];
            let a = fixture_pending(v, Role::Initiator, purpose);
            let b = fixture_pending(v, Role::Responder, purpose);
            let ah = a.hello.clone();
            let bh = b.hello.clone();
            let expected_a = ah.verify(purpose).unwrap();
            let expected_b = bh.verify(purpose).unwrap();
            let ak = a.finish(&bh, |peer| pinned(&expected_b, peer)).unwrap();
            let bk = b.finish(&ah, |peer| pinned(&expected_a, peer)).unwrap();
            assert_eq!(
                ak.send_key().as_slice(),
                bytes(v, "initiatorToResponderKey")
            );
            assert_eq!(
                ak.receive_key().as_slice(),
                bytes(v, "responderToInitiatorKey")
            );
            assert_eq!(ak.send_key(), bk.receive_key());
            assert_eq!(bk.send_key(), ak.receive_key());
            assert_eq!(ak.peer().fingerprint, expected_b.fingerprint);
            let kind = if layer == Layer::Hop { 6 } else { 8 };
            for (send, receive) in [
                (ak.send_key(), bk.receive_key()),
                (bk.send_key(), ak.receive_key()),
            ] {
                let mut seal = Sealer::new(layer, send, Policy::default()).unwrap();
                let mut open = Opener::new(layer, receive, Policy::default()).unwrap();
                let record = seal.seal(kind, 0, b"native loopback fixture").unwrap();
                assert_eq!(
                    open.open(&record).unwrap().plaintext,
                    b"native loopback fixture"
                );
            }
        }
    }
    #[test]
    fn fresh_native_handshakes_agree_without_reusing_nonce_or_ephemeral() {
        let v = vectors();
        let v = &v["link"];
        let a_identity = identity(v, Role::Initiator);
        let b_identity = identity(v, Role::Responder);
        let caps = hello(v, "initiatorHello").capabilities;
        for purpose in [Purpose::Link, Purpose::Endpoint] {
            let a = Pending::start(&a_identity, Role::Initiator, caps, purpose).unwrap();
            let b = Pending::start(&b_identity, Role::Responder, caps, purpose).unwrap();
            assert_ne!(a.hello.nonce, b.hello.nonce);
            assert_ne!(a.hello.ephemeral_key, b.hello.ephemeral_key);
            let ah = a.hello.clone();
            let bh = b.hello.clone();
            let expected_a = ah.verify(purpose).unwrap();
            let expected_b = bh.verify(purpose).unwrap();
            let ak = a.finish(&bh, |peer| pinned(&expected_b, peer)).unwrap();
            let bk = b.finish(&ah, |peer| pinned(&expected_a, peer)).unwrap();
            assert_eq!(ak.send_key(), bk.receive_key());
            assert_eq!(ak.receive_key(), bk.send_key());
        }
    }
    #[test]
    fn every_truncated_hello_and_invalid_length_version_role_or_trailer_refuses() {
        let v = vectors();
        let original = bytes(&v["link"]["initiatorHello"], "encoded");
        for cut in 0..original.len() {
            assert!(Hello::decode(&original[..cut]).is_err(), "cut {cut}");
        }
        let mut cases = Vec::new();
        for (offset, field) in [
            (21, 0),
            (21, 513),
            (114, 0),
            (114, 513),
            (246, 0),
            (246, 257),
        ] {
            let mut changed = original.clone();
            changed[offset..offset + 2].copy_from_slice(&(field as u16).to_be_bytes());
            cases.push(changed);
        }
        for (offset, value) in [(3, b'1'), (4, 0), (4, 3), (239, 0), (239, 2)] {
            let mut changed = original.clone();
            changed[offset] = value;
            cases.push(changed);
        }
        let mut trailing = original;
        trailing.push(0);
        cases.push(trailing);
        cases.push(vec![0; 2049]);
        for changed in cases {
            assert!(Hello::decode(&changed).is_err());
        }
        assert!(Capabilities::decode(&[1; 6]).is_err());
        assert!(Capabilities::decode(&[1; 8]).is_err());
    }
    #[test]
    fn every_single_byte_hello_mutation_is_rejected_before_acceptance() {
        let v = vectors();
        for (domain, purpose) in [("link", Purpose::Link), ("endpoint", Purpose::Endpoint)] {
            for name in ["initiatorHello", "responderHello"] {
                let original = bytes(&v[domain][name], "encoded");
                for index in 0..original.len() {
                    let mut changed = original.clone();
                    changed[index] ^= 1;
                    assert!(
                        Hello::decode(&changed)
                            .and_then(|h| h.verify(purpose))
                            .is_err(),
                        "{domain} {name} byte {index}"
                    );
                }
            }
        }
    }
    #[test]
    fn invalid_peer_crypto_role_and_contact_refusal_never_return_keys() {
        let v = vectors();
        let v = &v["link"];
        let called = Cell::new(false);
        let accept = |_: &AuthenticatedPeer| {
            called.set(true);
            Ok(())
        };
        let same = hello(v, "initiatorHello");
        assert!(matches!(
            fixture_pending(v, Role::Initiator, Purpose::Link).finish(&same, accept),
            Err(Error::Role)
        ));
        assert!(!called.get());
        let mut bad = hello(v, "responderHello");
        *bad.signature.last_mut().unwrap() ^= 1;
        assert!(
            fixture_pending(v, Role::Initiator, Purpose::Link)
                .finish(&bad, |_| {
                    called.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(!called.get());
        let good = hello(v, "responderHello");
        assert!(matches!(
            fixture_pending(v, Role::Initiator, Purpose::Link)
                .finish(&good, |_| Err(Error::PeerRejected)),
            Err(Error::PeerRejected)
        ));
        let mut wrong_node = good.clone();
        wrong_node.node_id[0] ^= 1;
        assert!(matches!(
            wrong_node.verify(Purpose::Link),
            Err(Error::Identity)
        ));
        let mut invalid_point = good;
        invalid_point.ephemeral_key[27..].fill(0);
        resign(
            &mut invalid_point,
            &identity(v, Role::Responder),
            Purpose::Link,
        );
        assert!(invalid_point.verify(Purpose::Link).is_err());
    }
    #[test]
    fn verified_link_build_tag_is_removed_from_capabilities_but_endpoint_bit_is_preserved() {
        let v = vectors();
        let v = &v["link"];
        let signer = identity(v, Role::Responder);
        for raw in [0_u16, 1, 0x7fff, 0x8000, 0x8001, 0xffff] {
            let mut h = hello(v, "responderHello");
            h.capabilities.transport |= 0x80;
            h.nonce[..2].copy_from_slice(&raw.to_be_bytes());
            for purpose in [Purpose::Link, Purpose::Endpoint] {
                resign(&mut h, &signer, purpose);
                let peer = h.verify(purpose).unwrap();
                if purpose == Purpose::Link {
                    assert_eq!(peer.capabilities.transport & 0x80, 0);
                    assert_eq!(
                        peer.build_tag,
                        if raw & 0x7fff == 0 {
                            None
                        } else {
                            Some(BuildTag {
                                release: raw & 0x7fff,
                                project_key: raw & 0x8000 != 0,
                            })
                        }
                    );
                } else {
                    assert_ne!(peer.capabilities.transport & 0x80, 0);
                    assert_eq!(peer.build_tag, None);
                }
            }
        }
        let signer = identity(v, Role::Initiator);
        let mut caps = hello(v, "initiatorHello").capabilities;
        caps.transport |= 0x80;
        assert!(matches!(
            Pending::start(&signer, Role::Initiator, caps, Purpose::Link),
            Err(Error::Format)
        ));
        let endpoint = Pending::start(&signer, Role::Initiator, caps, Purpose::Endpoint).unwrap();
        assert_ne!(endpoint.hello().capabilities().transport & 0x80, 0);
        for mask in 0..=255 {
            let caps = Capabilities {
                transport: mask,
                encode_codecs: mask,
                decode_codecs: mask,
                maximum_bitrate_kbps: u16::MAX,
                maximum_children: mask,
            };
            assert_eq!(Capabilities::decode(&caps.encode()).unwrap(), caps);
        }
    }
}
