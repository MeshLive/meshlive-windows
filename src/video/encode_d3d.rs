//! D3D11 input for hardware encoders: the adapter an encoder MFT runs on, a
//! device on that adapter, a GPU colour conversion (RGBA -> NV12, the D3D11
//! video processor, BT.601 limited range like the Android camera path) into a
//! small pool of encoder input textures, and the one-copy readback used only
//! when an MFT refuses D3D input. A live source renders or captures into an
//! RGBA/NV12 texture, converts on the GPU and hands the NV12 texture to the
//! encoder: no CPU copy of the picture.
use super::encoder::{EncodeError, EncoderSettings};
use std::mem::ManuallyDrop;
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID, RECT, TRUE},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

type Result<T> = std::result::Result<T, EncodeError>;

fn luid_value(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

/// The adapter LUID of a D3D11 device (same encoding as `MFT_ENUM_ADAPTER_LUID`).
pub fn device_luid(device: &ID3D11Device) -> Option<i64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(luid_value(desc.AdapterLuid))
}

/// Hardware adapters: (LUID, description).
pub fn adapters() -> Result<Vec<(i64, String)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else { break };
        let desc = unsafe { adapter.GetDesc1()? };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
        out.push((luid_value(desc.AdapterLuid), name));
    }
    Ok(out)
}

/// A multithread-protected D3D11 device on the adapter with `luid` (for an
/// encoder MFT on that GPU), set up like `video::device`.
pub fn device_on(luid: i64) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else { break };
        if luid_value(unsafe { adapter.GetDesc1()? }.AdapterLuid) != luid {
            continue;
        }
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
        let (mut gpu, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut gpu),
                None,
                Some(&mut context),
            )?;
        }
        let (gpu, context) = (gpu.ok_or(EncodeError::Unsupported)?, context.ok_or(EncodeError::Unsupported)?);
        let protection: ID3D11Multithread = context.cast()?;
        unsafe {
            let _ = protection.SetMultithreadProtected(true);
        }
        return Ok((gpu, context));
    }
    Err(EncodeError::Unsupported)
}

/// One readback of an NV12 texture into tight system-memory NV12 (the
/// fallback for MFTs that refuse D3D input, and software encoders).
pub fn read_nv12(gpu: &ID3D11Device, texture: &ID3D11Texture2D, subresource: u32, s: &EncoderSettings) -> Result<Vec<u8>> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_NV12 || desc.Width < s.width || desc.Height < s.height {
        return Err(EncodeError::Input);
    }
    desc.ArraySize = 1;
    desc.MipLevels = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    unsafe { gpu.CreateTexture2D(&desc, None, Some(&mut staging))? };
    let staging = staging.ok_or(EncodeError::Input)?;
    let context = unsafe { gpu.GetImmediateContext()? };
    let (w, h) = (s.width as usize, s.height as usize);
    let mut out = Vec::with_capacity(s.frame_bytes());
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, subresource, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let base = mapped.pData as *const u8;
        let pitch = mapped.RowPitch as usize;
        for y in 0..h {
            out.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), w));
        }
        // The UV plane follows the full allocated luma height.
        let uv = base.add(pitch * desc.Height as usize);
        for y in 0..h / 2 {
            out.extend_from_slice(std::slice::from_raw_parts(uv.add(y * pitch), w));
        }
        context.Unmap(&staging, 0);
    }
    Ok(out)
}

/// RGBA -> NV12 on the GPU with the D3D11 video processor, into a round-robin
/// pool of encoder input textures. A pool entry is reused only after `pool`
/// newer conversions, so an asynchronous encoder can still hold the older ones.
pub struct RgbaToNv12 {
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    width: u32,
    height: u32,
}
impl RgbaToNv12 {
    pub fn new(gpu: &ID3D11Device, width: u32, height: u32, fps: u32, pool: usize) -> Result<Self> {
        let video: ID3D11VideoDevice = gpu.cast()?;
        let context: ID3D11VideoContext = unsafe { gpu.GetImmediateContext()? }.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content)? };
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0)? };
        unsafe {
            // Input: full-range RGB. Output: BT.601 limited-range YCbCr
            // (bitfield: Usage 1, RGB_Range 1, YCbCr_Matrix 1 (0 = 601), xvYCC 1, Nominal_Range 2 (1 = 16-235)).
            context.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            context.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 });
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            let full = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
        }
        let mut textures = Vec::with_capacity(pool);
        for _ in 0..pool.max(1) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            unsafe { gpu.CreateTexture2D(&desc, None, Some(&mut texture))? };
            let texture = texture.ok_or(EncodeError::Input)?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut view = None;
            unsafe { video.CreateVideoProcessorOutputView(&texture, &enumerator, &view_desc, Some(&mut view))? };
            textures.push((texture, view.ok_or(EncodeError::Input)?));
        }
        Ok(Self { video, context, enumerator, processor, pool: textures, next: 0, width, height })
    }
    /// Converts one RGBA (R8G8B8A8_UNORM) texture; returns the NV12 pool texture.
    pub fn convert(&mut self, rgba: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { rgba.GetDesc(&mut desc) };
        if desc.Width != self.width || desc.Height != self.height {
            return Err(EncodeError::Input);
        }
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input = None;
        unsafe { self.video.CreateVideoProcessorInputView(rgba, &self.enumerator, &input_desc, Some(&mut input))? };
        let (texture, output) = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(input),
            ..Default::default()
        };
        let streams = [stream];
        let result = unsafe { self.context.VideoProcessorBlt(&self.processor, output, 0, &streams) };
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(texture.clone())
    }
}

/// Test/bench input: an RGBA texture with these pixels (a capture source's
/// texture stands in here; uploading is the test's job, not the live path's).
pub fn upload_rgba(gpu: &ID3D11Device, width: u32, height: u32, rgba: &[u8]) -> Result<ID3D11Texture2D> {
    if rgba.len() != (width * height * 4) as usize {
        return Err(EncodeError::Input);
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA { pSysMem: rgba.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
    let mut texture = None;
    unsafe { gpu.CreateTexture2D(&desc, Some(&data), Some(&mut texture))? };
    texture.ok_or(EncodeError::Input)
}

/// BT.601 limited-range NV12 -> full-range RGBA (for generating test input
/// whose GPU conversion back to NV12 should reproduce the pattern's luma).
pub fn nv12_to_rgba(nv12: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![255u8; width * height * 4];
    let chroma = &nv12[width * height..];
    for y in 0..height {
        for x in 0..width {
            let l = 1.164 * (f64::from(nv12[y * width + x]) - 16.0);
            let c = (y / 2) * width + (x / 2) * 2;
            let (u, v) = (f64::from(chroma[c]) - 128.0, f64::from(chroma[c + 1]) - 128.0);
            let px = &mut out[(y * width + x) * 4..][..3];
            px[0] = (l + 1.596 * v).round().clamp(0.0, 255.0) as u8;
            px[1] = (l - 0.392 * u - 0.813 * v).round().clamp(0.0, 255.0) as u8;
            px[2] = (l + 2.017 * u).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// BT.601 limited-range luma of full-range RGBA (the converter's expected Y).
pub fn rgba_luma(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|p| {
            let (r, g, b) = (f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
            (16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0).round().clamp(16.0, 235.0) as u8
        })
        .collect()
}
