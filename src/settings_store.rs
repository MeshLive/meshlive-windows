//! Saved settings (Settings > switches), DPAPI-sealed at `settings.bin` in the
//! app folder (on the wipe allowlist). The text inside is `name=0|1` lines,
//! so a later build can add switches: a missing name keeps its default, an
//! unknown one is ignored. An unreadable file means the defaults.
use crate::ui::model::Switches;
use meshlive::sealed::{self, Dpapi, Sealer};
use std::path::{Path, PathBuf};

pub const FILE: &str = "settings.bin";
const SEAL_LABEL: &str = "MeshLive settings";

pub struct SettingsStore {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
}
impl SettingsStore {
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self { path: dir.join(FILE), sealer }
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(Dpapi::new(SEAL_LABEL)))
    }
    pub fn load(&self) -> Option<Switches> {
        let bytes = sealed::read(&self.path, self.sealer.as_ref()).ok()??;
        Some(decode(&String::from_utf8(bytes).ok()?))
    }
    pub fn save(&self, switches: &Switches) -> bool {
        sealed::write(&self.path, self.sealer.as_ref(), encode(switches).as_bytes()).is_ok()
    }
}

fn fields(s: &mut Switches) -> [(&'static str, &mut bool); 15] {
    [
        ("generated_names", &mut s.generated_names),
        ("title_pages", &mut s.title_pages),
        ("names_before_answer", &mut s.names_before_answer),
        ("bvmid2", &mut s.bvmid2),
        ("bvmid1_compat", &mut s.bvmid1_compat),
        ("visible_by_name", &mut s.visible_by_name),
        ("voice_calls", &mut s.voice_calls),
        ("ringback", &mut s.ringback),
        ("block_unknown", &mut s.block_unknown),
        ("relay", &mut s.relay),
        ("receive_calls", &mut s.receive_calls),
        ("visible_to_contacts", &mut s.visible_to_contacts),
        ("onboarded", &mut s.onboarded),
        ("start_with_windows", &mut s.start_with_windows),
        ("tray_hint_shown", &mut s.tray_hint_shown),
    ]
}

pub fn encode(switches: &Switches) -> String {
    let mut copy = switches.clone();
    fields(&mut copy).iter().map(|(name, value)| format!("{name}={}\n", u8::from(**value))).collect()
}

pub fn decode(text: &str) -> Switches {
    let mut out = Switches::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else { continue };
        let value = match value.trim() {
            "1" => true,
            "0" => false,
            _ => continue,
        };
        if let Some((_, field)) = fields(&mut out).into_iter().find(|(n, _)| *n == name.trim()) {
            *field = value;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_unknown_or_missing_names_keep_defaults() {
        let mut s = Switches::default();
        s.block_unknown = true;
        s.receive_calls = false;
        s.ringback = false;
        s.start_with_windows = true;
        s.tray_hint_shown = true;
        assert_eq!(decode(&encode(&s)), s);
        let partial = decode("block_unknown=1\nfuture_switch=1\nringback=maybe\n");
        assert!(partial.block_unknown);
        assert_eq!(partial.ringback, Switches::default().ringback);
        assert_eq!(decode(""), Switches::default());
    }

    #[test]
    fn the_sealed_file_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("meshlive-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/settings/1")));
        assert_eq!(store.load(), None);
        let mut s = Switches::default();
        s.block_unknown = true;
        assert!(store.save(&s));
        let reopened = SettingsStore::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/settings/1")));
        assert_eq!(reopened.load(), Some(s));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
