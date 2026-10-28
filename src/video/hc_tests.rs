//! Design HC on this PC's Media Foundation: the shared phone vectors decode
//! through the H.264 / HEVC decoder MFTs, and this PC's own hardware
//! encoders round-trip through the sender packetiser, the receiver gate and
//! the decoder. Native: they need the decoders (HEVC: Microsoft's HEVC Video
//! Extensions) and a hardware encoder; a PC without one skips that part.
use super::annexb::{Codec, tests::fixture_units};
use super::*;
use crate::media::{self, VideoHeader};

fn registered(subtype: windows::core::GUID) -> bool {
    enumerate(subtype).map(|c| c.hardware + c.software > 0).unwrap_or(false)
}

/// Decodes `units` in order; returns the decoded pictures' sizes.
fn decode_all(owner: &DecodeDevice, header: VideoHeader, units: &[(Vec<u8>, bool)]) -> Vec<(u32, u32)> {
    let mut decoder = Decoder::open(owner, header).unwrap_or_else(|e| panic!("open codec {}: {e:?}", header.codec));
    let mut sizes = Vec::new();
    for (i, (unit, key)) in units.iter().enumerate() {
        let mut accepted = false;
        for _ in 0..8 {
            if decoder.push(unit, i as i64 * 66_667, *key).unwrap_or_else(|e| panic!("unit {i}: {e:?}")) {
                accepted = true;
                break;
            }
            while let Some(frame) = decoder.pull().unwrap() {
                sizes.push((frame.format.crop.width, frame.format.crop.height));
            }
        }
        assert!(accepted, "unit {i} never accepted");
        while let Some(frame) = decoder.pull().unwrap() {
            sizes.push((frame.format.crop.width, frame.format.crop.height));
        }
    }
    decoder.drain().unwrap();
    while let Some(frame) = decoder.pull().unwrap() {
        sizes.push((frame.format.crop.width, frame.format.crop.height));
    }
    sizes
}

#[test]
fn the_phone_hardware_encoder_vectors_decode_on_this_pc() {
    let (gpu, _, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    for name in ["s24-hevc", "asus-hevc", "m31-hevc", "a15s-h264"] {
        let (codec, units) = fixture_units(name);
        let subtype = if codec == Codec::Hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
        if !registered(subtype) {
            eprintln!("skipped {name}: no {} decoder MFT on this PC", codec.label());
            continue;
        }
        let header = VideoHeader { width: 352, height: 288, fps: 15, codec: codec.id(), quarter_turns: 0, opus: false };
        let started = std::time::Instant::now();
        let sizes = decode_all(&owner, header, &units);
        eprintln!("{name}: {} of {} pictures decoded in {} ms", sizes.len(), units.len(), started.elapsed().as_millis());
        assert_eq!(sizes.len(), units.len(), "{name}: every access unit is one picture");
        assert!(sizes.iter().all(|&s| s == (352, 288)), "{name}: {sizes:?}");
    }
}

#[test]
fn the_decode_start_check_passes_for_registered_decoders() {
    for (codec, subtype) in [(codecs::H264, MFVideoFormat_H264), (codecs::HEVC, MFVideoFormat_HEVC)] {
        let result = hc_decode_check(codec);
        eprintln!("decode check {}: {result:?}", codecs::label(codec));
        assert_eq!(result.is_ok(), registered(subtype), "{}: {result:?}", codecs::label(codec));
    }
}

/// This PC's hardware encoder, through `SourceEncoder` (packetiser, keyframe
/// requests), BTV3 packets, `VideoReceiver` (gate, format tracker) and
/// `MfFrames` (decoder): pictures come out at the session size, and a
/// keyframe request is answered by an IDR.
#[test]
fn hardware_hevc_and_h264_round_trip_through_the_receive_path() {
    use crate::{playout::Profile, viewer::VideoReceiver};
    for codec in [codecs::HEVC, codecs::H264] {
        // The start check runs at the call default (426x240), which NVIDIA's
        // HEVC MFT codes as 448x256 cropped back (accepted by rule 6 since the
        // 2026-10-28 HEVC padding decision). Hardware HEVC/H.264 must pass it.
        let check = hc_check(codec);
        eprintln!("encoder check {}: {check:?}", codecs::label(codec));
        let subtype = if codec == codecs::HEVC { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
        if hardware_candidates(codec).map_or(true, |c| c.is_empty()) || !registered(subtype) {
            eprintln!("skipped {}: no hardware encoder or decoder", codecs::label(codec));
            continue;
        }
        assert!(check.is_ok(), "{check:?}");
        let (gpu, _, _) = device(false, false).unwrap();
        let owner = DecodeDevice::new(&gpu).unwrap();
        let settings = EncoderSettings { codec, width: 640, height: 480, fps: 15, bitrate_kbps: 400, keyframe_interval_s: 60 };
        let encoder = VideoEncoder::open(&owner, settings, Backend::Hardware).unwrap();
        if let VideoEncoder::Hardware(e) = &encoder {
            eprintln!("{}: {} controls {:?}", codecs::label(codec), e.name(), e.controls());
        }
        let mut source = SourceEncoder::new(encoder, 60);
        let header = VideoHeader { width: 640, height: 480, fps: 15, codec, quarter_turns: 0, opus: true };
        let mut receiver = VideoReceiver::new(header);
        let mut clock = Profile::Call.clock();
        let decoded = std::rc::Rc::new(std::cell::Cell::new(Vec::<(u32, u32)>::new()));
        let seen = decoded.clone();
        let mut frames = MfFrames::new(&owner, header, move |_, frame| {
            let mut v = seen.take();
            v.push((frame.format.crop.width, frame.format.crop.height));
            seen.set(v);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (mut keys, mut packets) = (0, 0);
        for i in 0..30u32 {
            let pts = i64::from(i) * 66_667;
            if i == 15 {
                source.request_keyframe(u64::from(i) * 66_667_000, true);
            }
            let picture = crate::generated::pattern_nv12(640, 480, i);
            for packet in source.encode(&picture, pts, deadline).unwrap() {
                let p = media::Packet::decode(&packet, media::MAX_PAYLOAD).unwrap();
                assert_eq!(p.flags & media::CONFIG, 0, "no configuration packet for codecs 4 and 5");
                keys += usize::from(p.flags & media::KEY_FRAME != 0);
                packets += 1;
                receiver.on_packet(p, pts * 1000, &mut clock).unwrap();
            }
            assert!(receiver.feed(&mut frames).is_ok(), "frame {i}: {:?}", frames.last_error);
            receiver.drain(&mut frames, i64::MAX / 4, &clock).unwrap();
        }
        for packet in source.finish(deadline).unwrap() {
            let p = media::Packet::decode(&packet, media::MAX_PAYLOAD).unwrap();
            packets += 1;
            receiver.on_packet(p, 0, &mut clock).unwrap();
        }
        receiver.feed(&mut frames).unwrap();
        receiver.drain(&mut frames, i64::MAX / 4, &clock).unwrap();
        let sizes = decoded.take();
        eprintln!("{}: {packets} packets, {keys} IDRs, {} pictures, rejected {}", codecs::label(codec), sizes.len(), receiver.au_rejected);
        let rejected = receiver.au_rejected;
        assert_eq!(rejected, 0, "{:?}", receiver.take_au_log());
        assert!(keys >= 2, "the keyframe request was answered by an IDR");
        assert!(sizes.len() >= 25, "{} pictures", sizes.len());
        assert!(sizes.iter().all(|&s| s == (640, 480)), "{sizes:?}");
    }
}

/// The receive queue for codecs 4/5 without a decoder: the gate's
/// configuration comes first, stripped units follow, a rejected unit waits
/// for the next keyframe, and the closing rules end the session.
#[test]
fn the_receive_queue_gates_hc_units_and_closes_on_the_design_hc_rules() {
    use crate::{
        playout::Profile,
        viewer::{Error, FrameDecoder, VideoReceiver},
    };
    #[derive(Default)]
    struct Log(Vec<String>);
    impl FrameDecoder for Log {
        fn configure(&mut self, format: &VideoFormat) -> std::result::Result<(), ()> {
            self.0.push(format!("configure {}x{} sets={}", format.width, format.height, format.configurations().len()));
            Ok(())
        }
        fn push(&mut self, payload: &[u8], _: i64, key: bool, _: bool) -> std::result::Result<bool, ()> {
            self.0.push(format!("push key={key} bytes={}", payload.len()));
            Ok(true)
        }
        fn pull(&mut self) -> std::result::Result<Option<i64>, ()> {
            Ok(None)
        }
    }
    let (codec, units) = fixture_units("asus-hevc");
    let header = VideoHeader { width: 352, height: 288, fps: 15, codec: codec.id(), quarter_turns: 0, opus: false };
    let packet = |unit: &[u8], key: bool, extra: u8| -> Vec<u8> {
        media::Packet { flags: if key { media::KEY_FRAME } else { 0 } | extra, pts_us: 0, payload: unit }.encode().unwrap()
    };
    let mut clock = Profile::Call.clock();
    let mut rx = VideoReceiver::new(header);
    let mut decoder = Log::default();
    // A P picture before any keyframe is skipped; then the IDR configures.
    let p1 = packet(&units[1].0, false, 0);
    rx.on_packet(media::Packet::decode(&p1, media::MAX_PAYLOAD).unwrap(), 0, &mut clock).unwrap();
    for (i, (unit, key)) in units.iter().enumerate().take(4) {
        let raw = packet(unit, *key, 0);
        rx.on_packet(media::Packet::decode(&raw, media::MAX_PAYLOAD).unwrap(), i as i64, &mut clock).unwrap();
    }
    rx.feed(&mut decoder).unwrap();
    assert_eq!(decoder.0[0], "configure 352x288 sets=1");
    assert_eq!(decoder.0.len(), 5, "{:?}", decoder.0);
    // A corrupt P picture is dropped; the queue waits for the next keyframe.
    let mut broken = units[4].0.clone();
    broken.extend_from_slice(&[0, 0, 2]);
    rx.on_packet(media::Packet::decode(&packet(&broken, false, 0), media::MAX_PAYLOAD).unwrap(), 5, &mut clock).unwrap();
    rx.on_packet(media::Packet::decode(&packet(&units[5].0, false, 0), media::MAX_PAYLOAD).unwrap(), 6, &mut clock).unwrap();
    assert!(rx.awaiting_keyframe());
    assert_eq!(rx.au_rejected, 1);
    assert!(rx.take_au_log()[0].starts_with("au_reject: codec=HEVC | rule=1"));
    // Three rejected keyframes within 10 s close the session.
    let mut bad_key = units[0].0.clone();
    bad_key.extend_from_slice(&[0, 0, 2]);
    let raw = packet(&bad_key, true, 0);
    let s = 1_000_000_000;
    assert!(rx.on_packet(media::Packet::decode(&raw, media::MAX_PAYLOAD).unwrap(), 10 * s, &mut clock).is_ok());
    assert!(rx.on_packet(media::Packet::decode(&raw, media::MAX_PAYLOAD).unwrap(), 12 * s, &mut clock).is_ok());
    assert_eq!(
        rx.on_packet(media::Packet::decode(&raw, media::MAX_PAYLOAD).unwrap(), 14 * s, &mut clock),
        Err(Error::Format(FormatError::Unsupported))
    );
    // A configuration packet or the discardable flag closes the session at once.
    for extra in [media::CONFIG, media::DISCARDABLE] {
        let mut rx = VideoReceiver::new(header);
        let raw = packet(&units[0].0, true, extra);
        assert_eq!(rx.on_packet(media::Packet::decode(&raw, media::MAX_PAYLOAD).unwrap(), 0, &mut clock), Err(Error::Format(FormatError::Unsupported)));
    }
}


/// Every hello carries this PC's real Design HC masks once a master is on
/// (phone run 2026-10-28: the BMH2 end-to-end hello still said 0x03 while
/// the header named HEVC, and the phone ended the call); with both off the
/// bytes are today's. New mask bits from a phone never fail a hello.
#[test]
fn hellos_carry_the_hc_masks_and_new_bits_never_fail_a_hello() {
    use crate::bmh2::Capabilities;
    let base = Capabilities { transport: 0x0c, encode_codecs: 0x03, decode_codecs: 0x03, maximum_bitrate_kbps: 1100, maximum_children: 0 };
    let on = codecs::Switches { hevc: true, h264: true, ..codecs::Switches::LEGACY };
    assert_eq!(hello_codecs_with(base, codecs::Switches::LEGACY, || 0x0f, || 0x1f), base);
    let link = hello_codecs_with(base, on, || 0x0f, || 0x0f);
    assert_eq!((link.encode_codecs, link.decode_codecs, link.transport), (0x0f, 0x0f, 0x0c));
    let end_to_end = hello_codecs_with(base, on, || 0x0b, || 0x1b);
    assert_eq!((end_to_end.encode_codecs, end_to_end.decode_codecs), (0x0b, 0x1b));
    // A 0.4.2 phone's masks (HEVC, H.264, bits 4 and 7) decode unchanged.
    for mask in [0x0f, 0x1f, 0x9b, 0xff] {
        let raw = Capabilities { encode_codecs: mask, decode_codecs: mask, ..base }.encode();
        assert_eq!(Capabilities::decode(&raw).unwrap().decode_codecs, mask);
    }
}
