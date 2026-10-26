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
