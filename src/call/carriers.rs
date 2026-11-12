//! The per-call carrier set: the cutover machine, the at-most-one Wi-Fi
//! carrier, the records it holds for delivery in order, and the marker queue.
//! It owns no sockets; `Dialed` closes sockets from the carrier-end queue.

use crate::{
    framing::HopReader,
    records::{Header, Layer, Policy, Sealer},
    wifi::{
        cutover::{self, Action, CarrierCutover, CloseReason, Config, OLD, Rec, Wire},
        pulse::Pulse,
        wire::{self, Bwf1},
    },
};
use std::collections::{HashMap, VecDeque};
use zeroize::Zeroizing;

/// A marker whose ticket names no attempt of this session (kept for the sim only).
pub const UNKNOWN_TICKET_CARRIER: i32 = 63;

/// The Wi-Fi carrier of a call while it exists (`BIND` and `READY` are finished).
pub struct Extra {
    pub id: i32,
    pub ticket: [u8; 8],
    pub hop_out: Sealer,
    pub hop_in: HopReader,
    pub peer_ready: wire::Ready,
    pub pulse: Pulse,
}
impl std::fmt::Debug for Extra {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extra")
            .field("id", &self.id)
            .field("ticket", &crate::wifi::log::tag(&self.ticket))
            .field("hop_out", &"<redacted>")
            .field("hop_in", &"<redacted>")
            .field("peer_ready", &self.peer_ready)
            .field("pulse", &self.pulse)
            .finish()
    }
}

#[derive(Default, PartialEq, Eq)]
pub struct Effects {
    pub reassert: bool,
    pub force_keyframe: bool,
    pub request_keyframe: bool,
    pub reset_fragments: bool,
    pub suppress_fragments: bool,
    /// The carrier-end queue (amendment 10).
    pub closes: Vec<CarrierEnd>,
    pub ended: Option<&'static str>,
}
impl std::fmt::Debug for Effects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effects")
            .field("reassert", &self.reassert)
            .field("force_keyframe", &self.force_keyframe)
            .field("request_keyframe", &self.request_keyframe)
            .field("reset_fragments", &self.reset_fragments)
            .field("suppress_fragments", &self.suppress_fragments)
            .field("closes", &self.closes)
            .field("ended", &self.ended)
            .finish()
    }
}

/// One ended carrier: `reason` Some = the machine closed it, None = it died.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CarrierEnd {
    pub id: i32,
    pub reason: Option<CloseReason>,
}
impl std::fmt::Debug for CarrierEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarrierEnd")
            .field("id", &self.id)
            .field("reason", &self.reason)
            .finish()
    }
}

pub struct Carriers {
    machine: CarrierCutover,
    uw: Zeroizing<[u8; 32]>,
    initiator: bool,
    relay_edge: bool,
    extra: Option<Extra>,
    next_id: i32,
    /// Every ticket given an attempt id in this call (history for the markers).
    tickets: Vec<([u8; 8], i32)>,
    /// Received end-to-end records waiting for the machine to deliver them, by tag.
    stored: HashMap<u32, Vec<u8>>,
    next_tag: u32,
    markers: VecDeque<(i32, Bwf1)>,
    delivered: VecDeque<(i32, Vec<u8>)>,
    effects: Effects,
    /// The write the machine chose for the record being sealed.
    routed: Option<(i32, u32)>,
    caps_sent: bool,
    caps_peer: Option<wire::Caps>,
    last_fresh: bool,
    /// Markers whose ticket named no attempt (traced by the owner when it wires W3).
    pub marker_unknown_ticket: u64,
    unknown_ticket_seen: bool,
    /// The peer is proven to read `BWF1` in this call (R-1 proof for an
    /// initiator): it sent an authenticated `BWF1` record, or a carrier finished
    /// `BIND`/`READY`. It stays proven after that carrier ends.
    peer_proven: bool,
    /// Every carrier already reported on the carrier-end queue (each is told once).
    ended_ids: Vec<i32>,
    /// The sequence of the record sealed last (the machine requires increasing sequences).
    last_sealed: i64,
    /// Trace lines for the worker (`take_traces`); never a ticket, key or address.
    traces: Vec<(&'static str, String)>,
}
impl std::fmt::Debug for Carriers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Carriers")
            .field("send_on", &self.send_on())
            .field("accept", &self.accept())
            .field("ended", &self.ended())
            .field("extra", &self.extra)
            .field("uw", &"<redacted>")
            .finish()
    }
}

impl Carriers {
    pub fn new(uw: Zeroizing<[u8; 32]>, initiator: bool, relay_edge: bool) -> Self {
        let config = Config {
            hold_ms: crate::wifi::limits::CUTOVER_HOLD_MS,
            hold_bytes: crate::wifi::limits::CUTOVER_HOLD_BYTES,
            active_before_switch_ms: crate::wifi::limits::ACTIVE_BEFORE_SWITCH_MS,
            cooldown_ms: crate::wifi::limits::COOLDOWN_AFTER_FALLBACK_MS,
            max_fallbacks: crate::wifi::limits::MAX_FALLBACKS_PER_HOP,
            pending_max_ms: crate::wifi::limits::PENDING_MAX_MS,
            ..Config::default()
        };
        Self {
            machine: CarrierCutover::new(config),
            uw,
            initiator,
            relay_edge,
            extra: None,
            next_id: 1,
            tickets: Vec::new(),
            stored: HashMap::new(),
            next_tag: 0,
            markers: VecDeque::new(),
            delivered: VecDeque::new(),
            effects: Effects::default(),
            routed: None,
            caps_sent: false,
            caps_peer: None,
            last_fresh: true,
            marker_unknown_ticket: 0,
            unknown_ticket_seen: false,
            peer_proven: false,
            ended_ids: Vec::new(),
            last_sealed: -1,
            traces: Vec::new(),
        }
    }

    pub fn uw(&self) -> &[u8; 32] {
        &self.uw
    }
    pub fn initiator(&self) -> bool {
        self.initiator
    }
    pub fn relay_edge(&self) -> bool {
        self.relay_edge
    }
    /// R-1 at the write (Android `WifiHopGate.mayWriteBwf1`): this side's reader
    /// accepts `BWF1` (a `Carriers` exists only then) and the peer is proven to
    /// read it: a responder by the initiator's `WIFI` marker (its carriers are
    /// enabled only then), an initiator by a `BWF1` record the peer already
    /// sent this call or by a carrier that finished `BIND`/`READY`.
    pub fn may_write_bwf1(&self) -> bool {
        !self.initiator || self.peer_proven
    }
    pub fn note_peer_bwf1(&mut self) {
        self.peer_proven = true;
    }
    pub fn trace(&mut self, phase: &'static str, line: String) {
        self.traces.push((phase, line));
    }
    pub fn take_traces(&mut self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut self.traces)
    }
    /// Test only: received records kept for delivery in order.
    #[cfg(test)]
    pub fn stored_records(&self) -> usize {
        self.stored.len()
    }

    // attempts

    pub fn begin_attempt(&mut self, ticket: [u8; 8], now_ms: i64) -> Option<i32> {
        // A relay edge cannot derive the hop UW: it never starts an attempt (R-7).
        if self.relay_edge
            || self.machine.switched_off_now()
            || self.next_id > 62
            || self.machine.pending_carrier() != cutover::NONE
            || self.machine.new_carrier() != cutover::NONE
        {
            return None;
        }
        if self.tickets.iter().any(|(seen, _)| *seen == ticket) {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.tickets.push((ticket, id));
        let actions = self.machine.on_carrier_pending(id, now_ms);
        self.apply(actions);
        Some(id)
    }

    /// Whether the carrier was attached. An id that is not an attempt of this
    /// call, or one that already ended, never reaches the machine.
    pub fn attach(&mut self, extra: Extra, now_ms: i64, at_frame_boundary: bool) -> bool {
        let id = extra.id;
        if !(1..=62).contains(&id)
            || !self.tickets.iter().any(|(_, seen)| *seen == id)
            || self.ended_ids.contains(&id)
            || self.extra.is_some()
        {
            if (1..=62).contains(&id) && self.extra.as_ref().is_none_or(|extra| extra.id != id) {
                self.end(id, None);
            }
            return false;
        }
        let actions = self.machine.on_carrier_up(id, now_ms, 0, at_frame_boundary);
        self.apply(actions);
        if self.machine.new_carrier() != id {
            // The machine ignored the attempt: close it (switched off, or gone).
            let reason = self
                .machine
                .switched_off_now()
                .then_some(CloseReason::SwitchOff);
            self.end(id, reason);
            false
        } else {
            self.extra = Some(extra);
            self.peer_proven = true;
            true
        }
    }

    pub fn carrier_died(&mut self, id: i32, now_ms: i64) {
        if self.ended_ids.contains(&id) {
            return;
        }
        let actions = self.machine.on_carrier_died(id, now_ms);
        self.apply(actions);
        self.end(id, None);
    }

    /// One entry on the carrier-end queue per carrier (amendment 10: the owner
    /// is told once); the attached carrier's state goes with it.
    fn end(&mut self, id: i32, reason: Option<CloseReason>) {
        if self.ended_ids.contains(&id) {
            return;
        }
        self.ended_ids.push(id);
        if self.extra.as_ref().is_some_and(|extra| extra.id == id) {
            self.extra = None;
        }
        self.effects.closes.push(CarrierEnd { id, reason });
    }

    pub fn switch_off(&mut self, now_ms: i64) {
        let actions = self.machine.on_switch_off(now_ms);
        self.apply(actions);
    }

    pub fn quality_fallback(&mut self, now_ms: i64) {
        let actions = self.machine.on_quality_fallback(now_ms);
        self.apply(actions);
    }

    pub fn standby_replaced(&mut self, now_ms: i64) {
        let actions = self.machine.on_standby_replaced(now_ms);
        self.apply(actions);
    }

    pub fn standby_freshness(&mut self, fresh: bool, now_ms: i64) {
        if fresh != self.last_fresh {
            self.last_fresh = fresh;
            let actions = self.machine.on_standby_freshness(fresh, now_ms);
            self.apply(actions);
        }
    }

    pub fn on_call_active(&mut self, now_ms: i64, at_frame_boundary: bool) {
        let actions = self.machine.on_call_active(now_ms, 0, at_frame_boundary);
        self.apply(actions);
    }

    pub fn tick(&mut self, now_ms: i64, at_frame_boundary: bool) {
        let actions = self.machine.on_tick(now_ms, 0, at_frame_boundary);
        self.apply(actions);
    }

    // sending

    /// `Some((carrier, inner))`: write on that carrier (hop-seal there); `None`: the record is lost.
    pub fn route_sealed(
        &mut self,
        seq: i64,
        inner: Vec<u8>,
        now_ms: i64,
    ) -> Option<(i32, Vec<u8>)> {
        self.routed = None;
        if seq <= self.last_sealed {
            // Out of order (Android A3 P2-3): written on the send carrier
            // without telling the machine, which requires increasing sequences.
            let send_on = self.machine.send_on();
            return (send_on >= 0).then_some((send_on, inner));
        }
        self.last_sealed = seq;
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);
        let actions = self.machine.on_sealed(
            Rec {
                seq,
                bytes: inner.len() as i64,
                tag,
            },
            now_ms,
        );
        self.apply(actions);
        self.routed.take().map(|(carrier, _tag)| (carrier, inner))
    }

    /// `SWITCH`/`RESUME` the machine wants written.
    pub fn take_marker(&mut self) -> Option<(i32, Bwf1)> {
        self.markers.pop_front()
    }

    // receiving

    pub fn arrive_content(
        &mut self,
        carrier: i32,
        inner: Vec<u8>,
        content: Policy,
        now_ms: i64,
    ) -> Result<(), crate::call::Error> {
        if inner.len() < 18 {
            return Err(crate::call::Error::Wifi);
        }
        let header = Header::parse(Layer::Endpoint, &inner[..18], content)
            .map_err(|_| crate::call::Error::Wifi)?;
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);
        self.stored.insert(tag, inner.clone());
        let actions = self.machine.on_arrive(
            carrier,
            Wire::Content(Rec {
                seq: header.sequence as i64,
                bytes: inner.len() as i64,
                tag,
            }),
            now_ms,
        );
        self.apply(actions);
        Ok(())
    }

    pub fn arrive_marker(&mut self, carrier: i32, record: Bwf1, now_ms: i64) {
        let id = match &record {
            Bwf1::Switch(switch) => self.id_of(&switch.ticket),
            Bwf1::Resume(resume) => self.id_of(&resume.ticket),
            _ => None,
        };
        let Some(id) = id else {
            self.marker_unknown_ticket += 1;
            self.unknown_ticket_seen = true;
            self.trace("marker_unknown_ticket", format!("carrier={carrier}"));
            return;
        };
        let wire = match record {
            Bwf1::Switch(switch) => Wire::Switch {
                generation: i64::from(switch.generation),
                boundary: switch.e2e_boundary as i64,
                carrier: id,
            },
            Bwf1::Resume(resume) => Wire::Resume {
                generation: i64::from(resume.generation),
                next: resume.e2e_next as i64,
                carrier: id,
            },
            _ => return,
        };
        let actions = self.machine.on_arrive(carrier, wire, now_ms);
        self.apply(actions);
    }

    pub fn take_delivered(&mut self) -> Option<(i32, Vec<u8>)> {
        self.delivered.pop_front()
    }

    pub fn take_effects(&mut self) -> Effects {
        std::mem::take(&mut self.effects)
    }

    /// Whether the last `arrive_marker` carried a ticket naming no attempt.
    pub fn take_unknown_ticket(&mut self) -> bool {
        std::mem::take(&mut self.unknown_ticket_seen)
    }

    // facts

    pub fn send_on(&self) -> i32 {
        self.machine.send_on()
    }
    pub fn accept(&self) -> i32 {
        self.machine.accept()
    }
    pub fn ended(&self) -> bool {
        self.machine.ended()
    }
    pub fn extra(&self) -> Option<&Extra> {
        self.extra.as_ref()
    }
    pub fn extra_mut(&mut self) -> Option<&mut Extra> {
        self.extra.as_mut()
    }
    pub fn old_alive(&self) -> bool {
        self.machine.old_is_alive()
    }
    pub fn fallbacks(&self) -> i32 {
        self.machine.fallback_count()
    }
    pub fn switched_off_now(&self) -> bool {
        self.machine.switched_off_now()
    }
    pub fn pending(&self) -> i32 {
        self.machine.pending_carrier()
    }
    pub fn new_carrier(&self) -> i32 {
        self.machine.new_carrier()
    }
    pub fn ticket_of(&self, id: i32) -> Option<[u8; 8]> {
        if id == OLD {
            return Some([0; 8]);
        }
        self.tickets
            .iter()
            .find(|(_, seen)| *seen == id)
            .map(|(ticket, _)| *ticket)
    }

    fn id_of(&self, ticket: &[u8; 8]) -> Option<i32> {
        if *ticket == [0; 8] {
            return Some(OLD);
        }
        self.tickets
            .iter()
            .find(|(seen, _)| seen == ticket)
            .map(|(_, id)| *id)
    }

    // CAPS bookkeeping (the session sets/reads them)

    pub fn caps_sent(&self) -> bool {
        self.caps_sent
    }
    pub fn mark_caps_sent(&mut self) {
        self.caps_sent = true;
    }
    pub fn peer_caps(&self) -> Option<wire::Caps> {
        self.caps_peer
    }
    pub fn set_peer_caps(&mut self, caps: wire::Caps) {
        self.caps_peer = Some(caps);
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Write {
                    carrier,
                    wire: Wire::Content(rec),
                    ..
                } => {
                    self.routed = Some((carrier, rec.tag));
                }
                Action::Write {
                    carrier,
                    wire:
                        Wire::Switch {
                            generation,
                            boundary,
                            carrier: named,
                        },
                    ..
                } => {
                    let ticket = self.ticket_of(named).unwrap_or([0; 8]);
                    self.markers.push_back((
                        carrier,
                        Bwf1::Switch(wire::Switch {
                            generation: generation as u32,
                            ticket,
                            e2e_boundary: boundary as u64,
                            mesh_boundary: 0,
                        }),
                    ));
                }
                Action::Write {
                    carrier,
                    wire:
                        Wire::Resume {
                            generation,
                            next,
                            carrier: named,
                        },
                    ..
                } => {
                    let ticket = self.ticket_of(named).unwrap_or([0; 8]);
                    self.markers.push_back((
                        carrier,
                        Bwf1::Resume(wire::Resume {
                            generation: generation as u32,
                            ticket,
                            e2e_next: next as u64,
                            mesh_next: 0,
                        }),
                    ));
                }
                Action::Deliver { rec, carrier } => {
                    if let Some(bytes) = self.stored.remove(&rec.tag) {
                        self.delivered.push_back((carrier, bytes));
                    }
                }
                Action::Drop {
                    wire: Some(Wire::Content(rec)),
                    ..
                } => {
                    self.stored.remove(&rec.tag);
                }
                Action::Drop { .. } => {}
                Action::CloseCarrier { carrier, reason } => {
                    // Clears the attached carrier too: nothing is sealed for it again.
                    self.end(carrier, Some(reason));
                }
                Action::ReassertState => self.effects.reassert = true,
                Action::ForceLocalKeyframe => self.effects.force_keyframe = true,
                Action::RequestPeerKeyframe => self.effects.request_keyframe = true,
                Action::ResetSendFragments => self.effects.reset_fragments = true,
                Action::SuppressFragments => self.effects.suppress_fragments = true,
                Action::EndSession { reason } => self.effects.ended = Some(reason),
            }
        }
        // The machine clears its held records without a Drop when a carrier goes
        // (`new_carrier_gone`, OLD gone while waiting for pending). Every stored
        // record is a held one, so none is kept once nothing is held.
        if self.machine.held_count() == 0 && !self.stored.is_empty() {
            self.stored.clear();
        }
    }
}
