//! The persisted monotonic discovery epoch (CF-DISC-16/17, Android
//! `DiscoveryEpochClock` with its preference store): `rotating::EpochClock`
//! owned here, its state written after every read that changed it, so the
//! epoch E never goes back across restarts or a clock set backwards, and a
//! forward-jump recovery's new generation reaches the presence vault.
//!
//! File `discovery-epoch.bin` (not secret, not sealed, replaced atomically):
//! `"BEP1" | flags u8 (bit 0 last, bit 1 tick wall) | last u32 | tick wall u32
//! | tick elapsed ms u64 | boot u64 | generation u32`. An unreadable file
//! starts fresh at the wall epoch (E only ever moves forward from there).
//! Windows: elapsed time is `GetTickCount64` (milliseconds since boot, not
//! advanced by wall-clock changes); the boot identity is the boot instant
//! (wall now minus uptime) rounded to 10 s.
use crate::rotating::{EpochClock, EpochReading, EpochState};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"BEP1";

pub fn encode(state: &EpochState) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(u8::from(state.last_epoch.is_some()) | (u8::from(state.tick_wall_epoch.is_some()) << 1));
    out.extend(state.last_epoch.unwrap_or(0).to_be_bytes());
    out.extend(state.tick_wall_epoch.unwrap_or(0).to_be_bytes());
    out.extend(state.tick_elapsed_ms.to_be_bytes());
    out.extend(state.boot.to_be_bytes());
    out.extend(state.generation.to_be_bytes());
    out
}
pub fn decode(b: &[u8]) -> Option<EpochState> {
    if b.len() != 4 + 1 + 4 + 4 + 8 + 8 + 4 || &b[..4] != MAGIC || b[4] & !3 != 0 {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
    let generation = u32_at(29);
    if generation == 0 {
        return None;
    }
    Some(EpochState {
        last_epoch: (b[4] & 1 != 0).then(|| u32_at(5)),
        tick_wall_epoch: (b[4] & 2 != 0).then(|| u32_at(9)),
        tick_elapsed_ms: u64_at(13),
        boot: u64_at(21),
        generation,
    })
}

/// Milliseconds since boot (monotonic) and the boot identity.
pub fn system_clocks() -> (u64, u64) {
    let elapsed = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    (elapsed, wall.saturating_sub(elapsed) / 10_000)
}

pub struct EpochOwner {
    path: PathBuf,
    clock: EpochClock,
}
impl EpochOwner {
    pub fn open(dir: &Path, boot: u64) -> Self {
        let path = dir.join("discovery-epoch.bin");
        let state = std::fs::read(&path).ok().and_then(|b| decode(&b)).unwrap_or_default();
        Self { path, clock: EpochClock::new(state, boot) }
    }
    pub fn generation(&self) -> u32 {
        self.clock.state.generation
    }
    /// The current epoch E; the state is saved when it changed. A reading
    /// with `new_generation` tells the owner to start a fresh day-key window.
    pub fn read(&mut self, wall_ms: u64, elapsed_ms: u64) -> Option<EpochReading> {
        self.read_inner(wall_ms, elapsed_ms, false)
    }
    /// Stream publication requires the new epoch to survive a restart.
    pub fn read_persisted(&mut self, wall_ms: u64, elapsed_ms: u64) -> Option<EpochReading> {
        self.read_inner(wall_ms, elapsed_ms, true)
    }
    fn read_inner(&mut self, wall_ms: u64, elapsed_ms: u64, required: bool) -> Option<EpochReading> {
        let before = self.clock.state.clone();
        let reading = self.clock.read(wall_ms, elapsed_ms)?;
        if self.clock.state != before {
            if self.save().is_err() && required {
                self.clock.state = before;
                return None;
            }
        }
        Some(reading)
    }
    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut temporary = self.path.as_os_str().to_owned();
        temporary.push(".tmp");
        std::fs::write(&temporary, encode(&self.clock.state))?;
        std::fs::rename(&temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::tests::scratch;
    const DAY_MS: u64 = 86_400_000;
    // 2026-10-10T00:00:00Z: epoch 1990656 (CF-DISC-14 vector).
    const WALL: u64 = 1_990_656 * 900_000;

    #[test]
    fn failed_persistence_does_not_publish_an_uncommitted_epoch() {
        let dir = scratch("epoch-write-failure");
        std::fs::create_dir(dir.join("discovery-epoch.bin")).unwrap();
        let mut owner = EpochOwner::open(&dir, 7);
        assert!(owner.read_persisted(WALL, 1000).is_none());
        std::fs::remove_dir(dir.join("discovery-epoch.bin")).unwrap();
        assert_eq!(owner.read_persisted(WALL, 1000).unwrap().epoch, 1_990_656);
        let mut reopened = EpochOwner::open(&dir, 7);
        assert!(reopened.read(WALL - DAY_MS, 2000).unwrap().epoch >= 1_990_656);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_epoch_never_goes_back_across_a_restart_and_a_clock_set_backwards() {
        let dir = scratch("epoch");
        let mut owner = EpochOwner::open(&dir, 7);
        assert_eq!(owner.read(WALL, 1_000).unwrap().epoch, 1_990_656);
        assert_eq!(owner.read(WALL + 900_000, 901_000).unwrap().epoch, 1_990_657);
        // Restart (same boot), then the wall clock set back a day.
        let mut reopened = EpochOwner::open(&dir, 7);
        let back = reopened.read(WALL - DAY_MS, 902_000).unwrap();
        assert!(back.epoch >= 1_990_657, "E never goes down: {}", back.epoch);
        assert_eq!(reopened.generation(), 1);
        // The file round-trips exactly and rejects damage.
        let bytes = std::fs::read(dir.join("discovery-epoch.bin")).unwrap();
        let state = decode(&bytes).unwrap();
        assert_eq!(encode(&state), bytes);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        let mut zero_generation = bytes.clone();
        zero_generation[29..33].copy_from_slice(&[0; 4]);
        assert!(decode(&zero_generation).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_system_clocks_are_monotonic_and_the_boot_identity_is_stable() {
        let (e1, b1) = system_clocks();
        let (e2, b2) = system_clocks();
        assert!(e2 >= e1);
        assert!(b1.abs_diff(b2) <= 1);
    }
}
