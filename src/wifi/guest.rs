//! W4G: the guest role of the Wi-Fi Direct carrier (design decision "Windows Wi-Fi Direct",
//! 2026-11-06): the phone hosts a group and this PC joins it. A pure state machine ticked
//! from the call worker; it owns no thread or clock and every method takes `now_ms`. The
//! platform sits behind `GuestPlatform`. The network name and password come only from the
//! call's `UW` and the HOST ticket; every ticket is used once; the carrier is handed over
//! only after `BIND` and `READY` both ways; every exit path leaves the group.

use super::{
    attempt::Output,
    binding::{Bound, Step},
    faults::{self, Faults},
    guest_binding::GuestBinding,
    guest_platform::{self, GuestPlatform, JoinEvent, JoinRequest},
    keys,
    limits::{
        ATTEMPT_MAX_MS, GUEST_CONNECT_RETRY_MS, GUEST_CONNECT_WINDOW_MS, GUEST_CREDS_DEADLINE_MS,
        GUEST_JOIN_DEADLINE_MS, GUEST_MAX_HOSTS_PER_HOP, GUEST_REST_MS, HOST_VALID_MS,
    },
    log,
    platform::{PlatformError, ProbeState},
    roles::{self, Role, Roles},
    wire::{self, bearer, decline},
};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;

/// How often the joined interface's address is read while joining.
pub const ADDRESS_POLL_MS: i64 = 200;

/// What the call says at this moment (no secret in it).
#[derive(Debug)]
pub struct GuestFacts {
    pub now_ms: i64,
    /// The env arm with the guest role (and later W4's Settings switch) at this moment.
    pub armed: bool,
    pub caps_exchanged: bool,
    pub local_caps: wire::Caps,
    pub peer_caps: Option<wire::Caps>,
    pub local_is_initiator: bool,
    /// A Wi-Fi carrier is attached and alive.
    pub carrier_up: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Creds,
    Joining,
    Connecting,
    Binding,
}

struct Attempt {
    generation: u32,
    ticket: [u8; 8],
    host_ms: i64,
    freq_mhz: u16,
    stage: Stage,
    creds: Option<wire::Creds>,
    join_started_ms: i64,
    /// `Some(channel)` once the platform said the group's network connected.
    channel: Option<Option<u32>>,
    last_address_poll_ms: i64,
    local: Option<Ipv4Addr>,
    remote: Option<SocketAddrV4>,
    first_connect_ms: i64,
    connect_pending: bool,
    retry_at_ms: Option<i64>,
    binding: Option<GuestBinding>,
    begin_sent: bool,
    pending_bound: Option<Box<Bound>>,
}
impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt")
            .field("generation", &self.generation)
            .field("ticket", &log::tag(&self.ticket))
            .field("host_ms", &self.host_ms)
            .field("freq_mhz", &self.freq_mhz)
            .field("stage", &self.stage)
            .field("creds", &self.creds.map(|_| "<redacted>"))
            .field("joined", &self.channel.is_some())
            .field("addresses", &"<redacted>")
            .field("connect_pending", &self.connect_pending)
            .field("binding", &self.binding)
            .field("begin_sent", &self.begin_sent)
            .field("pending_bound", &self.pending_bound.is_some())
            .finish()
    }
}

enum State {
    Idle,
    Attempt(Box<Attempt>),
    Live { id: i32, generation: u32 },
    Off,
}

pub struct GuestOwner {
    platform: Box<dyn GuestPlatform>,
    state: State,
    generation: u32,
    /// The generation whose join was started and not left yet.
    joined_generation: Option<u32>,
    attempt_id: Option<i32>,
    /// Every non-zero ticket any HOST named in this call (S2: used once).
    seen_tickets: Vec<[u8; 8]>,
    hosts_accepted: u32,
    last_end_ms: Option<i64>,
    faults: Option<Arc<Mutex<Faults>>>,
    /// The phone-wide fault clock (`faults::clock_ms`), never the call clock.
    fault_clock: fn() -> i64,
}
impl std::fmt::Debug for GuestOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let attempt = match &self.state {
            State::Attempt(attempt) => Some(attempt),
            _ => None,
        };
        f.debug_struct("GuestOwner")
            .field("state", &self.state_name())
            .field("generation", &self.generation)
            .field("joined_generation", &self.joined_generation)
            .field("attempt_id", &self.attempt_id)
            .field("tickets_seen", &self.seen_tickets.len())
            .field("hosts_accepted", &self.hosts_accepted)
            .field("last_end_ms", &self.last_end_ms)
            .field("attempt", &attempt)
            .finish()
    }
}

impl GuestOwner {
    pub fn new(platform: Box<dyn GuestPlatform>) -> Self {
        Self::build(platform, None)
    }

    /// Test injection of the phone-wide fault record.
    #[cfg(test)]
    pub(crate) fn with_faults(platform: Box<dyn GuestPlatform>, faults: Arc<Mutex<Faults>>) -> Self {
        Self::build(platform, Some(faults))
    }

    fn build(platform: Box<dyn GuestPlatform>, faults: Option<Arc<Mutex<Faults>>>) -> Self {
        Self {
            platform,
            state: State::Idle,
            generation: 0,
            joined_generation: None,
            attempt_id: None,
            seen_tickets: Vec::new(),
            hosts_accepted: 0,
            last_end_ms: None,
            faults,
            fault_clock: faults::clock_ms,
        }
    }

    pub fn state_name(&self) -> &'static str {
        match &self.state {
            State::Idle => "idle",
            State::Attempt(attempt) => match attempt.stage {
                Stage::Creds => "creds",
                Stage::Joining => "joining",
                Stage::Connecting => "connecting",
                Stage::Binding => "binding",
            },
            State::Live { .. } => "live",
            State::Off => "off",
        }
    }

    /// Whether this PC may offer the client bit now: the probe answered with Wi-Fi on and
    /// no Mobile Hotspot, and no phone-wide local-fault pause.
    pub fn caps_possible(&mut self) -> bool {
        let eligible = match self.platform.probe() {
            ProbeState::Ready(caps) => guest_platform::guest_eligibility(&caps).is_ok(),
            ProbeState::Pending => false,
        };
        eligible && !self.paused()
    }

    /// A HOST from the phone (on the Bluetooth carrier). Declines in Android's guest order.
    pub fn on_peer_host(
        &mut self,
        host: wire::Host,
        uw: Option<&[u8; 32]>,
        facts: &GuestFacts,
        out: &mut Vec<Output>,
    ) {
        let ticket = host.ticket;
        let fresh = ticket != [0; 8] && !self.seen_tickets.contains(&ticket);
        if fresh {
            self.seen_tickets.push(ticket);
        }
        let guest_tuple = facts
            .peer_caps
            .and_then(|peer| roles::choose(&facts.local_caps, &peer, facts.local_is_initiator))
            == Some(Roles {
                bearer: bearer::DIRECT,
                local: Role::Guest,
            });
        let reason = if uw.is_none() {
            Some(decline::NOT_ELIGIBLE)
        } else if !guest_tuple {
            Some(decline::BUSY)
        } else if host.status == 0 {
            out.push(Self::trace("host_none", "", "", 0));
            return;
        } else if host.bearer != bearer::DIRECT
            || !(1..=HOST_VALID_MS).contains(&host.valid_ms)
            || !fresh
        {
            Some(decline::NOT_ELIGIBLE)
        } else if matches!(self.state, State::Off) {
            Some(decline::NOT_ELIGIBLE)
        } else if !matches!(self.state, State::Idle) || facts.carrier_up {
            Some(decline::BUSY)
        } else if self.hosts_accepted >= GUEST_MAX_HOSTS_PER_HOP
            || self
                .last_end_ms
                .is_some_and(|end| facts.now_ms - end < GUEST_REST_MS)
        {
            Some(decline::LIMIT_OR_COOLDOWN)
        } else if !facts.armed {
            Some(decline::NOT_ELIGIBLE)
        } else {
            match self.platform.probe() {
                ProbeState::Pending => Some(decline::WIFI_UNAVAILABLE),
                ProbeState::Ready(caps) => match guest_platform::guest_eligibility(&caps) {
                    Err(reason) => Some(reason),
                    Ok(()) if self.paused() => Some(decline::LIMIT_OR_COOLDOWN),
                    Ok(()) => None,
                },
            }
        };
        if let Some(reason) = reason {
            out.push(Output::Send(wire::Bwf1::Decline(wire::Decline { ticket, reason })));
            out.push(Self::trace("decline", "", decline::name(reason), 0));
            return;
        }
        self.hosts_accepted += 1;
        self.generation = self.generation.wrapping_add(1);
        self.state = State::Attempt(Box::new(Attempt {
            generation: self.generation,
            ticket,
            host_ms: facts.now_ms,
            freq_mhz: host.freq_mhz,
            stage: Stage::Creds,
            creds: None,
            join_started_ms: 0,
            channel: None,
            last_address_poll_ms: 0,
            local: None,
            remote: None,
            first_connect_ms: 0,
            connect_pending: false,
            retry_at_ms: None,
            binding: None,
            begin_sent: false,
            pending_bound: None,
        }));
        out.push(Self::trace("host", "", "", 0));
    }

    /// The CREDS that follow the HOST: derive the network name and password and join.
    pub fn on_peer_creds(
        &mut self,
        creds: wire::Creds,
        uw: Option<&[u8; 32]>,
        now_ms: i64,
        out: &mut Vec<Output>,
    ) {
        let generation = match &self.state {
            State::Attempt(attempt) if attempt.stage == Stage::Creds && attempt.ticket == creds.ticket => {
                attempt.generation
            }
            _ => {
                out.push(Self::trace("creds_ignored", "", "", 0));
                return;
            }
        };
        let Some(uw) = uw else {
            self.fail(decline::NOT_ELIGIBLE, false, now_ms, out);
            return;
        };
        let request = match (
            keys::direct_ssid(uw, &creds.ticket),
            keys::direct_psk(uw, &creds.ticket),
        ) {
            (Ok(ssid), Ok(psk)) => JoinRequest {
                ssid: Zeroizing::new(ssid),
                psk,
            },
            _ => {
                self.fail(decline::WIFI_UNAVAILABLE, true, now_ms, out);
                return;
            }
        };
        // Set before the call: a join the platform half-started is left on every path.
        self.joined_generation = Some(generation);
        match self.platform.join(generation, request) {
            Ok(()) => {
                if let State::Attempt(attempt) = &mut self.state {
                    attempt.stage = Stage::Joining;
                    attempt.creds = Some(creds);
                    attempt.join_started_ms = now_ms;
                    attempt.last_address_poll_ms = now_ms - ADDRESS_POLL_MS;
                }
                out.push(Self::trace("join", "", "", self.setup_ms(now_ms)));
            }
            Err(PlatformError::InUse) => self.fail(decline::BUSY, false, now_ms, out),
            Err(_) => self.fail(decline::WIFI_UNAVAILABLE, true, now_ms, out),
        }
    }

    /// Per worker tick: platform events, then deadlines and the next stage.
    pub fn tick(&mut self, facts: &GuestFacts, uw: Option<&[u8; 32]>, out: &mut Vec<Output>) {
        let now_ms = facts.now_ms;
        for event in self.platform.poll() {
            self.on_event(event, uw, now_ms, out);
        }
        if matches!(self.state, State::Attempt(_)) {
            self.poll_attempt(now_ms, out);
        }
    }

    fn on_event(&mut self, event: JoinEvent, uw: Option<&[u8; 32]>, now_ms: i64, out: &mut Vec<Output>) {
        let generation = event.generation();
        let current = match &self.state {
            State::Attempt(attempt) => Some((attempt.generation, attempt.stage)),
            _ => None,
        };
        let live = match self.state {
            State::Live { id, generation } => Some((id, generation)),
            _ => None,
        };
        let ours = current.is_some_and(|(running, _)| running == generation);
        match event {
            JoinEvent::LateJoinUndone { .. } => out.push(Self::trace("late_join_undone", "", "", 0)),
            JoinEvent::Joined { channel, .. } => {
                if current == Some((generation, Stage::Joining)) {
                    if let State::Attempt(attempt) = &mut self.state {
                        if attempt.channel.is_none() {
                            attempt.channel = Some(channel);
                        }
                    }
                    out.push(Self::trace("joined", "", "", self.setup_ms(now_ms)));
                } else if !ours && !live.is_some_and(|(_, running)| running == generation) {
                    // A join that completed for an attempt that already ended: undo it.
                    self.platform.leave(generation);
                    out.push(Self::trace("late_join_undone", "", "", 0));
                }
            }
            JoinEvent::JoinFailed { .. } => {
                if current == Some((generation, Stage::Joining)) {
                    self.fail(decline::SETUP_FAILED, false, now_ms, out);
                }
            }
            JoinEvent::JoinRefused { .. } => {
                if current == Some((generation, Stage::Joining)) {
                    self.fail(decline::WIFI_UNAVAILABLE, true, now_ms, out);
                }
            }
            JoinEvent::Left { .. } => {
                if let Some((id, running)) = live {
                    if running == generation {
                        // The group's network dropped under the carrier: it is dead at once.
                        out.push(Output::Died { id });
                    }
                } else if ours {
                    self.fail(decline::SETUP_FAILED, false, now_ms, out);
                }
            }
            JoinEvent::Connected { socket, .. } => {
                if current != Some((generation, Stage::Connecting)) {
                    // A connect of an attempt that already ended: closed at once.
                    drop(socket);
                } else if let Some(uw) = uw {
                    if let State::Attempt(attempt) = &mut self.state {
                        attempt.binding = Some(GuestBinding::new(socket, uw, attempt.ticket, now_ms));
                        attempt.stage = Stage::Binding;
                        attempt.connect_pending = false;
                    }
                    out.push(Self::trace("bind", "", "", self.setup_ms(now_ms)));
                } else {
                    drop(socket);
                    self.fail(decline::SETUP_FAILED, false, now_ms, out);
                }
            }
            JoinEvent::ConnectRefused { .. } => {
                if current == Some((generation, Stage::Connecting)) {
                    let mut give_up = false;
                    if let State::Attempt(attempt) = &mut self.state {
                        attempt.connect_pending = false;
                        if now_ms - attempt.first_connect_ms < GUEST_CONNECT_WINDOW_MS {
                            attempt.retry_at_ms = Some(now_ms + GUEST_CONNECT_RETRY_MS);
                        } else {
                            give_up = true;
                        }
                    }
                    if give_up {
                        self.fail(decline::SETUP_FAILED, false, now_ms, out);
                    }
                }
            }
            JoinEvent::ConnectFailed { .. } => {
                if current == Some((generation, Stage::Connecting)) {
                    self.fail(decline::SETUP_FAILED, false, now_ms, out);
                }
            }
        }
    }

    fn poll_attempt(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        let (host_ms, stage) = match &self.state {
            State::Attempt(attempt) => (attempt.host_ms, attempt.stage),
            _ => return,
        };
        if now_ms - host_ms >= ATTEMPT_MAX_MS {
            self.fail(decline::SETUP_FAILED, false, now_ms, out);
            return;
        }
        match stage {
            Stage::Creds => {
                if now_ms - host_ms >= GUEST_CREDS_DEADLINE_MS {
                    self.fail(decline::SETUP_FAILED, false, now_ms, out);
                }
            }
            Stage::Joining => self.poll_joining(now_ms, out),
            Stage::Connecting => self.poll_connecting(now_ms, out),
            Stage::Binding => self.poll_binding(now_ms, out),
        }
    }

    fn poll_joining(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        let (generation, join_started_ms, channel, last_poll_ms, freq_mhz, creds) = match &self.state {
            State::Attempt(attempt) => (
                attempt.generation,
                attempt.join_started_ms,
                attempt.channel,
                attempt.last_address_poll_ms,
                attempt.freq_mhz,
                attempt.creds,
            ),
            _ => return,
        };
        if now_ms - join_started_ms >= GUEST_JOIN_DEADLINE_MS {
            self.fail(decline::SETUP_FAILED, false, now_ms, out);
            return;
        }
        let (Some(channel), Some(creds)) = (channel, creds) else {
            return;
        };
        if now_ms - last_poll_ms < ADDRESS_POLL_MS {
            return;
        }
        if let State::Attempt(attempt) = &mut self.state {
            attempt.last_address_poll_ms = now_ms;
        }
        let Some(guid) = self.platform.interface_guid() else {
            return;
        };
        let adapters = self.platform.adapters();
        let Some(joined) = guest_platform::joined_address(&adapters, &guid) else {
            return;
        };
        if let Err(reason) = guest_platform::join_band(channel, freq_mhz) {
            self.fail(reason, false, now_ms, out);
            return;
        }
        let peer = Ipv4Addr::from(creds.ipv4);
        if let Err(reason) = guest_platform::creds_check(joined, peer) {
            self.fail(reason, false, now_ms, out);
            return;
        }
        if guest_platform::overlaps_other(&adapters, &guid, joined) {
            self.fail(decline::WIFI_UNAVAILABLE, false, now_ms, out);
            return;
        }
        let remote = SocketAddrV4::new(peer, creds.port);
        match self.platform.connect(generation, joined.ip, remote) {
            Ok(()) => {
                if let State::Attempt(attempt) = &mut self.state {
                    attempt.stage = Stage::Connecting;
                    attempt.local = Some(joined.ip);
                    attempt.remote = Some(remote);
                    attempt.first_connect_ms = now_ms;
                    attempt.connect_pending = true;
                    attempt.retry_at_ms = None;
                }
                out.push(Self::trace("connect", "", "", self.setup_ms(now_ms)));
            }
            Err(_) => self.fail(decline::WIFI_UNAVAILABLE, true, now_ms, out),
        }
    }

    fn poll_connecting(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        let (generation, first_ms, pending, retry_at_ms, local, remote) = match &self.state {
            State::Attempt(attempt) => (
                attempt.generation,
                attempt.first_connect_ms,
                attempt.connect_pending,
                attempt.retry_at_ms,
                attempt.local,
                attempt.remote,
            ),
            _ => return,
        };
        if now_ms - first_ms >= GUEST_CONNECT_WINDOW_MS {
            self.fail(decline::SETUP_FAILED, false, now_ms, out);
            return;
        }
        let (Some(local), Some(remote)) = (local, remote) else {
            return;
        };
        if pending || !retry_at_ms.is_some_and(|at| now_ms >= at) {
            return;
        }
        match self.platform.connect(generation, local, remote) {
            Ok(()) => {
                if let State::Attempt(attempt) = &mut self.state {
                    attempt.connect_pending = true;
                    attempt.retry_at_ms = None;
                }
            }
            Err(_) => self.fail(decline::WIFI_UNAVAILABLE, true, now_ms, out),
        }
    }

    /// A DECLINE from the phone. Only the running attempt's ticket counts; a DECLINE for
    /// the live carrier or an older ticket is stale and changes nothing.
    pub fn on_peer_decline(&mut self, ticket: [u8; 8], reason: u8, now_ms: i64, out: &mut Vec<Output>) {
        if !matches!(&self.state, State::Attempt(attempt) if attempt.ticket == ticket) {
            return;
        }
        let setup = self.setup_ms(now_ms);
        self.state = State::Idle;
        if let Some(id) = self.attempt_id.take() {
            out.push(Output::Died { id });
        }
        self.leave_joined();
        self.last_end_ms = Some(now_ms);
        out.push(Self::trace("abort", "peer", decline::name(reason), setup));
    }

    /// The switch went off: an attempt is declined 1 and left; a live carrier is handed to
    /// the cutover machine (the session falls back), and the group is left at its close.
    pub fn on_switch_off(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        if matches!(self.state, State::Attempt(_)) {
            self.fail(decline::NOT_ELIGIBLE, false, now_ms, out);
        } else if matches!(self.state, State::Live { .. }) {
            // Off for the rest of this call: turning the switch back on does not make this
            // owner attempt again (the session already fell back to Bluetooth for good).
            self.state = State::Off;
        }
    }

    /// Wi-Fi ended for this call (a `BWF1` violation): as switch off, and off for good.
    pub fn on_wifi_ended(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        self.on_switch_off(now_ms, out);
        if matches!(self.state, State::Idle) {
            self.state = State::Off;
        }
    }

    fn poll_binding(&mut self, now_ms: i64, out: &mut Vec<Output>) {
        let (step, begin, ticket) = match &mut self.state {
            State::Attempt(attempt) => {
                if attempt.pending_bound.is_some() {
                    return; // bound; waiting for the attempt id
                }
                let Some(binding) = attempt.binding.as_mut() else {
                    return;
                };
                let step = binding.poll(now_ms);
                // No Begin for a binding that failed in this very poll: the session would
                // register a pending carrier nobody ever attaches or kills.
                let begin = binding.keys_ready() && !attempt.begin_sent && !matches!(step, Step::Failed(_));
                if begin {
                    attempt.begin_sent = true;
                }
                (step, begin, attempt.ticket)
            }
            _ => return,
        };
        if begin {
            out.push(Output::Begin { ticket });
        }
        match step {
            Step::Pending => {}
            Step::Failed(failure) => {
                out.push(Self::trace("abort", "peer", failure.name(), self.setup_ms(now_ms)));
                self.fail(decline::SETUP_FAILED, false, now_ms, out);
            }
            Step::Done(bound) => match self.attempt_id {
                Some(id) => self.go_live(bound, id, now_ms, out),
                None => {
                    if let State::Attempt(attempt) = &mut self.state {
                        attempt.binding = None;
                        attempt.pending_bound = Some(bound);
                    }
                }
            },
        }
    }

    /// The session's answer to `Begin`: the cutover id of this attempt, or `None` when the
    /// session refused it (switched off, Wi-Fi ended, another carrier pending).
    ///
    /// `None` turns the guest role off for the rest of the call (the session said no).
    pub fn set_attempt_id(&mut self, id: Option<i32>, now_ms: i64, out: &mut Vec<Output>) {
        match id {
            Some(id) => {
                // An id is kept only for the attempt that asked for it. Any other answer
                // (the attempt already ended) is killed so the cutover frees its carrier.
                if !matches!(&self.state, State::Attempt(attempt) if attempt.begin_sent) {
                    out.push(Output::Died { id });
                    return;
                }
                self.attempt_id = Some(id);
                let bound = match &mut self.state {
                    State::Attempt(attempt) => attempt.pending_bound.take(),
                    _ => None,
                };
                if let Some(bound) = bound {
                    self.go_live(bound, id, now_ms, out);
                }
            }
            None => {
                if matches!(self.state, State::Attempt(_)) {
                    self.fail(decline::NOT_ELIGIBLE, false, now_ms, out);
                }
                // The session refused the attempt (switch off, Wi-Fi ended, another carrier
                // pending): no more Wi-Fi attempts for the rest of this call.
                self.state = State::Off;
            }
        }
    }

    fn go_live(&mut self, bound: Box<Bound>, id: i32, now_ms: i64, out: &mut Vec<Output>) {
        let State::Attempt(attempt) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let setup = (now_ms - attempt.host_ms).max(0);
        out.push(Output::Attach {
            bound,
            ticket: attempt.ticket,
            id,
        });
        out.push(Self::trace("ready", "", "", setup));
        self.state = State::Live {
            id,
            generation: attempt.generation,
        };
    }

    /// The cutover closed carrier `id`: the group is left; the next HOST waits 29 s.
    pub fn on_carrier_lost(&mut self, id: i32, now_ms: i64) {
        match self.state {
            State::Live { id: live, .. } if live == id => {
                self.leave_joined();
                self.attempt_id = None;
                self.last_end_ms = Some(now_ms);
                self.state = State::Idle;
            }
            State::Off => {
                self.attempt_id = None;
                self.leave_joined();
            }
            _ => {}
        }
    }

    /// Leaves the group and ends everything; the owner is off afterwards (hang-up, Bluetooth gone, drop).
    pub fn release(&mut self) {
        self.state = State::Off;
        self.attempt_id = None;
        self.leave_joined();
    }

    fn fail(&mut self, reason: u8, local: bool, now_ms: i64, out: &mut Vec<Output>) {
        let setup = self.setup_ms(now_ms);
        let State::Attempt(attempt) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        out.push(Output::Send(wire::Bwf1::Decline(wire::Decline {
            ticket: attempt.ticket,
            reason,
        })));
        if let Some(id) = self.attempt_id.take() {
            out.push(Output::Died { id });
        }
        // Dropping the attempt closes a binding socket at once.
        drop(attempt);
        self.leave_joined();
        if local {
            self.record_fault();
        }
        self.last_end_ms = Some(now_ms);
        let name = decline::name(reason);
        out.push(Self::trace("abort", if local { "local" } else { "peer" }, name, setup));
        out.push(Output::Trace {
            phase: "carrier",
            detail: format!("hop=call | requested=direct | effective=bluetooth | reason={name}"),
        });
    }

    fn leave_joined(&mut self) {
        if let Some(generation) = self.joined_generation.take() {
            self.platform.leave(generation);
        }
    }

    fn setup_ms(&self, now_ms: i64) -> i64 {
        match &self.state {
            State::Attempt(attempt) => (now_ms - attempt.host_ms).max(0),
            _ => 0,
        }
    }

    fn trace(event: &'static str, fault: &'static str, reason: &'static str, setup_ms: i64) -> Output {
        let setup = setup_ms.max(0).to_string();
        Output::Trace {
            phase: "wifi_upgrade",
            detail: log::detail(&[
                ("event", event),
                ("bearer", "direct"),
                ("role", "guest"),
                ("hop", "call"),
                ("level", "private"),
                ("carrier", "direct"),
                ("setupMs", &setup),
                ("fault", fault),
                ("reason", reason),
            ]),
        }
    }

    fn paused(&self) -> bool {
        let now_ms = (self.fault_clock)();
        match &self.faults {
            Some(faults) => faults.lock().map(|mut f| f.paused(now_ms)).unwrap_or(false),
            None => faults::global()
                .lock()
                .map(|mut f| f.paused(now_ms))
                .unwrap_or(false),
        }
    }

    fn record_fault(&self) {
        let now_ms = (self.fault_clock)();
        match &self.faults {
            Some(faults) => {
                if let Ok(mut f) = faults.lock() {
                    f.record(now_ms);
                }
            }
            None => {
                if let Ok(mut f) = faults::global().lock() {
                    f.record(now_ms);
                }
            }
        }
    }
}

impl Drop for GuestOwner {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests;
