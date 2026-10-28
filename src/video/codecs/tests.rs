use super::*;

fn available(av1: bool, vp9: bool, hevc: bool, h264: bool) -> Availability {
    let name = |on: bool, n: &str| if on { vec![n.to_owned()] } else { Vec::new() };
    Availability {
        av1_hardware: name(av1, "av1"),
        vp9_hardware: name(vp9, "vp9"),
        hevc_hardware: name(hevc, "hevc"),
        h264_hardware: name(h264, "h264"),
        av1_software: "libaom".into(),
        vp9_software: "libvpx".into(),
    }
}
const ON: Switches = Switches { hevc: true, h264: true, ..Switches::LEGACY };
const ALL: u8 = HELLO_CODECS;
use Backend::{Hardware as Hw, SoftwareAv1 as Aom, SoftwareVp9 as Vpx};

#[test]
fn one_table_maps_codecs_hello_bits_and_summary_bits() {
    assert_eq!([VP9, AV1, H264, HEVC].map(hello_bit), [1, 2, 4, 8]);
    assert_eq!([VP9, AV1, H264, HEVC].map(summary_bit), [1, 2, 4, 0x80]);
    assert_eq!(hello_bit(1), 0);
    // Dual-listening (0x08), relay consent (0x10) and audio-only (0x40) are not codecs.
    assert_eq!(summary_to_hello(0x80 | 0x40 | 0x10 | 0x08 | 0x01), HELLO_HEVC | HELLO_VP9);
    assert_eq!(summary_to_hello(0x87), HELLO_CODECS);
    // Bits 4 and 7 of a decode mask never reach a summary.
    assert_eq!(hello_to_summary(0xff), SUMMARY_CODECS);
    assert_eq!(hello_to_summary(HELLO_HEVC | DECODE_AV1_SOFTWARE), SUMMARY_HEVC);
    for codec in [VP9, AV1, H264, HEVC] {
        assert_eq!(summary_to_hello(hello_to_summary(hello_bit(codec))), hello_bit(codec));
    }
}

#[test]
fn masters_default_on_and_their_off_switches() {
    let none = Switches::from(|_| None);
    assert_eq!(none, ON, "both masters on by default (2026-10-28)");
    assert!(none.any());
    let set = |pairs: &'static [(&'static str, &'static str)]| Switches::from(move |n| pairs.iter().find(|p| p.0 == n).map(|p| p.1.to_owned()));
    let s = set(&[(SWITCH_H264, "0"), (SWITCH_PREFER_HEVC, "0"), (SWITCH_AV1_SOFT_DECODE_BIT, "0"), (SWITCH_BIT7_HW_HEVC, "0"), (SWITCH_HC_SESSION_LEVEL, "0")]);
    assert!(s.hevc && !s.h264 && !s.prefer_hevc && s.prefer_hw_avc && !s.av1_soft_decode_bit && !s.bit7_hw_hevc && !s.session_level);
    assert!(set(&[(SWITCH_HEVC, "false")]).hevc, "only 0 turns a master off");
    assert_eq!(set(&[(SWITCH_HEVC, "0"), (SWITCH_H264, "0")]), Switches::LEGACY, "both off: today's rules");
}

#[test]
fn with_the_masters_off_the_choice_is_exactly_todays_plan() {
    let a = available(true, true, true, true);
    for peer in [0, 1, 2, 3, 0x83, 0x81, 0x82, 0x0f, 0x8f, 0x1f] {
        for av in [a.clone(), available(false, false, true, true), available(true, false, false, false)] {
            assert_eq!(for_call(peer, &av, Switches::LEGACY).list, super::super::encoder::plan(peer, &av), "peer {peer:#x}");
        }
    }
    assert_eq!(for_call(0x83, &a, Switches::LEGACY).reason, "peer_prefers_vp9");
    assert_eq!(for_call(0x03, &a, Switches::LEGACY).reason, "hw_av1_preferred");
}

#[test]
fn calls_rank_hardware_av1_hevc_vp9_h264_then_software() {
    let a = available(true, true, true, true);
    let c = for_call(ALL, &a, ON);
    assert_eq!(c.list, vec![(AV1, Hw), (HEVC, Hw), (VP9, Hw), (H264, Hw), (AV1, Aom), (VP9, Vpx)]);
    assert_eq!(c.reason, "hw_av1_preferred");
    // Only codecs the peer decodes count: an old peer (0x03) never gets HEVC or H.264.
    assert_eq!(for_call(0x03, &a, ON).list, vec![(AV1, Hw), (VP9, Hw), (AV1, Aom), (VP9, Vpx)]);
    let c = for_call(HELLO_VP9 | HELLO_HEVC, &a, ON);
    assert_eq!((c.list[0], c.reason), ((HEVC, Hw), "peer_no_av1_decode"));
    // An NVIDIA PC (no hardware VP9) calling a VP9|HEVC phone.
    let nvidia = available(true, false, true, true);
    assert_eq!(for_call(HELLO_VP9 | HELLO_HEVC | HELLO_H264, &nvidia, ON).list, vec![(HEVC, Hw), (H264, Hw), (VP9, Vpx)]);
    // Hardware H.264 beats every software encoder.
    let c = for_call(ALL, &available(false, false, false, true), ON);
    assert_eq!((c.list[0], c.reason), ((H264, Hw), "hw_avc_preferred"));
    // Fallback walks the one filtered list: the top choice refused leaves the next.
    assert_eq!(for_call(HELLO_HEVC | HELLO_H264, &nvidia, ON).list, vec![(HEVC, Hw), (H264, Hw)]);
    assert_eq!(for_call(HELLO_HEVC, &available(true, true, false, false), ON).reason, "no_common_codec");
}

#[test]
fn bit_7_puts_vp9_first_except_below_hardware_hevc() {
    let a = available(true, true, true, true);
    let peer = HELLO_VP9 | HELLO_AV1 | HELLO_HEVC | DECODE_PREFERS_VP9;
    let c = for_call(peer, &a, ON);
    assert_eq!(c.list[..3], [(HEVC, Hw), (VP9, Hw), (AV1, Hw)]);
    assert_eq!(c.notes, vec!["bit7_hw_hevc_first"]);
    // The first draft's order (control arm): VP9 even over hardware HEVC.
    let c = for_call(peer, &a, Switches { bit7_hw_hevc: false, ..ON });
    assert_eq!((c.list[..2].to_vec(), c.reason), (vec![(VP9, Hw), (AV1, Hw)], "peer_prefers_vp9"));
    // A VP9|HEVC|bit-7 peer from a PC without hardware HEVC gets VP9 (libvpx here).
    let c = for_call(HELLO_VP9 | HELLO_HEVC | DECODE_PREFERS_VP9, &available(true, false, false, true), ON);
    assert_eq!((c.list[0], c.reason), ((VP9, Vpx), "peer_prefers_vp9"));
    // A peer without HEVC decode gets VP9 as before.
    assert_eq!(for_call(HELLO_VP9 | HELLO_AV1 | DECODE_PREFERS_VP9, &a, ON).list[0], (VP9, Hw));
    // An HEVC|bit-7 peer without VP9: the preference is ignored and logged.
    let c = for_call(HELLO_HEVC | HELLO_AV1 | DECODE_PREFERS_VP9, &a, ON);
    assert_eq!((c.list[0], c.notes.clone()), ((AV1, Hw), vec!["bit7_without_vp9_ignored"]));
}

#[test]
fn bit_4_moves_av1_below_hardware_hevc_and_vp9_only() {
    let peer = ALL | DECODE_AV1_SOFTWARE;
    let c = for_call(peer, &available(true, true, true, true), ON);
    assert_eq!(c.list, vec![(HEVC, Hw), (VP9, Hw), (AV1, Hw), (H264, Hw), (AV1, Aom), (VP9, Vpx)]);
    assert_eq!(c.reason, "peer_av1_software");
    let c = for_call(peer, &available(true, true, false, false), ON);
    assert_eq!(c.list[..2], [(VP9, Hw), (AV1, Hw)]);
    // Only hardware H.264 or software alternatives: AV1 stays first.
    assert_eq!(for_call(peer, &available(true, false, false, true), ON).list[0], (AV1, Hw));
    assert_eq!(for_call(peer, &available(false, false, false, false), ON).list[0], (AV1, Aom));
    // Bit 7 is tested first: with bit 7 the bit-4 order is then led by HEVC and VP9.
    let c = for_call(peer | DECODE_PREFERS_VP9, &available(true, true, true, true), ON);
    assert_eq!(c.list[..3], [(HEVC, Hw), (VP9, Hw), (AV1, Hw)]);
}

#[test]
fn control_arms_remove_a_codec_from_automatic() {
    let a = available(false, true, true, true);
    let c = for_call(ALL, &a, Switches { prefer_hevc: false, ..ON });
    assert!(c.list.iter().all(|e| e.0 != HEVC));
    assert_eq!((c.list[0], c.reason), ((VP9, Hw), "hevc_not_preferred"));
    let c = for_call(HELLO_H264 | HELLO_AV1, &available(false, false, false, true), Switches { prefer_hw_avc: false, ..ON });
    assert_eq!((c.list.clone(), c.reason), (vec![(AV1, Aom)], "avc_not_preferred"));
    // A master off: its encoders never appear, even if listed.
    assert!(for_call(ALL, &a, Switches { hevc: false, ..ON }).list.iter().all(|e| e.0 != HEVC));
}

#[test]
fn streams_follow_q_hc1_a_vp9_above_hevc() {
    assert_eq!(hardware_order(Mode::Stream), [AV1, VP9, HEVC, H264]);
    assert_eq!(hardware_order(Mode::Call), [AV1, HEVC, VP9, H264]);
    let c = for_stream(&available(true, true, true, true), ON);
    assert_eq!(c.list[..4], [(AV1, Hw), (VP9, Hw), (HEVC, Hw), (H264, Hw)]);
    // An older NVIDIA PC (no hardware AV1 or VP9) streams hardware HEVC.
    let c = for_stream(&available(false, false, true, true), ON);
    assert_eq!((c.list[0], c.reason), ((HEVC, Hw), "hw_hevc_preferred"));
    let c = for_stream(&available(false, true, true, true), ON);
    assert_eq!((c.list[0], c.reason), ((VP9, Hw), "no_hw_av1"));
}
