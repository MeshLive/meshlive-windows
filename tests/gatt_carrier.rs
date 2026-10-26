use meshlive::{
    bluetooth::carrier::{Error, LinkIo, Pipe, Writer},
    framing::{HandshakeReader, HopReader},
    records::{Layer, Policy, Sealer},
};
use serde_json::Value;
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

struct FakeRadio {
    capacity: usize,
    packets: Vec<Vec<u8>>,
}
impl LinkIo for FakeRadio {
    fn capacity(&self) -> Result<usize, Error> {
        Ok(self.capacity)
    }
    fn send(&mut self, value: &[u8], _: Instant, _: &AtomicBool) -> Result<(), Error> {
        self.packets.push(value.to_vec());
        Ok(())
    }
    fn abort(&mut self) {}
}
fn hex(value: &Value) -> Vec<u8> {
    let text = value.as_str().unwrap();
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

#[test]
fn source_signed_audio_decodes_to_pcm_after_fake_gatt_but_tampering_never_does() {
    use meshlive::{
        crypto,
        mesh::{Envelope, Grant},
        opus::{Decoder, Profile},
        watch::{Event, Header, Reader},
    };
    let v: Value = serde_json::from_str(include_str!("fixtures/audio-live-vectors.json")).unwrap();
    let source: [u8; 16] = crypto::sha256(&hex(&v["identity_spki"])).unwrap()[..16]
        .try_into()
        .unwrap();
    let stream: [u8; 16] = hex(&v["stream_id"]).try_into().unwrap();
    // Public fixture keys/full source are installed explicitly, not application admission.
    let mut raw_grant = [0; 68];
    raw_grant[..4].copy_from_slice(b"BFG1");
    raw_grant[4..20].copy_from_slice(&source);
    raw_grant[20..36].copy_from_slice(&stream);
    raw_grant[36..].fill(2);
    for tamper in [false, true] {
        for capacity in [20, 244, 512] {
            let mut h = Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap();
            let mut e = Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap();
            let mut wire = Vec::new();
            let mut media = hex(&v["records"][1]["packet"]);
            if tamper {
                media[17] ^= 1;
            } // Valid hop/endpoint sealing cannot repair the source signature.
            for (sequence, kind, flags, body) in [
                (0, 1, 0, hex(&v["header"])),
                (1, 2, 9, hex(&v["records"][0]["packet"])),
                (2, 2, 8, media),
                (3, 4, 0, vec![3]),
            ] {
                let sealed = e.seal(kind, flags, &body).unwrap();
                let envelope = Envelope {
                    stream,
                    source,
                    sequence,
                    ttl: 8,
                    flags: if kind == 4 { 48 } else { flags },
                    payload: &sealed,
                }
                .encode()
                .unwrap();
                wire.extend(h.seal(7, 0, &envelope).unwrap());
            }
            let stop = AtomicBool::new(false);
            let mut radio = FakeRadio {
                capacity,
                packets: Vec::new(),
            };
            Writer::default()
                .write(&wire, &mut radio, deadline(), &stop)
                .unwrap();
            let pipe = Pipe::default();
            for packet in radio.packets {
                pipe.offer(&packet).unwrap();
            }
            let mut reader = Reader::forwarded(
                HopReader::new(&[1; 32], Policy::default()).unwrap(),
                Grant::decode(&raw_grant).unwrap(),
                true,
                true,
            )
            .unwrap();
            let mut decoder = None;
            let mut pcm = [123; Decoder::MAX_SAMPLES];
            let mut frames = 0;
            let mut consumed = 0;
            let mut failure = None;
            let mut scratch = [0; 73];
            'receive: while consumed < wire.len() {
                let size = pipe.read(&mut scratch, deadline(), &stop).unwrap();
                consumed += size;
                let mut input = &scratch[..size];
                while !input.is_empty() {
                    match reader.read(&mut input, 0) {
                        Ok(Some(Event::Header(Header::Audio { preset: 2 }))) => {}
                        Ok(Some(Event::Media(media))) => {
                            let packet = media.packet();
                            if packet.flags == 9 {
                                decoder = Some(
                                    Decoder::new(packet.payload, Profile::AudioOnly20ms).unwrap(),
                                );
                            } else {
                                let output = decoder
                                    .as_mut()
                                    .unwrap()
                                    .decode(packet.payload, &mut pcm)
                                    .unwrap();
                                assert_eq!((output.samples.len(), output.discarded), (960, 0));
                                assert!(output.samples.iter().all(|sample| *sample == 0));
                                frames += 1;
                            }
                        }
                        Ok(Some(Event::State(3))) => {}
                        Ok(None) => {}
                        Err(error) => {
                            failure = Some(error);
                            break 'receive;
                        }
                        _ => panic!("unexpected audio ingress event"),
                    }
                }
            }
            assert_eq!(frames, if tamper { 0 } else { 1 });
            if tamper {
                assert!(matches!(
                    failure,
                    Some(meshlive::watch::Error::Audio(
                        meshlive::audio_live::Error::Crypto(_)
                    ))
                ));
            } else {
                assert!(failure.is_none());
            }
            assert!(matches!(
                reader.read(&mut &[][..], 0),
                Err(meshlive::watch::Error::Closed)
            ));
        }
    }
}

#[test]
fn reference_handshake_frames_and_eighteen_authenticated_records_survive_fake_gatt() {
    let vectors: Value =
        serde_json::from_str(include_str!("fixtures/android-vectors.json")).unwrap();
    let hello = hex(&vectors["framedHelloExample"]);
    let records = hex(&vectors["bms1Stream"]);
    let key: [u8; 32] = hex(&vectors["link"]["initiatorToResponderKey"])
        .try_into()
        .unwrap();
    let stream = [hello.as_slice(), records.as_slice()].concat();
    let stop = AtomicBool::new(false);
    for capacity in [20, 244, 512] {
        let mut radio = FakeRadio {
            capacity,
            packets: vec![],
        };
        Writer::default()
            .write(&stream, &mut radio, deadline(), &stop)
            .unwrap();
        let pipe = Pipe::default();
        for pair in radio.packets.chunks(2) {
            for packet in pair.iter().rev() {
                pipe.offer(packet).unwrap();
            }
            pipe.offer(&pair[0]).unwrap(); // An immediate duplicate cannot duplicate plaintext.
        }
        let mut handshake = HandshakeReader::default();
        let mut hello_done = false;
        // Fixture traffic keys are installed explicitly: this test does not claim peer admission.
        let mut reader = HopReader::new(&key, Policy::default()).unwrap();
        let mut consumed = 0;
        let mut count = 0;
        let mut scratch = [0; 73];
        while consumed < stream.len() {
            let size = pipe.read(&mut scratch, deadline(), &stop).unwrap();
            consumed += size;
            let mut input = &scratch[..size];
            if !hello_done {
                if let Some(message) = handshake.read(&mut input).unwrap() {
                    assert_eq!(message, hello[4..]);
                    hello_done = true;
                }
            }
            if hello_done {
                while !input.is_empty() {
                    if let Some(record) = reader.read(&mut input, usize::MAX).unwrap() {
                        let expected = &vectors["bms1Records"][count];
                        assert_eq!(record.header.sequence, count as u64);
                        assert_eq!(record.header.kind, expected["type"].as_u64().unwrap() as u8);
                        assert_eq!(record.plaintext, hex(&expected["payload"]));
                        count += 1;
                    }
                }
            }
        }
        assert!(hello_done);
        assert_eq!(count, 18);
        handshake.finish().unwrap();
        reader.finish().unwrap();
    }
}

#[test]
fn valid_gatt_sequences_cannot_make_tampered_ciphertext_deliver_plaintext() {
    let mut sealer = Sealer::new(Layer::Hop, &[7; 32], Policy::default()).unwrap();
    let mut encrypted = sealer.seal(6, 0, b"public carrier fixture").unwrap();
    *encrypted.last_mut().unwrap() ^= 1;
    let stop = AtomicBool::new(false);
    let mut radio = FakeRadio {
        capacity: 20,
        packets: vec![],
    };
    Writer::default()
        .write(&encrypted, &mut radio, deadline(), &stop)
        .unwrap();
    let pipe = Pipe::default();
    for packet in radio.packets {
        pipe.offer(&packet).unwrap();
    }
    let mut reader = HopReader::new(&[7; 32], Policy::default()).unwrap();
    let mut failed = false;
    let mut consumed = 0;
    let mut scratch = [0; 73];
    while consumed < encrypted.len() {
        let size = pipe.read(&mut scratch, deadline(), &stop).unwrap();
        consumed += size;
        let mut input = &scratch[..size];
        match reader.read(&mut input, usize::MAX) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("Unauthenticated plaintext escaped"),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert_eq!(
        reader.read(&mut &[][..], usize::MAX),
        Err(meshlive::framing::Error::Faulted)
    );
}
