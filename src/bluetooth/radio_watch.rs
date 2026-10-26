//! Bluetooth off/on observation (CF-LIFE-13/14/15). Windows raises
//! `Radio.StateChanged` with one clean edge per change (Microsoft Learn:
//! Windows.Devices.Radios.Radio.StateChanged); the app only observes and never
//! toggles the radio (no `SetStateAsync`, no `RequestAccessAsync`).
//!
//! `Epoch` is Android's adapter epoch: an off edge is reported once; an on
//! edge after an off episode schedules "on" after
//! `min(500 ms << n, 8000 ms) + jitter(0..250 ms)` (n = recoveries in the last
//! 60 s, shift capped at 5), and a new off before then cancels it; three or
//! more off episodes within 60 s flag a suspected Bluetooth crash loop.
use super::{Apartment, Error, wait};
use std::{
    collections::VecDeque,
    sync::{
        atomic::AtomicBool,
        mpsc::{Receiver, Sender, channel},
    },
    time::{Duration, Instant},
};
use windows::{
    Devices::{
        Bluetooth::BluetoothAdapter,
        Radios::{Radio, RadioState},
    },
    Foundation::TypedEventHandler,
    core::IInspectable,
};

pub const WINDOW_MS: u64 = 60_000;
pub const CRASH_LOOP_EPISODES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Bluetooth went off (once per episode).
    Off,
    /// Bluetooth is back; recovery may run (the new epoch).
    On(u32),
}

#[derive(Default)]
pub struct Epoch {
    off: bool,
    epoch: u32,
    pending_on_ms: Option<u64>,
    offs: VecDeque<u64>,
    recoveries: VecDeque<u64>,
}
impl Epoch {
    pub fn epoch(&self) -> u32 {
        self.epoch
    }
    pub fn is_off(&self) -> bool {
        self.off
    }
    /// A radio state report; `jitter_ms` in 0..250.
    pub fn on_state(&mut self, on: bool, now_ms: u64, jitter_ms: u64) -> Option<Event> {
        if !on {
            if self.off {
                return None;
            }
            self.off = true;
            self.pending_on_ms = None;
            self.offs.push_back(now_ms);
            return Some(Event::Off);
        }
        if !self.off {
            return None;
        }
        self.off = false;
        self.recoveries.retain(|t| now_ms.saturating_sub(*t) < WINDOW_MS);
        let shift = self.recoveries.len().min(5) as u32;
        let delay = (500u64 << shift).min(8_000) + jitter_ms.min(250);
        self.pending_on_ms = Some(now_ms + delay);
        None
    }
    /// The scheduled "on", once due.
    pub fn poll(&mut self, now_ms: u64) -> Option<Event> {
        let at = self.pending_on_ms?;
        if now_ms < at {
            return None;
        }
        self.pending_on_ms = None;
        self.epoch += 1;
        self.recoveries.push_back(now_ms);
        Some(Event::On(self.epoch))
    }
    /// Three or more off episodes within 60 s: tell the user to restart Bluetooth.
    pub fn crash_loop(&mut self, now_ms: u64) -> bool {
        self.offs.retain(|t| now_ms.saturating_sub(*t) < WINDOW_MS);
        self.offs.len() >= CRASH_LOOP_EPISODES
    }
}

/// What only a Bluetooth restart fixes: repeated scan failures that no user
/// setting explains, or Windows stopping the announcement again and again.
/// The UI shows it with "Open Bluetooth settings".
pub const RESTART_GUIDANCE: &str =
    "Bluetooth on this PC stopped responding. Turn Bluetooth off and on in Windows settings; if that doesn't help, restart this PC.";

/// Opens Windows Bluetooth settings (`ms-settings:bluetooth`); false when
/// Windows refused.
pub fn open_settings() -> bool {
    use windows::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let result = unsafe {
        ShellExecuteW(None, windows::core::w!("open"), windows::core::w!("ms-settings:bluetooth"), None, None, SW_SHOWNORMAL)
    };
    // ShellExecute reports success as a value above 32.
    result.0 as isize > 32
}

/// The user-facing guidance for a suspected crash loop (CF-LIFE-15).
pub const CRASH_LOOP_GUIDANCE: &str =
    "Bluetooth keeps turning off. Turn Bluetooth off and on in Windows settings, or restart this PC.";

/// The default adapter's radio with a StateChanged subscription; `true` = on.
pub struct RadioWatch {
    radio: Radio,
    token: i64,
    pub states: Receiver<bool>,
    _apartment: Apartment,
}
impl RadioWatch {
    pub fn start() -> Result<Self, Error> {
        let apartment = Apartment::new()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let cancelled = AtomicBool::new(false);
        let adapter = wait(BluetoothAdapter::GetDefaultAsync()?, deadline, &cancelled)?;
        let radio = wait(adapter.GetRadioAsync()?, deadline, &cancelled)?;
        let (tx, states): (Sender<bool>, Receiver<bool>) = channel();
        let _ = tx.send(radio.State()? == RadioState::On);
        let token = radio.StateChanged(&TypedEventHandler::<Radio, IInspectable>::new(move |sender, _| {
            if let Ok(radio) = sender.ok() {
                let _ = tx.send(radio.State()? == RadioState::On);
            }
            Ok(())
        }))?;
        Ok(Self {
            radio,
            token,
            states,
            _apartment: apartment,
        })
    }
}
impl Drop for RadioWatch {
    fn drop(&mut self) {
        let _ = self.radio.RemoveStateChanged(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_reported_once_and_on_waits_a_growing_delay_that_a_new_off_cancels() {
        let mut e = Epoch::default();
        assert_eq!(e.on_state(true, 0, 0), None, "on without an off episode: nothing");
        assert_eq!(e.on_state(false, 1_000, 0), Some(Event::Off));
        assert_eq!(e.on_state(false, 1_100, 0), None, "repeats ignored");
        assert_eq!(e.on_state(true, 2_000, 100), None);
        assert_eq!(e.poll(2_599), None);
        assert_eq!(e.poll(2_600), Some(Event::On(1)), "500 ms + jitter");
        // Second recovery within 60 s: 1000 ms; a new off before it cancels.
        assert_eq!(e.on_state(false, 3_000, 0), Some(Event::Off));
        e.on_state(true, 3_500, 0);
        assert_eq!(e.on_state(false, 3_900, 0), Some(Event::Off));
        assert_eq!(e.poll(10_000), None, "cancelled by the new off");
        assert!(e.crash_loop(4_000), "three off episodes within 60 s");
        e.on_state(true, 4_000, 0);
        assert_eq!(e.poll(4_999), None);
        assert_eq!(e.poll(5_000), Some(Event::On(2)));
        assert!(!e.crash_loop(70_000));
        // The delay caps at 8 s.
        let mut e = Epoch::default();
        let mut t = 0;
        for _ in 0..8 {
            e.on_state(false, t, 0);
            e.on_state(true, t, 0);
            while e.poll(t).is_none() {
                t += 10;
            }
        }
        e.on_state(false, t, 0);
        e.on_state(true, t, 250);
        assert_eq!(e.poll(t + 8_249), None);
        assert!(e.poll(t + 8_250).is_some());
    }
}
