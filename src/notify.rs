//! An incoming call while the window is in the background: a Windows toast
//! (the packaged app has an identity, so `CreateToastNotifier` works; an
//! unpackaged development build may have none, and then only the taskbar
//! flash shows). The toast says only "Incoming call" and the name the ring
//! screen shows; nothing is kept. Failures are traced, never fatal.
//!
//! An unpackaged release (the tester zip) has no package identity, so its
//! toasts go out under the app ID `io.github.meshlive.app`, which Windows
//! accepts only from an app with a Start-menu shortcut carrying that ID: the
//! app creates `MeshLive.lnk` in the user's Start menu once (never
//! overwritten). Without it `CreateToastNotifier` fails with 0x80070490 and
//! the in-app ring and taskbar flash remain.
use std::sync::atomic::{AtomicBool, Ordering};
use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    core::HSTRING,
};

pub const AUMID: &str = "io.github.meshlive.app";
static UNPACKAGED: AtomicBool = AtomicBool::new(false);
/// The process runs without a package identity (toasts use `AUMID`).
pub fn set_unpackaged(unpackaged: bool) {
    UNPACKAGED.store(unpackaged, Ordering::Relaxed);
}

/// The Start-menu shortcut that lets an unpackaged MeshLive show toasts.
#[cfg_attr(feature = "test-hooks", allow(dead_code))] // test-hooks builds create no shortcut
pub fn start_shortcut_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|d| std::path::PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs\MeshLive.lnk"))
}
/// Creates the shortcut once (true when created; an existing one is kept).
#[cfg_attr(feature = "test-hooks", allow(dead_code))] // test-hooks builds create no shortcut
pub fn ensure_start_shortcut() -> Result<bool, String> {
    let path = start_shortcut_path().ok_or("no APPDATA")?;
    if path.exists() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    create_shortcut(&path, &exe, AUMID).map(|()| true)
}
/// A `.lnk` to `exe` carrying `System.AppUserModel.ID = aumid`.
pub fn create_shortcut(path: &std::path::Path, exe: &std::path::Path, aumid: &str) -> Result<(), String> {
    use windows::Win32::{
        Foundation::PROPERTYKEY,
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, StructuredStorage::PROPVARIANT},
        UI::Shell::{IShellLinkW, PropertiesSystem::IPropertyStore, ShellLink},
    };
    use windows::core::{Interface, PCWSTR, PWSTR};
    // System.AppUserModel.ID (propkey.h).
    const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid: 5 };
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(Some(0)).collect()
    };
    let run = || -> windows::core::Result<()> {
        // S_FALSE (already initialised) is fine; a different mode is too.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let exe_w = wide(exe.as_os_str());
        unsafe { link.SetPath(PCWSTR(exe_w.as_ptr()))? };
        if let Some(dir) = exe.parent() {
            let dir_w = wide(dir.as_os_str());
            unsafe { link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()))? };
        }
        let mut id: Vec<u16> = aumid.encode_utf16().chain(Some(0)).collect();
        let mut value = PROPVARIANT::default();
        // SAFETY: a VT_LPWSTR pointing at `id`, alive until after Commit; the
        // PROPVARIANT is never cleared (it owns nothing).
        unsafe {
            (*value.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_LPWSTR;
            (*value.Anonymous.Anonymous).Anonymous.pwszVal = PWSTR(id.as_mut_ptr());
        }
        let store: IPropertyStore = link.cast()?;
        unsafe {
            store.SetValue(&PKEY_APP_USER_MODEL_ID, &value)?;
            store.Commit()?;
        }
        std::mem::forget(value);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file: IPersistFile = link.cast()?;
        let path_w = wide(path.as_os_str());
        unsafe { file.Save(PCWSTR(path_w.as_ptr()), true) }
    };
    run().map_err(|e| format!("{:#010x}", e.code().0))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

pub fn toast_xml(title: &str, body: Option<&str>) -> String {
    format!(
        "<toast scenario=\"incomingCall\"><visual><binding template=\"ToastGeneric\"><text>{}</text>{}</binding></visual></toast>",
        escape(title),
        body.map(|b| format!("<text>{}</text>", escape(b))).unwrap_or_default()
    )
}

/// What Windows says about interrupting the user now (SHQueryUserNotificationState).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// Normal: notifications accepted.
    Normal,
    /// Locked, screen saver or away (QUNS_NOT_PRESENT), or the input desktop is not ours.
    Locked,
    /// Presenting or sharing the screen (QUNS_PRESENTATION_MODE).
    Presenting,
    /// Do not disturb / quiet hours (QUNS_QUIET_TIME).
    Quiet,
    /// A full-screen app (QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN).
    FullScreen,
}
pub fn attention() -> Attention {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    if !crate::display_state::input_desktop() {
        return Attention::Locked;
    }
    match unsafe { SHQueryUserNotificationState() }.map(|s| s.0) {
        Ok(1) => Attention::Locked,
        Ok(2) | Ok(3) => Attention::FullScreen,
        Ok(4) => Attention::Presenting,
        Ok(6) => Attention::Quiet,
        _ => Attention::Normal,
    }
}
/// CF-PRIV-23: a locked, shared or presented screen gets a neutral alert
/// ("Incoming call", no name); otherwise the name the ring screen shows.
pub fn toast_text(attention: Attention, peer: &str, video: bool) -> (String, Option<String>) {
    match attention {
        Attention::Locked | Attention::Presenting => ("Incoming call".into(), None),
        _ => ((if video { "Incoming video call" } else { "Incoming voice call" }).into(), Some(shown_name(peer))),
    }
}
/// CF-PRIV-23 (Android `NotificationPrivacy.MAX_PEER_NAME_CHARS`): the name in
/// an alert loses its control and direction-changing characters and is cut to
/// 40 characters.
pub const MAX_PEER_NAME_CHARS: usize = 40;
pub fn shown_name(peer: &str) -> String {
    let format = |c: char| matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2069 | 0xFEFF);
    peer.chars().filter(|&c| !c.is_control() && !format(c)).take(MAX_PEER_NAME_CHARS).collect()
}
/// CF-CALL-25 on Windows: the ring sounds unless the user asked for quiet
/// (do not disturb) or is presenting.
pub fn ring_allowed(attention: Attention) -> bool {
    !matches!(attention, Attention::Quiet | Attention::Presenting)
}

pub fn incoming_call(peer: &str, video: bool) -> Result<(), String> {
    // Review builds keep the in-app ring/attention without the installed app's toast identity.
    if meshlive::app_mode::current().is_review() { return Ok(()); }
    let (title, body) = toast_text(attention(), peer, video);
    let xml_text = toast_xml(&title, body.as_deref());
    let show = || -> windows::core::Result<()> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(xml_text.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        let notifier = if UNPACKAGED.load(Ordering::Relaxed) {
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?
        } else {
            ToastNotificationManager::CreateToastNotifier()?
        };
        notifier.Show(&toast)
    };
    show().map_err(|e| format!("{:#010x}", e.code().0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_start_shortcut_carries_the_app_id() {
        let dir = std::env::temp_dir().join(format!("meshlive-lnk-{}", std::process::id()));
        let path = dir.join("MeshLive.lnk");
        let exe = std::env::current_exe().unwrap();
        create_shortcut(&path, &exe, "io.github.meshlive.app.test").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        // A shell link (header size 0x4C) whose property store holds the ID.
        assert_eq!(&bytes[..4], &[0x4c, 0, 0, 0]);
        let utf16: Vec<u8> = "io.github.meshlive.app.test".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert!(bytes.windows(utf16.len()).any(|w| w == utf16.as_slice()), "the AUMID is stored in the link");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_locked_or_shared_screen_gets_no_name_and_quiet_time_does_not_ring() {
        assert_eq!(toast_text(Attention::Locked, "Asha", true), ("Incoming call".into(), None));
        assert_eq!(toast_text(Attention::Presenting, "Asha", false), ("Incoming call".into(), None));
        assert_eq!(toast_text(Attention::Normal, "Asha", true), ("Incoming video call".into(), Some("Asha".into())));
        assert!(ring_allowed(Attention::Normal) && ring_allowed(Attention::Locked) && ring_allowed(Attention::FullScreen));
        assert!(!ring_allowed(Attention::Quiet) && !ring_allowed(Attention::Presenting));
        let long = "A\u{7}sha \u{202e}".to_owned() + &"x".repeat(60);
        let shown = toast_text(Attention::Normal, &long, false).1.unwrap();
        assert_eq!(shown.chars().count(), MAX_PEER_NAME_CHARS);
        assert!(shown.starts_with("Asha x"), "control and bidi characters removed: {shown:?}");
    }
    #[test]
    fn the_toast_escapes_the_name() {
        let xml = toast_xml("Incoming voice call", Some("<Asha & \"Ravi\">"));
        assert!(xml.contains("&lt;Asha &amp; &quot;Ravi&quot;&gt;"));
        assert!(xml.contains("Incoming voice call"));
        assert!(!toast_xml("Incoming call", None).contains("<text></text>"));
        assert!(!xml.contains("<Asha"));
    }
}
