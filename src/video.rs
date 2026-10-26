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
mod decoder;
pub use decoder::{CodecControls, DecodeError, DecodedFrame, Decoder, OutputFormat};
mod format;
pub use format::{FormatError, FormatTracker, PacketPlan, Rebuild, VideoFormat};
mod frames;
pub use frames::MfFrames;
mod picture;
pub use picture::{Picture, PictureSlot};
mod encoder;
pub use encoder::{
    Availability, Backend, EncodeError, EncodedUnit, Encoder, EncoderSettings, HardwareCandidate, MASK_AV1, MASK_VP9, DECODE_PREFERS_VP9,
    VideoEncoder, availability as encoder_availability, hardware_candidates, inventory as encoder_inventory,
    plan as encoder_plan,
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
/// (enumerated once per process). If enumeration itself fails the old
/// claim (both) is kept and the failure is visible in `decode_mask_probe`.
pub fn decode_mask() -> u8 {
    static MASK: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| decode_mask_probe().unwrap_or(0x03))
}
/// The enumeration behind `decode_mask` (Err: Media Foundation refused).
pub fn decode_mask_probe() -> Result<u8> {
    let present = |c: Candidates| c.hardware + c.software > 0;
    let mut mask = 0;
    if present(enumerate(MFVideoFormat_VP90)?) {
        mask |= 1;
    }
    if present(enumerate(MFVideoFormat_AV1)?) {
        mask |= 2;
    }
    Ok(mask)
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
