//! "Export diagnostics" (Settings): one plain-text report the user can attach
//! to a problem report. It holds the app and Windows versions, the Bluetooth
//! adapter facts the app already logs, the settings switches and the last
//! lines of the trace log, if tracing is on. The private-export experiment
//! omits free-form traces and device descriptions, which can identify a user.
//! The report is written only where the user
//! will see it: Documents\MeshLive diagnostics yyyy-mm-dd hhmmss UTC.txt.
use crate::ui::model::Switches;
use std::path::{Path, PathBuf};

pub const TRACE_LINES: usize = 400;

pub fn private_export_enabled() -> bool {
    std::env::var("MESHLIVE_PRIVATE_DIAGNOSTICS").as_deref() != Ok("0")
}

/// The report text (pure apart from what it is given).
pub fn report(app_version: &str, os_build: Option<u32>, adapter: Option<&str>, platform: &str, switches: &Switches, trace_tail: &[String]) -> String {
    report_with_privacy(app_version, os_build, adapter, platform, switches, trace_tail, private_export_enabled())
}

fn report_with_privacy(app_version: &str, os_build: Option<u32>, adapter: Option<&str>, platform: &str, switches: &Switches, trace_tail: &[String], private: bool) -> String {
    if private {
        // Same version allowlist as crash sharing. Never attempt to scrub a
        // free-form device description or trace by guessing which words are private.
        let version = if !app_version.is_empty() && app_version.len() <= 24
            && app_version.bytes().all(|b| b.is_ascii_digit() || b == b'.') { app_version } else { "unknown" };
        return format!("MeshLive diagnostics\napp_version={version}\nwindows_build={}\nsettings:\n{}trace: omitted for privacy\ndevice descriptions: omitted for privacy\n",
            os_build.map_or("unknown".into(), |b| b.to_string()), crate::settings_store::encode(switches));
    }
    let mut out = String::new();
    out.push_str(&format!("MeshLive diagnostics\napp_version={app_version}\nwindows_build={}\n", os_build.map_or("unknown".into(), |b| b.to_string())));
    out.push_str(&format!("bluetooth_adapter={}\nreceive_calls_platform={platform}\n", adapter.unwrap_or("none found")));
    out.push_str("settings:\n");
    out.push_str(&crate::settings_store::encode(switches));
    if trace_tail.is_empty() {
        out.push_str("trace: off (create trace.on in %LOCALAPPDATA%\\MeshLive and restart to record one)\n");
    } else {
        out.push_str(&format!("trace (last {} lines):\n", trace_tail.len()));
        for line in trace_tail {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A failed export must keep the next-launch crash report pending in the
/// private arm. The retained arm preserves its earlier acknowledgement point.
pub fn export(dir: Option<&Path>, text: &str) -> Result<PathBuf, String> {
    export_with(dir, text, private_export_enabled(), save)
}

fn export_with(dir: Option<&Path>, text: &str, private: bool, save: impl FnOnce(&str) -> Result<PathBuf, String>) -> Result<PathBuf, String> {
    if !private { if let Some(dir) = dir { let _ = meshlive::crash::mark_shown(dir); } }
    let saved = save(text);
    if private && saved.is_ok() { if let Some(dir) = dir { let _ = meshlive::crash::mark_shown(dir); } }
    saved
}

/// The last `n` lines of the app's trace log (empty when there is none).
pub fn trace_tail(dir: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join("trace.log")) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}

/// Writes the report into the user's Documents folder; returns its path.
pub fn save(text: &str) -> Result<PathBuf, String> {
    let documents = std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Documents"))
        .filter(|d| d.is_dir())
        .ok_or("Your Documents folder was not found.")?;
    let utc = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = documents.join(file_name(utc));
    std::fs::write(&path, text).map_err(|e| format!("Couldn't save the report ({e})."))?;
    Ok(path)
}

/// `MeshLive diagnostics yyyy-mm-dd hhmmss UTC.txt` for `unix_seconds`.
pub fn file_name(unix_seconds: u64) -> String {
    // Days to civil date (Howard Hinnant's algorithm, proleptic Gregorian).
    let days = (unix_seconds / 86_400) as i64;
    let secs = unix_seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "MeshLive diagnostics {year:04}-{month:02}-{day:02} {:02}{:02}{:02} UTC.txt",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_export_omits_untrusted_trace_device_and_version_text() {
        let private_input = "PRIVATE-CONTENT\napp_version=forged";
        let lines = vec![private_input.into()];
        let private = report_with_privacy(private_input, Some(26200), Some(private_input), private_input, &Switches::default(), &lines, true);
        assert!(!private.contains("PRIVATE-CONTENT") && !private.contains("forged"));
        assert!(private.contains("app_version=unknown\nwindows_build=26200\n"));
        assert!(private.contains("receive_calls=1\n") && private.contains("omitted for privacy"));
        let versioned = report_with_privacy("0.1.0", None, None, "", &Switches::default(), &[], true);
        assert!(versioned.contains("app_version=0.1.0\nwindows_build=unknown\n"));
        let control = report_with_privacy("0.1.0", None, Some(private_input), private_input, &Switches::default(), &lines, false);
        assert!(control.contains("PRIVATE-CONTENT"), "retain the original export arm for review");
    }

    #[test]
    fn failed_private_export_keeps_the_crash_pending_until_a_successful_retry() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".artifacts/test-stores")
            .join(format!("diagnostics-retry-{}", std::process::id()));
        for private in [false, true] {
            let dir = root.join(if private { "private" } else { "control" });
            meshlive::crash::write_in(&dir, "kind=native\n").unwrap();
            let failed = export_with(Some(&dir), "report", private, |_| Err("save failed".into()));
            assert!(failed.is_err());
            assert_eq!(!meshlive::crash::pending(&dir).is_empty(), private,
                "the private arm acknowledges only a saved report");
            let path = root.join("report.txt");
            export_with(Some(&dir), "report", private, |text| {
                std::fs::write(&path, text).map_err(|e| e.to_string())?;
                Ok(path.clone())
            }).unwrap();
            assert_eq!(std::fs::read_to_string(path).unwrap(), "report");
            assert!(meshlive::crash::pending(&dir).is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_file_name_is_a_readable_utc_time() {
        assert_eq!(file_name(1_792_517_301), "MeshLive diagnostics 2026-10-20 172821 UTC.txt");
        assert_eq!(file_name(951_782_400), "MeshLive diagnostics 2000-02-29 000000 UTC.txt");
        assert_eq!(file_name(0), "MeshLive diagnostics 1970-01-01 000000 UTC.txt");
    }

    #[test]
    fn the_report_has_versions_settings_and_the_trace_tail_only() {
        let text = report("0.1.0", Some(26200), Some("USB VID_8087"), "Ok(intel-usb-win11-24h2)", &Switches::default(), &["t=1 bluetooth phase=x".into()]);
        assert!(text.contains("app_version=0.1.0") && text.contains("windows_build=26200"));
        assert!(text.contains("receive_calls=1"));
        if private_export_enabled() {
            assert!(text.contains("trace: omitted for privacy") && !text.contains("t=1 bluetooth phase=x"));
            assert!(!text.contains("USB VID_8087") && !text.contains("intel-usb-win11-24h2"));
        } else { assert!(text.contains("trace (last 1 lines):\nt=1 bluetooth phase=x")); }
        let off = report("0.1.0", None, None, "Err(x)", &Switches::default(), &[]);
        assert!(off.contains("windows_build=unknown"));
        assert!(off.contains(if private_export_enabled() { "trace: omitted for privacy" } else { "trace: off" }));
    }

    #[test]
    fn the_trace_tail_keeps_the_newest_lines() {
        let dir = std::env::temp_dir().join(format!("meshlive-diag-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trace.log"), (0..10).map(|i| format!("line{i}\n")).collect::<String>()).unwrap();
        assert_eq!(trace_tail(&dir, 3), vec!["line7", "line8", "line9"]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(trace_tail(&dir, 3).is_empty());
    }
}
