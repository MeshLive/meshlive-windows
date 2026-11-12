//! Security-relevant control arms (audit SEC-13). A control arm that turns a
//! protection off (the call media gate, the private-address allowlist, the
//! private diagnostics export, the capture and camera fences, the wipe's
//! storage guard, a discoverable call offer) is read only in a debug build or
//! a `--features test-hooks` build, the way the test hooks are compiled out.
//! A release build behaves as if the variable were not set, as Android's
//! release builds refuse benchmark launch extras. Ordinary experiments and
//! switches that only add or remove behaviour without weakening a protection
//! keep their own environment reads.

/// True only where security-relevant control arms are honoured: a debug build
/// or a build with the `test-hooks` feature. Never true in the release app.
pub const READABLE: bool = cfg!(any(debug_assertions, feature = "test-hooks"));

/// What this build is, for the package scripts (`packaging/Assert-ReleaseExe.ps1`):
/// they package only an exe whose marker says SEALED (no arm is read). The
/// app references it at start so it stays in the exe.
pub static BUILD_MARKER: &str = if READABLE { "MESHLIVE-PROTECTION-ARMS=READABLE" } else { "MESHLIVE-PROTECTION-ARMS=SEALED" };

/// The value of the control arm `name`, or None (as if it were not set) in a
/// release build without test hooks.
pub fn var(name: &str) -> Option<String> {
    gated(READABLE, name, |name| std::env::var(name).ok())
}

/// `lookup(name)` only when `readable`; otherwise None, as with no variable.
pub fn gated(readable: bool, name: &str, lookup: impl FnOnce(&str) -> Option<String>) -> Option<String> {
    if readable { lookup(name) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protection-off values of every gated arm.
    const OFF: &[(&str, &str)] = &[
        ("MESHLIVE_CALL_MEDIA_GATE", "0"),
        ("MESHLIVE_QW_UNLISTED", "1"),
        ("MESHLIVE_SOURCE_CAMERA_FENCE", "0"),
        ("MESHLIVE_WIPE_STORAGE_GUARD", "0"),
        ("MESHLIVE_COMPACT_DISCOVERABLE", "1"),
        ("MESHLIVE_CALL_CAPTURE_FENCE", "0"),
        ("MESHLIVE_PRIVATE_DIAGNOSTICS", "0"),
        ("MESHLIVE_WATCH_AUDIO_FENCE", "0"),
    ];
    fn off(name: &str) -> Option<String> {
        OFF.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }
    /// An adapter that is not on the private-address allowlist (Realtek).
    const UNLISTED: &str = r"\\?\USB#VID_0BDA&PID_8771#00E04C000001#{92383b0e-f90e-4ac9-8d44-8c2d0d0ebda2}";

    #[test]
    fn a_release_configuration_with_the_arms_set_gates_media_and_never_announces_an_unlisted_adapter() {
        use crate::call::{MEDIA_GATE_SWITCH, media_gate_from};
        // Media gate: the release reading of "=0" is the gate on, exactly as with no variable.
        assert!(media_gate_from(gated(false, MEDIA_GATE_SWITCH, off).as_deref()));
        assert_eq!(media_gate_from(gated(false, MEDIA_GATE_SWITCH, off).as_deref()), media_gate_from(None));
        // A build that reads the arm does turn the gate off, so the release result is not a constant.
        assert!(!media_gate_from(gated(true, MEDIA_GATE_SWITCH, off).as_deref()));
        // Unlisted adapter: the release build refuses exactly as with no variable.
        let none = |_: &str| -> Option<String> { None };
        assert!(crate::qw_policy::current_in(false, Some(UNLISTED), &off).is_err());
        assert_eq!(crate::qw_policy::current_in(false, Some(UNLISTED), &off), crate::qw_policy::current_in(false, Some(UNLISTED), &none));
        if crate::qw_policy::os_build().is_some_and(|build| build >= 26100) {
            assert!(crate::qw_policy::current_in(true, Some(UNLISTED), &off).is_ok(), "a reading build would announce");
        }
    }

    /// The real process environment through the app's own functions: in a
    /// release build (the release test run) every arm set to its protection-off value
    /// changes nothing. Set only where the arms are unreadable, so no other
    /// test can observe the variables.
    #[test]
    fn with_every_protection_arm_set_the_release_app_keeps_every_protection() {
        if READABLE {
            return;
        }
        let before: Vec<_> = OFF.iter().map(|(key, _)| (*key, std::env::var_os(key))).collect();
        for (key, value) in OFF {
            unsafe { std::env::set_var(key, value) };
        }
        let gate = crate::call::media_gate_enabled();
        let unlisted = crate::qw_policy::current(Some(UNLISTED));
        let camera_fence = crate::stream_source::camera_fence_enabled();
        let wipe_guard = crate::wipe::storage_guard_enabled();
        let discoverable = crate::bluetooth::advertise::compact_discoverable();
        let watch_fence = crate::call_audio::watch_fence_enabled();
        for (key, value) in before {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        assert!(gate, "media gate on");
        assert!(unlisted.is_err(), "no announcement on an unlisted adapter");
        assert!(camera_fence, "camera fence on");
        assert!(wipe_guard, "wipe storage guard on");
        assert!(!discoverable, "call offer not discoverable");
        assert!(watch_fence, "Watch sound stops on Leave and End");
    }
}
