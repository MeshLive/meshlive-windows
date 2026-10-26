//! What this PC puts on air, as pure functions over the existing discovery
//! formats: version 2 (fixed prefix) or version 5 (rotating ID) announcements,
//! route tokens, the compact UUID, the recovery hint, the boundary silence and
//! the token grace. No advertising set is started here; every field remains an
//! unauthenticated hint that the handshakes and openings check later.
use crate::{
    crypto,
    discovery::{self, Announcement, RECOVERY, STREAM},
    rotating,
};

const ROUTE_DOMAIN: &[u8] = b"bluetooth-video-route-v1";
const RECOVERY_DOMAIN: &[u8] = b"BluetoothVideoMesh/Discovery/Recovery/2";
/// BMH3 hint (`capabilitySummary` bit 5) of a version 2 call offer.
pub const BMH3_HINT: u8 = 0x20;
/// Uniform 1-8 s silence between the old set stopping and the new one (RV12).
pub const SILENCE_MIN_MS: u64 = 1_000;
pub const SILENCE_SPAN_MS: u64 = 7_001;
/// A replaced route token stays acceptable this long (Phase 1b item 3).
pub const TOKEN_GRACE_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Fields,
    Discovery(discovery::Error),
    Rotating(rotating::Error),
    Crypto(crypto::Error),
}

/// Android `StreamRouteToken.advertiserMask`: FNV-1a over the domain and the six
/// advertised bytes, with bit 31 set.
pub fn route_mask(advertised: &[u8; 6]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in ROUTE_DOMAIN.iter().chain(advertised) {
        hash = (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
    }
    hash | 0x8000_0000
}
/// Logical tokens are 1..2^31-1; the advertised token is `logical XOR mask`.
pub fn route_token(logical: u32, advertised: &[u8; 6]) -> Result<u32, Error> {
    if logical == 0 || logical > i32::MAX as u32 {
        return Err(Error::Fields);
    }
    Ok(logical ^ route_mask(advertised))
}
/// A forwarder derives the logical token from the announced bytes of the offer it joined.
pub fn logical_token(route: u32, advertised: &[u8; 6]) -> u32 {
    route ^ route_mask(advertised)
}
/// Nearest 25 kbps, never 0 for a positive rate (version 5 stream offers).
pub fn rounded_bitrate(kbps: u16) -> u16 {
    if kbps == 0 {
        return 0;
    }
    let rounded = (u32::from(kbps) + 12) / 25 * 25;
    rounded.clamp(25, 0xffff) as u16
}

/// `Khint = HKDF-SHA256(Kmedia, 32 x 0x00, "...Recovery/2")`.
pub fn recovery_key(media_key: &[u8; 32]) -> Result<zeroize::Zeroizing<[u8; 32]>, Error> {
    crypto::hkdf_sha256::<32>(media_key, &[0; 32], RECOVERY_DOMAIN).map_err(Error::Crypto)
}
/// `HMAC(Khint, domain || 0 || failedNode || u32 logical || viewerRdi || u32 routeToken)[0:6]`.
pub fn recovery_hint(
    key: &[u8; 32],
    failed_node: &[u8; 16],
    logical: u32,
    viewer_rdi: &[u8; 6],
    route: u32,
) -> Result<[u8; 6], Error> {
    let input = [
        RECOVERY_DOMAIN,
        &[0],
        failed_node,
        &logical.to_be_bytes(),
        viewer_rdi,
        &route.to_be_bytes(),
    ]
    .concat();
    let mac = crypto::hmac_sha256(key, &input).map_err(Error::Crypto)?;
    Ok(mac[..6].try_into().unwrap())
}

/// The role a version 5 offer is built for, with its role-specific token input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    CallOffer,
    StreamOffer {
        logical: u32,
    },
    /// Copies the route token of the selected offer.
    ViewerRequest {
        route: u32,
    },
}
impl Role {
    pub fn id(self) -> u8 {
        match self {
            Self::CallOffer => rotating::ROLE_CALL_OFFER,
            Self::StreamOffer { .. } => rotating::ROLE_STREAM_OFFER,
            Self::ViewerRequest { .. } => rotating::ROLE_VIEWER_REQUEST,
        }
    }
}

/// Turns a filled-in version 2 offer into the version 5 offer for `rdi`: RDI in
/// bytes 1-6, call token 0, stream token from the RDI, viewer request with the
/// copied token and no codec summary, stream bitrate rounded to 25 kbps.
pub fn rotating_offer(
    base: &Announcement,
    role: Role,
    rdi: [u8; 6],
) -> Result<Announcement, Error> {
    let mut offer = base.clone();
    offer.version = 5;
    offer.prefix = rdi;
    match role {
        Role::CallOffer => {
            offer.token = 0;
            // The hint exists only on version 2 offers (version 5 implies BMH3).
            offer.summary &= !BMH3_HINT;
        }
        Role::StreamOffer { logical } => {
            if offer.flags & STREAM == 0 {
                return Err(Error::Fields);
            }
            offer.token = route_token(logical, &rdi)?;
            offer.bitrate = rounded_bitrate(offer.bitrate);
        }
        Role::ViewerRequest { route } => {
            offer.token = route;
            offer.summary = 0;
        }
    }
    offer.encode().map_err(Error::Discovery)?;
    Ok(offer)
}

/// The version 5 offer of this node for `(day key, epoch)`, plus the opening
/// entry that proves it; the entry is what the pins hold.
pub fn rotating_offer_for(
    base: &Announcement,
    role: Role,
    day_key: &[u8; 32],
    node_id: &[u8; 16],
    epoch: u32,
) -> Result<(Announcement, rotating::OpeningEntry), Error> {
    let entry =
        rotating::OpeningEntry::own(day_key, node_id, epoch, role.id()).map_err(Error::Rotating)?;
    Ok((rotating_offer(base, role, entry.rdi)?, entry))
}

/// The one service UUID a compact (iPhone-style) advertisement carries. The
/// compact layout cannot represent the BMH3 hint, so it is masked (PROTOCOL).
pub fn compact_uuid(offer: &Announcement) -> Result<[u8; 16], Error> {
    let mut value = offer.clone();
    if value.flags & discovery::CALL != 0 {
        value.summary &= !BMH3_HINT;
    }
    value.compact().map_err(Error::Discovery)
}

/// Experimental MESHLIVE_COMPACT_OFFER: the PC also announces its call offer
/// as a compact v2 service UUID (legacy advert) for phones without extended
/// scanning. On by default; `MESHLIVE_COMPACT_OFFER=0` is the off arm.
pub fn compact_offer_enabled() -> bool {
    compact_offer_switch(std::env::var("MESHLIVE_COMPACT_OFFER").ok().as_deref())
}
/// The switch's value rule (pure): anything but "0" is on.
pub fn compact_offer_switch(value: Option<&str>) -> bool {
    value.map(str::trim) != Some("0")
}

/// Default: only the compact alias is announced (no Find PCs needed on
/// Android, works without extended scanning, and no second provider carries
/// the same RDI from another address, which Android flags as a copied ID).
/// Alternative MESHLIVE_QW_FORM=1 brings back the Design QW extended
/// announcement (c8d5a10a service data) next to it. With the compact offer
/// switched off (MESHLIVE_COMPACT_OFFER=0) the QW form is always on, so the PC
/// never announces nothing.
pub fn qw_form_enabled() -> bool {
    qw_form_switch(std::env::var("MESHLIVE_QW_FORM").ok().as_deref(), compact_offer_enabled())
}
/// The rule (pure): the QW form is on when asked for ("1") or when it is the only form.
pub fn qw_form_switch(value: Option<&str>, compact_on: bool) -> bool {
    value.map(str::trim) == Some("1") || !compact_on
}

/// Experimental MESHLIVE_COMPACT_DISCOVERABLE=1 (default off, measurement
/// only): the compact alias provider asks for IsDiscoverable, which may make
/// Windows set LE General Discoverable in the flags (some Android hardware
/// filters drop adverts without it) but may also put the PC's name on air.
/// Measured 2026-10-20 (A15s raw observer): the legacy advert then carries
/// the PC's computer name (0x09) and Windows still sends a second, extended
/// advert from another address. Never a default.
pub fn compact_discoverable() -> bool {
    std::env::var("MESHLIVE_COMPACT_DISCOVERABLE").ok().as_deref().map(str::trim) == Some("1")
}

/// The PC's Design QW call offer (9-byte payload) as the compact v2 call
/// offer UUID (PROTOCOL "Experimental iPhone compact discovery"): the same
/// RDI, GATT bit 59, codec bits 54..56, cost, relay consent bit 18. Android
/// decodes it as a version 5 call offer and dials c8d5a105 over GATT.
/// Big-endian canonical order (GUID::from_u128 of these bytes).
pub fn pc_compact_alias(payload: &[u8; crate::pc_announcement::LEN]) -> Result<[u8; 16], Error> {
    let offer = crate::pc_announcement::CallOffer::decode(payload).map_err(|_| Error::Fields)?;
    compact_uuid(&offer.to_announcement())
}

/// The canonical UUID text of 16 big-endian bytes (8-4-4-4-12, uppercase).
pub fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Adds the recovery hint to a version 5 viewer request (flag and six spare bytes).
pub fn with_recovery(request: &Announcement, hint: [u8; 6]) -> Result<Announcement, Error> {
    let mut value = request.clone();
    value.flags |= RECOVERY;
    value.recovery = Some(hint);
    value.sequence = 0;
    value.width = 0;
    value.height = 0;
    value.fps = 0;
    value.encode().map_err(Error::Discovery)?;
    Ok(value)
}

/// Silence before the new set after a boundary, from one uniform random draw.
pub fn boundary_silence_ms(random: u32) -> u64 {
    SILENCE_MIN_MS + u64::from(random) % SILENCE_SPAN_MS
}

/// A source or forwarder accepts a viewer request whose token is its current
/// route token or one of the two it replaced most recently, each for 60 s.
#[derive(Default)]
pub struct TokenGrace {
    current: Option<u32>,
    replaced: [Option<(u32, u64)>; 2],
}
impl TokenGrace {
    pub fn advertise(&mut self, token: u32, elapsed_ms: u64) {
        if self.current == Some(token) {
            return;
        }
        if let Some(old) = self.current.replace(token) {
            self.replaced = [Some((old, elapsed_ms)), self.replaced[0]];
        }
    }
    pub fn accepts(&self, token: u32, elapsed_ms: u64) -> bool {
        self.current == Some(token)
            || self
                .replaced
                .iter()
                .flatten()
                .any(|(old, at)| *old == token && elapsed_ms.saturating_sub(*at) < TOKEN_GRACE_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{ACCEPTING, CALL, GATT};

    fn pc_payload(rdi: [u8; 6], codecs: u8, consent: bool) -> [u8; crate::pc_announcement::LEN] {
        crate::pc_announcement::CallOffer { rdi, relay: false, consent, codecs, cost: 0 }.encode().unwrap()
    }

    #[test]
    fn the_compact_alias_is_the_protocol_v2_call_offer_with_the_gatt_bit() {
        let rdi = [0x44, 0xe6, 0x9c, 0x0a, 0x7a, 0x7d];
        let uuid = pc_compact_alias(&pc_payload(rdi, 3, false)).unwrap();
        assert_eq!(uuid_text(&uuid), "C8D6844E-69C0-A7A7-D8C0-000000000000");
        // Without bit 59 (GATT) it is exactly compact-discovery-006 (LE CoC).
        let mut coc = uuid;
        coc[8] &= !0x08;
        assert_eq!(uuid_text(&coc), "C8D6844E-69C0-A7A7-D0C0-000000000000");
        // What a phone decodes (accept_rotating, as Android with rotating IDs on):
        // a version 5 direct call offer over GATT, the PC's RDI and codecs.
        let seen = Announcement::from_compact(uuid, true).unwrap();
        assert_eq!((seen.version, seen.prefix, seen.flags, seen.summary, seen.cost, seen.token), (5, rdi, ACCEPTING | CALL | GATT, 3, 0, 0));
        assert!(crate::dial::is_call_offer(&seen), "a phone dials it like any direct GATT call offer");
        // compact-discovery-007: refused while rotating IDs are off.
        assert!(Announcement::from_compact(uuid, false).is_err());
        // The same offer as the QW payload.
        assert!(seen == crate::pc_announcement::CallOffer::decode(&pc_payload(rdi, 3, false)).unwrap().to_announcement());
    }

    #[test]
    fn the_compact_alias_carries_consent_and_follows_the_rdi() {
        let a = pc_compact_alias(&pc_payload([1, 2, 3, 4, 5, 6], 1, true)).unwrap();
        assert_eq!(uuid_text(&a), "C8D68010-2030-4050-6840-000000040000", "VP9 only, relay consent bit 18");
        let b = pc_compact_alias(&pc_payload([1, 2, 3, 4, 5, 7], 1, true)).unwrap();
        assert_ne!(a, b);
        assert!(pc_compact_alias(&[0u8; crate::pc_announcement::LEN]).is_err());
    }

    #[test]
    fn the_qw_form_is_off_by_default_unless_asked_or_the_only_form() {
        assert!(!qw_form_switch(None, true), "default: compact only");
        assert!(!qw_form_switch(Some("0"), true));
        assert!(qw_form_switch(Some("1"), true), "the Alternative arm");
        assert!(qw_form_switch(None, false), "never announce nothing");
    }
    #[test]
    fn the_compact_offer_switch_is_on_unless_zero() {
        assert!(compact_offer_switch(None));
        assert!(compact_offer_switch(Some("1")));
        assert!(!compact_offer_switch(Some("0")));
        assert!(!compact_offer_switch(Some(" 0 ")));
    }
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    fn six(value: &str) -> [u8; 6] {
        hex(value).try_into().unwrap()
    }
    // PROTOCOL rotating-ID vectors: salt key 00..1f, node a1b2..8f90, epoch 0x001e6000.
    const NODE: [u8; 16] = [
        0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f,
        0x90,
    ];
    const EPOCH: u32 = 1_990_656;
    fn key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }
    fn blank(flags: u8, summary: u8) -> Announcement {
        Announcement {
            version: 2,
            prefix: NODE[..6].try_into().unwrap(),
            flags,
            summary,
            token: 0,
            cost: 0,
            sequence: 0,
            width: 0,
            height: 0,
            fps: 0,
            bitrate: 0,
            recovery: None,
        }
    }
    #[test]
    fn version_5_call_stream_and_viewer_offers_match_the_android_vectors() {
        let (call, entry) = rotating_offer_for(
            &blank(ACCEPTING | CALL, 0x03 | BMH3_HINT),
            Role::CallOffer,
            &key(),
            &NODE,
            EPOCH,
        )
        .unwrap();
        assert_eq!(entry.rdi, six("44e69c0a7a7d"));
        assert_eq!(
            call.encode().unwrap().as_slice(),
            hex("0544e69c0a7a7d410300000000000000000000000000")
        );
        assert_eq!(
            compact_uuid(&call).unwrap(),
            0xc8d6844e_69c0_a7a7_d0c0_000000000000u128.to_be_bytes()
        );
        assert_eq!(route_mask(&six("bb2d061f6480")), 0x8066c9bc);
        assert_eq!(
            route_token(0x12345678, &six("bb2d061f6480")).unwrap(),
            0x92529fc4
        );
        assert_eq!(logical_token(0x92529fc4, &six("bb2d061f6480")), 0x12345678);
        let mut stream = blank(STREAM | GATT, 0x01);
        stream.sequence = 1;
        stream.width = 640;
        stream.height = 360;
        stream.fps = 15;
        stream.bitrate = 296; // rounded to the nearest 25 kbps on air
        let (stream, entry) = rotating_offer_for(
            &stream,
            Role::StreamOffer {
                logical: 0x12345678,
            },
            &key(),
            &NODE,
            EPOCH,
        )
        .unwrap();
        assert_eq!(entry.rdi, six("bb2d061f6480"));
        assert_eq!(
            stream.encode().unwrap().as_slice(),
            hex("05bb2d061f64800c0192529fc40001028001680f012c")
        );
        let mut request = blank(ACCEPTING | GATT, 0x03);
        request.bitrate = 300;
        let (request, entry) = rotating_offer_for(
            &request,
            Role::ViewerRequest { route: 0x92529fc4 },
            &key(),
            &NODE,
            EPOCH,
        )
        .unwrap();
        assert_eq!(entry.rdi, six("974ae410ea91"));
        assert_eq!(
            request.encode().unwrap().as_slice(),
            hex("05974ae410ea91090092529fc400000000000000012c")
        );
        // Recovery hint (RD3) from Kmedia 40..5f for the failed node above.
        let media: [u8; 32] = std::array::from_fn(|i| 0x40 + i as u8);
        let khint = recovery_key(&media).unwrap();
        assert_eq!(&khint[..4], hex("07eb0508").as_slice());
        assert_eq!(&khint[29..], hex("305c28").as_slice());
        let hint = recovery_hint(&khint, &NODE, 0x12345678, &entry.rdi, 0x92529fc4).unwrap();
        assert_eq!(hint, six("4d832e9d4dc7"));
        assert_eq!(
            recovery_hint(&khint, &NODE, 0x12345678, &six("67aba670e65a"), 0x92529fc4).unwrap(),
            six("f9fa5ad0b780")
        );
        assert_eq!(
            with_recovery(&request, hint)
                .unwrap()
                .encode()
                .unwrap()
                .as_slice(),
            hex("05974ae410ea91890092529fc4004d832e9d4dc7012c")
        );
    }
    #[test]
    fn tokens_bitrates_and_wrong_roles_fail_closed() {
        for logical in [0, 0x8000_0000, u32::MAX] {
            assert_eq!(route_token(logical, &[0; 6]), Err(Error::Fields));
        }
        assert_ne!(route_token(1, &[0; 6]).unwrap() & 0x8000_0000, 0);
        for (input, output) in [
            (0, 0),
            (1, 25),
            (12, 25),
            (37, 25),
            (38, 50),
            (296, 300),
            (1500, 1500),
            (u16::MAX, 65525),
        ] {
            assert_eq!(rounded_bitrate(input), output, "{input}");
        }
        // Announcement deliberately has no Debug (no identifiers in logs).
        assert!(matches!(
            rotating_offer(
                &blank(ACCEPTING | CALL, 3),
                Role::StreamOffer { logical: 5 },
                [1; 6]
            ),
            Err(Error::Fields)
        ));
        // A version 2 call offer keeps its BMH3 hint; the compact form masks it.
        let v2 = blank(ACCEPTING | CALL | GATT, 0x01 | BMH3_HINT);
        assert_eq!(v2.encode().unwrap()[8], 0x21);
        let uuid = compact_uuid(&v2).unwrap();
        assert_eq!(
            Announcement::from_compact(uuid, false).unwrap().summary,
            0x01
        );
    }
    #[test]
    fn boundary_silence_is_one_to_eight_seconds_and_the_token_grace_keeps_two_for_a_minute() {
        assert_eq!(boundary_silence_ms(0), 1_000);
        assert_eq!(boundary_silence_ms(7_000), 8_000);
        assert_eq!(boundary_silence_ms(7_001), 1_000);
        assert!(
            (0..100_000)
                .map(boundary_silence_ms)
                .all(|ms| (1_000..=8_000).contains(&ms))
        );
        let mut grace = TokenGrace::default();
        assert!(!grace.accepts(1, 0));
        grace.advertise(1, 0);
        grace.advertise(2, 1_000);
        grace.advertise(3, 2_000);
        for token in [1, 2, 3] {
            assert!(grace.accepts(token, 2_500));
        }
        grace.advertise(4, 3_000);
        assert!(!grace.accepts(1, 3_000));
        assert!(grace.accepts(2, 61_999));
        assert!(!grace.accepts(2, 62_000));
        assert!(grace.accepts(4, u64::MAX));
    }
}
