//! The app's emergency wipe without the UI (CF-PRIV-26), for the planned
//! wipe-then-re-add test. It deletes this PC's MeshLive identity key and the
//! app's stores exactly as the Settings button does (`meshlive::wipe`).
//! IRREVERSIBLE: run only on a PC whose MeshLive identity you mean to delete. It refuses unless given
//! `--approved --confirm WIPE`. It cannot tell whether MeshLive or a MeshLive
//! tool still has the stores open: stop them first.
//!
//! `--data-dir D` wipes another folder instead of `%LOCALAPPDATA%\MeshLive`
//! and never touches the identity key (that is only the real install's).
//! `--dry-run` lists what would be deleted and touches nothing.
//!
//! cargo example: wipe_install --approved --confirm WIPE [--dry-run] [--data-dir D]
fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn log(phase: &str, detail: &str) {
    println!("t={} phase={phase} {detail}", now_ms());
}
fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let approved = std::env::args().any(|a| a == "--approved");
    let confirmed = arg("--confirm").as_deref() == Some("WIPE");
    let dry = std::env::args().any(|a| a == "--dry-run");
    if !(approved && (confirmed || dry)) {
        eprintln!("wipe_install deletes this PC's MeshLive identity and stores. Run only if you mean to delete it: --approved --confirm WIPE (or --approved --dry-run).");
        std::process::exit(2);
    }
    let other_folder = arg("--data-dir").is_some();
    let Some(dir) = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir) else {
        log("failed", "no data folder");
        std::process::exit(1);
    };
    let dir: std::path::PathBuf = dir;
    log("target", &format!("dir={} pending={}", dir.display(), meshlive::wipe::pending(&dir)));
    if dry {
        for relative in meshlive::wipe::FILES {
            log("would_delete", &format!("{relative} present={}", dir.join(relative).exists()));
        }
        if !other_folder {
            log("would_delete", "identity key io.github.meshlive.app.identity.v1 (CNG, current user)");
        }
        log("done", "dry run: nothing was touched");
        return;
    }
    let report = if other_folder {
        // Another folder: its files only; this PC's identity key stays.
        meshlive::wipe::run(&dir, &mut || Ok(false))
    } else {
        meshlive::wipe::run_installed(&dir)
    };
    log(
        "wiped",
        &format!(
            "complete={} key_deleted={} key_error={:?} removed={:?} failed={:?} unknown_kept={:?}",
            report.complete, report.key_deleted, report.key_error, report.removed, report.failed, report.unknown
        ),
    );
    std::process::exit(if report.complete { 0 } else { 1 });
}
