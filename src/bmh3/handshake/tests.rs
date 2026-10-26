use super::*;
use crate::{
    bmh3::tests::{bytes, hex, purpose, scalar, schedule, vectors},
    crypto::tests::{FixtureIdentity, fixture_ephemeral},
    records::{Layer, Policy, Sealer},
};
use serde_json::Value;
use std::cell::Cell;

struct VectorSigner {
    spki: [u8; 91],
    context: Vec<u8>,
    signature: Vec<u8>,
    calls: Cell<usize>,
}
impl VectorSigner {
    fn new(v: &Value, role: Role) -> Self {
        let (who, suffix) = if role == Role::Initiator {
            ("initiator_trace", "I")
        } else {
            ("responder_trace", "R")
        };
        Self {
            spki: bytes(&v["inputs"], &format!("identity_{suffix}_spki"))
                .try_into()
                .unwrap(),
            context: keys::signature_context(
                purpose(v),
                role,
                &bytes(&v[who], &format!("th_sig_{suffix}"))
                    .try_into()
                    .unwrap(),
            ),
            signature: crypto::raw_to_der(
                &bytes(&v[who], &format!("sig_{suffix}")).try_into().unwrap(),
            )
            .unwrap(),
            calls: Cell::new(0),
        }
    }
}
impl SigningIdentity for VectorSigner {
    fn public_key(&self) -> Result<[u8; 91], crypto::Error> {
        Ok(self.spki)
    }
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, crypto::Error> {
        self.calls.set(self.calls.get() + 1);
        if message != self.context {
            return Err(crypto::Error::Signature);
        }
        Ok(self.signature.clone())
    }
}
fn real(v: &Value, role: Role) -> FixtureIdentity {
    let suffix = if role == Role::Initiator { "I" } else { "R" };
    if v["reveal"].is_object() {
        FixtureIdentity::new(
            &bytes(&v["reveal"], &format!("real_{suffix}_spki")),
            &scalar(&v["reveal"], &format!("real_{suffix}_scalar")),
        )
    } else {
        FixtureIdentity::new(
            &bytes(&v["inputs"], &format!("identity_{suffix}_spki")),
            &scalar(&v["inputs"], &format!("identity_{suffix}_scalar")),
        )
    }
}
fn presented_identity(v: &Value, role: Role) -> FixtureIdentity {
    let suffix = if role == Role::Initiator { "I" } else { "R" };
    FixtureIdentity::new(
        &bytes(&v["inputs"], &format!("identity_{suffix}_spki")),
        &scalar(&v["inputs"], &format!("identity_{suffix}_scalar")),
    )
}
fn caps(v: &Value, role: Role) -> Capabilities {
    Capabilities::decode(&bytes(
        &v["inputs"],
        if role == Role::Initiator {
            "caps_I"
        } else {
            "caps_R"
        },
    ))
    .unwrap()
}
fn ext(v: &Value, role: Role) -> Extensions {
    Extensions::parse(&bytes(
        &v["inputs"],
        if role == Role::Initiator {
            "extensions_I"
        } else {
            "extensions_R"
        },
    ))
    .unwrap()
}
fn config<'a>(
    v: &Value,
    role: Role,
    signer: &'a VectorSigner,
    real: &'a FixtureIdentity,
) -> Config<'a> {
    let config = if ext(v, role).pseudonym {
        Config::new(purpose(v), real, caps(v, role))
            .unwrap()
            .with_stranger(signer, caps(v, role))
            .unwrap()
    } else {
        Config::new(purpose(v), signer, caps(v, role)).unwrap()
    };
    if let Some(tag) = ext(v, role).build_tag {
        config.with_build_tag(tag).unwrap()
    } else {
        config
    }
}
fn party<'a>(v: &Value, role: Role, config: Config<'a>) -> Party<'a> {
    let p = purpose(v);
    let m1 = bytes(&v["messages"], "m1");
    let m2 = bytes(&v["messages"], "m2");
    let (point, suffix) = if role == Role::Initiator {
        (*wire::Message1::parse(&m1, p).unwrap().point, "I")
    } else {
        (*wire::Message2::parse(&m2, p).unwrap().point, "R")
    };
    let ephemeral = fixture_ephemeral(
        &wire::spki_from_point(&point).unwrap(),
        &scalar(&v["inputs"], &format!("ephemeral_{suffix}_scalar")),
    );
    let mut party = Party::new(config, role);
    party.entropy = Some(TestEntropy {
        ephemeral: Some(ephemeral),
        nonce: bytes(&v["inputs"], &format!("nonce_{suffix}"))
            .try_into()
            .unwrap(),
        tag: bytes(&v["inputs"], "random_tag_I")
            .try_into()
            .unwrap_or([0; 16]),
    });
    party
}
fn options(v: &Value) -> InitiatorOptions {
    let input = &v["inputs"];
    let key = bytes(input, "tag_key_I");
    InitiatorOptions {
        may_reveal_first: input["may_reveal_first_I"].as_bool().unwrap(),
        intent: ext(v, Role::Initiator).intent,
        tag: if key.is_empty() {
            None
        } else {
            Some(ContactTag::new(
                key.try_into().unwrap(),
                bytes(input, "tag_binding_I").try_into().unwrap(),
                bytes(input, "node_id_R").try_into().unwrap(),
            ))
        },
    }
}
fn tags(v: &Value) -> Vec<TagCheck> {
    v["inputs"]["tag_checks_R"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let (key, binding) = s.as_str().unwrap().split_once(':').unwrap();
            TagCheck::new(
                hex(key).try_into().unwrap(),
                hex(binding).try_into().unwrap(),
            )
        })
        .collect()
}
fn responder_options<'a>(v: &Value, tags: &'a [TagCheck]) -> ResponderOptions<'a> {
    ResponderOptions {
        mode: if v["inputs"]["mode_R"] == "EVERYONE" {
            ResponderMode::Everyone
        } else {
            ResponderMode::ContactsOnly
        },
        tags,
        opening: ext(v, Role::Responder).opening,
        discovery_epoch: Some(17),
    }
}
fn check(v: &Value, peer: &Peer, role: Role) -> Result<(), Error> {
    let suffix = if role == Role::Initiator { "I" } else { "R" };
    if peer.spki.as_slice() != bytes(&v["inputs"], &format!("identity_{suffix}_spki")) {
        return Err(Error::Policy);
    }
    assert_eq!(
        peer.node_id.as_slice(),
        bytes(&v["inputs"], &format!("node_id_{suffix}"))
    );
    Ok(())
}

#[test]
fn consuming_states_reproduce_all_five_android_scenarios_exactly() {
    for v in vectors()["vectors"].as_array().unwrap() {
        let ai = VectorSigner::new(v, Role::Initiator);
        let bi = VectorSigner::new(v, Role::Responder);
        let ar = real(v, Role::Initiator);
        let br = real(v, Role::Responder);
        let a = party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar));
        let b = party(v, Role::Responder, config(v, Role::Responder, &bi, &br));
        let mut guards = Guards::default();
        let tags = tags(v);
        let (a, m1) = Waiting2::begin(a, options(v)).unwrap();
        assert_eq!(ai.calls.get(), 0);
        assert_eq!(m1, bytes(&v["messages"], "m1"));
        let (b, m2) = Waiting3::begin(b, &m1, responder_options(v, &tags), &mut guards)
            .unwrap_or_else(|e| panic!("{} responder: {e}", v["name"]));
        assert_eq!(m2, bytes(&v["messages"], "m2"));
        let (progress, m3) = a.receive(&m2, |p| check(v, p, Role::Responder)).unwrap();
        assert_eq!(m3, bytes(&v["messages"], "m3"));
        let (bs, m4) = b.receive(&m3, |p, _| check(v, p, Role::Initiator)).unwrap();
        let a = match (progress, m4) {
            (InitiatorProgress::Complete(s), None) => s,
            (InitiatorProgress::Waiting4(a), Some(m4)) => {
                assert_eq!(m4, bytes(&v["messages"], "m4"));
                a.receive(&m4, |p| check(v, p, Role::Responder)).unwrap()
            }
            _ => panic!("wrong order"),
        };
        assert_eq!(a.keys.send_key(), bs.keys.receive_key());
        assert_eq!(a.keys.receive_key(), bs.keys.send_key());
        assert_eq!(a.keys.exporter(), bs.keys.exporter());
        assert_eq!(
            a.keys.send_key().as_slice(),
            bytes(&v["initiator_trace"], "app_i2r")
        );
        assert_eq!(
            a.keys.receive_key().as_slice(),
            bytes(&v["initiator_trace"], "app_r2i")
        );
        assert_eq!(
            a.keys.exporter().as_slice(),
            bytes(&v["initiator_trace"], "exporter")
        );
        assert_eq!(a.own_pseudonym(), ext(v, Role::Initiator).pseudonym);
        assert_eq!(bs.own_pseudonym(), ext(v, Role::Responder).pseudonym);
        assert_eq!(a.peer().public_key(), &bi.spki);
        assert_eq!(bs.peer().public_key(), &ai.spki);
        assert_eq!(a.own_public_key(), &ai.spki);
        assert_eq!(bs.own_public_key(), &bi.spki);
        assert_eq!(a.order(), bs.order());
        assert_eq!(bs.tag_matched(), !tags.is_empty());
        assert_eq!(ai.calls.get(), 1);
        assert_eq!(bi.calls.get(), 1);
        for (key, field) in [
            (a.keys.send_key(), "bms1_first_record_i2r"),
            (bs.keys.send_key(), "bms1_first_record_r2i"),
        ] {
            assert_eq!(
                Sealer::new(Layer::Hop, key, Policy::default())
                    .unwrap()
                    .seal(6, 0, &[1])
                    .unwrap(),
                bytes(v, field)
            );
        }
    }
}

#[test]
fn owner_refusal_and_tag_identity_mismatch_prevent_the_callers_signature() {
    let root = vectors();
    for index in [0, 2] {
        let v = &root["vectors"][index];
        let ai = VectorSigner::new(v, Role::Initiator);
        let ar = real(v, Role::Initiator);
        let mut options = options(v);
        if let Some(tag) = &mut options.tag {
            tag.expected_peer[0] ^= 1;
        }
        let (a, _) = Waiting2::begin(
            party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar)),
            options,
        )
        .unwrap();
        let called = Cell::new(false);
        let result = a.receive(&bytes(&v["messages"], "m2"), |_| {
            called.set(true);
            Err(Error::Policy)
        });
        assert!(matches!(result, Err(Error::Policy)));
        assert_eq!(ai.calls.get(), 0);
        assert_eq!(called.get(), index == 0);
    }
}
#[test]
fn default_and_tagged_initiators_refuse_ifirst_without_signing() {
    let root = vectors();
    let v = &root["vectors"][1];
    for tagged in [false, true] {
        let ai = VectorSigner::new(v, Role::Initiator);
        let ar = real(v, Role::Initiator);
        let mut opts = InitiatorOptions::default();
        if tagged {
            opts.may_reveal_first = true;
            opts.tag = Some(ContactTag::new([7; 32], [0; 11], [0; 16]));
        }
        let (a, _) = Waiting2::begin(
            party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar)),
            opts,
        )
        .unwrap();
        assert!(matches!(
            a.receive(&bytes(&v["messages"], "m2"), |_| Ok(())),
            Err(Error::Policy)
        ));
        assert_eq!(ai.calls.get(), 0);
    }
}
#[test]
fn ifirst_admission_withholds_responder_identity_and_msg4_check_withholds_keys() {
    let root = vectors();
    let v = &root["vectors"][1];
    let ar = real(v, Role::Initiator);
    let br = real(v, Role::Responder);
    let ai = VectorSigner::new(v, Role::Initiator);
    let bi = VectorSigner::new(v, Role::Responder);
    let mut guards = Guards::default();
    let (b, m2) = Waiting3::begin(
        party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
        &bytes(&v["messages"], "m1"),
        responder_options(v, &[]),
        &mut guards,
    )
    .unwrap();
    assert_eq!(m2.len(), 105);
    assert_eq!(bi.calls.get(), 0);
    assert!(matches!(
        b.receive(&bytes(&v["messages"], "m3"), |_, _| Err(Error::Policy)),
        Err(Error::Policy)
    ));
    assert_eq!(bi.calls.get(), 0);
    let (a, _) = Waiting2::begin(
        party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar)),
        options(v),
    )
    .unwrap();
    let (progress, _) = a
        .receive(&m2, |_| panic!("no responder block yet"))
        .unwrap();
    let InitiatorProgress::Waiting4(a) = progress else {
        panic!("must await msg4")
    };
    assert!(matches!(
        a.receive(&bytes(&v["messages"], "m4"), |_| Err(Error::Policy)),
        Err(Error::Policy)
    ));
}
#[test]
fn every_mutated_responder_message_fails_before_any_caller_identity_is_signed() {
    let root = vectors();
    let v = &root["vectors"][0];
    let m2 = bytes(&v["messages"], "m2");
    let ar = real(v, Role::Initiator);
    for index in 0..m2.len() {
        let ai = VectorSigner::new(v, Role::Initiator);
        let (a, _) = Waiting2::begin(
            party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar)),
            options(v),
        )
        .unwrap();
        let mut changed = m2.clone();
        changed[index] ^= 1;
        assert!(
            a.receive(&changed, |_| panic!("tampered responder reached policy"))
                .is_err(),
            "byte {index}"
        );
        assert_eq!(ai.calls.get(), 0);
    }
}

fn forged_m2(
    v: &Value,
    change: impl FnOnce(&mut [u8; 310]),
    sign_as: Option<Role>,
    fix_mac: bool,
) -> Vec<u8> {
    let original = bytes(&v["messages"], "m2");
    let m = wire::Message2::parse(&original, purpose(v)).unwrap();
    let mut block = schedule(v, Role::Initiator)
        .open(m.clear, m.sealed.unwrap())
        .unwrap();
    change(&mut block);
    let transcript = [bytes(&v["messages"], "m1"), m.clear.to_vec()].concat();
    if let Some(role) = sign_as {
        let signer = real(v, Role::Responder);
        let hash =
            keys::transcript_hash(purpose(v), &[transcript.as_slice(), &block[..214]].concat())
                .unwrap();
        block[214..278].copy_from_slice(
            &crypto::der_to_raw(
                &signer
                    .sign(&keys::signature_context(purpose(v), role, &hash))
                    .unwrap(),
            )
            .unwrap(),
        );
    }
    let mut keys = schedule(v, Role::Responder);
    if fix_mac {
        let hash =
            keys::transcript_hash(purpose(v), &[transcript.as_slice(), &block[..278]].concat())
                .unwrap();
        block[278..].copy_from_slice(&keys.finished_mac(true, &hash).unwrap()[..]);
    }
    [m.clear, &keys.seal(m.clear, &block).unwrap()].concat()
}
#[test]
fn valid_aead_cannot_hide_bad_signature_finished_mac_or_misplaced_extension() {
    let root = vectors();
    let v = &root["vectors"][0];
    let ar = real(v, Role::Initiator);
    let wrong_signature = forged_m2(v, |b| b[214] ^= 1, None, true);
    let wrong_role = forged_m2(v, |_| {}, Some(Role::Initiator), true);
    let wrong_mac = forged_m2(v, |b| b[278] ^= 1, None, false);
    let intent = forged_m2(
        v,
        |b| {
            let mut e = Extensions::default();
            e.intent = Some(1);
            b[102..214].copy_from_slice(&e.encode().unwrap());
        },
        Some(Role::Responder),
        true,
    );
    for (message, error) in [
        (wrong_signature, Error::Signature),
        (wrong_role, Error::Signature),
        (wrong_mac, Error::Mac),
        (intent, Error::Extensions),
    ] {
        let ai = VectorSigner::new(v, Role::Initiator);
        let (a, _) = Waiting2::begin(
            party(v, Role::Initiator, config(v, Role::Initiator, &ai, &ar)),
            options(v),
        )
        .unwrap();
        assert!(
            matches!(a.receive(&message,|_|panic!("invalid block reached policy")),Err(e) if e==error)
        );
        assert_eq!(ai.calls.get(), 0);
    }
}
#[test]
fn responder_replay_and_order_conflict_refuse_before_signing() {
    let root = vectors();
    let v = &root["vectors"][0];
    let br = real(v, Role::Responder);
    let m1 = bytes(&v["messages"], "m1");
    let bi = VectorSigner::new(v, Role::Responder);
    let mut guards = Guards::default();
    Waiting3::begin(
        party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
        &m1,
        responder_options(v, &[]),
        &mut guards,
    )
    .unwrap();
    assert_eq!(bi.calls.get(), 1);
    let bi = VectorSigner::new(v, Role::Responder);
    assert!(matches!(
        Waiting3::begin(
            party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
            &m1,
            responder_options(v, &[]),
            &mut guards
        ),
        Err(Error::Replay)
    ));
    assert_eq!(bi.calls.get(), 0);
    let mut guards = Guards::default();
    let mut opts = responder_options(v, &[]);
    opts.mode = ResponderMode::ContactsOnly;
    assert!(matches!(
        Waiting3::begin(
            party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
            &m1,
            opts,
            &mut guards
        ),
        Err(Error::Policy)
    ));
    assert_eq!(bi.calls.get(), 0);
}

#[test]
fn tampered_msg3_and_wrong_stage_messages_never_reach_admission() {
    let root = vectors();
    let v = &root["vectors"][0];
    let br = real(v, Role::Responder);
    let m1 = bytes(&v["messages"], "m1");
    let m3 = bytes(&v["messages"], "m3");
    let mut changes = vec![
        m1.clone(),
        bytes(&v["messages"], "m2"),
        m3[..m3.len() - 1].to_vec(),
    ];
    for index in [0, 5, 6, 7, 8, m3.len() - 1] {
        let mut changed = m3.clone();
        changed[index] ^= 1;
        changes.push(changed);
    }
    for changed in changes {
        let bi = VectorSigner::new(v, Role::Responder);
        let mut guards = Guards::default();
        let (b, _) = Waiting3::begin(
            party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
            &m1,
            responder_options(v, &[]),
            &mut guards,
        )
        .unwrap();
        assert!(
            b.receive(&changed, |_, _| panic!("invalid msg3 reached admission"))
                .is_err()
        );
        assert_eq!(bi.calls.get(), 1);
    }
}

#[test]
fn fresh_cng_handshakes_complete_both_orders_domains_and_pseudonym_presentations() {
    let root = vectors();
    let v = &root["vectors"][0];
    let pseudo = &root["vectors"][3];
    let ai = real(v, Role::Initiator);
    let bi = real(v, Role::Responder);
    let ap = presented_identity(pseudo, Role::Initiator);
    let bp = presented_identity(pseudo, Role::Responder);
    let caps = caps(v, Role::Initiator);
    for p in [Purpose::Link, Purpose::Endpoint] {
        for ifirst in [false, true] {
            for pseudonyms in [false, true] {
                let mut ac = Config::new(p, &ai, caps).unwrap();
                let mut bc = Config::new(p, &bi, caps).unwrap();
                if pseudonyms {
                    ac = ac.with_stranger(&ap, caps).unwrap();
                    bc = bc.with_stranger(&bp, caps).unwrap();
                }
                let (a, m1) = Waiting2::start(
                    ac,
                    InitiatorOptions {
                        may_reveal_first: ifirst,
                        ..Default::default()
                    },
                )
                .unwrap();
                let opts = ResponderOptions {
                    mode: if ifirst {
                        ResponderMode::ContactsOnly
                    } else {
                        ResponderMode::Everyone
                    },
                    discovery_epoch: Some(17),
                    ..Default::default()
                };
                let mut guards = Guards::default();
                let (b, m2) = Waiting3::respond(bc, &m1, opts, &mut guards).unwrap();
                let expected_a = if pseudonyms {
                    ap.public_key().unwrap()
                } else {
                    ai.public_key().unwrap()
                };
                let expected_b = if pseudonyms && !ifirst {
                    bp.public_key().unwrap()
                } else {
                    bi.public_key().unwrap()
                };
                let check = |peer: &Peer| {
                    if peer.spki == expected_b {
                        Ok(())
                    } else {
                        Err(Error::Policy)
                    }
                };
                let (progress, m3) = a.receive(&m2, check).unwrap();
                if ifirst && pseudonyms {
                    // A throwaway key cannot be an admitted contact. Even an
                    // overly permissive owner callback must not disclose msg4.
                    assert!(matches!(b.receive(&m3, |_, _| Ok(())), Err(Error::Policy)));
                    assert!(matches!(progress, InitiatorProgress::Waiting4(_)));
                    continue;
                }
                let (b, m4) = b
                    .receive(&m3, |peer, _| {
                        if peer.spki == expected_a {
                            Ok(())
                        } else {
                            Err(Error::Policy)
                        }
                    })
                    .unwrap();
                let a = match (progress, m4) {
                    (InitiatorProgress::Complete(s), None) => s,
                    (InitiatorProgress::Waiting4(a), Some(m4)) => a.receive(&m4, check).unwrap(),
                    _ => panic!("wrong order"),
                };
                assert_eq!(a.keys.send_key(), b.keys.receive_key());
                assert_eq!(a.keys.receive_key(), b.keys.send_key());
                assert_eq!(a.keys.exporter(), b.keys.exporter());
                assert_eq!(a.own_pseudonym(), pseudonyms);
                assert_eq!(b.own_pseudonym(), pseudonyms && !ifirst);
                assert!(!a.peer.extensions.pseudonym || a.own_pseudonym());
            }
        }
    }
}

struct CountSigner<'a> {
    inner: &'a dyn SigningIdentity,
    calls: Cell<usize>,
}
impl SigningIdentity for CountSigner<'_> {
    fn public_key(&self) -> Result<[u8; 91], crypto::Error> {
        self.inner.public_key()
    }
    fn sign(&self, bytes: &[u8]) -> Result<Vec<u8>, crypto::Error> {
        self.calls.set(self.calls.get() + 1);
        self.inner.sign(bytes)
    }
}
#[test]
fn self_connection_and_pseudonymous_answerer_to_real_caller_never_reveal_the_caller() {
    let root = vectors();
    let v = &root["vectors"][0];
    let other = &root["vectors"][3];
    let a = real(v, Role::Initiator);
    let b = real(v, Role::Responder);
    let pseudo = presented_identity(other, Role::Responder);
    let caps = caps(v, Role::Initiator);
    for self_connection in [false, true] {
        let a = CountSigner {
            inner: &a,
            calls: Cell::new(0),
        };
        let ac = Config::new(Purpose::Link, &a, caps).unwrap();
        let bc = if self_connection {
            Config::new(Purpose::Link, a.inner, caps).unwrap()
        } else {
            Config::new(Purpose::Link, &b, caps)
                .unwrap()
                .with_stranger(&pseudo, caps)
                .unwrap()
        };
        let (waiting, m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
        let mut guards = Guards::default();
        let (_, m2) = Waiting3::respond(
            bc,
            &m1,
            ResponderOptions {
                discovery_epoch: Some(17),
                ..Default::default()
            },
            &mut guards,
        )
        .unwrap();
        assert!(matches!(
            waiting.receive(&m2, |_| Ok(())),
            Err(Error::Policy)
        ));
        assert_eq!(a.calls.get(), 0);
    }
    assert!(matches!(
        Config::new(Purpose::Link, &a, caps)
            .unwrap()
            .with_stranger(&a, caps),
        Err(Error::Policy)
    ));
}

fn fresh_pair<'a>(
    p: Purpose,
    a: &'a CountSigner<'a>,
    b: &'a CountSigner<'a>,
    caps: Capabilities,
) -> (Config<'a>, Config<'a>) {
    (
        Config::new(p, a, caps).unwrap(),
        Config::new(p, b, caps).unwrap(),
    )
}
#[test]
fn android_fixture_transit_replay_and_reflection_negatives_fail_closed() {
    let root = vectors();
    let v = &root["vectors"][0];
    let (ai, bi) = (real(v, Role::Initiator), real(v, Role::Responder));
    let a = CountSigner {
        inner: &ai,
        calls: Cell::new(0),
    };
    let b = CountSigner {
        inner: &bi,
        calls: Cell::new(0),
    };
    let caps = caps(v, Role::Initiator);
    let everyone = || ResponderOptions {
        discovery_epoch: Some(17),
        ..Default::default()
    };
    // 1: a msg1 flag flipped in transit changes TH2, so the caller's AEAD fails
    // before it verifies, checks or signs anything.
    let (ac, bc) = fresh_pair(Purpose::Link, &a, &b, caps);
    let (waiting, mut m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
    m1[7] ^= 1;
    let (_, m2) = Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()).unwrap();
    assert!(matches!(
        waiting.receive(&m2, |_| panic!("flipped flag reached policy")),
        Err(Error::Aead(_))
    ));
    assert_eq!(a.calls.get(), 0);
    // 3: a link msg1 offered to an end-to-end responder is refused unsigned.
    let calls = b.calls.get();
    let (ac, _) = fresh_pair(Purpose::Link, &a, &b, caps);
    let (_, m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
    let bc = Config::new(Purpose::Endpoint, &b, caps).unwrap();
    assert!(matches!(
        Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()),
        Err(Error::Format)
    ));
    assert_eq!(b.calls.get(), calls);
    // 4: the purpose byte rewritten in both directions still splits the domains.
    let (ac, _) = fresh_pair(Purpose::Link, &a, &b, caps);
    let (waiting, mut m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
    m1[5] = Purpose::Endpoint as u8;
    let bc = Config::new(Purpose::Endpoint, &b, caps).unwrap();
    let (_, mut m2) = Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()).unwrap();
    m2[5] = Purpose::Link as u8;
    assert!(matches!(
        waiting.receive(&m2, |_| panic!("rewritten purpose reached policy")),
        Err(Error::Aead(_))
    ));
    assert_eq!(a.calls.get(), 0);
    // 5: a recorded msg1 + msg3 replayed into a fresh responder session (fresh
    // e_R, an evicted replay memory) never reaches admission.
    let (ac, bc) = fresh_pair(Purpose::Link, &a, &b, caps);
    let (waiting, m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
    let (first, m2) = Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()).unwrap();
    let (_, m3) = waiting.receive(&m2, |_| Ok(())).unwrap();
    first.receive(&m3, |_, _| Ok(())).unwrap();
    let bc = Config::new(Purpose::Link, &b, caps).unwrap();
    let (replayed, m2b) = Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()).unwrap();
    assert_ne!(m2b, m2);
    assert!(matches!(
        replayed.receive(&m3, |_, _| panic!("replayed msg3 reached admission")),
        Err(Error::Aead(_))
    ));
    // 7: the responder's own sealed block reflected as msg3.
    let (ac, bc) = fresh_pair(Purpose::Link, &a, &b, caps);
    let (_, m1) = Waiting2::start(ac, InitiatorOptions::default()).unwrap();
    let (responder, m2) = Waiting3::respond(bc, &m1, everyone(), &mut Guards::default()).unwrap();
    let reflected = [
        wire::header(Purpose::Link, 3, 0).unwrap().as_slice(),
        &m2[105..],
    ]
    .concat();
    assert!(matches!(
        responder.receive(&reflected, |_, _| panic!(
            "reflected block reached admission"
        )),
        Err(Error::Aead(_))
    ));
}
#[test]
fn reflected_responder_block_resealed_under_the_right_key_and_stale_tags_fail() {
    let root = vectors();
    // 7b: the vector's responder block, re-sealed as msg3 under k_hs_i2r, fails
    // on the role-labelled signature before admission.
    let v = &root["vectors"][0];
    let br = real(v, Role::Responder);
    let bi = VectorSigner::new(v, Role::Responder);
    let m1 = bytes(&v["messages"], "m1");
    let m2 = bytes(&v["messages"], "m2");
    let (b, _) = Waiting3::begin(
        party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
        &m1,
        responder_options(v, &[]),
        &mut Guards::default(),
    )
    .unwrap();
    let m = wire::Message2::parse(&m2, Purpose::Link).unwrap();
    let mut initiator = schedule(v, Role::Initiator);
    let block = initiator.open(m.clear, m.sealed.unwrap()).unwrap();
    let h3 = wire::header(Purpose::Link, 3, 0).unwrap();
    let mut initiator = schedule(v, Role::Initiator);
    let resealed = [h3.as_slice(), &initiator.seal(&h3, &block).unwrap()].concat();
    assert!(matches!(
        b.receive(&resealed, |_, _| panic!(
            "reflected identity reached admission"
        )),
        Err(Error::Signature)
    ));
    // 2c: a recorded tag replayed against the responder's next-epoch advert does
    // not match, so a Contacts-only responder demands I_FIRST and closes on a
    // caller that refuses it, without signing. The current binding still matches.
    let v = &root["vectors"][2];
    let constants = &root["constants"];
    let k_tag: [u8; 32] = bytes(constants, "k_tag").try_into().unwrap();
    let current: [u8; 11] = bytes(constants, "vector_binding").try_into().unwrap();
    let next_rdi: [u8; 6] = hex("67aba670e65a").try_into().unwrap();
    let next = crate::rotating::advert_binding(&next_rdi, 0, None);
    assert_eq!(
        crate::rotating::advert_binding(&current[..6].try_into().unwrap(), 0, None),
        current
    );
    let m1 = bytes(&v["messages"], "m1");
    let br = real(v, Role::Responder);
    for (binding, matched) in [(next, false), (current, true)] {
        let bi = VectorSigner::new(v, Role::Responder);
        let tags = [TagCheck::new(k_tag, binding)];
        let options = ResponderOptions {
            mode: ResponderMode::ContactsOnly,
            tags: &tags,
            opening: ext(v, Role::Responder).opening,
            discovery_epoch: Some(17),
        };
        let result = Waiting3::begin(
            party(v, Role::Responder, config(v, Role::Responder, &bi, &br)),
            &m1,
            options,
            &mut Guards::default(),
        );
        if matched {
            let (b, _) = result.unwrap();
            assert!(b.tag_matched && b.order == Order::ResponderFirst);
            assert_eq!(bi.calls.get(), 1);
        } else {
            assert!(matches!(result, Err(Error::Policy)));
            assert_eq!(bi.calls.get(), 0);
        }
    }
}
