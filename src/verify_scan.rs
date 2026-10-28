//! The BVMID2 scanner (PROTOCOL "Verification code BVMID2", "Scanner") with
//! Design QL (presence leases inside the verification session), from a typed
//! or pasted code: this PC never shows a code (it cannot advertise).
//!
//! Flow: search 10 s for version 5 call offers carrying the code's RDI and
//! try every address; BMH3 R_FIRST at both layers with the code's `K_tag`
//! tag (bound to the RDI, role 0, no wall epoch) and `expectedPeer` = the
//! code's node ID; this PC presents its real key only after the answerer's
//! block proved the code's fingerprint (a PSEUDONYM answer is refused) and
//! opened the RDI; the link opening `BDO1` when both link hellos carry bit
//! 3. The completed end-to-end handshake is the verification event. The
//! session is then scoped (`bvmid2::Scope`): no Invite, no labels, no call.
//!
//! Design QL on the scanner: nothing is offered until this user answered "Is
//! this X's screen you scanned?" with yes AND the shield was stored; the
//! scanner offers first; an OFFER from the displayer before that is only
//! recorded. The scanner never arms the RV15 OPENING wait (the code's
//! fingerprint, the tag bound to its RDI and route cost 0 already bind the
//! answer). Eligibility is re-checked at send time and the delivery is
//! stored durably before the lease leaves (`presence::Vault::deliver_lease`).
//! Ends: "No", a cancelled dialog or a failed shield write at once; no yes
//! within 20 s; 2 s after the exchange settled; always at 60 s.
use crate::{
    bluetooth::{carrier, central::Central},
    bmh2::{Capabilities, SigningIdentity},
    bmh3::{
        self, Purpose,
        handshake::{Config, ContactTag, InitiatorOptions, InitiatorProgress, Peer, Waiting2},
    },
    bvmid2::{self, Code, Scope},
    call::{CallLink, Incoming},
    contacts::{ContactStore, Fingerprint, Method, Verify},
    dial::{Error, Stage},
    framing::{HandshakeReader, HopReader, frame_handshake},
    presence::{self, Action, PeerState, Record, Vault},
    records::{Layer, Policy, Sealer},
    rotating::{self, ROLE_CALL_OFFER, Selection},
};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn fail(stage: Stage, detail: &str) -> Error {
    Error {
        stage,
        detail: detail.into(),
    }
}
fn at<E: std::fmt::Debug>(stage: Stage) -> impl Fn(E) -> Error {
    move |e| Error {
        stage,
        detail: format!("{e:?}"),
    }
}

/// A byte pipe to the displayer (the GATT carrier, or an in-memory pipe in tests).
pub trait Carrier {
    fn send(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), String>;
    /// Appends what arrived within `wait`; Ok(false) when nothing did.
    fn receive(&mut self, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, String>;
}
impl Carrier for Central {
    fn send(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), String> {
        self.write(bytes, deadline).map_err(|e| format!("{e:?}"))
    }
    fn receive(&mut self, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, String> {
        let mut chunk = [0u8; 4096];
        match self.read(&mut chunk, Instant::now() + wait) {
            Ok(0) => Err("link closed".into()),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(carrier::Error::Timeout) => Ok(false),
            Err(e) => Err(format!("{e:?}")),
        }
    }
}

/// The same carrier over a link a phone opened to this PC (Design QW).
impl Carrier for crate::bluetooth::peripheral::Peripheral {
    fn send(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), String> {
        self.write(bytes, deadline).map_err(|e| format!("{e:?}"))
    }
    fn receive(&mut self, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, String> {
        let mut chunk = [0u8; 4096];
        match self.read(&mut chunk, Instant::now() + wait) {
            Ok(0) => Err("link closed".into()),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(carrier::Error::Timeout) => Ok(false),
            Err(e) => Err(format!("{e:?}")),
        }
    }
}

/// Link and endpoint hello capabilities of the scanner (the call values;
/// transport bit 3 announces BDO1/BDX1).
pub const CAPABILITIES: Capabilities = Capabilities {
    transport: 0x0c,
    encode_codecs: 0x03,
    decode_codecs: 0x03,
    maximum_bitrate_kbps: 1100,
    maximum_children: 0,
};
pub const HANDSHAKE_SECONDS: u64 = 15;

/// A direct, tagged handshake target: the selected advert's RDI, the tag
/// toward its owner (with `expectedPeer`) and the extra answerer checks.
pub struct Target<'a> {
    pub rdi: [u8; 6],
    pub tag: &'a dyn Fn() -> Result<ContactTag, rotating::Error>,
    pub extra: &'a dyn Fn(&Peer) -> Result<(), bmh3::Error>,
}

/// The checks before this PC presents its real key, at both layers: the
/// target's own checks, then the answerer's opening must open the RDI.
fn check(target: &Target, peer: &Peer, wall_epoch: u32) -> Result<(), bmh3::Error> {
    (target.extra)(peer)?;
    // CF-HS-50 before this PC presents its real key.
    crate::identity_gate::admit(peer.node_id(), peer.fingerprint()).map_err(|_| bmh3::Error::Policy)?;
    let entries = peer
        .extensions()
        .opening
        .as_deref()
        .map(rotating::decode_body)
        .transpose()
        .map_err(|_| bmh3::Error::Policy)?;
    match rotating::check_selection(&target.rdi, true, ROLE_CALL_OFFER, peer.node_id(), entries.as_deref(), wall_epoch) {
        Ok(Selection::Pass) => Ok(()),
        _ => Err(bmh3::Error::Policy),
    }
}

/// A verified answer: both handshakes completed with the code's key.
pub struct Reached {
    pub link: CallLink,
    pub peer_fingerprint: Fingerprint,
    pub peer_node: [u8; 16],
    /// Both endpoint hellos carried bit 3 (BDX1 allowed).
    pub negotiated: bool,
    /// The answerer's endpoint hello capabilities.
    pub peer_capabilities: Capabilities,
    /// Bytes received after the handshake, not yet read.
    pub buffer: Vec<u8>,
}

fn pull<C: Carrier>(carrier: &mut C, buffer: &mut Vec<u8>, stage: Stage, deadline: Instant, cancelled: &AtomicBool) -> Result<(), Error> {
    if Instant::now() >= deadline {
        return Err(fail(stage, "timeout"));
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(fail(stage, "cancelled"));
    }
    carrier
        .receive(buffer, Duration::from_millis(50))
        .map(|_| ())
        .map_err(|e| fail(stage, &e))
}

/// Both BMH3 handshakes toward the phone showing `code`, with this PC's real
/// `identity`. `trace` gets phases without keys or addresses.
pub fn handshake<C: Carrier>(
    carrier: &mut C,
    code: &Code,
    identity: &dyn SigningIdentity,
    wall_epoch: u32,
    cancelled: &AtomicBool,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Reached, Error> {
    let target = Target {
        rdi: code.rdi,
        tag: &|| bvmid2::tag(code),
        extra: &|peer| bvmid2::check_responder(code, peer),
    };
    tagged_handshake(carrier, &target, identity, wall_epoch, cancelled, trace)
}

/// Both BMH3 handshakes toward the owner of `target.rdi` with this PC's real
/// `identity` under the target's tag (a verification code or a contact's
/// lease), BDO1 when both link hellos carry bit 3, then the call link.
pub fn tagged_handshake<C: Carrier>(
    carrier: &mut C,
    target: &Target,
    identity: &dyn SigningIdentity,
    wall_epoch: u32,
    cancelled: &AtomicBool,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Reached, Error> {
    // CF-CALL-06/08: the link handshake within 10 s, both within 18 s.
    let handshake_started = Instant::now();
    let deadline = handshake_started + Duration::from_secs(crate::dial::HOP_HANDSHAKE_SECONDS);
    let options = || -> Result<InitiatorOptions, Error> {
        Ok(InitiatorOptions {
            tag: Some((target.tag)().map_err(at(Stage::Link))?),
            ..InitiatorOptions::default()
        })
    };
    // Link layer.
    let config = Config::new(Purpose::Link, identity, crate::video::hello_codecs(CAPABILITIES, false)).map_err(at(Stage::Link))?;
    let (waiting, m1) = Waiting2::start(config, options()?).map_err(at(Stage::Link))?;
    carrier
        .send(&frame_handshake(&m1).map_err(at(Stage::Link))?, deadline)
        .map_err(|e| fail(Stage::Link, &e))?;
    trace("qr_link_msg1", "");
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
        pull(carrier, &mut buffer, Stage::Link, deadline, cancelled)?;
    };
    let (progress, m3) = waiting
        .receive(&m2, |peer| check(target, peer, wall_epoch))
        .map_err(at(Stage::Link))?;
    let InitiatorProgress::Complete(link) = progress else {
        return Err(fail(Stage::Link, "responder demanded I_FIRST"));
    };
    carrier
        .send(&frame_handshake(&m3).map_err(at(Stage::Link))?, deadline)
        .map_err(|e| fail(Stage::Link, &e))?;
    let link_peer = *link.peer().fingerprint();
    // This PC presents its real key; a pseudonymous answer never passes
    // `check`, and would skip the opening anyway (Android `skipsLinkOpening`).
    let opening = rotating::link_opening_exchanged(
        true,
        CAPABILITIES.transport,
        link.peer().capabilities().transport,
        link.own_pseudonym(),
        link.peer().extensions().pseudonym,
    );
    trace("qr_link_done", &format!("bdo1={opening}"));
    let deadline = handshake_started + Duration::from_secs(crate::dial::CALL_HANDSHAKE_SECONDS);

    // BDO1 link opening: this side opens nothing (it advertised nothing), the
    // displayer's must open the code's RDI.
    let policy = Policy {
        require_opening: opening,
        ..Policy::default()
    };
    let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), policy).map_err(at(Stage::Link))?;
    let mut hop_in = HopReader::new(link.keys().receive_key(), policy).map_err(at(Stage::Link))?;
    let read_hop = |carrier: &mut C, buffer: &mut Vec<u8>, hop_in: &mut HopReader, stage: Stage| loop {
        let mut input = buffer.as_slice();
        let record = hop_in.read(&mut input, 4096).map_err(at(stage))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(r) = record {
            return Ok::<_, Error>(r);
        }
        pull(carrier, buffer, stage, deadline, cancelled)?;
    };
    if opening {
        let own = rotating::encode_bdo1(&[]).map_err(at(Stage::Link))?;
        carrier
            .send(&hop_out.seal(13, 0, &own).map_err(at(Stage::Link))?, deadline)
            .map_err(|e| fail(Stage::Link, &e))?;
        let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Link)?;
        if record.header.kind != 13 {
            return Err(fail(Stage::Link, "expected the discovery opening"));
        }
        let entries = rotating::decode_bdo1(&record.plaintext).map_err(at(Stage::Link))?;
        let node: [u8; 16] = link_peer[..16].try_into().unwrap();
        match rotating::check_selection(&target.rdi, true, ROLE_CALL_OFFER, &node, Some(&entries), wall_epoch) {
            Ok(Selection::Pass) => {}
            other => return Err(fail(Stage::Link, &format!("opening {other:?}"))),
        }
        trace("qr_bdo1", "pass");
    }

    // End-to-end layer in BMS1 type-4 records.
    // A contact call goes through here too: offer call video fragments (bit 7).
    let endpoint_caps = crate::video::hello_codecs(Capabilities {
        transport: crate::call::fragments::endpoint_transport(CAPABILITIES.transport) | if crate::labels::enabled() { crate::labels::CAPABILITY_BIT } else { 0 },
        // The decoders this PC really has (CF-HS-02), not a constant; Design
        // HC's masks (with bit 4) through `hello_codecs`.
        decode_codecs: crate::video::decode_mask(),
        ..CAPABILITIES
    }, true);
    let config = Config::new(Purpose::Endpoint, identity, endpoint_caps).map_err(at(Stage::Endpoint))?;
    let (waiting, m1) = Waiting2::start(config, options()?).map_err(at(Stage::Endpoint))?;
    carrier
        .send(&hop_out.seal(4, 0, &m1).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(|e| fail(Stage::Endpoint, &e))?;
    let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Endpoint)?;
    if record.header.kind != 4 {
        return Err(fail(Stage::Endpoint, "unexpected hop record type"));
    }
    let (progress, m3) = waiting
        .receive(&record.plaintext, |peer| {
            if *peer.fingerprint() != link_peer {
                return Err(bmh3::Error::Policy);
            }
            check(target, peer, wall_epoch)
        })
        .map_err(at(Stage::Endpoint))?;
    let InitiatorProgress::Complete(endpoint) = progress else {
        return Err(fail(Stage::Endpoint, "end-to-end I_FIRST"));
    };
    carrier
        .send(&hop_out.seal(4, 0, &m3).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(|e| fail(Stage::Endpoint, &e))?;
    let peer = endpoint.peer();
    let negotiated = peer.capabilities().transport & 8 != 0 && CAPABILITIES.transport & 8 != 0;
    let fragments = crate::call::fragments::negotiated(crate::call::fragments::local_enabled(), peer.capabilities().transport, 0);
    trace("qr_endpoint_done", &format!("negotiated={negotiated} fragments={fragments}"));
    // LABELS are opened (the verification scope allows a name label; a
    // contact call has no label support on this PC and never negotiates it).
    let link = CallLink::continue_link_with(
        hop_out,
        hop_in,
        endpoint.keys().send_key(),
        endpoint.keys().receive_key(),
        fragments,
        negotiated,
        true,
    )
    .map_err(at(Stage::Call))?;
    Ok(Reached {
        link,
        peer_fingerprint: *peer.fingerprint(),
        peer_node: *peer.node_id(),
        negotiated,
        peer_capabilities: peer.capabilities(),
        buffer,
    })
}

/// A BMH2 call link toward a version 2 offer without the BMH3 hint (a
/// default iPhone/Mac, or a phone with BMH3 off): hellos in 4-byte frames
/// (initiator first), BDO1 when both link hellos carry bit 3, end-to-end
/// hellos in hop type-4 records. BMH2 has no stranger mode: this PC's real
/// key is in its first hello. The answerer's node must start with the 6
/// announced bytes (version 2 selection), the link and end-to-end peer must
/// be the same key (direct call, CF-DISC-21), and the identity gate runs.
pub fn bmh2_handshake<C: Carrier, I: SigningIdentity>(
    carrier: &mut C,
    prefix: [u8; 6],
    identity: &I,
    cancelled: &AtomicBool,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Reached, Error> {
    use crate::bmh2::{AuthenticatedPeer, Hello, Pending, Purpose as P2, Role};
    let handshake_started = Instant::now();
    let deadline = handshake_started + Duration::from_secs(crate::dial::HOP_HANDSHAKE_SECONDS);
    let accept = |peer: &AuthenticatedPeer| -> Result<(), crate::bmh2::Error> {
        if peer.node_id[..6] != prefix || crate::identity_gate::admit(&peer.node_id, &peer.fingerprint).is_err() {
            return Err(crate::bmh2::Error::Format);
        }
        Ok(())
    };
    let pending = Pending::start(identity, Role::Initiator, crate::video::hello_codecs(CAPABILITIES, false), P2::Link).map_err(at(Stage::Link))?;
    carrier
        .send(&frame_handshake(&pending.hello().encode()).map_err(at(Stage::Link))?, deadline)
        .map_err(|e| fail(Stage::Link, &e))?;
    trace("bmh2_link_hello", "");
    let mut buffer = Vec::new();
    let mut frames = HandshakeReader::default();
    let remote = loop {
        let mut input = buffer.as_slice();
        let got = frames.read(&mut input).map_err(at(Stage::Link))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(m) = got {
            break Hello::decode(&m).map_err(at(Stage::Link))?;
        }
        pull(carrier, &mut buffer, Stage::Link, deadline, cancelled)?;
    };
    let link = pending.finish(&remote, &accept).map_err(at(Stage::Link))?;
    let link_peer = link.peer().fingerprint;
    // BMH2 has no pseudonyms: both hellos decide.
    let opening = rotating::link_opening_exchanged(true, CAPABILITIES.transport, link.peer().capabilities.transport, false, false);
    trace("bmh2_link_done", &format!("bdo1={opening}"));
    let deadline = handshake_started + Duration::from_secs(crate::dial::CALL_HANDSHAKE_SECONDS);
    let policy = Policy {
        require_opening: opening,
        ..Policy::default()
    };
    let mut hop_out = Sealer::new(Layer::Hop, link.send_key(), policy).map_err(at(Stage::Link))?;
    let mut hop_in = HopReader::new(link.receive_key(), policy).map_err(at(Stage::Link))?;
    let read_hop = |carrier: &mut C, buffer: &mut Vec<u8>, hop_in: &mut HopReader, stage: Stage| loop {
        let mut input = buffer.as_slice();
        let record = hop_in.read(&mut input, 4096).map_err(at(stage))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(r) = record {
            return Ok::<_, Error>(r);
        }
        pull(carrier, buffer, stage, deadline, cancelled)?;
    };
    if opening {
        let own = rotating::encode_bdo1(&[]).map_err(at(Stage::Link))?;
        carrier
            .send(&hop_out.seal(13, 0, &own).map_err(at(Stage::Link))?, deadline)
            .map_err(|e| fail(Stage::Link, &e))?;
        let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Link)?;
        if record.header.kind != 13 {
            return Err(fail(Stage::Link, "expected the discovery opening"));
        }
        rotating::decode_bdo1(&record.plaintext).map_err(at(Stage::Link))?;
    }
    // The BMH2 end-to-end hello (phones with BMH3 off): before Design HC's
    // fix it always said 0x03, while the header could name HEVC.
    let pending = Pending::start(identity, Role::Initiator, crate::video::hello_codecs(CAPABILITIES, true), P2::Endpoint).map_err(at(Stage::Endpoint))?;
    carrier
        .send(&hop_out.seal(4, 0, &pending.hello().encode()).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(|e| fail(Stage::Endpoint, &e))?;
    let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Endpoint)?;
    if record.header.kind != 4 {
        return Err(fail(Stage::Endpoint, "unexpected hop record type"));
    }
    let remote = Hello::decode(&record.plaintext).map_err(at(Stage::Endpoint))?;
    let endpoint = pending
        .finish(&remote, |peer| {
            if peer.fingerprint != link_peer {
                return Err(crate::bmh2::Error::Format);
            }
            accept(peer)
        })
        .map_err(at(Stage::Endpoint))?;
    let peer = endpoint.peer().clone();
    let negotiated = peer.capabilities.transport & 8 != 0 && CAPABILITIES.transport & 8 != 0;
    trace("bmh2_endpoint_done", &format!("negotiated={negotiated}"));
    let link = CallLink::continue_link(hop_out, hop_in, endpoint.send_key(), endpoint.receive_key(), false, negotiated)
        .map_err(at(Stage::Call))?;
    Ok(Reached {
        link,
        peer_fingerprint: peer.fingerprint,
        peer_node: peer.node_id,
        negotiated,
        peer_capabilities: peer.capabilities,
        buffer,
    })
}

/// What the scanner's presence exchange needs from the stores. Every call
/// is a fresh decision (eligibility at start and again at each send).
pub trait PresenceHooks {
    /// Why no OFFER may be sent now, or None.
    fn ineligible(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> Option<&'static str>;
    /// The lease to send, after its delivery was stored durably; None withholds it.
    fn deliver_lease(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> Option<presence::Lease>;
    /// The peer is a verified contact right now.
    fn verified_now(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> bool;
    /// Stores the peer's lease; true when it changed the store.
    fn store_lease(&mut self, peer_node: &[u8; 16], lease: &presence::Lease) -> bool;
}

/// Why a scan session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Declined,
    NoConfirmation,
    Finished,
    Limit,
    ScopeViolation,
    LinkError,
    /// The displayer closed the link after the verification (Android closes
    /// 2 s after the commit when it has no lease to give): a normal end.
    PeerClosed,
}

/// The scoped session after a verified answer.
pub struct ScanSession {
    link: CallLink,
    pub peer_fingerprint: Fingerprint,
    pub peer_node: [u8; 16],
    negotiated: bool,
    scope: Scope,
    presence: Option<presence::Session>,
    reached_ms: u64,
    lease_started: bool,
    close_at: Option<(u64, End)>,
    ended: Option<End>,
    lease_stored: bool,
    outbox: VecDeque<Vec<u8>>,
    last_heartbeat_ms: u64,
    /// This PC showed the code (the displayer side): it never offers at the
    /// commit and starts the exchange only at the scanner's OFFER.
    displayer: bool,
    pub log: Vec<String>,
}
impl ScanSession {
    /// The displayer side of a session a phone opened by scanning this PC's
    /// code (committed by this connection). `answering`: this PC's user
    /// confirmed this scanner (yes, shield stored) during the current visit
    /// of Add contact and no reuse alert fired; then the scanner's OFFER is
    /// answered with OFFER and LEASE (waiting at most 20 s for it).
    /// Otherwise the session ends 2 s after the commit and no lease moves.
    pub fn displayer(reached: Reached, now_ms: u64, answering: bool) -> (Self, Vec<u8>) {
        let (mut session, buffer) = Self::new(reached, now_ms);
        session.displayer = true;
        if !answering {
            session.presence = None;
        }
        session.log.push(format!("displayer answering={answering} negotiated={}", session.negotiated));
        if session.presence.is_none() {
            session.close_after_grace(now_ms);
        }
        (session, buffer)
    }
    pub fn new(reached: Reached, now_ms: u64) -> (Self, Vec<u8>) {
        let session = Self {
            link: reached.link,
            peer_fingerprint: reached.peer_fingerprint,
            peer_node: reached.peer_node,
            negotiated: reached.negotiated,
            scope: Scope::new(now_ms),
            // Design QL: eligibility is decided when this side starts, not now.
            presence: reached.negotiated.then(|| presence::Session::new(true)),
            reached_ms: now_ms,
            lease_started: false,
            close_at: None,
            ended: None,
            lease_stored: false,
            outbox: VecDeque::new(),
            last_heartbeat_ms: now_ms,
            displayer: false,
            log: Vec::new(),
        };
        (session, reached.buffer)
    }
    pub fn ended(&self) -> Option<End> {
        self.ended
    }
    /// A lease from the displayer was stored in this session.
    pub fn lease_stored(&self) -> bool {
        self.lease_stored
    }
    pub fn violations(&self) -> u32 {
        self.scope.violations()
    }
    fn end(&mut self, why: End) {
        if self.ended.is_none() {
            self.ended = Some(why);
            self.outbox.clear();
        }
    }
    /// The carrier closed. A session exists only after both handshakes with
    /// the code's key (the verification event), so a close is the
    /// displayer's normal end: Android closes 2 s after the commit when it
    /// does not take part in the lease exchange (measured on three phones,
    /// win-userflow-20261019: the PC's next write failed 4 s after its OFFER).
    pub fn link_lost(&mut self) {
        let why = self.close_at.map_or(End::PeerClosed, |(_, why)| why);
        self.end(why);
    }
    fn close_after_grace(&mut self, now_ms: u64) {
        if self.close_at.is_none() {
            self.close_at = Some((now_ms + Scope::CLOSE_GRACE_MS, End::Finished));
        }
    }
    /// Records waiting to be written, in order.
    pub fn take_writes(&mut self) -> Vec<Vec<u8>> {
        self.outbox.drain(..).collect()
    }
    /// Reads what arrived; a scope violation or a bad record ends the session.
    pub fn on_input(&mut self, input: &mut &[u8], now_ms: u64, hooks: &mut dyn PresenceHooks) {
        while self.ended.is_none() && !input.is_empty() {
            let incoming = match self.link.read(input) {
                Ok(Some(i)) => i,
                Ok(None) => return,
                Err(e) => {
                    self.log.push(format!("record_error {e:?}"));
                    self.end(End::LinkError);
                    return;
                }
            };
            match incoming {
                Incoming::Heartbeat(_) => {}
                // Allowed by the scope (Android buffers it until an Invite that
                // never comes in a verification session): read and dropped.
                Incoming::Labels(_) => self.log.push("labels ignored".into()),
                Incoming::Discovery(payload) => {
                    if self.scope.expired(now_ms) || self.scope.refuse_discovery(&payload, self.negotiated) {
                        self.log.push("qr_session violation=discovery".into());
                        self.end(End::ScopeViolation);
                        return;
                    }
                    let Some(record) = Record::decode(&payload) else {
                        self.log.push("lease_rejected reason=malformed".into());
                        continue;
                    };
                    let verified = hooks.verified_now(&self.peer_node, &self.peer_fingerprint);
                    let mut actions = self
                        .presence
                        .as_mut()
                        .map_or_else(Vec::new, |p| p.on_record(&record, verified));
                    // The displayer starts at the scanner's OFFER (never first).
                    if self.displayer && !self.lease_started && matches!(record, Record::Offer) {
                        if let Some(p) = self.presence.as_mut() {
                            self.lease_started = true;
                            self.log.push("offer_received start=answer".into());
                            actions.extend(p.start());
                        }
                    }
                    self.run(actions, now_ms, hooks);
                }
                // Invite and every other call signal, headers, media, key-frame requests.
                Incoming::Signal(_) | Incoming::Header(_) | Incoming::Media { .. } | Incoming::KeyFrameRequest => {
                    self.scope.refuse_content(0);
                    self.log.push("qr_session violation=content".into());
                    self.end(End::ScopeViolation);
                    return;
                }
            }
        }
    }
    /// The user's answer to "Is this X's screen you scanned?". `shield_stored`:
    /// the verified contact was written. Only yes with a stored shield starts
    /// the exchange; anything else ends the session at once.
    pub fn confirm(&mut self, yes: bool, shield_stored: bool, now_ms: u64, hooks: &mut dyn PresenceHooks) {
        if self.ended.is_some() {
            return;
        }
        if !yes || !shield_stored {
            self.end(End::Declined);
            return;
        }
        if self.presence.is_none() {
            self.close_after_grace(now_ms);
            return;
        }
        if self.lease_started {
            return;
        }
        self.lease_started = true;
        let actions = self.presence.as_mut().map_or_else(Vec::new, presence::Session::start);
        self.run(actions, now_ms, hooks);
    }
    fn run(&mut self, actions: Vec<Action>, now_ms: u64, hooks: &mut dyn PresenceHooks) {
        for action in actions {
            match action {
                Action::SendOffer => match hooks.ineligible(&self.peer_node, &self.peer_fingerprint) {
                    None => match self.link.discovery(&presence::offer()) {
                        Ok(bytes) => {
                            self.outbox.push_back(bytes);
                            self.log.push("offer_sent".into());
                        }
                        Err(_) => self.end(End::LinkError),
                    },
                    Some(reason) => {
                        if let Some(p) = self.presence.as_mut() {
                            p.offer_withheld();
                        }
                        self.log.push(format!("offer_withheld reason={reason}"));
                    }
                },
                Action::SendLease => match hooks.deliver_lease(&self.peer_node, &self.peer_fingerprint) {
                    Some(lease) => match self.link.discovery(&lease.encode()) {
                        Ok(bytes) => {
                            self.outbox.push_back(bytes);
                            self.log.push(format!("lease_sent generation={}", lease.generation));
                        }
                        Err(_) => self.end(End::LinkError),
                    },
                    None => self.log.push("lease_withheld reason=not_deliverable".into()),
                },
                Action::StoreLease(lease) => {
                    let stored = hooks.store_lease(&self.peer_node, &lease);
                    self.lease_stored |= stored;
                    self.log.push(format!("lease_stored changed={stored} generation={}", lease.generation));
                }
                Action::Reject(reason) => self.log.push(format!("lease_rejected reason={reason}")),
            }
        }
        if self.presence.as_ref().is_none_or(presence::Session::settled) {
            self.close_after_grace(now_ms);
        }
    }
    /// Timers: heartbeats every 2 s (the displayer's link dies after 8 s of
    /// silence), the 20 s wait for the user's yes, the 2 s grace and the 60 s limit.
    pub fn poll(&mut self, now_ms: u64) -> Option<End> {
        if self.ended.is_none() {
            if self.scope.expired(now_ms) {
                self.end(End::Limit);
            } else if !self.lease_started
                && self.close_at.is_none()
                && now_ms.saturating_sub(self.reached_ms) >= Scope::CONFIRM_WAIT_MS
            {
                self.end(End::NoConfirmation);
            } else if let Some((at, why)) = self.close_at
                && now_ms >= at
            {
                self.end(why);
            } else if now_ms.saturating_sub(self.last_heartbeat_ms) >= 2_000 {
                self.last_heartbeat_ms = now_ms;
                match self.link.heartbeat(None) {
                    Ok(bytes) => self.outbox.push_back(bytes),
                    Err(_) => self.end(End::LinkError),
                }
            }
        }
        self.ended
    }
}

/// Searches `bvmid2::SEARCH_MS` for version 5 call offers carrying the code's
/// RDI at route cost 0 and tries every address in the order seen (a copy of
/// the advert may answer first and fail), connecting as for a call. The first
/// verified answer wins. `trace` gets phases without addresses or keys.
pub fn find_and_reach(
    code: &Code,
    identity: &dyn SigningIdentity,
    wall_epoch: u32,
    cancelled: std::sync::Arc<AtomicBool>,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<(Central, Reached), Error> {
    use crate::bluetooth::scan::{ScanOptions, Scanner};
    let start = Instant::now();
    let until = start + Duration::from_millis(bvmid2::SEARCH_MS);
    let mut tried: Vec<(u64, i32)> = Vec::new();
    let mut last = fail(Stage::Connect, "code not found nearby");
    trace("qr_search_start", "");
    while Instant::now() < until && !cancelled.load(Ordering::Acquire) {
        // Scan until an untried address with the code's RDI appears.
        let scanner = Scanner::start(ScanOptions { rotating: true, compact: false }).map_err(at(Stage::Connect))?;
        let route = loop {
            match scanner.receive(until, &cancelled) {
                Ok(s) => {
                    let key = (s.route.address, s.route.address_type.0);
                    let seen = [((), s.announcement.clone())];
                    if !bvmid2::addresses_to_try(code, &seen).is_empty() && !tried.contains(&key) {
                        tried.push(key);
                        break Some(s.route);
                    }
                }
                Err(crate::bluetooth::Error::Timeout) => break None,
                Err(e) => return Err(at(Stage::Connect)(e)),
            }
        };
        trace("stack_fact", &scanner.facts());
        drop(scanner);
        let Some(route) = route else { break };
        trace("qr_found", &format!("ms={} attempt={}", start.elapsed().as_millis(), tried.len()));
        let mut central = match Central::connect_traced(&route, cancelled.clone(), &mut *trace) {
            Ok(c) => c,
            Err(e) => {
                last = at(Stage::Connect)(e);
                continue;
            }
        };
        match handshake(&mut central, code, identity, wall_epoch, &cancelled, &mut *trace) {
            Ok(reached) => {
                trace("qr_reached", &format!("ms={}", start.elapsed().as_millis()));
                return Ok((central, reached));
            }
            Err(e) => {
                trace("qr_attempt_failed", &e.to_string());
                last = e;
            }
        }
    }
    Err(last)
}

/// Stores the shield after the user's yes: verified (scanned) only when the
/// handshake proved the code's fingerprint. True when stored.
pub fn store_shield(contacts: &mut ContactStore, session: &ScanSession, code: &Code, name: &str, now_ms: i64) -> bool {
    contacts.mark_verified(Some(&session.peer_fingerprint), &code.fingerprint, name, Method::QrScan, now_ms) == Verify::Verified
}

/// The production hooks over the verified contacts, the presence vault and
/// the contact claims ("scanned both ways").
pub struct StoreHooks<'a> {
    pub contacts: &'a mut ContactStore,
    pub vault: &'a mut Vault,
    pub claims: Option<&'a mut crate::claims::ClaimsStore>,
    /// The current epoch and wall-clock milliseconds.
    pub epoch: u32,
    pub now_ms: i64,
}
impl StoreHooks<'_> {
    fn peer_state(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> PeerState {
        match self.contacts.get(peer) {
            Ok(Some(_)) => PeerState::Verified,
            Ok(None) => match self.contacts.by_node_id(peer_node) {
                Ok(Some(_)) => PeerState::Mismatch,
                Ok(None) => PeerState::Unverified,
                Err(_) => PeerState::StoreUnreadable,
            },
            Err(_) => PeerState::StoreUnreadable,
        }
    }
}
impl PresenceHooks for StoreHooks<'_> {
    fn ineligible(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> Option<&'static str> {
        let persistent = self.vault.ensure_loaded(self.epoch);
        let state = self.peer_state(peer_node, peer);
        let method = self.contacts.get(peer).ok().flatten().map(|c| c.method);
        let both_ways = self.claims.as_mut().is_some_and(|c| c.scanned_both_ways(peer));
        let code_only = presence::code_only_blocked(method, both_ways, self.vault.contact(peer_node).is_some_and(|c| c.lease.is_some()));
        presence::ineligible_reason(true, persistent, Some(state), self.vault.visible_to_them(peer_node), code_only)
    }
    fn deliver_lease(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> Option<presence::Lease> {
        let verified = self.peer_state(peer_node, peer) == PeerState::Verified;
        self.vault.deliver_lease(peer_node, self.epoch, || verified)
    }
    fn verified_now(&mut self, peer_node: &[u8; 16], peer: &Fingerprint) -> bool {
        self.peer_state(peer_node, peer) == PeerState::Verified
    }
    fn store_lease(&mut self, peer_node: &[u8; 16], lease: &presence::Lease) -> bool {
        let stored = self.vault.store_lease(peer_node, lease, self.epoch, self.now_ms);
        // This PC scanned their code (shield) and they sent a lease, which a
        // phone sends only to a contact it verified itself: both ways.
        if let Ok(Some(contact)) = self.contacts.by_node_id(peer_node) {
            if contact.method == Method::QrScan {
                if let Some(claims) = self.claims.as_mut() {
                    claims.mark_scanned_both_ways(&contact.fingerprint, self.now_ms);
                }
            }
        }
        stored
    }
}

#[cfg(test)]
mod tests;
