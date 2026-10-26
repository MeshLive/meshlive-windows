//! Every hardware encoder MFT on this PC by forced selection, each on a D3D11
//! device of its own adapter, through both inputs: system-memory NV12 and GPU
//! textures (RGBA -> NV12 on the GPU, then straight to the encoder). Same
//! decode-back, PSNR, bitrate, keyframe and timestamp checks as the other
//! encode tests. Generated input only.
use super::{
    DecodeDevice,
    encode_d3d::{RgbaToNv12, device_on, nv12_to_rgba, read_nv12, rgba_luma, upload_rgba},
    encode_tests::{check, round_trip, round_trip_with, settings, textured_nv12},
    encoder::{Encoder, VideoEncoder, hardware_candidates},
};
use crate::generated::luma_psnr;

const W: u32 = 640;
const H: u32 = 360;

fn rgba_frame(frame: u32) -> Vec<u8> {
    nv12_to_rgba(&textured_nv12(W as usize, H as usize, frame), W as usize, H as usize)
}

#[test]
fn the_gpu_converter_reproduces_bt601_limited_luma() {
    for (luid, name) in super::encode_d3d::adapters().unwrap() {
        let (gpu, _) = device_on(luid).unwrap();
        let mut converter = RgbaToNv12::new(&gpu, W, H, 15, 2).unwrap();
        let rgba = rgba_frame(3);
        let nv12 = converter.convert(&upload_rgba(&gpu, W, H, &rgba).unwrap()).unwrap();
        let read = read_nv12(&gpu, &nv12, 0, &settings(3, W, H, 400)).unwrap();
        let psnr = luma_psnr(&read[..(W * H) as usize], &rgba_luma(&rgba));
        println!("{name}: converter luma PSNR {psnr:.1} dB");
        assert!(psnr > 40.0, "{name}: GPU RGBA->NV12 luma PSNR {psnr:.1} dB");
    }
}

#[test]
fn every_hardware_encoder_round_trips_by_forced_selection_with_both_inputs() {
    let mut exercised = 0;
    let mut failed = Vec::new();
    for codec in [3u8, 2] {
        for candidate in hardware_candidates(codec).unwrap() {
            let label = format!("codec {codec} {} #{} on {}", candidate.name, candidate.index, candidate.adapter);
            let (gpu, context) = device_on(candidate.adapter_luid).unwrap();
            let owner = DecodeDevice::new(&gpu).unwrap();
            let open = || Encoder::open_candidate(&owner, settings(codec, W, H, 400), &candidate);
            // System-memory NV12.
            let encoder = match open() {
                Ok(encoder) => encoder,
                Err(e) => {
                    // Registered is not capable: reported, and counted below.
                    println!("{label}: OPEN FAILED {e:?} (with and without the D3D manager)");
                    failed.push(label);
                    continue;
                }
            };
            let texture_input = encoder.texture_input();
            println!("{label}: controls {:?}, texture input {texture_input}", encoder.controls());
            let report = round_trip(&owner, &context, VideoEncoder::Hardware(encoder), 60, 37);
            check(&report, 400, true);
            // GPU textures: RGBA -> NV12 on this adapter, no CPU copy into the encoder.
            let encoder = VideoEncoder::Hardware(open().unwrap());
            let mut converter = RgbaToNv12::new(&gpu, W, H, 15, 8).unwrap();
            let report = round_trip_with(
                &owner,
                &context,
                encoder,
                60,
                37,
                &mut |source, frame, pts, deadline| {
                    let rgba = upload_rgba(&gpu, W, H, &rgba_frame(frame)).unwrap();
                    let nv12 = converter.convert(&rgba).unwrap();
                    source.encode_texture(&gpu, &nv12, pts, deadline).expect("encode texture")
                },
                &|frame| rgba_luma(&rgba_frame(frame)),
            );
            println!("{label} texture input ({}):", if texture_input { "D3D11" } else { "readback fallback" });
            check(&report, 400, true);
            exercised += 1;
        }
    }
    println!("hardware encoders exercised: {exercised}; failed to open: {failed:?}");
    assert!(exercised > 0 || hardware_candidates(3).unwrap().is_empty() && hardware_candidates(2).unwrap().is_empty());
}
