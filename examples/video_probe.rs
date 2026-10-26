//! Bounded externally by the caller; no camera, microphone, output or radio.
use meshlive::video::{DecodeDevice, device};
use windows::core::Interface;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--decoder-inventory"] {
        return Err(
            "Use --decoder-inventory to enumerate local Media Foundation candidates".into(),
        );
    }
    let (gpu, _, level) = device(false, false)?;
    let decoder = DecodeDevice::new(&gpu)?;
    assert_eq!(decoder.gpu()?.as_raw(), gpu.as_raw());
    let inventory = decoder.inventory()?;
    println!(
        "D3D11 feature level: {:#x}; manager shares the exact device",
        level.0
    );
    println!(
        "VP9 candidates: hardware={}, software={}",
        inventory.vp9.hardware, inventory.vp9.software
    );
    println!(
        "AV1 candidates: hardware={}, software={}",
        inventory.av1.hardware, inventory.av1.software
    );
    println!("Enumeration only: no decoding, camera, media playback or interoperability claim.");
    Ok(())
}
