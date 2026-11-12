#![cfg(test)]

use super::*;
use crate::{
    bmh2::Purpose,
    call::CallLink,
    call_session::{Notice, WifiNotice},
    framing::HopReader,
    records::Policy,
    session::tests::{handshake, identities},
    wifi::{
        attempt::{self, fake::FakePlatform},
        quality::QualitySampler,
        roles,
        wire::{self},
    },
};
use std::{
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

const MS: u64 = 1_000_000;

fn make(description: &[u8]) -> Result<opus::Decoder, opus::Error> {
    opus::Decoder::new(description, opus::Profile::StreamOrCall)
}

fn session_pair() -> (
    CallSession<opus::Decoder>,
    CallSession<opus::Decoder>,
    Vec<Notice>,
) {
    let (mut caller, mut callee, _) =
        crate::call_session::wifi_test_support::wifi_session_pair_with(make, false, false);
    feed(&mut caller, &flush(&mut callee, 0), 0);
    callee.accept(false).unwrap();
    feed(&mut caller, &flush(&mut callee, MS), MS);
    caller.media_ready().unwrap();
    feed(&mut callee, &flush(&mut caller, 2 * MS), 2 * MS);
    callee.media_ready().unwrap();
    (caller, callee, Vec::new())
}

fn flush(session: &mut CallSession<opus::Decoder>, now: u64) -> Vec<(i32, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(write) = session.next_write_on(now).unwrap() {
        out.push((write.carrier, write.bytes));
    }
    out
}

fn feed(session: &mut CallSession<opus::Decoder>, records: &[(i32, Vec<u8>)], now: u64) {
    for (carrier, bytes) in records {
        let mut input = bytes.as_slice();
        session.on_input_from(*carrier, &mut input, now).unwrap();
    }
}

fn socket_pair() -> (WifiSocket, WifiSocket) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = std::net::TcpStream::connect(address).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        WifiSocket::from_stream(client).unwrap(),
        WifiSocket::from_stream(server).unwrap(),
    )
}

fn dialed_with(session: CallSession<opus::Decoder>, wifi: WifiLink) -> Dialed {
    dialed_with_peer(session, wifi).0
}

/// The `Dialed` and the far end of its carrier-0 stand-in.
fn dialed_with_peer(session: CallSession<opus::Decoder>, wifi: WifiLink) -> (Dialed, WifiSocket) {
    let (dummy, peer) = socket_pair();
    let dialed = Dialed {
        link: Link::Wifi(dummy),
        buffer: Vec::new(),
        session,
        start: Instant::now() - Duration::from_millis(4_000),
        peer_discovery: false,
        paired: false,
        header: crate::call_session::wifi_test_support::header(),
        video_plan: Vec::new(),
        peer_real: None,
        wifi,
    };
    (dialed, peer)
}

fn active_dialed() -> Dialed {
    let (caller, _callee, _) = session_pair();
    dialed_with(caller, WifiLink::default())
}

/// A call with carrier 1 attached now on the `Dialed` clock (the `Dialed`
/// starts 4 s in the past, so the attach and carrier 0's last record are at 4 s).
fn live_dialed() -> (Dialed, WifiSocket, CallSession<opus::Decoder>) {
    let (dialed, peer_wire, callee, _bluetooth_peer) = live_dialed_with_bluetooth_peer();
    (dialed, peer_wire, callee)
}

/// `live_dialed` with the callee's carrier attached too and the far end of carrier 0.
fn live_dialed_with_bluetooth_peer() -> (Dialed, WifiSocket, CallSession<opus::Decoder>, WifiSocket) {
    let (mut caller, mut callee, _) = session_pair();
    let now = 4_000 * MS;
    let heartbeat = flush(&mut callee, now);
    feed(&mut caller, &heartbeat, now);
    let ticket = [9; 8];
    let id = caller.wifi_begin_attempt(ticket, now).unwrap();
    assert_eq!(callee.wifi_begin_attempt(ticket, now).unwrap(), id);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let ready = wire::Ready {
        recv_ceiling_kbps: 1100,
        family_cap_kbps: 0,
        transport_budget_kbps: 1100,
    };
    caller.wifi_attach(
        id,
        Sealer::new(Layer::Hop, &[1; 32], policy).unwrap(),
        HopReader::new(&[2; 32], policy).unwrap(),
        ready,
        now,
    );
    callee.wifi_attach(
        id,
        Sealer::new(Layer::Hop, &[2; 32], policy).unwrap(),
        HopReader::new(&[1; 32], policy).unwrap(),
        ready,
        now,
    );
    let (wire, peer_wire) = socket_pair();
    let mut wifi = WifiLink::default();
    wifi.link = Some(Link::Wifi(wire));
    wifi.id = id;
    wifi.quality = Some(QualitySampler::new(4_000));
    let (dialed, bluetooth_peer) = dialed_with_peer(caller, wifi);
    (dialed, peer_wire, callee, bluetooth_peer)
}

fn prepare_caps(dialed: &mut Dialed) {
    dialed
        .session
        .wifi_set_local_caps(roles::windows_caps(true));
    let carriers = dialed.session.test_carriers_mut().unwrap();
    carriers.set_peer_caps(wire::Caps {
        bearers: wire::caps_bit::DIRECT_CLIENT,
        free_host: 0,
        free_guest: 0,
    });
    carriers.mark_caps_sent();
}

fn prepare_owner(
    dialed: &mut Dialed,
    counters: &mut Arc<Mutex<crate::wifi::attempt::fake::Counters>>,
) {    prepare_caps(dialed);
    dialed.wifi.armed = true;
    dialed.wifi.owner = Some(Box::new(attempt::Owner::new()));
    let fake = FakePlatform::new();
    *counters = fake.counters.clone();
    dialed.wifi.platform = Some(Box::new(fake));
    dialed.wifi.video_since_ms = Some(0);
}

fn read_exact(socket: &WifiSocket, bytes: &mut [u8]) {
    let mut done = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while done < bytes.len() {
        if std::time::Instant::now() > deadline {
            panic!("socket read timed out after {done}/{} bytes", bytes.len());
        }
        match socket.read(&mut bytes[done..], std::time::Instant::now() + Duration::from_millis(100)) {
            Ok(count) => done += count,
            Err(crate::bluetooth::carrier::Error::Timeout) => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(error) => panic!("socket read failed: {error:?}"),
        }
    }
}

#[test]
fn an_unarmed_call_never_creates_a_platform_or_an_owner() {
    let mut dialed = active_dialed();
    dialed.wifi_arm(false);
    assert!(dialed.wifi.owner.is_none());
    assert!(dialed.wifi.platform.is_none());
}

#[test]
fn an_armed_call_on_a_link_without_the_marker_never_creates_an_owner() {
    let (a, b, _) = identities();
    let (al, _bl) = handshake(&a, &b, Purpose::Link);
    let (ae, _be) = handshake(&a, &b, Purpose::Endpoint);
    let link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, al.send_key(), Policy::default()).unwrap(),
        HopReader::new(al.receive_key(), Policy::default()).unwrap(),
        ae.send_key(),
        ae.receive_key(),
        false,
        false,
        false,
    )
    .unwrap();
    let session = CallSession::outgoing(
        link,
        crate::call_session::wifi_test_support::ID,
        false,
        0,
        crate::call_session::wifi_test_support::header(),
        make,
        0,
    )
    .unwrap();
    let mut dialed = dialed_with(session, WifiLink::default());
    dialed.wifi_arm(true);
    assert!(dialed.wifi.owner.is_none());
}

#[test]
fn an_armed_call_on_a_relay_edge_never_creates_an_owner() {
    let (mut caller, _callee, _) = crate::call_session::wifi_test_support::wifi_session_pair_flags(
        make, false, false, true, false,
    );
    let mut dialed = dialed_with(caller, WifiLink::default());
    dialed.wifi_arm(true);
    assert!(!dialed.session.wifi_attempts_allowed());
    assert!(dialed.wifi.owner.is_none());
}

#[test]
fn turning_the_switch_on_mid_call_creates_the_owner_before_a_switch_off_and_not_after() {
    let previous = std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", "1"); }
    let mut dialed = active_dialed();
    prepare_caps(&mut dialed);
    dialed.wifi.armed = false;
    dialed.wifi_set_armed(true, &mut |_, _| {});
    assert!(dialed.wifi.owner.is_some());
    dialed.wifi_set_armed(false, &mut |_, _| {});
    assert!(dialed.wifi.owner.is_some());
    dialed.wifi_set_armed(true, &mut |_, _| {});
    assert!(dialed.wifi.owner.is_some());
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", value),
            None => std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"),
        }
    }
}

#[test]
fn the_owner_outputs_reach_the_session_in_order_and_the_carrier_attaches() {
    let previous = std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", "1"); }
    let mut dialed = active_dialed();
    prepare_caps(&mut dialed);
    dialed.wifi.armed = true;
    dialed.wifi.owner = Some(Box::new(attempt::Owner::new()));
    let (host_socket, guest_stream) = attempt::fake::socket_pair();
    let mut fake = FakePlatform::new();
    fake.accepts.push_back(host_socket);
    dialed.wifi.platform = Some(Box::new(fake));
    dialed.wifi.video_since_ms = Some(0);
    let mut binding = false;
    let mut labels = Vec::new();
    for _ in 0..20 {
        if let Some(label) = dialed.wifi_tick(&mut |_, _| {}) {
            labels.push(label);
        }
        if dialed
            .wifi
            .owner
            .as_ref()
            .is_some_and(|owner| owner.state_name() == "binding")
        {
            binding = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(binding, "no binding");
    let ticket = dialed.wifi.owner.as_ref().unwrap().ticket().unwrap();
    let uw = *dialed.session.wifi_uw().unwrap();
    let mut guest = crate::wifi::binding::test_guest::Guest::new(guest_stream, &uw, ticket);
    guest.send_frame2();
    for _ in 0..10 {
        let _ = dialed.wifi_tick(&mut |_, _| {});
        std::thread::sleep(Duration::from_millis(2));
    }
    guest.read_frame3();
    guest.send_bind(wire::role::GUEST, ticket);
    let _ = guest.read_record();
    let _ = dialed.wifi_tick(&mut |_, _| {});
    let _ = guest.read_record();
    guest.send_ready();
    for _ in 0..20 {
        if let Some(label) = dialed.wifi_tick(&mut |_, _| {}) {
            labels.push(label);
        }
        if dialed.wifi.link.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(dialed.wifi.link.is_some());
    assert!(
        dialed.session.test_carriers_mut().unwrap().extra().is_some(),
        "the session did not keep the attached carrier"
    );
    assert_eq!(labels.last().copied(), Some("Wi-Fi"));
    assert_eq!(dialed.wifi_transport(), None);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", value),
            None => std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"),
        }
    }
}

#[test]
fn dropping_the_call_releases_the_group_and_closes_the_listener() {
    let mut dialed = active_dialed();
    let mut counters = Arc::new(Mutex::new(crate::wifi::attempt::fake::Counters::default()));
    prepare_owner(&mut dialed, &mut counters);
    drop(dialed);
    assert!(counters.lock().unwrap().releases >= 1);
}

#[test]
fn a_decline_notice_from_the_phone_reaches_the_owner() {
    let mut dialed = active_dialed();
    prepare_caps(&mut dialed);
    let mut counters = Arc::new(Mutex::new(crate::wifi::attempt::fake::Counters::default()));
    prepare_owner(&mut dialed, &mut counters);
    for _ in 0..10 {
        let _ = dialed.wifi_tick(&mut |_, _| {});
        if dialed
            .wifi
            .owner
            .as_ref()
            .is_some_and(|owner| owner.state_name() == "offered")
        {
            break;
        }
    }
    let ticket = dialed.wifi.owner.as_ref().unwrap().ticket().unwrap();
    dialed.wifi_dispatch(&[Notice::Wifi(WifiNotice::PeerDecline {
        ticket,
        reason: wire::decline::BUSY,
    })]);
    assert!(!dialed.wifi.pending_out.is_empty());
    let _ = dialed.wifi_tick(&mut |_, _| {});
    assert_eq!(
        dialed.wifi.owner.as_ref().unwrap().state_name(),
        "idle"
    );
}

#[test]
fn a_wifi_carrier_that_dies_tells_the_owner_once_and_the_group_is_released() {
    let (mut dialed, peer_wire, _callee) = live_dialed();
    let mut counters = Arc::new(Mutex::new(crate::wifi::attempt::fake::Counters::default()));
    prepare_owner(&mut dialed, &mut counters);
    let id = dialed.wifi.id;
    dialed.wifi.owner.as_mut().unwrap().force_live(id);
    drop(peer_wire);
    let id = dialed.wifi.id;
    let mut tries = 0;
    while dialed.wifi.ended.is_empty() && tries < 50 {
        dialed.pump_input(Duration::from_millis(10)).expect("a Wi-Fi read failure never ends the call");
        tries += 1;
    }
    assert_eq!(dialed.wifi.ended, vec![id], "the closed socket queues its carrier end once");
    assert!(dialed.wifi.link.is_none());
    let _ = dialed.wifi_tick(&mut |_, _| {});
    let after_first = counters.lock().unwrap().releases;
    assert!(after_first >= 1, "releases={after_first} state={}", dialed.wifi.owner.as_ref().unwrap().state_name());
    let _ = dialed.wifi_tick(&mut |_, _| {});
    assert_eq!(counters.lock().unwrap().releases, after_first);
}

/// Everything readable now (the peer wrote it before this call).
fn read_available(socket: &WifiSocket) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        match socket.read(&mut chunk, std::time::Instant::now() + Duration::from_millis(20)) {
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(crate::bluetooth::carrier::Error::Timeout) if !bytes.is_empty() || std::time::Instant::now() > deadline => break,
            Err(crate::bluetooth::carrier::Error::Timeout) => {}
            Err(_) => break,
        }
    }
    bytes
}

#[test]
fn the_hang_up_written_on_wifi_is_still_delivered_during_the_linger() {
    let (mut dialed, peer_wire, mut callee, bluetooth_peer) = live_dialed_with_bluetooth_peer();
    {
        // Video since 1 s: at 4 s the machine switches to the attached carrier.
        let carriers = dialed.session.test_carriers_mut().unwrap();
        carriers.on_call_active(1_000, true);
        carriers.tick(4_000, true);
    }
    let id = dialed.wifi.id;
    assert_eq!(dialed.session.wifi_send_on(), id, "the call sends on Wi-Fi");
    dialed.session.hang_up(crate::call::Reason::Normal).unwrap();
    // The worker's linger: flush, then only the Wi-Fi socket is serviced.
    assert!(dialed.flush().expect("flush") > 0, "flush wrote nothing");
    dialed.wifi_service();
    assert!(dialed.wifi.link.is_some(), "the Wi-Fi socket stays during the linger");
    // The phone reads what arrived: carrier 0 (CLOSE, the SWITCH marker), then Wi-Fi.
    let now = 4_100 * MS;
    let bluetooth = read_available(&bluetooth_peer);
    let mut input = bluetooth.as_slice();
    callee.on_input_from(0, &mut input, now).unwrap();
    assert!(
        !callee.take_remote().iter().any(|m| m.signal == crate::call::Signal::HangUp),
        "the hang-up must not have gone on Bluetooth"
    );
    let wifi = read_available(&peer_wire);
    assert!(!wifi.is_empty(), "nothing was delivered on Wi-Fi");
    let mut input = wifi.as_slice();
    callee.on_input_from(id, &mut input, now).unwrap();
    assert!(
        callee.take_remote().iter().any(|m| m.signal == crate::call::Signal::HangUp),
        "the phone did not read the hang-up from the Wi-Fi carrier"
    );
    assert!(dialed.wifi.link.is_some());
    drop(dialed);
}

#[test]
fn switching_off_while_bluetooth_is_dead_keeps_the_only_carrier() {
    let (mut dialed, _peer_wire, _callee) = live_dialed();
    let mut counters = Arc::new(Mutex::new(crate::wifi::attempt::fake::Counters::default()));
    prepare_owner(&mut dialed, &mut counters);
    let id = dialed.wifi.id;
    dialed.wifi.owner.as_mut().unwrap().force_live(id);
    dialed.wifi.bt_dead = true;
    if let Some(owner) = dialed.wifi.owner.as_mut() {
        // The state is not Live here; the switch-off path still must not release a lone carrier.
        let _ = owner;
    }
    let before = counters.lock().unwrap().releases;
    dialed.wifi_set_armed(false, &mut |_, _| {});
    assert!(dialed.wifi.link.is_some());
    assert_eq!(counters.lock().unwrap().releases, before);
    assert!(dialed.flush().is_ok());
    assert!(dialed.pump_input(Duration::from_millis(1)).is_ok());
}

#[test]
fn the_written_caps_withholds_bit_2_until_the_windows_join_is_proven() {
    let previous = std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").ok();
    unsafe { std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"); }
    let mut dialed = active_dialed();
    dialed.wifi_arm(true);
    assert!(dialed.wifi.owner.is_none());
    assert_eq!(
        dialed.session.wifi_local_caps().bearers & wire::caps_bit::DIRECT_GO,
        0
    );
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", value),
            None => std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"),
        }
    }
}

/// WP6NF finding 1: a block made after the last write latches before the
/// next write's budget is chosen, so that write is bounded by the blocked
/// budget and never runs past the hard local deadline (not the ordinary 8 s).
#[test]
fn a_block_made_between_writes_bounds_the_very_next_write() {
    let mut dialed = active_dialed();
    let peer = [0x7cu8; 32];
    let policy = std::sync::Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: Some(crate::label_policy::NamesAudience::Everyone),
        blocked: Some(Vec::new()),
        contacts: None,
    }));
    dialed.session.use_policy(policy.clone());
    let now = dialed.now_ns();
    dialed.session.screen_peer(peer, now);
    assert!(dialed.write_budget(now) >= Duration::from_secs(1), "an ordinary call keeps its budget");
    policy.set_blocked(Some(vec![peer]));
    let budget = dialed.write_budget(now);
    assert!(dialed.session.peer_blocked(), "latched before the budget was chosen");
    assert!(budget <= Duration::from_millis(crate::call_session::BLOCKED_WRITE_BUDGET_MS));
    let late = now + (crate::call_session::BLOCKED_END_LATEST_MS + crate::call_session::BLOCKED_WRITE_BUDGET_MS) * MS - MS;
    assert!(dialed.write_budget(late) <= Duration::from_millis(1), "never past the hard local deadline");
}

/// WP6NG finding 5: a block that lands after the flush chose its budget but
/// before it chose its record (another thread's policy change) is latched
/// while the record is chosen and bounds that very write.
#[test]
fn a_block_landing_between_budget_and_record_choice_bounds_that_write() {
    // The carrier-0 stand-in's far end stays open (a dropped one reads as Closed).
    let (caller, _callee, _) = session_pair();
    let (mut dialed, _carrier_peer) = dialed_with_peer(caller, WifiLink::default());
    let peer = [0x7du8; 32];
    let policy = Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: Some(crate::label_policy::NamesAudience::Everyone),
        blocked: Some(Vec::new()),
        contacts: None,
    }));
    dialed.session.use_policy(policy.clone());
    dialed.session.screen_peer(peer, dialed.now_ns());
    let late = policy.clone();
    dialed.wifi.before_select = Some(Box::new(move || late.set_blocked(Some(vec![peer]))));
    dialed.flush().unwrap();
    assert!(dialed.session.peer_blocked(), "latched while the record was chosen");
    let budget = dialed.wifi.last_bt_budget.expect("a carrier-0 write");
    assert!(budget <= Duration::from_millis(crate::call_session::BLOCKED_WRITE_BUDGET_MS), "{budget:?}");
}

/// WP6NG finding 3: a blocked call runs no Bluetooth recovery (GR1) I/O: no
/// resend of pre-block content and no fresh 8 s recovery write.
#[test]
fn a_blocked_call_runs_no_bluetooth_recovery_io() {
    let mut dialed = active_dialed();
    if let Link::Wifi(socket) = &mut dialed.link {
        socket.force_stall();
    }
    assert!(dialed.service_bt().is_err(), "an ordinary call reaches the carrier's recovery service");
    let peer = [0x7fu8; 32];
    dialed.session.use_policy(Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: None,
        blocked: Some(vec![peer]),
        contacts: None,
    })));
    dialed.session.screen_peer(peer, dialed.now_ns());
    assert!(dialed.service_bt().is_ok(), "a blocked call starts no recovery I/O");
}

/// WP6NG finding 2: the blocked end counts as written only once the carrier
/// took it.
#[test]
fn the_blocked_end_counts_as_written_only_after_the_carrier_took_it() {
    let (caller, _callee, _) = session_pair();
    let (mut dialed, _carrier_peer) = dialed_with_peer(caller, WifiLink::default());
    let peer = [0x6au8; 32];
    dialed.session.use_policy(Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: None,
        blocked: Some(vec![peer]),
        contacts: None,
    })));
    dialed.session.set_blocked_delay_ms(0);
    dialed.session.screen_peer(peer, dialed.now_ns());
    assert_eq!(dialed.session.blocked_end_state(), Some(crate::call_session::BlockedEndState::Pending));
    dialed.flush().unwrap();
    assert_eq!(dialed.session.blocked_end_state(), Some(crate::call_session::BlockedEndState::Written));
}

/// WP6NG finding 6: a call moved to Wi-Fi keeps its carrier when the peer
/// turns out to be blocked: the owner stops all attempt work, the socket
/// stays, and the ordinary end arrives on Wi-Fi.
#[test]
fn a_blocked_call_on_wifi_keeps_its_carrier_and_its_ordinary_end_arrives() {
    let (mut dialed, peer_wire, mut callee, bluetooth_peer) = live_dialed_with_bluetooth_peer();
    {
        let carriers = dialed.session.test_carriers_mut().unwrap();
        carriers.on_call_active(1_000, true);
        carriers.tick(4_000, true);
    }
    let id = dialed.wifi.id;
    assert_eq!(dialed.session.wifi_send_on(), id, "the call sends on Wi-Fi");
    let mut counters = Arc::new(Mutex::new(crate::wifi::attempt::fake::Counters::default()));
    prepare_owner(&mut dialed, &mut counters);
    let peer = [0x7au8; 32];
    dialed.session.use_policy(Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: None,
        blocked: Some(vec![peer]),
        contacts: None,
    })));
    dialed.session.set_blocked_delay_ms(0);
    dialed.session.screen_peer(peer, dialed.now_ns());
    assert!(dialed.wifi_tick(&mut |_, _| {}).is_none());
    assert!(dialed.wifi.link.is_some(), "the Wi-Fi carrier stays for the ordinary end");
    dialed.flush().expect("flush");
    // The peer reads carrier 0 (the pending SWITCH marker the cutover needs), then Wi-Fi.
    let bluetooth = read_available(&bluetooth_peer);
    let mut input = bluetooth.as_slice();
    callee.on_input_from(0, &mut input, 4_100 * MS).unwrap();
    let wifi = read_available(&peer_wire);
    let mut input = wifi.as_slice();
    callee.on_input_from(id, &mut input, 4_100 * MS).unwrap();
    assert!(
        callee.take_remote().iter().any(|m| m.signal == crate::call::Signal::HangUp),
        "the ordinary end arrived on Wi-Fi"
    );
}


/// WP6NH step 1: a call moved to Wi-Fi that wrote records while the Wi-Fi
/// socket's kernel took nothing (`hold`), then blocks its peer with the end due now.
fn blocked_after_wifi_writes(hold: bool) -> (Dialed, WifiSocket, CallSession<opus::Decoder>, WifiSocket, i32) {
    let (mut dialed, peer_wire, callee, bluetooth_peer) = live_dialed_with_bluetooth_peer();
    {
        let carriers = dialed.session.test_carriers_mut().unwrap();
        carriers.on_call_active(1_000, true);
        carriers.tick(4_000, true);
    }
    let id = dialed.wifi.id;
    assert_eq!(dialed.session.wifi_send_on(), id, "the call sends on Wi-Fi");
    if let Some(Link::Wifi(socket)) = dialed.wifi.link.as_mut() {
        socket.hold_sends(hold);
    }
    for _ in 0..3 {
        dialed.session.request_keyframe().unwrap();
        assert!(dialed.flush().expect("flush") > 0, "flush wrote nothing");
    }
    if let Some(Link::Wifi(socket)) = dialed.wifi.link.as_ref() {
        assert_eq!(socket.oldest_unsent_age(Instant::now()).is_some(), hold, "sealed Wi-Fi records wait unsent only when held");
    }
    let peer = [0x6bu8; 32];
    dialed.session.use_policy(Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: None,
        blocked: Some(vec![peer]),
        contacts: None,
    })));
    dialed.session.set_blocked_delay_ms(0);
    dialed.session.screen_peer(peer, dialed.now_ns());
    dialed.flush().expect("flush after the block");
    // The kernel takes bytes again: whatever the socket still holds goes out.
    if let Some(Link::Wifi(socket)) = dialed.wifi.link.as_mut() {
        socket.hold_sends(false);
    }
    dialed.wifi_service();
    (dialed, peer_wire, callee, bluetooth_peer, id)
}

/// The phone reads carrier 0, then the Wi-Fi carrier; returns the Wi-Fi
/// bytes, whether the Wi-Fi input was accepted, and whether the end arrived.
fn peer_reads(callee: &mut CallSession<opus::Decoder>, bluetooth_peer: &WifiSocket, peer_wire: &WifiSocket, id: i32) -> (Vec<u8>, bool, bool) {
    let now = 4_100 * MS;
    let bluetooth = read_available(bluetooth_peer);
    let mut input = bluetooth.as_slice();
    callee.on_input_from(0, &mut input, now).expect("carrier 0 input");
    let wifi = read_available(peer_wire);
    let mut input = wifi.as_slice();
    let accepted = callee.on_input_from(id, &mut input, now).is_ok();
    let ended = callee.take_remote().iter().any(|m| m.signal == crate::call::Signal::HangUp);
    (wifi, accepted, ended)
}

/// WP6NH step 1: the block dropped sealed Wi-Fi records the kernel had not
/// started, so that carrier's hop sequence has a gap; it is retired at once
/// and the ordinary end goes on Bluetooth (none is sealed on Wi-Fi).
#[test]
fn a_block_that_drops_unsent_wifi_records_retires_the_carrier_and_the_end_still_arrives() {
    let (dialed, peer_wire, mut callee, bluetooth_peer, id) = blocked_after_wifi_writes(true);
    let (wifi, accepted, ended) = peer_reads(&mut callee, &bluetooth_peer, &peer_wire, id);
    assert!(ended, "the ordinary end arrived (Wi-Fi bytes {}, accepted {accepted})", wifi.len());
    assert!(wifi.is_empty(), "nothing was sealed on the retired Wi-Fi carrier ({} bytes)", wifi.len());
    assert!(dialed.wifi.link.is_none(), "the Wi-Fi carrier is retired");
    assert!(dialed.wifi.ended.contains(&id), "its end reaches the owner");
}

/// WP6NH step 1 control: a block with nothing unsent keeps the Wi-Fi carrier
/// and the ordinary end arrives on it (WP6NG behaviour).
#[test]
fn a_block_with_nothing_unsent_keeps_the_wifi_carrier_and_the_end_arrives_on_it() {
    let (dialed, peer_wire, mut callee, bluetooth_peer, id) = blocked_after_wifi_writes(false);
    assert!(dialed.wifi.link.is_some(), "the Wi-Fi carrier stays");
    let (wifi, accepted, ended) = peer_reads(&mut callee, &bluetooth_peer, &peer_wire, id);
    assert!(accepted && !wifi.is_empty(), "the Wi-Fi records are in sequence");
    assert!(ended, "the ordinary end arrived on Wi-Fi");
}

// ---------------------------------------------------------------------------
// Design SR: the renewal owner against a fake transport (no radio opens).
// ---------------------------------------------------------------------------

use crate::bluetooth::renewal::{Failure, RenewalIo, ScanPoll, Transport};
use std::collections::VecDeque;

#[derive(Default)]
struct FakeState {
    log: Vec<&'static str>,
    sighted: bool,
    register_fails: bool,
    connects: VecDeque<Result<Link, Failure>>,
    accepts: VecDeque<Link>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeState>>);
impl Fake {
    fn log(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().log.clone()
    }
    fn count(&self, what: &str) -> usize {
        self.log().iter().filter(|line| **line == what).count()
    }
    fn sight(&self) {
        self.0.lock().unwrap().sighted = true;
    }
    fn connect_with(&self, result: Result<Link, Failure>) {
        self.0.lock().unwrap().connects.push_back(result);
    }
}
impl Transport for Fake {
    fn scan_start(&mut self, _: [u8; 16], _: Instant) -> Result<(), Failure> {
        let mut state = self.0.lock().unwrap();
        state.log.push("scan_start");
        state.sighted = false;
        Ok(())
    }
    fn scan_poll(&mut self) -> ScanPoll {
        let mut state = self.0.lock().unwrap();
        if std::mem::take(&mut state.sighted) { ScanPoll::Found("rpa") } else { ScanPoll::Pending }
    }
    fn scan_stop(&mut self) {
        self.0.lock().unwrap().log.push("scan_stop");
    }
    fn connect(&mut self, _: Instant) -> Result<Link, Failure> {
        let mut state = self.0.lock().unwrap();
        state.log.push("connect");
        state.connects.pop_front().unwrap_or(Err(Failure("refused")))
    }
    fn register(&mut self, _: [u8; 16], _: Instant) -> Result<(), Failure> {
        let mut state = self.0.lock().unwrap();
        state.log.push("register");
        if state.register_fails { Err(Failure("registration")) } else { Ok(()) }
    }
    fn advertise(&mut self, _: Instant) -> Result<(), Failure> {
        self.0.lock().unwrap().log.push("advertise");
        Ok(())
    }
    fn stop_advert(&mut self) {
        self.0.lock().unwrap().log.push("stop_advert");
    }
    fn accept(&mut self) -> Result<Option<Link>, Failure> {
        Ok(self.0.lock().unwrap().accepts.pop_front())
    }
    fn next_connection(&mut self) -> Result<(), Failure> {
        self.0.lock().unwrap().log.push("next_connection");
        Ok(())
    }
    fn close_all(&mut self) {
        self.0.lock().unwrap().log.push("close_all");
    }
}

fn quiet() -> crate::call_session::RenewalEnv {
    crate::call_session::RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns: None }
}
/// One call under test: the `Dialed` side, the peer's session, and a fake radio.
struct Rig {
    dialed: Dialed,
    peer: CallSession<opus::Decoder>,
    fake: Fake,
    io: RenewalIo,
    _bluetooth_peer: WifiSocket,
}
impl Rig {
    /// An answered call whose two ends hold the same `K_renew`; the `Dialed` is
    /// the caller or the callee.
    fn new(dialed_is_caller: bool) -> Self {
        let (mut caller, mut callee, _) = session_pair();
        caller.test_enable_renewal(&[1; 32], &[2; 32], true);
        callee.test_enable_renewal(&[2; 32], &[1; 32], true);
        let (mine, peer) = if dialed_is_caller { (caller, callee) } else { (callee, caller) };
        let (dialed, bluetooth_peer) = dialed_with_peer(mine, WifiLink::default());
        Self::from_parts(dialed, peer, bluetooth_peer)
    }
    fn from_parts(dialed: Dialed, peer: CallSession<opus::Decoder>, bluetooth_peer: WifiSocket) -> Self {
        let fake = Fake::default();
        let mut io = RenewalIo::new(Box::new(fake.clone()));
        io.force_gatt = true;
        Self { dialed, peer, fake, io, _bluetooth_peer: bluetooth_peer }
    }
    fn step(&mut self) -> Option<&'static str> {
        let mut trace = |_: &str, _: &str| {};
        self.io.step(&mut self.dialed, false, &mut trace)
    }
    /// Moves this call's clock forward and marks a Bluetooth write as just completed.
    fn advance(&mut self, ms: u64) {
        self.dialed.start -= Duration::from_millis(ms);
        self.dialed.wifi.last_bt_write_ok = Some(Instant::now());
    }
    fn now(&self) -> u64 {
        self.dialed.now_ns()
    }
    /// The call has been ACTIVE long and silent: the offerer offers.
    fn stall(&mut self) {
        self.advance(0);
        assert_eq!(self.step(), None);
        self.advance(12_000);
        assert_eq!(self.step(), None);
        assert_eq!(self.fake.log(), vec!["scan_start"], "the offer scans for the advert");
    }
    /// The OFFER the `Dialed` wrote to the old link (taken at `flush_at` on its
    /// clock) reaches the peer, which arms by hand (the peer is a session, not an owner).
    fn deliver_offer(&mut self, flush_at: u64) {
        let wire = flush(&mut self.dialed.session, flush_at);
        let at = self.now();
        self.peer.renewal_tick(5 * MS, quiet());
        feed(&mut self.peer, &wire, at);
        let armed = self.peer.renewal_tick(at, quiet());
        assert!(armed.iter().any(|a| matches!(a, crate::call_renewal::Action::RegisterServer { .. })), "{armed:?}");
        self.peer.renewal_registered(at, true);
        self.peer.renewal_advertising(at, true);
    }
}

/// The armed phone's end of one connection: the opening, then the raw bytes
/// of the first BMS1 record (BIND). Returns the hop keys and those bytes.
fn armed_phone(
    socket: WifiSocket,
    key: zeroize::Zeroizing<[u8; 32]>,
    call_id: [u8; 16],
    ticket: [u8; 8],
) -> std::thread::JoinHandle<(zeroize::Zeroizing<[u8; 64]>, Vec<u8>)> {
    std::thread::spawn(move || {
        let mut link = Link::Wifi(socket);
        let mut opening = crate::call_renewal::Opening::armed(key, call_id, ticket, [9; 16]);
        let mut buffer = crate::bluetooth::renewal::open_as_armed(&mut link, &mut opening, Instant::now()).unwrap();
        let keys = opening.hop_keys().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // One BMS1 record: a 17-byte header whose last four bytes are the ciphertext length.
        loop {
            if buffer.len() >= 17 {
                let length = u32::from_be_bytes(buffer[13..17].try_into().unwrap()) as usize;
                if buffer.len() >= 17 + length {
                    buffer.truncate(17 + length);
                    return (keys, buffer);
                }
            }
            assert!(Instant::now() < deadline, "no BIND");
            pull_link(&link, &mut buffer, Duration::from_millis(50)).unwrap();
        }
    })
}

/// The peer session reads the BIND the armed-phone thread received and binds.
fn peer_binds(peer: &mut CallSession<opus::Decoder>, keys: &[u8; 64], bind: &[u8], now: u64) {
    let (out, mut inn) = peer.renewal_hop_layer(false, keys).unwrap();
    let mut input = bind;
    let record = inn.read(&mut input, crate::records::Layer::Hop.maximum_ciphertext()).unwrap().unwrap();
    assert_eq!(record.header.sequence, 0, "BIND is the new link's first record");
    assert!(peer.renewal_accept_bind(&record.plaintext, now), "the armed phone accepts BIND as the first record");
    peer.renewal_bound(out, inn, now);
}

/// Hands the fake a link whose far end is the armed-phone thread, then reports a sighting.
fn connect_to_armed_phone(rig: &mut Rig) -> std::thread::JoinHandle<(zeroize::Zeroizing<[u8; 64]>, Vec<u8>)> {
    let (client, server) = socket_pair();
    rig.fake.connect_with(Ok(Link::Wifi(client)));
    armed_phone(server, rig.peer.renewal_key().unwrap(), rig.peer.renewal_call_id(), rig.peer.renewal_ticket())
}

/// Runs a whole offerer-side renewal through the fake radio: the advert is
/// found, the old link closed, the opening and BIND run against a thread
/// playing the armed phone, and the peer session binds too.
fn run_offerer_renewal(rig: &mut Rig) -> u64 {
    let at = rig.now();
    rig.deliver_offer(at);
    let phone = connect_to_armed_phone(rig);
    rig.advance(1_000);
    rig.fake.sight();
    assert_eq!(rig.step(), None);
    let (keys, bind) = phone.join().unwrap();
    let now = rig.now();
    peer_binds(&mut rig.peer, &keys, &bind, now);
    now
}

#[test]
fn a_renewal_starts_only_after_ten_seconds_active_and_four_seconds_of_silence() {
    let mut rig = Rig::new(true);
    // The peer's heartbeat, to be heard once the call has been ACTIVE long enough.
    let heartbeat = flush(&mut rig.peer, 4_000 * MS);
    rig.advance(0);
    assert_eq!(rig.step(), None);
    rig.advance(5_000);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty(), "under ten seconds ACTIVE nothing is offered");
    rig.advance(6_000);
    let now = rig.now();
    feed(&mut rig.dialed.session, &heartbeat, now);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty(), "the peer was heard just now: no silence, no offer");
    rig.advance(3_000);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty(), "under four seconds of silence");
    rig.advance(1_500);
    assert_eq!(rig.step(), None);
    assert_eq!(rig.fake.log(), vec!["scan_start"]);
    assert!(rig.dialed.session.renewal_running());
}

#[test]
fn no_offer_while_own_writes_do_not_complete() {
    let mut rig = Rig::new(true);
    rig.advance(0);
    rig.step();
    rig.advance(12_000);
    // The last write to complete is old: this phone cannot say its own direction works.
    rig.dialed.wifi.last_bt_write_ok = Some(Instant::now() - Duration::from_millis(2_500));
    rig.step();
    assert!(rig.fake.log().is_empty());
    rig.dialed.wifi.last_bt_write_ok = None;
    rig.step();
    assert!(rig.fake.log().is_empty());
}

#[test]
fn the_old_link_is_kept_until_the_advert_is_found_and_closed_only_then_and_after_bind() {
    let mut rig = Rig::new(true);
    rig.stall();
    assert!(matches!(rig.dialed.link, Link::Wifi(_)), "scanning does not touch the old link");
    assert!(!rig.dialed.wifi.bt_dead);
    let at = rig.now();
    rig.deliver_offer(at);
    // The advert is found but the first connection is refused: the old link is already given up.
    rig.fake.connect_with(Err(Failure("refused")));
    rig.advance(500);
    rig.fake.sight();
    rig.step();
    assert!(matches!(rig.dialed.link, Link::Closed), "closed after the sighting, not at OFFER");
    assert!(rig.dialed.wifi.bt_dead);
    assert_eq!(rig.fake.log(), vec!["scan_start", "scan_stop", "connect", "scan_start"], "found, close, connect, look again");
    // Another sighting: this time the opening and BIND succeed and the new link replaces it.
    let phone = connect_to_armed_phone(&mut rig);
    rig.advance(500);
    rig.fake.sight();
    rig.step();
    let (keys, bind) = phone.join().unwrap();
    let now = rig.now();
    peer_binds(&mut rig.peer, &keys, &bind, now);
    assert!(matches!(rig.dialed.link, Link::Wifi(_)), "the new link carries the call");
    assert!(!rig.dialed.wifi.bt_dead);
    assert_eq!(rig.dialed.session.renewal_phase(), crate::call_renewal::Phase::Bound);
    assert!(!rig.dialed.session.renewal_running());
    let log = rig.fake.log();
    let connected = log.iter().rposition(|line| *line == "connect").unwrap();
    assert!(log[..connected].iter().all(|line| *line != "close_all"), "nothing is released before BIND: {log:?}");
    assert_eq!(log.last(), Some(&"close_all"));
}

#[test]
fn bind_swaps_only_the_hop_layer_and_keeps_the_end_to_end_sequence() {
    let mut rig = Rig::new(true);
    rig.stall();
    let key = rig.dialed.session.renewal_key().unwrap();
    let before = rig.dialed.session.next_content_sequence();
    // Bytes the old link had delivered but nobody had consumed are dropped unopened.
    rig.dialed.buffer.extend_from_slice(b"old link bytes");
    let bound_at = run_offerer_renewal(&mut rig);
    assert!(rig.dialed.buffer.is_empty(), "nothing of the old link is processed after the switch");
    assert_eq!(rig.dialed.session.renewal_key().unwrap().as_slice(), key.as_slice(), "the end-to-end session and its K_renew continue");
    assert!(rig.dialed.session.next_content_sequence() > before, "BIND took the next end-to-end sequence");
    // The call goes on over the new hop keys, in the same end-to-end sequence space.
    let wire = flush(&mut rig.dialed.session, bound_at + MS);
    let peer_input: Vec<u8> = wire.iter().flat_map(|(_, bytes)| bytes.clone()).collect();
    let notices = rig.peer.on_input(&mut peer_input.as_slice(), bound_at + MS).unwrap();
    assert!(notices.contains(&Notice::KeyFrameRequested), "{notices:?}");
}

#[test]
fn a_renewal_that_finds_nothing_leaves_the_old_link_and_resumes_liveness() {
    let mut rig = Rig::new(true);
    rig.stall();
    rig.advance(5_000);
    rig.step();
    assert!(rig.dialed.session.renewal_running());
    rig.advance(1_500);
    assert_eq!(rig.step(), None, "abandoning does not end the call by itself");
    assert!(matches!(rig.dialed.link, Link::Wifi(_)), "the old link was never broken");
    assert!(!rig.dialed.session.renewal_running());
    assert!(!rig.fake.log().contains(&"connect"));
    assert_eq!(rig.fake.count("close_all"), 1, "the scan stops");
    // The ordinary rules apply at once: the silence is long, so the call is dead by them.
    assert!(rig.dialed.session.is_dead(rig.now()));
}

#[test]
fn the_offerer_ends_at_twelve_seconds_and_the_armed_side_does_not_hang() {
    let mut rig = Rig::new(true);
    rig.stall();
    let at = rig.now();
    rig.deliver_offer(at);
    rig.advance(1_000);
    rig.fake.sight();
    rig.step();
    assert!(matches!(rig.dialed.link, Link::Closed));
    rig.advance(9_000);
    assert_eq!(rig.step(), None, "inside the bound");
    rig.advance(2_500);
    assert_eq!(rig.step(), Some("link_lost"), "12 s after the offer");
    assert_eq!(rig.step(), None, "once");
    // The armed phone's own bound: its offerer kept nothing alive, but it never hangs the call.
    let end = rig.peer.renewal_tick(at + 12_500 * MS, quiet());
    assert!(!end.iter().any(|a| matches!(a, crate::call_renewal::Action::EndCall(_))));
    assert!(!rig.peer.renewal_running());
}

#[test]
fn a_call_with_a_live_wifi_carrier_neither_offers_nor_arms_and_its_carrier_zero_is_untouched() {
    let (mut dialed, _wifi_far_end, mut callee, bluetooth_peer) = live_dialed_with_bluetooth_peer();
    dialed.session.test_enable_renewal(&[1; 32], &[2; 32], true);
    callee.test_enable_renewal(&[2; 32], &[1; 32], true);
    let (id, send_on) = (dialed.wifi.id, dialed.session.wifi_send_on());
    let mut rig = Rig::from_parts(dialed, callee, bluetooth_peer);
    // A stalled Bluetooth standby: long ACTIVE, long silent, own writes fine. Nothing is offered.
    rig.advance(0);
    assert_eq!(rig.step(), None);
    rig.advance(12_000);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty(), "no offer while a Wi-Fi carrier is live: {:?}", rig.fake.log());
    assert!(!rig.dialed.session.renewal_running());
    // The peer offers anyway: it is refused, nothing is registered or advertised.
    rig.peer.renewal_tick(5 * MS, quiet());
    rig.peer.send_renew(crate::call_renewal::encode(&crate::call_renewal::Op::Offer { ticket: [9; 8], role: 0 }));
    let wire = flush(&mut rig.peer, 4_000 * MS);
    let at = rig.now();
    feed(&mut rig.dialed.session, &wire, at);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty());
    assert!(!rig.dialed.session.renewal_running());
    assert_eq!(rig.dialed.session.renewal_phase(), crate::call_renewal::Phase::Idle);
    // Carrier-0 accounting is exactly what it was.
    assert!(rig.dialed.wifi.link.is_some());
    assert_eq!(rig.dialed.wifi.id, id);
    assert_eq!(rig.dialed.session.wifi_send_on(), send_on);
    assert!(!rig.dialed.wifi.bt_dead);
    assert!(rig.dialed.session.wifi_old_alive());
}

/// The answering side's owner: the OFFER arrives on the old link of a `Dialed` that is the callee.
fn armed_rig() -> Rig {
    let mut rig = Rig::new(false);
    rig.dialed.session.renewal_tick(5 * MS, quiet());
    rig.advance(12_000);
    // The calling session offers; its OFFER is read by the owner's session.
    rig.peer.renewal_tick(5 * MS, crate::call_session::RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns: Some(5 * MS) });
    let at = rig.now();
    rig.peer.renewal_tick(at, crate::call_session::RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns: Some(at) });
    let wire = flush(&mut rig.peer, 4_000 * MS);
    feed(&mut rig.dialed.session, &wire, at);
    rig
}

#[test]
fn the_armed_owner_registers_advertises_and_never_touches_the_live_link_for_a_second_offer() {
    let mut rig = armed_rig();
    assert_eq!(rig.step(), None);
    assert_eq!(rig.fake.log(), vec!["register", "advertise"]);
    assert!(rig.dialed.session.renewal_running());
    assert!(matches!(rig.dialed.link, Link::Wifi(_)));
    // A second OFFER (another ticket) while the first attempt is live is refused and closes nothing.
    rig.peer.send_renew(crate::call_renewal::encode(&crate::call_renewal::Op::Offer { ticket: [9; 8], role: 0 }));
    let wire = flush(&mut rig.peer, 4_000 * MS);
    let at = rig.now();
    feed(&mut rig.dialed.session, &wire, at);
    assert_eq!(rig.step(), None);
    assert_eq!(rig.fake.log(), vec!["register", "advertise"], "no second server, no close, no stopped advert");
    assert!(rig.dialed.session.renewal_running());
}

#[test]
fn a_registration_the_radio_refuses_leaves_the_old_link_alone() {
    let mut rig = armed_rig();
    rig.fake.0.lock().unwrap().register_fails = true;
    assert_eq!(rig.step(), None);
    assert!(!rig.dialed.session.renewal_running(), "refused: liveness goes back to the ordinary rules");
    assert!(matches!(rig.dialed.link, Link::Wifi(_)));
    assert_eq!(rig.fake.count("advertise"), 0);
}

#[test]
fn a_squatter_is_dropped_and_the_next_connection_is_served() {
    let mut rig = armed_rig();
    rig.step();
    let (squatter, mut far) = socket_pair();
    // It subscribed and sent a wrong proof; the server drops it and serves the next one.
    far.write(&[0u8; 60], Instant::now() + Duration::from_secs(1)).unwrap();
    rig.fake.0.lock().unwrap().accepts.push_back(Link::Wifi(squatter));
    rig.step();
    assert_eq!(rig.fake.count("next_connection"), 1, "the next connection is served afresh");
    assert!(rig.dialed.session.renewal_running(), "one squatter does not end the attempt");
    assert!(matches!(rig.dialed.link, Link::Wifi(_)));
}

#[test]
fn a_proof_with_an_invalid_first_record_fails_the_renewal_and_keeps_the_old_link() {
    let mut rig = armed_rig();
    rig.step();
    let (mine, far) = socket_pair();
    rig.fake.0.lock().unwrap().accepts.push_back(Link::Wifi(mine));
    let key = rig.peer.renewal_key().unwrap();
    let (call_id, ticket) = (rig.peer.renewal_call_id(), rig.peer.renewal_ticket());
    let offerer = std::thread::spawn(move || {
        let mut link = Link::Wifi(far);
        let mut opening = crate::call_renewal::Opening::offerer(key, call_id, ticket, [3; 16]);
        crate::bluetooth::renewal::open_as_offerer(&mut link, &mut opening, Instant::now() + Duration::from_secs(5)).unwrap();
        // The right proof, but the first record is not a BIND.
        link.write(&[7u8; 40], Instant::now() + Duration::from_secs(1)).unwrap();
    });
    rig.step();
    offerer.join().unwrap();
    assert!(!rig.dialed.session.renewal_running());
    assert!(matches!(rig.dialed.link, Link::Wifi(_)), "the old link is never given up by the armed side");
    assert!(rig.fake.count("close_all") >= 1);
}

#[test]
fn no_wifi_attempt_starts_while_a_renewal_runs() {
    let previous = std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", "1"); }
    // The Wi-Fi owner is ticked on a call that has video and exchanged CAPS; the
    // attempt starts, unless a renewal of the Bluetooth hop is running.
    let state_after_ticks = |renewal_running: bool| -> &'static str {
        let mut rig = Rig::new(true);
        prepare_caps(&mut rig.dialed);
        rig.dialed.wifi.armed = true;
        rig.dialed.wifi.owner = Some(Box::new(attempt::Owner::new()));
        let (host_socket, _guest_stream) = attempt::fake::socket_pair();
        let mut fake = FakePlatform::new();
        fake.accepts.push_back(host_socket);
        rig.dialed.wifi.platform = Some(Box::new(fake));
        rig.dialed.wifi.video_since_ms = Some(0);
        if renewal_running {
            rig.stall();
            assert!(rig.dialed.session.renewal_running());
        } else {
            rig.advance(12_000);
        }
        for _ in 0..20 {
            rig.dialed.wifi_tick(&mut |_, _| {});
            std::thread::sleep(Duration::from_millis(2));
        }
        rig.dialed.wifi.owner.as_ref().unwrap().state_name()
    };
    assert_ne!(state_after_ticks(false), "idle", "without a renewal the attempt starts");
    assert_eq!(state_after_ticks(true), "idle", "while a renewal runs, no attempt starts");
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", value),
            None => std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"),
        }
    }
}

#[test]
fn a_call_whose_hop_is_not_gatt_never_renews() {
    // The production fact, not the test override: a `Link::Wifi` hop is not GATT.
    let mut rig = Rig::new(true);
    rig.io.force_gatt = false;
    assert!(!rig.dialed.renewal_env().gatt);
    rig.advance(0);
    assert_eq!(rig.step(), None);
    rig.advance(12_000);
    assert_eq!(rig.step(), None);
    assert!(rig.fake.log().is_empty());
    assert!(!rig.dialed.session.renewal_running());
}

#[test]
fn no_connection_is_served_again_once_the_seven_second_advert_window_ended() {
    let mut rig = armed_rig();
    rig.step();
    assert_eq!(rig.fake.log(), vec!["register", "advertise"]);
    rig.advance(7_500);
    rig.step();
    assert_eq!(rig.fake.count("stop_advert"), 1, "the advert stops at seven seconds");
    let (squatter, mut far) = socket_pair();
    far.write(&[0u8; 60], Instant::now() + Duration::from_secs(1)).unwrap();
    rig.fake.0.lock().unwrap().accepts.push_back(Link::Wifi(squatter));
    rig.step();
    assert_eq!(rig.fake.count("next_connection"), 0, "the advert is never put back on air after its window");
    assert_eq!(rig.fake.count("register"), 1);
    assert!(matches!(rig.dialed.link, Link::Wifi(_)), "the old link is untouched");
}

#[test]
fn a_first_record_that_is_a_hop_record_but_not_a_bind_is_refused() {
    for (kind, payload) in [(6u8, crate::link::keepalive().to_vec()), (5u8, vec![1u8; 40])] {
        let mut rig = armed_rig();
        rig.step();
        let (mine, far) = socket_pair();
        rig.fake.0.lock().unwrap().accepts.push_back(Link::Wifi(mine));
        let key = rig.peer.renewal_key().unwrap();
        let (call_id, ticket) = (rig.peer.renewal_call_id(), rig.peer.renewal_ticket());
        let offerer = std::thread::spawn(move || {
            let mut link = Link::Wifi(far);
            let mut opening = crate::call_renewal::Opening::offerer(key, call_id, ticket, [3; 16]);
            crate::bluetooth::renewal::open_as_offerer(&mut link, &mut opening, Instant::now() + Duration::from_secs(5)).unwrap();
            // A genuine hop record under the right keys, as the first record: not a BIND.
            let keys = opening.hop_keys().unwrap();
            let mut hop_out = Sealer::new(Layer::Hop, keys[..32].try_into().unwrap(), Policy::default()).unwrap();
            let record = hop_out.seal(kind, 0, &payload).unwrap();
            link.write(&record, Instant::now() + Duration::from_secs(1)).unwrap();
        });
        rig.step();
        offerer.join().unwrap();
        assert!(!rig.dialed.session.renewal_running(), "kind {kind}");
        assert_eq!(rig.dialed.session.renewal_phase(), crate::call_renewal::Phase::Failed, "kind {kind}");
        assert!(matches!(rig.dialed.link, Link::Wifi(_)), "the old link is kept");
    }
}

#[test]
fn the_dialers_real_endpoint_hello_carries_bit_four_only_for_a_direct_call_with_the_switch_on() {
    for link_transport in [0x0cu8, 0x08] {
        for relay_hops in [0u8, 1] {
            let off = super::dialer_endpoint_transport(link_transport, relay_hops, false);
            assert_eq!(off & crate::call_renewal::CAPABILITY_BIT, 0, "switch off: {off:#04x}");
            let on = super::dialer_endpoint_transport(link_transport, relay_hops, true);
            assert_eq!(on & crate::call_renewal::CAPABILITY_BIT != 0, relay_hops == 0, "switch on, hops {relay_hops}");
        }
    }
}
