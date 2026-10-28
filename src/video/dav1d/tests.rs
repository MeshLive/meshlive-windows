use super::*;
use crate::{video::MfFrames, viewer::FrameDecoder};

fn header(width: u16, height: u16) -> VideoHeader {
    VideoHeader { width, height, fps: 30, codec: 3, quarter_turns: 0, opus: false }
}
fn sequence(profile: u32, width: u32, height: u32, layout: u32, high_bitdepth: u32) -> Sequence {
    Sequence { profile, max_width: width, max_height: height, layout, high_bitdepth, ..Sequence::default() }
}
fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 128);
    [&[kind << 3 | 2, payload.len() as u8][..], payload].concat()
}
fn bench_frames(index: usize) -> (u16, u16, Vec<(&'static [u8], u64)>) {
    let (w, h, _, _, frames) = ivf_frames(BENCHMARKS[index].1).unwrap();
    (w, h, frames)
}

#[test]
fn pending_input_is_bounded_by_four_units_and_one_mebibyte() {
    let mut queue = InputQueue::default();
    for i in 0..MAX_PENDING_UNITS {
        assert!(queue.offer(&[0; 100], i as i64));
    }
    assert!(!queue.offer(&[0; 1], 9), "a fifth unit is refused");
    assert_eq!((queue.len(), queue.bytes()), (4, 400));
    let mut queue = InputQueue::default();
    assert!(queue.offer(&vec![0; MAX_PENDING_BYTES - 10], 0));
    assert!(!queue.offer(&[0; 11], 1), "one byte over 1 MiB is refused");
    assert!(queue.offer(&[0; 10], 1));
    assert_eq!(queue.bytes(), MAX_PENDING_BYTES);
}

#[test]
fn a_new_generation_discards_older_queued_work() {
    let mut queue = InputQueue::default();
    queue.offer(&[1], 1);
    queue.offer(&[2], 2);
    let before = queue.generation();
    assert_eq!(queue.discard(), 2);
    assert!(queue.generation() > before);
    assert_eq!((queue.take(), queue.bytes()), (None, 0));
    queue.offer(&[3], 3);
    assert_eq!(queue.take(), Some((vec![3], 3)));
}

#[test]
fn a_temporal_unit_has_at_most_64_obus_and_1_mib() {
    let delimiter = obu(2, &[]);
    let unit: Vec<u8> = (0..MAX_OBUS_PER_UNIT).flat_map(|_| delimiter.clone()).collect();
    assert_eq!(scan_unit(&unit), Ok((64, false)));
    let over = [unit.clone(), delimiter.clone()].concat();
    assert_eq!(scan_unit(&over), Err(Dav1dError::Unit));
    assert_eq!(scan_unit(&vec![0; MAX_PENDING_BYTES + 1]), Err(Dav1dError::Unit));
    assert_eq!(scan_unit(&[]), Err(Dav1dError::Unit));
    // A truncated OBU is refused, not read past its end.
    assert_eq!(scan_unit(&[1 << 3 | 2, 5, 0]), Err(Dav1dError::Unit));
    let with_sequence = [obu(2, &[]), obu(1, &[0; 4])].concat();
    assert_eq!(scan_unit(&with_sequence), Ok((2, true)));
}

#[test]
fn the_session_envelope_is_8_bit_420_main_inside_the_header() {
    assert_eq!(Envelope::for_header(header(1920, 1088)).map(|e| e.pixels), Ok(FRAME_SIZE_LIMIT));
    assert_eq!(Envelope::for_header(header(1920, 1090)), Err(Dav1dError::Envelope));
    assert_eq!(Envelope::for_header(header(2048, 1080)), Err(Dav1dError::Envelope));
    let mut vp9 = header(640, 480);
    vp9.codec = 2;
    assert_eq!(Envelope::for_header(vp9), Err(Dav1dError::Envelope));
    let envelope = Envelope::for_header(header(640, 480)).unwrap();
    assert_eq!(envelope.admits(&sequence(0, 640, 480, 1, 0)), Ok(()));
    assert_eq!(envelope.admits(&sequence(0, 320, 240, 1, 0)), Ok(()));
    for refused in [
        sequence(1, 640, 480, 1, 0), // High profile
        sequence(0, 640, 480, 1, 1), // 10-bit
        sequence(0, 640, 480, 0, 0), // monochrome
        sequence(0, 640, 480, 3, 0), // 4:4:4
        sequence(0, 641, 480, 1, 0), // wider than the header
        sequence(0, 640, 481, 1, 0), // taller than the header
    ] {
        assert_eq!(envelope.admits(&refused), Err(Dav1dError::Envelope), "{refused:?}");
    }
}

#[test]
fn the_sequence_header_is_read_by_dav1d() {
    let (_, _, frames) = bench_frames(1);
    let parsed = parse_sequence(frames[0].0).unwrap().unwrap();
    assert_eq!((parsed.profile, parsed.max_width, parsed.max_height, parsed.layout, parsed.high_bitdepth), (0, 640, 360, 1, 0));
    assert_eq!(parse_sequence(&obu(2, &[])), Ok(None));
}

#[test]
fn media_foundation_wins_whenever_it_starts_and_dav1d_only_backs_up_av1() {
    assert_eq!(choose(3, true, true), Choice::MediaFoundation);
    assert_eq!(choose(3, true, false), Choice::MediaFoundation);
    assert_eq!(choose(3, false, true), Choice::Dav1d);
    assert_eq!(choose(3, false, false), Choice::None);
    assert_eq!(choose(2, false, true), Choice::None, "dav1d never decodes VP9");
    assert!(advertise_av1(true, false));
    assert!(advertise_av1(false, true));
    assert!(!advertise_av1(false, false));
}

#[test]
fn av1_is_advertised_only_after_an_exact_fixture_and_a_fast_enough_benchmark() {
    let speed = |real_time| Speed { label: "854x480/30", pixels: 854 * 480, real_time };
    let slow = Speed { label: "640x360/15", pixels: 640 * 360, real_time: 1.49 };
    let ready = Readiness::from_parts(true, true, vec![speed(4.0), slow.clone()], 10);
    assert!(ready.ready());
    assert_eq!(ready.envelope_pixels, 854 * 480);
    assert!(!advertise_av1(false, Readiness::from_parts(true, false, vec![speed(4.0)], 10).ready()), "inexact fixture");
    assert!(!advertise_av1(false, Readiness::from_parts(true, true, vec![slow.clone()], 10).ready()), "too slow");
    assert!(!advertise_av1(false, Readiness::from_parts(true, true, Vec::new(), 10).ready()), "benchmark failed");
    assert!(!advertise_av1(false, Readiness::from_parts(false, true, vec![speed(4.0)], 10).ready()), "switched off");
    let only_small = Readiness::from_parts(true, true, vec![speed(1.0), Speed { real_time: 2.0, ..slow }], 10);
    assert_eq!(only_small.envelope_pixels, 640 * 360);
}

#[test]
fn the_embedded_fixture_decodes_hash_exact_and_the_benchmarks_decode_fully() {
    assert_eq!(version(), "1.5.3");
    assert!(fixture_decodes_exactly());
    for (label, data) in BENCHMARKS {
        let speed = benchmark(label, data).unwrap_or_else(|| panic!("{label} did not decode"));
        assert!(speed.real_time > 0.0);
    }
    // This PC (any PC that builds the app) passes the startup check.
    let readiness = readiness();
    assert!(readiness.ready(), "{}", readiness.line());
}

/// The fixture through the app's session path: MfFrames with no MF decoder,
/// dav1d into the NV12 texture pool, read back as the renderer does.
#[test]
fn the_session_path_presents_the_fixture_pixels_exactly_as_nv12() {
    let (gpu, _, _) = crate::video::device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let (_, _, _, _, frames) = ivf_frames(FIXTURE).unwrap();
    let mut hashes = Vec::new();
    let mut decoder = MfFrames::new(&owner, header(64, 64), |_, frame| {
        let nv12 = frame.nv12().unwrap().read_nv12_cropped(64, 64).unwrap();
        // Back to I420 to compare with the source hashes.
        let (y, uv) = nv12.split_at(64 * 64);
        let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
        let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
        hashes.push(hex(&crate::crypto::sha256(&[y, &u, &v].concat()).unwrap()));
        assert!(matches!(frame.format.range, Some(Range::Limited)));
        assert!(matches!(frame.format.colour, Some(Colour::Bt709)));
    })
    .with_policy(crate::video::DecoderPolicy::Dav1dOnly);
    decoder.configure_with(None).unwrap();
    assert_eq!(decoder.backend(), Some("dav1d"));
    for (i, (unit, _)) in frames.iter().enumerate() {
        assert!(FrameDecoder::push(&mut decoder, unit, i as i64 * 200_000, i == 0, false).unwrap());
        while FrameDecoder::pull(&mut decoder).unwrap().is_some() {}
    }
    drop(decoder);
    assert_eq!(hashes, FIXTURE_I420);
}

#[test]
fn media_foundation_only_never_falls_back() {
    let (gpu, _, _) = crate::video::device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let mut decoder = MfFrames::new(&owner, header(64, 64), |_, _| {}).with_policy(crate::video::DecoderPolicy::MediaFoundationOnly);
    match decoder.configure_with(None) {
        Ok(()) => assert_eq!(decoder.backend(), Some("media-foundation")),
        Err(()) => assert_eq!(decoder.backend(), None),
    }
    // Auto: MF when it starts here, else dav1d; never nothing on a PC that passed the check.
    let mut auto = MfFrames::new(&owner, header(64, 64), |_, _| {});
    auto.configure_with(None).unwrap();
    assert!(auto.backend().is_some());
}

#[test]
fn a_full_session_pushes_back_and_a_held_output_pool_drops_pictures() {
    let (gpu, _, _) = crate::video::device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let (w, h, frames) = bench_frames(1);
    let mut session = Dav1dSession::open(&owner, header(w, h), None).unwrap();
    // Push without pulling: dav1d decodes what it can hold, then input queues up to
    // its bound and is refused, never grown.
    let mut accepted = 0;
    for (i, (unit, _)) in frames.iter().enumerate() {
        let taken = session.push(unit, i as i64, i == 0).unwrap();
        assert!(session.queued().0 <= MAX_PENDING_UNITS && session.queued().1 <= MAX_PENDING_BYTES);
        if !taken {
            break;
        }
        accepted += 1;
    }
    assert!(accepted < frames.len(), "the session never pushed back");
    // Decoded pictures dav1d keeps for output (at most two: its output and
    // its layer cache) are not pending input; pending input stays at 4.
    assert!(accepted <= MAX_PENDING_UNITS + 2, "accepted {accepted}");
    assert_eq!(session.queued().0, MAX_PENDING_UNITS);
    // Keep every presented sample: the pool of 4 runs out and later
    // pictures are dropped and counted, not allocated.
    let mut held = Vec::new();
    let mut next = accepted;
    loop {
        while let Some((sample, _, _)) = session.pull().unwrap() {
            held.push(sample);
        }
        if next == frames.len() {
            break;
        }
        assert!(session.push(frames[next].0, next as i64, false).unwrap());
        next += 1;
    }
    assert_eq!(held.len(), OUTPUT_POOL);
    assert_eq!(session.drops(), (0, (frames.len() - OUTPUT_POOL) as u64));
    // Released samples are reused.
    held.clear();
    session.flush();
    assert!(session.push(frames[0].0, 0, true).unwrap());
    assert!(session.pull().unwrap().is_some());
}

#[test]
fn units_before_a_keyframe_and_after_a_flush_are_dropped_until_a_keyframe() {
    let (gpu, _, _) = crate::video::device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let (w, h, frames) = bench_frames(1);
    let mut session = Dav1dSession::open(&owner, header(w, h), None).unwrap();
    assert!(session.push(frames[1].0, 1, false).unwrap());
    assert_eq!(session.drops().0, 1);
    assert!(session.pull().unwrap().is_none());
    let generation = session.generation();
    session.flush();
    assert!(session.generation() > generation);
    assert!(session.push(frames[0].0, 0, true).unwrap());
    assert!(session.pull().unwrap().is_some());
}

#[test]
fn a_sequence_outside_the_envelope_fails_closed() {
    let (gpu, _, _) = crate::video::device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let (_, _, frames) = bench_frames(1);
    // A 640x360 stream in a session announced as 320x180.
    assert!(Dav1dSession::open(&owner, header(320, 180), Some(frames[0].0)).is_err());
    let mut session = Dav1dSession::open(&owner, header(320, 180), None).unwrap();
    assert_eq!(session.push(frames[0].0, 0, true).err(), Some(Dav1dError::Envelope));
}

#[test]
fn colour_metadata_follows_the_sequence_header_and_refuses_the_unknown() {
    let mut s = sequence(0, 64, 64, 1, 0);
    s.matrix = 1;
    let f = output_format(&s).unwrap();
    assert!(matches!((f.colour, f.range, f.chroma), (Some(Colour::Bt709), Some(Range::Limited), None)));
    s.matrix = 6;
    s.color_range = 1;
    s.chroma = 2;
    let f = output_format(&s).unwrap();
    assert!(matches!((f.colour, f.range, f.chroma), (Some(Colour::Bt601), Some(Range::Full), Some(ChromaLocation::TopLeft))));
    s.matrix = 9; // BT.2020: the SDR renderer has no conversion for it
    assert!(output_format(&s).is_err());
}

#[test]
fn percentiles_of_decode_times() {
    assert_eq!(percentiles(&[]), (0.0, 0.0));
    let samples: Vec<u32> = (1..=100).map(|ms| ms * 1000).collect();
    let (p50, p95) = percentiles(&samples);
    assert!((50.0..=51.0).contains(&p50) && (95.0..=96.0).contains(&p95), "{p50} {p95}");
}
