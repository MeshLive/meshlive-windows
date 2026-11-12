//! The pure carrier-cutover state machine of a hop with an old carrier and at
//! most one new one (ported from A1's final `CarrierCutover.kt`). One instance
//! is one phone's view of one session: the old link (`OLD`) and at most one new
//! link at a time (an id from 1 to 62), covering both directions: the sender
//! state `send_on` and the receiver state `accept`. No platform types and no
//! threads: every input carries the injected clock (`now_ms`) and returns the
//! actions the carrier owner must perform.
//!
//! Root invariant: a race may lose records but must never end a live session.
//! Records are dropped, never thrown at the opener: anything not strictly above
//! the last opened sequence is stale. No rule closes a carrier that is the last
//! one alive.
//!
//! The corrections the model found stay switchable in [`Config`]
//! ([`Config::as_first_draft`] turns every correction back) so counterexamples
//! stay reproducible; the defaults are the corrected behaviour.

pub const OLD: i32 = 0;
pub const NONE: i32 = -1;
pub const ANY: i32 = -2;
const NOT_ACTIVE: i64 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldExpiry {
    /// Fixed: a stuck `SWITCH` makes the receiver take the new carrier; no carrier is closed.
    Promote,
    /// First draft: drop the held records and close the new carrier (can end a live session).
    CloseNew,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Ceiling for early new-carrier records before their `SWITCH` (PROTOCOL: 1 s).
    pub hold_ms: i64,
    /// Ceiling for the same records in bytes (PROTOCOL: 5 MiB).
    pub hold_bytes: i64,
    /// The call must have been active this long before the sender switches (PROTOCOL: 3 s).
    pub active_before_switch_ms: i64,
    /// After a failure of the new carrier no new switch starts for this long (PROTOCOL: 30 s).
    pub cooldown_ms: i64,
    /// A switch starts only with fewer fallbacks than this in the session (PROTOCOL: 2).
    pub max_fallbacks: i32,
    /// An upgrade attempt that has not become ready after this long is closed.
    pub pending_max_ms: i64,
    pub hold_expiry: HoldExpiry,
    /// Fixed: losing `OLD` while a new carrier is still being set up waits for it.
    pub pending_keeps_session: bool,
    /// Fixed: a `SWITCH` that arrives while this side's new carrier is still pending is kept.
    pub switch_waits_for_pending_new: bool,
    /// Fixed: `SWITCH` and `RESUME` name the new-carrier instance they are about; `ANY` is burned.
    pub markers_name_carrier: bool,
    /// Fixed: `SWITCH` waits in the old link's ordinary media FIFO.
    pub switch_through_media_fifo: bool,
    /// Fixed: no normal switch while the old link's oldest queued record has waited `hold_ms / 2`.
    pub gate_switch_on_old_queue: bool,
    /// Fixed: `RESUME.next` is kept as the floor of what the old link may deliver afterwards.
    pub resume_sets_old_floor: bool,
    /// Fixed: a new carrier that dies while `OLD` lives always gets a `RESUME` naming it.
    pub resume_always_written: bool,
    /// F-6: a `SWITCH` that arrives on the new carrier while the old link is
    /// alive is only counted as `old-dead-hint`; it never closes the old link.
    pub old_dead_hint: bool,
    /// A1: an in-call link renewal is an event (`on_standby_replaced`).
    pub standby_replaced_event: bool,
    /// A1: after a standby replacement the last `SWITCH`, or the `RESUME` of the last fallback, is written again.
    pub reassert_marker_after_replace: bool,
    /// A1: a switch-off never closes the last live carrier; it waits for a standby.
    pub switch_off_keeps_last_carrier: bool,
    /// A1: after a switch-off no upgrade attempt is accepted.
    pub switch_off_blocks_attempts: bool,
    /// A1: a quality fallback counts toward `max_fallbacks` even when this direction never moved.
    pub quality_fallback_counts: bool,
    /// A1 (Opus A1 2): a switch-off or quality fallback never releases Wi-Fi while the standby is stale.
    pub standby_freshness_gates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hold_ms: 1_000,
            hold_bytes: 5 * 1024 * 1024,
            active_before_switch_ms: 3_000,
            cooldown_ms: 30_000,
            max_fallbacks: 2,
            pending_max_ms: 5_000,
            hold_expiry: HoldExpiry::Promote,
            pending_keeps_session: true,
            switch_waits_for_pending_new: true,
            markers_name_carrier: true,
            switch_through_media_fifo: true,
            gate_switch_on_old_queue: true,
            resume_sets_old_floor: true,
            resume_always_written: true,
            old_dead_hint: true,
            standby_replaced_event: true,
            reassert_marker_after_replace: true,
            switch_off_keeps_last_carrier: true,
            switch_off_blocks_attempts: true,
            quality_fallback_counts: true,
            standby_freshness_gates: true,
        }
    }
}

impl Config {
    /// The behaviour exactly as the first PROTOCOL draft (round 6) described it.
    pub fn as_first_draft(self) -> Self {
        Self {
            pending_max_ms: 10_000,
            hold_expiry: HoldExpiry::CloseNew,
            pending_keeps_session: false,
            switch_waits_for_pending_new: false,
            markers_name_carrier: false,
            switch_through_media_fifo: false,
            gate_switch_on_old_queue: false,
            resume_sets_old_floor: false,
            resume_always_written: false,
            old_dead_hint: false,
            standby_replaced_event: false,
            reassert_marker_after_replace: false,
            switch_off_keeps_last_carrier: false,
            switch_off_blocks_attempts: false,
            quality_fallback_counts: false,
            standby_freshness_gates: false,
            ..self
        }
    }
}

/// One sealed record: the sender's sequence, its size and an opaque owner tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rec {
    pub seq: i64,
    pub bytes: i64,
    pub tag: u32,
}

/// What travels on a carrier: media or state content, or one of the two in-band markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wire {
    Content(Rec),
    /// `carrier` is the new-carrier instance the marker is about, or `ANY` in the first draft.
    Switch {
        generation: i64,
        boundary: i64,
        carrier: i32,
    },
    Resume {
        generation: i64,
        next: i64,
        carrier: i32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    ClosedCarrier,
    Stale,
    Fenced,
    StaleMarker,
    UnknownCarrier,
    NoCarrier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// The first draft's hold expiry: a policy decision, not a carrier failure.
    HoldExpired,
    /// The peer wrote `RESUME`: its new carrier is gone, so clean up this end.
    PeerResumed,
    /// The upgrade attempt did not become ready in time.
    HandshakeTimeout,
    /// Never produced since review F-6; kept so the order of the values does not change.
    PeerOldDead,
    /// A1: the user turned the Wi-Fi switch off: a planned release of the new carrier.
    SwitchOff,
    /// A1: the quality rule fired: a planned fallback.
    Quality,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `priority`: the owner may serve this write before queued media (only the first draft's `SWITCH`).
    Write {
        carrier: i32,
        wire: Wire,
        priority: bool,
    },
    Deliver {
        rec: Rec,
        carrier: i32,
    },
    Drop {
        reason: DropReason,
        wire: Option<Wire>,
        carrier: i32,
    },
    CloseCarrier {
        carrier: i32,
        reason: CloseReason,
    },
    /// Seal the current value of every indispensable state under fresh sequences.
    ReassertState,
    /// This phone's own encoder must produce a keyframe next.
    ForceLocalKeyframe,
    /// Ask the peer's encoder for a keyframe.
    RequestPeerKeyframe,
    /// Drop the unsent fragments of the frame being sent.
    ResetSendFragments,
    /// Drop the fragment reassembler's partial frame and skip fragments until offset 0.
    SuppressFragments,
    EndSession {
        reason: &'static str,
    },
}

pub struct CarrierCutover {
    config: Config,
    // Local view of the carriers.
    old_alive: bool,
    new_id: i32,
    pending_id: i32,
    pending_since_ms: i64,
    dead_mask: u64,
    // A1: the user's switch and the standby-replacement bookkeeping.
    switched_off: bool,
    switch_off_pending: bool,
    standby_fresh: bool,
    ended: bool,
    // Sender (what this phone writes).
    send_on: i32,
    generation: i64,
    last_switch_generation: i64,
    last_switch_boundary: i64,
    last_sealed: i64,
    fallbacks: i32,
    cooldown_until_ms: i64,
    active_since_ms: i64,
    last_fallback_resume: Option<Wire>,
    // Receiver (what this phone opens).
    accept: i32,
    boundary: i64,
    old_from: i64,
    last_opened: i64,
    max_generation: i64,
    seen_switch_generation: i64,
    seen_switch_boundary: i64,
    pending_switch_boundary: i64,
    pending_switch_carrier: i32,
    held: Vec<Rec>,
    held_bytes: i64,
    hold_since_ms: i64,
    hold_exhaustions: u64,
    out: Vec<Action>,
    /// Statistics hook for the model test's coverage report; never used by the app.
    pub on_stat: Option<Box<dyn FnMut(&str)>>,
}

impl CarrierCutover {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            old_alive: true,
            new_id: NONE,
            pending_id: NONE,
            pending_since_ms: 0,
            dead_mask: 0,
            switched_off: false,
            switch_off_pending: false,
            standby_fresh: true,
            ended: false,
            send_on: OLD,
            generation: 0,
            last_switch_generation: 0,
            last_switch_boundary: 0,
            last_sealed: -1,
            fallbacks: 0,
            cooldown_until_ms: 0,
            active_since_ms: NOT_ACTIVE,
            last_fallback_resume: None,
            accept: OLD,
            boundary: 0,
            old_from: 0,
            last_opened: -1,
            max_generation: 0,
            seen_switch_generation: 0,
            seen_switch_boundary: 0,
            pending_switch_boundary: -1,
            pending_switch_carrier: NONE,
            held: Vec::new(),
            held_bytes: 0,
            hold_since_ms: 0,
            hold_exhaustions: 0,
            out: Vec::new(),
            on_stat: None,
        }
    }

    /// True while the machine waits for a timer (hold, upgrade attempt) that `on_tick` will act on.
    pub fn has_timer(&self) -> bool {
        !self.held.is_empty() || self.pending_id != NONE
    }
    /// True while a timer or a still-possible switch can change the carriers by itself.
    pub fn can_still_change_by_time(&self) -> bool {
        !self.ended
            && (self.has_timer()
                || self.send_on == OLD
                    && self.old_alive
                    && self.new_id != NONE
                    && self.fallbacks < self.config.max_fallbacks)
    }
    pub fn old_is_alive(&self) -> bool {
        self.old_alive
    }
    pub fn old_alive(&self) -> bool {
        self.old_alive
    }
    pub fn new_carrier(&self) -> i32 {
        self.new_id
    }
    pub fn pending_carrier(&self) -> i32 {
        self.pending_id
    }
    pub fn held_count(&self) -> usize {
        self.held.len()
    }
    pub fn fallback_count(&self) -> i32 {
        self.fallbacks
    }
    /// A1 (model test and owner): the user turned Wi-Fi off.
    pub fn switched_off_now(&self) -> bool {
        self.switched_off
    }
    /// A1: Wi-Fi is the only carrier and the switch is off; the release waits for a standby.
    pub fn switch_off_pending_now(&self) -> bool {
        self.switch_off_pending
    }
    /// A1: the highest `SWITCH`/`RESUME` generation this receiver has seen.
    pub fn generation_seen(&self) -> i64 {
        self.max_generation
    }
    /// A1: the owner's last freshness report for the standby.
    pub fn standby_fresh_now(&self) -> bool {
        self.standby_fresh
    }
    pub fn send_on(&self) -> i32 {
        self.send_on
    }
    pub fn accept(&self) -> i32 {
        self.accept
    }
    pub fn ended(&self) -> bool {
        self.ended
    }
    pub fn last_opened(&self) -> i64 {
        self.last_opened
    }
    /// How often the hold ceiling was reached (statistics only).
    pub fn hold_exhaustions(&self) -> u64 {
        self.hold_exhaustions
    }
    pub fn config(&self) -> Config {
        self.config
    }

    fn stat(&mut self, key: &str) {
        if let Some(hook) = &mut self.on_stat {
            hook(key);
        }
    }

    pub fn clone_state(&self) -> Self {
        Self {
            config: self.config,
            old_alive: self.old_alive,
            new_id: self.new_id,
            pending_id: self.pending_id,
            pending_since_ms: self.pending_since_ms,
            dead_mask: self.dead_mask,
            switched_off: self.switched_off,
            switch_off_pending: self.switch_off_pending,
            standby_fresh: self.standby_fresh,
            ended: self.ended,
            send_on: self.send_on,
            generation: self.generation,
            last_switch_generation: self.last_switch_generation,
            last_switch_boundary: self.last_switch_boundary,
            last_sealed: self.last_sealed,
            fallbacks: self.fallbacks,
            cooldown_until_ms: self.cooldown_until_ms,
            active_since_ms: self.active_since_ms,
            last_fallback_resume: self.last_fallback_resume.clone(),
            accept: self.accept,
            boundary: self.boundary,
            old_from: self.old_from,
            last_opened: self.last_opened,
            max_generation: self.max_generation,
            seen_switch_generation: self.seen_switch_generation,
            seen_switch_boundary: self.seen_switch_boundary,
            pending_switch_boundary: self.pending_switch_boundary,
            pending_switch_carrier: self.pending_switch_carrier,
            held: self.held.clone(),
            held_bytes: self.held_bytes,
            hold_since_ms: self.hold_since_ms,
            out: Vec::new(),
            on_stat: None,
            hold_exhaustions: self.hold_exhaustions,
        }
    }

    /// Mixes the whole state into `h` (model checking: equal states have equal hashes).
    pub fn hash_into(&self, h: &mut i64) {
        let mut add = |v: i64| {
            *h = (*h ^ v).wrapping_mul(1_099_511_628_211);
        };
        add(i64::from(self.old_alive));
        add(i64::from(self.new_id));
        add(i64::from(self.pending_id));
        add(self.pending_since_ms);
        add(self.dead_mask as i64);
        add(i64::from(self.ended));
        add(i64::from(self.send_on));
        add(self.generation);
        add(self.last_switch_generation);
        add(self.last_switch_boundary);
        add(self.last_sealed);
        add(i64::from(self.fallbacks));
        add(self.cooldown_until_ms);
        add(self.active_since_ms);
        add(i64::from(self.accept));
        add(self.boundary);
        add(self.old_from);
        add(self.last_opened);
        add(self.max_generation);
        add(self.seen_switch_generation);
        add(self.seen_switch_boundary);
        add(self.pending_switch_boundary);
        add(i64::from(self.pending_switch_carrier));
        add(self.held_bytes);
        add(self.hold_since_ms);
        add(self.held.len() as i64);
        for record in &self.held {
            add(record.seq);
        }
        add(i64::from(self.switched_off));
        add(i64::from(self.switch_off_pending));
        add(i64::from(self.standby_fresh));
        match &self.last_fallback_resume {
            None => add(0),
            Some(Wire::Resume {
                generation,
                next,
                carrier,
            }) => {
                add(*generation);
                add(*next);
                add(i64::from(*carrier));
            }
            Some(_) => add(0),
        }
    }

    // ---------------------------------------------------------------- inputs

    /// The call became (or is) active: starts the clock for the 3 s rule.
    pub fn on_call_active(
        &mut self,
        now_ms: i64,
        old_queue_age_ms: i64,
        at_frame_boundary: bool,
    ) -> Vec<Action> {
        if self.active_since_ms == NOT_ACTIVE {
            self.active_since_ms = now_ms;
        }
        self.maybe_switch(now_ms, old_queue_age_ms, at_frame_boundary);
        self.drain()
    }

    /// The owner sealed `rec` (its sequence is above every earlier one); it goes to `send_on`.
    pub fn on_sealed(&mut self, rec: Rec, _now_ms: i64) -> Vec<Action> {
        if self.ended {
            return self.drain();
        }
        assert!(
            rec.seq > self.last_sealed,
            "sequences must increase: {} after {}",
            rec.seq,
            self.last_sealed
        );
        self.last_sealed = rec.seq;
        if self.send_on == NONE {
            self.out.push(Action::Drop {
                reason: DropReason::NoCarrier,
                wire: Some(Wire::Content(rec)),
                carrier: NONE,
            });
        } else {
            self.out.push(Action::Write {
                carrier: self.send_on,
                wire: Wire::Content(rec),
                priority: false,
            });
        }
        self.drain()
    }

    /// An upgrade attempt has started (the new socket exists, `BIND` and `READY` are not done).
    pub fn on_carrier_pending(&mut self, carrier: i32, now_ms: i64) -> Vec<Action> {
        assert!(
            (1..=62).contains(&carrier),
            "attempt ids are small per-session counters"
        );
        if self.ended || self.is_dead(carrier) {
            return self.drain();
        }
        if self.switched_off && self.config.switch_off_blocks_attempts {
            self.stat("pending-ignored-after-switch-off");
            return self.drain();
        }
        self.pending_id = carrier;
        self.pending_since_ms = now_ms;
        self.pending_switch_boundary = -1;
        self.drain()
    }

    /// The new carrier finished `BIND` and `READY` on this side.
    pub fn on_carrier_up(
        &mut self,
        carrier: i32,
        now_ms: i64,
        old_queue_age_ms: i64,
        at_frame_boundary: bool,
    ) -> Vec<Action> {
        assert!(carrier != OLD);
        if self.ended || self.is_dead(carrier) {
            return self.drain();
        }
        if self.switched_off
            && self.config.switch_off_blocks_attempts
            && self.pending_id != carrier
        {
            return self.drain();
        }
        if self.pending_id == carrier {
            self.pending_id = NONE;
        }
        self.new_id = carrier;
        if !self.old_alive {
            self.only_new_carrier_left();
        } else {
            if self.pending_switch_boundary >= 0
                && self.accept == OLD
                && (self.pending_switch_carrier == carrier || self.pending_switch_carrier == ANY)
            {
                self.accept = carrier;
                self.boundary = self.pending_switch_boundary;
                self.stat("switch-applied-at-ready");
                self.out.push(Action::SuppressFragments);
            }
            self.pending_switch_boundary = -1;
            self.maybe_switch(now_ms, old_queue_age_ms, at_frame_boundary);
        }
        self.drain()
    }

    /// A carrier failed or was closed by the peer, as seen by this phone.
    pub fn on_carrier_died(&mut self, carrier: i32, now_ms: i64) -> Vec<Action> {
        if !self.ended {
            self.carrier_gone(carrier, now_ms);
        }
        self.drain()
    }

    /// An in-call link renewal bound a new Bluetooth link. Carrier `OLD` keeps
    /// its id but is a new instance: it is alive again, whatever this phone
    /// believed. A new instance is fresh. Nothing about the old instance may
    /// reach the machine after this call (the owner stamps queued items).
    pub fn on_standby_replaced(&mut self, now_ms: i64) -> Vec<Action> {
        if self.ended || !self.config.standby_replaced_event {
            return self.drain();
        }
        self.stat("standby-replaced");
        self.dead_mask &= !1;
        self.old_alive = true;
        self.standby_fresh = true;
        if self.send_on == NONE {
            self.stat("standby-replaced-from-none");
            self.send_on = OLD;
            self.out.push(Action::ResetSendFragments);
            self.out.push(Action::ForceLocalKeyframe);
        }
        if self.accept == NONE {
            self.accept = OLD;
            self.boundary = 0;
            self.out.push(Action::SuppressFragments);
            self.out.push(Action::RequestPeerKeyframe);
        }
        if self.switch_off_pending {
            self.complete_switch_off("switch-off-completed-on-replace", now_ms);
        } else if self.config.reassert_marker_after_replace {
            self.reassert_marker();
        }
        // A1F2: the marker is queued first so the re-sealed records follow it
        // and its boundary covers them.
        self.out.push(Action::ReassertState);
        self.drain()
    }

    /// The user turned the Wi-Fi switch off. From now on no upgrade attempt
    /// is accepted. An attempt in progress is cancelled when a usable standby
    /// exists. The new carrier is released as a planned fallback (no fallback
    /// counted, no cooldown). When Wi-Fi is all there is (the old carrier is
    /// dead, or stale) nothing changes for now.
    pub fn on_switch_off(&mut self, now_ms: i64) -> Vec<Action> {
        if self.ended || self.switched_off {
            return self.drain();
        }
        self.switched_off = true;
        self.stat("switch-off");
        let wifi_is_all_there_is = self.config.switch_off_keeps_last_carrier && !self.standby_usable();
        let wait_key = if self.old_alive {
            "switch-off-standby-stale"
        } else {
            "switch-off-wifi-only"
        };
        if self.pending_id != NONE && !wifi_is_all_there_is {
            self.stat("switch-off-cancelled-pending");
            let carrier = self.pending_id;
            self.out.push(Action::CloseCarrier {
                carrier,
                reason: CloseReason::SwitchOff,
            });
            self.carrier_gone(carrier, now_ms);
            if self.ended {
                return self.drain();
            }
        }
        if self.new_id != NONE {
            if wifi_is_all_there_is {
                self.stat(wait_key);
                self.switch_off_pending = true;
            } else {
                self.stat(if self.old_alive {
                    "switch-off-fell-back"
                } else {
                    "switch-off-closed-last-carrier"
                });
                self.release_new(CloseReason::SwitchOff, false, false, now_ms);
            }
        } else if self.pending_id != NONE && wifi_is_all_there_is {
            self.stat(wait_key);
            self.switch_off_pending = true;
        }
        self.drain()
    }

    /// The quality rule fired on the new carrier. A planned fallback that
    /// counts toward `max_fallbacks` and starts the cooldown. With no usable
    /// standby nothing happens.
    pub fn on_quality_fallback(&mut self, now_ms: i64) -> Vec<Action> {
        if self.ended || self.new_id == NONE {
            return self.drain();
        }
        if !self.standby_usable() {
            self.stat(if self.old_alive {
                "quality-fallback-standby-stale"
            } else {
                "quality-fallback-no-standby"
            });
            return self.drain();
        }
        self.stat("quality-fallback");
        let moved = self.send_on == self.new_id;
        self.release_new(CloseReason::Quality, true, true, now_ms);
        if !moved && self.config.quality_fallback_counts {
            self.fallbacks += 1;
        }
        self.drain()
    }

    /// The markers that were in flight on the old instance are lost. Write the
    /// current one again on the new standby; the receiver treats a repeat as a
    /// repeat and a stale generation as stale.
    fn reassert_marker(&mut self) {
        if self.send_on != OLD && self.send_on != NONE {
            self.stat("reassert-switch");
            self.out.push(Action::Write {
                carrier: OLD,
                wire: Wire::Switch {
                    generation: self.last_switch_generation,
                    boundary: self.last_switch_boundary,
                    carrier: self.marker(self.send_on),
                },
                priority: false,
            });
        } else if self.send_on == OLD {
            let Some(resume) = self.last_fallback_resume.clone() else {
                return;
            };
            self.stat("reassert-resume");
            self.out.push(Action::Write {
                carrier: OLD,
                wire: resume,
                priority: false,
            });
        }
    }

    /// A standby that can take the call back now: alive in this phone's view
    /// and, with the gate, heard lately.
    fn standby_usable(&self) -> bool {
        self.old_alive && (self.standby_fresh || !self.config.standby_freshness_gates)
    }

    /// The switch-off that waited for a usable standby completes now.
    fn complete_switch_off(&mut self, key: &str, now_ms: i64) {
        self.switch_off_pending = false;
        self.stat(key);
        if self.new_id != NONE {
            self.release_new(CloseReason::SwitchOff, false, false, now_ms);
        } else if self.pending_id != NONE {
            let carrier = self.pending_id;
            self.out.push(Action::CloseCarrier {
                carrier,
                reason: CloseReason::SwitchOff,
            });
            self.carrier_gone(carrier, now_ms);
        }
    }

    /// The owner's view of the standby's freshness changed. A stale standby is
    /// kept (only its own liveness failure closes it), but a switch-off waits
    /// for it and the quality rule does nothing. When it is fresh again a
    /// waiting switch-off completes. The owner calls it only on a change.
    pub fn on_standby_freshness(&mut self, fresh: bool, now_ms: i64) -> Vec<Action> {
        if self.ended || self.standby_fresh == fresh {
            return self.drain();
        }
        self.standby_fresh = fresh;
        self.stat(if fresh { "standby-fresh" } else { "standby-stale" });
        if fresh && self.switch_off_pending && self.old_alive {
            self.complete_switch_off("switch-off-completed-when-fresh", now_ms);
        }
        self.drain()
    }

    /// One record or marker was read from `carrier` and queued in order for the single consumer.
    pub fn on_arrive(&mut self, carrier: i32, wire: Wire, now_ms: i64) -> Vec<Action> {
        if self.ended {
            return self.drain();
        }
        if self.is_dead(carrier) {
            self.out.push(Action::Drop {
                reason: DropReason::ClosedCarrier,
                wire: Some(wire),
                carrier,
            });
            return self.drain();
        }
        match wire {
            Wire::Content(rec) => self.arrive_content(carrier, rec, now_ms),
            Wire::Switch {
                generation,
                boundary,
                carrier: named,
            } => self.arrive_switch(
                carrier,
                Wire::Switch {
                    generation,
                    boundary,
                    carrier: named,
                },
                now_ms,
            ),
            Wire::Resume {
                generation,
                next,
                carrier: named,
            } => self.arrive_resume(
                carrier,
                Wire::Resume {
                    generation,
                    next,
                    carrier: named,
                },
                now_ms,
            ),
        }
        self.drain()
    }

    /// Timers: the hold ceiling, the upgrade-attempt ceiling and the sender's switch conditions.
    pub fn on_tick(
        &mut self,
        now_ms: i64,
        old_queue_age_ms: i64,
        at_frame_boundary: bool,
    ) -> Vec<Action> {
        if self.ended {
            return self.drain();
        }
        if !self.held.is_empty() && now_ms - self.hold_since_ms >= self.config.hold_ms {
            self.hold_exhausted(now_ms);
        }
        if !self.ended
            && self.pending_id != NONE
            && now_ms - self.pending_since_ms >= self.config.pending_max_ms
        {
            let carrier = self.pending_id;
            self.out.push(Action::CloseCarrier {
                carrier,
                reason: CloseReason::HandshakeTimeout,
            });
            self.carrier_gone(carrier, now_ms);
        }
        if !self.ended {
            self.maybe_switch(now_ms, old_queue_age_ms, at_frame_boundary);
        }
        self.drain()
    }

    // ---------------------------------------------------------------- sender rows

    /// Sender row 1: the new carrier is ready and the conditions hold, so switch.
    fn maybe_switch(&mut self, now_ms: i64, old_queue_age_ms: i64, at_frame_boundary: bool) {
        if self.ended
            || self.send_on != OLD
            || !self.old_alive
            || self.new_id == NONE
            || (self.switched_off && self.config.switch_off_blocks_attempts)
        {
            return;
        }
        if self.active_since_ms == NOT_ACTIVE
            || now_ms - self.active_since_ms < self.config.active_before_switch_ms
        {
            return;
        }
        if now_ms < self.cooldown_until_ms || self.fallbacks >= self.config.max_fallbacks {
            return;
        }
        if !at_frame_boundary {
            self.stat("switch-waits-for-frame");
            return;
        }
        if self.config.gate_switch_on_old_queue && old_queue_age_ms >= self.config.hold_ms / 2 {
            self.stat("switch-waits-for-old-queue");
            return;
        }
        self.generation += 1;
        self.last_switch_generation = self.generation;
        self.last_switch_boundary = self.last_sealed + 1;
        self.out.push(Action::Write {
            carrier: OLD,
            wire: Wire::Switch {
                generation: self.generation,
                boundary: self.last_switch_boundary,
                carrier: self.marker(self.new_id),
            },
            priority: !self.config.switch_through_media_fifo,
        });
        self.send_on = self.new_id;
        self.out.push(Action::ReassertState);
    }

    /// Sender row 3 and receiver row 4: the old link is gone and a new carrier is ready.
    fn only_new_carrier_left(&mut self) {
        if self.send_on != self.new_id {
            self.generation += 1;
            self.last_switch_generation = self.generation;
            self.last_switch_boundary = self.last_sealed + 1;
            self.out.push(Action::ResetSendFragments);
        }
        self.send_on = self.new_id;
        self.out.push(Action::Write {
            carrier: self.new_id,
            wire: Wire::Switch {
                generation: self.last_switch_generation,
                boundary: self.last_switch_boundary,
                carrier: self.marker(self.new_id),
            },
            priority: false,
        });
        self.out.push(Action::ReassertState);
        if self.accept != self.new_id {
            self.accept = self.new_id;
            self.boundary = self.last_opened + 1;
            self.out.push(Action::SuppressFragments);
            self.out.push(Action::RequestPeerKeyframe);
            self.release_held();
        }
        self.pending_switch_boundary = -1;
    }

    /// Whatever carrier `carrier` was, this phone no longer has it.
    fn carrier_gone(&mut self, carrier: i32, now_ms: i64) {
        // A RESUME may name the attempt this phone still reads from even
        // though it is no longer new_id/pending_id here: then this direction
        // takes the old link again instead of staying fenced to an abandoned carrier.
        if carrier != OLD && carrier == self.accept && carrier != self.new_id && carrier != self.pending_id {
            self.set_dead(carrier);
            self.accept = OLD;
            self.boundary = 0;
            self.out.push(Action::SuppressFragments);
            self.out.push(Action::RequestPeerKeyframe);
            return;
        }
        if self.is_dead(carrier) {
            return;
        }
        let known = carrier == OLD || carrier == self.new_id || carrier == self.pending_id;
        self.set_dead(carrier);
        if !known {
            return;
        }
        if carrier == OLD {
            self.old_alive = false;
            if self.new_id != NONE {
                self.stat("old-died-new-left");
                self.only_new_carrier_left();
            } else if self.pending_id != NONE && self.config.pending_keeps_session {
                self.stat("old-died-waiting-for-pending");
                self.send_on = NONE;
                self.accept = NONE;
                self.held.clear();
                self.held_bytes = 0;
            } else {
                self.end("no carrier left");
            }
        } else if carrier == self.pending_id {
            self.pending_id = NONE;
            self.pending_switch_boundary = -1;
            if self.new_id == NONE {
                self.switch_off_pending = false;
            }
            if !self.old_alive {
                self.end("no carrier left");
            }
        } else {
            self.new_carrier_gone(carrier, now_ms, true, true);
        }
    }

    /// Sender row 2 and receiver row 5: the new carrier failed or was closed.
    fn new_carrier_gone(
        &mut self,
        carrier: i32,
        now_ms: i64,
        count_fallback: bool,
        cooldown: bool,
    ) {
        self.new_id = NONE;
        self.held.clear();
        self.held_bytes = 0;
        self.pending_switch_boundary = -1;
        if !self.old_alive {
            self.end("no carrier left");
            return;
        }
        if self.send_on == carrier {
            self.stat("new-died-fell-back");
            self.send_on = OLD;
            self.generation += 1;
            if count_fallback {
                self.fallbacks += 1;
            }
            let resume = Wire::Resume {
                generation: self.generation,
                next: self.last_sealed + 1,
                carrier: self.marker(carrier),
            };
            self.last_fallback_resume = Some(resume.clone());
            self.out.push(Action::Write {
                carrier: OLD,
                wire: resume,
                priority: false,
            });
            self.out.push(Action::ResetSendFragments);
            self.out.push(Action::ForceLocalKeyframe);
            self.out.push(Action::ReassertState);
        } else if self.config.resume_always_written {
            self.stat("new-died-resume-notice");
            self.generation += 1;
            self.out.push(Action::Write {
                carrier: OLD,
                wire: Wire::Resume {
                    generation: self.generation,
                    next: self.last_sealed + 1,
                    carrier: self.marker(carrier),
                },
                priority: false,
            });
        }
        if cooldown {
            self.cooldown_until_ms = now_ms + self.config.cooldown_ms;
        }
        if self.accept == carrier {
            self.accept = OLD;
            self.boundary = 0;
            self.out.push(Action::SuppressFragments);
            self.out.push(Action::RequestPeerKeyframe);
        }
        if self.pending_id == NONE {
            self.switch_off_pending = false;
        }
    }

    /// A planned release of the new carrier (switch-off or quality rule): the
    /// same effects as its death, then the owner is told to close it.
    fn release_new(
        &mut self,
        reason: CloseReason,
        count_fallback: bool,
        cooldown: bool,
        now_ms: i64,
    ) {
        let carrier = self.new_id;
        if carrier == NONE {
            return;
        }
        self.set_dead(carrier);
        self.new_carrier_gone(carrier, now_ms, count_fallback, cooldown);
        self.out.push(Action::CloseCarrier { carrier, reason });
    }

    // ---------------------------------------------------------------- receiver rows

    fn arrive_content(&mut self, carrier: i32, rec: Rec, now_ms: i64) {
        let wire = Wire::Content(rec.clone());
        if carrier == OLD {
            if self.accept != OLD && self.accept != NONE && rec.seq >= self.boundary {
                self.out.push(Action::Drop {
                    reason: DropReason::Fenced,
                    wire: Some(wire),
                    carrier,
                });
            } else if rec.seq < self.old_from || rec.seq <= self.last_opened {
                self.out.push(Action::Drop {
                    reason: DropReason::Stale,
                    wire: Some(wire),
                    carrier,
                });
            } else {
                self.deliver(rec, carrier);
            }
        } else if carrier == self.new_id {
            if self.accept == carrier {
                if rec.seq < self.boundary || rec.seq <= self.last_opened {
                    self.out.push(Action::Drop {
                        reason: DropReason::Stale,
                        wire: Some(wire),
                        carrier,
                    });
                } else {
                    self.deliver(rec, carrier);
                }
            } else {
                // Early new-carrier record: its SWITCH is still on the old link. Hold it.
                if self.held.is_empty() {
                    self.hold_since_ms = now_ms;
                }
                self.held.push(rec);
                self.held_bytes += wire_bytes(&wire);
                if self.held_bytes > self.config.hold_bytes {
                    self.hold_exhausted(now_ms);
                }
            }
        } else {
            self.out.push(Action::Drop {
                reason: DropReason::UnknownCarrier,
                wire: Some(wire),
                carrier,
            });
        }
    }

    fn arrive_switch(&mut self, carrier: i32, wire: Wire, _now_ms: i64) {
        let Wire::Switch {
            generation,
            boundary,
            carrier: named,
        } = wire.clone()
        else {
            return;
        };
        // F-6: a SWITCH on the new carrier says the sender found ITS old link
        // dead, but it names no instance of that link: delayed past an in-call
        // renewal it describes a standby that has already been replaced. It is
        // counted and ignored; the old link closes only by this phone's own
        // liveness rule or the session end.
        if self.config.old_dead_hint
            && carrier != OLD
            && carrier == self.new_id
            && self.old_alive
            && (named == carrier || named == ANY && !self.config.markers_name_carrier)
        {
            self.stat("old-dead-hint");
        }
        if generation < self.max_generation
            || generation == self.max_generation
                && (generation != self.seen_switch_generation
                    || boundary != self.seen_switch_boundary)
        {
            self.out.push(Action::Drop {
                reason: DropReason::StaleMarker,
                wire: Some(wire),
                carrier,
            });
            self.stat("switch-stale");
            return;
        }
        if generation == self.max_generation {
            self.stat("switch-repeat");
            return;
        }
        self.max_generation = generation;
        self.seen_switch_generation = generation;
        self.seen_switch_boundary = boundary;
        if named == ANY && self.config.markers_name_carrier {
            self.stat("switch-unnamed-burned");
            return;
        }
        let target = if named == ANY {
            if self.new_id != NONE {
                self.new_id
            } else {
                self.pending_id
            }
        } else {
            named
        };
        if target == NONE || target != self.new_id && target != self.pending_id {
            self.stat("switch-burned");
            return;
        }
        if target == self.pending_id {
            self.stat("switch-kept-for-pending");
            if self.config.switch_waits_for_pending_new && carrier == OLD {
                self.pending_switch_boundary = boundary;
                self.pending_switch_carrier = target;
            }
            return;
        }
        if self.accept == self.new_id {
            self.stat("switch-already-accepted");
            return;
        }
        self.accept = self.new_id;
        self.boundary = boundary;
        self.stat(if carrier == self.new_id {
            "switch-accepted-from-new"
        } else {
            "switch-accepted-from-old"
        });
        if carrier == self.new_id {
            self.out.push(Action::SuppressFragments);
            self.out.push(Action::RequestPeerKeyframe);
        }
        self.release_held();
    }

    fn arrive_resume(&mut self, carrier: i32, wire: Wire, now_ms: i64) {
        let Wire::Resume {
            generation,
            next,
            carrier: named,
        } = wire
        else {
            return;
        };
        if generation <= self.max_generation {
            self.out.push(Action::Drop {
                reason: DropReason::StaleMarker,
                wire: None,
                carrier,
            });
            return;
        }
        self.max_generation = generation;
        if named == ANY && self.config.markers_name_carrier {
            self.stat("resume-unnamed-burned");
            return;
        }
        let target = if named == ANY { self.accept } else { named };
        let closes = if self.config.resume_always_written {
            // A RESUME also names the carrier this side still accepts from.
            target != NONE
                && (target == self.new_id
                    || target == self.pending_id
                    || (target == self.accept && self.accept != OLD))
        } else {
            self.accept != OLD && self.accept != NONE && self.accept == target
        };
        if closes {
            self.stat(if self.accept == target {
                "resume-closed-new"
            } else {
                "resume-closed-new-not-accepted"
            });
            if self.config.resume_sets_old_floor {
                self.old_from = self.old_from.max(next);
            }
            self.out.push(Action::CloseCarrier {
                carrier: target,
                reason: CloseReason::PeerResumed,
            });
            self.carrier_gone(target, now_ms);
        } else {
            self.stat("resume-burned");
        }
    }

    /// The hold ceiling (time or bytes) was reached with the `SWITCH` still missing.
    fn hold_exhausted(&mut self, now_ms: i64) {
        self.hold_exhaustions += 1;
        self.stat("hold-exhausted");
        match self.config.hold_expiry {
            HoldExpiry::Promote => {
                self.accept = self.new_id;
                self.boundary = self.last_opened + 1;
                self.out.push(Action::SuppressFragments);
                self.out.push(Action::RequestPeerKeyframe);
                self.release_held();
            }
            HoldExpiry::CloseNew => {
                let carrier = self.new_id;
                for record in &self.held {
                    self.out.push(Action::Drop {
                        reason: DropReason::Stale,
                        wire: Some(Wire::Content(record.clone())),
                        carrier,
                    });
                }
                self.held.clear();
                self.held_bytes = 0;
                self.out.push(Action::CloseCarrier {
                    carrier,
                    reason: CloseReason::HoldExpired,
                });
                self.carrier_gone(carrier, now_ms);
            }
        }
    }

    fn release_held(&mut self) {
        let from = self.new_id;
        for record in std::mem::take(&mut self.held) {
            if record.seq > self.last_opened && record.seq >= self.boundary {
                self.deliver(record, from);
            } else {
                self.out.push(Action::Drop {
                    reason: DropReason::Stale,
                    wire: Some(Wire::Content(record)),
                    carrier: from,
                });
            }
        }
        self.held_bytes = 0;
    }

    fn deliver(&mut self, rec: Rec, carrier: i32) {
        self.last_opened = rec.seq;
        self.out.push(Action::Deliver { rec, carrier });
    }

    fn end(&mut self, reason: &'static str) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.out.push(Action::EndSession { reason });
    }

    fn marker(&self, carrier: i32) -> i32 {
        if self.config.markers_name_carrier {
            carrier
        } else {
            ANY
        }
    }

    fn is_dead(&self, carrier: i32) -> bool {
        carrier >= 0 && carrier < 64 && (self.dead_mask >> carrier) & 1 == 1
    }

    fn set_dead(&mut self, carrier: i32) {
        if carrier >= 0 && carrier < 64 {
            self.dead_mask |= 1u64 << carrier;
        }
    }

    fn drain(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.out)
    }
}

fn wire_bytes(wire: &Wire) -> i64 {
    match wire {
        Wire::Content(rec) => rec.bytes,
        Wire::Switch { .. } | Wire::Resume { .. } => 0,
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod sim;
