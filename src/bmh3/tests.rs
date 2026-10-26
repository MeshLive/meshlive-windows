use super::{keys::*, wire::*, *};
use crate::{
    bmh2::{Capabilities, Role},
    crypto::{self, tests::fixture_ephemeral},
    records::{Layer, Policy, Sealer},
};
use serde_json::Value;
use subtle::ConstantTimeEq;

pub(crate) fn vectors() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/bmh3-vectors.json")).unwrap()
}
pub(crate) fn hex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0);
    s.as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
pub(crate) fn bytes(v: &Value, key: &str) -> Vec<u8> {
    hex(v[key].as_str().unwrap())
}
pub(crate) fn scalar(v: &Value, key: &str) -> [u8; 32] {
    let value = v[key].as_str().unwrap().trim_start_matches("0x");
    let padded = format!("{value:0>64}");
    hex(&padded).try_into().unwrap()
}
pub(crate) fn purpose(v: &Value) -> Purpose {
    if v["purpose"].as_str().unwrap() == "Link" {
        Purpose::Link
    } else {
        Purpose::Endpoint
    }
}
pub(crate) fn schedule(v: &Value, role: Role) -> HandshakeKeys {
    let p = purpose(v);
    let m1 = bytes(&v["messages"], "m1");
    let m2 = bytes(&v["messages"], "m2");
    let a = Message1::parse(&m1, p).unwrap();
    let b = Message2::parse(&m2, p).unwrap();
    let (own, peer, key) = if role == Role::Initiator {
        (a.point, b.point, "ephemeral_I_scalar")
    } else {
        (b.point, a.point, "ephemeral_R_scalar")
    };
    let spki = spki_from_point(own).unwrap();
    HandshakeKeys::derive(
        p,
        role,
        fixture_ephemeral(&spki, &scalar(&v["inputs"], key)),
        peer,
        &[m1.as_slice(), b.clear].concat(),
    )
    .unwrap()
}
fn check_block(
    v: &Value,
    receiver: &HandshakeKeys,
    transcript: &[u8],
    carrier: &[u8],
    block: &[u8; 310],
    role: Role,
    number: u8,
    order: Order,
) {
    let p = purpose(v);
    let who = if role == Role::Initiator {
        "initiator_trace"
    } else {
        "responder_trace"
    };
    let suffix = if role == Role::Initiator { "I" } else { "R" };
    let t = &v[who];
    assert_eq!(&block[..2], &91_u16.to_be_bytes());
    assert_eq!(&block[100..102], &112_u16.to_be_bytes());
    let caps = Capabilities::decode(&block[93..100]).unwrap();
    assert_eq!(
        caps.encode().as_slice(),
        bytes(&v["inputs"], &format!("caps_{suffix}"))
    );
    let ext = Extensions::parse(&block[102..214]).unwrap();
    ext.check_placement(p, role, number, order).unwrap();
    assert_eq!(
        ext.encode().unwrap().as_slice(),
        bytes(&v["inputs"], &format!("extensions_{suffix}"))
    );
    let sig_hash = transcript_hash(p, &[transcript, carrier, &block[..214]].concat()).unwrap();
    assert_eq!(sig_hash.as_slice(), bytes(t, &format!("th_sig_{suffix}")));
    let signature: &[u8; 64] = block[214..278].try_into().unwrap();
    assert_eq!(signature.as_slice(), bytes(t, &format!("sig_{suffix}")));
    crypto::verify_p256_raw(
        &block[2..93],
        &signature_context(p, role, &sig_hash),
        signature,
    )
    .unwrap();
    let mac_hash = transcript_hash(p, &[transcript, carrier, &block[..278]].concat()).unwrap();
    assert_eq!(mac_hash.as_slice(), bytes(t, &format!("th_mac_{suffix}")));
    let mac = receiver.finished_mac(false, &mac_hash).unwrap();
    assert!(bool::from(mac[..].ct_eq(&block[278..])));
    assert_eq!(mac.as_slice(), bytes(t, &format!("mac_{suffix}")));
}

#[test]
fn all_five_android_vectors_match_key_schedule_blocks_messages_and_first_records() {
    for v in vectors()["vectors"].as_array().unwrap() {
        let p = purpose(v);
        let m = &v["messages"];
        let m1 = bytes(m, "m1");
        let m2 = bytes(m, "m2");
        let m3 = bytes(m, "m3");
        let first = Message1::parse(&m1, p).unwrap();
        let second = Message2::parse(&m2, p).unwrap();
        let order = if second.responder_first {
            Order::ResponderFirst
        } else {
            Order::InitiatorFirst
        };
        let mut transcript = [m1.as_slice(), second.clear].concat();
        let mut a = schedule(v, Role::Initiator);
        let mut b = schedule(v, Role::Responder);
        let own = spki_from_point(first.point).unwrap();
        let dh = fixture_ephemeral(&own, &scalar(&v["inputs"], "ephemeral_I_scalar"))
            .agree(&spki_from_point(second.point).unwrap())
            .unwrap();
        let th2 = transcript_hash(p, &transcript).unwrap();
        let hs = crypto::hmac_sha256(&th2, &dh[..]).unwrap();
        let trace = &v["initiator_trace"];
        assert_eq!(dh.as_slice(), bytes(trace, "dh"));
        assert_eq!(th2.as_slice(), bytes(trace, "th2"));
        assert_eq!(hs.as_slice(), bytes(trace, "hs"));
        for (label_name, field) in [
            ("hs i2r", "hs_i2r"),
            ("hs r2i", "hs_r2i"),
            ("finished i", "finished_i"),
            ("finished r", "finished_r"),
            ("derived", "derived"),
        ] {
            assert_eq!(
                label(p, &hs, label_name).unwrap().as_slice(),
                bytes(trace, field)
            );
        }
        if let Some(sealed) = second.sealed {
            let block = a.open(second.clear, sealed).unwrap();
            check_block(v, &a, &transcript, &[], &block, Role::Responder, 2, order);
            assert_eq!(b.seal(second.clear, &block).unwrap(), *sealed);
            transcript.extend_from_slice(&block[..]);
        }
        let (h3, c3) = parse_encrypted(&m3, p, 3).unwrap();
        let block = b.open(h3, c3).unwrap();
        check_block(v, &b, &transcript, h3, &block, Role::Initiator, 3, order);
        assert_eq!(a.seal(h3, &block).unwrap(), *c3);
        transcript.extend_from_slice(h3);
        transcript.extend_from_slice(&block[..]);
        if let Some(m4) = m["m4"].as_str() {
            let m4 = hex(m4);
            let (h4, c4) = parse_encrypted(&m4, p, 4).unwrap();
            let block = a.open(h4, c4).unwrap();
            check_block(v, &a, &transcript, h4, &block, Role::Responder, 4, order);
            assert_eq!(b.seal(h4, &block).unwrap(), *c4);
            transcript.extend_from_slice(h4);
            transcript.extend_from_slice(&block[..]);
        }
        let th_final = transcript_hash(p, &transcript).unwrap();
        assert_eq!(th_final.as_slice(), bytes(trace, "th_final"));
        let ms = crypto::hmac_sha256(&th_final, &label(p, &hs, "derived").unwrap()[..]).unwrap();
        assert_eq!(ms.as_slice(), bytes(trace, "ms"));
        let a = a.application(&transcript).unwrap();
        let b = b.application(&transcript).unwrap();
        assert_eq!(a.send_key().as_slice(), bytes(trace, "app_i2r"));
        assert_eq!(a.receive_key().as_slice(), bytes(trace, "app_r2i"));
        assert_eq!(a.send_key(), b.receive_key());
        assert_eq!(a.receive_key(), b.send_key());
        assert_eq!(a.exporter().as_slice(), bytes(trace, "exporter"));
        assert_eq!(a.exporter(), b.exporter());
        // The reference pins BMS1 ciphertext under these keys even for its endpoint
        // purpose vector; it does not label that as a complete endpoint session.
        for (key, field) in [
            (a.send_key(), "bms1_first_record_i2r"),
            (b.send_key(), "bms1_first_record_r2i"),
        ] {
            let expected = bytes(v, field);
            let mut seal = Sealer::new(Layer::Hop, key, Policy::default()).unwrap();
            assert_eq!(seal.seal(6, 0, &[1]).unwrap(), expected);
        }
    }
}

#[test]
fn native_hmac_extract_expand_and_contact_tag_keys_match_published_values() {
    assert_eq!(
        crypto::hmac_sha256(&[0x0b; 20], b"Hi There")
            .unwrap()
            .as_slice(),
        hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
    );
    let prk: [u8; 32] = hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5")
        .try_into()
        .unwrap();
    assert_eq!(
        crypto::hkdf_expand::<42>(&prk, &hex("f0f1f2f3f4f5f6f7f8f9"))
            .unwrap()
            .as_slice(),
        hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865")
    );
    let v = vectors();
    let c = &v["constants"];
    for (key, info, result) in [
        ("lease_key", "contact_tag_info", "k_tag"),
        ("k_qr", "qr_tag_info", "k_tag_qr"),
    ] {
        let key: [u8; 32] = bytes(c, key).try_into().unwrap();
        assert_eq!(
            crypto::hkdf_expand::<32>(&key, &bytes(c, info))
                .unwrap()
                .as_slice(),
            bytes(c, result)
        );
    }
}

#[test]
fn handshake_ciphers_are_one_use_and_failed_authentication_faults_both_directions() {
    let root = vectors();
    let v = &root["vectors"][0];
    let m2 = bytes(&v["messages"], "m2");
    let parsed = Message2::parse(&m2, purpose(v)).unwrap();
    let mut a = schedule(v, Role::Initiator);
    let sealed = parsed.sealed.unwrap();
    a.open(parsed.clear, sealed).unwrap();
    assert!(matches!(a.open(parsed.clear, sealed), Err(Error::State)));
    assert!(matches!(
        a.seal(&header(purpose(v), 3, 0).unwrap(), &[0; 310]),
        Err(Error::State)
    ));
    assert!(matches!(a.application(b"unfinished"), Err(Error::State)));
    for index in 0..326 {
        let mut changed = *sealed;
        changed[index] ^= 1;
        let mut a = schedule(v, Role::Initiator);
        assert!(a.open(parsed.clear, &changed).is_err());
        assert!(matches!(a.open(parsed.clear, sealed), Err(Error::State)));
        assert!(matches!(a.finished_mac(false, &[0; 32]), Err(Error::State)));
    }
    let mut b = schedule(v, Role::Responder);
    b.seal(parsed.clear, &[0; 310]).unwrap();
    assert!(matches!(b.seal(parsed.clear, &[0; 310]), Err(Error::State)));
}

#[test]
fn fixed_messages_reject_truncation_trailers_wrong_purpose_flags_suite_and_points() {
    let root = vectors();
    let v = &root["vectors"][0];
    let p = purpose(v);
    for (name, number) in [("m1", 1), ("m2", 2), ("m3", 3)] {
        let original = bytes(&v["messages"], name);
        let parse = |b: &[u8]| match number {
            1 => Message1::parse(b, p).map(|_| ()),
            2 => Message2::parse(b, p).map(|_| ()),
            _ => parse_encrypted(b, p, 3).map(|_| ()),
        };
        for cut in 0..original.len() {
            assert!(parse(&original[..cut]).is_err());
        }
        assert!(parse(&[original.as_slice(), &[0]].concat()).is_err());
        for (index, value) in [(0, 0), (4, 2), (5, 2), (6, 0), (7, 0x80)] {
            let mut changed = original.clone();
            changed[index] = value;
            assert!(parse(&changed).is_err());
        }
        if number == 1 {
            let mut changed = original.clone();
            changed[8] = 2;
            assert!(parse(&changed).is_err());
            changed = original.clone();
            changed[10..74].fill(0);
            assert!(parse(&changed).is_err());
            changed = original.clone();
            changed[10..42].fill(0xff);
            assert!(parse(&changed).is_err());
        }
    }
    assert!(header(p, 0, 0).is_err());
    assert!(header(p, 4, 1).is_err());
}

fn padded(tlvs: &[u8]) -> [u8; 112] {
    let spare = 112 - tlvs.len();
    assert!(spare >= 3);
    let mut area = [0; 112];
    area[1..3].copy_from_slice(&((spare - 3) as u16).to_be_bytes());
    area[spare..].copy_from_slice(tlvs);
    area
}
#[test]
fn extensions_reject_malformed_duplicate_out_of_order_critical_and_misplaced_values() {
    for count in 0..=2 {
        let mut e = Extensions::default();
        e.opening = Some(vec![0; 1 + 43 * count]);
        e.opening.as_mut().unwrap()[0] = count as u8;
        assert_eq!(
            Extensions::parse(&e.encode().unwrap()).unwrap().opening,
            e.opening
        );
    }
    for value in [&[][..], &[3][..], &[1][..], &[0, 0][..]] {
        let mut e = Extensions::default();
        e.opening = Some(value.to_vec());
        assert!(e.encode().is_err());
    }
    for raw in [
        vec![1, 0, 0],
        vec![1, 0, 1, 3],
        vec![2, 0, 0],
        vec![2, 0, 2, 1, 1],
        vec![3, 0, 1, 0],
        vec![4, 0, 1, 0],
        vec![2, 0, 1, 1, 2, 0, 1, 1],
        vec![4, 0, 0, 2, 0, 1, 1],
        vec![128, 0, 0],
    ] {
        assert!(Extensions::parse(&padded(&raw)).is_err());
    }
    assert!(Extensions::parse(&padded(&[5, 0, 1, 7])).is_ok());
    let mut nonzero = Extensions::default().encode().unwrap();
    nonzero[3] = 1;
    assert!(Extensions::parse(&nonzero).is_err());
    for cut in 0..112 {
        assert!(Extensions::parse(&nonzero[..cut]).is_err());
    }
    let bad_tag = Extensions::parse(&padded(&[3, 0, 2, 0, 0])).unwrap();
    assert_eq!(bad_tag.build_tag, None);
    assert!(
        bad_tag
            .check_placement(Purpose::Endpoint, Role::Responder, 2, Order::ResponderFirst)
            .is_err()
    );
    let intent = Extensions::parse(&padded(&[2, 0, 1, 1])).unwrap();
    assert!(
        intent
            .check_placement(Purpose::Link, Role::Initiator, 3, Order::ResponderFirst)
            .is_err()
    );
    intent
        .check_placement(Purpose::Endpoint, Role::Initiator, 3, Order::InitiatorFirst)
        .unwrap();
    let opening = Extensions::parse(&padded(&[1, 0, 1, 0, 4, 0, 0])).unwrap();
    assert!(
        opening
            .check_placement(Purpose::Link, Role::Responder, 2, Order::ResponderFirst)
            .is_err()
    );
}
