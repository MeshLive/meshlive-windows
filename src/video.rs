//! Explicit Media Foundation startup and the UI's shared D3D11 device.
//! Enumeration reports candidates, never successful decoding or phone support.
use std::{marker::PhantomData, rc::Rc};
use windows::{
    Win32::{
        Foundation::{HANDLE, HMODULE},
        Graphics::{Direct3D::*, Direct3D11::*},
        Media::MediaFoundation::*,
        System::Com::*,
    },
    core::{Interface, Result},
};
mod bitstream;
pub use bitstream::BitstreamError;
pub mod annexb;
pub mod codecs;
mod decoder;
pub use decoder::{CodecControls, DecodeError, DecodedFrame, Decoder, OutputFormat};
mod format;
pub use format::{FormatError, FormatTracker, PacketPlan, Rebuild, VideoFormat};
pub mod dav1d;
mod frames;
pub use frames::{DecoderPolicy, MfFrames};
mod picture;
pub use picture::{Picture, PictureSlot, nv12_to_rgba_thumbnail, self_view_step};
mod encoder;
pub use encoder::{
    Availability, Backend, EncodeError, EncodedUnit, Encoder, EncoderSettings, HardwareCandidate, MASK_AV1, MASK_VP9, DECODE_PREFERS_VP9,
    VideoEncoder, availability as encoder_availability, hardware_candidates, hc_check, hc_checked, hc_hardware,
    inventory as encoder_inventory, mark_ineligible, plan as encoder_plan,
};
mod encode_aom;
pub use encode_aom::{AomEncoder, version as aom_version};
mod encode_vpx;
pub use encode_vpx::{VpxEncoder, version as vpx_version};
mod encode_policy;
pub use encode_policy::{KeyframeBackoff, KeyframePolicy};
mod encode_bitstream;
mod encode_d3d;
pub use encode_d3d::{
    RgbaToNv12, adapters as gpu_adapters, device_luid, device_on, nv12_to_rgba, read_nv12, rgba_luma, upload_rgba,
};
mod encode_source;
pub use encode_source::{EncodeStats, SourceEncoder, SourceError};
#[cfg(test)]
mod encode_tests;
#[cfg(test)]
mod encode_tests_d3d;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod hc_tests;
mod surface;
pub use surface::{ChromaLocation, Colour, Crop, Nv12Converter, Nv12Frame, Range};

pub fn device(
    warp: bool,
    level10: bool,
) -> Result<(ID3D11Device, ID3D11DeviceContext, D3D_FEATURE_LEVEL)> {
    let levels = if level10 {
        vec![D3D_FEATURE_LEVEL_10_0]
    } else {
        vec![
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ]
    };
    let mut gpu = None;
    let mut context = None;
    let mut selected = D3D_FEATURE_LEVEL_10_0;
    unsafe {
        D3D11CreateDevice(
            None,
            if warp {
                D3D_DRIVER_TYPE_WARP
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut gpu),
            Some(&mut selected),
            Some(&mut context),
        )?;
    }
    let gpu = gpu.unwrap();
    let context = context.unwrap();
    // MF may use the immediate context on decoder threads. Do not use SINGLETHREADED.
    // VIDEO_SUPPORT is deliberately absent: do not exclude older UI-capable drivers.
    let protection: ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = protection.SetMultithreadProtected(true);
    }
    Ok((gpu, context, selected))
}

struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE).ok()?;
        }
        Ok(Self(PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
struct Runtime {
    _apartment: Apartment,
}
impl Runtime {
    fn new() -> Result<Self> {
        let apartment = Apartment::new()?;
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
        }
        Ok(Self {
            _apartment: apartment,
        })
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Own on a dedicated MTA worker, not the window's STA thread. The manager is
/// released before MF shutdown and COM teardown, including construction failures.
pub struct DecodeDevice {
    manager: IMFDXGIDeviceManager,
    gpu: ID3D11Device,
    _runtime: Runtime,
}
impl DecodeDevice {
    pub fn new(gpu: &ID3D11Device) -> Result<Self> {
        let runtime = Runtime::new()?;
        let context = unsafe { gpu.GetImmediateContext()? };
        let protection: ID3D11Multithread = context.cast()?;
        unsafe {
            let _ = protection.SetMultithreadProtected(true);
        }
        let mut token = 0;
        let mut manager = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        }
        let manager = manager.unwrap();
        unsafe {
            manager.ResetDevice(gpu, token)?;
        }
        Ok(Self {
            manager,
            gpu: gpu.clone(),
            _runtime: runtime,
        })
    }

    /// Inspect the actual manager binding; this is not a decoder activation.
    pub fn gpu(&self) -> Result<ID3D11Device> {
        struct DeviceHandle<'a>(&'a IMFDXGIDeviceManager, HANDLE);
        impl Drop for DeviceHandle<'_> {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.0.CloseDeviceHandle(self.1);
                }
            }
        }
        let handle = DeviceHandle(&self.manager, unsafe { self.manager.OpenDeviceHandle()? });
        let mut result = std::ptr::null_mut();
        unsafe {
            self.manager.TestDevice(handle.1)?;
            self.manager
                .GetVideoService(handle.1, &ID3D11Device::IID, &mut result)?;
            Ok(ID3D11Device::from_raw(result))
        }
    }

    pub fn inventory(&self) -> Result<DecoderInventory> {
        Ok(DecoderInventory {
            vp9: enumerate(MFVideoFormat_VP90)?,
            av1: enumerate(MFVideoFormat_AV1)?,
        })
    }
}

/// The decode mask this PC can honour (CF-HS-02, CF-MEDIA-48): bit 0 VP9,
/// bit 1 AV1, from the Media Foundation decoders actually registered
/// (enumerated once per process); Design HC bit 2 (H.264) and bit 3 (HEVC)
/// with their masters on, only after `hc_decodes`. Codec bits only: a link
/// hello and a header check use this. If enumeration itself fails the old
/// claim (VP9 and AV1) is kept and the failure is visible in `decode_mask_probe`.
pub fn decode_mask() -> u8 {
    decode_masks().0
}
/// The end-to-end call hello's decode mask: `decode_mask` plus Design HC
/// bit 4 ("my AV1 decoding is software") when the AV1 decoder this PC would
/// use is dav1d and it also decodes VP9 or HEVC (a master on, control arm
/// `MESHLIVE_AV1_SOFT_DECODE_BIT=0` off). Never in a link hello.
pub fn endpoint_decode_mask() -> u8 {
    let (mask, dav1d_av1) = decode_masks();
    mask | end_to_end_bits(mask, dav1d_av1, codecs::Switches::current())
}
/// Bit 4 from the codec mask and whether AV1 comes from dav1d (pure).
pub fn end_to_end_bits(mask: u8, dav1d_av1: bool, switches: codecs::Switches) -> u8 {
    let alternative = mask & (codecs::HELLO_VP9 | codecs::HELLO_HEVC) != 0;
    if switches.any() && switches.av1_soft_decode_bit && dav1d_av1 && mask & codecs::HELLO_AV1 != 0 && alternative {
        codecs::DECODE_AV1_SOFTWARE
    } else {
        0
    }
}
fn decode_masks() -> (u8, bool) {
    static MASK: std::sync::OnceLock<(u8, bool)> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| decode_mask_probe_detail().unwrap_or((0x03, false)))
}
/// The end-to-end hello's encode mask: VP9 and AV1 (always: libvpx and
/// libaom are built in), plus Design HC bits 2/3 for an HEVC/H.264 hardware
/// encoder that passed its start check with its master on.
pub fn encode_mask() -> u8 {
    let mut mask = codecs::HELLO_VP9 | codecs::HELLO_AV1;
    if !hc_hardware(codecs::H264).is_empty() {
        mask |= codecs::HELLO_H264;
    }
    if !hc_hardware(codecs::HEVC).is_empty() {
        mask |= codecs::HELLO_HEVC;
    }
    mask
}
/// Design HC: one hello's codec masks. With both masters off `caps` is
/// returned unchanged (today's bytes). With one on, every hello (link and
/// end to end, BMH2 and BMH3, both roles) carries this PC's real encoders
/// (`encode_mask`) and decoders: `decode_mask` in a link hello,
/// `endpoint_decode_mask` (with bit 4) end to end. The peer authenticates a
/// call header's codec against these, so `advertised_encode_mask` also
/// bounds this PC's own choice.
pub fn hello_codecs(caps: crate::bmh2::Capabilities, end_to_end: bool) -> crate::bmh2::Capabilities {
    hello_codecs_with(caps, codecs::Switches::current(), encode_mask, || {
        if end_to_end { endpoint_decode_mask() } else { decode_mask() }
    })
}
/// `hello_codecs` with its inputs (pure, for tests).
pub fn hello_codecs_with(
    caps: crate::bmh2::Capabilities,
    switches: codecs::Switches,
    encode: impl FnOnce() -> u8,
    decode: impl FnOnce() -> u8,
) -> crate::bmh2::Capabilities {
    if !switches.any() {
        return caps;
    }
    crate::bmh2::Capabilities { encode_codecs: encode(), decode_codecs: decode(), ..caps }
}
/// The encode mask this PC's end-to-end hellos advertise: today's 0x03 with
/// both masters off, else `encode_mask`. A call header never names a codec
/// outside it (the peer would refuse it as unauthenticated).
pub fn advertised_encode_mask() -> u8 {
    if codecs::Switches::current().any() { encode_mask() } else { codecs::HELLO_VP9 | codecs::HELLO_AV1 }
}
/// The enumeration behind `decode_mask` (Err: Media Foundation refused).
pub fn decode_mask_probe() -> Result<u8> {
    decode_mask_probe_detail().map(|(mask, _)| mask)
}
fn decode_mask_probe_detail() -> Result<(u8, bool)> {
    let present = |c: Candidates| c.hardware + c.software > 0;
    let mut mask = 0;
    if present(enumerate(MFVideoFormat_VP90)?) {
        mask |= 1;
    }
    // dav1d's startup check runs only on PCs without an MF AV1 decoder.
    let av1 = present(enumerate(MFVideoFormat_AV1)?);
    let dav1d_av1 = !av1 && dav1d::advertise_av1(av1, dav1d::ready());
    if av1 || dav1d_av1 {
        mask |= 2;
    }
    let switches = codecs::Switches::current();
    for (codec, bit) in [(codecs::H264, codecs::HELLO_H264), (codecs::HEVC, codecs::HELLO_HEVC)] {
        if switches.codec_on(codec) && hc_decodes(codec).is_ok() {
            mask |= bit;
        }
    }
    Ok((mask, dav1d_av1))
}

/// Design HC decode fixtures embedded for the start check (hardware-encoder
/// keyframes from the shared Android vectors, 352x288).
pub const HC_DECODE_FIXTURES: [(u8, &[u8]); 2] = [
    (codecs::H264, include_bytes!("../tests/fixtures/hc/a15s-h264/au-000.bin")),
    (codecs::HEVC, include_bytes!("../tests/fixtures/hc/s24-hevc/au-000.bin")),
];
/// Whether this PC decodes `codec` (4 H.264, 5 HEVC) through the whole
/// usable path: the Media Foundation decoder MFT (for HEVC, Microsoft's
/// Store extension) takes the input type, offers NV12 output, starts, and
/// decodes the embedded keyframe fixture to a picture. `SetInputType` alone
/// is not enough. Checked once per process on its own thread.
pub fn hc_decodes(codec: u8) -> std::result::Result<(), String> {
    static H264: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    static HEVC: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    let cell = match codec {
        codecs::H264 => &H264,
        codecs::HEVC => &HEVC,
        _ => return Err("not an HC codec".into()),
    };
    cell.get_or_init(|| std::thread::spawn(move || hc_decode_check(codec)).join().unwrap_or_else(|_| Err("check panicked".into())))
        .clone()
}
/// The check behind `hc_decodes`, uncached.
pub fn hc_decode_check(codec: u8) -> std::result::Result<(), String> {
    let fixture = HC_DECODE_FIXTURES.iter().find(|f| f.0 == codec).ok_or("not an HC codec")?.1;
    let (gpu, _, _) = device(false, false).map_err(|e| format!("device {e}"))?;
    let owner = DecodeDevice::new(&gpu).map_err(|e| format!("media foundation {e}"))?;
    let header = crate::media::VideoHeader { width: 352, height: 288, fps: 15, codec, quarter_turns: 0, opus: false };
    let mut decoder = Decoder::open(&owner, header).map_err(|e| format!("open {e:?}"))?;
    if !decoder.push(fixture, 0, true).map_err(|e| format!("push {e:?}"))? {
        return Err("the decoder refused its first input".into());
    }
    let size = |frame: &DecodedFrame<'_>| (frame.format.crop.width, frame.format.crop.height);
    if let Some(frame) = decoder.pull().map_err(|e| format!("pull {e:?}"))? {
        return if size(&frame) == (352, 288) { Ok(()) } else { Err(format!("decoded {:?}", size(&frame))) };
    }
    decoder.drain().map_err(|e| format!("drain {e:?}"))?;
    match decoder.pull().map_err(|e| format!("pull {e:?}"))? {
        Some(frame) if size(&frame) == (352, 288) => Ok(()),
        Some(frame) => Err(format!("decoded {:?}", size(&frame))),
        None => Err("no picture from the keyframe".into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidates {
    pub hardware: u32,
    pub software: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderInventory {
    pub vp9: Candidates,
    pub av1: Candidates,
}
struct Activations {
    pointer: *mut Option<IMFActivate>,
    count: u32,
}
impl Activations {
    fn new(subtype: windows::core::GUID, flags: MFT_ENUM_FLAG) -> Result<Self> {
        let mut result = Self {
            pointer: std::ptr::null_mut(),
            count: 0,
        };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input),
                None,
                &mut result.pointer,
                &mut result.count,
            )?;
        }
        Ok(result)
    }
}
impl Drop for Activations {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            unsafe {
                std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                    self.pointer,
                    self.count as usize,
                ));
                CoTaskMemFree(Some(self.pointer.cast()));
            }
        }
    }
}
fn enumerate(subtype: windows::core::GUID) -> Result<Candidates> {
    Ok(Candidates {
        hardware: Activations::new(
            subtype,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        )?
        .count,
        software: Activations::new(
            subtype,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )?
        .count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mf_manager_keeps_the_same_level10_warp_device_and_releases_repeatedly() {
        for _ in 0..3 {
            let (gpu, context, level) = device(true, true).unwrap();
            assert_eq!(level, D3D_FEATURE_LEVEL_10_0);
            let protection: ID3D11Multithread = context.cast().unwrap();
            assert!(unsafe { protection.GetMultithreadProtected() }.as_bool());
            let decoder = DecodeDevice::new(&gpu).unwrap();
            let actual = decoder.gpu().unwrap();
            assert_eq!(actual.as_raw(), gpu.as_raw());
            let actual_context = unsafe { actual.GetImmediateContext() }.unwrap();
            assert_eq!(actual_context.as_raw(), context.as_raw());
        }
    }
}
