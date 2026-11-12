//! The stranger caller's link opening (PROTOCOL "Link opening", hop type 13
//! `BDO1`) against in-memory callees that follow the Android responder wire:
//! a real-key responder (pseudonyms off) exchanges `BDO1` only when both
//! signed link hellos carry bit 3; this PC's throwaway link block never signs
//! it, so no BDO1 is promised or sent and the callee's opening is checked
//! through the BMH3 OPENING extension; a pseudonymous responder clears it too.
use super::*;
use crate::{
    bmh3::{
        guard::Guards,
        handshake::{ResponderOptions, Waiting3},
    },
    incoming::{self, Listener},
    rotating::OpeningEntry,
    session::tests::identities,
};
use std::sync::mpsc::{Receiver, Sender, channel};

const EPOCH: u32 = 1_990_656;
const DAY_KEY: [u8; 32] = [3; 32];

struct Pipe {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
}
impl Carrier for Pipe {
    fn send(&mut self, bytes: &[u8], _: Instant) -> Result<(), String> {
        self.tx.send(bytes.to_vec()).map_err(|_| "closed".to_string())
    }
    fn receive(&mut self, buffer: &mut Vec<u8>, wait: Duration) -> Result<bool, String> {
        match self.rx.recv_timeout(wait) {
            Ok(b) => {
                buffer.extend(b);
                Ok(true)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(_) => Err("closed".into()),
        }
    }
}
fn pipes() -> (Pipe, Pipe) {
    let (a_tx, b_rx) = channel();
    let (b_tx, a_rx) = channel();
    (Pipe { tx: a_tx, rx: a_rx }, Pipe { tx: b_tx, rx: b_rx })
}
fn caps(transport: u8) -> Capabilities {
    Capabilities { transport, encode_codecs: 3, decode_codecs: 3, maximum_bitrate_kbps: 1100, maximum_children: 0 }
}
/// The callee's (fixture identity 0) pinned role-0 entry.
fn callee_entry() -> OpeningEntry {
    let (_, _, node) = identities();
    OpeningEntry::own(&DAY_KEY, &node, EPOCH, rotating::ROLE_CALL_OFFER).unwrap()
}

fn read_until<T>(pipe: &mut Pipe, buffer: &mut Vec<u8>, mut step: impl FnMut(&mut Vec<u8>) -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = step(buffer) {
            return value;
        }
        assert!(Instant::now() < until, "callee timed out");
        pipe.receive(buffer, Duration::from_millis(50)).unwrap();
    }
}

/// An Android callee with pseudonyms off: its real key, the BMH3 opening
/// extension, then (when both link hellos carry bit 3) its `BDO1` with
/// `sent`, followed by reading the caller's first hop record. Returns the
/// caller's signed link transport and its opening entries (None: no exchange).
fn real_callee(pipe: Pipe, transport: u8, sent: Vec<OpeningEntry>) -> std::thread::JoinHandle<(u8, Option<Vec<OpeningEntry>>, bool)> {
    real_callee_with(pipe, caps(transport), sent)
}
/// As `real_callee`, with the responder's whole link capabilities (a relay: children and bit 6).
fn real_callee_with(mut pipe: Pipe, own: Capabilities, sent: Vec<OpeningEntry>) -> std::thread::JoinHandle<(u8, Option<Vec<OpeningEntry>>, bool)> {
    let transport = own.transport;
    std::thread::spawn(move || {
        let (phone, _, _) = identities();
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let mut frame = |b: &mut Vec<u8>| {
            let mut input = b.as_slice();
            let got = frames.read(&mut input).unwrap();
            let used = b.len() - input.len();
            b.drain(..used);
            got
        };
        let m1 = read_until(&mut pipe, &mut buffer, &mut frame);
        let options = ResponderOptions { opening: Some(rotating::encode_body(&[callee_entry()]).unwrap()), ..Default::default() };
        let (answer, m2) =
            Waiting3::respond(Config::new(Purpose::Link, &phone, own).unwrap(), &m1, options, &mut Guards::default()).unwrap();
        pipe.send(&frame_handshake(&m2).unwrap(), Instant::now()).unwrap();
        let m3 = read_until(&mut pipe, &mut buffer, &mut frame);
        let (link, m4) = answer.receive(&m3, |_, _| Ok(())).unwrap();
        assert!(m4.is_none());
        assert!(link.peer().extensions().pseudonym, "the stranger caller presented its throwaway");
        let exchanged = rotating::link_opening_exchanged(
            false,
            transport,
            link.peer().capabilities().transport,
            link.own_pseudonym(),
            link.peer().extensions().pseudonym,
        );
        let seen = link.peer().capabilities().transport;
        let wifi = link.peer().extensions().wifi;
        if !exchanged {
            return (seen, None, wifi);
        }
        // LinkOpeningExchange.exchange: write this side's BDO1, then read the peer's.
        let policy = Policy { require_opening: true, ..Policy::default() };
        let mut out = Sealer::new(Layer::Hop, link.keys().send_key(), policy).unwrap();
        let mut hop_in = HopReader::new(link.keys().receive_key(), policy).unwrap();
        pipe.send(&out.seal(13, 0, &rotating::encode_bdo1(&sent).unwrap()).unwrap(), Instant::now()).unwrap();
        let record = read_until(&mut pipe, &mut buffer, |b| {
            let mut input = b.as_slice();
            let got = hop_in.read(&mut input, 4096).unwrap();
            let used = b.len() - input.len();
            b.drain(..used);
            got
        });
        assert_eq!(record.header.kind, 13, "the caller's first hop record is its BDO1");
        (seen, Some(rotating::decode_bdo1(&record.plaintext).unwrap()), wifi)
    })
}

fn dial_link(pipe: &mut Pipe, rdi: [u8; 6]) -> Result<CallerLink, Error> {
    dial_link_wifi(pipe, rdi, false)
}
fn dial_link_wifi(pipe: &mut Pipe, rdi: [u8; 6], wifi: bool) -> Result<CallerLink, Error> {
    let (_, real, _) = identities();
    let pseudonym = Identity::ephemeral().unwrap();
    let never = AtomicBool::new(false);
    caller_link(
        pipe,
        &real,
        &pseudonym,
        caps(0x0c),
        wifi,
        |peer: &Peer| selected_peer(peer, &rdi, true, EPOCH, None),
        Some((rdi, EPOCH)),
        Instant::now(),
        &never,
        &mut |_, _| {},
    )
}

#[test]
fn a_direct_dial_sends_the_wifi_marker_only_when_the_build_implements_it() {
    crate::identity_gate::set_store_dir(None);
    let entry = callee_entry();
    for wifi in [false, true] {
        let (mut pc, phone) = pipes();
        let callee = real_callee(phone, 0x0c, Vec::new());
        let _ = dial_link_wifi(&mut pc, entry.rdi, wifi).unwrap();
        let (_, _, seen) = callee.join().unwrap();
        assert_eq!(seen, wifi, "the callee's view of the caller's WIFI marker");
    }
}

#[test]
fn every_dialled_hop_carries_the_wifi_marker_and_only_a_direct_one_may_start_attempts() {
    crate::identity_gate::set_store_dir(None);
    let previous = std::env::var("MESHLIVE_WIFI_MARKER").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_MARKER", "1") };
    let entry = callee_entry();
    for relay_hops in [0u8, 1] {
        let (mut pc, phone) = pipes();
        // The first hop of a relayed call is a relay: children and bit 6 signed.
        let own = if relay_hops == 0 {
            caps(0x0c)
        } else {
            Capabilities { maximum_children: 1, ..caps(0x44) }
        };
        let callee = real_callee_with(phone, own, Vec::new());
        let (_, real, _) = identities();
        let pseudonym = Identity::ephemeral().unwrap();
        let never = AtomicBool::new(false);
        let marker = crate::wifi::implemented();
        let link = caller_link(
            &mut pc,
            &real,
            &pseudonym,
            caps(0x0c),
            marker,
            |peer: &Peer| if relay_hops == 0 { selected_peer(peer, &entry.rdi, true, EPOCH, None) } else { relay_peer(peer) },
            (relay_hops == 0).then_some((entry.rdi, EPOCH)),
            Instant::now(),
            &never,
            &mut |_, _| {},
        )
        .unwrap();
        let (_, _, seen) = callee.join().unwrap();
        assert!(seen, "relay_hops={relay_hops}: the WIFI marker on this hop");
        let mut call_link = CallLink::continue_link_with(link.hop_out, link.hop_in, &[1; 32], &[2; 32], false, false, false).unwrap();
        dialled_wifi(&mut call_link, &[9; 32], marker, relay_hops).unwrap();
        let session = CallSession::outgoing(
            call_link,
            [7; 16],
            true,
            relay_hops,
            crate::call_session::wifi_test_support::header(),
            |c| opus::Decoder::new(c, opus::Profile::StreamOrCall),
            0,
        )
        .unwrap();
        assert!(session.wifi_negotiated(), "relay_hops={relay_hops}");
        assert_eq!(session.wifi_attempts_allowed(), relay_hops == 0, "relay_hops={relay_hops}");
    }
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_MARKER", value),
            None => std::env::remove_var("MESHLIVE_WIFI_MARKER"),
        }
    }
}

#[test]
fn a_real_key_callee_with_bit_3_sees_no_bit_3_from_this_pcs_throwaway_and_no_bdo1_runs() {
    crate::identity_gate::set_store_dir(None);
    let entry = callee_entry();
    for callee_transport in [0x0c, 0x04] {
        let (mut pc, phone) = pipes();
        let callee = real_callee(phone, callee_transport, vec![entry.clone()]);
        let link = dial_link(&mut pc, entry.rdi).unwrap();
        assert!(!link.opened);
        assert!(link.link.own_pseudonym());
        // The callee's opening was still checked (BMH3 OPENING extension).
        assert_eq!(link.link.peer().node_id(), &identities().2);
        let (seen, exchanged, _) = callee.join().unwrap();
        assert_eq!(seen & rotating::LINK_OPENING_BIT, 0, "the throwaway link hello promises no BDO1");
        assert_eq!(exchanged, None);
    }
}

#[test]
fn a_real_key_callee_that_does_not_open_the_selected_id_is_refused_at_the_link() {
    crate::identity_gate::set_store_dir(None);
    let mut other = callee_entry().rdi;
    other[0] ^= 1;
    let (mut pc, phone) = pipes();
    let callee = real_callee(phone, 0x0c, Vec::new());
    assert_eq!(dial_link(&mut pc, other).err().expect("RV15").stage, Stage::Link);
    drop(pc);
    let _ = callee.join();
}

#[test]
fn a_pseudonymous_callee_clears_bit_3_and_neither_side_sends_a_bdo1() {
    crate::identity_gate::set_store_dir(None);
    let entry = callee_entry();
    let (mut pc, mut phone) = pipes();
    // This PC's own callee: an untagged stranger meets its throwaway (rule P).
    let callee = std::thread::spawn(move || {
        let (real, _, _) = identities();
        let pseudonym = Identity::ephemeral().unwrap();
        let listener = Listener {
            real: &real,
            pseudonym: &pseudonym,
            tags: Vec::new(),
            owners: Vec::new(),
            entries: vec![callee_entry()],
            epoch: EPOCH,
        };
        let never = AtomicBool::new(false);
        incoming::respond(&mut phone, &listener, &mut Guards::default(), None, &|_| false, &never, &mut |_, _| {}).err()
    });
    let link = dial_link(&mut pc, entry.rdi).unwrap();
    assert!(link.link.peer().extensions().pseudonym);
    assert_eq!(link.link.peer().capabilities().transport & rotating::LINK_OPENING_BIT, 0, "a pseudonymous responder clears bit 3");
    assert!(!link.opened);
    // The callee's next expected record is the end-to-end msg1 (hop type 4),
    // not a BDO1: anything else would be refused as an unknown first record.
    drop(pc);
    let error = callee.join().unwrap().expect("the callee waits for the end-to-end handshake");
    assert_eq!(error.stage, Stage::Endpoint, "{}", error.detail);
}
