//! Design HC (DECISIONS "Design HC", PROTOCOL "Draft: HEVC and H.264 video
//! codecs"): the codec numbers, the one explicit codec <-> hello bit <->
//! summary bit table, the switches, and the one choice operation per
//! direction. With both masters off nothing here changes a byte: the call
//! choice is exactly `encoder::plan` and no new bit is set anywhere.
use super::encoder::{Availability, Backend};
use std::sync::OnceLock;

/// BTV3 video codec IDs.
pub const VP9: u8 = 2;
pub const AV1: u8 = 3;
pub const H264: u8 = 4;
pub const HEVC: u8 = 5;

/// Encode/decode-mask codec bits of link and end-to-end hellos.
pub const HELLO_VP9: u8 = 0x01;
pub const HELLO_AV1: u8 = 0x02;
pub const HELLO_H264: u8 = 0x04;
pub const HELLO_HEVC: u8 = 0x08;
pub const HELLO_CODECS: u8 = 0x0f;
/// End-to-end decode-mask bit 4, never a codec and never in a link hello:
/// "my AV1 decoding is software".
pub const DECODE_AV1_SOFTWARE: u8 = 0x10;
/// End-to-end decode-mask bit 7 (CF-MEDIA-50): the peer prefers VP9.
pub const DECODE_PREFERS_VP9: u8 = 0x80;

/// capabilitySummary codec bits (call offers and video stream offers).
/// Summary bit 3 is the dual-listening hint and bit 4 relay consent, so a
/// hello mask is never copied into a summary.
pub const SUMMARY_VP9: u8 = 0x01;
pub const SUMMARY_AV1: u8 = 0x02;
pub const SUMMARY_H264: u8 = 0x04;
pub const SUMMARY_HEVC: u8 = 0x80;
pub const SUMMARY_CODECS: u8 = 0x87;

/// (codec ID, hello bit, summary bit, label).
const TABLE: [(u8, u8, u8, &str); 4] = [
    (VP9, HELLO_VP9, SUMMARY_VP9, "VP9"),
    (AV1, HELLO_AV1, SUMMARY_AV1, "AV1"),
    (H264, HELLO_H264, SUMMARY_H264, "H264"),
    (HEVC, HELLO_HEVC, SUMMARY_HEVC, "HEVC"),
];
/// The hello bit of a BTV3 codec (0 for anything else).
pub fn hello_bit(codec: u8) -> u8 {
    TABLE.iter().find(|t| t.0 == codec).map_or(0, |t| t.1)
}
pub fn summary_bit(codec: u8) -> u8 {
    TABLE.iter().find(|t| t.0 == codec).map_or(0, |t| t.2)
}
pub fn label(codec: u8) -> &'static str {
    TABLE.iter().find(|t| t.0 == codec).map_or("unknown", |t| t.3)
}
/// A summary's codec bits as hello bits (other summary bits are dropped).
pub fn summary_to_hello(summary: u8) -> u8 {
    TABLE.iter().filter(|t| summary & t.2 != 0).fold(0, |m, t| m | t.1)
}
/// A hello mask's codec bits as summary bits (bits 4 and 7 are dropped).
pub fn hello_to_summary(mask: u8) -> u8 {
    TABLE.iter().filter(|t| mask & t.1 != 0).fold(0, |m, t| m | t.2)
}

/// DECISIONS "Design HC", "Decode coverage without an add-on", kept as
/// data: the codecs every MeshLive platform decodes without an add-on. It
/// drives the stream order (Design HC) and changes only by a reviewed update.
pub const UNIVERSAL: [u8; 2] = [VP9, H264];
/// Compression class, lower is better: AV1; HEVC and VP9; H.264.
pub fn efficiency_class(codec: u8) -> u8 {
    match codec {
        AV1 => 0,
        HEVC | VP9 => 1,
        _ => 2,
    }
}

// ------------------------------------------------------------ switches

pub const SWITCH_HEVC: &str = "MESHLIVE_HEVC";
pub const SWITCH_H264: &str = "MESHLIVE_H264";
pub const SWITCH_PREFER_HEVC: &str = "MESHLIVE_PREFER_HEVC";
pub const SWITCH_PREFER_HW_AVC: &str = "MESHLIVE_PREFER_HW_AVC";
pub const SWITCH_AV1_SOFT_DECODE_BIT: &str = "MESHLIVE_AV1_SOFT_DECODE_BIT";
pub const SWITCH_BIT7_HW_HEVC: &str = "MESHLIVE_BIT7_HW_HEVC";
pub const SWITCH_HC_SESSION_LEVEL: &str = "MESHLIVE_HC_SESSION_LEVEL";

/// The masters (on by default since 2026-10-28; `MESHLIVE_HEVC=0` /
/// `MESHLIVE_H264=0` turn them off) and their control arms (on by default;
/// `=0` selects the control). On is safe: this PC sends HEVC/H.264 only to
/// a peer whose hello advertises decoding it, and advertises its own decode
/// bit only after the start check decodes an embedded phone keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    pub hevc: bool,
    pub h264: bool,
    /// Off: HEVC stays advertised and decodable, but Automatic never picks it.
    pub prefer_hevc: bool,
    pub prefer_hw_avc: bool,
    /// Off: end-to-end bit 4 is not sent.
    pub av1_soft_decode_bit: bool,
    /// Off: peer bit 7 puts VP9 first even over hardware HEVC (first draft).
    pub bit7_hw_hevc: bool,
    /// Off: the encoder keeps its default level instead of the session's.
    pub session_level: bool,
}
impl Switches {
    pub const LEGACY: Self = Self {
        hevc: false,
        h264: false,
        prefer_hevc: true,
        prefer_hw_avc: true,
        av1_soft_decode_bit: true,
        bit7_hw_hevc: true,
        session_level: true,
    };
    pub fn from(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |name| get(name).as_deref() == Some("1");
        let not_off = |name| get(name).as_deref() != Some("0");
        Self {
            hevc: not_off(SWITCH_HEVC),
            h264: not_off(SWITCH_H264),
            prefer_hevc: not_off(SWITCH_PREFER_HEVC),
            prefer_hw_avc: not_off(SWITCH_PREFER_HW_AVC),
            av1_soft_decode_bit: not_off(SWITCH_AV1_SOFT_DECODE_BIT),
            bit7_hw_hevc: not_off(SWITCH_BIT7_HW_HEVC),
            session_level: not_off(SWITCH_HC_SESSION_LEVEL),
        }
    }
    /// This process's switches (environment variables read once).
    pub fn current() -> Self {
        static CURRENT: OnceLock<Switches> = OnceLock::new();
        *CURRENT.get_or_init(|| Self::from(|name| std::env::var(name).ok()))
    }
    /// Either master on: Design HC rules; both off: today's rules.
    pub fn any(self) -> bool {
        self.hevc || self.h264
    }
    /// The master of `codec` (VP9 and AV1 have none).
    pub fn codec_on(self, codec: u8) -> bool {
        match codec {
            HEVC => self.hevc,
            H264 => self.h264,
            VP9 | AV1 => true,
            _ => false,
        }
    }
    fn preferred(self, codec: u8) -> bool {
        match codec {
            HEVC => self.hevc && self.prefer_hevc,
            H264 => self.h264 && self.prefer_hw_avc,
            _ => true,
        }
    }
    pub fn line(self) -> String {
        format!(
            "hevc={} h264={} prefer_hevc={} prefer_hw_avc={} av1_soft_decode_bit={} bit7_hw_hevc={} session_level={}",
            self.hevc, self.h264, self.prefer_hevc, self.prefer_hw_avc, self.av1_soft_decode_bit, self.bit7_hw_hevc, self.session_level
        )
    }
}

// -------------------------------------------------------------- choice

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Call,
    Stream,
}

/// One choice: the ordered, filtered list fallback walks, and why its
/// first entry was chosen (`BENCHMARK encoder_choice` reasons).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub list: Vec<(u8, Backend)>,
    pub reason: &'static str,
    pub notes: Vec<&'static str>,
}
impl Choice {
    pub fn line(&self, peer_decode_mask: Option<u8>, switches: Switches) -> String {
        let list: Vec<String> = self.list.iter().map(|(c, b)| format!("{}:{}", label(*c), backend_label(*b))).collect();
        let mut line = format!(
            "encoder_choice: codec={} | reason={} | peer_decode_mask={} | list={}",
            self.list.first().map_or("none", |(c, _)| label(*c)),
            self.reason,
            peer_decode_mask.map_or("-".to_owned(), |m| m.to_string()),
            if list.is_empty() { "none".to_owned() } else { list.join(",") }
        );
        if switches.any() {
            line += &format!(" | {}", switches.line());
        }
        if !self.notes.is_empty() {
            line += &format!(" | notes={}", self.notes.join(","));
        }
        line
    }
}
pub fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Hardware => "hardware",
        Backend::SoftwareAv1 => "libaom",
        Backend::SoftwareVp9 => "libvpx",
    }
}

fn hardware(available: &Availability, codec: u8) -> bool {
    match codec {
        AV1 => !available.av1_hardware.is_empty(),
        VP9 => !available.vp9_hardware.is_empty(),
        HEVC => !available.hevc_hardware.is_empty(),
        H264 => !available.h264_hardware.is_empty(),
        _ => false,
    }
}
fn call_rank(codec: u8) -> u8 {
    match codec {
        AV1 => 0,
        HEVC => 1,
        VP9 => 2,
        _ => 3,
    }
}
/// The hardware codec order of `mode`: calls AV1, HEVC, VP9, H.264; streams
/// (Design HC) by compression class with the universally decodable codec first
/// inside a class: AV1, VP9, HEVC, H.264.
pub fn hardware_order(mode: Mode) -> [u8; 4] {
    let mut codecs = [AV1, HEVC, VP9, H264];
    match mode {
        Mode::Call => codecs.sort_by_key(|&c| call_rank(c)),
        Mode::Stream => codecs.sort_by_key(|&c| (efficiency_class(c), u8::from(c != AV1 && !UNIVERSAL.contains(&c)), call_rank(c))),
    }
    codecs
}

/// The Automatic list of `mode` before any peer filter: the hardware
/// encoders in the mode's order (an HEVC or H.264 encoder only with its
/// master and prefer arm on), then today's software order (libaom, libvpx).
pub fn automatic(available: &Availability, mode: Mode, switches: Switches) -> Vec<(u8, Backend)> {
    let mut out: Vec<(u8, Backend)> = hardware_order(mode)
        .into_iter()
        .filter(|&c| hardware(available, c) && switches.preferred(c))
        .map(|c| (c, Backend::Hardware))
        .collect();
    out.extend([(AV1, Backend::SoftwareAv1), (VP9, Backend::SoftwareVp9)]);
    out
}

/// Peer bit 4: AV1 entries move below this PC's hardware HEVC and hardware
/// VP9 entries (those the peer decodes), and only below those.
fn demote_av1(list: Vec<(u8, Backend)>) -> Vec<(u8, Backend)> {
    let preferred = |e: &(u8, Backend)| e.1 == Backend::Hardware && (e.0 == HEVC || e.0 == VP9);
    let Some(last) = list.iter().rposition(preferred) else { return list };
    let (head, tail) = list.split_at(last + 1);
    let mut out: Vec<_> = head.iter().copied().filter(|e| e.0 != AV1).collect();
    out.extend(head.iter().copied().filter(|e| e.0 == AV1));
    out.extend_from_slice(tail);
    out
}

/// The codec this PC sends to a call peer whose authenticated end-to-end
/// decode mask is `peer`. Legacy (both masters off): exactly `encoder::plan`.
/// With a master on: hardware AV1, HEVC, VP9, H.264, then libaom and libvpx,
/// only codecs the peer decodes; peer bit 7 (counted only with its VP9 bit)
/// puts VP9 first, below only this PC's hardware HEVC when the peer decodes
/// it; then peer bit 4 moves AV1 below hardware HEVC and VP9. Windows has no
/// family rules and no manual call codec.
pub fn for_call(peer: u8, available: &Availability, switches: Switches) -> Choice {
    let decodes = |codec: u8| peer & hello_bit(codec) != 0;
    let bit7 = peer & DECODE_PREFERS_VP9 != 0;
    let bit7_counts = bit7 && peer & HELLO_VP9 != 0;
    if !switches.any() {
        let list = super::encoder::plan(peer, available);
        let reason = match list.first() {
            None => "no_common_codec",
            Some((VP9, _)) if bit7_counts && decodes(AV1) && hardware(available, AV1) => "peer_prefers_vp9",
            Some((AV1, Backend::Hardware)) => "hw_av1_preferred",
            Some(_) if hardware(available, AV1) && !decodes(AV1) => "peer_no_av1_decode",
            Some(_) => "no_hw_av1",
        };
        return Choice { list, reason, notes: Vec::new() };
    }
    let mut notes = Vec::new();
    if bit7 && !bit7_counts {
        notes.push("bit7_without_vp9_ignored");
    }
    let auto = automatic(available, Mode::Call, switches);
    let mut list: Vec<_> = auto.iter().copied().filter(|e| decodes(e.0)).collect();
    if peer & DECODE_AV1_SOFTWARE != 0 {
        list = demote_av1(list);
    }
    let mut forced = None;
    let vp9 = auto.iter().copied().find(|e| e.0 == VP9);
    if let (true, Some(vp9)) = (bit7_counts, vp9) {
        let hw_hevc: Vec<_> = if switches.bit7_hw_hevc { list.iter().copied().filter(|e| e.0 == HEVC).collect() } else { Vec::new() };
        if !hw_hevc.is_empty() {
            notes.push("bit7_hw_hevc_first");
        }
        let rest: Vec<_> = list.iter().copied().filter(|e| *e != vp9 && !hw_hevc.contains(e)).collect();
        list = hw_hevc.into_iter().chain([vp9]).chain(rest).collect();
        forced = Some(vp9);
    }
    let reason = match list.first() {
        None => "no_common_codec",
        Some(sent) if Some(*sent) == forced => "peer_prefers_vp9",
        Some(&sent) => automatic_reason(sent, available, Mode::Call, switches, Some(peer)),
    };
    Choice { list, reason, notes }
}

/// The stream source's Automatic order (Design HC); a stream checks no viewer.
pub fn for_stream(available: &Availability, switches: Switches) -> Choice {
    let list = automatic(available, Mode::Stream, switches);
    let reason = list.first().map_or("no_common_codec", |&sent| automatic_reason(sent, available, Mode::Stream, switches, None));
    Choice { list, reason, notes: Vec::new() }
}

/// Why Automatic sent `sent`: the first better-ranked hardware codec that was
/// skipped names the reason; with none, `hw_av1_preferred`,
/// `hw_hevc_preferred`, `hw_avc_preferred`, or `no_hw_av1`.
fn automatic_reason(sent: (u8, Backend), available: &Availability, mode: Mode, switches: Switches, peer: Option<u8>) -> &'static str {
    let sent_hardware = sent.1 == Backend::Hardware;
    for codec in hardware_order(mode).into_iter().filter(|&c| switches.codec_on(c)) {
        if sent_hardware && codec == sent.0 {
            break;
        }
        if !hardware(available, codec) {
            continue;
        }
        if !switches.preferred(codec) {
            return if codec == HEVC { "hevc_not_preferred" } else { "avc_not_preferred" };
        }
        if let Some(peer) = peer {
            if peer & hello_bit(codec) == 0 {
                return match codec {
                    AV1 => "peer_no_av1_decode",
                    HEVC => "peer_no_hevc_decode",
                    H264 => "peer_no_avc_decode",
                    _ => "peer_no_vp9_decode",
                };
            }
            if codec == AV1 && peer & DECODE_AV1_SOFTWARE != 0 {
                return "peer_av1_software";
            }
        }
    }
    match sent {
        (AV1, Backend::Hardware) => "hw_av1_preferred",
        (HEVC, Backend::Hardware) => "hw_hevc_preferred",
        (H264, Backend::Hardware) => "hw_avc_preferred",
        _ => "no_hw_av1",
    }
}

#[cfg(test)]
mod tests;
