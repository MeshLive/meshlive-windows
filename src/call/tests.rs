use super::{fragments::*, *};
use crate::{
    bmh2::Purpose,
    framing::HopReader,
    records::{Layer, Policy, Sealer},
    session::tests::{handshake, hex, identities},
    wifi::{
        pulse::Pulse,
        wire::{Bwf1, Ready},
    },
};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/android-vectors.json")).unwrap()
}
const ID: [u8; 16] = [
    0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
];

#[test]
fn bmc2_matches_the_android_vectors_and_rejects_every_invalid_form() {
    let v = vectors();
    let invite = Message::decode(&hex(v["bmc2RelayedInvite"].as_str().unwrap())).unwrap();
    assert_eq!(
        invite,
        Message {
            call_id: ID,
            signal: Signal::Invite,
            reason: Reason::None,
            relay_hops: 3
        }
    );
    let decline = Message::decode(&hex(v["bmc2Decline"].as_str().unwrap())).unwrap();
    assert_eq!(
        (decline.signal, decline.reason),
        (Signal::Decline, Reason::Busy)
    );
    let mut signals = 0;
    for content in v["bme1Contents"].as_array().unwrap() {
        if content["type"] == 6 {
            let raw = hex(content["plaintext"].as_str().unwrap());
            assert_eq!(
                Message::decode(&raw).unwrap().encode().unwrap().to_vec(),
                raw
            );
            signals += 1;
        }
    }
    assert_eq!(signals, 3);
    let good = invite.encode().unwrap();
    for cut in 0..SIGNAL_BYTES {
        assert!(Message::decode(&good[..cut]).is_err());
    }
    assert!(Message::decode(&[good.as_slice(), &[0]].concat()).is_err());
    for (index, value) in [(0, b'X'), (4, 0), (4, 11), (21, 6), (22, 16)] {
        let mut bad = good;
        bad[index] = value;
        assert!(Message::decode(&bad).is_err(), "byte {index}");
    }
    let mut zero = good;
    zero[5..21].fill(0);
    assert_eq!(Message::decode(&zero), Err(Error::CallId));
    for (signal, reason, hops) in [
        (Signal::Ringing, Reason::None, 1),
        (Signal::Decline, Reason::None, 0),
        (Signal::Decline, Reason::Normal, 0),
        (Signal::HangUp, Reason::None, 0),
        (Signal::HangUp, Reason::Busy, 0),
        (Signal::Accept, Reason::Error, 0),
    ] {
        assert!(
            Message::new(ID, signal, reason, hops).is_err(),
            "{signal:?}"
        );
    }
    assert!(Message::new(ID, Signal::HangUp, Reason::Error, 0).is_ok());
}

#[test]
fn the_state_machine_allows_exactly_androids_transitions() {
    use {Event as E, Phase as P};
    let phases = [
        P::OutgoingInvite,
        P::OutgoingRinging,
        P::IncomingRinging,
        P::NegotiatingMedia,
        P::Active,
        P::Ended,
    ];
    let events = [
        E::RemoteRinging,
        E::LocalAccept,
        E::RemoteAccept,
        E::LocalDecline,
        E::RemoteDecline,
        E::LocalCancel,
        E::RemoteCancel,
        E::MediaReady,
        E::LocalHangUp,
        E::RemoteHangUp,
        E::LinkLost,
    ];
    let allowed = phases
        .iter()
        .flat_map(|p| events.iter().map(move |e| (*p, *e)))
        .filter(|(p, e)| transition(*p, *e).is_ok())
        .count();
    // 5 + 4 + 4 + 4 + 3 + 0 transitions in Android PrivateCallState.
    assert_eq!(allowed, 20);
    assert_eq!(
        transition(P::OutgoingInvite, E::RemoteAccept),
        Ok(P::NegotiatingMedia)
    );
    assert_eq!(
        transition(P::NegotiatingMedia, E::MediaReady),
        Ok(P::Active)
    );
    assert!(transition(P::Active, E::LocalAccept).is_err());
    assert!(
        phases
            .iter()
            .all(|p| *p != P::Ended || events.iter().all(|e| transition(*p, *e).is_err()))
    );
}

#[test]
fn video_and_voice_calls_follow_the_signal_order_and_media_rules() {
    // Video call.
    let (mut caller, opening) = Controller::outgoing(ID, false, 0).unwrap();
    assert_eq!(opening.len(), 1);
    let (mut callee, actions) = Controller::incoming(&opening[0]).unwrap();
    assert_eq!(actions[1], Action::Ring { voice: false });
    let Action::Send(ringing) = actions[0] else {
        panic!()
    };
    assert!(caller.on_message(&ringing).unwrap().is_empty());
    assert!(!caller.should_send_audio());
    let answer = callee.accept(false).unwrap();
    assert_eq!(answer.last(), Some(&Action::StartMedia { camera: true }));
    let Action::Send(accept) = answer[0] else {
        panic!()
    };
    assert_eq!(
        caller.on_message(&accept).unwrap(),
        vec![Action::StartMedia { camera: true }]
    );
    caller.media_ready().unwrap();
    callee.media_ready().unwrap();
    assert!(caller.should_send_audio() && caller.should_send_video());
    // Push-to-talk: released = muted, audio dropped; the peer hears the state.
    let muted = caller.push_to_talk(false).unwrap();
    assert!(!caller.should_send_audio());
    assert_eq!(
        callee.on_message(&muted).unwrap(),
        vec![Action::PeerMicrophone(false)]
    );
    let talk = caller.push_to_talk(true).unwrap();
    assert!(caller.should_send_audio());
    assert_eq!(
        callee.on_message(&talk).unwrap(),
        vec![Action::PeerMicrophone(true)]
    );
    let off = callee.set_camera(false).unwrap();
    assert!(!callee.should_send_video());
    assert_eq!(
        caller.on_message(&off).unwrap(),
        vec![Action::PeerCamera(false)]
    );
    let bye = caller.hang_up(Reason::Normal).unwrap();
    assert_eq!(
        callee.on_message(&bye).unwrap(),
        vec![Action::Ended(Reason::Normal)]
    );
    assert_eq!(callee.phase(), Phase::Ended);
    assert!(callee.on_message(&talk).is_err());
    assert!(caller.push_to_talk(true).is_err());

    // Voice call: Camera off before acceptance is a voice offer; the answer stays voice.
    let (mut caller, opening) = Controller::outgoing(ID, true, 2).unwrap();
    assert_eq!(opening[1].signal, Signal::CameraOff);
    let (mut callee, _) = Controller::incoming(&opening[0]).unwrap();
    assert_eq!(callee.relay_hops(), 2);
    assert_eq!(
        callee.on_message(&opening[1]).unwrap(),
        vec![Action::Ring { voice: true }]
    );
    assert!(callee.voice_offer());
    // Camera on while ringing withdraws the voice offer.
    let on = Message::simple(ID, Signal::CameraOn).unwrap();
    assert_eq!(
        callee.on_message(&on).unwrap(),
        vec![Action::Ring { voice: false }]
    );
    callee.on_message(&opening[1]).unwrap();
    let answer = callee.accept(true).unwrap();
    assert!(matches!(answer[1], Action::Send(m) if m.signal == Signal::CameraOff));
    assert_eq!(answer[2], Action::StartMedia { camera: false });
    let Action::Send(accept) = answer[0] else {
        panic!()
    };
    assert_eq!(
        caller.on_message(&accept).unwrap(),
        vec![Action::StartMedia { camera: false }]
    );
    assert!(!caller.should_send_video() && !callee.should_send_video());
}

#[test]
fn decline_cancel_wrong_ids_and_wrong_roles_are_refused() {
    let (mut caller, opening) = Controller::outgoing(ID, false, 0).unwrap();
    let (mut callee, _) = Controller::incoming(&opening[0]).unwrap();
    let mut other = ID;
    other[0] ^= 1;
    assert_eq!(
        caller.on_message(&Message::simple(other, Signal::Ringing).unwrap()),
        Err(Error::CallId)
    );
    assert_eq!(caller.on_message(&opening[0]), Err(Error::State));
    assert!(callee.cancel().is_err());
    assert!(caller.accept(false).is_err());
    assert!(caller.decline(Reason::Busy).is_err());
    assert!(Controller::incoming(&Message::simple(ID, Signal::Ringing).unwrap()).is_err());
    let decline = callee.decline(Reason::Busy).unwrap();
    assert_eq!(
        caller.on_message(&decline).unwrap(),
        vec![Action::Ended(Reason::Busy)]
    );
    let (mut caller, opening) = Controller::outgoing(ID, false, 0).unwrap();
    let (mut callee, _) = Controller::incoming(&opening[0]).unwrap();
    let cancel = caller.cancel().unwrap();
    assert_eq!(
        callee.on_message(&cancel).unwrap(),
        vec![Action::Ended(Reason::None)]
    );
    assert!(callee.accept(false).is_err());
    assert!(Controller::outgoing(ID, false, 16).is_err());
}

#[test]
fn fragments_match_the_android_vector_and_reassembly_fails_closed() {
    let v = vectors();
    let packet = hex(v["fragmentedPacket"].as_str().unwrap());
    let expected: Vec<Vec<u8>> = v["fragments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| hex(f.as_str().unwrap()))
        .collect();
    assert!(should_fragment(packet.len(), packet[4]));
    assert!(!should_fragment(3_072, 0));
    assert!(!should_fragment(5_000, AUDIO));
    assert_eq!(split(&packet, FRAGMENT_BYTES).unwrap(), expected);
    let mut r = Reassembler::default();
    assert_eq!(r.accept(packet[4], &expected[0]).unwrap(), None);
    assert!(r.in_progress());
    assert_eq!(r.accept(packet[4], &expected[1]).unwrap(), None);
    assert_eq!(
        r.accept(packet[4], &expected[2]).unwrap(),
        Some(packet.clone())
    );
    assert!(!r.in_progress());
    let flags = packet[4];
    let fail = |first: &[u8], second: Option<(&[u8], u8)>| {
        let mut r = Reassembler::default();
        match second {
            None => r.accept(flags, first).is_err(),
            Some((next, next_flags)) => {
                r.accept(flags, first).unwrap();
                r.accept(next_flags, next).is_err()
            }
        }
    };
    assert!(fail(&expected[1], None), "does not start at 0");
    assert!(fail(&expected[0], Some((&expected[2], flags))), "gap");
    assert!(fail(&expected[0], Some((&expected[0], flags))), "overlap");
    assert!(
        fail(&expected[0], Some((&expected[1], flags | 0x80))),
        "changed flags"
    );
    let mut total = expected[1].clone();
    total[3] ^= 1;
    assert!(fail(&expected[0], Some((&total, flags))), "changed total");
    assert!(Reassembler::default().accept(AUDIO, &expected[0]).is_err());
    assert!(
        Reassembler::default()
            .accept(flags, &expected[0][..8])
            .is_err()
    );
    let mut overrun = expected[2].clone();
    overrun.push(0);
    let mut r = Reassembler::default();
    r.accept(flags, &expected[0]).unwrap();
    r.accept(flags, &expected[1]).unwrap();
    assert!(r.accept(flags, &overrun).is_err());
    assert!(negotiated(true, CAPABILITY_BIT, 0));
    assert!(!negotiated(true, CAPABILITY_BIT, 1));
    assert!(!negotiated(false, CAPABILITY_BIT, 0));
    assert!(!negotiated(true, 0x7f, 0));
}

fn pair(fragments: bool) -> (CallLink, CallLink) {
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
fn drain(link: &mut CallLink, wire: &[u8]) -> Vec<Incoming> {
    let mut out = Vec::new();
    for chunk in wire.chunks(61) {
        let mut input = chunk;
        while let Some(event) = link.read(&mut input).unwrap() {
            out.push(event);
        }
    }
    out
}

#[test]
fn media_phase_gate_runs_after_authentication_and_before_parsing_or_reassembly() {
    for phase in [None, Some(Phase::OutgoingInvite), Some(Phase::OutgoingRinging), Some(Phase::IncomingRinging), Some(Phase::Ended)] {
        for (kind, flags) in [(1, 0), (2, AUDIO), (9, media::KEY_FRAME)] {
            let (mut sender, mut receiver) = pair(true);
            // Authenticated but malformed media: phase refusal/drop must happen
            // before header parsing, packet decoding or fragment allocation.
            let content = sender.content_out.seal(kind, flags, &[0]).unwrap();
            let wire = sender.hop_out.seal(5, 0, &content).unwrap();
            let result = receiver.read_in_phase(&mut wire.as_slice(), phase);
            if phase == Some(Phase::Ended) { assert_eq!(result, Ok(None)); }
            else { assert_eq!(result, Err(Error::State), "{phase:?} kind={kind}"); }
            assert!(!receiver.reassembler.in_progress());

            let (mut sender, mut receiver) = pair(true);
            let mut content = sender.content_out.seal(kind, flags, &[0]).unwrap();
            *content.last_mut().unwrap() ^= 1;
            let forged = sender.hop_out.seal(5, 0, &content).unwrap();
            assert!(matches!(receiver.read_in_phase(&mut forged.as_slice(), phase), Err(Error::Record(_))), "late media must still authenticate");
        }
    }
}

#[test]
fn fragment_control_parity_keeps_authentication_and_strict_media_reassembly() {
    let big = hex(vectors()["fragmentedPacket"].as_str().unwrap());
    for enabled in [false, true] {
        let (mut sender, mut receiver) = pair(true);
        receiver.allow_fragment_controls(enabled);
        let plan = sender.plan(&big).unwrap();
        assert_eq!(receiver.read(&mut sender.seal_part(&plan, 0).unwrap().as_slice()), Ok(None));
        let camera_off = Message::simple(ID, Signal::CameraOff).unwrap();
        let control = sender.signal(&camera_off).unwrap();
        let result = receiver.read(&mut control.as_slice());
        if !enabled { assert_eq!(result, Err(Error::Fragment)); continue; }
        assert_eq!(result, Ok(Some(Incoming::Signal(camera_off))));
        assert!(receiver.reassembler.in_progress());
        assert_eq!(receiver.read(&mut sender.keyframe_request().unwrap().as_slice()), Ok(Some(Incoming::KeyFrameRequest)));
        let mut remainder = Vec::new();
        for index in 1..plan.len() { remainder.extend(sender.seal_part(&plan, index).unwrap()); }
        let Some(Incoming::Media { flags, pts_us, payload }) = receiver.read(&mut remainder.as_slice()).unwrap() else { panic!("complete frame") };
        let expected = media::Packet::decode(&big, media::MAX_PAYLOAD).unwrap();
        assert_eq!((flags, pts_us, payload.as_slice()), (expected.flags, expected.pts_us, expected.payload));
        assert!(!receiver.reassembler.in_progress());
    }
    for kind in [1, 2, 6] {
        let (mut sender, mut receiver) = pair(true);
        receiver.allow_fragment_controls(true);
        let plan = sender.plan(&big).unwrap();
        assert_eq!(receiver.read(&mut sender.seal_part(&plan, 0).unwrap().as_slice()), Ok(None));
        let record = if kind == 6 {
            let mut content = sender.content_out.seal(6, 0, &Message::simple(ID, Signal::CameraOff).unwrap().encode().unwrap()).unwrap();
            *content.last_mut().unwrap() ^= 1;
            sender.hop_out.seal(5, 0, &content).unwrap()
        } else {
            // Header and non-audio media remain forbidden, even with valid AEAD.
            let content = sender.content_out.seal(kind, 0, &[0]).unwrap();
            sender.hop_out.seal(5, 0, &content).unwrap()
        };
        let result = receiver.read(&mut record.as_slice());
        if kind == 6 { assert!(matches!(result, Err(Error::Record(_)))); }
        else { assert_eq!(result, Err(Error::Fragment)); }
    }
}

#[test]
fn a_full_duplex_call_link_carries_signals_header_media_and_fragments_with_audio_between() {
    let v = vectors();
    let big = hex(v["fragmentedPacket"].as_str().unwrap());
    let audio = media::Packet {
        flags: AUDIO,
        pts_us: 20_000,
        payload: &[5; 60],
    }
    .encode()
    .unwrap();
    let header = media::VideoHeader {
        width: 426,
        height: 240,
        fps: 15,
        codec: 2,
        quarter_turns: 0,
        opus: true,
    };
    for fragments in [false, true] {
        let (mut caller, mut callee) = pair(fragments);
        let invite = Message::new(ID, Signal::Invite, Reason::None, 0).unwrap();
        let mut wire = caller.heartbeat(None).unwrap();
        wire.extend(caller.signal(&invite).unwrap());
        wire.extend(caller.header(&header).unwrap());
        let plan = caller.plan(&big).unwrap();
        assert_eq!(plan.len(), if fragments { 3 } else { 1 });
        // The writer may put call audio between fragments, and only audio.
        for i in 0..plan.len() {
            wire.extend(caller.seal_part(&plan, i).unwrap());
            if i == 0 {
                wire.extend(caller.media(&audio).unwrap().concat());
            }
        }
        wire.extend(caller.keyframe_request().unwrap());
        let got = drain(&mut callee, &wire);
        assert_eq!(got[0], Incoming::Heartbeat(None));
        assert_eq!(got[1], Incoming::Signal(invite));
        assert_eq!(got[2], Incoming::Header(header));
        let media = |flags: u8, pts: i64, payload: Vec<u8>| Incoming::Media {
            flags,
            pts_us: pts,
            payload,
        };
        let whole = media::Packet::decode(&big, media::MAX_PAYLOAD).unwrap();
        let big_event = media(whole.flags, whole.pts_us, whole.payload.to_vec());
        let audio_event = media(AUDIO, 20_000, vec![5; 60]);
        if fragments {
            assert_eq!(got[3], audio_event);
            assert_eq!(got[4], big_event);
        } else {
            assert_eq!(got[3], big_event);
            assert_eq!(got[4], audio_event);
        }
        assert_eq!(got[5], Incoming::KeyFrameRequest);
        // The other direction works independently on the same link.
        let ringing = Message::simple(ID, Signal::Ringing).unwrap();
        let back = callee.signal(&ringing).unwrap();
        assert_eq!(drain(&mut caller, &back), vec![Incoming::Signal(ringing)]);
    }
    // Non-audio media inside an unfinished fragmented frame fails the call.
    let (mut caller, mut callee) = pair(true);
    let plan = caller.plan(&big).unwrap();
    let mut wire = caller.seal_part(&plan, 0).unwrap();
    let small_video = media::Packet {
        flags: 0,
        pts_us: 1,
        payload: &[1; 10],
    }
    .encode()
    .unwrap();
    wire.extend(caller.media(&small_video).unwrap().concat());
    let mut input = wire.as_slice();
    assert_eq!(callee.read(&mut input), Err(Error::Fragment));
    // A receiver that did not negotiate fragments rejects type 9 as unknown.
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let mut sender = CallLink::new(
        al.send_key(),
        al.receive_key(),
        ae.send_key(),
        ae.receive_key(),
        true,
    )
    .unwrap();
    let mut plain = CallLink::new(
        bl.send_key(),
        bl.receive_key(),
        be.send_key(),
        be.receive_key(),
        false,
    )
    .unwrap();
    let wire = sender.media(&big).unwrap().concat();
    let mut input = wire.as_slice();
    assert!(matches!(
        plain.read(&mut input),
        Err(Error::Record(records::Error::Type))
    ));
}

#[test]
fn suppress_mode_skips_fragments_until_one_with_offset_zero_and_then_assembles() {
    let packet = vec![7u8; 6];
    let parts = fragments::split(&packet, 2).unwrap();
    assert_eq!(parts.len(), 3);
    let mut reassembler = Reassembler::default();
    reassembler.suppress();
    assert_eq!(reassembler.accept(0, &parts[1]), Ok(None));
    assert_eq!(reassembler.accept(0, &parts[2]), Ok(None));
    assert!(!reassembler.in_progress());
    assert_eq!(reassembler.accept(0, &parts[0]), Ok(None));
    assert_eq!(reassembler.accept(0, &parts[1]), Ok(None));
    assert_eq!(reassembler.accept(0, &parts[2]), Ok(Some(packet)));
}

#[test]
fn reassert_sends_the_current_camera_and_microphone_state_only_while_media_runs() {
    let (mut controller, _) = Controller::outgoing(ID, false, 0).unwrap();
    controller
        .on_message(&Message::simple(ID, Signal::Accept).unwrap())
        .unwrap();
    controller.media_ready().unwrap();
    controller.set_camera(true).unwrap();
    controller.push_to_talk(false).unwrap();
    assert_eq!(
        controller.reassert().unwrap(),
        vec![
            Message::simple(ID, Signal::CameraOn).unwrap(),
            Message::simple(ID, Signal::MicrophoneMuted).unwrap()
        ]
    );
    let (ringing, _) = Controller::outgoing(ID, false, 0).unwrap();
    assert!(ringing.reassert().unwrap().is_empty());
    controller.hang_up(Reason::Normal).unwrap();
    assert!(controller.reassert().unwrap().is_empty());
}

fn wifi_pair(fragments: bool) -> (CallLink, CallLink) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let mut a_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, al.send_key(), policy).unwrap(),
        HopReader::new(al.receive_key(), policy).unwrap(),
        ae.send_key(),
        ae.receive_key(),
        fragments,
        false,
        false,
    )
    .unwrap();
    let mut b_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, bl.send_key(), policy).unwrap(),
        HopReader::new(bl.receive_key(), policy).unwrap(),
        be.send_key(),
        be.receive_key(),
        fragments,
        false,
        false,
    )
    .unwrap();
    a_link.enable_wifi(&[7; 32], true, false).unwrap();
    b_link.enable_wifi(&[7; 32], false, false).unwrap();
    // The responder's CAPS reached the initiator: it may write BWF1 (R-1).
    a_link.carriers_mut().unwrap().note_peer_bwf1();
    (a_link, b_link)
}

fn attach_pair(a: &mut CallLink, b: &mut CallLink) {
    let ticket = [9u8; 8];
    let id = a.carriers_mut().unwrap().begin_attempt(ticket, 0).unwrap();
    assert_eq!(b.carriers_mut().unwrap().begin_attempt(ticket, 0).unwrap(), id);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let ready = Ready {
        recv_ceiling_kbps: 1100,
        family_cap_kbps: 0,
        transport_budget_kbps: 1100,
    };
    a.carriers_mut().unwrap().attach(
        carriers::Extra {
            id,
            ticket,
            hop_out: Sealer::new(Layer::Hop, &[1; 32], policy).unwrap(),
            hop_in: HopReader::new(&[2; 32], policy).unwrap(),
            peer_ready: ready,
            pulse: Pulse::new(0),
        },
        0,
        true,
    );
    b.carriers_mut().unwrap().attach(
        carriers::Extra {
            id,
            ticket,
            hop_out: Sealer::new(Layer::Hop, &[2; 32], policy).unwrap(),
            hop_in: HopReader::new(&[1; 32], policy).unwrap(),
            peer_ready: ready,
            pulse: Pulse::new(0),
        },
        0,
        true,
    );
}

#[test]
fn with_wifi_enabled_but_no_second_carrier_the_wire_bytes_are_unchanged() {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let make = |send: &[u8; 32], receive: &[u8; 32], es: &[u8; 32], er: &[u8; 32], wifi: bool| {
        let policy = if wifi {
            Policy {
                wifi: true,
                ..Policy::default()
            }
        } else {
            Policy::default()
        };
        let mut link = CallLink::continue_link_with(
            Sealer::new(Layer::Hop, send, policy).unwrap(),
            HopReader::new(receive, policy).unwrap(),
            es,
            er,
            false,
            false,
            false,
        )
        .unwrap();
        if wifi {
            link.enable_wifi(&[7; 32], true, false).unwrap();
        }
        link
    };
    let mut plain_a = make(al.send_key(), al.receive_key(), ae.send_key(), ae.receive_key(), false);
    let mut wifi_a = make(al.send_key(), al.receive_key(), ae.send_key(), ae.receive_key(), true);
    let mut plain_b = make(bl.send_key(), bl.receive_key(), be.send_key(), be.receive_key(), false);
    let mut wifi_b = make(bl.send_key(), bl.receive_key(), be.send_key(), be.receive_key(), true);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let header = media::VideoHeader {
        width: 640,
        height: 360,
        fps: 30,
        codec: 2,
        quarter_turns: 0,
        opus: true,
    };
    let packet = media::Packet {
        flags: media::KEY_FRAME,
        pts_us: 0,
        payload: &[1u8; 40],
    }
    .encode()
    .unwrap();
    let records = |link: &mut CallLink| {
        vec![
            link.signal(&message).unwrap(),
            link.header(&header).unwrap(),
            link.media(&packet).unwrap().concat(),
            link.heartbeat(None).unwrap(),
        ]
    };
    assert_eq!(records(&mut plain_a), records(&mut wifi_a));
    assert_eq!(records(&mut plain_b), records(&mut wifi_b));
}

#[test]
fn content_sealed_while_the_second_carrier_is_the_target_is_hop_sealed_under_its_keys_only() {
    let (mut a, mut b) = wifi_pair(false);
    attach_pair(&mut a, &mut b);
    let id = a.carriers_mut().unwrap().new_carrier();
    a.carriers_mut().unwrap().on_call_active(0, true);
    a.carriers_mut().unwrap().tick(3_000, true);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let wire = a.signal(&message).unwrap();
    assert_eq!(a.target(), Some(id));
    let opened = b
        .carriers_mut()
        .unwrap()
        .extra_mut()
        .unwrap()
        .hop_in
        .read(&mut wire.as_slice(), 4096)
        .unwrap()
        .unwrap();
    assert_eq!(opened.header.kind, 5);
    let mut bluetooth = wire.as_slice();
    assert!(b.hop_in.read(&mut bluetooth, 4096).is_err());
}

#[test]
fn a_stale_content_record_is_dropped_never_an_error() {
    let (mut a, mut b) = wifi_pair(false);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let inner = a.content_out.seal(6, 0, &message.encode().unwrap()).unwrap();
    let first = a.hop_out.seal(5, 0, &inner).unwrap();
    let second = a.hop_out.seal(5, 0, &inner).unwrap();
    let mut input = first.as_slice();
    assert!(matches!(b.read(&mut input), Ok(Some(Incoming::Signal(_)))));
    let mut again = second.as_slice();
    assert_eq!(b.read(&mut again), Ok(None));
    let fresh = a.signal(&Message::simple(ID, Signal::CameraOff).unwrap()).unwrap();
    let mut input = fresh.as_slice();
    assert_eq!(
        b.read(&mut input),
        Ok(Some(Incoming::Signal(Message::simple(ID, Signal::CameraOff).unwrap())))
    );
}

#[test]
fn switch_and_resume_markers_are_consumed_by_the_link_and_never_returned() {
    let (mut a, mut b) = wifi_pair(false);
    let ticket = [9u8; 8];
    b.carriers_mut().unwrap().begin_attempt(ticket, 0).unwrap();
    let marker = a
        .seal_bwf1(0, &Bwf1::Switch(crate::wifi::wire::Switch {
            generation: 1,
            ticket,
            e2e_boundary: 0,
            mesh_boundary: 0,
        }))
        .unwrap();
    let mut input = marker.as_slice();
    assert_eq!(b.read(&mut input), Ok(None));
    let caps = a
        .seal_bwf1(0, &Bwf1::Caps(crate::wifi::wire::Caps::default()))
        .unwrap();
    let mut input = caps.as_slice();
    assert!(matches!(b.read(&mut input), Ok(Some(Incoming::Wifi(_)))));
    let bind = a
        .seal_bwf1(0, &Bwf1::Bind(crate::wifi::wire::Bind {
            role: crate::wifi::wire::role::GUEST,
            ticket,
        }))
        .unwrap();
    let mut input = bind.as_slice();
    assert_eq!(
        b.read(&mut input),
        Ok(Some(Incoming::WifiFault("bwf1_unexpected")))
    );
}

#[test]
fn records_from_a_carrier_that_does_not_exist_are_an_error() {
    let (_a, mut b) = wifi_pair(false);
    assert_eq!(
        b.read_from(5, &mut &[][..], Some(Phase::Active)),
        Err(Error::Carrier(5))
    );
}

#[test]
fn an_unknown_bwf1_kind_is_ignored_and_a_malformed_record_on_bluetooth_ends_only_wifi() {
    let (mut a, mut b) = wifi_pair(false);
    let unknown = a.hop_out.seal(16, 0, b"BWF1\x0c\0").unwrap();
    let mut input = unknown.as_slice();
    assert_eq!(b.read(&mut input), Ok(None));
    let malformed = a.hop_out.seal(16, 0, b"BWF1\x01\0\0\0").unwrap();
    let mut input = malformed.as_slice();
    assert_eq!(
        b.read(&mut input),
        Ok(Some(Incoming::WifiFault("bwf1_malformed")))
    );
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let fresh = a.signal(&message).unwrap();
    let mut input = fresh.as_slice();
    assert!(matches!(b.read(&mut input), Ok(Some(Incoming::Signal(_)))));
}

#[test]
fn a_malformed_bwf1_record_on_the_wifi_carrier_closes_only_that_carrier() {
    let (mut a, mut b) = wifi_pair(false);
    attach_pair(&mut a, &mut b);
    let id = a.carriers_mut().unwrap().new_carrier();
    let malformed = a
        .carriers_mut()
        .unwrap()
        .extra_mut()
        .unwrap()
        .hop_out
        .seal(16, 0, b"BWF1\x01\0\0\0")
        .unwrap();
    let mut input = malformed.as_slice();
    assert_eq!(
        b.read_from(id, &mut input, Some(Phase::Active)),
        Err(Error::Carrier(id))
    );
    let caps = a
        .seal_bwf1(id, &Bwf1::Caps(crate::wifi::wire::Caps::default()))
        .unwrap();
    let mut input = caps.as_slice();
    assert_eq!(
        b.read_from(id, &mut input, Some(Phase::Active)),
        Err(Error::Carrier(id))
    );
}

#[test]
fn a_marker_with_an_unknown_ticket_is_ignored() {
    let (mut a, mut b) = wifi_pair(false);
    let send_on = b.carriers_mut().unwrap().send_on();
    let accept = b.carriers_mut().unwrap().accept();
    let marker = a
        .seal_bwf1(0, &Bwf1::Switch(crate::wifi::wire::Switch {
            generation: 1,
            ticket: [0x77; 8],
            e2e_boundary: 0,
            mesh_boundary: 0,
        }))
        .unwrap();
    let mut input = marker.as_slice();
    assert_eq!(b.read(&mut input), Ok(None));
    assert_eq!(b.carriers_mut().unwrap().send_on(), send_on);
    assert_eq!(b.carriers_mut().unwrap().accept(), accept);
}

#[test]
fn a_ticket_seen_once_gets_no_second_attempt_id() {
    let mut carriers = carriers::Carriers::new(zeroize::Zeroizing::new([0; 32]), true, false);
    let ticket = [3u8; 8];
    let id = carriers.begin_attempt(ticket, 0).unwrap();
    carriers.carrier_died(id, 1);
    assert_eq!(carriers.begin_attempt(ticket, 2), None);
}

#[test]
fn a_record_delivered_late_by_the_hold_is_opened_in_order() {
    let (mut a, mut b) = wifi_pair(false);
    attach_pair(&mut a, &mut b);
    let id = a.carriers_mut().unwrap().new_carrier();
    a.carriers_mut().unwrap().on_call_active(0, true);
    a.carriers_mut().unwrap().tick(3_000, true);
    let message = Message::simple(ID, Signal::CameraOn).unwrap();
    let content = a.signal(&message).unwrap();
    let mut input = content.as_slice();
    assert_eq!(b.read_from(id, &mut input, Some(Phase::Active)), Ok(None));
    let switch = a
        .seal_bwf1(0, &Bwf1::Switch(crate::wifi::wire::Switch {
            generation: 1,
            ticket: [9; 8],
            e2e_boundary: 0,
            mesh_boundary: 0,
        }))
        .unwrap();
    let mut input = switch.as_slice();
    assert_eq!(b.read_from(0, &mut input, Some(Phase::Active)), Ok(Some(Incoming::Signal(message))));
}

#[test]
fn an_attached_carrier_after_a_switch_off_is_closed_by_the_owner_and_never_used() {
    let mut carriers = carriers::Carriers::new(zeroize::Zeroizing::new([0; 32]), true, false);
    let ticket = [5u8; 8];
    let id = carriers.begin_attempt(ticket, 0).unwrap();
    carriers.switch_off(1);
    let mut closes = carriers.take_effects().closes;
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let attached = carriers.attach(
        carriers::Extra {
            id,
            ticket,
            hop_out: Sealer::new(Layer::Hop, &[1; 32], policy).unwrap(),
            hop_in: HopReader::new(&[2; 32], policy).unwrap(),
            peer_ready: Ready {
                recv_ceiling_kbps: 0,
                family_cap_kbps: 0,
                transport_budget_kbps: 0,
            },
            pulse: Pulse::new(0),
        },
        2,
        true,
    );
    assert!(!attached);
    assert_eq!(carriers.extra().map(|extra| extra.id), None);
    assert_eq!(carriers.send_on(), 0);
    // The owner is told exactly once that the carrier is closed by the switch-off.
    closes.extend(carriers.take_effects().closes);
    assert_eq!(
        closes,
        vec![carriers::CarrierEnd {
            id,
            reason: Some(crate::wifi::cutover::CloseReason::SwitchOff)
        }]
    );
}

/// Design SR: a pair of links whose ends opted into renewal as chosen.
fn renewal_link_pair(enable_left: bool, enable_right: bool) -> (CallLink, CallLink) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let mut left = CallLink::new(al.send_key(), al.receive_key(), ae.send_key(), ae.receive_key(), false).unwrap();
    let mut right = CallLink::new(bl.send_key(), bl.receive_key(), be.send_key(), be.receive_key(), false).unwrap();
    if enable_left {
        left.enable_renewal(ae.send_key(), ae.receive_key(), true).unwrap();
    }
    if enable_right {
        right.enable_renewal(be.send_key(), be.receive_key(), true).unwrap();
    }
    (left, right)
}

/// Design SR: a type 12 record round-trips only between links opened for
/// renewal; without it the type is unknown and fails closed on either side.
#[test]
fn a_renew_record_round_trips_only_when_the_link_was_opened_for_renewal() {
    let body = crate::call_renewal::encode(&crate::call_renewal::Op::Offer { ticket: [1; 8], role: 0 });
    let (mut a, mut b) = renewal_link_pair(true, true);
    assert_eq!(a.renewal_key().unwrap().as_slice(), b.renewal_key().unwrap().as_slice(), "role-free key");
    let wire = a.renew(&body).unwrap();
    assert!(matches!(b.read(&mut wire.as_slice()).unwrap(), Some(Incoming::Renew(got)) if got == body));
    // A link that never negotiated renewal cannot even seal one.
    let (mut plain_a, _) = renewal_link_pair(false, false);
    assert!(plain_a.renew(&body).is_err());
    assert!(plain_a.renewal_key().is_none());
    assert!(!plain_a.renewal_direct());
    // A sender that negotiated it to a reader that did not: type 12 is unknown.
    let (mut only_a, mut plain_b) = renewal_link_pair(true, false);
    let wire = only_a.renew(&body).unwrap();
    assert!(plain_b.read(&mut wire.as_slice()).is_err());
}

#[test]
fn the_renewal_key_and_the_direct_fact_are_kept_only_when_negotiated() {
    let (a, b, _) = identities();
    let (ae, _) = handshake(&a, &b, Purpose::Endpoint);
    let mut link = CallLink::new(&[1; 32], &[2; 32], ae.send_key(), ae.receive_key(), false).unwrap();
    assert!(link.renewal_key().is_none());
    link.enable_renewal(ae.send_key(), ae.receive_key(), false).unwrap();
    assert!(link.renewal_key().is_some());
    assert!(!link.renewal_direct(), "the caller's checked fact is kept as given");
    link.enable_renewal(ae.send_key(), ae.receive_key(), true).unwrap();
    assert!(link.renewal_direct());
}

/// SR7: BIND is the new link's first BMS1 record, once, for the current
/// ticket, and names its own end-to-end sequence plus one.
#[test]
fn bind_is_accepted_only_for_its_ticket_with_its_own_next_sequence() {
    use crate::call_renewal::{Op, encode};
    let max = Layer::Hop.maximum_ciphertext();
    let (mut offerer, mut armed) = renewal_link_pair(true, true);
    // Some end-to-end records first, so the BIND is not sequence zero.
    let wire = offerer.keyframe_request().unwrap();
    assert!(matches!(armed.read(&mut wire.as_slice()).unwrap(), Some(Incoming::KeyFrameRequest)));
    let (mut offerer_out, _offerer_in) = offerer.new_hop_layer(&[5; 32], &[6; 32]).unwrap();
    let (_armed_out, mut armed_in) = armed.new_hop_layer(&[6; 32], &[5; 32]).unwrap();
    let own = offerer.next_content_sequence();
    let wire = offerer.seal_bind(&[3; 8], &mut offerer_out).unwrap();
    assert_eq!(offerer.next_content_sequence(), own + 1, "BIND used exactly one sequence");
    let mut input = wire.as_slice();
    let record = armed_in.read(&mut input, max).unwrap().unwrap();
    assert_eq!(record.header.kind, 5);
    assert_eq!(record.header.sequence, 0, "the first record of the new hop layer");
    // Another ticket does not bind.
    let (mut other_offerer, mut other_armed) = renewal_link_pair(true, true);
    let (mut out, _) = other_offerer.new_hop_layer(&[5; 32], &[6; 32]).unwrap();
    let (_, mut hop_in) = other_armed.new_hop_layer(&[6; 32], &[5; 32]).unwrap();
    let wire_other = other_offerer.seal_bind(&[3; 8], &mut out).unwrap();
    let inner = hop_in.read(&mut wire_other.as_slice(), max).unwrap().unwrap().plaintext;
    assert_eq!(other_armed.open_bind(&inner, &[4; 8]), None);
    // The right ticket and the right floor bind.
    assert_eq!(armed.open_bind(&record.plaintext, &[3; 8]), Some(own + 1));
    // A BIND naming the wrong floor never binds.
    let (mut liar, mut victim) = renewal_link_pair(true, true);
    let (mut liar_out, _) = liar.new_hop_layer(&[5; 32], &[6; 32]).unwrap();
    let (_, mut victim_in) = victim.new_hop_layer(&[6; 32], &[5; 32]).unwrap();
    let wrong = encode(&Op::Bind { ticket: [3; 8], next_sequence: 9 });
    let inner = liar.test_seal_endpoint(12, 0, &wrong).unwrap();
    let hop = liar_out.seal(5, 0, &inner).unwrap();
    let inner = victim_in.read(&mut hop.as_slice(), max).unwrap().unwrap().plaintext;
    assert_eq!(victim.open_bind(&inner, &[3; 8]), None, "floor must be the BIND's own sequence + 1");
    // A first record that is not a BIND never binds.
    let (mut chatty, mut peer) = renewal_link_pair(true, true);
    let (mut out, _) = chatty.new_hop_layer(&[5; 32], &[6; 32]).unwrap();
    let (_, mut hop_in) = peer.new_hop_layer(&[6; 32], &[5; 32]).unwrap();
    let inner = chatty.test_seal_endpoint(12, 0, &encode(&Op::Offer { ticket: [3; 8], role: 0 })).unwrap();
    let hop = out.seal(5, 0, &inner).unwrap();
    let inner = hop_in.read(&mut hop.as_slice(), max).unwrap().unwrap().plaintext;
    assert_eq!(peer.open_bind(&inner, &[3; 8]), None);
    // A link without renewal never opens one.
    let (mut plain, _) = renewal_link_pair(false, false);
    assert_eq!(plain.open_bind(&record.plaintext, &[3; 8]), None);
}

/// A record sealed after BIND (a Wi-Fi carrier's) can reach the peer first.
/// The BIND is then refused, and the end-to-end session is not faulted by it.
#[test]
fn an_overtaken_bind_is_refused_without_faulting_the_end_to_end_session() {
    let max = Layer::Hop.maximum_ciphertext();
    let (mut offerer, mut armed) = renewal_link_pair(true, true);
    let (mut out, _) = offerer.new_hop_layer(&[5; 32], &[6; 32]).unwrap();
    let (_, mut hop_in) = armed.new_hop_layer(&[6; 32], &[5; 32]).unwrap();
    let bind_wire = offerer.seal_bind(&[3; 8], &mut out).unwrap();
    let later = offerer.keyframe_request().unwrap();
    // The later record arrives first.
    assert!(matches!(armed.read(&mut later.as_slice()).unwrap(), Some(Incoming::KeyFrameRequest)));
    let inner = hop_in.read(&mut bind_wire.as_slice(), max).unwrap().unwrap().plaintext;
    assert_eq!(armed.open_bind(&inner, &[3; 8]), None, "the BIND's sequence was overtaken");
    // The end-to-end session is intact: the next record still opens.
    let next = offerer.keyframe_request().unwrap();
    assert!(matches!(armed.read(&mut next.as_slice()).unwrap(), Some(Incoming::KeyFrameRequest)));
}
