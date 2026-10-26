//! Explicit generated-input encode check (`--generated`): no camera, no
//! microphone, no audio output, no radio. A moving 640x360@15 NV12 test pattern
//! is encoded once by each local AV1/VP9 encoder (hardware MFTs and the pinned
//! software libaom AV1 / libvpx VP9) and a quiet tone by
//! libopus; the packets are fanned out to two in-memory viewers (sealed per
//! child), opened by the watch reader, decoded back by Media Foundation and
//! libopus, and compared with the generated source (luma PSNR, tone SNR).
//! CPU readback is diagnostic only.
use meshlive::{
    crypto,
    fanout::{FanOut, Item},
    framing::HopReader,
    generated::{luma_psnr, pattern_nv12, tone_20ms},
    listen::AudioReceiver,
    media::{AUDIO, CONFIG, KEY_FRAME, Packet, VideoHeader},
    opus,
    playout::Profile,
    records::{Layer, Opener, Policy},
    session::SourceLink,
    video::*,
    viewer::VideoReceiver,
    watch::{Event, Reader},
};
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Graphics::{Direct3D11::*, Dxgi::Common::*},
        Media::MediaFoundation::*,
    },
    core::Interface,
};

const W: u32 = 640;
const H: u32 = 360;
const FPS: u32 = 15;
const FRAMES: u32 = 30;

fn read_luma(context: &ID3D11DeviceContext, frame: &DecodedFrame<'_>) -> windows::core::Result<Vec<u8>> {
    let buffer: IMFDXGIBuffer = unsafe { frame.sample().GetBufferByIndex(0)? }.cast()?;
    let mut pointer = std::ptr::null_mut();
    let (source, slice) = unsafe {
        buffer.GetResource(&ID3D11Texture2D::IID, &mut pointer)?;
        (ID3D11Texture2D::from_raw(pointer), buffer.GetSubresourceIndex()?)
    };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    assert_eq!(desc.Format, DXGI_FORMAT_NV12);
    desc.ArraySize = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    let gpu = unsafe { source.GetDevice()? };
    unsafe { gpu.CreateTexture2D(&desc, None, Some(&mut staging))? };
    let staging = staging.unwrap();
    let crop = frame.format.crop;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let mut out = Vec::with_capacity((crop.width * crop.height) as usize);
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &source, slice, None);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let p = mapped.pData as *const u8;
        for y in crop.top..crop.top + crop.height {
            out.extend_from_slice(std::slice::from_raw_parts(
                p.add(y as usize * mapped.RowPitch as usize + crop.left as usize),
                crop.width as usize,
            ));
        }
        context.Unmap(&staging, 0);
    }
    Ok(out)
}

fn packet(flags: u8, pts_us: i64, payload: &[u8]) -> Vec<u8> {
    Packet { flags, pts_us, payload }.encode().unwrap()
}

fn run(codec: u8, backend: Backend, owner: &DecodeDevice, context: &ID3D11DeviceContext) -> Result<(), Box<dyn std::error::Error>> {
    let name = match (codec, backend) {
        (3, Backend::Hardware) => "av1 hardware",
        (3, _) => "av1 software",
        (_, Backend::Hardware) => "vp9 hardware",
        _ => "vp9 software",
    };
    let settings = EncoderSettings {
        codec,
        width: W,
        height: H,
        fps: FPS,
        bitrate_kbps: 400,
        keyframe_interval_s: 60,
    };
    let mut encoder = match VideoEncoder::open(owner, settings, backend) {
        Ok(e) => e,
        Err(e) => {
            println!("{name}: no usable encoder ({e:?})");
            return Ok(());
        }
    };
    println!("{name}: encoder {}", encoder.name());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut units = Vec::new();
    for frame in 0..FRAMES {
        let pts = frame as i64 * 1_000_000 / FPS as i64;
        units.extend(encoder.encode(&pattern_nv12(W as usize, H as usize, frame), pts, frame == 0, deadline)?);
    }
    units.extend(encoder.finish(deadline)?);
    let bytes: usize = units.iter().map(|u| u.data.len()).sum();
    println!(
        "{name}: {} units, {} keyframes, {} bytes ({} kbps)",
        units.len(),
        units.iter().filter(|u| u.key_frame).count(),
        bytes,
        bytes * 8 * FPS as usize / FRAMES as usize / 1000
    );
    // Encode once, fan out to two viewers; each child has its own keys.
    let header = VideoHeader { width: W as u16, height: H as u16, fps: FPS as u8, codec, quarter_turns: 0, opus: true };
    let mut fan = FanOut::default();
    fan.set_preamble(vec![Item::header(header.encode()?.to_vec())]);
    let mut opus_encoder = opus::Encoder::new()?;
    let mut links = Vec::new();
    let mut readers = Vec::new();
    for child in 0..2u8 {
        let (link, endpoint) = (crypto::random_nonce()?, crypto::random_nonce()?);
        links.push(SourceLink::new(&link, &endpoint)?);
        readers.push(Reader::direct(
            HopReader::new(&link, Policy::default())?,
            Opener::new(Layer::Endpoint, &endpoint, Policy::default())?,
            [9; 16],
            false,
            true,
        ));
        fan.add_child(child, 0);
    }
    fan.publish(Item::media(packet(AUDIO | CONFIG, 0, &opus_encoder.config())).unwrap(), 0);
    let mut audio_frame = 0;
    for unit in &units {
        // Audio up to this video time first (20 ms units), then the picture.
        while (audio_frame as i64) * 20_000 <= unit.pts_us {
            let opus = opus_encoder.encode(&tone_20ms(audio_frame, 440.0, 330.0))?;
            fan.publish(Item::media(packet(AUDIO, audio_frame as i64 * 20_000, &opus)).unwrap(), 0);
            audio_frame += 1;
        }
        let flags = if unit.key_frame { KEY_FRAME } else { 0 };
        fan.publish(Item::media(packet(flags, unit.pts_us, &unit.data)).unwrap(), 0);
    }
    let mut results = Vec::new();
    for child in 0..2u8 {
        let mut wire = Vec::new();
        let queue = fan.queue(&child).unwrap();
        while let Some(item) = queue.take() {
            wire.extend(links[child as usize].seal(&item)?);
        }
        let mut clock = Profile::Auto.clock();
        let mut video = VideoReceiver::new(header);
        let mut audio = AudioReceiver::<opus::Decoder>::default();
        let mut input = wire.as_slice();
        while !input.is_empty() {
            let Some(event) = readers[child as usize].read(&mut input, 0).map_err(|e| format!("{e:?}"))? else { break };
            if let Event::Media(owned) = event {
                let p = owned.packet();
                let now = p.pts_us * 1_000;
                if p.flags & AUDIO != 0 {
                    audio
                        .on_packet(p, now, &mut clock, |c| opus::Decoder::new(c, opus::Profile::StreamOrCall))
                        .map_err(|e| format!("{e:?}"))?;
                } else {
                    video.on_packet(p, now, &mut clock).map_err(|e| format!("{e:?}"))?;
                }
            }
        }
        let presented = std::cell::RefCell::new(Vec::new());
        let mut frames = MfFrames::new(owner, header, |pts, frame: &DecodedFrame<'_>| {
            let index = ((pts * FPS as i64 + 500_000) / 1_000_000) as u32; // round: pts are truncated microseconds
            let source = pattern_nv12(W as usize, H as usize, index);
            let psnr = read_luma(context, frame)
                .map(|luma| luma_psnr(&luma, &source[..(W * H) as usize]))
                .unwrap_or(0.0);
            if std::env::var_os("MESH_PROBE_TRACE").is_some() {
                println!("  picture pts {pts} -> source {index}: {psnr:.1} dB");
            }
            presented.borrow_mut().push((pts, psnr));
        });
        let mut now = 0i64;
        while now < 10_000_000_000 && presented.borrow().len() < units.len() {
            video.feed_due(&mut frames, now, &clock, 20_000_000).map_err(|e| format!("{e:?}"))?;
            video.drain(&mut frames, now, &clock).map_err(|e| format!("{e:?}"))?;
            now += 10_000_000;
        }
        drop(frames);
        let presented = presented.into_inner();
        let mut pcm = [0i16; opus::Decoder::MAX_SAMPLES];
        let mut heard = Vec::new();
        audio.pump(i64::MAX / 2, 0, &mut pcm, |_, s| heard.extend_from_slice(s)).map_err(|e| format!("{e:?}"))?;
        let min = presented.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let mean = presented.iter().map(|p| p.1.min(99.0)).sum::<f64>() / presented.len().max(1) as f64;
        println!(
            "{name} viewer {child}: {} pictures decoded, luma PSNR min {min:.1} dB mean {mean:.1} dB; {} audio samples",
            presented.len(),
            heard.len()
        );
        results.push((presented, heard));
    }
    assert_eq!(results[0].0.len(), units.len(), "every encoded picture decodes");
    assert!(results[0].0.iter().all(|p| p.1 >= 30.0), "PSNR below 30 dB");
    assert_eq!(results[0], results[1], "both viewers decode identical media");
    assert!(results[0].1.len() >= 960 * 20, "tone decoded");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--generated"] {
        return Err("Use --generated for an explicit local encode of generated input".into());
    }
    println!("encoder availability: {:?}", encoder_availability()?);
    let (gpu, context, _) = device(false, false)?;
    let owner = DecodeDevice::new(&gpu)?;
    for (codec, backend) in [(3, Backend::Hardware), (3, Backend::SoftwareAv1), (2, Backend::Hardware), (2, Backend::SoftwareVp9)] {
        run(codec, backend, &owner, &context)?;
    }
    println!("No camera, microphone, audio output or radio was used.");
    Ok(())
}
