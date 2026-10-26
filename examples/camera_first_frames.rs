//! Measures the first frames of this PC's first camera at the call size:
//! per frame, the time since open and the NV12 Y/U/V means, and whether
//! `LeadingBlankFrames` counts it as blank. Prints numbers only (no image).
//! Usage: camera_first_frames [frames=30] [width=352] [height=288]
use meshlive::call_camera::{LeadingBlankFrames, plane_means};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let frames = args.first().copied().unwrap_or(30);
    let (width, height) = (args.get(1).copied().unwrap_or(352), args.get(2).copied().unwrap_or(288));
    let opened = Instant::now();
    let mut camera = match meshlive::capture::Camera::open_first(width, height) {
        Ok(camera) => camera,
        Err(error) => {
            println!("camera_open_failed {error:?}");
            return;
        }
    };
    println!("camera_open ms={}", opened.elapsed().as_millis());
    let mut blank = 0;
    for n in 0..frames {
        match camera.next_frame(Instant::now() + Duration::from_secs(2)) {
            Ok(picture) => {
                let (y, u, v) = plane_means(&picture, width as usize, height as usize);
                let is_blank = LeadingBlankFrames::is_blank(y, u, v);
                blank += u32::from(is_blank);
                println!("frame={n} ms={} y={y:.1} u={u:.1} v={v:.1} blank={is_blank}", opened.elapsed().as_millis());
            }
            Err(error) => println!("frame={n} error={error:?}"),
        }
    }
    println!("blank_frames={blank} of {frames}");
}
