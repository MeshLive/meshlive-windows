//! Call audio follows the playout clock by speed, not by dropping samples
//! (CF-MEDIA-68, Android `AudioPlayoutSync` + AudioTrack PlaybackParams;
//! "other platforms use their time-stretch equivalent"):
//! - `PlayoutSync`: learns the steady output lag from the first 25 chunks;
//!   then within +-40 ms of that baseline the speed is 1.0, else
//!   `1.0 + 0.00025 x errorMs` clamped to 0.96..1.06, changed only in steps
//!   of at least 0.01 (or straight back to 1.0).
//! - `Wsola`: pitch-preserving time stretch for 48 kHz mono (10 ms Hann
//!   windows, 50 % overlap, +-2.5 ms similarity search). At speed 1.0 it is
//!   bypassed, so normal playback is untouched.
use std::collections::VecDeque;

pub struct PlayoutSync {
    learned: Vec<f64>,
    baseline_ms: Option<f64>,
    speed: f64,
}
impl Default for PlayoutSync {
    fn default() -> Self {
        Self { learned: Vec::new(), baseline_ms: None, speed: 1.0 }
    }
}
impl PlayoutSync {
    pub const LEARN_CHUNKS: usize = 25;
    pub const DEAD_BAND_MS: f64 = 40.0;
    /// One played chunk with the output lag measured now; returns the speed.
    pub fn on_chunk(&mut self, lag_ms: f64) -> f64 {
        let Some(baseline) = self.baseline_ms else {
            self.learned.push(lag_ms);
            if self.learned.len() >= Self::LEARN_CHUNKS {
                let mut v = self.learned.clone();
                v.sort_by(f64::total_cmp);
                self.baseline_ms = Some(v[v.len() / 2]);
            }
            return 1.0;
        };
        let error = lag_ms - baseline;
        let wanted = if error.abs() <= Self::DEAD_BAND_MS { 1.0 } else { (1.0 + 0.00025 * error).clamp(0.96, 1.06) };
        if wanted == 1.0 || (wanted - self.speed).abs() >= 0.01 {
            self.speed = (wanted * 100.0).round() / 100.0;
        }
        self.speed
    }
    pub fn baseline_ms(&self) -> Option<f64> {
        self.baseline_ms
    }
}

const WINDOW: usize = 480; // 10 ms
const HOP: usize = WINDOW / 2;
const SEARCH: usize = 120; // +-2.5 ms

/// Streaming WSOLA time stretch (speed > 1 plays faster, pitch preserved).
pub struct Wsola {
    input: VecDeque<f32>,
    /// Fractional analysis position of the next window in `input`.
    position: f64,
    /// Where the previous chosen window started (for the natural continuation).
    previous: Option<usize>,
    /// The overlap tail waiting for the next window.
    tail: Vec<f32>,
    window: Vec<f32>,
}
impl Default for Wsola {
    fn default() -> Self {
        let window = (0..WINDOW).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / WINDOW as f32).cos()).collect();
        Self { input: VecDeque::new(), position: SEARCH as f64, previous: None, tail: vec![0.0; HOP], window }
    }
}
impl Wsola {
    /// Feeds `pcm` and returns what is ready at `speed`.
    pub fn process(&mut self, pcm: &[i16], speed: f64) -> Vec<i16> {
        self.input.extend(pcm.iter().map(|&s| f32::from(s)));
        let mut out = Vec::new();
        loop {
            let nominal = self.position.round() as usize;
            if nominal + SEARCH + WINDOW > self.input.len() {
                break;
            }
            let start = match self.previous {
                None => nominal,
                Some(prev) => {
                    // The segment that would naturally follow the previous one.
                    let natural = prev + HOP;
                    let lo = nominal.saturating_sub(SEARCH);
                    let hi = nominal + SEARCH;
                    let mut best = (f32::MIN, nominal);
                    for candidate in (lo..=hi).step_by(2) {
                        if natural + HOP > self.input.len() || candidate + HOP > self.input.len() {
                            continue;
                        }
                        let score: f32 = (0..HOP).step_by(2).map(|k| self.input[natural + k] * self.input[candidate + k]).sum();
                        if score > best.0 {
                            best = (score, candidate);
                        }
                    }
                    best.1
                }
            };
            // Overlap-add: the first half joins the previous tail; the second half becomes the new tail.
            for k in 0..HOP {
                out.push((self.tail[k] + self.input[start + k] * self.window[k]).clamp(-32768.0, 32767.0) as i16);
            }
            for k in 0..HOP {
                self.tail[k] = self.input[start + HOP + k] * self.window[HOP + k];
            }
            self.previous = Some(start);
            self.position += HOP as f64 * speed;
            // Drop input that no window can reach again.
            let keep_from = (self.position as usize).saturating_sub(SEARCH).min(start);
            if keep_from > 0 {
                self.input.drain(..keep_from);
                self.position -= keep_from as f64;
                self.previous = self.previous.map(|p| p - keep_from);
            }
        }
        out
    }
    /// Everything still held, unstretched (when the speed returns to 1.0).
    pub fn flush(&mut self) -> Vec<i16> {
        let mut out: Vec<i16> = self.tail.iter().map(|&s| s.clamp(-32768.0, 32767.0) as i16).collect();
        let from = (self.previous.map_or(self.position as usize, |p| p + WINDOW)).min(self.input.len());
        out.extend(self.input.iter().skip(from).map(|&s| s.clamp(-32768.0, 32767.0) as i16));
        *self = Self::default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(samples: usize, hz: f64) -> Vec<i16> {
        (0..samples).map(|i| ((i as f64 * hz * std::f64::consts::TAU / 48_000.0).sin() * 8_000.0) as i16).collect()
    }
    fn crossings(pcm: &[i16]) -> usize {
        pcm.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count()
    }

    #[test]
    fn the_speed_rule_follows_android() {
        let mut s = PlayoutSync::default();
        for _ in 0..PlayoutSync::LEARN_CHUNKS {
            assert_eq!(s.on_chunk(100.0), 1.0, "learning");
        }
        assert_eq!(s.baseline_ms(), Some(100.0));
        assert_eq!(s.on_chunk(130.0), 1.0, "inside the 40 ms dead band");
        assert_eq!(s.on_chunk(300.0), 1.05, "200 ms behind: 1 + 0.00025 x 200");
        assert_eq!(s.on_chunk(310.0), 1.05, "a change under 0.01 is not applied");
        assert_eq!(s.on_chunk(1_100.0), 1.06, "clamped");
        assert_eq!(s.on_chunk(-20.0), 0.97, "120 ms ahead: slower");
        assert_eq!(s.on_chunk(-500.0), 0.96, "clamped below");
        assert_eq!(s.on_chunk(110.0), 1.0, "back to 1.0 directly");
    }

    #[test]
    fn wsola_changes_length_by_the_speed_and_keeps_the_pitch() {
        for speed in [1.06, 0.96] {
            let mut w = Wsola::default();
            let input = sine(48_000, 440.0);
            let mut out = Vec::new();
            for chunk in input.chunks(960) {
                out.extend(w.process(chunk, speed));
            }
            let expected = 48_000.0 / speed;
            let ratio = out.len() as f64 / expected;
            assert!((0.97..=1.01).contains(&ratio), "speed {speed}: {} samples for {expected:.0}", out.len());
            // Pitch: zero crossings per sample stay those of 440 Hz (2 per period).
            let rate = crossings(&out) as f64 / out.len() as f64 * 48_000.0 / 2.0;
            assert!((rate - 440.0).abs() < 440.0 * 0.03, "speed {speed}: {rate:.1} Hz");
        }
    }
}
