//! Hop-local link control inside BMS1 type 6 (Android `SecureLinkControlProtocol`):
//! version 1 keepalive, version 2 capacity report (6 bytes) and version 3
//! delivery report (14 bytes, forward compatible up to 64). Heartbeats every
//! 2 s; a link silent for 8 s is dead. Reports describe only this one link and
//! are never forwarded (hop-local rule).

pub const HEARTBEAT_INTERVAL_NS: u64 = 2_000_000_000;
pub const HEARTBEAT_TIMEOUT_NS: u64 = 8_000_000_000;
const FLAG_LINK_LATE: u8 = 1;
const FLAG_DELIVERY_ONLY: u8 = 2;
const MAX_REPORT_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub received_bytes: u32,
    pub clock_ms: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// What this receiver measured from its parent; 0 = not measured yet.
    pub arrival_kbps: u16,
    /// The receiver's playout delay is pinned at its maximum.
    pub link_late: bool,
    pub added_delay_ms: u16,
    pub delivery: Option<Delivery>,
    pub delivery_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid;

/// Version 1 keepalive.
pub fn keepalive() -> [u8; 1] {
    [1]
}
/// Version 2 without delivery, version 3 with it.
pub fn encode(report: &Report) -> Vec<u8> {
    let mut flags = if report.link_late { FLAG_LINK_LATE } else { 0 };
    if report.delivery.is_some() && report.delivery_only {
        flags |= FLAG_DELIVERY_ONLY;
    }
    let mut out = vec![if report.delivery.is_some() { 3 } else { 2 }];
    out.extend_from_slice(&report.arrival_kbps.to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&report.added_delay_ms.to_be_bytes());
    if let Some(d) = report.delivery {
        out.extend_from_slice(&d.received_bytes.to_be_bytes());
        out.extend_from_slice(&d.clock_ms.to_be_bytes());
    }
    out
}
/// `Ok(None)` for a keepalive. Versions 1 and 2 are exact; version 3 and later
/// read the first 14 bytes and ignore later additions.
pub fn decode(payload: &[u8]) -> Result<Option<Report>, Invalid> {
    let base = |p: &[u8], flags: u8, delivery| Report {
        arrival_kbps: u16::from_be_bytes([p[1], p[2]]),
        link_late: flags & FLAG_LINK_LATE != 0,
        added_delay_ms: u16::from_be_bytes([p[4], p[5]]),
        delivery,
        delivery_only: delivery.is_some() && flags & FLAG_DELIVERY_ONLY != 0,
    };
    match payload {
        [1] => Ok(None),
        [2, _, _, flags, _, _] => {
            if flags & !FLAG_LINK_LATE != 0 {
                return Err(Invalid);
            }
            Ok(Some(base(payload, *flags, None)))
        }
        [v, ..] if *v >= 3 => {
            if !(14..=MAX_REPORT_BYTES).contains(&payload.len()) {
                return Err(Invalid);
            }
            let delivery = Delivery {
                received_bytes: u32::from_be_bytes(payload[6..10].try_into().unwrap()),
                clock_ms: u32::from_be_bytes(payload[10..14].try_into().unwrap()),
            };
            Ok(Some(base(payload, payload[3], Some(delivery))))
        }
        _ => Err(Invalid),
    }
}

/// When to send the next heartbeat and when the link is dead.
#[derive(Clone, Copy, Debug)]
pub struct Liveness {
    last_received_ns: u64,
    next_send_ns: u64,
}
impl Liveness {
    pub fn new(now_ns: u64) -> Self {
        Self {
            last_received_ns: now_ns,
            next_send_ns: now_ns,
        }
    }
    /// Any authenticated record from the peer proves the link alive.
    pub fn on_received(&mut self, now_ns: u64) {
        self.last_received_ns = self.last_received_ns.max(now_ns);
    }
    pub fn heartbeat_due(&mut self, now_ns: u64) -> bool {
        if now_ns < self.next_send_ns {
            return false;
        }
        self.next_send_ns = now_ns + HEARTBEAT_INTERVAL_NS;
        true
    }
    /// A report heartbeat went out: the next keepalive is due a full interval later.
    pub fn sent(&mut self, now_ns: u64) {
        self.next_send_ns = now_ns + HEARTBEAT_INTERVAL_NS;
    }
    pub fn dead(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.last_received_ns) >= HEARTBEAT_TIMEOUT_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_versions_round_trip_exactly_and_unknown_forms_close_the_link() {
        assert_eq!(decode(&keepalive()), Ok(None));
        let v2 = Report {
            arrival_kbps: 812,
            link_late: true,
            added_delay_ms: 350,
            delivery: None,
            delivery_only: false,
        };
        assert_eq!(encode(&v2), vec![2, 0x03, 0x2c, 1, 0x01, 0x5e]);
        assert_eq!(decode(&encode(&v2)), Ok(Some(v2)));
        let v3 = Report {
            delivery: Some(Delivery {
                received_bytes: 0x0102_0304,
                clock_ms: 0x0a0b_0c0d,
            }),
            delivery_only: true,
            ..v2
        };
        let raw = encode(&v3);
        assert_eq!(raw.len(), 14);
        assert_eq!(raw[3], 3);
        assert_eq!(decode(&raw), Ok(Some(v3)));
        // A later version's additions are ignored, not fatal.
        let mut v4 = raw.clone();
        v4[0] = 4;
        v4[3] |= 0xf0;
        v4.extend([9; 10]);
        assert_eq!(decode(&v4).unwrap().unwrap().delivery, v3.delivery);
        for bad in [
            vec![],
            vec![0],
            vec![1, 0],
            vec![2, 0, 0, 2, 0, 0],
            vec![2, 0, 0, 0, 0],
            raw[..13].to_vec(),
            [raw.clone(), vec![0; 51]].concat(),
        ] {
            assert_eq!(decode(&bad), Err(Invalid), "{bad:?}");
        }
    }
    #[test]
    fn heartbeats_go_every_two_seconds_and_eight_silent_seconds_end_the_link() {
        let s = 1_000_000_000;
        let mut l = Liveness::new(0);
        assert!(l.heartbeat_due(0));
        assert!(!l.heartbeat_due(s));
        assert!(l.heartbeat_due(2 * s));
        assert!(!l.dead(7 * s));
        assert!(l.dead(8 * s));
        l.on_received(7 * s);
        assert!(!l.dead(14 * s));
        l.on_received(1);
        assert!(l.dead(15 * s));
    }
}
