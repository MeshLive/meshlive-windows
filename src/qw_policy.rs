//! Design QW review R5: the PC announces itself only on a platform whose
//! advertising address is measured to rotate with the payload. Windows
//! exposes no advertising-address API, so this is a fail-closed allowlist
//! (the Windows counterpart of Android's DeviceFamilyPolicy registry):
//! matcher, behaviour, sources, evidence, confidence and an off-switch.
//! An unlisted platform does not announce, and the UI says why.

/// One allowlisted platform family.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: &'static str,
    /// USB vendor ID of the Bluetooth adapter.
    pub adapter_vid: u16,
    /// Lowest Windows build (the secondary-PHY API needs 24H2, build 26100).
    pub min_build: u32,
    pub behaviour: &'static str,
    pub sources: &'static [&'static str],
    pub evidence: &'static [&'static str],
    pub confidence: &'static str,
}

pub const ENTRIES: &[Entry] = &[Entry {
    id: "intel-usb-win11-24h2",
    adapter_vid: 0x8087,
    min_build: 26100,
    behaviour: "Connectable extended (LE 1M secondary) announcement; random address, new at every provider start, and Windows' own rotation every 15 min; restart at every rotating-ID change",
    sources: &[
        "Microsoft Learn: GattServiceProviderAdvertisingParameters.UseLowEnergyUncoded1MPhyAsSecondaryPhy (Windows 11 24H2, build 26100)",
        "Microsoft Learn: GattServiceProviderAdvertisementStatus (StartedWithoutAllAdvertisementData)",
        "Bluetooth Core 5.4 Vol 6 Part B 1.3.2 (random private addresses), Vol 3 Part C Appendix A TGAP(private_addr_int) 15 min",
    ],
    evidence: &[
        "gate 1 matrix, two Android raw observers (S22 Android 16, ASUS), 2026-10-20: only extended carries the 9-byte payload; every start a new random address",
        "gate 2 hold, 2026-10-20: one random address 01:31:13-01:46:12, rotated by Windows after exactly 15 min; a restart changed address and payload at the same moment; no address carried two payloads",
    ],
    confidence: "one adapter measured (an Intel USB Bluetooth adapter in a development laptop); other Intel adapters inferred",
}];

/// Experimental, off by default: `MESHLIVE_QW_UNLISTED=1` lets an unlisted
/// adapter announce so a new family can be measured. Never for users.
pub const UNLISTED_SWITCH: &str = "MESHLIVE_QW_UNLISTED";
/// Off-switch: `MESHLIVE_QW=0` never announces (outgoing calls only).
pub const OFF_SWITCH: &str = "MESHLIVE_QW";

pub const NOT_LISTED: &str = "This PC's Bluetooth adapter hasn't been checked for private addresses yet, so it won't announce itself. Calling a phone still works.";
pub const OLD_WINDOWS: &str = "Receiving calls needs Windows 11 version 24H2 or newer. Calling a phone still works.";
pub const SWITCHED_OFF: &str = "Announcing this PC is switched off (MESHLIVE_QW=0).";

/// The USB vendor ID in an adapter device ID (`...VID_8087&PID_0026...`).
pub fn vendor_id(device_id: &str) -> Option<u16> {
    let upper = device_id.to_ascii_uppercase();
    let at = upper.find("VID_")? + 4;
    u16::from_str_radix(upper.get(at..at + 4)?, 16).ok()
}

/// The verdict (pure): the matching entry, or the reason shown to the user.
pub fn verdict(device_id: Option<&str>, os_build: Option<u32>, off: bool, unlisted_allowed: bool) -> Result<&'static str, &'static str> {
    if off {
        return Err(SWITCHED_OFF);
    }
    let build = os_build.unwrap_or(0);
    if build < ENTRIES.iter().map(|e| e.min_build).min().unwrap_or(u32::MAX) {
        return Err(OLD_WINDOWS);
    }
    let vid = device_id.and_then(vendor_id);
    if let Some(entry) = ENTRIES.iter().find(|e| Some(e.adapter_vid) == vid && build >= e.min_build) {
        return Ok(entry.id);
    }
    if unlisted_allowed {
        return Ok("unlisted-experimental");
    }
    Err(NOT_LISTED)
}

/// Windows' build number (read only).
pub fn os_build() -> Option<u32> {
    use windows::{
        Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        core::HSTRING,
    };
    let mut buffer = [0u16; 32];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            &HSTRING::from("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]).trim().parse().ok()
}

/// The live verdict from this PC's adapter, Windows build and switches.
pub fn current(device_id: Option<&str>) -> Result<&'static str, &'static str> {
    let env = |name: &str| std::env::var(name).ok();
    verdict(device_id, os_build(), env(OFF_SWITCH).as_deref() == Some("0"), env(UNLISTED_SWITCH).as_deref() == Some("1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    const INTEL: &str = r"\\?\USB#VID_8087&PID_0026#5&0000000&0&1#{92383b0e-f90e-4ac9-8d44-8c2d0d0ebda2}";
    #[test]
    fn only_a_measured_family_on_24h2_announces_and_everything_else_fails_closed() {
        assert_eq!(vendor_id(INTEL), Some(0x8087));
        assert_eq!(vendor_id("usb#vid_0a12&pid_0001"), Some(0x0a12));
        assert_eq!(vendor_id(r"\\?\ACPI#QCOM"), None);
        assert_eq!(verdict(Some(INTEL), Some(26200), false, false), Ok("intel-usb-win11-24h2"));
        assert_eq!(verdict(Some(INTEL), Some(22631), false, false), Err(OLD_WINDOWS));
        assert_eq!(verdict(Some(INTEL), None, false, false), Err(OLD_WINDOWS));
        assert_eq!(verdict(Some("USB#VID_0A12&PID_0001"), Some(26200), false, false), Err(NOT_LISTED));
        assert_eq!(verdict(None, Some(26200), false, false), Err(NOT_LISTED));
        assert_eq!(verdict(Some(INTEL), Some(26200), true, false), Err(SWITCHED_OFF), "the off-switch wins");
        assert_eq!(verdict(None, Some(26200), false, true), Ok("unlisted-experimental"));
        for e in ENTRIES {
            assert!(!e.sources.is_empty() && !e.evidence.is_empty() && !e.confidence.is_empty(), "{}", e.id);
        }
    }
}
