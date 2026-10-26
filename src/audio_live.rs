//! Experimental BTL1 profile (Android PROTOCOL, Design AL). Call only after the
//! endpoint or BFG1 owner authenticates the full original-source ID. This module
//! neither trusts discovery prefixes nor provides application admission policy.
use crate::{
    crypto,
    media::{self, Packet, Reader},
};
use subtle::ConstantTimeEq;

pub const MAX_PAYLOAD: usize = 4 + 5 + 20 * (6 + 1275) + 64;
pub const HEADER_CIPHERTEXT_CAP: usize = 4 * 1024;
pub const MEDIA_CIPHERTEXT_CAP: usize = 28 * 1024;
const CERT_DOMAIN: &[u8] = b"BluetoothVideoMesh/AudioLive/Cert/1\0";
const RECORD_DOMAIN: &[u8] = b"BluetoothVideoMesh/AudioLive/Rec/1\0";
const FRAME_US: i64 = 20_000;

/// Radio's sample-count batches. The ten-unit experiment uses Android's
/// default BTA2 cadence; the original two-unit BTA1 arm remains available.
pub struct Batcher {
    units: Vec<Vec<u8>>,
    first_pts: i64,
    limit: usize,
    muted: bool,
}
impl Batcher {
    pub fn new(ten_units: bool) -> Self {
        Self { units: Vec::new(), first_pts: 0, limit: if ten_units { 10 } else { 2 }, muted: false }
    }
    pub fn mute(&mut self, muted: bool) {
        self.muted = muted;
        if muted { for unit in &mut self.units { *unit = vec![0xf8, 0xff, 0xfe]; } }
    }
    pub fn clear(&mut self) { self.units.clear(); }
    pub fn push(&mut self, pts: i64, unit: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        if !single_20ms_mono(&unit) { return Err(Error::Opus); }
        if self.units.is_empty() { self.first_pts = pts; }
        if self.first_pts.checked_add(self.units.len() as i64 * FRAME_US) != Some(pts) { return Err(Error::Clock); }
        self.units.push(if self.muted { vec![0xf8, 0xff, 0xfe] } else { unit });
        if self.units.len() < self.limit { return Ok(None); }
        let mut payload = if self.limit == 2 { b"BTA1".to_vec() } else { b"BTA2\x0a".to_vec() };
        if self.limit == 2 { payload.extend_from_slice(&(FRAME_US as i32).to_be_bytes()); }
        for (i, unit) in self.units.drain(..).enumerate() {
            if self.limit != 2 { payload.extend_from_slice(&(i as i32 * FRAME_US as i32).to_be_bytes()); }
            payload.extend_from_slice(&(unit.len() as u16).to_be_bytes()); payload.extend_from_slice(&unit);
        }
        Ok(Some(Packet { flags: media::AUDIO | media::AUDIO_BATCH, pts_us: self.first_pts, payload: &payload }.encode()?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Header,
    Source,
    Size,
    Flags,
    Sequence,
    Config,
    Opus,
    Clock,
    Budget,
    Faulted,
    Media(media::Error),
    Crypto(crypto::Error),
}
impl From<media::Error> for Error {
    fn from(value: media::Error) -> Self {
        Self::Media(value)
    }
}
impl From<crypto::Error> for Error {
    fn from(value: crypto::Error) -> Self {
        Self::Crypto(value)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Audio-only validation error: {self:?}")
    }
}
impl std::error::Error for Error {}

pub fn fixed_header(preset: u8) -> Result<[u8; 11], Error> {
    if !(1..=3).contains(&preset) {
        return Err(Error::Header);
    }
    let mut fixed = *b"BTL1\x01\xbb\x80\x01\x14\x02\0";
    fixed[9] = preset;
    Ok(fixed)
}
fn cert_message(stream_id: &[u8; 16], fixed: &[u8], stream_key: &[u8]) -> Vec<u8> {
    [CERT_DOMAIN, stream_id, fixed, stream_key].concat()
}
fn record_message(stream_id: &[u8; 16], packet: Packet<'_>, seq: u32) -> Vec<u8> {
    [
        RECORD_DOMAIN,
        stream_id,
        &[packet.flags],
        &packet.pts_us.to_be_bytes(),
        &seq.to_be_bytes(),
        packet.payload,
    ]
    .concat()
}

/// One ephemeral source signing key; private material is never serialized.
pub struct Signer { key: crate::identity::Identity, stream: [u8; 16], sequence: u32, header: Vec<u8> }
impl Signer {
    pub fn new(identity: &impl crate::bmh2::SigningIdentity, stream: [u8; 16], preset: u8) -> Result<Self, Error> {
        use crate::bmh2::SigningIdentity;
        let key = crate::identity::Identity::ephemeral().map_err(|_| Error::Source)?;
        let stream_key = key.public_key()?; let identity_key = identity.public_key()?;
        let fixed = fixed_header(preset)?;
        let certificate = identity.sign(&cert_message(&stream, &fixed, &stream_key))?;
        let mut header = fixed.to_vec();
        for block in [&stream_key[..], &identity_key[..], &certificate[..]] { header.extend_from_slice(&(block.len() as u16).to_be_bytes()); header.extend_from_slice(block); }
        Ok(Self { key, stream, sequence: 0, header })
    }
    pub fn header(&self) -> &[u8] { &self.header }
    pub fn packet(&mut self, packet: Packet<'_>) -> Result<Vec<u8>, Error> {
        use crate::bmh2::SigningIdentity;
        if !matches!(packet.flags, media::AUDIO | 9 | 72) || packet.payload.len() + 68 > MAX_PAYLOAD { return Err(Error::Flags); }
        let seq = self.sequence;
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Sequence)?;
        let signature = crypto::der_to_raw(&self.key.sign(&record_message(&self.stream, packet, seq))?)?;
        let payload = [&seq.to_be_bytes()[..], packet.payload, &signature].concat();
        Ok(Packet { payload: &payload, ..packet }.encode()?)
    }
}

/// Constructible only by certificate verification; moves into one stream guard.
pub struct VerifiedHeader {
    preset: u8,
    stream_id: [u8; 16],
    key: crypto::Verifier,
}
impl VerifiedHeader {
    /// `source` is the full authenticated endpoint/BFG1 source, never an advert.
    /// `stream_id` is canonical UUID bytes, or all zeros for a direct stream.
    pub fn verify(raw: &[u8], stream_id: [u8; 16], source: [u8; 16]) -> Result<Self, Error> {
        let mut r = Reader(raw);
        let fixed = r.take(11)?;
        if fixed != fixed_header(fixed[9])? {
            return Err(Error::Header);
        }
        let stream_key = r.block(128)?;
        let identity_key = r.block(128)?;
        let certificate = r.block(80)?;
        r.end()?;
        if stream_key.is_empty() || identity_key.is_empty() || certificate.is_empty() {
            return Err(Error::Header);
        }
        let hash = crypto::sha256(identity_key)?;
        if !bool::from(hash[..16].ct_eq(&source)) {
            return Err(Error::Source);
        }
        let key = crypto::Verifier::new(stream_key)?;
        crypto::verify_p256(
            identity_key,
            &cert_message(&stream_id, fixed, stream_key),
            certificate,
        )?;
        Ok(Self {
            preset: fixed[9],
            stream_id,
            key,
        })
    }
    pub fn preset(&self) -> u8 {
        self.preset
    }
}

pub fn single_20ms_mono(unit: &[u8]) -> bool {
    if unit.is_empty() || unit.len() > 1275 {
        return false;
    }
    let toc = unit[0];
    toc & 7 == 0 && matches!(toc >> 3, 1 | 5 | 9 | 13 | 15 | 19 | 23 | 27 | 31)
}

struct Budget {
    capacity: f64,
    per_second: f64,
    tokens: f64,
    last_ms: u64,
}
impl Budget {
    fn new(capacity: f64, per_second: f64, now_ms: u64) -> Self {
        Self {
            capacity,
            per_second,
            tokens: capacity,
            last_ms: now_ms,
        }
    }
    fn take(&mut self, amount: usize, now_ms: u64) -> bool {
        self.tokens = self.capacity.min(
            self.tokens + now_ms.saturating_sub(self.last_ms) as f64 * self.per_second / 1000.0,
        );
        self.last_ms = self.last_ms.max(now_ms);
        if self.tokens < amount as f64 {
            return false;
        }
        self.tokens -= amount as f64;
        true
    }
}

/// Exclusive ordered-reader state. Every invalid input permanently destroys its
/// native verifier. Owner supplies local monotonic arrival time, never peer time.
pub struct Guard {
    verified: Option<VerifiedHeader>,
    config: Option<Vec<u8>>,
    configs: u8,
    last_audio_seq: Option<u32>,
    previous: Option<(i64, u32)>,
    peak_offset_ms: Option<i128>,
    records: Budget,
    bytes: Budget,
    controls: Budget,
}
impl Guard {
    pub fn new(header: VerifiedHeader, now_ms: u64) -> Self {
        Self {
            verified: Some(header),
            config: None,
            configs: 0,
            last_audio_seq: None,
            previous: None,
            peak_offset_ms: None,
            records: Budget::new(600.0, 60.0, now_ms),
            bytes: Budget::new(512.0 * 1024.0, 16_000.0, now_ms),
            controls: Budget::new(40.0, 4.0, now_ms),
        }
    }
    fn finish<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.verified = None;
            self.config = None;
        }
        result
    }
    /// Authenticated lifecycle content (BME1 type 4, flags zero), not media.
    /// Resets only the clock baseline. Replay/configuration/ingress state survives.
    pub fn lifecycle(&mut self, payload: &[u8], now_ms: u64) -> Result<(), Error> {
        let result = if self.verified.is_none() {
            Err(Error::Faulted)
        } else if !matches!(payload, [1] | [2] | [3]) {
            Err(Error::Flags)
        } else if !self.controls.take(1, now_ms) {
            Err(Error::Budget)
        } else {
            self.previous = None;
            self.peak_offset_ms = None;
            Ok(())
        };
        self.finish(result)
    }
    pub fn open<'a>(&mut self, raw: &'a [u8], now_ms: u64) -> Result<Packet<'a>, Error> {
        if self.verified.is_none() {
            return Err(Error::Faulted);
        }
        let result = if raw.len() > MAX_PAYLOAD + media::PACKET_HEADER_BYTES {
            Err(Error::Size)
        } else {
            Packet::decode(raw, MAX_PAYLOAD)
                .map_err(Error::from)
                .and_then(|p| self.open_inner(p, now_ms))
        };
        self.finish(result)
    }
    fn open_inner<'a>(&mut self, packet: Packet<'a>, now_ms: u64) -> Result<Packet<'a>, Error> {
        if !matches!(packet.flags, media::AUDIO | 9 | 72) {
            return Err(Error::Flags);
        }
        if !self.records.take(1, now_ms) || !self.bytes.take(packet.payload.len(), now_ms) {
            return Err(Error::Budget);
        }
        if packet.payload.len() < 69 {
            return Err(Error::Size);
        }
        let seq = u32::from_be_bytes(packet.payload[..4].try_into().unwrap());
        let split = packet.payload.len() - 64;
        let signature: &[u8; 64] = packet.payload[split..].try_into().unwrap();
        let body = Packet {
            payload: &packet.payload[4..split],
            ..packet
        };
        let header = self.verified.as_ref().ok_or(Error::Faulted)?;
        header
            .key
            .verify(&record_message(&header.stream_id, body, seq), signature)?;
        if packet.flags & media::CONFIG == 0 {
            if self.last_audio_seq.is_some_and(|last| seq <= last) {
                return Err(Error::Sequence);
            }
            self.last_audio_seq = Some(seq);
        }
        if packet.flags == media::AUDIO | media::CONFIG {
            self.configs += 1;
            let head = media::opus_head(body.payload)?;
            if self.configs > 4
                || head[8] != 1
                || head[9] != 1
                || head[18] != 0
                || self
                    .config
                    .as_ref()
                    .is_some_and(|known| known.as_slice() != body.payload)
            {
                return Err(Error::Config);
            }
            if self.config.is_none() {
                self.config = Some(body.payload.to_vec());
            }
            return Ok(body);
        }
        let batch;
        let units = if packet.flags & media::AUDIO_BATCH != 0 {
            batch = body.split_audio()?;
            batch.as_slice()
        } else {
            std::slice::from_ref(&body)
        };
        let first = units[0].pts_us;
        for (index, unit) in units.iter().enumerate() {
            if !single_20ms_mono(unit.payload) {
                return Err(Error::Opus);
            }
            if first.checked_add(index as i64 * FRAME_US) != Some(unit.pts_us) {
                return Err(Error::Clock);
            }
        }
        if let Some((end, previous_seq)) = self.previous {
            let step = first.checked_sub(end).ok_or(Error::Clock)?;
            let dropped = seq.saturating_sub(previous_seq).saturating_sub(1) as i64;
            let allowed = (100_000 + dropped * 400_000).min(60_000_000);
            if !(0..=allowed).contains(&step) {
                return Err(Error::Clock);
            }
        }
        let end = first
            .checked_add(units.len() as i64 * FRAME_US)
            .ok_or(Error::Clock)?;
        self.previous = Some((end, seq));
        let offset = i128::from(now_ms) - i128::from(first / 1000);
        let peak = self.peak_offset_ms.unwrap_or(offset).max(offset);
        self.peak_offset_ms = Some(peak);
        if peak - offset > 32_000 {
            return Err(Error::Clock);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bmh2::SigningIdentity, crypto::tests::FixtureIdentity};
    use serde_json::Value;
    fn hex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/audio-live-vectors.json")).unwrap()
    }
    fn bytes(v: &Value, key: &str) -> Vec<u8> {
        hex(v[key].as_str().unwrap())
    }
    fn header() -> VerifiedHeader {
        let v = fixture();
        let source: [u8; 16] = crypto::sha256(&bytes(&v, "identity_spki")).unwrap()[..16]
            .try_into()
            .unwrap();
        VerifiedHeader::verify(
            &bytes(&v, "header"),
            bytes(&v, "stream_id").try_into().unwrap(),
            source,
        )
        .unwrap()
    }
    fn guard() -> Guard {
        Guard::new(header(), 0)
    }
    fn signed(seq: u32, flags: u8, pts_us: i64, payload: &[u8]) -> Vec<u8> {
        let v = fixture();
        let stream_id = bytes(&v, "stream_id").try_into().unwrap();
        let mut scalar = [0; 32];
        scalar[31] = 2;
        let key = FixtureIdentity::new(&bytes(&v, "stream_spki"), &scalar);
        let body = Packet {
            flags,
            pts_us,
            payload,
        };
        let signature =
            crypto::der_to_raw(&key.sign(&record_message(&stream_id, body, seq)).unwrap()).unwrap();
        let wire = [&seq.to_be_bytes()[..], payload, &signature].concat();
        Packet {
            payload: &wire,
            ..body
        }
        .encode()
        .unwrap()
    }
    fn config(seq: u32, preskip: u8) -> Vec<u8> {
        let mut head = b"OpusHead\x01\x01\0\0\x80\xbb\0\0\0\0\0".to_vec();
        head[10] = preskip;
        signed(seq, 9, 0, &head)
    }
    fn batch(count: u8, spacing: i32) -> Vec<u8> {
        let mut b = b"BTA2".to_vec();
        b.push(count);
        for i in 0..count {
            b.extend_from_slice(&(i32::from(i) * spacing).to_be_bytes());
            b.extend_from_slice(&3u16.to_be_bytes());
            b.extend_from_slice(&[0xf8, 0xff, 0xfe]);
        }
        b
    }
    #[test]
    fn radio_batch_cadences_verify_and_decode_after_mute_and_resume() {
        for ten in [false, true] {
            let limit = if ten { 10 } else { 2 };
            let identity = crate::identity::Identity::ephemeral().unwrap();
            let node = crypto::sha256(&identity.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
            let mut signer = Signer::new(&identity, [7; 16], 1).unwrap();
            let mut guard = Guard::new(VerifiedHeader::verify(signer.header(), [7; 16], node).unwrap(), 0);
            let mut encoder = crate::opus::Encoder::radio(1).unwrap();
            let config = encoder.config();
            let signed_config = signer.packet(Packet { flags: 9, pts_us: 0, payload: &config }).unwrap();
            guard.open(&signed_config, 0).unwrap();
            let mut batcher = Batcher::new(ten);
            // Mute must also scrub an already pending, non-silent unit.
            let tone = std::array::from_fn(|i| if i % 48 < 24 { 6000 } else { -6000 });
            assert!(batcher.push(0, encoder.encode(&tone).unwrap()).unwrap().is_none());
            batcher.mute(true);
            let mut wire = None;
            for n in 1..limit { wire = batcher.push(n * FRAME_US, encoder.encode(&tone).unwrap()).unwrap(); }
            let raw = wire.take().unwrap();
            let signed = signer.packet(Packet::decode(&raw, MAX_PAYLOAD).unwrap()).unwrap();
            let opened = guard.open(&signed, 200).unwrap();
            let units = opened.split_audio().unwrap();
            assert_eq!(units.len(), limit as usize);
            let mut decoder = crate::opus::Decoder::new(&config, crate::opus::Profile::AudioOnly20ms).unwrap();
            let mut pcm = [0; 960];
            for (i, unit) in units.iter().enumerate() {
                assert_eq!(unit.pts_us, i as i64 * FRAME_US);
                assert!(decoder.decode(unit.payload, &mut pcm).unwrap().samples.iter().all(|s| *s == 0));
            }
            // Pause discards an incomplete batch; resume anchors a new timeline.
            batcher.push(limit * FRAME_US, encoder.encode(&tone).unwrap()).unwrap();
            batcher.clear(); batcher.mute(false); guard.lifecycle(&[2], 250).unwrap(); guard.lifecycle(&[1], 500).unwrap();
            for n in 0..limit { wire = batcher.push(1_000_000 + n * FRAME_US, encoder.encode(&tone).unwrap()).unwrap(); }
            let raw = wire.unwrap();
            let signed = signer.packet(Packet::decode(&raw, MAX_PAYLOAD).unwrap()).unwrap();
            let opened = guard.open(&signed, 1000).unwrap();
            let mut heard = false;
            for unit in opened.split_audio().unwrap() { heard |= decoder.decode(unit.payload, &mut pcm).unwrap().samples.iter().any(|s| s.abs() > 100); }
            assert!(heard, "Unmute must restore non-silent audio");
        }
    }
    #[test]
    fn frozen_independent_certificate_and_record_inputs_match() {
        let v = fixture();
        let h = header();
        assert_eq!(h.preset(), 2);
        assert_eq!(
            cert_message(
                &h.stream_id,
                &fixed_header(2).unwrap(),
                &bytes(&v, "stream_spki")
            ),
            bytes(&v, "certificate_input")
        );
        let mut g = Guard::new(h, 0);
        for record in v["records"].as_array().unwrap() {
            let raw = bytes(record, "packet");
            let p = Packet::decode(&raw, MAX_PAYLOAD).unwrap();
            let body = bytes(record, "body");
            assert_eq!(
                record_message(
                    &g.verified.as_ref().unwrap().stream_id,
                    Packet {
                        payload: &body,
                        ..p
                    },
                    record["sequence"].as_u64().unwrap() as u32
                ),
                bytes(record, "signing_input")
            );
            assert_eq!(g.open(&raw, 0).unwrap().payload, body);
        }
        for (preset, last) in [(1, 1), (2, 2), (3, 3)] {
            let h = fixed_header(preset).unwrap();
            assert_eq!(h, [0x42, 0x54, 0x4c, 0x31, 1, 0xbb, 0x80, 1, 20, last, 0]);
        }
    }
    #[test]
    fn header_binds_full_source_stream_keys_and_all_fields() {
        let v = fixture();
        let raw = bytes(&v, "header");
        let stream = bytes(&v, "stream_id").try_into().unwrap();
        let source: [u8; 16] = crypto::sha256(&bytes(&v, "identity_spki")).unwrap()[..16]
            .try_into()
            .unwrap();
        for n in 0..raw.len() {
            assert!(VerifiedHeader::verify(&raw[..n], stream, source).is_err());
        }
        for index in [0, 4, 5, 7, 8, 9, 10, 11, 12, 13, 104, 196, raw.len() - 1] {
            let mut bad = raw.clone();
            bad[index] ^= 1;
            assert!(
                VerifiedHeader::verify(&bad, stream, source).is_err(),
                "field {index}"
            );
        }
        let mut bad = raw.clone();
        bad.push(0);
        assert!(VerifiedHeader::verify(&bad, stream, source).is_err());
        let mut other = source;
        other[15] ^= 1;
        assert!(matches!(
            VerifiedHeader::verify(&raw, stream, other),
            Err(Error::Source)
        ));
        let mut other = stream;
        other[15] ^= 1;
        assert!(VerifiedHeader::verify(&raw, other, source).is_err());
    }
    #[test]
    fn signatures_bind_flags_time_sequence_body_and_fault_permanently() {
        let original = signed(1, 8, 20_000, &[0xf8, 1, 2]);
        for index in [4, 5, 12, 13, 17, original.len() - 1] {
            let mut bad = original.clone();
            bad[index] ^= 1;
            let mut g = guard();
            assert!(g.open(&bad, 0).is_err());
            assert!(matches!(g.open(&original, 0), Err(Error::Faulted)));
        }
        let mut g = guard();
        g.open(&original, 0).unwrap();
        assert!(matches!(g.open(&original, 0), Err(Error::Sequence)));
        assert!(matches!(g.lifecycle(&[1], 0), Err(Error::Faulted)));
    }
    #[test]
    fn opus_toc_exhaustive_and_config_replay_limits() {
        for toc in 0u8..=255 {
            assert_eq!(
                single_20ms_mono(&[toc]),
                [8, 40, 72, 104, 120, 152, 184, 216, 248].contains(&toc)
            );
        }
        assert!(!single_20ms_mono(&[]));
        assert!(single_20ms_mono(&vec![0xf8; 1275]));
        assert!(!single_20ms_mono(&vec![0xf8; 1276]));
        let c = config(0, 0);
        let mut g = guard();
        for _ in 0..4 {
            g.open(&c, 0).unwrap();
        }
        assert!(matches!(g.open(&c, 0), Err(Error::Config)));
        let mut g = guard();
        g.open(&c, 0).unwrap();
        assert!(matches!(g.open(&config(1, 1), 0), Err(Error::Config)));
        let mut g = guard();
        g.open(&signed(1, 8, 0, &[0xf8]), 0).unwrap();
        g.open(&c, 0).unwrap(); // Cached config must not reset the audio replay floor.
        assert!(matches!(
            g.open(&signed(1, 8, 20_000, &[0xf8]), 0),
            Err(Error::Sequence)
        ));
    }
    #[test]
    fn clock_accepts_shedding_and_resume_without_resetting_replay() {
        let mut g = guard();
        g.open(&signed(1, 8, 0, &[0xf8]), 0).unwrap();
        g.open(&signed(10, 8, 3_220_000, &[0xf8]), 200).unwrap();
        g.lifecycle(&[2], 400).unwrap();
        g.lifecycle(&[1], 120_000).unwrap();
        g.open(&signed(11, 8, 300_000_000, &[0xf8]), 120_000)
            .unwrap();
        g.lifecycle(&[1], 120_000).unwrap();
        assert!(matches!(
            g.open(&signed(11, 8, 400_000_000, &[0xf8]), 120_000),
            Err(Error::Sequence)
        ));
        for next in [0, 120_001, 5_000_000] {
            let mut g = guard();
            g.open(&signed(1, 8, 0, &[0xf8]), 0).unwrap();
            assert!(matches!(
                g.open(&signed(2, 8, next, &[0xf8]), 0),
                Err(Error::Clock)
            ));
        }
        assert!(matches!(
            guard().open(&signed(1, 8, i64::MAX, &[0xf8]), 0),
            Err(Error::Clock)
        ));
    }
    #[test]
    fn batches_require_contiguous_twenty_ms_and_burst_stays_bounded() {
        let mut g = guard();
        for i in 0..81 {
            g.open(
                &signed(i, 72, i64::from(i) * 400_000, &batch(20, 20_000)),
                0,
            )
            .unwrap();
        }
        assert!(matches!(
            g.open(&signed(81, 72, 32_400_000, &batch(20, 20_000)), 0),
            Err(Error::Clock)
        ));
        assert!(matches!(
            guard().open(&signed(0, 72, 0, &batch(3, 25_000)), 0),
            Err(Error::Clock)
        ));
        for bad in [vec![], vec![0xf9], vec![0xfc], vec![0xf0], vec![0xf8; 1276]] {
            assert!(guard().open(&signed(0, 8, 0, &bad), 0).is_err());
        }
    }
    #[test]
    fn pre_signature_budgets_and_plaintext_limit_fail_closed() {
        let mut g = guard();
        for i in 0..600 {
            g.open(&signed(i, 8, i64::from(i) * 20_000, &[0xf8]), 0)
                .unwrap();
        }
        // Invalid signature still returns budget failure: no signature work beyond the cap.
        assert!(matches!(
            g.open(
                &Packet {
                    flags: 8,
                    pts_us: 12_000_000,
                    payload: &[0; 69]
                }
                .encode()
                .unwrap(),
                0
            ),
            Err(Error::Budget)
        ));
        let mut g = guard();
        for _ in 0..40 {
            g.lifecycle(&[1], 0).unwrap();
        }
        assert_eq!(g.lifecycle(&[1], 0), Err(Error::Budget));
        assert!(matches!(
            guard().open(&vec![0; MAX_PAYLOAD + 14], 0),
            Err(Error::Size)
        ));
        for flags in [0, 1, 2, 4, 10, 12, 24, 40, 136, 73] {
            assert!(matches!(
                guard().open(
                    &Packet {
                        flags,
                        pts_us: 0,
                        payload: &[0; 69]
                    }
                    .encode()
                    .unwrap(),
                    0
                ),
                Err(Error::Flags)
            ));
        }
        let mut full = b"BTA2\x14".to_vec();
        for i in 0i32..20 {
            full.extend_from_slice(&(i * 20_000).to_be_bytes());
            full.extend_from_slice(&1275u16.to_be_bytes());
            full.extend_from_slice(&[0xf8; 1275]);
        }
        let mut g = guard();
        for i in 0..20 {
            g.open(&signed(i, 72, i64::from(i) * 400_000, &full), 0)
                .unwrap();
        }
        // One second replenishes enough bytes for one more maximum-size record.
        let next = signed(20, 72, 8_000_000, &full);
        g.open(&next, 1000).unwrap();
        assert!(matches!(
            g.open(&signed(21, 72, 8_400_000, &full), 1000),
            Err(Error::Budget)
        ));
    }
}
