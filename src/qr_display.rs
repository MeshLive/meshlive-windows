//! The displaying side of a BVMID2 code (Android `QrCodeDisplay` and
//! `QrConnectionGate`, PROTOCOL "Displayer and single use"): one live code at
//! a time, 300 s of monotonic time, ended by leaving the screen or by the
//! role-0 RDI changing; the link handshake whose tag matches the code
//! RESERVES it for that connection (an attempt counted against 3, the signing
//! budget spent), the same connection's end-to-end handshake continues under
//! the reservation, a completed end-to-end handshake COMMITS it (consumed), a
//! failure releases it, an unfinished reservation expires after 45 s. Every
//! distinct first message is pinned while the code lives, so a replayed msg1
//! closes silently and only a genuinely new attempt on a consumed code
//! raises the reuse alert.
use crate::{bmh3::Purpose, bvmid2::Code, crypto, rotating};
use std::collections::HashSet;
use zeroize::Zeroizing;

pub const LIFETIME_MS: u64 = 300_000;
pub const RESERVATION_MS: u64 = 45_000;
pub const MAX_ATTEMPTS: u32 = 3;
pub const MAX_SEEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Live,
    Reserved,
    Consumed,
}

/// Why a tag-matched first message is refused (Android's `Bmh3Failure` reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Expired,
    Replay,
    Attempts,
    Consumed,
    Busy,
    EndToEndWithoutLink,
    Budget,
}
impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Expired => "qr expired",
            Self::Replay => "replay",
            Self::Attempts => "qr attempts",
            Self::Consumed => "qr consumed",
            Self::Busy => "qr busy",
            Self::EndToEndWithoutLink => "qr end-to-end without link",
            Self::Budget => "budget",
        }
    }
}

/// What happened to the code on this screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ScanCompleted([u8; 32]),
    ReuseAlert,
    Ended(&'static str),
}

/// One incoming connection's view (its link and end-to-end handshakes share it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gate(u64);

struct Live {
    code: Code,
    created_ms: u64,
    phase: Phase,
    owner: Option<Gate>,
    reserved_at_ms: u64,
    attempts: u32,
    seen: HashSet<[u8; 32]>,
    tag_key: Zeroizing<[u8; 32]>,
    binding: [u8; 11],
}

pub struct Display {
    clock_ms: Box<dyn FnMut() -> u64>,
    /// Spends one unit of the global signing budget; false when exhausted.
    spend: Box<dyn FnMut() -> bool>,
    current: Option<Live>,
    next_gate: u64,
    link_reserved: HashSet<Gate>,
    committed: HashSet<Gate>,
    pub events: Vec<Event>,
}
impl Display {
    pub fn new(clock_ms: Box<dyn FnMut() -> u64>, spend: Box<dyn FnMut() -> bool>) -> Self {
        Self {
            clock_ms,
            spend,
            current: None,
            next_gate: 1,
            link_reserved: HashSet::new(),
            committed: HashSet::new(),
            events: Vec::new(),
        }
    }
    fn now(&mut self) -> u64 {
        (self.clock_ms)()
    }
    fn end(&mut self, reason: &'static str) {
        if self.current.take().is_some() {
            self.events.push(Event::Ended(reason));
        }
    }
    fn expire_if_due(&mut self) {
        let now = self.now();
        if self.current.as_ref().is_some_and(|c| now.saturating_sub(c.created_ms) >= LIFETIME_MS) {
            self.end("expired");
        }
    }
    /// A new live code for this PC's fingerprint on its current role-0 RDI
    /// (any earlier code ends). `k_qr` comes from the CSPRNG (never zero).
    pub fn show(&mut self, fingerprint: [u8; 32], rdi: [u8; 6], k_qr: [u8; 32]) -> Result<String, rotating::Error> {
        self.end("replaced");
        let code = Code { fingerprint, k_qr: Zeroizing::new(k_qr), rdi };
        let tag_key = code.tag_key()?;
        let binding = rotating::advert_binding(&rdi, rotating::ROLE_CALL_OFFER, None);
        let text = code.encode();
        let created_ms = self.now();
        self.current = Some(Live {
            code,
            created_ms,
            phase: Phase::Live,
            owner: None,
            reserved_at_ms: 0,
            attempts: 0,
            seen: HashSet::new(),
            tag_key,
            binding,
        });
        Ok(text)
    }
    /// A fresh K_qr from the CSPRNG (never all zero).
    pub fn fresh_secret() -> [u8; 32] {
        loop {
            let mut k = [0u8; 32];
            k.copy_from_slice(&crypto::random_nonce().expect("CSPRNG")[..32]);
            if k.iter().any(|b| *b != 0) {
                return k;
            }
        }
    }
    pub fn leave(&mut self) {
        self.end("left");
    }
    pub fn on_rdi_changed(&mut self, rdi: &[u8; 6]) {
        if self.current.as_ref().is_some_and(|c| &c.code.rdi != rdi) {
            self.end("id_changed");
        }
    }
    pub fn has_code(&mut self) -> bool {
        self.expire_if_due();
        self.current.is_some()
    }
    pub fn phase(&mut self) -> Option<Phase> {
        self.expire_if_due();
        self.current.as_ref().map(|c| c.phase)
    }
    pub fn new_gate(&mut self) -> Gate {
        let gate = Gate(self.next_gate);
        self.next_gate += 1;
        gate
    }
    /// The tag check the responder adds for `gate`'s connection: (K_tag,
    /// binding), or None without a code.
    pub fn tag_check(&mut self) -> Option<([u8; 32], [u8; 11])> {
        self.expire_if_due();
        self.current.as_ref().map(|c| (*c.tag_key, c.binding))
    }
    /// The gate at msg1 of a tag-matched handshake, before any key is drawn.
    pub fn on_tag_match(&mut self, gate: Gate, purpose: Purpose, ephemeral: &[u8]) -> Result<(), Refusal> {
        self.expire_if_due();
        let now = self.now();
        let key = crypto::sha256(ephemeral).map_err(|_| Refusal::Replay)?;
        let link_reserved = self.link_reserved.contains(&gate);
        let Some(code) = self.current.as_mut() else { return Err(Refusal::Expired) };
        if !code.seen.insert(key) {
            return Err(Refusal::Replay);
        }
        if code.seen.len() > MAX_SEEN {
            return Err(Refusal::Attempts);
        }
        match code.phase {
            Phase::Consumed => {
                self.events.push(Event::ReuseAlert);
                return Err(Refusal::Consumed);
            }
            Phase::Reserved => {
                if code.owner == Some(gate) && purpose == Purpose::Endpoint && link_reserved {
                    return Ok(());
                }
                if now.saturating_sub(code.reserved_at_ms) < RESERVATION_MS {
                    return Err(Refusal::Busy);
                }
                code.owner = None;
                code.phase = Phase::Live;
            }
            Phase::Live => {}
        }
        if purpose != Purpose::Link {
            return Err(Refusal::EndToEndWithoutLink);
        }
        if code.attempts >= MAX_ATTEMPTS {
            self.end("attempt_cap");
            return Err(Refusal::Attempts);
        }
        if !(self.spend)() {
            return Err(Refusal::Budget);
        }
        let code = self.current.as_mut().unwrap();
        code.attempts += 1;
        code.phase = Phase::Reserved;
        code.owner = Some(gate);
        code.reserved_at_ms = now;
        self.link_reserved.insert(gate);
        Ok(())
    }
    /// `gate`'s end-to-end handshake completed with `peer`: commit (consumed).
    pub fn complete(&mut self, gate: Gate, peer: [u8; 32]) -> bool {
        if self.committed.contains(&gate) {
            return false;
        }
        let Some(code) = self.current.as_mut() else { return false };
        if code.phase != Phase::Reserved || code.owner != Some(gate) {
            return false;
        }
        code.phase = Phase::Consumed;
        code.owner = None;
        self.committed.insert(gate);
        self.events.push(Event::ScanCompleted(peer));
        true
    }
    /// `gate`'s connection failed or ended before commit: released (attempt counted).
    pub fn fail(&mut self, gate: Gate) {
        if self.committed.contains(&gate) {
            return;
        }
        if let Some(code) = self.current.as_mut() {
            if code.phase == Phase::Reserved && code.owner == Some(gate) {
                code.phase = Phase::Live;
                code.owner = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Android Bvmid2Test, display part, and shared vector bvmid2-015.
    use super::*;
    use std::{cell::Cell, rc::Rc};

    const FP: [u8; 32] = [0xab; 32];
    const RDI: [u8; 6] = [1, 2, 3, 4, 5, 6];
    fn rig() -> (Display, Rc<Cell<u64>>, Rc<Cell<u32>>) {
        let now = Rc::new(Cell::new(1_000));
        let budget = Rc::new(Cell::new(u32::MAX));
        let (n, b) = (now.clone(), budget.clone());
        let display = Display::new(
            Box::new(move || n.get()),
            Box::new(move || {
                if b.get() == 0 {
                    return false;
                }
                b.set(b.get() - 1);
                true
            }),
        );
        (display, now, budget)
    }
    fn eph(n: u8) -> [u8; 65] {
        let mut e = [0u8; 65];
        for (i, b) in e.iter_mut().enumerate() {
            *b = n.wrapping_add(i as u8);
        }
        e
    }

    #[test]
    fn a_code_lives_three_hundred_seconds_and_one_code_is_live_at_a_time() {
        let (mut d, now, _) = rig();
        let first = d.show(FP, RDI, [7; 32]).unwrap();
        now.set(now.get() + LIFETIME_MS - 1);
        assert!(d.has_code());
        now.set(now.get() + 1);
        assert!(!d.has_code());
        let second = d.show(FP, RDI, [8; 32]).unwrap();
        assert_ne!(first, second);
        d.show(FP, RDI, [9; 32]).unwrap();
        d.on_rdi_changed(&RDI);
        assert!(d.has_code());
        d.on_rdi_changed(&[9; 6]);
        assert!(!d.has_code());
        d.show(FP, RDI, [9; 32]).unwrap();
        d.leave();
        assert_eq!(
            d.events,
            vec![Event::Ended("expired"), Event::Ended("replaced"), Event::Ended("id_changed"), Event::Ended("left")]
        );
    }

    #[test]
    fn the_reservation_sequence_matches_bvmid2_015() {
        let (mut d, _, _) = rig();
        d.show(FP, RDI, [7; 32]).unwrap();
        let a = d.new_gate();
        assert_eq!(d.on_tag_match(a, Purpose::Endpoint, &eph(1)), Err(Refusal::EndToEndWithoutLink));
        assert_eq!(d.on_tag_match(a, Purpose::Link, &eph(2)), Ok(()));
        assert_eq!(d.phase(), Some(Phase::Reserved));
        assert_eq!(d.on_tag_match(a, Purpose::Endpoint, &eph(3)), Ok(()));
        let b = d.new_gate();
        assert_eq!(d.on_tag_match(b, Purpose::Link, &eph(4)).map_err(Refusal::reason), Err("qr busy"));
        assert!(d.complete(a, FP));
        assert_eq!(d.phase(), Some(Phase::Consumed));
        let replay = d.new_gate();
        assert_eq!(d.on_tag_match(replay, Purpose::Link, &eph(2)).map_err(Refusal::reason), Err("replay"));
        assert!(!d.events.contains(&Event::ReuseAlert));
        let other = d.new_gate();
        assert_eq!(d.on_tag_match(other, Purpose::Link, &eph(9)).map_err(Refusal::reason), Err("qr consumed"));
        assert_eq!(d.events.iter().filter(|e| **e == Event::ReuseAlert).count(), 1);
        assert!(d.events.contains(&Event::ScanCompleted(FP)));
    }

    #[test]
    fn failures_count_against_the_cap_reservations_time_out_and_the_budget_applies() {
        let (mut d, now, budget) = rig();
        d.show(FP, RDI, [7; 32]).unwrap();
        for i in 0..MAX_ATTEMPTS as u8 {
            let g = d.new_gate();
            d.on_tag_match(g, Purpose::Link, &eph(10 + i)).unwrap();
            d.fail(g);
            assert_eq!(d.phase(), Some(Phase::Live));
        }
        let last = d.new_gate();
        assert_eq!(d.on_tag_match(last, Purpose::Link, &eph(99)), Err(Refusal::Attempts));
        assert!(!d.has_code());
        // Timeout.
        d.show(FP, RDI, [8; 32]).unwrap();
        let a = d.new_gate();
        d.on_tag_match(a, Purpose::Link, &eph(1)).unwrap();
        now.set(now.get() + RESERVATION_MS - 1);
        let b = d.new_gate();
        assert_eq!(d.on_tag_match(b, Purpose::Link, &eph(2)), Err(Refusal::Busy));
        now.set(now.get() + 1);
        let c = d.new_gate();
        d.on_tag_match(c, Purpose::Link, &eph(3)).unwrap();
        assert!(!d.complete(a, FP), "the first attempt can no longer commit");
        assert!(d.complete(c, FP));
        // Pinned until the code ends: a late replay is still silent.
        now.set(now.get() + 100_000);
        let late = d.new_gate();
        assert_eq!(d.on_tag_match(late, Purpose::Link, &eph(3)), Err(Refusal::Replay));
        // Budget.
        d.show(FP, RDI, [9; 32]).unwrap();
        budget.set(1);
        let x = d.new_gate();
        d.on_tag_match(x, Purpose::Link, &eph(1)).unwrap();
        d.fail(x);
        let y = d.new_gate();
        assert_eq!(d.on_tag_match(y, Purpose::Link, &eph(2)).map_err(Refusal::reason), Err("budget"));
    }
}
