//! Design HC: Annex B access units for codecs 4 (H.264) and 5 (HEVC)
//! (PROTOCOL "Draft: HEVC and H.264 video codecs"). Written from ITU-T H.264
//! and H.265 and the rules shared with Android (`AccessUnitGate`,
//! `AnnexBPacketiser`); no external code.
//!
//! - `Gate`: the receiver's fail-closed structural gate (rules 1-11). It reads
//!   only the fields PROTOCOL names, within the parse budgets, and is not a
//!   sanitizer: an access unit that passes still reaches a platform decoder.
//!   Parameter sets become active only inside a fully valid IDR access unit.
//! - `Packetiser`: the sender's one-AU-per-packet rules (sets cached and
//!   prepended to every IDR, stripped elsewhere; SEI, AUD and filler removed;
//!   the keyframe flag from the NAL type).
//! - `session_level`: the lowest standard level covering a session.
use crate::media::MAX_PAYLOAD;
use std::sync::Arc;

pub const MAX_NALS_PER_ACCESS_UNIT: usize = 8;
pub const PARAMETER_SET_BUDGET: usize = 512;
pub const SLICE_HEADER_BUDGET: usize = 64;
pub const MAX_PARAMETER_SET_BYTES: usize = 1024;
/// Three rejected keyframes in a row within this window close the session.
pub const REJECTED_KEYFRAME_WINDOW_NS: i64 = 10_000_000_000;
const MIN_VISIBLE: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}
impl Codec {
    pub fn of(id: u8) -> Option<Self> {
        match id {
            4 => Some(Self::H264),
            5 => Some(Self::Hevc),
            _ => None,
        }
    }
    pub fn id(self) -> u8 {
        match self {
            Self::H264 => 4,
            Self::Hevc => 5,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Hevc => "HEVC",
        }
    }
    fn header_bytes(self) -> usize {
        match self {
            Self::H264 => 1,
            Self::Hevc => 2,
        }
    }
    pub fn nal_type(self, first: u8) -> u8 {
        match self {
            Self::H264 => first & 0x1f,
            Self::Hevc => (first >> 1) & 0x3f,
        }
    }
    /// Rule 2.
    fn header_valid(self, nal: &[u8]) -> bool {
        match self {
            Self::H264 => nal.first().is_some_and(|b| b & 0x80 == 0),
            Self::Hevc => {
                nal.len() >= 2 && nal[0] & 0x80 == 0 && ((nal[0] & 1) << 5 | nal[1] >> 3) == 0 && nal[1] & 7 == 1
            }
        }
    }
    /// Rule 3.
    fn allowed(self, t: u8) -> bool {
        match self {
            Self::H264 => matches!(t, 1 | 5 | 6 | 7 | 8 | 9 | 12),
            Self::Hevc => matches!(t, 0 | 1 | 19 | 20 | 32..=35 | 38..=40),
        }
    }
    fn is_slice(self, t: u8) -> bool {
        match self {
            Self::H264 => t == 1 || t == 5,
            Self::Hevc => matches!(t, 0 | 1 | 19 | 20),
        }
    }
    pub fn is_idr(self, t: u8) -> bool {
        match self {
            Self::H264 => t == 5,
            Self::Hevc => t == 19 || t == 20,
        }
    }
    /// SEI, AUD and filler: removed before decoding, contents never parsed.
    fn is_stripped(self, t: u8) -> bool {
        match self {
            Self::H264 => matches!(t, 6 | 9 | 12),
            Self::Hevc => matches!(t, 35 | 38 | 39 | 40),
        }
    }
    /// Filler and HEVC suffix SEI follow the slice.
    fn is_trailing(self, t: u8) -> bool {
        match self {
            Self::H264 => t == 12,
            Self::Hevc => t == 38 || t == 40,
        }
    }
    fn is_vcl(self, t: u8) -> bool {
        match self {
            Self::H264 => (1..=5).contains(&t),
            Self::Hevc => t <= 31,
        }
    }
    fn vps(self) -> Option<u8> {
        match self {
            Self::H264 => None,
            Self::Hevc => Some(32),
        }
    }
    fn sps(self) -> u8 {
        match self {
            Self::H264 => 7,
            Self::Hevc => 33,
        }
    }
    fn pps(self) -> u8 {
        match self {
            Self::H264 => 8,
            Self::Hevc => 34,
        }
    }
    fn is_parameter_set(self, t: u8) -> bool {
        Some(t) == self.vps() || t == self.sps() || t == self.pps()
    }
}

/// A violated validation rule (PROTOCOL rule 1..11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Violation {
    pub rule: u8,
    pub nal_type: Option<u8>,
    pub reason: &'static str,
}
type R<T> = Result<T, Violation>;
fn violation(rule: u8, nal_type: Option<u8>, reason: &'static str) -> Violation {
    Violation { rule, nal_type, reason }
}

// ------------------------------------------------------------- scanning

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nal {
    kind: u8,
    start: usize,
    end: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanFail {
    reason: &'static str,
    /// An IDR NAL unit was found before the failure (the unit was a keyframe).
    saw_idr: bool,
}

fn start_code_end(p: &[u8]) -> Option<usize> {
    if p.starts_with(&[0, 0, 1]) {
        Some(3)
    } else if p.starts_with(&[0, 0, 0, 1]) {
        Some(4)
    } else {
        None
    }
}

/// Rule 1 and "Scanning and escapes": the payload starts with a 3- or 4-byte
/// start code. Inside a NAL unit `00 00 03` followed by 00..03 is an escape,
/// `00 00 03` as the last bytes is allowed, `00 00 03` followed by a byte
/// above 03 and `00 00 02` reject, `00 00 00` and `00 00 01` end the unit
/// (zero bytes before a start code are trailing zeros). At most `max` units.
fn scan(p: &[u8], codec: Codec, max: usize) -> Result<Vec<Nal>, ScanFail> {
    let n = p.len();
    let mut out: Vec<Nal> = Vec::new();
    let fail = |reason, out: &Vec<Nal>| ScanFail { reason, saw_idr: out.iter().any(|x| codec.is_idr(x.kind)) };
    if n == 0 {
        return Err(fail("empty payload", &out));
    }
    let Some(mut s) = start_code_end(p) else { return Err(fail("no start code", &out)) };
    loop {
        if out.len() >= max {
            return Err(fail("too many NAL units", &out));
        }
        let mut k = s;
        let mut end = None;
        let mut next = None;
        while k < n {
            if p[k] != 0 || k + 1 >= n || p[k + 1] != 0 {
                k += 1;
                continue;
            }
            if k + 2 >= n {
                end = Some(k);
                break;
            }
            match p[k + 2] {
                0 => {
                    let mut q = k + 2;
                    while q < n && p[q] == 0 {
                        q += 1;
                    }
                    if q >= n {
                        end = Some(k);
                    } else if p[q] == 1 {
                        end = Some(k);
                        next = Some(q + 1);
                    } else {
                        return Err(fail("forbidden zero run", &out));
                    }
                    break;
                }
                1 => {
                    end = Some(k);
                    next = Some(k + 3);
                    break;
                }
                2 => return Err(fail("forbidden 00 00 02", &out)),
                3 => {
                    if k + 3 < n && p[k + 3] > 3 {
                        return Err(fail("bad escape", &out));
                    }
                    k += 3;
                }
                _ => k += 3,
            }
        }
        let end = end.unwrap_or_else(|| {
            let mut e = n;
            while e > s && p[e - 1] == 0 {
                e -= 1;
            }
            e
        });
        if end - s < codec.header_bytes() {
            return Err(fail("short NAL unit", &out));
        }
        out.push(Nal { kind: codec.nal_type(p[s]), start: s, end });
        match next {
            Some(at) => s = at,
            None => break,
        }
    }
    Ok(out)
}

/// The unescaped bytes of `nal` after its `from`-byte header, at most
/// `budget` of them: a field beyond the budget reads past the end and rejects.
fn unescape(nal: &[u8], from: usize, budget: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(budget.min(nal.len().saturating_sub(from)));
    let mut zeros = 0;
    for &b in nal.iter().skip(from) {
        if out.len() >= budget {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Bit reader over an unescaped, budget-limited RBSP; every failure is a
/// violation of `rule`.
struct Bits {
    data: Vec<u8>,
    bit: usize,
    rule: u8,
    nal: u8,
}
impl Bits {
    fn parameter_set(nal: &[u8], codec: Codec, rule: u8) -> Self {
        Self { data: unescape(nal, codec.header_bytes(), PARAMETER_SET_BUDGET), bit: 0, rule, nal: codec.nal_type(nal[0]) }
    }
    fn slice(nal: &[u8], codec: Codec) -> Self {
        Self { data: unescape(nal, codec.header_bytes(), SLICE_HEADER_BUDGET), bit: 0, rule: 4, nal: codec.nal_type(nal[0]) }
    }
    fn fail(&self, reason: &'static str) -> Violation {
        violation(self.rule, Some(self.nal), reason)
    }
    fn total(&self) -> usize {
        self.data.len() * 8
    }
    fn next(&mut self) -> u64 {
        let b = (self.data[self.bit / 8] >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        u64::from(b)
    }
    /// `n` in 0..=32.
    fn read(&mut self, n: u32) -> R<u64> {
        if self.bit + n as usize > self.total() {
            return Err(self.fail("read past the end or the parse budget"));
        }
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.next();
        }
        Ok(v)
    }
    fn u(&mut self, n: u32) -> R<u32> {
        self.read(n).map(|v| v as u32)
    }
    fn flag(&mut self) -> R<bool> {
        Ok(self.read(1)? == 1)
    }
    fn skip(&mut self, n: usize) -> R<()> {
        if self.bit + n > self.total() {
            return Err(self.fail("skip past the end or the parse budget"));
        }
        self.bit += n;
        Ok(())
    }
    /// Unsigned Exp-Golomb: more than 31 leading zeros rejects, so the value
    /// always fits 32 bits.
    fn ue(&mut self) -> R<u64> {
        let mut zeros = 0;
        loop {
            if self.bit >= self.total() {
                return Err(self.fail("ue past the end or the parse budget"));
            }
            if self.next() == 1 {
                break;
            }
            zeros += 1;
            if zeros > 31 {
                return Err(self.fail("ue with more than 31 leading zeros"));
            }
        }
        Ok((1u64 << zeros) - 1 + self.read(zeros)?)
    }
    fn ue_max(&mut self, max: u32) -> R<u32> {
        let v = self.ue()?;
        if v > u64::from(max) {
            return Err(self.fail("ue value above its maximum"));
        }
        Ok(v as u32)
    }
    fn se(&mut self) -> R<i64> {
        let k = self.ue()? as i64;
        Ok(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

// ---------------------------------------------------------- envelope

/// Rule 6. H.264: the larger coded side against the larger header side and
/// the smaller against the smaller (either orientation), each header side
/// rounded up to a multiple of 16. HEVC (root decision 2026-10-28, after
/// NVIDIA's HEVC MFT padded 426x240 to 448x256): the cropped
/// (conformance-window) size fits the header with the same orientation
/// handling (smaller ladder rungs under a maximum-size header stay valid),
/// and each coded side is a multiple of MinCbSize 8 and exceeds its cropped
/// side by less than 64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    big: u64,
    small: u64,
    /// The BTV3 header's larger and smaller side, unrounded (None: the
    /// sender's own unbounded parse).
    header: Option<(u64, u64)>,
}
/// HEVC: the coded side may exceed the cropped side by less than this.
pub const HEVC_MAX_PADDING: u64 = 64;
/// HEVC: coded sides are multiples of this (MinCbSizeY).
pub const HEVC_MIN_CB: u64 = 8;
impl Envelope {
    pub fn new(width: u32, height: u32) -> Self {
        let round = |v: u32| (u64::from(v) + 15) / 16 * 16;
        let (w, h) = (round(width), round(height));
        let (width, height) = (u64::from(width), u64::from(height));
        Self { big: w.max(h), small: w.min(h), header: Some((width.max(height), width.min(height))) }
    }
    pub const UNBOUNDED: Self = Self { big: u32::MAX as u64, small: u32::MAX as u64, header: None };
    /// HEVC coded size, before the crop is read: positive multiples of 8,
    /// each side within the header's (either orientation) plus the padding
    /// allowance, so nothing larger is parsed further.
    fn hevc_coded(self, width: u64, height: u64) -> R<()> {
        let aligned = |coded: u64| coded > 0 && coded % HEVC_MIN_CB == 0;
        let within = self.header.is_none_or(|(big, small)| {
            width.max(height) < big + HEVC_MAX_PADDING && width.min(height) < small + HEVC_MAX_PADDING
        });
        if !aligned(width) || !aligned(height) || !within {
            return Err(violation(6, None, "HEVC coded size not a multiple of 8 within the header plus 63"));
        }
        Ok(())
    }
    /// HEVC cropped size: padding below 64 per axis, and within the header
    /// (larger side against the larger, smaller against the smaller).
    fn hevc_visible(self, width: u64, height: u64, crop: [u64; 4]) -> R<(u32, u32)> {
        let w = self.visible(width, crop[0], crop[1])?;
        let h = self.visible(height, crop[2], crop[3])?;
        if width - u64::from(w) >= HEVC_MAX_PADDING || height - u64::from(h) >= HEVC_MAX_PADDING {
            return Err(violation(6, None, "HEVC padding of 64 or more"));
        }
        let (w64, h64) = (u64::from(w), u64::from(h));
        if self.header.is_some_and(|(big, small)| w64.max(h64) > big || w64.min(h64) > small) {
            return Err(violation(6, None, "HEVC cropped size larger than the header"));
        }
        Ok((w, h))
    }
    fn require_coded(self, width: u64, height: u64) -> R<()> {
        if width == 0 || height == 0 || width.max(height) > self.big || width.min(height) > self.small {
            return Err(violation(6, None, "coded size outside the session envelope"));
        }
        Ok(())
    }
    /// The visible size after cropping (crop units of 2 luma samples, 4:2:0
    /// frames), with checked arithmetic; at least 16.
    fn visible(self, coded: u64, a: u64, b: u64) -> R<u32> {
        let visible = a.checked_add(b).and_then(|c| c.checked_mul(2)).and_then(|c| coded.checked_sub(c));
        match visible {
            Some(v) if (MIN_VISIBLE..=u64::from(u32::MAX)).contains(&v) => Ok(v as u32),
            _ => Err(violation(6, None, "visible size below 16")),
        }
    }
}

// ------------------------------------------------------- parameter sets

struct SpsInfo {
    id: u32,
    vps_id: Option<u32>,
    width: u32,
    height: u32,
    unusual: Vec<(&'static str, u32)>,
}
struct PpsInfo {
    id: u32,
    sps_id: u32,
    extra_slice_header_bits: u32,
}
struct SliceInfo {
    pps_id: u32,
}

/// Rule 7.
fn hevc_vps(nal: &[u8]) -> R<u32> {
    let mut r = Bits::parameter_set(nal, Codec::Hevc, 7);
    let id = r.u(4)?;
    r.skip(2)?; // vps_base_layer_internal_flag, vps_base_layer_available_flag
    if r.u(6)? != 0 {
        return Err(r.fail("vps_max_layers_minus1 not 0"));
    }
    if r.u(3)? != 0 {
        return Err(r.fail("vps_max_sub_layers_minus1 not 0"));
    }
    Ok(id)
}

/// Rules 6 and 8.
fn hevc_sps(nal: &[u8], env: Envelope) -> R<SpsInfo> {
    let mut r = Bits::parameter_set(nal, Codec::Hevc, 8);
    let mut unusual = Vec::new();
    let vps_id = r.u(4)?;
    if r.u(3)? != 0 {
        return Err(r.fail("sps_max_sub_layers_minus1 not 0"));
    }
    r.skip(1)?; // sps_temporal_id_nesting_flag
    // profile_tier_level without sub-layers: the fixed 12-byte general part.
    if r.u(2)? != 0 {
        return Err(r.fail("general_profile_space not 0"));
    }
    r.skip(1)?; // general_tier_flag
    let profile = r.u(5)?;
    let compatibility = r.read(32)?;
    if profile != 1 && (compatibility >> 30) & 1 == 0 {
        return Err(r.fail("not Main profile"));
    }
    r.skip(48)?; // source flags, 43 reserved bits and one more
    let level = r.u(8)?;
    if level > 186 {
        return Err(r.fail("general_level_idc above 186"));
    }
    if level > 123 {
        unusual.push(("general_level_idc", level));
    }
    let id = r.ue_max(15)?;
    if r.ue()? != 1 {
        return Err(r.fail("chroma_format_idc not 1"));
    }
    let width = r.ue()?;
    let height = r.ue()?;
    env.hevc_coded(width, height)?;
    let mut crop = [0u64; 4];
    if r.flag()? {
        for c in &mut crop {
            *c = r.ue()?;
        }
    }
    if r.ue()? != 0 {
        return Err(r.fail("bit_depth_luma not 8"));
    }
    if r.ue()? != 0 {
        return Err(r.fail("bit_depth_chroma not 8"));
    }
    r.ue_max(12)?; // log2_max_pic_order_cnt_lsb_minus4
    r.flag()?; // sps_sub_layer_ordering_info_present_flag: one entry either way
    let dpb = r.ue_max(15)?;
    if dpb > 5 {
        unusual.push(("sps_max_dec_pic_buffering_minus1", dpb));
    }
    if r.ue()? != 0 {
        return Err(r.fail("sps_max_num_reorder_pics not 0"));
    }
    let (visible_width, visible_height) = env.hevc_visible(width, height, crop)?;
    Ok(SpsInfo { id, vps_id: Some(vps_id), width: visible_width, height: visible_height, unusual })
}

/// Rule 10 (HEVC).
fn hevc_pps(nal: &[u8]) -> R<PpsInfo> {
    let mut r = Bits::parameter_set(nal, Codec::Hevc, 10);
    let id = r.ue_max(63)?;
    let sps_id = r.ue_max(15)?;
    r.skip(2)?; // dependent_slice_segments_enabled_flag, output_flag_present_flag
    let extra_slice_header_bits = r.u(3)?;
    Ok(PpsInfo { id, sps_id, extra_slice_header_bits })
}

/// Rule 4 (HEVC).
fn hevc_slice(nal: &[u8], kind: u8, extra_bits: u32) -> R<SliceInfo> {
    let mut r = Bits::slice(nal, Codec::Hevc);
    if !r.flag()? {
        return Err(r.fail("first_slice_segment_in_pic_flag not 1"));
    }
    if (16..=23).contains(&kind) {
        r.skip(1)?; // no_output_of_prior_pics_flag
    }
    let pps_id = r.ue_max(63)?;
    r.skip(extra_bits as usize)?;
    let intra = match r.ue()? {
        1 => false,
        2 => true,
        _ => return Err(r.fail("slice_type not P or I")),
    };
    if Codec::Hevc.is_idr(kind) && !intra {
        return Err(r.fail("IDR slice_type not I"));
    }
    Ok(SliceInfo { pps_id })
}

/// Rules 6 and 9.
fn avc_sps(nal: &[u8], env: Envelope) -> R<SpsInfo> {
    let mut r = Bits::parameter_set(nal, Codec::H264, 9);
    let mut unusual = Vec::new();
    let profile = r.u(8)?;
    if !matches!(profile, 66 | 77 | 100) {
        return Err(r.fail("profile_idc not 66, 77 or 100"));
    }
    r.skip(8)?; // constraint_set flags and reserved bits
    let level = r.u(8)?;
    if level > 62 {
        return Err(r.fail("level_idc above 62"));
    }
    if level > 41 {
        unusual.push(("level_idc", level));
    }
    let id = r.ue_max(31)?;
    if profile == 100 {
        if r.ue()? != 1 {
            return Err(r.fail("chroma_format_idc not 1"));
        }
        if r.ue()? != 0 {
            return Err(r.fail("bit_depth_luma not 8"));
        }
        if r.ue()? != 0 {
            return Err(r.fail("bit_depth_chroma not 8"));
        }
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            // seq_scaling_matrix_present_flag
            for i in 0..8 {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue_max(12)?; // log2_max_frame_num_minus4
    let poc_type = r.ue_max(2)?;
    match poc_type {
        0 => {
            r.ue_max(12)?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.skip(1)?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue_max(255)? {
                r.se()?;
            }
        }
        _ => {}
    }
    let refs = r.ue_max(16)?;
    if refs > 4 {
        unusual.push(("max_num_ref_frames", refs));
    }
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width = (r.ue()? + 1) * 16;
    let height = (r.ue()? + 1) * 16;
    if !r.flag()? {
        return Err(r.fail("frame_mbs_only_flag not 1"));
    }
    r.skip(1)?; // direct_8x8_inference_flag
    env.require_coded(width, height)?;
    let mut crop = [0u64; 4];
    if r.flag()? {
        for c in &mut crop {
            *c = r.ue()?;
        }
    }
    if poc_type != 2 {
        if !r.flag()? {
            return Err(r.fail("pic_order_cnt_type 0 or 1 without VUI"));
        }
        if !vui_reorder_zero(&mut r)? {
            return Err(r.fail("pic_order_cnt_type 0 or 1 without max_num_reorder_frames 0"));
        }
    }
    let visible_width = env.visible(width, crop[0], crop[1])?;
    let visible_height = env.visible(height, crop[2], crop[3])?;
    Ok(SpsInfo { id, vps_id: None, width: visible_width, height: visible_height, unusual })
}

/// scaling_list(): at most 64 se(v) entries.
fn skip_scaling_list(r: &mut Bits, size: usize) -> R<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return Err(r.fail("delta_scale out of range"));
            }
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

fn skip_hrd(r: &mut Bits) -> R<()> {
    let count = r.ue_max(31)? + 1;
    r.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.skip(1)?; // cbr_flag
    }
    r.skip(20) // four 5-bit length fields
}

/// The VUI up to bitstream_restriction: true when bitstream_restriction_flag
/// is 1 and max_num_reorder_frames 0.
fn vui_reorder_zero(r: &mut Bits) -> R<bool> {
    if r.flag()? && r.u(8)? == 255 {
        r.skip(32)?; // Extended_SAR
    }
    if r.flag()? {
        r.skip(1)?; // overscan_appropriate_flag
    }
    if r.flag()? {
        r.skip(4)?; // video_format, video_full_range_flag
        if r.flag()? {
            r.skip(24)?; // colour description
        }
    }
    if r.flag()? {
        r.ue()?;
        r.ue()?; // chroma sample locations
    }
    if r.flag()? {
        r.skip(65)?; // timing info: 32 + 32 + fixed_frame_rate_flag
    }
    let nal = r.flag()?;
    if nal {
        skip_hrd(r)?;
    }
    let vcl = r.flag()?;
    if vcl {
        skip_hrd(r)?;
    }
    if nal || vcl {
        r.skip(1)?; // low_delay_hrd_flag
    }
    r.skip(1)?; // pic_struct_present_flag
    if !r.flag()? {
        return Ok(false); // bitstream_restriction_flag
    }
    r.skip(1)?; // motion_vectors_over_pic_boundaries_flag
    for _ in 0..4 {
        r.ue()?; // byte/bit denominators, log2 maximum motion vector lengths
    }
    Ok(r.ue()? == 0) // max_num_reorder_frames
}

/// Rule 10 (H.264).
fn avc_pps(nal: &[u8]) -> R<PpsInfo> {
    let mut r = Bits::parameter_set(nal, Codec::H264, 10);
    let id = r.ue_max(255)?;
    let sps_id = r.ue_max(31)?;
    r.skip(2)?; // entropy_coding_mode_flag, bottom_field_pic_order_in_frame_present_flag
    if r.ue()? != 0 {
        return Err(r.fail("num_slice_groups_minus1 not 0"));
    }
    r.ue()?;
    r.ue()?; // num_ref_idx_l0/l1_default_active_minus1
    r.skip(3)?; // weighted_pred_flag, weighted_bipred_idc
    r.se()?;
    r.se()?;
    r.se()?; // pic_init_qp/qs_minus26, chroma_qp_index_offset
    r.skip(2)?; // deblocking_filter_control_present_flag, constrained_intra_pred_flag
    if r.flag()? {
        return Err(r.fail("redundant_pic_cnt_present_flag not 0"));
    }
    Ok(PpsInfo { id, sps_id, extra_slice_header_bits: 0 })
}

/// Rule 4 (H.264).
fn avc_slice(nal: &[u8], kind: u8) -> R<SliceInfo> {
    let mut r = Bits::slice(nal, Codec::H264);
    if r.ue()? != 0 {
        return Err(r.fail("first_mb_in_slice not 0"));
    }
    let intra = match r.ue()? {
        0 | 5 => false,
        2 | 7 => true,
        _ => return Err(r.fail("slice_type not P or I")),
    };
    if Codec::H264.is_idr(kind) != intra {
        return Err(r.fail("slice_type does not fit the NAL type"));
    }
    Ok(SliceInfo { pps_id: r.ue_max(255)? })
}

fn slice_info(codec: Codec, nal: &[u8], kind: u8, extra_bits: u32) -> R<SliceInfo> {
    match codec {
        Codec::H264 => avc_slice(nal, kind),
        Codec::Hevc => hevc_slice(nal, kind, extra_bits),
    }
}

/// One validated parameter-set group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    pub codec: Codec,
    /// The sets (VPS for HEVC, SPS, PPS) behind 4-byte start codes.
    pub annex_b: Arc<[u8]>,
    /// The visible size (coded size minus cropping).
    pub width: u32,
    pub height: u32,
    pub pps_id: u32,
    pub extra_slice_header_bits: u32,
    /// Values above the usual range, accepted and logged as `au_unusual`.
    pub unusual: Vec<(&'static str, u32)>,
}

fn with_start_codes(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len() + 4).sum());
    for part in parts {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(part);
    }
    out
}

/// Rules 5 (sizes, chain), 6 to 10 for one VPS?/SPS/PPS group.
fn build(codec: Codec, vps: Option<&[u8]>, sps: &[u8], pps: &[u8], env: Envelope) -> R<ParameterSets> {
    for nal in [vps, Some(sps), Some(pps)].into_iter().flatten() {
        if nal.len() > MAX_PARAMETER_SET_BYTES {
            return Err(violation(5, Some(codec.nal_type(nal[0])), "parameter set above 1024 bytes"));
        }
    }
    let vps_id = match codec {
        Codec::Hevc => Some(hevc_vps(vps.ok_or(violation(5, None, "missing VPS"))?)?),
        Codec::H264 => None,
    };
    let s = match codec {
        Codec::H264 => avc_sps(sps, env)?,
        Codec::Hevc => hevc_sps(sps, env)?,
    };
    let p = match codec {
        Codec::H264 => avc_pps(pps)?,
        Codec::Hevc => hevc_pps(pps)?,
    };
    if s.vps_id != vps_id {
        return Err(violation(5, Some(codec.sps()), "SPS does not name the VPS"));
    }
    if p.sps_id != s.id {
        return Err(violation(5, Some(codec.pps()), "PPS does not name the SPS"));
    }
    let parts: Vec<&[u8]> = [vps, Some(sps), Some(pps)].into_iter().flatten().collect();
    Ok(ParameterSets {
        codec,
        annex_b: with_start_codes(&parts).into(),
        width: s.width,
        height: s.height,
        pps_id: p.id,
        extra_slice_header_bits: p.extra_slice_header_bits,
        unusual: s.unusual,
    })
}

// ------------------------------------------------------------- receiver

/// An access unit that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub key: bool,
    /// SEI, AUD and filler removed; 4-byte start codes (what the decoder gets).
    pub unit: Vec<u8>,
    /// Set only when this IDR changed the active configuration (including the first).
    pub activated: Option<Arc<ParameterSets>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected {
    pub violation: Violation,
    /// The keyframe flag was set or an IDR NAL unit was present.
    pub key: bool,
    pub bytes: usize,
}
impl Rejected {
    /// `BENCHMARK au_reject` fields.
    pub fn line(&self, codec: Codec) -> String {
        format!(
            "au_reject: codec={} | rule={} | type={} | bytes={} | reason={}",
            codec.label(),
            self.violation.rule,
            self.violation.nal_type.map_or("-".to_owned(), |t| t.to_string()),
            self.bytes,
            self.violation.reason
        )
    }
}

/// The receiver's gate for one session (rules 1-11; the flag rules that
/// close a session are the caller's: `FormatTracker`). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Gate {
    codec: Codec,
    envelope: Envelope,
    active: Option<Arc<ParameterSets>>,
    waiting_for_idr: bool,
}
impl Gate {
    pub fn new(codec: Codec, header_width: u32, header_height: u32) -> Self {
        Self { codec, envelope: Envelope::new(header_width, header_height), active: None, waiting_for_idr: false }
    }
    pub fn codec(&self) -> Codec {
        self.codec
    }
    pub fn active(&self) -> Option<&Arc<ParameterSets>> {
        self.active.as_ref()
    }
    pub fn waiting_for_idr(&self) -> bool {
        self.waiting_for_idr
    }
    /// One access unit and its BTV3 keyframe flag. A rejected keyframe keeps
    /// the old active configuration; non-IDR units then reject until the
    /// next valid IDR.
    pub fn check(&mut self, payload: &[u8], key_flag: bool) -> Result<Accepted, Rejected> {
        let mut key = key_flag;
        match self.evaluate(payload, key_flag, &mut key) {
            Ok(accepted) => {
                if accepted.key {
                    self.waiting_for_idr = false;
                }
                Ok(accepted)
            }
            Err(violation) => {
                if key {
                    self.waiting_for_idr = true;
                }
                Err(Rejected { violation, key, bytes: payload.len() })
            }
        }
    }
    fn evaluate(&mut self, payload: &[u8], key_flag: bool, key: &mut bool) -> R<Accepted> {
        let codec = self.codec;
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err(violation(1, None, "payload size outside the limit"));
        }
        let nals = scan(payload, codec, MAX_NALS_PER_ACCESS_UNIT).map_err(|f| {
            *key |= f.saw_idr;
            violation(1, None, f.reason)
        })?;
        let bytes = |n: &Nal| &payload[n.start..n.end];
        *key |= nals.iter().any(|n| codec.is_idr(n.kind));
        for n in &nals {
            if !codec.header_valid(bytes(n)) {
                return Err(violation(2, Some(n.kind), "NAL header not valid"));
            }
        }
        for n in &nals {
            if !codec.allowed(n.kind) {
                return Err(violation(3, Some(n.kind), "NAL type not allowed"));
            }
        }
        let slices: Vec<usize> = (0..nals.len()).filter(|&i| codec.is_slice(nals[i].kind)).collect();
        let &[at] = slices.as_slice() else {
            return Err(violation(4, slices.get(1).map(|&i| nals[i].kind), "not exactly one slice NAL unit"));
        };
        for (i, n) in nals.iter().enumerate() {
            if i < at && codec.is_trailing(n.kind) {
                return Err(violation(4, Some(n.kind), "NAL type before the slice"));
            }
            if i > at && !codec.is_trailing(n.kind) {
                return Err(violation(4, Some(n.kind), "NAL type after the slice"));
            }
        }
        let slice = nals[at];
        let idr = codec.is_idr(slice.kind);
        if key_flag != idr {
            return Err(violation(5, Some(slice.kind), if idr { "IDR without the keyframe flag" } else { "keyframe flag without an IDR" }));
        }
        let find = |t: Option<u8>| -> Vec<usize> { (0..nals.len()).filter(|&i| Some(nals[i].kind) == t).collect() };
        let (vps, sps, pps) = (find(codec.vps()), find(Some(codec.sps())), find(Some(codec.pps())));
        let mut activated = None;
        if idr {
            let want_vps = usize::from(codec.vps().is_some());
            if vps.len() != want_vps || sps.len() != 1 || pps.len() != 1 {
                return Err(violation(5, None, "an IDR needs exactly one VPS (HEVC), SPS and PPS"));
            }
            if vps.first().is_some_and(|&v| v > sps[0]) || sps[0] > pps[0] {
                return Err(violation(5, None, "parameter sets out of order"));
            }
            let parts: Vec<&[u8]> = vps.iter().chain(&sps).chain(&pps).map(|&i| bytes(&nals[i])).collect();
            let serialised = with_start_codes(&parts);
            let sets = match &self.active {
                // Identical bytes were already validated against this session's envelope.
                Some(current) if *current.annex_b == *serialised => current.clone(),
                _ => Arc::new(build(
                    codec,
                    vps.first().map(|&i| bytes(&nals[i])),
                    bytes(&nals[sps[0]]),
                    bytes(&nals[pps[0]]),
                    self.envelope,
                )?),
            };
            let info = slice_info(codec, bytes(&slice), slice.kind, sets.extra_slice_header_bits)?;
            if info.pps_id != sets.pps_id {
                return Err(violation(5, Some(slice.kind), "slice does not name the PPS"));
            }
            if self.active.as_ref().is_none_or(|current| !Arc::ptr_eq(current, &sets)) {
                activated = Some(sets.clone());
            }
            // Atomic: reached only when the whole access unit passed.
            self.active = Some(sets);
        } else {
            if let Some(&i) = vps.iter().chain(&sps).chain(&pps).next() {
                return Err(violation(5, Some(nals[i].kind), "non-IDR access unit carries a parameter set"));
            }
            let Some(current) = &self.active else {
                return Err(violation(5, Some(slice.kind), "no active configuration before the first IDR"));
            };
            if self.waiting_for_idr {
                return Err(violation(5, Some(slice.kind), "dropped until the next valid IDR"));
            }
            let info = slice_info(codec, bytes(&slice), slice.kind, current.extra_slice_header_bits)?;
            if info.pps_id != current.pps_id {
                return Err(violation(5, Some(slice.kind), "slice does not name the active PPS"));
            }
        }
        // Rule 11: SEI, AUD and filler are removed.
        let kept: Vec<&[u8]> = nals.iter().filter(|n| !codec.is_stripped(n.kind)).map(bytes).collect();
        Ok(Accepted { key: idr, unit: with_start_codes(&kept), activated })
    }
}

/// The rule "three rejected keyframes in a row within 10 s close the session".
#[derive(Debug, Clone, Default)]
pub struct KeyframeRejections {
    times: [i64; 3],
    run: usize,
}
impl KeyframeRejections {
    /// A rejected keyframe at `now_ns`; true when the session must close.
    pub fn rejected(&mut self, now_ns: i64) -> bool {
        if self.run < 3 {
            self.times[self.run] = now_ns;
            self.run += 1;
        } else {
            self.times.rotate_left(1);
            self.times[2] = now_ns;
        }
        self.run >= 3 && self.times[2] - self.times[0] <= REJECTED_KEYFRAME_WINDOW_NS
    }
    pub fn accepted_keyframe(&mut self) {
        self.run = 0;
    }
}

// --------------------------------------------------------------- sender

const MAX_CONFIG_NALS: usize = 16;
const MAX_INPUT_NALS: usize = 32;

/// One BTV3 video payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packetised {
    pub data: Vec<u8>,
    pub key: bool,
    /// The visible size from the SPS, keyframes only.
    pub size: Option<(u32, u32)>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketiseError {
    Malformed,
    /// A slice type the receiver does not accept (HEVC CRA, data partitions), or another NAL type.
    UnsupportedNal,
    /// No picture (sets only): the sets are kept.
    NoSlice,
    MultiSlice,
    /// Presentation timestamps did not strictly increase.
    Reorder,
    /// An IDR before a complete, parsable set.
    NoParameterSets,
}
impl PacketiseError {
    pub fn label(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnsupportedNal => "unsupported_nal",
            Self::NoSlice => "no_slice",
            Self::MultiSlice => "multi_slice",
            Self::Reorder => "reorder",
            Self::NoParameterSets => "no_parameter_sets",
        }
    }
}

/// The sender side (PROTOCOL "Sender rules"): the encoder's in-band and
/// configuration parameter sets are cached (newest wins), stripped from every
/// access unit and prepended to every IDR. Not thread-safe.
pub struct Packetiser {
    codec: Codec,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    parsed: Option<ParameterSets>,
    dirty: bool,
    media_since_config: bool,
    last_pts_us: Option<i64>,
    /// Why the cached sets do not parse (for logs).
    pub config_problem: Option<&'static str>,
}
impl Packetiser {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            vps: None,
            sps: None,
            pps: None,
            parsed: None,
            dirty: false,
            media_since_config: false,
            last_pts_us: None,
            config_problem: None,
        }
    }
    /// A configuration record (for example the output type's sequence
    /// header). Consecutive records merge; one after media starts a new cache.
    pub fn on_codec_config(&mut self, bytes: &[u8]) {
        if std::mem::take(&mut self.media_since_config) {
            (self.vps, self.sps, self.pps) = (None, None, None);
            self.dirty = true;
        }
        match scan(bytes, self.codec, MAX_CONFIG_NALS) {
            Ok(nals) => nals.iter().for_each(|n| self.absorb(n.kind, &bytes[n.start..n.end])),
            Err(_) => self.config_problem = Some("malformed codec configuration"),
        }
    }
    fn absorb(&mut self, kind: u8, nal: &[u8]) {
        let slot = if Some(kind) == self.codec.vps() {
            &mut self.vps
        } else if kind == self.codec.sps() {
            &mut self.sps
        } else if kind == self.codec.pps() {
            &mut self.pps
        } else {
            return;
        };
        if slot.as_deref() != Some(nal) {
            *slot = Some(nal.to_vec());
            self.dirty = true;
        }
    }
    /// The complete, parsed cached set.
    pub fn cached(&mut self) -> Option<&ParameterSets> {
        if std::mem::take(&mut self.dirty) {
            self.parsed = None;
            self.config_problem = None;
            if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
                if self.codec.vps().is_none() || self.vps.is_some() {
                    match build(self.codec, self.vps.as_deref(), sps, pps, Envelope::UNBOUNDED) {
                        Ok(sets) => self.parsed = Some(sets),
                        Err(v) => self.config_problem = Some(v.reason),
                    }
                }
            }
        }
        self.parsed.as_ref()
    }
    /// One encoder output unit.
    pub fn on_access_unit(&mut self, bytes: &[u8], pts_us: i64) -> Result<Packetised, PacketiseError> {
        let codec = self.codec;
        let nals = scan(bytes, codec, MAX_INPUT_NALS).map_err(|_| PacketiseError::Malformed)?;
        if nals.iter().any(|n| codec.is_vcl(n.kind) && !codec.is_slice(n.kind)) {
            return Err(PacketiseError::UnsupportedNal);
        }
        let slices: Vec<&Nal> = nals.iter().filter(|n| codec.is_slice(n.kind)).collect();
        if slices.is_empty() {
            nals.iter().for_each(|n| self.absorb(n.kind, &bytes[n.start..n.end]));
            self.media_since_config = true;
            return Err(PacketiseError::NoSlice);
        }
        if slices.len() > 1 {
            return Err(PacketiseError::MultiSlice);
        }
        if nals.iter().any(|n| !codec.is_slice(n.kind) && !codec.is_parameter_set(n.kind) && !codec.is_stripped(n.kind)) {
            return Err(PacketiseError::UnsupportedNal);
        }
        let slice = &bytes[slices[0].start..slices[0].end];
        let key = codec.is_idr(slices[0].kind);
        // In-band sets update the cache whether or not this unit is sent.
        nals.iter().for_each(|n| self.absorb(n.kind, &bytes[n.start..n.end]));
        self.media_since_config = true;
        if self.last_pts_us.is_some_and(|last| pts_us <= last) {
            return Err(PacketiseError::Reorder);
        }
        let (data, size) = if key {
            let sets = self.cached().ok_or(PacketiseError::NoParameterSets)?;
            let mut data = sets.annex_b.to_vec();
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(slice);
            (data, Some((sets.width, sets.height)))
        } else {
            (with_start_codes(&[slice]), None)
        };
        self.last_pts_us = Some(pts_us);
        Ok(Packetised { data, key, size })
    }
}

// --------------------------------------------------------------- levels

/// One standard level and the limits a session must fit (ITU-T H.264
/// Table A-1 in macroblocks; H.265 Tables A.8/A.9 in luma samples, Main tier).
struct LevelLimit {
    idc: u32,
    max_samples: u64,
    max_rate: u64,
    max_kbps: u64,
}
const fn level(idc: u32, max_samples: u64, max_rate: u64, max_kbps: u64) -> LevelLimit {
    LevelLimit { idc, max_samples, max_rate, max_kbps }
}
const HEVC_LEVELS: [LevelLimit; 13] = [
    level(30, 36_864, 552_960, 128),
    level(60, 122_880, 3_686_400, 1_500),
    level(63, 245_760, 7_372_800, 3_000),
    level(90, 552_960, 16_588_800, 6_000),
    level(93, 983_040, 33_177_600, 10_000),
    level(120, 2_228_224, 66_846_720, 12_000),
    level(123, 2_228_224, 133_693_440, 20_000),
    level(150, 8_912_896, 267_386_880, 25_000),
    level(153, 8_912_896, 534_773_760, 40_000),
    level(156, 8_912_896, 1_069_547_520, 60_000),
    level(180, 35_651_584, 1_069_547_520, 60_000),
    level(183, 35_651_584, 2_139_095_040, 120_000),
    level(186, 35_651_584, 4_278_190_080, 240_000),
];
const AVC_LEVELS: [LevelLimit; 16] = [
    level(10, 99, 1_485, 64),
    level(11, 396, 3_000, 192),
    level(12, 396, 6_000, 384),
    level(13, 396, 11_880, 768),
    level(20, 396, 11_880, 2_000),
    level(21, 792, 19_800, 4_000),
    level(22, 1_620, 20_250, 4_000),
    level(30, 1_620, 40_500, 10_000),
    level(31, 3_600, 108_000, 14_000),
    level(32, 5_120, 216_000, 20_000),
    level(40, 8_192, 245_760, 20_000),
    level(41, 8_192, 245_760, 50_000),
    level(42, 8_704, 522_240, 50_000),
    level(50, 22_080, 589_824, 135_000),
    level(51, 36_864, 983_040, 240_000),
    level(52, 36_864, 2_073_600, 240_000),
];

/// The lowest standard level (HEVC Main tier) whose limits cover a
/// `width`x`height` session at `fps` and `bitrate_bps`, as its level_idc
/// (which is also the Media Foundation `eAVEncH264VLevel` /
/// `eAVEncH265VLevel` value). The highest level when none does.
pub fn session_level(codec: Codec, width: u32, height: u32, fps: u32, bitrate_bps: u32) -> u32 {
    let (table, samples): (&[LevelLimit], u64) = match codec {
        Codec::Hevc => (&HEVC_LEVELS, u64::from(width) * u64::from(height)),
        Codec::H264 => (&AVC_LEVELS, u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16))),
    };
    table
        .iter()
        .find(|l| samples <= l.max_samples && samples * u64::from(fps) <= l.max_rate && u64::from(bitrate_bps) / 1000 <= l.max_kbps)
        .unwrap_or(&table[table.len() - 1])
        .idc
}

#[cfg(test)]
pub(crate) mod tests;

