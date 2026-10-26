//! Private calls (PROTOCOL "Private-call signaling", "Voice calls and
//! push-to-talk", "Experimental call video fragments"): the exact 23-byte
//! `BMC2` signal, Android's call state machine, a pure call controller (voice
//! offers, camera and push-to-talk microphone state) and the full-duplex call
//! link that seals and opens every call record under the endpoint keys. Media
//! capture, codecs and devices stay outside; no radio is touched here.
use crate::{
    framing::{self, HopReader},
    link,
    media::{self, AUDIO, CONFIG},
    records::{self, Layer, Opener, Policy, Sealer},
};

pub const SIGNAL_BYTES: usize = 23;
pub const MAX_RELAY_HOPS: u8 = 15;

/// Android CallMediaGate parity; retained control arm until review.
pub fn media_gate_enabled() -> bool {
    std::env::var("MESHLIVE_CALL_MEDIA_GATE").as_deref() != Ok("0")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Format,
    Signal,
    Reason,
    CallId,
    /// The event is not allowed in the current phase.
    State,
    Fragment,
    Kind,
    Heartbeat,
    Frame(framing::Error),
    Record(records::Error),
    Media(media::Error),
}
impl From<records::Error> for Error {
    fn from(error: records::Error) -> Self {
        Self::Record(error)
    }
}
impl From<framing::Error> for Error {
    fn from(error: framing::Error) -> Self {
        Self::Frame(error)
    }
}
impl From<media::Error> for Error {
    fn from(error: media::Error) -> Self {
        Self::Media(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Signal {
    Invite = 1,
    Ringing = 2,
    Accept = 3,
    Decline = 4,
    Cancel = 5,
    HangUp = 6,
    CameraOff = 7,
    CameraOn = 8,
    MicrophoneMuted = 9,
    MicrophoneUnmuted = 10,
}
impl Signal {
    fn from_wire(value: u8) -> Result<Self, Error> {
        Ok(match value {
            1 => Self::Invite,
            2 => Self::Ringing,
            3 => Self::Accept,
            4 => Self::Decline,
            5 => Self::Cancel,
            6 => Self::HangUp,
            7 => Self::CameraOff,
            8 => Self::CameraOn,
            9 => Self::MicrophoneMuted,
            10 => Self::MicrophoneUnmuted,
            _ => return Err(Error::Signal),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    None = 0,
    Rejected = 1,
    Busy = 2,
    Unsupported = 3,
    Normal = 4,
    Error = 5,
}
impl Reason {
    fn from_wire(value: u8) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::None,
            1 => Self::Rejected,
            2 => Self::Busy,
            3 => Self::Unsupported,
            4 => Self::Normal,
            5 => Self::Error,
            _ => return Err(Error::Reason),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub call_id: [u8; 16],
    pub signal: Signal,
    pub reason: Reason,
    pub relay_hops: u8,
}
impl Message {
    pub fn new(
        call_id: [u8; 16],
        signal: Signal,
        reason: Reason,
        relay_hops: u8,
    ) -> Result<Self, Error> {
        let message = Self {
            call_id,
            signal,
            reason,
            relay_hops,
        };
        message.validate()?;
        Ok(message)
    }
    pub fn simple(call_id: [u8; 16], signal: Signal) -> Result<Self, Error> {
        Self::new(call_id, signal, Reason::None, 0)
    }
    fn validate(&self) -> Result<(), Error> {
        if self.call_id == [0; 16] {
            return Err(Error::CallId);
        }
        if self.relay_hops > MAX_RELAY_HOPS
            || (self.signal != Signal::Invite && self.relay_hops != 0)
        {
            return Err(Error::Format);
        }
        let reason_ok = match self.signal {
            Signal::Decline => matches!(
                self.reason,
                Reason::Rejected | Reason::Busy | Reason::Unsupported
            ),
            Signal::HangUp => matches!(self.reason, Reason::Normal | Reason::Error),
            _ => self.reason == Reason::None,
        };
        if !reason_ok {
            return Err(Error::Reason);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<[u8; SIGNAL_BYTES], Error> {
        self.validate()?;
        let mut raw = [0; SIGNAL_BYTES];
        raw[..4].copy_from_slice(b"BMC2");
        raw[4] = self.signal as u8;
        raw[5..21].copy_from_slice(&self.call_id);
        raw[21] = self.reason as u8;
        raw[22] = self.relay_hops;
        Ok(raw)
    }
    pub fn decode(raw: &[u8]) -> Result<Self, Error> {
        if raw.len() != SIGNAL_BYTES || &raw[..4] != b"BMC2" {
            return Err(Error::Format);
        }
        Self::new(
            raw[5..21].try_into().unwrap(),
            Signal::from_wire(raw[4])?,
            Reason::from_wire(raw[21])?,
            raw[22],
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Caller,
    Callee,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    OutgoingInvite,
    OutgoingRinging,
    IncomingRinging,
    NegotiatingMedia,
    Active,
    Ended,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    RemoteRinging,
    LocalAccept,
    RemoteAccept,
    LocalDecline,
    RemoteDecline,
    LocalCancel,
    RemoteCancel,
    MediaReady,
    LocalHangUp,
    RemoteHangUp,
    LinkLost,
}
/// Android `PrivateCallState.transition`, unchanged.
pub fn transition(phase: Phase, event: Event) -> Result<Phase, Error> {
    use {Event as E, Phase as P};
    Ok(match (phase, event) {
        (P::OutgoingInvite, E::RemoteRinging) => P::OutgoingRinging,
        (P::OutgoingInvite | P::OutgoingRinging, E::RemoteAccept) => P::NegotiatingMedia,
        (
            P::OutgoingInvite | P::OutgoingRinging,
            E::RemoteDecline | E::LocalCancel | E::LinkLost,
        ) => P::Ended,
        (P::IncomingRinging, E::LocalAccept) => P::NegotiatingMedia,
        (P::IncomingRinging, E::LocalDecline | E::RemoteCancel | E::LinkLost) => P::Ended,
        (P::NegotiatingMedia, E::MediaReady) => P::Active,
        (P::NegotiatingMedia | P::Active, E::LocalHangUp | E::RemoteHangUp | E::LinkLost) => {
            P::Ended
        }
        _ => return Err(Error::State),
    })
}

/// What the owner must do after a controller step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Send(Message),
    /// Ring the user; `voice` while the caller's camera is off.
    Ring {
        voice: bool,
    },
    /// Start capture/encoders: the camera only when `camera` (voice answers stay off).
    StartMedia {
        camera: bool,
    },
    PeerCamera(bool),
    PeerMicrophone(bool),
    Ended(Reason),
}

/// One call's signaling, validated against its call ID and phase. Camera and
/// microphone start only after acceptance; push-to-talk drops captured audio
/// while muted (the encoder keeps running).
pub struct Controller {
    call_id: [u8; 16],
    role: Role,
    phase: Phase,
    camera: bool,
    microphone: bool,
    peer_camera: bool,
    relay_hops: u8,
}
impl Controller {
    /// The caller's opening records: Invite, then Camera off for a voice call.
    pub fn outgoing(
        call_id: [u8; 16],
        voice: bool,
        relay_hops: u8,
    ) -> Result<(Self, Vec<Message>), Error> {
        let invite = Message::new(call_id, Signal::Invite, Reason::None, relay_hops)?;
        let mut out = vec![invite];
        if voice {
            out.push(Message::simple(call_id, Signal::CameraOff)?);
        }
        Ok((
            Self {
                call_id,
                role: Role::Caller,
                phase: Phase::OutgoingInvite,
                camera: !voice,
                microphone: true,
                peer_camera: true,
                relay_hops,
            },
            out,
        ))
    }
    /// A callee after the first record (an Invite): answers Ringing and rings.
    pub fn incoming(invite: &Message) -> Result<(Self, Vec<Action>), Error> {
        if invite.signal != Signal::Invite {
            return Err(Error::State);
        }
        let ringing = Message::simple(invite.call_id, Signal::Ringing)?;
        Ok((
            Self {
                call_id: invite.call_id,
                role: Role::Callee,
                phase: Phase::IncomingRinging,
                camera: true,
                microphone: true,
                peer_camera: true,
                relay_hops: invite.relay_hops,
            },
            vec![Action::Send(ringing), Action::Ring { voice: false }],
        ))
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn call_id(&self) -> [u8; 16] {
        self.call_id
    }
    pub fn relay_hops(&self) -> u8 {
        self.relay_hops
    }
    /// The caller's camera is off while ringing: a voice offer.
    pub fn voice_offer(&self) -> bool {
        self.role == Role::Callee && !self.peer_camera
    }
    fn step(&mut self, event: Event) -> Result<(), Error> {
        self.phase = transition(self.phase, event)?;
        Ok(())
    }
    pub fn on_message(&mut self, message: &Message) -> Result<Vec<Action>, Error> {
        if message.call_id != self.call_id {
            return Err(Error::CallId);
        }
        let ringing = self.phase == Phase::IncomingRinging;
        Ok(match message.signal {
            Signal::Invite => return Err(Error::State),
            Signal::Ringing if self.role == Role::Caller => {
                self.step(Event::RemoteRinging)?;
                vec![]
            }
            Signal::Accept if self.role == Role::Caller => {
                self.step(Event::RemoteAccept)?;
                vec![Action::StartMedia {
                    camera: self.camera,
                }]
            }
            Signal::Decline if self.role == Role::Caller => {
                self.step(Event::RemoteDecline)?;
                vec![Action::Ended(message.reason)]
            }
            Signal::Cancel if self.role == Role::Callee => {
                self.step(Event::RemoteCancel)?;
                vec![Action::Ended(Reason::None)]
            }
            Signal::HangUp => {
                self.step(Event::RemoteHangUp)?;
                vec![Action::Ended(message.reason)]
            }
            Signal::CameraOff | Signal::CameraOn => {
                if self.phase == Phase::Ended {
                    return Err(Error::State);
                }
                self.peer_camera = message.signal == Signal::CameraOn;
                if ringing {
                    // Camera off while ringing makes it a voice offer; on withdraws it.
                    vec![Action::Ring {
                        voice: !self.peer_camera,
                    }]
                } else {
                    vec![Action::PeerCamera(self.peer_camera)]
                }
            }
            Signal::MicrophoneMuted | Signal::MicrophoneUnmuted => {
                if self.phase == Phase::Ended {
                    return Err(Error::State);
                }
                vec![Action::PeerMicrophone(
                    message.signal == Signal::MicrophoneUnmuted,
                )]
            }
            _ => return Err(Error::State),
        })
    }
    /// The callee's answer; a voice answer adds Camera off and never starts the camera.
    pub fn accept(&mut self, voice: bool) -> Result<Vec<Action>, Error> {
        if self.role != Role::Callee {
            return Err(Error::State);
        }
        self.step(Event::LocalAccept)?;
        self.camera = !voice;
        let mut out = vec![Action::Send(Message::simple(self.call_id, Signal::Accept)?)];
        if voice {
            out.push(Action::Send(Message::simple(
                self.call_id,
                Signal::CameraOff,
            )?));
        }
        out.push(Action::StartMedia {
            camera: self.camera,
        });
        Ok(out)
    }
    pub fn decline(&mut self, reason: Reason) -> Result<Message, Error> {
        let message = Message::new(self.call_id, Signal::Decline, reason, 0)?;
        if self.role != Role::Callee {
            return Err(Error::State);
        }
        self.step(Event::LocalDecline)?;
        Ok(message)
    }
    pub fn cancel(&mut self) -> Result<Message, Error> {
        if self.role != Role::Caller {
            return Err(Error::State);
        }
        self.step(Event::LocalCancel)?;
        Message::simple(self.call_id, Signal::Cancel)
    }
    pub fn media_ready(&mut self) -> Result<(), Error> {
        self.step(Event::MediaReady)
    }
    pub fn hang_up(&mut self, reason: Reason) -> Result<Message, Error> {
        let message = Message::new(self.call_id, Signal::HangUp, reason, 0)?;
        self.step(Event::LocalHangUp)?;
        Ok(message)
    }
    pub fn link_lost(&mut self) -> Result<(), Error> {
        self.step(Event::LinkLost)
    }
    /// Each endpoint controls only its own camera (upgrade/downgrade).
    pub fn set_camera(&mut self, on: bool) -> Result<Message, Error> {
        if self.phase == Phase::Ended {
            return Err(Error::State);
        }
        self.camera = on;
        Message::simple(
            self.call_id,
            if on {
                Signal::CameraOn
            } else {
                Signal::CameraOff
            },
        )
    }
    /// Hold-to-talk: pressed sends Microphone unmuted, release sends muted.
    pub fn push_to_talk(&mut self, pressed: bool) -> Result<Message, Error> {
        if self.phase == Phase::Ended {
            return Err(Error::State);
        }
        self.microphone = pressed;
        Message::simple(
            self.call_id,
            if pressed {
                Signal::MicrophoneUnmuted
            } else {
                Signal::MicrophoneMuted
            },
        )
    }
    /// Captured audio is sent only in a live call while not muted.
    pub fn should_send_audio(&self) -> bool {
        self.microphone && matches!(self.phase, Phase::NegotiatingMedia | Phase::Active)
    }
    pub fn should_send_video(&self) -> bool {
        self.camera && matches!(self.phase, Phase::NegotiatingMedia | Phase::Active)
    }
}

/// Experimental call video fragments (off by default).
pub mod fragments {
    use super::*;
    pub const FRAGMENT_BYTES: usize = 2_048;
    pub const MIN_FRAGMENTED_PACKET_BYTES: usize = 3_072;
    pub const HEADER_BYTES: usize = 8;
    /// End-to-end hello transport-mask bit 7.
    pub const CAPABILITY_BIT: u8 = 0x80;
    pub fn max_packet_bytes() -> usize {
        Layer::Endpoint.maximum_ciphertext() - 16
    }
    pub fn should_fragment(packet_bytes: usize, flags: u8) -> bool {
        flags & (AUDIO | CONFIG) == 0 && packet_bytes > MIN_FRAGMENTED_PACKET_BYTES
    }
    /// This PC offers call video fragments (CF-MEDIA-76/77, Android's default
    /// on direct GATT calls). Alternative off-switch: `MESHLIVE_CALL_FRAGMENTS=0`.
    pub fn local_enabled() -> bool {
        std::env::var("MESHLIVE_CALL_FRAGMENTS").ok().as_deref() != Some("0")
    }
    /// The endpoint hello transport byte: `base` plus bit 7 when offered.
    pub fn endpoint_transport(base: u8) -> u8 {
        if local_enabled() { base | CAPABILITY_BIT } else { base }
    }
    pub fn negotiated(local: bool, peer_transport: u8, relay_hops: u8) -> bool {
        local && relay_hops == 0 && peer_transport & CAPABILITY_BIT != 0
    }
    pub fn split(packet: &[u8], chunk: usize) -> Result<Vec<Vec<u8>>, Error> {
        if chunk == 0 || packet.is_empty() || packet.len() > max_packet_bytes() {
            return Err(Error::Fragment);
        }
        Ok(packet
            .chunks(chunk)
            .enumerate()
            .map(|(i, part)| {
                let mut out = Vec::with_capacity(HEADER_BYTES + part.len());
                out.extend_from_slice(&(packet.len() as u32).to_be_bytes());
                out.extend_from_slice(&((i * chunk) as u32).to_be_bytes());
                out.extend_from_slice(part);
                out
            })
            .collect())
    }
    /// Accepts fragments strictly in order; any inconsistency fails the call.
    #[derive(Default)]
    pub struct Reassembler {
        buffer: Option<Vec<u8>>,
        total: usize,
        flags: u8,
    }
    impl Reassembler {
        pub fn in_progress(&self) -> bool {
            self.buffer.is_some()
        }
        pub fn accept(&mut self, flags: u8, fragment: &[u8]) -> Result<Option<Vec<u8>>, Error> {
            if flags & (AUDIO | CONFIG) != 0 || fragment.len() <= HEADER_BYTES {
                return Err(Error::Fragment);
            }
            let total = u32::from_be_bytes(fragment[..4].try_into().unwrap()) as usize;
            let offset = u32::from_be_bytes(fragment[4..8].try_into().unwrap()) as usize;
            let chunk = &fragment[HEADER_BYTES..];
            match &self.buffer {
                None => {
                    if offset != 0 || !(1..=max_packet_bytes()).contains(&total) {
                        return Err(Error::Fragment);
                    }
                    self.buffer = Some(Vec::with_capacity(total));
                    self.total = total;
                    self.flags = flags;
                }
                Some(buffer) => {
                    if total != self.total || offset != buffer.len() || flags != self.flags {
                        return Err(Error::Fragment);
                    }
                }
            }
            let buffer = self.buffer.as_mut().unwrap();
            if chunk.len() > self.total - buffer.len() {
                return Err(Error::Fragment);
            }
            buffer.extend_from_slice(chunk);
            if buffer.len() < self.total {
                return Ok(None);
            }
            Ok(self.buffer.take())
        }
    }
}

/// One media packet's wire parts (content type 2 whole, 9 fragments).
pub struct MediaPlan {
    kind: u8,
    flags: u8,
    parts: Vec<Vec<u8>>,
}
impl MediaPlan {
    pub fn len(&self) -> usize {
        self.parts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// What a call link delivers.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    Signal(Message),
    Header(media::VideoHeader),
    /// One whole (possibly reassembled) BTV3 media packet: flags, pts, payload.
    Media {
        flags: u8,
        pts_us: i64,
        payload: Vec<u8>,
    },
    KeyFrameRequest,
    Heartbeat(Option<link::Report>),
    /// A BDX1 record (content type 11); only when the endpoint hellos carried bit 3.
    Discovery(Vec<u8>),
    /// A LABELS record (content type 10); only on links opened with labels
    /// (a BVMID2 verification session accepts the scanner's name label).
    Labels(Vec<u8>),
}

/// Full-duplex call records over one link: BMS1 hop records carrying BME1 call
/// content under the endpoint keys. Fragments are sent and accepted only when
/// negotiated (default off); while a fragmented frame is unfinished, whole
/// media must be audio. The fragment-order experiment also accepts Android's
/// existing authenticated call controls during reassembly.
pub struct CallLink {
    hop_out: Sealer,
    content_out: Sealer,
    hop_in: HopReader,
    content_in: Opener,
    fragments: bool,
    fragment_controls: bool,
    fragment_progress: bool,
    reassembler: fragments::Reassembler,
}
impl CallLink {
    pub fn new(
        link_send: &[u8; 32],
        link_receive: &[u8; 32],
        endpoint_send: &[u8; 32],
        endpoint_receive: &[u8; 32],
        fragments_negotiated: bool,
    ) -> Result<Self, Error> {
        let content = Policy {
            video_fragments: fragments_negotiated,
            ..Policy::default()
        };
        Ok(Self {
            hop_out: Sealer::new(Layer::Hop, link_send, Policy::default())?,
            content_out: Sealer::new(Layer::Endpoint, endpoint_send, content)?,
            hop_in: HopReader::new(link_receive, Policy::default())?,
            content_in: Opener::new(Layer::Endpoint, endpoint_receive, content)?,
            fragments: fragments_negotiated,
            fragment_controls: false,
            fragment_progress: false,
            reassembler: fragments::Reassembler::default(),
        })
    }
    /// Continues a link whose hop records already carried the endpoint
    /// handshake (BMS1 type 4): the hop sequence numbers carry on.
    /// `discovery` accepts BDX1 (type 11) when both endpoint hellos carry bit 3.
    pub fn continue_link(
        hop_out: Sealer,
        hop_in: HopReader,
        endpoint_send: &[u8; 32],
        endpoint_receive: &[u8; 32],
        fragments_negotiated: bool,
        discovery: bool,
    ) -> Result<Self, Error> {
        Self::continue_link_with(hop_out, hop_in, endpoint_send, endpoint_receive, fragments_negotiated, discovery, false)
    }
    /// `continue_link` that also opens LABELS records (content type 10).
    pub fn continue_link_with(
        hop_out: Sealer,
        hop_in: HopReader,
        endpoint_send: &[u8; 32],
        endpoint_receive: &[u8; 32],
        fragments_negotiated: bool,
        discovery: bool,
        labels: bool,
    ) -> Result<Self, Error> {
        let content = Policy {
            video_fragments: fragments_negotiated,
            discovery_exchange: discovery,
            labels,
            ..Policy::default()
        };
        Ok(Self {
            hop_out,
            content_out: Sealer::new(Layer::Endpoint, endpoint_send, content)?,
            hop_in,
            content_in: Opener::new(Layer::Endpoint, endpoint_receive, content)?,
            fragments: fragments_negotiated,
            fragment_controls: false,
            fragment_progress: false,
            reassembler: fragments::Reassembler::default(),
        })
    }
    /// Android's endpoint reader handles authenticated call controls during
    /// reassembly; the sender still interleaves only audio.
    pub(crate) fn allow_fragment_controls(&mut self, enabled: bool) {
        self.fragment_controls = enabled;
    }
    /// A complete, authenticated and valid fragment proves receive progress
    /// even when its video frame is not complete yet. Partial carrier bytes do not.
    pub(crate) fn take_fragment_progress(&mut self) -> bool {
        std::mem::take(&mut self.fragment_progress)
    }
    /// One LABELS record (content type 10); only on a link opened with labels.
    pub fn labels(&mut self, record: &[u8]) -> Result<Vec<u8>, Error> {
        self.wrap(10, 0, record)
    }
    /// One BDX1 record (content type 11), e.g. a REVEAL.
    pub fn discovery(&mut self, record: &[u8]) -> Result<Vec<u8>, Error> {
        self.wrap(11, 0, record)
    }
    fn wrap(&mut self, kind: u8, flags: u8, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let inner = self.content_out.seal(kind, flags, plaintext)?;
        Ok(self.hop_out.seal(5, 0, &inner)?)
    }
    pub fn signal(&mut self, message: &Message) -> Result<Vec<u8>, Error> {
        let raw = message.encode()?;
        self.wrap(6, 0, &raw)
    }
    pub fn header(&mut self, header: &media::VideoHeader) -> Result<Vec<u8>, Error> {
        let raw = header.encode()?;
        self.wrap(1, 0, &raw)
    }
    pub fn keyframe_request(&mut self) -> Result<Vec<u8>, Error> {
        self.wrap(5, 0, &[])
    }
    pub fn heartbeat(&mut self, report: Option<&link::Report>) -> Result<Vec<u8>, Error> {
        let payload = report.map_or_else(|| link::keepalive().to_vec(), link::encode);
        Ok(self.hop_out.seal(6, 0, &payload)?)
    }
    /// How one encoded BTV3 packet goes on the wire: whole, or as ordered
    /// fragments when negotiated. Parts are sealed one at a time at write time,
    /// so the writer can put queued call audio between fragments.
    pub fn plan(&self, packet: &[u8]) -> Result<MediaPlan, Error> {
        let flags = media::Packet::decode(packet, media::MAX_PAYLOAD)?.flags;
        if self.fragments && fragments::should_fragment(packet.len(), flags) {
            return Ok(MediaPlan {
                kind: 9,
                flags,
                parts: fragments::split(packet, fragments::FRAGMENT_BYTES)?,
            });
        }
        Ok(MediaPlan {
            kind: 2,
            flags,
            parts: vec![packet.to_vec()],
        })
    }
    pub fn seal_part(&mut self, plan: &MediaPlan, index: usize) -> Result<Vec<u8>, Error> {
        let part = plan.parts.get(index).ok_or(Error::Fragment)?;
        self.wrap(plan.kind, plan.flags, part)
    }
    /// The whole packet sealed in order (no interleaving).
    pub fn media(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let plan = self.plan(packet)?;
        (0..plan.parts.len())
            .map(|i| self.seal_part(&plan, i))
            .collect()
    }
    /// `Ok(None)` only when more input is needed; an unfinished fragmented
    /// frame keeps reading the following records.
    pub fn read(&mut self, input: &mut &[u8]) -> Result<Option<Incoming>, Error> {
        self.read_in_phase(input, Some(Phase::Active))
    }
    /// Authenticate before applying Android's call-media phase rule, and apply
    /// it before parsing a header/packet or retaining a video fragment.
    pub(crate) fn read_in_phase(&mut self, input: &mut &[u8], phase: Option<Phase>) -> Result<Option<Incoming>, Error> {
        self.fragment_progress = false;
        loop {
            if let Some(incoming) = self.read_record(input, phase)? {
                return Ok(Some(incoming));
            }
            if input.is_empty() {
                return Ok(None);
            }
        }
    }
    fn read_record(&mut self, input: &mut &[u8], phase: Option<Phase>) -> Result<Option<Incoming>, Error> {
        let Some(record) = self.hop_in.read(input, Layer::Hop.maximum_ciphertext())? else {
            return Ok(None);
        };
        match record.header.kind {
            6 => {
                let report = link::decode(&record.plaintext).map_err(|_| Error::Heartbeat)?;
                return Ok(Some(Incoming::Heartbeat(report)));
            }
            5 => {}
            _ => return Err(Error::Kind),
        }
        let content = self.content_in.open(&record.plaintext)?;
        if matches!(content.header.kind, 1 | 2 | 9) {
            match phase {
                Some(Phase::NegotiatingMedia | Phase::Active) => {},
                Some(Phase::Ended) => return Ok(None),
                _ => return Err(Error::State),
            }
        }
        let flags = content.header.flags;
        let busy = self.reassembler.in_progress();
        let packet = |raw: Vec<u8>| -> Result<Incoming, Error> {
            let p = media::Packet::decode(&raw, media::MAX_PAYLOAD)?;
            if p.flags != flags {
                return Err(Error::Media(media::Error::Header));
            }
            Ok(Incoming::Media {
                flags,
                pts_us: p.pts_us,
                payload: p.payload.to_vec(),
            })
        };
        match content.header.kind {
            2 => {
                if busy && flags & AUDIO == 0 {
                    return Err(Error::Fragment);
                }
                packet(content.plaintext).map(Some)
            }
            9 => {
                let whole = self.reassembler.accept(flags, &content.plaintext)?;
                let incoming = whole.map(packet).transpose()?;
                self.fragment_progress = true;
                Ok(incoming)
            }
            1 if busy => Err(Error::Fragment),
            _ if busy && !self.fragment_controls => Err(Error::Fragment),
            1 => Ok(Some(Incoming::Header(media::VideoHeader::decode(
                &content.plaintext,
            )?))),
            5 if content.plaintext.is_empty() => Ok(Some(Incoming::KeyFrameRequest)),
            6 => {
                let message = Message::decode(&content.plaintext)?;
                if self.fragment_controls && matches!(message.signal, Signal::HangUp | Signal::Cancel | Signal::Decline) {
                    self.reassembler = fragments::Reassembler::default();
                }
                Ok(Some(Incoming::Signal(message)))
            },
            11 => Ok(Some(Incoming::Discovery(content.plaintext))),
            10 => Ok(Some(Incoming::Labels(content.plaintext))),
            _ => Err(Error::Kind),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod renewal_bit {
    /// Android's in-call link renewal (Design SR) is negotiated by end-to-end
    /// hello transport bit 4 (0x10). Windows does not implement it, so its
    /// end-to-end hellos never set that bit (a peer then never offers it).
    #[test]
    fn the_end_to_end_hello_never_sets_the_renewal_bit() {
        for base in [crate::incoming::CAPABILITIES.transport, 0x0c] {
            let transport = super::fragments::endpoint_transport(base) | crate::labels::CAPABILITY_BIT;
            assert_eq!(transport & 0x10, 0, "{transport:#04x}");
        }
    }
}
