use super::*;
use crate::{media::VideoHeader, session::tests::identities};
use std::sync::mpsc::{self, Receiver, Sender};

struct Pipe { send: Sender<Vec<u8>>, receive: Receiver<Vec<u8>> }
impl Carrier for Pipe {
    fn send(&mut self, bytes: &[u8], _: Instant) -> Result<(), String> { self.send.send(bytes.to_vec()).map_err(debug) }
    fn receive(&mut self, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, String> {
        match self.receive.recv_timeout(wait) {
            Ok(bytes) => { buffer.extend(bytes); Ok(true) },
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(error) => Err(debug(error)),
        }
    }
}
fn pipes() -> (Pipe, Pipe) {
    let (a, ar) = mpsc::channel(); let (b, br) = mpsc::channel();
    (Pipe { send: a, receive: br }, Pipe { send: b, receive: ar })
}
fn offer(prefix: [u8; 6], logical: u32) -> Announcement {
    Announcement { version: 5, prefix, flags: STREAM | GATT, summary: 1,
        token: advertise::route_token(logical, &prefix).unwrap(), cost: 0, sequence: 0,
        width: 640, height: 360, fps: 15, bitrate: 300, recovery: None }
}
const EPOCH: u32 = 1_990_656;

#[test]
fn the_newest_signed_title_is_shown_whatever_the_order() {
    let (identity, _, source) = identities();
    let old = labels::sign(&identity, labels::KIND_TITLE, 1, labels::BINDING_STREAM, &[0; 16], "Old title").unwrap();
    let new = labels::sign(&identity, labels::KIND_TITLE, 2, labels::BINDING_STREAM, &[0; 16], "New title").unwrap();
    for container in [[old.clone(), new.clone()], [new, old]] {
        let hop = HopReader::new(&[1; 32], Policy::default()).unwrap();
        let reader = Reader::direct(hop, Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(), source, false, true);
        let mut joined = Joined { reader, uplink: ViewerUplink::continue_link(
            Sealer::new(Layer::Hop, &[3; 32], Policy::default()).unwrap(),
            Sealer::new(Layer::Endpoint, &[4; 32], Policy::default()).unwrap()),
            buffer: Vec::new(), source, parent: source, recovery_key: None,
            codec_mask: 1, labels: Some(labels::container(&container)) };
        let mut writer = crate::session::SourceLink::continue_link(
            Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(),
            Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(), None);
        let header = VideoHeader { width: 640, height: 360, fps: 15, codec: 2, quarter_turns: 0, opus: true };
        let wire = writer.seal(&crate::fanout::Item::header(header.encode().unwrap().to_vec())).unwrap();
        assert!(matches!(joined.reader.read(&mut wire.as_slice(), 0).unwrap(), Some(crate::watch::Event::Header(_))));
        assert_eq!(joined.title_after_header().as_deref(), Some("New title"));
    }
}

#[test]
fn signed_direct_title_cannot_be_replayed_on_another_mesh_or_forwarded_stream() {
    let (identity, _, source) = identities();
    let zero_title = labels::sign(&identity, labels::KIND_TITLE, 0, labels::BINDING_STREAM, &[0; 16], "Direct title").unwrap();
    let bound_title = labels::sign(&identity, labels::KIND_TITLE, 0, labels::BINDING_STREAM, &[4; 16], "Bound title").unwrap();
    for forwarded in [false, true] {
        for stream in [None, Some([0; 16]), Some([4; 16]), Some([5; 16])] {
            if forwarded && stream.is_none() { continue; }
            for (title, binding) in [(&zero_title, [0; 16]), (&bound_title, [4; 16])] {
                let hop = HopReader::new(&[1; 32], Policy::default()).unwrap();
                let reader = if forwarded {
                    let grant = [b"BFG1".as_slice(), &source, &stream.unwrap(), &[2; 32]].concat();
                    Reader::forwarded(hop, mesh::Grant::decode(&grant).unwrap(), false, true).unwrap()
                } else { Reader::direct(hop, Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(), source, false, true) };
                let mut joined = Joined { reader, uplink: ViewerUplink::continue_link(
                    Sealer::new(Layer::Hop, &[3; 32], Policy::default()).unwrap(),
                    Sealer::new(Layer::Endpoint, &[4; 32], Policy::default()).unwrap()),
                    buffer: Vec::new(), source, parent: if forwarded { [9; 16] } else { source }, recovery_key: None,
                    codec_mask: 1, labels: Some(labels::container(&[title.clone()])) };
                let mut writer = crate::session::SourceLink::continue_link(
                    Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(),
                    Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap(), stream.map(|id| (source, id)));
                let header = VideoHeader { width: 640, height: 360, fps: 15, codec: 2, quarter_turns: 0, opus: true };
                let wire = writer.seal(&crate::fanout::Item::header(header.encode().unwrap().to_vec())).unwrap();
                assert!(matches!(joined.reader.read(&mut wire.as_slice(), 0).unwrap(), Some(crate::watch::Event::Header(_))));
                let expected = match stream {
                    None => binding == [0; 16],
                    Some(id) => id != [0; 16] && binding == id,
                };
                assert_eq!(joined.title_after_header().is_some(), expected,
                    "only this stream's signed binding may be displayed; forwarded={forwarded}");
                assert!(joined.title_after_header().is_none(), "container is consumed once");
            }
        }
    }
}

fn source(mut pipe: Pipe, forwarded_source: Option<[u8; 16]>) -> Result<(), String> {
    let (identity, _, node) = identities(); let cancel = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(5);
    let caps = Capabilities { transport: 0x0c, encode_codecs: 1, decode_codecs: 3, maximum_bitrate_kbps: 1500, maximum_children: 1 };
    let pending = Pending::start(&identity, Role::Initiator, caps, Purpose::Link).map_err(debug)?;
    pipe.send(&frame_handshake(&pending.hello().encode()).map_err(debug)?, deadline)?;
    let mut frames = HandshakeReader::default(); let mut buffer = Vec::new();
    let hello = loop {
        let mut input = buffer.as_slice(); let h = frames.read(&mut input).map_err(debug)?;
        let used = buffer.len() - input.len(); buffer.drain(..used);
        if let Some(h) = h { break Hello::decode(&h).map_err(debug)?; }
        pull(&mut pipe, &mut buffer, deadline, &cancel)?;
    };
    assert_eq!(hello.capabilities().maximum_children, 0);
    let link = pending.finish(&hello, |_| Ok(())).map_err(debug)?;
    let policy = Policy { require_opening: true, ..Default::default() };
    let mut send = Sealer::new(Layer::Hop, link.send_key(), policy).map_err(debug)?;
    let mut receive = HopReader::new(link.receive_key(), policy).map_err(debug)?;
    let own = OpeningEntry::own(&[3; 32], &node, EPOCH, rotating::ROLE_STREAM_OFFER).map_err(debug)?;
    pipe.send(&send.seal(13, 0, &rotating::encode_bdo1(&[own]).map_err(debug)?).map_err(debug)?, deadline)?;
    let entry = read_hop(&mut pipe, &mut buffer, &mut receive, deadline, &cancel)?;
    let entries = rotating::decode_bdo1(&entry.plaintext).map_err(debug)?;
    assert_eq!(entries[0].role, rotating::ROLE_VIEWER_REQUEST);
    assert!(entries[0].opens(&entries[0].rdi, &link.peer().node_id, rotating::ROLE_VIEWER_REQUEST).unwrap());
    let caps = Capabilities { transport: 4 | labels::CAPABILITY_BIT, ..caps };
    let pending = Pending::start(&identity, Role::Initiator, caps, Purpose::Endpoint).map_err(debug)?;
    pipe.send(&send.seal(4, 0, &pending.hello().encode()).map_err(debug)?, deadline)?;
    let hello = read_hop(&mut pipe, &mut buffer, &mut receive, deadline, &cancel)?;
    let endpoint = pending.finish(&Hello::decode(&hello.plaintext).map_err(debug)?, |peer| {
        if peer.fingerprint == link.peer().fingerprint { Ok(()) } else { Err(bmh2::Error::PeerRejected) }
    }).map_err(debug)?;
    let mut content = Sealer::new(Layer::Endpoint, endpoint.send_key(), Policy { labels: true, ..Default::default() }).map_err(debug)?;
    if let Some(source) = forwarded_source {
        let grant = [b"BFG1".as_slice(), &source, &[4; 16], &[8; 32]].concat();
        pipe.send(&send.seal(5, 0, &content.seal(7, 0, &grant).map_err(debug)?).map_err(debug)?, deadline)?;
    }
    pipe.send(&send.seal(5, 0, &content.seal(10, 0, &labels::container(&[])).map_err(debug)?).map_err(debug)?, deadline)?;
    let header = VideoHeader { width: 640, height: 360, fps: 15, codec: 2, quarter_turns: 0, opus: true };
    let mesh = forwarded_source.map(|source| (source, [4; 16]));
    if mesh.is_some() { content = Sealer::new(Layer::Endpoint, &[8; 32], Policy::default()).map_err(debug)?; }
    let mut media = crate::session::SourceLink::continue_link(send, content, mesh);
    pipe.send(&media.seal(&crate::fanout::Item::header(header.encode().unwrap().to_vec())).map_err(debug)?, deadline)?;
    if mesh.is_some() {
        let frame = crate::media::Packet { flags: crate::media::KEY_FRAME, pts_us: 20_000, payload: &[1, 2, 3] }.encode().unwrap();
        pipe.send(&media.seal(&crate::fanout::Item::media(frame).unwrap()).map_err(debug)?, deadline)?;
        for state in [2, 1] { pipe.send(&media.seal(&crate::fanout::Item::lifecycle(state)).map_err(debug)?, deadline)?; }
    }
    pipe.send(&media.seal(&crate::fanout::Item::lifecycle(3)).map_err(debug)?, deadline)?;
    let report = read_hop(&mut pipe, &mut buffer, &mut receive, deadline, &cancel)?;
    assert_eq!(report.header.kind, 6);
    assert!(link::decode(&report.plaintext).unwrap().is_some());
    let ack = read_hop(&mut pipe, &mut buffer, &mut receive, deadline, &cancel)?;
    let ack = Opener::new(Layer::Endpoint, endpoint.receive_key(), Policy::default()).map_err(debug)?.open(&ack.plaintext).map_err(debug)?;
    assert_eq!(ack.header.kind, 8);
    Ok(())
}

fn setup() -> (crate::crypto::tests::FixtureIdentity, OpeningEntry, Announcement) {
    let (_, viewer, source_node) = identities();
    let viewer_node = crate::crypto::sha256(&viewer.public_key().unwrap()).unwrap()[..16].try_into().unwrap();
    let entry = OpeningEntry::own(&[5; 32], &viewer_node, EPOCH, rotating::ROLE_VIEWER_REQUEST).unwrap();
    let source_entry = OpeningEntry::own(&[3; 32], &source_node, EPOCH, rotating::ROLE_STREAM_OFFER).unwrap();
    (viewer, entry, offer(source_entry.rdi, 42))
}

#[test]
fn authenticated_viewer_join_continues_handshake_counters_and_acknowledges_end() {
    crate::identity_gate::set_store_dir(None);
    let (viewer, entry, selected) = setup(); let (mut pipe, peer) = pipes();
    let parent = std::thread::spawn(move || source(peer, None));
    let mut joined = join(&mut pipe, &viewer, &selected, &[entry], EPOCH, 1, None, &AtomicBool::new(false)).unwrap();
    assert_eq!(joined.codec_mask, 1);
    let mut events = Vec::new();
    while events.len() < 2 {
        let mut input = joined.buffer.as_slice();
        let event = joined.reader.read(&mut input, 0).unwrap();
        let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
        if let Some(event) = event { events.push(event); }
        else { pipe.receive(&mut joined.buffer, Duration::from_secs(1)).unwrap(); }
    }
    assert!(matches!(events[0], crate::watch::Event::Header(crate::watch::Header::Video(_))));
    assert!(matches!(events[1], crate::watch::Event::State(3)));
    let report = link::Report { arrival_kbps: 300, link_late: false, added_delay_ms: 0, delivery: None, delivery_only: false };
    pipe.send(&joined.uplink.heartbeat(Some(&report)).unwrap(), Instant::now()).unwrap();
    pipe.send(&joined.uplink.end_ack().unwrap(), Instant::now()).unwrap();
    parent.join().unwrap().unwrap();
}

#[test]
fn unanswered_direct_route_yields_to_source_bound_forwarder_media_pause_resume_and_end() {
    crate::identity_gate::set_store_dir(None);
    let (viewer, entry, direct) = setup();
    let now = Instant::now();
    let sight = |address, announcement| Sighting { route: crate::bluetooth::scan::Route {
        address, address_type: windows::Devices::Bluetooth::BluetoothAddressType::Random },
        announcement, rssi: -50, received: now, kind: (5, true, false) };
    let direct = sight(1, direct);
    let mut relay = direct.clone(); relay.route.address = 2; relay.announcement.cost = 1;
    let mut offers = Offers::default(); offers.observe(direct.clone()); offers.observe(relay);
    let target = Target::from(&direct.announcement);
    for _ in 0..2 { assert_eq!(offers.next_route(target, 1, false, now).unwrap().announcement.cost, 0,
        "the original no-answer arm keeps choosing the unanswered direct route"); }
    offers.suppress(&direct, now, Duration::from_secs(1));
    let selected = offers.next_route(target, 1, false, now).unwrap().announcement.clone();
    assert_eq!(selected.cost, 1, "an unanswered attempt yields to the already-heard relay");
    let original = [6; 16];
    let (mut pipe, peer) = pipes();
    let parent = std::thread::spawn(move || source(peer, Some(original)));
    let mut joined = join(&mut pipe, &viewer, &selected, &[entry], EPOCH, 1, Some(original), &AtomicBool::new(false)).unwrap();
    assert_eq!(joined.source, original);
    assert_ne!(joined.parent, original, "the adjacent forwarder is not the authenticated original source");
    assert!(joined.recovery_key.is_some());
    let mut states = Vec::new(); let mut header = false; let mut frame = false;
    while states.last() != Some(&3) {
        let mut input = joined.buffer.as_slice();
        let event = joined.reader.read(&mut input, 0).unwrap();
        let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
        match event {
            Some(crate::watch::Event::Header(_)) => { header = true; assert_eq!(joined.reader.stream_id(), Some([4; 16])); assert!(joined.title_after_header().is_none()); },
            Some(crate::watch::Event::Media(packet)) => {
                assert!(header);
                assert_eq!((packet.packet().flags, packet.packet().pts_us, packet.packet().payload), (crate::media::KEY_FRAME, 20_000, [1, 2, 3].as_slice()));
                frame = true;
            }
            Some(crate::watch::Event::State(state)) => states.push(state),
            Some(_) => panic!("unexpected withdrawal"),
            None => { assert!(pipe.receive(&mut joined.buffer, Duration::from_secs(1)).unwrap()); }
        }
    }
    assert!(header && frame); assert_eq!(states, [2, 1, 3]);
    assert_eq!(joined.reader.stream_id(), None, "ENDED closes the media reader and releases its stream state");
    let report = link::Report { arrival_kbps: 300, link_late: false, added_delay_ms: 0, delivery: None, delivery_only: false };
    pipe.send(&joined.uplink.heartbeat(Some(&report)).unwrap(), Instant::now()).unwrap();
    pipe.send(&joined.uplink.end_ack().unwrap(), Instant::now()).unwrap();
    parent.join().unwrap().unwrap();
}

#[test]
fn retry_delay_stays_on_the_attempted_route_and_expires_without_poisoning_the_stream() {
    let now = Instant::now();
    let sight = |address, a| Sighting { route: crate::bluetooth::scan::Route {
        address, address_type: windows::Devices::Bluetooth::BluetoothAddressType::Random },
        announcement: a, rssi: -50, received: now, kind: (5, true, false) };
    let direct = sight(1, offer([1; 6], 42));
    let alternate = sight(2, direct.announcement.clone());
    let mut relayed = offer([2; 6], 42); relayed.cost = 1;
    let relay = sight(3, relayed);
    let other_stream = sight(4, offer([3; 6], 43));
    let target = Target::from(&direct.announcement);
    let mut offers = Offers::default();
    for route in [&direct, &alternate, &relay, &other_stream] { offers.observe(route.clone()); }
    offers.suppress(&direct, now, Duration::from_secs(1));
    assert_eq!(offers.next_route(target, 1, false, now).unwrap().route.address, 2,
        "a different advertising route with the same peer prefix is still eligible");
    offers.suppress(&alternate, now, Duration::from_secs(1));
    offers.observe(direct.clone());
    assert_eq!(offers.next_route(target, 1, false, now + Duration::from_millis(999)).unwrap().route.address, 3,
        "fresh sightings cannot erase a pending attempt delay");
    assert_eq!(offers.next_route(Target::from(&other_stream.announcement), 1, false, now).unwrap().route.address, 4);
    assert_eq!(offers.next_route(target, 1, false, now + Duration::from_secs(1)).unwrap().route.address, 1);
    let mut rotated = direct.clone(); rotated.announcement = offer([4; 6], 42);
    offers.suppress(&direct, now, Duration::from_secs(1));
    offers.observe(rotated);
    assert_eq!(offers.next_route(target, 1, false, now).unwrap().announcement.prefix, [4; 6],
        "the penalty cannot follow a peer or source into another advertising route");
}

#[test]
fn wrong_offer_opening_and_wrong_pinned_original_source_fail_before_media() {
    crate::identity_gate::set_store_dir(None);
    for case in 0..3 {
        let (viewer, entry, mut selected) = setup(); let (mut pipe, peer) = pipes();
        if case == 0 { selected.prefix[0] ^= 1; }
        if case == 2 { selected.cost = 1; }
        let parent = std::thread::spawn(move || source(peer, (case == 2).then_some([8; 16])));
        assert!(join(&mut pipe, &viewer, &selected, &[entry], EPOCH, 1, (case > 0).then_some([9; 16]), &AtomicBool::new(false)).is_err());
        drop(pipe); let _ = parent.join().unwrap();
    }
}

#[test]
fn request_bytes_follow_the_selected_route_and_audio_needs_no_video_decoder() {
    let (_, entry, mut selected) = setup(); selected.summary = 0x40;
    assert_eq!(unavailable(&selected, 0), None);
    let r = request(&selected, &entry, Some([7; 6])).unwrap();
    assert_eq!((r.prefix, r.token, r.cost, r.summary), (entry.rdi, selected.token, selected.cost, 0));
    assert_eq!(r.recovery, Some([7; 6]));
    assert!(matches!(crate::pc_announcement::decode(&crate::pc_announcement::wrap(1, &r).unwrap()).unwrap(), crate::pc_announcement::Pc::Request(_)));
    selected.flags &= !GATT;
    assert!(unavailable(&selected, 3).is_some() && request(&selected, &entry, None).is_err());
}

#[test]
fn reports_follow_new_trains_lateness_delay_and_unmeasured_on_time_arrivals() {
    let mut reports = Reports::default();
    assert!(reports.next(999, 1, 10, 400, false, 0).is_none());
    let r = reports.next(1000, 1, 10, 400, false, 0).unwrap();
    assert_eq!(link::encode(&r), [2, 1, 144, 0, 0, 0]);
    assert!(reports.next(1500, 2, 20, 300, false, 0).is_none());
    assert_eq!(reports.next(2000, 2, 20, 300, false, 0).unwrap().arrival_kbps, 300);
    assert!(reports.next(3000, 2, 30, 300, false, 49).is_none());
    assert!(reports.next(4000, 2, 30, 300, true, 50).unwrap().link_late);
    let mut unmeasured = Reports::default();
    assert_eq!(unmeasured.next(1000, 0, 100, 0, false, 0).unwrap().arrival_kbps, 0);
    assert!(unmeasured.next(2000, 0, 100, 0, false, 0).is_none());
    assert!(unmeasured.next(3000, 0, 200, 0, false, 0).is_some());
}

#[test]
fn route_rows_prefer_joinable_routes_survive_floods_and_expire() {
    use windows::Devices::Bluetooth::BluetoothAddressType;
    let now = Instant::now(); let mut offers = Offers::default();
    let sight = |address, a| Sighting { route: crate::bluetooth::scan::Route { address, address_type: BluetoothAddressType::Random },
        announcement: a, rssi: -50, received: now, kind: (5, true, false) };
    let direct = offer([1; 6], 42); let mut relay = offer([2; 6], 42); relay.cost = 1;
    let mut full = direct.clone(); full.sequence = 0x80;
    offers.observe(sight(1, full)); offers.observe(sight(2, relay));
    assert_eq!(offers.rows(1).len(), 1);
    let id = offers.rows(1)[0].0;
    assert_eq!(offers.best(id, 1).unwrap().announcement.cost, 1);
    offers.observe(sight(1, direct));
    assert_eq!(offers.best(id, 1).unwrap().announcement.cost, 0);
    // A failed direct route must not hide an already-heard relay on recovery.
    assert_eq!(offers.routes().filter(|s| s.route.address_key() != 1).min_by_key(|s| s.announcement.cost).unwrap().announcement.cost, 1);
    for logical in 100..300 { offers.observe(sight(logical as u64, offer([3; 6], logical))); }
    assert_eq!(offers.rows(1).len(), 64);
    assert!(offers.best(id, 1).is_some());
    offers.prune(now + Duration::from_secs(13));
    assert!(offers.rows(1).is_empty());
}
