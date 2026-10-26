//! Keep this PC awake during a call (CF-LIFE-09/11, Windows mechanism): a
//! named power request, `PowerCreateRequest` + `PowerSetRequest(
//! PowerRequestSystemRequired)`, held only while a call exists and cleared
//! when it ends. It shows by name in `powercfg /requests`, unlike
//! `SetThreadExecutionState`, and keeps the system (not the display) on, so
//! the 2 s link heartbeat keeps running on an idle PC (Microsoft Learn:
//! PowerSetRequest; "System sleep criteria"). The call audio is still a
//! NullSink, so Windows' own audio-stream keep-awake does not apply yet.
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Power::{PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest},
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
    },
    core::PWSTR,
};

pub const REASON: &str = "MeshLive call in progress";

pub struct KeepAwake(HANDLE);
impl KeepAwake {
    pub fn new(reason: &str) -> windows::core::Result<Self> {
        let mut text: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
        let context = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(text.as_mut_ptr()),
            },
        };
        // The string is copied by PowerCreateRequest.
        let handle = unsafe { PowerCreateRequest(&context) }?;
        if let Err(error) = unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
        Ok(Self(handle))
    }
}
impl Drop for KeepAwake {
    fn drop(&mut self) {
        unsafe {
            let _ = PowerClearRequest(self.0, PowerRequestSystemRequired);
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_power_request_is_held_and_released() {
        let held = super::KeepAwake::new("MeshLive test power request").unwrap();
        drop(held);
    }
}
