use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
    core::{Interface, Result},
};

pub use meshlive::video::device;

pub fn texture(
    gpu: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let mut tex = None;
    unsafe {
        gpu.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: 320,
                Height: 180,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut tex),
        )?;
    }
    let tex = tex.unwrap();
    let mut rtv = None;
    let mut srv = None;
    unsafe {
        gpu.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
        context.ClearRenderTargetView(rtv.as_ref().unwrap(), &[0.25, 0.5, 0.75, 1.0]);
        gpu.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
    }
    // The texture and UI share the same native device. No CPU upload or cross-API transfer.
    let owner = unsafe { tex.GetDevice()? };
    assert_eq!(owner.as_raw(), gpu.as_raw());
    Ok((tex, srv.unwrap()))
}

pub struct Graphics {
    pub gpu: ID3D11Device,
    pub context: ID3D11DeviceContext,
    swap: IDXGISwapChain,
    target: Option<ID3D11RenderTargetView>,
    pub renderer: egui_directx11::Renderer,
    pub texture_id: egui::TextureId,
    pub warp: bool,
    pub level: D3D_FEATURE_LEVEL,
}
impl Graphics {
    pub fn new(
        hwnd: HWND,
        width: u32,
        height: u32,
        force_warp: bool,
        level10: bool,
    ) -> Result<Self> {
        let (gpu, context, level, warp) = match device(force_warp, level10) {
            Ok((d, c, l)) => (d, c, l, force_warp),
            Err(_) if !force_warp => {
                let (d, c, l) = device(true, level10)?;
                (d, c, l, true)
            }
            Err(e) => return Err(e),
        };
        let dxgi: IDXGIDevice = gpu.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        let factory: IDXGIFactory = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let mut swap = None;
        unsafe {
            factory.CreateSwapChain(&gpu, &desc, &mut swap).ok()?;
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
        }
        let swap = swap.unwrap();
        let target = Some(Self::target(&gpu, &swap)?);
        let mut renderer = egui_directx11::Renderer::new(&gpu)?;
        let (_, srv) = texture(&gpu, &context)?;
        let texture_id = renderer.register_user_texture(srv);
        Ok(Self {
            gpu,
            context,
            swap,
            target,
            renderer,
            texture_id,
            warp,
            level,
        })
    }
    fn target(gpu: &ID3D11Device, swap: &IDXGISwapChain) -> Result<ID3D11RenderTargetView> {
        let texture: ID3D11Texture2D = unsafe { swap.GetBuffer(0)? };
        let mut view = None;
        unsafe {
            gpu.CreateRenderTargetView(&texture, None, Some(&mut view))?;
        }
        Ok(view.unwrap())
    }
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.target = None;
        unsafe {
            self.context.OMSetRenderTargets(None, None);
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        unsafe {
            self.swap.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.target = Some(Self::target(&self.gpu, &self.swap)?);
        Ok(())
    }
    pub fn render(
        &mut self,
        ctx: &egui::Context,
        output: egui_directx11::RendererOutput,
    ) -> Result<()> {
        if let Some(target) = &self.target {
            unsafe {
                self.context
                    .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            }
            self.renderer.render(&self.context, target, ctx, output)?;
            unsafe {
                self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_and_native_dpi_scale_geometry_and_clip_exactly_once() {
        let (gpu, context, _) = device(true, true).unwrap();
        let (target, _) = texture(&gpu, &context).unwrap();
        let mut view = None;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            gpu.CreateRenderTargetView(&target, None, Some(&mut view))
                .unwrap();
            target.GetDesc(&mut desc);
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            gpu.CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
        }
        let staging = staging.unwrap();
        for (zoom, native) in [(1.0, 1.0), (2.0, 1.0), (1.5, 1.5)] {
            let ctx = egui::Context::default();
            crate::ui::configure(&ctx);
            ctx.set_zoom_factor(zoom);
            let mut renderer = egui_directx11::Renderer::new(&gpu).unwrap();
            for _ in 0..2 {
                let mut input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(320.0, 180.0) / (zoom * native),
                    )),
                    ..Default::default()
                };
                input
                    .viewports
                    .get_mut(&egui::ViewportId::ROOT)
                    .unwrap()
                    .native_pixels_per_point = Some(native);
                let output = ctx.run(input, |ctx| {
                    let painter = ctx
                        .layer_painter(egui::LayerId::background())
                        .with_clip_rect(egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(40.0, 80.0),
                        ));
                    painter.rect_filled(
                        egui::Rect::from_min_max(egui::pos2(20.0, 20.0), egui::pos2(60.0, 60.0)),
                        0,
                        egui::Color32::WHITE,
                    );
                });
                unsafe {
                    context.ClearRenderTargetView(view.as_ref().unwrap(), &[0.0, 0.0, 0.0, 1.0]);
                }
                renderer
                    .render(
                        &context,
                        view.as_ref().unwrap(),
                        &ctx,
                        egui_directx11::split_output(output).0,
                    )
                    .unwrap();
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                context.CopyResource(&staging, &target);
                context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let pixel = |x: f32, y: f32| {
                    let offset = (y * zoom * native) as usize * mapped.RowPitch as usize
                        + (x * zoom * native) as usize * 4;
                    std::slice::from_raw_parts((mapped.pData as *const u8).add(offset), 4).to_vec()
                };
                let inside = pixel(25.0, 25.0);
                let clipped = pixel(45.0, 25.0);
                context.Unmap(&staging, 0);
                assert_eq!(
                    inside,
                    [255, 255, 255, 255],
                    "Geometry at zoom={zoom}, native={native}"
                );
                assert_eq!(
                    clipped,
                    [0, 0, 0, 255],
                    "Clip at zoom={zoom}, native={native}"
                );
            }
        }
    }

    #[test]
    fn desktop_pages_render_with_native_warp_at_normal_small_and_large_text_sizes() {
        use crate::ui::{Page, Ui, configure};
        // Each case sets up its screen on the scripted fake core (no radio or media).
        type Setup = fn(&mut Ui);
        fn demo(app: &mut Ui) {
            app.bluetooth = Some(meshlive::bluetooth::Capabilities {
                radio: meshlive::bluetooth::Radio::On,
                low_energy: Some(true),
                central: Some(true),
                peripheral: Some(true),
                extended_advertising: None,
                max_advertisement_bytes: None,
                error: None,
            });
            app.peripheral_proof = meshlive::bluetooth::gate::PeripheralProof::Started;
            app.set_demo(true);
            std::thread::sleep(std::time::Duration::from_millis(700));
            app.pump();
        }
        let (gpu, context, _) = device(true, true).unwrap();
        for (name, page, width, height, zoom, setup) in [
            ("live", Page::Home, 1000, 720, 1.0, (|_: &mut Ui| {}) as Setup),
            ("home-demo", Page::Home, 1000, 720, 1.0, demo as Setup),
            ("calls", Page::Calls, 1000, 720, 1.0, (|_: &mut Ui| {}) as Setup),
            ("calls-demo", Page::Calls, 1000, 720, 1.0, demo as Setup),
            ("contacts", Page::Contacts, 1000, 720, 1.0, (|_: &mut Ui| {}) as Setup),
            ("settings", Page::Settings, 1000, 720, 1.0, (|_: &mut Ui| {}) as Setup),
            ("watch", Page::Home, 1000, 720, 1.0, (|a: &mut Ui| {
                demo(a);
                a.start_watch(1);
                std::thread::sleep(std::time::Duration::from_millis(700));
                a.pump();
            }) as Setup),
            ("call", Page::Calls, 1000, 720, 1.0, (|a: &mut Ui| {
                demo(a);
                a.place_call(10, false);
                for _ in 0..4 {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    a.pump();
                }
            }) as Setup),
            ("golive", Page::Home, 1000, 720, 1.0, (|a: &mut Ui| {
                demo(a);
                a.start_live();
                std::thread::sleep(std::time::Duration::from_millis(700));
                a.pump();
                a.open_go_live();
            }) as Setup),
            ("live-small", Page::Home, 640, 480, 1.0, (|_: &mut Ui| {}) as Setup),
            ("live-large-text", Page::Home, 1000, 720, 2.0, (|_: &mut Ui| {}) as Setup),
        ] {
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut target = None;
            let mut view = None;
            unsafe {
                gpu.CreateTexture2D(&desc, None, Some(&mut target)).unwrap();
                gpu.CreateRenderTargetView(target.as_ref().unwrap(), None, Some(&mut view))
                    .unwrap();
            }
            let target = target.unwrap();
            let view = view.unwrap();
            let ctx = egui::Context::default();
            configure(&ctx);
            ctx.set_zoom_factor(zoom);
            let mut app = Ui::default();
            app.page = page;
            setup(&mut app);
            let mut renderer = egui_directx11::Renderer::new(&gpu).unwrap();
            for _ in 0..2 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width as f32 / zoom, height as f32 / zoom),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.show(ctx, egui::TextureId::User(1), "WARP"),
                );
                let (render, _, _) = egui_directx11::split_output(output);
                unsafe {
                    context.ClearRenderTargetView(&view, &[0.0, 0.0, 0.0, 1.0]);
                }
                renderer.render(&context, &view, &ctx, render).unwrap();
            }
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            unsafe {
                gpu.CreateTexture2D(&desc, None, Some(&mut staging))
                    .unwrap();
            }
            let staging = staging.unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            unsafe {
                context.CopyResource(&staging, &target);
                context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                for row in 0..height as usize {
                    rgba.extend_from_slice(std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                        width as usize * 4,
                    ));
                }
                context.Unmap(&staging, 0);
            }
            assert!(
                rgba.chunks_exact(4)
                    .filter(|p| p[0] > 180 && p[1] > 180 && p[2] > 180)
                    .count()
                    > 1000,
                "Missing UI/text pixels on {name}"
            );
            // Optional headless renderer artifacts; these never capture the desktop or its windows.
            if std::env::var_os("MESH_UI_REVIEW").is_some() {
                let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".artifacts/ui-refresh-review");
                std::fs::create_dir_all(&directory).unwrap();
                let file = std::fs::File::create(directory.join(format!("{name}.png"))).unwrap();
                let mut encoder = png::Encoder::new(file, width, height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&rgba)
                    .unwrap();
            }
        }
    }

    #[test]
    fn warp_level10_compiles_real_shaders_and_reads_gpu_texture() {
        let (gpu, context, level) = device(true, true).unwrap();
        assert_eq!(level, D3D_FEATURE_LEVEL_10_0);
        let mut renderer = egui_directx11::Renderer::new(&gpu).unwrap();
        let (texture, srv) = texture(&gpu, &context).unwrap();
        let id = renderer.register_user_texture(srv);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut readback = None;
        unsafe {
            gpu.CreateTexture2D(&desc, None, Some(&mut readback))
                .unwrap();
        }
        let readback = readback.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // Deliberate test-only readback verifies actual GPU-written bytes; production does not read back.
        unsafe {
            context.CopyResource(&readback, &texture);
            context
                .Map(&readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let pixel = std::slice::from_raw_parts(mapped.pData as *const u8, 4).to_vec();
            context.Unmap(&readback, 0);
            assert_eq!(pixel, [64, 128, 191, 255]);
        }
        assert!(renderer.unregister_user_texture(id));
    }
}
