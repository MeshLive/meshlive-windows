//! Store screenshots (test-only, `#[ignore]`): the app's own render path
//! (WARP, the real UI code) with privacy-safe sample data, written as plain
//! PNGs (no text chunks) into `MESH_SCREENSHOTS`. Names are names-v2
//! generated names from fixed sample IDs; the call pictures are procedural
//! scenes (no camera, no person). Run:
//! `MESH_SCREENSHOTS=<dir> cargo test --release --bin meshlive store_screenshots -- --ignored`
#![cfg(test)]

use meshlive::video::device;
use crate::ui::{
    Page, Ui,
    backend::{PictureHandle, Update},
    configure,
    model::{Contact, Person, Reach, StreamEntry},
};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

/// A names-v2 generated name for a fixed sample ID (no device's ID).
fn generated(seed: u8) -> String {
    let rdi = [0x5a, seed, 0x17, seed ^ 0x3c, 0x81, 0x2e];
    meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(&rdi)).expect("name lists").name
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}
fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn rgba(pixels: impl Iterator<Item = [f32; 3]>) -> Vec<u8> {
    pixels.flat_map(|c| [c[0].clamp(0.0, 255.0) as u8, c[1].clamp(0.0, 255.0) as u8, c[2].clamp(0.0, 255.0) as u8, 255]).collect()
}

/// A lake at dusk (portrait, as a phone sends it): sky, sun glow, two
/// mountain ridges, the lake with the sky's reflection and soft ripples.
pub fn lake_scene(w: usize, h: usize) -> Vec<u8> {
    let horizon = 0.58;
    rgba((0..w * h).map(|i| {
        let (x, y) = ((i % w) as f32 / w as f32, (i / w) as f32 / h as f32);
        let sky = |yy: f32| {
            let t = yy / horizon;
            let c = mix([38.0, 52.0, 104.0], [214.0, 120.0, 92.0], t.powf(1.6));
            mix(c, [250.0, 196.0, 128.0], smooth(0.75, 1.0, t))
        };
        let sun = |xx: f32, yy: f32| {
            let d = ((xx - 0.62).powi(2) + ((yy - 0.47) * 1.2).powi(2)).sqrt();
            (1.0 - smooth(0.02, 0.05, d)) * 1.0 + (1.0 - smooth(0.04, 0.28, d)) * 0.35
        };
        let far = horizon - 0.07 - 0.035 * (x * 9.0).sin() - 0.02 * (x * 23.0 + 1.0).sin();
        let near = horizon - 0.02 - 0.05 * (x * 5.0 + 2.0).sin().max(0.0) - 0.012 * (x * 31.0).sin();
        if y < horizon {
            let mut c = sky(y);
            let s = sun(x, y);
            c = mix(c, [255.0, 236.0, 196.0], s);
            if y > far {
                c = mix(c, [86.0, 72.0, 110.0], 0.85);
            }
            if y > near {
                c = mix(c, [40.0, 36.0, 58.0], 0.92);
            }
            c
        } else {
            // The lake: the mirrored sky, darker, with gentle ripples.
            let depth = (y - horizon) / (1.0 - horizon);
            let ripple = 0.012 * ((y * 140.0).sin() + 0.6 * (x * 40.0 + y * 90.0).sin());
            let my = (2.0 * horizon - y + ripple).clamp(0.0, horizon - 0.001);
            let mut c = mix(sky(my), [255.0, 236.0, 196.0], sun(x, my) * (1.0 - depth));
            c = mix(c, [18.0, 24.0, 44.0], 0.35 + 0.5 * depth);
            // A dark shore at the bottom.
            let shore = 0.94 + 0.02 * (x * 7.0).sin();
            if y > shore {
                c = mix(c, [14.0, 16.0, 20.0], 0.9);
            }
            c
        }
    }))
}

/// A quiet room corner (landscape, as a PC camera sends it): a warm wall, a
/// bright window, a shelf and a desk edge. No person.
pub fn room_scene(w: usize, h: usize) -> Vec<u8> {
    rgba((0..w * h).map(|i| {
        let (x, y) = ((i % w) as f32 / w as f32, (i / w) as f32 / h as f32);
        let mut c = mix([150.0, 128.0, 108.0], [92.0, 78.0, 70.0], x * 0.8 + y * 0.3);
        // Window light from the left.
        let window = x > 0.08 && x < 0.38 && y > 0.12 && y < 0.62;
        if window {
            c = mix([218.0, 230.0, 240.0], [176.0, 198.0, 222.0], (y - 0.12) / 0.5);
            if (x - 0.23).abs() < 0.008 || (y - 0.37).abs() < 0.01 {
                c = [120.0, 106.0, 96.0];
            }
        } else {
            let glow = 1.0 - smooth(0.0, 0.45, ((x - 0.23).powi(2) + (y - 0.37).powi(2)).sqrt());
            c = mix(c, [236.0, 214.0, 190.0], glow * 0.35);
        }
        // A shelf with two plain boxes.
        if y > 0.30 && y < 0.33 && x > 0.55 && x < 0.92 {
            c = [70.0, 54.0, 44.0];
        }
        if y > 0.20 && y < 0.30 && ((x > 0.60 && x < 0.67) || (x > 0.78 && x < 0.86)) {
            c = if x < 0.7 { [96.0, 120.0, 112.0] } else { [176.0, 150.0, 104.0] };
        }
        // The desk.
        if y > 0.80 {
            c = mix([84.0, 62.0, 46.0], [58.0, 42.0, 32.0], (y - 0.8) / 0.2);
        }
        c
    }))
}

fn render(setup: impl FnOnce(&mut Ui), page: Page, width: u32, height: u32, zoom: f32) -> Vec<u8> {
    let (gpu, context, _) = device(true, true).unwrap();
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let (mut target, mut view) = (None, None);
    unsafe {
        gpu.CreateTexture2D(&desc, None, Some(&mut target)).unwrap();
        gpu.CreateRenderTargetView(target.as_ref().unwrap(), None, Some(&mut view)).unwrap();
    }
    let (target, view) = (target.unwrap(), view.unwrap());
    let ctx = egui::Context::default();
    configure(&ctx);
    ctx.set_zoom_factor(zoom);
    let mut app = Ui::default();
    app.switches.onboarded = true;
    app.page = page;
    setup(&mut app);
    let mut renderer = egui_directx11::Renderer::new(&gpu).unwrap();
    for _ in 0..3 {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width as f32 / zoom, height as f32 / zoom))),
                ..Default::default()
            },
            |ctx| app.show(ctx, egui::TextureId::User(1), "WARP"),
        );
        let (render, _, _) = egui_directx11::split_output(output);
        unsafe { context.ClearRenderTargetView(&view, &[0.0, 0.0, 0.0, 1.0]) };
        renderer.render(&context, &view, &ctx, render).unwrap();
    }
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe { gpu.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap() };
    let staging = staging.unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    unsafe {
        context.CopyResource(&staging, &target);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
        for row in 0..height as usize {
            out.extend_from_slice(std::slice::from_raw_parts((mapped.pData as *const u8).add(row * mapped.RowPitch as usize), width as usize * 4));
        }
        context.Unmap(&staging, 0);
    }
    out
}

/// Opaque RGB PNG with no text, time or colour-profile chunks.
fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) {
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
}

/// The sample world: this PC on air, a verified contact nearby, two
/// strangers, a second contact out of range, two streams.
fn sample(app: &mut Ui) {
    let (contact, other_contact, stranger, far) = (generated(3), generated(41), generated(17), generated(88));
    app.bluetooth = Some(meshlive::bluetooth::Capabilities {
        radio: meshlive::bluetooth::Radio::On,
        low_energy: Some(true),
        central: Some(true),
        peripheral: Some(true),
        extended_advertising: Some(true),
        max_advertisement_bytes: None,
        error: None,
    });
    app.peripheral_proof = meshlive::bluetooth::gate::PeripheralProof::Started;
    app.set_demo(true);
    std::thread::sleep(std::time::Duration::from_millis(700));
    app.pump();
    app.switches.receive_calls = true;
    app.pc_visible = true;
    app.own_generated = Some(generated(9));
    // A synthetic handle (fixed sample bytes, no device's ID).
    app.own_id = Some(meshlive::names::handle(&[0x3a, 0x91, 0x5c, 0x07, 0xe2, 0x64]));
    app.apply(Update::People(vec![
        Person { id: 10, name: contact.clone(), reach: Reach::Direct, verified: true, copied: false },
        Person { id: 11, name: stranger.clone(), reach: Reach::Direct, verified: false, copied: false },
        Person { id: 12, name: far.clone(), reach: Reach::Via(1), verified: false, copied: false },
    ]));
    app.apply(Update::Contacts(vec![
        Contact { id: 10, name: contact.clone(), by_qr: true, nearby: true, lease: true, blocked: false },
        Contact { id: 13, name: other_contact, by_qr: true, nearby: false, lease: true, blocked: false },
    ]));
    app.apply(Update::Streams(vec![
        StreamEntry { id: 1, from: contact, title: Some("Sunset over the lake".into()), audio_only: false, reach: Reach::Direct, paused: false, full: false, confirmed: true, unavailable: None },
        StreamEntry { id: 2, from: stranger, title: None, audio_only: false, reach: Reach::Via(1), paused: false, full: false, confirmed: false, unavailable: None },
        StreamEntry { id: 3, from: far, title: Some("Evening music".into()), audio_only: true, reach: Reach::Via(2), paused: false, full: false, confirmed: false, unavailable: None },
    ]));
}

#[test]
#[ignore = "writes store screenshots; run with MESH_SCREENSHOTS set"]
fn store_screenshots() {
    let Some(dir) = std::env::var_os("MESH_SCREENSHOTS").map(std::path::PathBuf::from) else { return };
    std::fs::create_dir_all(&dir).unwrap();
    let (w, h, zoom) = (1600u32, 1000u32, 1.25f32);
    type Setup = fn(&mut Ui);
    let shots: [(&str, Page, Setup); 7] = [
        ("1-calls", Page::Calls, sample),
        ("2-calls-status", Page::Calls, |a| {
            sample(a);
            a.status_open = Some(egui::Rect::from_min_size(egui::pos2(1160.0, 18.0), egui::vec2(100.0, 32.0)));
        }),
        ("3-video-call", Page::Calls, |a| {
            sample(a);
            a.place_call(10, false);
            for _ in 0..4 {
                std::thread::sleep(std::time::Duration::from_millis(700));
                a.pump();
            }
            let peer = std::sync::Arc::new(meshlive::video::PictureSlot::default());
            peer.publish(288, 352, lake_scene(288, 352), 0);
            a.apply(Update::CallPictures(PictureHandle(peer)));
            a.apply(Update::CallCamera(None));
            let own = std::sync::Arc::new(meshlive::video::PictureSlot::default());
            own.publish(320, 240, room_scene(320, 240), 0);
            a.apply(Update::CallSelfPictures(PictureHandle(own)));
        }),
        ("4-contacts", Page::Contacts, sample),
        ("5-live", Page::Home, sample),
        ("6-settings", Page::Settings, sample),
        ("7-go-live", Page::Home, |a| {
            sample(a);
            a.open_go_live();
        }),
    ];
    for (name, page, setup) in shots {
        let pixels = render(setup, page, w, h, zoom);
        write_png(&dir.join(format!("{name}.png")), w, h, &pixels);
    }
}
