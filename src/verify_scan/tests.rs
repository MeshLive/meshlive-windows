//! The scanner against an in-memory displayer that follows the Android
//! displayer's wire behaviour (tag-matched R_FIRST with its real key and its
//! opening at both layers, BDO1, then Design QL's answer to the OFFER).
use super::*;
use crate::{
    bmh3::{
        guard::Guards,
        handshake::{ResponderMode, ResponderOptions, TagCheck, Waiting3},
    },
    presence::{Lease, PresenceStore, State},
    rotating::OpeningEntry,
    sealed::tests::{Plain, scratch},
    session::tests::identities,
};
use std::sync::mpsc::{Receiver, Sender, channel};
use zeroize::Zeroizing;

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

/// What the displayer does after the handshake.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Displayer {
    /// Design QL answer: OFFER + LEASE after the scanner's OFFER.
    Answers,
    /// Not confirmed this visit: records the OFFER and sends nothing.
    Silent,
    /// Sends its OFFER first (before the scanner's yes), then answers.
    OffersFirst,
    /// Sends an OPENING (an old build arming RV15 would) - a scope violation.
    SendsOpening,
    /// Sends a LABELS record first (allowed by the scope), then answers.
    SendsLabels,
}

struct Shown {
    code: Code,
    day_keys: std::collections::BTreeMap<u32, [u8; 32]>,
    node: [u8; 16],
}

/// The displayer's code: its fingerprint, a fresh K_qr and its role-0 RDI
/// under its own day key.
fn show() -> Shown {
    let (_, displayer, _) = identities();
    let spki = SigningIdentity::public_key(&displayer).unwrap();
    let fingerprint: [u8; 32] = crate::crypto::sha256(&spki).unwrap();
    let node: [u8; 16] = fingerprint[..16].try_into().unwrap();
    let mut n = 0u8;
    let day_keys = presence::roll(&Default::default(), EPOCH, &mut || {
        n += 1;
        [n; 32]
    });
    let entry = OpeningEntry::own(&day_keys[&presence::day_of(EPOCH)], &node, EPOCH, 0).unwrap();
    let mut k_qr = [0u8; 32];
    k_qr.copy_from_slice(&crate::crypto::random_nonce().unwrap()[..32]);
    Shown {
        code: Code {
            fingerprint,
            k_qr: Zeroizing::new(k_qr),
            rdi: entry.rdi,
        },
        day_keys,
        node,
    }
}

/// Runs the displayer on its own thread; returns the BDX1 records it received.
fn displayer(
    mut pipe: Pipe,
    tag_key: [u8; 32],
    day_keys: std::collections::BTreeMap<u32, [u8; 32]>,
    node: [u8; 16],
    behaviour: Displayer,
) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
    std::thread::spawn(move || {
        let (_, me, _) = identities();
        let entry = OpeningEntry::own(&day_keys[&presence::day_of(EPOCH)], &node, EPOCH, 0).unwrap();
        let tags = [TagCheck::new(tag_key, rotating::advert_binding(&entry.rdi, 0, None))];
        let body = rotating::encode_body(std::slice::from_ref(&entry)).unwrap();
        let mut guards = Guards::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let mut next_frame = |pipe: &mut Pipe, buffer: &mut Vec<u8>| loop {
            let mut input = buffer.as_slice();
            let got = frames.read(&mut input).unwrap();
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(m) = got {
                return m;
            }
            pull(pipe, buffer, Stage::Link, deadline, &never).unwrap();
        };
        let options = || ResponderOptions {
            mode: ResponderMode::ContactsOnly,
            tags: &tags,
            opening: Some(body.clone()),
            discovery_epoch: None,
        };
        let m1 = next_frame(&mut pipe, &mut buffer);
        let config = Config::new(Purpose::Link, &me, CAPABILITIES).unwrap();
        let (waiting, m2) = Waiting3::respond(config, &m1, options(), &mut guards).unwrap();
        pipe.send(&frame_handshake(&m2).unwrap(), deadline).unwrap();
        let m3 = next_frame(&mut pipe, &mut buffer);
        let (link, _) = waiting.receive(&m3, |_, matched| if matched { Ok(()) } else { Err(bmh3::Error::Policy) }).unwrap();
        let policy = Policy {
            require_opening: true,
            ..Policy::default()
        };
        let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), policy).unwrap();
        let mut hop_in = HopReader::new(link.keys().receive_key(), policy).unwrap();
        let next_hop = |pipe: &mut Pipe, buffer: &mut Vec<u8>, hop_in: &mut HopReader| loop {
            let mut input = buffer.as_slice();
            let r = hop_in.read(&mut input, 4096).unwrap();
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(r) = r {
                return r;
            }
            pull(pipe, buffer, Stage::Link, deadline, &never).unwrap();
        };
        let theirs = next_hop(&mut pipe, &mut buffer, &mut hop_in);
        assert_eq!(theirs.header.kind, 13);
        assert!(rotating::decode_bdo1(&theirs.plaintext).unwrap().is_empty(), "the scanner opens nothing");
        let own = rotating::encode_bdo1(std::slice::from_ref(&entry)).unwrap();
        pipe.send(&hop_out.seal(13, 0, &own).unwrap(), deadline).unwrap();
        let m1 = next_hop(&mut pipe, &mut buffer, &mut hop_in);
        let config = Config::new(Purpose::Endpoint, &me, CAPABILITIES).unwrap();
        let (waiting, m2) = Waiting3::respond(config, &m1.plaintext, options(), &mut guards).unwrap();
        pipe.send(&hop_out.seal(4, 0, &m2).unwrap(), deadline).unwrap();
        let m3 = next_hop(&mut pipe, &mut buffer, &mut hop_in);
        let (endpoint, _) = waiting
            .receive(&m3.plaintext, |peer, matched| {
                // The scanner presented its real key under the code's tag.
                if matched && !peer.extensions().pseudonym { Ok(()) } else { Err(bmh3::Error::Policy) }
            })
            .unwrap();
        let mut call = CallLink::continue_link_with(
            hop_out,
            hop_in,
            endpoint.keys().send_key(),
            endpoint.keys().receive_key(),
            false,
            true,
            true,
        )
        .unwrap();
        let lease = Lease::delivery(&day_keys, 1, EPOCH).unwrap();
        let send = |pipe: &mut Pipe, call: &mut CallLink, record: &[u8]| {
            let bytes = call.discovery(record).unwrap();
            let _ = pipe.send(&bytes, deadline);
        };
        match behaviour {
            Displayer::OffersFirst => send(&mut pipe, &mut call, &presence::offer()),
            Displayer::SendsOpening => send(&mut pipe, &mut call, b"BDX1\x01\x00"),
            Displayer::SendsLabels => {
                let bytes = call.labels(b"label record bytes").unwrap();
                let _ = pipe.send(&bytes, deadline);
            }
            _ => {}
        }
        let mut received: Vec<Vec<u8>> = Vec::new();
        let end = Instant::now() + Duration::from_secs(6);
        while Instant::now() < end {
            let mut input = buffer.as_slice();
            let got = call.read(&mut input);
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            match got {
                Ok(Some(Incoming::Discovery(p))) => {
                    let offer = presence::is_offer(&p);
                    received.push(p);
                    if offer {
                        match behaviour {
                            Displayer::Answers | Displayer::SendsLabels => {
                                send(&mut pipe, &mut call, &presence::offer());
                                send(&mut pipe, &mut call, &lease.encode());
                            }
                            Displayer::OffersFirst => send(&mut pipe, &mut call, &lease.encode()),
                            _ => {}
                        }
                    }
                    if received.len() == 2 {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => match pipe.receive(&mut buffer, Duration::from_millis(50)) {
                    Ok(_) => {}
                    Err(_) => break,
                },
                Err(_) => break,
            }
        }
        received
    })
}

struct Stores {
    dir: std::path::PathBuf,
    contacts: ContactStore,
    vault: Vault,
    claims: crate::claims::ClaimsStore,
}
fn stores() -> Stores {
    let dir = scratch("verify-scan");
    let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(Plain)), Box::new(presence::MemoryFloor::default()));
    assert!(vault.ensure_loaded(EPOCH));
    Stores {
        contacts: ContactStore::new(&dir, Box::new(Plain)),
        vault,
        claims: crate::claims::ClaimsStore::new(&dir, Box::new(Plain)),
        dir,
    }
}

/// Scans `shown` with the displayer acting as `behaviour`; `answer` is this
/// user's reply. Returns the session, the stores and what the displayer got.
fn scan(shown: Shown, behaviour: Displayer, answer: Option<bool>) -> (ScanSession, Stores, Vec<Vec<u8>>) {
    let (mut scanner_pipe, displayer_pipe) = pipes();
    let thread = displayer(displayer_pipe, *shown.code.tag_key().unwrap(), shown.day_keys.clone(), shown.node, behaviour);
    let (me, _, _) = identities();
    let never = AtomicBool::new(false);
    let reached = handshake(&mut scanner_pipe, &shown.code, &me, EPOCH, &never, &mut |_, _| {}).unwrap();
    assert_eq!(reached.peer_fingerprint, shown.code.fingerprint);
    assert!(reached.negotiated);
    let mut stores = stores();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (mut session, mut buffer) = ScanSession::new(reached, now());
    let mut asked = false;
    while session.poll(now()).is_none() {
        {
            let mut hooks = StoreHooks {
                contacts: &mut stores.contacts,
                vault: &mut stores.vault,
                claims: Some(&mut stores.claims),
                epoch: EPOCH,
                now_ms: 1_000,
            };
            let mut input = buffer.as_slice();
            session.on_input(&mut input, now(), &mut hooks);
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if !asked && now() >= 200 {
                asked = true;
                if let Some(yes) = answer {
                    let stored = yes && store_shield(hooks.contacts, &session, &shown.code, "Asha", 1_000);
                    session.confirm(yes, stored, now(), &mut hooks);
                }
            }
        }
        for record in session.take_writes() {
            let _ = scanner_pipe.send(&record, Instant::now());
        }
        if session.ended().is_none() {
            if scanner_pipe.receive(&mut buffer, Duration::from_millis(20)).is_err() {
                session.link_lost();
            }
        }
        assert!(now() < 25_000, "the session must end by itself");
    }
    drop(scanner_pipe);
    let received = thread.join().unwrap();
    (session, stores, received)
}

#[test]
fn a_mutual_second_scan_moves_both_leases_scanner_first_and_the_pc_recognises_the_phone() {
    let shown = show();
    let node = shown.node;
    let rdi = shown.code.rdi;
    let (session, stores, received) = scan(shown, Displayer::Answers, Some(true));
    assert_eq!(session.ended(), Some(End::Finished), "{:?}", session.log);
    assert!(session.lease_stored());
    assert_eq!(session.violations(), 0);
    // The scanner's OFFER went first, then its lease after the displayer's.
    assert!(presence::is_offer(&received[0]));
    let lease = Lease::decode(&received[1]).expect("the scanner's lease");
    assert_eq!(lease.keys.len(), 28);
    assert_eq!(stores.vault.contact(&node).unwrap().delivered_generation, lease.generation);
    // The PC now maps the phone's rotating call ID back to the contact.
    assert_eq!(stores.vault.recognise_call_offer(&rdi, EPOCH, 2_000), vec![node]);
    // Own scan (shield) + their lease: "scanned both ways" is known locally.
    let mut stores = stores;
    let fingerprint = stores.contacts.by_node_id(&node).unwrap().unwrap().fingerprint;
    assert!(stores.claims.scanned_both_ways(&fingerprint));
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

#[test]
fn a_first_scan_moves_no_lease_and_ends_after_the_grace() {
    let shown = show();
    let node = shown.node;
    let (session, stores, received) = scan(shown, Displayer::Silent, Some(true));
    assert!(!session.lease_stored());
    // The OFFER went, the displayer did not answer: the 20 s wait does not
    // apply once started; the scanner waits until the 60 s limit at most, and
    // here the displayer's end of the pipe closing ends it as a link error.
    assert_eq!(session.ended(), Some(End::PeerClosed), "the displayer closing is a clean end");
    assert_eq!(received.len(), 1);
    assert!(presence::is_offer(&received[0]));
    assert!(stores.vault.contact(&node).is_none_or(|c| c.lease.is_none()));
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

#[test]
fn no_offer_leaves_before_the_yes_and_an_early_displayer_offer_is_only_recorded() {
    let shown = show();
    let (session, stores, received) = scan(shown, Displayer::OffersFirst, Some(true));
    // The displayer's OFFER arrived before the yes; the scanner's start then sends OFFER and LEASE.
    assert_eq!(session.ended(), Some(End::Finished), "{:?}", session.log);
    assert!(session.lease_stored());
    assert!(presence::is_offer(&received[0]));
    assert!(Lease::decode(&received[1]).is_some());
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

#[test]
fn a_no_ends_at_once_and_stores_nothing() {
    let shown = show();
    let fingerprint = shown.code.fingerprint;
    let (session, mut stores, received) = scan(shown, Displayer::Answers, Some(false));
    assert_eq!(session.ended(), Some(End::Declined));
    assert!(received.is_empty());
    assert_eq!(stores.contacts.get(&fingerprint).unwrap(), None);
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

#[test]
fn an_opening_in_a_verification_session_is_a_scope_violation() {
    let shown = show();
    let (session, stores, _) = scan(shown, Displayer::SendsOpening, None);
    assert_eq!(session.ended(), Some(End::ScopeViolation));
    assert_eq!(session.violations(), 1);
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

#[test]
fn a_code_whose_fingerprint_is_not_the_answerers_never_reaches_it() {
    for flip in [0usize, 31] {
        let mut shown = show();
        shown.code.fingerprint[flip] ^= 1;
        let (mut scanner_pipe, displayer_pipe) = pipes();
        let key = *shown.code.tag_key().unwrap();
        let _thread = displayer(displayer_pipe, key, shown.day_keys.clone(), shown.node, Displayer::Silent);
        let (me, _, _) = identities();
        let never = AtomicBool::new(false);
        let error = handshake(&mut scanner_pipe, &shown.code, &me, EPOCH, &never, &mut |_, _| {})
            .err()
            .expect("refused");
        assert_eq!(error.stage, Stage::Link, "byte {flip}: refused before this PC presents its key");
    }
}

#[test]
fn the_scope_allows_one_offer_and_one_lease_only_when_negotiated() {
    // Android Bvmid2Test.theVerificationSessionAllowsOneOfferAndOneLeaseOnlyWhenBothHellosCarriedBit3.
    let lease = Lease {
        generation: 1,
        sender_epoch: 2_000_000,
        first_day: 2962,
        keys: vec![[1; 32]],
    }
    .encode();
    let offer = presence::offer();
    let mut plain = Scope::new(0);
    assert!(plain.refuse_discovery(&offer, false));
    assert_eq!(plain.violations(), 1);
    let mut scope = Scope::new(0);
    assert!(!scope.refuse_discovery(&offer, true));
    assert!(!scope.refuse_discovery(&lease, true));
    assert!(scope.refuse_discovery(&offer, true));
    assert!(scope.refuse_discovery(&lease, true));
    assert!(scope.refuse_discovery(b"BDX1\x01\x00", true));
    assert!(scope.refuse_discovery(b"BDX1\x04", true));
    assert!(scope.refuse_discovery(&[1, 2, 3], true));
    assert!(scope.refuse_discovery(b"BDX2\x02\x00", true));
    assert_eq!(scope.violations(), 6);
    assert!(Scope::allows_content(bvmid2::CONTENT_LABELS));
    assert!(!Scope::allows_content(11));
    for kind in 0..=255u8 {
        assert_eq!(Scope::allows_link_record(kind), [4, 5, 6, 13].contains(&kind));
    }
    assert!(!scope.expired(Scope::MAX_SESSION_MS - 1));
    assert!(scope.expired(Scope::MAX_SESSION_MS));
}

#[test]
fn the_exchange_rules_match_the_android_presence_session_tests() {
    use presence::{Action, Session};
    let lease = Lease {
        generation: 1,
        sender_epoch: 2_000_000,
        first_day: 2962,
        keys: vec![[1; 32]],
    };
    let deliver = |actions: Vec<Action>, peer: &mut Session, verified: bool| -> Vec<Action> {
        actions
            .into_iter()
            .flat_map(|a| match a {
                Action::SendOffer => peer.on_record(&Record::Offer, verified),
                Action::SendLease => peer.on_record(&Record::Lease(lease.clone()), verified),
                _ => Vec::new(),
            })
            .collect()
    };
    // First of two scans: the displayer does not take part.
    let mut displayer_a = Session::new(false);
    assert!(displayer_a.settled() && displayer_a.start().is_empty());
    let mut scanner_b = Session::new(true);
    let start = scanner_b.start();
    assert!(deliver(start, &mut displayer_a, false).is_empty());
    assert!(!scanner_b.settled());
    // Second scan: scanner first, displayer answers OFFER + LEASE, both store.
    let (mut displayer_b, mut scanner_a) = (Session::new(true), Session::new(true));
    let a_start = scanner_a.start();
    assert_eq!(a_start, vec![Action::SendOffer]);
    assert!(deliver(a_start, &mut displayer_b, true).is_empty());
    let b_answer = displayer_b.start();
    assert_eq!(b_answer, vec![Action::SendOffer, Action::SendLease]);
    let at_a = deliver(b_answer, &mut scanner_a, true);
    assert!(at_a.contains(&Action::SendLease) && at_a.iter().any(|a| matches!(a, Action::StoreLease(_))));
    let at_b = deliver(at_a, &mut displayer_b, true);
    assert!(matches!(at_b.as_slice(), [Action::StoreLease(_)]));
    assert!(displayer_b.settled() && scanner_a.settled());
    // A scanner that never confirms stores nothing; a withheld offer settles.
    let mut never = Session::new(true);
    assert!(never.on_record(&Record::Offer, false).is_empty());
    assert!(matches!(never.on_record(&Record::Lease(lease.clone()), false).as_slice(), [Action::Reject(_)]));
    assert!(!never.settled());
    let mut withheld = Session::new(true);
    withheld.start();
    withheld.offer_withheld();
    assert!(withheld.settled());
}

#[test]
fn eligibility_reasons_are_fixed_and_ordered_like_android() {
    use presence::ineligible_reason as r;
    let v = Some(PeerState::Verified);
    assert_eq!(r(true, true, v, true, false), None);
    assert_eq!(r(false, false, v, true, false), Some("no_vault"));
    assert_eq!(r(true, false, v, true, false), Some("vault_not_persistent"));
    assert_eq!(r(true, true, None, true, false), Some("no_contact_store"));
    assert_eq!(r(true, true, Some(PeerState::Unverified), true, false), Some("peer_unverified"));
    assert_eq!(r(true, true, Some(PeerState::Mismatch), true, false), Some("peer_mismatch"));
    assert_eq!(r(true, true, Some(PeerState::StoreUnreadable), true, false), Some("peer_store_unreadable"));
    assert_eq!(r(true, true, v, false, false), Some("contact_switch_off"));
    assert_eq!(r(true, true, v, true, true), Some("code_only_not_scanned"));
    assert_eq!(r(false, false, None, false, false), Some("no_vault"));
    assert_eq!(r(true, false, None, false, false), Some("vault_not_persistent"));
    assert_eq!(r(true, true, None, false, false), Some("no_contact_store"));
    assert_eq!(r(true, true, Some(PeerState::Unverified), false, false), Some("peer_unverified"));
    use crate::contacts::Method;
    assert!(presence::code_only_blocked(Some(Method::SafetyCode), false, false));
    assert!(!presence::code_only_blocked(Some(Method::SafetyCode), true, false));
    assert!(!presence::code_only_blocked(Some(Method::SafetyCode), false, true));
    assert!(!presence::code_only_blocked(Some(Method::QrScan), false, false));
}

#[test]
fn the_vault_revokes_on_forget_and_switch_off_after_delivery_and_respects_the_floor() {
    let dir = scratch("vault");
    let node = [7u8; 16];
    let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(Plain)), Box::new(presence::MemoryFloor(4)));
    assert!(vault.ensure_loaded(EPOCH));
    assert_eq!(vault.state().unwrap().generation, 5, "a new store starts above the floor");
    assert!(vault.deliver_lease(&node, EPOCH, || false).is_none(), "not verified: nothing recorded");
    assert!(vault.contact(&node).is_none());
    let lease = vault.deliver_lease(&node, EPOCH, || true).unwrap();
    assert_eq!(lease.generation, 5);
    assert_eq!(vault.contact(&node).unwrap().delivered_generation, 5);
    assert!(vault.set_visible(&node, false, EPOCH), "switch-off after delivery revokes");
    assert_eq!(vault.state().unwrap().generation, 6);
    assert!(vault.deliver_lease(&node, EPOCH, || true).is_none(), "switched off");
    vault.set_visible(&node, true, EPOCH);
    vault.deliver_lease(&node, EPOCH, || true).unwrap();
    assert!(vault.forget(&node, EPOCH));
    assert_eq!(vault.state().unwrap().generation, 7);
    assert!(vault.contact(&node).is_none());
    // A restored older store below the floor is replaced, its contacts kept.
    let older = State::fresh_at(2, EPOCH);
    PresenceStore::new(&dir, Box::new(Plain)).save(&older).unwrap();
    let mut reopened = Vault::new(PresenceStore::new(&dir, Box::new(Plain)), Box::new(presence::MemoryFloor(7)));
    assert!(reopened.ensure_loaded(EPOCH));
    assert_eq!(reopened.state().unwrap().generation, 8);
    assert_ne!(reopened.state().unwrap().keys, older.keys);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_contact_tag_from_a_stored_lease_reaches_the_contact_with_its_real_key() {
    // After a mutual scan the PC holds the phone's lease: its day key for the
    // sighted epoch gives the contact tag that the phone's responder checks.
    let shown = show();
    let day_key = shown.day_keys[&presence::day_of(EPOCH)];
    let contact_key = *rotating::contact_tag_key(&day_key).unwrap();
    let (mut pc, phone) = pipes();
    let _thread = displayer(phone, contact_key, shown.day_keys.clone(), shown.node, Displayer::Silent);
    let (me, _, _) = identities();
    let node = shown.node;
    let rdi = shown.code.rdi;
    let tag = || -> Result<ContactTag, rotating::Error> {
        Ok(ContactTag::new(contact_key, rotating::advert_binding(&rdi, 0, None), node))
    };
    let real = |peer: &Peer| if peer.extensions().pseudonym { Err(bmh3::Error::Policy) } else { Ok(()) };
    let target = Target { rdi, tag: &tag, extra: &real };
    let never = AtomicBool::new(false);
    let reached = tagged_handshake(&mut pc, &target, &me, EPOCH, &never, &mut |_, _| {}).unwrap();
    assert_eq!(reached.peer_node, node);
    assert_eq!(reached.peer_capabilities.transport & 8, 8);
    // Another contact's lease key is a different tag: the phone answers no one.
    let (mut pc, phone) = pipes();
    let _thread = displayer(phone, [9; 32], shown.day_keys.clone(), shown.node, Displayer::Silent);
    let (me, _, _) = identities();
    assert!(tagged_handshake(&mut pc, &target, &me, EPOCH, &never, &mut |_, _| {}).is_err());
}

#[test]
fn a_labels_record_is_within_the_verification_scope() {
    let shown = show();
    let (session, stores, received) = scan(shown, Displayer::SendsLabels, Some(true));
    assert_eq!(session.violations(), 0);
    assert_eq!(session.ended(), Some(End::Finished), "{:?}", session.log);
    assert!(session.log.iter().any(|l| l == "labels ignored"));
    assert_eq!(received.len(), 2);
    std::fs::remove_dir_all(&stores.dir).unwrap();
}

/// A BMH2-only callee (a default iPhone): hellos framed, BDO1, endpoint hellos in type 4.
fn bmh2_callee(mut pipe: Pipe) -> std::thread::JoinHandle<Result<[u8; 32], String>> {
    std::thread::spawn(move || {
        use crate::bmh2::{Hello, Pending, Purpose as P2, Role};
        let (_, me, _) = identities();
        let deadline = Instant::now() + Duration::from_secs(10);
        let never = AtomicBool::new(false);
        let mut buffer = Vec::new();
        let mut frames = HandshakeReader::default();
        let hello = loop {
            let mut input = buffer.as_slice();
            let got = frames.read(&mut input).map_err(|e| format!("{e:?}"))?;
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(m) = got {
                break Hello::decode(&m).map_err(|e| format!("{e:?}"))?;
            }
            pull(&mut pipe, &mut buffer, Stage::Link, deadline, &never).map_err(|e| e.to_string())?;
        };
        let pending = Pending::start(&me, Role::Responder, CAPABILITIES, P2::Link).unwrap();
        pipe.send(&frame_handshake(&pending.hello().encode()).unwrap(), deadline)?;
        let link = pending.finish(&hello, |_| Ok(())).map_err(|e| format!("{e:?}"))?;
        let policy = Policy { require_opening: true, ..Policy::default() };
        let mut hop_out = Sealer::new(Layer::Hop, link.send_key(), policy).unwrap();
        let mut hop_in = HopReader::new(link.receive_key(), policy).unwrap();
        let mut next = |pipe: &mut Pipe, buffer: &mut Vec<u8>| loop {
            let mut input = buffer.as_slice();
            let r = hop_in.read(&mut input, 4096).map_err(|e| format!("{e:?}"))?;
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if let Some(r) = r {
                return Ok::<_, String>(r);
            }
            pull(pipe, buffer, Stage::Link, deadline, &never).map_err(|e| e.to_string())?;
        };
        let theirs = next(&mut pipe, &mut buffer)?;
        assert_eq!(theirs.header.kind, 13);
        pipe.send(&hop_out.seal(13, 0, &rotating::encode_bdo1(&[]).unwrap()).unwrap(), deadline)?;
        let m = next(&mut pipe, &mut buffer)?;
        let remote = Hello::decode(&m.plaintext).map_err(|e| format!("{e:?}"))?;
        let pending = Pending::start(&me, Role::Responder, CAPABILITIES, P2::Endpoint).unwrap();
        pipe.send(&hop_out.seal(4, 0, &pending.hello().encode()).unwrap(), deadline)?;
        let endpoint = pending.finish(&remote, |_| Ok(())).map_err(|e| format!("{e:?}"))?;
        Ok(endpoint.peer().fingerprint)
    })
}

#[test]
fn a_bmh2_only_callee_is_reached_with_this_pcs_real_key_and_the_announced_prefix() {
    let (_, callee, _) = identities();
    let spki = SigningIdentity::public_key(&callee).unwrap();
    let fingerprint = crate::crypto::sha256(&spki).unwrap();
    let prefix: [u8; 6] = fingerprint[..6].try_into().unwrap();
    let (mut pc, phone) = pipes();
    let thread = bmh2_callee(phone);
    let (me, _, _) = identities();
    let never = AtomicBool::new(false);
    let reached = bmh2_handshake(&mut pc, prefix, &me, &never, &mut |_, _| {}).unwrap();
    assert_eq!(reached.peer_fingerprint, fingerprint);
    assert!(reached.negotiated);
    let my_spki = SigningIdentity::public_key(&me).unwrap();
    assert_eq!(thread.join().unwrap().unwrap(), crate::crypto::sha256(&my_spki).unwrap());
    // Another advertised prefix: refused before the end-to-end handshake.
    let (mut pc, phone) = pipes();
    let _thread = bmh2_callee(phone);
    let mut wrong = prefix;
    wrong[0] ^= 1;
    let error = bmh2_handshake(&mut pc, wrong, &me, &never, &mut |_, _| {}).err().expect("refused");
    assert_eq!(error.stage, Stage::Link);
}

/// The phone's side of `pc_shows` (sessions and stores stay on its thread).
struct ScannerSide {
    lease_stored: bool,
    log: Vec<String>,
    violations: u32,
}

/// The PC as the DISPLAYER (Design QW): the real responder (`incoming::respond`
/// with the shown code's reservation) and `ScanSession::displayer`, against
/// the real scanner session. Returns the scanner's outcome, the displayer
/// session and its stores.
fn pc_shows(answering: bool) -> (ScannerSide, ScanSession, Stores) {
    use crate::{
        identity::Identity,
        incoming::{Listener, TagOwner, respond},
        qr_display::Display,
    };
    let (scanner_id, displayer_id, _) = identities();
    let mut shown_stores = stores();
    let node: [u8; 16] = crate::crypto::sha256(&SigningIdentity::public_key(&displayer_id).unwrap()).unwrap()[..16].try_into().unwrap();
    let fingerprint = crate::crypto::sha256(&SigningIdentity::public_key(&displayer_id).unwrap()).unwrap();
    let day_key = shown_stores.vault.state().unwrap().keys[&presence::day_of(EPOCH)];
    let entry = OpeningEntry::own(&day_key, &node, EPOCH, 0).unwrap();
    let scanner_fp = crate::crypto::sha256(&SigningIdentity::public_key(&scanner_id).unwrap()).unwrap();
    if answering {
        // This PC scanned the phone's code earlier in this visit (shield).
        shown_stores.contacts.mark_verified(Some(&scanner_fp), &scanner_fp, "Phone", Method::QrScan, 1_000);
    }
    let start = Instant::now();
    let mut display = Display::new(Box::new(move || start.elapsed().as_millis() as u64), Box::new(|| true));
    let text = display.show(fingerprint, entry.rdi, Display::fresh_secret()).unwrap();
    let code = Code::parse(&text).unwrap();
    let (mut scanner_pipe, mut pc_pipe) = pipes();
    // The scanner (a phone) on its own thread: verify, answer yes, exchange.
    let scanner = std::thread::spawn(move || {
        let (me, _, _) = identities();
        let never = AtomicBool::new(false);
        let reached = handshake(&mut scanner_pipe, &code, &me, EPOCH, &never, &mut |_, _| {}).unwrap();
        let mut stores = stores();
        let clock = Instant::now();
        let now = || clock.elapsed().as_millis() as u64;
        let (mut session, mut buffer) = ScanSession::new(reached, now());
        let mut asked = false;
        while session.poll(now()).is_none() {
            {
                let mut hooks = StoreHooks { contacts: &mut stores.contacts, vault: &mut stores.vault, claims: Some(&mut stores.claims), epoch: EPOCH, now_ms: 1_000 };
                let mut input = buffer.as_slice();
                session.on_input(&mut input, now(), &mut hooks);
                let used = buffer.len() - input.len();
                buffer.drain(..used);
                if !asked {
                    asked = true;
                    let stored = store_shield(hooks.contacts, &session, &code, "PC", 1_000);
                    session.confirm(true, stored, now(), &mut hooks);
                }
            }
            for record in session.take_writes() {
                let _ = scanner_pipe.send(&record, Instant::now());
            }
            if session.ended().is_none() && scanner_pipe.receive(&mut buffer, Duration::from_millis(20)).is_err() {
                session.link_lost();
            }
            assert!(now() < 25_000);
        }
        std::fs::remove_dir_all(&stores.dir).unwrap();
        ScannerSide { lease_stored: session.lease_stored(), log: session.log.clone(), violations: session.violations() }
    });
    // This PC: the responder with the code's tag under its reservation.
    let pseudonym = Identity::ephemeral().unwrap();
    let (key, binding) = display.tag_check().unwrap();
    let listener = Listener {
        real: &displayer_id,
        pseudonym: &pseudonym,
        tags: vec![TagCheck::new(key, binding)],
        owners: vec![TagOwner::Code],
        entries: vec![entry],
        epoch: EPOCH,
    };
    let gate = display.new_gate();
    let mut guards = Guards::default();
    let never = AtomicBool::new(false);
    let answered = respond(&mut pc_pipe, &listener, &mut guards, Some((&mut display, gate)), &|_| false, &never, &mut |_, _| {}).unwrap();
    assert_eq!(answered.tag, Some(TagOwner::Code));
    assert!(!answered.own_pseudonymous, "a code match presents the real key (rule P)");
    assert!(display.events.iter().any(|e| matches!(e, crate::qr_display::Event::ScanCompleted(fp) if *fp == scanner_fp)), "committed");
    let reached = Reached {
        link: answered.link,
        peer_fingerprint: answered.peer_fingerprint,
        peer_node: answered.peer_node,
        negotiated: answered.negotiated,
        peer_capabilities: answered.peer_capabilities,
        buffer: answered.buffer,
    };
    let clock = Instant::now();
    let now = || clock.elapsed().as_millis() as u64;
    let (mut session, mut buffer) = ScanSession::displayer(reached, now(), answering);
    while session.poll(now()).is_none() {
        {
            let mut hooks = StoreHooks { contacts: &mut shown_stores.contacts, vault: &mut shown_stores.vault, claims: Some(&mut shown_stores.claims), epoch: EPOCH, now_ms: 1_000 };
            let mut input = buffer.as_slice();
            session.on_input(&mut input, now(), &mut hooks);
            let used = buffer.len() - input.len();
            buffer.drain(..used);
        }
        for record in session.take_writes() {
            let _ = pc_pipe.send(&record, Instant::now());
        }
        if session.ended().is_none() && pc_pipe.receive(&mut buffer, Duration::from_millis(20)).is_err() {
            session.link_lost();
        }
        assert!(now() < 25_000);
    }
    drop(pc_pipe);
    (scanner.join().unwrap(), session, shown_stores)
}

#[test]
fn a_pc_displayer_answers_the_scanners_offer_only_after_its_own_scan_in_this_visit() {
    // Confirmed this visit: both leases move, each side recognises the other.
    let (scanner, shown, shown_stores) = pc_shows(true);
    assert_eq!(shown.ended(), Some(End::Finished), "{:?}", shown.log);
    assert!(shown.lease_stored(), "the PC stored the phone's lease: {:?}", shown.log);
    assert!(scanner.lease_stored, "the phone stored the PC's lease: {:?}", scanner.log);
    assert!(shown.log.iter().any(|l| l == "offer_received start=answer"));
    assert_eq!(shown.violations() + scanner.violations, 0);
    std::fs::remove_dir_all(&shown_stores.dir).unwrap();
    // Not confirmed: the displayer never offers; the session ends after the grace.
    let (scanner, shown, shown_stores) = pc_shows(false);
    assert!(!shown.lease_stored() && !scanner.lease_stored);
    assert!(!shown.log.iter().any(|l| l.starts_with("offer_sent") || l.starts_with("lease_sent")), "{:?}", shown.log);
    assert!(matches!(shown.ended(), Some(End::Finished | End::PeerClosed)), "{:?}", shown.ended());
    std::fs::remove_dir_all(&shown_stores.dir).unwrap();
}

#[test]
fn the_start_up_reconcile_drops_records_of_former_contacts_and_revokes_a_current_delivery() {
    let dir = scratch("vault-reconcile");
    let (kept, gone, quiet) = ([1u8; 16], [2u8; 16], [3u8; 16]);
    let mut vault = Vault::new(PresenceStore::new(&dir, Box::new(Plain)), Box::new(presence::MemoryFloor(0)));
    assert!(vault.ensure_loaded(EPOCH));
    vault.deliver_lease(&kept, EPOCH, || true).unwrap();
    vault.deliver_lease(&gone, EPOCH, || true).unwrap();
    let generation = vault.state().unwrap().generation;
    // Nothing to do while every record is a verified contact.
    assert_eq!(vault.reconcile(&[kept, gone], EPOCH), (0, false));
    // `gone` is no longer verified and had this generation's lease: revoke, drop.
    assert_eq!(vault.reconcile(&[kept], EPOCH), (1, true));
    assert_eq!(vault.state().unwrap().generation, generation + 1);
    assert!(vault.contact(&gone).is_none() && vault.contact(&kept).is_some());
    // A former contact whose delivery is of an older generation is dropped
    // without a new one.
    vault.deliver_lease(&quiet, EPOCH, || true).unwrap();
    assert!(vault.bump(EPOCH));
    let generation = vault.state().unwrap().generation;
    assert_eq!(vault.reconcile(&[kept], EPOCH), (1, false));
    assert_eq!(vault.state().unwrap().generation, generation);
    std::fs::remove_dir_all(dir).unwrap();
}
