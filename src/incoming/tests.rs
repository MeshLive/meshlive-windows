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
fn stranger_caller(pipe: Pipe) -> std::thread::JoinHandle<Result<(bool, u8), String>> {
    stranger_caller_with(pipe, false)
}
/// As `stranger_caller`, optionally carrying the `WIFI` marker in the link block.
fn stranger_caller_with(mut pipe: Pipe, wifi: bool) -> std::thread::JoinHandle<Result<(bool, u8), String>> {
    std::thread::spawn(move || {
        let (_, me, _) = identities();
        let throwaway = Identity::ephemeral().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let config = Config::new(Purpose::Link, &me, CAPABILITIES).unwrap().with_stranger(&throwaway, CAPABILITIES).unwrap().with_wifi(wifi);
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
        let peer_link_transport = link.peer().capabilities().transport;
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
        Ok((peer_pseudonym, peer_link_transport))
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
    let (saw_pseudonym, link_transport) = thread.join().unwrap().unwrap();
    assert!(saw_pseudonym, "the caller saw a pseudonym");
    // The throwaway's link block clears bit 3: no BDO1 is promised, so the
    // caller's first hop record is its end-to-end msg1 (the handshake completed).
    assert_eq!(link_transport & rotating::LINK_OPENING_BIT, 0);
}

/// A relay in front of an Android stranger caller: the link block (with the
/// `WIFI` marker) is the relay's own throwaway, the end-to-end handshake is
/// the caller's, under another key. Then reads what this PC writes until it
/// falls silent and returns the `BWF1` records among it.
fn relayed_stranger_caller(mut pipe: Pipe) -> std::thread::JoinHandle<Result<Vec<crate::wifi::wire::Bwf1>, String>> {
    std::thread::spawn(move || {
        let (_, me, _) = identities();
        let relay = Identity::ephemeral().unwrap();
        let caller = Identity::ephemeral().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let config = Config::new(Purpose::Link, &me, CAPABILITIES).unwrap().with_stranger(&relay, CAPABILITIES).unwrap().with_wifi(true);
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
        let policy = Policy { wifi: true, ..Policy::default() };
        let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), policy).unwrap();
        let mut hop_in = HopReader::new(link.keys().receive_key(), policy).unwrap();
        let config = Config::new(Purpose::Endpoint, &me, CAPABILITIES).unwrap().with_stranger(&caller, CAPABILITIES).unwrap();
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
        let mut records = Vec::new();
        loop {
            let mut input = buffer.as_slice();
            let mut progressed = false;
            while let Some(record) = hop_in.read(&mut input, 4096).map_err(|e| format!("{e:?}"))? {
                progressed = true;
                if record.header.kind == 16 {
                    records.push(crate::wifi::wire::Bwf1::decode(&record.plaintext).map_err(|e| format!("{e:?}"))?);
                }
            }
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if !progressed && !pipe.receive(&mut buffer, Duration::from_millis(500))? {
                return Ok(records);
            }
        }
    })
}

#[test]
fn a_callee_reached_through_a_relay_sends_all_clear_caps_and_never_arms() {
    let previous = std::env::var("MESHLIVE_WIFI_MARKER").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_MARKER", "1") };
    let (relay, mut pc) = pipes();
    let thread = relayed_stranger_caller(relay);
    let answered = callee(&mut pc, None, Vec::new(), false).unwrap();
    assert!(answered.relayed);
    assert!(answered.wifi);
    let (_, invite) = crate::call::Controller::outgoing([7; 16], true, 1).unwrap();
    let invite = invite
        .into_iter()
        .find(|message| message.signal == crate::call::Signal::Invite)
        .unwrap();
    let (mut session, _) = crate::call_session::CallSession::incoming(
        answered.link,
        &invite,
        crate::call_session::wifi_test_support::header(),
        |c| crate::opus::Decoder::new(c, crate::opus::Profile::StreamOrCall),
        0,
    )
    .unwrap();
    session.accept(true).unwrap();
    session.media_ready().unwrap();
    // This PC could host a group, but a relay edge never offers one.
    session.wifi_set_local_caps(crate::wifi::roles::windows_caps(true));
    assert!(!session.wifi_attempts_allowed());
    assert_eq!(session.wifi_begin_attempt([9; 8], 1_000_000), None);
    let mut wire = Vec::new();
    while let Some(record) = session.next_write(1_000_000).unwrap() {
        wire.extend(record);
    }
    pc.send(&wire, Instant::now()).unwrap();
    let caps = thread.join().unwrap().unwrap();
    assert_eq!(caps, vec![crate::wifi::wire::Bwf1::Caps(crate::wifi::wire::Caps::default())]);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_MARKER", value),
            None => std::env::remove_var("MESHLIVE_WIFI_MARKER"),
        }
    }
}

#[test]
fn an_incoming_call_from_a_phone_with_the_marker_gets_wifi_records_accepted_and_one_without_does_not() {
    let previous = std::env::var("MESHLIVE_WIFI_MARKER").ok();
    unsafe { std::env::set_var("MESHLIVE_WIFI_MARKER", "1") };
    for marker in [false, true] {
        let (caller, mut pc) = pipes();
        let thread = stranger_caller_with(caller, marker);
        let answered = callee(&mut pc, None, Vec::new(), false).unwrap();
        assert_eq!(answered.wifi, marker);
        assert_eq!(answered.link.wifi_enabled(), marker);
        let _ = thread.join().unwrap();
    }
    unsafe {
        match previous {
            Some(value) => std::env::set_var("MESHLIVE_WIFI_MARKER", value),
            None => std::env::remove_var("MESHLIVE_WIFI_MARKER"),
        }
    }
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
            crate::verify_scan::tagged_handshake(&mut phone, &target, &me, EPOCH, false, &never, &mut |_, _| {}).map(|r| r.peer_node)
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

// ---- Design SR (step 5): who holds K_renew ----

/// The caller's hello carries bit 4 when `renewal`.
fn stranger_caller_hello(mut pipe: Pipe, wifi: bool, renewal: bool) -> std::thread::JoinHandle<Result<(bool, u8, u8), String>> {
    std::thread::spawn(move || {
        let (_, me, _) = identities();
        let throwaway = Identity::ephemeral().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let config = Config::new(Purpose::Link, &me, CAPABILITIES).unwrap().with_stranger(&throwaway, CAPABILITIES).unwrap().with_wifi(wifi);
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
        let peer_link_transport = link.peer().capabilities().transport;
        let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), Policy::default()).unwrap();
        let mut hop_in = HopReader::new(link.keys().receive_key(), Policy::default()).unwrap();
        let endpoint = Capabilities { transport: crate::call_renewal::transport(CAPABILITIES.transport, renewal), ..CAPABILITIES };
        let config = Config::new(Purpose::Endpoint, &me, endpoint).unwrap().with_stranger(&throwaway, endpoint).unwrap();
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
        let InitiatorProgress::Complete(endpoint) = progress else { return Err("I_FIRST".into()) };
        pipe.send(&hop_out.seal(4, 0, &m3).unwrap(), deadline)?;
        let responder_hello = endpoint.peer().capabilities().transport;
        Ok((peer_pseudonym, peer_link_transport, responder_hello))
    })
}

#[test]
fn the_responder_derives_k_renew_only_for_a_call_and_only_when_both_hellos_signed_bit_four() {
    let (both, none) = (0x1c, 0x0c);
    assert!(renewal_for(true, None, both, both));
    assert!(renewal_for(true, Some(TagOwner::Leased), both, both));
    assert!(renewal_for(true, Some(TagOwner::Contact([1; 16])), both, both));
    assert!(!renewal_for(true, Some(TagOwner::Code), both, both), "a verification link never holds K_renew");
    assert!(!renewal_for(true, None, both, none), "the caller did not sign it");
    assert!(!renewal_for(true, None, none, both), "this PC did not sign it");
    assert!(!renewal_for(false, None, both, both), "the switch is off");
}

#[test]
fn a_direct_stranger_call_whose_hellos_both_signed_bit_four_holds_k_renew() {
    let _on = crate::call_renewal::test_switch::on();
    let (caller, mut pc) = pipes();
    let thread = stranger_caller_hello(caller, false, true);
    let answered = callee(&mut pc, None, Vec::new(), false).unwrap();
    assert_ne!(answered.peer_capabilities.transport & crate::call_renewal::CAPABILITY_BIT, 0);
    assert!(answered.link.renewal_key().is_some());
    assert!(answered.link.renewal_direct(), "the link peer is the end-to-end peer");
    thread.join().unwrap().unwrap();
    // A caller that did not sign bit 4 leaves this PC without a key (stripping the bit only removes renewal).
    let (caller, mut pc) = pipes();
    let thread = stranger_caller_hello(caller, false, false);
    let answered = callee(&mut pc, None, Vec::new(), false).unwrap();
    assert!(answered.link.renewal_key().is_none());
    thread.join().unwrap().unwrap();
}

#[test]
fn a_bvmid2_scan_never_signs_bit_four_or_derives_k_renew() {
    let _on = crate::call_renewal::test_switch::on();
    let (pc_id, _, _) = identities();
    let fingerprint = crate::crypto::sha256(&SigningIdentity::public_key(&pc_id).unwrap()).unwrap();
    let mut display = Display::new(Box::new(|| 1_000), Box::new(|| true));
    let text = display.show(fingerprint, pc_entry().rdi, [0x42; 32]).unwrap();
    let (mut scanner, mut pc) = pipes();
    let thread = std::thread::spawn(move || {
        let (_, me, _) = identities();
        let code = Code::parse(&text).unwrap();
        let never = AtomicBool::new(false);
        crate::verify_scan::handshake(&mut scanner, &code, &me, EPOCH, &never, &mut |_, _| {}).map(|r| r.link.renewal_key().is_some())
    });
    let (key, binding) = display.tag_check().unwrap();
    let gate = display.new_gate();
    let answered = callee(&mut pc, Some((&mut display, gate)), vec![(TagCheck::new(key, binding), TagOwner::Code)], false).unwrap();
    assert_eq!(answered.peer_capabilities.transport & crate::call_renewal::CAPABILITY_BIT, 0, "the scanner's hello does not carry bit 4");
    assert!(answered.link.renewal_key().is_none());
    assert!(!thread.join().unwrap().unwrap(), "nor does the scanner hold a key");
}

#[test]
fn a_displayer_verification_link_holds_no_renewal_key() {
    let _on = crate::call_renewal::test_switch::on();
    let (pc_id, _, _) = identities();
    let fingerprint = crate::crypto::sha256(&SigningIdentity::public_key(&pc_id).unwrap()).unwrap();
    let mut display = Display::new(Box::new(|| 1_000), Box::new(|| true));
    let text = display.show(fingerprint, pc_entry().rdi, [0x42; 32]).unwrap();
    let (mut scanner, mut pc) = pipes();
    let thread = std::thread::spawn(move || {
        let (_, me, _) = identities();
        let code = Code::parse(&text).unwrap();
        let never = AtomicBool::new(false);
        let target = crate::verify_scan::Target {
            rdi: code.rdi,
            tag: &|| crate::bvmid2::tag(&code),
            extra: &|peer| crate::bvmid2::check_responder(&code, peer),
        };
        // A scanner that signs bit 4 anyway (as a contact call would).
        crate::verify_scan::tagged_handshake_with(&mut scanner, &target, &me, EPOCH, false, true, &never, &mut |_, _| {}).map(|_| ())
    });
    let (key, binding) = display.tag_check().unwrap();
    let gate = display.new_gate();
    let answered = callee(&mut pc, Some((&mut display, gate)), vec![(TagCheck::new(key, binding), TagOwner::Code)], false).unwrap();
    assert_ne!(answered.peer_capabilities.transport & crate::call_renewal::CAPABILITY_BIT, 0, "the scanner did sign bit 4");
    assert!(answered.link.renewal_key().is_none(), "but the displayer's verification link holds no K_renew");
    thread.join().unwrap().unwrap();
}

#[test]
fn the_responders_real_endpoint_hello_carries_bit_four_only_with_the_switch_on() {
    // What the caller reads out of the responder's signed hello, byte for byte.
    let (caller, mut pc) = pipes();
    let thread = stranger_caller_hello(caller, false, false);
    callee(&mut pc, None, Vec::new(), false).unwrap();
    let hello = thread.join().unwrap().unwrap().2;
    assert_eq!(hello & crate::call_renewal::CAPABILITY_BIT, 0, "switch off: {hello:#04x}");
    let _on = crate::call_renewal::test_switch::on();
    let (caller, mut pc) = pipes();
    let thread = stranger_caller_hello(caller, false, false);
    callee(&mut pc, None, Vec::new(), false).unwrap();
    let hello = thread.join().unwrap().unwrap().2;
    assert_ne!(hello & crate::call_renewal::CAPABILITY_BIT, 0, "switch on: {hello:#04x}");
}
