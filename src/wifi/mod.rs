//! Wi-Fi Direct carrier for direct calls (Design WF, Windows host role). The
//! pure core (wire, keys, roles, limits, cutover model) is platform independent
//! and tested without a radio; the platform modules are added by W3.
pub mod address;
pub mod attempt;
pub mod binding;
pub mod cutover;
pub mod faults;
pub mod group;
pub mod guest;
pub mod guest_binding;
pub mod guest_platform;
pub mod keys;
pub mod listener;
pub mod platform;
pub mod probe;
pub mod windows;
pub mod wlan_guest;
pub mod limits;
pub mod log;
pub mod pulse;
pub mod quality;
pub mod roles;
pub mod socket;
pub mod wire;

#[cfg(test)]
mod vectors;
#[cfg(test)]
mod it_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_2_is_withheld_until_the_windows_join_is_proven() {
        let previous = std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").ok();
        unsafe {
            std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN");
            assert_eq!(host_join_proven(), HOST_JOIN_PROVEN);
            std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", "1");
            assert!(host_join_proven());
            match previous {
                Some(value) => std::env::set_var("MESHLIVE_WIFI_JOIN_PROVEN", value),
                None => std::env::remove_var("MESHLIVE_WIFI_JOIN_PROVEN"),
            }
        }
    }

    #[test]
    fn the_guest_role_follows_its_environment_arm_and_the_constant() {
        let previous = std::env::var("MESHLIVE_WIFI_GUEST").ok();
        unsafe {
            std::env::set_var("MESHLIVE_WIFI_GUEST", "1");
            assert!(guest_role());
            std::env::set_var("MESHLIVE_WIFI_GUEST", "0");
            assert!(!guest_role());
            std::env::remove_var("MESHLIVE_WIFI_GUEST");
            assert_eq!(guest_role(), GUEST_ROLE);
            match previous {
                Some(value) => std::env::set_var("MESHLIVE_WIFI_GUEST", value),
                None => std::env::remove_var("MESHLIVE_WIFI_GUEST"),
            }
        }
    }

    #[test]
    fn implemented_follows_the_environment_arms_and_the_constant() {
        let previous = std::env::var("MESHLIVE_WIFI_MARKER").ok();
        unsafe {
            std::env::set_var("MESHLIVE_WIFI_MARKER", "1");
            assert!(implemented());
            std::env::set_var("MESHLIVE_WIFI_MARKER", "0");
            assert!(!implemented());
            std::env::remove_var("MESHLIVE_WIFI_MARKER");
            assert_eq!(implemented(), SEND_WIFI_MARKER);
            match previous {
                Some(value) => std::env::set_var("MESHLIVE_WIFI_MARKER", value),
                None => std::env::remove_var("MESHLIVE_WIFI_MARKER"),
            }
        }
    }
}

/// The environment arm (README register): MESHLIVE_WIFI_DIRECT=1. W4 combines it with the Settings switch.
pub fn env_arm() -> bool {
    std::env::var("MESHLIVE_WIFI_DIRECT").as_deref() == Ok("1")
}

/// Mirrors A9's `WINDOWS_HOST_JOIN_PROVEN` (R-21): false until W0 (a) and the frequency-readable result of (e).
/// Both constants change together, after review. CAPS bit 2 is set only when `host_join_proven()` (amendment 20).
pub const HOST_JOIN_PROVEN: bool = false;

/// MESHLIVE_WIFI_JOIN_PROVEN=1: a test arm only (see docs/dev/switches.md).
pub fn host_join_proven() -> bool {
    HOST_JOIN_PROVEN || std::env::var("MESHLIVE_WIFI_JOIN_PROVEN").as_deref() == Ok("1")
}

/// W4G (design decision "Windows Wi-Fi Direct", 2026-11-06): the phone hosts the group and
/// this PC joins it. Off until its phone checks pass. While on, this PC's CAPS
/// offers the Wi-Fi Direct client bit (3) only, never the group-owner bit (2).
pub const GUEST_ROLE: bool = false;

/// MESHLIVE_WIFI_GUEST=1 turns the guest role on and =0 off; anything else follows
/// `GUEST_ROLE` (README register). It acts only with MESHLIVE_WIFI_DIRECT=1 and the WIFI marker.
pub fn guest_role() -> bool {
    match std::env::var("MESHLIVE_WIFI_GUEST").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => GUEST_ROLE,
    }
}

/// Master 5.2: every build that implements `BWF1` sends the WIFI marker.
/// false until W4 step 4.10, which follows W3's last step.
pub const SEND_WIFI_MARKER: bool = false;
pub fn implemented() -> bool {
    match std::env::var("MESHLIVE_WIFI_MARKER").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => SEND_WIFI_MARKER,
    }
}
