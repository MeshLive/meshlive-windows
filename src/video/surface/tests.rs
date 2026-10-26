use super::*;

#[test]
fn nv12_array_planes_match_reference_after_gpu_slice_copy() {
    for level10 in [true, false] {
        let (gpu, context, _) = device(true, level10).unwrap();
        let owner = DecodeDevice::new(&gpu).unwrap();
        let mut converter = Nv12Converter::new(&gpu).unwrap();
        let mut shader = None;
        unsafe {
            gpu.CreatePixelShader(
                include_bytes!(concat!(env!("OUT_DIR"), "/ps_probe.bin")),
                None,
                Some(&mut shader),
            )
            .unwrap();
        }
        converter.pixel = shader.unwrap();
        let mut parts = [vec![0; 24], vec![0; 24]];
        parts[0][..16].fill(64);
        parts[0][16..].copy_from_slice(&[96, 160, 96, 160, 96, 160, 96, 160]);
        parts[1][..16].fill(128);
        parts[1][16..].copy_from_slice(&[144, 80, 144, 80, 144, 80, 144, 80]);
        let mut scratch = None;
        for (slice, expected) in [(0, [64, 96, 160, 255]), (1, [128, 144, 80, 255])] {
            let frame = owner
                .nv12_frame(
                    sample(&gpu, 4, 4, 4, &parts, slice),
                    Crop {
                        left: 0,
                        top: 0,
                        width: 4,
                        height: 4,
                    },
                )
                .unwrap();
            let view = converter
                .convert(&frame, Colour::Bt709, Range::Full, ChromaLocation::Center)
                .unwrap();
            assert_eq!(pixels(&context, view), vec![expected; 16]);
            let input = converter.input.as_ref().unwrap();
            assert!(input.copy);
            if let Some(previous) = scratch {
                assert_eq!(input.texture.as_raw(), previous);
            }
            scratch = Some(input.texture.as_raw());
        }
    }
}

fn sample(
    gpu: &ID3D11Device,
    width: u32,
    height: u32,
    pitch: u32,
    parts: &[Vec<u8>],
    slice: u32,
) -> IMFSample {
    let initial: Vec<_> = parts
        .iter()
        .map(|bytes| D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: pitch * (height + height / 2),
        })
        .collect();
    assert!(
        parts
            .iter()
            .all(|b| b.len() >= (pitch * (height + height / 2)) as usize)
    );
    let mut texture = None;
    unsafe {
        gpu.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: parts.len() as u32,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            },
            Some(initial.as_ptr()),
            Some(&mut texture),
        )
        .unwrap();
        // Diagnose the driver upload separately from the shader's plane views.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.as_ref().unwrap().GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        gpu.CreateTexture2D(&desc, None, Some(&mut staging))
            .unwrap();
        let staging = staging.unwrap();
        let context = gpu.GetImmediateContext().unwrap();
        context.CopyResource(&staging, texture.as_ref().unwrap());
        for (slice, bytes) in parts.iter().enumerate() {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, slice as u32, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let mut actual = Vec::new();
            let mut expected = Vec::new();
            for row in 0..(height + height / 2) as usize {
                actual.extend_from_slice(std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                    width as usize,
                ));
                expected.extend_from_slice(
                    &bytes[row * pitch as usize..row * pitch as usize + width as usize],
                );
            }
            context.Unmap(&staging, slice as u32);
            assert_eq!(actual, expected, "NV12 source upload at slice {slice}");
        }
        let buffer = MFCreateDXGISurfaceBuffer(
            &ID3D11Texture2D::IID,
            texture.as_ref().unwrap(),
            slice,
            false,
        )
        .unwrap();
        let sample = MFCreateSample().unwrap();
        sample.AddBuffer(&buffer).unwrap();
        sample
    }
}
fn pixels(context: &ID3D11DeviceContext, view: &ID3D11ShaderResourceView) -> Vec<[u8; 4]> {
    let texture: ID3D11Texture2D = unsafe { view.GetResource().unwrap() }.cast().unwrap();
    read(context, &texture)
}
fn read(context: &ID3D11DeviceContext, texture: &ID3D11Texture2D) -> Vec<[u8; 4]> {
    let gpu = unsafe { texture.GetDevice().unwrap() };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
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
    let mut result = Vec::new();
    // Deliberate test-only readback, never part of the display path.
    unsafe {
        context.CopyResource(&staging, texture);
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .unwrap();
        for y in 0..desc.Height as usize {
            for x in 0..desc.Width as usize {
                let p =
                    (mapped.pData as *const [u8; 4]).byte_add(y * mapped.RowPitch as usize + x * 4);
                result.push(*p);
            }
        }
        context.Unmap(&staging, 0);
    }
    result
}
fn close(actual: [u8; 4], expected: [u8; 4]) {
    assert!(
        actual
            .into_iter()
            .zip(expected)
            .all(|(a, b)| a.abs_diff(b) <= 2),
        "{actual:?} != {expected:?}"
    );
}

#[test]
fn nv12_range_and_matrix_write_real_rgba_and_reuse_the_target() {
    let (gpu, context, _) = device(true, true).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let mut converter = Nv12Converter::new(&gpu).unwrap();
    let crop = Crop {
        left: 0,
        top: 0,
        width: 2,
        height: 2,
    };
    let mut previous = None;
    for (y, u, v, colour, range, expected) in [
        (16, 128, 128, Colour::Bt601, Range::Limited, [0, 0, 0, 255]),
        (
            235,
            128,
            128,
            Colour::Bt709,
            Range::Limited,
            [255, 255, 255, 255],
        ),
        (81, 90, 240, Colour::Bt601, Range::Limited, [254, 0, 0, 255]),
        (
            63,
            102,
            240,
            Colour::Bt709,
            Range::Limited,
            [255, 1, 0, 255],
        ),
        (0, 128, 128, Colour::Bt709, Range::Full, [0, 0, 0, 255]),
        (
            255,
            128,
            128,
            Colour::Bt601,
            Range::Full,
            [255, 255, 255, 255],
        ),
        (76, 85, 255, Colour::Bt601, Range::Full, [254, 0, 0, 255]),
    ] {
        // Array slice 0 is deliberately wrong; the MF buffer owns slice 1.
        let source = sample(&gpu, 2, 2, 2, &[vec![255; 6], vec![y, y, y, y, u, v]], 1);
        let frame = owner.nv12_frame(source, crop).unwrap();
        let view = converter
            .convert(&frame, colour, range, ChromaLocation::Left)
            .unwrap();
        if let Some(previous) = previous {
            assert_eq!(view.as_raw(), previous);
        }
        previous = Some(view.as_raw());
        for pixel in pixels(&context, view) {
            close(pixel, expected);
        }
    }
}

#[test]
fn nv12_crop_pitch_and_array_slice_survive_the_same_device_egui_pass() {
    let (gpu, context, _) = device(true, true).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let crop = Crop {
        left: 1,
        top: 1,
        width: 5,
        height: 3,
    };
    let mut bytes = vec![255; 16 * 9];
    for y in 0..6 {
        for x in 0..8 {
            bytes[y * 16 + x] = (12 + 15 * x + 25 * y) as u8;
        }
    }
    for y in 6..9 {
        bytes[y * 16..y * 16 + 8].fill(128);
    }
    let frame = owner
        .nv12_frame(sample(&gpu, 8, 6, 16, &[vec![255; 16 * 9], bytes], 1), crop)
        .unwrap();
    let mut converter = Nv12Converter::new(&gpu).unwrap();
    let view = converter
        .convert(&frame, Colour::Bt709, Range::Full, ChromaLocation::Center)
        .unwrap();
    for (i, pixel) in pixels(&context, view).into_iter().enumerate() {
        let y = (12 + 15 * (i % 5 + 1) + 25 * (i / 5 + 1)) as u8;
        close(pixel, [y, y, y, 255]);
    }
    let mut renderer = egui_directx11::Renderer::new(&gpu).unwrap();
    let id = renderer.register_user_texture(view.clone());
    let mut target = None;
    let mut rtv = None;
    unsafe {
        gpu.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: 5,
                Height: 3,
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
            },
            None,
            Some(&mut target),
        )
        .unwrap();
        gpu.CreateRenderTargetView(target.as_ref().unwrap(), None, Some(&mut rtv))
            .unwrap();
    }
    let ctx = egui::Context::default();
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(5.0, 3.0));
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        },
        |ctx| {
            ctx.layer_painter(egui::LayerId::background()).image(
                id,
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        },
    );
    renderer
        .render(
            &context,
            rtv.as_ref().unwrap(),
            &ctx,
            egui_directx11::split_output(output).0,
        )
        .unwrap();
    for (i, pixel) in read(&context, target.as_ref().unwrap())
        .into_iter()
        .enumerate()
    {
        let y = (12 + 15 * (i % 5 + 1) + 25 * (i / 5 + 1)) as u8;
        close(pixel, [y, y, y, 255]);
    }
    // A later conversion can write the same target after egui sampled it.
    let _ = converter
        .convert(&frame, Colour::Bt709, Range::Full, ChromaLocation::Center)
        .unwrap();
    assert!(renderer.unregister_user_texture(id));
}

#[test]
fn nv12_chroma_is_bilinear_with_explicit_location_and_no_padding_bleed() {
    let (gpu, context, _) = device(true, true).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let mut bytes = vec![128; 24];
    bytes[16..].copy_from_slice(&[16, 128, 240, 128, 80, 128, 160, 128]);
    let source = sample(&gpu, 4, 4, 4, &[bytes], 0);
    let frame = owner
        .nv12_frame(
            source.clone(),
            Crop {
                left: 0,
                top: 0,
                width: 4,
                height: 4,
            },
        )
        .unwrap();
    let mut converter = Nv12Converter::new(&gpu).unwrap();
    for (location, expected) in [
        (ChromaLocation::Center, [128, 137, 37, 255]),
        (ChromaLocation::Left, [128, 128, 124, 255]),
        (ChromaLocation::TopLeft, [128, 129, 121, 255]),
    ] {
        let view = converter
            .convert(&frame, Colour::Bt709, Range::Full, location)
            .unwrap();
        close(pixels(&context, view)[5], expected);
    }
    let frame = owner
        .nv12_frame(
            source,
            Crop {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();
    let view = converter
        .convert(&frame, Colour::Bt709, Range::Full, ChromaLocation::Center)
        .unwrap();
    for p in pixels(&context, view) {
        close(p, [128, 149, 0, 255]);
    }
}

#[test]
fn nv12_rejects_invalid_sample_crop_and_device_before_drawing() {
    let (gpu, _, _) = device(true, true).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    assert!(
        owner
            .nv12_frame(
                unsafe { MFCreateSample().unwrap() },
                Crop {
                    left: 0,
                    top: 0,
                    width: 2,
                    height: 2
                }
            )
            .is_err()
    );
    let source = sample(&gpu, 2, 2, 2, &[vec![128; 6]], 0);
    for crop in [
        Crop {
            left: 0,
            top: 0,
            width: 0,
            height: 2,
        },
        Crop {
            left: 1,
            top: 0,
            width: 2,
            height: 2,
        },
        Crop {
            left: u32::MAX,
            top: 0,
            width: 2,
            height: 2,
        },
        Crop {
            left: 0,
            top: u32::MAX,
            width: 2,
            height: 2,
        },
    ] {
        assert!(owner.nv12_frame(source.clone(), crop).is_err());
    }
    let (other, _, _) = device(true, true).unwrap();
    let other_owner = DecodeDevice::new(&other).unwrap();
    let crop = Crop {
        left: 0,
        top: 0,
        width: 2,
        height: 2,
    };
    assert!(other_owner.nv12_frame(source.clone(), crop).is_err());
    let frame = owner.nv12_frame(source, crop).unwrap();
    assert!(
        Nv12Converter::new(&other)
            .unwrap()
            .convert(&frame, Colour::Bt709, Range::Full, ChromaLocation::Left)
            .is_err()
    );
}
