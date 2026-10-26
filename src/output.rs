//! Audio output driver: the receive path's due PCM (already timed on the shared
//! playout clock by `listen::AudioReceiver`) is written, in order, to a render
//! device. Production uses the shared-mode WASAPI client (`playback::Wasapi`);
//! tests and offline runs use `NullSink` or `MemorySink`, which never touch an
//! audio endpoint. Nothing here opens a device unless `open_wasapi` is called.
use crate::{
    listen::{self, AudioReceiver, UnitDecoder},
    playback::{self, Playback, RenderDevice, Wasapi},
};
use std::{
    sync::atomic::AtomicBool,
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Receive(listen::Error),
    Device(playback::Error),
}

/// Discards everything and is always ready: the silent sink for offline runs.
#[derive(Default)]
pub struct NullSink {
    pub samples_discarded: u64,
}
impl RenderDevice for NullSink {
    fn available(&mut self) -> Result<usize, playback::Error> {
        Ok(crate::opus::Decoder::MAX_SAMPLES)
    }
    fn write(&mut self, samples: &[i16]) -> Result<(), playback::Error> {
        self.samples_discarded += samples.len() as u64;
        Ok(())
    }
    fn wait(&mut self, _: Instant, _: &AtomicBool) -> Result<(), playback::Error> {
        Ok(())
    }
}

/// Collects PCM in memory with a small device-like buffer: `wait` plays the
/// buffer out, so a write longer than the buffer takes several waits.
pub struct MemorySink {
    pub pcm: Vec<i16>,
    capacity: usize,
    free: usize,
    pub waits: usize,
}
impl MemorySink {
    pub fn new(capacity: usize) -> Self {
        Self {
            pcm: Vec::new(),
            capacity,
            free: capacity,
            waits: 0,
        }
    }
}
impl RenderDevice for MemorySink {
    fn available(&mut self) -> Result<usize, playback::Error> {
        Ok(self.free)
    }
    fn write(&mut self, samples: &[i16]) -> Result<(), playback::Error> {
        if samples.len() > self.free {
            return Err(playback::Error::Capacity);
        }
        self.free -= samples.len();
        self.pcm.extend_from_slice(samples);
        Ok(())
    }
    fn wait(&mut self, _: Instant, _: &AtomicBool) -> Result<(), playback::Error> {
        self.waits += 1;
        self.free = self.capacity;
        Ok(())
    }
}

/// Opens the default shared-mode render endpoint. Explicit and never called by
/// the UI, unit tests or module initialization; call it on the output worker.
pub fn open_wasapi() -> Result<AudioOutput<Wasapi>, playback::Error> {
    Ok(AudioOutput::new(Wasapi::open_default()?))
}

pub struct AudioOutput<D: RenderDevice> {
    playback: Playback<D>,
    muted: bool,
    pub units_written: u64,
}
impl<D: RenderDevice> AudioOutput<D> {
    pub fn new(playback: Playback<D>) -> Self {
        Self {
            playback,
            muted: false,
            units_written: 0,
        }
    }
    /// Muted output keeps the clock running and writes silence of equal length,
    /// so un-muting never replays old audio.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }
    pub fn muted(&self) -> bool {
        self.muted
    }
    /// Decodes every unit due within `lead_ns` of `now_ns` (the playout clock's
    /// target time per unit) and writes its PCM in order. A device error closes
    /// the output (no retry); a decode error is reported once.
    pub fn pump<U: UnitDecoder>(
        &mut self,
        receiver: &mut AudioReceiver<U>,
        now_ns: i64,
        lead_ns: i64,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<usize, Error> {
        let mut scratch = vec![0i16; crate::opus::Decoder::MAX_SAMPLES];
        let mut chunks: Vec<Vec<i16>> = Vec::new();
        let muted = self.muted;
        let count = receiver
            .pump(now_ns, lead_ns, &mut scratch, |_, pcm| {
                chunks.push(if muted { vec![0; pcm.len()] } else { pcm.to_vec() });
            })
            .map_err(Error::Receive)?;
        for chunk in &chunks {
            self.playback
                .write(chunk, deadline, cancel)
                .map_err(Error::Device)?;
            self.units_written += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        media::{self, AUDIO, CONFIG},
        opus,
        playout::Profile,
    };
    use std::time::Duration;
    const MS: i64 = 1_000_000;
    struct Fake;
    impl UnitDecoder for Fake {
        fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error> {
            pcm[..unit.len()].fill(i16::from(unit[0]));
            Ok(unit.len())
        }
    }
    fn packet(flags: u8, pts_us: i64, payload: &[u8]) -> media::Packet<'_> {
        media::Packet { flags, pts_us, payload }
    }
    fn receiver(units: i64) -> AudioReceiver<Fake> {
        let mut clock = Profile::Radio.clock();
        let mut rx = AudioReceiver::<Fake>::default();
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, |_| Ok(Fake)).unwrap();
        for i in 0..units {
            rx.on_packet(packet(AUDIO, i * 20_000, &[i as u8 + 1; 4]), i * 20 * MS, &mut clock, |_| Ok(Fake)).unwrap();
        }
        rx
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }

    #[test]
    fn due_units_reach_the_memory_sink_in_order_through_a_small_buffer() {
        let mut rx = receiver(3);
        let mut out = AudioOutput::new(Playback::new(MemorySink::new(5)));
        let cancel = AtomicBool::new(false);
        // The radio profile starts at 600 ms: nothing is due earlier.
        assert_eq!(out.pump(&mut rx, 100 * MS, 0, deadline(), &cancel), Ok(0));
        assert_eq!(out.pump(&mut rx, 10_000 * MS, 0, deadline(), &cancel), Ok(3));
        assert_eq!(out.units_written, 3);
        assert_eq!(rx.queued(), 0);
    }
    #[test]
    fn muted_output_keeps_timing_and_writes_silence() {
        let mut rx = receiver(2);
        let sink = MemorySink::new(64);
        let mut out = AudioOutput::new(Playback::new(sink));
        out.set_muted(true);
        let cancel = AtomicBool::new(false);
        out.pump(&mut rx, 10_000 * MS, 0, deadline(), &cancel).unwrap();
        assert!(out.muted());
        assert_eq!(out.units_written, 2);
    }
    #[test]
    fn the_null_sink_counts_samples_and_a_device_error_closes_the_output() {
        let mut rx = receiver(2);
        let mut out = AudioOutput::new(Playback::new(NullSink::default()));
        let cancel = AtomicBool::new(false);
        assert_eq!(out.pump(&mut rx, 10_000 * MS, 0, deadline(), &cancel), Ok(2));
        // A cancelled write is a device error; the output is closed afterwards.
        let mut rx = receiver(1);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            out.pump(&mut rx, 10_000 * MS, 0, deadline(), &cancel),
            Err(Error::Device(playback::Error::Cancelled))
        );
        let mut rx = receiver(1);
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            out.pump(&mut rx, 10_000 * MS, 0, deadline(), &cancel),
            Err(Error::Device(playback::Error::Closed))
        );
    }
}
