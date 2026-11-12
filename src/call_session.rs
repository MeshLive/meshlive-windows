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
    call::carriers::{CarrierEnd, Extra},
    framing::HopReader,
    label_policy::SendMoment,
    link::Liveness,
    listen::{self, AudioReceiver, UnitDecoder},
    media::{self, AUDIO, VideoHeader},
    opus,
    playout::{Clock, Profile},
    records::Sealer,
    rotating::{self, OpeningEntry, Selection},
    viewer::{self, VideoReceiver},
    wifi::{
        pulse::Pulse,
        wire::{self, Bwf1},
    },
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
    /// WP6 (Android `endBlockedAfterReveal`): the peer's authenticated
    /// identity (its REVEAL, or its handshake real key) is a person this PC
    /// blocked. Nothing more of this PC's goes to them and their records are
    /// ignored; the ordinary end follows its delay (`blocked_ended`). Carries
    /// no identity.
    PeerBlocked,
    /// A BDX1 RECOGNITION_OFFER or PRESENCE_LEASE arrived: the owner's
    /// `CallPresence` takes it with `take_presence_records`.
    PresenceRecord,
    /// A verified peer label (CF-REC-43): kind 1 name (the kind 3 call subject is retired and dropped by the inbox).
    PeerLabel { kind: u8, text: String },
    /// A `BWF1` record or event that belongs to the Wi-Fi owner.
    Wifi(WifiNotice),
}

/// One record to write on a carrier (0 = Bluetooth, otherwise the Wi-Fi attempt id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub carrier: i32,
    pub bytes: Vec<u8>,
}

/// Wi-Fi events the session hands to the attempt owner.
#[derive(Clone, PartialEq, Eq)]
pub enum WifiNotice {
    PeerCaps(wire::Caps),
    PeerDecline { ticket: [u8; 8], reason: u8 },
    PeerClose { reason: u8 },
    /// A HOST from the peer. The host-role owner answers DECLINE 1; the guest-role owner (W4G) may join.
    PeerHost(wire::Host),
    /// W4G: a CREDS from the phone; only a PC whose CAPS offered the client bit (3) gets one.
    PeerCreds(wire::Creds),
    /// The effective sending carrier changed (0 = Bluetooth).
    CarrierChanged { send_on: i32 },
    /// Wi-Fi ended for this call after a `BWF1` fault; the call goes on.
    WifiEnded { reason: &'static str },
}
impl std::fmt::Debug for WifiNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PeerCaps(caps) => write!(f, "PeerCaps({caps:?})"),
            Self::PeerDecline { ticket, reason } => write!(
                f,
                "PeerDecline {{ ticket: {}, reason: {reason} }}",
                crate::wifi::log::tag(ticket)
            ),
            Self::PeerClose { reason } => write!(f, "PeerClose {{ reason: {reason} }}"),
            Self::PeerHost(host) => write!(f, "PeerHost({host:?})"),
            Self::PeerCreds(creds) => write!(f, "PeerCreds({creds:?})"),
            Self::CarrierChanged { send_on } => {
                write!(f, "CarrierChanged {{ send_on: {send_on} }}")
            }
            Self::WifiEnded { reason } => write!(f, "WifiEnded {{ reason: {reason} }}"),
        }
    }
}

/// Controls are sealed only when written, so wire order equals record sequence.
enum Control {
    Signal(Message),
    Header,
    KeyFrameRequest,
    Discovery(Vec<u8>),
    Labels(Vec<u8>),
    /// WP6NF/WP6NG: an early name (the caller's "Now" or after-OPENING, the
    /// callee's Ringing) that needs the peer in the loaded contact list when
    /// it is written; otherwise it waits for the given stranger's moment.
    LabelsForContact(Vec<u8>, SendMoment),
    /// Design SR: one RENEW record (BME1 content type 12).
    Renew(Vec<u8>),
    Wifi(i32, Bwf1),
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

/// WP6: the ordinary end of a call with a blocked person is queued its delay
/// (3,000-7,000 ms, Android `CallBlocking.declineDelayMs`) after detection; by
/// detection + `BLOCKED_END_LATEST_MS` + `BLOCKED_WRITE_BUDGET_MS` the owner
/// ends the call locally whatever the link does (`blocked_overdue`).
pub const BLOCKED_END_LATEST_MS: u64 = 7_000;
/// WP6: every write after a blocked person was found is bounded to this
/// (`dial::Dialed::flush`).
pub const BLOCKED_WRITE_BUDGET_MS: u64 = 300;

/// WP6NG finding 2: what became of a blocked call's ordinary end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedEndState {
    /// Not due yet.
    Pending,
    /// Queued behind the controls.
    Queued,
    /// Sealed into the record the writer is handing to the carrier.
    Sealed,
    /// The carrier took it (`note_written`).
    Written,
    /// Nothing could be queued: the call had already ended.
    Unsendable,
}

struct BlockedEnd {
    due_ns: u64,
    latest_ns: u64,
    ended: bool,
    state: BlockedEndState,
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

/// Experimental call audio coalescing (HEALTH-12; Android
/// `test_call_audio_coalesce`, default on there): `MESHLIVE_CALL_AUDIO_COALESCE=1`
/// sends the ordinary 20 ms Opus units already waiting in the call queue as one
/// existing `BTA1` pair or `BTA2` batch record. No added delay (nothing waits
/// for audio not yet captured), no wire change. Off by default; read once.
pub fn audio_coalesce_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var(AUDIO_COALESCE_SWITCH).ok().as_deref().map(str::trim) == Some("1"))
}
pub const AUDIO_COALESCE_SWITCH: &str = "MESHLIVE_CALL_AUDIO_COALESCE";

/// Android `coalesceQueuedCallAudio`: `first` plus the ordinary audio units
/// already queued behind it, as one pair or batch record, and the number of
/// units it carries. Each added unit must be ordinary audio (flags exactly
/// `AUDIO`: never configuration or an already-packed record), at most 16 KiB,
/// 0 to 1 s after the first, and at most 20 units go in one record; the first
/// unit that does not qualify stays queued. With nothing queued, or a `first`
/// that does not qualify, `first` is returned unchanged.
pub fn coalesce_queued_audio(first: Vec<u8>, queue: &mut VecDeque<Vec<u8>>) -> (Vec<u8>, usize) {
    let packed = {
        let Ok(head) = media::Packet::decode(&first, media::MAX_PAYLOAD) else { return (first, 1) };
        let ordinary = |unit: &media::Packet<'_>| unit.flags == AUDIO && unit.payload.len() <= 16 * 1024;
        if !ordinary(&head) || queue.is_empty() {
            return (first, 1);
        }
        let mut taken = 0;
        while taken + 1 < media::MAX_AUDIO_BATCH_UNITS {
            let Some(next) = queue.get(taken).and_then(|raw| media::Packet::decode(raw, media::MAX_PAYLOAD).ok()) else { break };
            let delta = next.pts_us.checked_sub(head.pts_us);
            if !ordinary(&next) || !delta.is_some_and(|d| (0..=media::MAX_AUDIO_GAP_US).contains(&d)) {
                break;
            }
            taken += 1;
        }
        if taken == 0 {
            return (first, 1);
        }
        let mut units = vec![head];
        units.extend(queue.iter().take(taken).filter_map(|raw| media::Packet::decode(raw, media::MAX_PAYLOAD).ok()));
        match media::pack_audio(&units) {
            Ok(packed) => (packed, taken),
            Err(_) => return (first, 1),
        }
    };
    queue.drain(..packed.1);
    (packed.0, packed.1 + 1)
}

pub struct CallSession<D> {
    controller: Controller,
    media_gate: bool,
    /// `audio_coalesce_enabled()` when the session was made.
    audio_coalesce: bool,
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
    /// Coalescing: records that combined queued units, and the units they carried.
    audio_coalesced_records: u64,
    audio_coalesced_units: u64,
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
    wifi_notices: VecDeque<WifiNotice>,
    wifi_ends: Vec<CarrierEnd>,
    wifi_ended: bool,
    reveal_peer_verified: bool,
    reveal_own_sent: bool,
    local_caps: wire::Caps,
    wifi_last_send_on: i32,
    pending_notices: Vec<Notice>,
    /// WP4/WP6: the policy this call is screened against (default: the app's).
    policy: std::sync::Arc<crate::call_policy::CallPolicy>,
    /// The policy generation this call last screened against.
    policy_generation: u64,
    /// The peer's authenticated full fingerprint: its handshake real key or
    /// its verified REVEAL.
    peer_fp: Option<[u8; 32]>,
    /// When the input being handled arrived (a blocked person's delay starts there).
    input_ns: u64,
    /// WP6: Some once the peer is known to be a blocked person.
    blocked: Option<BlockedEnd>,
    /// Tests: a fixed blocked-end delay instead of the random 3-7 s.
    blocked_delay_ms: Option<u64>,
    /// WP4: this PC's name label waiting for its moment (with Accept, after
    /// the OPENING, or after the peer's verified REVEAL).
    held_name: Option<(SendMoment, Vec<u8>)>,
    /// HangUp, Decline and Cancel written on a Wi-Fi carrier: re-sent on the
    /// surviving carrier when that carrier ends (Android A3 requeues them).
    wifi_end_signals: Vec<(i32, Message)>,
    /// Design SR: the offerer/armed state machine of this call and the
    /// actions it left for the owner (scan, advertise, connect, ...).
    renewal: crate::call_renewal::Renewal,
    renewal_out: Vec<crate::call_renewal::Action>,
    renewal_env: RenewalEnv,
    /// The LABELS container this side wrote, for the post-BIND re-assertion.
    own_labels: Option<Vec<u8>>,
    /// When the call became ACTIVE (this session's clock).
    active_since_ns: Option<u64>,
    /// When a record last authenticated on carrier 0.
    last_heard_ns: u64,
}

/// CF-MEDIA-22 call reports are on (alternative off-switch: `MESHLIVE_CALL_REPORTS=0`).
pub fn reports_enabled() -> bool {
    std::env::var("MESHLIVE_CALL_REPORTS").ok().as_deref() != Some("0")
}

/// What the owner knows about the transport that the session cannot (Design SR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenewalEnv {
    /// The hop link is GATT (not Wi-Fi, CoC or Classic) and carrier 0 is not retired.
    pub gatt: bool,
    /// A Wi-Fi attempt is in progress (RD12: the PC refuses to renew meanwhile).
    pub wifi_attempt: bool,
    /// This session's clock at the last Bluetooth write that completed.
    pub last_write_ok_ns: Option<u64>,
}

/// Queue and delivery counters, for the owner's per-second log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub audio_queued: usize,
    pub video_queued: usize,
    /// Bytes of the queued video records (the app backlog in bytes).
    pub video_queued_bytes: usize,
    /// Media packets written to the link (a fragmented packet counts once;
    /// a coalesced audio record counts once).
    pub audio_sent: u64,
    /// Coalescing: audio records that combined queued units, and those units.
    pub audio_coalesced_records: u64,
    pub audio_coalesced_units: u64,
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
    /// Design SR: queue one RENEW record, ordered with the signals.
    pub fn send_renew(&mut self, body: Vec<u8>) {
        self.controls.push_back(Control::Renew(body));
    }
    /// The end-to-end content sequence the next record will carry.
    pub fn next_content_sequence(&self) -> u64 {
        self.link.next_content_sequence()
    }
    /// Queues a BDX1 record ahead of everything else: a callee that
    /// presented its real key opens the selected RDI with its OPENING as its
    /// first end-to-end record (RV15).
    pub fn send_discovery_first(&mut self, record: Vec<u8>) {
        self.controls.push_front(Control::Discovery(record));
    }
    /// Design SR: `K_renew` exists (both endpoint hellos signed bit 4).
    pub fn renewal_negotiated(&self) -> bool {
        self.renewal.has_key()
    }
    /// A renewal runs: it owns the call's liveness and no Wi-Fi attempt starts meanwhile.
    pub fn renewal_running(&self) -> bool {
        self.renewal.running()
    }
    pub fn renewal_phase(&self) -> crate::call_renewal::Phase {
        self.renewal.phase()
    }
    /// A copy of `K_renew` for the opening (zeroed on drop).
    pub fn renewal_key(&self) -> Option<zeroize::Zeroizing<[u8; 32]>> {
        self.link.renewal_key()
    }
    pub fn renewal_call_id(&self) -> [u8; 16] {
        self.controller.call_id()
    }
    pub fn renewal_ticket(&self) -> [u8; 8] {
        self.renewal.ticket()
    }
    pub fn renewal_started_ms(&self) -> u64 {
        self.renewal.started_ms()
    }
    /// Test only: opt this session into renewal after construction.
    #[cfg(test)]
    pub(crate) fn test_enable_renewal(&mut self, k1: &[u8; 32], k2: &[u8; 32], direct: bool) {
        self.link.enable_renewal(k1, k2, direct).unwrap();
        if let Some(key) = self.link.renewal_key() {
            self.renewal.set_key(key);
        }
    }
    fn renewal_facts(&self, now_ns: u64) -> crate::call_renewal::Facts {
        crate::call_renewal::Facts {
            // The key exists only when this side's switch was on at the hello.
            enabled: self.renewal.has_key() && self.blocked.is_none(),
            relay_hops: self.controller.relay_hops(),
            direct: self.link.renewal_direct(),
            gatt: self.renewal_env.gatt,
            active: self.controller.phase() == call::Phase::Active,
            // A CallSession is never a verification session.
            verification: false,
            wifi_attempt: self.renewal_env.wifi_attempt,
            carrier_alive: self.link.carriers().is_some_and(|carriers| carriers.extra().is_some() && !carriers.ended()),
            active_ms: self.active_since_ns.map_or(0, |at| now_ns.saturating_sub(at) / 1_000_000),
            silence_ms: now_ns.saturating_sub(self.last_heard_ns) / 1_000_000,
            write_age_ms: self.renewal_env.last_write_ok_ns.map(|at| now_ns.saturating_sub(at) / 1_000_000),
        }
    }
    /// RENEW records go out on the old link in order with the signals; the
    /// rest are for the owner.
    fn renewal_apply(&mut self, actions: Vec<crate::call_renewal::Action>) -> Vec<crate::call_renewal::Action> {
        let mut rest = Vec::new();
        for action in actions {
            match action {
                crate::call_renewal::Action::Send(op) => {
                    self.controls.push_back(Control::Renew(crate::call_renewal::encode(&op)));
                }
                other => rest.push(other),
            }
        }
        rest
    }
    /// Per worker tick: the call's facts in, the owner's actions out.
    pub fn renewal_tick(&mut self, now_ns: u64, env: RenewalEnv) -> Vec<crate::call_renewal::Action> {
        self.renewal_env = env;
        let phase = self.controller.phase();
        match (phase == call::Phase::Active, self.active_since_ns) {
            (true, None) => self.active_since_ns = Some(now_ns),
            (false, Some(_)) => self.active_since_ns = None,
            _ => {}
        }
        let mut out = std::mem::take(&mut self.renewal_out);
        if phase == call::Phase::Ended {
            let actions = self.renewal.end();
            out.extend(self.renewal_apply(actions));
            return out;
        }
        let facts = self.renewal_facts(now_ns);
        let actions = self.renewal.tick(now_ns / 1_000_000, &facts);
        out.extend(self.renewal_apply(actions));
        out
    }
    /// A RENEW record from the peer. A malformed one fails closed like every BME1 record.
    fn on_renew(&mut self, body: &[u8], now_ns: u64) -> Result<(), Error> {
        if self.blocked.is_some() {
            return Ok(());
        }
        let op = crate::call_renewal::decode(body).map_err(|_| Error::Call(call::Error::Format))?;
        let facts = self.renewal_facts(now_ns);
        let actions = self.renewal.on_record(now_ns / 1_000_000, &op, &facts);
        let rest = self.renewal_apply(actions);
        self.renewal_out.extend(rest);
        Ok(())
    }
    /// The owner's reports back to the machine; each returns the next actions.
    pub fn renewal_found(&mut self, now_ns: u64) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_found(now_ns / 1_000_000);
        self.renewal_apply(actions)
    }
    pub fn renewal_scan_failed(&mut self, now_ns: u64) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_scan_failed(now_ns / 1_000_000);
        self.renewal_apply(actions)
    }
    pub fn renewal_connect_failed(&mut self, now_ns: u64) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_connect_failed(now_ns / 1_000_000);
        self.renewal_apply(actions)
    }
    pub fn renewal_registered(&mut self, now_ns: u64, ok: bool) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_registered(now_ns / 1_000_000, ok);
        self.renewal_apply(actions)
    }
    pub fn renewal_advertising(&mut self, now_ns: u64, ok: bool) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_advertising(now_ns / 1_000_000, ok);
        self.renewal_apply(actions)
    }
    /// The attempt cannot go on (an invalid first record on the new link, no free
    /// connection): the old link and the call stay as they are.
    pub fn renewal_failed(&mut self, why: &'static str) -> Vec<crate::call_renewal::Action> {
        let actions = self.renewal.on_failed(why);
        self.renewal_apply(actions)
    }
    /// Log names: which side of the attempt this session is.
    pub fn renewal_role(&self) -> &'static str {
        self.renewal.role()
    }
    /// The silence on carrier 0 so far, for the `call_renewal_silence` line.
    pub fn renewal_silence_ms(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.last_heard_ns) / 1_000_000
    }
    /// The old Bluetooth link failed while a renewal owned the call's liveness.
    pub fn renewal_old_link_failed(&mut self) {
        self.renewal.on_old_link_failed();
    }
    /// The new link's hop layer from the opening's hop keys (bytes 0-31 are
    /// offerer to armed, 32-63 armed to offerer).
    pub fn renewal_hop_layer(&self, offerer: bool, keys: &[u8; 64]) -> Result<(Sealer, HopReader), Error> {
        let mut first = zeroize::Zeroizing::new([0u8; 32]);
        let mut second = zeroize::Zeroizing::new([0u8; 32]);
        first.copy_from_slice(&keys[..32]);
        second.copy_from_slice(&keys[32..]);
        let (send, receive) = if offerer { (&first, &second) } else { (&second, &first) };
        Ok(self.link.new_hop_layer(send, receive)?)
    }
    /// Offerer: BIND, sealed in one step as the new link's first record. Never
    /// after the 12 s bound.
    pub fn renewal_seal_bind(&mut self, hop_out: &mut Sealer, now_ns: u64) -> Result<Vec<u8>, Error> {
        if !self.renewal.bind_allowed(now_ns / 1_000_000) {
            return Err(Error::Call(call::Error::State));
        }
        let ticket = self.renewal.ticket();
        Ok(self.link.seal_bind(&ticket, hop_out)?)
    }
    /// Armed: the first record of the proved connection (its type-5 plaintext)
    /// is the BIND of the current ticket, inside the bound.
    pub fn renewal_accept_bind(&mut self, inner: &[u8], now_ns: u64) -> bool {
        let ticket = self.renewal.ticket();
        if !self.renewal.accept_bind_allowed(now_ns / 1_000_000, &ticket) {
            return false;
        }
        self.link.open_bind(inner, &ticket).is_some()
    }
    /// BIND written (offerer) or accepted (armed): the new hop layer carries
    /// the call, liveness starts afresh, and media state is re-asserted.
    pub fn renewal_bound(&mut self, hop_out: Sealer, hop_in: HopReader, now_ns: u64) -> Vec<crate::call_renewal::Action> {
        let now_ms = now_ns / 1_000_000;
        self.link.rebind_hop(hop_out, hop_in);
        self.liveness = Liveness::new(now_ns);
        self.last_heard_ns = now_ns;
        // What was queued for the old link (RENEW records) is lost with it, and
        // so is a partly sent or received video frame.
        self.controls.retain(|control| !matches!(control, Control::Renew(_)));
        self.fragmenting = None;
        self.link.suppress_fragments();
        let actions = self.renewal.on_bound(now_ms);
        // SR11: current microphone and camera, a key frame both ways, and this
        // side's own LABELS once if it wrote one (authorised again when written).
        if let Ok(messages) = self.controller.reassert() {
            for message in messages {
                self.controls.push_back(Control::Signal(message));
            }
        }
        self.ask_keyframe();
        self.pending_notices.push(Notice::KeyFrameRequested);
        if let Some(container) = self.own_labels.clone() {
            self.controls.push_back(Control::Labels(container));
        }
        // A Wi-Fi carrier is never touched; its cutover learns the Bluetooth standby was replaced.
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.standby_replaced(now_ms as i64);
        }
        self.apply_carrier_effects(now_ns);
        self.renewal_apply(actions)
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
    /// WP4 (Android `callerPlan` / `calleeMoment`, `label_policy`): this PC's
    /// name for this call at its moment. Nothing unless LABELS were
    /// negotiated or for a blocked person; `Never` drops it; `Now` goes before
    /// the Invite and `AtRinging` after Ringing; the other moments wait here.
    /// Every record is authorised again when written (newest policy).
    pub fn plan_own_name(&mut self, moment: SendMoment, container: Vec<u8>) {
        if self.labels.is_none() || self.blocked.is_some() {
            return;
        }
        match moment {
            SendMoment::Never => {}
            // WP6NG finding 9: the early name (a contact's) keeps its
            // condition; if the contact is gone when it is written, it waits
            // for the answer like a stranger's.
            SendMoment::Now => self.controls.push_front(Control::LabelsForContact(container, SendMoment::AfterReveal)),
            // WP6NF finding 5: the Ringing-time name keeps its condition (a
            // caller in the loaded contact list) until it is written.
            SendMoment::AtRinging => self.controls.push_back(Control::LabelsForContact(container, SendMoment::AtAccept)),
            later => self.held_name = Some((later, container)),
        }
    }
    /// WP4: the held name after the peer's verified, unblocked REVEAL: the
    /// Accept-time name held for it (once this PC accepted) and the
    /// after-reveal names; the write decides whether it may go.
    fn release_name_after_reveal(&mut self) {
        let release = match self.held_name.as_ref().map(|(moment, _)| *moment) {
            Some(SendMoment::AtAccept) => self.answered(),
            Some(SendMoment::AfterReveal | SendMoment::AfterRevealIfVerified) => true,
            _ => false,
        };
        if release && let Some((_, container)) = self.held_name.take() {
            self.send_labels(container);
        }
    }
    /// WP4: the held name once a relayed callee's OPENING opened the selected offer.
    fn release_name_after_opening(&mut self) {
        if self.held_name.as_ref().is_some_and(|(moment, _)| *moment == SendMoment::AfterOpening)
            && let Some((_, container)) = self.held_name.take()
            && self.labels.is_some()
        {
            // WP6NG finding 9: the after-OPENING name is a contact's: it keeps
            // that condition until written.
            self.controls.push_back(Control::LabelsForContact(container, SendMoment::AfterReveal));
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
    /// WP4/WP6: screen this call against `policy` instead of the app's (tests
    /// and tools).
    pub fn use_policy(&mut self, policy: std::sync::Arc<crate::call_policy::CallPolicy>) {
        self.policy_generation = policy.generation();
        self.policy = policy;
    }
    /// WP6: a peer that authenticated its real key in the handshake (no
    /// REVEAL will come), screened now, before any name, media or presence; a
    /// blocked one gets the ordinary end like a blocked REVEAL.
    pub fn screen_peer(&mut self, fingerprint: [u8; 32], now_ns: u64) {
        self.peer_fp = Some(fingerprint);
        if self.policy.snapshot().is_blocked(&fingerprint) {
            self.block_now(now_ns);
            self.pending_notices.push(Notice::PeerBlocked);
        }
    }
    /// WP6: the peer is known to be a person this PC blocked.
    pub fn peer_blocked(&self) -> bool {
        self.blocked.is_some()
    }
    /// WP6: ... and the ordinary end has been handed to the link.
    pub fn blocked_ended(&self) -> bool {
        self.blocked.as_ref().is_some_and(|b| b.ended)
    }
    /// WP6: the hard local deadline passed: the owner ends the call locally
    /// even if the end could not be written.
    pub fn blocked_overdue(&self, now_ns: u64) -> bool {
        self.blocked.as_ref().is_some_and(|b| now_ns >= b.latest_ns)
    }
    /// WP6NG finding 2: what became of the blocked end (None: not blocked).
    pub fn blocked_end_state(&self) -> Option<BlockedEndState> {
        self.blocked.as_ref().map(|b| b.state)
    }
    /// WP6NG finding 2: the owner's carrier took the record just returned; a
    /// sealed blocked end is now written.
    pub fn note_written(&mut self) {
        if let Some(blocked) = self.blocked.as_mut()
            && blocked.state == BlockedEndState::Sealed
        {
            blocked.state = BlockedEndState::Written;
        }
    }
    /// WP6NF: the time left before the hard local deadline (None: not blocked).
    pub fn blocked_remaining_ns(&self, now_ns: u64) -> Option<u64> {
        self.blocked.as_ref().map(|b| b.latest_ns.saturating_sub(now_ns))
    }
    /// WP6NF: applies the newest policy now: a known peer that became blocked
    /// latches at once (before a write budget is chosen, an Accept is sent or
    /// media starts). Held and queued names and presence records are checked
    /// again when written.
    pub fn refresh_policy(&mut self, now_ns: u64) {
        let generation = self.policy.generation();
        if generation == self.policy_generation {
            return;
        }
        self.policy_generation = generation;
        if let Some(fingerprint) = self.peer_fp
            && self.blocked.is_none()
            && self.policy.snapshot().is_blocked(&fingerprint)
        {
            self.block_now(now_ns);
            self.pending_notices.push(Notice::PeerBlocked);
        }
    }
    #[cfg(test)]
    pub(crate) fn set_blocked_delay_ms(&mut self, ms: u64) {
        self.blocked_delay_ms = Some(ms);
    }
    /// WP6: the peer is a blocked person. Nothing more of this PC's goes to
    /// them: every queued control but the call signals is purged (no name,
    /// header, BDX1, keyframe request or Wi-Fi record), a held start is
    /// dropped, queued and received media cleared; the end follows its delay.
    fn block_now(&mut self, now_ns: u64) {
        if self.blocked.is_some() {
            return;
        }
        self.held_name = None;
        self.caller_reveal = None;
        self.controls.retain(|c| matches!(c, Control::Signal(_)));
        self.audio_out.clear();
        self.video_out.clear();
        self.fragmenting = None;
        self.clear_received_media();
        // WP6NF/WP6NG finding 6: no new attempt, attach, CAPS or Close to a
        // blocked person (each refuses on `blocked`), but the carrier in use
        // stays usable for the ordinary end: a switch-off is a user fallback,
        // not a block fence, and the cutover's own markers (no content) still
        // go so the peer reads the carrier the end is on.
        let delay_ms = self.blocked_delay_ms.unwrap_or_else(crate::blocking::delay_ms);
        self.blocked = Some(BlockedEnd {
            due_ns: now_ns.saturating_add(delay_ms * 1_000_000),
            latest_ns: now_ns.saturating_add((BLOCKED_END_LATEST_MS + BLOCKED_WRITE_BUDGET_MS) * 1_000_000),
            ended: false,
            state: BlockedEndState::Pending,
        });
    }
    /// WP6: Hang-up Normal once answered, else Cancel (caller) or Decline
    /// Rejected (callee): the ends any call has, so nothing says "blocked".
    fn end_blocked(&mut self) -> bool {
        self.hang_up(Reason::Normal).is_ok() || self.cancel().is_ok() || self.decline(Reason::Rejected).is_ok()
    }
    /// WP6: a presence OFFER or LEASE may go to this peer now: never to a
    /// blocked person or to one the loaded contact list no longer holds.
    fn presence_allowed(&self) -> bool {
        if self.blocked.is_some() {
            return false;
        }
        // WP6NF finding 2: only an identified peer in the loaded contact list;
        // an unknown list or an unidentified peer gets nothing (fail closed).
        let Some(fingerprint) = self.peer_fp else { return false };
        let policy = self.policy.snapshot();
        !policy.is_blocked(&fingerprint) && policy.is_contact(&fingerprint)
    }
    fn on_discovery(&mut self, record: &[u8], notices: &mut Vec<Notice>) -> Result<(), Error> {
        // WP6: nothing more from a blocked person is acted on (no presence
        // record, no second REVEAL).
        if self.blocked.is_some() {
            return Ok(());
        }
        if let Some(callee) = self.callee_reveal.as_mut() {
            if record.get(4) == Some(&4) {
                if self.unconfirmed {
                    return Ok(());
                }
                let verified = callee.session.on_peer_reveal(record).map_err(|_| Error::Reveal)?.clone();
                crate::identity_gate::admit(&verified.node_id, &verified.fingerprint).map_err(|_| Error::Reveal)?;
                // WP6 (Android review 2026-10-27, finding 2): a block follows
                // the real identity, which a stranger's call shows only now;
                // checked against the newest list before anything else goes
                // to the person (this PC's REVEAL already went with Accept:
                // the protocol's callee-first order).
                self.peer_fp = Some(verified.fingerprint);
                if self.policy.snapshot().is_blocked(&verified.fingerprint) {
                    self.block_now(self.input_ns);
                    notices.push(Notice::PeerBlocked);
                    return Ok(());
                }
                self.reveal_peer_verified = true;
                notices.push(Notice::PeerRevealed {
                    node_id: verified.node_id,
                    fingerprint: verified.fingerprint,
                });
                // WP4: a name held for this REVEAL goes first (Android: labels, then local media).
                self.release_name_after_reveal();
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
        // WP6: a callee revealed as a blocked person gets no REVEAL, name or
        // media of this PC's; its Accept is not shown and the call ends after
        // its delay (Android checks this before the opening).
        self.peer_fp = Some(verified.fingerprint);
        if self.policy.snapshot().is_blocked(&verified.fingerprint) {
            self.block_now(self.input_ns);
            notices.push(Notice::PeerBlocked);
            return Ok(());
        }
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
        self.reveal_peer_verified = true;
        let entries: [OpeningEntry; 0] = [];
        let own = gate
            .session
            .send_own(gate.real.as_ref(), &entries)
            .map_err(|_| Error::Reveal)?;
        // The caller reveal goes ahead of everything else still queued.
        self.controls.push_front(Control::Discovery(own));
        // WP4: a name held for the callee's REVEAL follows it.
        self.release_name_after_reveal();
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
        let mut renewal = crate::call_renewal::Renewal::new(controller.call_id(), controller.role() == call::Role::Caller);
        if let Some(key) = link.renewal_key() {
            renewal.set_key(key);
        }
        Self {
            controller,
            media_gate: call::media_gate_enabled(),
            audio_coalesce: audio_coalesce_enabled(),
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
            audio_coalesced_records: 0,
            audio_coalesced_units: 0,
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
            wifi_notices: VecDeque::new(),
            wifi_ends: Vec::new(),
            wifi_ended: false,
            reveal_peer_verified: false,
            reveal_own_sent: false,
            local_caps: wire::Caps::default(),
            wifi_last_send_on: 0,
            pending_notices: Vec::new(),
            wifi_end_signals: Vec::new(),
            policy: crate::call_policy::CallPolicy::global(),
            policy_generation: crate::call_policy::CallPolicy::global().generation(),
            peer_fp: None,
            input_ns: now_ns,
            blocked: None,
            blocked_delay_ms: None,
            held_name: None,
            renewal,
            renewal_out: Vec::new(),
            renewal_env: RenewalEnv::default(),
            own_labels: None,
            active_since_ns: None,
            last_heard_ns: now_ns,
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
                    // WP6: never for a blocked person (no header, no capture).
                    if self.blocked.is_some() {
                        continue;
                    }
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
                    self.close_carriers();
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
        // WP6NG finding 1: never earlier than this session's own clock.
        let now_ns = self.input_ns.max(self.started_at.elapsed().as_nanos() as u64);
        self.accept_at(voice, now_ns)
    }
    /// Accept at `now_ns` on this call's clock (the owner passes it), so a
    /// block found here counts its delay from now (WP6NG finding 1).
    pub fn accept_at(&mut self, voice: bool, now_ns: u64) -> Result<Vec<Notice>, Error> {
        // WP6NF finding 4: the newest policy decides first; a peer blocked
        // while it rang gets no Accept, REVEAL, name or media; the ordinary
        // end follows its delay.
        self.refresh_policy(now_ns);
        if self.blocked.is_some() {
            return Ok(std::mem::take(&mut self.pending_notices));
        }
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
        // WP4: the Accept-time name goes right before the Accept (Android:
        // REVEAL, labels, ACCEPT). In a pseudonym<->pseudonym call it waits
        // for the caller's verified, unblocked REVEAL instead (WP6).
        if self.caller_reveal.is_none()
            && self.held_name.as_ref().is_some_and(|(moment, _)| *moment == SendMoment::AtAccept)
            && let Some((_, container)) = self.held_name.take()
        {
            self.send_labels(container);
        }
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
        // Design SR: a running renewal owns the link's liveness; the ordinary
        // rules apply again, at once, when it ends without a bind.
        let silent = self.liveness.dead(now_ns) && !self.renewal.owns_liveness();
        match self.link.carriers() {
            None => silent,
            Some(carriers) => carriers.ended() || (carriers.extra().is_none() && silent),
        }
    }
    fn call_dead(&self, now_ns: u64) -> bool {
        self.is_dead(now_ns)
    }
    /// Opts this session into `BWF1`; `UW` is derived from the exporter (not kept).
    pub fn enable_wifi(
        &mut self,
        exporter: &[u8; 32],
        initiator: bool,
        relay_edge: bool,
    ) -> Result<(), Error> {
        Ok(self.link.enable_wifi(exporter, initiator, relay_edge)?)
    }
    pub fn wifi_negotiated(&self) -> bool {
        self.link.wifi_enabled()
    }
    pub fn wifi_attempts_allowed(&self) -> bool {
        self.link
            .carriers()
            .is_some_and(|carriers| !carriers.relay_edge())
            && !self.wifi_ended
            && self.blocked.is_none()
    }
    pub fn wifi_set_local_caps(&mut self, caps: wire::Caps) {
        if !self
            .link
            .carriers()
            .is_some_and(|carriers| carriers.relay_edge())
        {
            self.local_caps = caps;
        }
    }
    pub fn wifi_caps_exchanged(&self) -> bool {
        self.link
            .carriers()
            .is_some_and(|carriers| carriers.caps_sent() && carriers.peer_caps().is_some())
    }
    /// Whether this PC's single `CAPS` has been written.
    pub fn wifi_caps_sent(&self) -> bool {
        self.link
            .carriers()
            .is_some_and(|carriers| carriers.caps_sent())
    }
    /// The `CAPS` value this PC sent (or will send).
    pub fn wifi_local_caps(&self) -> wire::Caps {
        self.local_caps
    }
    pub fn wifi_initiator(&self) -> bool {
        self.link
            .carriers()
            .is_some_and(|carriers| carriers.initiator())
    }
    pub fn wifi_fallbacks(&self) -> i32 {
        self.link
            .carriers()
            .map_or(0, |carriers| carriers.fallbacks())
    }
    /// The machine's switch-off is permanent for the call after a `Live` release.
    pub fn wifi_switched_off(&self) -> bool {
        self.link
            .carriers()
            .is_some_and(|carriers| carriers.switched_off_now())
    }
    pub fn wifi_peer_caps(&self) -> Option<wire::Caps> {
        self.link
            .carriers()
            .and_then(|carriers| carriers.peer_caps())
    }
    pub fn wifi_uw(&self) -> Option<&[u8; 32]> {
        self.link.carriers().map(|carriers| carriers.uw())
    }
    pub fn wifi_begin_attempt(&mut self, ticket: [u8; 8], now_ns: u64) -> Option<i32> {
        if self.wifi_ended || self.blocked.is_some() {
            return None;
        }
        self.link
            .carriers_mut()
            .and_then(|carriers| carriers.begin_attempt(ticket, (now_ns / 1_000_000) as i64))
    }
    pub fn wifi_attach(
        &mut self,
        id: i32,
        hop_out: Sealer,
        hop_in: HopReader,
        peer_ready: wire::Ready,
        now_ns: u64,
    ) -> bool {
        let now_ms = (now_ns / 1_000_000) as i64;
        let at_frame_boundary = self.fragmenting.is_none();
        // Carrier 0 is Bluetooth and ids above 62 are never attempts: refused;
        // so is any attach after a block (WP6NF finding 6: a late completion).
        if !(1..=62).contains(&id) || self.blocked.is_some() {
            return false;
        }
        let Some(carriers) = self.link.carriers_mut() else {
            return false;
        };
        let Some(ticket) = carriers.ticket_of(id) else {
            return false;
        };
        let attached = carriers.attach(
            Extra {
                id,
                ticket,
                hop_out,
                hop_in,
                peer_ready,
                pulse: Pulse::new(now_ns),
            },
            now_ms,
            at_frame_boundary,
        );
        self.apply_carrier_effects(now_ns);
        attached
    }
    pub fn wifi_carrier_died(&mut self, id: i32, now_ns: u64) {
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.carrier_died(id, (now_ns / 1_000_000) as i64);
        }
        self.apply_carrier_effects(now_ns);
    }
    pub fn wifi_switch_off(&mut self, now_ns: u64) {
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.switch_off((now_ns / 1_000_000) as i64);
        }
        self.apply_carrier_effects(now_ns);
    }
    pub fn wifi_quality_fallback(&mut self, now_ns: u64) {
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.quality_fallback((now_ns / 1_000_000) as i64);
        }
        self.apply_carrier_effects(now_ns);
    }
    pub fn wifi_standby_freshness(&mut self, fresh: bool, now_ns: u64) {
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.standby_freshness(fresh, (now_ns / 1_000_000) as i64);
        }
        self.apply_carrier_effects(now_ns);
    }
    /// Trace lines of the Wi-Fi path (`bwf1_ignored`, `marker_unknown_ticket`,
    /// `close_ignored`) for the worker; no ticket, key or address is in them.
    pub fn wifi_take_traces(&mut self) -> Vec<(&'static str, String)> {
        self.link
            .carriers_mut()
            .map(|carriers| carriers.take_traces())
            .unwrap_or_default()
    }
    /// A `BWF1` record that belongs to the call session.
    pub fn wifi_send(&mut self, record: wire::Bwf1) -> Result<(), Error> {
        if !self.wifi_negotiated() || self.wifi_ended || self.blocked.is_some() || !self.link.may_write_bwf1() {
            return Err(Error::Call(call::Error::State));
        }
        if record.validate().is_err() {
            return Err(Error::Call(call::Error::Wifi));
        }
        if !matches!(record, wire::Bwf1::Caps(_)) && !self.wifi_caps_exchanged() {
            return Err(Error::Call(call::Error::State));
        }
        self.controls.push_back(Control::Wifi(0, record));
        Ok(())
    }
    pub fn wifi_take_ends(&mut self) -> Vec<CarrierEnd> {
        std::mem::take(&mut self.wifi_ends)
    }
    pub fn wifi_old_alive(&self) -> bool {
        self.link
            .carriers()
            .is_none_or(|carriers| carriers.old_alive())
    }
    pub fn wifi_send_on(&self) -> i32 {
        self.link.carriers().map_or(0, |carriers| carriers.send_on())
    }
    pub fn wifi_video_on(&self) -> bool {
        self.controller.should_send_video() || self.video_in.is_some()
    }
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.pending_notices)
    }
    #[cfg(test)]
    pub(crate) fn test_carriers_mut(&mut self) -> Option<&mut call::carriers::Carriers> {
        self.link.carriers_mut()
    }
    #[cfg(test)]
    pub(crate) fn test_seal_bwf1(
        &mut self,
        carrier: i32,
        record: &crate::wifi::wire::Bwf1,
    ) -> Result<Vec<u8>, Error> {
        Ok(self.link.seal_bwf1(carrier, record)?)
    }
    pub(crate) fn standby_received_within(&self, now_ns: u64, window_ms: i64) -> bool {
        self.liveness.received_within(now_ns, window_ms)
    }
    /// The equivalent of "after both reveals verified" for pseudonymous links.
    fn reveal_done(&self) -> bool {
        if self.reveal.is_none() && self.callee_reveal.is_none() {
            return true;
        }
        self.reveal_peer_verified && self.reveal_own_sent
    }
    fn caps_due(&self) -> bool {
        let Some(carriers) = self.link.carriers() else {
            return false;
        };
        // WP6: no CAPS to a blocked person; the initiator, like the
        // responder, waits until both reveals are done.
        if carriers.caps_sent() || self.wifi_ended || self.blocked.is_some() {
            return false;
        }
        if carriers.initiator() {
            carriers.peer_caps().is_some() && self.reveal_done()
        } else {
            self.controller.phase() == call::Phase::Active && self.reveal_done()
        }
    }
    pub fn hang_up(&mut self, reason: Reason) -> Result<(), Error> {
        let message = self.controller.hang_up(reason)?;
        self.audio_out.clear();
        self.video_out.clear();
        self.fragmenting = None;
        self.clear_received_media();
        self.close_carriers();
        self.queue_signal(&message)
    }
    /// One `CLOSE` on carrier 0 before the hang-up signal, only while a
    /// Wi-Fi carrier is attached, carrier 0 is alive and the peer is proven to
    /// read `BWF1` (R-1, R-2). A call that never had a Wi-Fi carrier, a relay
    /// edge, or a peer that never proved `BWF1` gets no record of type 16.
    fn close_carriers(&mut self) {
        // WP6NF finding 6: no Wi-Fi record to a blocked person.
        if self.blocked.is_none()
            && self.link.may_write_bwf1()
            && self
                .link
                .carriers()
                .is_some_and(|carriers| carriers.extra().is_some() && carriers.old_alive())
        {
            self.controls.push_back(Control::Wifi(
                0,
                Bwf1::Close(wire::Close {
                    reason: wire::close::HANG_UP,
                }),
            ));
        }
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
        // WP6NF finding 6: no keyframe request to a blocked person.
        if self.blocked.is_none() && !self.controls.iter().any(|c| matches!(c, Control::KeyFrameRequest)) {
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
        // Held until the caller of a pseudonym<->pseudonym call is confirmed,
        // and never sent to a blocked person (WP6).
        if !allowed || self.caller_reveal.is_some() || self.blocked.is_some() {
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
    /// The next record to write and the carrier it belongs on.
    pub fn next_write_on(&mut self, now_ns: u64) -> Result<Option<Write>, Error> {
        self.link.set_clock(now_ns);
        loop {
            self.service_carriers(now_ns)?;
            // WP6NG finding 6: the cutover's markers (no content) keep the
            // carrier the ordinary end is on readable for the peer.
            let marker = self.link.carriers_mut().and_then(|carriers| carriers.take_marker());
            if let Some((carrier, record)) = marker {
                // Never write a marker to a carrier that is no longer there.
                if let Ok(bytes) = self.link.seal_bwf1(carrier, &record) {
                    return Ok(Some(Write { carrier, bytes }));
                }
                continue;
            }
            self.link.clear_target();
            match self.next_write_inner(now_ns)? {
                None => return Ok(None),
                Some(bytes) => match self.link.target() {
                    Some(carrier) => {
                        if carrier > 0
                            && let Some(extra) = self
                                .link
                                .carriers_mut()
                                .and_then(|carriers| carriers.extra_mut())
                                .filter(|extra| extra.id == carrier)
                        {
                            extra.pulse.sent(now_ns);
                        }
                        return Ok(Some(Write { carrier, bytes }));
                    }
                    None => continue,
                },
            }
        }
    }
    /// The next record bytes; only meaningful without Wi-Fi (the carrier is not reported).
    pub fn next_write(&mut self, now_ns: u64) -> Result<Option<Vec<u8>>, Error> {
        self.next_write_on(now_ns)
            .map(|write| write.map(|write| write.bytes))
    }
    /// The old writer body: seals one record, in order.
    fn next_write_inner(&mut self, now_ns: u64) -> Result<Option<Vec<u8>>, Error> {
        if self.call_dead(now_ns) {
            // WP6NG finding 2: a blocked person's delay owns the end: until it
            // is due the liveness close waits (the hard deadline bounds it).
            if self.blocked.as_ref().is_some_and(|b| !b.ended && now_ns < b.due_ns) {
                return Ok(None);
            }
            // PROTOCOL "Hang-up before a liveness close" (Android, default
            // on; MESHLIVE_CALL_LIVENESS_HANG_UP=0 is the control arm): once,
            // in an answered call, Hang-up/Error goes out before the close so a
            // peer that can still read ends at once instead of after its 8 s.
            if !self.liveness_hung_up && self.answered() && liveness_hang_up() {
                self.liveness_hung_up = true;
                // WP6NF finding 8: a blocked person's end is the ordinary one,
                // also when the link goes silent first.
                let reason = if self.blocked.is_some() { Reason::Normal } else { Reason::Error };
                let hung_up = self.controller.hang_up(reason);
                // WP6NG finding 2: the blocked end's real outcome.
                if let Some(blocked) = self.blocked.as_mut()
                    && !blocked.ended
                {
                    blocked.ended = true;
                    blocked.state = if hung_up.is_ok() { BlockedEndState::Sealed } else { BlockedEndState::Unsendable };
                }
                if let Ok(message) = hung_up {
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
        // WP4/WP6: the newest policy first (WP6NF: one place, `refresh_policy`).
        self.refresh_policy(now_ns);
        // WP6: the ordinary end, exactly its delay after the blocked person was found.
        if let Some(blocked) = self.blocked.as_mut()
            && !blocked.ended
            && now_ns >= blocked.due_ns
        {
            blocked.ended = true;
            // WP6NG finding 2: its real outcome: queued, or nothing could be.
            let state = if self.end_blocked() { BlockedEndState::Queued } else { BlockedEndState::Unsendable };
            if let Some(blocked) = self.blocked.as_mut() {
                blocked.state = state;
            }
        }
        let old_carrier_dead = self
            .link
            .carriers()
            .is_some_and(|carriers| !carriers.old_alive());
        if !old_carrier_dead && !self.sent_v1 {
            self.sent_v1 = true;
            self.liveness.sent(now_ns);
            return Ok(Some(self.link.heartbeat(None)?));
        }
        if !old_carrier_dead && self.liveness.heartbeat_due(now_ns) {
            return Ok(Some(self.link.heartbeat(None)?));
        }
        let wifi_heartbeat = self
            .link
            .carriers_mut()
            .and_then(|carriers| carriers.extra_mut())
            .and_then(|extra| {
                if extra.pulse.heartbeat_due(now_ns) {
                    extra.pulse.sent(now_ns);
                    Some(extra.id)
                } else {
                    None
                }
            });
        if let Some(id) = wifi_heartbeat {
            return Ok(Some(self.link.heartbeat_on(id, None)?));
        }
        if self.caps_due() {
            let caps = self.local_caps;
            if let Some(carriers) = self.link.carriers_mut() {
                carriers.mark_caps_sent();
            }
            // A record that cannot be sealed is not written (`target` None); the call goes on.
            return Ok(Some(self.link.seal_bwf1(0, &Bwf1::Caps(caps)).unwrap_or_else(|_| {
                self.link.target = None;
                Vec::new()
            })));
        }
        // Android writes one frame atomically except for queued audio.
        // Do not insert Camera off, labels or requests into its fragments.
        if (!self.ordered_fragments || self.fragmenting.is_none()) && let Some(control) = self.controls.pop_front() {
            return Ok(Some(match control {
                Control::Signal(message) => {
                    let record = self.link.signal(&message)?;
                    // WP6NG finding 2: the blocked end is sealed now.
                    if matches!(message.signal, Signal::HangUp | Signal::Decline | Signal::Cancel)
                        && let Some(blocked) = self.blocked.as_mut()
                        && blocked.state == BlockedEndState::Queued
                    {
                        blocked.state = BlockedEndState::Sealed;
                    }
                    if let Some(carrier) = self.link.target().filter(|carrier| *carrier > 0)
                        && matches!(message.signal, Signal::HangUp | Signal::Decline | Signal::Cancel)
                    {
                        self.wifi_end_signals.push((carrier, message));
                    }
                    record
                }
                Control::Header => self.link.header(&self.header)?,
                Control::KeyFrameRequest => self.link.keyframe_request()?,
                Control::Discovery(record) => {
                    // WP6: a presence OFFER or LEASE (a late presence task's
                    // too) never goes to a blocked person or to a peer the
                    // loaded contact list no longer holds: not written.
                    if record.len() > 4 && &record[..4] == b"BDX1" && matches!(record[4], 2 | 3) && !self.presence_allowed() {
                        self.link.target = None;
                        return Ok(Some(Vec::new()));
                    }
                    // This PC's REVEAL leaves now: the caller's deadline starts.
                    if record.get(4) == Some(&4) {
                        self.reveal_own_sent = true;
                        if let Some(wait) = self.caller_reveal.as_mut()
                            && wait.deadline.is_none()
                        {
                            wait.deadline = Some(now_ns.saturating_add(wait.budget_ns));
                        }
                    }
                    self.link.discovery(&record)?
                }
                Control::LabelsForContact(container, fallback) => {
                    let policy = self.policy.snapshot();
                    let contact = self.peer_fp.is_some_and(|fingerprint| policy.is_contact(&fingerprint));
                    if self.blocked.is_none() && contact && policy.name_allowed(self.peer_fp.as_ref()) {
                        self.own_labels = Some(container.clone());
                        self.link.labels(&container)?
                    } else {
                        // WP6NG finding 9: the early (contact-only) moment no
                        // longer applies: the name waits for a stranger's moment,
                        // where the write checks the audience again.
                        if self.blocked.is_none() && self.held_name.is_none() {
                            // WP6NG finding 9: if that stranger's moment already
                            // passed while this record waited (the OPENING and
                            // the Accept arrived in one read), send it now
                            // rather than holding it past its release.
                            let passed = match fallback {
                                SendMoment::AtAccept => self.answered(),
                                SendMoment::AfterReveal => self.answered() || self.reveal_peer_verified,
                                _ => false,
                            };
                            if passed {
                                self.send_labels(container);
                            } else {
                                self.held_name = Some((fallback, container));
                            }
                        }
                        self.link.target = None;
                        Vec::new()
                    }
                }
                Control::Labels(container) => {
                    // WP4/WP6: authorised again when written, against the
                    // newest audience, contacts and blocks; otherwise not written.
                    if self.blocked.is_some() || !self.policy.snapshot().name_allowed(self.peer_fp.as_ref()) {
                        self.link.target = None;
                        Vec::new()
                    } else {
                        self.own_labels = Some(container.clone());
                        self.link.labels(&container)?
                    }
                }
                Control::Renew(body) => self.link.renew(&body).unwrap_or_else(|_| {
                    // Not negotiated or not sealable: nothing is written, the call goes on.
                    self.link.target = None;
                    Vec::new()
                }),
                Control::Wifi(carrier, record) => self.link.seal_bwf1(carrier, &record).unwrap_or_else(|_| {
                    // Not sealed (peer not proven, invalid, carrier gone): nothing is written.
                    self.link.target = None;
                    Vec::new()
                }),
            }));
        }
        // Reports every 1 s once the call is answered, after queued signals and controls (they also keep the link alive).
        if !old_carrier_dead && reports_enabled() && self.answered() {
            let due = *self.next_report_ns.get_or_insert(now_ns);
            if now_ns >= due {
                self.next_report_ns = Some(now_ns + 1_000_000_000);
                self.liveness.sent(now_ns);
                let report = self.report(now_ns);
                return Ok(Some(self.link.heartbeat(Some(&report))?));
            }
        }
        if let Some(packet) = self.audio_out.pop_front() {
            let packet = if self.audio_coalesce {
                let (packed, units) = coalesce_queued_audio(packet, &mut self.audio_out);
                if units > 1 {
                    self.audio_coalesced_records += 1;
                    self.audio_coalesced_units += units as u64;
                }
                packed
            } else {
                packet
            };
            let payload = media::Packet::decode(&packet, media::MAX_PAYLOAD).ok().map(|p| p.payload.len());
            let mut records = self.link.media(&packet)?;
            if let Some(payload) = payload
                && self.link.target() == Some(0)
            {
                self.delivery.on_written(payload, now_ns);
            }
            self.audio_sent += 1;
            return Ok(records.pop());
        }
        if let Some((plan, index)) = self.fragmenting.take() {
            let record = self.link.seal_part(&plan, index)?;
            if index + 1 < plan.len() {
                self.fragmenting = Some((plan, index + 1));
            } else if self.link.target() == Some(0) {
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
            } else if self.link.target() == Some(0) {
                self.delivery.on_written(payload, now_ns);
            }
            return Ok(Some(record));
        }
        Ok(None)
    }
    /// Per-carrier liveness, the machine clock, and the effects it produces.
    fn service_carriers(&mut self, now_ns: u64) -> Result<(), Error> {
        let Some(_) = self.link.carriers() else {
            return Ok(());
        };
        let now_ms = (now_ns / 1_000_000) as i64;
        let boundary = self.fragmenting.is_none();
        let phase = self.controller.phase();
        let video_on = self.wifi_video_on();
        let bluetooth_dead = self.liveness.dead(now_ns) && !self.renewal.owns_liveness();
        {
            let carriers = self.link.carriers_mut().unwrap();
            let dead_id = carriers
                .extra()
                .filter(|extra| extra.pulse.dead(now_ns))
                .map(|extra| extra.id);
            if let Some(id) = dead_id {
                carriers.carrier_died(id, now_ms);
            }
            if carriers.extra().is_some() && bluetooth_dead && carriers.old_alive() {
                carriers.carrier_died(0, now_ms);
            }
            if phase == call::Phase::Active && video_on {
                carriers.on_call_active(now_ms, boundary);
            }
            carriers.tick(now_ms, boundary);
        }
        self.apply_carrier_effects(now_ns);
        if self.link.carriers().is_some_and(|carriers| carriers.ended()) {
            return Err(Error::Dead);
        }
        Ok(())
    }
    fn apply_carrier_effects(&mut self, _now_ns: u64) {
        let (effects, send_on) = {
            let Some(carriers) = self.link.carriers_mut() else {
                return;
            };
            (carriers.take_effects(), carriers.send_on())
        };
        if effects.reset_fragments {
            self.fragmenting = None;
        }
        if effects.suppress_fragments {
            self.link.suppress_fragments();
        }
        if effects.force_keyframe {
            self.pending_notices.push(Notice::KeyFrameRequested);
        }
        if effects.request_keyframe {
            self.ask_keyframe();
        }
        if effects.reassert {
            if let Ok(messages) = self.controller.reassert() {
                for message in messages {
                    self.controls.push_back(Control::Signal(message));
                }
            }
        }
        for end in &effects.closes {
            if end.id > 0 {
                self.requeue_end_signals(end.id);
            }
        }
        self.wifi_ends.extend(effects.closes);
        if send_on != self.wifi_last_send_on {
            self.wifi_last_send_on = send_on;
            self.wifi_notices
                .push_back(WifiNotice::CarrierChanged { send_on });
        }
    }
    /// A HangUp, Decline or Cancel last written on carrier `id` may have died
    /// with its socket: queue it again, ahead of everything, for the carrier
    /// that is left (the receiver's controller ignores a repeat).
    fn requeue_end_signals(&mut self, id: i32) {
        let mut again = Vec::new();
        self.wifi_end_signals.retain(|(carrier, message)| {
            if *carrier == id {
                again.push(*message);
                false
            } else {
                true
            }
        });
        for message in again.into_iter().rev() {
            self.controls.push_front(Control::Signal(message));
        }
    }
    fn credit_carrier(&mut self, carrier: i32, now_ns: u64) {
        if carrier == 0 {
            self.liveness.on_received(now_ns);
        } else if let Some(extra) = self
            .link
            .carriers_mut()
            .and_then(|carriers| carriers.extra_mut())
            .filter(|extra| extra.id == carrier)
        {
            extra.pulse.on_received(now_ns);
        }
    }
    fn wifi_trace(&mut self, phase: &'static str, line: String) {
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.trace(phase, line);
        }
    }
    fn on_wifi(
        &mut self,
        carrier: i32,
        record: Bwf1,
        now_ns: u64,
        notices: &mut Vec<Notice>,
    ) {
        if self.wifi_ended && !matches!(record, Bwf1::Close(_)) {
            self.wifi_trace("bwf1_ignored", format!("carrier={carrier} | reason=wifi_ended | kind={}", record.kind()));
            return;
        }
        match record {
            Bwf1::Caps(caps) => {
                if self
                    .link
                    .carriers()
                    .is_some_and(|carriers| carriers.peer_caps().is_some())
                {
                    self.end_wifi("caps_repeat", now_ns);
                } else {
                    if let Some(carriers) = self.link.carriers_mut() {
                        carriers.set_peer_caps(caps);
                    }
                    notices.push(Notice::Wifi(WifiNotice::PeerCaps(caps)));
                }
            }
            Bwf1::Decline(decline) => notices.push(Notice::Wifi(WifiNotice::PeerDecline {
                ticket: decline.ticket,
                reason: decline.reason,
            })),
            Bwf1::Close(close) => {
                self.wifi_trace("close_ignored", format!("carrier={carrier} | reason={}", close.reason));
                notices.push(Notice::Wifi(WifiNotice::PeerClose {
                    reason: close.reason,
                }))
            }
            Bwf1::Host(host) => notices.push(Notice::Wifi(WifiNotice::PeerHost(host))),
            Bwf1::Creds(creds) => {
                // W4G: a guest PC (its CAPS offered the client bit) joins with them;
                // CREDS to a host PC is still a violation that ends Wi-Fi for the call.
                if self.wifi_caps_sent()
                    && self.local_caps.bearers & wire::caps_bit::DIRECT_CLIENT != 0
                {
                    notices.push(Notice::Wifi(WifiNotice::PeerCreds(creds)));
                } else {
                    self.end_wifi("creds_to_host", now_ns);
                }
            }
            _ => {}
        }
    }
    fn end_wifi(&mut self, reason: &'static str, now_ns: u64) {
        if self.wifi_ended {
            return;
        }
        self.wifi_ended = true;
        if let Some(carriers) = self.link.carriers_mut() {
            carriers.switch_off((now_ns / 1_000_000) as i64);
        }
        self.wifi_notices
            .push_back(WifiNotice::WifiEnded { reason });
    }
    /// Feeds received link bytes; returns what the owner must act on.
    pub fn on_input(&mut self, input: &mut &[u8], now_ns: u64) -> Result<Vec<Notice>, Error> {
        self.on_input_from(0, input, now_ns)
    }
    /// Feeds bytes of one carrier (0 = Bluetooth, otherwise the attached Wi-Fi attempt).
    pub fn on_input_from(
        &mut self,
        carrier: i32,
        input: &mut &[u8],
        now_ns: u64,
    ) -> Result<Vec<Notice>, Error> {
        self.input_ns = now_ns;
        // WP6NF finding 4: the newest policy before anything of this input is acted on.
        self.refresh_policy(now_ns);
        self.link.set_clock(now_ns);
        let mut notices = Vec::new();
        loop {
            let phase = if self.media_gate { self.controller.phase() } else { call::Phase::Active };
            let incoming = match self.link.read_from(carrier, input, Some(phase)) {
                Ok(incoming) => incoming,
                Err(call::Error::Carrier(id)) => {
                    // Only carrier `id` closes (X5): what was decoded before
                    // its fault still reaches the owner (`take_notices`, or
                    // the next input), a HangUp's `Ended` included.
                    self.apply_carrier_effects(now_ns);
                    notices.extend(self.wifi_notices.drain(..).map(Notice::Wifi));
                    notices.append(&mut self.pending_notices);
                    self.pending_notices = notices;
                    return Err(Error::Call(call::Error::Carrier(id)));
                }
                Err(other) => return Err(Error::Call(other)),
            };
            let fragment_progress = self.link.take_fragment_progress();
            let heard = self.link.take_heard();
            if heard && carrier == 0 {
                self.last_heard_ns = now_ns;
                // Hearing the peer again (anything but RENEW) before the old link
                // was broken abandons a renewal.
                if !matches!(incoming, Some(Incoming::Renew(_))) {
                    let actions = self.renewal.on_peer_heard(now_ns / 1_000_000);
                    let rest = self.renewal_apply(actions);
                    self.renewal_out.extend(rest);
                }
            }
            let dual = self.link.carriers().is_some();
            if dual {
                // Liveness belongs to the carrier the record authenticated on
                // (markers and held records count), not to the item returned.
                if heard {
                    self.credit_carrier(carrier, now_ns);
                }
            } else if self.ordered_fragments && fragment_progress {
                self.liveness.on_received(now_ns);
            }
            let Some(incoming) = incoming else { break };
            if !dual {
                self.liveness.on_received(now_ns);
            }
            if !matches!(
                incoming,
                Incoming::Heartbeat(_) | Incoming::Wifi(_) | Incoming::WifiFault(_)
            ) && let Some(gate) = self.opening.take()
            {
                Self::check_opening(&gate, &incoming)?;
                // WP4: a name held for the relayed callee's OPENING follows it.
                self.release_name_after_opening();
                continue;
            }
            match incoming {
                Incoming::Heartbeat(report) => {
                    if carrier != 0 {
                        continue;
                    }
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
                Incoming::Labels(bytes) => {
                    // WP6: a blocked person's labels are not shown.
                    if self.blocked.is_none() {
                        notices.extend(self.on_labels(&bytes));
                    }
                }
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
                    // WP6: the Accept of a blocked person is not shown; the
                    // call ends shortly as if never answered.
                    // WP6NF finding 8: nor its own end: the owner reports the
                    // blocked outcome.
                    if !(self.blocked.is_some()
                        && matches!(message.signal, Signal::Accept | Signal::HangUp | Signal::Decline | Signal::Cancel))
                    {
                        self.remote.push(message);
                    }
                    notices.extend(self.apply(actions)?);
                    // Design decision 2026-11-06: a stranger met with its real key gets
                    // the name held for it once it accepted (no REVEAL comes).
                    // WP6NF: the production stranger route still installs a
                    // reveal gate for a real-key callee, so ask whether a
                    // REVEAL is actually required, not whether a gate exists.
                    if message.signal == Signal::Accept
                        && self.reveal.as_ref().is_none_or(|gate| !gate.session.peer_reveal_required())
                        && self.blocked.is_none()
                    {
                        self.release_name_after_reveal();
                    }
                }
                Incoming::Discovery(record) => self.on_discovery(&record, &mut notices)?,
                // An unconfirmed caller's header or media is dropped, and so
                // is a blocked person's (WP6).
                Incoming::Header(_) | Incoming::Media { .. } if self.caller_reveal.is_some() || self.blocked.is_some() => {}
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
                // Design SR: only a link that negotiated renewal delivers one.
                Incoming::Renew(body) => self.on_renew(&body, now_ns)?,
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
                    if flags & media::ROTATION == 0 && self.link.last_carrier() == 0 {
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
                Incoming::Wifi(record) => {
                    // WP6: a blocked person's Wi-Fi records are ignored.
                    if self.blocked.is_none() {
                        self.on_wifi(carrier, record, now_ns, &mut notices);
                    }
                }
                Incoming::WifiFault(reason) => self.end_wifi(reason, now_ns),
            }
        }
        self.apply_carrier_effects(now_ns);
        notices.extend(self.wifi_notices.drain(..).map(Notice::Wifi));
        notices.append(&mut self.pending_notices);
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
            audio_coalesced_records: self.audio_coalesced_records,
            audio_coalesced_units: self.audio_coalesced_units,
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
pub(crate) mod wifi_test_support;
#[cfg(test)]
mod tests;
