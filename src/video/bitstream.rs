//! Picture-allocation preflight, not a complete codec or source validator.
//! VP9 follows Android Vp9FrameHeader/CodedVideoSize and VP9 sections 6.2/B.
//! AV1 follows Android BitstreamDroppability and AV1 sections 5.5/5.9. Only
//! sequence-sized AV1 frames are supported here; overrides/tile lists refuse.
//! No compressed-payload copies, native calls, or heap allocations.
use crate::media::{MAX_PAYLOAD, VideoHeader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitstreamError {
    Malformed,
    Size,
    NeedKeyFrame,
    Unsupported,
}
type Result<T> = std::result::Result<T, BitstreamError>;
use BitstreamError::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dimensions {
    width: u32,
    height: u32,
}
impl Dimensions {
    fn check(self, maximum: Self) -> Result<Self> {
        if self.width == 0
            || self.height == 0
            || self.width > maximum.width
            || self.height > maximum.height
        {
            return Err(Size);
        }
        Ok(self)
    }
}
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    fn read(&mut self, count: u32) -> Result<u32> {
        if count > 32 || self.position + count as usize > self.data.len() * 8 {
            return Err(Malformed);
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | ((self.data[self.position / 8] >> (7 - self.position % 8)) & 1) as u32;
            self.position += 1;
        }
        Ok(value)
    }
    fn bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }
    fn uvlc(&mut self) -> Result<()> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros >= 32 {
                return Err(Malformed);
            }
        }
        self.read(zeros)?;
        Ok(())
    }
}

/// Decoder-owned, speculative state. Commit the returned copy only when the
/// native transform accepts the access unit. Flush discards all references.
#[derive(Clone, Copy)]
pub(super) struct SizeGuard {
    maximum: Dimensions,
    codec: u8,
    vp9: [Option<Dimensions>; 8],
    av1: Option<Sequence>,
    started: bool,
}
impl SizeGuard {
    pub(super) fn new(header: VideoHeader) -> Self {
        Self {
            maximum: Dimensions {
                width: header.width as u32,
                height: header.height as u32,
            },
            codec: header.codec,
            vp9: [None; 8],
            av1: None,
            started: false,
        }
    }
    pub(super) fn inspect(self, payload: &[u8]) -> Result<Self> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err(Malformed);
        }
        let mut next = self;
        match self.codec {
            2 => next.vp9_unit(payload)?,
            3 => next.av1_unit(payload)?,
            _ => return Err(Unsupported),
        }
        Ok(next)
    }
    fn vp9_unit(&mut self, bytes: &[u8]) -> Result<()> {
        vp9_frames(bytes, |frame| self.vp9_frame(frame))
    }
    fn vp9_frame(&mut self, bytes: &[u8]) -> Result<()> {
        let mut r = Bits::new(bytes);
        let profile = vp9_profile(&mut r)?;
        if r.bit()? {
            // show_existing_frame: no new dimensions
            self.vp9[r.read(3)? as usize]
                .ok_or(NeedKeyFrame)?
                .check(self.maximum)?;
            return Ok(());
        }
        let key = !r.bit()?;
        let shown = r.bit()?;
        let resilient = r.bit()?;
        if !key && !self.started {
            return Err(NeedKeyFrame);
        }
        let (size, refresh) = if key {
            vp9_sync(&mut r)?;
            vp9_colour(&mut r, profile)?;
            (vp9_size(&mut r)?, 0xff)
        } else {
            let intra = !shown && r.bit()?;
            if !resilient {
                r.read(2)?;
            }
            if intra {
                vp9_sync(&mut r)?;
                if profile > 0 {
                    vp9_colour(&mut r, profile)?;
                }
                let refresh = r.read(8)?;
                (vp9_size(&mut r)?, refresh)
            } else {
                let refresh = r.read(8)?;
                let mut refs = [0; 3];
                for slot in &mut refs {
                    *slot = r.read(3)? as usize;
                    r.read(1)?; // sign bias
                }
                let mut inherited = None;
                for slot in refs {
                    if r.bit()? {
                        inherited = Some(self.vp9[slot].ok_or(NeedKeyFrame)?);
                        break;
                    }
                }
                (
                    match inherited {
                        Some(size) => size,
                        None => vp9_size(&mut r)?,
                    },
                    refresh,
                )
            }
        };
        let size = size.check(self.maximum)?;
        if r.bit()? {
            r.read(16)?;
            r.read(16)?;
        } // render hint, not coded size
        for (index, slot) in self.vp9.iter_mut().enumerate() {
            if refresh & (1 << index) != 0 {
                *slot = Some(size);
            }
        }
        self.started = true;
        Ok(())
    }
    fn av1_unit(&mut self, bytes: &[u8]) -> Result<()> {
        let mut input = bytes;
        let mut frames = 0;
        let mut picture_in_unit = false;
        while !input.is_empty() {
            let (kind, payload) = obu(&mut input)?;
            match kind {
                1 => {
                    if picture_in_unit {
                        return Err(Malformed);
                    }
                    let sequence = Sequence::parse(payload)?;
                    sequence.size.check(self.maximum)?;
                    if self.av1 != Some(sequence) {
                        self.started = false;
                    }
                    self.av1 = Some(sequence);
                }
                2 => {
                    if !payload.is_empty() {
                        return Err(Malformed);
                    }
                    picture_in_unit = false;
                }
                3 | 6 | 7 => {
                    let sequence = self.av1.ok_or(NeedKeyFrame)?;
                    let key = sequence.frame(payload)?;
                    if !key && !self.started {
                        return Err(NeedKeyFrame);
                    }
                    self.started = true;
                    frames += 1;
                    picture_in_unit = true;
                }
                4 | 5 | 15 => (), // tiles, metadata, padding do not set picture size
                8 => return Err(Unsupported), // large-scale tile-list output needs separate bounds
                _ => (),          // reserved OBUs are ignored, as required by AV1
            }
        }
        // Codec configuration must be handled by the session's CSD owner,
        // not submitted as a picture. This API currently requires in-band CSD.
        if frames == 0 {
            return Err(NeedKeyFrame);
        }
        Ok(())
    }
}
fn vp9_frames(bytes: &[u8], mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let marker = *bytes.last().ok_or(Malformed)?;
    if marker & 0xe0 == 0xc0 {
        let count = (marker & 7) as usize + 1;
        let magnitude = ((marker >> 3) & 3) as usize + 1;
        let index_size = 2 + count * magnitude;
        if let Some(start) = bytes.len().checked_sub(index_size)
            && bytes[start] == marker
        {
            let mut offset = 0usize;
            for index in 0..count {
                let mut size = 0u32;
                for byte in 0..magnitude {
                    size |= (bytes[start + 1 + index * magnitude + byte] as u32) << (8 * byte);
                }
                let end = offset.checked_add(size as usize).ok_or(Malformed)?;
                if size == 0 || end > start {
                    return Err(Malformed);
                }
                visit(&bytes[offset..end])?;
                offset = end;
            }
            // A recognized index must partition the payload exactly. Do not
            // fall back to a single frame after accepting its marker pair.
            if offset != start {
                return Err(Malformed);
            }
            return Ok(());
        }
    }
    visit(bytes)
}
fn vp9_profile(r: &mut Bits<'_>) -> Result<u32> {
    if r.read(2)? != 2 {
        return Err(Malformed);
    }
    let profile = r.read(1)? | (r.read(1)? << 1);
    if profile == 3 && r.bit()? {
        return Err(Malformed);
    }
    Ok(profile)
}
fn vp9_size(r: &mut Bits<'_>) -> Result<Dimensions> {
    Ok(Dimensions {
        width: r.read(16)? + 1,
        height: r.read(16)? + 1,
    })
}

/// Format-selection hint, separate from allocation admission: unreadable
/// metadata leaves the current decoder format unchanged, as on Android.
pub(super) fn key_size(
    codec: u8,
    bytes: &[u8],
    configurations: &[impl AsRef<[u8]>],
) -> Option<(u32, u32)> {
    let size = if codec == 2 {
        let mut found = None;
        vp9_frames(bytes, |frame| {
            if found.is_none() {
                found = (|| {
                    let mut r = Bits::new(frame);
                    let profile = vp9_profile(&mut r)?;
                    if r.bit()? || r.bit()? {
                        return Err(NeedKeyFrame);
                    }
                    r.read(2)?;
                    vp9_sync(&mut r)?;
                    vp9_colour(&mut r, profile)?;
                    vp9_size(&mut r)
                })()
                .ok();
            }
            Ok(())
        })
        .ok()?;
        found
    } else if codec == 3 {
        av1_size(bytes).or_else(|| {
            configurations.iter().rev().find_map(|c| {
                let c = c.as_ref();
                av1_size(if c.first() == Some(&0x81) {
                    c.get(4..)?
                } else {
                    c
                })
            })
        })
    } else {
        None
    }?;
    Some((size.width, size.height))
}
fn av1_size(mut bytes: &[u8]) -> Option<Dimensions> {
    while !bytes.is_empty() {
        let (kind, payload) = obu(&mut bytes).ok()?;
        if kind == 1 {
            return Sequence::parse(payload).ok().map(|s| s.size);
        }
    }
    None
}
fn vp9_sync(r: &mut Bits<'_>) -> Result<()> {
    if r.read(24)? != 0x498342 {
        return Err(Malformed);
    }
    Ok(())
}
fn vp9_colour(r: &mut Bits<'_>, profile: u32) -> Result<()> {
    if profile >= 2 {
        r.read(1)?;
    }
    if r.read(3)? != 7 {
        r.read(1)?;
        if profile == 1 || profile == 3 {
            r.read(2)?;
            if r.bit()? {
                return Err(Malformed);
            }
        }
    } else {
        if profile != 1 && profile != 3 {
            return Err(Malformed);
        }
        if r.bit()? {
            return Err(Malformed);
        }
    }
    Ok(())
}

/// Consume one Android-style raw OBU. Size-less final OBUs consume the
/// remaining access unit, matching Android's parser; no Annex B guessing.
fn obu<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8])> {
    let mut take = || {
        let (&byte, rest) = input.split_first().ok_or(Malformed)?;
        *input = rest;
        Ok(byte)
    };
    let header = take()?;
    if header & 0x81 != 0 {
        return Err(Malformed);
    }
    if header & 4 != 0 && take()? & 7 != 0 {
        return Err(Malformed);
    }
    let size = if header & 2 != 0 {
        let mut size = 0u64;
        let mut finished = false;
        for index in 0..8 {
            let byte = take()?;
            size |= ((byte & 127) as u64) << (index * 7);
            if byte & 128 == 0 {
                finished = true;
                break;
            }
        }
        if !finished || size > u32::MAX as u64 {
            return Err(Malformed);
        }
        size as usize
    } else {
        input.len()
    };
    if size > input.len() {
        return Err(Malformed);
    }
    let (payload, rest) = input.split_at(size);
    *input = rest;
    Ok(((header >> 3) & 15, payload))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Sequence {
    size: Dimensions,
    reduced: bool,
    model: bool,
    equal_interval: bool,
    presentation_bits: u32,
    id_bits: u32,
    screen_tools: u32,
    integer_mv: u32,
}
impl Sequence {
    fn parse(payload: &[u8]) -> Result<Self> {
        let mut r = Bits::new(payload);
        if r.read(3)? > 2 {
            return Err(Malformed);
        }
        let still = r.bit()?;
        let reduced = r.bit()?;
        if reduced && !still {
            return Err(Malformed);
        }
        let (mut model, mut equal_interval, mut presentation_bits) = (false, false, 0);
        if reduced {
            r.read(5)?;
        } else {
            let mut delay_bits = 0;
            if r.bit()? {
                r.read(32)?;
                r.read(32)?;
                equal_interval = r.bit()?;
                if equal_interval {
                    r.uvlc()?;
                }
                model = r.bit()?;
                if model {
                    delay_bits = r.read(5)? + 1;
                    r.read(32)?;
                    r.read(5)?;
                    presentation_bits = r.read(5)? + 1;
                }
            }
            let initial_delay = r.bit()?;
            let count = r.read(5)? + 1;
            for _ in 0..count {
                r.read(12)?;
                if r.read(5)? > 7 {
                    r.read(1)?;
                }
                if model && r.bit()? {
                    r.read(delay_bits)?;
                    r.read(delay_bits)?;
                    r.read(1)?;
                }
                if initial_delay && r.bit()? {
                    r.read(4)?;
                }
            }
        }
        let width_bits = r.read(4)? + 1;
        let height_bits = r.read(4)? + 1;
        let size = Dimensions {
            width: r.read(width_bits)? + 1,
            height: r.read(height_bits)? + 1,
        };
        let id_bits = if !reduced && r.bit()? {
            r.read(4)? + 2 + r.read(3)? + 1
        } else {
            0
        };
        r.read(3)?; // superblock, filter intra, intra edge filter
        let (mut screen_tools, mut integer_mv) = (2, 2);
        if !reduced {
            r.read(4)?;
            let order_hint = r.bit()?;
            if order_hint {
                r.read(2)?;
            }
            screen_tools = if r.bit()? { 2 } else { r.read(1)? };
            if screen_tools > 0 {
                integer_mv = if r.bit()? { 2 } else { r.read(1)? };
            }
            if order_hint {
                r.read(3)?;
            }
        }
        Ok(Self {
            size,
            reduced,
            model,
            equal_interval,
            presentation_bits,
            id_bits,
            screen_tools,
            integer_mv,
        })
    }
    fn frame(self, payload: &[u8]) -> Result<bool> {
        let mut r = Bits::new(payload);
        let (kind, shown) = if self.reduced {
            (0, true)
        } else {
            if r.bit()? {
                r.read(3)?;
                if self.model && !self.equal_interval {
                    r.read(self.presentation_bits)?;
                }
                r.read(self.id_bits)?;
                return Ok(false); // only a previously bounded reference can be shown
            }
            let kind = r.read(2)?;
            let shown = r.bit()?;
            if shown && self.model && !self.equal_interval {
                r.read(self.presentation_bits)?;
            }
            if !shown {
                r.read(1)?;
            }
            if kind != 3 && !(kind == 0 && shown) {
                r.read(1)?;
            }
            (kind, shown)
        };
        r.read(1)?; // disable_cdf_update
        let screen = if self.screen_tools == 2 {
            r.read(1)?
        } else {
            self.screen_tools
        };
        if screen == 1 && self.integer_mv == 2 {
            r.read(1)?;
        }
        r.read(self.id_bits)?;
        // The Android encoder uses sequence size. Refuse other conformant AV1
        // syntax until its frame-size/reference parser is implemented and tested.
        if kind == 3 || (!self.reduced && r.bit()?) {
            return Err(Unsupported);
        }
        Ok(kind == 0 && shown)
    }
}

#[cfg(test)]
mod tests;
