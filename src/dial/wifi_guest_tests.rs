#![cfg(test)]
//! W4G: the guest role through the session and the call glue (production seams).

use super::*;
use crate::{
    call_session::{Notice, WifiNotice},
    wifi::{
        roles,
        wire::{self, caps_bit},
    },
};
use std::net::TcpListener;

const MS: u64 = 1_000_000;

fn make(description: &[u8]) -> Result<opus::Decoder, opus::Error> {
    opus::Decoder::new(description, opus::Profile::StreamOrCall)
}

fn flush(session: &mut CallSession<opus::Decoder>, now: u64) -> Vec<(i32, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(write) = session.next_write_on(now).unwrap() {
        out.push((write.carrier, write.bytes));
    }
    out
}

fn feed(session: &mut CallSession<opus::Decoder>, records: &[(i32, Vec<u8>)], now: u64) -> Vec<Notice> {
    let mut notices = Vec::new();
    for (carrier, bytes) in records {
        let mut input = bytes.as_slice();
        notices.extend(session.on_input_from(*carrier, &mut input, now).unwrap());
    }
    notices
}

/// An active call: the PC is the caller (link initiator), the phone the callee.
fn active_pair() -> (CallSession<opus::Decoder>, CallSession<opus::Decoder>) {
    let (mut pc, mut phone, _) =
        crate::call_session::wifi_test_support::wifi_session_pair_with(make, false, false);
    feed(&mut pc, &flush(&mut phone, 0), 0);
    phone.accept(false).unwrap();
    feed(&mut pc, &flush(&mut phone, MS), MS);
    pc.media_ready().unwrap();
    feed(&mut phone, &flush(&mut pc, 2 * MS), 2 * MS);
    phone.media_ready().unwrap();
    (pc, phone)
}

fn phone_caps() -> wire::Caps {
    wire::Caps {
        bearers: caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT,
        free_host: 0,
        free_guest: 0,
    }
}

/// Both CAPS as if exchanged on the wire: the PC sent `pc_caps`, the phone GO|CLIENT.
fn exchange_caps(pc: &mut CallSession<opus::Decoder>, pc_caps: wire::Caps, phone: &mut CallSession<opus::Decoder>) {
    pc.wifi_set_local_caps(pc_caps);
    let carriers = pc.test_carriers_mut().unwrap();
    carriers.set_peer_caps(phone_caps());
    carriers.mark_caps_sent();
    carriers.note_peer_bwf1();
    phone.wifi_set_local_caps(phone_caps());
    let carriers = phone.test_carriers_mut().unwrap();
    carriers.set_peer_caps(pc_caps);
    carriers.mark_caps_sent();
    carriers.note_peer_bwf1();
}

#[test]
fn creds_reach_a_pc_that_offered_the_client_bit_and_end_wifi_for_one_that_did_not() {
    for (pc_caps, guest) in [(roles::windows_guest_caps(true), true), (roles::windows_caps(true), false)] {
        let (mut pc, mut phone) = active_pair();
        exchange_caps(&mut pc, pc_caps, &mut phone);
        let creds = wire::Creds {
            ticket: [3; 8],
            port: 47_000,
            ipv4: [192, 168, 49, 1],
        };
        phone.wifi_send(wire::Bwf1::Creds(creds)).unwrap();
        let notices = feed(&mut pc, &flush(&mut phone, 5 * MS), 5 * MS);
        let got = notices
            .iter()
            .any(|notice| matches!(notice, Notice::Wifi(WifiNotice::PeerCreds(c)) if *c == creds));
        assert_eq!(got, guest, "guest {guest}");
        assert_eq!(pc.wifi_attempts_allowed(), guest, "a host PC ends Wi-Fi for the call on CREDS");
        assert!(!format!("{notices:?}").contains("192.168.49"), "the address is never printed");
    }
}

use crate::wifi::{
    binding::{HostBinding, Step},
    guest_platform::fake::{FakeGuestPlatform, FakeState},
};

fn socket_pair() -> (WifiSocket, WifiSocket) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        WifiSocket::from_stream(client).unwrap(),
        WifiSocket::from_stream(server).unwrap(),
    )
}

fn dialed(session: CallSession<opus::Decoder>) -> Dialed {
    let (carrier0, _far_end) = socket_pair();
    Dialed {
        link: Link::Wifi(carrier0),
        buffer: Vec::new(),
        session,
        start: Instant::now() - Duration::from_millis(4_000),
        peer_discovery: false,
        paired: false,
        header: crate::call_session::wifi_test_support::header(),
        video_plan: Vec::new(),
        peer_real: None,
        wifi: WifiLink::default(),
    }
}

/// An active call whose CAPS made this PC the guest, with the fake platform armed.
fn guest_dialed() -> (Dialed, std::sync::Arc<std::sync::Mutex<FakeState>>) {
    let (mut pc, mut phone) = active_pair();
    exchange_caps(&mut pc, roles::windows_guest_caps(true), &mut phone);
    let mut dialed = dialed(pc);
    let fake = FakeGuestPlatform::ready();
    let state = fake.state.clone();
    dialed.guest_arm_with(Box::new(fake));
    (dialed, state)
}

fn host_notice(ticket: [u8; 8]) -> Notice {
    Notice::Wifi(WifiNotice::PeerHost(wire::Host {
        status: 1,
        bearer: wire::bearer::DIRECT,
        ticket,
        valid_ms: 20_000,
        freq_mhz: 5_745,
    }))
}

fn creds_notice(ticket: [u8; 8]) -> Notice {
    Notice::Wifi(WifiNotice::PeerCreds(wire::Creds {
        ticket,
        port: 47_000,
        ipv4: [192, 168, 49, 1],
    }))
}

#[test]
fn a_guest_role_call_offers_only_the_client_bit_and_has_no_host_owner() {
    let (pc, _phone, _) = crate::call_session::wifi_test_support::wifi_session_pair_with(make, false, false);
    let mut dialed = dialed(pc);
    assert!(!dialed.session.wifi_caps_sent());
    dialed.guest_arm_with(Box::new(FakeGuestPlatform::ready()));
    dialed.wifi_tick(&mut |_, _| {});
    assert_eq!(dialed.session.wifi_local_caps(), roles::windows_guest_caps(true));
    assert!(dialed.wifi.owner.is_none());
    assert!(dialed.wifi.platform.is_none());
}

#[test]
fn a_guest_attempt_binds_with_the_phones_host_binding_and_attaches_the_wifi_carrier() {
    let (mut dialed, state) = guest_dialed();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    state.lock().unwrap().connect_to = Some(listener.local_addr().unwrap());
    let uw = *dialed.session.wifi_uw().unwrap();
    let ticket = [0x31; 8];
    let mut trace = |_: &str, _: &str| {};
    dialed.wifi_dispatch(&[host_notice(ticket)]);
    dialed.wifi_tick(&mut trace);
    dialed.wifi_dispatch(&[creds_notice(ticket)]);
    let mut host: Option<HostBinding> = None;
    let mut host_bound = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while dialed.wifi.link.is_none() {
        assert!(Instant::now() < deadline, "no Wi-Fi carrier");
        dialed.wifi_tick(&mut trace);
        if host.is_none() && host_bound.is_none() {
            if let Ok((stream, _)) = listener.accept() {
                host = Some(HostBinding::new(WifiSocket::from_stream(stream).unwrap(), &uw, ticket, 0).unwrap());
            }
        }
        if let Some(binding) = host.as_mut() {
            match binding.poll(0) {
                Step::Pending => {}
                Step::Done(bound) => {
                    host_bound = Some(bound);
                    host = None;
                }
                Step::Failed(failure) => panic!("the phone's binding failed: {}", failure.name()),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(dialed.wifi.id > 0);
    assert_eq!(dialed.wifi.guest.as_ref().unwrap().state_name(), "live");
    let joins = state.lock().unwrap().joins.clone();
    assert_eq!(joins.len(), 1);
    assert_eq!(joins[0].1, crate::wifi::keys::direct_ssid(&uw, &ticket).unwrap());
}

#[test]
fn releasing_the_call_leaves_the_group() {
    let (mut dialed, state) = guest_dialed();
    let ticket = [0x32; 8];
    dialed.wifi_dispatch(&[host_notice(ticket)]);
    dialed.wifi_tick(&mut |_, _| {});
    dialed.wifi_dispatch(&[creds_notice(ticket)]);
    dialed.wifi_tick(&mut |_, _| {});
    assert_eq!(state.lock().unwrap().joins.len(), 1);
    dialed.wifi_release();
    assert!(dialed.wifi.guest.is_none());
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
}

#[test]
fn turning_the_switch_off_mid_attempt_declines_and_leaves() {
    let (mut dialed, state) = guest_dialed();
    let ticket = [0x33; 8];
    dialed.wifi_dispatch(&[host_notice(ticket)]);
    dialed.wifi_tick(&mut |_, _| {});
    dialed.wifi_dispatch(&[creds_notice(ticket)]);
    dialed.wifi_tick(&mut |_, _| {});
    unsafe {
        std::env::set_var("MESHLIVE_WIFI_GUEST", "1");
    }
    dialed.wifi_set_armed(false, &mut |_, _| {});
    unsafe {
        std::env::remove_var("MESHLIVE_WIFI_GUEST");
    }
    assert_eq!(state.lock().unwrap().leaves, vec![1]);
    assert_eq!(dialed.wifi.guest.as_ref().unwrap().state_name(), "idle");
}

#[test]
fn a_guest_declines_busy_while_a_renewal_runs() {
    let joins_after_host_and_creds = |renewal_running: bool| -> usize {
        let (mut pc, mut phone) = active_pair();
        pc.test_enable_renewal(&[1; 32], &[2; 32], true);
        phone.test_enable_renewal(&[2; 32], &[1; 32], true);
        exchange_caps(&mut pc, roles::windows_guest_caps(true), &mut phone);
        let mut dialed = dialed(pc);
        let fake = FakeGuestPlatform::ready();
        let state = fake.state.clone();
        dialed.guest_arm_with(Box::new(fake));
        if renewal_running {
            // The PC's own call has stalled and its renewal is looking for the advert.
            let env = |last_write_ok_ns| crate::call_session::RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns };
            dialed.session.renewal_tick(5 * MS, env(Some(5 * MS)));
            dialed.session.renewal_tick(12_000 * MS, env(Some(11_900 * MS)));
            assert!(dialed.session.renewal_running());
        }
        let ticket = [0x34; 8];
        dialed.wifi_dispatch(&[host_notice(ticket)]);
        dialed.wifi_tick(&mut |_, _| {});
        dialed.wifi_dispatch(&[creds_notice(ticket)]);
        dialed.wifi_tick(&mut |_, _| {});
        let joins = state.lock().unwrap().joins.len();
        joins
    };
    assert_eq!(joins_after_host_and_creds(false), 1, "without a renewal the phone's group is joined");
    assert_eq!(joins_after_host_and_creds(true), 0, "while a renewal runs the HOST is declined busy and nothing is joined");
}
