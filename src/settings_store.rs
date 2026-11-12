//! Saved settings (Settings > switches), DPAPI-sealed at `settings.bin` in the
//! app folder (on the wipe allowlist). The text inside is `name=0|1` lines (and `names_audience=everyone|verified|nobody`),
//! so a later build can add switches: a missing name keeps its default, an
//! unknown one is ignored. An unreadable file means the defaults.
use crate::ui::model::Switches;
use meshlive::sealed::{self, Dpapi, Sealer};
use std::path::{Path, PathBuf};

pub const FILE: &str = "settings.bin";
const SEAL_LABEL: &str = "MeshLive settings";
/// "Who sees my name": `everyone`, `verified` or `nobody` (a missing line:
/// everyone, Android's default; anything else: nobody, fail closed).
const AUDIENCE: &str = "names_audience";

/// What `settings.bin` says about "Who sees my name" (WP4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudienceLoad {
    /// No file, or no saved choice (a first run or an older build): Android's
    /// default (`fromPref(null)`), everyone.
    Missing,
    Saved(meshlive::label_policy::NamesAudience),
    /// The file exists but cannot be read or decoded, or holds an unknown
    /// value: Nobody (fail closed).
    Failed(&'static str),
}
impl AudienceLoad {
    pub fn audience(self) -> meshlive::label_policy::NamesAudience {
        use meshlive::label_policy::NamesAudience;
        match self {
            AudienceLoad::Missing => NamesAudience::Everyone,
            AudienceLoad::Saved(audience) => audience,
            AudienceLoad::Failed(_) => NamesAudience::Nobody,
        }
    }
}

fn audience_value(value: &str) -> Option<meshlive::label_policy::NamesAudience> {
    meshlive::label_policy::NamesAudience::ALL.into_iter().find(|a| a.stored() == value)
}

/// The saved audience in the decoded settings text (WP6NF finding 9: the one
/// parser): one known value (repeated the same is fine); none is Missing; an
/// unknown value or two different values is Failed (fail closed).
pub fn audience_in(text: &str) -> AudienceLoad {
    let mut found = AudienceLoad::Missing;
    for line in text.lines() {
        if let Some((name, value)) = line.split_once('=')
            && name.trim() == AUDIENCE
        {
            let this = audience_value(value.trim()).map_or(AudienceLoad::Failed("unknown value"), AudienceLoad::Saved);
            found = match (found, this) {
                (AudienceLoad::Missing, this) => this,
                (same, this) if same == this => same,
                (AudienceLoad::Failed(reason), _) => AudienceLoad::Failed(reason),
                _ => AudienceLoad::Failed("conflicting values"),
            };
        }
    }
    found
}

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
    /// Tests only: start-up uses `read` (WP6NF finding 9).
    #[cfg(test)]
    pub fn load(&self) -> Option<Switches> {
        self.read().0
    }
    /// WP6NF finding 9: one read of the file: the saved switches and the
    /// audience the same bytes decided, so start-up applies one answer.
    pub fn read(&self) -> (Option<Switches>, AudienceLoad) {
        match sealed::read(&self.path, self.sealer.as_ref()) {
            Ok(None) => (None, AudienceLoad::Missing),
            Err(_) => (None, AudienceLoad::Failed("unreadable")),
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => (Some(decode(&text)), audience_in(&text)),
                Err(_) => (None, AudienceLoad::Failed("undecodable")),
            },
        }
    }
    /// WP4: "Who sees my name" as saved, telling a missing choice from one
    /// that cannot be recovered.
    /// Tests only: start-up uses `read` (WP6NF finding 9).
    #[cfg(test)]
    pub fn load_audience(&self) -> AudienceLoad {
        self.read().1
    }
    pub fn save(&self, switches: &Switches) -> bool {
        sealed::write(&self.path, self.sealer.as_ref(), encode(switches).as_bytes()).is_ok()
    }
}

/// The saved names. A name a newer or older build wrote that is not here (a
/// removed switch such as `voice_calls`, `ringback`, `title_pages`, `relay`,
/// `bvmid2`, `bvmid1_compat`, `visible_by_name`, `visible_to_contacts`) is
/// ignored on load and dropped on the next save.
fn fields(s: &mut Switches) -> [(&'static str, &mut bool); 8] {
    [
        ("block_unknown", &mut s.block_unknown),
        ("receive_calls", &mut s.receive_calls),
        ("onboarded", &mut s.onboarded),
        ("start_with_windows", &mut s.start_with_windows),
        ("tray_hint_shown", &mut s.tray_hint_shown),
        ("block_unknown_offer_dismissed", &mut s.block_unknown_offer_dismissed),
        ("video_fill", &mut s.video_fill),
        ("frame_age", &mut s.frame_age),
    ]
}

pub fn encode(switches: &Switches) -> String {
    let mut copy = switches.clone();
    let mut text: String = fields(&mut copy).iter().map(|(name, value)| format!("{name}={}\n", u8::from(**value))).collect();
    // The one choice that is not on/off (Android's preference values).
    text.push_str(&format!("{AUDIENCE}={}\n", switches.names_audience.stored()));
    text
}

pub fn decode(text: &str) -> Switches {
    let mut out = Switches::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else { continue };
        if name.trim() == AUDIENCE {
            // Decided once, below, by the one parser (`audience_in`).
            continue;
        }
        let value = match value.trim() {
            "1" => true,
            "0" => false,
            _ => continue,
        };
        if let Some((_, field)) = fields(&mut out).into_iter().find(|(n, _)| *n == name.trim()) {
            *field = value;
        }
    }
    // WP6NF finding 9: one parser for the audience (conflicting or unknown: Nobody).
    out.names_audience = audience_in(text).audience();
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
        s.start_with_windows = true;
        s.tray_hint_shown = true;
        s.block_unknown_offer_dismissed = true;
        s.video_fill = false;
        s.frame_age = true;
        assert_eq!(decode(&encode(&s)), s);
        let partial = decode("block_unknown=1\nfuture_switch=1\nreceive_calls=maybe\n");
        assert!(partial.block_unknown);
        assert_eq!(partial.receive_calls, Switches::default().receive_calls);
        assert_eq!(decode(""), Switches::default());
    }

    #[test]
    fn an_old_saved_off_for_friendly_names_or_names_before_answer_is_ignored() {
        // Both settings are always on since 2026-11-04 and have no fields to
        // load into; their old keys must not change any other setting.
        let old = decode("generated_names=0\nnames_before_answer=0\n");
        assert_eq!(old, Switches::default());
        let with_a_real_switch = decode("generated_names=0\nnames_before_answer=0\nreceive_calls=0\n");
        assert!(!with_a_real_switch.receive_calls, "the remaining saved switches still load");
    }

    #[test]
    fn a_saved_choice_for_a_switch_removed_on_2026_11_05_is_ignored() {
        let old = "title_pages=0\nbvmid2=1\nbvmid1_compat=1\nvisible_by_name=1\nvoice_calls=0\nringback=0\nrelay=0\nvisible_to_contacts=1\n";
        assert_eq!(decode(old), Switches::default());
        let mixed = decode(&format!("{old}block_unknown=1\nstart_with_windows=1\n"));
        assert!(mixed.block_unknown && mixed.start_with_windows, "the switches that remain still load");
        assert_eq!(decode(&encode(&mixed)), mixed);
    }

    /// "Who sees my name" is saved with Android's values; none (an older
    /// build's file) keeps the default; a damaged or newer value is Nobody
    /// (WP4: fail closed); the switches around it still load.
    #[test]
    fn the_name_audience_round_trips_and_a_bad_value_is_nobody() {
        use meshlive::label_policy::NamesAudience;
        for audience in NamesAudience::ALL {
            let s = Switches { names_audience: audience, block_unknown: true, ..Switches::default() };
            assert_eq!(decode(&encode(&s)), s, "{audience:?}");
        }
        let odd = decode("names_audience=friends\nblock_unknown=1\n");
        assert_eq!(odd.names_audience, NamesAudience::Nobody);
        assert!(odd.block_unknown);
        assert_eq!(decode("receive_calls=0\n").names_audience, NamesAudience::Everyone, "an older build's file");
        assert_eq!(decode("names_audience=nobody\n").names_audience, NamesAudience::Nobody);
    }

    /// WP4: a missing choice is Android's default (everyone); a file that
    /// exists but cannot be read, or an unknown value, is Nobody.
    #[test]
    fn the_saved_audience_is_missing_saved_or_failed_closed() {
        use meshlive::label_policy::NamesAudience;
        assert_eq!(audience_in("receive_calls=1\n"), AudienceLoad::Missing);
        assert_eq!(audience_in("names_audience=verified\n"), AudienceLoad::Saved(NamesAudience::VerifiedOnly));
        assert!(matches!(audience_in("names_audience=friends\n"), AudienceLoad::Failed(_)));
        assert_eq!(AudienceLoad::Missing.audience(), NamesAudience::Everyone);
        assert_eq!(AudienceLoad::Failed("unreadable").audience(), NamesAudience::Nobody);
        let n = meshlive::crypto::random_nonce().unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".artifacts/test-stores")
            .join(format!("settings-audience-{:02x}{:02x}", n[0], n[1]));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::open_default(&dir);
        assert_eq!(store.load_audience(), AudienceLoad::Missing, "a first run");
        assert!(store.save(&Switches { names_audience: NamesAudience::Nobody, ..Switches::default() }));
        assert_eq!(store.load_audience(), AudienceLoad::Saved(NamesAudience::Nobody));
        std::fs::write(dir.join(FILE), b"not sealed").unwrap();
        assert!(matches!(store.load_audience(), AudienceLoad::Failed(_)), "an unreadable file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// WP6NF finding 9: one parser and one read: conflicting or repeated
    /// audience lines decide the same answer for Settings and the call code.
    #[test]
    fn one_read_decides_the_audience_for_settings_and_calls_alike() {
        use meshlive::label_policy::NamesAudience;
        for (text, expected) in [
            ("names_audience=friends\nnames_audience=everyone\n", NamesAudience::Nobody),
            ("names_audience=everyone\nnames_audience=nobody\n", NamesAudience::Nobody),
            ("names_audience=verified\nnames_audience=verified\n", NamesAudience::VerifiedOnly),
            ("receive_calls=1\n", NamesAudience::Everyone),
        ] {
            assert_eq!(decode(text).names_audience, expected, "{text:?}");
            assert_eq!(audience_in(text).audience(), expected, "{text:?}");
        }
        let n = meshlive::crypto::random_nonce().unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".artifacts/test-stores")
            .join(format!("settings-one-read-{:02x}{:02x}", n[0], n[1]));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SettingsStore::open_default(&dir);
        assert!(store.save(&Switches { names_audience: NamesAudience::Nobody, ..Switches::default() }));
        let (saved, audience) = store.read();
        assert_eq!(saved.map(|s| s.names_audience), Some(audience.audience()));
        std::fs::remove_dir_all(&dir).unwrap();
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
