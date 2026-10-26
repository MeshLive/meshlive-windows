//! GPU-only 8-bit SDR conversion. Array/decoder-only textures use one same-device
//! GPU slice copy; directly sampleable single textures have no frame copy.
use super::*;
use windows::{
    Win32::{Foundation::E_INVALIDARG, Graphics::Dxgi::Common::*},
    core::Error,
};

#[derive(Debug, Clone, Copy)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Copy)]
pub enum Colour {
    Bt601,
    Bt709,
}
#[derive(Debug, Clone, Copy)]
pub enum Range {
    Limited,
    Full,
}
#[derive(Debug, Clone, Copy)]
pub enum ChromaLocation {
    Left,
    Center,
    TopLeft,
}

/// The original MF sample stays alive until conversion releases this frame,
/// preventing premature decoder-pool reuse. It cannot outlive its MF owner.
pub struct Nv12Frame<'a> {
    texture: ID3D11Texture2D,
    desc: D3D11_TEXTURE2D_DESC,
    crop: Crop,
    coded: [u32; 2],
    slice: u32,
    owner: &'a DecodeDevice,
    _sample: IMFSample,
}
impl DecodeDevice {
    pub fn nv12_frame(&self, sample: IMFSample, crop: Crop) -> Result<Nv12Frame<'_>> {
        if unsafe { sample.GetBufferCount()? } != 1 {
            return Err(invalid());
        }
        let buffer: IMFDXGIBuffer = unsafe { sample.GetBufferByIndex(0)? }.cast()?;
        let mut resource = std::ptr::null_mut();
        let (texture, slice) = unsafe {
            buffer.GetResource(&ID3D11Texture2D::IID, &mut resource)?;
            (
                ID3D11Texture2D::from_raw(resource),
                buffer.GetSubresourceIndex()?,
            )
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        if unsafe { texture.GetDevice()? }.as_raw() != self.gpu.as_raw()
            || desc.Format != DXGI_FORMAT_NV12
            || desc.MipLevels != 1
            || desc.SampleDesc.Count != 1
            || slice >= desc.ArraySize
            || desc.Width == 0
            || desc.Height == 0
            || desc.Width > 4096
            || desc.Height > 4096
            || desc.Width % 2 != 0
            || desc.Height % 2 != 0
            || crop.width == 0
            || crop.height == 0
            || crop
                .left
                .checked_add(crop.width)
                .is_none_or(|x| x > desc.Width)
            || crop
                .top
                .checked_add(crop.height)
                .is_none_or(|y| y > desc.Height)
        {
            return Err(invalid());
        }
        Ok(Nv12Frame {
            texture,
            desc,
            crop,
            coded: [desc.Width, desc.Height],
            slice,
            owner: self,
            _sample: sample,
        })
    }
}
impl Nv12Frame<'_> {
    /// The visible picture size (the crop).
    pub fn crop_size(&self) -> [u32; 2] {
        [self.crop.width, self.crop.height]
    }
    /// One staging copy of this frame's slice, cropped to `w` x `h`, as tight
    /// NV12 in system memory (Y plane, then interleaved UV). The decode
    /// device's context is multithread-protected (`DecodeDevice::new`).
    pub fn read_nv12_cropped(&self, w: usize, h: usize) -> Result<Vec<u8>> {
        let gpu = &self.owner.gpu;
        let mut desc = self.desc;
        desc.ArraySize = 1;
        desc.MipLevels = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { gpu.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let staging = staging.ok_or_else(invalid)?;
        let context = unsafe { gpu.GetImmediateContext()? };
        let (left, top) = (self.crop.left as usize & !1, self.crop.top as usize & !1);
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.texture, self.slice, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let base = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add((top + y) * pitch + left), w));
            }
            // The UV plane follows the full allocated luma height.
            let uv = base.add(pitch * desc.Height as usize);
            for y in 0..h / 2 {
                out.extend_from_slice(std::slice::from_raw_parts(uv.add((top / 2 + y) * pitch + left), w));
            }
            context.Unmap(&staging, 0);
        }
        Ok(out)
    }
}
fn invalid() -> Error {
    Error::from_hresult(E_INVALIDARG)
}

struct Output {
    size: [u32; 2],
    target: ID3D11RenderTargetView,
    view: ID3D11ShaderResourceView,
}
struct Input {
    texture: ID3D11Texture2D,
    views: [Option<ID3D11ShaderResourceView>; 2],
    coded: [u32; 2],
    copy: bool,
}
/// One render-thread owner. Run the conversion before the egui pass; output SRV
/// is reused at the same visible size and must be re-registered after a resize.
/// Colour/range/chroma location must come from decoder metadata, not BTV3 size.
pub struct Nv12Converter {
    gpu: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    raster: ID3D11RasterizerState,
    depth: ID3D11DepthStencilState,
    input: Option<Input>,
    output: Option<Output>,
    _thread: PhantomData<Rc<()>>,
}
impl Nv12Converter {
    pub fn new(gpu: &ID3D11Device) -> Result<Self> {
        let (mut vertex, mut pixel, mut constants, mut sampler, mut raster, mut depth) =
            (None, None, None, None, None, None);
        unsafe {
            gpu.CreateVertexShader(
                include_bytes!(concat!(env!("OUT_DIR"), "/vs_video.bin")),
                None,
                Some(&mut vertex),
            )?;
            gpu.CreatePixelShader(
                include_bytes!(concat!(env!("OUT_DIR"), "/ps_video.bin")),
                None,
                Some(&mut pixel),
            )?;
            gpu.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 96,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut constants),
            )?;
            gpu.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxAnisotropy: 1,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            gpu.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut raster),
            )?;
            gpu.CreateDepthStencilState(
                &D3D11_DEPTH_STENCIL_DESC {
                    DepthFunc: D3D11_COMPARISON_ALWAYS,
                    ..Default::default()
                },
                Some(&mut depth),
            )?;
        }
        Ok(Self {
            gpu: gpu.clone(),
            context: unsafe { gpu.GetImmediateContext()? },
            vertex: vertex.unwrap(),
            pixel: pixel.unwrap(),
            constants: constants.unwrap(),
            sampler: sampler.unwrap(),
            raster: raster.unwrap(),
            depth: depth.unwrap(),
            input: None,
            output: None,
            _thread: PhantomData,
        })
    }
    pub fn convert(
        &mut self,
        frame: &Nv12Frame<'_>,
        colour: Colour,
        range: Range,
        chroma: ChromaLocation,
    ) -> Result<&ID3D11ShaderResourceView> {
        if frame.owner.gpu.as_raw() != self.gpu.as_raw() {
            return Err(invalid());
        }
        // NV12 array SRVs accepted by the runtime returned incorrect plane bytes
        // on both local hardware and WARP. Decoder-bound arrays also cannot be
        // sampled directly per D3D11_BIND_DECODER. Keep the workaround GPU-local.
        let copy = frame.desc.ArraySize != 1
            || frame.desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0
            || frame.desc.BindFlags & D3D11_BIND_DECODER.0 as u32 != 0;
        let reuse = self.input.as_ref().is_some_and(|i| {
            i.copy == copy
                && i.coded == frame.coded
                && (copy || i.texture.as_raw() == frame.texture.as_raw())
        });
        if !reuse {
            let texture = if copy {
                let mut desc = frame.desc;
                desc.ArraySize = 1;
                desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                desc.Usage = D3D11_USAGE_DEFAULT;
                desc.CPUAccessFlags = 0;
                desc.MiscFlags = 0;
                let mut texture = None;
                unsafe {
                    self.gpu.CreateTexture2D(&desc, None, Some(&mut texture))?;
                }
                texture.unwrap()
            } else {
                frame.texture.clone()
            };
            let gpu: ID3D11Device3 = self.gpu.cast()?;
            let mut views = [None, None];
            for (plane, (view, format)) in views
                .iter_mut()
                .zip([DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM])
                .enumerate()
            {
                let desc = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
                    Format: format,
                    ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
                        Texture2D: D3D11_TEX2D_SRV1 {
                            MostDetailedMip: 0,
                            MipLevels: 1,
                            PlaneSlice: plane as u32,
                        },
                    },
                };
                let mut selected = None;
                unsafe {
                    gpu.CreateShaderResourceView1(&texture, Some(&desc), Some(&mut selected))?;
                }
                *view = Some(selected.unwrap().cast()?);
            }
            self.input = Some(Input {
                texture,
                views,
                coded: frame.coded,
                copy,
            });
        }
        let crop = frame.crop;
        let size = [crop.width, crop.height];
        if self.output.as_ref().is_none_or(|o| o.size != size) {
            let (mut texture, mut target, mut view) = (None, None, None);
            unsafe {
                self.gpu.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: size[0],
                        Height: size[1],
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )?;
                self.gpu.CreateRenderTargetView(
                    texture.as_ref().unwrap(),
                    None,
                    Some(&mut target),
                )?;
                self.gpu.CreateShaderResourceView(
                    texture.as_ref().unwrap(),
                    None,
                    Some(&mut view),
                )?;
            }
            self.output = Some(Output {
                size,
                target: target.unwrap(),
                view: view.unwrap(),
            });
        }
        let rows = parameters(frame, colour, range, chroma);
        let input = self.input.as_ref().unwrap();
        let output = self.output.as_ref().unwrap();
        unsafe {
            // The UI may have sampled this same output in its previous pass.
            self.context.PSSetShaderResources(0, Some(&[None, None]));
            if copy {
                self.context.CopySubresourceRegion(
                    &input.texture,
                    0,
                    0,
                    0,
                    0,
                    &frame.texture,
                    frame.slice,
                    None,
                );
            }
            self.context
                .UpdateSubresource(&self.constants, 0, None, rows.as_ptr().cast(), 0, 0);
            self.context.IASetInputLayout(None);
            self.context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.GSSetShader(None, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.PSSetShaderResources(0, Some(&input.views));
            self.context.RSSetState(&self.raster);
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: size[0] as f32,
                Height: size[1] as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context.OMSetDepthStencilState(&self.depth, 0);
            self.context
                .OMSetRenderTargets(Some(&[Some(output.target.clone())]), None);
            self.context.Draw(3, 0);
            // Release decoder input bindings before its sample/pool can be reused.
            self.context.PSSetShaderResources(0, Some(&[None, None]));
            self.context.OMSetRenderTargets(None, None);
            self.gpu.GetDeviceRemovedReason()?;
        }
        Ok(&output.view)
    }
}
fn parameters(
    frame: &Nv12Frame<'_>,
    colour: Colour,
    range: Range,
    chroma: ChromaLocation,
) -> [f32; 24] {
    let (kr, kb) = match colour {
        Colour::Bt601 => (0.299, 0.114),
        Colour::Bt709 => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = match range {
        Range::Limited => (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0),
        Range::Full => (1.0, 1.0, 0.0),
    };
    let (r, bu, gu, gv) = (
        2.0 * (1.0 - kr) * cs,
        2.0 * (1.0 - kb) * cs,
        -2.0 * kb * (1.0 - kb) / kg * cs,
        -2.0 * kr * (1.0 - kr) / kg * cs,
    );
    let co = 128.0 / 255.0;
    let crop = frame.crop;
    let inv = [2.0 / frame.coded[0] as f32, 2.0 / frame.coded[1] as f32];
    let origin = match chroma {
        ChromaLocation::Left => [0.0, 0.5],
        ChromaLocation::Center => [0.5, 0.5],
        ChromaLocation::TopLeft => [0.0, 0.0],
    };
    [
        ys,
        0.0,
        r,
        -ys * yo - r * co,
        ys,
        gu,
        gv,
        -ys * yo - (gu + gv) * co,
        ys,
        bu,
        0.0,
        -ys * yo - bu * co,
        crop.left as f32,
        crop.top as f32,
        inv[0],
        inv[1],
        origin[0],
        origin[1],
        0.0,
        0.0,
        ((crop.left / 2) as f32 + 0.5) * inv[0],
        ((crop.top / 2) as f32 + 0.5) * inv[1],
        (((crop.left + crop.width - 1) / 2) as f32 + 0.5) * inv[0],
        (((crop.top + crop.height - 1) / 2) as f32 + 0.5) * inv[1],
    ]
}

#[cfg(test)]
mod tests;
