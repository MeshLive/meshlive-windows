use meshlive::{
    framing::{Error, HandshakeReader, HopReader, frame_handshake},
    records::{self, Layer, Policy, Sealer},
};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/android-vectors.json")).unwrap()
}
fn hex(value: &Value) -> Vec<u8> {
    value
        .as_str()
        .unwrap()
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn reference_hop_stream_survives_fragmentation_and_coalescing() {
    let v = vectors();
    let stream = hex(&v["bms1Stream"]);
    let key: [u8; 32] = hex(&v["link"]["initiatorToResponderKey"])
        .try_into()
        .unwrap();
    let expected = v["bms1Records"].as_array().unwrap();
    for size in [1, 7, 17, 512, 65536] {
        let mut reader = HopReader::new(&key, Policy::default()).unwrap();
        let mut count = 0;
        for chunk in stream.chunks(size) {
            let mut input = chunk;
            while !input.is_empty() {
                if let Some(record) = reader.read(&mut input, usize::MAX).unwrap() {
                    assert_eq!(record.header.sequence, count as u64);
                    assert_eq!(
                        record.header.kind,
                        expected[count]["type"].as_u64().unwrap() as u8
                    );
                    assert_eq!(record.plaintext, hex(&expected[count]["payload"]));
                    count += 1;
                }
            }
        }
        assert_eq!(count, 18);
        reader.finish().unwrap();
    }
}

#[test]
fn handshake_reference_framing_leaves_the_next_protocol_bytes_untouched() {
    let v = vectors();
    let frame = hex(&v["framedHelloExample"]);
    assert_eq!(frame_handshake(&frame[4..]).unwrap(), frame);
    for size in [1, 3, 4, 17, 512, 4096] {
        let mut reader = HandshakeReader::default();
        let mut messages = Vec::new();
        for chunk in frame.chunks(size) {
            let mut input = chunk;
            while !input.is_empty() {
                if let Some(message) = reader.read(&mut input).unwrap() {
                    messages.push(message);
                }
            }
        }
        assert_eq!(messages, [frame[4..].to_vec()]);
        reader.finish().unwrap();
    }
    let mut combined = frame.clone();
    combined.extend_from_slice(b"BMS1 next protocol");
    let mut input = combined.as_slice();
    let mut reader = HandshakeReader::default();
    assert_eq!(reader.read(&mut input).unwrap(), Some(frame[4..].to_vec()));
    assert_eq!(input, b"BMS1 next protocol");
    reader.finish().unwrap();
}

#[test]
fn handshake_exact_boundaries_coalescing_and_invalid_lengths() {
    let mut combined = frame_handshake(b"a").unwrap();
    combined.extend(frame_handshake(&[7; 2048]).unwrap());
    let mut reader = HandshakeReader::default();
    let mut input = combined.as_slice();
    assert_eq!(reader.read(&mut input).unwrap(), Some(b"a".to_vec()));
    assert_eq!(reader.read(&mut input).unwrap(), Some(vec![7; 2048]));
    assert!(input.is_empty());
    reader.finish().unwrap();

    for size in [0_u32, 2049, 0x80000000, u32::MAX] {
        let mut reader = HandshakeReader::default();
        let prefix = size.to_be_bytes();
        assert_eq!(reader.read(&mut prefix.as_slice()), Err(Error::Length));
        assert_eq!(reader.read(&mut &[][..]), Err(Error::Faulted));
        assert_eq!(reader.finish(), Err(Error::Faulted));
    }
    assert_eq!(frame_handshake(&[]), Err(Error::Length));
    assert_eq!(frame_handshake(&[0; 2049]), Err(Error::Length));
}

#[test]
fn every_partial_frame_fails_at_eof_but_an_empty_stream_is_complete() {
    let frame = frame_handshake(b"public handshake fixture").unwrap();
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let record = seal.seal(6, 0, b"public record fixture").unwrap();
    HandshakeReader::default().finish().unwrap();
    HopReader::new(&[0; 32], Policy::default())
        .unwrap()
        .finish()
        .unwrap();
    for cut in 1..frame.len() {
        let mut reader = HandshakeReader::default();
        assert_eq!(reader.read(&mut &frame[..cut]).unwrap(), None);
        assert_eq!(reader.finish(), Err(Error::Truncated));
    }
    for cut in 1..record.len() {
        let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
        assert_eq!(reader.read(&mut &record[..cut], usize::MAX).unwrap(), None);
        assert_eq!(reader.finish(), Err(Error::Truncated));
    }
}

#[test]
fn coalesced_records_allow_owner_validation_before_audio_cap_changes() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    // These are synthetic payloads, not source-signed audio headers. The session
    // owner's separate source verification is deliberately outside this test.
    let first = seal.seal(5, 0, b"small metadata fixture").unwrap();
    let second = seal.seal(5, 0, &[0; 5000]).unwrap();
    let combined = [first.as_slice(), second.as_slice()].concat();
    let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
    let mut input = combined.as_slice();
    assert_eq!(
        reader.read(&mut input, 4096).unwrap().unwrap().plaintext,
        b"small metadata fixture"
    );
    assert_eq!(input, second);
    assert_eq!(
        reader
            .read(&mut input, 28 * 1024)
            .unwrap()
            .unwrap()
            .plaintext
            .len(),
        5000
    );
    reader.finish().unwrap();

    let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
    let mut input = combined.as_slice();
    assert!(reader.read(&mut input, 4096).unwrap().is_some());
    assert_eq!(
        reader.read(&mut input, 4096),
        Err(Error::Record(records::Error::Size))
    );
    // Only the invalid header was consumed; no body copy/allocation is needed.
    assert_eq!(input, &second[17..]);
    assert_eq!(reader.read(&mut input, usize::MAX), Err(Error::Faulted));
}

#[test]
fn unauthenticated_first_record_never_delivers_or_consumes_the_next_record() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let mut first = seal.seal(5, 0, b"metadata").unwrap();
    let second = seal.seal(5, 0, &[0; 5000]).unwrap();
    *first.last_mut().unwrap() ^= 1;
    let combined = [first.as_slice(), second.as_slice()].concat();
    let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
    let mut input = combined.as_slice();
    assert!(matches!(
        reader.read(&mut input, 4096),
        Err(Error::Record(records::Error::Crypto(_)))
    ));
    assert_eq!(input, second);
    assert_eq!(reader.read(&mut input, 28 * 1024), Err(Error::Faulted));
    assert_eq!(reader.finish(), Err(Error::Faulted));
}

#[test]
fn wrong_sequence_or_type_fails_before_consuming_the_body() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let first = seal.seal(6, 0, b"one").unwrap();
    let second = seal.seal(6, 0, b"two").unwrap();
    let mut wrong_type = first.clone();
    wrong_type[4] = 255;
    for (record, error) in [
        (&second, records::Error::Sequence),
        (&wrong_type, records::Error::Type),
    ] {
        let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
        let mut input = record.as_slice();
        assert_eq!(
            reader.read(&mut input, usize::MAX),
            Err(Error::Record(error))
        );
        assert_eq!(input, &record[17..]);
    }
    let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
    assert!(
        reader
            .read(&mut first.as_slice(), usize::MAX)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        reader.read(&mut first.as_slice(), usize::MAX),
        Err(Error::Record(records::Error::Sequence))
    );
}

#[test]
fn immutable_policy_cap_cannot_be_relaxed_by_a_read_argument() {
    let mut seal = Sealer::new(Layer::Hop, &[0; 32], Policy::default()).unwrap();
    let record = seal.seal(5, 0, &[0; 4096]).unwrap();
    let policy = Policy {
        maximum_ciphertext: 4096,
        ..Policy::default()
    };
    let mut reader = HopReader::new(&[0; 32], policy).unwrap();
    assert_eq!(
        reader.read(&mut record.as_slice(), usize::MAX),
        Err(Error::Record(records::Error::Size))
    );
}
