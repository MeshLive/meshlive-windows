use super::*;

// ---------------------------------------------------------- synthetic units

/// RBSP bit writer for synthetic parameter sets and slice headers.
#[derive(Default)]
struct W {
    bytes: Vec<u8>,
    bits: usize,
}
impl W {
    fn put(&mut self, value: u64, count: u32) -> &mut Self {
        for bit in (0..count).rev() {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= (((value >> bit) & 1) as u8) << (7 - self.bits % 8);
            self.bits += 1;
        }
        self
    }
    fn ue(&mut self, value: u64) -> &mut Self {
        let v = value + 1;
        let len = 64 - v.leading_zeros();
        self.put(0, len - 1).put(v, len)
    }
    fn se(&mut self, value: i64) -> &mut Self {
        self.ue(if value > 0 { (2 * value - 1) as u64 } else { (-2 * value) as u64 })
    }
    /// rbsp_trailing_bits, then `filler` bytes of 0xAA (an opaque slice body).
    fn finish(&mut self, filler: usize) -> Vec<u8> {
        self.put(1, 1);
        while self.bits % 8 != 0 {
            self.put(0, 1);
        }
        self.bytes.extend(std::iter::repeat_n(0xaa, filler));
        std::mem::take(&mut self.bytes)
    }
}
/// Emulation prevention: 03 after two zeros before any byte 00..03.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}
fn hevc_nal(kind: u8, rbsp: &[u8]) -> Vec<u8> {
    [&[kind << 1, 1][..], &escape(rbsp)].concat()
}
fn avc_nal(kind: u8, rbsp: &[u8]) -> Vec<u8> {
    [&[0x60 | kind][..], &escape(rbsp)].concat()
}
fn au(nals: &[Vec<u8>]) -> Vec<u8> {
    let parts: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
    with_start_codes(&parts)
}

#[derive(Clone, Copy)]
struct HevcSps {
    vps_id: u64,
    sps_id: u64,
    width: u64,
    height: u64,
    crop: Option<[u64; 4]>,
    profile: u64,
    level: u64,
    chroma: u64,
    depth: u64,
    reorder: u64,
    dpb: u64,
    sub_layers: u64,
}
impl Default for HevcSps {
    fn default() -> Self {
        Self { vps_id: 0, sps_id: 0, width: 352, height: 288, crop: None, profile: 1, level: 90, chroma: 1, depth: 0, reorder: 0, dpb: 1, sub_layers: 0 }
    }
}
fn hevc_vps(id: u64, layers: u64, sub_layers: u64) -> Vec<u8> {
    let mut w = W::default();
    w.put(id, 4).put(3, 2).put(layers, 6).put(sub_layers, 3).put(1, 1).put(0xffff, 16);
    hevc_nal(32, &w.finish(0))
}
fn hevc_sps(s: HevcSps) -> Vec<u8> {
    let mut w = W::default();
    w.put(s.vps_id, 4).put(s.sub_layers, 3).put(1, 1);
    let compatibility = if s.profile == 1 { 0x6000_0000 } else { 0 };
    w.put(0, 2).put(0, 1).put(s.profile, 5).put(compatibility, 32).put(0b1001, 4).put(0, 44).put(s.level, 8);
    w.ue(s.sps_id).ue(s.chroma).ue(s.width).ue(s.height);
    match s.crop {
        Some(c) => {
            w.put(1, 1);
            for v in c {
                w.ue(v);
            }
        }
        None => {
            w.put(0, 1);
        }
    }
    w.ue(s.depth).ue(s.depth).ue(4).put(1, 1).ue(s.dpb).ue(s.reorder).ue(0);
    hevc_nal(33, &w.finish(0))
}
fn hevc_pps(id: u64, sps_id: u64, extra: u64) -> Vec<u8> {
    let mut w = W::default();
    w.ue(id).ue(sps_id).put(0, 2).put(extra, 3).put(0, 5);
    hevc_nal(34, &w.finish(0))
}
fn hevc_slice(kind: u8, pps_id: u64, extra: u32, slice_type: u64) -> Vec<u8> {
    let mut w = W::default();
    w.put(1, 1);
    if (16..=23).contains(&kind) {
        w.put(0, 1);
    }
    w.ue(pps_id).put(0, extra).ue(slice_type);
    hevc_nal(kind, &w.finish(24))
}
fn hevc_idr(s: HevcSps) -> Vec<u8> {
    au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)])
}
fn hevc_p() -> Vec<u8> {
    au(&[hevc_slice(1, 0, 0, 1)])
}

#[derive(Clone, Copy)]
struct AvcSps {
    profile: u64,
    level: u64,
    sps_id: u64,
    poc_type: u64,
    width_mbs: u64,
    height_mbs: u64,
    frame_mbs_only: u64,
    crop: Option<[u64; 4]>,
    /// Some(reorder): VUI with only bitstream_restriction.
    vui_reorder: Option<u64>,
}
impl Default for AvcSps {
    fn default() -> Self {
        Self { profile: 66, level: 30, sps_id: 0, poc_type: 2, width_mbs: 22, height_mbs: 18, frame_mbs_only: 1, crop: None, vui_reorder: None }
    }
}
fn avc_sps(s: AvcSps) -> Vec<u8> {
    let mut w = W::default();
    w.put(s.profile, 8).put(0xc0, 8).put(s.level, 8).ue(s.sps_id);
    if s.profile == 100 {
        w.ue(1).ue(0).ue(0).put(0, 1).put(1, 1); // scaling matrix present
        for i in 0..8 {
            if i == 0 {
                w.put(1, 1);
                for _ in 0..16 {
                    w.se(1);
                }
            } else {
                w.put(0, 1);
            }
        }
    }
    w.ue(0).ue(s.poc_type);
    if s.poc_type == 0 {
        w.ue(2);
    }
    w.ue(1).put(0, 1).ue(s.width_mbs - 1).ue(s.height_mbs - 1).put(s.frame_mbs_only, 1);
    if s.frame_mbs_only == 0 {
        w.put(0, 1);
    }
    w.put(1, 1);
    match s.crop {
        Some(c) => {
            w.put(1, 1);
            for v in c {
                w.ue(v);
            }
        }
        None => {
            w.put(0, 1);
        }
    }
    match s.vui_reorder {
        Some(reorder) => {
            // No aspect, overscan, signal, chroma, timing or HRD; no pic_struct.
            w.put(1, 1).put(0, 8).put(1, 1).put(1, 1).ue(0).ue(0).ue(16).ue(16).ue(reorder).ue(1);
        }
        None => {
            w.put(0, 1);
        }
    }
    avc_nal(7, &w.finish(0))
}
fn avc_pps(id: u64, sps_id: u64, slice_groups: u64, redundant: u64) -> Vec<u8> {
    let mut w = W::default();
    w.ue(id).ue(sps_id).put(0, 2).ue(slice_groups).ue(0).ue(0).put(0, 3).se(0).se(0).se(0).put(1, 1).put(0, 1).put(redundant, 1);
    avc_nal(8, &w.finish(0))
}
fn avc_slice(kind: u8, first_mb: u64, slice_type: u64, pps_id: u64) -> Vec<u8> {
    let mut w = W::default();
    w.ue(first_mb).ue(slice_type).ue(pps_id);
    avc_nal(kind, &w.finish(24))
}
fn avc_idr(s: AvcSps) -> Vec<u8> {
    au(&[avc_sps(s), avc_pps(0, 0, 0, 0), avc_slice(5, 0, 7, 0)])
}
fn avc_p() -> Vec<u8> {
    au(&[avc_slice(1, 0, 5, 0)])
}

fn hevc_gate() -> Gate {
    Gate::new(Codec::Hevc, 352, 288)
}
fn avc_gate() -> Gate {
    Gate::new(Codec::H264, 352, 288)
}
#[track_caller]
fn rule(result: Result<Accepted, Rejected>) -> u8 {
    result.expect_err("expected a rejection").violation.rule
}

// ----------------------------------------------------------- phone vectors

struct Vector {
    name: &'static str,
    codec: Codec,
    units: Vec<(Vec<u8>, bool)>,
}
/// The Android hardware-encoder vectors in tests/fixtures/hc, hash-checked
/// against their manifests.
fn vectors() -> Vec<Vector> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hc");
    ["s24-hevc", "asus-hevc", "m31-hevc", "a15s-h264"]
        .into_iter()
        .map(|name| {
            let dir = format!("{root}/{name}");
            let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/manifest.json")).unwrap()).unwrap();
            let codec = Codec::of(manifest["codecId"].as_u64().unwrap() as u8).unwrap();
            let units = manifest["accessUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| {
                    let data = std::fs::read(format!("{dir}/{}", u["file"].as_str().unwrap())).unwrap();
                    let hash: String = crate::crypto::sha256(&data).unwrap().iter().map(|b| format!("{b:02x}")).collect();
                    assert_eq!(hash, u["sha256"].as_str().unwrap(), "{name} {}", u["file"]);
                    (data, u["keyFrame"].as_bool().unwrap())
                })
                .collect();
            Vector { name, codec, units }
        })
        .collect()
}
pub(crate) fn fixture_units(name: &str) -> (Codec, Vec<(Vec<u8>, bool)>) {
    let v = vectors().into_iter().find(|v| v.name == name).unwrap();
    (v.codec, v.units)
}

#[test]
fn phone_hardware_encoder_vectors_pass_the_gate_and_activate_352x288() {
    for v in vectors() {
        let mut gate = Gate::new(v.codec, 352, 288);
        let mut activations = 0;
        for (i, (unit, key)) in v.units.iter().enumerate() {
            let accepted = gate.check(unit, *key).unwrap_or_else(|e| panic!("{} unit {i}: {e:?}", v.name));
            assert_eq!(accepted.key, *key);
            if let Some(sets) = &accepted.activated {
                activations += 1;
                assert_eq!((sets.codec, sets.width, sets.height), (v.codec, 352, 288), "{}", v.name);
            }
            // Re-validating the decoder's (stripped) input passes too.
            let mut again = gate.clone();
            if *key {
                again = Gate::new(v.codec, 352, 288);
            }
            assert!(again.check(&accepted.unit, *key).is_ok(), "{} unit {i} stripped", v.name);
        }
        assert_eq!(activations, 1, "{}: the same sets on every keyframe activate once", v.name);
        // A smaller session envelope rejects the 352x288 sets (rule 6).
        assert_eq!(rule(Gate::new(v.codec, 320, 240).check(&v.units[0].0, true)), 6, "{}", v.name);
    }
}

#[test]
fn unusual_phone_values_are_logged_not_rejected() {
    for name in ["s24-hevc", "asus-hevc", "m31-hevc", "a15s-h264"] {
        let (codec, units) = fixture_units(name);
        let sets = Gate::new(codec, 352, 288).check(&units[0].0, true).unwrap().activated.unwrap();
        assert!(sets.unusual.iter().all(|(f, _)| ["general_level_idc", "sps_max_dec_pic_buffering_minus1", "level_idc", "max_num_ref_frames"].contains(f)), "{name}: {:?}", sets.unusual);
    }
}

#[test]
fn the_packetiser_reproduces_the_android_packets_from_raw_encoder_output() {
    for v in vectors() {
        let mut p = Packetiser::new(v.codec);
        for (i, (unit, key)) in v.units.iter().enumerate() {
            // A non-keyframe gains nothing; a keyframe gets the cached sets.
            let out = p.on_access_unit(unit, i as i64).unwrap_or_else(|e| panic!("{} unit {i}: {e:?}", v.name));
            assert_eq!((&out.data, out.key), (unit, *key), "{} unit {i}", v.name);
            assert_eq!(out.size, key.then_some((352, 288)));
        }
        // The same picture without its sets (an encoder that sends them once) gets them back.
        let mut p = Packetiser::new(v.codec);
        p.on_access_unit(&v.units[0].0, 0).unwrap();
        let slice_only = {
            let nals = scan(&v.units[0].0, v.codec, 8).unwrap();
            let slice = nals.iter().find(|n| v.codec.is_slice(n.kind)).unwrap();
            with_start_codes(&[&v.units[0].0[slice.start..slice.end]])
        };
        assert_eq!(p.on_access_unit(&slice_only, 1).unwrap().data, v.units[0].0, "{}", v.name);
        assert_eq!(p.on_access_unit(&v.units[1].0, 1), Err(PacketiseError::Reorder));
    }
}

// --------------------------------------------------------------- rules

#[test]
fn rule_1_framing_escapes_and_counts() {
    let idr = hevc_idr(HevcSps::default());
    let mut g = hevc_gate();
    assert_eq!(rule(g.check(&[], true)), 1);
    assert_eq!(rule(g.check(&idr[4..], true)), 1, "no start code");
    // Three-byte start codes and trailing zeros are accepted.
    let short: Vec<u8> = {
        let nals = scan(&idr, Codec::Hevc, 8).unwrap();
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(&idr[n.start..n.end]);
        }
        out.extend_from_slice(&[0, 0, 0]);
        out
    };
    assert!(hevc_gate().check(&short, true).is_ok());
    for bad in [[0, 0, 3, 4], [0, 0, 2, 9]] {
        let mut unit = idr.clone();
        unit.extend_from_slice(&bad);
        assert_eq!(rule(hevc_gate().check(&unit, true)), 1, "{bad:?}");
    }
    // 00 00 03 as the last bytes of a NAL unit is allowed.
    let mut tail = idr.clone();
    tail.extend_from_slice(&[0, 0, 3]);
    assert!(hevc_gate().check(&tail, true).is_ok());
    // At most 8 NAL units (AUDs are allowed and stripped).
    let aud = hevc_nal(35, &[0x50]);
    let mut nals = vec![aud.clone(); 4];
    nals.extend([hevc_vps(0, 0, 0), hevc_sps(HevcSps::default()), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)]);
    assert!(hevc_gate().check(&au(&nals), true).is_ok(), "exactly 8");
    nals.insert(0, aud);
    assert_eq!(rule(hevc_gate().check(&au(&nals), true)), 1, "9 NAL units");
    // A NAL unit shorter than its header.
    assert_eq!(rule(hevc_gate().check(&[0, 0, 1, 0x26, 0, 0, 1, 0x26], true)), 1);
    // The rejected unit was still a keyframe (an IDR NAL unit before the failure).
    let mut broken = idr.clone();
    broken.extend_from_slice(&[0, 0, 1, 38 << 1, 1, 0xff, 0, 0, 2]);
    assert!(hevc_gate().check(&broken, false).unwrap_err().key);
    // A failure inside the IDR NAL unit itself has not recognised it yet (as Android).
    let mut inside = idr.clone();
    inside.extend_from_slice(&[0, 0, 2]);
    assert!(!hevc_gate().check(&inside, false).unwrap_err().key);
}

#[test]
fn rules_2_and_3_headers_and_types() {
    let mut idr = hevc_idr(HevcSps::default());
    idr[5] = 0x09; // VPS nuh_layer_id 1
    assert_eq!(rule(hevc_gate().check(&idr, true)), 2);
    let mut idr = hevc_idr(HevcSps::default());
    idr[5] = 0x02; // temporal id plus1 = 2
    assert_eq!(rule(hevc_gate().check(&idr, true)), 2);
    let mut avc = avc_idr(AvcSps::default());
    avc[4] |= 0x80;
    assert_eq!(rule(avc_gate().check(&avc, true)), 2);
    // HEVC CRA (21) is deferred; H.264 data partitions are not allowed.
    let cra = au(&[hevc_vps(0, 0, 0), hevc_sps(HevcSps::default()), hevc_pps(0, 0, 0), hevc_slice(21, 0, 0, 2)]);
    assert_eq!(rule(hevc_gate().check(&cra, false)), 3);
    assert_eq!(rule(avc_gate().check(&au(&[avc_slice(2, 0, 5, 0)]), false)), 3);
    assert_eq!(rule(hevc_gate().check(&au(&[hevc_nal(36, &[0x80])]), false)), 3, "end of sequence");
}

#[test]
fn rule_4_one_picture_one_slice_and_slice_headers() {
    let s = HevcSps::default();
    let two = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(hevc_gate().check(&two, true)), 4);
    let filler_first = au(&[hevc_nal(38, &[0xff, 0x80]), hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(hevc_gate().check(&filler_first, true)), 4);
    let sei_after = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2), hevc_nal(39, &[5, 1, 0, 0x80])]);
    assert_eq!(rule(hevc_gate().check(&sei_after, true)), 4, "prefix SEI after the slice");
    let mut later = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0)]);
    let mut w = W::default();
    w.put(0, 1).put(0, 1).ue(0).ue(2);
    later.extend(au(&[hevc_nal(19, &w.finish(8))]));
    assert_eq!(rule(hevc_gate().check(&later, true)), 4, "first_slice_segment_in_pic_flag 0");
    let b = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 0)]);
    assert_eq!(rule(hevc_gate().check(&b, true)), 4, "B slice");
    let p_idr = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 1)]);
    assert_eq!(rule(hevc_gate().check(&p_idr, true)), 4, "IDR must be I");
    // num_extra_slice_header_bits comes from the PPS.
    let extra = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 2), hevc_slice(19, 0, 2, 2)]);
    assert!(hevc_gate().check(&extra, true).is_ok());
    let mut g = avc_gate();
    g.check(&avc_idr(AvcSps::default()), true).unwrap();
    assert_eq!(rule(g.clone().check(&au(&[avc_slice(1, 3, 5, 0)]), false)), 4, "first_mb_in_slice");
    assert_eq!(rule(g.clone().check(&au(&[avc_slice(1, 0, 6, 0)]), false)), 4, "B slice");
    assert_eq!(rule(g.clone().check(&au(&[avc_slice(1, 0, 7, 0)]), false)), 4, "I only in an IDR");
    assert!(g.check(&avc_p(), false).is_ok());
}

#[test]
fn rule_5_keyframes_activation_and_replacement() {
    let s = HevcSps::default();
    let mut g = hevc_gate();
    assert_eq!(rule(g.check(&hevc_p(), false)), 5, "no active configuration");
    assert_eq!(rule(g.check(&hevc_idr(s), false)), 5, "IDR without the keyframe flag");
    assert_eq!(rule(g.check(&hevc_p(), true)), 5, "keyframe flag without an IDR");
    let no_pps = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(g.check(&no_pps, true)), 5);
    let swapped = au(&[hevc_vps(0, 0, 0), hevc_pps(0, 0, 0), hevc_sps(s), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(g.check(&swapped, true)), 5, "order");
    let wrong_vps = au(&[hevc_vps(1, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(g.check(&wrong_vps, true)), 5, "SPS names another VPS");
    let wrong_pps = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 1, 0), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(g.check(&wrong_pps, true)), 5, "PPS names another SPS");
    let wrong_slice = au(&[hevc_vps(0, 0, 0), hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 1, 0, 2)]);
    assert_eq!(rule(g.check(&wrong_slice, true)), 5, "slice names another PPS");

    let mut g = hevc_gate();
    let first = g.check(&hevc_idr(s), true).unwrap();
    let active = first.activated.unwrap();
    assert!(g.check(&hevc_p(), false).is_ok());
    let with_sets = au(&[hevc_sps(s), hevc_slice(1, 0, 0, 1)]);
    assert_eq!(rule(g.check(&with_sets, false)), 5, "parameter set in a non-IDR unit");
    assert_eq!(rule(g.check(&au(&[hevc_slice(1, 3, 0, 1)]), false)), 5, "not the active PPS");
    // The same sets again: nothing new is activated.
    assert!(g.check(&hevc_idr(s), true).unwrap().activated.is_none());
    // Same-ID replacement at an IDR (a new size) activates atomically.
    let wide = HevcSps { width: 320, height: 240, ..s };
    let replaced = g.check(&hevc_idr(wide), true).unwrap().activated.unwrap();
    assert_eq!((replaced.width, replaced.height), (320, 240));
    // A rejected replacement keeps the old active sets, and non-IDR units
    // are dropped until the next valid IDR.
    let bad = HevcSps { reorder: 1, ..s };
    assert_eq!(rule(g.check(&hevc_idr(bad), true)), 8);
    assert_eq!(g.active().map(|a| (a.width, a.height)), Some((320, 240)));
    assert!(g.waiting_for_idr());
    assert_eq!(rule(g.check(&hevc_p(), false)), 5);
    let again = g.check(&hevc_idr(s), true).unwrap();
    assert_eq!(again.activated.map(|a| (a.width, a.height)), Some((352, 288)));
    assert!(!g.waiting_for_idr());
    assert!(g.check(&hevc_p(), false).is_ok());
    assert_ne!(active.annex_b, replaced.annex_b);

    // Parameter sets: 1,024 bytes pass, 1,025 reject (0xAA padding after the trailing bits).
    let padded = |extra: usize| {
        let mut sps = hevc_sps(s);
        sps.extend(std::iter::repeat_n(0xaa, extra));
        sps
    };
    let base = hevc_sps(s).len();
    let at = |len: usize| au(&[hevc_vps(0, 0, 0), padded(len - base), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)]);
    assert!(hevc_gate().check(&at(1024), true).is_ok());
    assert_eq!(rule(hevc_gate().check(&at(1025), true)), 5);
}

#[test]
fn rule_6_envelope_crop_and_padding() {
    let s = HevcSps::default();
    let call = |w: u32, h: u32| Gate::new(Codec::Hevc, w, h);
    // HEVC (root decision 2026-10-28): NVIDIA's 448x256 for a 426x240 header,
    // cropped back to exactly 426x240, is accepted.
    let nvidia = HevcSps { width: 448, height: 256, crop: Some([0, 11, 0, 8]), ..s };
    let sets = call(426, 240).check(&hevc_idr(nvidia), true).unwrap().activated.unwrap();
    assert_eq!((sets.width, sets.height), (426, 240));
    // 640x384 -> 640x360 and 864x480 -> 854x480, as measured.
    assert!(call(640, 360).check(&hevc_idr(HevcSps { width: 640, height: 384, crop: Some([0, 0, 0, 12]), ..s }), true).is_ok());
    assert!(call(854, 480).check(&hevc_idr(HevcSps { width: 864, height: 480, crop: Some([0, 5, 0, 0]), ..s }), true).is_ok());
    // 490 coded for 426 (not a multiple of 8, and 64 or more of padding).
    assert_eq!(rule(call(426, 240).check(&hevc_idr(HevcSps { width: 490, height: 240, crop: Some([0, 32, 0, 0]), ..s }), true)), 6);
    // A multiple of 8 with 64 or more of padding: 496 for 426 (70).
    assert_eq!(rule(call(426, 240).check(&hevc_idr(HevcSps { width: 496, height: 240, crop: Some([0, 35, 0, 0]), ..s }), true)), 6);
    // Not a multiple of 8, even within the padding: 434 for 426.
    assert_eq!(rule(call(426, 240).check(&hevc_idr(HevcSps { width: 434, height: 240, crop: Some([0, 4, 0, 0]), ..s }), true)), 6);
    // A smaller ladder rung under a maximum-size header: 320x192 coded,
    // 320x180 cropped, under 426x240.
    let rung = call(426, 240).check(&hevc_idr(HevcSps { width: 320, height: 192, crop: Some([0, 0, 0, 6]), ..s }), true).unwrap();
    assert_eq!(rung.activated.map(|a| (a.width, a.height)), Some((320, 180)));
    // Cropped larger than the header: 432x240 uncropped under 426x240, and a
    // crop that leaves 426x256.
    assert_eq!(rule(call(426, 240).check(&hevc_idr(HevcSps { width: 432, height: 240, ..s }), true)), 6);
    assert_eq!(rule(call(426, 240).check(&hevc_idr(HevcSps { width: 448, height: 256, crop: Some([0, 11, 0, 0]), ..s }), true)), 6);
    // Either orientation, and an odd crop inside the header.
    assert!(hevc_gate().check(&hevc_idr(HevcSps { width: 288, height: 352, ..s }), true).is_ok());
    let cropped = hevc_gate().check(&hevc_idr(HevcSps { crop: Some([0, 1, 0, 4]), ..s }), true).unwrap();
    assert_eq!(cropped.activated.map(|a| (a.width, a.height)), Some((350, 280)));
    assert_eq!(rule(hevc_gate().check(&hevc_idr(HevcSps { crop: Some([0, 169, 0, 0]), ..s }), true)), 6, "visible below 16");
    assert_eq!(rule(hevc_gate().check(&hevc_idr(HevcSps { crop: Some([u32::MAX as u64 - 1, 9, 0, 0]), ..s }), true)), 6, "overflowing crop");
    assert_eq!(rule(hevc_gate().check(&hevc_idr(HevcSps { width: 0, ..s }), true)), 6);
    // H.264 keeps the multiple-of-16 envelope (either orientation, smaller allowed).
    let avc = AvcSps { crop: Some([0, 0, 0, 4]), ..AvcSps::default() };
    assert_eq!(avc_gate().check(&avc_idr(avc), true).unwrap().activated.map(|a| (a.width, a.height)), Some((352, 280)));
    assert!(avc_gate().check(&avc_idr(AvcSps { width_mbs: 18, height_mbs: 22, ..AvcSps::default() }), true).is_ok());
    assert_eq!(rule(avc_gate().check(&avc_idr(AvcSps { width_mbs: 23, ..AvcSps::default() }), true)), 6);
    assert!(Gate::new(Codec::H264, 426, 240).check(&avc_idr(AvcSps { width_mbs: 27, height_mbs: 15, crop: Some([0, 3, 0, 0]), ..AvcSps::default() }), true).is_ok());
}

#[test]
fn rules_7_and_8_hevc_vps_and_sps() {
    let s = HevcSps::default();
    let with_vps = |vps| au(&[vps, hevc_sps(s), hevc_pps(0, 0, 0), hevc_slice(19, 0, 0, 2)]);
    assert_eq!(rule(hevc_gate().check(&with_vps(hevc_vps(0, 1, 0)), true)), 7);
    assert_eq!(rule(hevc_gate().check(&with_vps(hevc_vps(0, 0, 1)), true)), 7);
    for (bad, what) in [
        (HevcSps { sub_layers: 1, ..s }, "sub-layers"),
        (HevcSps { profile: 2, ..s }, "Main 10"),
        (HevcSps { chroma: 2, ..s }, "4:2:2"),
        (HevcSps { depth: 2, ..s }, "10-bit"),
        (HevcSps { reorder: 1, ..s }, "reordering"),
        (HevcSps { level: 187, ..s }, "level above 6.2"),
        (HevcSps { dpb: 16, ..s }, "DPB above 16"),
        (HevcSps { sps_id: 16, ..s }, "SPS ID above 15"),
    ] {
        assert_eq!(rule(hevc_gate().check(&hevc_idr(bad), true)), 8, "{what}");
    }
    let unusual = hevc_gate().check(&hevc_idr(HevcSps { level: 186, dpb: 15, ..s }), true).unwrap().activated.unwrap();
    assert_eq!(unusual.unusual, vec![("general_level_idc", 186), ("sps_max_dec_pic_buffering_minus1", 15)]);
}

#[test]
fn rules_9_and_10_avc_sps_and_pps() {
    let s = AvcSps::default();
    for (bad, what) in [
        (AvcSps { profile: 88, ..s }, "Extended"),
        (AvcSps { level: 63, ..s }, "level above 6.2"),
        (AvcSps { frame_mbs_only: 0, ..s }, "fields"),
        (AvcSps { poc_type: 0, ..s }, "POC 0 without VUI"),
        (AvcSps { poc_type: 0, vui_reorder: Some(1), ..s }, "POC 0 with reordering"),
    ] {
        assert_eq!(rule(avc_gate().check(&avc_idr(bad), true)), 9, "{what}");
    }
    for good in [AvcSps { poc_type: 0, vui_reorder: Some(0), ..s }, AvcSps { profile: 100, ..s }, AvcSps { profile: 77, level: 42, ..s }] {
        assert!(avc_gate().check(&avc_idr(good), true).is_ok());
    }
    let fmo = au(&[avc_sps(s), avc_pps(0, 0, 1, 0), avc_slice(5, 0, 7, 0)]);
    assert_eq!(rule(avc_gate().check(&fmo, true)), 10);
    let redundant = au(&[avc_sps(s), avc_pps(0, 0, 0, 1), avc_slice(5, 0, 7, 0)]);
    assert_eq!(rule(avc_gate().check(&redundant, true)), 10);
    let hevc_pps_id = au(&[hevc_vps(0, 0, 0), hevc_sps(HevcSps::default()), hevc_pps(64, 0, 0), hevc_slice(19, 64, 0, 2)]);
    assert_eq!(rule(hevc_gate().check(&hevc_pps_id, true)), 10);
}

#[test]
fn rule_11_sei_aud_and_filler_are_removed() {
    let s = HevcSps::default();
    let full = au(&[
        hevc_nal(35, &[0x50]),
        hevc_vps(0, 0, 0),
        hevc_sps(s),
        hevc_pps(0, 0, 0),
        hevc_nal(39, &[5, 1, 0, 0x80]),
        hevc_slice(19, 0, 0, 2),
        hevc_nal(40, &[5, 1, 0, 0x80]),
        hevc_nal(38, &[0xff, 0x80]),
    ]);
    assert_eq!(hevc_gate().check(&full, true).unwrap().unit, hevc_idr(s));
    let avc = au(&[avc_nal(9, &[0x10]), avc_nal(6, &[5, 1, 0, 0x80]), avc_sps(AvcSps::default()), avc_pps(0, 0, 0, 0), avc_slice(5, 0, 7, 0), avc_nal(12, &[0xff, 0x80])]);
    assert_eq!(avc_gate().check(&avc, true).unwrap().unit, avc_idr(AvcSps::default()));
}

#[test]
fn exp_golomb_and_parse_budget_bounds() {
    // 32 leading zeros reject; 31 parse (and then fail a range check).
    let mut w = W::default();
    w.put(0, 32).put(1, 1).put(0, 32);
    let mut r = Bits { data: w.finish(0), bit: 0, rule: 8, nal: 33 };
    assert_eq!(r.ue().unwrap_err().reason, "ue with more than 31 leading zeros");
    let mut w = W::default();
    w.put(0, 31).put(1, 1).put(0x7fff_ffff, 31);
    let mut r = Bits { data: w.finish(0), bit: 0, rule: 8, nal: 33 };
    assert_eq!(r.ue().unwrap(), u64::from(u32::MAX) - 1);
    // An H.264 SPS whose VUI lies beyond the 512-byte budget rejects (rule 9):
    // a NAL HRD with 32 entries of 63-bit values pushes the VUI end past it.
    let mut w = W::default();
    w.put(66, 8).put(0xc0, 8).put(30, 8).ue(0).ue(0).ue(0).ue(2).ue(1).put(0, 1).ue(21).ue(17).put(1, 1).put(1, 1).put(0, 1);
    w.put(1, 1).put(0, 5).put(1, 1).ue(31).put(0, 8);
    for _ in 0..32 {
        w.ue(1 << 31).ue(1 << 31).put(0, 1);
    }
    w.put(0, 20).put(0, 1).put(0, 1).put(0, 1).put(1, 1).put(1, 1).ue(0).ue(0).ue(16).ue(16).ue(0).ue(1);
    let long_sps = avc_nal(7, &w.finish(0));
    assert!(long_sps.len() > 512 + 1);
    let unit = au(&[long_sps, avc_pps(0, 0, 0, 0), avc_slice(5, 0, 7, 0)]);
    let rejected = avc_gate().check(&unit, true).unwrap_err();
    assert_eq!(rejected.violation.rule, 9);
    assert!(rejected.violation.reason.ends_with("past the end or the parse budget"), "{:?}", rejected.violation);
    // Emulation-prevention bytes inside a parameter set are removed before parsing.
    assert!(hevc_sps(HevcSps::default()).windows(3).any(|w| w == [0, 0, 3]));
    assert!(hevc_gate().check(&hevc_idr(HevcSps::default()), true).is_ok());
}

#[test]
fn three_rejected_keyframes_within_ten_seconds_close() {
    let s = 1_000_000_000;
    let mut k = KeyframeRejections::default();
    assert!(!k.rejected(0));
    assert!(!k.rejected(4 * s));
    assert!(k.rejected(10 * s), "three within 10 s");
    let mut k = KeyframeRejections::default();
    assert!(!k.rejected(0));
    assert!(!k.rejected(6 * s));
    assert!(!k.rejected(11 * s), "spread over 11 s");
    assert!(k.rejected(12 * s), "the last three within 6 s");
    k.accepted_keyframe();
    assert!(!k.rejected(13 * s));
    assert!(!k.rejected(13 * s));
}

#[test]
fn session_levels_are_the_lowest_covering_level() {
    // DECISIONS "Encoder level": HEVC 640x480 and 854x480 at level 3, 720p at 3.1;
    // H.264 640x480 at 15 FPS at level 2.2.
    assert_eq!(session_level(Codec::Hevc, 640, 480, 30, 1_000_000), 90);
    assert_eq!(session_level(Codec::Hevc, 854, 480, 30, 1_500_000), 90);
    assert_eq!(session_level(Codec::Hevc, 1280, 720, 30, 2_000_000), 93);
    assert_eq!(session_level(Codec::Hevc, 426, 240, 15, 200_000), 60);
    assert_eq!(session_level(Codec::H264, 640, 480, 15, 400_000), 22);
    assert_eq!(session_level(Codec::H264, 426, 240, 15, 200_000), 21);
    assert_eq!(session_level(Codec::H264, 7680, 4320, 120, 900_000_000), 52, "the highest when none covers");
}
