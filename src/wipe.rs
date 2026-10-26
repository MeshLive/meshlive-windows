//! The emergency wipe (CF-PRIV-26, Android `EmergencyWipe`): this PC
//! forgets everything MeshLive stored. Sequence:
//! 1. write the persistent "wipe pending" marker (an interrupted wipe
//!    resumes at the next launch before anything starts);
//! 2. delete the CNG identity key first (the node ID and every
//!    recognition derived from it end here);
//! 3. delete MeshLive's own files from an exact allowlist (never a pattern
//!    sweep, never outside the app folder), including their temporary
//!    siblings;
//! 4. a final sweep: anything still present from the allowlist makes the
//!    report incomplete; unknown files are reported, never deleted;
//! 5. remove the marker only when complete.
//!
//! Android also clears its settings, recent calls, onboarding state and
//! notifications. On Windows those live only in memory (nothing persisted),
//! so ending the process after the wipe covers them; the owner ends calls,
//! scans and announcements before calling `run`.
use std::path::{Path, PathBuf};

pub const MARKER: &str = "wipe-pending";
/// Every file MeshLive writes in its folder (relative paths).
pub const FILES: &[&str] = &[
    "identity-verification/verified-contacts.bin",
    "identity-verification/contact-claims.bin",
    "identity-verification/presence-leases.bin",
    "identity-verification/presence-generation-floor.bin",
    "identity-verification/blocked-callers.bin",
    "discovery-epoch.bin",
    "profile.bin",
    "settings.bin",
    "trace.log",
    "trace.on",
    // Crash reports (crate::crash): a ring of five fixed names and the marker.
    "crash/crash-1.txt",
    "crash/crash-2.txt",
    "crash/crash-3.txt",
    "crash/crash-4.txt",
    "crash/crash-5.txt",
    "crash/local-1.dmp",
    "crash/local-2.dmp",
    "crash/local-3.dmp",
    "crash/shown",
];
/// Folders MeshLive creates; removed only when empty.
const FOLDERS: &[&str] = &["identity-verification", "crash"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The identity key existed and was deleted (false: there was none).
    pub key_deleted: bool,
    pub key_error: Option<String>,
    pub removed: Vec<String>,
    /// Allowlisted files or storage boundaries that could not be removed safely.
    pub failed: Vec<String>,
    /// Files in the app folder that MeshLive does not know (left alone).
    pub unknown: Vec<String>,
    pub complete: bool,
}

fn storage_guard_enabled() -> bool {
    std::env::var("MESHLIVE_WIPE_STORAGE_GUARD").as_deref() != Ok("0")
}

pub(crate) fn linked_or_unreadable(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT, including junctions.
        }
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn unsafe_root(dir: &Path) -> bool {
    !dir.is_absolute() || dir.ancestors().any(linked_or_unreadable)
        || std::fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir())
}

/// A known file is unsafe to delete through a link: the file itself or the
/// MeshLive folder that holds it.
fn linked_known(dir: &Path, path: &Path) -> bool {
    linked_or_unreadable(path) || path.parent().is_some_and(|folder| folder != dir && linked_or_unreadable(folder))
}

/// Only the marker means a wipe was requested. Unsafe storage alone never
/// starts one: a wipe deletes the identity, so it needs the user's request.
pub fn pending(dir: &Path) -> bool {
    pending_with_guard(dir, storage_guard_enabled())
}
fn pending_with_guard(dir: &Path, _guarded: bool) -> bool {
    std::fs::symlink_metadata(dir.join(MARKER)).is_ok()
}

/// A failed pending wipe must not create a fresh identity, start transport or
/// reopen stores. The opener is the owner's resource factory, not a key hook.
pub fn resume_before_open<T>(dir: Option<&Path>, delete_key: &mut dyn FnMut() -> Result<bool, String>,
    open: impl FnOnce() -> T) -> (Option<Report>, Option<T>) {
    resume_with_guard(dir, storage_guard_enabled(), delete_key, open)
}
fn resume_with_guard<T>(dir: Option<&Path>, guarded: bool, delete_key: &mut dyn FnMut() -> Result<bool, String>,
    open: impl FnOnce() -> T) -> (Option<Report>, Option<T>) {
    let report = dir.filter(|dir| pending_with_guard(dir, guarded)).map(|dir| run_with_guard(dir, delete_key, guarded));
    let resources = if guarded && report.as_ref().is_some_and(|r| !r.complete) { None } else { Some(open()) };
    (report, resources)
}

fn allowlisted(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for relative in FILES {
        out.push((relative.to_string(), dir.join(relative)));
        let temporary = format!("{relative}.tmp");
        out.push((temporary.clone(), dir.join(&temporary)));
    }
    out
}

/// Runs the wipe in `dir` (the app folder). `delete_key` deletes the
/// identity key: Ok(true) deleted, Ok(false) none existed.
pub fn run(dir: &Path, delete_key: &mut dyn FnMut() -> Result<bool, String>) -> Report {
    run_with_guard(dir, delete_key, storage_guard_enabled())
}
fn run_with_guard(dir: &Path, delete_key: &mut dyn FnMut() -> Result<bool, String>, guarded: bool) -> Report {
    let mut report = Report::default();
    let marker = dir.join(MARKER);
    let root_unsafe = guarded && unsafe_root(dir);
    // Never write the marker through a link; a failed marker still lets the
    // rest of this requested wipe run, as on Android.
    if root_unsafe || guarded && linked_or_unreadable(&marker) {
        report.failed.push(MARKER.into());
    } else {
        let _ = std::fs::create_dir_all(dir);
        if std::fs::write(&marker, b"1").is_err() {
            report.failed.push(MARKER.into());
        }
    }
    // The CNG key is not in the folder, so a storage problem never keeps it.
    match delete_key() {
        Ok(deleted) => report.key_deleted = deleted,
        Err(error) => report.key_error = Some(error),
    }
    if root_unsafe {
        report.failed.push("app data path".into());
        return report;
    }
    for (name, path) in allowlisted(dir) {
        if guarded && linked_known(dir, &path) {
            report.failed.push(name);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => report.removed.push(name),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => report.failed.push(name),
        }
    }
    for folder in FOLDERS {
        // remove_dir refuses a non-empty folder: unknown files stay.
        if !(guarded && linked_or_unreadable(&dir.join(folder))) {
            let _ = std::fs::remove_dir(dir.join(folder));
        }
    }
    // Final sweep: the allowlist must be gone; anything else is only reported.
    for (name, path) in allowlisted(dir) {
        if path.exists() && !report.failed.contains(&name) {
            report.failed.push(name);
        }
    }
    let known: Vec<PathBuf> = allowlisted(dir).into_iter().map(|(_, p)| p).collect();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if guarded && linked_or_unreadable(&path) {
                report.unknown.push(path.strip_prefix(dir).unwrap_or(&path).display().to_string());
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path != marker && !known.contains(&path) {
                report.unknown.push(path.strip_prefix(dir).unwrap_or(&path).display().to_string());
            }
        }
    }
    report.complete = report.failed.is_empty() && report.key_error.is_none();
    if report.complete {
        // Complete only once the marker is really gone; otherwise the next
        // launch resumes and startup stays closed.
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                report.failed.push(MARKER.into());
                report.complete = false;
            }
        }
    }
    report
}

/// The wipe of this PC's real install: the app's key and `%LOCALAPPDATA%\MeshLive`.
pub fn run_installed(dir: &Path) -> Report {
    run(dir, &mut || crate::identity::Identity::delete_persisted().map_err(|e| format!("{e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary_fixture() -> PathBuf {
        let workspace = std::env::current_dir().unwrap().canonicalize().unwrap();
        let parent = workspace.join(".artifacts"); std::fs::create_dir_all(&parent).unwrap();
        let nonce: String = crate::crypto::random_nonce().unwrap().iter().map(|b| format!("{b:02x}")).collect();
        let base = parent.join(format!("wipe-boundary-{nonce}"));
        std::fs::create_dir(&base).unwrap();
        assert_eq!(base.canonicalize().unwrap().parent().unwrap(), parent.canonicalize().unwrap());
        base
    }
    fn junction(link: &Path, target: &Path, owned: &Path) {
        use std::os::windows::process::CommandExt;
        assert!(link.parent().unwrap().canonicalize().unwrap().starts_with(owned.canonicalize().unwrap()));
        assert!(target.canonicalize().unwrap().starts_with(owned.canonicalize().unwrap()));
        let status = std::process::Command::new("cmd.exe").args(["/c", "mklink", "/J"])
            .arg(link).arg(target).creation_flags(0x08000000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "native fixture junction must be created");
        assert!(linked_or_unreadable(link));
    }
    fn remove_fixture(base: PathBuf) {
        let workspace = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(base.canonicalize().unwrap().parent().unwrap(), workspace.join(".artifacts").canonicalize().unwrap());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn linked_crash_folder_cannot_delete_foreign_files_or_the_key_and_can_resume_after_repair() {
        for guarded in [false, true] {
            let base = boundary_fixture(); let app = base.join("app"); let protected = base.join("protected");
            std::fs::create_dir(&app).unwrap(); std::fs::create_dir(&protected).unwrap();
            let file = protected.join("local-1.dmp"); std::fs::write(&file, b"protected fixture").unwrap();
            std::fs::write(app.join("profile.bin"), b"app fixture").unwrap();
            let link = app.join("crash"); junction(&link, &protected, &base);
            assert!(!pending_with_guard(&app, true), "a link alone never starts a wipe the user did not request");
            let (report, opened) = resume_with_guard(Some(&app), true, &mut || panic!("no unrequested key deletion"), || 7);
            assert!(report.is_none() && opened == Some(7) && app.join("profile.bin").exists());
            let mut key_calls = 0;
            let report = run_with_guard(&app, &mut || { key_calls += 1; Ok(true) }, guarded);
            if guarded {
                assert!(!report.complete && report.key_deleted && key_calls == 1, "the requested wipe still deletes the key");
                assert!(!app.join("profile.bin").exists(), "unlinked app files still go");
                assert_eq!(std::fs::read(&file).unwrap(), b"protected fixture");
                assert!(pending_with_guard(&app, true), "the incomplete wipe resumes at the next launch");
            } else {
                assert!(report.complete && report.key_deleted && !file.exists(), "the retained control reproduces traversal outside the app folder");
            }
            if std::fs::symlink_metadata(&link).is_ok() { std::fs::remove_dir(&link).unwrap(); }
            assert!(std::fs::symlink_metadata(&link).is_err(), "detach the junction before recursive fixture cleanup");
            if guarded {
                let resumed = run_with_guard(&app, &mut || { key_calls += 1; Ok(false) }, true);
                assert!(resumed.complete && key_calls == 2 && !pending_with_guard(&app, true));
                assert_eq!(std::fs::read(&file).unwrap(), b"protected fixture");
            }
            remove_fixture(base);
        }
    }

    #[test]
    fn unknown_directory_links_are_reported_without_traversal() {
        let base = boundary_fixture(); let app = base.join("app"); let protected = base.join("protected");
        std::fs::create_dir(&app).unwrap(); std::fs::create_dir(&protected).unwrap();
        std::fs::write(protected.join("private-fixture.txt"), b"leave alone").unwrap();
        let link = app.join("linked"); junction(&link, &protected, &base);
        let report = run_with_guard(&app, &mut || Ok(false), true);
        assert!(report.complete); assert_eq!(report.unknown, ["linked"]);
        assert!(protected.join("private-fixture.txt").exists());
        std::fs::remove_dir(&link).unwrap(); assert!(std::fs::symlink_metadata(&link).is_err());
        remove_fixture(base);
    }

    #[test]
    fn failed_wipe_keeps_identity_and_store_initialization_closed_until_a_successful_resume() {
        for guarded in [false, true] {
            let base = boundary_fixture(); let app = base.join("app"); std::fs::create_dir(&app).unwrap();
            std::fs::write(app.join(MARKER), b"1").unwrap();
            let nonce: String = crate::crypto::random_nonce().unwrap().iter().map(|b| format!("{b:02x}")).collect();
            let name = format!("io.github.meshlive.app.wipe-startup-test.{nonce}");
            let mut opened = 0;
            let (report, resources) = resume_with_guard(Some(&app), guarded, &mut || Err("key refused".into()), || {
                opened += 1; crate::identity::Identity::create_test_key(&name).unwrap();
                std::fs::write(app.join("resource-opened"), b"fixture").unwrap();
            });
            let created = crate::identity::Identity::delete_test_key(&name).unwrap();
            assert!(!report.unwrap().complete && pending_with_guard(&app, guarded));
            assert_eq!((opened, created, resources.is_some(), app.join("resource-opened").exists()),
                if guarded { (0, false, false, false) } else { (1, true, true, true) });
            let (report, resources) = resume_with_guard(Some(&app), guarded, &mut || Ok(false), || {
                assert!(!app.join(MARKER).exists(), "initialization follows successful cleanup"); 7
            });
            assert!(report.unwrap().complete); assert_eq!(resources, Some(7));
            remove_fixture(base);
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meshlive-wipe-test-{}", crate::crypto::random_nonce().map(|n| n[0] as u32 * 256 + n[1] as u32).unwrap_or(0)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("identity-verification")).unwrap();
        std::fs::create_dir_all(dir.join("crash")).unwrap();
        dir
    }

    #[test]
    fn a_wipe_deletes_the_test_key_first_then_only_allowlisted_files_and_reports_the_rest() {
        let dir = scratch();
        for f in FILES {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::write(dir.join("identity-verification/verified-contacts.bin.tmp"), b"x").unwrap();
        std::fs::write(dir.join("someone-elses.txt"), b"keep").unwrap();
        // A test-named CNG key, never the app's.
        let name = "io.github.meshlive.app.wipe-test.v1";
        let _ = crate::identity::Identity::delete_test_key(name);
        crate::identity::Identity::create_test_key(name).unwrap();
        let mut order = Vec::new();
        let report = run(&dir, &mut || {
            order.push(dir.join(FILES[0]).exists());
            crate::identity::Identity::delete_test_key(name).map_err(|e| format!("{e:?}"))
        });
        assert_eq!(order, vec![true], "the key goes before any file");
        assert!(report.key_deleted && report.complete, "{report:?}");
        assert_eq!(report.removed.len(), FILES.len() + 1);
        assert_eq!(report.unknown, vec!["someone-elses.txt".to_string()]);
        assert!(dir.join("someone-elses.txt").exists(), "unknown files are never deleted");
        assert!(!pending(&dir), "the marker goes only when complete");
        assert!(!dir.join("identity-verification").exists());
        assert!(matches!(crate::identity::Identity::delete_test_key(name), Ok(false)), "the test key is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_incomplete_wipe_keeps_the_marker_for_the_next_launch() {
        let dir = scratch();
        let report = run(&dir, &mut || Err("key refused".into()));
        assert!(!report.complete);
        assert!(pending(&dir), "resumed at the next launch");
        let again = run(&dir, &mut || Ok(false));
        assert!(again.complete && !pending(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_marker_that_cannot_be_removed_keeps_startup_closed() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch();
        std::fs::write(dir.join(MARKER), b"1").unwrap();
        // Another process holding the marker without delete sharing.
        let held = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(dir.join(MARKER)).unwrap();
        let (report, opened) = resume_with_guard(Some(&dir), true, &mut || Ok(false), || 7);
        assert!(!report.unwrap().complete && opened.is_none() && pending(&dir));
        drop(held);
        let (report, opened) = resume_with_guard(Some(&dir), true, &mut || Ok(false), || 7);
        assert!(report.unwrap().complete && opened == Some(7) && !pending(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_crash_report_file_is_on_the_wipe_allowlist() {
        for file in crate::crash::files() {
            assert!(FILES.contains(&file.as_str()), "{file}");
        }
    }
}
