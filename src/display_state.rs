//! Display and session facts for the UI-loop trace (`ui_slow`): whether the
//! console display is on, off or dimmed (GUID_CONSOLE_DISPLAY_STATE through a
//! power-setting callback, no window message needed) and whether the input
//! desktop is reachable (it is not while the session is locked or the secure
//! desktop is up). Read-only; nothing here changes power or display state.
use std::sync::atomic::{AtomicU8, Ordering};
use windows::{
    Win32::{
        Foundation::HANDLE,
        System::{
            Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING, PowerSettingRegisterNotification},
            StationsAndDesktops::{CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP, OpenInputDesktop},
        },
        UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK,
    },
    core::GUID,
};

const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
/// 0 off, 1 on, 2 dimmed, 255 unknown.
static DISPLAY: AtomicU8 = AtomicU8::new(255);

unsafe extern "system" fn on_setting(_: *const core::ffi::c_void, kind: u32, setting: *const core::ffi::c_void) -> u32 {
    if kind == PBT_POWERSETTINGCHANGE && !setting.is_null() {
        // SAFETY: the system passes a POWERBROADCAST_SETTING for this event.
        let s = unsafe { &*(setting as *const POWERBROADCAST_SETTING) };
        if s.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && s.DataLength >= 1 {
            DISPLAY.store(s.Data[0], Ordering::Relaxed);
        }
    }
    0
}

/// Registers the display-state callback once (the system reports the
/// current state right away). Failures leave the state "unknown".
pub fn start() {
    static PARAMS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    PARAMS.get_or_init(|| {
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(on_setting), Context: std::ptr::null_mut() }));
        let mut handle = std::ptr::null_mut();
        // SAFETY: the parameters live for the process; the registration is never removed.
        let _ = unsafe {
            PowerSettingRegisterNotification(&GUID_CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_CALLBACK, HANDLE(params as *mut _ as *mut _), &mut handle)
        };
        0
    });
}

pub fn display() -> &'static str {
    match DISPLAY.load(Ordering::Relaxed) {
        0 => "off",
        1 => "on",
        2 => "dimmed",
        _ => "unknown",
    }
}

/// False while the input desktop cannot be opened (locked session, UAC or
/// sign-in screen).
pub fn input_desktop() -> bool {
    // SAFETY: plain query; the handle is closed at once.
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) } {
        Ok(desk) => {
            let _ = unsafe { CloseDesktop(desk) };
            true
        }
        Err(_) => false,
    }
}
