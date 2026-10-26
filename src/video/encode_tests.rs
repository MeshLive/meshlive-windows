//! Generated-input encode checks: no camera, microphone, radio or phone. A
//! moving NV12 picture is encoded, the output is checked with the receiver's
//! own size guard (what the Windows receiver admits), keyframes/timestamps and
//! bitrate are measured, and the units are decoded back by the Media
//! Foundation decoder for a luma PSNR. Hardware encoders/decoders are used
//! when this PC has them; their absence is reported, never faked.
use super::{
    DecodeDevice, Decoder,
    bitstream::{SizeGuard, key_size},
    device,
    encode_aom::AomEncoder,
    encode_source::SourceEncoder,
    encoder::{Backend, EncodedUnit, EncoderSettings, VideoEncoder, availability, plan},
};
use crate::{
    fanout::{FanOut, Item},
    generated::{luma_psnr, pattern_nv12},
    media::{self, KEY_FRAME, Packet, VideoHeader},
};
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Graphics::{Direct3D11::*, Dxgi::Common::*},
        Media::MediaFoundation::*,
    },
    core::Interface,
};

/// A detailed moving picture (pseudo-random texture that scrolls, plus the
/// generated pattern's gradient and box), so CBR has something to spend bits
/// on. Deterministic per frame.
pub(super) fn textured_nv12(width: usize, height: usize, frame: u32) -> Vec<u8> {
    let mut out = pattern_nv12(width, height, frame);
    let shift = frame as usize * 3;
    for y in 0..height {
        for x in 0..width {
            let (u, v) = ((x + shift) as u32, (y + shift / 2) as u32);
            let mut h = u.wrapping_mul(0x9E37_79B1) ^ v.wrapping_mul(0x85EB_CA77);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            let noise = (h & 15) as i32 - 8;
            let value = out[y * width + x] as i32 + noise;
            out[y * width + x] = value.clamp(16, 235) as u8;
        }
    }
    out
}

/// AV1 OBU types in one temporal unit (low-overhead format, sizes present).
pub(super) fn obu_types(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let header = data[0];
        if header & 0x80 != 0 || header & 0x02 == 0 {
            return None;
        }
        let kind = (header >> 3) & 15;
        let mut at = 1 + usize::from(header & 0x04 != 0);
        let mut size = 0usize;
        for i in 0..8 {
            let byte = *data.get(at)?;
            at += 1;
            size |= usize::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                break;
            }
        }
        out.push(kind);
        data = data.get(at + size..)?;
    }
    Some(out)
}
const OBU_SEQUENCE_HEADER: u8 = 1;

fn read_luma(context: &ID3D11DeviceContext, sample: &IMFSample, crop: super::Crop) -> windows::core::Result<Vec<u8>> {
    let buffer: IMFDXGIBuffer = unsafe { sample.GetBufferByIndex(0)? }.cast()?;
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

/// Printed whole (Debug) as the test's measurement record.
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct Report {
    pub encoder: String,
    pub units: usize,
    pub keyframes: Vec<usize>,
    pub kbps: f64,
    pub steady_kbps: f64,
    pub decoded: usize,
    pub psnr_min: f64,
    pub psnr_mean: f64,
    pub encode_ms_per_frame: f64,
}

/// Decodes `units` with the MF decoder and returns per-picture luma PSNR
/// against `source(frame index)`; `None` when this PC has no decoder for it.
fn decode_psnr(
    owner: &DecodeDevice,
    context: &ID3D11DeviceContext,
    header: VideoHeader,
    units: &[EncodedUnit],
    source: &dyn Fn(u32) -> Vec<u8>,
) -> Option<Vec<f64>> {
    let mut decoder = match Decoder::open(owner, header) {
        Ok(d) => d,
        Err(e) => {
            println!("  no MF decoder for codec {}: {e:?}", header.codec);
            return None;
        }
    };
    let (w, h) = (header.width as usize, header.height as usize);
    let mut psnr = Vec::new();
    let take = |decoder: &mut Decoder<'_>, psnr: &mut Vec<f64>| {
        while let Some(frame) = decoder.pull().expect("decoder output") {
            let index = ((frame.timestamp_100ns / 10) * header.fps as i64 + 500_000) / 1_000_000;
            let luma = read_luma(context, frame.sample(), frame.format.crop).expect("readback");
            assert_eq!(luma.len(), w * h, "decoded picture keeps the coded size");
            psnr.push(luma_psnr(&luma, &source(index as u32)[..w * h]));
        }
    };
    for unit in units {
        let mut tries = 0;
        while !decoder.push(&unit.data, unit.pts_us, unit.key_frame).expect("decoder input") {
            take(&mut decoder, &mut psnr);
            tries += 1;
            assert!(tries < 100, "decoder never accepted input");
        }
        take(&mut decoder, &mut psnr);
    }
    decoder.drain().expect("drain");
    take(&mut decoder, &mut psnr);
    Some(psnr)
}

/// Encodes `frames` pictures through `SourceEncoder` with three coalesced
/// keyframe requests before frame `request_at`, then checks and decodes them.
pub(super) fn round_trip(
    owner: &DecodeDevice,
    context: &ID3D11DeviceContext,
    encoder: VideoEncoder<'_>,
    frames: u32,
    request_at: u32,
) -> Report {
    let settings = encoder.settings();
    let (w, h) = (settings.width as usize, settings.height as usize);
    round_trip_with(
        owner,
        context,
        encoder,
        frames,
        request_at,
        &mut |source, frame, pts, deadline| source.encode(&textured_nv12(w, h, frame), pts, deadline).expect("encode"),
        &|frame| textured_nv12(w, h, frame)[..w * h].to_vec(),
    )
}

/// Feeds each picture through `feed`; `reference(frame)` is the expected luma.
pub(super) type Feed<'f, 'a> = dyn FnMut(&mut SourceEncoder<'a>, u32, i64, Instant) -> Vec<Vec<u8>> + 'f;
pub(super) fn round_trip_with<'a>(
    owner: &DecodeDevice,
    context: &ID3D11DeviceContext,
    encoder: VideoEncoder<'a>,
    frames: u32,
    request_at: u32,
    feed: &mut Feed<'_, 'a>,
    reference: &dyn Fn(u32) -> Vec<u8>,
) -> Report {
    let settings = encoder.settings();
    let name = encoder.name();
    let mut source = SourceEncoder::new(encoder, 60);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut packets = Vec::new();
    let started = Instant::now();
    for frame in 0..frames {
        if frame == request_at {
            for _ in 0..3 {
                source.request_keyframe(1_000_000_000 * u64::from(frame), true);
            }
        }
        let pts = i64::from(frame) * 1_000_000 / i64::from(settings.fps);
        packets.extend(feed(&mut source, frame, pts, deadline));
    }
    packets.extend(source.finish(deadline).expect("finish"));
    let encode_ms_per_frame = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
    let units: Vec<EncodedUnit> = packets
        .iter()
        .map(|p| {
            let p = Packet::decode(p, media::MAX_PAYLOAD).unwrap();
            assert_eq!(p.flags & !KEY_FRAME, 0, "no discontinuity without a restart");
            EncodedUnit {
                data: p.payload.to_vec(),
                pts_us: p.pts_us,
                key_frame: p.flags & KEY_FRAME != 0,
            }
        })
        .collect();
    if std::env::var_os("MESH_ENCODE_TRACE").is_some() {
        for (i, u) in units.iter().enumerate() {
            println!("  {name} unit {i}: pts {} key {} bytes {} obus {:?}", u.pts_us, u.key_frame, u.data.len(), obu_types(&u.data));
        }
    }
    // One unit per picture, in order, with the source timestamps.
    assert_eq!(units.len(), frames as usize, "{name}: one unit per picture");
    for (i, unit) in units.iter().enumerate() {
        assert_eq!(unit.pts_us, i as i64 * 1_000_000 / i64::from(settings.fps), "{name}: timestamp {i}");
    }
    let keyframes: Vec<usize> = units.iter().enumerate().filter(|u| u.1.key_frame).map(|u| u.0).collect();
    assert_eq!(keyframes, vec![0, request_at as usize], "{name}: first frame + one coalesced request");
    assert_eq!((source.policy().honoured, source.policy().coalesced), (1, 2));
    // What the Windows receiver admits: every unit passes its size guard, keys carry the size.
    let header = VideoHeader {
        width: settings.width as u16,
        height: settings.height as u16,
        fps: settings.fps as u8,
        codec: settings.codec,
        quarter_turns: 0,
        opus: true,
    };
    let mut guard = SizeGuard::new(header);
    for (i, unit) in units.iter().enumerate() {
        guard = guard.inspect(&unit.data).unwrap_or_else(|e| panic!("{name}: unit {i} refused: {e:?}"));
        if unit.key_frame {
            assert_eq!(key_size(settings.codec, &unit.data, &[] as &[&[u8]]), Some((settings.width, settings.height)));
            if settings.codec == 2 {
                // One shown frame per unit: no superframe index (marker 0b110xxxxx closing the unit).
                assert_ne!(unit.data.last().map(|b| b & 0xe0), Some(0xc0), "{name}: no superframe at {i}");
            }
            if settings.codec == 3 {
                let types = obu_types(&unit.data).expect("AV1 low-overhead OBUs with sizes");
                assert!(types.contains(&OBU_SEQUENCE_HEADER), "{name}: in-band sequence header on keyframe {i}");
            }
        }
    }
    let bytes: usize = units.iter().map(|u| u.data.len()).sum();
    let kbps = bytes as f64 * 8.0 * f64::from(settings.fps) / f64::from(frames) / 1000.0;
    // Steady state: after the first second, excluding keyframes' own burst.
    let steady: Vec<&EncodedUnit> = units[settings.fps as usize..].iter().filter(|u| !u.key_frame).collect();
    let steady_kbps =
        steady.iter().map(|u| u.data.len()).sum::<usize>() as f64 * 8.0 * f64::from(settings.fps) / steady.len() as f64 / 1000.0;
    let psnr = decode_psnr(owner, context, header, &units, reference).unwrap_or_default();
    let psnr_min = psnr.iter().cloned().fold(f64::INFINITY, f64::min);
    let psnr_mean = psnr.iter().map(|p| p.min(99.0)).sum::<f64>() / psnr.len().max(1) as f64;
    Report {
        encoder: name,
        units: units.len(),
        keyframes,
        kbps,
        steady_kbps,
        decoded: psnr.len(),
        psnr_min,
        psnr_mean,
        encode_ms_per_frame,
    }
}

pub(super) fn settings(codec: u8, width: u32, height: u32, bitrate_kbps: u32) -> EncoderSettings {
    EncoderSettings {
        codec,
        width,
        height,
        fps: 15,
        bitrate_kbps,
        keyframe_interval_s: 60,
    }
}

pub(super) fn check(report: &Report, target_kbps: u32, decoded_required: bool) {
    println!("{report:?}");
    let target = f64::from(target_kbps);
    assert!(
        (0.6 * target..=1.25 * target).contains(&report.steady_kbps),
        "{}: steady {:.0} kbps outside 60-125% of {target_kbps}",
        report.encoder,
        report.steady_kbps
    );
    assert!(report.kbps <= 1.6 * target, "{}: total {:.0} kbps", report.encoder, report.kbps);
    if decoded_required || report.decoded > 0 {
        assert_eq!(report.decoded, report.units, "{}: every unit decodes", report.encoder);
        assert!(report.psnr_min > 30.0, "{}: luma PSNR min {:.1} dB", report.encoder, report.psnr_min);
    }
}

#[test]
fn software_av1_round_trip_at_android_call_and_stream_sizes() {
    let (gpu, context, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let av1_decoders = owner.inventory().unwrap().av1;
    for (w, h, kbps) in [(426, 240, 200), (640, 360, 400), (854, 480, 600)] {
        let encoder = VideoEncoder::Software(AomEncoder::open(settings(3, w, h, kbps)).unwrap());
        let report = round_trip(&owner, &context, encoder, 60, 37);
        check(&report, kbps, av1_decoders.hardware + av1_decoders.software > 0);
    }
}

#[test]
fn software_vp9_round_trip_at_android_call_and_stream_sizes() {
    let (gpu, context, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let vp9_decoders = owner.inventory().unwrap().vp9;
    for (w, h, kbps) in [(426, 240, 200), (640, 360, 400), (854, 480, 600)] {
        let encoder = VideoEncoder::open(&owner, settings(2, w, h, kbps), Backend::SoftwareVp9).unwrap();
        let report = round_trip(&owner, &context, encoder, 60, 37);
        check(&report, kbps, vp9_decoders.hardware + vp9_decoders.software > 0);
    }
}

#[test]
fn hardware_encoders_round_trip_when_this_pc_has_them() {
    let (gpu, context, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let available = availability().unwrap();
    println!("encoder availability: {available:?}");
    for (codec, names) in [(3u8, &available.av1_hardware), (2, &available.vp9_hardware)] {
        if names.is_empty() {
            println!("codec {codec}: no hardware encoder MFT on this PC");
            continue;
        }
        match VideoEncoder::open(&owner, settings(codec, 640, 360, 400), Backend::Hardware) {
            Ok(encoder) => {
                if let VideoEncoder::Hardware(e) = &encoder {
                    println!("{}: controls (HRESULT, 0 = accepted) {:?}", e.name(), e.controls());
                }
                check(&round_trip(&owner, &context, encoder, 60, 37), 400, true)
            }
            Err(e) => {
                println!("codec {codec}: registered hardware {names:?} cannot open: {e:?}; checking the call fallback");
                let mask = if codec == 3 { 2 } else { 1 };
                let (encoder, skipped) = VideoEncoder::open_planned(&owner, settings(codec, 640, 360, 400), &plan(mask, &available)).unwrap();
                assert!(skipped.iter().any(|&(failed_codec, backend, _)| failed_codec == codec && backend == Backend::Hardware));
                assert_eq!(encoder.settings().codec, codec, "fallback must preserve the peer's only decodable codec");
                assert_eq!(encoder.backend(), if codec == 3 { Backend::SoftwareAv1 } else { Backend::SoftwareVp9 });
                check(&round_trip(&owner, &context, encoder, 60, 37), 400, true);
            }
        }
    }
}

#[test]
fn plan_follows_android_order_and_the_peer_decode_mask() {
    let mut available = availability().unwrap();
    available.av1_hardware = vec!["hw av1".into()];
    available.vp9_hardware = vec!["hw vp9".into()];
    assert_eq!(
        plan(3, &available),
        vec![(3, Backend::Hardware), (2, Backend::Hardware), (3, Backend::SoftwareAv1), (2, Backend::SoftwareVp9)]
    );
    assert_eq!(plan(1, &available), vec![(2, Backend::Hardware), (2, Backend::SoftwareVp9)]);
    assert_eq!(plan(2, &available), vec![(3, Backend::Hardware), (3, Backend::SoftwareAv1)]);
    available.av1_hardware.clear();
    available.vp9_hardware.clear();
    assert_eq!(plan(3, &available), vec![(3, Backend::SoftwareAv1), (2, Backend::SoftwareVp9)]);
    assert_eq!(plan(1, &available), vec![(2, Backend::SoftwareVp9)], "a VP9-only peer gets software VP9");
    assert!(plan(0, &available).is_empty());
    assert!(available.av1_software.starts_with("v3.14.1"), "{}", available.av1_software);
    assert!(available.vp9_software.starts_with("v1.17.0"), "{}", available.vp9_software);
}

#[test]
fn encode_once_fans_the_same_bytes_to_every_viewer() {
    let (gpu, _, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let s = settings(3, 426, 240, 200);
    let mut source = SourceEncoder::new(VideoEncoder::open(&owner, s, Backend::SoftwareAv1).unwrap(), 60);
    let mut fan = FanOut::default();
    for child in 0..3u8 {
        fan.add_child(child, 0);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    for frame in 0..20u32 {
        if frame == 10 {
            // A fourth viewer joins: one keyframe for everyone, not a second encode.
            fan.add_child(3, 0);
            source.request_keyframe(10, true);
        }
        let pts = i64::from(frame) * 1_000_000 / 15;
        for packet in source.encode(&textured_nv12(426, 240, frame), pts, deadline).unwrap() {
            fan.publish(Item::media(packet).unwrap(), 0);
        }
    }
    let stats = source.stats();
    assert_eq!((stats.frames_in, stats.units_out, stats.keyframes), (20, 20, 2), "encoded once per picture");
    let mut queues: Vec<Vec<Item>> = Vec::new();
    for child in 0..4u8 {
        let queue = fan.queue(&child).unwrap();
        let mut items = Vec::new();
        while let Some(item) = queue.take() {
            items.push(item);
        }
        queues.push(items);
    }
    for child in 1..3 {
        assert_eq!(queues[child].len(), 20);
        for (a, b) in queues[0].iter().zip(&queues[child]) {
            assert!(std::sync::Arc::ptr_eq(&a.bytes, &b.bytes), "shared, not copied");
        }
    }
    // The joiner starts at the coalesced keyframe with the same bytes.
    assert_eq!(queues[3].len(), 10);
    assert!(std::sync::Arc::ptr_eq(&queues[3][0].bytes, &queues[0][10].bytes));
}

#[test]
fn a_size_changing_restart_marks_its_first_keyframe_discontinuity() {
    let (gpu, _, _) = device(false, false).unwrap();
    let owner = DecodeDevice::new(&gpu).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let open = |w, h| VideoEncoder::open(&owner, settings(3, w, h, 300), Backend::SoftwareAv1).unwrap();
    let mut source = SourceEncoder::new(open(640, 360), 60);
    let flags = |packets: Vec<Vec<u8>>| -> Vec<u8> {
        packets.iter().map(|p| Packet::decode(p, media::MAX_PAYLOAD).unwrap().flags).collect()
    };
    assert_eq!(flags(source.encode(&textured_nv12(640, 360, 0), 0, deadline).unwrap()), vec![KEY_FRAME]);
    assert_eq!(flags(source.encode(&textured_nv12(640, 360, 1), 66_666, deadline).unwrap()), vec![0]);
    // Down a rung: the new size's first keyframe is a discontinuity.
    source.restart(open(426, 240), deadline).unwrap();
    let first = source.encode(&textured_nv12(426, 240, 2), 133_333, deadline).unwrap();
    assert_eq!(flags(first.clone()), vec![KEY_FRAME | media::DISCONTINUITY]);
    let payload = Packet::decode(&first[0], media::MAX_PAYLOAD).unwrap().payload.to_vec();
    assert_eq!(key_size(3, &payload, &[] as &[&[u8]]), Some((426, 240)));
    assert_eq!(flags(source.encode(&textured_nv12(426, 240, 3), 200_000, deadline).unwrap()), vec![0]);
    // Same size (camera flip): keyframe, not marked.
    source.restart(open(426, 240), deadline).unwrap();
    assert_eq!(flags(source.encode(&textured_nv12(426, 240, 4), 266_666, deadline).unwrap()), vec![KEY_FRAME]);
}
