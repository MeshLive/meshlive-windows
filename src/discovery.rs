//! Existing Android manufacturer/compact discovery formats. Every field remains an untrusted hint.
#[derive(Clone, PartialEq, Eq)]
pub struct Announcement {
    pub version: u8,
    pub prefix: [u8; 6],
    pub flags: u8,
    pub summary: u8,
    pub token: u32,
    pub cost: u8,
    pub sequence: u8,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub bitrate: u16,
    pub recovery: Option<[u8; 6]>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Version,
    Fields,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    AudioOnly,
    Malformed,
}
pub const ACCEPTING: u8 = 1;
pub const RELAY: u8 = 2;
pub const STREAM: u8 = 4;
pub const GATT: u8 = 8;
pub const CLASSIC: u8 = 16;
pub const PAUSED: u8 = 32;
pub const CALL: u8 = 64;
pub const RECOVERY: u8 = 128;
const RESOLUTIONS: [(u16, u16); 3] = [(426, 240), (640, 360), (854, 480)];
const FPS: [u8; 3] = [15, 24, 30];

impl Announcement {
    pub fn stream_kind(&self) -> StreamKind {
        if self.summary & 0x40 == 0 {
            StreamKind::Video
        } else if self.flags & STREAM != 0
            && self.summary == 0x40
            && self.width == 0
            && self.height == 0
            && self.fps == 0
        {
            StreamKind::AudioOnly
        } else {
            StreamKind::Malformed
        }
    }
    pub fn is_gatt(&self) -> bool {
        self.flags & (GATT | CLASSIC) == GATT
    }
    pub fn decode(bytes: &[u8], rotating: bool) -> Result<Self, Error> {
        let raw: &[u8; 22] = bytes.try_into().map_err(|_| Error::Length)?;
        if raw[0] != 2 && !(raw[0] == 5 && rotating) {
            return Err(Error::Version);
        }
        let recovery = (raw[7] & RECOVERY != 0).then(|| raw[14..20].try_into().unwrap());
        Ok(Self {
            version: raw[0],
            prefix: raw[1..7].try_into().unwrap(),
            flags: raw[7],
            summary: raw[8],
            token: u32::from_be_bytes(raw[9..13].try_into().unwrap()),
            cost: raw[13],
            sequence: if recovery.is_none() { raw[14] } else { 0 },
            width: if recovery.is_none() {
                u16::from_be_bytes(raw[15..17].try_into().unwrap())
            } else {
                0
            },
            height: if recovery.is_none() {
                u16::from_be_bytes(raw[17..19].try_into().unwrap())
            } else {
                0
            },
            fps: if recovery.is_none() { raw[19] } else { 0 },
            bitrate: u16::from_be_bytes(raw[20..22].try_into().unwrap()),
            recovery,
        })
    }
    pub fn encode(&self) -> Result<[u8; 22], Error> {
        if !matches!(self.version, 2 | 5) {
            return Err(Error::Version);
        }
        if (self.flags & RECOVERY != 0) != self.recovery.is_some() {
            return Err(Error::Fields);
        }
        let mut raw = [0; 22];
        raw[0] = self.version;
        raw[1..7].copy_from_slice(&self.prefix);
        raw[7] = self.flags;
        raw[8] = self.summary;
        raw[9..13].copy_from_slice(&self.token.to_be_bytes());
        raw[13] = self.cost;
        if let Some(recovery) = self.recovery {
            raw[14..20].copy_from_slice(&recovery);
        } else {
            raw[14] = self.sequence;
            raw[15..17].copy_from_slice(&self.width.to_be_bytes());
            raw[17..19].copy_from_slice(&self.height.to_be_bytes());
            raw[19] = self.fps;
        }
        raw[20..22].copy_from_slice(&self.bitrate.to_be_bytes());
        Ok(raw)
    }
    pub fn from_compact(raw: [u8; 16], rotating: bool) -> Result<Self, Error> {
        let high = u64::from_be_bytes(raw[..8].try_into().unwrap());
        let low = u64::from_be_bytes(raw[8..].try_into().unwrap());
        let version = (high >> 46) & 3;
        let kind = (high >> 44) & 3;
        if high >> 48 != 0xc8d6 || (version != 1 && !(version == 2 && rotating)) {
            return Err(Error::Version);
        }
        if kind == 3 || (low & (1 << 57) != 0 && kind != 2) {
            return Err(Error::Fields);
        }
        let prefix_bits = ((high & 0xfffffffffff) << 4) | (low >> 60);
        let mut flags = match kind {
            0 => ACCEPTING | CALL,
            1 => ACCEPTING,
            _ => STREAM,
        };
        if low & (1 << 59) != 0 {
            flags |= GATT;
        }
        if low & (1 << 58) != 0 {
            flags |= RELAY;
        }
        if low & (1 << 57) != 0 {
            flags |= PAUSED;
        }
        let mut value = Self {
            version: if version == 2 { 5 } else { 2 },
            prefix: prefix_bits.to_be_bytes()[2..].try_into().unwrap(),
            flags,
            summary: ((low >> 54) & 7) as u8,
            token: ((low >> 19) & 0x7fffffff) as u32,
            cost: ((low >> 50) & 15) as u8,
            sequence: 0,
            width: 0,
            height: 0,
            fps: 0,
            bitrate: 0,
            recovery: None,
        };
        let specific = low & 0x7ffff;
        if kind == 0 {
            if value.token != 0 || specific & !(1 << 18) != 0 {
                return Err(Error::Fields);
            }
            if specific != 0 {
                value.summary |= 0x10;
            }
        } else {
            value.token |= 0x80000000;
            if kind == 1 {
                if specific & 0xfff != 0 {
                    return Err(Error::Fields);
                }
                value.bitrate = ((specific >> 12) * 25) as u16;
            } else {
                let resolution = ((specific >> 9) & 3) as usize;
                let fps = ((specific >> 7) & 3) as usize;
                if let Some(&(width, height)) = RESOLUTIONS.get(resolution) {
                    value.width = width;
                    value.height = height;
                }
                value.fps = FPS.get(fps).copied().unwrap_or(0);
                value.sequence = (specific >> 11) as u8;
                value.bitrate = ((specific & 127) * 25) as u16;
                if value.summary == 0 && resolution == 3 && fps == 3 {
                    value.summary = 0x40;
                }
            }
        }
        Ok(value)
    }
    pub fn compact(&self) -> Result<[u8; 16], Error> {
        if !matches!(self.version, 2 | 5) {
            return Err(Error::Version);
        }
        let kind = if self.flags & STREAM != 0 {
            2
        } else if self.flags & CALL != 0 {
            0
        } else if self.flags & ACCEPTING != 0 {
            1
        } else {
            return Err(Error::Fields);
        };
        let required = match kind {
            0 => ACCEPTING | CALL,
            1 => ACCEPTING,
            _ => STREAM,
        };
        let audio = self.flags & STREAM != 0 && self.summary == 0x40;
        if self.flags & required != required
            || self.flags & !(required | GATT | RELAY | PAUSED) != 0
            || (self.flags & PAUSED != 0 && kind != 2)
            || self.cost > 15
            || self.recovery.is_some()
        {
            return Err(Error::Fields);
        }
        if (!audio && self.summary & !(7 | if kind == 0 { 0x10 } else { 0 }) != 0)
            || (audio && (self.width != 0 || self.height != 0 || self.fps != 0))
            || (kind == 0 && self.token != 0)
            || (kind != 0 && self.token & 0x80000000 == 0)
        {
            return Err(Error::Fields);
        }
        let units = (self.bitrate / 25).min(127) as u64;
        let specific = match kind {
            0 => {
                if self.summary & 0x10 != 0 {
                    1 << 18
                } else {
                    0
                }
            }
            1 => units << 12,
            _ => {
                let resolution = RESOLUTIONS
                    .iter()
                    .position(|&r| r == (self.width, self.height))
                    .unwrap_or(3) as u64;
                let fps = FPS.iter().position(|&v| v == self.fps).unwrap_or(3) as u64;
                (u64::from(self.sequence) << 11) | (resolution << 9) | (fps << 7) | units
            }
        };
        let prefix = u64::from_be_bytes([
            0,
            0,
            self.prefix[0],
            self.prefix[1],
            self.prefix[2],
            self.prefix[3],
            self.prefix[4],
            self.prefix[5],
        ]);
        let high = (0xc8d6u64 << 48)
            | ((if self.version == 5 { 2 } else { 1 }) << 46)
            | (kind << 44)
            | (prefix >> 4);
        let low = ((prefix & 15) << 60)
            | (u64::from(self.flags & GATT != 0) << 59)
            | (u64::from(self.flags & RELAY != 0) << 58)
            | (u64::from(self.flags & PAUSED != 0) << 57)
            | ((if audio {
                0
            } else {
                u64::from(self.summary & 7)
            }) << 54)
            | (u64::from(self.cost) << 50)
            | (u64::from(self.token & 0x7fffffff) << 19)
            | specific;
        let mut raw = [0; 16];
        raw[..8].copy_from_slice(&high.to_be_bytes());
        raw[8..].copy_from_slice(&low.to_be_bytes());
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn uuid(text: &str) -> [u8; 16] {
        let s = text.replace('-', "");
        std::array::from_fn(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
    }
    #[test]
    fn android_compact_vectors_and_amendments_match_byte_for_byte() {
        for (text, prefix, flags, summary, token, cost, sequence, width, height, fps, bitrate) in [
            (
                "C8D64010-2030-4050-60C0-000000000000",
                [1, 2, 3, 4, 5, 6],
                ACCEPTING | CALL,
                3,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ),
            (
                "C8D64A1B-2C3D-4E5F-6848-000000000000",
                [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6],
                ACCEPTING | CALL | GATT,
                1,
                0,
                2,
                0,
                0,
                0,
                0,
                0,
            ),
            (
                "C8D65010-2030-4050-60C0-D5E6F780C000",
                [1, 2, 3, 4, 5, 6],
                ACCEPTING,
                3,
                0x9abcdef0,
                0,
                0,
                0,
                0,
                0,
                300,
            ),
            (
                "C8D66A1B-2C3D-4E5F-6E46-F56DF77C2A0C",
                [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6],
                STREAM | GATT | RELAY | PAUSED,
                1,
                0xdeadbeef,
                1,
                0x85,
                640,
                360,
                15,
                300,
            ),
            (
                "C8D64010-2030-4050-60C0-000000040000",
                [1, 2, 3, 4, 5, 6],
                ACCEPTING | CALL,
                0x13,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ),
            (
                "C8D66010-2030-4050-6806-F56DF7782F81",
                [1, 2, 3, 4, 5, 6],
                STREAM | GATT,
                0x40,
                0xdeadbeef,
                1,
                5,
                0,
                0,
                0,
                25,
            ),
        ] {
            let expected = Announcement {
                version: 2,
                prefix,
                flags,
                summary,
                token,
                cost,
                sequence,
                width,
                height,
                fps,
                bitrate,
                recovery: None,
            };
            let decoded = Announcement::from_compact(uuid(text), false).unwrap();
            assert!(decoded == expected, "Wrong fields for {text}");
            assert_eq!(expected.compact().unwrap(), uuid(text));
        }
    }
    #[test]
    fn frozen_manufacturer_examples_keep_exact_unknown_bits_and_recovery() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap();
        for name in ["offer", "viewerRecovery", "call"] {
            let text = fixture["discovery"][name].as_str().unwrap();
            let raw: Vec<_> = (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
                .collect();
            let decoded = Announcement::decode(&raw, false).unwrap();
            assert_eq!(decoded.encode().unwrap().as_slice(), raw);
            for length in 0..raw.len() {
                assert!(Announcement::decode(&raw[..length], true).is_err());
            }
            assert!(Announcement::decode(&[raw.as_slice(), &[0]].concat(), true).is_err());
        }
    }
    #[test]
    fn rotating_gates_reserved_fields_and_audio_markers_fail_closed() {
        let raw = uuid("C8D6844E-69C0-A7A7-D0C0-000000000000");
        assert!(Announcement::from_compact(raw, false).is_err());
        let rotating = Announcement::from_compact(raw, true).unwrap();
        assert_eq!(rotating.version, 5);
        assert_eq!(rotating.compact().unwrap(), raw);
        let mut manufacturer = rotating.encode().unwrap();
        assert!(Announcement::decode(&manufacturer, false).is_err());
        manufacturer[0] = 8;
        assert!(Announcement::decode(&manufacturer, true).is_err());
        let valid = uuid("C8D64010-2030-4050-60C0-000000000000");
        for bit in [0, 17, 19, 49, 57] {
            let bad = (u128::from_be_bytes(valid) ^ (1 << bit)).to_be_bytes();
            assert!(Announcement::from_compact(bad, false).is_err());
        }
        let mut audio =
            Announcement::from_compact(uuid("C8D66010-2030-4050-6806-F56DF7782F81"), false)
                .unwrap();
        assert_eq!(audio.stream_kind(), StreamKind::AudioOnly);
        for bad in [0x41, 0x42, 0x60, 0xc0] {
            audio.summary = bad;
            assert_eq!(audio.stream_kind(), StreamKind::Malformed);
            assert!(audio.compact().is_err());
        }
        audio.summary = 0x40;
        audio.width = 640;
        assert_eq!(audio.stream_kind(), StreamKind::Malformed);
        assert!(audio.compact().is_err());
    }
}

/// GAPS P3: the shared conformance vectors not pinned elsewhere
/// (compact-discovery 008-016 and 027; announcements 024-026).
#[cfg(test)]
mod shared_vectors {
    use super::*;
    use serde_json::Value;
    fn uuid(text: &str) -> [u8; 16] {
        let s = text.replace('-', "");
        std::array::from_fn(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
    }
    fn vector(file: &str, id: &str) -> Value {
        let doc: Value = serde_json::from_str(file).unwrap();
        doc["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap_or_else(|| panic!("{id}")).clone()
    }
    const COMPACT: &str = include_str!("../tests/vectors/conformance/compact-discovery.json");
    const ANNOUNCEMENTS: &str = include_str!("../tests/vectors/conformance/announcements.json");
    fn uuids(v: &Value) -> Vec<[u8; 16]> {
        match v["inputs"]["cases"].as_array() {
            Some(cases) => cases.iter().map(|c| uuid(c["uuid"].as_str().unwrap())).collect(),
            None => vec![uuid(v["inputs"]["uuid"].as_str().unwrap())],
        }
    }
    #[test]
    fn compact_discovery_decoder_vectors() {
        let v = vector(COMPACT, "compact-discovery-008");
        let a = Announcement::from_compact(uuids(&v)[0], true).unwrap();
        assert_eq!(a.version, 2);
        assert_eq!(a.prefix, [1, 2, 3, 4, 5, 6]);
        for id in 9..=14 {
            let v = vector(COMPACT, &format!("compact-discovery-{id:03}"));
            let cases = uuids(&v);
            assert!(!cases.is_empty());
            for raw in cases {
                assert!(Announcement::from_compact(raw, false).is_err(), "compact-discovery-{id:03} must be rejected");
            }
        }
        for raw in uuids(&vector(COMPACT, "compact-discovery-015")) {
            assert!(Announcement::from_compact(raw, false).is_ok(), "every stream-offer bit is defined");
        }
        let v = vector(COMPACT, "compact-discovery-016");
        assert_eq!(Announcement::from_compact(uuids(&v)[0], false).unwrap().summary, 5);
        let a = Announcement::from_compact(uuids(&vector(COMPACT, "compact-discovery-027"))[0], false).unwrap();
        assert!(a.flags & STREAM != 0);
        assert_eq!((a.sequence & 0x80, a.sequence & 0x7f), (0x80, 5), "full stream offer, capacity generation 5");
    }
    /// compact-discovery 017-024: what a sender must never emit.
    #[test]
    fn the_compact_encoder_refuses_what_it_cannot_represent() {
        for id in 17..=24 {
            let v = vector(COMPACT, &format!("compact-discovery-{id:03}"));
            let a = &v["inputs"]["announcement"];
            let num = |k: &str| a[k].as_u64().unwrap_or(0);
            let six = |k: &str| -> [u8; 6] {
                let s = a[k].as_str().unwrap();
                std::array::from_fn(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            };
            let token = match (a["stream_token_hex"].as_str(), a["stream_token_u32"].as_u64()) {
                (Some(hex), _) => u32::from_str_radix(hex, 16).unwrap(),
                (None, Some(n)) => n as u32,
                _ => 0,
            };
            let announcement = Announcement {
                version: num("version") as u8,
                prefix: six("node_id_prefix"),
                flags: num("flags") as u8,
                summary: num("capability_summary") as u8,
                token,
                cost: num("route_cost") as u8,
                sequence: num("sequence") as u8,
                width: num("stream_width") as u16,
                height: num("stream_height") as u16,
                fps: num("stream_fps") as u8,
                bitrate: num("stream_bitrate_kbps") as u16,
                recovery: a["recovery_from_node_id_prefix"].as_str().map(|_| six("recovery_from_node_id_prefix")),
            };
            assert!(announcement.compact().is_err(), "compact-discovery-{id:03} must be refused");
        }
    }
    #[test]
    fn announcement_versions_3_4_and_6_are_rejected_with_rotating_on() {
        for id in ["announcements-024", "announcements-025", "announcements-026"] {
            let hex = vector(ANNOUNCEMENTS, id)["inputs"]["bytes"].as_str().unwrap().to_owned();
            let raw: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
            assert!(Announcement::decode(&raw, true).is_err(), "{id}");
        }
    }
}
