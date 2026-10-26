//! Audio receive path for watching and audio-only radio: admitted audio
//! packets are timed at arrival on the shared playout clock, kept in arrival
//! order, decoded when due and written as continuous PCM. Nothing is dropped:
//! late audio plays at once (Android keeps audio continuous and speeds up
//! slightly instead of dropping; that pitch-preserving catch-up is not ported
//! yet). A bounded queue fails closed instead of silently evicting audio.
use crate::{
    media::{self, AUDIO, AUDIO_BATCH, CONFIG},
    opus,
    playout::Clock,
};
use std::collections::VecDeque;

/// About 12 s of 20 ms units: well beyond the 3 s radio maximum delay.
pub const MAX_QUEUED_UNITS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Audio before its configuration.
    NoConfig,
    Overflow,
    Media(media::Error),
    Decode(opus::Error),
}

/// A decoder for one stream's units (Opus in production, fakes in tests).
pub trait UnitDecoder {
    /// Decodes into the start of `pcm`; returns the sample count.
    fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error>;
}
impl UnitDecoder for opus::Decoder {
    fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error> {
        let decoded = opus::Decoder::decode(self, unit, pcm)?;
        let (start, count) = (decoded.discarded, decoded.samples.len());
        pcm.copy_within(start..start + count, 0);
        Ok(count)
    }
}

struct Unit {
    target_ns: i64,
    payload: Vec<u8>,
}

pub struct AudioReceiver<D> {
    config: Option<Vec<u8>>,
    decoder: Option<D>,
    queue: VecDeque<Unit>,
    late_units: u64,
}
impl<D: UnitDecoder> Default for AudioReceiver<D> {
    fn default() -> Self {
        Self {
            config: None,
            decoder: None,
            queue: VecDeque::new(),
            late_units: 0,
        }
    }
}
impl<D: UnitDecoder> AudioReceiver<D> {
    /// One admitted audio packet at arrival. A repeated identical configuration
    /// (a relay's late-joiner cache) keeps the decoder; a new one replaces it.
    pub fn on_packet(
        &mut self,
        packet: media::Packet<'_>,
        now_ns: i64,
        clock: &mut Clock,
        make: impl FnOnce(&[u8]) -> Result<D, opus::Error>,
    ) -> Result<(), Error> {
        if packet.flags & AUDIO == 0 {
            return Ok(());
        }
        if packet.flags & CONFIG != 0 {
            if self.config.as_deref() != Some(packet.payload) {
                self.decoder = Some(make(packet.payload).map_err(Error::Decode)?);
                self.config = Some(packet.payload.to_vec());
            }
            return Ok(());
        }
        if self.decoder.is_none() {
            return Err(Error::NoConfig);
        }
        let units = if packet.flags & AUDIO_BATCH != 0 {
            packet.split_audio().map_err(Error::Media)?
        } else {
            vec![packet]
        };
        if self.queue.len() + units.len() > MAX_QUEUED_UNITS {
            return Err(Error::Overflow);
        }
        for unit in units {
            let timing = clock.timing(unit.pts_us, now_ns);
            if timing.lateness_ms > 0 {
                self.late_units += 1;
            }
            self.queue.push_back(Unit {
                target_ns: timing.target_ns,
                payload: unit.payload.to_vec(),
            });
        }
        Ok(())
    }
    /// Decodes every unit due within `lead_ns` and hands its PCM to `sink` in
    /// order. Returns the number of units written.
    pub fn pump(
        &mut self,
        now_ns: i64,
        lead_ns: i64,
        pcm: &mut [i16],
        mut sink: impl FnMut(i64, &[i16]),
    ) -> Result<usize, Error> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(0);
        };
        let mut written = 0;
        while self
            .queue
            .front()
            .is_some_and(|u| u.target_ns <= now_ns + lead_ns)
        {
            let unit = self.queue.pop_front().unwrap();
            let count = decoder.decode(&unit.payload, pcm).map_err(Error::Decode)?;
            sink(unit.target_ns, &pcm[..count]);
            written += 1;
        }
        Ok(written)
    }
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
    pub fn clear_queued(&mut self) { self.queue.clear(); }
    pub fn late_units(&self) -> u64 {
        self.late_units
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playout::Profile;
    const MS: i64 = 1_000_000;
    /// Writes `unit[0]` repeated `unit.len()` times: enough to check order.
    struct Fake;
    impl UnitDecoder for Fake {
        fn decode(&mut self, unit: &[u8], pcm: &mut [i16]) -> Result<usize, opus::Error> {
            if unit[0] == 0xff {
                return Err(opus::Error::Packet);
            }
            pcm[..unit.len()].fill(i16::from(unit[0]));
            Ok(unit.len())
        }
    }
    fn packet(flags: u8, pts_us: i64, payload: &[u8]) -> media::Packet<'_> {
        media::Packet {
            flags,
            pts_us,
            payload,
        }
    }
    #[test]
    fn audio_is_timed_at_arrival_kept_in_order_and_written_when_due() {
        let mut clock = Profile::Radio.clock();
        let mut rx = AudioReceiver::<Fake>::default();
        assert_eq!(
            rx.on_packet(packet(AUDIO, 0, &[1; 4]), 0, &mut clock, |_| Ok(Fake)),
            Err(Error::NoConfig)
        );
        let mut made = 0;
        let mut make = |_: &[u8]| {
            made += 1;
            Ok(Fake)
        };
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, &mut make)
            .unwrap();
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, &mut make)
            .unwrap();
        assert_eq!(
            made, 1,
            "an identical repeated configuration keeps the decoder"
        );
        for i in 0..5 {
            rx.on_packet(
                packet(AUDIO, i * 20_000, &[i as u8 + 1; 3]),
                i * 20 * MS,
                &mut clock,
                |_| Ok(Fake),
            )
            .unwrap();
        }
        // Radio starts at 600 ms: nothing is due before then.
        let mut pcm = [0; 16];
        let mut out = Vec::new();
        assert_eq!(
            rx.pump(500 * MS, 20 * MS, &mut pcm, |_, s| out.push(s.to_vec()))
                .unwrap(),
            0
        );
        assert_eq!(
            rx.pump(620 * MS, 20 * MS, &mut pcm, |_, s| out.push(s.to_vec()))
                .unwrap(),
            3
        );
        assert_eq!(
            rx.pump(10_000 * MS, 0, &mut pcm, |_, s| out.push(s.to_vec()))
                .unwrap(),
            2
        );
        assert_eq!(out, (1..=5).map(|v| vec![v; 3]).collect::<Vec<_>>());
        assert_eq!(rx.late_units(), 0);
        // A late unit is still played, never dropped; the clock grows its delay.
        rx.on_packet(
            packet(AUDIO, 100_000, &[9; 2]),
            2_000 * MS,
            &mut clock,
            |_| Ok(Fake),
        )
        .unwrap();
        // Arrival grew the delay to cover it, so it is not counted as late.
        assert_eq!(rx.late_units(), 0);
        assert!(clock.current_delay_ms() > 600);
        assert_eq!(rx.pump(2_000 * MS, 0, &mut pcm, |_, _| {}).unwrap(), 0);
        assert_eq!(rx.pump(5_000 * MS, 0, &mut pcm, |_, _| {}).unwrap(), 1);
    }
    #[test]
    fn batches_split_into_units_video_is_ignored_and_overflow_or_bad_audio_fails() {
        let mut clock = Profile::Auto.clock();
        let mut rx = AudioReceiver::<Fake>::default();
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, |_| {
            Ok(Fake)
        })
        .unwrap();
        // BTA1 pair: magic, 32-bit delta (20 ms), two length-prefixed units.
        let pair = media::Packet {
            flags: AUDIO | AUDIO_BATCH,
            pts_us: 0,
            payload: &[
                b'B', b'T', b'A', b'1', 0, 0, 0x4e, 0x20, 0, 2, 7, 7, 0, 2, 8, 8,
            ],
        };
        rx.on_packet(pair, 0, &mut clock, |_| Ok(Fake)).unwrap();
        assert_eq!(rx.queued(), 2);
        let mut pcm = [0; 4];
        let mut got = Vec::new();
        rx.pump(i64::MAX / 2, 0, &mut pcm, |t, s| got.push((t, s.to_vec())))
            .unwrap();
        assert_eq!(got, vec![(250 * MS, vec![7, 7]), (270 * MS, vec![8, 8])]);
        let before = rx.queued();
        rx.on_packet(packet(media::KEY_FRAME, 0, &[1; 10]), 0, &mut clock, |_| {
            Ok(Fake)
        })
        .unwrap();
        assert_eq!(rx.queued(), before);
        let mut rx = AudioReceiver::<Fake>::default();
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, |_| {
            Ok(Fake)
        })
        .unwrap();
        for i in 0..MAX_QUEUED_UNITS as i64 {
            rx.on_packet(packet(AUDIO, i * 20_000, &[1]), 0, &mut clock, |_| Ok(Fake))
                .unwrap();
        }
        assert_eq!(
            rx.on_packet(packet(AUDIO, 0, &[1]), 0, &mut clock, |_| Ok(Fake)),
            Err(Error::Overflow)
        );
        let mut rx = AudioReceiver::<Fake>::default();
        rx.on_packet(packet(AUDIO | CONFIG, 0, b"cfg"), 0, &mut clock, |_| {
            Ok(Fake)
        })
        .unwrap();
        rx.on_packet(packet(AUDIO, 0, &[0xff]), 0, &mut clock, |_| Ok(Fake))
            .unwrap();
        let mut pcm = [0; 4];
        assert_eq!(
            rx.pump(i64::MAX / 2, 0, &mut pcm, |_, _| {}),
            Err(Error::Decode(opus::Error::Packet))
        );
        assert_eq!(
            AudioReceiver::<Fake>::default().on_packet(
                packet(AUDIO | CONFIG, 0, b"x"),
                0,
                &mut clock,
                |_| Err(opus::Error::Config)
            ),
            Err(Error::Decode(opus::Error::Config))
        );
    }
}
