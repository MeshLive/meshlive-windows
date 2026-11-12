//! W4G: the call glue of the guest role (the phone hosts the Wi-Fi Direct group, this PC
//! joins). `dial.rs` forwards here while the guest role is on; its host-role glue is
//! unchanged. The cutover, the carrier attach, the Wi-Fi socket service and the UI label
//! are the existing ones.

use super::*;
use crate::wifi::{
    attempt,
    guest::{GuestFacts, GuestOwner},
    log, roles,
    wlan_guest::WlanGuest,
};

impl Dialed {
    /// `wifi_arm` with the guest role on: creates the owner only for a call that negotiated
    /// `BWF1` and may attempt (not a relay edge, Wi-Fi not ended).
    pub(super) fn guest_arm(&mut self, armed: bool) {
        self.wifi.armed = armed;
        if armed && self.wifi.guest.is_none() && self.session.wifi_attempts_allowed() {
            self.wifi.guest = Some(Box::new(GuestOwner::new(Box::new(WlanGuest::new()))));
        }
    }

    /// Test seam: the guest owner over a given platform, armed.
    #[cfg(test)]
    pub(super) fn guest_arm_with(&mut self, platform: Box<dyn crate::wifi::guest_platform::GuestPlatform>) {
        self.wifi.armed = true;
        self.wifi.guest = Some(Box::new(GuestOwner::new(platform)));
    }

    /// The switch changed mid-call (guest role).
    pub(super) fn guest_set_armed(&mut self, armed: bool, trace: &mut dyn FnMut(&str, &str)) {
        self.wifi.armed = armed;
        let now_ns = self.now_ns();
        let now_ms = (now_ns / 1_000_000) as i64;
        // Note: an owner that went Off (a refused attempt, or the switch off during a live
        // carrier) stays off for the rest of the call; arming again creates no new owner.
        if armed {
            if self.wifi.guest.is_none()
                && !self.session.wifi_switched_off()
                && self.session.wifi_attempts_allowed()
            {
                self.wifi.guest = Some(Box::new(GuestOwner::new(Box::new(WlanGuest::new()))));
            }
            return;
        }
        let was_live = self
            .wifi
            .guest
            .as_ref()
            .is_some_and(|guest| guest.state_name() == "live");
        let mut out = Vec::new();
        if let Some(guest) = self.wifi.guest.as_mut() {
            guest.on_switch_off(now_ms, &mut out);
        }
        self.guest_apply(out, trace);
        if was_live {
            self.session.wifi_switch_off(now_ns);
        }
    }

    fn guest_facts(&self, now_ms: i64) -> GuestFacts {
        GuestFacts {
            now_ms,
            armed: self.wifi.armed,
            caps_exchanged: self.session.wifi_caps_exchanged(),
            local_caps: self.session.wifi_local_caps(),
            peer_caps: self.session.wifi_peer_caps(),
            local_is_initiator: self.session.wifi_initiator(),
            carrier_up: self.wifi.link.is_some() || self.session.renewal_running(),
        }
    }

    /// Per worker tick (guest role): outputs, carrier ends, the single CAPS, the owner.
    pub(super) fn guest_tick(&mut self, trace: &mut dyn FnMut(&str, &str)) -> Option<&'static str> {
        // WP6NF finding 6 holds for the guest too: a call with a blocked person keeps no Wi-Fi.
        if self.session.peer_blocked() {
            self.wifi_release();
            return None;
        }
        let pending = std::mem::take(&mut self.wifi.pending_out);
        self.guest_apply(pending, trace);
        let now_ms = (self.now_ns() / 1_000_000) as i64;
        let ended = std::mem::take(&mut self.wifi.ended);
        if let Some(guest) = self.wifi.guest.as_mut() {
            for id in ended {
                guest.on_carrier_lost(id, now_ms);
            }
        }
        if !self.session.wifi_caps_sent() {
            let armed = self.wifi.armed;
            let possible = armed
                && self
                    .wifi
                    .guest
                    .as_mut()
                    .is_some_and(|guest| guest.caps_possible());
            self.session.wifi_set_local_caps(roles::windows_guest_caps(possible));
        }
        let facts = self.guest_facts(now_ms);
        let mut out = Vec::new();
        if let Some(guest) = self.wifi.guest.as_mut() {
            guest.tick(&facts, self.session.wifi_uw(), &mut out);
        }
        self.guest_apply(out, trace);
        self.wifi_transport()
    }

    /// The session's Wi-Fi notices for the guest owner; outputs are applied at the next tick.
    pub(super) fn guest_dispatch(&mut self, notices: &[crate::call_session::Notice]) {
        if self.wifi.guest.is_none() {
            return;
        }
        let now_ms = (self.now_ns() / 1_000_000) as i64;
        let facts = self.guest_facts(now_ms);
        let mut out = Vec::new();
        for notice in notices {
            let crate::call_session::Notice::Wifi(notice) = notice else {
                continue;
            };
            let Some(guest) = self.wifi.guest.as_mut() else {
                break;
            };
            match notice {
                crate::call_session::WifiNotice::PeerHost(host) => {
                    guest.on_peer_host(*host, self.session.wifi_uw(), &facts, &mut out)
                }
                crate::call_session::WifiNotice::PeerCreds(creds) => {
                    guest.on_peer_creds(*creds, self.session.wifi_uw(), now_ms, &mut out)
                }
                crate::call_session::WifiNotice::PeerDecline { ticket, reason } => {
                    guest.on_peer_decline(*ticket, *reason, now_ms, &mut out)
                }
                crate::call_session::WifiNotice::WifiEnded { reason } => {
                    guest.on_wifi_ended(now_ms, &mut out);
                    out.push(attempt::Output::Trace {
                        phase: "wifi_upgrade",
                        detail: format!("event=ended | role=guest | reason={reason}"),
                    });
                }
                // The session already traces `close_ignored` (W2F).
                crate::call_session::WifiNotice::PeerClose { .. } => {}
                crate::call_session::WifiNotice::PeerCaps(caps) => out.push(attempt::Output::Trace {
                    phase: "wifi_caps",
                    detail: log::detail(&[
                        ("dir", "recv"),
                        ("bearers", &format!("0x{:02x}", caps.bearers)),
                        ("freeHost", &caps.free_host.to_string()),
                        ("freeGuest", &caps.free_guest.to_string()),
                    ]),
                }),
                crate::call_session::WifiNotice::CarrierChanged { send_on } => {
                    let (effective, reason) = if *send_on == 0 {
                        ("bluetooth", "fallback")
                    } else {
                        ("direct", "switch")
                    };
                    out.push(attempt::Output::Trace {
                        phase: "carrier",
                        detail: format!("hop=call | requested=direct | effective={effective} | reason={reason}"),
                    });
                }
            }
        }
        self.wifi.pending_out.extend(out);
    }

    /// Applies guest outputs in order; the attach runs after the queue (as the host glue does).
    fn guest_apply(&mut self, outputs: Vec<attempt::Output>, trace: &mut dyn FnMut(&str, &str)) {
        let mut queue: std::collections::VecDeque<attempt::Output> = outputs.into();
        let mut attach: Option<(Box<crate::wifi::binding::Bound>, i32)> = None;
        while let Some(output) = queue.pop_front() {
            match output {
                attempt::Output::Send(record) => {
                    if self.session.wifi_send(record).is_err() {
                        trace("wifi_upgrade", "event=send_failed | role=guest | reason=state");
                    }
                }
                attempt::Output::Begin { ticket } => {
                    let now_ns = self.now_ns();
                    let id = self.session.wifi_begin_attempt(ticket, now_ns);
                    let mut extra = Vec::new();
                    if let Some(guest) = self.wifi.guest.as_mut() {
                        guest.set_attempt_id(id, (now_ns / 1_000_000) as i64, &mut extra);
                    }
                    queue.extend(extra);
                }
                attempt::Output::Attach { bound, id, .. } => attach = Some((bound, id)),
                attempt::Output::Died { id } => {
                    let now_ns = self.now_ns();
                    self.session.wifi_carrier_died(id, now_ns);
                }
                attempt::Output::Trace { phase, detail } => trace(phase, &detail),
            }
        }
        if let Some((bound, id)) = attach {
            let bound = *bound;
            self.wifi.buffer = bound.leftover.clone();
            self.attach_wifi(bound.socket, bound.hop_out, bound.hop_in, id, bound.peer_ready);
        }
    }

    /// Leaves the group and drops the guest owner (hang-up, call lost, Dialed dropped).
    pub(super) fn guest_release(&mut self) {
        if let Some(mut guest) = self.wifi.guest.take() {
            guest.release();
        }
    }
}
