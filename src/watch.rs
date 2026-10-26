//! Admitted stream ingress only. Discovery/contact/reveal admission and transport
//! orchestration remain the application's responsibility; no radio is activated.
use crate::{
    audio_live, framing, media, mesh,
    records::{self, Layer, Opener, Policy},
};
use std::ops::Range;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Closed,
    Header,
    Kind,
    Source,
    Stream,
    Sequence,
    Ttl,
    Flags,
    Marker,
    AudioDisabled,
    Frame(framing::Error),
    Record(records::Error),
    Media(media::Error),
    Audio(audio_live::Error),
    Mesh(mesh::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Watch ingress error: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    Video(media::VideoHeader),
    Audio { preset: u8 },
}
pub enum Event {
    Header(Header),
    Media(OwnedPacket),
    State(u8),
    Withdrawn,
}

/// Own the single decrypted record allocation. Stripping an integrity hash or
/// signed-audio envelope changes the view, without copying the encoded payload.
pub struct OwnedPacket {
    storage: Zeroizing<Vec<u8>>,
    range: Range<usize>,
    flags: u8,
    pts_us: i64,
}
impl OwnedPacket {
    pub fn packet(&self) -> media::Packet<'_> {
        media::Packet {
            flags: self.flags,
            pts_us: self.pts_us,
            payload: &self.storage[self.range.clone()],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Direct,
    Mesh,
}
enum Format {
    Video(media::VideoHeader),
    Audio(audio_live::Guard),
}
struct Session {
    hop: framing::HopReader,
    content: Opener,
    source: [u8; 16],
    stream: Option<[u8; 16]>,
    mode: Option<Mode>,
    last_mesh: Option<u64>,
    marked_audio: bool,
    allow_audio: bool,
    format: Option<Format>,
}
pub struct Reader {
    session: Option<Session>,
}
impl Reader {
    /// Move in the established readers, including all already-consumed handshake
    /// sequences. `source` must be the authenticated endpoint after owner policy.
    /// Negotiated label/discovery records must be handled before this reader.
    pub fn direct(
        hop: framing::HopReader,
        content: Opener,
        source: [u8; 16],
        marked_audio: bool,
        allow_audio: bool,
    ) -> Self {
        Self {
            session: Some(Session {
                hop,
                content,
                source,
                stream: None,
                mode: None,
                last_mesh: None,
                marked_audio,
                allow_audio,
                format: None,
            }),
        }
    }
    /// The parent endpoint must already authenticate/decrypt BFG1 and pass its
    /// original source through the same verified-contact/mismatch owner policy.
    pub fn forwarded(
        hop: framing::HopReader,
        grant: mesh::Grant,
        marked_audio: bool,
        allow_audio: bool,
    ) -> Result<Self, Error> {
        let content =
            Opener::new(Layer::Endpoint, grant.key(), Policy::default()).map_err(Error::Record)?;
        let mut reader = Self::direct(hop, content, *grant.source(), marked_audio, allow_audio);
        let session = reader.session.as_mut().unwrap();
        session.stream = Some(*grant.stream());
        session.mode = Some(Mode::Mesh);
        Ok(reader)
    }
    /// One authenticated record per hand-over lets the owner validate the header
    /// before increasing the cap, including when records arrive in one callback.
    pub fn read(&mut self, input: &mut &[u8], now_ms: u64) -> Result<Option<Event>, Error> {
        let result = self
            .session
            .as_mut()
            .ok_or(Error::Closed)?
            .read(input, now_ms);
        if result.is_err() || matches!(result, Ok(Some(Event::State(3) | Event::Withdrawn))) {
            self.session = None;
        }
        result
    }
    pub fn ciphertext_cap(&self) -> Result<usize, Error> {
        self.session.as_ref().map(Session::cap).ok_or(Error::Closed)
    }
    pub fn stream_id(&self) -> Option<[u8; 16]> { self.session.as_ref().and_then(|s| s.stream) }
}
impl Session {
    fn cap(&self) -> usize {
        match self.format {
            None => audio_live::HEADER_CIPHERTEXT_CAP,
            Some(Format::Audio(_)) => audio_live::MEDIA_CIPHERTEXT_CAP,
            Some(Format::Video(_)) => Layer::Hop.maximum_ciphertext(),
        }
    }
    fn read(&mut self, input: &mut &[u8], now_ms: u64) -> Result<Option<Event>, Error> {
        let cap = self.cap();
        let Some(record) = self.hop.read(input, cap).map_err(Error::Frame)? else {
            return Ok(None);
        };
        if record.header.kind == 9 {
            if record.plaintext != [1] {
                return Err(Error::Kind);
            }
            return Ok(Some(Event::Withdrawn));
        }
        let envelope;
        let sealed = match record.header.kind {
            5 => {
                if self.mode == Some(Mode::Mesh) {
                    return Err(Error::Kind);
                }
                self.mode = Some(Mode::Direct);
                envelope = None;
                record.plaintext.as_slice()
            }
            7 => {
                if self.mode == Some(Mode::Direct) {
                    return Err(Error::Kind);
                }
                self.mode = Some(Mode::Mesh);
                let e = mesh::Envelope::decode(&record.plaintext).map_err(Error::Mesh)?;
                if e.source != self.source {
                    return Err(Error::Source);
                }
                if e.ttl == 0 {
                    return Err(Error::Ttl);
                }
                if self.stream.is_some_and(|known| known != e.stream) {
                    return Err(Error::Stream);
                }
                if self.last_mesh.is_some_and(|last| e.sequence <= last) {
                    return Err(Error::Sequence);
                }
                self.stream = Some(e.stream);
                self.last_mesh = Some(e.sequence);
                envelope = Some(e);
                e.payload
            }
            _ => return Err(Error::Kind),
        };
        // The hop cap already prevented allocation. Also bound endpoint
        // ciphertext before its plaintext allocation, regardless of its type.
        if sealed.len() < 18 {
            return Err(Error::Record(records::Error::Header));
        }
        self.content
            .inspect_header(&sealed[..18], cap)
            .map_err(Error::Record)?;
        let content = self.content.open(sealed).map_err(Error::Record)?;
        if let Some(e) = envelope {
            let expected = match content.header.kind {
                1 => 0,
                2 => content.header.flags & !(16 | 32),
                4 => 16 | if content.plaintext == [3] { 32 } else { 0 },
                _ => return Err(Error::Kind),
            };
            if e.flags != expected {
                return Err(Error::Flags);
            }
        }
        match content.header.kind {
            1 => {
                if self.format.is_some() {
                    return Err(Error::Header);
                }
                let audio = content.plaintext.starts_with(b"BTL1");
                let header = if audio {
                    if !self.allow_audio {
                        return Err(Error::AudioDisabled);
                    }
                    let h = audio_live::VerifiedHeader::verify(
                        &content.plaintext,
                        self.stream.unwrap_or([0; 16]),
                        self.source,
                    )
                    .map_err(Error::Audio)?;
                    let preset = h.preset();
                    self.format = Some(Format::Audio(audio_live::Guard::new(h, now_ms)));
                    Header::Audio { preset }
                } else {
                    let h = media::VideoHeader::decode(&content.plaintext).map_err(Error::Media)?;
                    self.format = Some(Format::Video(h));
                    Header::Video(h)
                };
                if audio != self.marked_audio {
                    return Err(Error::Marker);
                }
                Ok(Some(Event::Header(header)))
            }
            2 => {
                let Some(format) = self.format.as_mut() else {
                    return Err(Error::Header);
                };
                let raw = content.plaintext;
                let packet = match format {
                    Format::Audio(guard) => guard.open(&raw, now_ms).map_err(Error::Audio)?,
                    Format::Video(header) => {
                        let packet = media::Packet::decode(&raw, media::MAX_PAYLOAD).map_err(Error::Media)?;
                        if packet.flags & media::AUDIO != 0 && !header.opus { return Err(Error::Flags); }
                        packet
                    }
                };
                if packet.flags != content.header.flags {
                    return Err(Error::Flags);
                }
                if packet.flags & media::AUDIO_BATCH != 0 {
                    packet.split_audio().map_err(Error::Media)?;
                }
                if packet.flags & media::ROTATION != 0 {
                    packet.rotation().map_err(Error::Media)?;
                }
                let packet = packet.check_integrity().map_err(Error::Media)?;
                let start = packet.payload.as_ptr() as usize - raw.as_ptr() as usize;
                let range = start..start + packet.payload.len();
                let flags = packet.flags;
                let pts_us = packet.pts_us;
                Ok(Some(Event::Media(OwnedPacket {
                    storage: Zeroizing::new(raw),
                    range,
                    flags,
                    pts_us,
                })))
            }
            4 => {
                let Some(format) = self.format.as_mut() else {
                    return Err(Error::Header);
                };
                let state = match content.plaintext.as_slice() {
                    [value @ 1..=6] => *value,
                    _ => return Err(Error::Kind),
                };
                if let Format::Audio(guard) = format {
                    guard
                        .lifecycle(&content.plaintext, now_ms)
                        .map_err(Error::Audio)?;
                }
                Ok(Some(Event::State(state)))
            }
            _ => Err(Error::Kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::Sealer;
    use serde_json::Value;
    fn hex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    fn direct(marked: bool) -> (Reader, Sealer, Sealer) {
        (
            Reader::direct(
                framing::HopReader::new(&[1; 32], Policy::default()).unwrap(),
                Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(),
                [3; 16],
                marked,
                true,
            ),
            Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(),
            Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(),
        )
    }
    fn video() -> [u8; 15] {
        media::VideoHeader {
            width: 426,
            height: 240,
            fps: 15,
            codec: 2,
            quarter_turns: 0,
            opus: true,
        }
        .encode()
        .unwrap()
    }
    fn wire(hop: &mut Sealer, endpoint: &mut Sealer, kind: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        hop.seal(5, 0, &endpoint.seal(kind, flags, body).unwrap())
            .unwrap()
    }
    #[test]
    fn authenticated_media_states_resume_after_pause_and_allow_intentional_silence() {
        use crate::stream_watch::PlaybackState;
        use std::time::Duration;
        for declares_audio in [true, false] {
            let (mut reader, mut hop, mut endpoint) = direct(false);
            let mut configured = media::VideoHeader::decode(&video()).unwrap();
            configured.opus = declares_audio;
            let header = wire(&mut hop, &mut endpoint, 1, 0, &configured.encode().unwrap());
            let Some(Event::Header(Header::Video(header))) = reader.read(&mut header.as_slice(), 0).unwrap() else { panic!("header missing") };
            let mut playback = PlaybackState::default();
            for value in [2, 4, 5, 6, 1] {
                let bytes = wire(&mut hop, &mut endpoint, 4, 0, &[value]);
                let Some(Event::State(state)) = reader.read(&mut bytes.as_slice(), 0).unwrap() else { panic!("state missing") };
                playback = PlaybackState::from_record(state).unwrap();
                assert_eq!(playback.paused, value == 2, "4 must resume a paused viewer without waiting for 1");
                assert_eq!(playback.source_muted, matches!(value, 4 | 6));
                assert_eq!(playback.camera_off, matches!(value, 5 | 6));
                let silent = value == 6 || value == 5 && !declares_audio;
                assert_eq!(playback.timed_out(false, header.opus, Duration::from_secs(10)), !silent);
                assert!(playback.timed_out(true, true, Duration::from_secs(10)), "radio's liveness rule is unchanged");
            }
            assert_eq!(playback, PlaybackState::default());
            let ended = wire(&mut hop, &mut endpoint, 4, 0, &[3]);
            assert!(matches!(reader.read(&mut ended.as_slice(), 0).unwrap(), Some(Event::State(3))));
            assert_eq!(reader.ciphertext_cap(), Err(Error::Closed));
        }
    }
    #[test]
    fn frozen_mesh_watch_accepts_header_media_and_end_without_copying_frames() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap();
        let v = &v["mesh"];
        let grant = mesh::Grant::decode(&hex(v["grant"].as_str().unwrap())).unwrap();
        let key = hex(v["linkKey"].as_str().unwrap()).try_into().unwrap();
        let mut r = Reader::forwarded(
            framing::HopReader::new(&key, Policy::default()).unwrap(),
            grant,
            false,
            true,
        )
        .unwrap();
        let bytes = hex(v["stream"].as_str().unwrap());
        let mut input = bytes.as_slice();
        let mut kinds = Vec::new();
        while !input.is_empty() {
            match r.read(&mut input, 0).unwrap().unwrap() {
                Event::Header(Header::Video(_)) => kinds.push(1),
                Event::Media(p) => {
                    let packet = p.packet();
                    match packet.flags {
                        8 => {
                            assert_eq!(packet.pts_us, 1000);
                            assert_eq!(packet.payload, [0xfc, 0]);
                            kinds.push(8);
                        }
                        2 => {
                            assert_eq!(packet.pts_us, 2000);
                            assert_eq!(packet.payload, [0x82, 0x49, 0x83, 0x42, 0]);
                            kinds.push(2);
                        }
                        _ => panic!("unexpected fixture media flags"),
                    }
                }
                Event::State(s) => kinds.push(s + 10),
                _ => panic!("unexpected fixture event"),
            }
        }
        assert_eq!(kinds, [1, 8, 2, 13]);
        assert_eq!(r.ciphertext_cap(), Err(Error::Closed));
    }
    #[test]
    fn marker_second_header_and_repeated_flags_fail_closed() {
        let (mut r, mut h, mut e) = direct(true);
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        assert!(matches!(r.read(&mut raw.as_slice(), 0), Err(Error::Marker)));
        assert!(matches!(r.read(&mut &[][..], 0), Err(Error::Closed)));
        let (mut r, mut h, mut e) = direct(false);
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        r.read(&mut raw.as_slice(), 0).unwrap();
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        assert!(matches!(r.read(&mut raw.as_slice(), 0), Err(Error::Header)));
        let (mut r, mut h, mut e) = direct(false);
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        r.read(&mut raw.as_slice(), 0).unwrap();
        let p = media::Packet {
            flags: media::AUDIO,
            pts_us: 0,
            payload: &[0xf8],
        }
        .encode()
        .unwrap();
        let raw = wire(&mut h, &mut e, 2, 2, &p);
        assert!(matches!(r.read(&mut raw.as_slice(), 0), Err(Error::Flags)));
    }
    #[test]
    fn source_stream_ttl_mesh_flags_and_mode_are_bound() {
        for field in [4, 20, 43, 44, 45] {
            let (mut r, mut h, mut e) = direct(false);
            let sealed = e.seal(1, 0, &video()).unwrap();
            let envelope = mesh::Envelope {
                stream: [4; 16],
                source: [3; 16],
                sequence: 0,
                ttl: 8,
                flags: 0,
                payload: &sealed,
            };
            let raw = h.seal(7, 0, &envelope.encode().unwrap()).unwrap();
            r.read(&mut raw.as_slice(), 0).unwrap();
            let sealed = e
                .seal(
                    2,
                    2,
                    &media::Packet {
                        flags: 2,
                        pts_us: 0,
                        payload: &[0],
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap();
            let mut bad = mesh::Envelope {
                sequence: 1,
                payload: &sealed,
                flags: 2,
                ..envelope
            }
            .encode()
            .unwrap();
            if field == 44 {
                bad[field] = 0;
            } else {
                bad[field] ^= 1;
            }
            let raw = h.seal(7, 0, &bad).unwrap();
            assert!(r.read(&mut raw.as_slice(), 0).is_err());
            assert!(matches!(r.read(&mut &[][..], 0), Err(Error::Closed)));
        }
        let (mut r, mut h, mut e) = direct(false);
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        r.read(&mut raw.as_slice(), 0).unwrap();
        let sealed = e.seal(4, 0, &[1]).unwrap();
        let raw = h
            .seal(
                7,
                0,
                &mesh::Envelope {
                    stream: [4; 16],
                    source: [3; 16],
                    sequence: 1,
                    ttl: 8,
                    flags: 16,
                    payload: &sealed,
                }
                .encode()
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(r.read(&mut raw.as_slice(), 0), Err(Error::Kind)));
    }
    #[test]
    fn signed_audio_cap_changes_only_after_verified_header_in_coalesced_input() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/audio-live-vectors.json"))
                .unwrap();
        let source: [u8; 16] = crate::crypto::sha256(&hex(v["identity_spki"].as_str().unwrap()))
            .unwrap()[..16]
            .try_into()
            .unwrap();
        let stream: [u8; 16] = hex(v["stream_id"].as_str().unwrap()).try_into().unwrap();
        let mut raw_grant = [0; 68];
        raw_grant[..4].copy_from_slice(b"BFG1");
        raw_grant[4..20].copy_from_slice(&source);
        raw_grant[20..36].copy_from_slice(&stream);
        raw_grant[36..].fill(2);
        let mut r = Reader::forwarded(
            framing::HopReader::new(&[1; 32], Policy::default()).unwrap(),
            mesh::Grant::decode(&raw_grant).unwrap(),
            true,
            true,
        )
        .unwrap();
        let mut h = Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap();
        let mut e = Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap();
        let mut combined = Vec::new();
        for (seq, kind, flags, body) in [
            (0, 1, 0, hex(v["header"].as_str().unwrap())),
            (1, 2, 9, hex(v["records"][0]["packet"].as_str().unwrap())),
            (2, 2, 8, hex(v["records"][1]["packet"].as_str().unwrap())),
        ] {
            let sealed = e.seal(kind, flags, &body).unwrap();
            let env = mesh::Envelope {
                stream,
                source,
                sequence: seq,
                ttl: 8,
                flags,
                payload: &sealed,
            };
            combined.extend(h.seal(7, 0, &env.encode().unwrap()).unwrap());
        }
        assert_eq!(r.ciphertext_cap(), Ok(4096));
        let mut input = combined.as_slice();
        assert!(matches!(
            r.read(&mut input, 0).unwrap(),
            Some(Event::Header(Header::Audio { preset: 2 }))
        ));
        assert_eq!(r.ciphertext_cap(), Ok(28 * 1024));
        assert!(matches!(
            r.read(&mut input, 0).unwrap(),
            Some(Event::Media(_))
        ));
        match r.read(&mut input, 0).unwrap().unwrap() {
            Event::Media(p) => assert_eq!(p.packet().payload, [0xf8, 0xff, 0xfe]),
            _ => panic!("expected audio"),
        }
        assert!(input.is_empty());
        let raw = h.seal(5, 0, &[0; 28 * 1024]).unwrap();
        let mut input = raw.as_slice();
        assert!(matches!(
            r.read(&mut input, 0),
            Err(Error::Frame(framing::Error::Record(records::Error::Size)))
        ));
        assert_eq!(input.len(), raw.len() - 17); // Cap rejected the hop header before body consumption.
    }

    #[test]
    fn established_hop_and_endpoint_counters_are_preserved() {
        let mut hop = framing::HopReader::new(&[1; 32], Policy::default()).unwrap();
        let mut h = Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap();
        let handshake = h.seal(4, 0, b"opaque handshake fixture").unwrap();
        hop.read(&mut handshake.as_slice(), 4096).unwrap().unwrap();
        let policy = Policy {
            labels: true,
            ..Policy::default()
        };
        let mut e = Sealer::new(Layer::Endpoint, &[2; 32], policy).unwrap();
        let mut content = Opener::new(Layer::Endpoint, &[2; 32], policy).unwrap();
        let label = e.seal(10, 0, b"BLB1\0").unwrap();
        content.open(&label).unwrap();
        // This is a counter hand-over test; no handshake/label admission is claimed.
        let mut r = Reader::direct(hop, content, [3; 16], false, true);
        let raw = wire(&mut h, &mut e, 1, 0, &video());
        assert!(matches!(
            r.read(&mut raw.as_slice(), 0).unwrap(),
            Some(Event::Header(Header::Video(_)))
        ));
    }

    #[test]
    fn video_without_audio_rejects_unsolicited_opus_and_closes() {
        let (mut r, mut h, mut e) = direct(false);
        let mut header = media::VideoHeader::decode(&video()).unwrap(); header.opus = false;
        let bytes = wire(&mut h, &mut e, 1, 0, &header.encode().unwrap());
        r.read(&mut bytes.as_slice(), 0).unwrap();
        let packet = media::Packet { flags: media::AUDIO, pts_us: 0, payload: &[0xf8, 0xff, 0xfe] }.encode().unwrap();
        let bytes = wire(&mut h, &mut e, 2, media::AUDIO, &packet);
        assert!(matches!(r.read(&mut bytes.as_slice(), 0), Err(Error::Flags)));
        assert!(matches!(r.read(&mut &[][..], 0), Err(Error::Closed)));
    }
}
