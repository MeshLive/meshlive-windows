use meshlive::records::{Error, Header, Layer, Opener, Policy, Sealer};
use serde_json::Value;
fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/android-vectors.json")).unwrap()
}
fn hex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0);
    s.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn bytes(v: &Value, name: &str) -> Vec<u8> {
    hex(v[name].as_str().unwrap())
}

#[test]
fn every_endpoint_ciphertext_matches_the_published_reference() {
    let v = vectors();
    let key: [u8; 32] = bytes(&v["endpoint"], "initiatorToResponderKey")
        .try_into()
        .unwrap();
    let mut seal = Sealer::new(Layer::Endpoint, &key, Policy::default()).unwrap();
    let mut open = Opener::new(Layer::Endpoint, &key, Policy::default()).unwrap();
    for entry in v["bme1Contents"].as_array().unwrap() {
        let expected = bytes(entry, "sealed");
        let plain = bytes(entry, "plaintext");
        assert_eq!(
            seal.seal(
                entry["type"].as_u64().unwrap() as u8,
                entry["flags"].as_u64().unwrap() as u8,
                &plain
            )
            .unwrap(),
            expected
        );
        assert_eq!(open.open(&expected).unwrap().plaintext, plain);
    }
    assert_eq!(open.next_sequence(), 13);
}
#[test]
fn every_hop_record_and_combined_stream_match_the_published_reference() {
    let v = vectors();
    let key: [u8; 32] = bytes(&v["link"], "initiatorToResponderKey")
        .try_into()
        .unwrap();
    let mut seal = Sealer::new(Layer::Hop, &key, Policy::default()).unwrap();
    let mut open = Opener::new(Layer::Hop, &key, Policy::default()).unwrap();
    let mut stream = Vec::new();
    for entry in v["bms1Records"].as_array().unwrap() {
        let plain = bytes(entry, "payload");
        let record = seal
            .seal(entry["type"].as_u64().unwrap() as u8, 0, &plain)
            .unwrap();
        assert_eq!(open.open(&record).unwrap().plaintext, plain);
        stream.extend(record);
    }
    assert_eq!(stream, bytes(&v, "bms1Stream"));
    assert_eq!(open.next_sequence(), 18);
}
#[test]
fn empty_authenticated_records_are_not_size_queries() {
    for (layer, kind) in [(Layer::Hop, 6), (Layer::Endpoint, 8)] {
        let mut seal = Sealer::new(layer, &[0; 32], Policy::default()).unwrap();
        let mut open = Opener::new(layer, &[0; 32], Policy::default()).unwrap();
        let record = seal.seal(kind, 0, &[]).unwrap();
        assert_eq!(record.len(), layer.header_bytes() + 16);
        assert_eq!(open.open(&record).unwrap().plaintext, Vec::<u8>::new());
    }
}
#[test]
fn every_tampered_byte_fails_and_permanently_faults_the_direction() {
    for (layer, kind) in [(Layer::Hop, 6), (Layer::Endpoint, 2)] {
        let mut seal = Sealer::new(layer, &[0; 32], Policy::default()).unwrap();
        let record = seal.seal(kind, 0, b"public fixture").unwrap();
        for index in 0..record.len() {
            let mut changed = record.clone();
            changed[index] ^= 1;
            let mut open = Opener::new(layer, &[0; 32], Policy::default()).unwrap();
            assert!(open.open(&changed).is_err(), "accepted byte {index}");
            assert_eq!(open.open(&record), Err(Error::Faulted));
        }
    }
}
#[test]
fn endpoint_gaps_work_but_hop_gaps_and_replay_never_do() {
    for layer in [Layer::Endpoint, Layer::Hop] {
        let kind = if layer == Layer::Hop { 6 } else { 2 };
        let mut seal = Sealer::new(layer, &[0; 32], Policy::default()).unwrap();
        let first = seal.seal(kind, 0, b"one").unwrap();
        let _ = seal.seal(kind, 0, b"two").unwrap();
        let third = seal.seal(kind, 0, b"three").unwrap();
        let mut open = Opener::new(layer, &[0; 32], Policy::default()).unwrap();
        if layer == Layer::Endpoint {
            assert_eq!(open.open(&third).unwrap().plaintext, b"three");
            assert_eq!(open.open(&first), Err(Error::Sequence));
        } else {
            assert_eq!(open.open(&third), Err(Error::Sequence));
        }
        assert_eq!(open.open(&first), Err(Error::Faulted));
    }
}
#[test]
fn truncated_and_trailing_records_fail_without_a_retry() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let record = seal.seal(6, 0, b"fixture").unwrap();
    for length in 0..record.len() {
        let mut open = Opener::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
        assert!(open.open(&record[..length]).is_err());
        assert_eq!(open.open(&record), Err(Error::Faulted));
    }
    let mut extra = record.clone();
    extra.push(0);
    let mut open = Opener::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    assert_eq!(open.open(&extra), Err(Error::Size));
}
#[test]
fn removed_and_unnegotiated_types_flags_and_opening_context_are_rejected() {
    let mut hop = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    for kind in [0, 1, 2, 3, 11, 12, 13, 255] {
        assert_eq!(hop.seal(kind, 0, b"x"), Err(Error::Type));
    }
    assert_eq!(hop.next_sequence(), 0);
    assert_eq!(hop.seal(6, 1, b"x"), Err(Error::Flags));
    let mut endpoint = Sealer::new(Layer::Endpoint, &[0; 32], Policy::default()).unwrap();
    for kind in [0, 3, 9, 10, 11, 255] {
        assert_eq!(endpoint.seal(kind, 0, b"x"), Err(Error::Type));
    }
    assert_eq!(endpoint.seal(6, 1, b"x"), Err(Error::Flags));
    let mut opening = Sealer::new(
        Layer::Hop,
        &[0; 32],
        Policy {
            require_opening: true,
            ..Policy::default()
        },
    )
    .unwrap();
    assert_eq!(opening.seal(4, 0, b"x"), Err(Error::Type));
    opening.seal(13, 0, b"opening fixture").unwrap();
    assert_eq!(opening.seal(13, 0, b"again"), Err(Error::Type));
    opening.seal(4, 0, b"hello fixture").unwrap();
}
#[test]
fn header_only_oversize_and_stream_role_caps_reject_before_body_allocation() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let record = seal.seal(6, 0, b"fixture").unwrap();
    let mut header = record[..17].to_vec();
    for size in [
        0u32,
        15,
        (Layer::Hop.maximum_ciphertext() + 1) as u32,
        u32::MAX,
    ] {
        header[13..17].copy_from_slice(&size.to_be_bytes());
        assert_eq!(
            Header::parse(Layer::Hop, &header, Policy::default()),
            Err(Error::Size)
        );
    }
    header[13..17].copy_from_slice(&4097u32.to_be_bytes());
    assert_eq!(
        Header::parse(
            Layer::Hop,
            &header,
            Policy {
                maximum_ciphertext: 4096,
                ..Policy::default()
            }
        ),
        Err(Error::Size)
    );
}
#[test]
fn exact_maximum_records_work_and_larger_records_refuse() {
    for (layer, kind) in [(Layer::Hop, 6), (Layer::Endpoint, 2)] {
        let mut seal = Sealer::new(layer, &[0; 32], Policy::default()).unwrap();
        let mut open = Opener::new(layer, &[0; 32], Policy::default()).unwrap();
        let plain = vec![0x5a; layer.maximum_ciphertext() - 16];
        assert_eq!(
            open.open(&seal.seal(kind, 0, &plain).unwrap())
                .unwrap()
                .plaintext,
            plain
        );
        assert_eq!(
            seal.seal(kind, 0, &vec![0; plain.len() + 1]),
            Err(Error::Size)
        );
    }
}
