//! Outgoing call setup over a direct GATT link, as a stranger (Android Phase 2
//! rules): connect to the selected rotating call offer, BMH3 link handshake
//! with a per-attempt throwaway key, the end-to-end BMH3 handshake in BMS1
//! type-4 records, then a `CallSession` whose Accept counts only after the
//! callee's REVEAL proves the selected rotating ID. The `call_phone` tool and
//! the app's Bluetooth backend both use this one path.
//!
//! Nothing here scans; the caller supplies the sighting's route. Nothing here
//! opens a camera, microphone or audio endpoint.
use crate::{
    bluetooth::{carrier, central::Central, scan::Route},
    bmh2::{Capabilities, SigningIdentity},
    bmh3::{
        self, Purpose,
        handshake::{Config, InitiatorOptions, InitiatorProgress, Peer, Waiting2},
        reveal::{ROLE_CALLER, RevealSession},
    },
    call::CallLink,
    call_session::{CallSession, RevealGate},
    discovery::CALL,
    framing::{HandshakeReader, HopReader, frame_handshake},
    identity::Identity,
    media::VideoHeader,
    opus,
    records::{Layer, Policy, Sealer},
    rotating::{self, Selection},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Process authenticated setup bytes even when the next carrier read times out.
/// Kept separate from the native carrier so pipelined call flows are testable.
pub(crate) fn feed_buffered<D: crate::listen::UnitDecoder>(
    session: &mut CallSession<D>, buffer: &mut Vec<u8>, pulled: bool, now_ns: u64, enabled: bool,
) -> Result<Vec<crate::call_session::Notice>, Error> {
    if !pulled && (!enabled || buffer.is_empty()) { return Ok(Vec::new()); }
    let mut input = buffer.as_slice();
    let result = session.on_input(&mut input, now_ns);
    let used = buffer.len() - input.len(); buffer.drain(..used);
    result.map_err(at(Stage::Call))
}

/// Where an outgoing call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Connect,
    Link,
    Endpoint,
    Call,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub stage: Stage,
    pub detail: String,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.detail)
    }
}
impl std::error::Error for Error {}
fn at<E: std::fmt::Debug>(stage: Stage) -> impl Fn(E) -> Error {
    move |error| Error {
        stage,
        detail: format!("{error:?}"),
    }
}
fn fail(stage: Stage, detail: &str) -> Error {
    Error {
        stage,
        detail: detail.into(),
    }
}

/// CF-CALL-07 / CF-LINK-31: a setup failure (before the end-to-end
/// handshake completed) may be retried once, on a fresh sighting, only while
/// at most this much has passed since the first dial started.
pub const RETRY_WINDOW: Duration = Duration::from_secs(15);
/// The rescan before the retry waits at most this long (and never past the window).
pub const RETRY_RESIGHT: Duration = Duration::from_secs(8);

/// Whether a failed first attempt is retried, and how long to rescan.
/// CF-CALL-04: a callee already in a call closes a new link before the hop
/// handshake. That is a clear answer ("they may be in another call"), shown
/// at once instead of after a retry.
pub fn refused_by_peer(error: &Error) -> bool {
    // "link closed" (end of stream) or the carrier's Closed (the device disconnected).
    error.stage == Stage::Link && (error.detail.contains("link closed") || error.detail == "Closed")
}

/// CF-PLAT-05: a stale Windows pairing; retrying cannot help.
pub fn bond_mismatch(error: &Error) -> bool {
    error.stage == Stage::Connect && error.detail == "BondMismatch"
}

/// CF-LIFE-13: this PC's Bluetooth was off (or turned off) during the
/// dial; retrying cannot help until the user turns it on.
pub fn radio_off(error: &Error) -> bool {
    error.stage == Stage::Connect && error.detail.starts_with("Radio(")
}

pub fn retry_plan(error: &Error, since_dial: Duration, cancelled: bool) -> Option<Duration> {
    let setup = matches!(error.stage, Stage::Connect | Stage::Link | Stage::Endpoint);
    if cancelled || !setup || since_dial >= RETRY_WINDOW || refused_by_peer(error) || bond_mismatch(error) || radio_off(error) {
        return None;
    }
    Some(RETRY_RESIGHT.min(RETRY_WINDOW - since_dial))
}

/// CF-CALL-21: what the user reads after a setup failure.
pub fn setup_failure_text(error: Option<&Error>) -> String {
    match error {
        None => "Not found nearby".into(),
        Some(e) if bond_mismatch(e) => "Couldn't connect the call: an old Bluetooth pairing with this phone is in the way. Remove the pairing in Windows Bluetooth settings, then call again.".into(),
        Some(e) if radio_off(e) => "Couldn't connect the call: Bluetooth is off on this PC. Turn it on in Windows settings, then call again.".into(),
        Some(e) if refused_by_peer(e) => "Couldn't connect the call: their phone closed the connection. They may be in another call. Try again in a moment.".into(),
        // CF-HS-56: the hop handshake's own 10 s bound (HOP_HANDSHAKE_SECONDS).
        Some(e) if e.stage == Stage::Link && e.detail == "no msg2" => {
            "Couldn't connect the call: their phone didn't answer in time. Try again.".into()
        }
        Some(e) => match e.stage {
            Stage::Connect => "Couldn't connect the call: their phone didn't accept the connection. Try again.".into(),
            Stage::Link | Stage::Endpoint => "Couldn't connect the call. Try again.".into(),
            Stage::Call => "Couldn't connect the call. Try again.".into(),
        },
    }
}

/// Both handshakes must finish well inside Android's 20 s callee deadline.
pub const HANDSHAKE_SECONDS: u64 = 15;
/// CF-CALL-06: the hop (link) handshake must complete within 10 s.
pub const HOP_HANDSHAKE_SECONDS: u64 = 10;
/// CF-CALL-08: the callee closes a link without both handshakes 20 s after
/// it accepted it; measured here from the start of our handshake, so 18 s
/// leaves the connect, discovery and subscription time before it.
pub const CALL_HANDSHAKE_SECONDS: u64 = 18;

/// A GATT call offer this PC dials: version 5 (rotating ID, BMH3), or a
/// direct, non-helper version 2 offer: BMH3 when it carries the hint
/// (summary bit 5, CF-HS-13), otherwise BMH2 (`needs_bmh2`; a default
/// iPhone/Mac or a phone with BMH3 off).
pub fn is_call_offer(announcement: &crate::discovery::Announcement) -> bool {
    call_offer(announcement, relay_calls_enabled())
}
pub fn relay_calls_enabled() -> bool { std::env::var("MESHLIVE_RELAY_CALLS").as_deref() == Ok("1") }
fn call_offer(announcement: &crate::discovery::Announcement, relay: bool) -> bool {
    // CF-DISC-04: a call offer has bit 6 and not bit 2 (stream).
    if announcement.flags & CALL == 0 || announcement.flags & crate::discovery::STREAM != 0 || !announcement.is_gatt() {
        return false;
    }
    match announcement.version {
        // A relayed (cost > 0) v5 offer cannot open its RDI on a direct link.
        5 => announcement.cost == 0 || relay && relay_offer(announcement),
        // Direct, non-helper version 2 offers: BMH3 with the hint, else BMH2.
        2 => announcement.cost == 0 && announcement.flags & crate::discovery::RELAY == 0,
        _ => false,
    }
}
fn relay_offer(offer: &crate::discovery::Announcement) -> bool {
    offer.version == 5 && (1..=4).contains(&offer.cost) && offer.is_gatt()
        && offer.flags & (CALL | crate::discovery::RELAY | crate::discovery::STREAM) == (CALL | crate::discovery::RELAY)
}
fn relay_peer(peer: &Peer) -> Result<(), bmh3::Error> {
    let caps = peer.capabilities();
    if peer.extensions().pseudonym || caps.maximum_children == 0 || caps.transport & 0x4c != 0x44 { return Err(bmh3::Error::Policy); }
    crate::identity_gate::admit(peer.node_id(), peer.fingerprint()).map(|_| ()).map_err(|_| bmh3::Error::Policy)
}
fn stranger_config<'a>(purpose: Purpose, real: &'a dyn SigningIdentity, pseudonym: &'a dyn SigningIdentity, caps: Capabilities) -> Result<Config<'a>, bmh3::Error> {
    Config::new(purpose, real, caps)?.with_stranger(pseudonym, caps)
}
fn selected_peer(peer: &Peer, rdi: &[u8; 6], version5: bool, wall: u32, who: Option<&ContactDial>) -> Result<(), bmh3::Error> {
    if who.is_some_and(|contact| peer.extensions().pseudonym || *peer.node_id() != contact.contact) { return Err(bmh3::Error::Policy); }
    if peer.extensions().pseudonym { return Ok(()); }
    crate::identity_gate::admit(peer.node_id(), peer.fingerprint()).map_err(|_| bmh3::Error::Policy)?;
    let entries = peer.extensions().opening.as_deref().map(rotating::decode_body).transpose().map_err(|_| bmh3::Error::Policy)?;
    match rotating::check_selection(rdi, version5, 0, peer.node_id(), entries.as_deref(), wall) {
        Ok(Selection::Pass) => Ok(()),
        _ => Err(bmh3::Error::Policy),
    }
}
fn endpoint_matches_link(peer: &Peer, link_peer: &[u8; 91], relay_hops: u8) -> bool {
    if relay_hops == 0 { peer.public_key() == link_peer } else { peer.public_key() != link_peer }
}
/// `capabilitySummary` bit 5 of a version 2 call offer.
pub const BMH3_HINT: u8 = 0x20;

/// A version 2 call offer without the BMH3 hint is dialled with BMH2.
pub fn needs_bmh2(announcement: &crate::discovery::Announcement) -> bool {
    announcement.version == 2 && announcement.summary & BMH3_HINT == 0
}

/// Calls the owner of a version 2 offer that speaks BMH2 only, with this
/// PC's persistent `identity` (BMH2 has no stranger keys).
pub fn dial_bmh2(
    route: &Route,
    prefix: [u8; 6],
    identity: &Identity,
    call_id: [u8; 16],
    voice: bool,
    cancelled: Arc<AtomicBool>,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    let mut central =
        Central::connect_traced(route, cancelled.clone(), &mut *trace).map_err(at(Stage::Connect))?;
    trace("gatt_connected", "service=c8d5a105 cccd=notify");
    let reached = crate::verify_scan::bmh2_handshake(&mut central, prefix, identity, &cancelled, &mut *trace)?;
    let peer_discovery = reached.negotiated;
    let (header, video_plan) = crate::call_video::header_for(reached.peer_capabilities.decode_codecs, &encoders());
    let mut session = CallSession::<opus::Decoder>::outgoing(
        reached.link,
        call_id,
        voice,
        0,
        header,
        |c| opus::Decoder::new(c, opus::Profile::StreamOrCall),
        0,
    )
    .map_err(at(Stage::Call))?;
    session.check_header_codecs(crate::video::decode_mask(), reached.peer_capabilities.encode_codecs);
    call_preset(&mut central, voice, &mut *trace);
    let paired = central.paired() == Some(true);
    Ok(Dialed {
        paired,
        header,
        video_plan,
        link: Link::Central(central),
        peer_real: Some((reached.peer_node, reached.peer_fingerprint)),
        buffer: reached.buffer,
        session,
        start: Instant::now(),
        peer_discovery,
    })
}

pub fn wall_epoch() -> Option<u32> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    rotating::wall_epoch(seconds)
}

/// A connected, authenticated call whose Invite (and Camera off for a voice
/// call) is queued. The owner drives `session.next_write`/`on_input` with
/// `now_ns()` and moves bytes with `pull`/`central.write`.
/// The carrier of a call: a link this PC opened (central, outgoing) or one
/// a phone opened to this PC (peripheral, Design QW incoming).
pub enum Link {
    Central(Central),
    Peripheral(crate::bluetooth::peripheral::Peripheral),
}
impl Link {
    pub fn read(&self, output: &mut [u8], deadline: Instant) -> Result<usize, crate::bluetooth::carrier::Error> {
        match self {
            Self::Central(c) => c.read(output, deadline),
            Self::Peripheral(p) => p.read(output, deadline),
        }
    }
    pub fn write(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), crate::bluetooth::carrier::Error> {
        match self {
            Self::Central(c) => c.write(bytes, deadline),
            Self::Peripheral(p) => p.write(bytes, deadline),
        }
    }
    /// Connection parameters are the central's to ask for; as peripheral
    /// the phone decides them.
    pub fn request_preset(&mut self, video: bool, why: &str, trace: &mut dyn FnMut(&str, &str)) {
        match self {
            Self::Central(c) => c.request_preset(video, why, trace),
            Self::Peripheral(_) => trace("preset_not_ours", &format!("why={why} role=peripheral (the phone sets connection parameters)")),
        }
    }
    pub fn loss_context(&self) -> String {
        match self {
            Self::Central(c) => c.loss_context(),
            Self::Peripheral(p) => format!("role=peripheral native_error={:?}", p.native_error()),
        }
    }
    /// The link's parameters as Windows reports them (peripheral; the
    /// central traces its own on every change).
    pub fn link_state(&mut self) -> Option<String> {
        match self {
            Self::Central(_) => None,
            Self::Peripheral(p) => Some(p.link_state()),
        }
    }
    pub fn drain_events(&self, trace: &mut dyn FnMut(&str, &str)) {
        match self {
            Self::Central(c) => c.drain_events(trace),
            Self::Peripheral(p) => {
                for (phase, line) in p.take_notes() {
                    trace(phase, &line);
                }
            }
        }
    }
    /// Experimental GR1 while idle: HELLO, HIGH, the reader's rounds and
    /// queued resends (no-op while `MESHLIVE_GATT_CHUNK_RECOVERY` is off).
    pub fn service(&mut self) -> Result<(), crate::bluetooth::carrier::Error> {
        match self {
            Self::Central(c) => c.service(),
            Self::Peripheral(p) => p.service(),
        }
    }
    /// CF-LINK-23 read-back of a requested connection preset (central only).
    pub fn recheck_preset(&mut self, trace: &mut dyn FnMut(&str, &str)) {
        if let Self::Central(c) = self {
            c.recheck_preset(trace);
        }
    }
    pub fn stack_facts(&self) -> Vec<String> {
        match self {
            Self::Central(c) => c.stack_facts(),
            Self::Peripheral(_) => vec!["role=peripheral".into()],
        }
    }
    pub fn paired(&self) -> Option<bool> {
        match self {
            Self::Central(c) => c.paired(),
            Self::Peripheral(_) => None,
        }
    }
    pub fn take_arrivals(&self) -> Vec<(Instant, usize)> {
        match self {
            Self::Central(c) => c.take_arrivals(),
            Self::Peripheral(p) => p.take_arrivals(),
        }
    }
}

pub struct Dialed {
    pub link: Link,
    /// Received bytes not yet consumed by the session.
    pub buffer: Vec<u8>,
    pub session: CallSession<opus::Decoder>,
    pub start: Instant,
    /// The callee announced BDX1 (transport bit 3).
    pub peer_discovery: bool,
    /// Windows is paired (bonded) with this phone; see `Central::paired`.
    pub paired: bool,
    /// This PC's announced stream header and the encoders usable for it
    /// (empty: no common codec, the call stays voice-only).
    pub header: VideoHeader,
    pub video_plan: Vec<(u8, crate::video::Backend)>,
    /// The peer's real identity (node ID, fingerprint) when the handshake
    /// proved it; None for a pseudonym until its REVEAL (`Notice::PeerRevealed`).
    pub peer_real: Option<([u8; 16], [u8; 32])>,
}
impl Dialed {
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
    /// Receives and feeds the session; returns its notices.
    pub fn pump_input(&mut self, wait: Duration) -> Result<Vec<crate::call_session::Notice>, Error> {
        let pulled = pull_link(&self.link, &mut self.buffer, wait)?;
        // Arrival trains for the report's arrivalKbps (CF-MEDIA-23).
        let start = self.start;
        let arrivals: Vec<(u64, usize)> = self
            .link
            .take_arrivals()
            .into_iter()
            .map(|(at, bytes)| (at.saturating_duration_since(start).as_nanos() as u64, bytes))
            .collect();
        self.session.on_arrivals(&arrivals);
        let now = self.now_ns();
        feed_buffered(&mut self.session, &mut self.buffer, pulled, now,
            std::env::var("MESHLIVE_CALL_BUFFERED_INPUT").as_deref() == Ok("1"))
    }
    /// Writes every record the session has due, in sequence order.
    pub fn flush(&mut self) -> Result<usize, Error> {
        let mut count = 0;
        loop {
            let now = self.now_ns();
            // The liveness hang-up gets at most 300 ms (the peer is silent).
            let budget = if self.session.is_dead(now) { Duration::from_millis(300) } else { Duration::from_secs(8) };
            let Some(record) = self.session.next_write(now).map_err(at(Stage::Call))? else {
                return Ok(count);
            };
            self.link.write(&record, Instant::now() + budget).map_err(at(Stage::Call))?;
            count += 1;
        }
    }
}

/// `pull` for either role.
pub fn pull_link(link: &Link, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, Error> {
    let mut chunk = [0u8; 4096];
    match link.read(&mut chunk, Instant::now() + wait) {
        Ok(0) => Err(fail(Stage::Call, "link closed")),
        Ok(n) => {
            buffer.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(crate::bluetooth::carrier::Error::Timeout) => Ok(false),
        Err(error) => Err(at(Stage::Call)(error)),
    }
}

/// Bytes from the GATT carrier into `buffer`, waiting at most `wait`.
pub fn pull(central: &Central, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, Error> {
    let mut chunk = [0u8; 4096];
    match central.read(&mut chunk, Instant::now() + wait) {
        Ok(0) => Err(fail(Stage::Call, "link closed")),
        Ok(n) => {
            buffer.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(carrier::Error::Timeout) => Ok(false),
        Err(error) => Err(at(Stage::Call)(error)),
    }
}

/// What this PC can encode; on an enumeration failure only the built-in
/// software AV1 encoder is assumed.
pub fn encoders() -> crate::video::Availability {
    crate::video::encoder_availability().unwrap_or_else(|_| crate::video::Availability {
        av1_hardware: Vec::new(),
        vp9_hardware: Vec::new(),
        av1_software: crate::video::aom_version(),
        vp9_software: crate::video::vpx_version(),
    })
}

/// Connects to `route` (the sighting of rotating call offer `rdi`) and sets
/// up an outgoing call with `call_id` (CSPRNG bytes from the caller). Setting
/// `cancelled` aborts the connection and both handshakes. `trace` gets
/// `(phase, detail)` pairs without addresses or keys.
pub fn dial(
    route: &Route,
    rdi: [u8; 6],
    call_id: [u8; 16],
    voice: bool,
    cancelled: Arc<AtomicBool>,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    dial_selected(route, rdi, true, None, call_id, voice, cancelled, trace)
}

/// `dial` for the selected offer's version: `version5` (rotating ID) checks
/// openings of the RDI; version 2 checks that the 6 announced bytes are the
/// callee's node prefix, at the link (a real-key answer) and in its REVEAL.
pub fn dial_selected(
    route: &Route,
    rdi: [u8; 6],
    version5: bool,
    real: Option<Identity>,
    call_id: [u8; 16],
    voice: bool,
    cancelled: Arc<AtomicBool>,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    dial_selected_route(route, rdi, version5, real, None, 0, call_id, voice, cancelled, trace)
}

/// Existing Android relay-chain protocol; never a BMH2 downgrade or a helper.
pub fn dial_relay(
    route: &Route, offer: &crate::discovery::Announcement, real: Identity, who: Option<&ContactDial>,
    call_id: [u8; 16], voice: bool, cancelled: Arc<AtomicBool>, trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    if !relay_calls_enabled() || !relay_offer(offer) { return Err(fail(Stage::Connect, "Unsupported call relay offer")); }
    dial_selected_route(route, offer.prefix, true, Some(real), who, offer.cost, call_id, voice, cancelled, trace)
}

fn dial_selected_route(
    route: &Route, rdi: [u8; 6], version5: bool, real: Option<Identity>, who: Option<&ContactDial>, relay_hops: u8,
    call_id: [u8; 16], voice: bool, cancelled: Arc<AtomicBool>, trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    let wall = wall_epoch().ok_or(fail(Stage::Connect, "clock before 1970"))?;
    let mut central =
        Central::connect_traced(route, cancelled.clone(), &mut *trace).map_err(at(Stage::Connect))?;
    trace("gatt_connected", "service=c8d5a105 cccd=notify");

    // Link handshake as a stranger: a fresh test identity plus a per-attempt throwaway.
    // The identity revealed to the callee after its REVEAL: this PC's
    // persistent one when given (contacts can then recognise it and leases
    // can move), otherwise a fresh non-persisted test identity.
    let real = match real {
        Some(identity) => identity,
        None => Identity::ephemeral().map_err(at(Stage::Link))?,
    };
    let pseudonym = Identity::ephemeral().map_err(at(Stage::Link))?;
    let link_caps = Capabilities {
        transport: 0x0c,
        encode_codecs: 0x03,
        decode_codecs: 0x03,
        maximum_bitrate_kbps: 1100,
        maximum_children: 0,
    };
    let check = |peer: &Peer| selected_peer(peer, &rdi, version5, wall, who);
    let config = stranger_config(Purpose::Link, &real, &pseudonym, link_caps).map_err(at(Stage::Link))?;
    let (waiting, m1) = Waiting2::start(config, InitiatorOptions::default()).map_err(at(Stage::Link))?;
    let handshake_started = Instant::now();
    let deadline = handshake_started + Duration::from_secs(HOP_HANDSHAKE_SECONDS);
    central
        .write(&frame_handshake(&m1).map_err(at(Stage::Link))?, deadline)
        .map_err(at(Stage::Link))?;
    trace("link_msg1", "bytes=122");
    let mut buffer = Vec::new();
    let mut frames = HandshakeReader::default();
    let m2 = loop {
        let mut input = buffer.as_slice();
        let got = frames.read(&mut input).map_err(at(Stage::Link))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(m) = got {
            break m;
        }
        if Instant::now() >= deadline {
            return Err(fail(Stage::Link, "no msg2"));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(fail(Stage::Link, "cancelled"));
        }
        pull(&central, &mut buffer, Duration::from_millis(50)).map_err(|e| Error { stage: Stage::Link, ..e })?;
    };
    trace("link_msg2", &format!("bytes={}", m2.len()));
    let (progress, m3) = waiting.receive(&m2, |peer: &Peer| {
        if relay_hops == 0 { check(peer) } else { relay_peer(peer) }
    }).map_err(at(Stage::Link))?;
    let InitiatorProgress::Complete(link) = progress else {
        return Err(fail(Stage::Link, "responder demanded I_FIRST"));
    };
    central
        .write(&frame_handshake(&m3).map_err(at(Stage::Link))?, deadline)
        .map_err(at(Stage::Link))?;
    let link_peer = *link.peer().public_key();
    trace(
        "link_done",
        &format!(
            "peer_pseudonymous={} peer_bit3={}",
            link.peer().extensions().pseudonym,
            link.peer().capabilities().transport & 8 != 0
        ),
    );
    // CF-CALL-08: both handshakes inside the callee's 20 s.
    let deadline = handshake_started + Duration::from_secs(CALL_HANDSHAKE_SECONDS);

    // Endpoint handshake in BMS1 type-4 records (no BDO1: pseudonymous responder).
    let mut hop_out =
        Sealer::new(Layer::Hop, link.keys().send_key(), Policy::default()).map_err(at(Stage::Endpoint))?;
    let mut hop_in =
        HopReader::new(link.keys().receive_key(), Policy::default()).map_err(at(Stage::Endpoint))?;
    // The endpoint hello offers call video fragments (bit 7, CF-MEDIA-76).
    let endpoint_caps = Capabilities {
        transport: (if relay_hops == 0 { crate::call::fragments::endpoint_transport(link_caps.transport) } else { link_caps.transport }) | if crate::labels::enabled() { crate::labels::CAPABILITY_BIT } else { 0 },
        // The decoders this PC really has (CF-HS-02), not a constant.
        decode_codecs: crate::video::decode_mask(),
        ..link_caps
    };
    let config = stranger_config(Purpose::Endpoint, &real, &pseudonym, endpoint_caps).map_err(at(Stage::Endpoint))?;
    // A contact tag belongs only to the callee, never to the first-hop relay.
    let tag = who.map(|contact| {
        let key = rotating::contact_tag_key(&contact.lease_key).map_err(at(Stage::Endpoint))?;
        Ok::<_, Error>(crate::bmh3::handshake::ContactTag::new(*key, rotating::advert_binding(&rdi, rotating::ROLE_CALL_OFFER, None), contact.contact))
    }).transpose()?;
    let (waiting, m1) = Waiting2::start(config, InitiatorOptions { tag, ..Default::default() }).map_err(at(Stage::Endpoint))?;
    central
        .write(&hop_out.seal(4, 0, &m1).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(at(Stage::Endpoint))?;
    trace("endpoint_msg1", "");
    let m2 = loop {
        let mut input = buffer.as_slice();
        let record = hop_in.read(&mut input, 4096).map_err(at(Stage::Endpoint))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(r) = record {
            if r.header.kind != 4 {
                return Err(fail(Stage::Endpoint, "unexpected hop record type"));
            }
            break r.plaintext;
        }
        if Instant::now() >= deadline {
            return Err(fail(Stage::Endpoint, "no msg2"));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(fail(Stage::Endpoint, "cancelled"));
        }
        pull(&central, &mut buffer, Duration::from_millis(50))
            .map_err(|e| Error { stage: Stage::Endpoint, ..e })?;
    };
    let same_throwaway = |peer: &Peer| -> Result<(), bmh3::Error> {
        // A direct call reaches the same key at the link and end to end: the
        // same throwaway for a pseudonymous callee, and the same real key for
        // every other one (CF-DISC-21, as the listener and the contact path).
        if !endpoint_matches_link(peer, &link_peer, relay_hops) {
            return Err(bmh3::Error::Policy);
        }
        check(peer)
    };
    let (progress, m3) = waiting.receive(&m2, same_throwaway).map_err(at(Stage::Endpoint))?;
    let InitiatorProgress::Complete(endpoint) = progress else {
        return Err(fail(Stage::Endpoint, "end-to-end I_FIRST"));
    };
    central
        .write(&hop_out.seal(4, 0, &m3).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(at(Stage::Endpoint))?;
    let peer = endpoint.peer();
    let peer_caps = peer.capabilities();
    let (peer_pseudonymous, peer_node, peer_fingerprint) = (peer.extensions().pseudonym, *peer.node_id(), *peer.fingerprint());
    trace(
        "endpoint_done",
        &format!(
            "peer_transport={:02x} peer_decode={:02x} peer_encode={:02x}",
            peer_caps.transport, peer_caps.decode_codecs, peer_caps.encode_codecs
        ),
    );
    let peer_discovery = peer_caps.transport & 8 != 0;
    let fragments = crate::call::fragments::negotiated(crate::call::fragments::local_enabled(), peer_caps.transport, relay_hops);
    trace("fragments", &format!("negotiated={fragments}"));

    let labels = crate::labels::enabled() && peer_caps.transport & crate::labels::CAPABILITY_BIT != 0;
    let call_link = CallLink::continue_link_with(
        hop_out,
        hop_in,
        endpoint.keys().send_key(),
        endpoint.keys().receive_key(),
        fragments,
        peer_discovery,
        labels,
    )
    .map_err(at(Stage::Call))?;
    let (header, video_plan) = crate::call_video::header_for(peer_caps.decode_codecs, &encoders());
    trace(
        "video_plan",
        &format!("codec={} encoders={}", header.codec, video_plan.len()),
    );
    let start = Instant::now();
    let mut session = CallSession::<opus::Decoder>::outgoing(
        call_link,
        call_id,
        voice,
        relay_hops,
        header,
        |c| opus::Decoder::new(c, opus::Profile::StreamOrCall),
        0,
    )
    .map_err(at(Stage::Call))?;
    session.check_header_codecs(crate::video::decode_mask(), peer_caps.encode_codecs);
    let own_spki = pseudonym.public_key().map_err(at(Stage::Call))?;
    let real_spki = real.public_key().map_err(at(Stage::Call))?;
    if labels {
        // Labels are signed by the key the endpoint handshake authenticated.
        session.accept_labels(*peer.node_id());
        if who.is_some() { if let Some(container) = crate::labels::own_call_container(&real, &call_id) { session.send_labels_first(container); } }
    }
    if opening_required(version5, endpoint_caps.transport, peer_caps.transport, peer_pseudonymous) {
        // CF-DISC-22: a real-key callee's first end-to-end record must be
        // its OPENING of the selected v5 offer (as on the contact path).
        session.require_opening(crate::call_session::OpeningGate { selected: rdi, node_id: peer_node, wall_epoch: wall });
        trace("opening_required", "first_record=bdx1_opening");
    }
    if who.is_none() { session.require_reveal(RevealGate {
        session: RevealSession::new(
            ROLE_CALLER,
            endpoint.keys().exporter(),
            own_spki,
            *peer.public_key(),
            true,
            peer.extensions().pseudonym,
            vec![own_spki, real_spki],
        )
        .map_err(at(Stage::Call))?,
        selected: rdi,
        rotating: version5,
        real: Box::new(real),
        wall_epoch: wall,
    }); }
    call_preset(&mut central, voice, &mut *trace);
    let paired = central.paired() == Some(true);
    let peer_real = (!peer_pseudonymous).then_some((peer_node, peer_fingerprint));
    Ok(Dialed {
        paired,
        header,
        video_plan,
        link: Link::Central(central),
        buffer,
        session,
        start,
        peer_discovery,
        peer_real,
    })
}

/// CF-DISC-22 on the stranger path: the caller requires the callee's BDX1
/// OPENING as its first end-to-end record when it selected a v5 (rotating)
/// offer, both endpoint hellos carry bit 3 and the callee met it with its
/// real key (a pseudonymous callee proves the RDI by its REVEAL instead).
pub fn opening_required(version5: bool, own_transport: u8, peer_transport: u8, peer_pseudonymous: bool) -> bool {
    version5 && own_transport & 8 != 0 && peer_transport & 8 != 0 && !peer_pseudonymous
}

/// Video calls ask Windows for ThroughputOptimized (15 ms, 2 s timeout);
/// voice calls keep the Balanced preset set at connect (4 s timeout).
fn call_preset(central: &mut Central, voice: bool, trace: &mut dyn FnMut(&str, &str)) {
    if !voice {
        central.request_preset(true, "video_call", trace);
    }
}

/// A recognised contact to call by its rotating ID (Phase 2): the contact's
/// node ID and the day key of the sighted epoch from the lease it gave this
/// PC (`presence::Vault::lease_for`).
pub struct ContactDial {
    pub contact: [u8; 16],
    pub lease_key: [u8; 32],
}

/// Calls a recognised contact over a direct link to its sighted rotating
/// call offer `rdi`: BMH3 R_FIRST at both layers with the contact tag
/// (`HKDF(L_d)` bound to the RDI, `expectedPeer` = the contact) and this PC's
/// real `identity`; the answerer must be that contact with its real key and
/// open the RDI (BMH3 opening, BDO1), and its first end-to-end record must be
/// its OPENING (RV15). No stranger keys and no REVEAL exchange.
pub fn dial_contact(
    route: &Route,
    rdi: [u8; 6],
    who: &ContactDial,
    identity: &Identity,
    call_id: [u8; 16],
    voice: bool,
    cancelled: Arc<AtomicBool>,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Dialed, Error> {
    use crate::{bmh3::handshake::ContactTag, verify_scan};
    let wall = wall_epoch().ok_or(fail(Stage::Connect, "clock before 1970"))?;
    let mut central =
        Central::connect_traced(route, cancelled.clone(), &mut *trace).map_err(at(Stage::Connect))?;
    trace("gatt_connected", "service=c8d5a105 cccd=notify");
    let tag = || -> Result<ContactTag, rotating::Error> {
        let key = rotating::contact_tag_key(&who.lease_key)?;
        Ok(ContactTag::new(*key, rotating::advert_binding(&rdi, rotating::ROLE_CALL_OFFER, None), who.contact))
    };
    let real_answer = |peer: &Peer| -> Result<(), bmh3::Error> {
        if peer.extensions().pseudonym || *peer.node_id() != who.contact {
            return Err(bmh3::Error::Policy);
        }
        Ok(())
    };
    let target = verify_scan::Target {
        rdi,
        tag: &tag,
        extra: &real_answer,
    };
    let reached = verify_scan::tagged_handshake(&mut central, &target, identity, wall, &cancelled, &mut *trace)?;
    let peer_discovery = reached.negotiated;
    trace("endpoint_done", &format!("contact=1 negotiated={peer_discovery}"));
    let (header, video_plan) = crate::call_video::header_for(reached.peer_capabilities.decode_codecs, &encoders());
    let start = Instant::now();
    let mut session = CallSession::<opus::Decoder>::outgoing(
        reached.link,
        call_id,
        voice,
        0,
        header,
        |c| opus::Decoder::new(c, opus::Profile::StreamOrCall),
        0,
    )
    .map_err(at(Stage::Call))?;
    session.check_header_codecs(crate::video::decode_mask(), reached.peer_capabilities.encode_codecs);
    if crate::labels::enabled() && reached.peer_capabilities.transport & crate::labels::CAPABILITY_BIT != 0 {
        session.accept_labels(reached.peer_node);
        // A verified contact: this PC's name goes right before the Invite.
        if let Some(container) = crate::labels::own_call_container(identity, &call_id) {
            session.send_labels_first(container);
            trace("labels_sent", "kinds=[1] position=before_invite");
        }
    }
    if peer_discovery {
        session.require_opening(crate::call_session::OpeningGate {
            selected: rdi,
            node_id: reached.peer_node,
            wall_epoch: wall,
        });
    }
    call_preset(&mut central, voice, &mut *trace);
    let paired = central.paired() == Some(true);
    Ok(Dialed {
        paired,
        header,
        video_plan,
        link: Link::Central(central),
        peer_real: Some((reached.peer_node, reached.peer_fingerprint)),
        buffer: reached.buffer,
        session,
        start,
        peer_discovery,
    })
}

#[cfg(test)]
mod relay_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Announcement;

    #[test]
    fn a_stranger_dial_requires_the_opening_only_from_a_real_key_v5_callee_with_both_bits() {
        assert!(opening_required(true, 0x0c, 0x08, false));
        assert!(!opening_required(false, 0x0c, 0x08, false), "a v4 offer has no rotating ID to open");
        assert!(!opening_required(true, 0x04, 0x08, false), "this PC did not sign bit 3");
        assert!(!opening_required(true, 0x0c, 0x04, false), "the callee did not sign bit 3");
        assert!(!opening_required(true, 0x0c, 0x08, true), "a pseudonymous callee proves it by REVEAL");
    }

    fn vector(id: &str) -> serde_json::Value {
        let all: serde_json::Value =
            serde_json::from_str(include_str!("../tests/vectors/conformance/announcements.json")).unwrap();
        all["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone()
    }
    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn version_2_call_offers_are_dialled_direct_and_not_as_a_helper_with_bmh3_only_when_hinted() {
        // CF-DISC-13 / CF-HS-13 on the shared vectors: announcements-003 is a
        // GATT-primary v2 call offer (flags 0x49) without the hint.
        let plain = Announcement::decode(&bytes(vector("announcements-003")["expected"]["bytes"].as_str().unwrap()), true).unwrap();
        assert!(is_call_offer(&plain) && needs_bmh2(&plain), "no hint: dialled with BMH2");
        let mut hinted = plain.clone();
        hinted.summary |= BMH3_HINT;
        let hinted_read = Announcement::decode(&hinted.encode().unwrap(), true).unwrap();
        assert!(is_call_offer(&hinted_read) && !needs_bmh2(&hinted_read));
        let mut relayed = hinted.clone();
        relayed.cost = 1;
        assert!(!is_call_offer(&relayed));
        let mut helper = hinted.clone();
        helper.flags |= crate::discovery::RELAY;
        assert!(!is_call_offer(&helper));
        // A version 5 offer needs no hint (announcements-004 is CoC: not GATT, not dialled).
        let v5 = Announcement::decode(&bytes(vector("announcements-004")["expected"]["bytes"].as_str().unwrap()), true).unwrap();
        assert!(!is_call_offer(&v5));
        let mut v5_gatt = v5.clone();
        v5_gatt.flags |= crate::discovery::GATT;
        assert!(is_call_offer(&v5_gatt));
        // CF-DISC-04/39: a relayed v5 mirror and a stream flag are not direct call offers.
        let mut relayed_v5 = v5_gatt.clone();
        relayed_v5.cost = 1;
        assert!(!is_call_offer(&relayed_v5));
        let mut stream = v5_gatt.clone();
        stream.flags |= crate::discovery::STREAM;
        assert!(!is_call_offer(&stream));
        // CF-PLAT-05: a stale pairing is not retried and says what to do.
        let stale = Error { stage: Stage::Connect, detail: "BondMismatch".into() };
        assert_eq!(retry_plan(&stale, std::time::Duration::from_secs(1), false), None);
        assert!(setup_failure_text(Some(&stale)).contains("Remove the pairing"));
        // CF-LIFE-13: Bluetooth off on this PC is not retried and says so.
        let off = at::<crate::bluetooth::Error>(Stage::Connect)(crate::bluetooth::Error::Radio(crate::bluetooth::Radio::Off));
        assert_eq!(retry_plan(&off, std::time::Duration::from_secs(1), false), None);
        assert!(setup_failure_text(Some(&off)).contains("Bluetooth is off on this PC"));
        // CF-HS-56: a phone that never answers the hop handshake is named.
        let silent = fail(Stage::Link, "no msg2");
        assert!(setup_failure_text(Some(&silent)).contains("their phone didn't answer in time"));
        assert_eq!(HOP_HANDSHAKE_SECONDS, 10);
    }
}
