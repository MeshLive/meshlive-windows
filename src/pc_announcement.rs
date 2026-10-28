//! Design QW (A2) v1: the PC's call-offer announcement, carried as 128-bit
//! service data under the fixed UUID `c8d5a10a-9f88-4f65-a03d-7f5a2fb921fa`
//! in an extended (LE 1M secondary PHY), connectable advert (gates 1 and 2,
//! 2026-10-20). Exactly 9 bytes, every field an untrusted hint:
//!
//! ```text
//! byte 0     version (bits 7..4) = 1, kind (bits 3..0) = 0 call offer
//! bytes 1..6 RDI: the rotating call ID for the current epoch (role 0)
//! byte 7     flags: bit 0 GATT (required in v1), bit 1 relay-capable,
//!            bit 2 relay consent ("may be relayed", Android call bit 18 /
//!            capabilitySummary 0x10); bits 3..7 zero
//! byte 8     bits 0..2 codecs (capabilitySummary codec bits), bits 3..6
//!            route cost 0..15, bit 7 zero
//! ```
//!
//! Decoders are strict (review R7): any other length, version, kind or a
//! reserved bit set is rejected and counted, never guessed. Shared vectors:
//! `tests/vectors/pc_announcement_v1.json` (Rust, Kotlin and Swift use the
//! same file).
use crate::discovery::{ACCEPTING, Announcement, CALL, GATT, PAUSED, RECOVERY, RELAY, STREAM};

/// The fixed announcement service-data UUID (Design QW A2).
pub const UUID: u128 = 0xc8d5a10a_9f88_4f65_a03d_7f5a2fb921fa;
pub const LEN: usize = 9;
pub const VERSION: u8 = 1;
pub const KIND_CALL_OFFER: u8 = 0;
pub const FLAG_GATT: u8 = 1;
pub const FLAG_RELAY: u8 = 2;
pub const FLAG_CONSENT: u8 = 4;
/// capabilitySummary bit for relay consent on call offers.
const SUMMARY_CONSENT: u8 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Version,
    Kind,
    /// A reserved bit is set, GATT is missing, or a field is out of range.
    Fields,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallOffer {
    pub rdi: [u8; 6],
    pub relay: bool,
    pub consent: bool,
    /// capabilitySummary codec bits (3).
    pub codecs: u8,
    pub cost: u8,
}

impl CallOffer {
    pub fn encode(&self) -> Result<[u8; LEN], Error> {
        if self.codecs > 7 || self.cost > 15 {
            return Err(Error::Fields);
        }
        let mut out = [0u8; LEN];
        out[0] = (VERSION << 4) | KIND_CALL_OFFER;
        out[1..7].copy_from_slice(&self.rdi);
        out[7] = FLAG_GATT | if self.relay { FLAG_RELAY } else { 0 } | if self.consent { FLAG_CONSENT } else { 0 };
        out[8] = self.codecs | (self.cost << 3);
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let raw: &[u8; LEN] = bytes.try_into().map_err(|_| Error::Length)?;
        if raw[0] >> 4 != VERSION {
            return Err(Error::Version);
        }
        if raw[0] & 15 != KIND_CALL_OFFER {
            return Err(Error::Kind);
        }
        let flags = raw[7];
        if flags & !(FLAG_GATT | FLAG_RELAY | FLAG_CONSENT) != 0 || flags & FLAG_GATT == 0 || raw[8] & 0x80 != 0 {
            return Err(Error::Fields);
        }
        Ok(Self {
            rdi: raw[1..7].try_into().unwrap(),
            relay: flags & FLAG_RELAY != 0,
            consent: flags & FLAG_CONSENT != 0,
            codecs: raw[8] & 7,
            cost: (raw[8] >> 3) & 15,
        })
    }
    /// The same offer as the app's common discovery record (version 5:
    /// rotating), so it goes through the same acceptance as other offers.
    pub fn to_announcement(&self) -> Announcement {
        Announcement {
            version: 5,
            prefix: self.rdi,
            flags: ACCEPTING | CALL | GATT | if self.relay { RELAY } else { 0 },
            summary: self.codecs | if self.consent { SUMMARY_CONSENT } else { 0 },
            token: 0,
            cost: self.cost,
            sequence: 0,
            width: 0,
            height: 0,
            fps: 0,
            bitrate: 0,
            recovery: None,
        }
    }
}

// ---------------------------------------------------------------- kinds 1, 2

/// Design QV revision 4 (draft; PROTOCOL "Experimental PC announcement"):
/// kinds 1 (viewer request) and 2 (stream offer) are one header byte plus
/// the 22-byte version 5 announcement of that role, verbatim. Used only by
/// the test-hooks build for gates W1/W2 (`MESHLIVE_TEST_PC_ANNOUNCE`).
pub const STREAM_LEN: usize = 23;
pub const KIND_VIEWER_REQUEST: u8 = 1;
pub const KIND_STREAM_OFFER: u8 = 2;
/// capabilitySummary bits a kind 2 offer may carry: codecs (Design HC adds
/// HEVC's 0x80, which old strict readers hide) and audio only.
const OFFER_SUMMARY: u8 = 0x07 | 0x40 | crate::video::codecs::SUMMARY_HEVC;

/// The kind-0 packed codec field from hello codec bits (review 2 F11): VP9,
/// AV1 and H.264 hints only; HEVC is never put there (bit 3 is the route
/// cost) and is negotiated in the authenticated hello. With both Design HC
/// masters off the field is exactly today's (`hello & 7`); with one on, the
/// PC's real encoders and decoders replace the 0x03 constant.
pub fn call_offer_codecs(constant: u8) -> u8 {
    use crate::video::codecs;
    if !codecs::Switches::current().any() {
        return constant & 7;
    }
    codecs::hello_to_summary(crate::video::encode_mask() & crate::video::decode_mask()) & 0x07
}

/// Any PC announcement, decoded strictly in PROTOCOL's order (version,
/// kind, length, fields).
#[derive(Clone, PartialEq, Eq)]
pub enum Pc {
    Call(CallOffer),
    Request(Announcement),
    Offer(Announcement),
}
impl std::fmt::Debug for Pc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pc::Call(offer) => write!(f, "Call({offer:?})"),
            Pc::Request(a) => write!(f, "Request(flags={:02x} token={:08x})", a.flags, a.token),
            Pc::Offer(a) => write!(f, "Offer(flags={:02x} summary={:02x} token={:08x})", a.flags, a.summary, a.token),
        }
    }
}

/// The rules of PROTOCOL's strict decoding, step 4, for kinds 1 and 2.
fn check_inner(kind: u8, a: &Announcement) -> Result<(), Error> {
    if a.version != 5 || a.token & 0x8000_0000 == 0 {
        return Err(Error::Fields);
    }
    match kind {
        KIND_VIEWER_REQUEST => {
            let plain = a.flags == ACCEPTING | GATT && a.recovery.is_none() && a.sequence == 0 && a.width == 0 && a.height == 0 && a.fps == 0;
            let recovering = a.flags == ACCEPTING | GATT | RECOVERY && a.recovery.is_some_and(|hint| hint != [0; 6]);
            if !(plain || recovering) || a.summary != 0 {
                return Err(Error::Fields);
            }
        }
        KIND_STREAM_OFFER => {
            if a.flags & !PAUSED != STREAM | GATT || a.summary & !OFFER_SUMMARY != 0 || a.cost != 0 || a.recovery.is_some() {
                return Err(Error::Fields);
            }
        }
        _ => return Err(Error::Kind),
    }
    Ok(())
}

/// Kind 1 or 2: the header byte, then `inner` (checked first).
pub fn wrap(kind: u8, inner: &Announcement) -> Result<[u8; STREAM_LEN], Error> {
    check_inner(kind, inner)?;
    let raw = inner.encode().map_err(|_| Error::Fields)?;
    let mut out = [0u8; STREAM_LEN];
    out[0] = (VERSION << 4) | kind;
    out[1..].copy_from_slice(&raw);
    Ok(out)
}

/// Strict decoding of any kind: empty is Length; then version, kind,
/// the kind's length, and the fields.
pub fn decode(bytes: &[u8]) -> Result<Pc, Error> {
    let first = *bytes.first().ok_or(Error::Length)?;
    if first >> 4 != VERSION {
        return Err(Error::Version);
    }
    let kind = first & 15;
    let length = match kind {
        KIND_CALL_OFFER => LEN,
        KIND_VIEWER_REQUEST | KIND_STREAM_OFFER => STREAM_LEN,
        _ => return Err(Error::Kind),
    };
    if bytes.len() != length {
        return Err(Error::Length);
    }
    if kind == KIND_CALL_OFFER {
        return CallOffer::decode(bytes).map(Pc::Call);
    }
    let inner = Announcement::decode(&bytes[1..], true).map_err(|_| Error::Fields)?;
    check_inner(kind, &inner)?;
    Ok(if kind == KIND_VIEWER_REQUEST { Pc::Request(inner) } else { Pc::Offer(inner) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn the_shared_vectors_encode_and_decode_byte_for_byte() {
        let file: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/pc_announcement_v1.json")).unwrap();
        assert_eq!(file["uuid"], "c8d5a10a-9f88-4f65-a03d-7f5a2fb921fa");
        let vectors = file["vectors"].as_array().unwrap();
        assert!(vectors.len() >= 10);
        for v in vectors {
            let name = v["name"].as_str().unwrap();
            let bytes = hex(v["hex"].as_str().unwrap());
            match v["error"].as_str() {
                Some(expected) => {
                    let error = CallOffer::decode(&bytes).expect_err(name);
                    assert_eq!(format!("{error:?}"), expected, "{name}");
                }
                None => {
                    let d = &v["decoded"];
                    let rdi: [u8; 6] = hex(d["rdi"].as_str().unwrap()).try_into().unwrap();
                    let offer = CallOffer {
                        rdi,
                        relay: d["relay"].as_bool().unwrap(),
                        consent: d["consent"].as_bool().unwrap(),
                        codecs: d["codecs"].as_u64().unwrap() as u8,
                        cost: d["cost"].as_u64().unwrap() as u8,
                    };
                    assert_eq!(CallOffer::decode(&bytes), Ok(offer), "{name}");
                    assert_eq!(offer.encode().unwrap().as_slice(), bytes.as_slice(), "{name}");
                }
            }
        }
    }

    #[test]
    fn an_offer_maps_to_a_version_5_gatt_call_offer() {
        let offer = CallOffer { rdi: [1, 2, 3, 4, 5, 6], relay: false, consent: true, codecs: 3, cost: 0 };
        let a = offer.to_announcement();
        assert_eq!(a.version, 5);
        assert!(a.is_gatt());
        assert_eq!(a.flags, ACCEPTING | CALL | GATT);
        assert_eq!(a.summary, 0x13);
        // The compact form accepts it: the same call offer shape as a phone's.
        assert!(a.compact().is_ok());
        assert_eq!(CallOffer { cost: 16, ..offer }.encode(), Err(Error::Fields));
        assert_eq!(CallOffer { codecs: 8, ..offer }.encode(), Err(Error::Fields));
    }

    fn request(rdi: [u8; 6]) -> Announcement {
        Announcement {
            version: 5,
            prefix: rdi,
            flags: ACCEPTING | GATT,
            summary: 0,
            token: 0x8123_4567,
            cost: 0,
            sequence: 0,
            width: 0,
            height: 0,
            fps: 0,
            bitrate: 500,
            recovery: None,
        }
    }
    fn offer(rdi: [u8; 6]) -> Announcement {
        Announcement { flags: STREAM | GATT, summary: 0x02, width: 640, height: 360, fps: 15, ..request(rdi) }
    }

    #[test]
    fn kinds_1_and_2_are_a_header_byte_and_the_version_5_announcement_verbatim() {
        let rdi = [1, 2, 3, 4, 5, 6];
        for (kind, inner) in [(KIND_VIEWER_REQUEST, request(rdi)), (KIND_STREAM_OFFER, offer(rdi))] {
            let bytes = wrap(kind, &inner).unwrap();
            assert_eq!(bytes.len(), 23);
            assert_eq!(bytes[0], 0x10 | kind);
            assert_eq!(&bytes[1..], inner.encode().unwrap().as_slice());
            let decoded = decode(&bytes).unwrap();
            assert_eq!(decoded, if kind == 1 { Pc::Request(inner) } else { Pc::Offer(inner) });
        }
        let paused = Announcement { flags: STREAM | GATT | PAUSED, ..offer(rdi) };
        assert!(wrap(KIND_STREAM_OFFER, &paused).is_ok());
        let audio = Announcement { summary: 0x40, width: 0, height: 0, fps: 0, ..offer(rdi) };
        assert!(wrap(KIND_STREAM_OFFER, &audio).is_ok());
        let hinted = Announcement { flags: ACCEPTING | GATT | RECOVERY, recovery: Some([9; 6]), ..request(rdi) };
        assert!(decode(&wrap(KIND_VIEWER_REQUEST, &hinted).unwrap()).is_ok());
        // Kind 0 still decodes through the same entry point.
        let call = CallOffer { rdi, relay: false, consent: false, codecs: 3, cost: 0 };
        assert_eq!(decode(&call.encode().unwrap()), Ok(Pc::Call(call)));
    }

    #[test]
    fn kinds_1_and_2_are_refused_in_protocol_order() {
        let rdi = [1, 2, 3, 4, 5, 6];
        let good = wrap(KIND_STREAM_OFFER, &offer(rdi)).unwrap();
        assert_eq!(decode(&[]), Err(Error::Length));
        assert_eq!(decode(&[[0x20].as_slice(), &good[1..]].concat()), Err(Error::Version));
        assert_eq!(decode(&[[0x13].as_slice(), &good[1..]].concat()), Err(Error::Kind));
        assert_eq!(decode(&good[..22]), Err(Error::Length));
        assert_eq!(decode(&[0x10; 23]), Err(Error::Length), "a 23-byte kind 0 is Length (kind before length)");
        // Kind/role confusion and the field rules.
        let swap = |kind: u8, bytes: &[u8; 23]| [[0x10 | kind].as_slice(), &bytes[1..]].concat();
        assert_eq!(decode(&swap(KIND_VIEWER_REQUEST, &good)), Err(Error::Fields), "offer flags in kind 1");
        let req = wrap(KIND_VIEWER_REQUEST, &request(rdi)).unwrap();
        assert_eq!(decode(&swap(KIND_STREAM_OFFER, &req)), Err(Error::Fields), "request flags in kind 2");
        let bad = |kind: u8, a: Announcement| wrap(kind, &a);
        assert_eq!(bad(1, Announcement { summary: 3, ..request(rdi) }), Err(Error::Fields));
        assert_eq!(bad(1, Announcement { token: 0x0123_4567, ..request(rdi) }), Err(Error::Fields));
        assert_eq!(bad(1, Announcement { flags: ACCEPTING | GATT | RECOVERY, recovery: Some([0; 6]), ..request(rdi) }), Err(Error::Fields));
        assert_eq!(bad(1, Announcement { flags: ACCEPTING | GATT | CALL, ..request(rdi) }), Err(Error::Fields));
        assert_eq!(bad(2, Announcement { cost: 1, ..offer(rdi) }), Err(Error::Fields));
        assert_eq!(bad(2, Announcement { flags: STREAM | GATT | RELAY, ..offer(rdi) }), Err(Error::Fields));
        assert_eq!(bad(2, Announcement { summary: 0x08 | 0x02, ..offer(rdi) }), Err(Error::Fields));
        assert_eq!(bad(2, Announcement { version: 2, ..offer(rdi) }), Err(Error::Fields));
        assert_eq!(bad(3, offer(rdi)), Err(Error::Kind));
    }
}
