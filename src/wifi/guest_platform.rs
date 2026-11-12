//! W4G: the guest role's platform boundary and its pure rules (the band after joining,
//! the CREDS address, the joined subnet, the temporary profile). Nothing here touches a
//! radio; `wlan_guest` is the Windows implementation and `fake` the test one.

use crate::wifi::{
    address::{AdapterInfo, AddressInfo, GroupAddress},
    limits::MIN_5GHZ_MHZ,
    platform::{PlatformError, ProbeState},
    probe::{self, Band, WifiCaps, WifiRadio},
    socket::WifiSocket,
    wire::decline,
};
use std::net::{Ipv4Addr, SocketAddrV4};
use zeroize::Zeroizing;

/// The per-attempt network name and password, derived from the call's `UW` and the HOST
/// ticket. Moved into the platform, which drops (and so wipes) them right after the join
/// request. Never stored by the owner.
pub struct JoinRequest {
    pub ssid: Zeroizing<String>,
    pub psk: Zeroizing<String>,
}
impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("ssid", &"<redacted>")
            .field("psk", &"<redacted>")
            .finish()
    }
}

/// What the platform reports, stamped with the generation the owner gave the join.
pub enum JoinEvent {
    /// The group's network connected; `channel` is the joined channel when readable.
    Joined { generation: u32, channel: Option<u32> },
    /// The network refused or never answered the join (a peer-side failure: DECLINE 6).
    JoinFailed { generation: u32 },
    /// This PC's platform refused to start the join (a local fault: DECLINE 3).
    JoinRefused { generation: u32 },
    /// The group's network dropped after it had connected.
    Left { generation: u32 },
    /// A join that completed after its leave was disconnected again by the platform.
    LateJoinUndone { generation: u32 },
    Connected { generation: u32, socket: WifiSocket },
    ConnectRefused { generation: u32 },
    ConnectFailed { generation: u32 },
}
impl JoinEvent {
    pub fn generation(&self) -> u32 {
        match self {
            Self::Joined { generation, .. }
            | Self::JoinFailed { generation }
            | Self::JoinRefused { generation }
            | Self::Left { generation }
            | Self::LateJoinUndone { generation }
            | Self::Connected { generation, .. }
            | Self::ConnectRefused { generation }
            | Self::ConnectFailed { generation } => *generation,
        }
    }
}
impl std::fmt::Debug for JoinEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Joined { .. } => "Joined",
            Self::JoinFailed { .. } => "JoinFailed",
            Self::JoinRefused { .. } => "JoinRefused",
            Self::Left { .. } => "Left",
            Self::LateJoinUndone { .. } => "LateJoinUndone",
            Self::Connected { .. } => "Connected",
            Self::ConnectRefused { .. } => "ConnectRefused",
            Self::ConnectFailed { .. } => "ConnectFailed",
        };
        let mut out = f.debug_struct(name);
        out.field("generation", &self.generation());
        if let Self::Joined { channel, .. } = self {
            out.field("channel", channel);
        }
        if let Self::Connected { .. } = self {
            out.field("socket", &"<redacted>");
        }
        out.finish()
    }
}

/// The guest role's platform. Every method returns at once; results come from `poll`.
pub trait GuestPlatform {
    /// `Ready` once the read-only probe (Wi-Fi radio, Mobile Hotspot) answered.
    fn probe(&mut self) -> ProbeState;
    /// Starts joining the group named in `request` (consumed and wiped). `Err(InUse)`: the
    /// adapter is busy; any other `Err`: the platform refused (a local fault).
    fn join(&mut self, generation: u32, request: JoinRequest) -> Result<(), PlatformError>;
    fn poll(&mut self) -> Vec<JoinEvent>;
    /// The GUID text of the Wi-Fi interface that joins (as `AdapterInfo.guid` spells it).
    fn interface_guid(&mut self) -> Option<String>;
    /// Every IPv4 adapter (read-only; `address::adapters`).
    fn adapters(&mut self) -> Vec<AdapterInfo>;
    /// Starts one TCP connect from `local` to `remote`; the result comes from `poll`.
    fn connect(&mut self, generation: u32, local: Ipv4Addr, remote: SocketAddrV4) -> Result<(), PlatformError>;
    /// Leaves the group joined (or being joined) under `generation`. Idempotent; it never
    /// disconnects a network this guest did not join.
    fn leave(&mut self, generation: u32);
}

/// Whether this PC may offer the client bit and take a HOST now. Its own Wi-Fi band does
/// not matter (it leaves that network to join); the group's band is judged after joining.
pub fn guest_eligibility(caps: &WifiCaps) -> Result<(), u8> {
    match caps.radio {
        WifiRadio::Off => return Err(decline::WIFI_UNAVAILABLE),
        WifiRadio::Missing | WifiRadio::Unknown => return Err(decline::NOT_ELIGIBLE),
        WifiRadio::On => {}
    }
    if caps.hotspot == Some(true) {
        return Err(decline::BUSY);
    }
    Ok(())
}

/// PROTOCOL "Wi-Fi Direct", guest: the joined channel decides; a channel that cannot be read
/// falls back to the host's authenticated `HOST.freqMhz`; otherwise DECLINE 7.
pub fn join_band(channel: Option<u32>, host_freq_mhz: u16) -> Result<(), u8> {
    match probe::band_from_channel(channel) {
        Band::Ghz5Plus => Ok(()),
        Band::Ghz24 => Err(decline::NOT_5GHZ),
        Band::NotConnected | Band::Unknown => {
            if host_freq_mhz >= MIN_5GHZ_MHZ {
                Ok(())
            } else {
                Err(decline::NOT_5GHZ)
            }
        }
    }
}

/// F-4: the CREDS address is private, on the joined subnet and not this PC's own address.
pub fn creds_check(joined: GroupAddress, peer: Ipv4Addr) -> Result<(), u8> {
    if peer.is_private() && crate::wifi::address::same_subnet(joined, peer) {
        Ok(())
    } else {
        Err(decline::SETUP_FAILED)
    }
}

fn guid_key(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch != '{' && *ch != '}' && *ch != '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

pub fn same_guid_text(a: &str, b: &str) -> bool {
    let a = guid_key(a);
    !a.is_empty() && a == guid_key(b)
}

fn usable(address: &AddressInfo) -> bool {
    address.preferred && !address.ip.is_loopback() && !address.ip.is_link_local()
}

/// The usable IPv4 of the joined Wi-Fi interface (up, preferred, not link-local).
pub fn joined_address(adapters: &[AdapterInfo], guid: &str) -> Option<GroupAddress> {
    adapters
        .iter()
        .filter(|adapter| adapter.up && same_guid_text(&adapter.guid, guid))
        .flat_map(|adapter| adapter.ipv4.iter())
        .find(|address| usable(address))
        .map(|address| GroupAddress {
            ip: address.ip,
            prefix: address.prefix,
        })
}

fn mask(prefix: u8) -> u32 {
    match prefix.min(32) {
        0 => 0,
        bits => u32::MAX << (32 - bits),
    }
}

/// F-4: another interface that is up reaches the joined subnet (or a subnet containing it).
pub fn overlaps_other(adapters: &[AdapterInfo], guid: &str, joined: GroupAddress) -> bool {
    adapters
        .iter()
        .filter(|adapter| adapter.up && !same_guid_text(&adapter.guid, guid))
        .flat_map(|adapter| adapter.ipv4.iter())
        .filter(|address| !address.ip.is_loopback() && !address.ip.is_link_local())
        .any(|address| {
            let bits = mask(address.prefix.min(joined.prefix));
            (u32::from(address.ip) & bits) == (u32::from(joined.ip) & bits)
        })
}

/// Design decision: a random Wi-Fi address per group network (Windows profile schema v3).
pub const RANDOM_MAC: bool = true;

/// The WPA2-PSK temporary profile for `WlanConnect`. `None` unless the name is 1-32 of
/// `[A-Za-z0-9-]` and the password 8-63 of `[A-Za-z0-9_-]` (what `keys` derives), so
/// nothing in it needs XML escaping.
pub fn profile_xml(ssid: &str, psk: &str, random_mac: bool) -> Option<Zeroizing<String>> {
    let ssid_ok = (1..=32).contains(&ssid.len())
        && ssid.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let psk_ok = (8..=63).contains(&psk.len())
        && psk
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !ssid_ok || !psk_ok {
        return None;
    }
    let mut xml = Zeroizing::new(String::with_capacity(1_024));
    xml.push_str(r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>"#);
    xml.push_str(ssid);
    xml.push_str("</name><SSIDConfig><SSID><name>");
    xml.push_str(ssid);
    xml.push_str("</name></SSID><nonBroadcast>false</nonBroadcast></SSIDConfig>");
    xml.push_str("<connectionType>ESS</connectionType><connectionMode>manual</connectionMode>");
    xml.push_str("<MSM><security><authEncryption><authentication>WPA2PSK</authentication><encryption>AES</encryption><useOneX>false</useOneX></authEncryption>");
    xml.push_str("<sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>");
    xml.push_str(psk);
    xml.push_str("</keyMaterial></sharedKey></security></MSM>");
    if random_mac {
        xml.push_str(r#"<MacRandomization xmlns="http://www.microsoft.com/networking/WLAN/profile/v3"><enableRandomization>true</enableRandomization></MacRandomization>"#);
    }
    xml.push_str("</WLANProfile>");
    Some(xml)
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    pub(crate) const JOINED_GUID: &str = "{11111111-2222-3333-4444-555555555555}";

    pub(crate) struct FakeState {
        /// `(generation, ssid, psk)` of every join request (test copies only).
        pub joins: Vec<(u32, String, String)>,
        pub leaves: Vec<u32>,
        pub connects: Vec<(u32, Ipv4Addr, SocketAddrV4)>,
        pub events: VecDeque<JoinEvent>,
        /// `None`: the probe has not answered yet.
        pub probe: Option<WifiCaps>,
        pub join_error: Option<PlatformError>,
        pub connect_error: Option<PlatformError>,
        pub adapters: Vec<AdapterInfo>,
        pub guid: Option<String>,
        /// `Some(channel)`: every join is answered at once with `Joined { channel }`.
        pub auto_join: Option<Option<u32>>,
        /// A loopback listener every connect really connects to (the test's phone).
        pub connect_to: Option<std::net::SocketAddr>,
    }

    pub(crate) struct FakeGuestPlatform {
        pub state: Arc<Mutex<FakeState>>,
    }

    impl FakeGuestPlatform {
        /// Wi-Fi on, no hotspot, the joined adapter with 192.168.49.23/24 and a home adapter
        /// with 10.0.0.5/24; joins answer at once on channel 149.
        pub(crate) fn ready() -> Self {
            let adapter = |guid: &str, ip: [u8; 4]| AdapterInfo {
                guid: guid.to_owned(),
                description: "Wi-Fi".to_owned(),
                up: true,
                ipv4: vec![AddressInfo {
                    ip: Ipv4Addr::from(ip),
                    prefix: 24,
                    preferred: true,
                }],
            };
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    joins: Vec::new(),
                    leaves: Vec::new(),
                    connects: Vec::new(),
                    events: VecDeque::new(),
                    probe: Some(WifiCaps {
                        radio: WifiRadio::On,
                        hotspot: Some(false),
                        band: Band::Ghz5Plus,
                    }),
                    join_error: None,
                    connect_error: None,
                    adapters: vec![
                        adapter(JOINED_GUID, [192, 168, 49, 23]),
                        adapter("{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}", [10, 0, 0, 5]),
                    ],
                    guid: Some(JOINED_GUID.to_owned()),
                    auto_join: Some(Some(149)),
                    connect_to: None,
                })),
            }
        }
    }

    impl GuestPlatform for FakeGuestPlatform {
        fn probe(&mut self) -> ProbeState {
            match self.state.lock().unwrap().probe {
                Some(caps) => ProbeState::Ready(caps),
                None => ProbeState::Pending,
            }
        }
        fn join(&mut self, generation: u32, request: JoinRequest) -> Result<(), PlatformError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = state.join_error.take() {
                return Err(error);
            }
            state
                .joins
                .push((generation, request.ssid.to_string(), request.psk.to_string()));
            if let Some(channel) = state.auto_join {
                state.events.push_back(JoinEvent::Joined {
                    generation,
                    channel,
                });
            }
            Ok(())
        }
        fn poll(&mut self) -> Vec<JoinEvent> {
            self.state.lock().unwrap().events.drain(..).collect()
        }
        fn interface_guid(&mut self) -> Option<String> {
            self.state.lock().unwrap().guid.clone()
        }
        fn adapters(&mut self) -> Vec<AdapterInfo> {
            self.state.lock().unwrap().adapters.clone()
        }
        fn connect(&mut self, generation: u32, local: Ipv4Addr, remote: SocketAddrV4) -> Result<(), PlatformError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = state.connect_error.take() {
                return Err(error);
            }
            state.connects.push((generation, local, remote));
            if let Some(address) = state.connect_to {
                let stream = std::net::TcpStream::connect(address).expect("the test's phone listens");
                state.events.push_back(JoinEvent::Connected {
                    generation,
                    socket: WifiSocket::from_stream(stream).expect("socket"),
                });
            }
            Ok(())
        }
        fn leave(&mut self, generation: u32) {
            self.state.lock().unwrap().leaves.push(generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::{address::AddressInfo, wire::decline};

    fn adapter(guid: &str, up: bool, addresses: &[([u8; 4], u8, bool)]) -> AdapterInfo {
        AdapterInfo {
            guid: guid.to_owned(),
            description: "adapter".to_owned(),
            up,
            ipv4: addresses
                .iter()
                .map(|(ip, prefix, preferred)| AddressInfo {
                    ip: Ipv4Addr::from(*ip),
                    prefix: *prefix,
                    preferred: *preferred,
                })
                .collect(),
        }
    }

    fn joined() -> GroupAddress {
        GroupAddress {
            ip: Ipv4Addr::new(192, 168, 49, 23),
            prefix: 24,
        }
    }

    #[test]
    fn the_band_is_the_joined_channels_or_else_the_hosts_authenticated_frequency() {
        assert_eq!(join_band(Some(149), 0), Ok(()));
        assert_eq!(join_band(Some(36), 0), Ok(()));
        assert_eq!(join_band(Some(6), 5_745), Err(decline::NOT_5GHZ), "a 2.4 GHz group, whatever HOST said");
        assert_eq!(join_band(None, 5_745), Ok(()));
        assert_eq!(join_band(Some(0), 5_180), Ok(()));
        assert_eq!(join_band(None, 0), Err(decline::NOT_5GHZ));
    }

    #[test]
    fn creds_must_be_a_private_address_on_the_joined_subnet_and_not_this_pc() {
        assert_eq!(creds_check(joined(), Ipv4Addr::new(192, 168, 49, 1)), Ok(()));
        for peer in [
            Ipv4Addr::new(192, 168, 50, 1),
            Ipv4Addr::new(8, 8, 8, 8),
            Ipv4Addr::new(192, 168, 49, 23),
        ] {
            assert_eq!(creds_check(joined(), peer), Err(decline::SETUP_FAILED), "{peer}");
        }
        let public_joined = GroupAddress {
            ip: Ipv4Addr::new(100, 64, 0, 2),
            prefix: 24,
        };
        assert_eq!(creds_check(public_joined, Ipv4Addr::new(100, 64, 0, 1)), Err(decline::SETUP_FAILED));
    }

    #[test]
    fn the_joined_address_is_the_usable_ipv4_of_the_joined_interface_only() {
        let adapters = [
            adapter("{AAAAAAAA-0000-0000-0000-000000000000}", true, &[([192, 168, 49, 99], 24, true)]),
            adapter(
                "{11111111-2222-3333-4444-555555555555}",
                true,
                &[([169, 254, 3, 4], 16, true), ([192, 168, 49, 23], 24, false), ([192, 168, 49, 24], 24, true)],
            ),
        ];
        assert_eq!(
            joined_address(&adapters, "11111111-2222-3333-4444-555555555555"),
            Some(GroupAddress {
                ip: Ipv4Addr::new(192, 168, 49, 24),
                prefix: 24
            })
        );
        assert_eq!(joined_address(&adapters, "{99999999-2222-3333-4444-555555555555}"), None);
        let down = [adapter("{11111111-2222-3333-4444-555555555555}", false, &[([192, 168, 49, 24], 24, true)])];
        assert_eq!(joined_address(&down, "{11111111-2222-3333-4444-555555555555}"), None);
    }

    #[test]
    fn a_guid_matches_with_or_without_braces_in_any_case_and_never_when_empty() {
        assert!(same_guid_text("{11111111-AAAA-3333-4444-555555555555}", "11111111-aaaa-3333-4444-555555555555"));
        assert!(!same_guid_text("{11111111-AAAA-3333-4444-555555555555}", "{11111111-AAAA-3333-4444-555555555556}"));
        assert!(!same_guid_text("", "{}"));
    }

    #[test]
    fn a_joined_subnet_that_another_interface_also_reaches_overlaps() {
        let guid = "{11111111-2222-3333-4444-555555555555}";
        let own = adapter(guid, true, &[([192, 168, 49, 23], 24, true)]);
        let home = adapter("{AAAAAAAA-0000-0000-0000-000000000000}", true, &[([10, 0, 0, 5], 24, true)]);
        assert!(!overlaps_other(&[own.clone(), home.clone()], guid, joined()));
        let clash = adapter("{BBBBBBBB-0000-0000-0000-000000000000}", true, &[([192, 168, 49, 77], 24, true)]);
        assert!(overlaps_other(&[own.clone(), clash], guid, joined()));
        let wide = adapter("{CCCCCCCC-0000-0000-0000-000000000000}", true, &[([192, 168, 0, 9], 16, true)]);
        assert!(overlaps_other(&[own.clone(), wide], guid, joined()), "a wider subnet that contains it");
        let down = adapter("{DDDDDDDD-0000-0000-0000-000000000000}", false, &[([192, 168, 49, 77], 24, true)]);
        assert!(!overlaps_other(&[own, down], guid, joined()), "an adapter that is down");
    }

    #[test]
    fn eligibility_needs_wifi_on_and_no_hotspot_but_not_a_5_ghz_network() {
        let caps = |radio, hotspot, band| WifiCaps { radio, hotspot, band };
        assert_eq!(guest_eligibility(&caps(WifiRadio::On, Some(false), Band::Ghz24)), Ok(()));
        assert_eq!(guest_eligibility(&caps(WifiRadio::On, None, Band::NotConnected)), Ok(()));
        assert_eq!(guest_eligibility(&caps(WifiRadio::Off, Some(false), Band::Ghz5Plus)), Err(decline::WIFI_UNAVAILABLE));
        assert_eq!(guest_eligibility(&caps(WifiRadio::Missing, Some(false), Band::Ghz5Plus)), Err(decline::NOT_ELIGIBLE));
        assert_eq!(guest_eligibility(&caps(WifiRadio::On, Some(true), Band::Ghz5Plus)), Err(decline::BUSY));
    }

    #[test]
    fn the_temporary_profile_carries_the_derived_name_and_password_and_refuses_anything_else() {
        let ssid = "DIRECT-ab-cdefgh";
        let psk = "Ab0_-xyzXYZ0123456789abcdefghijklmnopqrstu";
        let xml = profile_xml(ssid, psk, false).expect("a valid profile");
        assert!(xml.contains("<name>DIRECT-ab-cdefgh</name></SSID>"));
        assert!(xml.contains("<keyMaterial>Ab0_-xyzXYZ0123456789abcdefghijklmnopqrstu</keyMaterial>"));
        assert!(xml.contains("<authentication>WPA2PSK</authentication><encryption>AES</encryption>"));
        assert!(xml.contains("<connectionMode>manual</connectionMode>"));
        assert!(!xml.contains("MacRandomization"));
        assert!(profile_xml(ssid, psk, true).unwrap().contains("<enableRandomization>true</enableRandomization>"));
        assert!(profile_xml("DIRECT-ab-<x>", psk, false).is_none());
        assert!(profile_xml(ssid, "short", false).is_none());
        assert!(profile_xml(ssid, "has space in the password ok", false).is_none());
        assert!(profile_xml("", psk, false).is_none());
    }

    #[test]
    fn nothing_a_join_request_or_an_event_prints_shows_the_name_password_or_socket() {
        let request = JoinRequest {
            ssid: Zeroizing::new("DIRECT-ab-cdefgh".to_owned()),
            psk: Zeroizing::new("SecretPassword0123456789".to_owned()),
        };
        let text = format!("{request:?}");
        assert!(!text.contains("DIRECT-ab"), "{text}");
        assert!(!text.contains("SecretPassword"), "{text}");
        let event = JoinEvent::Joined {
            generation: 3,
            channel: Some(149),
        };
        assert_eq!(event.generation(), 3);
        assert!(format!("{event:?}").contains('3'));
    }
}
