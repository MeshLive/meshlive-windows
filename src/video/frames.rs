//! Media Foundation behind the receive path's `FrameDecoder` trait. Each
//! decoded picture is handed to the presenter while the decoder is still
//! borrowed (same-device NV12, no copy; see `Nv12Converter`), and only its
//! timestamp returns to the pipeline. Limits carried over from the native
//! decoder: it reads codec configuration only in band. A VP9 configuration
//! record is a no-op (VP9 carries none in practice); an AV1 one (av1C or raw
//! OBUs) is kept and its configuration OBUs are spliced into each following
//! keyframe's temporal unit, after its temporal delimiter.
use super::{DecodeDevice, DecodedFrame, Decoder, VideoFormat};
use crate::{media::VideoHeader, viewer::FrameDecoder};

pub struct MfFrames<'a, P: FnMut(i64, &DecodedFrame<'_>)> {
    owner: &'a DecodeDevice,
    header: VideoHeader,
    decoder: Option<Decoder<'a>>,
    present: P,
    av1_config: Option<Vec<u8>>,
    /// Why the last configure, push or pull failed (the trait reports `()`).
    pub last_error: Option<String>,
}

/// The configuration OBUs of an AV1 configuration record: an
/// AV1CodecConfigurationRecord (marker 1, version 1: first byte 0x81) carries
/// them after its 4-byte header; anything else is taken as raw OBUs.
pub fn av1_config_obus(config: &[u8]) -> &[u8] {
    if config.len() >= 4 && config[0] == 0x81 {
        &config[4..]
    } else {
        config
    }
}
/// Inserts configuration OBUs into a keyframe temporal unit, after a leading
/// temporal delimiter (OBU type 2) when there is one.
pub fn splice_av1_config(obus: &[u8], key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(obus.len() + key.len());
    let delimiter =
        key.len() >= 2 && (key[0] >> 3) & 0x0f == 2 && key[0] & 0x02 != 0 && key[1] == 0;
    let split = if delimiter { 2 } else { 0 };
    out.extend_from_slice(&key[..split]);
    out.extend_from_slice(obus);
    out.extend_from_slice(&key[split..]);
    out
}
impl<'a, P: FnMut(i64, &DecodedFrame<'_>)> MfFrames<'a, P> {
    pub fn new(owner: &'a DecodeDevice, header: VideoHeader, present: P) -> Self {
        Self {
            owner,
            header,
            decoder: None,
            present,
            av1_config: None,
            last_error: None,
        }
    }
}
impl<P: FnMut(i64, &DecodedFrame<'_>)> MfFrames<'_, P> {
    /// `FrameDecoder::configure` from the format's newest configuration
    /// record alone (what the decode thread receives).
    pub fn configure_with(&mut self, newest: Option<&[u8]>) -> Result<(), ()> {
        // A rebuild drops the old transform first (its pool is released).
        self.decoder = None;
        // AV1 uses the newest configuration; none means in-band only.
        self.av1_config = (self.header.codec == 3).then_some(newest).flatten().map(|c| av1_config_obus(c).to_vec());
        match Decoder::open(self.owner, self.header) {
            Ok(decoder) => self.decoder = Some(decoder),
            Err(error) => {
                self.last_error = Some(format!("open {}x{} codec {}: {error:?}", self.header.width, self.header.height, self.header.codec));
                return Err(());
            }
        }
        Ok(())
    }
}
impl<P: FnMut(i64, &DecodedFrame<'_>)> FrameDecoder for MfFrames<'_, P> {
    fn configure(&mut self, format: &VideoFormat) -> Result<(), ()> {
        self.configure_with(format.configurations().last().map(|c| &c[..]))
    }
    fn push(&mut self, payload: &[u8], pts_us: i64, key: bool, config: bool) -> Result<bool, ()> {
        if config {
            if self.header.codec == 3 {
                self.av1_config = Some(av1_config_obus(payload).to_vec());
            }
            return Ok(true);
        }
        let Some(decoder) = self.decoder.as_mut() else {
            self.last_error = Some("push without a decoder".into());
            return Err(());
        };
        let result = match (&self.av1_config, key) {
            (Some(obus), true) => decoder.push(&splice_av1_config(obus, payload), pts_us, key),
            _ => decoder.push(payload, pts_us, key),
        };
        result.map_err(|error| {
            self.last_error = Some(format!("push {} bytes key={key}: {error:?}", payload.len()));
        })
    }
    fn pull(&mut self) -> Result<Option<i64>, ()> {
        // Nothing configured yet (the stream header arrived before its first
        // keyframe): nothing to pull. A15s run: this was reported as a decoder
        // failure, so the phone's video was never decoded.
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(None);
        };
        let pulled = decoder.pull().map_err(|error| {
            self.last_error = Some(format!("pull: {error:?}"));
        })?;
        let Some(frame) = pulled else {
            return Ok(None);
        };
        let pts_us = frame.timestamp_100ns / 10;
        (self.present)(pts_us, &frame);
        Ok(Some(pts_us))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn av1_configuration_obus_are_spliced_after_the_temporal_delimiter() {
        let sequence = [0x0a, 0x03, 1, 2, 3];
        let av1c = [&[0x81, 0x00, 0x0c, 0x00][..], &sequence].concat();
        assert_eq!(av1_config_obus(&av1c), sequence);
        assert_eq!(av1_config_obus(&sequence), sequence);
        let key = [0x12, 0x00, 0x32, 0x01, 9];
        assert_eq!(
            splice_av1_config(&sequence, &key),
            [0x12, 0x00, 0x0a, 0x03, 1, 2, 3, 0x32, 0x01, 9]
        );
        let bare = [0x32, 0x01, 9];
        assert_eq!(
            splice_av1_config(&sequence, &bare),
            [0x0a, 0x03, 1, 2, 3, 0x32, 0x01, 9]
        );
    }
}

/// A phone's call video as Android sends it: libaom AV1 at 352x288 with a
/// rotated header (quarter_turns 1), in-band sequence header on keyframes,
/// through the receive queue into the native decoder. Rotation is applied
/// at display, so the decoder sees the coded 352x288 size.
#[cfg(test)]
mod android_call_video {
    use super::*;
    use crate::{
        media::{self, VideoHeader},
        playout::Profile,
        video::{Backend, EncoderSettings, SourceEncoder, VideoEncoder},
        viewer::VideoReceiver,
    };
    use std::{
        cell::Cell,
        rc::Rc,
        time::{Duration, Instant},
    };

    #[test]
    fn a_rotated_352x288_libaom_call_stream_decodes() {
        let (gpu, _, _) = crate::video::device(false, false).unwrap();
        let owner = DecodeDevice::new(&gpu).unwrap();
        let settings = EncoderSettings {
            codec: 3,
            width: 352,
            height: 288,
            fps: 15,
            bitrate_kbps: 200,
            keyframe_interval_s: 60,
        };
        let encoder = VideoEncoder::open(&owner, settings, Backend::SoftwareAv1).unwrap();
        let mut source = SourceEncoder::new(encoder, 60);
        source.request_keyframe(0, true);
        let header = VideoHeader {
            width: 352,
            height: 288,
            fps: 15,
            codec: 3,
            quarter_turns: 1,
            opus: true,
        };
        let mut receiver = VideoReceiver::new(header);
        let mut clock = Profile::Call.clock();
        let decoded = Rc::new(Cell::new(0u32));
        let counter = decoded.clone();
        // Every decoded picture is also read back as RGBA for the call screen.
        let slot = std::sync::Arc::new(crate::video::PictureSlot::default());
        let shown = slot.clone();
        let mut frames = MfFrames::new(&owner, header, move |pts, frame| {
            counter.set(counter.get() + 1);
            match frame.nv12().and_then(|nv12| nv12.read_rgba(frame.format.colour, frame.format.range)) {
                Ok((w, h, rgba)) => shown.publish(w, h, rgba, pts),
                Err(_) => shown.read_failed(),
            }
        });
        // Draining before the first keyframe (no decoder yet) is not a failure.
        receiver.drain(&mut frames, 0, &clock).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        for i in 0..15u32 {
            let pts = i64::from(i) * 66_667;
            let picture = crate::generated::pattern_nv12(352, 288, i);
            for packet in source.encode(&picture, pts, deadline).unwrap() {
                let p = media::Packet::decode(&packet, media::MAX_PAYLOAD).unwrap();
                receiver.on_packet(p, pts * 1000, &mut clock).unwrap();
            }
            let fed = receiver.feed(&mut frames);
            assert!(fed.is_ok(), "frame {i}: {:?}", frames.last_error);
            receiver.drain(&mut frames, i64::MAX / 4, &clock).unwrap();
        }
        assert!(decoded.get() >= 10, "decoded {} ({:?})", decoded.get(), frames.last_error);
        assert_eq!((slot.published(), slot.failures()), (u64::from(decoded.get()), 0));
        let picture = slot.take_newer(0).unwrap();
        assert_eq!((picture.width, picture.height, picture.rgba.len()), (352, 288, 352 * 288 * 4));
        let first = &picture.rgba[..4];
        assert!(picture.rgba.chunks(4).any(|p| p != first), "the pattern is not one flat colour");
    }
}
