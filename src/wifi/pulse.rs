//! Per-carrier liveness for the Wi-Fi carrier.

use super::limits::{CARRIER_DEAD_SILENCE_MS, HEARTBEAT_IDLE_MS};

const HEARTBEAT_IDLE_NS: u64 = HEARTBEAT_IDLE_MS as u64 * 1_000_000;
const CARRIER_DEAD_SILENCE_NS: u64 = CARRIER_DEAD_SILENCE_MS as u64 * 1_000_000;

/// Liveness of one Wi-Fi carrier: a heartbeat every 500 ms when idle, dead after
/// 2.5 s without a complete authenticated record. Times are the session clock in
/// nanoseconds.
#[derive(Clone, Copy, Debug)]
pub struct Pulse {
    received_ns: u64,
    sent_ns: u64,
}
impl Pulse {
    pub fn new(now_ns: u64) -> Self {
        Self {
            received_ns: now_ns,
            sent_ns: now_ns,
        }
    }
    /// Every complete authenticated record (heartbeat or content).
    pub fn on_received(&mut self, now_ns: u64) {
        self.received_ns = self.received_ns.max(now_ns);
    }
    /// Every record written on this carrier.
    pub fn sent(&mut self, now_ns: u64) {
        self.sent_ns = self.sent_ns.max(now_ns);
    }
    pub fn heartbeat_due(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.sent_ns) >= HEARTBEAT_IDLE_NS
    }
    pub fn dead(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.received_ns) >= CARRIER_DEAD_SILENCE_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> u64 {
        value * 1_000_000
    }

    #[test]
    fn a_carrier_is_alive_until_2_5_seconds_of_silence() {
        let pulse = Pulse::new(0);
        assert!(!pulse.dead(ms(2_499)));
        assert!(pulse.dead(ms(2_500)));
    }

    #[test]
    fn a_received_record_restarts_the_silence_clock() {
        let mut pulse = Pulse::new(0);
        pulse.on_received(ms(2_000));
        assert!(!pulse.dead(ms(4_499)));
        assert!(pulse.dead(ms(4_500)));
    }

    #[test]
    fn an_idle_carrier_wants_a_heartbeat_every_500_ms_and_a_write_resets_that() {
        let mut pulse = Pulse::new(0);
        assert!(!pulse.heartbeat_due(ms(499)));
        assert!(pulse.heartbeat_due(ms(500)));
        pulse.sent(ms(500));
        assert!(!pulse.heartbeat_due(ms(999)));
        assert!(pulse.heartbeat_due(ms(1_000)));
    }
}
