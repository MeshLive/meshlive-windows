//! One direct stream link, both directions. Source to viewer: every fan-out
//! record sealed once for this child (its own BME1 endpoint key and sequence)
//! inside BMS1, plus the hop-local route withdrawal. Viewer to source (Android
//! `readSourceControl`): hop heartbeats with link reports, keyframe requests,
//! the stream-end acknowledgement and the withdrawal acknowledgement. Stream
//! parents never send heartbeats: Android viewers reject them. The viewer's
//! media side is `watch::Reader`. Session establishment (BMH2/BMH3, discovery
//! binding, admission) happens before; these owners only take the keys.
use crate::{
    bmh2::SessionKeys,
    fanout::Item,
    framing::{self, HopReader},
    link,
    records::{self, Layer, Opener, Policy, Sealer},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Opaque relay bytes cannot be sealed by a source.
    Kind,
    /// A record the source does not accept from a viewer.
    Control,
    Record(records::Error),
    Frame(framing::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stream session error: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<records::Error> for Error {
    fn from(error: records::Error) -> Self {
        Self::Record(error)
    }
}
impl From<framing::Error> for Error {
    fn from(error: framing::Error) -> Self {
        Self::Frame(error)
    }
}

const HOP_CONTENT: u8 = 5;
const HOP_HEARTBEAT: u8 = 6;
const HOP_ROUTE_CONTROL: u8 = 9;
const HOP_ROUTE_ACK: u8 = 10;
const KEY_FRAME_REQUEST: u8 = 5;
const STREAM_END_ACK: u8 = 8;
/// `LOCAL_CAPACITY`: the only route withdrawal reason.
pub const LOCAL_CAPACITY: u8 = 1;

/// Source to one child.
pub struct SourceLink {
    hop: Sealer,
    content: Sealer,
    mesh: Option<([u8; 16], [u8; 16])>,
    next_mesh_sequence: u64,
}
impl SourceLink {
    pub fn new(link_send: &[u8; 32], endpoint_send: &[u8; 32]) -> Result<Self, Error> {
        Ok(Self {
            hop: Sealer::new(Layer::Hop, link_send, Policy::default())?,
            content: Sealer::new(Layer::Endpoint, endpoint_send, Policy::default())?,
            mesh: None,
            next_mesh_sequence: 0,
        })
    }
    pub fn from_keys(link: &SessionKeys, endpoint: &SessionKeys) -> Result<Self, Error> {
        Self::new(link.send_key(), endpoint.send_key())
    }
    pub fn continue_link(hop: Sealer, content: Sealer, mesh: Option<([u8; 16], [u8; 16])>) -> Self {
        Self { hop, content, mesh, next_mesh_sequence: 0 }
    }
    /// One fan-out record for this child; the shared plaintext is only read.
    pub fn seal(&mut self, item: &Item) -> Result<Vec<u8>, Error> {
        let flags = match item.kind {
            1 | 4 => 0,
            2 => *item.bytes.get(4).ok_or(Error::Kind)?,
            _ => return Err(Error::Kind),
        };
        let inner = self.content.seal(item.kind, flags, &item.bytes)?;
        if let Some((source, stream)) = self.mesh {
            let mapped = match item.kind { 1 => 0, 2 => flags & !(16 | 32), 4 => 16 | if item.bytes.as_ref() == [3] { 32 } else { 0 }, _ => return Err(Error::Kind) };
            let envelope = crate::mesh::Envelope { source, stream, sequence: self.next_mesh_sequence, ttl: 8, flags: mapped, payload: &inner }
                .encode().map_err(|_| Error::Kind)?;
            self.next_mesh_sequence += 1;
            return Ok(self.hop.seal(7, 0, &envelope)?);
        }
        Ok(self.hop.seal(HOP_CONTENT, 0, &inner)?)
    }
    /// Overload: withdraw this one child (the stream itself continues).
    pub fn withdraw(&mut self) -> Result<Vec<u8>, Error> {
        Ok(self.hop.seal(HOP_ROUTE_CONTROL, 0, &[LOCAL_CAPACITY])?)
    }
}

/// Viewer to its parent.
pub struct ViewerUplink {
    hop: Sealer,
    content: Sealer,
}
impl ViewerUplink {
    pub fn continue_link(hop: Sealer, content: Sealer) -> Self {
        Self { hop, content }
    }
    pub fn from_keys(link: &SessionKeys, endpoint: &SessionKeys) -> Result<Self, Error> {
        Ok(Self {
            hop: Sealer::new(Layer::Hop, link.send_key(), Policy::default())?,
            content: Sealer::new(Layer::Endpoint, endpoint.send_key(), Policy::default())?,
        })
    }
    pub fn heartbeat(&mut self, report: Option<&link::Report>) -> Result<Vec<u8>, Error> {
        let payload = report.map_or_else(|| link::keepalive().to_vec(), link::encode);
        Ok(self.hop.seal(HOP_HEARTBEAT, 0, &payload)?)
    }
    fn control(&mut self, kind: u8) -> Result<Vec<u8>, Error> {
        let inner = self.content.seal(kind, 0, &[])?;
        Ok(self.hop.seal(HOP_CONTENT, 0, &inner)?)
    }
    pub fn keyframe_request(&mut self) -> Result<Vec<u8>, Error> {
        self.control(KEY_FRAME_REQUEST)
    }
    pub fn end_ack(&mut self) -> Result<Vec<u8>, Error> {
        self.control(STREAM_END_ACK)
    }
    pub fn withdrawal_ack(&mut self) -> Result<Vec<u8>, Error> {
        Ok(self.hop.seal(HOP_ROUTE_ACK, 0, &[LOCAL_CAPACITY])?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Heartbeat(Option<link::Report>),
    KeyFrameRequest,
    EndAck,
    WithdrawalAck,
}
/// What a source reads from one child; anything else closes that child.
pub struct SourceControl {
    hop: HopReader,
    content: Opener,
}
impl SourceControl {
    pub fn continue_link(hop: HopReader, content: Opener) -> Self { Self { hop, content } }
    pub fn from_keys(link: &SessionKeys, endpoint: &SessionKeys) -> Result<Self, Error> {
        Ok(Self {
            hop: HopReader::new(link.receive_key(), Policy::default())?,
            content: Opener::new(Layer::Endpoint, endpoint.receive_key(), Policy::default())?,
        })
    }
    pub fn read(&mut self, input: &mut &[u8]) -> Result<Option<Control>, Error> {
        // Controls are tiny; this bounds the allocation before any body is read.
        let Some(record) = self.hop.read(input, 1024)? else {
            return Ok(None);
        };
        Ok(Some(match record.header.kind {
            HOP_HEARTBEAT => {
                Control::Heartbeat(link::decode(&record.plaintext).map_err(|_| Error::Control)?)
            }
            HOP_ROUTE_ACK if record.plaintext == [LOCAL_CAPACITY] => Control::WithdrawalAck,
            HOP_CONTENT => {
                let content = self.content.open(&record.plaintext)?;
                if !content.plaintext.is_empty() {
                    return Err(Error::Control);
                }
                match content.header.kind {
                    KEY_FRAME_REQUEST => Control::KeyFrameRequest,
                    STREAM_END_ACK => Control::EndAck,
                    _ => return Err(Error::Control),
                }
            }
            _ => return Err(Error::Control),
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        bmh2::{Capabilities, Pending, Purpose, Role},
        crypto::tests::FixtureIdentity,
        fanout::FanOut,
        framing::HopReader,
        media,
        records::Opener,
        watch::{Event, Header, Reader},
    };
    use serde_json::Value;

    pub(crate) fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    pub(crate) fn identities() -> (FixtureIdentity, FixtureIdentity, [u8; 16]) {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap();
        let v = &v["link"];
        let key = |hello: &str, private: &str| {
            FixtureIdentity::new(
                &hex(v[hello]["identityKey"].as_str().unwrap()),
                &hex(v[private].as_str().unwrap()),
            )
        };
        let source_node: [u8; 16] = hex(v["initiatorHello"]["nodeId"].as_str().unwrap())
            .try_into()
            .unwrap();
        (
            key("initiatorHello", "initiatorIdentityPrivate"),
            key("responderHello", "responderIdentityPrivate"),
            source_node,
        )
    }
    fn caps() -> Capabilities {
        Capabilities {
            transport: 4,
            encode_codecs: 1,
            decode_codecs: 1,
            maximum_bitrate_kbps: 1500,
            maximum_children: 2,
        }
    }
    /// Fresh link and endpoint handshakes: (source keys, viewer keys) per purpose.
    pub(crate) fn handshake(
        source: &FixtureIdentity,
        viewer: &FixtureIdentity,
        purpose: Purpose,
    ) -> (SessionKeys, SessionKeys) {
        let a = Pending::start(source, Role::Initiator, caps(), purpose).unwrap();
        let b = Pending::start(viewer, Role::Responder, caps(), purpose).unwrap();
        let (ah, bh) = (a.hello().clone(), b.hello().clone());
        (
            a.finish(&bh, |_| Ok(())).unwrap(),
            b.finish(&ah, |_| Ok(())).unwrap(),
        )
    }
    fn packet(flags: u8, pts_us: i64, payload: &[u8]) -> Item {
        Item::media(
            media::Packet {
                flags,
                pts_us,
                payload,
            }
            .encode()
            .unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn android_mesh_sequence_starts_at_zero_after_endpoint_labels_vector() {
        let policy = Policy { labels: true, ..Policy::default() };
        let mut content = Sealer::new(Layer::Endpoint, &[2; 32], policy).unwrap();
        let labels = content.seal(10, 0, b"BLB1\0").unwrap();
        let mut endpoint = Opener::new(Layer::Endpoint, &[2; 32], policy).unwrap();
        assert_eq!(endpoint.open(&labels).unwrap().header.sequence, 0);
        let mut source = SourceLink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(),
            content, Some(([3; 16], [4; 16])));
        let mut hop = HopReader::new(&[1; 32], Policy::default()).unwrap();
        // Independent Python cryptography AESGCM vectors from the Android
        // EndToEndStreamWriter layout, not an Android runtime capture.
        // LABELS consumed BME1 sequence zero; BMV1 still begins at zero.
        let vectors = [
            (Item::header(hex("42545633028001680f020001bb8001")), "424d563104040404040404040404040404040404030303030303030303030303030303030000000000000000080000000031424d4531010000000000000001000000001f0e08de72be6198610f5728c51796b60dc5666132efe910198e01a6c99a4a73"),
            (Item::lifecycle(2), "424d563104040404040404040404040404040404030303030303030303030303030303030000000000000001081000000023424d45310400000000000000020000000011f71c97ee886a8455e864a464188c0c2525"),
            (Item::lifecycle(3), "424d563104040404040404040404040404040404030303030303030303030303030303030000000000000002083000000023424d4531040000000000000003000000001159ac5532cdceb44d6ee3b0d4a65295ed01"),
        ];
        for (sequence, (item, expected)) in vectors.into_iter().enumerate() {
            let wire = source.seal(&item).unwrap();
            let record = hop.read(&mut wire.as_slice(), 4096).unwrap().unwrap();
            assert_eq!(record.header.kind, 7);
            assert_eq!(record.plaintext, hex(expected));
            let envelope = crate::mesh::Envelope::decode(&record.plaintext).unwrap();
            assert_eq!(envelope.sequence, sequence as u64);
            let opened = endpoint.open(envelope.payload).unwrap();
            assert_eq!(opened.header.sequence, sequence as u64 + 1);
            assert_eq!(opened.plaintext, item.bytes.as_ref());
        }
    }

    #[test]
    fn one_encode_reaches_two_authenticated_viewers_through_the_watch_reader() {
        let (source_id, viewer_id, source_node) = identities();
        let header = media::VideoHeader {
            width: 426,
            height: 240,
            fps: 15,
            codec: 2,
            quarter_turns: 0,
            opus: true,
        };
        let mut fan = FanOut::default();
        fan.set_preamble(vec![Item::header(header.encode().unwrap().to_vec())]);
        let mut links = Vec::new();
        let mut readers = Vec::new();
        for child in 0..2 {
            let (sl, vl) = handshake(&source_id, &viewer_id, Purpose::Link);
            let (se, ve) = handshake(&source_id, &viewer_id, Purpose::Endpoint);
            links.push(SourceLink::from_keys(&sl, &se).unwrap());
            readers.push(Reader::direct(
                HopReader::new(vl.receive_key(), Policy::default()).unwrap(),
                Opener::new(Layer::Endpoint, ve.receive_key(), Policy::default()).unwrap(),
                source_node,
                false,
                true,
            ));
            fan.add_child(child, 0);
        }
        // Encoded once: config, a keyframe, audio, a delta frame, then End.
        fan.publish(packet(media::CONFIG, 0, &[0x82, 0x49, 0x83]), 0);
        fan.publish(packet(media::KEY_FRAME, 0, &[7; 900]), 0);
        fan.publish(packet(media::AUDIO, 20_000, &[1; 40]), 0);
        fan.publish(packet(0, 66_666, &[8; 300]), 0);
        fan.publish(Item::lifecycle(3), 0);
        for child in 0..2 {
            let mut wire = Vec::new();
            let queue = fan.queue(&child).unwrap();
            while let Some(item) = queue.take() {
                wire.extend(links[child].seal(&item).unwrap());
            }
            // Deliver in 37-byte pieces, as a GATT carrier would.
            let mut events = Vec::new();
            for chunk in wire.chunks(37) {
                let mut input = chunk;
                while events.len() < 6 {
                    match readers[child].read(&mut input, 0).unwrap() {
                        Some(event) => events.push(event),
                        None => break,
                    }
                }
            }
            assert_eq!(events.len(), 6, "child {child}");
            assert!(matches!(events[0], Event::Header(Header::Video(h)) if h == header));
            let media: Vec<(u8, i64, usize)> = events[1..5]
                .iter()
                .map(|e| match e {
                    Event::Media(p) => {
                        let p = p.packet();
                        (p.flags, p.pts_us, p.payload.len())
                    }
                    _ => panic!("media expected"),
                })
                .collect();
            assert_eq!(
                media,
                vec![
                    (media::CONFIG, 0, 3),
                    (media::KEY_FRAME, 0, 900),
                    (media::AUDIO, 20_000, 40),
                    (0, 66_666, 300)
                ]
            );
            assert!(matches!(events[5], Event::State(3)));
            // End closes the reader.
            let mut empty: &[u8] = &[];
            assert!(readers[child].read(&mut empty, 0).is_err());
        }
    }
    #[test]
    fn tampering_parent_heartbeats_and_withdrawal_follow_androids_viewer_rules() {
        let (source_id, viewer_id, source_node) = identities();
        let (sl, vl) = handshake(&source_id, &viewer_id, Purpose::Link);
        let (se, ve) = handshake(&source_id, &viewer_id, Purpose::Endpoint);
        let reader = || {
            Reader::direct(
                HopReader::new(vl.receive_key(), Policy::default()).unwrap(),
                Opener::new(Layer::Endpoint, ve.receive_key(), Policy::default()).unwrap(),
                source_node,
                false,
                true,
            )
        };
        let mut link = SourceLink::from_keys(&sl, &se).unwrap();
        let mut bad = link.seal(&Item::lifecycle(1)).unwrap();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(reader().read(&mut bad.as_slice(), 0).is_err());
        // A stream parent never sends heartbeats; an Android viewer rejects them.
        let mut hop = Sealer::new(Layer::Hop, sl.send_key(), Policy::default()).unwrap();
        let beat = hop.seal(HOP_HEARTBEAT, 0, &[1]).unwrap();
        assert_eq!(
            reader().read(&mut beat.as_slice(), 0).err(),
            Some(crate::watch::Error::Kind)
        );
        // Withdrawal reaches the viewer as Withdrawn.
        let mut link = SourceLink::from_keys(&sl, &se).unwrap();
        let withdraw = link.withdraw().unwrap();
        assert!(matches!(
            reader().read(&mut withdraw.as_slice(), 0),
            Ok(Some(Event::Withdrawn))
        ));
        assert_eq!(
            link.seal(&Item::new(vec![1, 2], Default::default())),
            Err(Error::Kind)
        );
    }
    #[test]
    fn the_source_reads_viewer_heartbeats_reports_and_controls_only() {
        let (source_id, viewer_id, _) = identities();
        let (sl, vl) = handshake(&source_id, &viewer_id, Purpose::Link);
        let (se, ve) = handshake(&source_id, &viewer_id, Purpose::Endpoint);
        let mut up = ViewerUplink::from_keys(&vl, &ve).unwrap();
        let mut control = SourceControl::from_keys(&sl, &se).unwrap();
        let report = link::Report {
            arrival_kbps: 640,
            link_late: false,
            added_delay_ms: 0,
            delivery: None,
            delivery_only: false,
        };
        let wire = [
            up.heartbeat(None).unwrap(),
            up.heartbeat(Some(&report)).unwrap(),
            up.keyframe_request().unwrap(),
            up.withdrawal_ack().unwrap(),
            up.end_ack().unwrap(),
        ]
        .concat();
        let mut got = Vec::new();
        for chunk in wire.chunks(23) {
            let mut input = chunk;
            while let Some(c) = control.read(&mut input).unwrap() {
                got.push(c);
            }
        }
        assert_eq!(
            got,
            vec![
                Control::Heartbeat(None),
                Control::Heartbeat(Some(report)),
                Control::KeyFrameRequest,
                Control::WithdrawalAck,
                Control::EndAck
            ]
        );
        // Media or a control with a payload from a viewer closes that child.
        let mut fresh = SourceControl::from_keys(&sl, &se).unwrap();
        let mut viewer = Sealer::new(Layer::Hop, vl.send_key(), Policy::default()).unwrap();
        let mut content = Sealer::new(Layer::Endpoint, ve.send_key(), Policy::default()).unwrap();
        let media = viewer
            .seal(HOP_CONTENT, 0, &content.seal(2, 0, &[0; 13]).unwrap())
            .unwrap();
        assert_eq!(fresh.read(&mut media.as_slice()), Err(Error::Control));
    }
}
