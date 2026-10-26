//! The callee against in-memory callers that follow the Android caller wire
//! (stranger with a throwaway, BVMID2 scanner, contact with a lease tag).
use super::*;
use crate::{
    bmh3::handshake::{InitiatorOptions, InitiatorProgress, Waiting2},
    bvmid2::Code,
    identity::Identity,
    presence,
    qr_display::{Display, Phase},
    session::tests::identities,
};
use std::sync::mpsc::{Receiver, Sender, channel};

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

const EPOCH: u32 = 1_990_656;
const DAY_KEY: [u8; 32] = [3; 32];

/// This PC (the callee): fixture identity 0 as its real key.
fn pc_node() -> [u8; 16] {
    let (pc, _, _) = identities();
    crate::crypto::sha256(&SigningIdentity::public_key(&pc).unwrap()).unwrap()[..16].try_into().unwrap()
}
fn pc_entry() -> OpeningEntry {
    OpeningEntry::own(&DAY_KEY, &pc_node(), EPOCH, 0).unwrap()
}

/// Runs the callee on this thread; the caller runs on another.
fn callee(
    pipe: &mut Pipe,
    display: Option<(&mut Display, Gate)>,
    extra: Vec<(TagCheck, TagOwner)>,
    delivered: bool,
) -> Result<Answered, Error> {
    let (pc, _, _) = identities();
    let pseudonym = Identity::ephemeral().unwrap();
    let entry = pc_entry();
    let mut tags = Vec::new();
    let mut owners = Vec::new();
    for (t, o) in extra {
        tags.push(t);
        owners.push(o);
    }
    let listener = Listener { real: &pc, pseudonym: &pseudonym, tags, owners, entries: vec![entry], epoch: EPOCH };
    let mut guards = Guards::default();
    let never = AtomicBool::new(false);
    respond(pipe, &listener, &mut guards, display, &|_| delivered, &never, &mut |_, _| {})
}

/// An Android stranger caller: throwaway at both layers, no tag.
fn stranger_caller(mut pipe: Pipe) -> std::thread::JoinHandle<Result<bool, String>> {
    std::thread::spawn(move || {
        let (_, me, _) = identities();
        let throwaway = Identity::ephemeral().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let config = Config::new(Purpose::Link, &me, CAPABILITIES).unwrap().with_stranger(&throwaway, CAPABILITIES).unwrap();
        let (waiting, m1) = Waiting2::start(config, InitiatorOptions::default()).map_err(|e| format!("{e:?}"))?;
        pipe.send(&frame_handshake(&m1).unwrap(), deadline)?;
        let m2 = loop {
            let mut input = buffer.as_slice();
            let got = frames.read(&mut input).map_err(|e| format!("{e:?}"))?;
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(m) = got {
                break m;
            }
            pull(&mut pipe, &mut buffer, Stage::Link, deadline, &never).map_err(|e| e.to_string())?;
        };
        let (progress, m3) = waiting.receive(&m2, |_| Ok(())).map_err(|e| format!("{e:?}"))?;
        let InitiatorProgress::Complete(link) = progress else { return Err("I_FIRST".into()) };
        pipe.send(&frame_handshake(&m3).unwrap(), deadline)?;
        let peer_pseudonym = link.peer().extensions().pseudonym;
        let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), Policy::default()).unwrap();
        let mut hop_in = HopReader::new(link.keys().receive_key(), Policy::default()).unwrap();
        let config = Config::new(Purpose::Endpoint, &me, CAPABILITIES).unwrap().with_stranger(&throwaway, CAPABILITIES).unwrap();
        let (waiting, m1) = Waiting2::start(config, InitiatorOptions::default()).map_err(|e| format!("{e:?}"))?;
        pipe.send(&hop_out.seal(4, 0, &m1).unwrap(), deadline)?;
        let m2 = loop {
            let mut input = buffer.as_slice();
            let r = hop_in.read(&mut input, 4096).map_err(|e| format!("{e:?}"))?;
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(r) = r {
                break r;
            }
            pull(&mut pipe, &mut buffer, Stage::Endpoint, deadline, &never).map_err(|e| e.to_string())?;
        };
        let (progress, m3) = waiting.receive(&m2.plaintext, |_| Ok(())).map_err(|e| format!("{e:?}"))?;
        let InitiatorProgress::Complete(_) = progress else { return Err("I_FIRST".into()) };
        pipe.send(&hop_out.seal(4, 0, &m3).unwrap(), deadline)?;
        Ok(peer_pseudonym)
    })
}

#[test]
fn a_stranger_meets_this_pcs_throwaway_never_its_real_key() {
    let (caller, mut pc) = pipes();
    let thread = stranger_caller(caller);
    let answered = callee(&mut pc, None, Vec::new(), false).unwrap();
    assert!(answered.own_pseudonymous, "rule P: an untagged stranger gets the throwaway");
    assert!(answered.peer_pseudonymous);
    assert_eq!(answered.tag, None);
    assert!(thread.join().unwrap().unwrap(), "the caller saw a pseudonym");
}

#[test]
fn a_scanner_of_the_shown_code_gets_the_real_key_and_consumes_the_code_once() {
    let (pc_id, _, _) = identities();
    let fingerprint = crate::crypto::sha256(&SigningIdentity::public_key(&pc_id).unwrap()).unwrap();
    let mut display = Display::new(Box::new(|| 1_000), Box::new(|| true));
    let text = display.show(fingerprint, pc_entry().rdi, [0x42; 32]).unwrap();
    let code = Code::parse(&text).unwrap();
    for attempt in 0..2 {
        let (mut scanner, mut pc) = pipes();
        let code_text = text.clone();
        let thread = std::thread::spawn(move || {
            let (_, me, _) = identities();
            let code = Code::parse(&code_text).unwrap();
            let never = AtomicBool::new(false);
            crate::verify_scan::handshake(&mut scanner, &code, &me, EPOCH, &never, &mut |_, _| {}).map(|r| r.peer_fingerprint)
        });
        let (key, binding) = display.tag_check().unwrap();
        let gate = display.new_gate();
        let result = callee(&mut pc, Some((&mut display, gate)), vec![(TagCheck::new(key, binding), TagOwner::Code)], false);
        if attempt == 0 {
            let answered = result.unwrap();
            assert_eq!(answered.tag, Some(TagOwner::Code));
            assert!(!answered.own_pseudonymous && !answered.peer_pseudonymous);
            assert_eq!(thread.join().unwrap().unwrap(), code.fingerprint);
            assert_eq!(display.phase(), Some(Phase::Consumed));
        } else {
            // A second scan of the consumed code: refused at msg1, reuse alert.
            assert!(result.is_err());
            drop(pc);
            let _ = thread.join();
            assert!(display.events.contains(&crate::qr_display::Event::ReuseAlert));
        }
    }
}

#[test]
fn a_contacts_lease_tag_gets_the_real_key_only_if_this_pc_delivered_that_lease() {
    let (_, caller_id, _) = identities();
    let caller_node: [u8; 16] =
        crate::crypto::sha256(&SigningIdentity::public_key(&caller_id).unwrap()).unwrap()[..16].try_into().unwrap();
    let entry = pc_entry();
    let tag_key = *rotating::contact_tag_key(&DAY_KEY).unwrap();
    let binding = rotating::advert_binding(&entry.rdi, 0, None);
    for (leased, delivered) in [(false, true), (false, false), (true, true), (true, false)] {
        let (mut phone, mut pc) = pipes();
        let rdi = entry.rdi;
        let node = pc_node();
        let thread = std::thread::spawn(move || {
            let (_, me, _) = identities();
            let tag = || -> Result<crate::bmh3::handshake::ContactTag, rotating::Error> {
                Ok(crate::bmh3::handshake::ContactTag::new(tag_key, rotating::advert_binding(&rdi, 0, None), node))
            };
            let real = |_: &Peer| Ok(());
            let target = crate::verify_scan::Target { rdi, tag: &tag, extra: &real };
            let never = AtomicBool::new(false);
            crate::verify_scan::tagged_handshake(&mut phone, &target, &me, EPOCH, &never, &mut |_, _| {}).map(|r| r.peer_node)
        });
        // Both owners: a named contact, and any contact holding the current lease.
        let owner = if leased { TagOwner::Leased } else { TagOwner::Contact(caller_node) };
        let result = callee(&mut pc, None, vec![(TagCheck::new(tag_key, binding), owner)], delivered);
        if delivered {
            let answered = result.unwrap();
            assert_eq!(answered.tag, Some(owner));
            // Both endpoint hellos offer call video fragments (bit 7).
            assert_ne!(answered.peer_capabilities.transport & crate::call::fragments::CAPABILITY_BIT, 0);
            assert!(!answered.own_pseudonymous);
            assert_eq!(thread.join().unwrap().unwrap(), pc_node());
        } else {
            assert!(result.is_err(), "a lease this PC never delivered is refused");
            drop(pc);
            let _ = thread.join();
        }
    }
    let _ = presence::day_of(EPOCH);
}
