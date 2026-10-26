//! Existing BFG1 grants and BMV1 routing envelopes. A grant is secret plaintext
//! carried inside an authenticated endpoint session, never a discovery payload.
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Header,
    Size,
    Sequence,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stream envelope error: {self:?}")
    }
}
impl std::error::Error for Error {}

pub struct Grant {
    source: [u8; 16],
    stream: [u8; 16],
    key: Zeroizing<[u8; 32]>,
}
impl Grant {
    /// Input must be BME1 GROUP_KEY plaintext from the admitted parent. Its
    /// source still needs the contact/mismatch policy; this parse grants no trust.
    pub fn decode(raw: &[u8]) -> Result<Self, Error> {
        if raw.len() != 68 || &raw[..4] != b"BFG1" {
            return Err(Error::Header);
        }
        Ok(Self {
            source: raw[4..20].try_into().unwrap(),
            stream: raw[20..36].try_into().unwrap(),
            key: Zeroizing::new(raw[36..].try_into().unwrap()),
        })
    }
    pub fn source(&self) -> &[u8; 16] {
        &self.source
    }
    pub fn stream(&self) -> &[u8; 16] {
        &self.stream
    }
    pub(crate) fn key(&self) -> &[u8; 32] {
        &self.key
    }
    pub fn encode(&self) -> Zeroizing<[u8; 68]> {
        let mut raw = Zeroizing::new([0; 68]);
        raw[..4].copy_from_slice(b"BFG1");
        raw[4..20].copy_from_slice(&self.source);
        raw[20..36].copy_from_slice(&self.stream);
        raw[36..].copy_from_slice(&self.key[..]);
        raw
    }
}

pub const HEADER_BYTES: usize = 50;
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy)]
pub struct Envelope<'a> {
    pub stream: [u8; 16],
    pub source: [u8; 16],
    pub sequence: u64,
    pub ttl: u8,
    pub flags: u8,
    pub payload: &'a [u8],
}
impl<'a> Envelope<'a> {
    /// Structural parse only. The hop/endpoint reader authenticates it and binds
    /// its source, stream, sequence and mapped media flags before delivery.
    pub fn decode(raw: &'a [u8]) -> Result<Self, Error> {
        if raw.len() < HEADER_BYTES || &raw[..4] != b"BMV1" {
            return Err(Error::Header);
        }
        let length = u32::from_be_bytes(raw[46..50].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD || raw.len() != HEADER_BYTES + length {
            return Err(Error::Size);
        }
        let sequence = u64::from_be_bytes(raw[36..44].try_into().unwrap());
        if sequence > i64::MAX as u64 {
            return Err(Error::Sequence);
        }
        Ok(Self {
            stream: raw[4..20].try_into().unwrap(),
            source: raw[20..36].try_into().unwrap(),
            sequence,
            ttl: raw[44],
            flags: raw[45],
            payload: &raw[50..],
        })
    }
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(Error::Size);
        }
        if self.sequence > i64::MAX as u64 {
            return Err(Error::Sequence);
        }
        let mut raw = Vec::with_capacity(HEADER_BYTES + self.payload.len());
        raw.extend_from_slice(b"BMV1");
        raw.extend_from_slice(&self.stream);
        raw.extend_from_slice(&self.source);
        raw.extend_from_slice(&self.sequence.to_be_bytes());
        raw.extend_from_slice(&[self.ttl, self.flags]);
        raw.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        raw.extend_from_slice(self.payload);
        Ok(raw)
    }
    /// Relays change TTL only; the sealed endpoint payload remains byte-identical.
    pub fn forwarded(self) -> Result<Option<Vec<u8>>, Error> {
        if self.ttl <= 1 {
            return Ok(None);
        }
        Self {
            ttl: self.ttl - 1,
            ..self
        }
        .encode()
        .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{Layer, Opener, Policy};
    use serde_json::Value;
    fn hex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    #[test]
    fn frozen_grant_and_all_four_envelopes_match_without_resealing() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap();
        let v = &v["mesh"];
        let raw = hex(v["grant"].as_str().unwrap());
        let grant = Grant::decode(&raw).unwrap();
        assert_eq!(grant.encode().as_slice(), raw);
        assert_eq!(
            grant.source().as_slice(),
            hex(v["sourceNodeId"].as_str().unwrap())
        );
        assert_eq!(grant.key().as_slice(), hex(v["grantKey"].as_str().unwrap()));
        let key: [u8; 32] = hex(v["linkKey"].as_str().unwrap()).try_into().unwrap();
        let mut hop = Opener::new(Layer::Hop, &key, Policy::default()).unwrap();
        let mut content = Opener::new(Layer::Endpoint, grant.key(), Policy::default()).unwrap();
        let raw = hex(v["stream"].as_str().unwrap());
        let mut rest = raw.as_slice();
        let mut count = 0;
        while !rest.is_empty() {
            let size = u32::from_be_bytes(rest[13..17].try_into().unwrap()) as usize + 17;
            let record = hop.open(&rest[..size]).unwrap();
            rest = &rest[size..];
            assert_eq!(record.header.kind, 7);
            let e = Envelope::decode(&record.plaintext).unwrap();
            assert_eq!(e.encode().unwrap(), record.plaintext);
            assert_eq!(&e.source, grant.source());
            assert_eq!(&e.stream, grant.stream());
            let opened = content.open(e.payload).unwrap();
            assert!(matches!(opened.header.kind, 1 | 2 | 4));
            let forwarded = e.forwarded().unwrap().unwrap();
            assert_eq!(forwarded[44] + 1, e.ttl);
            assert_eq!(&forwarded[..44], &record.plaintext[..44]);
            assert_eq!(&forwarded[45..], &record.plaintext[45..]);
            count += 1;
        }
        assert_eq!(count, 4);
    }
    #[test]
    fn malformed_grants_envelopes_and_ttl_boundaries() {
        let raw = Envelope {
            stream: [1; 16],
            source: [2; 16],
            sequence: 0,
            ttl: 1,
            flags: 0,
            payload: b"sealed",
        }
        .encode()
        .unwrap();
        for n in 0..raw.len() {
            assert!(Envelope::decode(&raw[..n]).is_err());
        }
        assert!(
            Envelope::decode(&raw)
                .unwrap()
                .forwarded()
                .unwrap()
                .is_none()
        );
        let mut bad = raw.clone();
        bad.push(0);
        assert!(Envelope::decode(&bad).is_err());
        bad = raw.clone();
        bad[46..50].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Envelope::decode(&bad).is_err());
        bad = raw.clone();
        bad[36] = 0x80;
        assert!(matches!(Envelope::decode(&bad), Err(Error::Sequence)));
        bad = raw;
        bad[44] = 0;
        assert!(
            Envelope::decode(&bad)
                .unwrap()
                .forwarded()
                .unwrap()
                .is_none()
        );
        for n in 0..68 {
            assert!(Grant::decode(&vec![0; n]).is_err());
        }
        let mut grant = [0; 68];
        grant[..4].copy_from_slice(b"BFG1");
        assert!(Grant::decode(&grant).is_ok());
        grant[3] = b'2';
        assert!(Grant::decode(&grant).is_err());
    }
}
