//! Crash reports (Android parity: nothing is sent automatically). A Rust
//! panic and a native unhandled exception each write one small text report to
//! `%LOCALAPPDATA%\MeshLive\crash\crash-<1..5>.txt` (a ring of five fixed
//! names, so the wipe's exact allowlist covers them). A report holds the app
//! and Windows versions, the panic location and a scrubbed message (hex runs,
//! long tokens and quoted text removed: no names, keys, codes or contact
//! IDs), the thread name and code addresses relative to `meshlive.exe`.
//! Native minidumps are LOCAL ONLY, in three fixed slots covered
//! by the wipe. Sharing constructs an allowlisted text summary, never a dump.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const FOLDER: &str = "crash";
pub const SLOTS: usize = 5;
pub const DUMP_SLOTS: usize = 3;
pub const SHOWN: &str = "crash/shown";
const MESSAGE_CHARS: usize = 300;
const FRAMES: usize = 48;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static VERSION: OnceLock<String> = OnceLock::new();

pub fn parity_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_CRASH_PARITY").ok().as_deref() != Some("0"))
}

/// The exact relative paths a crash report can use (for the wipe allowlist).
pub fn files() -> Vec<String> {
    let mut out: Vec<String> = (1..=SLOTS).map(|n| format!("{FOLDER}/crash-{n}.txt")).collect();
    out.push(SHOWN.into());
    out.extend((1..=DUMP_SLOTS).map(|n| format!("{FOLDER}/local-{n}.dmp")));
    out
}

/// Installs the panic hook and the native exception filter for `dir` (the
/// app folder). Writes nothing until something crashes.
pub fn install(dir: PathBuf, app_version: &str) {
    if DIR.set(dir).is_err() {
        return;
    }
    let _ = VERSION.set(app_version.to_owned());
    if parity_enabled() {
        native::start_dump_worker();
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or("unknown".into(), |l| format!("{}:{}", short_path(l.file()), l.line()));
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-text panic".into());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
        let text = report("panic", &format!("location={location}\nthread={}\nmessage={}", scrub(&thread), scrub(&message)), &native::stack());
        write(&text);
        native::capture(std::ptr::null());
        previous(info);
    }));
    native::install();
}

/// The report text (pure apart from the version and build it is given).
pub fn report_with(app_version: &str, os_build: Option<u32>, kind: &str, body: &str, frames: &[String]) -> String {
    let utc = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut out = format!(
        "MeshLive crash report\napp_version={app_version}\nwindows_build={}\ntime_unix={utc}\nkind={kind}\n{body}\n",
        os_build.map_or("unknown".into(), |b| b.to_string())
    );
    if !frames.is_empty() {
        out.push_str(&format!("frames (meshlive.exe offsets, {}):\n", frames.len()));
        for frame in frames {
            out.push_str(frame);
            out.push('\n');
        }
    }
    out
}
fn report(kind: &str, body: &str, frames: &[String]) -> String {
    report_with(VERSION.get().map_or("unknown", String::as_str), crate::qw_policy::os_build(), kind, body, frames)
}

/// Code addresses as offsets into this exe (symbols are stripped in release
/// builds; the offsets map back to a build's PDB).
fn frames(ips: &[u64]) -> Vec<String> {
    let base = native::module_base();
    ips.iter()
        .copied()
        .map(|ip| match base {
            Some((start, size)) if ip >= start && ip < start + size => format!("meshlive.exe+0x{:x}", ip - start),
            _ => "other".into(),
        })
        .take(FRAMES)
        .collect()
}

fn short_path(file: &str) -> &str {
    // The path inside the crate only; never a user path.
    match file.rfind("src") {
        Some(cut) => &file[cut..],
        None => file.rsplit(['/', '\\']).next().unwrap_or(file),
    }
}

/// Removes what could identify a person or a key: quoted text, runs of 8 or
/// more hex digits, long letter/digit tokens (base64 or IDs) and paths under
/// the user's profile; then cuts to 300 characters.
pub fn scrub(text: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in text.chars() {
        if c == '"' {
            if !quoted {
                out.push_str("\"<text>");
            }
            quoted = !quoted;
            if !quoted {
                out.push('"');
            }
            continue;
        }
        if !quoted {
            out.push(c);
        }
    }
    // A path under a user profile, up to the next space.
    let out: String = out
        .split(' ')
        .map(|w| if w.contains(":\\Users\\") || w.contains(":/Users/") { "<path>" } else { w })
        .collect::<Vec<_>>()
        .join(" ");
    let mut cleaned = String::new();
    for token in split_keep(&out) {
        let hex = token.len() >= 8 && token.chars().all(|c| c.is_ascii_hexdigit());
        let long = token.len() >= 20 && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '_' || c == '-');
        if hex {
            cleaned.push_str("<hex>");
        } else if long {
            cleaned.push_str("<data>");
        } else {
            cleaned.push_str(token);
        }
    }
    cleaned.chars().filter(|c| !c.is_control() || *c == '\n').take(MESSAGE_CHARS).collect()
}
/// Splits into word tokens and the separators between them (all kept).
fn split_keep(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let is_word = |c: char| c.is_ascii_alphanumeric() || "+/_-".contains(c);
    let mut in_word = None;
    for (i, c) in text.char_indices() {
        let w = is_word(c);
        if in_word != Some(w) {
            if i > start {
                parts.push(&text[start..i]);
            }
            start = i;
            in_word = Some(w);
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// Writes a report into the oldest (or a free) slot of the ring.
fn write(text: &str) {
    let Some(dir) = DIR.get() else { return };
    let _ = write_in(dir, text);
}
/// The crash folder, refused when it or the chosen slot is a link: crash
/// files stay inside the app folder the wipe owns.
fn owned_slot(dir: &Path, names: impl Iterator<Item = String>) -> std::io::Result<PathBuf> {
    let folder = dir.join(FOLDER);
    if crate::wipe::linked_or_unreadable(&folder) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "linked crash folder"));
    }
    std::fs::create_dir_all(&folder)?;
    let slot = names.map(|name| folder.join(name))
        .min_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .expect("crash slots");
    if crate::wipe::linked_or_unreadable(&slot) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "linked crash slot"));
    }
    Ok(slot)
}
pub fn write_in(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    let slot = owned_slot(dir, (1..=SLOTS).map(|n| format!("crash-{n}.txt")))?;
    std::fs::write(&slot, text)?;
    Ok(slot)
}

/// Reports written since the last `mark_shown`, newest first.
pub fn pending(dir: &Path) -> Vec<PathBuf> {
    let shown = std::fs::metadata(dir.join(SHOWN)).and_then(|m| m.modified()).ok();
    let mut reports: Vec<(std::time::SystemTime, PathBuf)> = (1..=SLOTS)
        .map(|n| dir.join(FOLDER).join(format!("crash-{n}.txt")))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .filter(|(t, _)| shown.is_none_or(|s| *t > s))
        .collect();
    reports.sort_by(|a, b| b.0.cmp(&a.0));
    reports.into_iter().map(|(_, p)| p).collect()
}
/// The newest report's text (for Export diagnostics), if any.
pub fn newest(dir: &Path) -> Option<String> {
    let mut reports: Vec<(std::time::SystemTime, PathBuf)> = (1..=SLOTS)
        .map(|n| dir.join(FOLDER).join(format!("crash-{n}.txt")))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    reports.sort_by(|a, b| b.0.cmp(&a.0));
    reports.first().and_then(|(_, p)| std::fs::read_to_string(p).ok())
}
/// The pending reports were offered (shared or dismissed).
pub fn mark_shown(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join(FOLDER))?;
    std::fs::write(dir.join(SHOWN), b"1")
}

/// Only these structured fields may leave the app. Free-form panic payloads,
/// thread names, paths and binary files are deliberately not inputs to sharing.
pub fn share_text(report: &str) -> String {
    let mut out = String::from("MeshLive crash report\n");
    for line in report.lines().take(128) {
        let allowed = if let Some(version) = line.strip_prefix("app_version=") {
            !version.is_empty() && version.len() <= 24 && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        } else if let Some(build) = line.strip_prefix("windows_build=") {
            build == "unknown" || build.parse::<u32>().is_ok()
        } else if let Some(kind) = line.strip_prefix("kind=") {
            matches!(kind, "panic" | "native")
        } else if let Some(code) = line.strip_prefix("exception=0x") {
            code.len() == 8 && code.bytes().all(|b| b.is_ascii_hexdigit())
        } else {
            let offset = line.strip_prefix("address=").unwrap_or(line);
            offset.strip_prefix("meshlive.exe+0x").is_some_and(|hex| !hex.is_empty() && hex.len() <= 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        };
        if allowed {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("summary=MeshLive closed unexpectedly. Stack entries are application code offsets only.\n");
    out
}

/// No recipient or attachment; Windows opens the user's email composer.
/// Sending remains the user's action in that application.
pub fn share_uri(report: &str) -> String {
    let text = share_text(report);
    let body: String = text.bytes().map(|b| format!("%{b:02X}")).collect();
    format!("mailto:?subject=MeshLive%20crash%20report&body={body}")
}

pub fn share_with(dir: &Path, open: &mut dyn FnMut(&str) -> bool) -> Result<(), String> {
    let report = newest(dir).ok_or("No crash report is available.")?;
    if !open(&share_uri(&report)) {
        return Err("Couldn't open your email app. The report is still available.".into());
    }
    mark_shown(dir).map_err(|_| "Couldn't mark the report as offered.".into())
}

pub fn share(dir: &Path) -> Result<(), String> {
    share_with(dir, &mut |uri| {
        use windows::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let uri = windows::core::HSTRING::from(uri);
        let result = unsafe { ShellExecuteW(None, windows::core::w!("open"), windows::core::PCWSTR(uri.as_ptr()), None, None, SW_SHOWNORMAL) };
        result.0 as isize > 32
    })
}

mod native {
    use std::{fs::File, os::windows::io::AsRawHandle, sync::mpsc};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
    use windows::Win32::System::Threading::GetCurrentProcess;

    struct DumpRequest {
        thread: u32,
        pointers: usize,
        done: mpsc::Sender<()>,
    }
    static DUMPS: super::OnceLock<mpsc::SyncSender<DumpRequest>> = super::OnceLock::new();
    static DUMP_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    // Prestarted, single DbgHelp owner: the failing thread keeps its exception
    // pointers alive until this worker has finished using them.
    pub fn start_dump_worker() {
        let (send, receive) = mpsc::sync_channel::<DumpRequest>(1);
        if std::thread::Builder::new().name("meshlive-crash".into()).spawn(move || {
            DUMP_THREAD.store(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }, std::sync::atomic::Ordering::Release);
            for request in receive {
                if let Some(dir) = super::DIR.get() {
                    let _ = dump_in(dir, request.thread, request.pointers);
                }
                let _ = request.done.send(());
            }
        }).is_ok() {
            let _ = DUMPS.set(send);
        }
    }

    pub fn capture(pointers: *const EXCEPTION_POINTERS) {
        let Some(send) = DUMPS.get() else { return };
        let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        if thread == DUMP_THREAD.load(std::sync::atomic::Ordering::Acquire) { return; }
        let (done, wait) = mpsc::channel();
        let request = DumpRequest {
            thread,
            pointers: pointers as usize,
            done,
        };
        if send.try_send(request).is_ok() {
            let _ = wait.recv();
        }
    }

    pub(super) fn dump_in(dir: &std::path::Path, thread: u32, pointers: usize) -> std::io::Result<()> {
        use windows::{core::BOOL, Win32::{Foundation::HANDLE, System::{Diagnostics::Debug::*, Threading::GetCurrentProcessId}}};
        let path = super::owned_slot(dir, (1..=super::DUMP_SLOTS).map(|n| format!("local-{n}.dmp")))?;
        let file = File::create(&path)?;
        let exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: thread, ExceptionPointers: pointers as *mut EXCEPTION_POINTERS, ClientPointers: BOOL(0),
        };
        let result = unsafe {
            MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), HANDLE(file.as_raw_handle()),
                MiniDumpFilterMemory | MiniDumpFilterModulePaths | MiniDumpWithoutOptionalData,
                (pointers != 0).then_some(&exception as *const _), None, None)
        };
        drop(file);
        if let Err(error) = result {
            let _ = std::fs::remove_file(path);
            return Err(std::io::Error::other(error));
        }
        Ok(())
    }

    /// (base, size) of meshlive.exe in memory.
    pub fn module_base() -> Option<(u64, u64)> {
        let module: HMODULE = unsafe { GetModuleHandleW(None) }.ok()?;
        let mut info = MODULEINFO::default();
        unsafe { GetModuleInformation(GetCurrentProcess(), module, &mut info, std::mem::size_of::<MODULEINFO>() as u32) }.ok()?;
        Some((info.lpBaseOfDll as u64, u64::from(info.SizeOfImage)))
    }
    /// The calling thread's return addresses (no memory contents).
    pub fn stack() -> Vec<String> {
        use windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
        let mut buffer = [std::ptr::null_mut(); super::FRAMES];
        let n = usize::from(unsafe { RtlCaptureStackBackTrace(1, &mut buffer, None) });
        let ips: Vec<u64> = buffer[..n].iter().map(|p| *p as u64).collect();
        super::frames(&ips)
    }
    pub fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(filter));
        }
    }
    /// A native crash: its code and address (as an exe offset) only. Windows
    /// Error Reporting still runs afterwards (continue search).
    unsafe extern "system" fn filter(pointers: *const EXCEPTION_POINTERS) -> i32 {
        let (code, address) = unsafe {
            pointers
                .as_ref()
                .and_then(|p| p.ExceptionRecord.as_ref())
                .map_or((0, 0), |r| (r.ExceptionCode.0 as u32, r.ExceptionAddress as u64))
        };
        let place = match module_base() {
            Some((start, size)) if address >= start && address < start + size => format!("meshlive.exe+0x{:x}", address - start),
            _ => "outside meshlive.exe".into(),
        };
        let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
        let text = super::report("native", &format!("exception=0x{code:08x}\naddress={place}\nthread={}", super::scrub(&thread)), &[]);
        super::write(&text);
        capture(pointers);
        if super::parity_enabled() { 1 } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_only_offers_allowlisted_text_and_a_failed_composer_keeps_it_pending() {
        let dir = crate::sealed::tests::scratch("crash-share");
        let report = report_with("0.1.0", Some(26200), "native",
            "exception=0xc0000005\naddress=meshlive.exe+0x1234\nmessage=private-runtime-data\nthread=private-thread",
            &["meshlive.exe+0x10".into(), "private-stack-data".into()]);
        write_in(&dir, &report).unwrap();
        std::fs::write(dir.join(FOLDER).join("local-1.dmp"), b"PRIVATE DUMP").unwrap();
        let shared = share_text(&report);
        assert!(shared.contains("app_version=0.1.0\nwindows_build=26200\nkind=native\n"));
        assert!(shared.contains("exception=0xc0000005") && shared.contains("meshlive.exe+0x10"));
        assert!(!shared.contains("private") && !shared.contains("PRIVATE"));
        assert!(share_with(&dir, &mut |_| false).is_err());
        assert!(!pending(&dir).is_empty());
        let mut opened = None;
        share_with(&dir, &mut |uri| { opened = Some(uri.to_owned()); true }).unwrap();
        assert_eq!(opened.unwrap(), share_uri(&report));
        assert!(pending(&dir).is_empty());
        assert_eq!(std::fs::read(dir.join(FOLDER).join("local-1.dmp")).unwrap(), b"PRIVATE DUMP");
        assert!(!share_text("app_version=private\nwindows_build=secret\nkind=private\nmeshlive.exe+0xprivate").contains("private"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_linked_crash_folder_receives_no_reports_or_dumps() {
        use std::os::windows::process::CommandExt;
        // Backslash-only paths: mklink rejects the scratch helper's forward slash.
        let nonce: String = crate::crypto::random_nonce().unwrap()[..4].iter().map(|b| format!("{b:02x}")).collect();
        let base = std::env::current_dir().unwrap().join(".artifacts").join(format!("crash-link-{nonce}"));
        let (dir, outside) = (base.join("app"), base.join("outside"));
        std::fs::create_dir_all(&dir).unwrap(); std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("local-1.dmp"), b"outside fixture").unwrap();
        let status = std::process::Command::new("cmd.exe").args(["/c", "mklink", "/J"])
            .arg(dir.join(FOLDER)).arg(&outside).creation_flags(0x08000000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "native fixture junction must be created");
        assert!(write_in(&dir, "kind=panic").is_err());
        let target = dir.clone();
        assert!(std::thread::spawn(move || native::dump_in(&target, 0, 0)).join().unwrap().is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(std::fs::read(outside.join("local-1.dmp")).unwrap(), b"outside fixture");
        std::fs::remove_dir(dir.join(FOLDER)).unwrap();
        assert!(std::fs::symlink_metadata(dir.join(FOLDER)).is_err(), "detach the junction before recursive cleanup");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn local_minidumps_keep_three_and_the_app_wipe_removes_every_dump() {
        let dir = crate::sealed::tests::scratch("native-dumps");
        for _ in 0..4 {
            let target = dir.clone();
            std::thread::spawn(move || native::dump_in(&target, 0, 0)).join().unwrap().unwrap();
        }
        for n in 1..=DUMP_SLOTS {
            let bytes = std::fs::read(dir.join(FOLDER).join(format!("local-{n}.dmp"))).unwrap();
            assert_eq!(&bytes[..4], b"MDMP", "a real Windows minidump");
        }
        assert_eq!(std::fs::read_dir(dir.join(FOLDER)).unwrap().count(), 3);
        let wiped = crate::wipe::run(&dir, &mut || Ok(false));
        assert!(wiped.complete && wiped.unknown.is_empty(), "{wiped:?}");
        assert!(!dir.join(FOLDER).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn native_exception_child() {
        let Some(dir) = std::env::var_os("MESHLIVE_CRASH_TEST_CHILD") else { return };
        unsafe {
            use windows::Win32::System::Diagnostics::Debug::*;
            SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        }
        install(dir.into(), "0.1.0");
        unsafe { windows::Win32::System::Diagnostics::Debug::RaiseException(0xe0000042, 1, None) };
        panic!("the native exception must terminate this child");
    }

    #[test]
    fn a_native_exception_writes_a_local_dump_and_offers_text_on_the_next_launch() {
        let dir = crate::sealed::tests::scratch("native-exception");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::tests::native_exception_child", "--nocapture"])
            .env("MESHLIVE_CRASH_TEST_CHILD", &dir).env("MESHLIVE_CRASH_PARITY", "1")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().unwrap();
        assert!(!status.success());
        let report = newest(&dir).expect("next-launch text report");
        assert!(report.contains("kind=native\nexception=0xe0000042"), "{report}");
        assert_eq!(pending(&dir).len(), 1);
        let dump = std::fs::read(dir.join(FOLDER).join("local-1.dmp")).unwrap();
        assert_eq!(&dump[..4], b"MDMP");
        assert!(share_text(&report).contains("kind=native"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scrub_removes_names_keys_codes_and_ids() {
        let text = scrub("called \"Asha K\" key=0011223344556677 code=BVMID2.QUJDREVGR0hJSktMTU5PUFFSU1RVVldY id 7f3a9c21 at C:\\Users\\Asha\\x");
        assert!(!text.contains("Asha"), "{text}");
        assert!(!text.contains("0011223344556677"), "{text}");
        assert!(!text.contains("QUJDREVGR0hJSktMTU5PUFFSU1RVVldY"), "{text}");
        assert!(!text.contains("7f3a9c21"), "{text}");
        assert!(text.contains("called \"<text>\" key=<hex>"), "{text}");
        assert!(scrub(&"x".repeat(1000)).chars().count() <= MESSAGE_CHARS);
        assert_eq!(scrub("index out of bounds: the len is 3 but the index is 7"), "index out of bounds: the len is 3 but the index is 7");
    }

    #[test]
    fn reports_ring_through_five_fixed_files_and_the_shown_marker_clears_pending() {
        let dir = std::env::temp_dir().join(format!("meshlive-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(pending(&dir).is_empty() && newest(&dir).is_none());
        let mut written = Vec::new();
        for n in 0..7 {
            written.push(write_in(&dir, &report_with("0.1.0", Some(26200), "panic", &format!("n={n}"), &[])).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let names: std::collections::BTreeSet<_> = written.iter().collect();
        assert_eq!(names.len(), SLOTS, "a ring of five fixed names");
        for path in &written {
            let relative = path.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/");
            assert!(files().contains(&relative), "{relative} is in the wipe allowlist");
        }
        assert!(newest(&dir).unwrap().contains("n=6"));
        assert_eq!(pending(&dir).len(), SLOTS);
        std::thread::sleep(std::time::Duration::from_millis(20));
        mark_shown(&dir).unwrap();
        assert!(pending(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_has_versions_kind_and_offsets_only() {
        let text = report_with("0.1.0", Some(26200), "native", "exception=0xc0000005\naddress=meshlive.exe+0x1234", &["meshlive.exe+0x10".into()]);
        assert!(text.starts_with("MeshLive crash report\napp_version=0.1.0\nwindows_build=26200\n"));
        assert!(text.contains("kind=native\nexception=0xc0000005"));
        assert!(text.contains("frames (meshlive.exe offsets, 1):\nmeshlive.exe+0x10\n"));
        assert!(frames(&[0x1234_5678]).iter().all(|f| f.starts_with("meshlive.exe+0x") || f == "other"));
        let here = native::stack();
        assert!(!here.is_empty(), "{here:?}");
    }
}
