use super::{fragments::*, *};
use crate::{
    bmh2::Purpose,
    session::tests::{handshake, hex, identities},
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
