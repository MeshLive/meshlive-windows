//! Settings > Start with Windows (off by default). An unpackaged build
//! writes a value under HKCU\...\Run that starts it in the notification
//! area (`--background`). The MSIX package uses its StartupTask
//! (`MeshLiveStartup` in AppxManifest.xml); Windows also lets the user turn
//! that off in Task Manager or Settings > Apps > Startup, which then wins.
use std::path::Path;

pub const RUN_VALUE: &str = "MeshLive";
pub const BACKGROUND_ARG: &str = "--background";
pub const TASK_ID: &str = "MeshLiveStartup";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The Run value for `exe`: quoted, so a path with spaces starts.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// Turns starting at sign-in on or off. Ok(false): Windows keeps it off
/// (the user disabled it in Task Manager or by policy).
pub fn set(enabled: bool, packaged: bool) -> Result<bool, String> {
    if meshlive::app_mode::current().is_review() {
        return if enabled { Err("Start with Windows is unavailable in developer review mode".into()) } else { Ok(false) };
    }
    if packaged { set_task(enabled) } else { set_run_value(enabled).map(|()| enabled) }
}

fn set_task(enabled: bool) -> Result<bool, String> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    let task = StartupTask::GetAsync(&windows::core::HSTRING::from(TASK_ID)).and_then(|a| a.join()).map_err(|e| format!("{e:?}"))?;
    if enabled {
        let state = task.RequestEnableAsync().and_then(|a| a.join()).map_err(|e| format!("{e:?}"))?;
        Ok(matches!(state, StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy))
    } else {
        task.Disable().map_err(|e| format!("{e:?}"))?;
        Ok(false)
    }
}

fn set_run_value(enabled: bool) -> Result<(), String> {
    use windows::Win32::System::Registry::*;
    let key_name: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
    let value_name: Vec<u16> = RUN_VALUE.encode_utf16().chain(Some(0)).collect();
    let mut key = HKEY::default();
    // SAFETY: the current user's Run key, opened, written and closed here.
    unsafe {
        let opened = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key_name.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if opened.is_err() {
            return Err(format!("{opened:?}"));
        }
        let result = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let data: Vec<u16> = run_command(&exe).encode_utf16().chain(Some(0)).collect();
            let bytes = std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2);
            RegSetValueExW(key, windows::core::PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(bytes))
        } else {
            let deleted = RegDeleteValueW(key, windows::core::PCWSTR(value_name.as_ptr()));
            if deleted == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND { windows::Win32::Foundation::ERROR_SUCCESS } else { deleted }
        };
        let _ = RegCloseKey(key);
        result.ok().map_err(|e| format!("{e:?}"))
    }
}

/// Started by Windows at sign-in: `--background` (Run value), or the
/// package's StartupTask activation.
pub fn launched_at_sign_in(background_arg: bool, packaged: bool) -> bool {
    if background_arg {
        return true;
    }
    if !packaged {
        return false;
    }
    use windows::ApplicationModel::{Activation::ActivationKind, AppInstance};
    AppInstance::GetActivatedEventArgs().and_then(|args| args.Kind()).is_ok_and(|kind| kind == ActivationKind::StartupTask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_value_quotes_the_path_and_starts_in_the_background() {
        let exe = Path::new(r"C:\Program Files\MeshLive\meshlive.exe");
        assert_eq!(run_command(exe), "\"C:\\Program Files\\MeshLive\\meshlive.exe\" --background");
    }
}
