//! Encoder output normalisation, so every backend's bitstream matches what the
//! receivers accept (PROTOCOL "Mid-stream size changes"; the Windows receiver's
//! size guard): the keyframe flag comes from the bitstream (a shown AV1
//! KEY_FRAME / a VP9 key frame), not from an encoder's sample attribute, and
//! every AV1 keyframe temporal unit carries the sequence header in-band.
//! Measured reason: this PC's NVIDIA AV1 MFT writes the sequence header only
//! on its first keyframe and marks an inter frame as a clean point.

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// One OBU: (type, whole OBU bytes, payload range start).
fn next_obu(data: &[u8]) -> Option<(u8, usize, usize)> {
    let header = *data.first()?;
    if header & 0x80 != 0 || header & 0x02 == 0 {
        return None; // forbidden bit, or no size field (not low-overhead format)
    }
    let mut at = 1 + usize::from(header & 0x04 != 0);
    let mut size = 0usize;
    for i in 0..8 {
        let byte = *data.get(at)?;
        at += 1;
        size |= usize::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            break;
        }
    }
    let end = at.checked_add(size)?;
    (end <= data.len()).then_some(((header >> 3) & 15, end, at))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Av1Unit {
    pub key: bool,
    /// Byte range of the sequence header OBU, when present.
    pub sequence: Option<(usize, usize)>,
    /// End of a leading temporal delimiter (0 when absent).
    pub after_delimiter: usize,
}

/// Reads one AV1 temporal unit. `reduced_still` comes from the stream's
/// sequence header (the real-time encoders here never set it).
pub(super) fn av1_unit(data: &[u8], reduced_still: bool) -> Option<Av1Unit> {
    let mut offset = 0;
    let mut unit = Av1Unit {
        key: false,
        sequence: None,
        after_delimiter: 0,
    };
    let mut frame_seen = false;
    while offset < data.len() {
        let (kind, end, payload) = next_obu(&data[offset..])?;
        match kind {
            OBU_TEMPORAL_DELIMITER if offset == 0 => unit.after_delimiter = end,
            OBU_SEQUENCE_HEADER => unit.sequence = Some((offset, offset + end)),
            OBU_FRAME | OBU_FRAME_HEADER if !frame_seen => {
                frame_seen = true;
                let first = *data.get(offset + payload)?;
                unit.key = if reduced_still || unit.sequence.is_some_and(|(s, e)| reduced(&data[s..e])) {
                    true
                } else {
                    // show_existing_frame (1 bit), frame_type (2 bits, 0 = KEY_FRAME), show_frame.
                    first & 0x80 == 0 && (first >> 5) & 3 == 0 && first & 0x10 != 0
                };
            }
            _ => {}
        }
        offset += end;
    }
    frame_seen.then_some(unit)
}

/// reduced_still_picture_header of a whole sequence header OBU.
fn reduced(obu: &[u8]) -> bool {
    next_obu(obu).and_then(|(_, _, payload)| obu.get(payload)).is_some_and(|b| b & 0x08 != 0)
}

/// A VP9 key frame (first frame of a superframe); `None` if not VP9 syntax.
pub(super) fn vp9_key(data: &[u8]) -> Option<bool> {
    let mut position = 0usize;
    let mut bit = || -> Option<u8> {
        let byte = *data.get(position / 8)?;
        let value = (byte >> (7 - position % 8)) & 1;
        position += 1;
        Some(value)
    };
    if (bit()?, bit()?) != (1, 0) {
        return None; // frame_marker
    }
    let low = bit()?;
    let high = bit()?;
    if (high << 1 | low) == 3 && bit()? != 0 {
        return None; // reserved_zero
    }
    if bit()? == 1 {
        return Some(false); // show_existing_frame
    }
    Some(bit()? == 0) // frame_type: 0 = KEY_FRAME
}

/// Normalises one AV1 encoder unit: the keyframe flag from the bitstream, the
/// newest sequence header remembered, and inserted after the temporal
/// delimiter of a keyframe that lacks it. Returns the corrected key flag;
/// `None` when the unit is not a readable temporal unit (left unchanged).
pub(super) fn normalise_av1(data: &mut Vec<u8>, last_sequence: &mut Option<Vec<u8>>) -> Option<bool> {
    let unit = av1_unit(data, false)?;
    if let Some((start, end)) = unit.sequence {
        *last_sequence = Some(data[start..end].to_vec());
    } else if let (true, Some(sequence)) = (unit.key, last_sequence.as_ref()) {
        data.splice(unit.after_delimiter..unit.after_delimiter, sequence.iter().copied());
    }
    Some(unit.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    const TD: [u8; 2] = [0x12, 0x00];
    // Sequence header OBU (type 1) with a 3-byte dummy payload: profile 0, not reduced.
    const SEQ: [u8; 5] = [0x0a, 0x03, 0x00, 0x00, 0x00];
    fn frame(first: u8) -> [u8; 4] {
        [0x32, 0x02, first, 0x00] // OBU_FRAME, size 2
    }
    #[test]
    fn av1_keyframes_come_from_the_bitstream_and_get_the_sequence_header() {
        let key = 0x10; // show_existing 0, KEY_FRAME, show_frame 1
        let inter = 0x30; // INTER_FRAME (1), shown
        let mut first = [&TD[..], &SEQ, &frame(key)].concat();
        let mut sequence = None;
        assert_eq!(normalise_av1(&mut first, &mut sequence), Some(true));
        assert_eq!(sequence.as_deref(), Some(&SEQ[..]));
        let mut later_key = [&TD[..], &frame(key)].concat();
        assert_eq!(normalise_av1(&mut later_key, &mut sequence), Some(true));
        assert_eq!(later_key, [&TD[..], &SEQ, &frame(key)].concat(), "inserted after the delimiter");
        let mut delta = [&TD[..], &frame(inter)].concat();
        let before = delta.clone();
        assert_eq!(normalise_av1(&mut delta, &mut sequence), Some(false), "a clean-point inter frame is not a key");
        assert_eq!(delta, before);
        // Hidden key (show_frame 0) is not a recovery point; show_existing is not either.
        assert_eq!(av1_unit(&[&TD[..], &frame(0x00)].concat(), false).unwrap().key, false);
        assert_eq!(av1_unit(&[&TD[..], &frame(0x80)].concat(), false).unwrap().key, false);
        assert_eq!(av1_unit(&[0x12], false), None, "truncated");
        assert_eq!(av1_unit(&TD, false), None, "no picture");
    }
    #[test]
    fn vp9_key_reads_frame_type_for_each_profile() {
        // marker 10, profile 0 (bits 0,0), show_existing 0, frame_type 0.
        assert_eq!(vp9_key(&[0b1000_0000, 0]), Some(true));
        assert_eq!(vp9_key(&[0b1000_0100, 0]), Some(false)); // frame_type 1
        assert_eq!(vp9_key(&[0b1000_1000, 0]), Some(false)); // show_existing
        assert_eq!(vp9_key(&[0b0000_0000, 0]), None);
    }
}
