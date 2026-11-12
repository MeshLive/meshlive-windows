use super::*;
use crate::{
    bmh2::Purpose,
    media::{CONFIG, KEY_FRAME},
    session::tests::{handshake, identities},
};

struct Fake;
impl UnitDecoder for Fake {
    fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error> {
        pcm[..unit.len()].fill(i16::from(unit[0]));
        Ok(unit.len())
    }
}
fn make(_: &[u8]) -> Result<Fake, opus::Error> {
    Ok(Fake)
}
const ID: [u8; 16] = [7; 16];
const MS: u64 = 1_000_000;

fn links(fragments: bool) -> (CallLink, CallLink) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    (
        CallLink::new(
            al.send_key(),
            al.receive_key(),
            ae.send_key(),
            ae.receive_key(),
            fragments,
        )
        .unwrap(),
        CallLink::new(
            bl.send_key(),
            bl.receive_key(),
            be.send_key(),
            be.receive_key(),
            fragments,
        )
        .unwrap(),
    )
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
fn wifi_dial(voice: bool, fragments: bool) -> (CallSession<Fake>, CallSession<Fake>, Vec<Notice>) {
    wifi_test_support::wifi_session_pair_with(make, voice, fragments)
}
fn wifi_dial_flags(
    voice: bool,
    caller_relay_edge: bool,
    callee_relay_edge: bool,
) -> (CallSession<Fake>, CallSession<Fake>, Vec<Notice>) {
    wifi_test_support::wifi_session_pair_flags(make, voice, false, caller_relay_edge, callee_relay_edge)
}
fn flush_on(session: &mut CallSession<Fake>, now: u64) -> Vec<(i32, Vec<u8>)> {
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
fn deliver(session: &mut CallSession<Fake>, writes: &[(i32, Vec<u8>)], now: u64) -> Vec<Notice> {
    let mut notices = Vec::new();
    for (carrier, bytes) in writes {
        notices.extend(feed(session, *carrier, bytes, now));
    }
    notices
}
fn attach_pair(a: &mut CallSession<Fake>, b: &mut CallSession<Fake>, now_ns: u64) {
    let ticket = [9u8; 8];
    let id = a.wifi_begin_attempt(ticket, now_ns).unwrap();
    assert_eq!(b.wifi_begin_attempt(ticket, now_ns).unwrap(), id);
    let policy = crate::records::Policy {
        wifi: true,
        ..crate::records::Policy::default()
    };
    let ready = wire::Ready {
        recv_ceiling_kbps: 1100,
        family_cap_kbps: 0,
        transport_budget_kbps: 1100,
    };
    a.wifi_attach(
        id,
        Sealer::new(crate::records::Layer::Hop, &[1; 32], policy).unwrap(),
        HopReader::new(&[2; 32], policy).unwrap(),
        ready,
        now_ns,
    );
    b.wifi_attach(
        id,
        Sealer::new(crate::records::Layer::Hop, &[2; 32], policy).unwrap(),
        HopReader::new(&[1; 32], policy).unwrap(),
        ready,
        now_ns,
    );
}
fn established(voice: bool) -> (CallSession<Fake>, CallSession<Fake>) {
    established_with(voice, false, false)
}
fn established_with(
    voice: bool,
    caller_relay_edge: bool,
    callee_relay_edge: bool,
) -> (CallSession<Fake>, CallSession<Fake>) {
    let (mut caller, mut callee, _) = wifi_dial_flags(voice, caller_relay_edge, callee_relay_edge);
    feed(&mut caller, 0, &flush_on(&mut callee, 0).concat_bytes(), 0);
    callee.accept(voice).unwrap();
    feed(&mut caller, 0, &flush_on(&mut callee, MS).concat_bytes(), MS);
    caller.media_ready().unwrap();
    feed(&mut callee, 0, &flush_on(&mut caller, 2 * MS).concat_bytes(), 2 * MS);
    callee.media_ready().unwrap();
    (caller, callee)
}
trait ConcatBytes {
    fn concat_bytes(&self) -> Vec<u8>;
}
impl ConcatBytes for Vec<(i32, Vec<u8>)> {
    fn concat_bytes(&self) -> Vec<u8> {
        self.iter().flat_map(|(_, bytes)| bytes.clone()).collect()
    }
}
fn switch_pair(
    caller: &mut CallSession<Fake>,
    callee: &mut CallSession<Fake>,
) -> (i32, Vec<Notice>) {
    caller.send(packet(KEY_FRAME, 0, 2_000, 1)).unwrap();
    let first = flush_on(caller, 4_000 * MS);
    assert!(first.iter().all(|(carrier, _)| *carrier == 0));
    for (carrier, bytes) in &first {
        feed(callee, *carrier, bytes, 4_000 * MS);
    }
    assert_eq!(caller.phase(), call::Phase::Active, "phase before attach");
    assert!(caller.wifi_video_on(), "video before attach");
    assert!(caller.wifi_attempts_allowed(), "attempts allowed");
    attach_pair(caller, callee, 6_000 * MS);
    // The peer's records keep both carriers' sequence and silence clocks alive.
    let peer = flush_on(callee, 6_500 * MS);
    for (carrier, bytes) in &peer {
        feed(caller, *carrier, bytes, 6_500 * MS);
    }
    let switched = flush_on(caller, 7_000 * MS);
    let mut notices = Vec::new();
    for (carrier, bytes) in &switched {
        notices.extend(feed(callee, *carrier, bytes, 7_000 * MS));
    }
    let id = caller.wifi_send_on();
    (id, notices)
}

/// Everything `from` has to write, as one byte stream.
fn flush(from: &mut CallSession<Fake>, now: u64) -> Vec<u8> {
    let mut wire = Vec::new();
    while let Some(record) = from.next_write(now).unwrap() {
        wire.extend(record);
    }
    wire
}
/// The callee is created from the caller's first signal (after its heartbeat).
fn dial(voice: bool, fragments: bool) -> (CallSession<Fake>, CallSession<Fake>, Vec<Notice>) {
    let (a, mut b) = links(fragments);
    let mut caller = CallSession::outgoing(a, ID, voice, 0, header(), make, 0).unwrap();
    let wire = flush(&mut caller, 0);
    let mut input = wire.as_slice();
    let Some(Incoming::Heartbeat(None)) = b.read(&mut input).unwrap() else {
        panic!()
    };
    let Some(Incoming::Signal(invite)) = b.read(&mut input).unwrap() else {
        panic!()
    };
    let (mut callee, mut notices) = CallSession::incoming(b, &invite, header(), make, 0).unwrap();
    notices.extend(callee.on_input(&mut input, 0).unwrap());
    (caller, callee, notices)
}

#[test]
fn a_call_with_wifi_enabled_but_unused_writes_the_same_bytes_as_a_call_without() {
    let (mut plain, mut plain_callee, _) = dial(true, false);
    let (mut wifi, mut wifi_callee, _) = wifi_dial(true, false);
    assert_eq!(
        plain_callee.accept(true).unwrap(),
        wifi_callee.accept(true).unwrap()
    );
    deliver(&mut plain, &flush_on(&mut plain_callee, 0), 0);
    deliver(&mut wifi, &flush_on(&mut wifi_callee, 0), 0);
    plain.media_ready().unwrap();
    wifi.media_ready().unwrap();
    let plain_writes = flush_on(&mut plain, MS);
    let wifi_writes = flush_on(&mut wifi, MS);
    assert_eq!(plain_writes.len(), wifi_writes.len());
    assert_eq!(
        plain_writes.iter().map(|(_, bytes)| bytes.len()).collect::<Vec<_>>(),
        wifi_writes.iter().map(|(_, bytes)| bytes.len()).collect::<Vec<_>>()
    );
    assert!(wifi_writes.iter().all(|(carrier, _)| *carrier == 0));
}

#[test]
fn media_moves_to_the_wifi_carrier_after_three_active_seconds_and_markers_use_the_bluetooth_one() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    assert!(id > 0);
    assert_eq!(caller.wifi_send_on(), id);
    caller.send(packet(KEY_FRAME, 0, 1_000, 2)).unwrap();
    let writes = flush_on(&mut caller, 7_200 * MS);
    assert!(writes.iter().any(|(carrier, _)| *carrier == id));
}

#[test]
fn when_the_wifi_carrier_dies_the_call_falls_back_with_a_resume_and_goes_on() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_carrier_died(id, 8_000 * MS);
    let writes = flush_on(&mut caller, 8_000 * MS);
    assert!(writes.iter().any(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16));
    assert!(caller.take_notices().contains(&Notice::KeyFrameRequested));
    let notices = deliver(&mut callee, &writes, 8_000 * MS);
    assert!(notices.iter().all(|notice| !matches!(notice, Notice::Ended(_))));
    assert!(callee
        .wifi_take_ends()
        .iter()
        .any(|end| end.id == id && end.reason == Some(crate::wifi::cutover::CloseReason::PeerResumed)));
}

#[test]
fn a_call_ends_only_when_no_carrier_is_left() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_carrier_died(0, 8_000 * MS);
    assert!(caller.next_write_on(8_000 * MS).is_ok());
    caller.wifi_carrier_died(id, 8_000 * MS);
    assert_eq!(caller.next_write_on(9_000 * MS).err(), Some(Error::Dead));
    assert!(caller.is_dead(9_000 * MS));
}

#[test]
fn silence_on_the_wifi_carrier_kills_only_that_carrier() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    assert_eq!(caller.next_write_on(7_000 * MS + 2_500 * MS + 1).err(), None);
    assert_eq!(caller.wifi_send_on(), 0);
    assert!(caller
        .wifi_take_ends()
        .iter()
        .any(|end| end.id == id && end.reason.is_none()));
}

#[test]
fn an_idle_wifi_carrier_gets_a_heartbeat_every_500_ms() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    let writes = flush_on(&mut caller, 7_000 * MS + 500 * MS);
    assert!(writes
        .iter()
        .any(|(carrier, bytes)| *carrier == id && bytes[4] == 6));
    assert_eq!(caller.wifi_send_on(), id);
}

#[test]
fn a_replayed_content_record_is_dropped_and_the_session_continues() {
    let (mut caller, mut callee) = established(false);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let inner = caller
        .link
        .test_seal_endpoint(6, 0, &message.encode().unwrap())
        .unwrap();
    let first = caller.link.test_seal_hop(&inner).unwrap();
    let second = caller.link.test_seal_hop(&inner).unwrap();
    let mut input = first.as_slice();
    let notices = callee.on_input_from(0, &mut input, 3_000 * MS).unwrap();
    assert!(notices.contains(&Notice::PeerCamera(true)));
    let mut again = second.as_slice();
    assert_eq!(
        callee.on_input_from(0, &mut again, 3_000 * MS).unwrap(),
        Vec::new()
    );
}

#[test]
fn a_wifi_record_without_the_marker_negotiated_is_an_error_but_a_normal_call_is_unaffected() {
    use crate::{bmh2::Purpose, records::{Layer, Policy, Sealer}, session::tests::{handshake, identities}};
    let (a, b, _) = identities();
    let (al, _) = handshake(&a, &b, Purpose::Link);
    let (ae, _) = handshake(&a, &b, Purpose::Endpoint);
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
    let mut caller: CallSession<Fake> =
        CallSession::outgoing(link, ID, true, 0, header(), make, 0).unwrap();
    let mut peer = Sealer::new(Layer::Hop, al.receive_key(), Policy { wifi: true, ..Policy::default() }).unwrap();
    let marker = peer.seal(16, 0, b"BWF1\x0c\0").unwrap();
    let mut input = marker.as_slice();
    assert!(caller.on_input(&mut input, MS).is_err());
}

#[test]
fn a_wifi_notice_printed_for_the_trace_never_contains_the_ticket() {
    let text = format!(
        "{:?}",
        WifiNotice::PeerDecline {
            ticket: [0xab; 8],
            reason: 2
        }
    );
    assert!(!text.contains("abab"));
    assert!(!text.contains("171"));
}

#[test]
fn hanging_up_writes_close_on_bluetooth_only_before_the_signal() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    let pending = flush_on(&mut caller, 7_500 * MS);
    deliver(&mut callee, &pending, 7_500 * MS);
    caller.hang_up(Reason::Normal).unwrap();
    let writes = flush_on(&mut caller, 7_600 * MS);
    assert_eq!(writes[0].0, 0);
    assert_eq!(writes[0].1[4], 16);
    assert!(writes.iter().any(|(carrier, bytes)| *carrier == id && bytes[4] == 5));
    let notices = deliver(&mut callee, &writes, 7_600 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerClose {
        reason: crate::wifi::wire::close::HANG_UP
    })));
    assert!(notices.iter().any(|notice| matches!(notice, Notice::Ended(Reason::Normal))));

    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_carrier_died(0, 7_500 * MS);
    let pending = flush_on(&mut caller, 7_500 * MS);
    deliver(&mut callee, &pending, 7_500 * MS);
    caller.hang_up(Reason::Normal).unwrap();
    let writes = flush_on(&mut caller, 7_600 * MS);
    assert!(!writes.iter().any(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16));
    assert!(writes.iter().any(|(carrier, bytes)| *carrier == id && bytes[4] == 5));
}

#[test]
fn a_close_from_the_peer_on_either_carrier_changes_nothing() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    let send_on = caller.wifi_send_on();
    let close = Bwf1::Close(wire::Close {
        reason: wire::close::HANG_UP,
    });
    for carrier in [0, id] {
        let bytes = callee.link.seal_bwf1(carrier, &close).unwrap();
        let notices = feed(&mut caller, carrier, &bytes, 7_500 * MS);
        assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerClose {
            reason: wire::close::HANG_UP
        })));
        assert_eq!(caller.wifi_send_on(), send_on);
        assert!(caller.wifi_take_ends().is_empty());
        assert_eq!(caller.phase(), call::Phase::Active);
    }
}

#[test]
fn an_end_to_end_violation_over_wifi_still_ends_the_call() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let mut inner = caller
        .link
        .test_seal_endpoint(6, 0, &message.encode().unwrap())
        .unwrap();
    let last = inner.len() - 1;
    inner[last] ^= 1;
    let bytes = caller.link.test_seal_hop_on(id, &inner).unwrap();
    let mut input = bytes.as_slice();
    let error = callee
        .on_input_from(id, &mut input, 7_500 * MS)
        .unwrap_err();
    assert!(matches!(error, Error::Call(_)), "{error:?}");
    assert!(!matches!(error, Error::Call(call::Error::Carrier(_))));
}

#[test]
fn a_quality_fallback_writes_resume_and_closes_the_wifi_carrier_with_quality() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_quality_fallback(7_500 * MS);
    let writes = flush_on(&mut caller, 7_500 * MS);
    assert!(writes.iter().any(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16));
    assert!(caller
        .wifi_take_ends()
        .iter()
        .any(|end| end.id == id && end.reason == Some(crate::wifi::cutover::CloseReason::Quality)));
}

#[test]
fn bluetooth_heartbeats_and_reports_stop_once_the_old_carrier_is_dead() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_carrier_died(0, 7_100 * MS);
    for step in 0..3 {
        let now = 7_100 * MS + step * 500 * MS;
        match caller.next_write_on(now) {
            Ok(Some(write)) => assert_ne!(write.carrier, 0),
            Ok(None) => {}
            Err(Error::Dead) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert_eq!(caller.wifi_send_on(), id);
}

#[test]
fn delivery_reports_count_only_bluetooth_bytes_while_wifi_carries_media() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.send(packet(KEY_FRAME, 0, 1_000, 3)).unwrap();
    let writes = flush_on(&mut caller, 7_500 * MS);
    let before = callee.received_media_bytes;
    for (carrier, bytes) in &writes {
        if *carrier == id {
            feed(&mut callee, *carrier, bytes, 7_500 * MS);
        }
    }
    assert_eq!(callee.received_media_bytes, before);
}

#[test]
fn a_call_with_carriers_but_no_wifi_carrier_is_declared_dead_by_bluetooth_silence() {
    let (mut caller, _callee, _) = wifi_dial(true, false);
    assert!(caller.is_dead(9_000 * MS));
    assert_eq!(caller.next_write_on(9_000 * MS).err(), Some(Error::Dead));
}

#[test]
fn bluetooth_off_during_a_call_on_wifi_leaves_the_call_running() {
    let (mut caller, mut callee) = established(false);
    switch_pair(&mut caller, &mut callee);
    caller.wifi_carrier_died(0, 7_500 * MS);
    assert!(!caller.is_dead(7_500 * MS));
    assert!(caller.next_write_on(7_600 * MS).is_ok());
    assert!(caller.wifi_send_on() > 0);
}

#[test]
fn bluetooth_off_without_wifi_ends_the_call_as_today() {
    let (mut caller, _callee, _) = wifi_dial(true, false);
    caller.wifi_carrier_died(0, 1_000 * MS);
    assert!(caller.is_dead(1_000 * MS));
    assert_eq!(caller.next_write_on(1_000 * MS).err(), Some(Error::Dead));
}

#[test]
fn switch_off_with_a_stale_standby_waits_until_the_standby_is_heard_again() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_standby_freshness(false, 7_100 * MS);
    caller.wifi_switch_off(7_200 * MS);
    let _ = flush_on(&mut caller, 7_300 * MS);
    assert_eq!(caller.wifi_send_on(), id);
    caller.wifi_standby_freshness(true, 7_400 * MS);
    let writes = flush_on(&mut caller, 7_500 * MS);
    assert!(writes.iter().any(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16));
    assert!(caller
        .wifi_take_ends()
        .iter()
        .any(|end| end.id == id && end.reason == Some(crate::wifi::cutover::CloseReason::SwitchOff)));
}

#[test]
fn a_quality_fallback_with_a_stale_standby_keeps_wifi() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_standby_freshness(false, 7_100 * MS);
    caller.wifi_quality_fallback(7_200 * MS);
    assert_eq!(caller.wifi_send_on(), id);
    assert!(caller.wifi_take_ends().is_empty());
}

#[test]
fn the_responder_sends_one_caps_when_active_and_the_initiator_answers_with_its_own() {
    let (mut caller, mut callee) = established(true);
    let responder = flush_on(&mut callee, 3_000 * MS);
    let caps: Vec<_> = responder
        .iter()
        .filter(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16)
        .collect();
    assert_eq!(caps.len(), 1);
    let notices = deliver(&mut caller, &responder, 3_000 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerCaps(wire::Caps::default()))));
    let answer = flush_on(&mut caller, 4_000 * MS);
    assert_eq!(
        answer
            .iter()
            .filter(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16)
            .count(),
        1
    );
    let notices = deliver(&mut callee, &answer, 4_000 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerCaps(wire::Caps::default()))));
    assert!(caller.wifi_caps_exchanged());
    assert!(callee.wifi_caps_exchanged());
    assert!(flush_on(&mut caller, 5_000 * MS)
        .iter()
        .all(|(carrier, bytes)| *carrier != 0 || bytes[4] != 16));
}

#[test]
fn a_second_caps_ends_wifi_for_the_call_but_the_call_goes_on() {
    let (mut caller, mut callee) = established(false);
    let responder = flush_on(&mut callee, 3_000 * MS);
    deliver(&mut caller, &responder, 3_000 * MS);
    let answer = flush_on(&mut caller, 4_000 * MS);
    deliver(&mut callee, &answer, 4_000 * MS);
    let second = callee
        .link
        .seal_bwf1(0, &Bwf1::Caps(wire::Caps::default()))
        .unwrap();
    let notices = feed(&mut caller, 0, &second, 5_000 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::WifiEnded {
        reason: "caps_repeat"
    })));
    assert!(caller
        .wifi_send(Bwf1::Host(wire::Host {
            status: 1,
            bearer: wire::bearer::DIRECT,
            ticket: [1; 8],
            valid_ms: 1_000,
            freq_mhz: 0,
        }))
        .is_err());
    assert!(caller.next_write_on(5_500 * MS).is_ok());
}

#[test]
fn caps_use_the_bits_set_at_send_time() {
    let (mut caller, mut callee) = established(true);
    callee.wifi_set_local_caps(crate::wifi::roles::windows_caps(true));
    let responder = flush_on(&mut callee, 3_000 * MS);
    deliver(&mut caller, &responder, 3_000 * MS);
    assert_eq!(caller.wifi_peer_caps().unwrap().bearers, wire::caps_bit::DIRECT_GO);
    callee.wifi_set_local_caps(wire::Caps {
        bearers: wire::caps_bit::DIRECT_CLIENT,
        ..wire::Caps::default()
    });
    assert!(flush_on(&mut callee, 4_000 * MS)
        .iter()
        .all(|(carrier, bytes)| *carrier != 0 || bytes[4] != 16));
}

#[test]
fn a_non_caps_wifi_record_cannot_be_queued_before_both_caps_were_exchanged() {
    let (mut caller, mut callee) = established(false);
    let host = Bwf1::Host(wire::Host {
        status: 1,
        bearer: wire::bearer::DIRECT,
        ticket: [1; 8],
        valid_ms: 1_000,
        freq_mhz: 0,
    });
    assert!(caller.wifi_send(host).is_err());
    let responder = flush_on(&mut callee, 3_000 * MS);
    deliver(&mut caller, &responder, 3_000 * MS);
    let answer = flush_on(&mut caller, 4_000 * MS);
    deliver(&mut callee, &answer, 4_000 * MS);
    assert!(caller.wifi_send(host).is_ok());
}

/// Every record type-16 free: what a reader that refuses `BWF1` can read.
fn no_wifi_record(writes: &[(i32, Vec<u8>)]) -> bool {
    writes.iter().all(|(carrier, bytes)| *carrier == 0 && bytes[4] != 16)
}

#[test]
fn a_peer_without_the_marker_never_gets_a_wifi_record_not_even_at_hang_up() {
    // The caller signed the marker and reads BWF1; the callee never saw it.
    let (mut caller, mut callee, _) = wifi_test_support::caller_only_wifi_pair(make, false);
    deliver(&mut caller, &flush_on(&mut callee, 0), 0);
    callee.accept(false).unwrap();
    deliver(&mut caller, &flush_on(&mut callee, MS), MS);
    caller.media_ready().unwrap();
    let mut sent = flush_on(&mut caller, 2 * MS);
    deliver(&mut callee, &sent, 2 * MS);
    callee.media_ready().unwrap();
    for second in 3..8 {
        let now = second * 1_000 * MS;
        caller.send(packet(KEY_FRAME, 0, 500, 1)).unwrap();
        let writes = flush_on(&mut caller, now);
        deliver(&mut callee, &writes, now);
        deliver(&mut caller, &flush_on(&mut callee, now), now);
        sent.extend(writes);
    }
    caller.hang_up(Reason::Normal).unwrap();
    let last = flush_on(&mut caller, 8_000 * MS);
    let notices = deliver(&mut callee, &last, 8_000 * MS);
    sent.extend(last);
    assert!(no_wifi_record(&sent));
    assert!(notices.iter().any(|notice| matches!(notice, Notice::Ended(Reason::Normal))));
}

#[test]
fn hanging_up_a_call_that_never_attached_a_wifi_carrier_writes_no_close() {
    let (mut caller, mut callee) = established(false);
    deliver(&mut caller, &flush_on(&mut callee, 3_000 * MS), 3_000 * MS);
    deliver(&mut callee, &flush_on(&mut caller, 4_000 * MS), 4_000 * MS);
    assert!(caller.wifi_caps_exchanged());
    caller.hang_up(Reason::Normal).unwrap();
    let writes = flush_on(&mut caller, 5_000 * MS);
    assert!(no_wifi_record(&writes));
    let notices = deliver(&mut callee, &writes, 5_000 * MS);
    assert!(notices.iter().any(|notice| matches!(notice, Notice::Ended(Reason::Normal))));
}

#[test]
fn a_hang_up_decoded_before_a_wifi_carrier_fault_still_ends_the_call() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    deliver(&mut callee, &flush_on(&mut caller, 7_500 * MS), 7_500 * MS);
    caller.hang_up(Reason::Normal).unwrap();
    let writes = flush_on(&mut caller, 7_600 * MS);
    let mut on_wifi = Vec::new();
    for (carrier, bytes) in &writes {
        if *carrier == id {
            on_wifi.extend_from_slice(bytes);
        } else {
            feed(&mut callee, *carrier, bytes, 7_600 * MS);
        }
    }
    // Right behind the HangUp: a record the Wi-Fi carrier may not carry.
    on_wifi.extend(caller.link.seal_bwf1(id, &Bwf1::Caps(wire::Caps::default())).unwrap());
    let mut input = on_wifi.as_slice();
    let error = callee.on_input_from(id, &mut input, 7_600 * MS).unwrap_err();
    assert_eq!(error, Error::Call(call::Error::Carrier(id)));
    assert!(callee
        .take_notices()
        .iter()
        .any(|notice| matches!(notice, Notice::Ended(Reason::Normal))));
}

#[test]
fn a_wifi_heartbeat_that_does_not_decode_is_a_pulse_and_never_ends_the_call() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    let heartbeat = caller.link.test_seal_kind_on(id, 6, &[0xff; 9]).unwrap();
    let mut input = heartbeat.as_slice();
    assert!(callee.on_input_from(id, &mut input, 9_000 * MS).is_ok());
    // Silence is counted from that pulse: 2 s later the carrier still lives.
    assert!(callee.next_write_on(11_000 * MS).is_ok());
    assert!(!callee.wifi_take_ends().iter().any(|end| end.id == id));
}

#[test]
fn a_carrier_the_machine_closes_is_reported_once_and_gets_no_more_heartbeats() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    caller.wifi_quality_fallback(7_500 * MS);
    let ends = caller.wifi_take_ends();
    assert_eq!(ends.iter().filter(|end| end.id == id).count(), 1);
    assert!(!caller.is_dead(7_500 * MS));
    for step in 0..8 {
        let now = 7_500 * MS + step * 500 * MS;
        let writes = flush_on(&mut caller, now);
        assert!(writes.iter().all(|(carrier, _)| *carrier == 0), "a write for the closed carrier");
        let bluetooth: Vec<_> = flush_on(&mut callee, now).into_iter().filter(|(carrier, _)| *carrier == 0).collect();
        deliver(&mut caller, &bluetooth, now);
    }
    assert!(!caller.wifi_take_ends().iter().any(|end| end.id == id));
}

#[test]
fn a_hang_up_written_on_a_wifi_carrier_that_then_fails_is_sent_again_on_bluetooth() {
    let (mut caller, mut callee) = established(false);
    let (id, _) = switch_pair(&mut caller, &mut callee);
    deliver(&mut callee, &flush_on(&mut caller, 7_500 * MS), 7_500 * MS);
    caller.hang_up(Reason::Normal).unwrap();
    let first = flush_on(&mut caller, 7_600 * MS);
    assert!(first.iter().any(|(carrier, bytes)| *carrier == id && bytes[4] == 5));
    // The socket dies with the HangUp unsent: only the Bluetooth records arrive.
    for (carrier, bytes) in &first {
        if *carrier == 0 {
            feed(&mut callee, 0, bytes, 7_600 * MS);
        }
    }
    caller.wifi_carrier_died(id, 7_700 * MS);
    let again = flush_on(&mut caller, 7_700 * MS);
    assert!(again.iter().all(|(carrier, _)| *carrier == 0));
    let notices = deliver(&mut callee, &again, 7_700 * MS);
    assert!(notices.iter().any(|notice| matches!(notice, Notice::Ended(Reason::Normal))));
}

#[test]
fn an_unknown_bwf1_kind_is_traced_and_counts_as_heard_on_its_carrier() {
    let (mut caller, mut callee) = established(false);
    let mut unknown = b"BWF1".to_vec();
    unknown.push(12);
    unknown.extend_from_slice(&[0; 20]);
    let record = callee.link.test_seal_kind_on(0, 16, &unknown).unwrap();
    // Nothing else reached the caller for 5 s: its standby is stale until this record.
    assert!(!caller.standby_received_within(5_000 * MS, crate::wifi::limits::STANDBY_FRESH_MS));
    assert_eq!(feed(&mut caller, 0, &record, 5_000 * MS), Vec::new());
    assert!(caller.standby_received_within(5_000 * MS, crate::wifi::limits::STANDBY_FRESH_MS));
    assert!(caller
        .wifi_take_traces()
        .iter()
        .any(|(phase, line)| *phase == "bwf1_ignored" && line.contains("carrier=0")));
}

#[test]
fn a_bluetooth_id_or_an_id_that_is_not_an_attempt_is_never_attached() {
    let (mut caller, _callee) = established(false);
    let policy = crate::records::Policy {
        wifi: true,
        ..crate::records::Policy::default()
    };
    let ready = wire::Ready {
        recv_ceiling_kbps: 1100,
        family_cap_kbps: 0,
        transport_budget_kbps: 1100,
    };
    for id in [0, 5, 63] {
        let attached = caller.wifi_attach(
            id,
            Sealer::new(crate::records::Layer::Hop, &[1; 32], policy).unwrap(),
            HopReader::new(&[2; 32], policy).unwrap(),
            ready,
            3_000 * MS,
        );
        assert!(!attached, "id {id}");
    }
    assert!(caller.wifi_old_alive());
    assert!(caller.next_write_on(3_000 * MS).is_ok());
}

#[test]
fn held_records_are_freed_when_their_carrier_dies_before_its_switch() {
    let (mut caller, mut callee) = established(false);
    caller.send(packet(KEY_FRAME, 0, 2_000, 1)).unwrap();
    deliver(&mut callee, &flush_on(&mut caller, 4_000 * MS), 4_000 * MS);
    attach_pair(&mut caller, &mut callee, 6_000 * MS);
    deliver(&mut caller, &flush_on(&mut callee, 6_500 * MS), 6_500 * MS);
    let switched = flush_on(&mut caller, 7_000 * MS);
    let id = caller.wifi_send_on();
    assert!(id > 0);
    caller.send(packet(KEY_FRAME, 0, 2_000, 2)).unwrap();
    let more = flush_on(&mut caller, 7_100 * MS);
    // The SWITCH on Bluetooth is late: the Wi-Fi records wait at the callee.
    for (carrier, bytes) in switched.iter().chain(&more) {
        if *carrier == id {
            feed(&mut callee, *carrier, bytes, 7_100 * MS);
        }
    }
    assert!(callee.test_carriers_mut().unwrap().stored_records() > 0);
    callee.wifi_carrier_died(id, 7_200 * MS);
    assert_eq!(callee.test_carriers_mut().unwrap().stored_records(), 0);
}

#[test]
fn caps_wait_for_both_reveals_on_a_pseudonymous_call() {
    use crate::{
        bmh3::reveal::{ROLE_CALLEE, ROLE_CALLER, RevealSession},
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
        session::tests::hex,
    };
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap();
    let spki = |name: &str| -> [u8; 91] {
        hex(v["link"][name]["ephemeralKey"].as_str().unwrap())
            .try_into()
            .unwrap()
    };
    let (caller_pseudo, callee_pseudo) = (spki("initiatorHello"), spki("responderHello"));
    let (caller_real, callee_real, _) = identities();
    let callee_real_spki = crate::bmh2::SigningIdentity::public_key(&callee_real).unwrap();
    let callee_node: [u8; 16] = crate::crypto::sha256(&callee_real_spki).unwrap()[..16]
        .try_into()
        .unwrap();
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &callee_node, epoch, 0).unwrap();
    let exporter = [7u8; 32];
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link_with(
            Sealer::new(Layer::Hop, s.send_key(), policy).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), policy).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
            false,
        )
        .unwrap()
    };
    let mut caller =
        CallSession::<Fake>::outgoing(link(&al, &ae), ID, true, 0, header(), make, 0).unwrap();
    caller.enable_wifi(&exporter, true, false).unwrap();
    caller.require_reveal(RevealGate {
        session: RevealSession::new(
            ROLE_CALLER,
            &exporter,
            caller_pseudo,
            callee_pseudo,
            true,
            true,
            vec![caller_pseudo],
        )
        .unwrap(),
        selected: entry.rdi,
        rotating: true,
        real: Box::new(caller_real),
        wall_epoch: epoch,
    });
    let mut callee_link = link(&bl, &be);
    callee_link.enable_wifi(&exporter, false, false).unwrap();
    let opening = flush_on(&mut caller, 0);
    let mut invite = None;
    for (_, bytes) in &opening {
        let mut input = bytes.as_slice();
        while let Some(incoming) = callee_link.read(&mut input).unwrap() {
            if let Incoming::Signal(message) = incoming {
                invite.get_or_insert(message);
            }
        }
    }
    let (mut callee, _) =
        CallSession::<Fake>::incoming(callee_link, &invite.unwrap(), header(), make, 0).unwrap();
    callee.reveal_on_accept(CalleeReveal {
        session: RevealSession::new(
            ROLE_CALLEE,
            &exporter,
            callee_pseudo,
            caller_pseudo,
            true,
            true,
            vec![callee_pseudo],
        )
        .unwrap(),
        real: Box::new(callee_real),
        entries: vec![entry],
        relay_hops: 0,
    });
    callee.accept(true).unwrap();
    let first = flush_on(&mut callee, MS);
    assert!(first
        .iter()
        .all(|(carrier, bytes)| *carrier != 0 || bytes[4] != 16));
    for (carrier, bytes) in &first {
        feed(&mut caller, *carrier, bytes, MS);
    }
    let answer = flush_on(&mut caller, 2 * MS);
    for (carrier, bytes) in &answer {
        feed(&mut callee, *carrier, bytes, 2 * MS);
    }
    callee.media_ready().unwrap();
    let caps = flush_on(&mut callee, 3_000 * MS);
    assert_eq!(
        caps.iter()
            .filter(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16)
            .count(),
        1
    );
}
#[test]
fn a_peer_caps_is_accepted_once_whether_it_arrives_before_or_after_our_own() {
    // (a) the relay-edge initiator hears the peer first and then answers.
    let (mut caller, mut callee) = established_with(false, true, false);
    let peer = flush_on(&mut callee, 3_000 * MS);
    let notices = deliver(&mut caller, &peer, 3_000 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerCaps(wire::Caps::default()))));
    let answer = flush_on(&mut caller, 4_000 * MS);
    let caps = answer
        .iter()
        .filter(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16)
        .count();
    assert_eq!(caps, 1);
    deliver(&mut callee, &answer, 4_000 * MS);
    assert!(caller.wifi_caps_exchanged());

    // (b) the responder writes first and the peer's CAPS arrives later.
    let (mut caller, mut callee) = established(false);
    let responder = flush_on(&mut callee, 3_000 * MS);
    let notices = deliver(&mut caller, &responder, 3_000 * MS);
    assert!(notices.contains(&Notice::Wifi(WifiNotice::PeerCaps(wire::Caps::default()))));
    let answer = flush_on(&mut caller, 4_000 * MS);
    deliver(&mut callee, &answer, 4_000 * MS);
    assert!(callee.wifi_caps_exchanged());
}

#[test]
fn on_a_relay_edge_the_initiator_waits_for_the_relays_caps_and_never_starts_an_attempt() {
    let (mut caller, mut callee) = established_with(false, true, false);
    assert!(!caller.wifi_attempts_allowed());
    let peer = flush_on(&mut callee, 3_000 * MS);
    deliver(&mut caller, &peer, 3_000 * MS);
    let answer = flush_on(&mut caller, 4_000 * MS);
    let caps: Vec<_> = answer
        .iter()
        .filter(|(carrier, bytes)| *carrier == 0 && bytes[4] == 16)
        .collect();
    assert_eq!(caps.len(), 1);
    deliver(&mut callee, &answer, 4_000 * MS);
    assert_eq!(callee.wifi_peer_caps(), Some(wire::Caps::default()));
}

#[test]
fn a_silent_ringing_call_closes_without_sending_a_hang_up() {
    let (mut caller, mut callee, _) = dial(true, false);
    let wire = flush(&mut callee, 0);
    caller.on_input(&mut wire.as_slice(), 0).unwrap();
    assert_eq!(caller.phase(), call::Phase::OutgoingRinging);
    assert_eq!(callee.phase(), call::Phase::IncomingRinging);
    for session in [&mut caller, &mut callee] {
        assert_eq!(session.next_write(9_000 * MS).err(), Some(Error::Dead));
        assert!(!session.liveness_hung_up());
    }
}

#[test]
fn a_searching_callee_sends_busy_without_ringing_or_starting_media() {
    let (a, mut b) = links(false);
    let mut caller = CallSession::outgoing(a, ID, true, 0, header(), make, 0).unwrap();
    let wire = flush(&mut caller, 0);
    let mut input = wire.as_slice();
    assert!(matches!(b.read(&mut input).unwrap(), Some(Incoming::Heartbeat(None))));
    let Some(Incoming::Signal(invite)) = b.read(&mut input).unwrap() else { panic!("Invite") };
    let mut callee = CallSession::incoming_busy(b, &invite, header(), make, 0).unwrap();
    assert_eq!(callee.phase(), call::Phase::Ended);
    let response = flush(&mut callee, 0);
    assert_eq!(caller.on_input(&mut response.as_slice(), 0).unwrap(), [Notice::Ended(Reason::Busy)]);
    assert_eq!(caller.take_remote().iter().map(|m| m.signal).collect::<Vec<_>>(), [Signal::Decline]);
    assert!(!callee.answered() && callee.controls.is_empty());
}

#[test]
fn pipelined_call_accept_is_processed_without_waiting_for_another_carrier_packet() {
    for enabled in [false, true] {
        let (mut caller, mut callee, _) = dial(true, false);
        caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
        callee.accept(true).unwrap();
        let mut buffered = flush(&mut callee, 2 * MS);
        let saved = buffered.clone();
        let mut notices = crate::dial::feed_buffered(&mut caller, &mut buffered, false, 3 * MS, enabled).unwrap();
        if !enabled {
            assert!(notices.is_empty()); assert_eq!(buffered, saved);
            assert_eq!(caller.phase(), call::Phase::OutgoingRinging, "the control arm waits for a new packet");
            notices = crate::dial::feed_buffered(&mut caller, &mut buffered, true, 4 * MS, enabled).unwrap();
        }
        assert!(buffered.is_empty());
        assert!(notices.contains(&Notice::StartMedia { camera: false }));
        assert!(notices.contains(&Notice::PeerHeader(header())));
        caller.media_ready().unwrap();
        callee.on_input(&mut flush(&mut caller, 5 * MS).as_slice(), 5 * MS).unwrap();
        caller.hang_up(Reason::Normal).unwrap();
        assert_eq!(callee.on_input(&mut flush(&mut caller, 6 * MS).as_slice(), 6 * MS).unwrap(), [Notice::Ended(Reason::Normal)]);
    }
}

#[test]
fn a_relayed_call_keeps_endpoint_records_opaque_through_answer_audio_and_hangup() {
    use crate::{framing::HopReader, records::{Layer, Policy, Sealer}};
    struct Relay { incoming: HopReader, outgoing: Sealer }
    impl Relay {
        fn forward(&mut self, bytes: &[u8]) -> Vec<u8> {
            let mut input = bytes; let mut out = Vec::new();
            while let Some(record) = self.incoming.read(&mut input, 1024 * 1024).unwrap() {
                let heartbeat = crate::link::keepalive();
                let body = if record.header.kind == 6 { heartbeat.as_slice() } else { record.plaintext.as_slice() };
                if record.header.kind == 5 {
                    assert!(record.plaintext.starts_with(b"BME1"));
                    assert!(crate::records::Opener::new(Layer::Endpoint, &[1; 32], Policy::default()).unwrap().open(&record.plaintext).is_err(), "adjacent-hop keys cannot open endpoint records");
                }
                out.extend(self.outgoing.seal(record.header.kind, record.header.flags, body).unwrap());
            }
            assert!(input.is_empty()); out
        }
    }
    let (caller_link, mut callee_link) = (
        CallLink::new(&[1; 32], &[2; 32], &[3; 32], &[4; 32], false).unwrap(),
        CallLink::new(&[5; 32], &[6; 32], &[4; 32], &[3; 32], false).unwrap());
    let mut forward = Relay { incoming: HopReader::new(&[1; 32], Policy::default()).unwrap(), outgoing: Sealer::new(Layer::Hop, &[6; 32], Policy::default()).unwrap() };
    let mut reverse = Relay { incoming: HopReader::new(&[5; 32], Policy::default()).unwrap(), outgoing: Sealer::new(Layer::Hop, &[2; 32], Policy::default()).unwrap() };
    let mut caller = CallSession::outgoing(caller_link, ID, true, 4, header(), make, 0).unwrap();
    let wire = forward.forward(&flush(&mut caller, 0)); let mut input = wire.as_slice();
    assert!(matches!(callee_link.read(&mut input).unwrap(), Some(Incoming::Heartbeat(None))));
    let Some(Incoming::Signal(invite)) = callee_link.read(&mut input).unwrap() else { panic!("Invite") };
    assert_eq!(invite.relay_hops, 4);
    let (mut callee, _) = CallSession::incoming(callee_link, &invite, header(), make, 0).unwrap();
    callee.on_input(&mut input, 0).unwrap();
    assert_eq!(callee.relay_hops(), 4);
    callee.accept(true).unwrap();
    let notices = caller.on_input(&mut reverse.forward(&flush(&mut callee, MS)).as_slice(), MS).unwrap();
    assert!(notices.contains(&Notice::StartMedia { camera: false }));
    caller.media_ready().unwrap(); callee.media_ready().unwrap();
    caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
    caller.send(packet(AUDIO, 20_000, 4, 7)).unwrap();
    callee.on_input(&mut forward.forward(&flush(&mut caller, 2 * MS)).as_slice(), 2 * MS).unwrap();
    let mut heard = Vec::new(); let mut pcm = [0; 8];
    callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, samples| heard.push(samples.to_vec())).unwrap();
    assert_eq!(heard, [vec![7; 4]]);
    caller.hang_up(Reason::Normal).unwrap();
    assert_eq!(callee.on_input(&mut forward.forward(&flush(&mut caller, 3 * MS)).as_slice(), 3 * MS).unwrap(), [Notice::Ended(Reason::Normal)]);
}

#[test]
fn a_voice_call_rings_as_voice_answers_without_camera_and_carries_audio_both_ways() {
    let (mut caller, mut callee, notices) = dial(true, false);
    assert_eq!(
        notices,
        vec![Notice::Ring { voice: false }, Notice::Ring { voice: true }]
    );
    let answer = callee.accept(true).unwrap();
    assert_eq!(answer, vec![Notice::StartMedia { camera: false }]);
    let wire = flush(&mut callee, MS);
    let got = caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert_eq!(
        got,
        vec![
            Notice::StartMedia { camera: false },
            Notice::PeerCamera(false),
            Notice::PeerHeader(header())
        ]
    );
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    // Video is never sent in a voice call; audio is.
    assert!(!caller.send(packet(KEY_FRAME, 0, 100, 1)).unwrap());
    let wire = flush(&mut caller, 2 * MS);
    callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert!(caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap());
    assert!(caller.send(packet(AUDIO, 0, 4, 5)).unwrap());
    assert!(callee.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap());
    assert!(callee.send(packet(AUDIO, 0, 2, 6)).unwrap());
    let to_callee = flush(&mut caller, 3 * MS);
    let to_caller = flush(&mut callee, 3 * MS);
    callee.on_input(&mut to_callee.as_slice(), 3 * MS).unwrap();
    caller.on_input(&mut to_caller.as_slice(), 3 * MS).unwrap();
    let mut pcm = [0; 8];
    let mut heard = Vec::new();
    callee
        .audio()
        .pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec()))
        .unwrap();
    assert_eq!(heard, vec![vec![5; 4]]);
    heard.clear();
    caller
        .audio()
        .pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec()))
        .unwrap();
    assert_eq!(heard, vec![vec![6; 2]]);
    // Hold-to-talk released: captured audio is dropped, the peer sees the state.
    caller.push_to_talk(false).unwrap();
    assert!(!caller.send(packet(AUDIO, 20_000, 4, 5)).unwrap());
    let wire = flush(&mut caller, 4 * MS);
    assert_eq!(
        callee.on_input(&mut wire.as_slice(), 4 * MS).unwrap(),
        vec![Notice::PeerMicrophone(false)]
    );
    // Hang-up ends both sides.
    caller.hang_up(Reason::Normal).unwrap();
    let wire = flush(&mut caller, 5 * MS);
    assert_eq!(
        callee.on_input(&mut wire.as_slice(), 5 * MS).unwrap(),
        vec![Notice::Ended(Reason::Normal)]
    );
    assert_eq!(callee.phase(), call::Phase::Ended);
}

#[test]
fn an_unanswered_call_cannot_prime_a_decoder_or_play_injected_audio_with_the_media_gate() {
    for enabled in [false, true] {
        for phase in [call::Phase::OutgoingInvite, call::Phase::OutgoingRinging, call::Phase::IncomingRinging] {
            let (mut caller, mut callee, _) = dial(true, false);
            if phase == call::Phase::OutgoingRinging {
                caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
            }
            let (sender, receiver) = if phase == call::Phase::IncomingRinging { (&mut caller, &mut callee) } else { (&mut callee, &mut caller) };
            receiver.media_gate = enabled;
            assert_eq!(receiver.phase(), phase);
            // Bypass the honest sender's controller to model an authenticated
            // peer that sends its header/config/audio before an answer.
            let mut wire = sender.link.header(&header()).unwrap();
            wire.extend(sender.link.media(&packet(AUDIO | CONFIG, 0, 3, 9)).unwrap().concat());
            wire.extend(sender.link.media(&packet(AUDIO, 0, 4, 7)).unwrap().concat());
            let result = receiver.on_input(&mut wire.as_slice(), 2 * MS);
            if enabled {
                assert_eq!(result, Err(Error::Call(call::Error::State)));
                assert!(receiver.video().is_none());
                assert_eq!(receiver.audio().queued(), 0);
            } else { assert!(result.unwrap().contains(&Notice::PeerHeader(header()))); }
            let mut heard = Vec::new(); let mut pcm = [0; 8];
            receiver.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec())).unwrap();
            assert_eq!(heard, if enabled { Vec::new() } else { vec![vec![7; 4]] }, "retained control exposes early playback");
            assert_eq!(receiver.phase(), phase);
        }
    }
}

#[test]
fn answered_media_is_admitted_but_hangup_clears_received_queues_and_drops_late_media() {
    for local_hangup in [false, true] {
        let (mut caller, mut callee, _) = dial(false, true);
        caller.media_gate = true; callee.media_gate = true;
        callee.accept(false).unwrap();
        // Accept and header arrive together: the phase changes before the header.
        let notices = caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
        assert!(notices.contains(&Notice::StartMedia { camera: true }));
        assert!(notices.contains(&Notice::PeerHeader(header())));
        caller.media_ready().unwrap(); callee.media_ready().unwrap();
        caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
        caller.send(packet(AUDIO, 0, 4, 7)).unwrap();
        caller.send(packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
        callee.on_input(&mut flush(&mut caller, 2 * MS).as_slice(), 2 * MS).unwrap();
        assert_eq!(callee.audio().queued(), 1);
        assert_eq!(callee.video().unwrap().queued_frames(), 1);
        if local_hangup { callee.hang_up(Reason::Normal).unwrap(); }
        else {
            caller.hang_up(Reason::Normal).unwrap();
            assert_eq!(callee.on_input(&mut flush(&mut caller, 3 * MS).as_slice(), 3 * MS).unwrap(), [Notice::Ended(Reason::Normal)]);
        }
        // Even an incomplete late frame is dropped before reassembly.
        let plan = caller.link.plan(&packet(KEY_FRAME, 0, 5_000, 3)).unwrap();
        let mut late = caller.link.seal_part(&plan, 0).unwrap();
        late.extend(caller.link.header(&header()).unwrap());
        late.extend(caller.link.media(&packet(AUDIO, 0, 4, 8)).unwrap().concat());
        late.extend(caller.link.heartbeat(None).unwrap());
        assert!(callee.on_input(&mut late.as_slice(), 4 * MS).unwrap().is_empty());
        assert!(callee.video().is_none());
        let mut heard = Vec::new(); let mut pcm = [0; 8];
        callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec())).unwrap();
        assert!(heard.is_empty());
        let mut decoder = crate::viewer::DiscardFrames::default();
        assert_eq!(callee.pump_video(&mut decoder, i64::MAX / 2, 0).unwrap(), 0);
        assert_eq!(decoder.inputs, 0);
        let mut forged = caller.link.media(&packet(AUDIO, 0, 4, 8)).unwrap().concat();
        *forged.last_mut().unwrap() ^= 1;
        assert!(callee.on_input(&mut forged.as_slice(), 5 * MS).is_err(), "Hang-up does not bypass authentication");
    }
}

#[test]
fn call_mute_discards_queued_microphone_audio_keeps_setup_and_fences_old_input_on_unmute() {
    let (mut caller, mut callee, _) = dial(true, false);
    callee.accept(true).unwrap();
    caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
    caller.media_ready().unwrap(); callee.media_ready().unwrap();
    callee.on_input(&mut flush(&mut caller, 2 * MS).as_slice(), 2 * MS).unwrap();
    caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
    caller.send(packet(AUDIO, 0, 4, 5)).unwrap();
    caller.push_to_talk(false).unwrap(); caller.discard_queued_audio();
    assert!(!caller.send(packet(AUDIO, 20_000, 4, 6)).unwrap());
    let notices = callee.on_input(&mut flush(&mut caller, 3 * MS).as_slice(), 3 * MS).unwrap();
    assert_eq!(notices, [Notice::PeerMicrophone(false)]);
    let mut pcm = [0; 8]; let mut heard = Vec::new();
    callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec())).unwrap();
    assert!(heard.is_empty(), "no queued microphone audio is delivered after Mute");

    let mut fence = crate::stream_source::CaptureFence::default();
    let fresh = fence.restart();
    caller.push_to_talk(true).unwrap();
    let (captured, events) = std::sync::mpsc::channel();
    captured.send(crate::call_audio::Event::Frame(Box::new([7; 960]))).unwrap();
    captured.send(crate::call_audio::Event::CaptureRestarted(fresh)).unwrap();
    captured.send(crate::call_audio::Event::Frame(Box::new([8; 960]))).unwrap();
    drop(captured);
    let mut pts = 40_000;
    for event in events {
        match event {
            crate::call_audio::Event::CaptureRestarted(generation) => fence.acknowledged(generation),
            crate::call_audio::Event::Frame(mut frame) => {
                fence.scrub(false, &mut frame[..]);
                // Fake decoder exposes the generated input byte as PCM.
                assert!(caller.send(packet(AUDIO, pts, 4, frame[0] as u8)).unwrap()); pts += 20_000;
            }
            _ => unreachable!(),
        }
    }
    let notices = callee.on_input(&mut flush(&mut caller, 4 * MS).as_slice(), 4 * MS).unwrap();
    assert_eq!(notices, [Notice::PeerMicrophone(true)]);
    callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec())).unwrap();
    assert_eq!(heard, [vec![0; 4], vec![8; 4]], "old capture stays silent; only acknowledged fresh input passes");
    caller.hang_up(Reason::Normal).unwrap();
    assert_eq!(callee.on_input(&mut flush(&mut caller, 5 * MS).as_slice(), 5 * MS).unwrap(), [Notice::Ended(Reason::Normal)]);
}

#[test]
fn a_video_call_puts_audio_ahead_of_video_and_between_fragments() {
    let (mut caller, mut callee, _) = dial(false, true);
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    // Config, a 5000-byte keyframe (fragmented) and two audio packets.
    caller.send(packet(CONFIG, 0, 3, 1)).unwrap();
    caller.send(packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
    caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
    let mut records = Vec::new();
    // Header, audio config (ahead of video), video config, first keyframe fragment...
    while let Some(r) = caller.next_write(2 * MS).unwrap() {
        records.push(r);
        if records.len() == 4 {
            // An audio packet captured while the keyframe is half sent goes between fragments.
            caller.send(packet(AUDIO, 20_000, 4, 5)).unwrap();
        }
    }
    let wire = records.concat();
    let notices = callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert_eq!(notices, vec![Notice::PeerHeader(header())]);
    let mut pcm = [0; 8];
    let mut heard = Vec::new();
    callee
        .audio()
        .pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec()))
        .unwrap();
    assert_eq!(heard, vec![vec![5; 4]]);
    // The video receiver got configuration and the whole reassembled keyframe.
    let video = callee.video().unwrap();
    assert_eq!(video.queued_frames(), 1);
    // A link silent for 8 s ends the call: once, Hang-up/Error goes out
    // first (PROTOCOL "Hang-up before a liveness close"), then Dead.
    assert!(caller.is_dead(9_000 * MS));
    let last = caller.next_write(9_000 * MS).unwrap().expect("the liveness hang-up");
    let mut ended = callee.on_input(&mut last.as_slice(), 9_000 * MS).unwrap();
    assert!(ended.iter().any(|n| matches!(n, Notice::Ended(Reason::Error))), "{ended:?}");
    ended.clear();
    assert_eq!(caller.next_write(9_000 * MS).err(), Some(Error::Dead));
    // Camera off drops queued video and stops new video.
    let (mut caller, mut callee, _) = dial(false, false);
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.send(packet(KEY_FRAME, 0, 100, 2)).unwrap();
    caller.set_camera(false).unwrap();
    assert!(!caller.send(packet(KEY_FRAME, 0, 100, 2)).unwrap());
    let wire = flush(&mut caller, 2 * MS);
    assert_eq!(
        callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap(),
        vec![Notice::PeerHeader(header()), Notice::PeerCamera(false)]
    );
}

#[test]
fn camera_off_and_requests_wait_for_the_started_frame_while_audio_interleaves() {
    for enabled in [false, true] {
        let (mut caller, mut callee, _) = dial(false, true);
        caller.ordered_fragments = enabled;
        caller.media_gate = true; callee.media_gate = true;
        callee.accept(false).unwrap();
        caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
        caller.media_ready().unwrap(); callee.media_ready().unwrap();
        caller.send(packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
        // Deliver the header/report and first fragment, but not the whole frame.
        while caller.fragmenting.is_none() {
            let record = caller.next_write(2 * MS).unwrap().expect("first fragment");
            callee.on_input(&mut record.as_slice(), 2 * MS).unwrap();
        }
        assert_eq!(callee.video().unwrap().queued_frames(), 0);
        caller.send(packet(KEY_FRAME, 20_000, 100, 3)).unwrap();
        caller.set_camera(false).unwrap();
        caller.request_keyframe().unwrap();
        caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
        caller.send(packet(AUDIO, 0, 4, 7)).unwrap();
        assert!(!caller.send(packet(KEY_FRAME, 40_000, 100, 4)).unwrap());
        if !enabled {
            let control = caller.next_write(3 * MS).unwrap().unwrap();
            assert_eq!(callee.on_input(&mut control.as_slice(), 3 * MS), Err(Error::Call(call::Error::Fragment)),
                "the control arm inserts Camera off into an unfinished frame");
            continue;
        }
        // Audio config and audio arrive before the remaining video fragments.
        for _ in 0..2 {
            let audio = caller.next_write(3 * MS).unwrap().unwrap();
            assert!(callee.on_input(&mut audio.as_slice(), 3 * MS).unwrap().is_empty());
            assert_eq!(callee.video().unwrap().queued_frames(), 0);
        }
        assert_eq!(callee.audio().queued(), 1);
        let notices = callee.on_input(&mut flush(&mut caller, 4 * MS).as_slice(), 4 * MS).unwrap();
        assert_eq!(notices, [Notice::PeerCamera(false), Notice::KeyFrameRequested]);
        assert_eq!(callee.video().unwrap().queued_frames(), 1, "only the already-started frame finishes");
        assert_eq!(caller.stats().video_queued, 0, "Camera off discards unstarted frames");
        let mut heard = Vec::new(); let mut pcm = [0; 8];
        callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s.to_vec())).unwrap();
        assert_eq!(heard, [vec![7; 4]]);
        // Re-enable video with a fresh keyframe, then end the surviving call.
        caller.set_camera(true).unwrap();
        assert!(caller.send(packet(KEY_FRAME, 60_000, 100, 5)).unwrap());
        let notices = callee.on_input(&mut flush(&mut caller, 5 * MS).as_slice(), 5 * MS).unwrap();
        assert_eq!(notices, [Notice::PeerCamera(true)]);
        assert_eq!(callee.phase(), call::Phase::Active);
        caller.hang_up(Reason::Normal).unwrap();
        assert_eq!(callee.on_input(&mut flush(&mut caller, 6 * MS).as_slice(), 6 * MS).unwrap(), [Notice::Ended(Reason::Normal)]);
    }
}

#[test]
fn authenticated_video_fragments_keep_either_call_role_alive_before_the_frame_completes() {
    for enabled in [false, true] {
        for receiver_is_caller in [false, true] {
            let (mut caller, mut callee, _) = dial(false, true);
            caller.media_gate = true; callee.media_gate = true;
            callee.accept(false).unwrap();
            caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
            caller.media_ready().unwrap(); callee.media_ready().unwrap();
            callee.on_input(&mut flush(&mut caller, 2 * MS).as_slice(), 2 * MS).unwrap();
            caller.on_input(&mut flush(&mut callee, 2 * MS).as_slice(), 2 * MS).unwrap();
            let (sender, receiver) = if receiver_is_caller { (&mut callee, &mut caller) } else { (&mut caller, &mut callee) };
            receiver.ordered_fragments = enabled;
            let plan = sender.link.plan(&packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
            for part in 0..plan.len() {
                let now = (part as u64 + 1) * 7_000 * MS;
                let wire = sender.link.seal_part(&plan, part).unwrap();
                receiver.on_input(&mut wire.as_slice(), now).unwrap();
                if !enabled {
                    assert!(receiver.is_dead(now + 2_000 * MS), "the old arm ignores a valid partial frame");
                    assert_eq!(receiver.video().unwrap().queued_frames(), 0);
                    break;
                }
                assert!(!receiver.is_dead(now + 2_000 * MS));
                assert!(!receiver.is_dead(now + 8_000 * MS - 1));
                assert!(receiver.is_dead(now + 8_000 * MS), "the silence deadline stays exactly eight seconds");
                assert_eq!(receiver.video().unwrap().queued_frames(), usize::from(part + 1 == plan.len()));
                // Only video fragments arrive in this direction; the peer's
                // ordinary return heartbeats keep the sender alive as well.
                sender.on_input(&mut flush(receiver, now + MS).as_slice(), now + MS).unwrap();
                assert!(!sender.is_dead(now + 2 * MS));
            }
        }
    }
}

#[test]
fn partial_tampered_and_replayed_fragment_bytes_do_not_extend_call_liveness() {
    for replay in [false, true] {
        let (mut caller, mut callee, _) = dial(false, true);
        callee.ordered_fragments = true;
        caller.media_gate = true; callee.media_gate = true;
        callee.accept(false).unwrap();
        caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
        caller.media_ready().unwrap(); callee.media_ready().unwrap();
        callee.on_input(&mut flush(&mut caller, 2 * MS).as_slice(), 2 * MS).unwrap();
        let plan = caller.link.plan(&packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
        let mut wire = caller.link.seal_part(&plan, 0).unwrap();
        if replay {
            callee.on_input(&mut wire.as_slice(), 7_000 * MS).unwrap();
            assert!(!callee.is_dead(9_000 * MS));
            assert!(callee.on_input(&mut wire.as_slice(), 14_000 * MS).is_err());
            assert!(callee.is_dead(15_000 * MS));
        } else {
            let split = wire.len() / 2;
            callee.on_input(&mut &wire[..split], 7_000 * MS).unwrap();
            assert!(callee.is_dead(9_000 * MS), "unauthenticated carrier progress is insufficient");
            *wire.last_mut().unwrap() ^= 1;
            assert!(callee.on_input(&mut &wire[split..], 9_000 * MS).is_err());
            assert!(callee.is_dead(9_000 * MS));
        }
    }
}

#[test]
fn an_authenticated_hangup_during_a_partial_frame_ends_either_call_role_normally() {
    for enabled in [false, true] {
        for receiver_is_caller in [false, true] {
            for reason in [Reason::Normal, Reason::Error] {
                let (mut caller, mut callee, _) = dial(false, true);
                caller.media_gate = true; callee.media_gate = true;
                callee.accept(false).unwrap();
                caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
                caller.media_ready().unwrap(); callee.media_ready().unwrap();
                let (sender, receiver) = if receiver_is_caller { (&mut callee, &mut caller) } else { (&mut caller, &mut callee) };
                receiver.link.allow_fragment_controls(enabled);
                sender.send(packet(KEY_FRAME, 0, 5_000, 2)).unwrap();
                while sender.fragmenting.is_none() {
                    let record = sender.next_write(2 * MS).unwrap().expect("first fragment");
                    receiver.on_input(&mut record.as_slice(), 2 * MS).unwrap();
                }
                sender.hang_up(reason).unwrap();
                let result = receiver.on_input(&mut flush(sender, 3 * MS).as_slice(), 3 * MS);
                if !enabled { assert_eq!(result, Err(Error::Call(call::Error::Fragment))); continue; }
                assert_eq!(result, Ok(vec![Notice::Ended(reason)]));
                assert_eq!(receiver.phase(), call::Phase::Ended);
                assert!(receiver.video().is_none());
                assert_eq!(receiver.audio().queued(), 0);
                // A late fragment is authenticated/dropped by the ended gate.
                let plan = sender.link.plan(&packet(KEY_FRAME, 20_000, 5_000, 3)).unwrap();
                let late = sender.link.seal_part(&plan, 0).unwrap();
                assert!(receiver.on_input(&mut late.as_slice(), 4 * MS).unwrap().is_empty());
            }
        }
    }
}

/// Pseudonymous call: the caller requires a verified callee REVEAL that opens
/// the selected rotating ID before Accept counts, then reveals itself.
#[test]
fn a_pseudonymous_callee_must_reveal_before_accept_and_the_caller_reveals_back() {
    use crate::{
        bmh3::reveal::{ROLE_CALLEE, ROLE_CALLER, RevealSession},
        call::{Message, Signal},
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
        session::tests::hex,
    };
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap();
    let spki = |name: &str| -> [u8; 91] {
        hex(v["link"][name]["ephemeralKey"].as_str().unwrap())
            .try_into()
            .unwrap()
    };
    // Throwaway keys of both sides (any canonical SPKI) and the two real identities.
    let (caller_pseudo, callee_pseudo) = (spki("initiatorHello"), spki("responderHello"));
    let (caller_real, callee_real, _) = identities();
    let callee_real_spki = crate::bmh2::SigningIdentity::public_key(&callee_real).unwrap();
    let callee_node: [u8; 16] = crate::crypto::sha256(&callee_real_spki).unwrap()[..16]
        .try_into()
        .unwrap();
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &callee_node, epoch, 0).unwrap();
    let exporter = [7u8; 32];
    for accept_first in [true, false] {
        // Call links that accept BDX1 (type 11) in both directions.
        let (a, b, _) = identities();
        let (al, bl) = handshake(&a, &b, Purpose::Link);
        let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
        let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
            CallLink::continue_link(
                Sealer::new(Layer::Hop, s.send_key(), Policy::default()).unwrap(),
                crate::framing::HopReader::new(s.receive_key(), Policy::default()).unwrap(),
                e.send_key(),
                e.receive_key(),
                false,
                true,
            )
            .unwrap()
        };
        let mut callee_link = link(&bl, &be);
        let mut caller =
            CallSession::<Fake>::outgoing(link(&al, &ae), ID, true, 0, header(), make, 0).unwrap();
        caller.require_reveal(RevealGate {
            session: RevealSession::new(
                ROLE_CALLER,
                &exporter,
                caller_pseudo,
                callee_pseudo,
                true,
                true,
                vec![caller_pseudo],
            )
            .unwrap(),
            selected: entry.rdi,
            rotating: true,
            real: Box::new(identities().0),
            wall_epoch: epoch,
        });
        // The callee link consumes the caller's opening records (heartbeat, Invite, Camera off).
        let opening = flush(&mut caller, 0);
        let mut input = opening.as_slice();
        while callee_link.read(&mut input).unwrap().is_some() {}
        let mut callee_reveal = RevealSession::new(
            ROLE_CALLEE,
            &exporter,
            callee_pseudo,
            caller_pseudo,
            true,
            true,
            vec![callee_pseudo],
        )
        .unwrap();
        let mut wire = callee_link.heartbeat(None).unwrap();
        wire.extend(
            callee_link
                .signal(&Message::simple(ID, Signal::Ringing).unwrap())
                .unwrap(),
        );
        let reveal = callee_reveal
            .send_own(&callee_real, std::slice::from_ref(&entry))
            .unwrap();
        if !accept_first {
            wire.extend(callee_link.discovery(&reveal).unwrap());
        }
        wire.extend(
            callee_link
                .signal(&Message::simple(ID, Signal::Accept).unwrap())
                .unwrap(),
        );
        let result = caller.on_input(&mut wire.as_slice(), MS);
        if accept_first {
            assert_eq!(result.err(), Some(Error::Reveal));
            continue;
        }
        let notices = result.unwrap();
        assert_eq!(
            notices,
            vec![
                Notice::PeerRevealed {
                    node_id: callee_node,
                    fingerprint: crate::crypto::sha256(&callee_real_spki).unwrap(),
                },
                Notice::StartMedia { camera: false }
            ]
        );
        // The caller reveal is the first thing written after that.
        let record = caller.next_write(2 * MS).unwrap().unwrap();
        let mut input = record.as_slice();
        let Some(Incoming::Discovery(caller_record)) = callee_link.read(&mut input).unwrap() else {
            panic!("caller reveal first");
        };
        let verified = callee_reveal.on_peer_reveal(&caller_record).unwrap();
        let caller_real_spki = crate::bmh2::SigningIdentity::public_key(&caller_real).unwrap();
        assert_eq!(verified.spki, caller_real_spki);
    }
}

/// Generated call video end to end: the caller's picture is encoded once by
/// the pinned software AV1 encoder at Android's call defaults after Accept,
/// sealed by the session and queued as a decodable keyframe at the callee.
#[test]
fn a_video_call_sends_an_encoded_test_pattern_keyframe_first() {
    use crate::{
        call_video::{CallVideo, header_for},
        video::{Availability, Backend, DecodeDevice, MASK_AV1},
    };
    let available = Availability {
        av1_hardware: Vec::new(),
        vp9_hardware: Vec::new(),
        av1_software: crate::video::aom_version(),
        vp9_software: crate::video::vpx_version(),
        ..Availability::default()
    };
    let (header, plan) = header_for(MASK_AV1, &available);
    assert_eq!(plan, vec![(3, Backend::SoftwareAv1)]);
    let (a, mut b) = links(true);
    let mut caller = CallSession::outgoing(a, ID, false, 0, header, make, 0).unwrap();
    let wire = flush(&mut caller, 0);
    let mut input = wire.as_slice();
    let Some(Incoming::Heartbeat(None)) = b.read(&mut input).unwrap() else {
        panic!()
    };
    let Some(Incoming::Signal(invite)) = b.read(&mut input).unwrap() else {
        panic!()
    };
    let (mut callee, _) = CallSession::incoming(b, &invite, header, make, 0).unwrap();
    callee.on_input(&mut input, 0).unwrap();
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    let notices = caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(notices.contains(&Notice::StartMedia { camera: true }));
    caller.media_ready().unwrap();
    let (gpu, _, _) = crate::video::device(true, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let (mut video, name) = CallVideo::open(&owner, &header, &plan).unwrap();
    assert!(!name.is_empty());
    // Inline the packets come back from tick; on the encode thread they
    // come back when the thread finishes the picture.
    let ticked = video.tick(&mut caller).unwrap();
    let waited = video.wait_encoded(&mut caller, std::time::Duration::from_secs(10)).unwrap();
    assert!(ticked + waited >= 1, "threaded={}", video.threaded());
    assert_eq!((video.sent, video.refused), (1, 0));
    let wire = flush(&mut caller, 2 * MS);
    let notices = callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert!(notices.contains(&Notice::PeerHeader(header)));
    assert_eq!(callee.video().unwrap().queued_frames(), 1);
    // The next picture is not due yet: nothing is encoded in a burst.
    assert_eq!(video.tick(&mut caller).unwrap(), 0);
}

/// An owner that never drains the peer's video no longer ends the call: the
/// receive queue overflows, drops to the next keyframe and asks the peer for
/// one (A15s run: `Call: Video(Full)` after 16 s of undrained video).
#[test]
fn a_full_receive_queue_drops_to_the_next_keyframe_and_asks_for_one() {
    let (mut caller, mut callee, _) = dial(false, false);
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    let mut now = 2 * MS;
    for i in 0..(crate::viewer::MAX_QUEUED_FRAMES as i64 + 5) {
        let flags = if i == 0 { KEY_FRAME } else { 0 };
        assert!(callee.send(packet(flags, i * 66_000, 40, 3)).unwrap());
        let wire = flush(&mut callee, now);
        caller.on_input(&mut wire.as_slice(), now).unwrap();
        now += MS;
    }
    let stats = caller.stats();
    assert_eq!(stats.video_overflows, 1);
    assert_eq!(stats.video_rx_queued, 0);
    assert!(caller.video().unwrap().waiting_for_keyframe());
    // The keyframe request reaches the peer.
    let wire = flush(&mut caller, now);
    let notices = callee.on_input(&mut wire.as_slice(), now).unwrap();
    assert!(notices.contains(&Notice::KeyFrameRequested));
    // Inter frames are skipped until the keyframe, which resumes the queue.
    callee.send(packet(0, 99_000_000, 40, 3)).unwrap();
    callee.send(packet(KEY_FRAME, 99_066_000, 40, 3)).unwrap();
    let wire = flush(&mut callee, now);
    caller.on_input(&mut wire.as_slice(), now).unwrap();
    assert_eq!(caller.video().unwrap().queued_frames(), 1);
    assert!(!caller.video().unwrap().waiting_for_keyframe());
    // A discarding owner keeps the queue empty.
    let mut discard = crate::viewer::DiscardFrames::default();
    caller.pump_video(&mut discard, i64::MAX / 4, 0).unwrap();
    assert_eq!(caller.stats().video_rx_queued, 0);
    assert!(discard.inputs >= 1);
}

#[test]
fn a_contact_callers_first_record_from_the_callee_must_be_an_opening_of_the_selected_offer() {
    use crate::{
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
    };
    let node = [0x42u8; 16];
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &node, epoch, 0).unwrap();
    let opening = [b"BDX1".as_slice(), &[1], &rotating::encode_body(std::slice::from_ref(&entry)).unwrap()].concat();
    let other = OpeningEntry::own(&[6; 32], &node, epoch, 0).unwrap();
    let wrong = [b"BDX1".as_slice(), &[1], &rotating::encode_body(std::slice::from_ref(&other)).unwrap()].concat();
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link(
            Sealer::new(Layer::Hop, s.send_key(), Policy::default()).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), Policy::default()).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
        )
        .unwrap()
    };
    let gate = |node_id| OpeningGate {
        selected: entry.rdi,
        node_id,
        wall_epoch: epoch,
    };
    // (first record from the callee, gate node, accepted)
    let cases: [(&[u8], [u8; 16], bool); 4] =
        [(&opening, node, true), (&wrong, node, false), (&opening, [0x43; 16], false), (b"", node, false)];
    for (first, gate_node, accepted) in cases {
        let mut caller = CallSession::<Fake>::outgoing(link(&al, &ae), ID, true, 0, header(), make, 0).unwrap();
        caller.require_opening(gate(gate_node));
        let mut callee = link(&bl, &be);
        let wire = if first.is_empty() {
            // An Accept-like signal before any OPENING.
            callee.signal(&crate::call::Message::simple(ID, crate::call::Signal::Ringing).unwrap()).unwrap()
        } else {
            callee.discovery(first).unwrap()
        };
        let result = caller.on_input(&mut wire.as_slice(), 0);
        assert_eq!(result.is_ok(), accepted, "{result:?}");
        if !accepted {
            assert_eq!(result.err(), Some(Error::Opening));
        }
    }
}

#[test]
fn a_mid_call_display_rotation_is_reported_and_never_ends_the_call() {
    // CF-REC-21 / CF-MEDIA-51, shared vectors stream-protocol-014 and -015.
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/vectors/conformance/stream-protocol.json")).unwrap();
    let find = |id: &str| v["vectors"].as_array().unwrap().iter().find(|x| x["id"] == id).unwrap().clone();
    let hex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
    let ok = find("stream-protocol-014");
    let flags = ok["expected"]["packet_flags"].as_u64().unwrap() as u8;
    for (payload, degrees) in ok["expected"]["valid_payloads"].as_object().unwrap() {
        let (a, mut b) = links(false);
        let mut caller = CallSession::<Fake>::outgoing(a, ID, false, 0, header(), make, 0).unwrap();
        let accept = b.signal(&Message::simple(ID, Signal::Accept).unwrap()).unwrap();
        caller.on_input(&mut accept.as_slice(), 0).unwrap();
        caller.media_ready().unwrap();
        // After Accept, rotation can precede the peer's header.
        let mut wire = b.media(&media::Packet { flags, pts_us: 0, payload: &hex(payload) }.encode().unwrap()).unwrap().concat();
        wire.extend(b.header(&header()).unwrap());
        let notices = caller.on_input(&mut wire.as_slice(), MS).unwrap();
        let turns = (degrees.as_u64().unwrap() / 90) as u8;
        assert_eq!(notices.first(), Some(&Notice::PeerRotation(turns)));
        assert!(notices.contains(&Notice::PeerHeader(header())));
    }
    for payload in find("stream-protocol-015")["inputs"]["payloads"].as_array().unwrap() {
        let (a, mut b) = links(false);
        let mut caller = CallSession::<Fake>::outgoing(a, ID, false, 0, header(), make, 0).unwrap();
        let accept = b.signal(&Message::simple(ID, Signal::Accept).unwrap()).unwrap();
        caller.on_input(&mut accept.as_slice(), 0).unwrap();
        caller.media_ready().unwrap();
        let bytes = media::Packet { flags, pts_us: 0, payload: &hex(payload.as_str().unwrap()) }.encode().unwrap();
        let wire = b.media(&bytes).unwrap().concat();
        assert!(caller.on_input(&mut wire.as_slice(), MS).is_err(), "{payload}");
    }
}

/// Fails `fail` pushes, then accepts; counts configures.
struct Flaky {
    fail: u32,
    configures: u32,
    out: std::collections::VecDeque<i64>,
}
impl crate::viewer::FrameDecoder for Flaky {
    fn configure(&mut self, _: &crate::video::VideoFormat) -> Result<(), ()> {
        self.configures += 1;
        Ok(())
    }
    fn push(&mut self, _: &[u8], pts: i64, _: bool, config: bool) -> Result<bool, ()> {
        if self.fail > 0 {
            self.fail -= 1;
            return Err(());
        }
        if !config {
            self.out.push_back(pts);
        }
        Ok(true)
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        Ok(self.out.pop_front())
    }
}

#[test]
fn a_failed_call_decoder_is_recreated_twice_with_keyframe_requests_then_only_video_stops() {
    // CF-MEDIA-44 / CF-MEDIA-59.
    let (mut caller, mut callee, _) = dial(false, false);
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    let mut decoder = Flaky { fail: 1, configures: 0, out: Default::default() };
    let mut now = 2 * MS;
    let mut pts = 0i64;
    let mut deliver = |caller: &mut CallSession<Fake>, callee: &mut CallSession<Fake>, flags: u8, now: u64| {
        pts += 66_000;
        assert!(callee.send(packet(flags, pts, 40, 3)).unwrap());
        let wire = flush(callee, now);
        caller.on_input(&mut wire.as_slice(), now).unwrap();
    };
    let asked = |caller: &mut CallSession<Fake>, callee: &mut CallSession<Fake>, now: u64| {
        let wire = flush(caller, now);
        callee.on_input(&mut wire.as_slice(), now).unwrap().contains(&Notice::KeyFrameRequested)
    };
    // First failure: recovered (configured again before the next keyframe), keyframe asked.
    deliver(&mut caller, &mut callee, KEY_FRAME, now);
    caller.pump_video(&mut decoder, i64::MAX / 4, 0).unwrap();
    assert_eq!(caller.video_recoveries(), 1);
    assert!(asked(&mut caller, &mut callee, now));
    deliver(&mut caller, &mut callee, 0, now);
    deliver(&mut caller, &mut callee, KEY_FRAME, now);
    caller.pump_video(&mut decoder, i64::MAX / 4, 0).unwrap();
    assert_eq!(decoder.configures, 2, "recreated from the newest format");
    assert!(!caller.video_stopped());
    // While waiting for a keyframe the request repeats every 2 s, not more often.
    decoder.fail = 1;
    deliver(&mut caller, &mut callee, 0, now);
    caller.pump_video(&mut decoder, 10_000_000_000, 0).unwrap();
    assert_eq!(caller.video_recoveries(), 2);
    assert!(asked(&mut caller, &mut callee, now));
    deliver(&mut caller, &mut callee, 0, now);
    caller.pump_video(&mut decoder, 11_000_000_000, 0).unwrap();
    assert!(!asked(&mut caller, &mut callee, now), "within 2 s");
    caller.pump_video(&mut decoder, 12_000_000_000, 0).unwrap();
    assert!(asked(&mut caller, &mut callee, now), "after 2 s");
    // A third failure stops video only; audio and the call go on.
    decoder.fail = 1;
    deliver(&mut caller, &mut callee, KEY_FRAME, now);
    caller.pump_video(&mut decoder, i64::MAX / 4, 0).unwrap();
    assert!(caller.video_stopped());
    now += MS;
    deliver(&mut caller, &mut callee, KEY_FRAME, now);
    assert!(callee.send(packet(AUDIO | CONFIG, 0, 19, 1)).unwrap());
    let wire = flush(&mut callee, now);
    assert!(caller.on_input(&mut wire.as_slice(), now).is_ok());
    assert_eq!(caller.phase(), call::Phase::Active);
}

#[test]
fn the_peers_camera_coming_back_asks_for_a_keyframe() {
    let (mut caller, mut callee, _) = dial(false, false);
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    callee.set_camera(false).unwrap();
    callee.set_camera(true).unwrap();
    let wire = flush(&mut callee, 2 * MS);
    caller.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    let wire = flush(&mut caller, 2 * MS);
    assert!(callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap().contains(&Notice::KeyFrameRequested));
}

#[test]
fn a_pseudonymous_callee_session_reveals_right_before_its_accept_and_verifies_the_callers_reveal() {
    use crate::{
        bmh3::reveal::{ROLE_CALLEE, ROLE_CALLER, RevealSession},
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
        session::tests::hex,
    };
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap();
    let spki = |name: &str| -> [u8; 91] { hex(v["link"][name]["ephemeralKey"].as_str().unwrap()).try_into().unwrap() };
    let (caller_pseudo, callee_pseudo) = (spki("initiatorHello"), spki("responderHello"));
    let (caller_real, callee_real, _) = identities();
    let caller_real_spki = crate::bmh2::SigningIdentity::public_key(&caller_real).unwrap();
    let callee_real_spki = crate::bmh2::SigningIdentity::public_key(&callee_real).unwrap();
    let callee_node: [u8; 16] = crate::crypto::sha256(&callee_real_spki).unwrap()[..16].try_into().unwrap();
    let caller_node: [u8; 16] = crate::crypto::sha256(&caller_real_spki).unwrap()[..16].try_into().unwrap();
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &callee_node, epoch, 0).unwrap();
    let exporter = [7u8; 32];
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link(
            Sealer::new(Layer::Hop, s.send_key(), Policy::default()).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), Policy::default()).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
        )
        .unwrap()
    };
    let mut caller = CallSession::<Fake>::outgoing(link(&al, &ae), ID, true, 0, header(), make, 0).unwrap();
    caller.require_reveal(RevealGate {
        session: RevealSession::new(ROLE_CALLER, &exporter, caller_pseudo, callee_pseudo, true, true, vec![caller_pseudo])
            .unwrap(),
        selected: entry.rdi,
        rotating: true,
        real: Box::new(caller_real),
        wall_epoch: epoch,
    });
    // The callee (this PC) is created from the caller's Invite.
    let mut callee_link = link(&bl, &be);
    let opening = flush(&mut caller, 0);
    let mut input = opening.as_slice();
    let mut invite = None;
    while let Some(incoming) = callee_link.read(&mut input).unwrap() {
        if let Incoming::Signal(m) = incoming {
            invite.get_or_insert(m);
        }
    }
    let (mut callee, _) = CallSession::<Fake>::incoming(callee_link, &invite.unwrap(), header(), make, 0).unwrap();
    callee.reveal_on_accept(CalleeReveal {
        session: RevealSession::new(ROLE_CALLEE, &exporter, callee_pseudo, caller_pseudo, true, true, vec![callee_pseudo])
            .unwrap(),
        real: Box::new(callee_real),
        entries: vec![entry.clone()],
        relay_hops: 0,
    });
    callee.accept(true).unwrap();
    // The caller accepts only because the REVEAL came first; then it reveals back.
    let wire = flush(&mut callee, MS);
    let notices = caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(notices.contains(&Notice::PeerRevealed {
        node_id: callee_node,
        fingerprint: crate::crypto::sha256(&callee_real_spki).unwrap(),
    }));
    let wire = flush(&mut caller, 2 * MS);
    let notices = callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert!(notices.contains(&Notice::PeerRevealed {
        node_id: caller_node,
        fingerprint: crate::crypto::sha256(&caller_real_spki).unwrap(),
    }));
}

#[test]
fn an_answered_call_sends_v1_first_then_v3_reports_with_the_delivery_counters() {
    let (mut caller, mut callee, _) = dial(true, false);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    assert!(caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap());
    assert!(caller.send(packet(AUDIO, 0, 4, 5)).unwrap());
    let wire = flush(&mut caller, 2 * MS);
    callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    // The caller saw the callee's v1, so its report is v3; the callee then reports v3 too.
    let first = callee.take_peer_reports();
    assert!(first.iter().any(|(_, r)| r.delivery.is_some()), "{first:?}");
    let wire = flush(&mut callee, 1_100 * MS);
    caller.on_input(&mut wire.as_slice(), 1_100 * MS).unwrap();
    let reports = caller.take_peer_reports();
    let (_, last) = reports.last().expect("a report a second later");
    let delivery = last.delivery.expect("v3 after the caller's v3");
    assert_eq!(delivery.received_bytes, 7, "media payload bytes taken off the link");
    assert_eq!(delivery.clock_ms, 2, "the callee's link clock at the last counted byte");
    assert_eq!(last.arrival_kbps, 0, "not measured on this PC yet");
}

#[test]
fn a_verified_name_label_reaches_the_owner_and_a_forged_one_never_fails_the_call() {
    let (_caller, mut callee, _) = dial(true, false);
    let (a, _, _) = identities();
    let spki = crate::bmh2::SigningIdentity::public_key(&a).unwrap();
    let node: [u8; 16] = crate::crypto::sha256(&spki).unwrap()[..16].try_into().unwrap();
    // No labels before they are negotiated.
    let label = crate::labels::sign(&a, crate::labels::KIND_NAME, 1, crate::labels::BINDING_CALL, &ID, "Asha").unwrap();
    let container = crate::labels::container(&[label.clone()]);
    assert!(callee.on_labels(&container).is_empty());
    callee.accept_labels(node);
    assert_eq!(callee.on_labels(&container), vec![Notice::PeerLabel { kind: 1, text: "Asha".into() }]);
    // Bound to another call: dropped and logged, the call goes on.
    let mut other = ID;
    other[0] ^= 1;
    std::thread::sleep(std::time::Duration::from_millis(2_100));
    let replayed = crate::labels::sign(&a, crate::labels::KIND_NAME, 2, crate::labels::BINDING_CALL, &other, "Mallory").unwrap();
    assert!(callee.on_labels(&crate::labels::container(&[replayed])).is_empty());
    assert!(callee.take_label_log().iter().any(|l| l.contains("SECURITY LABEL REJECTED") && l.contains("binding") && !l.contains("Mallory")));
}

#[test]
fn a_call_subject_from_an_older_build_never_reaches_the_owner_and_the_call_goes_on() {
    let (_caller, mut callee, _) = dial(true, false);
    let (a, _, _) = identities();
    let spki = crate::bmh2::SigningIdentity::public_key(&a).unwrap();
    let node: [u8; 16] = crate::crypto::sha256(&spki).unwrap()[..16].try_into().unwrap();
    callee.accept_labels(node);
    // Kind 3 as an older build signs it, sent with its name.
    let mut subject = crate::labels::signed_part(crate::labels::RETIRED_KIND_SUBJECT, &spki, 0, crate::labels::BINDING_CALL, &ID, "Water");
    let signature = crate::bmh2::SigningIdentity::sign(&a, &[b"BluetoothVideoMesh/Label/1\0".as_slice(), subject.as_slice()].concat()).unwrap();
    subject.extend_from_slice(&(signature.len() as u16).to_be_bytes());
    subject.extend_from_slice(&signature);
    let name = crate::labels::sign(&a, crate::labels::KIND_NAME, 1, crate::labels::BINDING_CALL, &ID, "Asha").unwrap();
    assert_eq!(
        callee.on_labels(&crate::labels::container(&[subject, name])),
        vec![Notice::PeerLabel { kind: 1, text: "Asha".into() }]
    );
    assert!(callee.take_label_log().is_empty(), "no error for a retired kind");
    assert!(callee.accept(true).is_ok(), "the call can still be answered");
}

#[test]
fn a_header_outside_the_authenticated_codecs_or_a_second_header_fails_the_call() {
    // The callee announces VP9 (header()); the caller decodes only AV1.
    let (mut caller, mut callee, _) = dial(true, false);
    caller.check_header_codecs(crate::video::MASK_AV1, crate::video::MASK_VP9 | crate::video::MASK_AV1);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    assert!(matches!(caller.on_input(&mut wire.as_slice(), MS), Err(Error::Codec)));
    // The peer did not authenticate VP9 for encoding.
    let (mut caller, mut callee, _) = dial(true, false);
    caller.check_header_codecs(crate::video::MASK_VP9, crate::video::MASK_AV1);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    assert!(matches!(caller.on_input(&mut wire.as_slice(), MS), Err(Error::Codec)));
    // Both masks allow VP9: accepted once; a second header fails the call.
    let (mut caller, mut callee, _) = dial(true, false);
    caller.check_header_codecs(crate::video::MASK_VP9, crate::video::MASK_VP9);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    assert!(caller.on_input(&mut wire.as_slice(), MS).unwrap().contains(&Notice::PeerHeader(header())));
    callee.controls.push_back(Control::Header);
    let wire = flush(&mut callee, 2 * MS);
    assert!(matches!(caller.on_input(&mut wire.as_slice(), 2 * MS), Err(Error::SecondHeader)));
}

#[test]
fn audio_after_a_header_without_opus_fails_the_call() {
    let (mut caller, mut callee, _) = dial(true, false);
    callee.header.opus = false;
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    assert!(callee.send(packet(AUDIO, 0, 20, 2)).unwrap());
    let wire = flush(&mut callee, 2 * MS);
    assert!(matches!(caller.on_input(&mut wire.as_slice(), 2 * MS), Err(Error::UndeclaredAudio)));
}

/// A pseudonym<->pseudonym call with this PC as the callee, up to (not
/// including) its Accept. The caller holds a reveal gate only when `reveals`
/// (a caller without one never reveals itself).
fn pseudonym_call(reveals: bool) -> (CallSession<Fake>, CallSession<Fake>) {
    pseudonym_call_with(reveals, false)
}
/// `pseudonym_call` with LABELS open on both links when `labels` (WP4/WP6).
fn pseudonym_call_with(reveals: bool, labels: bool) -> (CallSession<Fake>, CallSession<Fake>) {
    use crate::{
        bmh3::reveal::{ROLE_CALLEE, ROLE_CALLER, RevealSession},
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
        session::tests::hex,
    };
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap();
    let spki = |name: &str| -> [u8; 91] { hex(v["link"][name]["ephemeralKey"].as_str().unwrap()).try_into().unwrap() };
    let (caller_pseudo, callee_pseudo) = (spki("initiatorHello"), spki("responderHello"));
    let (caller_real, callee_real, _) = identities();
    let callee_real_spki = crate::bmh2::SigningIdentity::public_key(&callee_real).unwrap();
    let callee_node: [u8; 16] = crate::crypto::sha256(&callee_real_spki).unwrap()[..16].try_into().unwrap();
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &callee_node, epoch, 0).unwrap();
    let exporter = [7u8; 32];
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link_with(
            Sealer::new(Layer::Hop, s.send_key(), Policy::default()).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), Policy::default()).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
            labels,
        )
        .unwrap()
    };
    let mut caller = CallSession::<Fake>::outgoing(link(&al, &ae), ID, false, 0, header(), make, 0).unwrap();
    if reveals {
        caller.require_reveal(RevealGate {
            session: RevealSession::new(ROLE_CALLER, &exporter, caller_pseudo, callee_pseudo, true, true, vec![caller_pseudo])
                .unwrap(),
            selected: entry.rdi,
            rotating: true,
            real: Box::new(caller_real),
            wall_epoch: epoch,
        });
    }
    let mut callee_link = link(&bl, &be);
    let opening = flush(&mut caller, 0);
    let mut input = opening.as_slice();
    let mut invite = None;
    while let Some(incoming) = callee_link.read(&mut input).unwrap() {
        if let Incoming::Signal(m) = incoming {
            invite.get_or_insert(m);
        }
    }
    let (mut callee, _) = CallSession::<Fake>::incoming(callee_link, &invite.unwrap(), header(), make, 0).unwrap();
    callee.reveal_on_accept(CalleeReveal {
        session: RevealSession::new(ROLE_CALLEE, &exporter, callee_pseudo, caller_pseudo, true, true, vec![callee_pseudo])
            .unwrap(),
        real: Box::new(callee_real),
        entries: vec![entry],
        relay_hops: 0,
    });
    (caller, callee)
}

/// One round both ways at `now` (keeps both liveness timers fed).
fn round(caller: &mut CallSession<Fake>, callee: &mut CallSession<Fake>, now: u64) -> (Vec<Notice>, Vec<Notice>) {
    let wire = flush(caller, now);
    let at_callee = callee.on_input(&mut wire.as_slice(), now).unwrap();
    let wire = flush(callee, now);
    let at_caller = caller.on_input(&mut wire.as_slice(), now).unwrap();
    (at_caller, at_callee)
}

#[test]
fn a_caller_that_withholds_its_reveal_is_hung_up_and_never_gets_the_callees_media() {
    const S: u64 = 1_000_000_000;
    let (mut caller, mut callee) = pseudonym_call(false);
    let notices = callee.accept(false).unwrap();
    assert!(!notices.iter().any(|n| matches!(n, Notice::StartMedia { .. })), "capture is held: {notices:?}");
    assert!(callee.awaiting_caller_reveal());
    // Capture offered while held is refused.
    assert!(!callee.send(packet(AUDIO | CONFIG, 0, 3, 1)).unwrap());
    // The REVEAL and Accept leave at 1 ms: the caller (no gate) accepts and
    // starts its own media, which the callee drops.
    let (at_caller, _) = round(&mut caller, &mut callee, MS);
    assert_eq!(caller.phase(), call::Phase::NegotiatingMedia, "{at_caller:?}");
    caller.media_ready().unwrap();
    assert!(caller.send(packet(AUDIO | CONFIG, 0, 3, 1)).unwrap());
    let mut caller_saw = at_caller;
    for t in [2 * S, 4 * S, 6 * S, 8 * S, 10 * S] {
        let (at_caller, at_callee) = round(&mut caller, &mut callee, t);
        assert!(!at_callee.iter().any(|n| matches!(n, Notice::PeerHeader(_))), "an unconfirmed caller's header is dropped");
        caller_saw.extend(at_caller);
        assert!(!callee.unconfirmed(), "still inside the 10 s at {t}");
    }
    // 10 s after the REVEAL left: Hang-up/Error, and nothing of the callee's media was ever written.
    let (at_caller, _) = round(&mut caller, &mut callee, 10 * S + MS);
    caller_saw.extend(at_caller);
    assert!(callee.unconfirmed());
    assert!(caller_saw.contains(&Notice::Ended(call::Reason::Error)), "{caller_saw:?}");
    assert!(!caller_saw.iter().any(|n| matches!(n, Notice::PeerHeader(_))), "no callee header");
    let stats = callee.stats();
    assert_eq!((stats.audio_sent, stats.video_sent), (0, 0));
}

#[test]
fn a_late_but_valid_caller_reveal_releases_the_callees_media() {
    const S: u64 = 1_000_000_000;
    let (mut caller, mut callee) = pseudonym_call(true);
    callee.accept(false).unwrap();
    // The callee's REVEAL and Accept leave at 1 ms but the radio delivers
    // them (and the callee's heartbeats) to the caller only at 8.9 s; the
    // caller's heartbeats keep reaching the callee meanwhile.
    let mut to_caller = flush(&mut callee, MS);
    for t in [1_900 * MS, 3_900 * MS, 5_900 * MS, 7_900 * MS] {
        let wire = flush(&mut caller, t);
        assert!(callee.on_input(&mut wire.as_slice(), t).unwrap().is_empty());
        to_caller.extend(flush(&mut callee, t));
        assert!(!callee.unconfirmed());
    }
    let notices = caller.on_input(&mut to_caller.as_slice(), 8_900 * MS).unwrap();
    assert!(notices.iter().any(|n| matches!(n, Notice::PeerRevealed { .. })), "{notices:?}");
    // The caller's REVEAL reaches the callee at 9 s: inside its 10 s.
    let wire = flush(&mut caller, 9 * S);
    let notices = callee.on_input(&mut wire.as_slice(), 9 * S).unwrap();
    assert!(notices.iter().any(|n| matches!(n, Notice::PeerRevealed { .. })), "{notices:?}");
    assert!(notices.contains(&Notice::StartMedia { camera: true }), "{notices:?}");
    assert!(!callee.awaiting_caller_reveal());
    // The callee's held header now goes out, and the deadline no longer applies.
    let wire = flush(&mut callee, 11 * S);
    let at_caller = caller.on_input(&mut wire.as_slice(), 11 * S).unwrap();
    assert!(at_caller.contains(&Notice::PeerHeader(header())), "{at_caller:?}");
    assert!(!callee.unconfirmed());
}

#[test]
fn the_caller_reveal_deadline_grows_per_relay_hop_up_to_40_s() {
    const S: u64 = 1_000_000_000;
    assert_eq!(caller_reveal_budget_ns(0), 10 * S);
    assert_eq!(caller_reveal_budget_ns(1), 20 * S);
    assert_eq!(caller_reveal_budget_ns(3), 40 * S);
    assert_eq!(caller_reveal_budget_ns(9), 40 * S);
}

/// WP4/WP6 tests: a policy under which everyone may get the name, with these
/// lists (None: unknown).
fn open_policy(blocked: Vec<[u8; 32]>, contacts: Option<Vec<[u8; 32]>>) -> std::sync::Arc<crate::call_policy::CallPolicy> {
    std::sync::Arc::new(crate::call_policy::CallPolicy::new(crate::call_policy::Snapshot {
        generation: 0,
        audience: Some(crate::label_policy::NamesAudience::Everyone),
        blocked: Some(blocked),
        contacts,
    }))
}

fn fingerprint_of(who: &dyn crate::bmh2::SigningIdentity) -> [u8; 32] {
    crate::crypto::sha256(&who.public_key().unwrap()).unwrap()
}

/// A direct call link pair with BDX1 and LABELS open.
fn labelled_links() -> (CallLink, CallLink) {
    use crate::records::{Layer, Policy, Sealer};
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link_with(
            Sealer::new(Layer::Hop, s.send_key(), Policy::default()).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), Policy::default()).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
            true,
        )
        .unwrap()
    };
    (link(&al, &ae), link(&bl, &be))
}

/// The Invite, Ringing, Accept, Cancel and Hang-up signals, the LABELS
/// records and the BDX1 presence records in `wire`, in order.
fn kinds(link: &mut CallLink, wire: &[u8]) -> Vec<&'static str> {
    let mut input = wire;
    let mut out = Vec::new();
    while let Some(incoming) = link.read(&mut input).unwrap() {
        match incoming {
            Incoming::Labels(_) => out.push("labels"),
            Incoming::Discovery(record) if matches!(record.get(4), Some(2 | 3)) => out.push("presence"),
            Incoming::Signal(m) if m.signal == Signal::Invite => out.push("invite"),
            Incoming::Signal(m) if m.signal == Signal::Ringing => out.push("ringing"),
            Incoming::Signal(m) if m.signal == Signal::Accept => out.push("accept"),
            Incoming::Signal(m) if m.signal == Signal::Cancel => out.push("cancel"),
            Incoming::Signal(m) if m.signal == Signal::HangUp => out.push("hangup"),
            _ => {}
        }
    }
    out
}

/// WP6: a pseudonymous caller whose verified REVEAL is someone this PC
/// blocked: this PC's held media never starts, the caller's header and media
/// are dropped, and the ordinary Hang-up goes out exactly its delay after the
/// REVEAL arrived, never earlier (both ends of the 3-7 s range).
#[test]
fn a_caller_revealed_as_blocked_gets_no_media_and_the_ordinary_end_exactly_at_its_delay() {
    let (caller_real, _, _) = identities();
    let caller_fp = fingerprint_of(&caller_real);
    for delay_ms in [3_000u64, 7_000] {
        let (mut caller, mut callee) = pseudonym_call(true);
        // Blocked wins even over a verified contact.
        callee.use_policy(open_policy(vec![caller_fp], Some(vec![caller_fp])));
        callee.set_blocked_delay_ms(delay_ms);
        assert!(!callee.accept(false).unwrap().iter().any(|n| matches!(n, Notice::StartMedia { .. })));
        round(&mut caller, &mut callee, MS);
        // The caller's REVEAL (and its stream header) arrive at 2 ms.
        let wire = flush(&mut caller, 2 * MS);
        let at_callee = callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
        assert!(at_callee.contains(&Notice::PeerBlocked), "{at_callee:?}");
        assert!(
            !at_callee.iter().any(|n| matches!(n, Notice::PeerRevealed { .. } | Notice::StartMedia { .. } | Notice::PeerHeader(_))),
            "{at_callee:?}"
        );
        assert!(!callee.send(packet(AUDIO | CONFIG, 0, 3, 1)).unwrap(), "no capture goes out");
        let due = 2 * MS + delay_ms * MS;
        let early = flush(&mut callee, due - 1);
        assert!(!callee.blocked_ended(), "{delay_ms} ms: not a nanosecond early");
        assert!(!caller.on_input(&mut early.as_slice(), due - 1).unwrap().iter().any(|n| matches!(n, Notice::Ended(_))));
        let end = flush(&mut callee, due);
        assert!(callee.blocked_ended(), "{delay_ms} ms: on time");
        let at_caller = caller.on_input(&mut end.as_slice(), due).unwrap();
        assert!(at_caller.contains(&Notice::Ended(call::Reason::Normal)), "an ordinary Hang-up: {at_caller:?}");
        assert!(!callee.unconfirmed(), "not the 10 s reveal deadline");
        let stats = callee.stats();
        assert_eq!((stats.audio_sent, stats.video_sent), (0, 0));
    }
}

/// WP6: this PC calls a stranger whose REVEAL shows someone it blocked: no
/// REVEAL of this PC's goes out, the Accept is not shown, no media starts,
/// and the ordinary end goes out exactly its delay after that REVEAL arrived.
#[test]
fn a_callee_revealed_as_blocked_gets_no_reveal_no_media_and_the_ordinary_end_exactly_at_its_delay() {
    let (_, callee_real, _) = identities();
    let callee_fp = fingerprint_of(&callee_real);
    for delay_ms in [3_000u64, 7_000] {
        let (mut caller, mut callee) = pseudonym_call(true);
        caller.use_policy(open_policy(vec![callee_fp], None));
        caller.set_blocked_delay_ms(delay_ms);
        callee.accept(false).unwrap();
        // The callee's REVEAL and Accept arrive at 1 ms.
        let wire = flush(&mut callee, MS);
        let at_caller = caller.on_input(&mut wire.as_slice(), MS).unwrap();
        assert!(at_caller.contains(&Notice::PeerBlocked), "{at_caller:?}");
        assert!(!at_caller.iter().any(|n| matches!(n, Notice::PeerRevealed { .. } | Notice::StartMedia { .. })), "{at_caller:?}");
        assert!(!caller.take_remote().iter().any(|m| m.signal == Signal::Accept), "the Accept is not shown");
        assert!(!caller.send(packet(AUDIO | CONFIG, 0, 3, 1)).unwrap(), "no capture goes out");
        let due = MS + delay_ms * MS;
        let early = flush(&mut caller, due - 1);
        assert!(!caller.blocked_ended(), "{delay_ms} ms: not a nanosecond early");
        let at_callee = callee.on_input(&mut early.as_slice(), due - 1).unwrap();
        assert!(
            !at_callee.iter().any(|n| matches!(n, Notice::PeerRevealed { .. } | Notice::PeerHeader(_) | Notice::Ended(_))),
            "no REVEAL or header of this PC's: {at_callee:?}"
        );
        let end = flush(&mut caller, due);
        assert!(caller.blocked_ended(), "{delay_ms} ms: on time");
        assert!(callee.on_input(&mut end.as_slice(), due).unwrap().contains(&Notice::Ended(call::Reason::Normal)));
        assert!(!callee.unconfirmed(), "ended before the callee's own 10 s deadline");
    }
}

/// WP6: the owner's hard local deadline: 7 s after the blocked REVEAL arrived
/// plus one bounded write, whether or not the end could be written (a
/// stalled writer).
#[test]
fn a_blocked_call_is_overdue_for_local_teardown_7_3_s_after_the_reveal_even_if_nothing_was_written() {
    let (_, callee_real, _) = identities();
    let (mut caller, mut callee) = pseudonym_call(true);
    caller.use_policy(open_policy(vec![fingerprint_of(&callee_real)], None));
    callee.accept(false).unwrap();
    let wire = flush(&mut callee, MS);
    assert!(caller.on_input(&mut wire.as_slice(), MS).unwrap().contains(&Notice::PeerBlocked));
    // The writer stalls: nothing of the caller's is written again.
    let latest = MS + (BLOCKED_END_LATEST_MS + BLOCKED_WRITE_BUDGET_MS) * MS;
    assert!(!caller.blocked_overdue(latest - 1));
    assert!(caller.blocked_overdue(latest));
    assert!(!caller.blocked_ended(), "nothing was written");
}

/// WP6: an unknown block list (unreadable: CF-CALL-24 fails open, owner
/// question pending) or one that holds only someone else: the revealed
/// caller's call goes on.
#[test]
fn a_revealed_caller_who_is_not_on_the_block_list_is_not_blocked() {
    let unknown = std::sync::Arc::new(crate::call_policy::CallPolicy::default());
    for policy in [unknown, open_policy(vec![[9; 32]], None)] {
        let (mut caller, mut callee) = pseudonym_call(true);
        callee.use_policy(policy);
        callee.accept(false).unwrap();
        round(&mut caller, &mut callee, MS);
        let (_, at_callee) = round(&mut caller, &mut callee, 2 * MS);
        assert!(at_callee.iter().any(|n| matches!(n, Notice::PeerRevealed { .. })), "{at_callee:?}");
        assert!(at_callee.iter().any(|n| matches!(n, Notice::StartMedia { .. })), "{at_callee:?}");
        assert!(!callee.peer_blocked());
    }
}

/// WP6, documented limit (PROTOCOL Phase 2, callee-first REVEAL): a PC that
/// answers a stranger sends its own REVEAL with Accept, so a caller later
/// found to be blocked already holds this PC's real identity, as for any
/// answered call. The block withholds what comes after (the other tests).
#[test]
fn answering_a_stranger_shows_them_this_pcs_identity_before_their_own_reveal_is_checked() {
    let (caller_real, callee_real, _) = identities();
    let callee_fp = fingerprint_of(&callee_real);
    let (mut caller, mut callee) = pseudonym_call(true);
    callee.use_policy(open_policy(vec![fingerprint_of(&caller_real)], None));
    callee.accept(false).unwrap();
    let (at_caller, _) = round(&mut caller, &mut callee, MS);
    assert!(
        at_caller.iter().any(|n| matches!(n, Notice::PeerRevealed { fingerprint, .. } if *fingerprint == callee_fp)),
        "the callee-first REVEAL: {at_caller:?}"
    );
    let (_, at_callee) = round(&mut caller, &mut callee, 2 * MS);
    assert!(at_callee.contains(&Notice::PeerBlocked), "found only now: {at_callee:?}");
}

/// WP6 (B3): a peer that showed its real key in the handshake (no REVEAL will
/// come) and is blocked: screened before any name or media; its Accept starts
/// nothing of this PC's and the call gets the ordinary end.
#[test]
fn a_peer_whose_handshake_key_is_blocked_gets_no_media_and_the_ordinary_end() {
    let blocked = [0x5au8; 32];
    let (mut caller, mut callee, _) = dial(true, false);
    caller.use_policy(open_policy(vec![blocked], None));
    caller.set_blocked_delay_ms(3_000);
    caller.screen_peer(blocked, 0);
    assert!(caller.take_notices().contains(&Notice::PeerBlocked));
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    let at_caller = caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(!at_caller.iter().any(|n| matches!(n, Notice::StartMedia { .. } | Notice::PeerHeader(_))), "{at_caller:?}");
    assert!(!caller.send(packet(AUDIO | CONFIG, 0, 3, 1)).unwrap());
    let end = flush(&mut caller, 3_000 * MS);
    assert!(caller.blocked_ended());
    assert!(callee.on_input(&mut end.as_slice(), 3_000 * MS).unwrap().contains(&Notice::Ended(call::Reason::Normal)));
}

/// WP6/WP4 (B5): someone blocked during a call (a newer policy): the next
/// write latches the blocked end, and a presence record a late task queues is
/// never written.
#[test]
fn a_peer_blocked_during_the_call_gets_no_further_presence_and_the_ordinary_end() {
    let peer = [0x6bu8; 32];
    let (caller_link, mut callee_link) = labelled_links();
    let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
    // WP6NF finding 2: presence needs the peer in the loaded contact list.
    let policy = open_policy(Vec::new(), Some(vec![peer]));
    caller.use_policy(policy.clone());
    caller.set_blocked_delay_ms(3_000);
    caller.screen_peer(peer, 0);
    let lease = [b"BDX1".as_slice(), &[3], &[0u8; 8]].concat();
    caller.send_discovery(lease.clone());
    assert!(kinds(&mut callee_link, &flush(&mut caller, 0)).contains(&"presence"), "allowed before the block");
    policy.set_blocked(Some(vec![peer]));
    caller.send_discovery(lease);
    let wire = flush(&mut caller, MS);
    assert!(caller.peer_blocked());
    assert!(!kinds(&mut callee_link, &wire).contains(&"presence"), "a late presence record never goes out");
    let end = flush(&mut caller, MS + 3_000 * MS);
    assert!(caller.blocked_ended());
    assert!(kinds(&mut callee_link, &end).contains(&"cancel"), "still ringing: the ordinary Cancel");
}

/// WP6: after a caller's REVEAL showed a blocked person, its LABELS and its
/// presence OFFER and LEASE records are ignored.
#[test]
fn a_blocked_callers_labels_and_presence_records_are_ignored() {
    let (caller_real, _, _) = identities();
    let (mut caller, mut callee) = pseudonym_call_with(true, true);
    callee.use_policy(open_policy(vec![fingerprint_of(&caller_real)], None));
    caller.use_policy(open_policy(Vec::new(), None));
    let spki = caller_real.public_key().unwrap();
    let caller_node: [u8; 16] = crate::crypto::sha256(&spki).unwrap()[..16].try_into().unwrap();
    callee.accept_labels(caller_node);
    caller.accept_labels([0; 16]);
    callee.accept(false).unwrap();
    round(&mut caller, &mut callee, MS);
    let (_, at_callee) = round(&mut caller, &mut callee, 2 * MS);
    assert!(at_callee.contains(&Notice::PeerBlocked));
    // The blocked caller sends a name and presence records anyway.
    let label = crate::labels::sign(&caller_real, crate::labels::KIND_NAME, 0, crate::labels::BINDING_CALL, &ID, "Mallory").unwrap();
    caller.send_labels(crate::labels::container(&[label]));
    for kind in [2u8, 3] {
        caller.send_discovery([b"BDX1".as_slice(), &[kind], &[0u8; 8]].concat());
    }
    let wire = flush(&mut caller, 3 * MS);
    let at_callee = callee.on_input(&mut wire.as_slice(), 3 * MS).unwrap();
    assert!(!at_callee.iter().any(|n| matches!(n, Notice::PeerLabel { .. } | Notice::PresenceRecord)), "{at_callee:?}");
    assert!(callee.take_presence_records().is_empty());
}

/// WP6: with Wi-Fi negotiated, this PC as the initiator sends no CAPS
/// before the callee's REVEAL is done, and none once that REVEAL showed a
/// blocked person, whatever the callee sends.
#[test]
fn no_wifi_caps_go_to_a_callee_before_its_reveal_or_once_it_is_blocked() {
    use crate::{
        bmh3::reveal::{ROLE_CALLEE, ROLE_CALLER, RevealSession},
        records::{Layer, Policy, Sealer},
        rotating::OpeningEntry,
        session::tests::hex,
    };
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap();
    let spki = |name: &str| -> [u8; 91] { hex(v["link"][name]["ephemeralKey"].as_str().unwrap()).try_into().unwrap() };
    let (caller_pseudo, callee_pseudo) = (spki("initiatorHello"), spki("responderHello"));
    let (caller_real, callee_real, _) = identities();
    let callee_fp = fingerprint_of(&callee_real);
    let callee_node: [u8; 16] = callee_fp[..16].try_into().unwrap();
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &callee_node, epoch, 0).unwrap();
    let exporter = [7u8; 32];
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let policy = Policy { wifi: true, ..Policy::default() };
    let link = |s: &crate::bmh2::SessionKeys, e: &crate::bmh2::SessionKeys| {
        CallLink::continue_link_with(
            Sealer::new(Layer::Hop, s.send_key(), policy).unwrap(),
            crate::framing::HopReader::new(s.receive_key(), policy).unwrap(),
            e.send_key(),
            e.receive_key(),
            false,
            true,
            false,
        )
        .unwrap()
    };
    let mut caller = CallSession::<Fake>::outgoing(link(&al, &ae), ID, true, 0, header(), make, 0).unwrap();
    caller.enable_wifi(&exporter, true, false).unwrap();
    caller.use_policy(open_policy(vec![callee_fp], None));
    caller.require_reveal(RevealGate {
        session: RevealSession::new(ROLE_CALLER, &exporter, caller_pseudo, callee_pseudo, true, true, vec![caller_pseudo]).unwrap(),
        selected: entry.rdi,
        rotating: true,
        real: Box::new(caller_real),
        wall_epoch: epoch,
    });
    let mut callee_link = link(&bl, &be);
    callee_link.enable_wifi(&exporter, false, false).unwrap();
    let mut invite = None;
    for (_, bytes) in &flush_on(&mut caller, 0) {
        let mut input = bytes.as_slice();
        while let Some(incoming) = callee_link.read(&mut input).unwrap() {
            if let Incoming::Signal(message) = incoming {
                invite.get_or_insert(message);
            }
        }
    }
    let (mut callee, _) = CallSession::<Fake>::incoming(callee_link, &invite.unwrap(), header(), make, 0).unwrap();
    callee.reveal_on_accept(CalleeReveal {
        session: RevealSession::new(ROLE_CALLEE, &exporter, callee_pseudo, caller_pseudo, true, true, vec![callee_pseudo]).unwrap(),
        real: Box::new(callee_real),
        entries: vec![entry],
        relay_hops: 0,
    });
    let no_caps = |writes: &[(i32, Vec<u8>)]| writes.iter().all(|(carrier, bytes)| *carrier != 0 || bytes[4] != 16);
    // Ringing, then a CAPS before any REVEAL: no CAPS of this PC's answers it.
    deliver(&mut caller, &flush_on(&mut callee, MS / 2), MS / 2);
    let early = callee.test_seal_bwf1(0, &Bwf1::Caps(wire::Caps::default())).unwrap();
    feed(&mut caller, 0, &early, MS / 2);
    assert!(no_caps(&flush_on(&mut caller, MS)), "no CAPS before the callee's REVEAL");
    // The callee's REVEAL shows a blocked person; a later CAPS changes nothing.
    callee.accept(true).unwrap();
    assert!(deliver(&mut caller, &flush_on(&mut callee, 2 * MS), 2 * MS).contains(&Notice::PeerBlocked));
    let late = callee.test_seal_bwf1(0, &Bwf1::Caps(wire::Caps::default())).unwrap();
    feed(&mut caller, 0, &late, 3 * MS);
    assert!(no_caps(&flush_on(&mut caller, 4 * MS)), "no CAPS to a blocked person");
    assert!(!caller.wifi_attempts_allowed());
}

/// The signed name container `who` sends in this call, and the node ID a
/// receiver accepts it from (WP4 tests).
fn name_from(who: &dyn crate::bmh2::SigningIdentity, text: &str) -> (Vec<u8>, [u8; 16]) {
    let spki = who.public_key().unwrap();
    let node: [u8; 16] = crate::crypto::sha256(&spki).unwrap()[..16].try_into().unwrap();
    let label = crate::labels::sign(who, crate::labels::KIND_NAME, 0, crate::labels::BINDING_CALL, &ID, text).unwrap();
    (crate::labels::container(&[label]), node)
}

/// WP4: "Now" puts the caller's name right before its Invite; Never, a held
/// moment or a link without LABELS sends nothing then.
#[test]
fn the_callers_name_goes_before_the_invite_only_when_planned_now() {
    for (moment, negotiated, expected) in [
        (SendMoment::Now, true, &["labels", "invite"][..]),
        (SendMoment::Never, true, &["invite"][..]),
        (SendMoment::AtAccept, true, &["invite"][..]),
        (SendMoment::Now, false, &["invite"][..]),
    ] {
        let (caller_link, mut callee_link) = labelled_links();
        let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
        // WP6NG finding 9: "Now" is a contact's moment: the callee is one.
        let callee_fp = [0x9cu8; 32];
        caller.use_policy(open_policy(Vec::new(), Some(vec![callee_fp])));
        caller.screen_peer(callee_fp, 0);
        if negotiated {
            caller.accept_labels([0; 16]);
        }
        let (a, _, _) = identities();
        caller.plan_own_name(moment, name_from(&a, "Ravi").0);
        assert_eq!(kinds(&mut callee_link, &flush(&mut caller, 0)), expected, "{moment:?} negotiated={negotiated}");
    }
}

/// WP4: a callee met with its real key sends its name at Ringing (a verified
/// contact), right before its Accept ("People I call or who call me"), or
/// not at all; never before the Accept to anyone else.
#[test]
fn the_callees_name_goes_at_ringing_or_right_before_its_accept() {
    for (moment, at_ringing, at_answer) in [
        (SendMoment::AtRinging, &["ringing", "labels"][..], &["accept"][..]),
        (SendMoment::AtAccept, &["ringing"][..], &["labels", "accept"][..]),
        (SendMoment::Never, &["ringing"][..], &["accept"][..]),
    ] {
        let (mut caller_link, mut callee_link) = labelled_links();
        let wire = caller_link.signal(&Message::simple(ID, Signal::Invite).unwrap()).unwrap();
        let Some(Incoming::Signal(invite)) = callee_link.read(&mut wire.as_slice()).unwrap() else {
            panic!("no Invite")
        };
        let (mut callee, _) = CallSession::<Fake>::incoming(callee_link, &invite, header(), make, 0).unwrap();
        // WP6NF finding 5: the Ringing name needs an identified caller in the
        // loaded contact list (WP6N's test planned it for an unknown one).
        let caller_fp = [0x1bu8; 32];
        callee.use_policy(open_policy(Vec::new(), Some(vec![caller_fp])));
        callee.screen_peer(caller_fp, 0);
        callee.accept_labels([0; 16]);
        let (a, _, _) = identities();
        callee.plan_own_name(moment, name_from(&a, "Asha").0);
        assert_eq!(kinds(&mut caller_link, &flush(&mut callee, 0)), at_ringing, "{moment:?}");
        callee.accept(true).unwrap();
        assert_eq!(kinds(&mut caller_link, &flush(&mut callee, MS)), at_answer, "{moment:?}");
    }
}

/// WP4: in a pseudonym<->pseudonym call the callee's name waits for the
/// caller's verified REVEAL: the Accept-time name follows that REVEAL; with
/// "Only my contacts" only a caller whose fingerprint is in the loaded
/// contact list gets it; Never sends nothing.
#[test]
fn the_callees_name_waits_for_the_callers_reveal_and_follows_the_audience() {
    use crate::label_policy::NamesAudience::{Everyone, VerifiedOnly};
    let (caller_real, callee_real, _) = identities();
    let caller_fp = fingerprint_of(&caller_real);
    for (moment, audience, contacts, named) in [
        (SendMoment::AtAccept, Everyone, Some(Vec::new()), true),
        (SendMoment::AfterRevealIfVerified, VerifiedOnly, Some(vec![caller_fp]), true),
        (SendMoment::AfterRevealIfVerified, VerifiedOnly, Some(Vec::new()), false),
        (SendMoment::AfterRevealIfVerified, VerifiedOnly, None, false),
        (SendMoment::Never, Everyone, Some(vec![caller_fp]), false),
    ] {
        let (mut caller, mut callee) = pseudonym_call_with(true, true);
        let policy = open_policy(Vec::new(), contacts.clone());
        policy.set_audience(audience);
        callee.use_policy(policy);
        let (name, callee_node) = name_from(&callee_real, "Asha");
        caller.accept_labels(callee_node);
        callee.accept_labels(name_from(&caller_real, "Ravi").1);
        callee.plan_own_name(moment, name);
        callee.accept(false).unwrap();
        let (first, _) = round(&mut caller, &mut callee, MS);
        assert!(!first.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "{moment:?}: nothing before the caller's REVEAL: {first:?}");
        let (second, at_callee) = round(&mut caller, &mut callee, 2 * MS);
        assert!(at_callee.iter().any(|n| matches!(n, Notice::PeerRevealed { .. })), "{at_callee:?}");
        let got = second.contains(&Notice::PeerLabel { kind: crate::labels::KIND_NAME, text: "Asha".into() });
        assert_eq!(got, named, "{moment:?} {audience:?} {contacts:?}: {second:?}");
    }
}

/// WP4 + WP6: a name held for the caller's REVEAL never reaches a caller
/// revealed as blocked, also when that person is a verified contact.
#[test]
fn a_caller_revealed_as_blocked_never_gets_the_held_name() {
    let (caller_real, callee_real, _) = identities();
    let caller_fp = fingerprint_of(&caller_real);
    for moment in [SendMoment::AtAccept, SendMoment::AfterRevealIfVerified] {
        let (mut caller, mut callee) = pseudonym_call_with(true, true);
        callee.use_policy(open_policy(vec![caller_fp], Some(vec![caller_fp])));
        let (name, callee_node) = name_from(&callee_real, "Asha");
        caller.accept_labels(callee_node);
        callee.accept_labels(name_from(&caller_real, "Ravi").1);
        callee.plan_own_name(moment, name);
        callee.accept(false).unwrap();
        let mut caller_saw = Vec::new();
        for t in [MS, 2 * MS, 1_000 * MS] {
            caller_saw.extend(round(&mut caller, &mut callee, t).0);
        }
        assert!(callee.peer_blocked(), "{moment:?}");
        assert!(!caller_saw.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "{moment:?}: {caller_saw:?}");
    }
}

/// WP4: this PC as the caller with "Only my contacts" names itself to a
/// pseudonymous callee only after the callee's REVEAL shows a fingerprint in
/// the loaded contact list, and never to one revealed as blocked.
#[test]
fn the_callers_name_waits_for_a_revealed_verified_contact() {
    let (caller_real, callee_real, _) = identities();
    let callee_fp = fingerprint_of(&callee_real);
    for (blocked, contacts, named) in [
        (Vec::new(), Some(vec![callee_fp]), true),
        (Vec::new(), Some(Vec::new()), false),
        (Vec::new(), None, false),
        (vec![callee_fp], Some(vec![callee_fp]), false),
    ] {
        let (mut caller, mut callee) = pseudonym_call_with(true, true);
        let policy = open_policy(blocked.clone(), contacts.clone());
        policy.set_audience(crate::label_policy::NamesAudience::VerifiedOnly);
        caller.use_policy(policy);
        let (name, caller_node) = name_from(&caller_real, "Ravi");
        callee.accept_labels(caller_node);
        caller.accept_labels(name_from(&callee_real, "Asha").1);
        caller.plan_own_name(SendMoment::AfterRevealIfVerified, name);
        callee.accept(false).unwrap();
        let (_, before) = round(&mut caller, &mut callee, MS);
        assert!(!before.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "nothing before the callee's REVEAL");
        let (_, after) = round(&mut caller, &mut callee, 2 * MS);
        let got = after.contains(&Notice::PeerLabel { kind: crate::labels::KIND_NAME, text: "Ravi".into() });
        assert_eq!(got, named, "blocked={} contacts={contacts:?}: {after:?}", !blocked.is_empty());
    }
}

/// WP4 + WP6 (A2): a name already queued when a blocked REVEAL arrives
/// (before the next write) is purged with everything but the signals.
#[test]
fn a_name_queued_before_a_blocked_reveal_arrives_is_never_written() {
    let (caller_real, callee_real, _) = identities();
    let (mut caller, mut callee) = pseudonym_call_with(true, true);
    caller.use_policy(open_policy(vec![fingerprint_of(&callee_real)], None));
    let (name, caller_node) = name_from(&caller_real, "Ravi");
    callee.accept_labels(caller_node);
    caller.accept_labels([0; 16]);
    // Queued now, not yet written.
    caller.plan_own_name(SendMoment::Now, name);
    callee.accept(false).unwrap();
    // The blocked callee's REVEAL arrives before the caller's next write.
    let wire = flush(&mut callee, MS);
    assert!(caller.on_input(&mut wire.as_slice(), MS).unwrap().contains(&Notice::PeerBlocked));
    let wire = flush(&mut caller, 2 * MS);
    let at_callee = callee.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert!(!at_callee.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "{at_callee:?}");
}

/// WP4/WP6 (B5): a name waiting for its moment follows the newest policy:
/// switched to Nobody, a removed contact or a new block withholds it; a new
/// block also keeps the held media start from ever starting.
#[test]
fn a_held_name_follows_the_newest_audience_contacts_and_blocks() {
    use crate::label_policy::NamesAudience::{Everyone, Nobody, VerifiedOnly};
    let (caller_real, callee_real, _) = identities();
    let caller_fp = fingerprint_of(&caller_real);
    for (moment, audience, change) in [
        (SendMoment::AtAccept, Everyone, "nobody"),
        (SendMoment::AfterRevealIfVerified, VerifiedOnly, "contact removed"),
        (SendMoment::AtAccept, Everyone, "blocked"),
    ] {
        let (mut caller, mut callee) = pseudonym_call_with(true, true);
        let policy = open_policy(Vec::new(), Some(vec![caller_fp]));
        policy.set_audience(audience);
        callee.use_policy(policy.clone());
        let (name, callee_node) = name_from(&callee_real, "Asha");
        caller.accept_labels(callee_node);
        callee.accept_labels(name_from(&caller_real, "Ravi").1);
        callee.plan_own_name(moment, name);
        callee.accept(false).unwrap();
        round(&mut caller, &mut callee, MS);
        match change {
            "nobody" => policy.set_audience(Nobody),
            "contact removed" => policy.set_contacts(Some(Vec::new())),
            _ => policy.set_blocked(Some(vec![caller_fp])),
        }
        let (mut caller_saw, mut callee_saw) = (Vec::new(), Vec::new());
        for t in [2 * MS, 3 * MS] {
            let (at_caller, at_callee) = round(&mut caller, &mut callee, t);
            caller_saw.extend(at_caller);
            callee_saw.extend(at_callee);
        }
        assert!(!caller_saw.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "{change}: {caller_saw:?}");
        if change == "blocked" {
            assert!(callee_saw.contains(&Notice::PeerBlocked), "{callee_saw:?}");
            assert!(!callee_saw.iter().any(|n| matches!(n, Notice::StartMedia { .. })), "the held start stays held");
        }
    }
}

/// WP4 (B5): a name already queued is checked again when written: switched
/// to Nobody before the write, it never goes out.
#[test]
fn a_queued_name_is_not_written_after_the_audience_becomes_nobody() {
    let (caller_link, mut callee_link) = labelled_links();
    let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
    let policy = open_policy(Vec::new(), None);
    caller.use_policy(policy.clone());
    caller.accept_labels([0; 16]);
    let (a, _, _) = identities();
    caller.plan_own_name(SendMoment::Now, name_from(&a, "Ravi").0);
    policy.set_audience(crate::label_policy::NamesAudience::Nobody);
    assert_eq!(kinds(&mut callee_link, &flush(&mut caller, 0)), ["invite"]);
}

/// WP4: a relayed callee is proven only by its OPENING after the Invite; the
/// caller's name follows that OPENING, never before it, and only if the
/// newest policy still allows it.
#[test]
fn a_relayed_callees_name_waits_for_its_opening_and_the_newest_audience() {
    use crate::rotating::OpeningEntry;
    let node = [0x42u8; 16];
    let epoch = 1_990_656;
    let entry = OpeningEntry::own(&[5; 32], &node, epoch, 0).unwrap();
    let opening = [b"BDX1".as_slice(), &[1], &rotating::encode_body(std::slice::from_ref(&entry)).unwrap()].concat();
    for still_allowed in [true, false] {
        let (caller_link, mut callee_link) = labelled_links();
        let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 1, header(), make, 0).unwrap();
        // WP6NG finding 9: the after-OPENING name is a contact's: the callee is one.
        let callee_fp = [0x43u8; 32];
        let policy = open_policy(Vec::new(), Some(vec![callee_fp]));
        caller.use_policy(policy.clone());
        caller.screen_peer(callee_fp, 0);
        caller.accept_labels([0; 16]);
        caller.require_opening(OpeningGate { selected: entry.rdi, node_id: node, wall_epoch: epoch });
        let (a, _, _) = identities();
        caller.plan_own_name(SendMoment::AfterOpening, name_from(&a, "Ravi").0);
        assert!(!kinds(&mut callee_link, &flush(&mut caller, 0)).contains(&"labels"), "nothing before the OPENING");
        if !still_allowed {
            policy.set_audience(crate::label_policy::NamesAudience::Nobody);
        }
        let wire = callee_link.discovery(&opening).unwrap();
        caller.on_input(&mut wire.as_slice(), MS).unwrap();
        assert_eq!(kinds(&mut callee_link, &flush(&mut caller, MS)).contains(&"labels"), still_allowed);
    }
}

/// WP6NF finding 2: a presence OFFER or LEASE goes only to an identified peer
/// in the loaded contact list: an unknown list, a removed contact or an
/// unidentified peer gets none (also from a task that finished late).
#[test]
fn presence_records_go_only_to_an_identified_peer_in_the_loaded_contact_list() {
    let peer = [0x6cu8; 32];
    let lease = [b"BDX1".as_slice(), &[3], &[0u8; 8]].concat();
    for (contacts, identified, sent) in [
        (Some(vec![peer]), true, true),
        (None, true, false),
        (Some(Vec::new()), true, false),
        (Some(vec![peer]), false, false),
    ] {
        let (caller_link, mut callee_link) = labelled_links();
        let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
        caller.use_policy(open_policy(Vec::new(), contacts.clone()));
        if identified {
            caller.screen_peer(peer, 0);
        }
        caller.send_discovery(lease.clone());
        let written = kinds(&mut callee_link, &flush(&mut caller, 0)).contains(&"presence");
        assert_eq!(written, sent, "contacts={contacts:?} identified={identified}");
    }
}

/// WP6NF finding 4: a caller blocked while it rang (no write or input in
/// between) gets no Accept and starts nothing: the newest policy decides at
/// Accept.
#[test]
fn a_caller_blocked_while_it_rang_gets_no_accept_and_starts_nothing() {
    let caller_peer = [0x2fu8; 32];
    let (mut caller, mut callee, _) = dial(true, false);
    let policy = open_policy(Vec::new(), None);
    callee.use_policy(policy.clone());
    callee.set_blocked_delay_ms(3_000);
    callee.screen_peer(caller_peer, 0);
    policy.set_blocked(Some(vec![caller_peer]));
    let notices = callee.accept(true).unwrap();
    assert!(!notices.iter().any(|n| matches!(n, Notice::StartMedia { .. })), "{notices:?}");
    assert!(callee.peer_blocked());
    let wire = flush(&mut callee, MS);
    let at_caller = caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(!at_caller.iter().any(|n| matches!(n, Notice::StartMedia { .. })), "no Accept reached the caller: {at_caller:?}");
    assert_ne!(caller.phase(), call::Phase::NegotiatingMedia);
}

/// WP6NF finding 5: the name queued at Ringing for a verified contact is not
/// written once that caller left the loaded contact list (forgotten, or the
/// list became unreadable), even under "People I call or who call me".
#[test]
fn a_ringing_name_is_not_written_after_the_caller_left_the_contact_list() {
    let caller_fp = [0x1au8; 32];
    for (change, written) in [("none", true), ("forgotten", false), ("unreadable", false)] {
        let (mut caller_link, mut callee_link) = labelled_links();
        let wire = caller_link.signal(&Message::simple(ID, Signal::Invite).unwrap()).unwrap();
        let Some(Incoming::Signal(invite)) = callee_link.read(&mut wire.as_slice()).unwrap() else {
            panic!("no Invite")
        };
        let (mut callee, _) = CallSession::<Fake>::incoming(callee_link, &invite, header(), make, 0).unwrap();
        let policy = open_policy(Vec::new(), Some(vec![caller_fp]));
        callee.use_policy(policy.clone());
        callee.screen_peer(caller_fp, 0);
        callee.accept_labels([0; 16]);
        let (a, _, _) = identities();
        callee.plan_own_name(SendMoment::AtRinging, name_from(&a, "Asha").0);
        match change {
            "forgotten" => policy.set_contacts(Some(Vec::new())),
            "unreadable" => policy.set_contacts(None),
            _ => {}
        }
        assert_eq!(kinds(&mut caller_link, &flush(&mut callee, 0)).contains(&"labels"), written, "{change}");
    }
}

/// WP6NF finding 6: with a Wi-Fi carrier attached, a block ends Wi-Fi for the
/// call: no new attempt and no BWF1 record (marker, CAPS or Close) through
/// the blocked end.
#[test]
fn a_block_ends_wifi_for_the_call_and_no_bwf1_record_follows() {
    let (mut caller, mut callee) = established(false);
    attach_pair(&mut caller, &mut callee, 4_000 * MS);
    let peer = [0x8du8; 32];
    caller.use_policy(open_policy(vec![peer], None));
    caller.set_blocked_delay_ms(3_000);
    caller.screen_peer(peer, 4_000 * MS);
    assert!(caller.peer_blocked());
    assert_eq!(caller.wifi_begin_attempt([3; 8], 4_000 * MS), None, "no new attempt");
    let writes = flush_on(&mut caller, 7_000 * MS);
    assert!(caller.blocked_ended(), "the end was queued");
    // WP6NG finding 6: the cutover's markers (no content) may still go so the
    // carrier stays readable; no CAPS and no Close do, and the ordinary end arrives.
    let at_callee = deliver(&mut callee, &writes, 7_000 * MS);
    assert!(
        !at_callee.iter().any(|n| matches!(n, Notice::Wifi(WifiNotice::PeerCaps(_) | WifiNotice::PeerClose { .. }))),
        "{at_callee:?}"
    );
    assert!(at_callee.contains(&Notice::Ended(call::Reason::Normal)), "the ordinary end arrives: {at_callee:?}");
}

/// WP6NF finding 8: a blocked call's end stays ordinary and its outcome stays
/// with the owner: the peer's own end is not shown (the owner says
/// "blocked"), and a liveness close during the blocked delay is Normal.
#[test]
fn a_blocked_calls_end_stays_ordinary_and_its_outcome_stays_with_the_owner() {
    let peer = [0x5bu8; 32];
    // The peer ends first.
    let (mut caller, mut callee, _) = dial(true, false);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.use_policy(open_policy(vec![peer], None));
    caller.screen_peer(peer, MS);
    let _ = caller.take_remote();
    callee.hang_up(call::Reason::Normal).unwrap();
    let wire = flush(&mut callee, 2 * MS);
    let notices = caller.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    assert!(notices.iter().any(|n| matches!(n, Notice::Ended(_))), "{notices:?}");
    assert!(!caller.take_remote().iter().any(|m| m.signal == Signal::HangUp), "the owner reports the blocked outcome");
    // A silent peer: the liveness end during the blocked delay is Normal.
    let (mut caller, mut callee, _) = dial(true, false);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.use_policy(open_policy(vec![peer], None));
    caller.set_blocked_delay_ms(7_000);
    caller.screen_peer(peer, 4_000 * MS);
    // WP6NG finding 2: the blocked delay owns the end: nothing (no Error)
    // before it is due, even though the liveness limit passed at 8 s.
    assert_eq!(caller.next_write(8_500 * MS).unwrap(), None, "held until the blocked end is due");
    assert!(!caller.blocked_ended());
    let record = caller.next_write(11_000 * MS).unwrap().expect("the ordinary end when due");
    assert!(caller.blocked_ended());
    let notices = callee.on_input(&mut record.as_slice(), 11_000 * MS).unwrap();
    assert!(notices.contains(&Notice::Ended(call::Reason::Normal)), "{notices:?}");
}

/// Design decision 2026-11-06 (Android AP1): under "People I call or who call me" a
/// stranger gets this PC's name only once it answered: a throwaway after its
/// verified, unblocked REVEAL; a real key once it accepted; never while it
/// rings.
#[test]
fn a_stranger_gets_the_callers_name_only_once_it_answered_and_passed_the_block_check() {
    use crate::label_policy::NamesAudience::Everyone;
    let (caller_real, callee_real, _) = identities();
    let callee_fp = fingerprint_of(&callee_real);
    for (blocked, named) in [(Vec::new(), true), (vec![callee_fp], false)] {
        let (mut caller, mut callee) = pseudonym_call_with(true, true);
        caller.use_policy(open_policy(blocked.clone(), None));
        let (name, caller_node) = name_from(&caller_real, "Ravi");
        callee.accept_labels(caller_node);
        caller.accept_labels([0; 16]);
        caller.plan_own_name(crate::dial::caller_name_moment(Everyone, true, false, true, false), name);
        let (_, ringing) = round(&mut caller, &mut callee, MS);
        assert!(!ringing.iter().any(|n| matches!(n, Notice::PeerLabel { .. })), "nothing while it rings");
        callee.accept(false).unwrap();
        let mut at_callee = Vec::new();
        for t in [2 * MS, 3 * MS] {
            at_callee.extend(round(&mut caller, &mut callee, t).1);
        }
        let got = at_callee.contains(&Notice::PeerLabel { kind: crate::labels::KIND_NAME, text: "Ravi".into() });
        assert_eq!(got, named, "blocked={}: {at_callee:?}", !blocked.is_empty());
    }
    // A stranger met with its real key: once it accepted.
    let (caller_link, mut callee_link) = labelled_links();
    let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
    caller.use_policy(open_policy(Vec::new(), None));
    caller.screen_peer([0x9eu8; 32], 0);
    // WP6NF: the production stranger route installs a reveal gate even when
    // the callee showed its real key; it must not hold the name past Accept.
    caller.require_reveal(RevealGate {
        session: crate::bmh3::reveal::RevealSession::new(
            crate::bmh3::reveal::ROLE_CALLER,
            &[7; 32],
            [0x21; 91],
            [0x22; 91],
            true,
            false,
            vec![[0x21; 91]],
        )
        .unwrap(),
        selected: [0; 6],
        rotating: false,
        real: Box::new(identities().0),
        wall_epoch: 0,
    });
    caller.accept_labels([0; 16]);
    caller.plan_own_name(crate::dial::caller_name_moment(Everyone, true, false, false, false), name_from(&caller_real, "Ravi").0);
    assert!(!kinds(&mut callee_link, &flush(&mut caller, 0)).contains(&"labels"), "not before the answer");
    let mut wire = callee_link.signal(&Message::simple(ID, Signal::Ringing).unwrap()).unwrap();
    wire.extend(callee_link.signal(&Message::simple(ID, Signal::Accept).unwrap()).unwrap());
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(kinds(&mut callee_link, &flush(&mut caller, MS)).contains(&"labels"), "once it accepted");
}

/// WP6NG finding 1: a block found at Accept counts its delay and its hard
/// deadline from the Accept's call time, not from the last input seconds
/// earlier.
#[test]
fn a_block_found_at_accept_counts_its_delay_from_the_accept() {
    let caller_peer = [0x2eu8; 32];
    let (_caller, mut callee, _) = dial(true, false);
    let policy = open_policy(Vec::new(), None);
    callee.use_policy(policy.clone());
    callee.set_blocked_delay_ms(3_000);
    callee.screen_peer(caller_peer, 0);
    policy.set_blocked(Some(vec![caller_peer]));
    // The last input was at 0; the user answers 3 s later.
    let answered_at = 3_000 * MS;
    callee.accept_at(true, answered_at).unwrap();
    assert!(callee.peer_blocked());
    let _ = flush(&mut callee, answered_at + 2_999 * MS);
    assert!(!callee.blocked_ended(), "not before its delay from the Accept");
    assert!(!callee.blocked_overdue(answered_at + 7_299 * MS), "the hard deadline counts from the Accept");
    let _ = flush(&mut callee, answered_at + 3_000 * MS);
    assert!(callee.blocked_ended());
}

/// WP6NG finding 2: when the call had already ended (the peer hung up first)
/// the blocked end cannot be queued, and its state says so instead of
/// claiming an end went out.
#[test]
fn a_blocked_end_that_cannot_be_queued_is_reported_as_such() {
    let peer = [0x5cu8; 32];
    let (mut caller, mut callee, _) = dial(true, false);
    callee.accept(true).unwrap();
    let wire = flush(&mut callee, MS);
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    caller.use_policy(open_policy(vec![peer], None));
    caller.set_blocked_delay_ms(3_000);
    caller.screen_peer(peer, MS);
    callee.hang_up(call::Reason::Normal).unwrap();
    let wire = flush(&mut callee, 2 * MS);
    caller.on_input(&mut wire.as_slice(), 2 * MS).unwrap();
    let _ = flush(&mut caller, MS + 3_000 * MS);
    assert_eq!(caller.blocked_end_state(), Some(BlockedEndState::Unsendable));
}

/// WP6NG finding 9: the caller's early ("Now") name for a contact keeps its
/// condition until written: once that contact is forgotten it waits like a
/// stranger's, for the Accept, never before the answer.
#[test]
fn a_contacts_early_name_waits_like_a_strangers_once_the_contact_is_forgotten() {
    let callee_fp = [0x9du8; 32];
    let (caller_link, mut callee_link) = labelled_links();
    let mut caller = CallSession::<Fake>::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
    let policy = open_policy(Vec::new(), Some(vec![callee_fp]));
    caller.use_policy(policy.clone());
    caller.screen_peer(callee_fp, 0);
    caller.accept_labels([0; 16]);
    let (a, _, _) = identities();
    caller.plan_own_name(SendMoment::Now, name_from(&a, "Ravi").0);
    policy.set_contacts(Some(Vec::new()));
    assert!(!kinds(&mut callee_link, &flush(&mut caller, 0)).contains(&"labels"), "not before the answer");
    let mut wire = callee_link.signal(&Message::simple(ID, Signal::Ringing).unwrap()).unwrap();
    wire.extend(callee_link.signal(&Message::simple(ID, Signal::Accept).unwrap()).unwrap());
    caller.on_input(&mut wire.as_slice(), MS).unwrap();
    assert!(kinds(&mut callee_link, &flush(&mut caller, MS)).contains(&"labels"), "once it accepted");
}

// ---- Design SR (step 4): RENEW records through the session writer ----

/// A caller session whose link opted into renewal as chosen, and the callee's
/// bare link (a reader), also as chosen. The callee is not given a session so
/// the test reads exactly what the caller wrote.
fn renewal_writer(enable_caller: bool, enable_reader: bool) -> (CallSession<Fake>, CallLink) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let mut caller_link = CallLink::new(al.send_key(), al.receive_key(), ae.send_key(), ae.receive_key(), false).unwrap();
    let mut reader = CallLink::new(bl.send_key(), bl.receive_key(), be.send_key(), be.receive_key(), false).unwrap();
    if enable_caller {
        caller_link.enable_renewal(ae.send_key(), ae.receive_key(), true).unwrap();
    }
    if enable_reader {
        reader.enable_renewal(be.send_key(), be.receive_key(), true).unwrap();
    }
    let caller = CallSession::outgoing(caller_link, ID, true, 0, header(), make, 0).unwrap();
    (caller, reader)
}
/// Everything a reader link decodes from `wire`, heartbeats left out.
fn read_all(reader: &mut CallLink, wire: &[u8]) -> Result<Vec<Incoming>, call::Error> {
    let mut input = wire;
    let mut out = Vec::new();
    while let Some(incoming) = reader.read(&mut input)? {
        if !matches!(incoming, Incoming::Heartbeat(_)) {
            out.push(incoming);
        }
    }
    Ok(out)
}

#[test]
fn a_renew_record_is_queued_and_written_in_order_with_the_signals() {
    let (mut caller, mut reader) = renewal_writer(true, true);
    let first = crate::call_renewal::encode(&crate::call_renewal::Op::Armed { ticket: [1; 8] });
    let second = crate::call_renewal::encode(&crate::call_renewal::Op::Armed { ticket: [2; 8] });
    let before = caller.next_content_sequence();
    caller.send_renew(first.clone());
    caller.request_keyframe().unwrap();
    caller.send_renew(second.clone());
    let wire = flush(&mut caller, MS);
    let got = read_all(&mut reader, &wire).unwrap();
    // The call's opening signals went first (queued when it was created), then ours in queue order.
    let n = got.len();
    assert!(n >= 3);
    assert!(got[..n - 3].iter().all(|incoming| matches!(incoming, Incoming::Signal(_))));
    assert_eq!(got[n - 3..], [Incoming::Renew(first), Incoming::KeyFrameRequest, Incoming::Renew(second)]);
    assert_eq!(caller.next_content_sequence(), before + n as u64, "one end-to-end sequence per record");
}

#[test]
fn renew_records_are_not_written_when_the_link_was_not_negotiated() {
    // Neither end negotiated it: the queued record is dropped at the writer.
    let (mut caller, mut reader) = renewal_writer(false, false);
    let before = caller.next_content_sequence();
    caller.send_renew(crate::call_renewal::encode(&crate::call_renewal::Op::Armed { ticket: [1; 8] }));
    let wire = flush(&mut caller, MS);
    let got = read_all(&mut reader, &wire).unwrap();
    assert!(got.iter().all(|incoming| !matches!(incoming, Incoming::Renew(_))));
    assert_eq!(caller.next_content_sequence(), before + got.len() as u64, "the dropped record used no sequence");
    // Only the writer negotiated it: a reader that did not sign bit 4 fails closed.
    let (mut caller, mut reader) = renewal_writer(true, false);
    caller.send_renew(crate::call_renewal::encode(&crate::call_renewal::Op::Armed { ticket: [1; 8] }));
    let wire = flush(&mut caller, MS);
    assert!(read_all(&mut reader, &wire).is_err());
}

// ---- Design SR (step 5): negotiation, scope, BIND, liveness, re-assertion ----

const SEC: u64 = 1_000 * MS;

fn stall_env(last_write_ok_ns: u64) -> RenewalEnv {
    RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns: Some(last_write_ok_ns) }
}
fn has_action(actions: &[crate::call_renewal::Action], wanted: impl Fn(&crate::call_renewal::Action) -> bool) -> bool {
    actions.iter().any(wanted)
}
fn is_register(action: &crate::call_renewal::Action) -> bool {
    matches!(action, crate::call_renewal::Action::RegisterServer { .. })
}
fn is_end(action: &crate::call_renewal::Action) -> bool {
    matches!(action, crate::call_renewal::Action::EndCall(_))
}

/// Answers and starts media on a freshly dialled pair, then starts their ACTIVE clocks.
fn establish(mut caller: CallSession<Fake>, mut callee: CallSession<Fake>) -> (CallSession<Fake>, CallSession<Fake>) {
    feed(&mut caller, 0, &flush_on(&mut callee, 0).concat_bytes(), 0);
    callee.accept(true).unwrap();
    feed(&mut caller, 0, &flush_on(&mut callee, MS).concat_bytes(), MS);
    caller.media_ready().unwrap();
    feed(&mut callee, 0, &flush_on(&mut caller, 2 * MS).concat_bytes(), 2 * MS);
    callee.media_ready().unwrap();
    assert_eq!(caller.phase(), call::Phase::Active);
    assert_eq!(callee.phase(), call::Phase::Active);
    // The call's ACTIVE clock starts at the first tick that sees it. The
    // callee never offers itself here: it has no completed write to report.
    caller.renewal_tick(3 * MS, stall_env(3 * MS));
    callee.renewal_tick(3 * MS, quiet_env());
    (caller, callee)
}

/// An answered, ACTIVE call whose two ends hold the same `K_renew` (a stand-in
/// for both signed hellos); `*_direct` is each end's checked link fact.
fn renewal_active_with(caller_direct: bool, callee_direct: bool) -> (CallSession<Fake>, CallSession<Fake>) {
    let (mut caller, mut callee, _) = dial(true, false);
    caller.test_enable_renewal(&[1; 32], &[2; 32], caller_direct);
    callee.test_enable_renewal(&[2; 32], &[1; 32], callee_direct);
    establish(caller, callee)
}
/// The same over links that accept LABELS.
fn renewal_active_labelled() -> (CallSession<Fake>, CallSession<Fake>) {
    let (mut a, mut b) = labelled_links();
    a.enable_renewal(&[1; 32], &[2; 32], true).unwrap();
    b.enable_renewal(&[2; 32], &[1; 32], true).unwrap();
    let mut caller = CallSession::outgoing(a, ID, true, 0, header(), make, 0).unwrap();
    let wire = flush(&mut caller, 0);
    let mut input = wire.as_slice();
    let Some(Incoming::Heartbeat(None)) = b.read(&mut input).unwrap() else { panic!() };
    let Some(Incoming::Signal(invite)) = b.read(&mut input).unwrap() else { panic!() };
    let (mut callee, _) = CallSession::incoming(b, &invite, header(), make, 0).unwrap();
    callee.on_input(&mut input, 0).unwrap();
    establish(caller, callee)
}
fn renewal_active() -> (CallSession<Fake>, CallSession<Fake>) {
    renewal_active_with(true, true)
}
/// An environment in which this side never starts an offer (no completed write).
fn quiet_env() -> RenewalEnv {
    RenewalEnv { gatt: true, wifi_attempt: false, last_write_ok_ns: None }
}

/// The caller stalls and offers; the callee reads the OFFER under `env`.
/// Returns the caller's and the callee's actions.
fn offer_at(
    caller: &mut CallSession<Fake>,
    callee: &mut CallSession<Fake>,
    at: u64,
    env: RenewalEnv,
) -> (Vec<crate::call_renewal::Action>, Vec<crate::call_renewal::Action>) {
    callee.renewal_tick(at - MS, env);
    let offered = caller.renewal_tick(at, stall_env(at - 100 * MS));
    let wire = flush_on(caller, at).concat_bytes();
    feed(callee, 0, &wire, at);
    let armed = callee.renewal_tick(at, env);
    (offered, armed)
}

#[test]
fn renewal_is_negotiated_only_when_both_endpoint_hellos_sign_bit_four() {
    assert!(crate::call_renewal::negotiated(true, 0x1c, 0x1c));
    assert!(!crate::call_renewal::negotiated(true, 0x1c, 0x0c));
    assert!(!crate::call_renewal::negotiated(true, 0x0c, 0x1c));
    let (with, _) = renewal_active();
    assert!(with.renewal_negotiated());
    assert!(with.renewal_key().is_some());
    // Without a key a session never starts or answers a renewal, however long the stall.
    let (mut plain, mut other, _) = dial(true, false);
    assert!(!plain.renewal_negotiated());
    plain.renewal_tick(3 * MS, stall_env(3 * MS));
    assert!(plain.renewal_tick(60 * SEC, stall_env(60 * SEC)).is_empty());
    assert!(!plain.renewal_running());
    assert!(other.renewal_tick(60 * SEC, stall_env(60 * SEC)).is_empty());
}

#[test]
fn no_renewal_key_is_derived_when_either_side_did_not_sign_bit_four() {
    // The writer refuses to seal type 12 and the reader fails closed on it.
    let (mut caller, mut reader) = renewal_writer(false, true);
    caller.send_renew(crate::call_renewal::encode(&crate::call_renewal::Op::Armed { ticket: [1; 8] }));
    let wire = flush(&mut caller, MS);
    let got = read_all(&mut reader, &wire).unwrap();
    assert!(got.iter().all(|incoming| !matches!(incoming, Incoming::Renew(_))));
    assert!(caller.renewal_key().is_none());
}

#[test]
fn a_stalled_call_offers_then_the_peer_arms_and_advertises() {
    let (mut caller, mut callee) = renewal_active();
    let (offered, armed) = offer_at(&mut caller, &mut callee, 13 * SEC, quiet_env());
    assert!(has_action(&offered, |a| matches!(a, crate::call_renewal::Action::Scan { .. })));
    assert!(caller.renewal_running());
    assert!(has_action(&armed, is_register));
    assert!(callee.renewal_running());
    let registered = callee.renewal_registered(13 * SEC + 50 * MS, true);
    let advertise = registered.iter().find_map(|a| if let crate::call_renewal::Action::Advertise { uuid, .. } = a { Some(*uuid) } else { None });
    let expected = crate::call_renewal::advert_uuid(&caller.renewal_key().unwrap(), &caller.renewal_call_id(), &caller.renewal_ticket()).unwrap();
    assert_eq!(advertise, Some(expected), "both ends derive the same per-attempt UUID");
    // ARMED is queued on the old link, best effort.
    callee.renewal_advertising(13 * SEC + 100 * MS, true);
    assert_eq!(callee.renewal_phase(), crate::call_renewal::Phase::Armed);
    assert!(!has_action(&caller.renewal_tick(13 * SEC + 200 * MS, stall_env(13 * SEC)), is_end));
    assert!(caller.renewal_running());
}

#[test]
fn renewal_scope_requires_a_direct_gatt_active_call() {
    // `direct` and the transport facts are checked on the answering side: it
    // arms (a server is asked for) or refuses and arms nothing.
    let armed_for = |callee_direct: bool, gatt: bool, wifi_attempt: bool| {
        let (mut caller, mut callee) = renewal_active_with(true, callee_direct);
        let env = RenewalEnv { gatt, wifi_attempt, last_write_ok_ns: None };
        let (offered, armed) = offer_at(&mut caller, &mut callee, 13 * SEC, env);
        assert!(has_action(&offered, |a| matches!(a, crate::call_renewal::Action::Scan { .. })), "the offerer itself may offer");
        (has_action(&armed, is_register), callee.renewal_running())
    };
    // Direct and GATT: armed.
    assert_eq!(armed_for(true, true, false), (true, true));
    // The link peer is not the end-to-end peer: REFUSED not direct, nothing armed.
    assert_eq!(armed_for(false, true, false), (false, false));
    // Not GATT: the same.
    assert_eq!(armed_for(true, false, false), (false, false));
    // A Wi-Fi attempt in progress: REFUSED limit (RD12), nothing armed.
    assert_eq!(armed_for(true, true, true), (false, false));
}

#[test]
fn a_renewal_that_finds_nothing_hands_liveness_back_at_once() {
    let (mut caller, mut callee) = renewal_active();
    offer_at(&mut caller, &mut callee, 13 * SEC, quiet_env());
    // Owning liveness, the call is not dead however long the silence.
    assert!(!caller.is_dead(15 * SEC));
    assert!(!callee.is_dead(15 * SEC));
    // Six seconds without the advert: the offerer keeps the old link and the ordinary 8 s rule applies at once.
    let actions = caller.renewal_tick(19 * SEC, stall_env(19 * SEC));
    assert!(!has_action(&actions, |a| matches!(a, crate::call_renewal::Action::CloseOldLink)));
    assert!(!caller.renewal_running());
    assert!(caller.is_dead(19 * SEC), "19 s of silence is dead the moment ownership ends");
}

#[test]
fn the_offerer_ends_twelve_seconds_after_offer_and_the_armed_side_suspends_liveness() {
    let (mut caller, mut callee) = renewal_active();
    offer_at(&mut caller, &mut callee, 13 * SEC, quiet_env());
    callee.renewal_registered(13 * SEC + 10 * MS, true);
    callee.renewal_advertising(13 * SEC + 20 * MS, true);
    // Found: the old link is broken.
    let found = caller.renewal_found(14 * SEC);
    assert!(has_action(&found, |a| matches!(a, crate::call_renewal::Action::CloseOldLink)));
    assert!(!caller.is_dead(24 * SEC), "inside the 12 s bound the renewal owns liveness");
    assert!(!has_action(&caller.renewal_tick(24 * SEC + 999 * MS, stall_env(0)), is_end));
    let ended = caller.renewal_tick(25 * SEC, stall_env(0));
    assert!(has_action(&ended, |a| matches!(a, crate::call_renewal::Action::EndCall("link_lost"))));
    assert!(caller.is_dead(25 * SEC), "the ordinary rules apply once it ends");
    // The armed side suspended its liveness hang-up from OFFER on...
    assert!(callee.renewal_running() && !callee.is_dead(24 * SEC));
    // ...and, its offerer having kept the old link, only disarms at its own bound.
    let disarmed = callee.renewal_tick(25 * SEC, quiet_env());
    assert!(!has_action(&disarmed, is_end));
    assert!(!callee.renewal_running());
}

/// Runs the whole renewal between two sessions: the opening, BIND, and both
/// switches. Returns the instant of the bind.
fn bind_both(caller: &mut CallSession<Fake>, callee: &mut CallSession<Fake>) -> u64 {
    use crate::call_renewal::{Action, Opening, challenge_frame, parse_challenge};
    use crate::records::Layer;
    let (offered, _) = offer_at(caller, callee, 13 * SEC, quiet_env());
    assert!(has_action(&offered, |a| matches!(a, Action::Scan { .. })));
    callee.renewal_registered(13 * SEC + 10 * MS, true);
    callee.renewal_advertising(13 * SEC + 20 * MS, true);
    let found = caller.renewal_found(14 * SEC);
    assert!(has_action(&found, |a| matches!(a, Action::Connect { .. })));
    // The opening on the new link.
    let (call_id, ticket) = (caller.renewal_call_id(), caller.renewal_ticket());
    let mut armed = Opening::armed(callee.renewal_key().unwrap(), call_id, ticket, [7; 16]).at(14 * SEC / MS);
    let mut offerer = Opening::offerer(caller.renewal_key().unwrap(), call_id, ticket, [8; 16]);
    let challenge = parse_challenge(&challenge_frame(&armed.challenge())).unwrap();
    let proof = offerer.proof(&challenge).unwrap();
    assert!(armed.accept_proof(&proof));
    assert!(offerer.accept_confirm(&armed.confirm().unwrap()));
    let (offerer_keys, armed_keys) = (offerer.hop_keys().unwrap(), armed.hop_keys().unwrap());
    let (mut caller_out, caller_in) = caller.renewal_hop_layer(true, &offerer_keys).unwrap();
    let (callee_out, mut callee_in) = callee.renewal_hop_layer(false, &armed_keys).unwrap();
    // BIND is the first record of the new link.
    let at = 14 * SEC + 100 * MS;
    let bind = caller.renewal_seal_bind(&mut caller_out, at).unwrap();
    let mut input = bind.as_slice();
    let record = callee_in.read(&mut input, Layer::Hop.maximum_ciphertext()).unwrap().unwrap();
    assert_eq!(record.header.sequence, 0);
    assert!(callee.renewal_accept_bind(&record.plaintext, at));
    caller.renewal_bound(caller_out, caller_in, at);
    callee.renewal_bound(callee_out, callee_in, at);
    at
}

#[test]
fn bind_is_accepted_only_as_the_first_record_with_its_own_next_sequence() {
    let (mut caller, mut callee) = renewal_active();
    let at = bind_both(&mut caller, &mut callee);
    assert_eq!(caller.renewal_phase(), crate::call_renewal::Phase::Bound);
    assert_eq!(callee.renewal_phase(), crate::call_renewal::Phase::Bound);
    // A BIND is accepted once: a second one finds the renewal already bound.
    assert!(!callee.renewal_accept_bind(&[0u8; 40], at));
    // Neither side owns liveness any more; the new link starts with a clean clock.
    assert!(!caller.is_dead(at + 7 * SEC) && !callee.is_dead(at + 7 * SEC));
    assert!(caller.is_dead(at + 8 * SEC), "eight silent seconds after the bind end it as ever");
}

#[test]
fn a_bind_after_the_twelve_second_bound_is_never_sent_or_accepted() {
    use crate::call_renewal::{Opening, challenge_frame, parse_challenge};
    let (mut caller, mut callee) = renewal_active();
    offer_at(&mut caller, &mut callee, 13 * SEC, quiet_env());
    callee.renewal_registered(13 * SEC + 10 * MS, true);
    callee.renewal_advertising(13 * SEC + 20 * MS, true);
    caller.renewal_found(14 * SEC);
    let (call_id, ticket) = (caller.renewal_call_id(), caller.renewal_ticket());
    let mut armed = Opening::armed(callee.renewal_key().unwrap(), call_id, ticket, [7; 16]);
    let mut offerer = Opening::offerer(caller.renewal_key().unwrap(), call_id, ticket, [8; 16]);
    let proof = offerer.proof(&parse_challenge(&challenge_frame(&armed.challenge())).unwrap()).unwrap();
    assert!(armed.accept_proof(&proof) && offerer.accept_confirm(&armed.confirm().unwrap()));
    let (mut out, _) = caller.renewal_hop_layer(true, &offerer.hop_keys().unwrap()).unwrap();
    assert!(caller.renewal_seal_bind(&mut out, 25 * SEC).is_err(), "12 s after the offer is too late");
    assert!(caller.renewal_seal_bind(&mut out, 24 * SEC + 999 * MS).is_ok());
}

#[test]
fn the_call_goes_on_over_the_new_hop_with_the_same_end_to_end_session() {
    let (mut caller, mut callee) = renewal_active();
    let before = caller.next_content_sequence();
    let at = bind_both(&mut caller, &mut callee);
    // What the offerer writes after the bind is sealed by the new hop keys and read by the peer's.
    let wire = flush(&mut caller, at + MS);
    let notices = callee.on_input(&mut wire.as_slice(), at + MS).unwrap();
    assert!(notices.contains(&Notice::KeyFrameRequested), "{notices:?}");
    assert!(caller.next_content_sequence() > before, "the end-to-end sequence continues");
}

#[test]
fn after_bind_the_session_reasserts_a_key_frame_microphone_camera_and_labels_once() {
    let (mut caller, mut callee) = renewal_active_labelled();
    let (caller_id, _, _) = identities();
    caller.use_policy(open_policy(Vec::new(), None));
    callee.use_policy(open_policy(Vec::new(), None));
    callee.accept_labels(crate::crypto::sha256(&caller_id.public_key().unwrap()).unwrap()[..16].try_into().unwrap());
    caller.accept_labels([0; 16]);
    let label = crate::labels::sign(&caller_id, crate::labels::KIND_NAME, 0, crate::labels::BINDING_CALL, &ID, "Alice").unwrap();
    caller.send_labels(crate::labels::container(&[label]));
    let name = flush_on(&mut caller, 4 * MS).concat_bytes();
    let first = callee.on_input(&mut name.as_slice(), 4 * MS).unwrap();
    assert!(first.iter().any(|n| matches!(n, Notice::PeerLabel { .. })));
    caller.push_to_talk(false).unwrap();
    let mute = flush_on(&mut caller, 5 * MS).concat_bytes();
    callee.on_input(&mut mute.as_slice(), 5 * MS).unwrap();
    callee.take_remote();
    let at = bind_both(&mut caller, &mut callee);
    let sequence = caller.next_content_sequence();
    let wire = flush(&mut caller, at + MS);
    let notices = callee.on_input(&mut wire.as_slice(), at + MS).unwrap();
    assert!(notices.contains(&Notice::KeyFrameRequested), "{notices:?}");
    let signals: Vec<_> = callee.take_remote().iter().map(|m| m.signal).collect();
    assert!(signals.contains(&Signal::MicrophoneMuted), "the current microphone state is re-asserted: {signals:?}");
    assert!(signals.contains(&Signal::CameraOff) || signals.contains(&Signal::CameraOn));
    // Camera, microphone, the key frame request and the caller's own LABELS: four records, once.
    assert_eq!(caller.next_content_sequence() - sequence, 4);
    let again = flush(&mut caller, at + 2 * MS);
    callee.on_input(&mut again.as_slice(), at + 2 * MS).unwrap();
    assert_eq!(caller.next_content_sequence() - sequence, 4, "nothing is re-asserted a second time");
}

#[test]
fn an_offerer_without_a_name_sent_re_asserts_no_labels() {
    let (mut caller, mut callee) = renewal_active_labelled();
    caller.accept_labels([0; 16]);
    callee.accept_labels([0; 16]);
    let at = bind_both(&mut caller, &mut callee);
    let sequence = caller.next_content_sequence();
    let wire = flush(&mut caller, at + MS);
    callee.on_input(&mut wire.as_slice(), at + MS).unwrap();
    // Camera, microphone and the key frame request; no LABELS.
    assert_eq!(caller.next_content_sequence() - sequence, 3);
}

#[test]
fn a_malformed_renew_record_ends_the_call_and_a_bind_on_the_old_link_changes_nothing() {
    use crate::call_renewal::Op;
    let (mut caller, mut callee) = renewal_active();
    // A well-formed BIND on the old link is logged and ignored: nothing binds.
    caller.send_renew(crate::call_renewal::encode(&Op::Bind { ticket: [3; 8], next_sequence: 9 }));
    let wire = flush(&mut caller, 4 * MS);
    callee.on_input(&mut wire.as_slice(), 4 * MS).unwrap();
    assert_eq!(callee.renewal_phase(), crate::call_renewal::Phase::Idle);
    // A malformed one (an all-zero ticket) fails closed.
    caller.send_renew(vec![2, 0, 0, 0, 0, 0, 0, 0, 0]);
    let wire = flush(&mut caller, 5 * MS);
    assert!(callee.on_input(&mut wire.as_slice(), 5 * MS).is_err());
}

/// An answered voice call with the caller's Opus configuration delivered.
fn talking(coalesce: bool) -> (CallSession<Fake>, CallSession<Fake>) {
    let (mut caller, mut callee, _) = dial(true, false);
    caller.audio_coalesce = coalesce;
    callee.accept(true).unwrap();
    caller.on_input(&mut flush(&mut callee, MS).as_slice(), MS).unwrap();
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    callee.on_input(&mut flush(&mut caller, 2 * MS).as_slice(), 2 * MS).unwrap();
    caller.send(packet(AUDIO | CONFIG, 0, 3, 9)).unwrap();
    callee.on_input(&mut flush(&mut caller, 3 * MS).as_slice(), 3 * MS).unwrap();
    (caller, callee)
}
/// The first sample of every unit the callee plays, in order (the fake
/// decoder fills a unit's PCM with its first byte).
fn played(callee: &mut CallSession<Fake>) -> Vec<i16> {
    let mut pcm = [0; 64];
    let mut heard = Vec::new();
    callee.audio().pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.push(s[0])).unwrap();
    heard
}

/// HEALTH-12: with coalescing on, units that queued while the writer was
/// busy leave as one record and the peer plays every one of them in order.
#[test]
fn with_coalescing_on_queued_call_audio_leaves_as_one_record_and_plays_every_unit() {
    for (coalesce, records) in [(false, 5), (true, 1)] {
        let (mut caller, mut callee) = talking(coalesce);
        let sent = caller.stats().audio_sent;
        for i in 0..5 {
            caller.send(packet(AUDIO, 20_000 * i, 4, 1 + i as u8)).unwrap();
        }
        callee.on_input(&mut flush(&mut caller, 4 * MS).as_slice(), 4 * MS).unwrap();
        assert_eq!(caller.stats().audio_sent - sent, records, "coalescing {coalesce}");
        assert_eq!(played(&mut callee), [1, 2, 3, 4, 5]);
        let stats = caller.stats();
        assert_eq!((stats.audio_coalesced_records, stats.audio_coalesced_units), if coalesce { (1, 5) } else { (0, 0) });
    }
}

/// No added delay: a unit with nothing behind it is written at once and
/// alone, and a unit captured later is never waited for.
#[test]
fn coalescing_never_waits_for_audio_that_has_not_been_captured() {
    let (mut caller, mut callee) = talking(true);
    caller.send(packet(AUDIO, 0, 4, 1)).unwrap();
    let first = caller.next_write(4 * MS).unwrap().expect("written at once");
    assert!(caller.next_write(4 * MS).unwrap().is_none());
    caller.send(packet(AUDIO, 20_000, 4, 2)).unwrap();
    let second = caller.next_write(5 * MS).unwrap().expect("written at once");
    callee.on_input(&mut [first, second].concat().as_slice(), 5 * MS).unwrap();
    assert_eq!(played(&mut callee), [1, 2]);
    assert_eq!(caller.stats().audio_coalesced_records, 0);
}

/// The receiver plays packed and unpacked call audio alike, mixed in one call
/// (a peer that coalesces and one that does not).
#[test]
fn a_receiver_plays_packed_and_unpacked_call_audio() {
    let (mut caller, mut callee) = talking(false);
    let unit = |pts, byte| packet(AUDIO, pts, 4, byte);
    let bodies: Vec<[u8; 4]> = (0..8).map(|b| [b; 4]).collect();
    let packed = |bytes: &[u8]| media::pack_audio(&bytes.iter().map(|&b| media::Packet {
        flags: AUDIO, pts_us: 20_000 * i64::from(b - 1), payload: &bodies[b as usize] }).collect::<Vec<_>>()).unwrap();
    let pair = packed(&[1, 2]);
    let batch = packed(&[4, 5, 6]);
    for packet in [pair, unit(40_000, 3), batch, unit(120_000, 7)] {
        caller.send(packet).unwrap();
    }
    callee.on_input(&mut flush(&mut caller, 4 * MS).as_slice(), 4 * MS).unwrap();
    assert_eq!(played(&mut callee), [1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn coalescing_takes_only_ordinary_units_within_one_second_and_at_most_twenty() {
    let unit = |pts: i64| packet(AUDIO, pts, 4, 1);
    let units_in = |raw: &[u8]| {
        let p = media::Packet::decode(raw, media::MAX_PAYLOAD).unwrap();
        if p.flags & media::AUDIO_BATCH != 0 { p.split_audio().unwrap().len() } else { 1 }
    };
    // Twenty-five queued units: twenty in the first record, five in the next.
    let mut queue: VecDeque<Vec<u8>> = (1..25).map(|i| unit(20_000 * i)).collect();
    let (record, n) = coalesce_queued_audio(unit(0), &mut queue);
    assert_eq!((n, units_in(&record), queue.len()), (20, 20, 5));
    // A configuration record behind the first stops the record and stays queued.
    let mut queue = VecDeque::from([unit(20_000), packet(AUDIO | CONFIG, 40_000, 3, 9), unit(60_000)]);
    let (record, n) = coalesce_queued_audio(unit(0), &mut queue);
    assert_eq!((n, units_in(&record), queue.len()), (2, 2, 2));
    assert_eq!(media::Packet::decode(&queue[0], media::MAX_PAYLOAD).unwrap().flags, AUDIO | CONFIG);
    // More than 1 s after the first, or earlier than it: not taken.
    for late in [media::MAX_AUDIO_GAP_US + 1, -20_000] {
        let mut queue = VecDeque::from([unit(late)]);
        let first = unit(0);
        assert_eq!(coalesce_queued_audio(first.clone(), &mut queue), (first, 1));
        assert_eq!(queue.len(), 1);
    }
    // Never packed again, and a configuration first is sent as it is.
    for first in [packet(AUDIO | CONFIG, 0, 3, 9), media::pack_audio(&[media::Packet { flags: AUDIO, pts_us: 0, payload: &[1] }; 2]).unwrap()] {
        let mut queue = VecDeque::from([unit(20_000)]);
        assert_eq!(coalesce_queued_audio(first.clone(), &mut queue), (first, 1));
        assert_eq!(queue.len(), 1);
    }
    // An empty queue: the unit exactly as it was.
    let first = unit(0);
    assert_eq!(coalesce_queued_audio(first.clone(), &mut VecDeque::new()), (first, 1));
}
