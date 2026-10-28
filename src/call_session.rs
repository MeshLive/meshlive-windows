//! One private call, end to end over its link (voice or video, either role):
//! the controller's signals, the ordered call writer and both receive paths.
//! Writer order follows Android: controls (signals, header, keyframe
//! requests) first, then queued audio ahead of video
//! (`pollNextPrivateCallMedia`), and between the fragments of one video frame
//! only audio. Call-link pacing is an Android experiment that is off by default
//! and never used on GATT, so it is not applied. Each endpoint sends one
//! version 1 heartbeat after the handshake and then every 2 s; 8 s of silence
//! ends the call. Capture, encoders and output devices stay outside.
use crate::{
    bmh2::SigningIdentity,
    bmh3::reveal::RevealSession,
    call::{self, Action, CallLink, Controller, Incoming, MediaPlan, Message, Reason, Signal},
    link::Liveness,
    listen::{self, AudioReceiver, UnitDecoder},
    media::{self, AUDIO, VideoHeader},
    opus,
    playout::{Clock, Profile},
    rotating::{self, OpeningEntry, Selection},
    viewer::{self, VideoReceiver},
};
use std::collections::VecDeque;

/// About 4 s of 20 ms audio and 2 s of 30 FPS video: the sender's own backlog bound.
pub const MAX_QUEUED_AUDIO: usize = 200;
pub const MAX_QUEUED_VIDEO: usize = 60;
/// A call receiver repeats its keyframe request this often while waiting.
pub const KEY_REQUEST_INTERVAL_NS: i64 = 2_000_000_000;
/// Decoder recoveries per call before video stops (audio and the call stay).
pub const MAX_VIDEO_RECOVERIES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Call(call::Error),
    Audio(listen::Error),
    Video(viewer::Error),
    /// The sender's own queue is full (capture must slow down).
    Backlog,
    /// The peer was silent for 8 s.
    Dead,
    /// Media before the peer's stream header.
    NoHeader,
    /// The peer's identity reveal failed or did not open the selected offer.
    Reveal,
    /// RV15: the callee's first end-to-end record was not an OPENING that
    /// opens the selected rotating offer.
    Opening,
    /// CF-MEDIA-51: a second stream header in one call.
    SecondHeader,
    /// CF-MEDIA-51: the peer's codec is not in this PC's advertised decode
    /// mask or not in the peer's authenticated encode mask.
    Codec,
    /// CF-MEDIA-51: audio after a header that declared none.
    UndeclaredAudio,
}
impl From<call::Error> for Error {
    fn from(error: call::Error) -> Self {
        Self::Call(error)
    }
}

/// What the owner sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Ring {
        voice: bool,
    },
    /// Start capture; the camera only when `camera`.
    StartMedia {
        camera: bool,
    },
    PeerCamera(bool),
    PeerMicrophone(bool),
    PeerHeader(VideoHeader),
    KeyFrameRequested,
    /// The peer's display rotation in quarter turns (flag 4, CF-REC-21):
    /// Android sends one on every camera switch, also inside calls.
    PeerRotation(u8),
    Ended(Reason),
    /// The callee proved its real identity for the selected offer.
    PeerRevealed {
        node_id: [u8; 16],
        fingerprint: [u8; 32],
    },
    /// A BDX1 RECOGNITION_OFFER or PRESENCE_LEASE arrived: the owner's
    /// `CallPresence` takes it with `take_presence_records`.
    PresenceRecord,
    /// A verified peer label (CF-REC-43): kind 1 name, 3 call subject.
    PeerLabel { kind: u8, text: String },
}

/// Controls are sealed only when written, so wire order equals record sequence.
enum Control {
    Signal(Message),
    Header,
    KeyFrameRequest,
    Discovery(Vec<u8>),
    Labels(Vec<u8>),
}

/// Pseudonymous call (BMH3 Phase 2), caller side: the callee must send a
/// verified REVEAL that opens the selected rotating ID before its ACCEPT
/// counts; then the caller reveals itself (role 1, binding the callee record).
pub struct RevealGate {
    pub session: RevealSession,
    pub selected: [u8; 6],
    /// Version 5 (rotating) offers check the opening; version 2 the prefix.
    pub rotating: bool,
    pub real: Box<dyn SigningIdentity>,
    /// The verifier's wall-clock epoch for the two-epoch window.
    pub wall_epoch: u32,
}

/// Callee of a pseudonymous call (BMH3 Phase 2): this PC answered with its
/// throwaway; its REVEAL (real key, opening the role-0 entries the caller
/// selected) goes out right before its Accept, and the caller's REVEAL
/// (verified against ours) proves the caller's real identity.
pub struct CalleeReveal {
    pub session: RevealSession,
    pub real: Box<dyn SigningIdentity>,
    pub entries: Vec<OpeningEntry>,
    /// Relay hops for the caller-reveal deadline: 0 on a direct link (the
    /// authenticated link peer is the end-to-end peer) whatever the Invite
    /// claims; the Invite's count only when they differ.
    pub relay_hops: u8,
}

/// Caller-reveal deadline of a pseudonym<->pseudonym call (callee side):
/// 10 s plus 10 s per relay hop, at most 40 s, from the moment this PC's
/// REVEAL is written.
pub fn caller_reveal_budget_ns(relay_hops: u8) -> u64 {
    (10 + 10 * u64::from(relay_hops)).min(40) * 1_000_000_000
}

/// Callee of a pseudonym<->pseudonym call between its REVEAL and the
/// caller's verified REVEAL: caller media/header are dropped and this PC's
/// own header and capture are held (`held_start`).
struct CallerRevealWait {
    budget_ns: u64,
    /// Set when this PC's REVEAL is written.
    deadline: Option<u64>,
    /// A StartMedia held until the caller is confirmed.
    held_start: Option<bool>,
}

/// RV15, caller of a version 5 offer with a non-pseudonymous callee: the
/// callee's first end-to-end record must be a BDX1 OPENING whose entry
/// opens the selected RDI for the authenticated endpoint node.
pub struct OpeningGate {
    pub selected: [u8; 6],
    pub node_id: [u8; 16],
    pub wall_epoch: u32,
}

/// `MESHLIVE_CALL_LIVENESS_HANG_UP=0`: the control arm (no Hang-up/Error
/// before a liveness close).
pub fn liveness_hang_up() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_CALL_LIVENESS_HANG_UP").ok().as_deref() != Some("0"))
}

pub struct CallSession<D> {
    controller: Controller,
    media_gate: bool,
    ordered_fragments: bool,
    link: CallLink,
    header: VideoHeader,
    controls: VecDeque<Control>,
    audio_out: VecDeque<Vec<u8>>,
    video_out: VecDeque<Vec<u8>>,
    fragmenting: Option<(MediaPlan, usize)>,
    liveness: Liveness,
    clock: Clock,
    audio_in: AudioReceiver<D>,
    video_in: Option<VideoReceiver>,
    /// CF-MEDIA-51: (own advertised decode mask, peer's authenticated encode
    /// mask); None until the owner sets it (unit tests without hellos).
    codec_gate: Option<(u8, u8)>,
    /// The peer's header declared Opus audio.
    peer_opus: bool,
    make_decoder: fn(&[u8]) -> Result<D, opus::Error>,
    reveal: Option<RevealGate>,
    callee_reveal: Option<CalleeReveal>,
    caller_reveal: Option<CallerRevealWait>,
    /// The caller's REVEAL did not verify in time: Hang-up/Error queued.
    unconfirmed: bool,
    opening: Option<OpeningGate>,
    /// The last keyframe request while the decoder waited (CF-MEDIA-44).
    last_key_request_ns: Option<i64>,
    /// Decoder recoveries so far (CF-MEDIA-59: at most 2, then video stops).
    video_recoveries: u32,
    video_stopped: bool,
    /// Received BDX1 kinds 2 and 3, for the owner's presence exchange.
    presence_in: Vec<Vec<u8>>,
    /// Authenticated peer signals the controller applied, for a mirroring UI.
    remote: Vec<Message>,
    video_overflows: u64,
    video_sent: u64,
    audio_sent: u64,
    /// CF-MEDIA-24: media payload bytes taken off this link (mod 2^32), and
    /// this link's clock (ms since it started here) at the last counted byte.
    received_media_bytes: u32,
    received_clock_ms: u32,
    started_ns: u64,
    /// CF-REC-28: the peer sent a v1 heartbeat or a v3 report ("I accept v3").
    peer_accepts_v3: bool,
    /// CF-MEDIA-22: the next once-per-second report (from the answer on).
    next_report_ns: Option<u64>,
    /// CF-REC-28: this side's first record is a v1 heartbeat ("I accept v3").
    sent_v1: bool,
    /// Hang-up/Error already written after the liveness timeout.
    liveness_hung_up: bool,
    /// The peer's reports (arrival time, report), for this side's rate control.
    peer_reports: Vec<(u64, crate::link::Report)>,
    /// CF-MEDIA-25: written media bytes against the peer's v3 counters.
    delivery: crate::call_rate::DeliveryEstimate,
    /// The peer's last report said its playout clock is pinned late.
    peer_late: bool,
    /// Payload bytes of the packet being fragmented (counted at its last part).
    fragment_payload: usize,
    /// CF-MEDIA-23: the arrival-train rate this side reports.
    arrival: crate::call_rate::ArrivalMeter,
    /// CF-REC-40/43: the peer's labels, verified against the endpoint
    /// peer's node ID and this call ID (None: labels not negotiated).
    labels: Option<(crate::labels::Inbox, [u8; 16])>,
    started_at: std::time::Instant,
}

/// CF-MEDIA-22 call reports are on (alternative off-switch: `MESHLIVE_CALL_REPORTS=0`).
pub fn reports_enabled() -> bool {
    std::env::var("MESHLIVE_CALL_REPORTS").ok().as_deref() != Some("0")
}

/// Queue and delivery counters, for the owner's per-second log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub audio_queued: usize,
    pub video_queued: usize,
    /// Bytes of the queued video records (the app backlog in bytes).
    pub video_queued_bytes: usize,
    /// Media packets written to the link (a fragmented packet counts once).
    pub audio_sent: u64,
    pub video_sent: u64,
    /// Times the receive queue overflowed (input dropped to the next keyframe).
    pub video_overflows: u64,
    pub video_rx_queued: usize,
    pub video_rx_skipped: u64,
}
impl<D: UnitDecoder> CallSession<D> {
    /// Requires a verified callee reveal before Accept (pseudonymous callee).
    pub fn require_reveal(&mut self, gate: RevealGate) {
        self.reveal = Some(gate);
    }
    /// Requires the callee's OPENING as its first end-to-end record (RV15).
    pub fn require_opening(&mut self, gate: OpeningGate) {
        self.opening = Some(gate);
    }
    fn check_opening(gate: &OpeningGate, incoming: &Incoming) -> Result<(), Error> {
        let Incoming::Discovery(record) = incoming else {
            return Err(Error::Opening);
        };
        if record.len() < 6 || &record[..4] != b"BDX1" || record[4] != 1 {
            return Err(Error::Opening);
        }
        let entries = rotating::decode_body(&record[5..]).map_err(|_| Error::Opening)?;
        match rotating::check_selection(
            &gate.selected,
            true,
            rotating::ROLE_CALL_OFFER,
            &gate.node_id,
            Some(&entries),
            gate.wall_epoch,
        ) {
            Ok(Selection::Pass) => Ok(()),
            _ => Err(Error::Opening),
        }
    }
    /// BDX1 OFFER/LEASE records received since the last call (Phase 2).
    pub fn take_presence_records(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.presence_in)
    }
    /// Queues one BDX1 record of this side's presence exchange.
    pub fn send_discovery(&mut self, record: Vec<u8>) {
        self.controls.push_back(Control::Discovery(record));
    }
    /// Queues a BDX1 record ahead of everything else: a callee that
    /// presented its real key opens the selected RDI with its OPENING as its
    /// first end-to-end record (RV15).
    pub fn send_discovery_first(&mut self, record: Vec<u8>) {
        self.controls.push_front(Control::Discovery(record));
    }
    /// The caller's camera is off while ringing (a voice offer).
    pub fn voice_offer(&self) -> bool {
        self.controller.voice_offer()
    }
    /// This side's report (CF-MEDIA-22): the arrival-train median (0 until a
    /// train was measured), the playout clock pinned late, its added delay, and the v3
    /// delivery counters when the peer accepts v3 on a direct call.
    fn report(&mut self, now_ns: u64) -> crate::link::Report {
        let (start, _) = Profile::Call.delays_ms();
        let direct = self.controller.relay_hops() == 0;
        crate::link::Report {
            arrival_kbps: self.arrival.kbps(now_ns),
            link_late: self.clock.is_saturated(now_ns as i64),
            added_delay_ms: (self.clock.current_delay_ms() - start).clamp(0, 65_535) as u16,
            delivery: (self.peer_accepts_v3 && direct)
                .then_some(crate::link::Delivery { received_bytes: self.received_media_bytes, clock_ms: self.received_clock_ms }),
            delivery_only: false,
        }
    }
    /// Carrier arrivals (time ns on this session's clock, bytes), in order.
    pub fn on_arrivals(&mut self, arrivals: &[(u64, usize)]) {
        for &(at, bytes) in arrivals {
            self.arrival.on_arrival(at, bytes);
        }
    }
    /// The arrival meter (its samples, for logs).
    pub fn arrival(&self) -> &crate::call_rate::ArrivalMeter {
        &self.arrival
    }
    /// What the peer's v3 counters say about delivery (CF-MEDIA-25).
    pub fn delivery(&self) -> &crate::call_rate::DeliveryEstimate {
        &self.delivery
    }
    /// The peer's last report said it is late (its playout clock pinned).
    pub fn peer_late(&self) -> bool {
        self.peer_late
    }
    /// CF-MEDIA-51: the peer's header must name a codec in `own_decode` (the
    /// mask this PC signed in its endpoint hello) and in `peer_encode` (the
    /// peer's authenticated encode mask).
    pub fn check_header_codecs(&mut self, own_decode: u8, peer_encode: u8) {
        self.codec_gate = Some((own_decode, peer_encode));
    }
    /// LABELS were negotiated (both endpoint hellos carry bit 5): labels
    /// from `signer` (the endpoint peer's node ID) bound to this call are accepted.
    pub fn accept_labels(&mut self, signer: [u8; 16]) {
        self.labels = Some((crate::labels::Inbox::default(), signer));
    }
    /// The caller's own labels, immediately before its Invite (CF-REC-40);
    /// nothing unless LABELS were negotiated.
    pub fn send_labels_first(&mut self, container: Vec<u8>) {
        if self.labels.is_some() {
            self.controls.push_front(Control::Labels(container));
        }
    }
    /// The callee's own name, queued now (at Ringing for a verified contact).
    pub fn send_labels(&mut self, container: Vec<u8>) {
        if self.labels.is_some() {
            self.controls.push_back(Control::Labels(container));
        }
    }
    /// A LABELS container (also one held from before the Invite).
    pub fn on_labels(&mut self, container: &[u8]) -> Vec<Notice> {
        let call_id = self.controller.call_id();
        let now_ms = self.started_at.elapsed().as_millis() as u64;
        let Some((inbox, signer)) = self.labels.as_mut() else { return Vec::new() };
        let changed = inbox.accept(container, now_ms, signer, crate::labels::BINDING_CALL, &call_id);
        changed
            .into_iter()
            .filter_map(|kind| inbox.text(kind).map(|text| Notice::PeerLabel { kind, text: text.to_owned() }))
            .collect()
    }
    /// Label decisions to log (rejections carry no text).
    pub fn take_label_log(&mut self) -> Vec<String> {
        self.labels.as_mut().map(|(inbox, _)| std::mem::take(&mut inbox.log)).unwrap_or_default()
    }
    /// The peer's reports since the last call (arrival time ns, report).
    pub fn take_peer_reports(&mut self) -> Vec<(u64, crate::link::Report)> {
        std::mem::take(&mut self.peer_reports)
    }
    /// The callee reveals itself at Accept (pseudonymous call).
    pub fn reveal_on_accept(&mut self, reveal: CalleeReveal) {
        self.callee_reveal = Some(reveal);
    }
    /// The caller's REVEAL did not verify within its deadline; the call was
    /// hung up with Error ("Couldn't confirm who this is").
    pub fn unconfirmed(&self) -> bool {
        self.unconfirmed
    }
    /// Waiting for the caller's REVEAL (its media dropped, this PC's held).
    pub fn awaiting_caller_reveal(&self) -> bool {
        self.caller_reveal.is_some()
    }
    fn on_discovery(&mut self, record: &[u8], notices: &mut Vec<Notice>) -> Result<(), Error> {
        if let Some(callee) = self.callee_reveal.as_mut() {
            if record.get(4) == Some(&4) {
                if self.unconfirmed {
                    return Ok(());
                }
                let verified = callee.session.on_peer_reveal(record).map_err(|_| Error::Reveal)?.clone();
                crate::identity_gate::admit(&verified.node_id, &verified.fingerprint).map_err(|_| Error::Reveal)?;
                notices.push(Notice::PeerRevealed {
                    node_id: verified.node_id,
                    fingerprint: verified.fingerprint,
                });
                // The caller is confirmed: this PC's held header and capture start now.
                if let Some(CallerRevealWait { held_start, .. }) = self.caller_reveal.take()
                    && let Some(camera) = held_start
                {
                    self.controls.push_back(Control::Header);
                    notices.push(Notice::StartMedia { camera });
                }
                return Ok(());
            }
        }
        if record.len() > 4 && &record[..4] == b"BDX1" && matches!(record[4], 2 | 3) {
            // At most a few per call; the exchange itself rejects repeats.
            if self.presence_in.len() < 8 {
                self.presence_in.push(record.to_vec());
            }
            notices.push(Notice::PresenceRecord);
            return Ok(());
        }
        let Some(gate) = self.reveal.as_mut() else {
            // Not a reveal exchange: BDX1 never fails a call (other kinds are hints).
            return Ok(());
        };
        if record.get(4) != Some(&4) {
            return Ok(());
        }
        let verified = gate
            .session
            .on_peer_reveal(record)
            .map_err(|_| Error::Reveal)?
            .clone();
        // CF-HS-50: the revealed real key, before this PC reveals itself.
        crate::identity_gate::admit(&verified.node_id, &verified.fingerprint).map_err(|_| Error::Reveal)?;
        let opened = rotating::check_selection(
            &gate.selected,
            gate.rotating,
            rotating::ROLE_CALL_OFFER,
            &verified.node_id,
            Some(&verified.entries),
            gate.wall_epoch,
        )
        .map_err(|_| Error::Reveal)?;
        if opened != Selection::Pass {
            return Err(Error::Reveal);
        }
        let entries: [OpeningEntry; 0] = [];
        let own = gate
            .session
            .send_own(gate.real.as_ref(), &entries)
            .map_err(|_| Error::Reveal)?;
        // The caller reveal goes ahead of everything else still queued.
        self.controls.push_front(Control::Discovery(own));
        notices.push(Notice::PeerRevealed {
            node_id: verified.node_id,
            fingerprint: verified.fingerprint,
        });
        Ok(())
    }
    fn new(
        controller: Controller,
        mut link: CallLink,
        header: VideoHeader,
        make_decoder: fn(&[u8]) -> Result<D, opus::Error>,
        now_ns: u64,
    ) -> Self {
        let ordered_fragments = std::env::var("MESHLIVE_CALL_FRAGMENT_ORDER").as_deref() == Ok("1");
        link.allow_fragment_controls(ordered_fragments);
        Self {
            controller,
            media_gate: call::media_gate_enabled(),
            ordered_fragments,
            link,
            header,
            controls: VecDeque::new(),
            audio_out: VecDeque::new(),
            video_out: VecDeque::new(),
            fragmenting: None,
            liveness: Liveness::new(now_ns),
            clock: Profile::Call.clock(),
            audio_in: AudioReceiver::default(),
            video_in: None,
            codec_gate: None,
            peer_opus: false,
            make_decoder,
            reveal: None,
            callee_reveal: None,
            caller_reveal: None,
            unconfirmed: false,
            opening: None,
            presence_in: Vec::new(),
            last_key_request_ns: None,
            video_recoveries: 0,
            video_stopped: false,
            remote: Vec::new(),
            video_overflows: 0,
            video_sent: 0,
            audio_sent: 0,
            received_media_bytes: 0,
            received_clock_ms: 0,
            started_ns: now_ns,
            peer_accepts_v3: false,
            next_report_ns: None,
            sent_v1: false,
            liveness_hung_up: false,
            peer_reports: Vec::new(),
            delivery: Default::default(),
            peer_late: false,
            fragment_payload: 0,
            arrival: Default::default(),
            labels: None,
            started_at: std::time::Instant::now(),
        }
    }
    /// The caller: Invite (and Camera off for a voice call) are the first records.
    pub fn outgoing(
        link: CallLink,
        call_id: [u8; 16],
        voice: bool,
        relay_hops: u8,
        header: VideoHeader,
        make_decoder: fn(&[u8]) -> Result<D, opus::Error>,
        now_ns: u64,
    ) -> Result<Self, Error> {
        let (controller, opening) = Controller::outgoing(call_id, voice, relay_hops)?;
        let mut session = Self::new(controller, link, header, make_decoder, now_ns);
        for message in opening {
            session.queue_signal(&message)?;
        }
        Ok(session)
    }
    /// The callee, created from the first authenticated call record (an Invite).
    pub fn incoming(
        link: CallLink,
        invite: &Message,
        header: VideoHeader,
        make_decoder: fn(&[u8]) -> Result<D, opus::Error>,
        now_ns: u64,
    ) -> Result<(Self, Vec<Notice>), Error> {
        let (controller, actions) = Controller::incoming(invite)?;
        let mut session = Self::new(controller, link, header, make_decoder, now_ns);
        let notices = session.apply(actions)?;
        Ok((session, notices))
    }
    pub fn phase(&self) -> call::Phase {
        self.controller.phase()
    }
    pub fn relay_hops(&self) -> u8 { self.controller.relay_hops() }

    /// A searching callee declines before queuing Ringing or starting media.
    pub fn incoming_busy(
        link: CallLink, invite: &Message, header: VideoHeader,
        make_decoder: fn(&[u8]) -> Result<D, opus::Error>, now_ns: u64,
    ) -> Result<Self, Error> {
        let (controller, _) = Controller::incoming(invite)?;
        let mut session = Self::new(controller, link, header, make_decoder, now_ns);
        session.decline(Reason::Busy)?;
        Ok(session)
    }
    /// The call was answered (Accept sent or received): the moment Phase 2
    /// presence leases may start (RD6).
    pub fn answered(&self) -> bool {
        matches!(self.controller.phase(), call::Phase::NegotiatingMedia | call::Phase::Active)
    }
    fn queue_signal(&mut self, message: &Message) -> Result<(), Error> {
        message.encode()?;
        self.controls.push_back(Control::Signal(*message));
        Ok(())
    }
    fn apply(&mut self, actions: Vec<Action>) -> Result<Vec<Notice>, Error> {
        let mut notices = Vec::new();
        for action in actions {
            match action {
                Action::Send(message) => self.queue_signal(&message)?,
                Action::Ring { voice } => notices.push(Notice::Ring { voice }),
                Action::StartMedia { camera } => {
                    // Pseudonym<->pseudonym callee: nothing of this PC's media
                    // (header or capture) before the caller is confirmed.
                    if let Some(wait) = self.caller_reveal.as_mut() {
                        wait.held_start = Some(camera);
                        continue;
                    }
                    // Every endpoint describes its own outgoing stream first.
                    self.controls.push_back(Control::Header);
                    notices.push(Notice::StartMedia { camera });
                }
                Action::PeerCamera(on) => notices.push(Notice::PeerCamera(on)),
                Action::PeerMicrophone(on) => notices.push(Notice::PeerMicrophone(on)),
                Action::Ended(reason) => {
                    self.audio_out.clear();
                    self.video_out.clear();
                    self.fragmenting = None;
                    self.clear_received_media();
                    notices.push(Notice::Ended(reason));
                }
            }
        }
        Ok(notices)
    }
    pub fn accept(&mut self, voice: bool) -> Result<Vec<Notice>, Error> {
        // The callee's REVEAL goes right before its Accept (same ordered queue).
        if let Some(callee) = self.callee_reveal.as_mut() {
            let own = callee
                .session
                .send_own(callee.real.as_ref(), &callee.entries)
                .map_err(|_| Error::Reveal)?;
            self.controls.push_back(Control::Discovery(own));
            // Both sides pseudonymous (send_own refuses otherwise): the
            // caller's REVEAL must now verify within the deadline.
            self.caller_reveal = Some(CallerRevealWait {
                budget_ns: caller_reveal_budget_ns(callee.relay_hops),
                deadline: None,
                held_start: None,
            });
        }
        let actions = self.controller.accept(voice)?;
        self.apply(actions)
    }
    pub fn decline(&mut self, reason: Reason) -> Result<(), Error> {
        let message = self.controller.decline(reason)?;
        self.queue_signal(&message)
    }
    pub fn cancel(&mut self) -> Result<(), Error> {
        let message = self.controller.cancel()?;
        self.queue_signal(&message)
    }
    /// The liveness Hang-up/Error was handed to the link (for the trace).
    pub fn liveness_hung_up(&self) -> bool {
        self.liveness_hung_up
    }
    /// The peer has been silent past the liveness timeout.
    pub fn is_dead(&self, now_ns: u64) -> bool {
        self.liveness.dead(now_ns)
    }
    pub fn hang_up(&mut self, reason: Reason) -> Result<(), Error> {
        let message = self.controller.hang_up(reason)?;
        self.audio_out.clear();
        self.video_out.clear();
        self.fragmenting = None;
        self.clear_received_media();
        self.queue_signal(&message)
    }
    fn clear_received_media(&mut self) {
        if self.media_gate {
            self.audio_in = AudioReceiver::default();
            self.video_in = None;
        }
    }
    pub fn media_ready(&mut self) -> Result<(), Error> {
        Ok(self.controller.media_ready()?)
    }
    pub fn push_to_talk(&mut self, pressed: bool) -> Result<(), Error> {
        let message = self.controller.push_to_talk(pressed)?;
        self.queue_signal(&message)
    }
    /// Privacy experiment: keep setup, discard microphone media not yet sealed.
    pub fn discard_queued_audio(&mut self) {
        self.audio_out.retain(|packet| media::Packet::decode(packet, media::MAX_PAYLOAD)
            .is_ok_and(|p| p.flags & media::CONFIG != 0));
    }
    pub fn set_camera(&mut self, on: bool) -> Result<(), Error> {
        let message = self.controller.set_camera(on)?;
        if !on {
            self.video_out.clear();
        }
        self.queue_signal(&message)
    }
    pub fn request_keyframe(&mut self) -> Result<(), Error> {
        self.ask_keyframe();
        Ok(())
    }
    /// Coalesced: at most one keyframe request queued at a time.
    fn ask_keyframe(&mut self) {
        if !self.controls.iter().any(|c| matches!(c, Control::KeyFrameRequest)) {
            self.controls.push_back(Control::KeyFrameRequest);
        }
    }
    /// Video was stopped after repeated decoder failures (audio and the call stay).
    pub fn video_stopped(&self) -> bool {
        self.video_stopped
    }
    pub fn video_recoveries(&self) -> u32 {
        self.video_recoveries
    }
    /// One encoded packet from capture. Audio is dropped while muted, video
    /// while the camera is off or before acceptance; returns whether it was queued.
    pub fn send(&mut self, packet: Vec<u8>) -> Result<bool, Error> {
        let flags = media::Packet::decode(&packet, media::MAX_PAYLOAD)
            .map_err(|e| Error::Call(call::Error::Media(e)))?
            .flags;
        // CF-MEDIA-07: nominal capacity max(16, ceil((fps + 50) x 500 ms)).
        let video_limit = MAX_QUEUED_VIDEO.min(16usize.max((usize::from(self.header.fps) + 50).div_ceil(2)));
        let (allowed, queue, limit) = if flags & AUDIO != 0 {
            (
                // CF-MEDIA-73: the Opus config passes the mute gate (a call
                // that starts muted must still tell the peer its config).
                self.controller.should_send_audio() || flags & media::CONFIG != 0,
                &mut self.audio_out,
                MAX_QUEUED_AUDIO,
            )
        } else {
            (
                self.controller.should_send_video(),
                &mut self.video_out,
                video_limit,
            )
        };
        // Held until the caller of a pseudonym<->pseudonym call is confirmed.
        if !allowed || self.caller_reveal.is_some() {
            return Ok(false);
        }
        if queue.len() >= limit {
            if flags & AUDIO == 0 {
                // A refusal removes every queued video record (the owner then
                // drops video until a forced keyframe flagged DISCONTINUITY).
                queue.clear();
                return Err(Error::Backlog);
            }
            // CF-MEDIA-07: audio evicts its oldest unit (fresh speech wins).
            queue.pop_front();
        }
        queue.push_back(packet);
        Ok(true)
    }
    /// The next record to write, sealed at write time so order equals sequence.
    pub fn next_write(&mut self, now_ns: u64) -> Result<Option<Vec<u8>>, Error> {
        if self.liveness.dead(now_ns) {
            // PROTOCOL "Hang-up before a liveness close" (Android, default
            // on; MESHLIVE_CALL_LIVENESS_HANG_UP=0 is the control arm): once,
            // in an answered call, Hang-up/Error goes out before the close so a
            // peer that can still read ends at once instead of after its 8 s.
            if !self.liveness_hung_up && self.answered() && liveness_hang_up() {
                self.liveness_hung_up = true;
                if let Ok(message) = self.controller.hang_up(Reason::Error) {
                    return Ok(Some(self.link.signal(&message)?));
                }
            }
            return Err(Error::Dead);
        }
        // The caller's REVEAL is overdue: hang up once with Error.
        if !self.unconfirmed
            && self.caller_reveal.as_ref().is_some_and(|w| w.deadline.is_some_and(|d| now_ns >= d))
            && self.hang_up(Reason::Error).is_ok()
        {
            self.unconfirmed = true;
        }
        if !self.sent_v1 {
            self.sent_v1 = true;
            self.liveness.sent(now_ns);
            return Ok(Some(self.link.heartbeat(None)?));
        }
        if self.liveness.heartbeat_due(now_ns) {
            return Ok(Some(self.link.heartbeat(None)?));
        }
        // Android writes one frame atomically except for queued audio.
        // Do not insert Camera off, labels or requests into its fragments.
        if (!self.ordered_fragments || self.fragmenting.is_none()) && let Some(control) = self.controls.pop_front() {
            return Ok(Some(match control {
                Control::Signal(message) => self.link.signal(&message)?,
                Control::Header => self.link.header(&self.header)?,
                Control::KeyFrameRequest => self.link.keyframe_request()?,
                Control::Discovery(record) => {
                    // This PC's REVEAL leaves now: the caller's deadline starts.
                    if record.get(4) == Some(&4)
                        && let Some(wait) = self.caller_reveal.as_mut()
                        && wait.deadline.is_none()
                    {
                        wait.deadline = Some(now_ns.saturating_add(wait.budget_ns));
                    }
                    self.link.discovery(&record)?
                }
                Control::Labels(container) => self.link.labels(&container)?,
            }));
        }
        // Reports every 1 s once the call is answered, after queued signals and controls (they also keep the link alive).
        if reports_enabled() && self.answered() {
            let due = *self.next_report_ns.get_or_insert(now_ns);
            if now_ns >= due {
                self.next_report_ns = Some(now_ns + 1_000_000_000);
                self.liveness.sent(now_ns);
                let report = self.report(now_ns);
                return Ok(Some(self.link.heartbeat(Some(&report))?));
            }
        }
        if let Some(packet) = self.audio_out.pop_front() {
            if let Ok(p) = media::Packet::decode(&packet, media::MAX_PAYLOAD) {
                self.delivery.on_written(p.payload.len(), now_ns);
            }
            let mut records = self.link.media(&packet)?;
            self.audio_sent += 1;
            return Ok(records.pop());
        }
        if let Some((plan, index)) = self.fragmenting.take() {
            let record = self.link.seal_part(&plan, index)?;
            if index + 1 < plan.len() {
                self.fragmenting = Some((plan, index + 1));
            } else {
                self.delivery.on_written(self.fragment_payload, now_ns);
            }
            return Ok(Some(record));
        }
        if let Some(packet) = self.video_out.pop_front() {
            self.video_sent += 1;
            let payload = media::Packet::decode(&packet, media::MAX_PAYLOAD).map_or(0, |p| p.payload.len());
            let plan = self.link.plan(&packet)?;
            let record = self.link.seal_part(&plan, 0)?;
            if plan.len() > 1 {
                self.fragmenting = Some((plan, 1));
                self.fragment_payload = payload;
            } else {
                self.delivery.on_written(payload, now_ns);
            }
            return Ok(Some(record));
        }
        Ok(None)
    }
    /// Feeds received link bytes; returns what the owner must act on.
    pub fn on_input(&mut self, input: &mut &[u8], now_ns: u64) -> Result<Vec<Notice>, Error> {
        let mut notices = Vec::new();
        loop {
            let incoming = if self.media_gate {
                self.link.read_in_phase(input, Some(self.controller.phase()))?
            } else { self.link.read(input)? };
            let fragment_progress = self.link.take_fragment_progress();
            if self.ordered_fragments && fragment_progress {
                self.liveness.on_received(now_ns);
            }
            let Some(incoming) = incoming else { break };
            self.liveness.on_received(now_ns);
            if !matches!(incoming, Incoming::Heartbeat(_))
                && let Some(gate) = self.opening.take()
            {
                Self::check_opening(&gate, &incoming)?;
                continue;
            }
            match incoming {
                Incoming::Heartbeat(report) => {
                    // A v1 heartbeat or a v3 report means the peer reads v3 (CF-REC-28).
                    if report.is_none_or(|r| r.delivery.is_some()) {
                        self.peer_accepts_v3 = true;
                    }
                    if let Some(r) = report {
                        self.peer_late = r.link_late;
                        if let Some(d) = r.delivery {
                            self.delivery.on_report(d.received_bytes, d.clock_ms, now_ns);
                        }
                        if self.peer_reports.len() >= 64 {
                            self.peer_reports.remove(0);
                        }
                        self.peer_reports.push((now_ns, r));
                    }
                }
                // Calls never open LABELS on this PC (no label support yet).
                // A label never fails a call (CF-REC-40): verified or dropped.
                Incoming::Labels(bytes) => notices.extend(self.on_labels(&bytes)),
                Incoming::Signal(message) => {
                    // An Accept before a verified reveal fails a pseudonymous call.
                    if message.signal == Signal::Accept
                        && self
                            .reveal
                            .as_ref()
                            .is_some_and(|g| g.session.peer_reveal_required())
                    {
                        return Err(Error::Reveal);
                    }
                    let actions = self.controller.on_message(&message)?;
                    // CF-MEDIA-44: the peer's camera came back: ask for a keyframe.
                    if message.signal == Signal::CameraOn && !self.video_stopped {
                        self.ask_keyframe();
                    }
                    self.remote.push(message);
                    notices.extend(self.apply(actions)?);
                }
                Incoming::Discovery(record) => self.on_discovery(&record, &mut notices)?,
                // An unconfirmed caller's header or media is dropped.
                Incoming::Header(_) | Incoming::Media { .. } if self.caller_reveal.is_some() => {}
                Incoming::Header(header) => {
                    // CF-MEDIA-51 / CF-CALL-15: exactly one header, in a codec
                    // both hellos authenticated (as Android: the call fails).
                    if self.video_in.is_some() {
                        return Err(Error::SecondHeader);
                    }
                    if let Some((own_decode, peer_encode)) = self.codec_gate {
                        let bit = codec_mask(header.codec);
                        if own_decode & bit == 0 || peer_encode & bit == 0 {
                            return Err(Error::Codec);
                        }
                    }
                    self.peer_opus = header.opus;
                    let mut video = VideoReceiver::new(header);
                    // CF-MEDIA-66: speech-led playout on a direct call (every
                    // PC call is direct GATT): late video grows the shared
                    // delay by at most 150 ms; late audio keeps the full range.
                    if self.controller.relay_hops() == 0 {
                        video.set_growth_limit_ms(Some(SPEECH_LED_VIDEO_GROWTH_LIMIT_MS));
                    }
                    self.video_in = Some(video);
                    notices.push(Notice::PeerHeader(header));
                }
                Incoming::KeyFrameRequest => notices.push(Notice::KeyFrameRequested),
                Incoming::Media {
                    flags,
                    pts_us,
                    payload,
                } => {
                    let packet = media::Packet {
                        flags,
                        pts_us,
                        payload: &payload,
                    };
                    // A display rotation is metadata: never decoded, never timed,
                    // never counted as delivered media (CF-MEDIA-24).
                    if flags & media::ROTATION == 0 {
                        self.received_media_bytes = self.received_media_bytes.wrapping_add(payload.len() as u32);
                        self.received_clock_ms = (now_ns.saturating_sub(self.started_ns) / 1_000_000) as u32;
                    }
                    if flags & media::ROTATION != 0 {
                        let turns = packet.rotation().map_err(|e| Error::Call(call::Error::Media(e)))?;
                        notices.push(Notice::PeerRotation(turns));
                        continue;
                    }
                    let video = self.video_in.as_mut().ok_or(Error::NoHeader)?;
                    let now = now_ns as i64;
                    if flags & AUDIO != 0 && !self.peer_opus {
                        return Err(Error::UndeclaredAudio);
                    }
                    if flags & AUDIO != 0 {
                        self.audio_in
                            .on_packet(packet, now, &mut self.clock, self.make_decoder)
                            .map_err(Error::Audio)?;
                    } else if self.video_stopped {
                        // Video ended after repeated decoder failures; the call goes on.
                    } else {
                        match video.on_packet(packet, now, &mut self.clock) {
                            Ok(()) => {}
                            // The owner fell behind: drop to the next keyframe and ask
                            // the peer for one; the call goes on (audio is unaffected).
                            Err(viewer::Error::Full) => {
                                video.overflow();
                                self.video_overflows += 1;
                                if !self.controls.iter().any(|c| matches!(c, Control::KeyFrameRequest)) {
                                    self.controls.push_back(Control::KeyFrameRequest);
                                }
                                self.last_key_request_ns = Some(now);
                            }
                            Err(error) => return Err(Error::Video(error)),
                        }
                    }
                }
            }
        }
        Ok(notices)
    }
    /// Feeds the peer's due video to `decoder` and pulls what it decoded;
    /// returns the number of pictures out. An owner without a surface passes
    /// `viewer::DiscardFrames` so the receive queue never fills.
    pub fn pump_video(
        &mut self,
        decoder: &mut impl viewer::FrameDecoder,
        now_ns: i64,
        lead_ns: i64,
    ) -> Result<usize, Error> {
        if self.video_stopped {
            return Ok(0);
        }
        let Some(video) = self.video_in.as_mut() else {
            return Ok(0);
        };
        // CF-MEDIA-44: every 2 s while the decoder waits for a keyframe.
        let waiting = video.awaiting_keyframe();
        let result = video
            .feed_due(decoder, now_ns, &self.clock, lead_ns)
            .and_then(|_| video.drain(decoder, now_ns, &self.clock).map(|f| f.len()));
        let outcome = match result {
            Ok(n) => Ok(n),
            // CF-MEDIA-59: recreate from the newest format, drop to the next
            // keyframe and ask for one; after two recoveries stop video only.
            Err(viewer::Error::Decoder) if self.video_recoveries < MAX_VIDEO_RECOVERIES => {
                self.video_recoveries += 1;
                video.recover().map_err(Error::Video)?;
                self.ask_keyframe();
                self.last_key_request_ns = Some(now_ns);
                return Ok(0);
            }
            Err(viewer::Error::Decoder) => {
                // Keep the receiver (audio still needs the header); drop its queue.
                let _ = video.recover();
                self.video_stopped = true;
                return Ok(0);
            }
            Err(error) => Err(Error::Video(error)),
        };
        if waiting && self.last_key_request_ns.is_none_or(|t| now_ns - t >= KEY_REQUEST_INTERVAL_NS) {
            self.ask_keyframe();
            self.last_key_request_ns = Some(now_ns);
        }
        outcome
    }
    pub fn stats(&self) -> Stats {
        Stats {
            audio_queued: self.audio_out.len(),
            video_queued: self.video_out.len(),
            video_queued_bytes: self.video_out.iter().map(Vec::len).sum(),
            audio_sent: self.audio_sent,
            video_sent: self.video_sent,
            video_overflows: self.video_overflows,
            video_rx_queued: self.video_in.as_ref().map_or(0, |v| v.queued_frames()),
            video_rx_skipped: self.video_in.as_ref().map_or(0, |v| v.skipped()),
        }
    }
    /// The peer signals applied since the last call, in arrival order (each
    /// passed the controller and, for Accept, the reveal gate).
    pub fn take_remote(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.remote)
    }
    pub fn audio(&mut self) -> &mut AudioReceiver<D> {
        &mut self.audio_in
    }
    pub fn video(&mut self) -> Option<&mut VideoReceiver> {
        self.video_in.as_mut()
    }
    pub fn clock(&self) -> &Clock {
        &self.clock
    }
}

/// CF-MEDIA-66 (Android `CallPlayoutPolicy.SPEECH_LED_VIDEO_GROWTH_LIMIT_MS`).
pub const SPEECH_LED_VIDEO_GROWTH_LIMIT_MS: i64 = 150;

/// The capability-mask bit of a BTV3 codec (2 VP9, 3 AV1; Design HC 4
/// H.264, 5 HEVC; others none), from the one codec table.
pub fn codec_mask(codec: u8) -> u8 {
    crate::video::codecs::hello_bit(codec)
}

#[cfg(test)]
mod tests;
