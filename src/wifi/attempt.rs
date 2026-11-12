//! The host-side attempt owner. A pure state machine ticked from the call
//! worker; it owns no thread, timer or clock and every method takes `now_ms`.

use super::{
    address::{AdapterInfo, GroupAddress},
    binding::{Bound, HostBinding, Step},
    faults::{self, Faults},
    keys,
    limits::{
        ACTIVE_BEFORE_SWITCH_MS, ATTEMPT_MAX_MS, COOLDOWN_AFTER_FALLBACK_MS, HOST_VALID_MS,
        MAX_ACCEPTED_SOCKETS_PER_ATTEMPT, MAX_FAILED_ATTEMPTS_PER_HOP, MAX_FALLBACKS_PER_HOP,
        RETRY_DELAY_MS, START_DEADLINE_MS,
    },
    log,
    platform::{Platform, PlatformError, ProbeState, ProfileRead},
    probe::{eligibility, UNKNOWN_BAND_OK},
    roles::{self, Role, Roles},
    wire::{self, bearer},
};
use std::{
    sync::{Arc, Mutex},
};
#[cfg(test)]
use std::collections::VecDeque;

pub struct Facts {
    pub now_ms: i64,
    /// env arm AND Settings switch (W4) at this moment.
    pub armed: bool,
    pub phase_active: bool,
    /// Since when the call has had video (either direction); `None` = audio only.
    pub video_on_since_ms: Option<i64>,
    pub caps_exchanged: bool,
    /// What this PC sent.
    pub local_caps: wire::Caps,
    pub peer_caps: Option<wire::Caps>,
    pub local_is_initiator: bool,
    /// The session has `UW` (the UW itself is borrowed by `tick`).
    pub uw_ready: bool,
    /// Machine fallback count.
    pub fallbacks: i32,
    /// A Wi-Fi carrier is attached and alive.
    pub carrier_up: bool,
    pub send_on: i32,
}
impl std::fmt::Debug for Facts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Facts")
            .field("now_ms", &self.now_ms)
            .field("armed", &self.armed)
            .field("phase_active", &self.phase_active)
            .field("video_on_since_ms", &self.video_on_since_ms)
            .field("caps_exchanged", &self.caps_exchanged)
            .field("local_caps", &self.local_caps)
            .field("peer_caps", &self.peer_caps)
            .field("local_is_initiator", &self.local_is_initiator)
            .field("uw_ready", &self.uw_ready)
            .field("fallbacks", &self.fallbacks)
            .field("carrier_up", &self.carrier_up)
            .field("send_on", &self.send_on)
            .finish()
    }
}

pub enum Output {
    /// `session.wifi_send(..)` on carrier 0.
    Send(wire::Bwf1),
    /// Once per ticket, in the tick where the binding consumed it.
    Begin { ticket: [u8; 8] },
    Attach {
        bound: Box<Bound>,
        ticket: [u8; 8],
        id: i32,
    },
    /// Only for an id a `Begin` produced.
    Died { id: i32 },
    Trace { phase: &'static str, detail: String },
}
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Send(record) => f.debug_tuple("Send").field(record).finish(),
            Self::Begin { ticket } => f
                .debug_struct("Begin")
                .field("ticket", &log::tag(ticket))
                .finish(),
            Self::Attach { bound, ticket, id } => f
                .debug_struct("Attach")
                .field("bound", bound)
                .field("ticket", &log::tag(ticket))
                .field("id", id)
                .finish(),
            Self::Died { id } => f.debug_struct("Died").field("id", id).finish(),
            Self::Trace { phase, detail } => f
                .debug_struct("Trace")
                .field("phase", phase)
                .field("detail", detail)
                .finish(),
        }
    }
}

pub const ADDRESS_POLL_MS: i64 = 200;
pub const BUSY_BACKOFF_MAX_MS: i64 = 240_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle { retry_at: i64 },
    Starting,
    Offered,
    Binding,
    Live { id: i32 },
    Off,
}

pub struct Owner {
    state: State,
    failed: u32,
    busy_streak: u32,
    ticket: Option<[u8; 8]>,
    binding: Option<HostBinding>,
    pending_bound: Option<Box<Bound>>,
    attempt_id: Option<i32>,
    begin_sent: bool,
    accepted: u32,
    host_sent: bool,
    host_sent_ms: i64,
    began_ms: i64,
    before: Vec<AdapterInfo>,
    address: Option<GroupAddress>,
    last_address_poll_ms: i64,
    profile_pending: bool,
    caps_logged: bool,
    faults: Option<Arc<Mutex<Faults>>>,
    /// The phone-wide fault clock (`faults::clock_ms`): every fault is recorded
    /// and checked on it, never on the call clock of `Facts.now_ms`.
    fault_clock: fn() -> i64,
}
impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner")
            .field("state", &self.state)
            .field("failed", &self.failed)
            .field("busy_streak", &self.busy_streak)
            .field(
                "ticket",
                &self.ticket.as_ref().map(|ticket| log::tag(ticket)),
            )
            .field("binding", &self.binding)
            .field("attempt_id", &self.attempt_id)
            .field("accepted", &self.accepted)
            .field("host_sent", &self.host_sent)
            .field("began_ms", &self.began_ms)
            .finish()
    }
}

impl Owner {
    pub fn new() -> Self {
        Self::with_faults_opt(None)
    }

    fn with_faults_opt(faults: Option<Arc<Mutex<Faults>>>) -> Self {
        Self {
            state: State::Idle { retry_at: 0 },
            failed: 0,
            busy_streak: 0,
            ticket: None,
            binding: None,
            pending_bound: None,
            attempt_id: None,
            begin_sent: false,
            accepted: 0,
            host_sent: false,
            host_sent_ms: 0,
            began_ms: 0,
            before: Vec::new(),
            address: None,
            last_address_poll_ms: 0,
            profile_pending: false,
            caps_logged: false,
            faults,
            fault_clock: faults::clock_ms,
        }
    }

    /// Test injection for the phone-wide fault clock.
    pub fn with_faults(faults: Arc<Mutex<Faults>>) -> Self {
        Self::with_faults_opt(Some(faults))
    }

    /// Test injection of the phone-wide fault clock.
    #[cfg(test)]
    pub(crate) fn with_faults_and_clock(faults: Arc<Mutex<Faults>>, clock: fn() -> i64) -> Self {
        let mut owner = Self::with_faults_opt(Some(faults));
        owner.fault_clock = clock;
        owner
    }

    /// Test-only: the current offer's ticket.
    #[cfg(test)]
    pub(crate) fn ticket(&self) -> Option<[u8; 8]> {
        self.ticket
    }

    /// Test-only: put the machine in the state a finished attach reaches.
    #[cfg(test)]
    pub(crate) fn force_live(&mut self, id: i32) {
        self.state = State::Live { id };
        self.busy_streak = 0;
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Idle { .. } => "idle",
            State::Starting => "starting",
            State::Offered => "offered",
            State::Binding => "binding",
            State::Live { .. } => "live",
            State::Off => "off",
        }
    }

    pub fn tick(
        &mut self,
        facts: &Facts,
        uw: Option<&[u8; 32]>,
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        self.log_caps(facts, out);
        match self.state {
            State::Off => {}
            State::Idle { retry_at } => {
                if self.may_start(facts, uw, retry_at, platform) {
                    self.start(facts, uw.expect("checked by may_start"), platform, out);
                }
            }
            State::Starting => self.poll_starting(facts, platform, out),
            State::Offered => self.poll_offered(facts, uw, platform, out),
            State::Binding => self.poll_binding(facts, platform, out),
            State::Live { .. } => {}
        }
    }

    fn log_caps(&mut self, facts: &Facts, out: &mut Vec<Output>) {
        if self.caps_logged || !facts.caps_exchanged {
            return;
        }
        let Some(_peer) = facts.peer_caps else {
            return;
        };
        out.push(Output::Trace {
            phase: "wifi_caps",
            detail: log::detail(&[
                ("dir", "sent"),
                ("bearers", &format!("0x{:02x}", facts.local_caps.bearers)),
                ("freeHost", &facts.local_caps.free_host.to_string()),
                ("freeGuest", &facts.local_caps.free_guest.to_string()),
            ]),
        });
        self.caps_logged = true;
    }

    fn may_start(
        &self,
        facts: &Facts,
        uw: Option<&[u8; 32]>,
        retry_at: i64,
        platform: &mut dyn Platform,
    ) -> bool {
        if !facts.armed
            || !facts.phase_active
            || !facts.caps_exchanged
            || facts.carrier_up
            || facts.now_ms < retry_at
            || self.failed >= MAX_FAILED_ATTEMPTS_PER_HOP
            || facts.fallbacks >= MAX_FALLBACKS_PER_HOP
            || uw.is_none()
            || !facts.uw_ready
        {
            return false;
        }
        let Some(video_since) = facts.video_on_since_ms else {
            return false;
        };
        if facts.now_ms - video_since < ACTIVE_BEFORE_SWITCH_MS {
            return false;
        }
        let Some(peer) = facts.peer_caps else {
            return false;
        };
        if roles::choose(&facts.local_caps, &peer, facts.local_is_initiator)
            != Some(Roles {
                bearer: bearer::DIRECT,
                local: Role::Host,
            })
        {
            return false;
        }
        if self.paused() || faults::group_not_public() || faults::profile_unknown() {
            return false;
        }
        let ProbeState::Ready(caps) = platform.probe() else {
            return false;
        };
        eligibility(&caps, UNKNOWN_BAND_OK).is_ok()
    }

    fn start(
        &mut self,
        facts: &Facts,
        uw: &[u8; 32],
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        let ticket = match keys::new_ticket() {
            Ok(ticket) => ticket,
            Err(_) => {
                self.fail(facts.now_ms, true, wire::decline::WIFI_UNAVAILABLE, platform, out);
                return;
            }
        };
        let (ssid, psk) = match (
            keys::direct_ssid(uw, &ticket),
            keys::direct_psk(uw, &ticket),
        ) {
            (Ok(ssid), Ok(psk)) => (ssid, psk),
            _ => {
                self.fail(facts.now_ms, true, wire::decline::WIFI_UNAVAILABLE, platform, out);
                return;
            }
        };
        self.before = platform.snapshot();
        self.began_ms = facts.now_ms;
        self.last_address_poll_ms = facts.now_ms - ADDRESS_POLL_MS;
        self.address = None;
        self.profile_pending = false;
        self.accepted = 0;
        self.host_sent = false;
        self.attempt_id = None;
        self.begin_sent = false;
        self.binding = None;
        self.pending_bound = None;
        match platform.start_group(&ssid, psk.as_str()) {
            Ok(()) => {
                self.ticket = Some(ticket);
                self.state = State::Starting;
                out.push(self.trace("group_start", "", "", facts.now_ms));
            }
            Err(error) => {
                self.ticket = Some(ticket);
                let reason = match error {
                    PlatformError::InUse => wire::decline::BUSY,
                    _ => wire::decline::WIFI_UNAVAILABLE,
                };
                self.fail(facts.now_ms, true, reason, platform, out);
            }
        }
    }

    fn poll_starting(&mut self, facts: &Facts, platform: &mut dyn Platform, out: &mut Vec<Output>) {
        match platform.group_state() {
            super::group::GroupState::Aborted(2) => {
                self.fail(facts.now_ms, false, wire::decline::BUSY, platform, out);
                return;
            }
            super::group::GroupState::Aborted(_) | super::group::GroupState::Stopped => {
                self.fail(facts.now_ms, true, wire::decline::WIFI_UNAVAILABLE, platform, out);
                return;
            }
            super::group::GroupState::Pending => {}
            super::group::GroupState::Started => {
                if facts.now_ms - self.last_address_poll_ms >= ADDRESS_POLL_MS {
                    self.last_address_poll_ms = facts.now_ms;
                    if self.address.is_none() {
                        self.address = platform.group_address(&self.before);
                    }
                    if let Some(address) = self.address {
                        match platform.group_is_public(address) {
                            ProfileRead::Pending => self.profile_pending = true,
                            ProfileRead::Unreadable => {
                                faults::set_profile_unknown();
                                platform.release();
                                out.push(self.trace(
                                    "abort",
                                    "",
                                    "profile_unknown",
                                    facts.now_ms,
                                ));
                                self.state = State::Off;
                                return;
                            }
                            ProfileRead::NotPublic => {
                                faults::set_group_not_public();
                                platform.release();
                                out.push(self.trace(
                                    "abort",
                                    "",
                                    "profile_not_public",
                                    facts.now_ms,
                                ));
                                self.state = State::Off;
                                return;
                            }
                            ProfileRead::Public => {
                                self.profile_pending = false;
                                match platform.listen(address) {
                                    Err(_) => {
                                        self.fail(
                                            facts.now_ms,
                                            true,
                                            wire::decline::WIFI_UNAVAILABLE,
                                            platform,
                                            out,
                                        );
                                        return;
                                    }
                                    Ok(port) => {
                                        let ticket = self.ticket.unwrap_or([0; 8]);
                                        out.push(Output::Send(wire::Bwf1::Host(wire::Host {
                                            status: 1,
                                            bearer: wire::bearer::DIRECT,
                                            ticket,
                                            valid_ms: HOST_VALID_MS,
                                            freq_mhz: 0,
                                        })));
                                        out.push(Output::Send(wire::Bwf1::Creds(wire::Creds {
                                            ticket,
                                            port,
                                            ipv4: address.ip.octets(),
                                        })));
                                        self.host_sent = true;
                                        self.host_sent_ms = facts.now_ms;
                                        self.accepted = 0;
                                        out.push(self.trace("host", "", "", facts.now_ms));
                                        self.state = State::Offered;
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if facts.now_ms - self.began_ms >= START_DEADLINE_MS {
            if self.address.is_some() && self.profile_pending {
                faults::set_profile_unknown();
                platform.release();
                out.push(self.trace("abort", "", "profile_unknown", facts.now_ms));
                self.state = State::Off;
            } else {
                self.fail(facts.now_ms, true, wire::decline::WIFI_UNAVAILABLE, platform, out);
            }
        }
    }

    fn poll_offered(
        &mut self,
        facts: &Facts,
        uw: Option<&[u8; 32]>,
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        if facts.now_ms - self.host_sent_ms >= i64::from(HOST_VALID_MS)
            || facts.now_ms - self.host_sent_ms >= ATTEMPT_MAX_MS
        {
            if self.accepted == 0 {
                faults::note_zero_accept();
            }
            self.fail(facts.now_ms, false, wire::decline::SETUP_FAILED, platform, out);
            return;
        }
        if self.binding.is_some() || self.accepted >= MAX_ACCEPTED_SOCKETS_PER_ATTEMPT {
            return;
        }
        let Some(socket) = platform.accept() else {
            return;
        };
        self.accepted += 1;
        faults::clear_zero_accept();
        let Some(uw) = uw else {
            self.fail(facts.now_ms, false, wire::decline::SETUP_FAILED, platform, out);
            return;
        };
        let Some(ticket) = self.ticket else {
            return;
        };
        match HostBinding::new(socket, uw, ticket, facts.now_ms) {
            Ok(binding) => {
                self.binding = Some(binding);
                self.state = State::Binding;
            }
            Err(_) => {
                self.fail(facts.now_ms, false, wire::decline::SETUP_FAILED, platform, out);
            }
        }
    }

    fn poll_binding(&mut self, facts: &Facts, platform: &mut dyn Platform, out: &mut Vec<Output>) {
        let Some(mut binding) = self.binding.take() else {
            self.state = State::Offered;
            return;
        };
        let step = binding.poll(facts.now_ms);
        if binding.ticket_consumed() && !self.begin_sent {
            self.begin_sent = true;
            out.push(Output::Begin {
                ticket: self.ticket.unwrap_or([0; 8]),
            });
        }
        match step {
            Step::Pending => {
                self.binding = Some(binding);
            }
            Step::Failed(failure) => {
                let consumed = binding.ticket_consumed();
                out.push(self.trace("abort", "peer", failure.name(), facts.now_ms));
                if let Some(id) = self.attempt_id.take() {
                    out.push(Output::Died { id });
                }
                if consumed {
                    self.fail(facts.now_ms, false, wire::decline::SETUP_FAILED, platform, out);
                } else {
                    self.state = State::Offered;
                }
            }
            Step::Done(bound) => {
                platform.stop_listening();
                out.push(self.trace("ready", "", "", facts.now_ms));
                if let Some(id) = self.attempt_id.take() {
                    let ticket = self.ticket.unwrap_or([0; 8]);
                    out.push(Output::Attach { bound, ticket, id });
                    self.state = State::Live { id };
                    self.busy_streak = 0;
                } else {
                    self.pending_bound = Some(bound);
                    self.state = State::Binding;
                }
            }
        }
    }

    /// `None`: the session refused the attempt (switched off, another carrier
    /// still pending, Wi-Fi ended). The offer ends without a failure or a
    /// fault: the phone is declined and the group and listener are released.
    pub fn set_attempt_id(
        &mut self,
        id: Option<i32>,
        now_ms: i64,
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        match id {
            Some(id) => {
                self.attempt_id = Some(id);
                if let Some(bound) = self.pending_bound.take() {
                    let ticket = self.ticket.unwrap_or([0; 8]);
                    out.push(Output::Attach { bound, ticket, id });
                    self.state = State::Live { id };
                    self.busy_streak = 0;
                }
            }
            None => {
                if self.host_sent {
                    let ticket = self.ticket.unwrap_or([0; 8]);
                    out.push(Output::Send(wire::Bwf1::Decline(wire::Decline {
                        ticket,
                        reason: wire::decline::NOT_ELIGIBLE,
                    })));
                }
                self.binding = None;
                self.pending_bound = None;
                self.attempt_id = None;
                platform.release();
                self.host_sent = false;
                self.ticket = None;
                out.push(self.trace(
                    "abort",
                    "",
                    wire::decline::name(wire::decline::NOT_ELIGIBLE),
                    now_ms,
                ));
                self.state = State::Off;
            }
        }
    }

    pub fn on_peer_decline(
        &mut self,
        ticket: [u8; 8],
        reason: u8,
        now_ms: i64,
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        if self.ticket != Some(ticket) || !matches!(self.state, State::Offered | State::Binding) {
            return;
        }
        if reason == wire::decline::BUSY {
            self.busy_end(now_ms, platform, out);
        } else {
            self.fail(now_ms, false, reason, platform, out);
        }
    }

    pub fn on_peer_host(&mut self, host: wire::Host, out: &mut Vec<Output>) {
        out.push(Output::Send(wire::Bwf1::Decline(wire::Decline {
            ticket: host.ticket,
            reason: wire::decline::NOT_ELIGIBLE,
        })));
    }

    pub fn on_switch_off(&mut self, now_ms: i64, platform: &mut dyn Platform, out: &mut Vec<Output>) {
        match self.state {
            State::Live { .. } => {
                self.state = State::Off;
            }
            State::Offered | State::Binding => {
                let ticket = self.ticket.unwrap_or([0; 8]);
                out.push(Output::Send(wire::Bwf1::Decline(wire::Decline {
                    ticket,
                    reason: wire::decline::NOT_ELIGIBLE,
                })));
                if let Some(id) = self.attempt_id.take() {
                    out.push(Output::Died { id });
                }
                self.binding = None;
                self.pending_bound = None;
                platform.release();
                self.host_sent = false;
                self.ticket = None;
                self.state = State::Idle {
                    retry_at: now_ms + RETRY_DELAY_MS,
                };
            }
            State::Starting => {
                self.binding = None;
                self.pending_bound = None;
                platform.release();
                self.ticket = None;
                self.state = State::Idle {
                    retry_at: now_ms + RETRY_DELAY_MS,
                };
            }
            State::Idle { .. } | State::Off => {}
        }
    }

    pub fn on_wifi_ended(&mut self, now_ms: i64, platform: &mut dyn Platform, out: &mut Vec<Output>) {
        self.on_switch_off(now_ms, platform, out);
        self.state = State::Off;
    }

    pub fn on_carrier_lost(&mut self, id: i32, now_ms: i64, platform: &mut dyn Platform) {
        match self.state {
            State::Live { id: live } if live == id => {
                platform.release();
                self.host_sent = false;
                self.ticket = None;
                self.state = State::Idle {
                    retry_at: now_ms + COOLDOWN_AFTER_FALLBACK_MS,
                };
            }
            State::Off => platform.release(),
            _ => {}
        }
    }

    pub fn release(&mut self, platform: &mut dyn Platform) {
        platform.release();
        self.binding = None;
        self.pending_bound = None;
        self.attempt_id = None;
        self.host_sent = false;
        self.state = State::Off;
    }

    fn fail(
        &mut self,
        now_ms: i64,
        local: bool,
        reason: u8,
        platform: &mut dyn Platform,
        out: &mut Vec<Output>,
    ) {
        if self.host_sent {
            let ticket = self.ticket.unwrap_or([0; 8]);
            out.push(Output::Send(wire::Bwf1::Decline(wire::Decline {
                ticket,
                reason,
            })));
        }
        if let Some(id) = self.attempt_id.take() {
            out.push(Output::Died { id });
        }
        self.binding = None;
        self.pending_bound = None;
        platform.release();
        self.failed += 1;
        if local {
            self.record_fault();
        }
        self.host_sent = false;
        self.ticket = None;
        let name = wire::decline::name(reason);
        out.push(self.trace("abort", if local { "local" } else { "peer" }, name, now_ms));
        out.push(Output::Trace {
            phase: "carrier",
            detail: format!(
                "hop=call | requested=direct | effective=bluetooth | reason={name}"
            ),
        });
        self.state = if self.failed >= MAX_FAILED_ATTEMPTS_PER_HOP {
            State::Off
        } else {
            State::Idle {
                retry_at: now_ms + RETRY_DELAY_MS,
            }
        };
    }

    fn busy_end(&mut self, now_ms: i64, platform: &mut dyn Platform, out: &mut Vec<Output>) {
        if let Some(id) = self.attempt_id.take() {
            out.push(Output::Died { id });
        }
        self.binding = None;
        self.pending_bound = None;
        platform.release();
        self.host_sent = false;
        self.ticket = None;
        self.busy_streak = self.busy_streak.saturating_add(1);
        let shift = self.busy_streak.saturating_sub(1).min(8);
        let delay = (RETRY_DELAY_MS << shift).min(BUSY_BACKOFF_MAX_MS);
        out.push(self.trace("abort", "peer", "busy", now_ms));
        out.push(Output::Trace {
            phase: "carrier",
            detail: "hop=call | requested=direct | effective=bluetooth | reason=busy".to_owned(),
        });
        self.state = State::Idle {
            retry_at: now_ms + delay,
        };
    }

    fn trace(
        &self,
        event: &'static str,
        fault: &'static str,
        reason: &'static str,
        now_ms: i64,
    ) -> Output {
        let setup = (now_ms - self.began_ms).max(0).to_string();
        Output::Trace {
            phase: "wifi_upgrade",
            detail: log::detail(&[
                ("event", event),
                ("bearer", "direct"),
                ("role", "host"),
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

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use crate::wifi::{platform::PlatformError, probe::Band, probe::WifiCaps, probe::WifiRadio};
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct Counters {
        pub starts: u32,
        pub releases: u32,
        pub listens: u32,
        pub stops_listening: u32,
        pub address_polls: u32,
    }

    pub(crate) struct FakePlatform {
        pub counters: Arc<Mutex<Counters>>,
        pub probe: ProbeState,
        pub group_states: VecDeque<super::super::group::GroupState>,
        pub last_group_state: super::super::group::GroupState,
        pub address_after: u32,
        pub address: Option<GroupAddress>,
        pub profile: VecDeque<ProfileRead>,
        pub last_profile: ProfileRead,
        pub port: u16,
        pub accepts: VecDeque<crate::wifi::socket::WifiSocket>,
        pub start_error: Option<PlatformError>,
        pub before: Vec<AdapterInfo>,
        pub last_ssid: Option<String>,
    }

    impl FakePlatform {
        pub(crate) fn new() -> Self {
            Self {
                counters: Arc::new(Mutex::new(Counters::default())),
                probe: ProbeState::Ready(WifiCaps {
                    radio: WifiRadio::On,
                    hotspot: Some(false),
                    band: Band::Ghz5Plus,
                }),
                group_states: VecDeque::from([super::super::group::GroupState::Started]),
                last_group_state: super::super::group::GroupState::Started,
                address_after: 1,
                address: Some(GroupAddress {
                    ip: "192.168.137.1".parse().unwrap(),
                    prefix: 24,
                }),
                profile: VecDeque::new(),
                last_profile: ProfileRead::Public,
                port: 47123,
                accepts: VecDeque::new(),
                start_error: None,
                before: Vec::new(),
                last_ssid: None,
            }
        }

        pub(crate) fn counts(&self) -> (u32, u32, u32, u32, u32) {
            let counts = self.counters.lock().unwrap();
            (
                counts.starts,
                counts.releases,
                counts.listens,
                counts.stops_listening,
                counts.address_polls,
            )
        }
    }

    impl Platform for FakePlatform {
        fn probe(&mut self) -> ProbeState {
            self.probe
        }
        fn snapshot(&mut self) -> Vec<AdapterInfo> {
            self.before.clone()
        }
        fn start_group(&mut self, ssid: &str, _psk: &str) -> Result<(), PlatformError> {
            self.counters.lock().unwrap().starts += 1;
            self.last_ssid = Some(ssid.to_owned());
            match self.start_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn group_state(&mut self) -> super::super::group::GroupState {
            if let Some(state) = self.group_states.pop_front() {
                self.last_group_state = state;
            }
            self.last_group_state
        }
        fn group_address(&mut self, _before: &[AdapterInfo]) -> Option<GroupAddress> {
            self.counters.lock().unwrap().address_polls += 1;
            if self.counters.lock().unwrap().address_polls >= self.address_after {
                self.address
            } else {
                None
            }
        }
        fn group_is_public(&mut self, _address: GroupAddress) -> ProfileRead {
            self.profile.pop_front().unwrap_or(self.last_profile)
        }
        fn listen(&mut self, _address: GroupAddress) -> Result<u16, PlatformError> {
            self.counters.lock().unwrap().listens += 1;
            Ok(self.port)
        }
        fn accept(&mut self) -> Option<crate::wifi::socket::WifiSocket> {
            self.accepts.pop_front()
        }
        fn stop_listening(&mut self) {
            self.counters.lock().unwrap().stops_listening += 1;
        }
        fn release(&mut self) {
            self.counters.lock().unwrap().releases += 1;
        }
    }

    pub(crate) fn socket_pair() -> (crate::wifi::socket::WifiSocket, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let guest = std::net::TcpStream::connect(address).unwrap();
        let (host, _) = listener.accept().unwrap();
        (
            crate::wifi::socket::WifiSocket::from_stream(host).unwrap(),
            guest,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakePlatform;
    use super::*;
    use crate::wifi::{
        binding::test_guest::Guest,
        group::GroupState,
        limits::HOST_VALID_MS,
        probe::{Band, WifiCaps, WifiRadio},
        wire::caps_bit,
    };
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    const UW: [u8; 32] = [7; 32];
    const TICK: i64 = 4_000;

    fn base_facts(now_ms: i64) -> Facts {
        Facts {
            now_ms,
            armed: true,
            phase_active: true,
            video_on_since_ms: Some(0),
            caps_exchanged: true,
            local_caps: roles::windows_caps(true),
            peer_caps: Some(wire::Caps {
                bearers: caps_bit::DIRECT_CLIENT,
                free_host: 0,
                free_guest: 0,
            }),
            local_is_initiator: true,
            uw_ready: true,
            fallbacks: 0,
            carrier_up: false,
            send_on: 0,
        }
    }

    fn owner() -> (Owner, Arc<Mutex<Faults>>) {
        let faults = Arc::new(Mutex::new(Faults::new()));
        (Owner::with_faults(faults.clone()), faults)
    }

    fn tick(owner: &mut Owner, now_ms: i64, platform: &mut FakePlatform) -> Vec<Output> {
        let facts = base_facts(now_ms);
        let mut out = Vec::new();
        owner.tick(&facts, Some(&UW), platform, &mut out);
        out
    }

    fn host_ticket(outputs: &[Output]) -> Option<[u8; 8]> {
        outputs.iter().find_map(|output| match output {
            Output::Send(wire::Bwf1::Host(host)) => Some(host.ticket),
            _ => None,
        })
    }

    fn decline_reason(outputs: &[Output]) -> Option<u8> {
        outputs.iter().find_map(|output| match output {
            Output::Send(wire::Bwf1::Decline(decline)) => Some(decline.reason),
            _ => None,
        })
    }

    fn trace_contains(outputs: &[Output], needle: &str) -> bool {
        outputs.iter().any(|output| match output {
            Output::Trace { detail, .. } => detail.contains(needle),
            _ => false,
        })
    }

    fn drive_to_offered(owner: &mut Owner, platform: &mut FakePlatform, now_ms: i64) -> [u8; 8] {
        for _ in 0..8 {
            let outputs = tick(owner, now_ms, platform);
            if let Some(ticket) = host_ticket(&outputs) {
                return ticket;
            }
        }
        panic!("no host offer: {}", owner.state_name());
    }

    fn drive_to_binding(
        owner: &mut Owner,
        platform: &mut FakePlatform,
        now_ms: i64,
    ) -> [u8; 8] {
        let ticket = drive_to_offered(owner, platform, now_ms);
        for _ in 0..4 {
            let _ = tick(owner, now_ms, platform);
            if owner.state_name() == "binding" {
                return ticket;
            }
        }
        panic!("no binding: {}", owner.state_name());
    }

    fn drive_to_live(
        owner: &mut Owner,
        platform: &mut FakePlatform,
        now_ms: i64,
        ticket: [u8; 8],
        guest: &mut Guest,
        next_id: &mut i32,
        out: &mut Vec<Output>,
    ) -> i32 {
        guest.send_frame2();
        let mut id = None;
        for _ in 0..40 {
            let mut outputs = tick(owner, now_ms, platform);
            if outputs.iter().any(|output| matches!(output, Output::Begin { .. })) {
                let attempt = *next_id;
                *next_id += 1;
                let mut extra = Vec::new();
                owner.set_attempt_id(Some(attempt), now_ms, platform, &mut extra);
                outputs.extend(extra);
                id = Some(attempt);
                out.extend(outputs);
                break;
            }
            out.extend(outputs);
            std::thread::sleep(Duration::from_millis(2));
        }
        let id = id.expect("no Begin");
        guest.read_frame3();
        guest.send_bind(wire::role::GUEST, ticket);
        let _ = guest.read_record();
        std::thread::sleep(Duration::from_millis(2));
        out.extend(tick(owner, now_ms, platform));
        let ready = guest.read_record();
        assert_eq!(ready.header.kind, wire::HOP_TYPE);
        guest.send_ready();
        for _ in 0..40 {
            out.extend(tick(owner, now_ms, platform));
            if owner.state_name() == "live" {
                return id;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("not live: {}", owner.state_name());
    }

    #[test]
    fn no_attempt_starts_before_the_call_has_had_video_for_three_seconds() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let _ = tick(&mut owner, 2_999, &mut platform);
        assert_eq!(platform.counts().0, 0);
        let _ = tick(&mut owner, 3_000, &mut platform);
        assert_eq!(platform.counts().0, 1);
    }

    #[test]
    fn an_audio_only_call_never_starts_an_attempt() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut facts = base_facts(0);
        facts.video_on_since_ms = None;
        for now in (0..=60_000).step_by(1_000) {
            facts.now_ms = now;
            let mut out = Vec::new();
            owner.tick(&facts, Some(&UW), &mut platform, &mut out);
        }
        assert_eq!(platform.counts().0, 0);
    }

    #[test]
    fn no_attempt_starts_when_the_peer_has_no_wifi_or_caps_are_not_exchanged_or_the_switch_is_off() {
        for (peer, caps_exchanged, armed) in [
            (None, true, true),
            (Some(wire::Caps::default()), false, true),
            (
                Some(wire::Caps {
                    bearers: caps_bit::DIRECT_CLIENT,
                    free_host: 0,
                    free_guest: 0,
                }),
                true,
                false,
            ),
        ] {
            let (mut owner, _faults) = owner();
            let mut platform = FakePlatform::new();
            let mut facts = base_facts(5_000);
            facts.peer_caps = peer;
            facts.caps_exchanged = caps_exchanged;
            facts.armed = armed;
            let mut out = Vec::new();
            owner.tick(&facts, Some(&UW), &mut platform, &mut out);
            assert_eq!(platform.counts().0, 0);
        }
    }

    #[test]
    fn a_probe_that_is_still_pending_waits_and_a_hotspot_or_2_4_ghz_refusal_does_not_start_or_count_as_a_failure() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        platform.probe = ProbeState::Pending;
        let _ = tick(&mut owner, TICK, &mut platform);
        assert_eq!(platform.counts().0, 0);
        platform.probe = ProbeState::Ready(WifiCaps {
            radio: WifiRadio::On,
            hotspot: Some(true),
            band: Band::Ghz5Plus,
        });
        let _ = tick(&mut owner, TICK + 1, &mut platform);
        assert_eq!(platform.counts().0, 0);
        assert_eq!(owner.failed, 0);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        platform.probe = ProbeState::Ready(WifiCaps {
            radio: WifiRadio::On,
            hotspot: Some(false),
            band: Band::Ghz24,
        });
        let _ = tick(&mut owner, TICK + 2, &mut platform);
        assert_eq!(platform.counts().0, 0);
        assert_eq!(owner.failed, 0);
    }

    #[test]
    fn a_successful_attempt_offers_host_then_creds_binds_and_attaches() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let (host_socket, guest_stream) = super::fake::socket_pair();
        platform.accepts.push_back(host_socket);
        let mut offered = Vec::new();
        let mut ticket = None;
        for _ in 0..8 {
            offered = tick(&mut owner, TICK, &mut platform);
            if let Some(found) = host_ticket(&offered) {
                ticket = Some(found);
                break;
            }
        }
        let ticket = ticket.expect("host offer");
        let host = offered.iter().find_map(|output| match output {
            Output::Send(wire::Bwf1::Host(host)) => Some(*host),
            _ => None,
        });
        let host = host.expect("host record");
        assert_eq!(host.status, 1);
        assert_eq!(host.bearer, wire::bearer::DIRECT);
        assert_eq!(host.valid_ms, HOST_VALID_MS);
        assert_eq!(host.freq_mhz, 0);
        let creds = offered.iter().find_map(|output| match output {
            Output::Send(wire::Bwf1::Creds(creds)) => Some(*creds),
            _ => None,
        });
        let creds = creds.expect("creds record in the same tick as HOST");
        assert_eq!(creds.ticket, ticket);
        assert_eq!(creds.port, platform.port);
        assert_eq!(creds.ipv4, [192, 168, 137, 1]);
        let expected_ssid = keys::direct_ssid(&UW, &ticket).unwrap();
        assert_eq!(platform.last_ssid.as_deref(), Some(expected_ssid.as_str()));
        for _ in 0..4 {
            let _ = tick(&mut owner, TICK, &mut platform);
            if owner.state_name() == "binding" {
                break;
            }
        }
        assert_eq!(owner.state_name(), "binding");
        let mut guest = Guest::new(guest_stream, &UW, ticket);
        let mut next_id = 1;
        let mut live_out = Vec::new();
        let id = drive_to_live(&mut owner, &mut platform, TICK, ticket, &mut guest, &mut next_id, &mut live_out);
        let begin = live_out
            .iter()
            .position(|output| matches!(output, Output::Begin { .. }))
            .expect("Begin");
        let attach = live_out
            .iter()
            .position(|output| matches!(output, Output::Attach { .. }))
            .expect("Attach");
        assert!(begin < attach);
        assert_eq!(platform.counts().0, 1);
        assert_eq!(id, 1);
        assert_eq!(owner.state_name(), "live");
    }

    #[test]
    fn at_most_four_sockets_are_accepted_per_attempt() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        for _ in 0..5 {
            let (socket, mut guest) = super::fake::socket_pair();
            platform.accepts.push_back(socket);
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = guest.write_all(&[0u8; 126]);
            });
        }
        let _ = drive_to_offered(&mut owner, &mut platform, TICK);
        for now in (TICK + 1)..(TICK + 200) {
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert_eq!(owner.accepted, 4);
        assert_eq!(platform.accepts.len(), 1);
        let outputs = tick(&mut owner, TICK + i64::from(HOST_VALID_MS), &mut platform);
        assert_eq!(decline_reason(&outputs), Some(wire::decline::SETUP_FAILED));
    }

    #[test]
    fn a_peer_that_never_joins_ends_the_attempt_at_the_host_validity_without_a_local_fault() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        let _ = drive_to_offered(&mut owner, &mut platform, TICK);
        let outputs = tick(&mut owner, TICK + i64::from(HOST_VALID_MS), &mut platform);
        assert_eq!(decline_reason(&outputs), Some(wire::decline::SETUP_FAILED));
        assert!(platform.counts().1 >= 1);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        assert!(matches!(owner.state, State::Idle { retry_at } if retry_at == TICK + i64::from(HOST_VALID_MS) + RETRY_DELAY_MS));
    }

    #[test]
    fn the_group_that_does_not_start_in_ten_seconds_is_a_local_fault_and_nothing_is_offered() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        platform.group_states.clear();
        platform.last_group_state = GroupState::Pending;
        let _ = tick(&mut owner, TICK, &mut platform);
        let mut offered = false;
        for now in (TICK + 1)..=(TICK + START_DEADLINE_MS) {
            let outputs = tick(&mut owner, now, &mut platform);
            offered |= host_ticket(&outputs).is_some();
        }
        assert!(!offered);
        assert!(platform.counts().1 >= 1);
        assert_eq!(faults.lock().unwrap().recorded(), 1);
    }

    #[test]
    fn a_busy_radio_is_not_a_local_fault() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        platform.group_states.clear();
        platform.last_group_state = GroupState::Aborted(2);
        let _ = tick(&mut owner, TICK, &mut platform);
        let _ = tick(&mut owner, TICK + 1, &mut platform);
        assert_eq!(owner.failed, 1);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
    }

    #[test]
    fn a_consumed_ticket_that_fails_later_ends_the_attempt_and_the_next_one_uses_a_fresh_ticket() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let (host_socket, guest_stream) = super::fake::socket_pair();
        platform.accepts.push_back(host_socket);
        let ticket = drive_to_binding(&mut owner, &mut platform, TICK);
        let mut guest = Guest::new(guest_stream, &UW, ticket);
        let mut point = [0u8; 65];
        point[0] = 4;
        guest.send_frame2_for(ticket, ticket, point);
        let outputs = tick(&mut owner, TICK, &mut platform);
        assert!(matches!(outputs.first(), Some(Output::Begin { .. })));
        assert_eq!(owner.failed, 1);
        assert_eq!(owner.state_name(), "idle");
        let mut next = Vec::new();
        for offset in 0..5 {
            next = tick(&mut owner, TICK + RETRY_DELAY_MS + offset, &mut platform);
            if host_ticket(&next).is_some() {
                break;
            }
        }
        assert_eq!(platform.counts().0, 2);
        let fresh = host_ticket(&next).expect("next host offer");
        assert_ne!(fresh, ticket);
    }

    #[test]
    fn a_failed_pre_keeps_listening_for_the_real_phone() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let (first_socket, first_stream) = super::fake::socket_pair();
        let (second_socket, second_stream) = super::fake::socket_pair();
        platform.accepts.push_back(first_socket);
        platform.accepts.push_back(second_socket);
        let ticket = drive_to_offered(&mut owner, &mut platform, TICK);
        let _ = tick(&mut owner, TICK, &mut platform);
        let mut wrong = [9; 32];
        wrong[0] = 8;
        let mut first = Guest::new(first_stream, &wrong, ticket);
        first.send_frame2();
        let outputs = tick(&mut owner, TICK, &mut platform);
        assert!(!outputs.iter().any(|output| matches!(output, Output::Begin { .. })));
        assert!(!outputs.iter().any(|output| matches!(output, Output::Died { .. })));
        let _ = tick(&mut owner, TICK + 1, &mut platform);
        let mut second = Guest::new(second_stream, &UW, ticket);
        let mut next_id = 1;
        let mut live_out = Vec::new();
        let _ = drive_to_live(&mut owner, &mut platform, TICK + 1, ticket, &mut second, &mut next_id, &mut live_out);
        assert_eq!(owner.failed, 0);
        assert_eq!(owner.state_name(), "live");
    }

    #[test]
    fn one_ticket_gets_exactly_one_attempt_id() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut bad_streams = Vec::new();
        for _ in 0..3 {
            let (socket, stream) = super::fake::socket_pair();
            platform.accepts.push_back(socket);
            bad_streams.push(stream);
        }
        let (live_socket, live_stream) = super::fake::socket_pair();
        platform.accepts.push_back(live_socket);
        let ticket = drive_to_offered(&mut owner, &mut platform, TICK);
        let mut begins = 0usize;
        for (index, stream) in bad_streams.into_iter().enumerate() {
            let _ = tick(&mut owner, TICK + index as i64, &mut platform);
            assert_eq!(owner.state_name(), "binding");
            let mut wrong = [9; 32];
            wrong[0] = 8;
            let mut guest = Guest::new(stream, &wrong, ticket);
            guest.send_frame2();
            let outputs = tick(&mut owner, TICK + index as i64, &mut platform);
            begins += outputs
                .iter()
                .filter(|output| matches!(output, Output::Begin { .. }))
                .count();
        }
        let _ = tick(&mut owner, TICK + 3, &mut platform);
        assert_eq!(owner.state_name(), "binding");
        let mut guest = Guest::new(live_stream, &UW, ticket);
        let mut next_id = 1;
        let mut out = Vec::new();
        let id = drive_to_live(&mut owner, &mut platform, TICK + 3, ticket, &mut guest, &mut next_id, &mut out);
        begins += out
            .iter()
            .filter(|output| matches!(output, Output::Begin { .. }))
            .count();
        assert_eq!(begins, 1);
        let attach = out.iter().find_map(|output| match output {
            Output::Attach { id, ticket: attached, .. } => Some((*id, *attached)),
            _ => None,
        });
        assert_eq!(attach, Some((id, ticket)));
    }

    #[test]
    fn the_attempt_reads_the_adapters_at_most_every_200_ms() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        platform.address = None;
        platform.address_after = 100_000;
        let _ = tick(&mut owner, TICK, &mut platform);
        for now in (TICK + 1)..=(TICK + 1_000) {
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert!(platform.counts().4 <= 6, "{} polls", platform.counts().4);
    }

    #[test]
    fn a_group_on_a_private_profile_is_never_offered_and_withholds_bit_2() {
        faults::reset_for_tests();
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        platform.last_profile = ProfileRead::NotPublic;
        let _ = tick(&mut owner, TICK, &mut platform);
        let _ = tick(&mut owner, TICK + 1, &mut platform);
        assert_eq!(owner.state_name(), "off");
        assert_eq!(owner.failed, 0);
        assert_eq!(platform.counts().2, 0);
        assert!(platform.counts().1 >= 1);
        assert!(faults::group_not_public());
        faults::reset_for_tests();
    }

    #[test]
    fn a_profile_not_yet_known_is_polled_until_the_start_deadline() {
        faults::reset_for_tests();
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        platform.last_profile = ProfileRead::Pending;
        let _ = tick(&mut owner, TICK, &mut platform);
        let mut trace = false;
        for now in (TICK + 1)..=(TICK + START_DEADLINE_MS) {
            let outputs = tick(&mut owner, now, &mut platform);
            trace |= trace_contains(&outputs, "reason=profile_unknown");
        }
        assert!(trace);
        assert_eq!(owner.state_name(), "off");
        assert_eq!(owner.failed, 0);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        assert!(faults::profile_unknown());
        faults::reset_for_tests();
    }

    #[test]
    fn a_failing_profile_reader_records_no_pause() {
        faults::reset_for_tests();
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        platform.last_profile = ProfileRead::Unreadable;
        let _ = tick(&mut owner, TICK, &mut platform);
        let _ = tick(&mut owner, TICK + 1, &mut platform);
        assert_eq!(owner.state_name(), "off");
        assert_eq!(owner.failed, 0);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        assert!(faults::profile_unknown());
        faults::reset_for_tests();
    }

    #[test]
    fn a_busy_decline_is_not_a_failed_attempt_and_backs_off() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut now = TICK;
        let mut expected = [30_000, 60_000, 120_000];
        for delay in expected.iter_mut() {
            let ticket = drive_to_offered(&mut owner, &mut platform, now);
            let outputs = {
                let mut out = Vec::new();
                owner.on_peer_decline(ticket, wire::decline::BUSY, now, &mut platform, &mut out);
                out
            };
            assert_eq!(owner.failed, 0);
            assert_ne!(owner.state_name(), "off");
            assert!(trace_contains(&outputs, "reason=busy"));
            let State::Idle { retry_at } = owner.state else {
                panic!("not idle");
            };
            assert!(retry_at - now >= *delay, "{} < {delay}", retry_at - now);
            *delay = retry_at - now;
            now = retry_at;
        }
    }

    #[test]
    fn a_live_attempt_resets_the_busy_back_off() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut now = TICK;
        for _ in 0..2 {
            let ticket = drive_to_offered(&mut owner, &mut platform, now);
            let mut out = Vec::new();
            owner.on_peer_decline(ticket, wire::decline::BUSY, now, &mut platform, &mut out);
            let State::Idle { retry_at } = owner.state else {
                panic!("not idle");
            };
            now = retry_at;
        }
        let (socket, stream) = super::fake::socket_pair();
        platform.accepts.push_back(socket);
        let ticket = drive_to_binding(&mut owner, &mut platform, now);
        let mut guest = Guest::new(stream, &UW, ticket);
        let mut next_id = 1;
        let mut live_out = Vec::new();
        let id = drive_to_live(&mut owner, &mut platform, now, ticket, &mut guest, &mut next_id, &mut live_out);
        owner.on_carrier_lost(id, now + 1_000, &mut platform);
        let State::Idle { retry_at } = owner.state else {
            panic!("not idle after loss");
        };
        now = retry_at;
        let ticket = drive_to_offered(&mut owner, &mut platform, now);
        let mut out = Vec::new();
        owner.on_peer_decline(ticket, wire::decline::BUSY, now, &mut platform, &mut out);
        let State::Idle { retry_at } = owner.state else {
            panic!("not idle after busy");
        };
        assert_eq!(retry_at - now, RETRY_DELAY_MS);
    }

    #[test]
    fn two_zero_accept_expiries_raise_the_firewall_note() {
        faults::reset_for_tests();
        let (mut first, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut now = TICK;
        for _ in 0..2 {
            let _ = drive_to_offered(&mut first, &mut platform, now);
            let _ = tick(&mut first, now + i64::from(HOST_VALID_MS), &mut platform);
            now += i64::from(HOST_VALID_MS) + RETRY_DELAY_MS;
        }
        assert!(faults::firewall_suspected());
        let (mut third, _faults) = owner();
        let (socket, mut guest) = super::fake::socket_pair();
        platform.accepts.push_back(socket);
        let _ = drive_to_offered(&mut third, &mut platform, now);
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = guest.write_all(&[0u8; 126]);
        });
        let _ = tick(&mut third, now, &mut platform);
        assert!(!faults::firewall_suspected());
        faults::reset_for_tests();
    }

    #[test]
    fn an_owner_output_printed_for_the_trace_never_contains_the_ticket_or_the_address() {
        let host = Output::Send(wire::Bwf1::Host(wire::Host {
            status: 1,
            bearer: wire::bearer::DIRECT,
            ticket: [0xab; 8],
            valid_ms: 20_000,
            freq_mhz: 0,
        }));
        let creds = Output::Send(wire::Bwf1::Creds(wire::Creds {
            ticket: [0xab; 8],
            port: 47_123,
            ipv4: [192, 168, 137, 1],
        }));
        for output in [host, creds] {
            let text = format!("{output:?}");
            assert!(!text.contains("abab"), "{text}");
            assert!(!text.contains("171"), "{text}");
            assert!(!text.contains("192.168"), "{text}");
            assert!(!text.contains("47123"), "{text}");
        }
    }

    #[test]
    fn two_failed_attempts_end_wifi_for_the_call() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let mut now = TICK;
        for _ in 0..2 {
            let _ = drive_to_offered(&mut owner, &mut platform, now);
            let _ = tick(&mut owner, now + i64::from(HOST_VALID_MS), &mut platform);
            now += i64::from(HOST_VALID_MS) + RETRY_DELAY_MS;
        }
        assert_eq!(owner.state_name(), "off");
        for _ in 0..20 {
            now += 30_000;
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert_eq!(platform.counts().0, 2);
    }

    #[test]
    fn a_peer_decline_for_the_current_ticket_releases_and_counts_one_failure() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let ticket = drive_to_offered(&mut owner, &mut platform, TICK);
        let before = platform.counts().1;
        let mut out = Vec::new();
        owner.on_peer_decline(ticket, wire::decline::WIFI_UNAVAILABLE, TICK, &mut platform, &mut out);
        assert_eq!(owner.failed, 1);
        assert!(platform.counts().1 > before);
        assert_eq!(owner.state_name(), "idle");
    }

    #[test]
    fn a_decline_for_another_ticket_is_ignored() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let ticket = drive_to_offered(&mut owner, &mut platform, TICK);
        let mut other = ticket;
        other[0] ^= 1;
        let mut out = Vec::new();
        owner.on_peer_decline(other, wire::decline::WIFI_UNAVAILABLE, TICK, &mut platform, &mut out);
        assert_eq!(owner.failed, 0);
        assert_eq!(owner.state_name(), "offered");
    }

    #[test]
    fn a_peer_decline_of_reason_7_is_a_non_local_failure_and_names_not_5ghz() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        let ticket = drive_to_offered(&mut owner, &mut platform, TICK);
        let mut out = Vec::new();
        owner.on_peer_decline(ticket, wire::decline::NOT_5GHZ, TICK, &mut platform, &mut out);
        assert_eq!(owner.failed, 1);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        assert!(trace_contains(&out, "reason=not_5ghz"));
    }

    #[test]
    fn a_host_from_the_peer_is_answered_with_decline_one_and_changes_nothing() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let _ = drive_to_offered(&mut owner, &mut platform, TICK);
        let mut out = Vec::new();
        owner.on_peer_host(
            wire::Host {
                status: 1,
                bearer: wire::bearer::DIRECT,
                ticket: [3; 8],
                valid_ms: 20_000,
                freq_mhz: 0,
            },
            &mut out,
        );
        assert_eq!(decline_reason(&out), Some(wire::decline::NOT_ELIGIBLE));
        assert_eq!(owner.state_name(), "offered");
    }

    #[test]
    fn turning_the_switch_off_during_an_offer_declines_releases_and_counts_no_failure() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let _ = drive_to_offered(&mut owner, &mut platform, TICK);
        let mut out = Vec::new();
        owner.on_switch_off(TICK, &mut platform, &mut out);
        assert_eq!(decline_reason(&out), Some(wire::decline::NOT_ELIGIBLE));
        assert_eq!(platform.counts().1, 1);
        assert_eq!(owner.failed, 0);
        assert_eq!(owner.state_name(), "idle");
        let mut facts = base_facts(TICK + RETRY_DELAY_MS);
        facts.armed = false;
        let mut out = Vec::new();
        owner.tick(&facts, Some(&UW), &mut platform, &mut out);
        assert_eq!(platform.counts().0, 1);
        facts.armed = true;
        let mut out = Vec::new();
        owner.tick(&facts, Some(&UW), &mut platform, &mut out);
        assert_eq!(platform.counts().0, 2);
    }

    #[test]
    fn turning_the_switch_off_while_live_hands_the_carrier_to_the_machine_and_releases_after_its_close() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let (socket, stream) = super::fake::socket_pair();
        platform.accepts.push_back(socket);
        let ticket = drive_to_binding(&mut owner, &mut platform, TICK);
        let mut guest = Guest::new(stream, &UW, ticket);
        let mut next_id = 1;
        let mut live_out = Vec::new();
        let id = drive_to_live(&mut owner, &mut platform, TICK, ticket, &mut guest, &mut next_id, &mut live_out);
        let before = platform.counts().1;
        let mut out = Vec::new();
        owner.on_switch_off(TICK + 1, &mut platform, &mut out);
        assert_eq!(owner.state_name(), "off");
        assert_eq!(platform.counts().1, before);
        owner.on_carrier_lost(id, TICK + 2, &mut platform);
        assert_eq!(platform.counts().1, before + 1);
        for now in (TICK + 3)..(TICK + 30_000) {
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert_eq!(platform.counts().0, 1);
    }

    #[test]
    fn when_the_live_carrier_is_lost_the_group_is_released_and_a_new_attempt_waits_thirty_seconds() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        let (socket, stream) = super::fake::socket_pair();
        platform.accepts.push_back(socket);
        let ticket = drive_to_binding(&mut owner, &mut platform, TICK);
        let mut guest = Guest::new(stream, &UW, ticket);
        let mut next_id = 1;
        let mut live_out = Vec::new();
        let id = drive_to_live(&mut owner, &mut platform, TICK, ticket, &mut guest, &mut next_id, &mut live_out);
        let before = platform.counts().1;
        owner.on_carrier_lost(id, TICK + 1_000, &mut platform);
        assert_eq!(platform.counts().1, before + 1);
        let State::Idle { retry_at } = owner.state else {
            panic!("not idle");
        };
        assert_eq!(retry_at, TICK + 1_000 + COOLDOWN_AFTER_FALLBACK_MS);
        for now in (TICK + 1_001)..retry_at {
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert_eq!(platform.counts().0, 1);
        owner.on_carrier_lost(99, retry_at, &mut platform);
        assert_eq!(platform.counts().1, before + 1);
    }

    #[test]
    fn three_local_faults_pause_attempts_phone_wide() {
        let shared = Arc::new(Mutex::new(Faults::new()));
        for _ in 0..3 {
            let mut owner = Owner::with_faults(shared.clone());
            let mut platform = FakePlatform::new();
            platform.start_error = Some(PlatformError::Bind);
            let _ = tick(&mut owner, TICK, &mut platform);
            assert_eq!(owner.failed, 1);
        }
        let mut fourth = Owner::with_faults(shared);
        let mut platform = FakePlatform::new();
        let _ = tick(&mut fourth, TICK + 1_000, &mut platform);
        assert_eq!(platform.counts().0, 0);
    }

    static FAULT_CLOCK_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    fn fault_clock() -> i64 {
        FAULT_CLOCK_MS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn a_local_fault_pause_ends_an_hour_after_the_third_fault_for_a_call_that_starts_later() {
        use std::sync::atomic::Ordering::SeqCst;
        const HOUR: i64 = 3_600_000;
        let shared = Arc::new(Mutex::new(Faults::new()));
        // MeshLive has run for ten hours; three calls each hit a local fault.
        FAULT_CLOCK_MS.store(10 * HOUR, SeqCst);
        for _ in 0..3 {
            let mut owner = Owner::with_faults_and_clock(shared.clone(), fault_clock);
            let mut platform = FakePlatform::new();
            platform.start_error = Some(PlatformError::Native(5));
            let _ = tick(&mut owner, TICK, &mut platform);
            assert_eq!(platform.counts().0, 1);
        }
        // The next call, a minute later, is paused.
        FAULT_CLOCK_MS.store(10 * HOUR + 60_000, SeqCst);
        let mut owner = Owner::with_faults_and_clock(shared.clone(), fault_clock);
        let mut platform = FakePlatform::new();
        let _ = tick(&mut owner, TICK, &mut platform);
        assert_eq!(platform.counts().0, 0, "a call within the hour must not start");
        // A call that starts 65 minutes after the faults (its own clock reads
        // a few seconds) is no longer paused.
        FAULT_CLOCK_MS.store(11 * HOUR + 5 * 60_000, SeqCst);
        let mut owner = Owner::with_faults_and_clock(shared, fault_clock);
        let mut platform = FakePlatform::new();
        let _ = tick(&mut owner, TICK, &mut platform);
        assert_eq!(platform.counts().0, 1, "the pause lasts one hour, not the rest of the process");
    }

    #[test]
    fn a_session_that_refuses_the_attempt_id_gets_the_group_released_and_the_phone_declined() {
        let (mut owner, faults) = owner();
        let mut platform = FakePlatform::new();
        let (socket, stream) = super::fake::socket_pair();
        platform.accepts.push_back(socket);
        let ticket = drive_to_binding(&mut owner, &mut platform, TICK);
        let mut guest = Guest::new(stream, &UW, ticket);
        guest.send_frame2();
        let mut began = false;
        for _ in 0..40 {
            let outputs = tick(&mut owner, TICK, &mut platform);
            if outputs.iter().any(|output| matches!(output, Output::Begin { .. })) {
                began = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(began, "no Begin");
        let releases = platform.counts().1;
        let mut out = Vec::new();
        owner.set_attempt_id(None, TICK, &mut platform, &mut out);
        assert_eq!(platform.counts().1, releases + 1, "the group must be released");
        let declined = out.iter().find_map(|output| match output {
            Output::Send(wire::Bwf1::Decline(decline)) => Some(*decline),
            _ => None,
        });
        let declined = declined.expect("the phone is told the offer is over");
        assert_eq!(declined.ticket, ticket);
        assert_eq!(declined.reason, wire::decline::NOT_ELIGIBLE);
        assert_eq!(owner.failed, 0);
        assert_eq!(faults.lock().unwrap().recorded(), 0);
        assert_eq!(owner.state_name(), "off");
        // Nothing starts again in this call.
        for now in (TICK..TICK + 120_000).step_by(1_000) {
            let _ = tick(&mut owner, now, &mut platform);
        }
        assert_eq!(platform.counts().0, 1);
    }

    #[test]
    fn releasing_twice_is_harmless() {
        let (mut owner, _faults) = owner();
        let mut platform = FakePlatform::new();
        owner.release(&mut platform);
        owner.release(&mut platform);
        assert_eq!(platform.counts().1, 2);
    }
}


