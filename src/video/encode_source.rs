//! Encode once per source. One `SourceEncoder` owns the source's single
//! encoder; each picture is encoded once and its BTV3 packet is published to
//! the fan-out, which hands the same shared bytes to every child queue (no
//! per-viewer encode). Keyframe requests from any number of viewers are
//! coalesced by `KeyframePolicy` into one forced keyframe. An encoder restart
//! at another coded size marks its first keyframe with the inner
//! DISCONTINUITY flag (PROTOCOL "Mid-stream size changes"); a restart that
//! keeps the size is not marked. Bitstreams are sent unchanged: AV1 temporal
//! units carry the sequence header in-band on every keyframe, VP9 frames are
//! whole frames (no hidden alt-ref with lag 0), like Android's encoders.
//!
//! Design HC (H.264, HEVC): every unit goes through `annexb::Packetiser`
//! (sets prepended to every IDR, stripped elsewhere; SEI, AUD and filler
//! removed; the keyframe flag from the NAL type; no configuration packet).
//! A forced keyframe must come back as an IDR within 1 s of media time; it
//! is requested again, and after 3 misses video ends with `no_idr`. A
//! `no_idr`, `reorder` or unsupported bitstream marks the component
//! ineligible for this process.
use super::{
    annexb::{Codec, PacketiseError, Packetiser},
    encode_bitstream::{normalise_av1, vp9_key},
    encode_policy::KeyframePolicy,
    encoder::{EncodeError, EncodedUnit, VideoEncoder, mark_ineligible},
};
use crate::{
    live,
    media::{self, DISCONTINUITY, KEY_FRAME, Packet},
};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    Encode(EncodeError),
    Media(media::Error),
    Live(live::Error),
    /// Design HC: video ends (`no_idr`, `reorder`, `multi_slice`, ...).
    Bitstream(&'static str),
}
impl From<EncodeError> for SourceError {
    fn from(e: EncodeError) -> Self {
        Self::Encode(e)
    }
}
impl From<media::Error> for SourceError {
    fn from(e: media::Error) -> Self {
        Self::Media(e)
    }
}
impl From<live::Error> for SourceError {
    fn from(e: live::Error) -> Self {
        Self::Live(e)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeStats {
    pub frames_in: u64,
    pub units_out: u64,
    pub keyframes: u64,
    pub bytes_out: u64,
}

pub struct SourceEncoder<'a> {
    encoder: VideoEncoder<'a>,
    policy: KeyframePolicy,
    discontinuity: bool,
    stats: EncodeStats,
    /// The current encoder's newest AV1 sequence header OBU.
    sequence: Option<Vec<u8>>,
    /// Design HC: the current H.264/HEVC encoder's packetiser.
    hc: Option<Packetiser>,
    /// Design HC: a forced keyframe not yet answered by an IDR (its media
    /// time and how many times it was repeated).
    idr_wait: Option<(i64, u32)>,
}
/// Design HC: a keyframe request repeats after this much media time without an IDR.
pub const IDR_DEADLINE_US: i64 = 1_000_000;
/// Design HC: after this many repeats video ends with `no_idr`.
pub const IDR_MISSES: u32 = 3;
fn packetiser(encoder: &VideoEncoder<'_>) -> Option<Packetiser> {
    let mut packetiser = Packetiser::new(Codec::of(encoder.settings().codec)?);
    if let VideoEncoder::Hardware(hardware) = encoder {
        if let Some(header) = hardware.sequence_header() {
            packetiser.on_codec_config(&header);
        }
    }
    Some(packetiser)
}
impl<'a> SourceEncoder<'a> {
    /// `keyframe_interval_s`: the safety keyframe (Android 60; 0 = requests only).
    pub fn new(encoder: VideoEncoder<'a>, keyframe_interval_s: u32) -> Self {
        Self {
            policy: KeyframePolicy::new(keyframe_interval_s),
            discontinuity: false,
            stats: EncodeStats::default(),
            sequence: None,
            hc: packetiser(&encoder),
            idr_wait: None,
            encoder,
        }
    }
    pub fn encoder(&self) -> &VideoEncoder<'a> {
        &self.encoder
    }
    pub fn encoder_mut(&mut self) -> &mut VideoEncoder<'a> {
        &mut self.encoder
    }
    pub fn policy(&self) -> &KeyframePolicy {
        &self.policy
    }
    pub fn stats(&self) -> EncodeStats {
        self.stats
    }
    /// A viewer asked for a keyframe (`force`: a joining viewer, never delayed).
    pub fn request_keyframe(&mut self, now_ns: u64, force: bool) {
        self.policy.request(now_ns, force);
    }
    /// Replaces the encoder (ladder rung, codec or backend change). The next
    /// picture is a keyframe; it carries DISCONTINUITY when the size changed.
    /// Returns the old encoder's drained packets (send them before the new ones).
    pub fn restart(&mut self, mut encoder: VideoEncoder<'a>, deadline: Instant) -> Result<Vec<Vec<u8>>, SourceError> {
        std::mem::swap(&mut self.encoder, &mut encoder);
        let (old, new) = (encoder.settings(), self.encoder.settings());
        let drained = encoder.finish(deadline).unwrap_or_default();
        let packets = self.packets_for(old.codec, &encoder.name(), drained)?;
        self.discontinuity |= (old.width, old.height) != (new.width, new.height);
        self.sequence = None;
        self.hc = packetiser(&self.encoder);
        self.idr_wait = None;
        self.policy.restart();
        Ok(packets)
    }
    /// Encodes one NV12 picture; returns the BTV3 media packets ready to publish.
    pub fn encode(&mut self, nv12: &[u8], pts_us: i64, deadline: Instant) -> Result<Vec<Vec<u8>>, SourceError> {
        let force = self.force(pts_us)?;
        self.stats.frames_in += 1;
        let units = self.encoder.encode(nv12, pts_us, force, deadline)?;
        self.packets(units)
    }
    /// Encodes one NV12 texture of `gpu` (no CPU copy with a texture-input
    /// hardware encoder); returns the BTV3 media packets ready to publish.
    pub fn encode_texture(
        &mut self,
        gpu: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        pts_us: i64,
        deadline: Instant,
    ) -> Result<Vec<Vec<u8>>, SourceError> {
        let force = self.force(pts_us)?;
        self.stats.frames_in += 1;
        let units = self.encoder.encode_texture(gpu, texture, 0, pts_us, force, deadline)?;
        self.packets(units)
    }
    /// End of input: the encoder's remaining packets.
    pub fn finish(&mut self, deadline: Instant) -> Result<Vec<Vec<u8>>, SourceError> {
        let units = self.encoder.finish(deadline)?;
        self.packets(units)
    }
    /// Whether this picture is forced: the policy's decision, and for
    /// Design HC a keyframe request repeated while no IDR came back.
    fn force(&mut self, pts_us: i64) -> Result<bool, SourceError> {
        let mut force = self.policy.force_for(pts_us);
        if self.hc.is_none() {
            return Ok(force);
        }
        match self.idr_wait {
            None if force => self.idr_wait = Some((pts_us, 0)),
            Some((since, misses)) if pts_us - since >= IDR_DEADLINE_US => {
                if misses + 1 >= IDR_MISSES {
                    mark_ineligible(&self.encoder.name());
                    return Err(SourceError::Bitstream("no_idr"));
                }
                self.idr_wait = Some((pts_us, misses + 1));
                force = true;
            }
            _ => {}
        }
        Ok(force)
    }
    fn packets(&mut self, units: Vec<EncodedUnit>) -> Result<Vec<Vec<u8>>, SourceError> {
        let codec = self.encoder.settings().codec;
        let name = if self.hc.is_some() { self.encoder.name() } else { String::new() };
        self.packets_for(codec, &name, units)
    }
    fn packets_for(&mut self, codec: u8, name: &str, units: Vec<EncodedUnit>) -> Result<Vec<Vec<u8>>, SourceError> {
        let mut out = Vec::with_capacity(units.len());
        for mut unit in units {
            // Bitstream truth for the key flag; AV1 keys get their sequence header in-band.
            let key = match codec {
                3 => normalise_av1(&mut unit.data, &mut self.sequence),
                2 => vp9_key(&unit.data),
                4 | 5 => match self.hc.as_mut().map(|p| p.on_access_unit(&unit.data, unit.pts_us)) {
                    Some(Ok(packet)) => {
                        unit.data = packet.data;
                        if packet.key {
                            self.idr_wait = None;
                        }
                        Some(packet.key)
                    }
                    // A unit with parameter sets only: they are cached for the next IDR.
                    Some(Err(PacketiseError::NoSlice)) => continue,
                    Some(Err(error)) => {
                        mark_ineligible(name);
                        return Err(SourceError::Bitstream(error.label()));
                    }
                    None => None,
                },
                _ => None,
            };
            unit.key_frame = key.unwrap_or(unit.key_frame);
            self.policy.produced(unit.pts_us, unit.key_frame);
            let mut flags = 0;
            if unit.key_frame {
                flags |= KEY_FRAME;
                self.stats.keyframes += 1;
                if std::mem::take(&mut self.discontinuity) {
                    flags |= DISCONTINUITY;
                }
            }
            self.stats.units_out += 1;
            self.stats.bytes_out += unit.data.len() as u64;
            out.push(
                Packet {
                    flags,
                    pts_us: unit.pts_us,
                    payload: &unit.data,
                }
                .encode()?,
            );
        }
        Ok(out)
    }

    /// One picture into a going-live source: the source's coalesced keyframe
    /// request (joins and viewer requests) is applied, the picture is encoded
    /// once and every packet is published to all children. A join (or resume)
    /// is forced; a viewer's recovery request uses the policy's backoff.
    pub fn encode_into<K: PartialEq + Clone>(
        &mut self,
        source: &mut live::Source<K>,
        nv12: &[u8],
        pts_us: i64,
        now_ns: u64,
        deadline: Instant,
    ) -> Result<usize, SourceError> {
        let requests = source.take_keyframe_requests();
        if requests.join {
            self.request_keyframe(now_ns, true);
        } else if requests.recovery {
            self.request_keyframe(now_ns, false);
        }
        let packets = self.encode(nv12, pts_us, deadline)?;
        let count = packets.len();
        for packet in packets {
            source.publish(packet, now_ns)?;
        }
        Ok(count)
    }
}
