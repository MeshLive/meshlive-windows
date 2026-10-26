use super::*;
use crate::video::fixtures::*;

fn header(codec: u8, width: u16, height: u16) -> VideoHeader {
    VideoHeader {
        width,
        height,
        fps: 5,
        codec,
        quarter_turns: 0,
        opus: false,
    }
}
fn dimensions(width: u32, height: u32) -> Dimensions {
    Dimensions { width, height }
}
fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
fn vp9_inter(size: Option<Dimensions>, refresh: u32, reference: usize) -> Vec<u8> {
    let mut w = Writer::default();
    w.put(0x86, 8).put(0, 2).put(refresh, 8);
    for index in [reference, 6, 7] {
        w.put(index as u32, 3).put(0, 1);
    }
    if let Some(size) = size {
        w.put(0, 3).put(size.width - 1, 16).put(size.height - 1, 16);
    } else {
        w.put(1, 1);
    }
    w.put(0, 1);
    w.0
}
fn vp9_intra(width: u32, height: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.put(0x84, 8)
        .put(1, 1)
        .put(0, 2)
        .put(0x498342, 24)
        .put(1, 8)
        .put(width - 1, 16)
        .put(height - 1, 16)
        .put(0, 1);
    w.0
}
fn superframe(frames: &[Vec<u8>], magnitude: usize) -> Vec<u8> {
    let marker = 0xc0 | (((magnitude - 1) as u8) << 3) | (frames.len() - 1) as u8;
    let mut bytes = frames.concat();
    bytes.push(marker);
    for frame in frames {
        bytes.extend_from_slice(&u32::to_le_bytes(frame.len() as u32)[..magnitude]);
    }
    bytes.push(marker);
    bytes
}
#[test]
fn android_real_vp9_header_vectors_give_exact_640_by_480() {
    for bytes in [
        "824983420027f01df41438241c18dc3bfdfeff7fbfdfefea3187221872118320",
        "824983426027f01df414301b877fbfdfeff7fbfdfd4630e4430e423064002108",
    ] {
        let bytes = hex(bytes);
        let state = SizeGuard::new(header(2, 640, 480)).inspect(&bytes).unwrap();
        let format =
            crate::video::FormatTracker::new(header(2, 640, 480), &[], Some(&bytes)).unwrap();
        assert_eq!(
            (
                format.current().unwrap().width,
                format.current().unwrap().height
            ),
            (640, 480)
        );
        assert!(
            state
                .vp9
                .iter()
                .all(|size| *size == Some(dimensions(640, 480)))
        );
        for cut in 0..9 {
            assert!(
                SizeGuard::new(header(2, 640, 480))
                    .inspect(&bytes[..cut])
                    .is_err()
            );
        }
        assert!(matches!(
            SizeGuard::new(header(2, 639, 480)).inspect(&bytes),
            Err(Size)
        ));
    }
}
#[test]
fn android_ladder_sizes_and_full_u16_bitstream_range_never_wrap() {
    for (width, height) in [(426, 240), (640, 360), (854, 480), (960, 540), (1280, 720)] {
        let state = SizeGuard::new(header(2, 1280, 720))
            .inspect(&vp9_key(width, height))
            .unwrap();
        assert_eq!(state.vp9[0], Some(dimensions(width, height)));
    }
    for (w, h) in [(4097, 64), (64, 4097), (65536, 65536)] {
        assert!(matches!(
            SizeGuard::new(header(2, 4096, 4096)).inspect(&vp9_key(w, h)),
            Err(Size)
        ));
    }
}
#[test]
fn vp9_inter_explicit_intra_hidden_and_each_superframe_picture_are_bounded() {
    let guard = SizeGuard::new(header(2, 64, 64))
        .inspect(&vp9_key(64, 64))
        .unwrap();
    let bad = [
        vp9_inter(Some(dimensions(65536, 64)), 1, 0),
        vp9_intra(64, 65536),
    ];
    for picture in bad {
        assert!(matches!(guard.inspect(&picture), Err(Size)));
        for magnitude in 1..=4 {
            assert!(matches!(
                guard.inspect(&superframe(&[vp9_key(32, 32), picture.clone()], magnitude)),
                Err(Size)
            ));
        }
    }
    for magnitude in 1..=4 {
        let next = guard
            .inspect(&superframe(
                &[vp9_intra(32, 16), vp9_inter(None, 2, 0)],
                magnitude,
            ))
            .unwrap();
        assert_eq!(next.vp9[1], Some(dimensions(32, 16)));
        assert_eq!(next.vp9[2], Some(dimensions(64, 64)));
    }
}
#[test]
fn vp9_selected_reference_and_show_existing_use_the_exact_refresh_slot() {
    let first = SizeGuard::new(header(2, 64, 64))
        .inspect(&vp9_key(64, 64))
        .unwrap();
    let next = first
        .inspect(&vp9_inter(Some(dimensions(32, 16)), 1 << 5, 0))
        .unwrap();
    assert_eq!(next.vp9[5], Some(dimensions(32, 16)));
    let inherited = next.inspect(&vp9_inter(None, 1 << 3, 5)).unwrap();
    assert_eq!(inherited.vp9[3], Some(dimensions(32, 16)));
    inherited.inspect(&[0x8d]).unwrap(); // profile 0, show slot 5
    assert!(matches!(
        SizeGuard::new(header(2, 64, 64)).inspect(&[0x8d]),
        Err(NeedKeyFrame)
    ));
    assert_eq!(first.vp9[5], Some(dimensions(64, 64))); // speculative state is unchanged
}
#[test]
fn recognized_vp9_indexes_reject_zeros_overflows_and_unaccounted_bytes() {
    let picture = vp9_key(64, 64);
    for magnitude in 1..=4 {
        let bytes = superframe(&[picture.clone(), picture.clone()], magnitude);
        let start = picture.len() * 2;
        let mut zero = bytes.clone();
        zero[start + 1..start + 1 + magnitude].fill(0);
        assert!(SizeGuard::new(header(2, 64, 64)).inspect(&zero).is_err());
        let mut too_big = bytes.clone();
        too_big[start + 1..start + 1 + magnitude].fill(255);
        assert!(SizeGuard::new(header(2, 64, 64)).inspect(&too_big).is_err());
        let mut short = bytes;
        short[start + 1] -= 1;
        assert!(SizeGuard::new(header(2, 64, 64)).inspect(&short).is_err());
    }
}
#[test]
fn android_av1_sequence_prefixes_give_each_ladder_size() {
    for (width, height) in [(426, 240), (640, 360), (960, 540), (1280, 720)] {
        let bytes = android_av1_sequence(width, height);
        let mut data = bytes.as_slice();
        let (kind, payload) = obu(&mut data).unwrap();
        assert_eq!(kind, 1);
        assert!(data.is_empty());
        assert_eq!(
            Sequence::parse(payload).unwrap().size,
            dimensions(width, height)
        );
        for cut in 0..payload.len() - 1 {
            assert!(Sequence::parse(&payload[..cut]).is_err());
        }
    }
}
#[test]
fn av1_checks_every_sequence_and_rejects_size_override_and_tile_list_syntax() {
    let sequence = android_av1_sequence(640, 360);
    let unit = [sequence.as_slice(), &frame(0, false)].concat();
    let guard = SizeGuard::new(header(3, 640, 360)).inspect(&unit).unwrap();
    guard.inspect(&frame(1, false)).unwrap();
    assert!(matches!(guard.inspect(&frame(1, true)), Err(Unsupported)));
    assert!(matches!(guard.inspect(&frame(3, true)), Err(Unsupported)));
    assert!(matches!(
        guard.inspect(&obu_bytes(8, &[0, 0])),
        Err(Unsupported)
    ));
    assert!(matches!(
        SizeGuard::new(header(3, 639, 360)).inspect(&unit),
        Err(Size)
    ));
    // A safe first sequence must not hide a larger second sequence in one unit.
    let hidden = [sequence, android_av1_sequence(1280, 720), frame(0, false)].concat();
    assert!(matches!(guard.inspect(&hidden), Err(Size)));
    let late = [frame(1, false), android_av1_sequence(1280, 720)].concat();
    assert!(matches!(guard.inspect(&late), Err(Malformed)));
}
#[test]
fn av1_requires_in_band_sequence_and_key_after_start_or_size_change() {
    let empty = SizeGuard::new(header(3, 1280, 720));
    assert!(matches!(empty.inspect(&frame(0, false)), Err(NeedKeyFrame)));
    let guard = empty
        .inspect(&[android_av1_sequence(640, 360), frame(0, false)].concat())
        .unwrap();
    assert!(matches!(
        empty.inspect(&[android_av1_sequence(640, 360), frame(1, false)].concat()),
        Err(NeedKeyFrame)
    ));
    assert!(matches!(
        guard.inspect(&[android_av1_sequence(1280, 720), frame(1, false)].concat()),
        Err(NeedKeyFrame)
    ));
    let changed = guard
        .inspect(&[android_av1_sequence(1280, 720), frame(0, false)].concat())
        .unwrap();
    assert_eq!(changed.av1.unwrap().size, dimensions(1280, 720));
    assert_eq!(guard.av1.unwrap().size, dimensions(640, 360));
    assert!(matches!(
        guard.inspect(&android_av1_sequence(640, 360)),
        Err(NeedKeyFrame)
    ));
}

#[test]
fn av1_reserved_obus_and_multiple_delimited_temporal_units_keep_size_checks() {
    let first = [android_av1_sequence(640, 360), frame(0, false)].concat();
    let second = [
        obu_bytes(2, &[]),
        obu_bytes(0, &[0x80]),
        android_av1_sequence(1280, 720),
        obu_bytes(14, &[0x80]),
        frame(0, false),
    ]
    .concat();
    let bytes = [first, second].concat();
    let state = SizeGuard::new(header(3, 1280, 720))
        .inspect(&bytes)
        .unwrap();
    assert_eq!(state.av1.unwrap().size, dimensions(1280, 720));
    assert!(matches!(
        SizeGuard::new(header(3, 640, 360)).inspect(&bytes),
        Err(Size)
    ));
}
#[test]
fn obu_sizes_extensions_and_reserved_bits_are_checked_before_slicing() {
    for bytes in [
        vec![0x80],
        vec![0x0b, 0],
        vec![0x0e],
        vec![0x0e, 1, 0],
        vec![0x0a, 5, 0],
        vec![0x0a, 0x80],
        vec![0x0a, 0xff, 0xff, 0xff, 0xff, 0x10],
        vec![0x0a, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0],
    ] {
        assert!(obu(&mut bytes.as_slice()).is_err(), "{bytes:02x?}");
    }
    let mut input = &[0x0e, 0, 0x81, 0, 42, 0x12, 0][..];
    assert_eq!(obu(&mut input).unwrap(), (1, &[42][..])); // non-minimal LEB128 is legal
    assert_eq!(obu(&mut input).unwrap(), (2, &[][..]));
    assert!(input.is_empty());
    assert_eq!(obu(&mut &[0x08, 42, 43][..]).unwrap(), (1, &[42, 43][..]));
}
#[test]
fn generated_native_decode_fixtures_pass_preflight_and_reset_requires_recovery_key() {
    for (codec, data) in [
        (
            2,
            include_bytes!("../../../tests/fixtures/video-vp9.ivf").as_slice(),
        ),
        (
            3,
            include_bytes!("../../../tests/fixtures/video-av1.ivf").as_slice(),
        ),
    ] {
        let empty = SizeGuard::new(header(codec, 64, 64));
        let mut guard = empty;
        let mut position = 32;
        for frame in 0..3 {
            let length =
                u32::from_le_bytes(data[position..position + 4].try_into().unwrap()) as usize;
            position += 12;
            let packet = &data[position..position + length];
            guard = guard.inspect(packet).unwrap();
            if frame != 0 {
                assert!(matches!(empty.inspect(packet), Err(NeedKeyFrame)));
            } else {
                assert!(matches!(
                    SizeGuard::new(header(codec, 63, 64)).inspect(packet),
                    Err(Size)
                ));
            }
            position += length;
        }
        assert_eq!(position, data.len());
    }
}
#[test]
fn arbitrary_short_inputs_and_payload_caps_never_panic() {
    for codec in [2, 3] {
        let guard = SizeGuard::new(header(codec, 4096, 4096));
        assert!(matches!(guard.inspect(&[]), Err(Malformed)));
        assert!(matches!(
            guard.inspect(&vec![0; MAX_PAYLOAD + 1]),
            Err(Malformed)
        ));
        for value in 0..=u16::MAX {
            let _ = guard.inspect(&value.to_be_bytes());
        }
    }
}

#[test]
fn vp9_profiles_colour_prefixes_and_render_hints_do_not_shift_coded_dimensions() {
    for profile in 0..=3 {
        for colour in [2, 7] {
            if colour == 7 && profile != 1 && profile != 3 {
                continue;
            }
            let mut w = Writer::default();
            w.put(2, 2).put(profile & 1, 1).put(profile >> 1, 1);
            if profile == 3 {
                w.put(0, 1);
            }
            w.put(0, 1).put(0, 1).put(1, 1).put(0, 1).put(0x498342, 24);
            if profile >= 2 {
                w.put(1, 1);
            }
            w.put(colour, 3);
            if colour != 7 {
                w.put(1, 1);
                if profile == 1 || profile == 3 {
                    w.put(0, 1).put(1, 1).put(0, 1);
                }
            } else {
                w.put(0, 1);
            }
            w.put(36, 16)
                .put(18, 16)
                .put(1, 1)
                .put(65535, 16)
                .put(65535, 16);
            let state = SizeGuard::new(header(2, 37, 19)).inspect(&w.0).unwrap();
            assert_eq!(state.vp9[0], Some(dimensions(37, 19)));
        }
    }
    assert!(matches!(
        SizeGuard::new(header(2, 64, 64)).inspect(&[0xb8]),
        Err(Malformed)
    ));
}

#[test]
fn av1_timing_operating_points_and_frame_ids_preserve_override_bit_alignment() {
    for equal in [false, true] {
        let mut w = Writer::default();
        w.put(0, 3)
            .put(0, 1)
            .put(0, 1)
            .put(1, 1) // profile, still/reduced, timing
            .put(1, 32)
            .put(30, 32)
            .put(equal as u32, 1);
        if equal {
            w.put(1, 1);
        } // uvlc(0)
        w.put(1, 1)
            .put(31, 5)
            .put(1, 32)
            .put(31, 5)
            .put(31, 5) // decoder model
            .put(1, 1)
            .put(1, 5) // display delay, two operating points
            .put(0, 12)
            .put(8, 5)
            .put(1, 1)
            .put(1, 1)
            .put(0x81234567, 32)
            .put(0x89abcdef, 32)
            .put(0, 1)
            .put(1, 1)
            .put(15, 4)
            .put(0x101, 12)
            .put(0, 5)
            .put(0, 1)
            .put(0, 1)
            .put(15, 4)
            .put(15, 4)
            .put(639, 16)
            .put(359, 16)
            .put(1, 1)
            .put(15, 4)
            .put(7, 3) // 25-bit frame IDs
            .put(7, 3)
            .put(0, 4)
            .put(0, 1) // features, order hints off
            .put(0, 1)
            .put(1, 1)
            .put(0, 1)
            .put(0, 1); // forced screen=1, integer=0
        let sequence = Sequence::parse(&w.0).unwrap();
        assert_eq!(sequence.size, dimensions(640, 360));
        assert_eq!(sequence.presentation_bits, 32);
        assert_eq!(sequence.id_bits, 25);
        for override_size in [false, true] {
            let mut picture = Writer::default();
            picture.put(0, 1).put(0, 2).put(1, 1);
            if !equal {
                picture.put(0x87654321, 32);
            }
            picture
                .put(0, 1)
                .put(0x1abcdef, 25)
                .put(override_size as u32, 1);
            assert_eq!(
                sequence.frame(&picture.0),
                if override_size {
                    Err(Unsupported)
                } else {
                    Ok(true)
                }
            );
        }
        let mut existing = Writer::default();
        existing.put(1, 1).put(7, 3);
        if !equal {
            existing.put(0x87654321, 32);
        }
        existing.put(0x1abcdef, 25);
        assert_eq!(sequence.frame(&existing.0), Ok(false));
    }
}

#[test]
fn av1_reduced_headers_and_sixteen_bit_dimensions_remain_bounded() {
    for (width, height, valid) in [(64, 64, true), (4097, 64, false), (65536, 65536, false)] {
        let mut w = Writer::default();
        w.put(0, 3)
            .put(1, 1)
            .put(1, 1)
            .put(31, 5) // reduced level has no tier bit
            .put(15, 4)
            .put(15, 4)
            .put(width - 1, 16)
            .put(height - 1, 16)
            .put(7, 3);
        let sequence = Sequence::parse(&w.0).unwrap();
        assert_eq!(sequence.size, dimensions(width, height));
        assert_eq!(sequence.frame(&[0]), Ok(true));
        let result = SizeGuard::new(header(3, 4096, 4096))
            .inspect(&[obu_bytes(1, &w.0), obu_bytes(6, &[0])].concat());
        assert_eq!(result.is_ok(), valid);
        if !valid {
            assert!(matches!(result, Err(Size)));
        }
    }
}
