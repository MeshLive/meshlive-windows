//! Two-endpoint world for the cutover model test. It owns the ground truth:
//! two carrier slots per pair (the old link and up to `Cfg::max_attempts`
//! numbered new links used one after another), a FIFO queue per sender and
//! carrier with arbitrary delay between carriers, carrier death at any step on
//! either side with each side detecting it up to `Cfg::detect_max_ticks` later
//! (the half-open case), optionally a reader queue between the carriers and the
//! single consumer, the producers of media and of indispensable state, and the
//! invariants. The machine under test only ever sees what a phone would see.
//!
//! All nondeterminism is one integer action code, so a run is a list of codes:
//! an exhaustive search enumerates `enabled`, a random run picks codes, and a
//! counterexample is a code list that `replay_cutover` re-executes and
//! `shrink_cutover` shrinks.

use super::{Action, CarrierCutover, CloseReason, Config, DropReason, Rec, Wire, ANY, NONE, OLD};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

#[derive(Clone)]
pub struct Cfg {
    pub tick_ms: i64,
    pub machine: Config,
    /// Ticks in which anything may happen; the run then quiesces.
    pub horizon_ticks: i32,
    /// Ticks of undisturbed time after quiescence (covers every machine timer).
    pub quiet_ticks: i32,
    /// A fault is detected by each side at most this many ticks later (the 2 s of PROTOCOL).
    pub detect_max_ticks: i32,
    /// Media records each endpoint seals, one per tick.
    pub media_per_endpoint: i32,
    pub media_bytes: i64,
    /// State changes each endpoint makes at arbitrary times.
    pub state_changes_per_endpoint: i32,
    /// Environment faults in the run: a carrier dies, or a side's watchdog closes it.
    pub fault_budget: i32,
    /// Numbered new carriers that may be attempted one after another.
    pub max_attempts: i32,
    /// Chaos beyond the transport contract: duplicate or drop whole records on a live carrier.
    pub duplicates: bool,
    pub drops: bool,
    /// Indispensable states per endpoint (lifecycle, microphone, camera).
    pub keys: usize,
    /// Size of media record (endpoint, sequence) for runs with varied sizes; None: `media_bytes`.
    pub media_bytes_of: Option<Arc<dyn Fn(usize, i64) -> i64 + Send + Sync>>,
    /// Exhaustive search: an in-flight record must be delivered within this many ticks (-1: no bound).
    pub max_delay_ticks: i32,
    /// Exhaustive search: false lets the run quiesce only at the end of the horizon.
    pub quiesce_anywhere: bool,
    /// A side's watchdog may close a healthy carrier.
    pub close_by: bool,
    /// Exhaustive search: the first upgrade attempt starts by this tick.
    pub attempt_by_tick: i32,
    /// Exhaustive search: faults are injected only up to this tick.
    pub fault_by_tick: i32,
    /// Exhaustive search: `BIND`/`READY` complete on both sides the moment the attempt starts.
    pub up_immediately: bool,
    /// Exhaustive search: the world quiesces by itself when the horizon is reached.
    pub auto_quiesce: bool,
    /// Machine configuration of phone B when it differs from A.
    pub machine_b: Option<Config>,
    /// Reader threads and the single consumer are separate steps.
    pub reader_queue: bool,
    /// Exhaustive search: a queued record is consumed within this many ticks of being read (-1: no bound).
    pub max_consume_lag_ticks: i32,
    /// Every n-th sealed record starts a frame.
    pub frame_every: i64,
    /// Invariant 8: a run without faults, duplicates or drops loses no media record.
    pub require_lossless: bool,
    /// A fault (or watchdog close) discards what is still in flight on that carrier, both ways.
    pub fault_drops_in_flight: bool,
    /// A1: in-call link renewals.
    pub replacements: u32,
    /// A1: phones whose user turns the Wi-Fi switch off during the run.
    pub switch_offs: u32,
    /// A1: quality-rule fallbacks the owners may trigger during the run.
    pub quality_fallbacks: u32,
    /// A1 (Opus A1 2): each phone learns that its standby is stale within this
    /// many ticks of the old link's death on the ground; -1: not modelled.
    pub stale_max_ticks: i32,
}

impl Cfg {
    pub fn new(
        tick_ms: i64,
        machine: Config,
        horizon_ticks: i32,
        quiet_ticks: i32,
        detect_max_ticks: i32,
        media_per_endpoint: i32,
        media_bytes: i64,
        state_changes_per_endpoint: i32,
        fault_budget: i32,
        max_attempts: i32,
    ) -> Self {
        Self {
            tick_ms,
            machine,
            horizon_ticks,
            quiet_ticks,
            detect_max_ticks,
            media_per_endpoint,
            media_bytes,
            state_changes_per_endpoint,
            fault_budget,
            max_attempts,
            duplicates: false,
            drops: false,
            keys: KEYS,
            media_bytes_of: None,
            max_delay_ticks: -1,
            quiesce_anywhere: true,
            close_by: true,
            attempt_by_tick: i32::MAX,
            fault_by_tick: i32::MAX,
            up_immediately: false,
            auto_quiesce: false,
            machine_b: None,
            reader_queue: false,
            max_consume_lag_ticks: -1,
            frame_every: 1,
            require_lossless: false,
            fault_drops_in_flight: false,
            replacements: 0,
            switch_offs: 0,
            quality_fallbacks: 0,
            stale_max_ticks: -1,
        }
    }
}

struct Pkt {
    wire: Wire,
    sent_tick: i32,
}

/// An entry of the single consumer's queue: a record (`ev` 0) or a carrier
/// event in reader order (1 died, 2 up, 3 pending, 4 standby replaced,
/// 5 switch off, 6 quality). `gen0`: for a carrier-0 record or death, the
/// reading phone's `view_gen0` when it was read (-1: not stamped).
struct Inb {
    carrier: i32,
    wire: Option<Wire>,
    read_tick: i32,
    ev: i32,
    gen0: i32,
}

pub const KEYS: usize = 3;
pub const TICK: i32 = 0;
pub const FAULT: i32 = 1;
pub const CLOSE_BY: i32 = 2;
pub const DETECT: i32 = 3;
pub const DELIVER: i32 = 4;
pub const STATE_CHANGE: i32 = 5;
pub const NEW_ATTEMPT: i32 = 6;
pub const NEW_UP: i32 = 7;
pub const QUIESCE: i32 = 8;
pub const DUPLICATE: i32 = 9;
pub const DROP: i32 = 10;
pub const CONSUME: i32 = 11;
pub const REPLACE_STANDBY: i32 = 12;
pub const STANDBY_EVENT: i32 = 13;
pub const SWITCH_OFF: i32 = 14;
pub const QUALITY: i32 = 15;
pub const STANDBY_STALE: i32 = 16;
pub const RENEWAL_FAILED: i32 = 17;

pub fn code(kind: i32, a: i32, b: i32) -> i32 {
    kind * 64 + a * 8 + b
}
pub fn kind(code: i32) -> i32 {
    code / 64
}
pub fn a(code: i32) -> i32 {
    code / 8 % 8
}
pub fn b(code: i32) -> i32 {
    code % 8
}

fn carrier_name(carrier: i32) -> &'static str {
    match carrier {
        OLD => "O",
        NONE => "-",
        ANY => "*",
        1 => "N1",
        2 => "N2",
        3 => "N3",
        4 => "N4",
        _ => "N?",
    }
}

pub fn describe(code: i32) -> String {
    match kind(code) {
        TICK => "tick".into(),
        FAULT => format!("fault: {} dies", carrier_name(a(code))),
        CLOSE_BY => format!(
            "watchdog of phone {} closes {}",
            (b'A' + a(code) as u8) as char,
            carrier_name(b(code))
        ),
        DETECT => format!(
            "phone {} detects {} dead",
            (b'A' + a(code) as u8) as char,
            carrier_name(b(code))
        ),
        DELIVER => format!(
            "deliver next {} record from {}",
            carrier_name(b(code)),
            (b'A' + a(code) as u8) as char
        ),
        STATE_CHANGE => format!(
            "phone {} changes state key {}",
            (b'A' + a(code) as u8) as char,
            b(code)
        ),
        NEW_ATTEMPT => "upgrade attempt starts (new socket pending on both)".into(),
        NEW_UP => format!(
            "phone {} finishes BIND/READY on the new carrier",
            (b'A' + a(code) as u8) as char
        ),
        QUIESCE => "quiesce (no new faults or changes)".into(),
        DUPLICATE => format!(
            "duplicate next {} record from {}",
            carrier_name(b(code)),
            (b'A' + a(code) as u8) as char
        ),
        DROP => format!(
            "drop next {} record from {}",
            carrier_name(b(code)),
            (b'A' + a(code) as u8) as char
        ),
        CONSUME => format!(
            "consumer at {} takes the next queued record",
            (b'A' + a(code) as u8) as char
        ),
        REPLACE_STANDBY => "in-call renewal replaces the old link".into(),
        STANDBY_EVENT => format!(
            "phone {} hears of the new standby",
            (b'A' + a(code) as u8) as char
        ),
        SWITCH_OFF => format!(
            "user of phone {} turns Wi-Fi off",
            (b'A' + a(code) as u8) as char
        ),
        QUALITY => format!(
            "quality rule fires at phone {}",
            (b'A' + a(code) as u8) as char
        ),
        STANDBY_STALE => format!(
            "phone {} finds its standby stale",
            (b'A' + a(code) as u8) as char
        ),
        RENEWAL_FAILED => format!(
            "phone {}'s renewal failed",
            (b'A' + a(code) as u8) as char
        ),
        _ => "?".into(),
    }
}

fn drop_keys() -> [&'static str; 6] {
    [
        "drop-closed_carrier",
        "drop-stale",
        "drop-fenced",
        "drop-stale_marker",
        "drop-unknown_carrier",
        "drop-no_carrier",
    ]
}
fn close_keys() -> [&'static str; 6] {
    [
        "close-hold_expired",
        "close-peer_resumed",
        "close-handshake_timeout",
        "close-peer_old_dead",
        "close-switch_off",
        "close-quality",
    ]
}
fn wire_type(wire: &Wire) -> usize {
    match wire {
        Wire::Content(_) => 0,
        Wire::Switch { .. } => 1,
        Wire::Resume { .. } => 2,
    }
}

pub struct CutoverSim {
    pub cfg: Cfg,
    pub tick: i32,
    pub phase2: bool,
    quiesce_tick: i32,
    pub m: [CarrierCutover; 2],
    slots: usize,
    next_seq: [i64; 2],
    media_sealed: [i32; 2],
    state_changes: [i32; 2],
    state_cur: [Vec<i32>; 2],
    state_view: [Vec<i32>; 2],
    last_delivered: [i64; 2],
    pub ended: [bool; 2],
    reader_stopped: [Vec<bool>; 2],
    inbound: [VecDeque<Inb>; 2],
    chan: [Vec<VecDeque<Pkt>>; 2],
    fault: Vec<i32>,
    created: Vec<bool>,
    pending: [Vec<bool>; 2],
    up: [Vec<bool>; 2],
    local_dead: [Vec<bool>; 2],
    faults_used: i32,
    attempts: i32,
    pub replacements_used: u32,
    pub switch_offs_used: u32,
    pub quality_used: u32,
    ground_gen0: i32,
    view_gen0: [i32; 2],
    switched_off_ground: [bool; 2],
    machine_switched_off: [bool; 2],
    stale_seen: [bool; 2],
    renewal_busy: [bool; 2],
    renewal_failed: [bool; 2],
    held_death: [bool; 2],
    pub held_deaths: u64,
    pub dropped_held_deaths: u64,
    old_gen_discards: u64,
    pub violation: Option<String>,
    sealed_tag: [Vec<i32>; 2],
    sealed_at_tick: [Vec<i32>; 2],
    delivered_media: [Vec<i64>; 2],
    pub dropped_stale: u64,
    pub dropped_other: u64,
    pub void_writes: u64,
    pub keyframes: u64,
    pub log: Option<Vec<String>>,
    /// When set, every applied action code is appended (to shrink a counterexample later).
    pub record: Option<Vec<i32>>,
    pub switch_writes: u64,
    pub resume_writes: u64,
    cov: Option<Rc<RefCell<HashMap<String, u64>>>>,
    media_bytes_fn: Option<Arc<dyn Fn(usize, i64) -> i64 + Send + Sync>>,
}

impl CutoverSim {
    fn empty(cfg: Cfg) -> Self {
        let slots = 1 + cfg.max_attempts.max(0) as usize;
        let keys = cfg.keys;
        let machine_a = cfg.machine;
        let machine_b = cfg.machine_b.unwrap_or(cfg.machine);
        let media_bytes_fn = cfg.media_bytes_of.clone();
        Self {
            cfg,
            tick: 0,
            phase2: false,
            quiesce_tick: 0,
            m: [CarrierCutover::new(machine_a), CarrierCutover::new(machine_b)],
            slots,
            next_seq: [0; 2],
            media_sealed: [0; 2],
            state_changes: [0; 2],
            state_cur: [vec![0; keys], vec![0; keys]],
            state_view: [vec![0; keys], vec![0; keys]],
            last_delivered: [-1; 2],
            ended: [false; 2],
            reader_stopped: [vec![false; slots], vec![false; slots]],
            inbound: [VecDeque::new(), VecDeque::new()],
            chan: [
                (0..slots).map(|_| VecDeque::new()).collect(),
                (0..slots).map(|_| VecDeque::new()).collect(),
            ],
            fault: vec![-1; slots],
            created: {
                let mut created = vec![false; slots];
                created[0] = true;
                created
            },
            pending: [vec![false; slots], vec![false; slots]],
            up: [vec![false; slots], vec![false; slots]],
            local_dead: [vec![false; slots], vec![false; slots]],
            faults_used: 0,
            attempts: 0,
            replacements_used: 0,
            switch_offs_used: 0,
            quality_used: 0,
            ground_gen0: 0,
            view_gen0: [0; 2],
            switched_off_ground: [false; 2],
            machine_switched_off: [false; 2],
            stale_seen: [false; 2],
            renewal_busy: [false; 2],
            renewal_failed: [false; 2],
            held_death: [false; 2],
            held_deaths: 0,
            dropped_held_deaths: 0,
            old_gen_discards: 0,
            violation: None,
            sealed_tag: [Vec::new(), Vec::new()],
            sealed_at_tick: [Vec::new(), Vec::new()],
            delivered_media: [Vec::new(), Vec::new()],
            dropped_stale: 0,
            dropped_other: 0,
            void_writes: 0,
            keyframes: 0,
            log: None,
            record: None,
            switch_writes: 0,
            resume_writes: 0,
            cov: None,
            media_bytes_fn,
        }
    }

    pub fn new(cfg: Cfg) -> Self {
        let mut sim = Self::empty(cfg);
        for x in 0..2 {
            let actions = sim.m[x].on_call_active(0, 0, true);
            sim.run(x, actions);
        }
        sim
    }

    pub fn copy(&self) -> Self {
        let mut sim = Self::empty(self.cfg.clone());
        sim.tick = self.tick;
        sim.phase2 = self.phase2;
        sim.quiesce_tick = self.quiesce_tick;
        sim.m = [self.m[0].clone_state(), self.m[1].clone_state()];
        sim.next_seq = self.next_seq;
        sim.media_sealed = self.media_sealed;
        sim.state_changes = self.state_changes;
        sim.state_cur = [self.state_cur[0].clone(), self.state_cur[1].clone()];
        sim.state_view = [self.state_view[0].clone(), self.state_view[1].clone()];
        sim.last_delivered = self.last_delivered;
        sim.ended = self.ended;
        for x in 0..2 {
            sim.reader_stopped[x] = self.reader_stopped[x].clone();
            sim.inbound[x] = self
                .inbound[x]
                .iter()
                .map(|item| Inb {
                    carrier: item.carrier,
                    wire: item.wire.clone(),
                    read_tick: item.read_tick,
                    ev: item.ev,
                    gen0: item.gen0,
                })
                .collect();
            for c in 0..self.slots {
                sim.chan[x][c] = self.chan[x][c]
                    .iter()
                    .map(|pkt| Pkt {
                        wire: pkt.wire.clone(),
                        sent_tick: pkt.sent_tick,
                    })
                    .collect();
            }
            sim.pending[x] = self.pending[x].clone();
            sim.up[x] = self.up[x].clone();
            sim.local_dead[x] = self.local_dead[x].clone();
        }
        sim.fault = self.fault.clone();
        sim.created = self.created.clone();
        sim.faults_used = self.faults_used;
        sim.attempts = self.attempts;
        sim.replacements_used = self.replacements_used;
        sim.switch_offs_used = self.switch_offs_used;
        sim.quality_used = self.quality_used;
        sim.ground_gen0 = self.ground_gen0;
        sim.view_gen0 = self.view_gen0;
        sim.switched_off_ground = self.switched_off_ground;
        sim.machine_switched_off = self.machine_switched_off;
        sim.stale_seen = self.stale_seen;
        sim.renewal_busy = self.renewal_busy;
        sim.renewal_failed = self.renewal_failed;
        sim.held_death = self.held_death;
        sim.held_deaths = self.held_deaths;
        sim.dropped_held_deaths = self.dropped_held_deaths;
        sim.old_gen_discards = self.old_gen_discards;
        if let Some(cov) = &self.cov {
            sim.enable_coverage(cov.clone());
        }
        for x in 0..2 {
            sim.sealed_tag[x] = self.sealed_tag[x].clone();
            sim.sealed_at_tick[x] = self.sealed_at_tick[x].clone();
            sim.delivered_media[x] = self.delivered_media[x].clone();
        }
        sim
    }

    pub fn enable_coverage(&mut self, map: Rc<RefCell<HashMap<String, u64>>>) {
        self.cov = Some(map.clone());
        for x in 0..2 {
            let map = map.clone();
            self.m[x].on_stat = Some(Box::new(move |key: &str| {
                *map.borrow_mut().entry(key.to_string()).or_insert(0) += 1;
            }));
        }
    }

    fn hit(&self, key: &str) {
        self.hit_n(key, 1);
    }
    fn hit_n(&self, key: &str, n: u64) {
        if let Some(cov) = &self.cov {
            *cov.borrow_mut().entry(key.to_string()).or_insert(0) += n;
        }
    }

    fn latest_new(&self) -> i32 {
        self.attempts
    }
    fn queue_age_ms(&self, x: usize) -> i64 {
        self.chan[x][0]
            .front()
            .map_or(0, |pkt| i64::from(self.tick - pkt.sent_tick) * self.cfg.tick_ms)
    }
    fn at_boundary(&self, x: usize) -> bool {
        self.cfg.frame_every <= 1 || self.next_seq[x] % self.cfg.frame_every == 0
    }
    fn discard_in_flight(&mut self, carrier: usize) {
        if self.cfg.fault_drops_in_flight {
            for x in 0..2 {
                self.chan[x][carrier].clear();
            }
        }
    }
    pub fn attempts_started(&self) -> i32 {
        self.attempts
    }
    pub fn can_deliver(&self, from: usize, carrier: usize) -> bool {
        self.deliverable(from, carrier)
    }
    pub fn head_age_ticks(&self, from: usize, carrier: usize) -> i32 {
        self.chan[from][carrier]
            .front()
            .map_or(0, |pkt| self.tick - pkt.sent_tick)
    }
    fn ground_live(&self, carrier: usize) -> bool {
        self.created[carrier] && self.fault[carrier] < 0
    }
    fn any_ground_live(&self, except: Option<usize>) -> bool {
        (0..self.slots).any(|c| Some(c) != except && self.ground_live(c))
    }
    fn forced_detection(&self) -> bool {
        for carrier in 0..self.slots {
            if self.fault[carrier] < 0 {
                continue;
            }
            for x in 0..2 {
                if !self.ended[x]
                    && !self.reader_stopped[x][carrier]
                    && self.tick + 1 - self.fault[carrier] > self.cfg.detect_max_ticks
                {
                    return true;
                }
            }
        }
        false
    }
    fn forced_stale(&self) -> bool {
        self.cfg.stale_max_ticks >= 0
            && self.fault[0] >= 0
            && (0..2).any(|x| {
                !self.ended[x]
                    && !self.stale_seen[x]
                    && self.view_gen0[x] == self.ground_gen0
                    && self.tick + 1 - self.fault[0] > self.cfg.stale_max_ticks
            })
    }
    fn forced_delivery(&self) -> bool {
        if self.cfg.max_delay_ticks < 0 {
            return false;
        }
        for carrier in 0..self.slots {
            for from in 0..2 {
                let Some(head) = self.chan[from][carrier].front() else {
                    continue;
                };
                if self.deliverable(from, carrier)
                    && self.tick + 1 - head.sent_tick > self.cfg.max_delay_ticks
                {
                    return true;
                }
            }
        }
        false
    }
    fn forced_consume(&self) -> bool {
        if !self.cfg.reader_queue || self.cfg.max_consume_lag_ticks < 0 {
            return false;
        }
        for x in 0..2 {
            let Some(head) = self.inbound[x].front() else {
                continue;
            };
            if !self.ended[x] && self.tick + 1 - head.read_tick > self.cfg.max_consume_lag_ticks {
                return true;
            }
        }
        false
    }
    pub fn can_consume(&self, x: usize) -> bool {
        self.cfg.reader_queue && !self.ended[x] && !self.inbound[x].is_empty()
    }
    fn deliverable(&self, from: usize, carrier: usize) -> bool {
        let to = 1 - from;
        !self.chan[from][carrier].is_empty()
            && !self.reader_stopped[to][carrier]
            && !self.ended[to]
            && (carrier == 0 || self.up[to][carrier])
            && (carrier != 0 || self.view_gen0[to] == self.ground_gen0)
    }
    /// Some live phone has not yet heard of the latest replacement of the old link.
    fn replacement_in_flight(&self) -> bool {
        (0..2).any(|x| !self.ended[x] && self.view_gen0[x] != self.ground_gen0)
    }
    fn new_up_enabled(&self, x: usize) -> bool {
        let carrier = self.latest_new();
        carrier > 0
            && !self.ended[x]
            && self.pending[x][carrier as usize]
            && !self.up[x][carrier as usize]
            && self.fault[carrier as usize] < 0
            && !self.reader_stopped[x][carrier as usize]
    }

    /// Appends every enabled action code to `out` (cleared first).
    pub fn enabled(&self, out: &mut Vec<i32>) {
        out.clear();
        let mut drain = false;
        for carrier in 0..self.slots {
            for from in 0..2 {
                if self.deliverable(from, carrier) {
                    out.push(code(DELIVER, from as i32, carrier as i32));
                    drain = true;
                    if self.cfg.duplicates {
                        out.push(code(DUPLICATE, from as i32, carrier as i32));
                    }
                    if self.cfg.drops {
                        out.push(code(DROP, from as i32, carrier as i32));
                    }
                }
            }
        }
        for carrier in 0..self.slots {
            for x in 0..2 {
                if self.fault[carrier] >= 0
                    && !self.reader_stopped[x][carrier]
                    && !self.ended[x]
                    && (carrier == 0 || self.pending[x][carrier] || self.up[x][carrier])
                    && !self.replacement_in_flight()
                {
                    out.push(code(DETECT, x as i32, carrier as i32));
                }
            }
        }
        for x in 0..2 {
            if self.new_up_enabled(x) {
                out.push(code(NEW_UP, x as i32, 0));
                drain = true;
            }
        }
        for x in 0..2 {
            if self.can_consume(x) {
                out.push(code(CONSUME, x as i32, 0));
                drain = true;
            }
        }
        for x in 0..2 {
            if !self.ended[x] && self.view_gen0[x] != self.ground_gen0 {
                out.push(code(STANDBY_EVENT, x as i32, 0));
                drain = true;
            }
        }
        for x in 0..2 {
            if !self.ended[x] && self.renewal_busy[x] && self.view_gen0[x] == self.ground_gen0 {
                out.push(code(RENEWAL_FAILED, x as i32, 0));
            }
        }
        if self.cfg.stale_max_ticks >= 0 {
            for x in 0..2 {
                if !self.ended[x] && !self.stale_seen[x] && self.fault[0] >= 0 {
                    out.push(code(STANDBY_STALE, x as i32, 0));
                }
            }
        }
        if !self.phase2 {
            if self.faults_used < self.cfg.fault_budget
                && self.tick <= self.cfg.fault_by_tick
                && !self.replacement_in_flight()
            {
                for carrier in 0..self.slots {
                    if self.created[carrier] && self.fault[carrier] < 0 {
                        out.push(code(FAULT, carrier as i32, 0));
                        if self.cfg.close_by {
                            for x in 0..2 {
                                if !self.ended[x]
                                    && !self.reader_stopped[x][carrier]
                                    && (carrier == 0 || self.pending[x][carrier] || self.up[x][carrier])
                                {
                                    out.push(code(CLOSE_BY, x as i32, carrier as i32));
                                }
                            }
                        }
                    }
                }
            }
            for x in 0..2 {
                if !self.ended[x] && self.state_changes[x] < self.cfg.state_changes_per_endpoint {
                    for k in 0..self.cfg.keys {
                        if next_value(k, self.state_cur[x][k]) >= 0 {
                            out.push(code(STATE_CHANGE, x as i32, k as i32));
                        }
                    }
                }
            }
            if self.attempts < self.cfg.max_attempts
                && self.fault[0] < 0
                && (self.attempts > 0 || self.tick <= self.cfg.attempt_by_tick)
                && (1..=self.attempts).all(|it| {
                    self.local_dead[0][it as usize] && self.local_dead[1][it as usize]
                })
            {
                out.push(code(NEW_ATTEMPT, 0, 0));
            }
            if self.replacements_used < self.cfg.replacements
                && self.fault[0] >= 0
                && !self.ended[0]
                && !self.ended[1]
                && !self.replacement_in_flight()
                && (0..2).all(|x| self.ended[x] || !self.renewal_failed[x])
            {
                out.push(code(REPLACE_STANDBY, 0, 0));
            }
            for x in 0..2 {
                if !self.ended[x]
                    && !self.switched_off_ground[x]
                    && self.switch_offs_used < self.cfg.switch_offs
                {
                    out.push(code(SWITCH_OFF, x as i32, 0));
                }
            }
            for x in 0..2 {
                if !self.ended[x]
                    && self.quality_used < self.cfg.quality_fallbacks
                    && self.m[x].new_carrier() != NONE
                {
                    out.push(code(QUALITY, x as i32, 0));
                }
            }
            if !self.cfg.auto_quiesce && (self.cfg.quiesce_anywhere || self.tick >= self.cfg.horizon_ticks) {
                out.push(code(QUIESCE, 0, 0));
            }
        }
        if self.phase2
            && out.is_empty()
            && (0..2).all(|x| self.ended[x] || !self.m[x].can_still_change_by_time())
        {
            return;
        }
        let limit = if self.phase2 {
            self.quiesce_tick + self.cfg.quiet_ticks
        } else {
            self.cfg.horizon_ticks
        };
        let tick_ok = self.tick < limit
            && !self.forced_detection()
            && !self.forced_delivery()
            && !self.forced_consume()
            && !self.forced_stale()
            && (!self.phase2 || !drain);
        if tick_ok {
            out.push(code(TICK, 0, 0));
        }
    }

    pub fn is_enabled(&self, action: i32) -> bool {
        let mut codes = Vec::new();
        self.enabled(&mut codes);
        codes.contains(&action)
    }

    fn now(&self) -> i64 {
        i64::from(self.tick) * self.cfg.tick_ms
    }

    pub fn apply(&mut self, action: i32) {
        if self.violation.is_some() {
            return;
        }
        if let Some(record) = &mut self.record {
            record.push(action);
        }
        if let Some(log) = &mut self.log {
            log.push(format!("t={} {}", self.tick, describe(action)));
        }
        let first = a(action) as usize;
        let second = b(action) as usize;
        match kind(action) {
            TICK => {
                self.tick += 1;
                if self.cfg.auto_quiesce && !self.phase2 && self.tick >= self.cfg.horizon_ticks {
                    self.phase2 = true;
                    self.quiesce_tick = self.tick;
                }
                for x in 0..2 {
                    if !self.ended[x] {
                        let now = self.now();
                        let age = self.queue_age_ms(x);
                        let boundary = self.at_boundary(x);
                        let actions = self.m[x].on_tick(now, age, boundary);
                        self.run(x, actions);
                    }
                }
                if !self.phase2 {
                    for x in 0..2 {
                        if !self.ended[x] && self.media_sealed[x] < self.cfg.media_per_endpoint {
                            self.media_sealed[x] += 1;
                            let seq = self.next_seq[x];
                            let bytes = self
                                .media_bytes_fn
                                .as_ref()
                                .map_or(self.cfg.media_bytes, |f| f(x, seq));
                            self.seal(x, 0, bytes);
                        }
                    }
                }
            }
            FAULT => {
                self.fault[first] = self.tick;
                self.faults_used += 1;
                self.discard_in_flight(first);
                if first == 0 {
                    self.renewal_failed = [false; 2];
                }
            }
            CLOSE_BY => {
                self.fault[second] = self.tick;
                self.faults_used += 1;
                self.discard_in_flight(second);
                self.reader_stopped[first][second] = true;
                if second == 0 && self.replacements_used < self.cfg.replacements {
                    self.renewal_busy[first] = true;
                }
                if self.cfg.reader_queue {
                    let gen0 = if second == 0 { self.view_gen0[first] } else { -1 };
                    self.inbound[first].push_back(Inb {
                        carrier: second as i32,
                        wire: None,
                        read_tick: self.tick,
                        ev: 1,
                        gen0,
                    });
                } else {
                    self.carrier_death(first, second);
                }
            }
            DETECT => {
                self.reader_stopped[first][second] = true;
                if second == 0 && self.replacements_used < self.cfg.replacements {
                    self.renewal_busy[first] = true;
                }
                if self.cfg.reader_queue {
                    let gen0 = if second == 0 { self.view_gen0[first] } else { -1 };
                    self.inbound[first].push_back(Inb {
                        carrier: second as i32,
                        wire: None,
                        read_tick: self.tick,
                        ev: 1,
                        gen0,
                    });
                } else {
                    self.carrier_death(first, second);
                }
            }
            DELIVER => {
                let wire = self.chan[first][second].pop_front().unwrap().wire;
                if self.cfg.reader_queue {
                    let to = 1 - first;
                    let gen0 = if second == 0 { self.view_gen0[to] } else { -1 };
                    self.inbound[to].push_back(Inb {
                        carrier: second as i32,
                        wire: Some(wire),
                        read_tick: self.tick,
                        ev: 0,
                        gen0,
                    });
                } else {
                    self.arrive(1 - first, second as i32, wire);
                }
            }
            DUPLICATE => {
                let wire = self.chan[first][second].front().unwrap().wire.clone();
                if self.cfg.reader_queue {
                    let to = 1 - first;
                    let gen0 = if second == 0 { self.view_gen0[to] } else { -1 };
                    self.inbound[to].push_back(Inb {
                        carrier: second as i32,
                        wire: Some(wire),
                        read_tick: self.tick,
                        ev: 0,
                        gen0,
                    });
                } else {
                    self.arrive(1 - first, second as i32, wire);
                }
            }
            CONSUME => {
                let item = self.inbound[first].pop_front().unwrap();
                if item.gen0 >= 0 && item.gen0 != self.view_gen0[first] {
                    self.old_gen_discards += 1;
                } else {
                    match item.ev {
                        0 => self.arrive(first, item.carrier, item.wire.unwrap()),
                        1 => self.carrier_death(first, item.carrier as usize),
                        2 => {
                            let now = self.now();
                            let age = self.queue_age_ms(first);
                            let boundary = self.at_boundary(first);
                            let actions =
                                self.m[first].on_carrier_up(item.carrier, now, age, boundary);
                            self.run(first, actions);
                        }
                        3 => self.pending_on(first, item.carrier as usize),
                        4 => self.standby_replaced_at(first),
                        5 => {
                            self.machine_switched_off[first] = true;
                            let now = self.now();
                            let actions = self.m[first].on_switch_off(now);
                            self.run(first, actions);
                        }
                        6 => {
                            let now = self.now();
                            let actions = self.m[first].on_quality_fallback(now);
                            self.run(first, actions);
                        }
                        other => panic!("unknown consumer entry {other}"),
                    }
                }
            }
            REPLACE_STANDBY => {
                self.replacements_used += 1;
                self.renewal_failed = [false; 2];
                self.ground_gen0 += 1;
                self.fault[0] = -1;
                self.chan[0][0].clear();
                self.chan[1][0].clear();
            }
            STANDBY_EVENT => {
                self.view_gen0[first] = self.ground_gen0;
                self.reader_stopped[first][0] = false;
                self.local_dead[first][0] = false;
                self.renewal_busy[first] = false;
                if self.held_death[first] {
                    self.held_death[first] = false;
                    self.dropped_held_deaths += 1;
                    self.hit("droppedHeldDeaths");
                }
                if self.cfg.reader_queue {
                    self.inbound[first].push_back(Inb {
                        carrier: 0,
                        wire: None,
                        read_tick: self.tick,
                        ev: 4,
                        gen0: -1,
                    });
                } else {
                    self.standby_replaced_at(first);
                }
            }
            SWITCH_OFF => {
                self.switch_offs_used += 1;
                self.switched_off_ground[first] = true;
                if self.cfg.reader_queue {
                    self.inbound[first].push_back(Inb {
                        carrier: 0,
                        wire: None,
                        read_tick: self.tick,
                        ev: 5,
                        gen0: -1,
                    });
                } else {
                    self.machine_switched_off[first] = true;
                    let now = self.now();
                    let actions = self.m[first].on_switch_off(now);
                    self.run(first, actions);
                }
            }
            QUALITY => {
                self.quality_used += 1;
                if self.cfg.reader_queue {
                    self.inbound[first].push_back(Inb {
                        carrier: 0,
                        wire: None,
                        read_tick: self.tick,
                        ev: 6,
                        gen0: -1,
                    });
                } else {
                    let now = self.now();
                    let actions = self.m[first].on_quality_fallback(now);
                    self.run(first, actions);
                }
            }
            STANDBY_STALE => {
                self.stale_seen[first] = true;
                let now = self.now();
                let actions = self.m[first].on_standby_freshness(false, now);
                self.run(first, actions);
            }
            RENEWAL_FAILED => {
                self.renewal_busy[first] = false;
                self.renewal_failed[first] = true;
                if self.held_death[first] {
                    self.held_death[first] = false;
                    let now = self.now();
                    let actions = self.m[first].on_carrier_died(0, now);
                    self.run(first, actions);
                }
            }
            DROP => {
                self.chan[first][second].pop_front();
            }
            STATE_CHANGE => {
                self.state_changes[first] += 1;
                self.state_cur[first][second] = next_value(second, self.state_cur[first][second]);
                let tag = 1 + second as i32 * 4 + self.state_cur[first][second];
                self.seal(first, tag, 1);
            }
            NEW_ATTEMPT => {
                self.attempts += 1;
                let attempt = self.attempts as usize;
                self.created[attempt] = true;
                for x in 0..2 {
                    self.pending[x][attempt] = true;
                    if self.cfg.reader_queue {
                        self.inbound[x].push_back(Inb {
                            carrier: self.attempts,
                            wire: None,
                            read_tick: self.tick,
                            ev: 3,
                            gen0: -1,
                        });
                    } else {
                        self.pending_on(x, attempt);
                    }
                }
                if self.cfg.up_immediately {
                    for x in 0..2 {
                        if !self.machine_switched_off[x] {
                            self.up[x][attempt] = true;
                            self.pending[x][attempt] = false;
                            if self.cfg.reader_queue {
                                self.inbound[x].push_back(Inb {
                                    carrier: self.attempts,
                                    wire: None,
                                    read_tick: self.tick,
                                    ev: 2,
                                    gen0: -1,
                                });
                            } else {
                                let now = self.now();
                                let age = self.queue_age_ms(x);
                                let boundary = self.at_boundary(x);
                                let actions = self.m[x].on_carrier_up(
                                    self.attempts,
                                    now,
                                    age,
                                    boundary,
                                );
                                self.run(x, actions);
                            }
                        }
                    }
                }
            }
            NEW_UP => {
                let carrier = self.latest_new() as usize;
                self.up[first][carrier] = true;
                self.pending[first][carrier] = false;
                if self.cfg.reader_queue {
                    self.inbound[first].push_back(Inb {
                        carrier: carrier as i32,
                        wire: None,
                        read_tick: self.tick,
                        ev: 2,
                        gen0: -1,
                    });
                } else {
                    let now = self.now();
                    let age = self.queue_age_ms(first);
                    let boundary = self.at_boundary(first);
                    let actions =
                        self.m[first].on_carrier_up(carrier as i32, now, age, boundary);
                    self.run(first, actions);
                }
            }
            QUIESCE => {
                self.phase2 = true;
                self.quiesce_tick = self.tick;
            }
            _ => {}
        }
    }

    fn seal(&mut self, x: usize, tag: i32, bytes: i64) {
        if self.ended[x] {
            return;
        }
        let seq = self.next_seq[x];
        self.next_seq[x] += 1;
        self.sealed_tag[x].push(tag);
        self.sealed_at_tick[x].push(self.tick);
        let now = self.now();
        let actions = self.m[x].on_sealed(
            Rec {
                seq,
                bytes,
                tag: tag as u32,
            },
            now,
        );
        self.run(x, actions);
    }

    /// Delivers one record or marker to phone `x` and checks invariant 11.
    fn arrive(&mut self, x: usize, carrier: i32, wire: Wire) {
        let before = self.m[x].accept();
        let seen = self.m[x].generation_seen();
        let now = self.now();
        let actions = self.m[x].on_arrive(carrier, wire.clone(), now);
        self.run(x, actions);
        let stale_marker = carrier == 0
            && match wire {
                Wire::Switch { generation, .. } | Wire::Resume { generation, .. } => {
                    generation <= seen
                }
                Wire::Content(_) => false,
            };
        if stale_marker && self.violation.is_none() && self.m[x].accept() != before {
            self.fail(format!(
                "11 a repeated or stale marker on the standby changed what {} accepts ({} to {})",
                (b'A' + x as u8) as char,
                carrier_name(before),
                carrier_name(self.m[x].accept())
            ));
        }
    }

    /// An upgrade attempt starts at phone `x`; invariant 9.
    fn pending_on(&mut self, x: usize, carrier: usize) {
        let now = self.now();
        let actions = self.m[x].on_carrier_pending(carrier as i32, now);
        if self.machine_switched_off[x] && self.violation.is_none() {
            if !actions.is_empty() {
                self.fail(format!(
                    "9 onCarrierPending after the switch-off produced actions at {}: {actions:?}",
                    (b'A' + x as u8) as char
                ));
            }
            if self.m[x].pending_carrier() != NONE {
                self.fail(format!(
                    "9 the machine of {} tracks an attempt after the switch-off",
                    (b'A' + x as u8) as char
                ));
            }
            self.local_dead[x][carrier] = true;
            self.reader_stopped[x][carrier] = true;
            self.pending[x][carrier] = false;
            if self.fault[carrier] < 0 {
                self.fault[carrier] = self.tick;
                self.discard_in_flight(carrier);
            }
        }
        self.run(x, actions);
    }

    /// Hands a carrier death to the machine unless A3 holds carrier 0 while an in-call renewal runs.
    fn carrier_death(&mut self, x: usize, carrier: usize) {
        if carrier == 0 && self.renewal_busy[x] {
            self.held_death[x] = true;
            self.local_dead[x][0] = true;
            self.held_deaths += 1;
            self.hit("heldDeaths");
        } else {
            self.local_dead[x][carrier] = true;
            let now = self.now();
            let actions = self.m[x].on_carrier_died(carrier as i32, now);
            self.run(x, actions);
        }
    }

    fn standby_replaced_at(&mut self, x: usize) {
        self.local_dead[x][0] = false;
        self.stale_seen[x] = false;
        let now = self.now();
        let actions = self.m[x].on_standby_replaced(now);
        if actions
            .iter()
            .any(|action| matches!(action, Action::EndSession { .. }))
        {
            self.fail(format!(
                "10 the standby replacement ended the session at {}",
                (b'A' + x as u8) as char
            ));
        }
        self.run(x, actions);
    }

    fn fail(&mut self, message: String) {
        if self.violation.is_none() {
            if let Some(log) = &mut self.log {
                log.push(format!("VIOLATION: {message}"));
            }
            self.violation = Some(message);
        }
    }

    fn run(&mut self, x: usize, actions: Vec<Action>) {
        for action in actions {
            if let Some(log) = &mut self.log {
                log.push(format!("    {}: {action:?}", (b'A' + x as u8) as char));
            }
            match action {
                Action::Write {
                    carrier,
                    wire,
                    priority,
                } => {
                    if carrier < 0 {
                        continue;
                    }
                    let carrier_idx = carrier as usize;
                    if self.local_dead[x][carrier_idx]
                        && !(carrier == 0 && self.held_death[x])
                    {
                        self.fail(format!(
                            "7 write on a carrier this phone already closed: {} {wire:?}",
                            (b'A' + x as u8) as char
                        ));
                    } else if carrier == 0 && self.view_gen0[x] != self.ground_gen0 {
                        self.void_writes += 1;
                    } else if self.fault[carrier_idx] >= 0 {
                        self.void_writes += 1;
                    } else if priority {
                        self.chan[x][carrier_idx].push_front(Pkt {
                            wire: wire.clone(),
                            sent_tick: self.tick,
                        });
                    } else {
                        self.chan[x][carrier_idx].push_back(Pkt {
                            wire: wire.clone(),
                            sent_tick: self.tick,
                        });
                    }
                    if matches!(wire, Wire::Switch { .. }) {
                        self.switch_writes += 1;
                    }
                    if matches!(wire, Wire::Resume { .. }) {
                        self.resume_writes += 1;
                    }
                    let side = if carrier == 0 { "old" } else { "new" };
                    let dead = if self.fault[carrier_idx] >= 0 { "-into-dead" } else { "" };
                    let type_name = ["content", "switch", "resume"][wire_type(&wire)];
                    self.hit(&format!("write-{type_name}-on-{side}{dead}"));
                }
                Action::Deliver { rec, carrier } => {
                    let peer = 1 - x;
                    let seq = rec.seq;
                    let carrier_idx = carrier as usize;
                    if self.local_dead[x][carrier_idx] {
                        self.fail(format!(
                            "6 closed carrier delivered rec#{seq} from {} at {}",
                            carrier_name(carrier),
                            (b'A' + x as u8) as char
                        ));
                    }
                    if seq <= self.last_delivered[x] {
                        self.fail(format!(
                            "2 opener at {} fed rec#{seq} not above {}",
                            (b'A' + x as u8) as char,
                            self.last_delivered[x]
                        ));
                    }
                    if seq >= self.next_seq[peer]
                        || self.sealed_tag[peer][seq as usize] != rec.tag as i32
                    {
                        self.fail(format!(
                            "3 rec#{seq} delivered at {} was never sealed like that",
                            (b'A' + x as u8) as char
                        ));
                    }
                    self.last_delivered[x] = seq;
                    self.hit(if carrier == 0 {
                        "deliver-from-old"
                    } else {
                        "deliver-from-new"
                    });
                    let tag = rec.tag as i32;
                    if tag == 0 {
                        self.delivered_media[peer].push(seq);
                    } else {
                        self.state_view[x][((tag - 1) / 4) as usize] = (tag - 1) % 4;
                    }
                }
                Action::Drop { reason, .. } => {
                    self.hit(drop_keys()[reason as usize]);
                    if matches!(reason, DropReason::Stale | DropReason::Fenced) {
                        self.dropped_stale += 1;
                    } else {
                        self.dropped_other += 1;
                    }
                }
                Action::CloseCarrier { carrier, reason } => {
                    let carrier_idx = carrier as usize;
                    self.hit(close_keys()[reason as usize]);
                    if reason == CloseReason::HoldExpired
                        && !self.any_ground_live(Some(carrier_idx))
                    {
                        self.fail(format!(
                            "1 phone {} closed carrier {} by policy ({reason:?}) while every other carrier was already dead",
                            (b'A' + x as u8) as char,
                            carrier_name(carrier)
                        ));
                    }
                    if reason == CloseReason::PeerOldDead && self.fault[carrier_idx] < 0 {
                        self.fail(format!(
                            "1 phone {} closed a healthy {} on the peer's hint",
                            (b'A' + x as u8) as char,
                            carrier_name(carrier)
                        ));
                    }
                    if matches!(reason, CloseReason::SwitchOff | CloseReason::Quality)
                        && !self.m[x].old_is_alive()
                    {
                        self.fail(format!(
                            "1 phone {} closed carrier {} by policy ({reason:?}) while it was the last carrier in its view",
                            (b'A' + x as u8) as char,
                            carrier_name(carrier)
                        ));
                    }
                    if matches!(reason, CloseReason::SwitchOff | CloseReason::Quality)
                        && self.cfg.stale_max_ticks >= 0
                        && !self.any_ground_live(Some(carrier_idx))
                        && self.fault[0] >= 0
                        && self.tick - self.fault[0] > self.cfg.stale_max_ticks
                    {
                        self.fail(format!(
                            "12 phone {} closed carrier {} by policy ({reason:?}) although the standby was dead on the ground for longer than its freshness window",
                            (b'A' + x as u8) as char,
                            carrier_name(carrier)
                        ));
                    }
                    self.local_dead[x][carrier_idx] = true;
                    self.reader_stopped[x][carrier_idx] = true;
                    if self.fault[carrier_idx] < 0 {
                        self.fault[carrier_idx] = self.tick;
                        self.discard_in_flight(carrier_idx);
                    }
                }
                Action::ReassertState => {
                    self.hit("reassert");
                    for k in 0..self.cfg.keys {
                        let tag = 1 + k as i32 * 4 + self.state_cur[x][k];
                        self.seal(x, tag, 1);
                    }
                }
                Action::ForceLocalKeyframe | Action::RequestPeerKeyframe => self.keyframes += 1,
                Action::ResetSendFragments => self.hit("reset-send-fragments"),
                Action::SuppressFragments => self.hit("suppress-fragments"),
                Action::EndSession { reason } => {
                    self.hit("end-session");
                    self.ended[x] = true;
                    if self.any_ground_live(None) {
                        let live = (0..self.slots)
                            .filter(|carrier| self.ground_live(*carrier))
                            .map(|carrier| carrier_name(carrier as i32))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.fail(format!(
                            "1 phone {} ended the session ({reason}) while carrier(s) {live} lived",
                            (b'A' + x as u8) as char
                        ));
                    }
                }
            }
            if self.violation.is_some() {
                return;
            }
        }
    }

    /// Quiescent final state: invariants 4 and 5 (and 1 again).
    pub fn check_final(&mut self) {
        if self.violation.is_some() {
            return;
        }
        let live: Vec<usize> = (0..self.slots).filter(|c| self.ground_live(*c)).collect();
        if live.is_empty() {
            for x in 0..2 {
                if !self.ended[x] {
                    self.fail(format!(
                        "1b all carriers dead but phone {} has not ended after the quiet period",
                        (b'A' + x as u8) as char
                    ));
                }
            }
            return;
        }
        for x in 0..2 {
            if self.ended[x] {
                self.fail(format!(
                    "1 phone {} ended while carrier(s) lived",
                    (b'A' + x as u8) as char
                ));
            }
        }
        if self.cfg.require_lossless
            && self.faults_used == 0
            && self.switch_offs_used == 0
            && self.quality_used == 0
            && !self.cfg.duplicates
            && !self.cfg.drops
        {
            for x in 0..2 {
                let stats = self.media_stats(x);
                if stats.opened != stats.sealed {
                    self.fail(format!(
                        "8 normal switch lost media: phone {} sealed {}, the peer opened {}",
                        (b'A' + x as u8) as char,
                        stats.sealed,
                        stats.opened
                    ));
                }
            }
        }
        if self.cfg.drops {
            return;
        }
        for x in 0..2 {
            let y = 1 - x;
            let send = self.m[x].send_on();
            let accept = self.m[y].accept();
            if send != accept || send < 0 || !self.ground_live(send as usize) {
                self.fail(format!(
                    "5 no convergence {}->{}: sendOn={} acceptFrom={} live={}",
                    (b'A' + x as u8) as char,
                    (b'A' + y as u8) as char,
                    carrier_name(send),
                    carrier_name(accept),
                    live.iter().map(|c| carrier_name(*c as i32)).collect::<Vec<_>>().join(", ")
                ));
            }
            if !self.cfg.drops {
                for k in 0..self.cfg.keys {
                    if self.state_view[y][k] != self.state_cur[x][k] {
                        self.fail(format!(
                            "4 state key {k}: {} holds {} but {} opened {}",
                            (b'A' + x as u8) as char,
                            self.state_cur[x][k],
                            (b'A' + y as u8) as char,
                            self.state_view[y][k]
                        ));
                    }
                }
            }
        }
    }

    pub fn hash(&self) -> i64 {
        struct Hasher(i64);
        impl Hasher {
            fn add(&mut self, v: i64) {
                self.0 = (self.0 ^ v).wrapping_mul(1_099_511_628_211);
            }
        }
        let mut hasher = Hasher(-3_750_763_034_362_895_579);
        hasher.add(i64::from(self.tick));
        hasher.add(if self.phase2 {
            i64::from(self.quiesce_tick) + 1
        } else {
            0
        });
        hasher.add(i64::from(self.faults_used));
        hasher.add(i64::from(self.attempts));
        hasher.add(i64::from(self.ground_gen0));
        hasher.add(i64::from(self.replacements_used));
        hasher.add(i64::from(self.switch_offs_used));
        hasher.add(i64::from(self.quality_used));
        for x in 0..2 {
            self.m[x].hash_into(&mut hasher.0);
            hasher.add(self.next_seq[x]);
            hasher.add(i64::from(self.media_sealed[x]));
            hasher.add(i64::from(self.state_changes[x]));
            hasher.add(self.last_delivered[x]);
            hasher.add(i64::from(self.ended[x]));
            hasher.add(i64::from(self.view_gen0[x]));
            hasher.add(i64::from(self.switched_off_ground[x])
                + 2 * i64::from(self.machine_switched_off[x])
                + 4 * i64::from(self.stale_seen[x])
                + 8 * i64::from(self.renewal_busy[x])
                + 16 * i64::from(self.held_death[x])
                + 32 * i64::from(self.renewal_failed[x]));
            if self.cfg.require_lossless {
                hasher.add(self.delivered_media[x].len() as i64);
            }
            for k in 0..self.cfg.keys {
                hasher.add(i64::from(self.state_cur[x][k]));
                hasher.add(i64::from(self.state_view[x][k]));
            }
            for carrier in 0..self.slots {
                hasher.add(i64::from(self.pending[x][carrier])
                    + 2 * i64::from(self.up[x][carrier])
                    + 4 * i64::from(self.local_dead[x][carrier])
                    + 8 * i64::from(self.reader_stopped[x][carrier]));
                hasher.add(self.chan[x][carrier].len() as i64);
                for pkt in &self.chan[x][carrier] {
                    if self.cfg.max_delay_ticks >= 0 {
                        hasher.add(i64::from(
                            (self.tick - pkt.sent_tick).min(self.cfg.max_delay_ticks + 1),
                        ));
                    }
                    match &pkt.wire {
                        Wire::Content(rec) => {
                            hasher.add(1);
                            hasher.add(rec.seq);
                            hasher.add(i64::from(rec.tag));
                        }
                        Wire::Switch {
                            generation,
                            boundary,
                            carrier,
                        } => {
                            hasher.add(2);
                            hasher.add(*generation);
                            hasher.add(*boundary);
                            hasher.add(i64::from(*carrier));
                        }
                        Wire::Resume {
                            generation,
                            next,
                            carrier,
                        } => {
                            hasher.add(3);
                            hasher.add(*generation);
                            hasher.add(*next);
                            hasher.add(i64::from(*carrier));
                        }
                    }
                }
            }
        }
        for x in 0..2 {
            hasher.add(self.inbound[x].len() as i64);
            for item in &self.inbound[x] {
                if self.cfg.max_consume_lag_ticks >= 0 {
                    hasher.add(i64::from(
                        (self.tick - item.read_tick).min(self.cfg.max_consume_lag_ticks + 1),
                    ));
                }
                hasher.add(i64::from(item.carrier));
                hasher.add(i64::from(item.ev));
                hasher.add(i64::from(item.gen0));
                match &item.wire {
                    None => hasher.add(0),
                    Some(Wire::Content(rec)) => {
                        hasher.add(1);
                        hasher.add(rec.seq);
                        hasher.add(i64::from(rec.tag));
                    }
                    Some(Wire::Switch {
                        generation,
                        boundary,
                        carrier,
                    }) => {
                        hasher.add(2);
                        hasher.add(*generation);
                        hasher.add(*boundary);
                        hasher.add(i64::from(*carrier));
                    }
                    Some(Wire::Resume {
                        generation,
                        next,
                        carrier,
                    }) => {
                        hasher.add(3);
                        hasher.add(*generation);
                        hasher.add(*next);
                        hasher.add(i64::from(*carrier));
                    }
                }
            }
        }
        for carrier in 0..self.slots {
            hasher.add(i64::from(self.fault[carrier]));
            hasher.add(i64::from(self.created[carrier]));
        }
        hasher.0
    }

    /// Media records sealed by `sender` / opened by the peer, and the longest gap between two opened ones.
    pub fn media_stats(&self, sender: usize) -> MediaStats {
        let opened = &self.delivered_media[sender];
        let mut previous_tick = -1;
        let mut max_gap = 0;
        for seq in opened {
            let tick = self.sealed_at_tick[sender][*seq as usize];
            if previous_tick >= 0 {
                max_gap = max_gap.max(tick - previous_tick);
            }
            previous_tick = tick;
        }
        let sealed = self.sealed_tag[sender].iter().filter(|tag| **tag == 0).count();
        MediaStats {
            sealed,
            opened: opened.len(),
            max_gap_ticks: max_gap,
        }
    }

    /// Runs the deterministic completion: quiesce, then the first enabled action until none.
    pub fn complete(&mut self) {
        let mut codes = Vec::new();
        if !self.phase2 {
            self.apply(code(QUIESCE, 0, 0));
        }
        while self.violation.is_none() {
            codes.clear();
            self.enabled(&mut codes);
            if codes.is_empty() {
                break;
            }
            let preferred = codes.iter().copied().find(|cd| {
                matches!(
                    kind(*cd),
                    DELIVER
                        | CONSUME
                        | DETECT
                        | NEW_UP
                        | STANDBY_EVENT
                        | STANDBY_STALE
                        | RENEWAL_FAILED
                )
            });
            let pick = preferred.unwrap_or(codes[0]);
            self.apply(pick);
        }
        self.check_final();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MediaStats {
    pub sealed: usize,
    pub opened: usize,
    pub max_gap_ticks: i32,
}

fn next_value(key: usize, value: i32) -> i32 {
    if key == 0 {
        if value < 2 { value + 1 } else { -1 }
    } else {
        1 - value
    }
}

/// Replays `codes` on a fresh world; None when an action was not enabled (an invalid trace).
pub fn replay_cutover(cfg: Cfg, codes: &[i32], trace: bool) -> Option<CutoverSim> {
    let mut sim = CutoverSim::new(cfg);
    if trace {
        sim.log = Some(Vec::new());
    }
    for action in codes {
        if sim.violation.is_some() {
            break;
        }
        if !sim.is_enabled(*action) {
            return None;
        }
        sim.apply(*action);
    }
    if sim.violation.is_none() {
        sim.complete();
    }
    Some(sim)
}

/// Delta-debugging shrink: drop chunks of actions while the same invariant family keeps failing.
pub fn shrink_cutover(cfg: Cfg, codes: &[i32], family: &str) -> Vec<i32> {
    let mut current = codes.to_vec();
    let mut chunk = (current.len() / 2).max(1);
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < current.len() {
            let end = (i + chunk).min(current.len());
            let candidate: Vec<i32> = current[..i]
                .iter()
                .chain(current[end..].iter())
                .copied()
                .collect();
            let result = replay_cutover(cfg.clone(), &candidate, false);
            if let Some(sim) = result {
                if sim
                    .violation
                    .as_deref()
                    .is_some_and(|violation| violation.starts_with(family))
                {
                    current = candidate;
                    changed = true;
                } else {
                    i += chunk;
                }
            } else {
                i += chunk;
            }
        }
        if chunk == 1 && !changed {
            return current;
        }
        if chunk > 1 {
            chunk = (chunk / 2).max(1);
        }
    }
}

pub struct LongSet {
    table: Vec<u64>,
    pub size: u64,
}

impl Default for LongSet {
    fn default() -> Self {
        Self {
            table: vec![0; 1 << 16],
            size: 0,
        }
    }
}

impl LongSet {
    pub fn add(&mut self, value: i64) -> bool {
        let v = if value == 0 { 1 } else { value as u64 };
        // A1F2: an 85% load keeps the mandated 30M-state search inside the
        // unit-test worker heap; membership is unchanged.
        if self.size * 20 >= self.table.len() as u64 * 17 {
            self.grow();
        }
        let inserted = Self::insert(&mut self.table, v);
        if inserted {
            self.size += 1;
        }
        inserted
    }

    fn insert(table: &mut [u64], value: u64) -> bool {
        let mut index = ((value ^ (value >> 29)) as usize) & (table.len() - 1);
        while table[index] != 0 {
            if table[index] == value {
                return false;
            }
            index = (index + 1) & (table.len() - 1);
        }
        table[index] = value;
        true
    }

    fn grow(&mut self) {
        let old = std::mem::take(&mut self.table);
        self.table = vec![0; old.len() * 2];
        for value in old {
            if value != 0 {
                Self::insert(&mut self.table, value);
            }
        }
    }
}

pub struct Explored {
    pub states: u64,
    pub terminals: u64,
    pub violation: Option<String>,
    pub trace: Option<Vec<i32>>,
    pub capped: bool,
    pub coverage: HashMap<String, u64>,
}

fn dfs(
    sim: CutoverSim,
    seen: &mut LongSet,
    path: &mut Vec<i32>,
    terminals: &mut u64,
    found: &mut Option<String>,
    found_trace: &mut Option<Vec<i32>>,
    capped: &mut bool,
    max_states: u64,
) {
    if found.is_some() || *capped {
        return;
    }
    if !seen.add(sim.hash()) {
        return;
    }
    if seen.size > max_states {
        *capped = true;
        return;
    }
    let mut codes = Vec::new();
    sim.enabled(&mut codes);
    if codes.is_empty() {
        *terminals += 1;
        let mut terminal = sim;
        terminal.check_final();
        if let Some(violation) = terminal.violation {
            *found = Some(violation);
            *found_trace = Some(path.clone());
        }
        return;
    }
    for action in codes {
        let mut child = sim.copy();
        child.apply(action);
        path.push(action);
        if let Some(violation) = &child.violation {
            *found = Some(violation.clone());
            *found_trace = Some(path.clone());
            return;
        }
        dfs(
            child,
            seen,
            path,
            terminals,
            found,
            found_trace,
            capped,
            max_states,
        );
        if found.is_some() {
            return;
        }
        path.pop();
    }
}

pub fn explore_cutover(cfg: Cfg, max_states: u64) -> Explored {
    let handle = std::thread::Builder::new()
        .name("cx-dfs".into())
        .stack_size(512 * 1024 * 1024)
        .spawn(move || {
            let coverage: Rc<RefCell<HashMap<String, u64>>> = Rc::new(RefCell::new(HashMap::new()));
            let mut root = CutoverSim::new(cfg);
            root.enable_coverage(coverage.clone());
            let mut seen = LongSet::default();
            let mut path = Vec::new();
            let mut terminals = 0;
            let mut found = None;
            let mut found_trace = None;
            let mut capped = false;
            dfs(
                root,
                &mut seen,
                &mut path,
                &mut terminals,
                &mut found,
                &mut found_trace,
                &mut capped,
                max_states,
            );
            let coverage = coverage.borrow().clone();
            Explored {
                states: seen.size,
                terminals,
                violation: found,
                trace: found_trace,
                capped,
                coverage,
            }
        })
        .expect("start the model search thread");
    handle.join().expect("model search thread")
}
