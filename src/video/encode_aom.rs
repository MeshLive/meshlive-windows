//! Software AV1 encoding with the pinned static libaom 3.14.1 (the same
//! encoder Android always builds in as its "optimized software AV1"), through
//! the narrow C boundary in `encode_aom_shim.c`. Settings follow Android's
//! default arm (one-pass real time, CBR, lag 0, cyclic-refresh AQ, the WebRTC
//! real-time tool profile, no frame dropping, keyframes only when forced).
//! Every keyframe temporal unit carries its sequence header in-band, which is
//! what the Windows decoder and Android's native-AV1 receivers require.
use super::encoder::{EncodeError, EncodedUnit, EncoderSettings};
use std::ffi::{CStr, c_char, c_int, c_void};

#[repr(C)]
struct Config {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    cpu_used: u32,
    threads: u32,
    rate_buffer_ms: u32,
}
#[repr(C)]
struct Native {
    _opaque: [u8; 0],
}
pub(super) type Sink = unsafe extern "C" fn(*mut c_void, *const u8, usize, i64, c_int);

#[link(name = "meshlive_aom", kind = "static")]
unsafe extern "C" {
    fn meshlive_aom_version() -> *const c_char;
    fn meshlive_aom_open(config: *const Config) -> *mut Native;
    fn meshlive_aom_close(native: *mut Native);
    fn meshlive_aom_encode(
        native: *mut Native,
        nv12: *const u8,
        size: usize,
        pts_us: i64,
        force_key: c_int,
        sink: Sink,
        context: *mut c_void,
    ) -> c_int;
    fn meshlive_aom_flush(native: *mut Native, sink: Sink, context: *mut c_void) -> c_int;
    fn meshlive_aom_set_bitrate(native: *mut Native, kbps: u32) -> c_int;
}

pub(super) unsafe extern "C" fn collect(context: *mut c_void, data: *const u8, size: usize, pts: i64, key: c_int) {
    // SAFETY: `context` is the `Vec` passed by `encode`/`finish` for this call
    // only, and libaom's packet buffer is valid for `size` bytes until the
    // next libaom call.
    let out = unsafe { &mut *context.cast::<Vec<EncodedUnit>>() };
    if data.is_null() || size == 0 {
        return;
    }
    out.push(EncodedUnit {
        data: unsafe { std::slice::from_raw_parts(data, size) }.to_vec(),
        pts_us: pts,
        key_frame: key != 0,
    });
}

/// libaom version string, e.g. "3.14.1".
pub fn version() -> String {
    unsafe { CStr::from_ptr(meshlive_aom_version()) }.to_string_lossy().into_owned()
}

/// Android's `VideoCodec.nativeCpuUsed` picks 7-10 by size/FPS for phones; a
/// desktop core is several times faster, so the same table is used as a
/// starting point (never slower than Android's choice).
pub fn cpu_used(settings: &EncoderSettings) -> u32 {
    let pixels = settings.width as u64 * settings.height as u64;
    match pixels {
        p if p <= 640 * 360 && settings.fps <= 24 => 7,
        p if p <= 640 * 360 => 8,
        p if p <= 720 * 480 && settings.fps <= 20 => 7,
        p if p <= 720 * 480 && settings.fps <= 24 => 8,
        p if p <= 720 * 480 => 9,
        _ if settings.fps <= 20 => 8,
        _ => 9,
    }
}
/// Android's `nativeThreadCount` rule (row MT uses the extra cores).
pub(super) fn threads(settings: &EncoderSettings) -> u32 {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
    let pixels = settings.width as u64 * settings.height as u64;
    let wanted = if pixels <= 640 * 360 {
        2
    } else if pixels <= 720 * 480 {
        4
    } else {
        6
    };
    wanted.min(cores.max(1))
}

pub struct AomEncoder {
    native: *mut Native,
    settings: EncoderSettings,
}
// The native context is used only through `&mut self`.
unsafe impl Send for AomEncoder {}
impl Drop for AomEncoder {
    fn drop(&mut self) {
        unsafe { meshlive_aom_close(self.native) }
    }
}
impl AomEncoder {
    pub fn open(settings: EncoderSettings) -> Result<Self, EncodeError> {
        if settings.codec != 3 {
            return Err(EncodeError::Unsupported);
        }
        let config = Config {
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            bitrate_kbps: settings.bitrate_kbps,
            cpu_used: cpu_used(&settings),
            threads: threads(&settings),
            rate_buffer_ms: 1_000,
        };
        let native = unsafe { meshlive_aom_open(&config) };
        if native.is_null() {
            return Err(EncodeError::Input);
        }
        Ok(Self { native, settings })
    }
    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }
    pub fn encode(&mut self, nv12: &[u8], pts_us: i64, force_key: bool) -> Result<Vec<EncodedUnit>, EncodeError> {
        if nv12.len() != self.settings.frame_bytes() || pts_us < 0 {
            return Err(EncodeError::Input);
        }
        let mut out = Vec::new();
        let status = unsafe {
            meshlive_aom_encode(
                self.native,
                nv12.as_ptr(),
                nv12.len(),
                pts_us,
                c_int::from(force_key),
                collect,
                (&mut out as *mut Vec<EncodedUnit>).cast(),
            )
        };
        if status < 0 {
            return Err(EncodeError::Native(status));
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Vec<EncodedUnit>, EncodeError> {
        let mut out = Vec::new();
        let status = unsafe { meshlive_aom_flush(self.native, collect, (&mut out as *mut Vec<EncodedUnit>).cast()) };
        if status < 0 {
            return Err(EncodeError::Native(status));
        }
        Ok(out)
    }
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), EncodeError> {
        if unsafe { meshlive_aom_set_bitrate(self.native, kbps) } < 0 {
            return Err(EncodeError::Input);
        }
        self.settings.bitrate_kbps = kbps;
        Ok(())
    }
}
