//! BTV3 and Opus framing, before codec admission. Payloads borrow authenticated
//! record storage; these parsers do not authenticate a source or allocate frames.
use crate::crypto;
use subtle::ConstantTimeEq;

pub const CONFIG: u8 = 1;
pub const KEY_FRAME: u8 = 2;
pub const ROTATION: u8 = 4;
pub const AUDIO: u8 = 8;
pub const INTEGRITY: u8 = 16;
pub const DISCONTINUITY: u8 = 32;
pub const AUDIO_BATCH: u8 = 64;
pub const DISCARDABLE: u8 = 128;
pub const PACKET_HEADER_BYTES: usize = 13;
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_AUDIO_UNIT: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Header,
    Size,
    Flags,
    Timestamp,
    Audio,
    Integrity,
    Crypto(crypto::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Media framing error: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    /// 2 = VP9, 3 = AV1.
    pub codec: u8,
    pub quarter_turns: u8,
    pub opus: bool,
}
impl VideoHeader {
    pub fn decode(raw: &[u8]) -> Result<Self, Error> {
        if raw.len() != 15 || &raw[..4] != b"BTV3" {
            return Err(Error::Header);
        }
        let value = Self {
            width: u16::from_be_bytes(raw[4..6].try_into().unwrap()),
            height: u16::from_be_bytes(raw[6..8].try_into().unwrap()),
            fps: raw[8],
            codec: raw[9],
            quarter_turns: raw[10],
            opus: raw[11] == 1,
        };
        if !matches!(&raw[11..], [0, 0, 0, 0] | [1, 0xbb, 0x80, 1]) {
            return Err(Error::Header);
        }
        value.validate()?;
        Ok(value)
    }
    fn validate(self) -> Result<(), Error> {
        if !(1..=4096).contains(&self.width)
            || !(1..=4096).contains(&self.height)
            || !(1..=120).contains(&self.fps)
            || !matches!(self.codec, 2 | 3)
            || self.quarter_turns > 3
        {
            return Err(Error::Header);
        }
        Ok(())
    }
    pub fn encode(self) -> Result<[u8; 15], Error> {
        self.validate()?;
        let mut raw = [0; 15];
        raw[..4].copy_from_slice(b"BTV3");
        raw[4..6].copy_from_slice(&self.width.to_be_bytes());
        raw[6..8].copy_from_slice(&self.height.to_be_bytes());
        raw[8..11].copy_from_slice(&[self.fps, self.codec, self.quarter_turns]);
        if self.opus {
            raw[11..].copy_from_slice(&[1, 0xbb, 0x80, 1]);
        }
        Ok(raw)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub flags: u8,
    pub pts_us: i64,
    pub payload: &'a [u8],
}
impl<'a> Packet<'a> {
    /// Caller supplies its stream-specific limit. Length is checked before any
    /// payload access; the outer record owner enforces its earlier ciphertext cap.
    pub fn decode(raw: &'a [u8], maximum: usize) -> Result<Self, Error> {
        let mut r = Reader(raw);
        let size = u32::from_be_bytes(r.take(4)?.try_into().unwrap()) as usize;
        if size > maximum.min(MAX_PAYLOAD) || raw.len() != PACKET_HEADER_BYTES + size {
            return Err(Error::Size);
        }
        let flags = r.byte()?;
        let pts_us = i64::from_be_bytes(r.take(8)?.try_into().unwrap());
        Ok(Self {
            flags,
            pts_us,
            payload: r.take(size)?,
        })
    }
    pub fn encode(self) -> Result<Vec<u8>, Error> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(Error::Size);
        }
        let mut raw = Vec::with_capacity(PACKET_HEADER_BYTES + self.payload.len());
        raw.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        raw.push(self.flags);
        raw.extend_from_slice(&self.pts_us.to_be_bytes());
        raw.extend_from_slice(self.payload);
        Ok(raw)
    }
    pub fn rotation(self) -> Result<u8, Error> {
        if self.flags & ROTATION == 0 || self.payload.len() != 1 || self.payload[0] > 3 {
            return Err(Error::Header);
        }
        Ok(self.payload[0])
    }
    pub fn check_integrity(self) -> Result<Self, Error> {
        if self.flags & INTEGRITY == 0 {
            return Ok(self);
        }
        if self.flags & AUDIO != 0 || self.payload.len() < 32 {
            return Err(Error::Integrity);
        }
        let digest = crypto::sha256(&self.payload[32..]).map_err(Error::Crypto)?;
        if !bool::from(digest.ct_eq(&self.payload[..32])) {
            return Err(Error::Integrity);
        }
        Ok(Self {
            flags: self.flags & !INTEGRITY,
            payload: &self.payload[32..],
            ..self
        })
    }
    pub fn split_audio(self) -> Result<Vec<Self>, Error> {
        if self.flags != AUDIO | AUDIO_BATCH {
            return Err(Error::Flags);
        }
        let mut r = Reader(self.payload);
        let magic = r.take(4)?;
        let (count, pair_delta) = match magic {
            b"BTA1" => (2, Some(r.delta()?)),
            b"BTA2" => {
                let count = r.byte()?;
                if !(3..=20).contains(&count) {
                    return Err(Error::Audio);
                }
                (count, None)
            }
            _ => return Err(Error::Audio),
        };
        let mut units = Vec::with_capacity(count as usize);
        for index in 0..count {
            let delta = match pair_delta {
                Some(delta) => {
                    if index == 0 {
                        0
                    } else {
                        delta
                    }
                }
                None => r.delta()?,
            };
            units.push(Self {
                flags: AUDIO,
                pts_us: self.pts_us.checked_add(delta).ok_or(Error::Timestamp)?,
                payload: r.block(MAX_AUDIO_UNIT)?,
            });
        }
        r.end()?;
        Ok(units)
    }
}

/// Matches Android's bare OpusHead / Codec2 marker normalization. Marker tail
/// bytes shorter than a block header are ignored there, as are unknown markers.
/// Keep that wire behavior, while bounding work by the enclosing record limit.
pub fn opus_head(config: &[u8]) -> Result<&[u8], Error> {
    let mut head = config;
    while head.starts_with(b"AOPUSHDR") {
        let mut r = Reader(head);
        let mut found = None;
        while r.0.len() >= 16 {
            let marker = r.take(8)?;
            let size = u64::from_le_bytes(r.take(8)?.try_into().unwrap());
            let size = usize::try_from(size).map_err(|_| Error::Size)?;
            let data = r.take(size)?;
            if marker == b"AOPUSHDR" {
                found = Some(data);
            }
        }
        head = found.ok_or(Error::Audio)?;
    }
    if head.len() < 19 || !head.starts_with(b"OpusHead") {
        return Err(Error::Audio);
    }
    Ok(head)
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);
impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, size: usize) -> Result<&'a [u8], Error> {
        let value = self.0.get(..size).ok_or(Error::Size)?;
        self.0 = &self.0[size..];
        Ok(value)
    }
    pub(crate) fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn block(&mut self, maximum: usize) -> Result<&'a [u8], Error> {
        let size = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        if size > maximum {
            return Err(Error::Size);
        }
        self.take(size)
    }
    fn delta(&mut self) -> Result<i64, Error> {
        let value = i32::from_be_bytes(self.take(4)?.try_into().unwrap());
        if !(0..=1_000_000).contains(&value) {
            return Err(Error::Timestamp);
        }
        Ok(i64::from(value))
    }
    pub(crate) fn end(self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    #[test]
    fn frozen_header_and_six_media_records_round_trip() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap();
        let raw = unhex(v["btv3Header"].as_str().unwrap());
        let h = VideoHeader::decode(&raw).unwrap();
        assert_eq!(
            (h.width, h.height, h.fps, h.codec, h.quarter_turns, h.opus),
            (426, 240, 15, 2, 1, true)
        );
        assert_eq!(h.encode().unwrap().as_slice(), raw);
        let mut count = 0;
        for r in v["bme1Contents"].as_array().unwrap() {
            if r["type"] != 2 {
                continue;
            }
            let raw = unhex(r["plaintext"].as_str().unwrap());
            let p = Packet::decode(&raw, MAX_PAYLOAD).unwrap();
            assert_eq!(p.encode().unwrap(), raw);
            if p.flags & AUDIO_BATCH != 0 {
                let units = p.split_audio().unwrap();
                assert_eq!(units.len(), if r["label"] == "audioPair" { 2 } else { 3 });
                assert!(
                    units
                        .windows(2)
                        .all(|w| w[1].pts_us - w[0].pts_us == 20_000)
                );
            }
            if p.flags == ROTATION {
                assert_eq!(p.rotation(), Ok(3));
            }
            if p.flags == CONFIG | AUDIO {
                assert_eq!(opus_head(p.payload).unwrap(), p.payload);
            }
            count += 1;
        }
        assert_eq!(count, 6);
    }
    #[test]
    fn header_fields_and_packet_size_are_bounded() {
        let valid = VideoHeader {
            width: 4096,
            height: 4096,
            fps: 120,
            codec: 3,
            quarter_turns: 3,
            opus: false,
        }
        .encode()
        .unwrap();
        for (index, value) in [
            (3, b'2'),
            (4, 0x11),
            (6, 0),
            (8, 0),
            (9, 1),
            (10, 4),
            (11, 1),
            (13, 1),
            (14, 1),
        ] {
            let mut raw = valid;
            raw[index] = value;
            if index == 6 {
                raw[7] = 0;
            }
            assert!(VideoHeader::decode(&raw).is_err(), "field {index}");
        }
        for n in 0..15 {
            assert!(VideoHeader::decode(&valid[..n]).is_err());
        }
        let raw = Packet {
            flags: 0,
            pts_us: -1,
            payload: &[1, 2, 3],
        }
        .encode()
        .unwrap();
        assert_eq!(Packet::decode(&raw, 3).unwrap().pts_us, -1);
        assert!(Packet::decode(&raw, 2).is_err());
        for n in 0..raw.len() {
            assert!(Packet::decode(&raw[..n], MAX_PAYLOAD).is_err());
        }
        let mut extra = raw.clone();
        extra.push(0);
        assert!(Packet::decode(&extra, MAX_PAYLOAD).is_err());
        extra[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Packet::decode(&extra, MAX_PAYLOAD).is_err());
    }
    #[test]
    fn audio_pair_rejects_bad_counts_lengths_gaps_and_overflow() {
        let valid = unhex("4254413203000000000001f800004e200001f800009c400001f8");
        let p = Packet {
            flags: AUDIO | AUDIO_BATCH,
            pts_us: 0,
            payload: &valid,
        };
        assert_eq!(p.split_audio().unwrap().len(), 3);
        for n in 0..valid.len() {
            assert!(
                Packet {
                    payload: &valid[..n],
                    ..p
                }
                .split_audio()
                .is_err()
            );
        }
        for (index, value) in [(3, b'3'), (4, 2), (4, 21), (5, 0xff), (9, 0xff)] {
            let mut bad = valid.clone();
            bad[index] = value;
            assert!(Packet { payload: &bad, ..p }.split_audio().is_err());
        }
        assert!(
            Packet {
                pts_us: i64::MAX,
                ..p
            }
            .split_audio()
            .is_err()
        );
        assert!(Packet { flags: AUDIO, ..p }.split_audio().is_err());
    }
    #[test]
    fn integrity_and_codec2_config_follow_android() {
        let mut raw = crypto::sha256(b"frame").unwrap().to_vec();
        raw.extend_from_slice(b"frame");
        let p = Packet {
            flags: KEY_FRAME | INTEGRITY,
            pts_us: 0,
            payload: &raw,
        };
        assert_eq!(p.check_integrity().unwrap().payload, b"frame");
        assert!(
            Packet {
                flags: AUDIO | INTEGRITY,
                ..p
            }
            .check_integrity()
            .is_err()
        );
        raw[32] ^= 1;
        assert!(
            Packet {
                flags: KEY_FRAME | INTEGRITY,
                pts_us: 0,
                payload: &raw
            }
            .check_integrity()
            .is_err()
        );
        let head = b"OpusHead\x01\x01\0\0\x80\xbb\0\0\0\0\0";
        let mut config = b"AOPUSHDR".to_vec();
        config.extend_from_slice(&19u64.to_le_bytes());
        config.extend_from_slice(head);
        assert_eq!(opus_head(&config), Ok(head.as_slice()));
        let mut nested = b"AOPUSHDR".to_vec();
        nested.extend_from_slice(&(config.len() as u64).to_le_bytes());
        nested.extend_from_slice(&config);
        assert_eq!(opus_head(&nested), Ok(head.as_slice()));
        config[15] = 0xff;
        assert!(opus_head(&config).is_err());
    }
}
