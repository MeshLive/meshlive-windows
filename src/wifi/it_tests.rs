//! End-to-end cross-check at the `CallSession` level: two sessions switch media
//! to a loopback Wi-Fi socket and survive each carrier dying in turn.

use crate::{
    call_session::{CallSession, Notice},
    framing::HopReader,
    media::{self, VideoHeader},
    opus,
    records::{Layer, Policy, Sealer},
    wifi::{
        cutover::CloseReason,
        socket::WifiSocket,
        wire::{self, Ready},
    },
};
use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    time::Instant,
};

const MS: u64 = 1_000_000;
const ID: [u8; 16] = [7; 16];

struct Fake;
impl crate::listen::UnitDecoder for Fake {
    fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error> {
        pcm[..unit.len()].fill(i16::from(unit[0]));
        Ok(unit.len())
    }
}
fn make(_: &[u8]) -> Result<Fake, opus::Error> {
    Ok(Fake)
}
fn header() -> VideoHeader {
    VideoHeader {
        width: 426,
        height: 240,
        fps: 15,
        codec: 2,
        quarter_turns: 0,
        opus: true,
    }
}
fn packet(flags: u8, pts: i64, len: usize, byte: u8) -> Vec<u8> {
    media::Packet {
        flags,
        pts_us: pts,
        payload: &vec![byte; len],
    }
    .encode()
    .unwrap()
}
fn socket_pair() -> (WifiSocket, WifiSocket) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = TcpStream::connect(address).unwrap();
    let (server, _) = listener.accept().unwrap();
    (
        WifiSocket::from_stream(client).unwrap(),
        WifiSocket::from_stream(server).unwrap(),
    )
}
fn writes(session: &mut CallSession<Fake>, now: u64) -> Vec<(i32, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(write) = session.next_write_on(now).unwrap() {
        out.push((write.carrier, write.bytes));
    }
    out
}
fn feed(session: &mut CallSession<Fake>, carrier: i32, bytes: &[u8], now: u64) -> Vec<Notice> {
    let mut input = bytes;
    session.on_input_from(carrier, &mut input, now).unwrap()
}
fn established() -> (CallSession<Fake>, CallSession<Fake>) {
    let (mut caller, mut callee, _) =
        crate::call_session::wifi_test_support::wifi_session_pair_with(make, false, false);
    for (carrier, bytes) in writes(&mut callee, 0) {
        feed(&mut caller, carrier, &bytes, 0);
    }
    callee.accept(false).unwrap();
    for (carrier, bytes) in writes(&mut callee, MS) {
        feed(&mut caller, carrier, &bytes, MS);
    }
    caller.media_ready().unwrap();
    for (carrier, bytes) in writes(&mut caller, 2 * MS) {
        feed(&mut callee, carrier, &bytes, 2 * MS);
    }
    callee.media_ready().unwrap();
    (caller, callee)
}
fn attach(
    caller: &mut CallSession<Fake>,
    callee: &mut CallSession<Fake>,
    first: WifiSocket,
    second: WifiSocket,
    ticket: [u8; 8],
    now: u64,
) -> (i32, WifiSocket, WifiSocket) {
    let id = caller.wifi_begin_attempt(ticket, now).unwrap();
    assert_eq!(callee.wifi_begin_attempt(ticket, now).unwrap(), id);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let ready = Ready {
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
    (id, first, second)
}

#[test]
fn two_sessions_over_loopback_sockets_switch_media_to_wifi_and_survive_each_carrier_dying_in_turn() {
    let (mut caller, mut callee) = established();
    let (start, end) = socket_pair();
    let (id, mut caller_wire, mut callee_wire) =
        attach(&mut caller, &mut callee, start, end, [9; 8], 3_900 * MS);
    caller
        .test_carriers_mut()
        .unwrap()
        .on_call_active(0, true);
    caller.test_carriers_mut().unwrap().tick(4_000, true);
    assert_eq!(caller.wifi_send_on(), id, "did not switch");
    callee.test_carriers_mut().unwrap().tick(4_000, true);
    // The sender's first media after the switch travels on the Wi-Fi socket.
    caller
        .send(packet(media::KEY_FRAME, 0, 1_000, 1))
        .unwrap();
    let mut sent_on_wifi = 0;
    for (carrier, bytes) in writes(&mut caller, 4_100 * MS) {
        if carrier == id {
            sent_on_wifi += 1;
            caller_wire
                .write(&bytes, Instant::now() + std::time::Duration::from_millis(100))
                .unwrap();
        } else {
            let _ = feed(&mut callee, 0, &bytes, 4_100 * MS);
        }
    }
    assert!(sent_on_wifi > 0);
    let mut received = Vec::new();
    let mut scratch = vec![0u8; 256 * 1024];
    let result = callee_wire.read(&mut scratch, Instant::now() + std::time::Duration::from_millis(100));
    if let Ok(count) = result {
        received.extend_from_slice(&scratch[..count]);
    }
    for _ in 0..10 {
        if let Ok(count) = callee_wire.read(&mut scratch, Instant::now()) {
            received.extend_from_slice(&scratch[..count]);
        }
    }
    let _ = feed(&mut callee, id, &received, 4_100 * MS);

    // Wi-Fi dies: the call falls back with a RESUME and continues.
    caller.wifi_carrier_died(id, 5_000 * MS);
    let fallback = writes(&mut caller, 5_000 * MS);
    assert!(fallback.iter().any(|(carrier, _)| *carrier == 0));
    for (carrier, bytes) in &fallback {
        let _ = feed(&mut callee, *carrier, bytes, 5_000 * MS);
    }
    assert!(!caller.is_dead(5_000 * MS));
    assert!(caller.take_notices().contains(&Notice::KeyFrameRequested));

    // A second attempt after the cooldown switches again.
    let (start2, end2) = socket_pair();
    let (id2, mut caller_wire2, mut callee_wire2) =
        attach(&mut caller, &mut callee, start2, end2, [10; 8], 36_000 * MS);
    caller.test_carriers_mut().unwrap().tick(36_100, true);
    callee.test_carriers_mut().unwrap().tick(36_100, true);
    caller
        .send(packet(media::KEY_FRAME, 0, 1_000, 2))
        .unwrap();
    let mut second = 0;
    for (carrier, bytes) in writes(&mut caller, 36_200 * MS) {
        if carrier == id2 {
            second += 1;
            caller_wire2
                .write(&bytes, Instant::now() + std::time::Duration::from_millis(100))
                .unwrap();
        }
    }
    assert!(second > 0);

    // Bluetooth dies: media continues over Wi-Fi; then Wi-Fi dies too.
    caller.wifi_carrier_died(0, 37_000 * MS);
    assert!(!caller.is_dead(37_000 * MS));
    assert!(caller.next_write_on(37_100 * MS).is_ok());
    caller.wifi_carrier_died(id2, 37_200 * MS);
    assert_eq!(caller.next_write_on(37_300 * MS).err(), Some(crate::call_session::Error::Dead));
    let _ = callee
        .wifi_take_ends()
        .iter()
        .any(|end| end.id == id && end.reason == Some(CloseReason::PeerResumed));
}
