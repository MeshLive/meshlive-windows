//! Explicit generated-fixture decoder check. CPU readback here is diagnostic
//! only, never part of production presentation. No capture/output/radio opened.
use meshlive::{crypto::sha256, media::VideoHeader, video::*};
use windows::{
    Win32::{
        Graphics::{Direct3D11::*, Dxgi::Common::*},
        Media::MediaFoundation::*,
    },
    core::Interface,
};

fn read_i420(
    context: &ID3D11DeviceContext,
    frame: &DecodedFrame<'_>,
) -> windows::core::Result<Vec<u8>> {
    let buffer: IMFDXGIBuffer = unsafe { frame.sample().GetBufferByIndex(0)? }.cast()?;
    let mut pointer = std::ptr::null_mut();
    let (source, slice) = unsafe {
        buffer.GetResource(&ID3D11Texture2D::IID, &mut pointer)?;
        (
            ID3D11Texture2D::from_raw(pointer),
            buffer.GetSubresourceIndex()?,
        )
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        source.GetDesc(&mut desc);
    }
    assert_eq!(desc.Format, DXGI_FORMAT_NV12);
    desc.ArraySize = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    let gpu = unsafe { source.GetDevice()? };
    unsafe {
        gpu.CreateTexture2D(&desc, None, Some(&mut staging))?;
    }
    let staging = staging.unwrap();
    let crop = frame.format.crop;
    assert_eq!(
        [crop.left, crop.top, crop.width, crop.height],
        [0, 0, 64, 64]
    );
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let mut result = Vec::with_capacity(6144);
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &source, slice, None);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let p = mapped.pData as *const u8;
        for y in 0..64 {
            result.extend_from_slice(std::slice::from_raw_parts(
                p.add(y * mapped.RowPitch as usize),
                64,
            ));
        }
        for component in 0..2 {
            for y in 0..32 {
                for x in 0..32 {
                    result.push(*p.add(
                        (desc.Height as usize + y) * mapped.RowPitch as usize + x * 2 + component,
                    ));
                }
            }
        }
        context.Unmap(&staging, 0);
    }
    Ok(result)
}

fn verify_rgba(
    context: &ID3D11DeviceContext,
    view: &ID3D11ShaderResourceView,
    i420: &[u8],
    location: ChromaLocation,
) -> windows::core::Result<u8> {
    let texture: ID3D11Texture2D = unsafe { view.GetResource()? }.cast()?;
    let gpu = unsafe { texture.GetDevice()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }
    assert_eq!([desc.Width, desc.Height], [64, 64]);
    assert_eq!(desc.Format, DXGI_FORMAT_R8G8B8A8_UNORM);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe {
        gpu.CreateTexture2D(&desc, None, Some(&mut staging))?;
    }
    let staging = staging.unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let mut rgba = Vec::with_capacity(64 * 64 * 4);
    unsafe {
        context.CopyResource(&staging, &texture);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        for y in 0..64 {
            rgba.extend_from_slice(std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                64 * 4,
            ));
        }
        context.Unmap(&staging, 0);
    }
    // Independent double-precision BT.709 limited-range reference with explicit
    // bilinear chroma coordinates; coefficients are not imported from the GPU.
    let origin = match location {
        ChromaLocation::Left => (0.0, 0.5),
        ChromaLocation::Center => (0.5, 0.5),
        ChromaLocation::TopLeft => (0.0, 0.0),
    };
    let chroma = |plane: usize, x: f64, y: f64| {
        let x = x.clamp(0.0, 31.0);
        let y = y.clamp(0.0, 31.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(31), (y0 + 1).min(31));
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let at = |x, y| i420[4096 + plane * 1024 + y * 32 + x] as f64;
        (at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx) * (1.0 - fy)
            + (at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx) * fy
    };
    let mut maximum = 0;
    for y in 0..64 {
        for x in 0..64 {
            let u = chroma(0, (x as f64 - origin.0) * 0.5, (y as f64 - origin.1) * 0.5) - 128.0;
            let v = chroma(1, (x as f64 - origin.0) * 0.5, (y as f64 - origin.1) * 0.5) - 128.0;
            let luma = (i420[y * 64 + x] as f64 - 16.0) * 1.1643835616;
            let expected = [
                luma + 1.7927410714 * v,
                luma - 0.2132486143 * u - 0.5329093286 * v,
                luma + 2.1124017857 * u,
            ];
            let pixel = &rgba[(y * 64 + x) * 4..(y * 64 + x + 1) * 4];
            assert_eq!(pixel[3], 255);
            for channel in 0..3 {
                maximum = maximum.max(
                    pixel[channel].abs_diff(expected[channel].round().clamp(0.0, 255.0) as u8),
                );
            }
        }
    }
    assert!(
        maximum <= 2,
        "Decoded RGBA differs from independent reference by {maximum}"
    );
    Ok(maximum)
}

/// Frozen fixtures through the receive path (shared clock, format tracker,
/// time-gated feed) into the native decoder; each presented picture is hashed.
fn pipeline() -> Result<(), Box<dyn std::error::Error>> {
    use meshlive::{
        media::{KEY_FRAME, Packet},
        playout::Profile,
        video::MfFrames,
        viewer::VideoReceiver,
    };
    let (gpu, context, _) = device(false, false)?;
    let owner = DecodeDevice::new(&gpu)?;
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/video-lossless.json"))?;
    for (name, codec, data) in [
        (
            "vp9",
            2,
            include_bytes!("../tests/fixtures/video-vp9.ivf").as_slice(),
        ),
        (
            "av1",
            3,
            include_bytes!("../tests/fixtures/video-av1.ivf").as_slice(),
        ),
    ] {
        let header = VideoHeader {
            width: 128,
            height: 128,
            fps: 5,
            codec,
            quarter_turns: 0,
            opus: false,
        };
        let mut clock = Profile::Auto.clock();
        let mut receiver = VideoReceiver::new(header);
        let presented = std::cell::RefCell::new(Vec::new());
        let mut frames = MfFrames::new(&owner, header, |pts, frame: &DecodedFrame<'_>| {
            let hash = read_i420(&context, frame)
                .ok()
                .and_then(|raw| sha256(&raw).ok())
                .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .unwrap_or_default();
            presented.borrow_mut().push((pts, hash));
        });
        let mut position = 32;
        for index in 0..3 {
            let size = u32::from_le_bytes(data[position..position + 4].try_into()?) as usize;
            let ticks = u64::from_le_bytes(data[position + 4..position + 12].try_into()?);
            position += 12;
            let pts_us = ticks as i64 * 200_000;
            let packet = Packet {
                flags: if index == 0 { KEY_FRAME } else { 0 },
                pts_us,
                payload: &data[position..position + size],
            };
            // Arrival exactly on the source cadence: target = arrival + 250 ms.
            receiver
                .on_packet(packet, pts_us * 1_000, &mut clock)
                .map_err(|e| format!("{e:?}"))?;
            position += size;
        }
        let mut now = 0_i64;
        while presented.borrow().len() < 3 && now < 5_000_000_000 {
            receiver
                .feed_due(&mut frames, now, &clock, 20_000_000)
                .map_err(|e| format!("{e:?}"))?;
            receiver
                .drain(&mut frames, now, &clock)
                .map_err(|e| format!("{e:?}"))?;
            now += 10_000_000;
        }
        drop(frames);
        let presented = presented.into_inner();
        for (index, (pts, hash)) in presented.iter().enumerate() {
            let expected = &manifest["frames"][index];
            assert_eq!(*pts, expected["pts_us"].as_i64().unwrap());
            assert_eq!(hash, expected["i420_sha256"].as_str().unwrap());
        }
        assert_eq!(presented.len(), 3, "{name}");
        println!(
            "{name}: three frames through the receive path, exact hashes, presented by {} ms",
            now / 1_000_000
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["--pipeline"] {
        return pipeline();
    }
    if args != ["--decode-fixtures"] && args != ["--decode-fixtures", "--warp"] {
        return Err(
            "Use --decode-fixtures [--warp] for explicit local native decoding of generated frames"
                .into(),
        );
    }
    let warp = args.len() == 2;
    let (gpu, context, level) = device(warp, false)?;
    let owner = DecodeDevice::new(&gpu)?;
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/video-lossless.json"))?;
    for (name, codec, data) in [
        (
            "vp9",
            2,
            include_bytes!("../tests/fixtures/video-vp9.ivf").as_slice(),
        ),
        (
            "av1",
            3,
            include_bytes!("../tests/fixtures/video-av1.ivf").as_slice(),
        ),
    ] {
        // Header is a maximum picture size, deliberately larger than these frames.
        let header = VideoHeader {
            width: 128,
            height: 128,
            fps: 5,
            codec,
            quarter_turns: 0,
            opus: false,
        };
        let mut decoder = Decoder::open(&owner, header)?;
        println!("{name} codec control HRESULTs: {:?}", decoder.controls());
        let mut converter = Nv12Converter::new(&gpu)?;
        let mut count = 0;
        let mut verify = |frame: DecodedFrame<'_>| -> Result<(), Box<dyn std::error::Error>> {
            let expected = &manifest["frames"][count];
            assert_eq!(
                frame.timestamp_100ns,
                expected["pts_us"].as_i64().unwrap() * 10
            );
            let _ = frame.nv12()?; // Validates actual decoded surface/device/crop.
            let raw = read_i420(&context, &frame)?;
            let hash = sha256(&raw)?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            assert_eq!(hash, expected["i420_sha256"].as_str().unwrap());
            // Fixture metadata is known independently. Unknown native metadata
            // is printed rather than invented by the decoder component.
            assert!(
                frame.format.colour.is_none() || matches!(frame.format.colour, Some(Colour::Bt709))
            );
            assert!(
                frame.format.range.is_none() || matches!(frame.format.range, Some(Range::Limited))
            );
            let location = frame.format.chroma.unwrap_or(ChromaLocation::Left);
            let view =
                converter.convert(&frame.nv12()?, Colour::Bt709, Range::Limited, location)?;
            let rgba_error = verify_rgba(&context, view, &raw, location)?;
            println!(
                "{name} frame {count}: exact I420 hash={hash}, RGBA max error={rgba_error}, time={}, format={:?}",
                frame.timestamp_100ns, frame.format
            );
            count += 1;
            Ok(())
        };
        let mut position = 32;
        for index in 0..3 {
            let size =
                u32::from_le_bytes(data[position..position + 4].try_into().unwrap()) as usize;
            let ticks = u64::from_le_bytes(data[position + 4..position + 12].try_into().unwrap());
            position += 12;
            let packet = &data[position..position + size];
            assert!(decoder.push(packet, ticks as i64 * 200000, index == 0)?);
            assert!(!decoder.push(packet, ticks as i64 * 200000, index == 0)?);
            println!("{name} input {index} accepted; draining available output");
            let mut delivered = 0;
            while let Some(frame) = decoder.pull()? {
                verify(frame)?;
                delivered += 1;
            }
            println!(
                "{name} input {index}: {delivered} frames available before the next input/EOS"
            );
            position += size;
        }
        decoder.drain()?;
        while let Some(frame) = decoder.pull()? {
            verify(frame)?;
        }
        drop(verify);
        assert_eq!(count, 3);
        // Reuse after a flush must start from the original fixture keyframe.
        decoder.flush()?;
        let size = u32::from_le_bytes(data[32..36].try_into().unwrap()) as usize;
        assert!(decoder.push(&data[44..44 + size], 0, true)?);
        // A decoder may delay output; finite-input recovery must drain it too.
        decoder.drain()?;
        let frame = decoder.pull()?.ok_or("No recovery frame after flush")?;
        assert_eq!(
            sha256(&read_i420(&context, &frame)?)?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            manifest["frames"][0]["i420_sha256"].as_str().unwrap()
        );
        drop(frame);
        while decoder.pull()?.is_some() {}
        // This input fits the compressed-byte cap but has no valid codec
        // header. It must fail in preflight, then leave the decoder closed.
        let mut rejected = Decoder::open(&owner, header)?;
        assert!(matches!(
            rejected.push(&[0], 0, false),
            Err(DecodeError::Bitstream(_))
        ));
        assert!(matches!(
            rejected.push(&data[44..44 + size], 0, true),
            Err(DecodeError::Closed)
        ));
        println!("{name}: malformed bitstream rejected by preflight; subsequent input is closed.");
        decoder.flush()?;
        assert!(decoder.push(&[], 0, true).is_err());
        assert!(matches!(
            decoder.push(&data[44..44 + size], 0, true),
            Err(DecodeError::Closed)
        ));
        println!(
            "{name}: three exact decoded frames, same-device conversion, drain/flush/recovery and terminal input rejection passed."
        );
    }
    println!(
        "warp={warp}, feature level={:#x}. No hardware-acceleration, playback-timing, camera, radio or interoperability claim.",
        level.0
    );
    Ok(())
}
