use super::*;
use crate::{
    bmh2::{Purpose, SessionKeys},
    framing::HopReader,
    link,
    records::{Layer, Opener, Policy},
    session::{
        SourceControl, ViewerUplink,
        tests::{handshake, identities},
    },
    watch::{Event, Reader},
};
const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;

struct Viewer {
    reader: Reader,
    uplink: ViewerUplink,
    control: SourceControl,
}
fn keys() -> (SessionKeys, SessionKeys, SessionKeys, SessionKeys, [u8; 16]) {
    let (source, viewer, node) = identities();
    let (sl, vl) = handshake(&source, &viewer, Purpose::Link);
    let (se, ve) = handshake(&source, &viewer, Purpose::Endpoint);
    (sl, se, vl, ve, node)
}
fn child(source: &mut Source<u8>, key: u8, now: u64) -> Result<Viewer, Error> {
    let (sl, se, vl, ve, node) = keys();
    source.admit(
        key,
        SourceLink::from_keys(&sl, &se).unwrap(),
        300,
        None,
        now,
    )?;
    Ok(Viewer {
        reader: Reader::direct(
            HopReader::new(vl.receive_key(), Policy::default()).unwrap(),
            Opener::new(Layer::Endpoint, ve.receive_key(), Policy::default()).unwrap(),
            node,
            source.header.bytes.starts_with(b"BTL1"),
            true,
        ),
        uplink: ViewerUplink::from_keys(&vl, &ve).unwrap(),
        control: SourceControl::from_keys(&sl, &se).unwrap(),
    })
}
fn header() -> Vec<u8> {
    media::VideoHeader {
        width: 426,
        height: 240,
        fps: 15,
        codec: 2,
        quarter_turns: 0,
        opus: true,
    }
    .encode()
    .unwrap()
    .to_vec()
}
fn packet(flags: u8, pts_us: i64, len: usize) -> Vec<u8> {
    media::Packet {
        flags,
        pts_us,
        payload: &vec![flags.wrapping_add(1); len],
    }
    .encode()
    .unwrap()
}
/// Runs `step` for one child until Idle, collecting the viewer's events; the
/// clock advances on every pacer wait.
fn drain(
    source: &mut Source<u8>,
    key: u8,
    viewer: &mut Viewer,
    now: &mut u64,
) -> (Vec<Event>, u64) {
    let mut events = Vec::new();
    let mut waited = 0;
    loop {
        match source.step(&key, *now).unwrap() {
            Step::Idle => break,
            Step::Wait(ns) => {
                waited += ns;
                *now += ns;
            }
            Step::Write(bytes) => {
                source.written(&key, bytes.len(), *now, *now);
                let mut input = bytes.as_slice();
                // One record per write; End closes the reader afterwards.
                while !input.is_empty() {
                    match viewer.reader.read(&mut input, 0).unwrap() {
                        Some(event) => events.push(event),
                        None => break,
                    }
                }
            }
            other => panic!("{other:?}"),
        }
    }
    (events, waited)
}
fn summary(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            Event::Header(_) => "header".to_owned(),
            Event::Media(p) => {
                let p = p.packet();
                format!("media {:#04x} {} {}", p.flags, p.pts_us, p.payload.len())
            }
            Event::State(s) => format!("state {s}"),
            Event::Withdrawn => "withdrawn".to_owned(),
        })
        .collect()
}

#[test]
fn audio_pairs_round_trip_through_the_receivers_batch_split() {
    let a = media::Packet {
        flags: AUDIO,
        pts_us: 0,
        payload: &[1; 30],
    };
    let b = media::Packet {
        flags: AUDIO,
        pts_us: 20_000,
        payload: &[2; 31],
    };
    let raw = pair_audio(a, b).unwrap();
    let units = media::Packet::decode(&raw, media::MAX_PAYLOAD)
        .unwrap()
        .split_audio()
        .unwrap();
    assert!(units == vec![a, b]);
    assert!(pair_audio(b, a).is_err(), "time goes backwards");
    let c = media::Packet {
        flags: AUDIO,
        pts_us: 1_000_001,
        payload: &[3],
    };
    assert!(pair_audio(a, c).is_err(), "gap above 1 s");
    let config = media::Packet {
        flags: AUDIO | media::CONFIG,
        ..a
    };
    assert!(pair_audio(config, b).is_err());
}

#[test]
fn going_live_fans_one_encode_to_paced_viewers_and_joiners_start_at_a_keyframe() {
    let mut now = S;
    let mut source = Source::new(header(), 1_000).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    assert_eq!(
        source.take_keyframe_requests(),
        KeyframeRequests {
            join: true,
            recovery: false
        }
    );
    assert!(!source.take_keyframe_request());
    source.publish(packet(media::CONFIG, 0, 3), now).unwrap();
    source
        .publish(packet(media::KEY_FRAME, 0, 6_000), now)
        .unwrap();
    source.publish(packet(AUDIO, 0, 40), now).unwrap();
    source.publish(packet(AUDIO, 20_000, 40), now).unwrap();
    source.publish(packet(0, 66_000, 3_000), now).unwrap();
    let (events, waited) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(
        summary(&events),
        vec![
            "header",
            "media 0x01 0 3",
            "media 0x02 0 6000",
            "media 0x48 0 92",
            "media 0x00 66000 3000"
        ]
    );
    // Seeded at the 350 kbps GATT prior (52 500 B/s): 6 KB of keyframe plus the
    // audio pair holds the delta frame for the link time beyond the 50 ms burst.
    assert!(waited >= 50 * MS, "waited {waited}");
    // A second viewer joins mid-stream: header and current config, then the next keyframe.
    let mut b = child(&mut source, 2, now).unwrap();
    assert!(source.take_keyframe_request());
    source.publish(packet(0, 133_000, 500), now).unwrap();
    source
        .publish(packet(media::KEY_FRAME, 200_000, 900), now)
        .unwrap();
    let (events, _) = drain(&mut source, 2, &mut b, &mut now);
    assert_eq!(
        summary(&events),
        vec!["header", "media 0x01 0 3", "media 0x02 200000 900"]
    );
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events).len(), 2);
}

#[test]
fn reports_pace_the_link_keyframe_requests_reach_the_encoder_and_admission_is_bounded() {
    let now = S;
    let mut source = Source::new(header(), 700).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    source.take_keyframe_request();
    let report = link::Report {
        arrival_kbps: 800,
        link_late: false,
        added_delay_ms: 0,
        delivery: None,
        delivery_only: false,
    };
    let wire = [
        a.uplink.heartbeat(Some(&report)).unwrap(),
        a.uplink.keyframe_request().unwrap(),
    ]
    .concat();
    let mut input = wire.as_slice();
    while let Some(control) = a.control.read(&mut input).unwrap() {
        source.on_control(&1, control, now).unwrap();
    }
    // A viewer's request is recovery (backoff applies), not a join.
    assert_eq!(
        source.take_keyframe_requests(),
        KeyframeRequests {
            join: false,
            recovery: true
        }
    );
    // 800 kbps measured: 120 000 B/s, so 12 KB is 100 ms, 50 ms of it allowed ahead.
    source
        .publish(packet(media::KEY_FRAME, 0, 12_000 - 13), now)
        .unwrap();
    source.publish(packet(0, 66_000, 100), now).unwrap();
    let Step::Write(first) = source.step(&1, now).unwrap() else {
        panic!()
    };
    source.written(&1, 12_000, now, now);
    assert!(matches!(source.step(&1, now).unwrap(), Step::Wait(_)));
    assert!(matches!(
        source.step(&1, now + 50 * MS).unwrap(),
        Step::Write(_)
    ));
    assert!(!first.is_empty());
    // 300 + 300 fits 700; a third 300 does not.
    child(&mut source, 2, now).unwrap();
    assert_eq!(child(&mut source, 3, now).err(), Some(Error::NotAdmitted));
    assert_eq!(source.children(), 2);
    assert_eq!(
        source.on_control(&9, Control::KeyFrameRequest, now),
        Err(Error::UnknownChild)
    );
}

#[test]
fn pause_resume_and_end_follow_the_lifecycle_rules() {
    let mut now = S;
    let mut source = Source::new(header(), 1_000).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    source
        .publish(packet(media::KEY_FRAME, 0, 100), now)
        .unwrap();
    source.publish(packet(0, 66_000, 100), now).unwrap();
    source.pause(now);
    // Queued media cleared, PAUSED queued; media while paused is not sent.
    source.publish(packet(0, 133_000, 100), now).unwrap();
    source.tick(now + PAUSED_REPEAT_NS);
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events), vec!["header", "state 2", "state 2"]);
    // A viewer joining while paused learns the state from the preamble.
    let mut b = child(&mut source, 2, now).unwrap();
    let (events, _) = drain(&mut source, 2, &mut b, &mut now);
    assert_eq!(summary(&events), vec!["header", "state 2"]);
    source.take_keyframe_request();
    source.resume(now);
    assert!(source.take_keyframe_requests().join);
    source
        .publish(packet(media::KEY_FRAME, 200_000, 100), now)
        .unwrap();
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events), vec!["state 1", "media 0x02 200000 100"]);
    // End: ENDED is delivered, the acknowledged child closes, the other after 5 s.
    source.publish(packet(0, 266_000, 100), now).unwrap();
    source.end(now);
    assert_eq!(source.publish(packet(0, 0, 1), now), Err(Error::Ended));
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events), vec!["state 3"]);
    let ack = a.uplink.end_ack().unwrap();
    let mut input = ack.as_slice();
    let control = a.control.read(&mut input).unwrap().unwrap();
    source.on_control(&1, control, now).unwrap();
    assert_eq!(source.step(&1, now).unwrap(), Step::Close);
    let (events, _) = drain(&mut source, 2, &mut b, &mut now);
    assert!(summary(&events).ends_with(&["state 3".to_owned()]));
    assert_eq!(source.step(&2, now + END_GRACE_NS).unwrap(), Step::Close);
    assert_eq!(source.children(), 0);
}

#[test]
fn camera_off_keeps_started_video_and_audio_but_discards_unstarted_video_until_a_fresh_keyframe() {
    let mut now = S;
    let mut source = Source::new(header(), 1_000).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    let mut b = child(&mut source, 2, now).unwrap();
    drain(&mut source, 1, &mut a, &mut now);
    drain(&mut source, 2, &mut b, &mut now);
    source.publish(packet(media::KEY_FRAME, 0, 100), now).unwrap();
    source.publish(packet(0, 40_000, 90), now).unwrap();
    source.publish(packet(AUDIO, 0, 8), now).unwrap();
    source.publish(packet(AUDIO, 20_000, 8), now).unwrap();
    let Step::Write(started) = source.step(&1, now).unwrap() else { panic!("First frame must start"); };
    source.written(&1, started.len(), now, now);
    source.discard_video();
    source.set_media_state(false, true, now);
    assert!(matches!(a.reader.read(&mut started.as_slice(), 0).unwrap(), Some(Event::Media(_))), "a sealed frame finishes in order");
    for (key, viewer) in [(1, &mut a), (2, &mut b)] {
        let (events, _) = drain(&mut source, key, viewer, &mut now);
        assert_eq!(summary(&events), ["media 0x48 0 28", "state 5"], "only audio and Camera off follow; neither child receives queued old video");
    }
    assert!(source.take_keyframe_requests().join, "a reopened encoder must start fresh");
    source.set_media_state(false, false, now);
    source.publish(packet(0, 80_000, 90), now).unwrap();
    source.publish(packet(media::KEY_FRAME, 120_000, 110), now).unwrap();
    for (key, viewer) in [(1, &mut a), (2, &mut b)] {
        let (events, _) = drain(&mut source, key, viewer, &mut now);
        assert_eq!(summary(&events), ["state 1", "media 0x02 120000 110"], "Camera on cannot resume with an old dependent picture");
    }
    source.end(now);
    for (key, viewer) in [(1, &mut a), (2, &mut b)] {
        let (events, _) = drain(&mut source, key, viewer, &mut now);
        assert_eq!(summary(&events), ["state 3"]);
        let ack = viewer.uplink.end_ack().unwrap();
        source.on_control(&key, viewer.control.read(&mut ack.as_slice()).unwrap().unwrap(), now).unwrap();
        assert_eq!(source.step(&key, now).unwrap(), Step::Close);
    }
}

#[test]
fn a_child_that_stays_behind_is_withdrawn_and_a_silent_one_is_closed() {
    let now = S;
    let mut source = Source::new(header(), 1_000).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    child(&mut source, 2, now).unwrap();
    source
        .publish(packet(media::KEY_FRAME, 0, 100), now)
        .unwrap();
    // Child 1 never drains for 6 s: withdrawn, the stream continues for child 2.
    let later = now + 6 * S;
    source
        .on_control(&1, Control::Heartbeat(None), later)
        .unwrap();
    source
        .on_control(&2, Control::Heartbeat(None), later)
        .unwrap();
    let Step::Withdraw(bytes) = source.step(&1, later).unwrap() else {
        panic!()
    };
    let mut input = bytes.as_slice();
    assert!(matches!(
        a.reader.read(&mut input, 0),
        Ok(Some(Event::Withdrawn))
    ));
    assert!(matches!(source.step(&1, later + S).unwrap(), Step::Wait(_)));
    let ack = a.uplink.withdrawal_ack().unwrap();
    let mut input = ack.as_slice();
    let control = a.control.read(&mut input).unwrap().unwrap();
    source.on_control(&1, control, later + S).unwrap();
    assert_eq!(source.step(&1, later + S).unwrap(), Step::Close);
    // Its budget is released: a new 300 kbps child fits again.
    assert_eq!(source.children(), 1);
    // Child 2 sends nothing for 8 s: dead, closed.
    assert_eq!(source.step(&2, later + 8 * S).unwrap(), Step::Close);
    let mut c = child(&mut source, 3, later + 8 * S).unwrap();
    let _ = &mut c;
    assert_eq!(source.children(), 1);
}

#[test]
fn delivery_only_reports_keep_the_viewer_alive_without_retuning_video_pacing() {
    for delivery_only in [true, false] {
        let mut source = Source::new(header(), 700).unwrap();
        let mut viewer = child(&mut source, 1, S).unwrap();
        let mut now = S;
        let (events, _) = drain(&mut source, 1, &mut viewer, &mut now);
        assert_eq!(summary(&events), vec!["header"]);
        let measured = link::Report {
            arrival_kbps: 200,
            link_late: false,
            added_delay_ms: 0,
            delivery: None,
            delivery_only: false,
        };
        let delivery = link::Report {
            arrival_kbps: 800,
            delivery: Some(link::Delivery {
                received_bytes: 100,
                clock_ms: 9_000,
            }),
            delivery_only,
            ..measured
        };
        // The second authenticated heartbeat arrives after the original
        // liveness deadline. Both forms must keep the viewer connected.
        for (at, report) in [(S, measured), (10 * S, delivery)] {
            let wire = viewer.uplink.heartbeat(Some(&report)).unwrap();
            let control = viewer.control.read(&mut wire.as_slice()).unwrap().unwrap();
            source.on_control(&1, control, at).unwrap();
        }
        let mut now = 10 * S;
        source
            .publish(packet(media::KEY_FRAME, 0, 12_000), now)
            .unwrap();
        source.publish(packet(0, 66_000, 100), now).unwrap();
        let (events, waited) = drain(&mut source, 1, &mut viewer, &mut now);
        assert_eq!(
            summary(&events),
            vec!["media 0x02 0 12000", "media 0x00 66000 100"]
        );
        if delivery_only {
            assert!(waited >= 300 * MS, "delivery-only changed the 200 kbps pace: {waited}");
        } else {
            assert!(waited < 100 * MS, "a full report did not apply the 800 kbps pace: {waited}");
        }
    }
}

#[test]
fn video_mute_flushes_pending_audio_and_keeps_video_configuration_and_resume_working() {
    let mut now = S;
    let mut source = Source::new(header(), 700).unwrap();
    let mut viewer = child(&mut source, 1, now).unwrap();
    let mut opus = crate::opus::Encoder::new().unwrap();
    let config = opus.config();
    let before = opus.encode(&[1_000; 960]).unwrap();
    let muted = opus.encode(&[0; 960]).unwrap();
    let after = opus.encode(&[-1_000; 960]).unwrap();
    let next = opus.encode(&[2_000; 960]).unwrap();
    let audio = |flags, pts_us, payload: &[u8]| media::Packet { flags, pts_us, payload }.encode().unwrap();
    source.publish(audio(AUDIO | media::CONFIG, 0, &config), now).unwrap();
    source.publish(audio(AUDIO, 20_000, &before), now).unwrap();
    source.mute_audio(true, now);
    source.publish(audio(AUDIO, 40_000, &muted), now).unwrap();
    source.publish(audio(AUDIO | media::CONFIG, 0, &config), now).unwrap();
    source.publish(packet(media::KEY_FRAME, 0, 100), now).unwrap();
    let (events, _) = drain(&mut source, 1, &mut viewer, &mut now);
    let mut heard = Vec::new();
    let mut config_received = false; let mut video_received = false;
    for event in &events {
        if let Event::Media(p) = event {
            let p = p.packet();
            if p.flags == AUDIO { heard.push((p.pts_us, p.payload.to_vec())); }
            if p.flags == (AUDIO | media::CONFIG) { config_received = p.payload == config.as_slice(); }
            if p.flags == media::KEY_FRAME { video_received = true; }
        }
    }
    assert_eq!(heard, [(20_000, before)], "only the pending pre-mute unit is flushed");
    assert!(config_received && video_received, "video and audio setup continue while muted");
    source.pause(now); source.resume(now);
    source.publish(audio(AUDIO, 80_000, &muted), now).unwrap();
    let (events, _) = drain(&mut source, 1, &mut viewer, &mut now);
    assert_eq!(summary(&events), ["state 2", "state 1"], "Pause/Resume must preserve mute");
    source.mute_audio(false, now);
    source.publish(audio(AUDIO, 140_000, &after), now).unwrap();
    source.publish(audio(AUDIO, 160_000, &next), now).unwrap();
    let (events, _) = drain(&mut source, 1, &mut viewer, &mut now);
    let [Event::Media(pair)] = events.as_slice() else { panic!("one resumed audio pair expected") };
    let units = pair.packet().split_audio().unwrap();
    assert_eq!(units.iter().map(|p| (p.pts_us, p.payload.to_vec())).collect::<Vec<_>>(),
        [(140_000, after), (160_000, next)], "no pair straddles the mute interval");
}

#[test]
fn radio_mute_removes_unsent_signed_audio_for_each_listener_and_preserves_setup_and_lifecycle() {
    let mut now = S;
    let (identity, _, _) = identities();
    let mut signer = crate::audio_live::Signer::new(&identity, [0; 16], 2).unwrap();
    let mut source = Source::new(signer.header().to_vec(), 1_000).unwrap();
    let mut opus = crate::opus::Encoder::radio(2).unwrap();
    let config = opus.config();
    source.publish(signer.packet(media::Packet { flags: AUDIO | media::CONFIG, pts_us: 0, payload: &config }).unwrap(), now).unwrap();
    let mut viewers = [child(&mut source, 1, now).unwrap(), child(&mut source, 2, now).unwrap()];
    source.pause(now); source.resume(now);
    let tone = std::array::from_fn(|i| if i % 48 < 24 { 6_000 } else { -6_000 });
    let mut batcher = crate::audio_live::Batcher::new(false);
    batcher.push(0, opus.encode(&tone).unwrap()).unwrap();
    let old = batcher.push(20_000, opus.encode(&tone).unwrap()).unwrap().unwrap();
    source.publish(signer.packet(media::Packet::decode(&old, media::MAX_PAYLOAD).unwrap()).unwrap(), now).unwrap();
    batcher.mute(true); source.discard_radio_audio();
    batcher.push(40_000, opus.encode(&tone).unwrap()).unwrap();
    let silent = batcher.push(60_000, opus.encode(&tone).unwrap()).unwrap().unwrap();
    source.publish(signer.packet(media::Packet::decode(&silent, media::MAX_PAYLOAD).unwrap()).unwrap(), now).unwrap();
    for (n, viewer) in viewers.iter_mut().enumerate() {
        let (events, _) = drain(&mut source, n as u8 + 1, viewer, &mut now);
        assert_eq!(events.len(), 5, "header, config, Pause, Resume and the current silent batch remain");
        assert!(matches!(events[0], Event::Header(_)));
        let Event::Media(setup) = &events[1] else { panic!("audio configuration") };
        assert_eq!((setup.packet().flags, setup.packet().payload), (AUDIO | media::CONFIG, config.as_slice()));
        assert_eq!(&summary(&events)[2..4], ["state 2", "state 1"]);
        let Event::Media(batch) = &events[4] else { panic!("signed silent audio") };
        let units = batch.packet().split_audio().unwrap();
        assert_eq!(units.iter().map(|p| p.pts_us).collect::<Vec<_>>(), [40_000, 60_000]);
        let mut decoder = crate::opus::Decoder::new(&config, crate::opus::Profile::AudioOnly20ms).unwrap();
        for unit in units {
            let mut pcm = [0; 960];
            assert!(decoder.decode(unit.payload, &mut pcm).unwrap().samples.iter().all(|s| *s == 0), "pre-mute audio must never reach either listener");
        }
    }
    batcher.mute(false);
    batcher.push(80_000, opus.encode(&tone).unwrap()).unwrap();
    let fresh = batcher.push(100_000, opus.encode(&tone).unwrap()).unwrap().unwrap();
    source.publish(signer.packet(media::Packet::decode(&fresh, media::MAX_PAYLOAD).unwrap()).unwrap(), now).unwrap();
    for (n, viewer) in viewers.iter_mut().enumerate() {
        let (events, _) = drain(&mut source, n as u8 + 1, viewer, &mut now);
        let [Event::Media(batch)] = events.as_slice() else { panic!("fresh unmuted batch") };
        assert!(batch.packet().split_audio().unwrap().iter().any(|p| p.payload != [0xf8, 0xff, 0xfe]));
    }
    source.end(now); source.discard_radio_audio();
    for (n, viewer) in viewers.iter_mut().enumerate() {
        assert_eq!(summary(&drain(&mut source, n as u8 + 1, viewer, &mut now).0), ["state 3"]);
    }
}

#[test]
fn signed_single_radio_units_keep_source_authentication_for_each_viewer() {
    for preserve in [false, true] {
        let mut now = S;
        let (identity, _, _) = identities();
        let mut signer = crate::audio_live::Signer::new(&identity, [0; 16], 2).unwrap();
        let mut source = Source::new(signer.header().to_vec(), 1_000).unwrap();
        source.preserve_signed_audio = preserve;
        let opus = crate::opus::Encoder::radio(2).unwrap();
        let config = opus.config();
        source.publish(signer.packet(media::Packet { flags: AUDIO | media::CONFIG, pts_us: 0, payload: &config }).unwrap(), now).unwrap();
        let mut viewers = [child(&mut source, 1, now).unwrap(), child(&mut source, 2, now).unwrap()];
        for (n, viewer) in viewers.iter_mut().enumerate() {
            let events = drain(&mut source, n as u8 + 1, viewer, &mut now).0;
            assert!(matches!(events.as_slice(), [Event::Header(_), Event::Media(_)]));
        }
        for pts in [0, 20_000] {
            source.publish(signer.packet(media::Packet { flags: AUDIO, pts_us: pts, payload: &[0xf8, 0xff, 0xfe] }).unwrap(), now).unwrap();
            for (n, viewer) in viewers.iter_mut().enumerate() {
                let key = n as u8 + 1;
                if !preserve && pts == 0 {
                    assert_eq!(source.step(&key, now).unwrap(), Step::Idle, "the old arm buffers the first signed unit");
                    continue;
                }
                if !preserve {
                    let Step::Write(bytes) = source.step(&key, now).unwrap() else { panic!("repacked record") };
                    assert!(viewer.reader.read(&mut bytes.as_slice(), 0).is_err(), "repacking destroys source authentication");
                    continue;
                }
                let events = drain(&mut source, key, viewer, &mut now).0;
                let [Event::Media(unit)] = events.as_slice() else { panic!("one authenticated single unit") };
                assert_eq!((unit.packet().flags, unit.packet().pts_us), (AUDIO, pts));
                let mut decoder = crate::opus::Decoder::new(&config, crate::opus::Profile::AudioOnly20ms).unwrap();
                let mut pcm = [0; 960];
                let decoded = decoder.decode(unit.packet().payload, &mut pcm).unwrap();
                assert!(!decoded.samples.is_empty() && decoded.samples.iter().all(|s| *s == 0));
            }
        }
        if preserve {
            source.end(now);
            for (n, viewer) in viewers.iter_mut().enumerate() {
                let key = n as u8 + 1;
                assert_eq!(summary(&drain(&mut source, key, viewer, &mut now).0), ["state 3"]);
                let ack = viewer.uplink.end_ack().unwrap();
                let control = viewer.control.read(&mut ack.as_slice()).unwrap().unwrap();
                source.on_control(&key, control, now).unwrap();
                assert_eq!(source.step(&key, now).unwrap(), Step::Close);
            }
        }
    }
}

fn radio_policy_source(age_shed: bool) -> (Source<u8>, crate::audio_live::Signer, Vec<u8>) {
    let (identity, _, _) = identities();
    let mut signer = crate::audio_live::Signer::new(&identity, [0; 16], 2).unwrap();
    let mut source = Source::new(signer.header().to_vec(), 1_000).unwrap();
    source.radio_queue = true;
    source.radio_age_shed = age_shed;
    source.preserve_signed_audio = true;
    let config = crate::opus::Encoder::radio(2).unwrap().config();
    source.publish(signer.packet(media::Packet { flags: AUDIO | media::CONFIG, pts_us: 0, payload: &config }).unwrap(), S).unwrap();
    (source, signer, config)
}
fn publish_radio(source: &mut Source<u8>, signer: &mut crate::audio_live::Signer, pts: i64, now: u64) {
    source.publish(signer.packet(media::Packet { flags: AUDIO, pts_us: pts, payload: &[0xf8, 0xff, 0xfe] }).unwrap(), now).unwrap();
}

#[test]
fn radio_listener_recovers_across_whole_signed_gaps_without_affecting_the_fast_listener() {
    let (mut source, mut signer, config) = radio_policy_source(true);
    let mut viewers = [child(&mut source, 1, S).unwrap(), child(&mut source, 2, S).unwrap()];
    let mut now = S;
    for (index, viewer) in viewers.iter_mut().enumerate() {
        assert!(matches!(drain(&mut source, index as u8 + 1, viewer, &mut now).0.as_slice(), [Event::Header(_), Event::Media(_)]));
    }
    // A brief stalled listener should recover; most of its recent records
    // were delivered, so a small age shed is below Android's overload share.
    for n in 0..20 {
        publish_radio(&mut source, &mut signer, n * 20_000, now);
        for (index, viewer) in viewers.iter_mut().enumerate() {
            assert!(matches!(drain(&mut source, index as u8 + 1, viewer, &mut now).0.as_slice(), [Event::Media(_)]));
        }
    }
    let base = now;
    source.pause(now); source.resume(now);
    assert_eq!(summary(&drain(&mut source, 1, &mut viewers[0], &mut now).0), ["state 2", "state 1"]);
    for (index, offset) in [0, 1_000, 1_501, 3_001].into_iter().enumerate() {
        now = base + offset * MS;
        publish_radio(&mut source, &mut signer, (20 + index as i64) * 20_000, now);
        let events = drain(&mut source, 1, &mut viewers[0], &mut now).0;
        let [Event::Media(unit)] = events.as_slice() else { panic!("fast listener retains every signed record") };
        assert_eq!(unit.packet().pts_us, (20 + index as i64) * 20_000);
    }
    assert_eq!(source.take_radio_shed(), 2);
    assert_eq!(source.take_radio_shed(), 0, "each removal is reported once");
    let recovered = drain(&mut source, 2, &mut viewers[1], &mut now).0;
    assert_eq!(&summary(&recovered)[..2], ["state 2", "state 1"], "lifecycle is never shed");
    let mut decoder = crate::opus::Decoder::new(&config, crate::opus::Profile::AudioOnly20ms).unwrap();
    for (event, expected_pts) in recovered[2..].iter().zip([440_000, 460_000]) {
        let Event::Media(unit) = event else { panic!("verified audio after the signed sequence gap") };
        assert_eq!(unit.packet().pts_us, expected_pts);
        let mut pcm = [0; 960];
        assert!(decoder.decode(unit.packet().payload, &mut pcm).unwrap().samples.iter().all(|s| *s == 0));
    }
    assert_eq!(recovered.len(), 4);
    source.end(now);
    for (index, viewer) in viewers.iter_mut().enumerate() {
        let key = index as u8 + 1;
        assert_eq!(summary(&drain(&mut source, key, viewer, &mut now).0), ["state 3"]);
        let ack = viewer.uplink.end_ack().unwrap();
        source.on_control(&key, viewer.control.read(&mut ack.as_slice()).unwrap().unwrap(), now).unwrap();
        assert_eq!(source.step(&key, now).unwrap(), Step::Close);
    }
}

#[test]
fn excessive_radio_shedding_withdraws_only_that_authenticated_listener_and_releases_its_budget() {
    let (mut source, mut signer, _) = radio_policy_source(true);
    let mut slow = child(&mut source, 1, S).unwrap();
    let mut fast = child(&mut source, 2, S).unwrap();
    let mut now = S;
    drain(&mut source, 1, &mut slow, &mut now);
    drain(&mut source, 2, &mut fast, &mut now);
    let base = now;
    for n in 0..10 {
        now = base + if n == 9 { 3_001 * MS } else { n * 20 * MS };
        publish_radio(&mut source, &mut signer, n as i64 * 20_000, now);
        assert!(matches!(drain(&mut source, 2, &mut fast, &mut now).0.as_slice(), [Event::Media(_)]));
    }
    assert_eq!(source.take_radio_shed(), 9);
    // The overload verdict stays latched even after its ten-second window.
    now += 11 * S;
    let beat = slow.uplink.heartbeat(None).unwrap();
    source.on_control(&1, slow.control.read(&mut beat.as_slice()).unwrap().unwrap(), now).unwrap();
    let Step::Withdraw(bytes) = source.step(&1, now).unwrap() else { panic!("radio capacity withdrawal") };
    assert!(matches!(slow.reader.read(&mut bytes.as_slice(), 0).unwrap(), Some(Event::Withdrawn)));
    let ack = slow.uplink.withdrawal_ack().unwrap();
    source.on_control(&1, slow.control.read(&mut ack.as_slice()).unwrap().unwrap(), now).unwrap();
    assert_eq!(source.step(&1, now).unwrap(), Step::Close);
    assert_eq!(source.children(), 1);
    child(&mut source, 3, now).unwrap();
    child(&mut source, 4, now).unwrap();
    assert_eq!(child(&mut source, 5, now).err(), Some(Error::NotAdmitted));
    let beat = fast.uplink.heartbeat(None).unwrap();
    source.on_control(&2, fast.control.read(&mut beat.as_slice()).unwrap().unwrap(), now).unwrap();
    publish_radio(&mut source, &mut signer, 200_000, now);
    assert!(matches!(drain(&mut source, 2, &mut fast, &mut now).0.as_slice(), [Event::Media(_)]));
    source.end(now);
    assert_eq!(summary(&drain(&mut source, 2, &mut fast, &mut now).0), ["state 3"]);
}

#[test]
fn radio_no_shed_control_keeps_audio_until_the_independent_age_or_memory_guard_withdraws() {
    for (age_ms, overload_bytes, withdraw) in [(6_001, false, false), (30_000, false, false), (30_001, false, true), (0, true, true)] {
        let (mut source, mut signer, _) = radio_policy_source(false);
        let mut viewer = child(&mut source, 1, S).unwrap();
        let mut now = S;
        drain(&mut source, 1, &mut viewer, &mut now);
        let base = now;
        publish_radio(&mut source, &mut signer, 0, now);
        if overload_bytes {
            // Model an oversized producer backlog: the queue must withdraw
            // before attempting to seal or send any of these extra bytes.
            let queue = source.fan.queue(&1).unwrap();
            queue.offer(Item::new(vec![0; RadioHealth::MAX_BYTES], crate::fanout::Class::control()), now);
        }
        now = base + age_ms * MS;
        let beat = viewer.uplink.heartbeat(None).unwrap();
        source.on_control(&1, viewer.control.read(&mut beat.as_slice()).unwrap().unwrap(), now).unwrap();
        match source.step(&1, now).unwrap() {
            Step::Withdraw(bytes) if withdraw => {
                assert!(matches!(viewer.reader.read(&mut bytes.as_slice(), 0).unwrap(), Some(Event::Withdrawn)));
            }
            Step::Write(bytes) if !withdraw => {
                let Some(Event::Media(unit)) = viewer.reader.read(&mut bytes.as_slice(), 0).unwrap() else { panic!("unshed authenticated audio") };
                assert_eq!(unit.packet().pts_us, 0);
            }
            other => panic!("age={age_ms} memory={overload_bytes}: {other:?}"),
        }
        assert_eq!(source.take_radio_shed(), 0);
    }
}

#[test]
fn optional_video_notices_replay_whole_state_and_follow_pause_resume_and_end_in_order() {
    let mut now = S;
    let mut source = Source::new(header(), 1_000).unwrap();
    source.set_media_state(true, true, now);
    source.publish(packet(media::CONFIG, 0, 3), now).unwrap();
    let mut a = child(&mut source, 1, now).unwrap();
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events), ["header", "state 6", "media 0x01 0 3"]);
    let notice = crate::stream_watch::PlaybackState::from_record(6).unwrap();
    assert!(!notice.timed_out(false, true, std::time::Duration::from_secs(10)),
        "a both-off joining viewer can wait silently without a parent heartbeat");

    source.set_media_state(true, true, now);
    assert!(drain(&mut source, 1, &mut a, &mut now).0.is_empty(), "no duplicate notice for an unchanged choice");
    source.set_media_state(false, true, now);
    source.set_media_state(false, false, now);
    source.publish(packet(media::KEY_FRAME, 20_000, 100), now).unwrap();
    source.set_media_state(true, false, now);
    let (events, _) = drain(&mut source, 1, &mut a, &mut now);
    assert_eq!(summary(&events), ["state 5", "state 1", "media 0x02 20000 100", "state 4"],
        "notices describe the whole state and cannot overtake queued media");

    source.set_media_state(true, true, now);
    assert_eq!(summary(&drain(&mut source, 1, &mut a, &mut now).0), ["state 6"]);
    let mut b = child(&mut source, 2, now).unwrap();
    assert_eq!(summary(&drain(&mut source, 2, &mut b, &mut now).0), ["header", "state 6", "media 0x01 0 3"]);
    source.pause(now);
    source.set_media_state(false, true, now);
    assert_eq!(summary(&drain(&mut source, 1, &mut a, &mut now).0), ["state 2"]);
    assert_eq!(summary(&drain(&mut source, 2, &mut b, &mut now).0), ["state 2"]);
    let mut c = child(&mut source, 3, now).unwrap();
    assert_eq!(summary(&drain(&mut source, 3, &mut c, &mut now).0), ["header", "state 2", "media 0x01 0 3"]);
    source.resume(now);
    assert_eq!(summary(&drain(&mut source, 1, &mut a, &mut now).0), ["state 1", "state 5"]);
    assert_eq!(summary(&drain(&mut source, 2, &mut b, &mut now).0), ["state 1", "state 5"]);
    assert_eq!(summary(&drain(&mut source, 3, &mut c, &mut now).0), ["state 1", "state 5"]);
    source.end(now);
    source.set_media_state(false, false, now);
    assert_eq!(summary(&drain(&mut source, 1, &mut a, &mut now).0), ["state 3"], "End is terminal");
}

#[test]
fn optional_video_notices_never_change_signed_radio_lifecycle() {
    let mut now = S;
    let (identity, _, _) = identities();
    let signer = crate::audio_live::Signer::new(&identity, [0; 16], 2).unwrap();
    let mut source = Source::new(signer.header().to_vec(), 1_000).unwrap();
    source.set_media_state(true, true, now);
    let mut viewer = child(&mut source, 1, now).unwrap();
    assert_eq!(summary(&drain(&mut source, 1, &mut viewer, &mut now).0), ["header"]);
    source.set_media_state(true, false, now);
    source.pause(now);
    source.set_media_state(false, true, now);
    source.resume(now);
    source.set_media_state(false, false, now);
    assert_eq!(summary(&drain(&mut source, 1, &mut viewer, &mut now).0), ["state 2", "state 1"]);
    source.end(now);
    assert_eq!(summary(&drain(&mut source, 1, &mut viewer, &mut now).0), ["state 3"]);
}
