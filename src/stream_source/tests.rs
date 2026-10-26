use super::*;
use crate::{fanout::Item, stream_watch, watch::{Event, Header}};
use std::sync::mpsc::{self, Sender, Receiver};
struct Pipe(Sender<Vec<u8>>, Receiver<Vec<u8>>);
impl Carrier for Pipe {
    fn send(&mut self, bytes: &[u8], _: Instant) -> Result<(), String> { self.0.send(bytes.to_vec()).map_err(debug) }
    fn receive(&mut self, bytes: &mut Vec<u8>, wait: Duration) -> Result<bool, String> {
        match self.1.recv_timeout(wait) { Ok(b) => { bytes.extend(b); Ok(true) }, Err(mpsc::RecvTimeoutError::Timeout) => Ok(false), Err(e) => Err(debug(e)) }
    }
}
fn pipes() -> (Pipe, Pipe) { let (a, ar) = mpsc::channel(); let (b, br) = mpsc::channel(); (Pipe(a, br), Pipe(b, ar)) }
const EPOCH: u32 = 1_990_656;

fn receive_hello(pipe: &mut Pipe, buffer: &mut Vec<u8>, deadline: Instant) -> Hello {
    let mut frames = HandshakeReader::default();
    loop {
        let mut input = buffer.as_slice(); let hello = frames.read(&mut input).unwrap();
        let used = buffer.len() - input.len(); buffer.drain(..used);
        if let Some(hello) = hello { return Hello::decode(&hello).unwrap(); }
        pull(pipe, buffer, deadline, &AtomicBool::new(false)).unwrap();
    }
}

#[test]
fn pc_signed_radio_crosses_a_distinct_relay_with_unchanged_ciphertext_and_ordered_lifecycle() {
    use crate::{mesh, session::ViewerUplink};
    crate::identity_gate::set_store_dir(None);
    let (_, relay_identity, source_node) = crate::session::tests::identities();
    let relay_node: [u8; 16] = crate::crypto::sha256(&relay_identity.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
    let viewer = crate::identity::Identity::ephemeral().unwrap();
    let viewer_node: [u8; 16] = crate::crypto::sha256(&viewer.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
    assert_ne!(viewer_node, source_node); assert_ne!(viewer_node, relay_node);
    let source_entry = OpeningEntry::own(&[3; 32], &source_node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
    let relay_request = OpeningEntry::own(&[4; 32], &relay_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
    let relay_offer = OpeningEntry::own(&[5; 32], &relay_node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
    let viewer_request = OpeningEntry::own(&[6; 32], &viewer_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
    let settings = Settings::default();
    let original = offer(settings, true, &source_entry, 42, false, false).unwrap();
    let request = stream_watch::request(&original, &relay_request, None).unwrap();
    let mut forwarded = offer(settings, true, &relay_offer, 42, false, false).unwrap(); forwarded.cost = 1;
    let (mut pc, mut upstream) = pipes(); let (mut downstream, mut receiver) = pipes();
    let source = std::thread::spawn(move || {
        let (pc_identity, _, _) = crate::session::tests::identities();
        let stop = AtomicBool::new(false); let deadline = Instant::now() + Duration::from_secs(5);
        let mut admitted = admit(&mut pc, &pc_identity, &request, &[source_entry], EPOCH, settings, true, [7; 16], "Relay fixture", Instant::now() + Duration::from_secs(12), &stop).unwrap();
        let mut signer = crate::audio_live::Signer::new(&pc_identity, [7; 16], 2).unwrap();
        pc.send(&admitted.link.seal(&Item::header(signer.header().to_vec())).unwrap(), deadline).unwrap();
        let config = crate::opus::Encoder::radio(2).unwrap().config();
        let config = signer.packet(crate::media::Packet { flags: 9, pts_us: 0, payload: &config }).unwrap();
        pc.send(&admitted.link.seal(&Item::media(config).unwrap()).unwrap(), deadline).unwrap();
        for round in 0..2 {
            let mut batcher = crate::audio_live::Batcher::new(true);
            let mut batch = None;
            for n in 0..10 { batch = batcher.push((round * 10 + n) * 20_000, vec![0xf8, 0xff, 0xfe]).unwrap(); }
            let batch = signer.packet(crate::media::Packet::decode(batch.as_ref().unwrap(), crate::media::MAX_PAYLOAD).unwrap()).unwrap();
            pc.send(&admitted.link.seal(&Item::media(batch).unwrap()).unwrap(), deadline).unwrap();
            if round == 0 { for state in [2, 1] { pc.send(&admitted.link.seal(&Item::lifecycle(state)).unwrap(), deadline).unwrap(); } }
        }
        pc.send(&admitted.link.seal(&Item::lifecycle(3)).unwrap(), deadline).unwrap();
        loop {
            let mut input = admitted.buffer.as_slice(); let control = admitted.control.read(&mut input).unwrap();
            let used = admitted.buffer.len() - input.len(); admitted.buffer.drain(..used);
            if let Some(control) = control { assert_eq!(control, crate::session::Control::EndAck); break; }
            pull(&mut pc, &mut admitted.buffer, deadline, &stop).unwrap();
        }
    });
    let relay = std::thread::spawn(move || {
        let (_, relay_identity, _) = crate::session::tests::identities();
        let stop = AtomicBool::new(false); let deadline = Instant::now() + Duration::from_secs(5);
        let caps = Capabilities { transport: 0x0c, encode_codecs: 1, decode_codecs: 3, maximum_bitrate_kbps: 1500, maximum_children: 1 };
        let mut buffer = Vec::new();
        let hello = receive_hello(&mut upstream, &mut buffer, deadline);
        let pending = Pending::start(&relay_identity, Role::Responder, caps, Purpose::Link).unwrap();
        upstream.send(&frame_handshake(&pending.hello().encode()).unwrap(), deadline).unwrap();
        let link = pending.finish(&hello, |peer| if peer.node_id == source_node { Ok(()) } else { Err(bmh2::Error::PeerRejected) }).unwrap();
        let opening_policy = Policy { require_opening: true, ..Policy::default() };
        let mut up_in = HopReader::new(link.receive_key(), opening_policy).unwrap();
        let mut up_out = Sealer::new(Layer::Hop, link.send_key(), opening_policy).unwrap();
        let opened = read_hop(&mut upstream, &mut buffer, &mut up_in, deadline, &stop).unwrap();
        let entries = rotating::decode_bdo1(&opened.plaintext).unwrap();
        assert!(entries[0].opens(&original.prefix, &source_node, rotating::ROLE_STREAM_OFFER).unwrap());
        upstream.send(&up_out.seal(13, 0, &rotating::encode_bdo1(&[relay_request]).unwrap()).unwrap(), deadline).unwrap();
        let hello = read_hop(&mut upstream, &mut buffer, &mut up_in, deadline, &stop).unwrap();
        let hello = Hello::decode(&hello.plaintext).unwrap();
        let labels_on = hello.capabilities().transport & labels::CAPABILITY_BIT != 0;
        let pending = Pending::start(&relay_identity, Role::Responder, Capabilities { transport: 4 | labels::CAPABILITY_BIT, ..caps }, Purpose::Endpoint).unwrap();
        upstream.send(&up_out.seal(4, 0, &pending.hello().encode()).unwrap(), deadline).unwrap();
        let endpoint = pending.finish(&hello, |peer| if peer.node_id == source_node { Ok(()) } else { Err(bmh2::Error::PeerRejected) }).unwrap();
        let grant = mesh::Grant::decode(&[b"BFG1".as_slice(), &source_node, &[7; 16], endpoint.receive_key()].concat()).unwrap();
        let mut media = Opener::new(Layer::Endpoint, endpoint.receive_key(), Policy { labels: labels_on, ..Policy::default() }).unwrap();
        let label_payload = if labels_on {
            let record = read_hop(&mut upstream, &mut buffer, &mut up_in, deadline, &stop).unwrap();
            let label = media.open(&record.plaintext).unwrap(); assert_eq!(label.header.kind, 10); Some(label.plaintext)
        } else { None };
        let pending = Pending::start(&relay_identity, Role::Initiator, caps, Purpose::Link).unwrap();
        downstream.send(&frame_handshake(&pending.hello().encode()).unwrap(), deadline).unwrap();
        let mut child_buffer = Vec::new(); let hello = receive_hello(&mut downstream, &mut child_buffer, deadline);
        let child = pending.finish(&hello, |peer| if peer.node_id == viewer_node { Ok(()) } else { Err(bmh2::Error::PeerRejected) }).unwrap();
        let mut child_in = HopReader::new(child.receive_key(), opening_policy).unwrap();
        let mut child_out = Sealer::new(Layer::Hop, child.send_key(), opening_policy).unwrap();
        downstream.send(&child_out.seal(13, 0, &rotating::encode_bdo1(&[relay_offer]).unwrap()).unwrap(), deadline).unwrap();
        let opening = read_hop(&mut downstream, &mut child_buffer, &mut child_in, deadline, &stop).unwrap();
        let opened = rotating::decode_bdo1(&opening.plaintext).unwrap();
        assert!(opened[0].opens(&viewer_request.rdi, &viewer_node, rotating::ROLE_VIEWER_REQUEST).unwrap());
        let pending = Pending::start(&relay_identity, Role::Initiator, Capabilities { transport: 4 | if labels_on { labels::CAPABILITY_BIT } else { 0 }, ..caps }, Purpose::Endpoint).unwrap();
        downstream.send(&child_out.seal(4, 0, &pending.hello().encode()).unwrap(), deadline).unwrap();
        let hello = read_hop(&mut downstream, &mut child_buffer, &mut child_in, deadline, &stop).unwrap();
        let child_endpoint = pending.finish(&Hello::decode(&hello.plaintext).unwrap(), |peer| if peer.node_id == viewer_node { Ok(()) } else { Err(bmh2::Error::PeerRejected) }).unwrap();
        let mut content = Sealer::new(Layer::Endpoint, child_endpoint.send_key(), Policy { labels: labels_on, ..Policy::default() }).unwrap();
        downstream.send(&child_out.seal(5, 0, &content.seal(7, 0, &grant.encode()[..]).unwrap()).unwrap(), deadline).unwrap();
        if let Some(labels) = label_payload { downstream.send(&child_out.seal(5, 0, &content.seal(10, 0, &labels).unwrap()).unwrap(), deadline).unwrap(); }
        loop {
            let record = read_hop(&mut upstream, &mut buffer, &mut up_in, deadline, &stop).unwrap(); assert_eq!(record.header.kind, 7);
            let mesh = mesh::Envelope::decode(&record.plaintext).unwrap(); assert_eq!((mesh.source, mesh.stream), (source_node, [7; 16]));
            let inspected = media.open(mesh.payload).unwrap();
            let forwarded = mesh.forwarded().unwrap().unwrap();
            assert_eq!(mesh::Envelope::decode(&forwarded).unwrap().payload, mesh.payload, "the relay never re-seals source content");
            downstream.send(&child_out.seal(7, 0, &forwarded).unwrap(), deadline).unwrap();
            if inspected.header.kind == 4 && inspected.plaintext == [3] { break; }
        }
        let mut control = crate::session::SourceControl::continue_link(child_in, Opener::new(Layer::Endpoint, child_endpoint.receive_key(), Policy::default()).unwrap());
        loop {
            let mut input = child_buffer.as_slice(); let record = control.read(&mut input).unwrap();
            let used = child_buffer.len() - input.len(); child_buffer.drain(..used);
            if let Some(record) = record { assert_eq!(record, crate::session::Control::EndAck); break; }
            pull(&mut downstream, &mut child_buffer, deadline, &stop).unwrap();
        }
        let mut uplink = ViewerUplink::continue_link(up_out, Sealer::new(Layer::Endpoint, endpoint.send_key(), Policy::default()).unwrap());
        upstream.send(&uplink.end_ack().unwrap(), deadline).unwrap();
    });
    let mut joined = stream_watch::join(&mut receiver, &viewer, &forwarded, &[viewer_request], EPOCH, 0, Some(source_node), &AtomicBool::new(false)).unwrap();
    assert_eq!(joined.source, source_node); assert_eq!(joined.parent, relay_node); assert_ne!(joined.source, joined.parent);
    let mut decoder = None; let mut states = Vec::new(); let mut units = 0;
    while states.last() != Some(&3) {
        let mut input = joined.buffer.as_slice(); let event = joined.reader.read(&mut input, 0).unwrap();
        let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
        match event {
            Some(Event::Header(Header::Audio { .. })) => { assert_eq!(joined.reader.stream_id(), Some([7; 16])); if labels::enabled() { assert_eq!(joined.title_after_header().as_deref(), Some("Relay fixture")); } },
            Some(Event::Media(packet)) if packet.packet().flags & crate::media::CONFIG != 0 => decoder = Some(crate::opus::Decoder::new(packet.packet().payload, crate::opus::Profile::AudioOnly20ms).unwrap()),
            Some(Event::Media(packet)) => for unit in packet.packet().split_audio().unwrap() {
                assert_eq!(unit.pts_us, units * 20_000); let mut pcm = [0; 960];
                assert!(decoder.as_mut().unwrap().decode(unit.payload, &mut pcm).unwrap().samples.iter().all(|s| *s == 0)); units += 1;
            },
            Some(Event::State(state)) => states.push(state),
            Some(_) => panic!("unexpected stream event"),
            None => { receiver.receive(&mut joined.buffer, Duration::from_secs(1)).unwrap(); },
        }
    }
    assert_eq!(units, 20); assert_eq!(states, [2, 1, 3]);
    receiver.send(&joined.uplink.end_ack().unwrap(), Instant::now() + Duration::from_secs(1)).unwrap();
    relay.join().unwrap(); source.join().unwrap();
}

#[test]
fn radio_microphone_follows_authenticated_listeners_and_pause_without_reopening_while_waiting() {
    let mut capture = CaptureLifecycle::new(true);
    assert_eq!(capture.update(false, false), None, "Go live and unanswered offers leave the microphone closed");
    assert!(!capture.running());
    assert_eq!(capture.update(true, true), None, "a listener joining while paused cannot start capture");
    assert_eq!(capture.update(false, true), Some(true), "Resume with an admitted listener opens capture");
    assert!(capture.running());
    assert_eq!(capture.update(false, true), None, "additional service ticks do not reopen a live microphone");
    assert_eq!(capture.update(false, false), Some(false), "losing the last listener closes capture");
    assert!(!capture.running());
    assert_eq!(capture.update(true, false), None);
    assert_eq!(capture.update(false, false), None, "Resume without a listener still leaves capture closed");
    assert_eq!(capture.update(false, true), Some(true), "authenticated re-admission starts a fresh capture");
    assert_eq!(capture.update(true, true), Some(false), "Pause closes an admitted microphone");
    assert_eq!(capture.update(true, false), None, "a paused link loss cannot reopen it");

    let mut video = CaptureLifecycle::new(false);
    assert_eq!(video.update(false, false), Some(true), "video retains its existing preview capture lifecycle");
    assert_eq!(video.update(true, false), Some(false));
    assert_eq!(video.update(false, false), Some(true));
}

#[test]
fn unmute_and_resume_keep_old_pcm_silent_until_the_latest_capture_restart() {
    use crate::call_audio::Event as CaptureEvent;
    let mut fence = CaptureFence::default();
    let mut pcm = [2_000; 960];
    fence.scrub(false, &mut pcm);
    assert_eq!(pcm, [2_000; 960], "normal capture passes unchanged");
    fence.scrub(true, &mut pcm);
    assert_eq!(pcm, [0; 960], "Mute suppresses input even without a pending restart");

    assert_eq!(fence.unmute(true), None, "Unmute while paused leaves the microphone closed until Resume");
    let unmute = fence.unmute(false).unwrap();
    let (producer, queued) = mpsc::channel();
    producer.send(CaptureEvent::Frame(Box::new([3_000; 960]))).unwrap();
    producer.send(CaptureEvent::CaptureRestarted(unmute)).unwrap();
    producer.send(CaptureEvent::Frame(Box::new([4_000; 960]))).unwrap();
    // Pause/Resume overtakes the previous restart before its events are drained.
    let resume = fence.restart();
    producer.send(CaptureEvent::CaptureRestarted(resume)).unwrap();
    producer.send(CaptureEvent::Frame(Box::new([5_000; 960]))).unwrap();
    drop(producer);
    let mut delivered = Vec::new();
    for event in queued {
        match event {
            CaptureEvent::CaptureRestarted(generation) => fence.acknowledged(generation),
            CaptureEvent::Frame(mut pcm) => { fence.scrub(false, &mut pcm[..]); delivered.push(pcm); }
            _ => unreachable!(),
        }
    }
    assert_eq!(delivered, [Box::new([0; 960]), Box::new([0; 960]), Box::new([5_000; 960])],
        "queued or superseded captures must not become audible on a later Unmute/Resume");
    let mut pcm = [6_000; 960];
    fence.scrub(true, &mut pcm);
    assert_eq!(pcm, [0; 960], "a new Mute remains effective after the acknowledgement");
}

#[test]
fn pc_source_and_viewer_exchange_authenticated_video_radio_pause_end_and_ack() {
    crate::identity_gate::set_store_dir(None);
    // A title too long to sign is omitted; the viewer still joins and plays.
    let too_long: &'static str = "A title far longer than the forty-eight signed bytes";
    for (radio, title) in [(false, "Test stream"), (true, "Test stream"), (false, too_long)] {
        let (_, viewer, source_node) = crate::session::tests::identities();
        let viewer_node = crate::crypto::sha256(&viewer.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
        let own = OpeningEntry::own(&[3; 32], &source_node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
        let remote = OpeningEntry::own(&[4; 32], &viewer_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
        let settings = Settings::default(); let selected = offer(settings, radio, &own, 42, false, false).unwrap();
        let request = stream_watch::request(&selected, &remote, None).unwrap();
        let (mut pc, mut phone) = pipes();
        let sender = std::thread::spawn(move || {
            let (source, _, _) = crate::session::tests::identities();
            let mut joined = admit(&mut pc, &source, &request, &[own], EPOCH, settings, radio, [7; 16], title, Instant::now() + Duration::from_secs(12), &AtomicBool::new(false)).unwrap();
            let mut signer = radio.then(|| crate::audio_live::Signer::new(&source, [7; 16], 2).unwrap());
            let header = if let Some(s) = signer.as_ref() { s.header().to_vec() } else { settings.video_header().encode().unwrap().to_vec() };
            let deadline = Instant::now() + Duration::from_secs(3);
            pc.send(&joined.link.seal(&Item::header(header)).unwrap(), deadline).unwrap();
            if let Some(signer) = signer.as_mut() {
                let mut encoder = crate::opus::Encoder::radio(2).unwrap(); let config = encoder.config();
                let config = signer.packet(crate::media::Packet { flags: 9, pts_us: 0, payload: &config }).unwrap();
                pc.send(&joined.link.seal(&Item::media(config).unwrap()).unwrap(), deadline).unwrap();
                let a = encoder.encode(&[0; 960]).unwrap(); let b = encoder.encode(&[0; 960]).unwrap();
                let pair = crate::live::pair_audio(crate::media::Packet { flags: 8, pts_us: 0, payload: &a }, crate::media::Packet { flags: 8, pts_us: 20_000, payload: &b }).unwrap();
                let pair = signer.packet(crate::media::Packet::decode(&pair, crate::media::MAX_PAYLOAD).unwrap()).unwrap();
                pc.send(&joined.link.seal(&Item::media(pair).unwrap()).unwrap(), deadline).unwrap();
            }
            for state in [2, 1, 3] { pc.send(&joined.link.seal(&Item::lifecycle(state)).unwrap(), deadline).unwrap(); }
            loop {
                let mut input = joined.buffer.as_slice(); let control = joined.control.read(&mut input).unwrap();
                let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
                if let Some(control) = control { assert_eq!(control, crate::session::Control::EndAck); break; }
                pull(&mut pc, &mut joined.buffer, deadline, &AtomicBool::new(false)).unwrap();
            }
        });
        let mut joined = stream_watch::join(&mut phone, &viewer, &selected, &[remote], EPOCH, if radio { 0 } else { 1 }, Some(source_node), &AtomicBool::new(false)).unwrap();
        let mut states = Vec::new(); let mut audio_units = 0; let mut header = false;
        while states.last() != Some(&3) {
            let mut input = joined.buffer.as_slice(); let event = joined.reader.read(&mut input, 0).unwrap();
            let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
            match event {
                Some(Event::Header(value)) => { header = true; assert_eq!(matches!(value, Header::Audio { .. }), radio); assert_eq!(joined.source, source_node); if crate::labels::enabled() { assert_eq!(joined.title_after_header().as_deref(), (title.len() <= 48).then_some(title)); } },
                Some(Event::Media(value)) => if value.packet().flags == 72 { audio_units += value.packet().split_audio().unwrap().len(); },
                Some(Event::State(state)) => states.push(state), Some(_) => panic!("unexpected withdrawal"),
                None => { assert!(phone.receive(&mut joined.buffer, Duration::from_secs(2)).unwrap()); }
            }
        }
        assert!(header); assert_eq!(states, [2, 1, 3]); assert_eq!(audio_units, if radio { 2 } else { 0 });
        phone.send(&joined.uplink.end_ack().unwrap(), Instant::now()).unwrap(); sender.join().unwrap();
    }
}

#[test]
fn a_silent_requester_is_dropped_at_the_admission_deadline() {
    crate::identity_gate::set_store_dir(None);
    let (source, viewer, source_node) = crate::session::tests::identities();
    let viewer_node = crate::crypto::sha256(&viewer.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
    let own = OpeningEntry::own(&[3; 32], &source_node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
    let remote = OpeningEntry::own(&[4; 32], &viewer_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
    let settings = Settings::default(); let selected = offer(settings, false, &own, 42, false, false).unwrap();
    let request = stream_watch::request(&selected, &remote, None).unwrap();
    let (mut pc, _silent_phone) = pipes();
    let started = Instant::now();
    let result = admit(&mut pc, &source, &request, &[own], EPOCH, settings, false, [7; 16], "",
        started + Duration::from_millis(300), &AtomicBool::new(false));
    assert!(result.is_err() && started.elapsed() < Duration::from_secs(3), "admission must not outlive its budget");
}

#[test]
fn source_requests_obey_token_grace_transport_and_rotating_openings() {
    let (_, _, node) = crate::session::tests::identities();
    let entry = OpeningEntry::own(&[3; 32], &node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
    let settings = Settings::default(); let a = offer(settings, false, &entry, 42, false, false).unwrap();
    let viewer = OpeningEntry::own(&[4; 32], &node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
    let mut request = stream_watch::request(&a, &viewer, None).unwrap();
    let mut grace = advertise::TokenGrace::default(); grace.advertise(a.token, 0); grace.advertise(a.token ^ 1, 1000);
    assert!(accepts(&request, &grace, 60_999)); assert!(!accepts(&request, &grace, 61_000));
    request.flags |= RECOVERY; request.recovery = Some([0; 6]); assert!(!accepts(&request, &grace, 1000));
    request.recovery = Some([8; 6]); assert!(accepts(&request, &grace, 1000));
    request.flags &= !GATT; assert!(!accepts(&request, &grace, 1000));
    let mut invalid = settings; invalid.codec = 1; assert!(!invalid.valid() && offer(invalid, false, &entry, 42, false, false).is_err());
    for preset in 1..=3 {
        let mut encoder = crate::opus::Encoder::radio(preset).unwrap();
        assert!(crate::audio_live::single_20ms_mono(&encoder.encode(&[0; 960]).unwrap()));
    }
    assert!(crate::opus::Encoder::radio(0).is_err());
}

#[test]
fn a_stray_slot_expires_during_rdi_silence_without_putting_an_offer_on_air() {
    let mut window = OfferWindow::default();
    assert!(!window.close_inbound(true, 0));
    window.rotate(14_500, 0);
    assert!(!window.can_announce());
    assert!(!window.close_inbound(true, 14_999));
    assert!(window.close_inbound(true, 15_000));
    window.close_inbound(false, 15_000);
    assert!(!window.can_announce() && !window.end_silence(15_499));
    assert!(window.end_silence(15_500) && window.can_announce());
    assert!(!window.close_inbound(true, 15_500));
    assert!(window.close_inbound(true, 30_500));
}

#[test]
fn source_rejects_a_wrong_viewer_opening_or_unsupported_decoder_before_media() {
    crate::identity_gate::set_store_dir(None);
    for wrong_codec in [false, true] {
        let (_, viewer, node) = crate::session::tests::identities();
        let viewer_node = crate::crypto::sha256(&viewer.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
        let own = OpeningEntry::own(&[3; 32], &node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
        let remote = OpeningEntry::own(&[4; 32], &viewer_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
        let selected = offer(Settings::default(), false, &own, 42, false, false).unwrap();
        let mut request = stream_watch::request(&selected, &remote, None).unwrap();
        let mut settings = Settings::default();
        if wrong_codec { settings.codec = 3; } else { request.prefix[0] ^= 1; }
        let (mut source, mut receiver) = pipes();
        let sender = std::thread::spawn(move || {
            let (identity, _, _) = crate::session::tests::identities();
            assert!(admit(&mut source, &identity, &request, &[own], EPOCH, settings, false, [7; 16], "", Instant::now() + Duration::from_secs(12), &AtomicBool::new(false)).is_err());
        });
        // With labels negotiated, source refusal closes before any header.
        if let Ok(mut joined) = stream_watch::join(&mut receiver, &viewer, &selected, &[remote], EPOCH, 1, None, &AtomicBool::new(false)) {
            assert!(receiver.receive(&mut joined.buffer, Duration::from_secs(1)).is_err());
        }
        sender.join().unwrap();
    }
}

#[test]
fn source_mesh_uses_androids_eight_hop_budget_without_changing_sealed_media() {
    let mut link = SourceLink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(),
        Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(), Some(([3; 16], [4; 16])));
    let wire = link.seal(&Item::header(Settings::default().video_header().encode().unwrap().to_vec())).unwrap();
    let mut hop = HopReader::new(&[1; 32], Policy::default()).unwrap();
    let record = hop.read(&mut wire.as_slice(), 4096).unwrap().unwrap(); assert_eq!(record.header.kind, 7);
    let mut bytes = record.plaintext.to_vec();
    let ciphertext = crate::mesh::Envelope::decode(&bytes).unwrap().payload.to_vec();
    for remaining in (1..=8).rev() {
        let envelope = crate::mesh::Envelope::decode(&bytes).unwrap();
        assert_eq!((envelope.ttl, envelope.source, envelope.stream), (remaining, [3; 16], [4; 16]));
        assert_eq!(envelope.payload, ciphertext);
        let next = envelope.forwarded().unwrap();
        if remaining == 1 { assert!(next.is_none()); } else { bytes = next.unwrap(); }
    }
}
