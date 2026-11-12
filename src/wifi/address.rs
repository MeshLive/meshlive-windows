//! The group interface IPv4: a read-only enumeration of adapters and the pure
//! choice of the Wi-Fi Direct group address. The MAC is never read.

use std::net::Ipv4Addr;
use windows::Win32::{
    NetworkManagement::{
        IpHelper::{GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH},
        Ndis::IfOperStatusUp,
    },
    Networking::WinSock::{IpDadStatePreferred, SOCKADDR_IN},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressInfo {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub preferred: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterInfo {
    pub guid: String,
    pub description: String,
    pub up: bool,
    pub ipv4: Vec<AddressInfo>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupAddress {
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

/// Every IPv4 adapter, without its MAC. The two-call pattern of W0's `adapters()`.
pub fn adapters() -> Vec<AdapterInfo> {
    let mut size: u32 = 16_000;
    loop {
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let first = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        // Family AF_INET = 2; flags: skip anycast (2), multicast (4), DNS servers (8).
        let code = unsafe {
            GetAdaptersAddresses(2, GET_ADAPTERS_ADDRESSES_FLAGS(0x0e), None, Some(first), &mut size)
        };
        if code == 111 {
            continue; // ERROR_BUFFER_OVERFLOW: size now holds the needed bytes.
        }
        if code != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = first;
        while !cursor.is_null() {
            let adapter = unsafe { &*cursor };
            let mut ipv4 = Vec::new();
            let mut unicast = adapter.FirstUnicastAddress;
            while !unicast.is_null() {
                let entry = unsafe { &*unicast };
                let sockaddr = entry.Address.lpSockaddr;
                if !sockaddr.is_null() && unsafe { (*sockaddr).sa_family.0 } == 2 {
                    let raw = unsafe { (*(sockaddr as *const SOCKADDR_IN)).sin_addr.S_un.S_addr };
                    ipv4.push(AddressInfo {
                        ip: Ipv4Addr::from(raw.to_ne_bytes()),
                        prefix: entry.OnLinkPrefixLength,
                        preferred: entry.DadState == IpDadStatePreferred,
                    });
                }
                unicast = entry.Next;
            }
            out.push(AdapterInfo {
                guid: unsafe { adapter.AdapterName.to_string() }.unwrap_or_default(),
                description: unsafe { adapter.Description.to_string() }.unwrap_or_default(),
                up: adapter.OperStatus == IfOperStatusUp,
                ipv4,
            });
            cursor = adapter.Next;
        }
        return out;
    }
}

fn direct_adapter(adapter: &AdapterInfo) -> bool {
    adapter.up && adapter.description.to_ascii_lowercase().contains("wi-fi direct")
}

fn usable(address: &AddressInfo) -> bool {
    address.preferred && !address.ip.is_loopback() && !address.ip.is_link_local()
}

fn has_group_address(adapter: &AdapterInfo) -> bool {
    direct_adapter(adapter) && adapter.ipv4.iter().any(usable)
}

/// The group is the Wi-Fi Direct adapter that appeared or gained its address
/// because `Start()` ran. An adapter that already had a usable address before
/// (Windows Mobile Hotspot, another app's group, a group not yet torn down) is
/// never chosen, even when it is the only candidate: its network is not ours.
pub fn select_group_address(before: &[AdapterInfo], now: &[AdapterInfo]) -> Option<GroupAddress> {
    select_group_adapter(before, now).map(|(_, address)| address)
}

/// `select_group_address` with the chosen adapter's GUID (its Firewall profile is read by GUID).
pub fn select_group_adapter(before: &[AdapterInfo], now: &[AdapterInfo]) -> Option<(String, GroupAddress)> {
    let chosen = now.iter().find(|adapter| {
        has_group_address(adapter)
            && !before
                .iter()
                .any(|old| old.guid == adapter.guid && has_group_address(old))
    })?;
    chosen
        .ipv4
        .iter()
        .find(|address| usable(address))
        .map(|address| {
            (
                chosen.guid.clone(),
                GroupAddress {
                    ip: address.ip,
                    prefix: address.prefix,
                },
            )
        })
}

pub fn same_subnet(group: GroupAddress, peer: Ipv4Addr) -> bool {
    if peer == group.ip {
        return false;
    }
    let prefix = group.prefix.min(32);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(group.ip) & mask) == (u32::from(peer) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(guid: &str, description: &str, up: bool, ipv4: &[(&str, u8, bool)]) -> AdapterInfo {
        AdapterInfo {
            guid: guid.to_owned(),
            description: description.to_owned(),
            up,
            ipv4: ipv4
                .iter()
                .map(|(ip, prefix, preferred)| AddressInfo {
                    ip: ip.parse().unwrap(),
                    prefix: *prefix,
                    preferred: *preferred,
                })
                .collect(),
        }
    }

    #[test]
    fn the_adapter_that_gained_an_address_after_start_is_chosen_over_an_older_direct_adapter() {
        let old = adapter(
            "{OLD}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            true,
            &[("192.168.137.1", 24, true)],
        );
        let fresh = adapter(
            "{NEW}",
            "Microsoft Wi-Fi Direct Virtual Adapter #2",
            true,
            &[("192.168.138.1", 24, true)],
        );
        let before = vec![old.clone()];
        let now = vec![old, fresh];
        assert_eq!(
            select_group_address(&before, &now),
            Some(GroupAddress {
                ip: "192.168.138.1".parse().unwrap(),
                prefix: 24
            })
        );
    }

    #[test]
    fn a_direct_adapter_that_already_had_an_address_before_start_is_never_the_group() {
        // Mobile Hotspot (or another app's group) runs on a Wi-Fi Direct
        // adapter with 192.168.137.1; ours has no address yet.
        let hotspot = adapter(
            "{HOTSPOT}",
            "Microsoft Wi-Fi Direct Virtual Adapter #2",
            true,
            &[("192.168.137.1", 24, true)],
        );
        let ours_tentative = adapter(
            "{OURS}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            true,
            &[("192.168.137.1", 24, false)],
        );
        let before = vec![hotspot.clone()];
        assert_eq!(select_group_address(&before, &[hotspot.clone()]), None);
        assert_eq!(select_group_address(&before, &[hotspot.clone(), ours_tentative]), None);
        let ours = adapter(
            "{OURS}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            true,
            &[("192.168.137.1", 24, true)],
        );
        let (guid, address) = select_group_adapter(&before, &[hotspot, ours]).unwrap();
        assert_eq!(guid, "{OURS}");
        assert_eq!(address.ip, "192.168.137.1".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn a_link_local_address_is_not_a_group_address_yet() {
        let direct = adapter(
            "{D}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            true,
            &[("169.254.10.2", 16, true)],
        );
        assert_eq!(select_group_address(&[], &[direct]), None);
    }

    #[test]
    fn a_tentative_address_is_not_a_group_address_yet() {
        let direct = adapter(
            "{D}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            true,
            &[("192.168.137.1", 24, false)],
        );
        assert_eq!(select_group_address(&[], &[direct]), None);
    }

    #[test]
    fn an_adapter_that_is_down_or_not_wifi_direct_is_ignored() {
        let down = adapter(
            "{D}",
            "Microsoft Wi-Fi Direct Virtual Adapter",
            false,
            &[("192.168.137.1", 24, true)],
        );
        let wifi = adapter(" {W}", "Intel Wi-Fi", true, &[("10.0.0.8", 24, true)]);
        assert_eq!(select_group_address(&[], &[down, wifi]), None);
    }

    #[test]
    fn same_subnet_uses_the_prefix_and_rejects_the_hosts_own_address() {
        let group = GroupAddress {
            ip: "192.168.137.1".parse().unwrap(),
            prefix: 24,
        };
        assert!(same_subnet(group, "192.168.137.5".parse().unwrap()));
        assert!(!same_subnet(group, "192.168.138.5".parse().unwrap()));
        assert!(!same_subnet(group, "192.168.137.1".parse().unwrap()));
    }

    #[test]
    fn description_matching_ignores_case() {
        for description in [
            "Microsoft Wi-Fi Direct Virtual Adapter",
            "microsoft wi-fi direct virtual adapter #2",
            "MICROSOFT WI-FI DIRECT VIRTUAL ADAPTER",
        ] {
            let direct = adapter("{D}", description, true, &[("192.168.137.1", 24, true)]);
            assert_eq!(
                select_group_address(&[], &[direct]),
                Some(GroupAddress {
                    ip: "192.168.137.1".parse().unwrap(),
                    prefix: 24
                }),
                "{description}"
            );
        }
    }
}
