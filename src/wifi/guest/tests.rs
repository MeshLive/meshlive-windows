//! W4G guest owner: behaviour through `GuestOwner` with the fake platform.

use super::*;
use crate::wifi::{
    address::{AdapterInfo, AddressInfo},
    guest_platform::fake::{FakeGuestPlatform, FakeState},
    probe::{Band, WifiCaps, WifiRadio},
    wire::caps_bit,
};

const UW: [u8; 32] = [7; 32];
const TICKET: [u8; 8] = [0x5a, 1, 2, 3, 4, 5, 6, 7];
const PHONE: [u8; 4] = [192, 168, 49, 1];
const T0: i64 = 10_000;

fn facts(now_ms: i64) -> GuestFacts {
    GuestFacts {
        now_ms,
        armed: true,
        caps_exchanged: true,
        local_caps: roles::windows_guest_caps(true),
        peer_caps: Some(wire::Caps {
            bearers: caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT,
            free_host: 0,
            free_guest: 0,
        }),
        local_is_initiator: true,
        carrier_up: false,
    }
}

fn host(ticket: [u8; 8]) -> wire::Host {
    wire::Host {
        status: 1,
        bearer: bearer::DIRECT,
        ticket,
        valid_ms: 20_000,
        freq_mhz: 5_745,
    }
}

fn creds(ticket: [u8; 8], ipv4: [u8; 4]) -> wire::Creds {
    wire::Creds {
        ticket,
        port: 47_000,
        ipv4,
    }
}

fn owner_with(fake: FakeGuestPlatform) -> (GuestOwner, Arc<Mutex<FakeState>>, Arc<Mutex<Faults>>) {
    let state = fake.state.clone();
    let faults = Arc::new(Mutex::new(Faults::new()));
    (GuestOwner::with_faults(Box::new(fake), faults.clone()), state, faults)
}

fn owner() -> (GuestOwner, Arc<Mutex<FakeState>>, Arc<Mutex<Faults>>) {
    owner_with(FakeGuestPlatform::ready())
}

fn declines(out: &[Output]) -> Vec<u8> {
    out.iter()
        .filter_map(|output| match output {
            Output::Send(wire::Bwf1::Decline(decline)) => Some(decline.reason),
            _ => None,
        })
        .collect()
}

fn tick(owner: &mut GuestOwner, now_ms: i64) -> Vec<Output> {
    let mut out = Vec::new();
    owner.tick(&facts(now_ms), Some(&UW), &mut out);
    out
}

/// HOST and its CREDS at `now_ms`.
fn offer(owner: &mut GuestOwner, ticket: [u8; 8], now_ms: i64) -> Vec<Output> {
    let mut out = Vec::new();
    owner.on_peer_host(host(ticket), Some(&UW), &facts(now_ms), &mut out);
    owner.on_peer_creds(creds(ticket, PHONE), Some(&UW), now_ms, &mut out);
    out
}

fn verdict(uw: bool, change: impl FnOnce(&mut GuestFacts, &mut wire::Host, &mut FakeState)) -> (Vec<u8>, &'static str) {
    let fake = FakeGuestPlatform::ready();
    let mut f = facts(T0);
    let mut h = host(TICKET);
    change(&mut f, &mut h, &mut *fake.state.lock().unwrap());
    let (mut owner, state, _faults) = owner_with(fake);
    let mut out = Vec::new();
    owner.on_peer_host(h, if uw { Some(&UW) } else { None }, &f, &mut out);
    assert!(state.lock().unwrap().joins.is_empty(), "a HOST alone never joins");
    (declines(&out), owner.state_name())
}

#[test]
fn a_host_is_declined_in_androids_order_and_nothing_starts() {
    let wifi = |radio, hotspot, band| Some(WifiCaps { radio, hotspot, band });
    assert_eq!(verdict(true, |_, _, _| {}), (vec![], "creds"));
    assert_eq!(verdict(false, |_, _, _| {}), (vec![decline::NOT_ELIGIBLE], "idle"));
    assert_eq!(
        verdict(true, |f, _, _| f.local_caps = roles::windows_caps(true)),
        (vec![decline::BUSY], "idle")
    );
    assert_eq!(verdict(true, |f, _, _| f.peer_caps = None), (vec![decline::BUSY], "idle"));
    assert_eq!(
        verdict(true, |_, h, _| {
            h.status = 0;
            h.valid_ms = 0;
            h.freq_mhz = 0;
        }),
        (vec![], "idle")
    );
    assert_eq!(verdict(true, |_, h, _| h.bearer = bearer::AWARE), (vec![decline::NOT_ELIGIBLE], "idle"));
    assert_eq!(verdict(true, |_, h, _| h.ticket = [0; 8]), (vec![decline::NOT_ELIGIBLE], "idle"));
    assert_eq!(verdict(true, |f, _, _| f.carrier_up = true), (vec![decline::BUSY], "idle"));
    assert_eq!(verdict(true, |f, _, _| f.armed = false), (vec![decline::NOT_ELIGIBLE], "idle"));
    assert_eq!(verdict(true, |_, _, s| s.probe = None), (vec![decline::WIFI_UNAVAILABLE], "idle"));
    assert_eq!(
        verdict(true, |_, _, s| s.probe = wifi(WifiRadio::Off, Some(false), Band::Ghz5Plus)),
        (vec![decline::WIFI_UNAVAILABLE], "idle")
    );
    assert_eq!(
        verdict(true, |_, _, s| s.probe = wifi(WifiRadio::On, Some(true), Band::Ghz5Plus)),
        (vec![decline::BUSY], "idle")
    );
    // This PC's own Wi-Fi band does not matter: it leaves that network to join the group.
    assert_eq!(
        verdict(true, |_, _, s| s.probe = wifi(WifiRadio::On, Some(false), Band::Ghz24)),
        (vec![], "creds")
    );
}

#[test]
fn a_host_and_its_creds_join_with_the_name_and_password_derived_from_the_calls_uw() {
    let (mut owner, state, _) = owner();
    let out = offer(&mut owner, TICKET, T0);
    assert!(declines(&out).is_empty());
    assert_eq!(owner.state_name(), "joining");
    let joins = state.lock().unwrap().joins.clone();
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].0, 1);
    assert_eq!(joins[0].1, keys::direct_ssid(&UW, &TICKET).unwrap());
    assert_eq!(joins[0].2, keys::direct_psk(&UW, &TICKET).unwrap().as_str());
    // Without the call's key there is nothing to join with.
    let (mut owner, state, _) = owner_with(FakeGuestPlatform::ready());
    let mut out = Vec::new();
    owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0), &mut out);
    owner.on_peer_creds(creds(TICKET, PHONE), None, T0, &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
    assert!(state.lock().unwrap().joins.is_empty());
}

#[test]
fn every_ticket_is_used_once_even_when_its_host_was_declined() {
    let (mut owner, state, _) = owner();
    let mut not_armed = facts(T0);
    not_armed.armed = false;
    let mut out = Vec::new();
    owner.on_peer_host(host(TICKET), Some(&UW), &not_armed, &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
    let mut out = Vec::new();
    owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0), &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE], "a reused ticket");
    let other = [0x6b; 8];
    assert!(declines(&offer(&mut owner, other, T0)).is_empty());
    let mut out = Vec::new();
    owner.on_peer_decline(other, decline::SETUP_FAILED, T0 + 1, &mut out);
    let mut out = Vec::new();
    owner.on_peer_host(host(other), Some(&UW), &facts(T0 + 60_000), &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE], "a ticket that already ran");
    assert_eq!(state.lock().unwrap().joins.len(), 1, "one join per ticket");
}

#[test]
fn a_host_while_an_attempt_runs_is_declined_busy() {
    let (mut owner, _state, _) = owner();
    offer(&mut owner, TICKET, T0);
    let mut out = Vec::new();
    owner.on_peer_host(host([0x6b; 8]), Some(&UW), &facts(T0 + 1), &mut out);
    assert_eq!(declines(&out), vec![decline::BUSY]);
    assert_eq!(owner.state_name(), "joining");
}

#[test]
fn a_seventh_host_or_one_within_29_s_of_the_last_end_is_declined_4() {
    let (mut owner, _state, _) = owner();
    let mut now = T0;
    for i in 0..6u8 {
        let ticket = [i + 1; 8];
        assert!(declines(&offer(&mut owner, ticket, now)).is_empty(), "host {i}");
        let mut out = Vec::new();
        owner.on_peer_decline(ticket, decline::SETUP_FAILED, now, &mut out);
        let mut out = Vec::new();
        owner.on_peer_host(host([0x40 + i; 8]), Some(&UW), &facts(now + GUEST_REST_MS - 1), &mut out);
        assert_eq!(declines(&out), vec![decline::LIMIT_OR_COOLDOWN], "rest after {i}");
        now += GUEST_REST_MS;
    }
    let mut out = Vec::new();
    owner.on_peer_host(host([0x77; 8]), Some(&UW), &facts(now + 60_000), &mut out);
    assert_eq!(declines(&out), vec![decline::LIMIT_OR_COOLDOWN], "a seventh host");
}

#[test]
fn creds_must_come_within_8_s_and_for_the_running_ticket() {
    let (mut owner, state, _) = owner();
    let mut out = Vec::new();
    owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0), &mut out);
    owner.on_peer_creds(creds([9; 8], PHONE), Some(&UW), T0, &mut out);
    assert_eq!(owner.state_name(), "creds", "another ticket's CREDS change nothing");
    assert!(declines(&tick(&mut owner, T0 + GUEST_CREDS_DEADLINE_MS - 1)).is_empty());
    assert_eq!(declines(&tick(&mut owner, T0 + GUEST_CREDS_DEADLINE_MS)), vec![decline::SETUP_FAILED]);
    assert_eq!(owner.state_name(), "idle");
    assert!(state.lock().unwrap().joins.is_empty());
    assert!(state.lock().unwrap().leaves.is_empty(), "nothing was joined, nothing to leave");
}

#[test]
fn a_join_that_does_not_complete_in_12_s_declines_6_and_leaves() {
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().auto_join = None;
    let (mut owner, state, faults) = owner_with(fake);
    offer(&mut owner, TICKET, T0);
    assert!(declines(&tick(&mut owner, T0 + GUEST_JOIN_DEADLINE_MS - 1)).is_empty());
    assert_eq!(declines(&tick(&mut owner, T0 + GUEST_JOIN_DEADLINE_MS)), vec![decline::SETUP_FAILED]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert_eq!(faults.lock().unwrap().recorded(), 0, "a timeout is not a local fault");
}

#[test]
fn a_platform_refusal_is_a_local_fault_and_a_refused_network_is_not() {
    for (error, reason, recorded) in [
        (PlatformError::Native(5), decline::WIFI_UNAVAILABLE, 1),
        (PlatformError::InUse, decline::BUSY, 0),
    ] {
        let fake = FakeGuestPlatform::ready();
        fake.state.lock().unwrap().join_error = Some(error);
        let (mut owner, state, faults) = owner_with(fake);
        assert_eq!(declines(&offer(&mut owner, TICKET, T0)), vec![reason]);
        assert_eq!(state.lock().unwrap().leaves, vec![1], "leave runs on every exit path");
        assert_eq!(faults.lock().unwrap().recorded(), recorded);
    }
    for (event, reason, recorded) in [
        (JoinEvent::JoinRefused { generation: 1 }, decline::WIFI_UNAVAILABLE, 1),
        (JoinEvent::JoinFailed { generation: 1 }, decline::SETUP_FAILED, 0),
    ] {
        let fake = FakeGuestPlatform::ready();
        fake.state.lock().unwrap().auto_join = None;
        let (mut owner, state, faults) = owner_with(fake);
        offer(&mut owner, TICKET, T0);
        state.lock().unwrap().events.push_back(event);
        assert_eq!(declines(&tick(&mut owner, T0 + 10)), vec![reason]);
        assert_eq!(state.lock().unwrap().leaves, vec![1]);
        assert_eq!(faults.lock().unwrap().recorded(), recorded);
    }
}

fn after_join(channel: Option<u32>, freq_mhz: u16) -> (Vec<u8>, Arc<Mutex<FakeState>>) {
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().auto_join = Some(channel);
    let (mut owner, state, _) = owner_with(fake);
    let mut out = Vec::new();
    let mut offered = host(TICKET);
    offered.freq_mhz = freq_mhz;
    owner.on_peer_host(offered, Some(&UW), &facts(T0), &mut out);
    owner.on_peer_creds(creds(TICKET, PHONE), Some(&UW), T0, &mut out);
    (declines(&tick(&mut owner, T0)), state)
}

#[test]
fn the_band_comes_from_the_joined_channel_or_else_the_hosts_frequency() {
    let (declined, state) = after_join(Some(6), 5_745);
    assert_eq!(declined, vec![decline::NOT_5GHZ]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert!(state.lock().unwrap().connects.is_empty());
    let (declined, state) = after_join(Some(149), 0);
    assert!(declined.is_empty());
    assert_eq!(
        state.lock().unwrap().connects,
        vec![(1, Ipv4Addr::new(192, 168, 49, 23), SocketAddrV4::new(Ipv4Addr::from(PHONE), 47_000))]
    );
    let (declined, _) = after_join(None, 5_745);
    assert!(declined.is_empty());
    let (declined, state) = after_join(None, 0);
    assert_eq!(declined, vec![decline::NOT_5GHZ]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn creds_off_the_joined_subnet_public_or_this_pcs_own_address_decline_6_without_connecting() {
    for ipv4 in [[192, 168, 50, 1], [8, 8, 8, 8], [192, 168, 49, 23]] {
        let (mut owner, state, _) = owner();
        let mut out = Vec::new();
        owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0), &mut out);
        owner.on_peer_creds(creds(TICKET, ipv4), Some(&UW), T0, &mut out);
        assert_eq!(declines(&tick(&mut owner, T0)), vec![decline::SETUP_FAILED], "{ipv4:?}");
        assert!(state.lock().unwrap().connects.is_empty());
        assert_eq!(state.lock().unwrap().leaves, vec![1]);
    }
}

#[test]
fn a_joined_subnet_that_another_interface_reaches_declines_3() {
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().adapters.push(AdapterInfo {
        guid: "{CCCCCCCC-0000-0000-0000-000000000000}".to_owned(),
        description: "Ethernet".to_owned(),
        up: true,
        ipv4: vec![AddressInfo {
            ip: Ipv4Addr::new(192, 168, 49, 77),
            prefix: 24,
            preferred: true,
        }],
    });
    let (mut owner, state, _) = owner_with(fake);
    offer(&mut owner, TICKET, T0);
    assert_eq!(declines(&tick(&mut owner, T0)), vec![decline::WIFI_UNAVAILABLE]);
    assert!(state.lock().unwrap().connects.is_empty());
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn the_address_is_read_at_most_every_200_ms_until_it_shows() {
    let fake = FakeGuestPlatform::ready();
    let joined = fake.state.lock().unwrap().adapters.remove(0);
    let (mut owner, state, _) = owner_with(fake);
    offer(&mut owner, TICKET, T0);
    tick(&mut owner, T0);
    tick(&mut owner, T0 + ADDRESS_POLL_MS - 1);
    assert_eq!(owner.state_name(), "joining", "no address yet");
    state.lock().unwrap().adapters.push(joined);
    tick(&mut owner, T0 + ADDRESS_POLL_MS - 1);
    assert_eq!(owner.state_name(), "joining", "not read again before 200 ms");
    tick(&mut owner, T0 + ADDRESS_POLL_MS);
    assert_eq!(owner.state_name(), "connecting");
}

#[test]
fn a_refused_connect_is_retried_every_250_ms_for_5_s_then_declined_6() {
    let (mut owner, state, _) = owner();
    offer(&mut owner, TICKET, T0);
    tick(&mut owner, T0);
    assert_eq!(owner.state_name(), "connecting");
    state.lock().unwrap().events.push_back(JoinEvent::ConnectRefused { generation: 1 });
    tick(&mut owner, T0 + 100);
    tick(&mut owner, T0 + 100 + GUEST_CONNECT_RETRY_MS - 1);
    assert_eq!(state.lock().unwrap().connects.len(), 1);
    tick(&mut owner, T0 + 100 + GUEST_CONNECT_RETRY_MS);
    assert_eq!(state.lock().unwrap().connects.len(), 2);
    state.lock().unwrap().events.push_back(JoinEvent::ConnectRefused { generation: 1 });
    assert!(declines(&tick(&mut owner, T0 + GUEST_CONNECT_WINDOW_MS - 10)).is_empty());
    assert_eq!(declines(&tick(&mut owner, T0 + GUEST_CONNECT_WINDOW_MS)), vec![decline::SETUP_FAILED]);
    assert_eq!(state.lock().unwrap().connects.len(), 2);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn a_failed_connect_declines_6_and_a_refused_one_by_the_platform_is_local() {
    let (mut owner, state, faults) = owner();
    offer(&mut owner, TICKET, T0);
    tick(&mut owner, T0);
    state.lock().unwrap().events.push_back(JoinEvent::ConnectFailed { generation: 1 });
    assert_eq!(declines(&tick(&mut owner, T0 + 10)), vec![decline::SETUP_FAILED]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert_eq!(faults.lock().unwrap().recorded(), 0);
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().connect_error = Some(PlatformError::Native(10_050));
    let (mut owner, state, faults) = owner_with(fake);
    offer(&mut owner, TICKET, T0);
    assert_eq!(declines(&tick(&mut owner, T0)), vec![decline::WIFI_UNAVAILABLE]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert_eq!(faults.lock().unwrap().recorded(), 1);
}

#[test]
fn a_peer_decline_for_the_running_attempt_leaves_without_answering() {
    let (mut owner, state, _) = owner();
    offer(&mut owner, TICKET, T0);
    let mut out = Vec::new();
    owner.on_peer_decline([9; 8], decline::SETUP_FAILED, T0 + 1, &mut out);
    assert_eq!(owner.state_name(), "joining", "another ticket's decline changes nothing");
    owner.on_peer_decline(TICKET, decline::SETUP_FAILED, T0 + 2, &mut out);
    assert!(declines(&out).is_empty());
    assert_eq!(owner.state_name(), "idle");
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn turning_the_switch_off_mid_attempt_declines_1_and_leaves() {
    let (mut owner, state, _) = owner();
    offer(&mut owner, TICKET, T0);
    let mut out = Vec::new();
    owner.on_switch_off(T0 + 1, &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    let mut out = Vec::new();
    owner.on_switch_off(T0 + 2, &mut out);
    assert!(out.is_empty(), "nothing runs, nothing to say");
}

#[test]
fn wifi_ended_for_the_call_leaves_and_every_later_host_is_declined_1() {
    let (mut owner, state, _) = owner();
    offer(&mut owner, TICKET, T0);
    let mut out = Vec::new();
    owner.on_wifi_ended(T0 + 1, &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
    assert_eq!(owner.state_name(), "off");
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    let mut out = Vec::new();
    owner.on_peer_host(host([0x6b; 8]), Some(&UW), &facts(T0 + 120_000), &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
}

#[test]
fn release_and_drop_leave_the_group_in_every_stage() {
    for stage in ["creds", "joining", "connecting"] {
        let fake = FakeGuestPlatform::ready();
        if stage == "joining" {
            fake.state.lock().unwrap().auto_join = None;
        }
        let (mut owner, state, _) = owner_with(fake);
        let mut out = Vec::new();
        owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0), &mut out);
        if stage != "creds" {
            owner.on_peer_creds(creds(TICKET, PHONE), Some(&UW), T0, &mut out);
        }
        if stage == "connecting" {
            tick(&mut owner, T0);
        }
        assert_eq!(owner.state_name(), stage);
        drop(owner);
        let expected: Vec<u32> = if stage == "creds" { vec![] } else { vec![1] };
        assert_eq!(state.lock().unwrap().leaves, expected, "{stage}");
    }
    let (mut owner, state, _) = owner();
    offer(&mut owner, TICKET, T0);
    owner.release();
    assert_eq!(owner.state_name(), "off");
    drop(owner);
    assert_eq!(state.lock().unwrap().leaves, vec![1], "released once, not again at drop");
}

#[test]
fn a_join_that_completes_after_leave_is_undone() {
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().auto_join = None;
    let (mut owner, state, _) = owner_with(fake);
    offer(&mut owner, TICKET, T0);
    let mut out = Vec::new();
    owner.on_peer_decline(TICKET, decline::SETUP_FAILED, T0 + 1, &mut out);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    state.lock().unwrap().events.push_back(JoinEvent::Joined {
        generation: 1,
        channel: Some(149),
    });
    let out = tick(&mut owner, T0 + 2);
    assert_eq!(state.lock().unwrap().leaves, vec![1, 1], "left again");
    assert!(out.iter().any(|output| matches!(output, Output::Trace { detail, .. } if detail.contains("late_join_undone"))));
    assert!(state.lock().unwrap().connects.is_empty());
    assert_eq!(owner.state_name(), "idle");
}

#[test]
fn nothing_the_owner_prints_names_the_network_the_password_the_ticket_or_an_address() {
    let ssid = keys::direct_ssid(&UW, &TICKET).unwrap();
    let psk = keys::direct_psk(&UW, &TICKET).unwrap().to_string();
    let ticket_hex: String = TICKET.iter().map(|byte| format!("{byte:02x}")).collect();
    let (mut owner, state, _) = owner();
    let mut printed = Vec::new();
    let mut out = offer(&mut owner, TICKET, T0);
    printed.push(format!("{owner:?}"));
    out.extend(tick(&mut owner, T0));
    printed.push(format!("{owner:?}"));
    state.lock().unwrap().events.push_back(JoinEvent::ConnectFailed { generation: 1 });
    out.extend(tick(&mut owner, T0 + 1));
    printed.push(format!("{owner:?}"));
    for output in &out {
        printed.push(format!("{output:?}"));
        if let Output::Trace { detail, .. } = output {
            printed.push(detail.clone());
        }
    }
    assert!(printed.len() > 4);
    for text in &printed {
        // Rust's Debug prints byte arrays as decimals: the ticket and the CREDS address too.
        let ticket_array = format!("{TICKET:?}");
        let address_array = format!("{PHONE:?}");
        for secret in [
            ssid.as_str(),
            psk.as_str(),
            ticket_hex.as_str(),
            ticket_array.as_str(),
            address_array.as_str(),
            "192.168.49",
            "47000",
        ] {
            assert!(!text.contains(secret), "a secret was printed");
        }
    }
}

use crate::wifi::{
    binding::{HostBinding, Step as HostStep},
    socket::WifiSocket,
};
use std::{
    net::TcpListener,
    time::{Duration, Instant},
};

/// The phone's side: a loopback listener and the real host binding.
struct Phone {
    listener: TcpListener,
    uw: [u8; 32],
    binding: Option<HostBinding>,
    bound: Option<Box<Bound>>,
}

impl Phone {
    fn new(uw: [u8; 32]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            listener,
            uw,
            binding: None,
            bound: None,
        }
    }
    fn address(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }
    fn step(&mut self) {
        if self.binding.is_none() && self.bound.is_none() {
            if let Ok((stream, _)) = self.listener.accept() {
                let socket = WifiSocket::from_stream(stream).unwrap();
                self.binding = Some(HostBinding::new(socket, &self.uw, TICKET, 0).unwrap());
            }
        }
        if let Some(binding) = self.binding.as_mut() {
            match binding.poll(0) {
                HostStep::Pending => {}
                HostStep::Done(bound) => {
                    self.bound = Some(bound);
                    self.binding = None;
                }
                // The host closes silently (the socket drops with the binding).
                HostStep::Failed(_) => self.binding = None,
            }
        }
    }
}

fn phone_owner(phone: &Phone) -> (GuestOwner, Arc<Mutex<FakeState>>) {
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().connect_to = Some(phone.address());
    let (owner, state, _faults) = owner_with(fake);
    (owner, state)
}

/// Ticks the owner (answering `Begin` with `id` as the session would) and the phone until
/// `until` holds; at most 3 s of real time.
fn drive(
    owner: &mut GuestOwner,
    phone: &mut Phone,
    now_ms: i64,
    id: Option<i32>,
    out: &mut Vec<Output>,
    until: impl Fn(&GuestOwner) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !until(owner) {
        assert!(Instant::now() < deadline, "stuck in {}", owner.state_name());
        let mut step = tick(owner, now_ms);
        if step.iter().any(|output| matches!(output, Output::Begin { .. })) {
            let mut extra = Vec::new();
            owner.set_attempt_id(id, now_ms, &mut extra);
            step.extend(extra);
        }
        out.extend(step);
        phone.step();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn live_owner() -> (GuestOwner, Arc<Mutex<FakeState>>, Phone) {
    let mut phone = Phone::new(UW);
    let (mut owner, state) = phone_owner(&phone);
    let mut out = offer(&mut owner, TICKET, T0);
    drive(&mut owner, &mut phone, T0 + 10, Some(1), &mut out, |o| o.state_name() == "live");
    (owner, state, phone)
}

#[test]
fn the_carrier_is_handed_over_only_after_bind_and_ready_both_ways() {
    let mut phone = Phone::new(UW);
    let (mut owner, state) = phone_owner(&phone);
    let mut out = offer(&mut owner, TICKET, T0);
    drive(&mut owner, &mut phone, T0 + 10, Some(1), &mut out, |o| o.state_name() == "live");
    let begin = out
        .iter()
        .position(|output| matches!(output, Output::Begin { ticket } if *ticket == TICKET))
        .expect("begin");
    let attach = out
        .iter()
        .position(|output| matches!(output, Output::Attach { id: 1, ticket, .. } if *ticket == TICKET))
        .expect("attach");
    assert!(begin < attach);
    assert_eq!(out.iter().filter(|output| matches!(output, Output::Attach { .. })).count(), 1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while phone.bound.is_none() {
        assert!(Instant::now() < deadline, "the phone did not finish BIND and READY");
        phone.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(declines(&out).is_empty());
    assert_eq!(state.lock().unwrap().connects.len(), 1);
    assert!(state.lock().unwrap().leaves.is_empty(), "a live carrier keeps the group");
}

#[test]
fn a_phone_with_another_key_never_gets_a_carrier_and_the_ticket_is_never_used_again() {
    let mut phone = Phone::new([8; 32]);
    let (mut owner, state) = phone_owner(&phone);
    let mut out = offer(&mut owner, TICKET, T0);
    drive(&mut owner, &mut phone, T0 + 10, Some(1), &mut out, |o| o.state_name() == "binding");
    for _ in 0..200 {
        if owner.state_name() == "idle" {
            break;
        }
        phone.step();
        std::thread::sleep(Duration::from_millis(2));
        out.extend(tick(&mut owner, T0 + 20));
    }
    out.extend(tick(&mut owner, T0 + 20 + crate::wifi::limits::GUEST_FRAME_DEADLINE_MS));
    assert_eq!(declines(&out), vec![decline::SETUP_FAILED]);
    assert!(!out.iter().any(|output| matches!(output, Output::Begin { .. } | Output::Attach { .. })));
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert_eq!(state.lock().unwrap().connects.len(), 1, "no second socket for the ticket");
    let mut out = Vec::new();
    owner.on_peer_host(host(TICKET), Some(&UW), &facts(T0 + 120_000), &mut out);
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
}

#[test]
fn a_refused_attempt_id_declines_1_leaves_and_turns_the_guest_off() {
    let mut phone = Phone::new(UW);
    let (mut owner, state) = phone_owner(&phone);
    let mut out = offer(&mut owner, TICKET, T0);
    drive(&mut owner, &mut phone, T0 + 10, None, &mut out, |o| o.state_name() == "off");
    assert_eq!(declines(&out), vec![decline::NOT_ELIGIBLE]);
    assert!(!out.iter().any(|output| matches!(output, Output::Attach { .. })));
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn a_lost_live_carrier_leaves_the_group_and_the_next_host_waits_29_s() {
    let (mut owner, state, _phone) = live_owner();
    owner.on_carrier_lost(7, T0 + 1_000);
    assert_eq!(owner.state_name(), "live", "another carrier's loss changes nothing");
    owner.on_carrier_lost(1, T0 + 1_000);
    assert_eq!(owner.state_name(), "idle");
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    let mut out = Vec::new();
    owner.on_peer_host(host([0x6b; 8]), Some(&UW), &facts(T0 + 1_000 + GUEST_REST_MS - 1), &mut out);
    assert_eq!(declines(&out), vec![decline::LIMIT_OR_COOLDOWN]);
    let mut out = Vec::new();
    owner.on_peer_host(host([0x6c; 8]), Some(&UW), &facts(T0 + 1_000 + GUEST_REST_MS), &mut out);
    assert!(declines(&out).is_empty());
    assert_eq!(owner.state_name(), "creds");
}

#[test]
fn the_network_dropping_under_a_live_carrier_kills_it_at_once() {
    let (mut owner, state, _phone) = live_owner();
    state.lock().unwrap().events.push_back(JoinEvent::Left { generation: 1 });
    let out = tick(&mut owner, T0 + 500);
    assert!(out.iter().any(|output| matches!(output, Output::Died { id: 1 })));
    owner.on_carrier_lost(1, T0 + 501);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn switching_off_while_live_hands_the_carrier_over_and_leaves_at_its_close() {
    let (mut owner, state, _phone) = live_owner();
    let mut out = Vec::new();
    owner.on_switch_off(T0 + 500, &mut out);
    assert!(declines(&out).is_empty());
    assert_eq!(owner.state_name(), "off");
    assert!(state.lock().unwrap().leaves.is_empty(), "the session still falls back on it");
    owner.on_carrier_lost(1, T0 + 600);
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

/// A raw host that answers frame 1/2/3 correctly but sends a corrupt `BIND` in the same
/// write as frame 3 (so the guest derives its keys and fails in one poll).
fn corrupt_bind_together_with_frame_3() -> (GuestOwner, Vec<Output>) {
    use crate::{
        crypto,
        records::{Layer, Policy, Sealer},
    };
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fake = FakeGuestPlatform::ready();
    fake.state.lock().unwrap().connect_to = Some(listener.local_addr().unwrap());
    let (mut owner, _state, _faults) = owner_with(fake);
    let mut out = offer(&mut owner, TICKET, T0);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = None;
    while stream.is_none() {
        assert!(Instant::now() < deadline, "no connect");
        out.extend(tick(&mut owner, T0 + 10));
        listener.set_nonblocking(true).unwrap();
        if let Ok((accepted, _)) = listener.accept() {
            accepted.set_nonblocking(false).unwrap();
            stream = Some(accepted);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut stream = stream.unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    let frame1 = wire::frame1(&[0x33; 16]);
    stream.write_all(&frame1).unwrap();
    let mut frame2 = [0u8; wire::BWU1_FRAME2_BYTES];
    let mut have = 0;
    while have < frame2.len() {
        assert!(Instant::now() < deadline, "no frame 2");
        out.extend(tick(&mut owner, T0 + 10));
        match stream.read(&mut frame2[have..]) {
            Ok(count) => have += count,
            Err(_) => {}
        }
    }
    let parsed = wire::parse_frame2(&frame2).unwrap();
    let ephemeral = crypto::Ephemeral::generate().unwrap();
    let e_h = keys::point_of(&ephemeral.public_key().unwrap());
    let frame3 = wire::frame3(&e_h, &[0x44; 32]);
    let th = keys::transcript_hash(&frame1, &frame2, &frame3).unwrap();
    let dh = ephemeral.agree(&keys::spki_of(&parsed.e_g).unwrap()).unwrap();
    let bind_keys = keys::bind_keys(&UW, &TICKET, &th, &dh).unwrap();
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let mut sealer = Sealer::new(Layer::Hop, &bind_keys.h2g, policy).unwrap();
    // The guest's own role where the host's must be: a corrupt BIND.
    let bind = wire::Bwf1::Bind(wire::Bind {
        role: wire::role::GUEST,
        ticket: TICKET,
    })
    .encode()
    .unwrap();
    let mut both = frame3.to_vec();
    both.extend_from_slice(&sealer.seal(wire::HOP_TYPE, 0, &bind).unwrap());
    stream.write_all(&both).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    for _ in 0..50 {
        out.extend(tick(&mut owner, T0 + 10));
        if owner.state_name() == "idle" {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    (owner, out)
}

#[test]
fn a_bind_that_fails_in_the_poll_that_derived_the_keys_registers_no_cutover_carrier() {
    let (mut owner, out) = corrupt_bind_together_with_frame_3();
    assert_eq!(owner.state_name(), "idle");
    assert_eq!(declines(&out), vec![decline::SETUP_FAILED]);
    assert!(
        !out.iter().any(|output| matches!(output, Output::Begin { .. })),
        "a failed binding must not ask the cutover for a pending carrier"
    );
    // Even if a Begin raced ahead, the id the session answers with is never kept.
    let mut late = Vec::new();
    owner.set_attempt_id(Some(5), T0 + 50, &mut late);
    assert!(late.iter().any(|output| matches!(output, Output::Died { id: 5 })));
    assert_eq!(owner.attempt_id, None);
}
