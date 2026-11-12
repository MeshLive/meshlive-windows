//! Read-only Wi-Fi facts. Nothing here changes a radio.
//!
//! The three WinRT/WLAN reads run on their own thread (`spawn`), never on the
//! call worker. The pure helpers are tested without a radio.

use crate::wifi::wire;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::{
    Devices::Radios::{Radio, RadioKind, RadioState},
    Networking::{
        Connectivity::NetworkInformation,
        NetworkOperators::{NetworkOperatorTetheringManager, TetheringOperationalState},
    },
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::WiFi::{
            WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle,
            WlanQueryInterface, wlan_intf_opcode_channel_number,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiRadio {
    On,
    Off,
    Missing,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    NotConnected,
    Ghz24,
    Ghz5Plus,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCaps {
    pub radio: WifiRadio,
    pub hotspot: Option<bool>,
    pub band: Band,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    WifiOff,
    NoWifi,
    Hotspot,
    Band24,
    BandUnknown,
    NotConnected,
}
impl Refusal {
    pub fn text(self) -> &'static str {
        match self {
            Self::WifiOff => "Wi-Fi is off. Turn it on to use Wi-Fi Direct.",
            Self::NoWifi => "This PC has no usable Wi-Fi adapter for Wi-Fi Direct.",
            Self::Hotspot => "Windows Mobile Hotspot is on. Turn it off to use Wi-Fi Direct.",
            Self::Band24 => {
                "This PC's Wi-Fi is on 2.4 GHz, which shares its channel with Bluetooth. Connect to a 5 GHz network or turn Wi-Fi off."
            }
            Self::BandUnknown => "Couldn't check which Wi-Fi band this PC uses.",
            Self::NotConnected => {
                "This PC's Wi-Fi isn't connected to a network, so a Wi-Fi Direct link would likely use 2.4 GHz. Connect this PC to a 5 GHz Wi-Fi network to use Wi-Fi Direct."
            }
        }
    }

    pub fn decline_reason(self) -> u8 {
        match self {
            Self::WifiOff | Self::NoWifi => wire::decline::WIFI_UNAVAILABLE,
            Self::Hotspot => wire::decline::BUSY,
            Self::Band24 | Self::BandUnknown | Self::NotConnected => wire::decline::NOT_5GHZ,
        }
    }
}

/// Amendment 16: true only if W0 (e) "client disconnected" put the group on
/// 5 GHz every time. Set by the person running the check.
pub const UNCONNECTED_OK: bool = false;
/// Amendment 16: pinned from W0 (e); false when W0 recorded a consent prompt
/// for the `--sta` read (then the band is Unknown and WLAN is never called).
pub const READ_STA_BAND: bool = true;
/// The W0 decision (e): the phone's DECLINE 7 check protects the data path.
pub const UNKNOWN_BAND_OK: bool = true;

/// `None` and 0 mean "no channel"; 1..=14 are 2.4 GHz, higher channels 5 GHz+.
pub fn band_from_channel(channel: Option<u32>) -> Band {
    match channel {
        None | Some(0) => Band::NotConnected,
        Some(1..=14) => Band::Ghz24,
        Some(_) => Band::Ghz5Plus,
    }
}

pub fn eligibility(caps: &WifiCaps, unknown_band_ok: bool) -> Result<(), Refusal> {
    match caps.radio {
        WifiRadio::Off => return Err(Refusal::WifiOff),
        WifiRadio::Missing | WifiRadio::Unknown => return Err(Refusal::NoWifi),
        WifiRadio::On => {}
    }
    if caps.hotspot == Some(true) {
        return Err(Refusal::Hotspot);
    }
    match caps.band {
        Band::Ghz24 => Err(Refusal::Band24),
        Band::NotConnected if !UNCONNECTED_OK => Err(Refusal::NotConnected),
        Band::Unknown if !unknown_band_ok => Err(Refusal::BandUnknown),
        _ => Ok(()),
    }
}

pub fn probe_blocking(deadline: Instant, cancelled: &AtomicBool) -> WifiCaps {
    let _apartment = crate::bluetooth::Apartment::new();
    let radio = probe_radio(deadline, cancelled);
    let hotspot = probe_hotspot(deadline, cancelled);
    let band = if READ_STA_BAND {
        probe_band()
    } else {
        Band::Unknown
    };
    WifiCaps {
        radio,
        hotspot,
        band,
    }
}

fn probe_radio(deadline: Instant, cancelled: &AtomicBool) -> WifiRadio {
    let operation = match Radio::GetRadiosAsync() {
        Ok(operation) => operation,
        Err(_) => return WifiRadio::Unknown,
    };
    let list = match crate::bluetooth::wait_with(operation, deadline, || {
        cancelled.load(Ordering::Acquire)
    }) {
        Ok(list) => list,
        Err(_) => return WifiRadio::Unknown,
    };
    let size = match list.Size() {
        Ok(size) => size,
        Err(_) => return WifiRadio::Unknown,
    };
    let mut seen = false;
    let mut on = false;
    for index in 0..size {
        let radio = match list.GetAt(index) {
            Ok(radio) => radio,
            Err(_) => return WifiRadio::Unknown,
        };
        if radio.Kind().ok() != Some(RadioKind::WiFi) {
            continue;
        }
        seen = true;
        if radio.State().ok() == Some(RadioState::On) {
            on = true;
        }
    }
    if !seen {
        WifiRadio::Missing
    } else if on {
        WifiRadio::On
    } else {
        WifiRadio::Off
    }
}

fn probe_hotspot(_deadline: Instant, _cancelled: &AtomicBool) -> Option<bool> {
    let profile = match NetworkInformation::GetInternetConnectionProfile() {
        Ok(profile) => profile,
        // No internet profile: Windows cannot have the hotspot tied to it.
        Err(_) => return Some(false),
    };
    let manager = match NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile) {
        Ok(manager) => manager,
        Err(_) => return None,
    };
    match manager.TetheringOperationalState() {
        Ok(TetheringOperationalState::On) | Ok(TetheringOperationalState::InTransition) => Some(true),
        Ok(TetheringOperationalState::Off) => Some(false),
        Ok(_) | Err(_) => None,
    }
}

fn probe_band() -> Band {
    unsafe {
        let mut negotiated = 0u32;
        let mut handle = HANDLE::default();
        if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != 0 {
            return Band::Unknown;
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let code = WlanEnumInterfaces(handle, None, &mut list);
        let result = if code != 0 {
            Band::Unknown
        } else if list.is_null() {
            Band::NotConnected
        } else if (*list).dwNumberOfItems == 0 {
            Band::NotConnected
        } else {
            let info = &*(*list).InterfaceInfo.as_ptr();
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            let query = WlanQueryInterface(
                handle,
                &info.InterfaceGuid,
                wlan_intf_opcode_channel_number,
                None,
                &mut size,
                &mut data,
                None,
            );
            let band = if query == 0 && !data.is_null() {
                band_from_channel(Some(*(data as *const u32)))
            } else if query == 5023 {
                // ERROR_INVALID_STATE: the WLAN service reports "not connected".
                // Pinned from W0 check (e) (the probe report names the exact code).
                Band::NotConnected
            } else {
                Band::Unknown
            };
            if !data.is_null() {
                WlanFreeMemory(data);
            }
            band
        };
        if !list.is_null() {
            WlanFreeMemory(list as *const core::ffi::c_void);
        }
        let _ = WlanCloseHandle(handle, None);
        result
    }
}

/// The probe runs on its own thread; it never touches the call worker.
pub fn spawn(cancelled: Arc<AtomicBool>) -> mpsc::Receiver<WifiCaps> {
    let (sender, receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("meshlive-wifi-probe".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let caps = probe_blocking(deadline, &cancelled);
            let _ = sender.send(caps);
        });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(radio: WifiRadio, hotspot: Option<bool>, band: Band) -> WifiCaps {
        WifiCaps {
            radio,
            hotspot,
            band,
        }
    }

    #[test]
    fn channels_one_to_fourteen_are_2_4_ghz_and_higher_are_5_ghz_or_above() {
        for channel in 1..=14 {
            assert_eq!(band_from_channel(Some(channel)), Band::Ghz24, "{channel}");
        }
        for channel in [15, 36, 100, 149, 169] {
            assert_eq!(band_from_channel(Some(channel)), Band::Ghz5Plus, "{channel}");
        }
        assert_eq!(band_from_channel(None), Band::NotConnected);
        assert_eq!(band_from_channel(Some(0)), Band::NotConnected);
    }

    #[test]
    fn eligibility_refuses_in_the_documented_order() {
        assert_eq!(
            eligibility(&caps(WifiRadio::Off, Some(true), Band::Ghz24), true),
            Err(Refusal::WifiOff)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::Missing, Some(true), Band::Ghz24), true),
            Err(Refusal::NoWifi)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::Unknown, Some(true), Band::Ghz24), true),
            Err(Refusal::NoWifi)
        );
        // Hotspot beats the band.
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(true), Band::Ghz24), true),
            Err(Refusal::Hotspot)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(false), Band::Ghz24), true),
            Err(Refusal::Band24)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(false), Band::NotConnected), true),
            Err(Refusal::NotConnected)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(false), Band::Unknown), true),
            Ok(())
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(false), Band::Unknown), false),
            Err(Refusal::BandUnknown)
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, None, Band::Ghz5Plus), false),
            Ok(())
        );
        assert_eq!(
            eligibility(&caps(WifiRadio::On, Some(false), Band::Ghz5Plus), false),
            Ok(())
        );
    }

    #[test]
    fn each_refusal_has_text_and_a_decline_reason() {
        for (refusal, reason) in [
            (Refusal::WifiOff, 3),
            (Refusal::NoWifi, 3),
            (Refusal::Hotspot, 2),
            (Refusal::Band24, 7),
            (Refusal::BandUnknown, 7),
            (Refusal::NotConnected, 7),
        ] {
            assert!(!refusal.text().is_empty());
            assert_eq!(refusal.decline_reason(), reason);
        }
    }
}
