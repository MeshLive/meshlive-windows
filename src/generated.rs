//! Generated, license-clean test inputs: a moving NV12 test pattern and a quiet
//! PCM tone. Deterministic per frame index, so encoded output can be decoded
//! back and compared (PSNR) without any camera or microphone.

/// NV12 picture: full-resolution Y plane, then interleaved half-resolution UV.
pub fn pattern_nv12(width: usize, height: usize, frame: u32) -> Vec<u8> {
    let mut out = vec![0; width * height * 3 / 2];
    let shift = (frame as usize * 4) % width.max(1);
    let (bx, by) = (
        (frame as usize * 7) % width.saturating_sub(64).max(1),
        (frame as usize * 3) % height.saturating_sub(64).max(1),
    );
    for y in 0..height {
        for x in 0..width {
            // A diagonal gradient that scrolls, with a bright moving box.
            let mut luma = (((x + shift) * 255 / width.max(1)) + y * 64 / height.max(1)) as u8;
            if (bx..bx + 64).contains(&x) && (by..by + 64).contains(&y) {
                luma = 235;
            }
            out[y * width + x] = luma.clamp(16, 235);
        }
    }
    let chroma = &mut out[width * height..];
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            chroma[y * width + 2 * x] = (64 + (x * 128 / (width / 2).max(1))) as u8;
            chroma[y * width + 2 * x + 1] = (192 - (y * 128 / (height / 2).max(1))) as u8;
        }
    }
    out
}

/// PSNR of the Y planes (dB); identical planes give `f64::INFINITY`.
pub fn luma_psnr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mse = a[..n]
        .iter()
        .zip(&b[..n])
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum::<f64>()
        / n as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// One 20 ms mono 48 kHz frame of a quiet sine (amplitude in sample units;
/// about 330 is -40 dBFS). Amplitude 0 is silence.
pub fn tone_20ms(frame: u32, hz: f64, amplitude: f64) -> [i16; 960] {
    let mut out = [0; 960];
    for (i, sample) in out.iter_mut().enumerate() {
        let t = (u64::from(frame) * 960 + i as u64) as f64 / 48_000.0;
        *sample = ((2.0 * std::f64::consts::PI * hz * t).sin() * amplitude) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_pattern_moves_is_deterministic_and_psnr_measures_it() {
        let a = pattern_nv12(640, 360, 0);
        assert_eq!(a.len(), 640 * 360 * 3 / 2);
        assert_eq!(a, pattern_nv12(640, 360, 0));
        let b = pattern_nv12(640, 360, 1);
        assert_ne!(a, b);
        assert_eq!(luma_psnr(&a, &a), f64::INFINITY);
        assert!(luma_psnr(&a[..640 * 360], &b[..640 * 360]) < 40.0);
        assert!(a[..640 * 360].iter().all(|v| (16..=235).contains(v)));
        let tone = tone_20ms(0, 440.0, 330.0);
        assert!(tone.iter().any(|s| *s != 0) && tone.iter().all(|s| s.abs() <= 330));
        assert!(tone_20ms(3, 440.0, 0.0).iter().all(|s| *s == 0));
    }
}
