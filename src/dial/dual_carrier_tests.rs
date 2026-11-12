#![cfg(test)]

use super::*;
use crate::{
    call_session::wifi_test_support,
    wifi::{
        quality::QualitySampler,
        wire::{self, Bwf1},
    },
};
use std::{net::TcpListener, time::Duration};

const MS: u64 = 1_000_000;

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

fn flush_session(session: &mut CallSession<opus::Decoder>, now: u64) -> Vec<(i32, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(write) = session.next_write_on(now).unwrap() {
        out.push((write.carrier, write.bytes));
    }
    out
}

fn feed_session(session: &mut CallSession<opus::Decoder>, records: &[(i32, Vec<u8>)], now: u64) {
    for (carrier, bytes) in records {
        let mut input = bytes.as_slice();
        session.on_input_from(*carrier, &mut input, now).unwrap();
    }
}

fn make(description: &[u8]) -> Result<opus::Decoder, opus::Error> {
    opus::Decoder::new(description, opus::Profile::StreamOrCall)
}

fn rig() -> (Dialed, CallSession<opus::Decoder>, WifiSocket, WifiSocket) {
    let (mut caller, mut callee, _) = wifi_test_support::wifi_session_pair_with(make, false, false);
    feed_session(&mut caller, &flush_session(&mut callee, 0), 0);
    callee.accept(false).unwrap();
    feed_session(&mut caller, &flush_session(&mut callee, MS), MS);
    caller.media_ready().unwrap();
    feed_session(&mut callee, &flush_session(&mut caller, 2 * MS), 2 * MS);
    callee.media_ready().unwrap();
    let ticket = [9u8; 8];
    let id = caller.wifi_begin_attempt(ticket, 0).unwrap();
    assert_eq!(callee.wifi_begin_attempt(ticket, 0).unwrap(), id);
    let policy = crate::records::Policy {
        wifi: true,
        ..crate::records::Policy::default()
    };
    let ready = wire::Ready {
        recv_ceiling_kbps: 1100,
        family_cap_kbps: 0,
        transport_budget_kbps: 1100,
    };
    caller.wifi_attach(
        id,
        Sealer::new(crate::records::Layer::Hop, &[1; 32], policy).unwrap(),
        HopReader::new(&[2; 32], policy).unwrap(),
        ready,
        0,
    );
    callee.wifi_attach(
        id,
        Sealer::new(crate::records::Layer::Hop, &[2; 32], policy).unwrap(),
        HopReader::new(&[1; 32], policy).unwrap(),
        ready,
        0,
    );
    let (wire, peer_wire) = socket_pair();
    let (dummy, dummy_peer) = socket_pair();
    let mut wifi = WifiLink::default();
    wifi.link = Some(Link::Wifi(wire));
    wifi.id = id;
    wifi.quality = Some(QualitySampler::new(0));
    let dialed = Dialed {
        link: Link::Wifi(dummy),
        buffer: Vec::new(),
        session: caller,
        start: Instant::now(),
        peer_discovery: false,
        paired: false,
        header: wifi_test_support::header(),
        video_plan: Vec::new(),
        peer_real: None,
        wifi,
    };
    (dialed, callee, peer_wire, dummy_peer)
}

fn switch_session(session: &mut CallSession<opus::Decoder>) {
    session
        .send(wifi_test_support::packet(0, 0, 100, 1))
        .unwrap();
    let carriers = session.test_carriers_mut().unwrap();
    carriers.on_call_active(0, true);
    carriers.tick(4_000, true);
}

fn switch_dialed(dialed: &mut Dialed) {
    switch_session(&mut dialed.session);
    dialed.start = Instant::now() - Duration::from_millis(4_100);
}

fn accept_wifi(dialed: &mut Dialed) {
    dialed
        .session
        .test_carriers_mut()
        .unwrap()
        .carrier_died(0, 4_100);
}

#[test]
fn input_from_the_wifi_carrier_is_fed_to_the_session_and_the_call_goes_on_without_bluetooth() {
    let (mut dialed, mut peer, mut peer_wire, _dummy) = rig();
    dialed.wifi.bt_dead = true;
    switch_dialed(&mut dialed);
    accept_wifi(&mut dialed);
    let id = dialed.wifi.id;
    switch_session(&mut peer);
    peer.set_camera(false).unwrap();
    for (carrier, bytes) in flush_session(&mut peer, 4_200) {
        if carrier == id {
            peer_wire
                .write(&bytes, Instant::now() + Duration::from_millis(100))
                .unwrap();
        }
    }
    let notices = dialed.pump_input(Duration::from_millis(50)).unwrap();
    assert!(notices.contains(&crate::call_session::Notice::PeerCamera(false)));
}

#[test]
fn a_wifi_read_error_with_bluetooth_alive_falls_back_and_never_ends_the_call() {
    let (mut dialed, _peer, peer_wire, _dummy) = rig();
    switch_dialed(&mut dialed);
    assert_eq!(dialed.wifi_transport(), Some("Wi-Fi"));
    drop(peer_wire);
    assert!(dialed.pump_input(Duration::from_millis(10)).is_ok());
    assert!(dialed.wifi.link.is_none());
    assert_eq!(dialed.wifi_transport(), Some("Bluetooth"));
}

#[test]
fn a_wifi_stall_in_flush_drops_only_the_wifi_carrier() {
    let (mut dialed, _peer, _peer_wire, _dummy) = rig();
    switch_dialed(&mut dialed);
    dialed
        .session
        .send(wifi_test_support::packet(0, 0, 100, 2))
        .unwrap();
    if let Some(Link::Wifi(socket)) = dialed.wifi.link.as_mut() {
        socket.force_stall();
    }
    assert!(dialed.flush().is_ok());
    assert!(dialed.wifi.link.is_none());
}

#[test]
fn the_effective_transport_is_reported_once_per_change() {
    let (mut dialed, _peer, peer_wire, _dummy) = rig();
    assert_eq!(dialed.wifi_transport(), None);
    switch_dialed(&mut dialed);
    assert_eq!(dialed.wifi_transport(), Some("Wi-Fi"));
    assert_eq!(dialed.wifi_transport(), None);
    drop(peer_wire);
    assert!(dialed.pump_input(Duration::from_millis(10)).is_ok());
    assert_eq!(dialed.wifi_transport(), Some("Bluetooth"));
    assert_eq!(dialed.wifi_transport(), None);
}

#[test]
fn mark_bt_dead_keeps_a_call_that_has_a_wifi_carrier() {
    let (mut dialed, _peer, _peer_wire, _dummy) = rig();
    assert!(dialed.mark_bt_dead());
    assert!(dialed.wifi.bt_dead);
    assert!(dialed.flush().is_ok());
    assert!(dialed.pump_input(Duration::from_millis(1)).is_ok());
}

#[test]
fn mark_bt_dead_without_a_wifi_carrier_changes_nothing_and_reports_the_call_over() {
    let (mut dialed, _peer, _peer_wire, _dummy) = rig();
    dialed.wifi.link = None;
    dialed.wifi.id = 0;
    assert!(!dialed.mark_bt_dead());
    assert!(!dialed.wifi.bt_dead);
}

#[test]
fn a_dead_bluetooth_link_is_not_serviced_while_wifi_lives() {
    let (mut dialed, _peer, _peer_wire, _dummy) = rig();
    dialed.wifi.bt_dead = true;
    assert!(dialed.service_bt().is_ok());
    assert!(dialed.flush().is_ok());
    assert!(dialed.pump_input(Duration::from_millis(1)).is_ok());
}

#[test]
fn a_bluetooth_service_error_without_wifi_still_ends_the_call() {
    let (mut dialed, _peer, _peer_wire, _dummy) = rig();
    dialed.wifi.link = None;
    dialed.wifi.id = 0;
    if let Link::Wifi(socket) = &mut dialed.link {
        socket.force_stall();
    }
    assert!(dialed.service_bt().is_err());
}

#[test]
fn the_bluetooth_write_budget_is_one_second_while_wifi_lives() {
    assert_eq!(bt_write_budget(true, false), Duration::from_millis(300));
    assert_eq!(bt_write_budget(false, true), Duration::from_millis(1_000));
    assert_eq!(bt_write_budget(false, false), Duration::from_secs(8));
    assert_eq!(BT_WRITE_BUDGET_WITH_WIFI, Duration::from_millis(1_000));
}

#[test]
fn a_wifi_read_error_ends_the_wifi_carrier_but_a_carrier_error_from_the_session_is_only_that_carrier()
 {
    let (mut dialed, mut peer, mut peer_wire, _dummy) = rig();
    dialed.wifi.bt_dead = true;
    switch_dialed(&mut dialed);
    accept_wifi(&mut dialed);
    switch_session(&mut peer);
    let id = dialed.wifi.id;
    let caps = peer
        .test_seal_bwf1(id, &Bwf1::Caps(wire::Caps::default()))
        .unwrap();
    peer_wire
        .write(&caps, Instant::now() + Duration::from_millis(100))
        .unwrap();
    assert!(dialed.pump_input(Duration::from_millis(50)).is_ok());
    assert!(dialed.wifi.link.is_none());
}

fn ended_normally(notices: &[crate::call_session::Notice]) -> bool {
    notices.iter().any(|notice| {
        matches!(
            notice,
            crate::call_session::Notice::Ended(crate::call::Reason::Normal)
        )
    })
}

#[test]
fn a_hang_up_the_phone_writes_on_wifi_right_before_closing_ends_the_call_normally() {
    let (mut dialed, mut peer, mut peer_wire, _dummy) = rig();
    dialed.wifi.bt_dead = true;
    switch_dialed(&mut dialed);
    accept_wifi(&mut dialed);
    let id = dialed.wifi.id;
    switch_session(&mut peer);
    peer.hang_up(crate::call::Reason::Normal).unwrap();
    for (carrier, bytes) in flush_session(&mut peer, 4_200) {
        if carrier == id {
            peer_wire
                .write(&bytes, Instant::now() + Duration::from_millis(100))
                .unwrap();
        }
    }
    // The phone closes its socket in the same go: the read after the bytes fails.
    peer_wire.shutdown();
    let notices = dialed.pump_input(Duration::from_millis(50)).unwrap();
    assert!(ended_normally(&notices), "{notices:?}");
    assert!(dialed.wifi.link.is_none());
}

#[test]
fn a_hang_up_already_received_when_the_wifi_write_stalls_is_not_lost() {
    let (mut dialed, mut peer, mut peer_wire, mut dummy_peer) = rig();
    switch_dialed(&mut dialed);
    let id = dialed.wifi.id;
    // The phone switches too: its SWITCH reaches this PC over Bluetooth.
    switch_session(&mut peer);
    for (carrier, bytes) in flush_session(&mut peer, 4_200) {
        let wire = if carrier == 0 { &mut dummy_peer } else { &mut peer_wire };
        wire.write(&bytes, Instant::now() + Duration::from_millis(100))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(20));
    dialed.pump_input(Duration::from_millis(1)).unwrap();
    peer.hang_up(crate::call::Reason::Normal).unwrap();
    for (carrier, bytes) in flush_session(&mut peer, 4_300) {
        if carrier == id {
            peer_wire
                .write(&bytes, Instant::now() + Duration::from_millis(100))
                .unwrap();
        }
    }
    std::thread::sleep(Duration::from_millis(20));
    dialed
        .session
        .send(wifi_test_support::packet(0, 0, 100, 2))
        .unwrap();
    if let Some(Link::Wifi(socket)) = dialed.wifi.link.as_mut() {
        socket.force_stall();
    }
    // The stall is met in flush, before this tick pumped any input.
    assert!(dialed.flush().is_ok());
    assert!(dialed.wifi.link.is_none());
    let notices = dialed.pump_input(Duration::from_millis(1)).unwrap();
    assert!(ended_normally(&notices), "{notices:?}");
}

#[test]
fn records_bluetooth_delivered_before_it_failed_are_read_while_wifi_carries_the_call() {
    let (mut dialed, mut peer, _peer_wire, mut dummy_peer) = rig();
    peer.set_camera(false).unwrap();
    for (carrier, bytes) in flush_session(&mut peer, 3 * MS) {
        assert_eq!(carrier, 0);
        dummy_peer
            .write(&bytes, Instant::now() + Duration::from_millis(100))
            .unwrap();
    }
    dummy_peer.shutdown();
    std::thread::sleep(Duration::from_millis(20));
    let notices = dialed.pump_input(Duration::from_millis(10)).unwrap();
    assert!(
        notices.contains(&crate::call_session::Notice::PeerCamera(false)),
        "{notices:?}"
    );
    assert!(dialed.wifi.bt_dead);
    assert!(dialed.wifi.link.is_some());
}
