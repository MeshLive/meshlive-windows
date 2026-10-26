//! Software VP9 encoding with the pinned static libvpx 1.17.0, through the C
//! boundary in `encode_vpx_shim.c` (WebRTC real-time VP9 settings with
//! Android's CBR, 1 s rate buffer, no frame dropping and keyframes only when
//! forced). Lag 0 and no auto alt-ref: one shown frame per picture, never a
//! superframe. Used when the peer decodes VP9 and this PC has no usable
//! hardware VP9 encoder (or the peer cannot decode AV1).
use super::{
    encode_aom::{Sink, collect, threads},
    encoder::{EncodeError, EncodedUnit, EncoderSettings},
};
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

// Linked by build.rs (static, unbundled).
unsafe extern "C" {
    fn meshlive_vpx_version() -> *const c_char;
    fn meshlive_vpx_open(config: *const Config) -> *mut Native;
    fn meshlive_vpx_close(native: *mut Native);
    fn meshlive_vpx_encode(
        native: *mut Native,
        nv12: *const u8,
        size: usize,
        pts_us: i64,
        force_key: c_int,
        sink: Sink,
        context: *mut c_void,
    ) -> c_int;
    fn meshlive_vpx_flush(native: *mut Native, sink: Sink, context: *mut c_void) -> c_int;
    fn meshlive_vpx_set_bitrate(native: *mut Native, kbps: u32) -> c_int;
}

/// libvpx version string, e.g. "v1.17.0".
pub fn version() -> String {
    unsafe { CStr::from_ptr(meshlive_vpx_version()) }.to_string_lossy().into_owned()
}

/// WebRTC-style speed by picture size (real-time-only build: 5..9).
pub fn cpu_used(settings: &EncoderSettings) -> u32 {
    match settings.width as u64 * settings.height as u64 {
        p if p <= 352 * 288 => 6,
        p if p <= 640 * 480 => 7,
        _ => 8,
    }
}

pub struct VpxEncoder {
    native: *mut Native,
    settings: EncoderSettings,
}
// The native context is used only through `&mut self`.
unsafe impl Send for VpxEncoder {}
impl Drop for VpxEncoder {
    fn drop(&mut self) {
        unsafe { meshlive_vpx_close(self.native) }
    }
}
impl VpxEncoder {
    pub fn open(settings: EncoderSettings) -> Result<Self, EncodeError> {
        if settings.codec != 2 {
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
        let native = unsafe { meshlive_vpx_open(&config) };
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
            meshlive_vpx_encode(
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
        let status = unsafe { meshlive_vpx_flush(self.native, collect, (&mut out as *mut Vec<EncodedUnit>).cast()) };
        if status < 0 {
            return Err(EncodeError::Native(status));
        }
        Ok(out)
    }
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), EncodeError> {
        if unsafe { meshlive_vpx_set_bitrate(self.native, kbps) } < 0 {
            return Err(EncodeError::Input);
        }
        self.settings.bitrate_kbps = kbps;
        Ok(())
    }
}
