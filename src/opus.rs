//! Pinned, statically linked libopus. One decoder per stream, owned by the media
//! worker. No radio, audio device or capture API is opened by this module.
use crate::{audio_live, media};
use std::{
    ffi::{c_int, c_void},
    ptr::NonNull,
};

unsafe extern "C" {
    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decoder_destroy(decoder: *mut c_void);
    fn opus_decoder_ctl(decoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_packet_get_nb_samples(data: *const u8, length: c_int, fs: c_int) -> c_int;
    fn opus_decode(
        decoder: *mut c_void,
        data: *const u8,
        length: c_int,
        pcm: *mut i16,
        frame_size: c_int,
        fec: c_int,
    ) -> c_int;
    fn opus_encoder_create(
        fs: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_destroy(encoder: *mut c_void);
    fn opus_encoder_ctl(encoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(
        encoder: *mut c_void,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_bytes: c_int,
    ) -> c_int;
}

/// Android's call/stream Opus settings: mono 48 kHz, VOIP, 32 kbps,
/// complexity 10, constrained VBR, 20 ms frames.
pub struct Encoder {
    native: NonNull<c_void>,
    lookahead: u16,
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.native.as_ptr()) }
    }
}
impl Encoder {
    pub const FRAME_SAMPLES: usize = 960;
    pub const BITRATE: i32 = 32_000;
    const APPLICATION_VOIP: c_int = 2048;
    const MAX_PACKET: usize = 4_000;
    const PREROLL_NS: u64 = 80_000_000;
    pub fn new() -> Result<Self, Error> {
        Self::at_rate(Self::BITRATE, Self::APPLICATION_VOIP)
    }
    pub fn radio(preset: u8) -> Result<Self, Error> {
        let rate = match preset { 1 => 16_000, 2 => 24_000, 3 => 48_000, _ => return Err(Error::Packet) };
        Self::at_rate(rate, 2049)
    }
    fn at_rate(rate: i32, application: c_int) -> Result<Self, Error> {
        let mut error = 0;
        let pointer = unsafe { opus_encoder_create(48_000, 1, application, &mut error) };
        if error != 0 {
            if let Some(p) = NonNull::new(pointer) {
                unsafe { opus_encoder_destroy(p.as_ptr()) };
            }
            return Err(Error::Native(error));
        }
        let native = NonNull::new(pointer).ok_or(Error::Native(-7))?;
        let mut encoder = Self {
            native,
            lookahead: 0,
        };
        // OPUS_SET_BITRATE, OPUS_SET_COMPLEXITY, OPUS_SET_VBR_CONSTRAINT.
        for (request, value) in [(4002, rate), (4010, 10), (4020, 1)] {
            let status = unsafe { opus_encoder_ctl(native.as_ptr(), request, value) };
            if status != 0 {
                return Err(Error::Native(status));
            }
        }
        let mut lookahead: i32 = 0;
        // OPUS_GET_LOOKAHEAD: the pre-skip a decoder must discard.
        let status = unsafe { opus_encoder_ctl(native.as_ptr(), 4027, &mut lookahead as *mut i32) };
        if status != 0 || !(0..=u16::MAX as i32).contains(&lookahead) {
            return Err(Error::Native(status));
        }
        encoder.lookahead = lookahead as u16;
        Ok(encoder)
    }
    pub fn pre_skip(&self) -> u16 {
        self.lookahead
    }
    /// The configuration record senders publish: the Codec2
    /// AOPUSHDR/AOPUSDLY/AOPUSPRL block that Android decoders accept.
    pub fn config(&self) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend_from_slice(&self.lookahead.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut out = Vec::with_capacity(16 + head.len() + 48);
        out.extend_from_slice(b"AOPUSHDR");
        out.extend_from_slice(&(head.len() as u64).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(b"AOPUSDLY");
        out.extend_from_slice(&8u64.to_le_bytes());
        out.extend_from_slice(&(u64::from(self.lookahead) * 1_000_000_000 / 48_000).to_le_bytes());
        out.extend_from_slice(b"AOPUSPRL");
        out.extend_from_slice(&8u64.to_le_bytes());
        out.extend_from_slice(&Self::PREROLL_NS.to_le_bytes());
        out
    }
    /// One 20 ms frame of mono PCM to one Opus packet.
    pub fn encode(&mut self, pcm: &[i16; 960]) -> Result<Vec<u8>, Error> {
        let mut out = vec![0; Self::MAX_PACKET];
        let size = unsafe {
            opus_encode(
                self.native.as_ptr(),
                pcm.as_ptr(),
                960,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if size <= 0 {
            return Err(Error::Native(size));
        }
        out.truncate(size as usize);
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Config,
    Packet,
    Buffer,
    Faulted,
    Native(i32),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Opus decode error: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    StreamOrCall,
    AudioOnly20ms,
}

struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.0.as_ptr()) };
    }
}
/// OpusHead pre-skip is consumed once per stream, not again when a relay repeats
/// its cached config. The caller places output after `discarded` samples in PTS.
pub struct Decoded<'a> {
    pub samples: &'a [i16],
    pub discarded: usize,
}
pub struct Decoder {
    native: Option<Native>,
    skip: usize,
    profile: Profile,
}
impl Decoder {
    pub const RATE: usize = 48_000;
    pub const MAX_SAMPLES: usize = 5760; // RFC 6716 maximum 120 ms, mono at 48 kHz.
    pub fn new(config: &[u8], profile: Profile) -> Result<Self, Error> {
        let head = media::opus_head(config).map_err(|_| Error::Config)?;
        if head[8] != 1 || head[9] != 1 || head[18] != 0 {
            return Err(Error::Config);
        }
        let skip = usize::from(u16::from_le_bytes(head[10..12].try_into().unwrap()));
        let gain = i32::from(i16::from_le_bytes(head[16..18].try_into().unwrap()));
        let mut error = 0;
        let pointer = unsafe { opus_decoder_create(Self::RATE as i32, 1, &mut error) };
        let native = NonNull::new(pointer).map(Native);
        if error != 0 {
            return Err(Error::Native(error));
        }
        let native = native.ok_or(Error::Native(-7))?;
        // OPUS_SET_GAIN_REQUEST, signed Q8 dB from OpusHead, applied by libopus.
        let status = unsafe { opus_decoder_ctl(native.0.as_ptr(), 4034, gain) };
        if status != 0 {
            return Err(Error::Native(status));
        }
        Ok(Self {
            native: Some(native),
            skip,
            profile,
        })
    }
    /// Caller reuses a bounded PCM buffer. Validate before FFI; a malformed
    /// record/native failure drops decoder state so it cannot resume silently.
    pub fn decode<'a>(&mut self, unit: &[u8], pcm: &'a mut [i16]) -> Result<Decoded<'a>, Error> {
        if self.native.is_none() {
            return Err(Error::Faulted);
        }
        let result = self.decode_inner(unit, pcm);
        if result.is_err() {
            self.native = None;
            pcm.fill(0);
        }
        result.map(|(count, discarded)| Decoded {
            samples: &pcm[discarded..count],
            discarded,
        })
    }
    fn decode_inner(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<(usize, usize), Error> {
        if unit.is_empty() || unit.len() > 16 * 1024 {
            return Err(Error::Packet);
        }
        if self.profile == Profile::AudioOnly20ms && !audio_live::single_20ms_mono(unit) {
            return Err(Error::Packet);
        }
        let samples = unsafe {
            opus_packet_get_nb_samples(unit.as_ptr(), unit.len() as i32, Self::RATE as i32)
        };
        if samples < 0 {
            return Err(Error::Native(samples));
        }
        let samples = samples as usize;
        if samples == 0
            || samples > Self::MAX_SAMPLES
            || self.profile == Profile::AudioOnly20ms && samples != 960
        {
            return Err(Error::Packet);
        }
        if pcm.len() < samples {
            return Err(Error::Buffer);
        }
        let decoded = unsafe {
            opus_decode(
                self.native.as_ref().unwrap().0.as_ptr(),
                unit.as_ptr(),
                unit.len() as i32,
                pcm.as_mut_ptr(),
                samples as i32,
                0,
            )
        };
        if decoded < 0 {
            return Err(Error::Native(decoded));
        }
        if decoded as usize != samples {
            return Err(Error::Packet);
        }
        let discarded = self.skip.min(samples);
        self.skip -= discarded;
        Ok((samples, discarded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    unsafe extern "C" {
        fn opus_get_version_string() -> *const std::ffi::c_char;
    }
    struct RawEncoder(NonNull<c_void>);
    impl Drop for RawEncoder {
        fn drop(&mut self) {
            unsafe { opus_encoder_destroy(self.0.as_ptr()) }
        }
    }
    fn head(skip: u16) -> Vec<u8> {
        let mut h = b"OpusHead\x01\x01\0\0\x80\xbb\0\0\0\0\0".to_vec();
        h[10..12].copy_from_slice(&skip.to_le_bytes());
        h
    }
    #[test]
    fn pinned_native_decoder_reads_silence_and_applies_preskip_once() {
        let version = unsafe { CStr::from_ptr(opus_get_version_string()) }
            .to_str()
            .unwrap();
        assert_eq!(version, "libopus 1.5.2");
        let mut decoder = Decoder::new(&head(312), Profile::AudioOnly20ms).unwrap();
        let mut pcm = [123; Decoder::MAX_SAMPLES];
        let first = decoder.decode(&[0xf8, 0xff, 0xfe], &mut pcm).unwrap();
        assert_eq!((first.samples.len(), first.discarded), (648, 312));
        assert!(first.samples.iter().all(|s| *s == 0));
        let next = decoder.decode(&[0xf8, 0xff, 0xfe], &mut pcm).unwrap();
        assert_eq!((next.samples.len(), next.discarded), (960, 0));
        assert!(next.samples.iter().all(|s| *s == 0));
    }
    #[test]
    fn generated_tone_encodes_to_real_packets_and_decodes_bounded_pcm() {
        let mut error = 0;
        let pointer = unsafe { opus_encoder_create(48_000, 1, 2048, &mut error) };
        assert_eq!(error, 0);
        let encoder = RawEncoder(NonNull::new(pointer).unwrap());
        let mut decoder = Decoder::new(&head(0), Profile::AudioOnly20ms).unwrap();
        let mut pcm = [0; Decoder::MAX_SAMPLES];
        let mut encoded = [0; 1275];
        let mut energy = 0i64;
        for frame in 0..8 {
            let mut input = [0i16; 960];
            for (i, sample) in input.iter_mut().enumerate() {
                *sample = ((2.0 * std::f64::consts::PI * 500.0 * (frame * 960 + i) as f64
                    / 48_000.0)
                    .sin()
                    * 6000.0) as i16;
            }
            let size = unsafe {
                opus_encode(
                    encoder.0.as_ptr(),
                    input.as_ptr(),
                    960,
                    encoded.as_mut_ptr(),
                    encoded.len() as i32,
                )
            };
            assert!(size > 0);
            let decoded = decoder.decode(&encoded[..size as usize], &mut pcm).unwrap();
            assert_eq!(decoded.samples.len(), 960);
            energy += decoded
                .samples
                .iter()
                .map(|s| i64::from(*s) * i64::from(*s))
                .sum::<i64>();
        }
        assert!(energy > 1_000_000); // Actual non-silent PCM, not a packet/header-only success.
    }
    #[test]
    fn the_production_encoder_publishes_a_codec2_config_and_round_trips_a_quiet_tone() {
        let mut encoder = super::Encoder::new().unwrap();
        let config = encoder.config();
        let head = media::opus_head(&config).unwrap();
        assert_eq!(&head[..10], b"OpusHead\x01\x01");
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), encoder.pre_skip());
        let mut decoder = Decoder::new(&config, Profile::StreamOrCall).unwrap();
        let mut pcm = [0; Decoder::MAX_SAMPLES];
        let mut source = Vec::new();
        let mut decoded = Vec::new();
        for frame in 0..50 {
            let tone = crate::generated::tone_20ms(frame, 440.0, 330.0);
            source.extend_from_slice(&tone);
            let packet = encoder.encode(&tone).unwrap();
            assert!(packet.len() < 200, "about 32 kbps: {}", packet.len());
            decoded.extend_from_slice(decoder.decode(&packet, &mut pcm).unwrap().samples);
        }
        // After the pre-skip the decoded tone lines up with the source.
        let skip = encoder.pre_skip() as usize;
        assert_eq!(decoded.len(), source.len() - skip);
        let tail = 960 * 10;
        let s = &source[tail..tail + 960 * 20];
        let d = &decoded[tail..tail + 960 * 20];
        let signal: f64 = s.iter().map(|v| f64::from(*v).powi(2)).sum();
        let noise: f64 = s
            .iter()
            .zip(d)
            .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
            .sum();
        let snr = 10.0 * (signal / noise).log10();
        assert!(snr > 10.0, "snr {snr:.1} dB");
        // Silence encodes to a bounded packet (no DTX, as on Android).
        let packet = encoder.encode(&[0; 960]).unwrap();
        assert!(!packet.is_empty() && packet.len() < 200);
    }
    #[test]
    fn malformed_packets_short_buffers_and_bad_configs_never_reach_playback() {
        let mut pcm = [123; Decoder::MAX_SAMPLES];
        for bad in [
            &[][..],
            &[0xf9][..],
            &[0xfc][..],
            &[0xf0][..],
            &[0xff; 1276][..],
        ] {
            let mut decoder = Decoder::new(&head(0), Profile::AudioOnly20ms).unwrap();
            assert!(decoder.decode(bad, &mut pcm).is_err());
            assert!(pcm.iter().all(|s| *s == 0));
            assert!(matches!(
                decoder.decode(&[0xf8, 0xff, 0xfe], &mut pcm),
                Err(Error::Faulted)
            ));
        }
        let mut decoder = Decoder::new(&head(0), Profile::StreamOrCall).unwrap();
        assert!(matches!(
            decoder.decode(&[0xf8, 0xff, 0xfe], &mut pcm[..959]),
            Err(Error::Buffer)
        ));
        for field in [8, 9, 18] {
            let mut h = head(0);
            h[field] ^= 1;
            assert!(Decoder::new(&h, Profile::StreamOrCall).is_err());
        }
    }
}
