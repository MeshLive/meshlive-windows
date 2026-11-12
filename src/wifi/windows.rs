//! The Windows implementation of `Platform`: WinRT Wi-Fi Direct, the group
//! adapter/IPv4 reads, the non-blocking listener and the NLM profile check.

use crate::wifi::{
    address::{self, AdapterInfo, GroupAddress},
    group::{GroupState, HostGroup},
    listener::{self, GroupListener},
    platform::{Platform, PlatformError, ProbeState, ProfileRead},
    probe::{self, WifiCaps},
    socket::WifiSocket,
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
};
use windows::{
    Win32::{
        Networking::NetworkListManager::{
            INetworkListManager, NLM_NETWORK_CATEGORY_PUBLIC, NetworkListManager,
        },
        System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
    },
    core::GUID,
};

pub struct WinPlatform {
    cancelled: Arc<AtomicBool>,
    probe: Option<Receiver<WifiCaps>>,
    caps: Option<ProbeState>,
    group: Option<HostGroup>,
    listener: Option<GroupListener>,
    address: Option<GroupAddress>,
    /// The GUID of the adapter `group_address` chose; its profile is the one read.
    guid: Option<String>,
}

impl WinPlatform {
    pub fn new() -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let probe = Some(probe::spawn(cancelled.clone()));
        Self {
            cancelled,
            probe,
            caps: None,
            group: None,
            listener: None,
            address: None,
            guid: None,
        }
    }
}

impl Default for WinPlatform {
    fn default() -> Self {
        Self::new()
    }
}

fn platform_error(error: crate::bluetooth::Error) -> PlatformError {
    match error {
        crate::bluetooth::Error::Native(code) => PlatformError::Native(code),
        crate::bluetooth::Error::Gatt(code) | crate::bluetooth::Error::Bluetooth(code) => {
            PlatformError::Native(code)
        }
        _ => PlatformError::Native(0),
    }
}

thread_local! {
    /// 0 unknown, 1 initialised, 2 failed (never retried on this thread).
    static COM_STATE: Cell<u8> = const { Cell::new(0) };
}

fn com_ready() -> bool {
    COM_STATE.with(|state| {
        if state.get() == 1 {
            return true;
        }
        if state.get() == 2 {
            return false;
        }
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK and S_FALSE both mean this thread is initialised.
        let ready = result.0 == 0 || result.0 == 1;
        state.set(if ready { 1 } else { 2 });
        ready
    })
}

fn same_guid(guid: GUID, text: &str) -> bool {
    let normalized: String = text
        .chars()
        .filter(|ch| *ch != '{' && *ch != '}')
        .filter(|ch| *ch != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{:032x}", guid.to_u128()) == normalized
}

fn profile_of(guid: &str) -> ProfileRead {
    if !com_ready() {
        return ProfileRead::Unreadable;
    }
    let manager: INetworkListManager = match unsafe {
        CoCreateInstance(&NetworkListManager, None::<&windows::core::IUnknown>, CLSCTX_ALL)
    } {
        Ok(manager) => manager,
        Err(_) => return ProfileRead::Unreadable,
    };
    let connections = match unsafe { manager.GetNetworkConnections() } {
        Ok(connections) => connections,
        Err(_) => return ProfileRead::Unreadable,
    };
    loop {
        let mut items = [None];
        let mut fetched = 0u32;
        match unsafe { connections.Next(&mut items, Some(&mut fetched)) } {
            Ok(()) => {}
            Err(_) => return ProfileRead::Unreadable,
        }
        if fetched == 0 {
            break;
        }
        let Some(connection) = items[0].take() else {
            break;
        };
        let adapter = match unsafe { connection.GetAdapterId() } {
            Ok(adapter) => adapter,
            Err(_) => return ProfileRead::Unreadable,
        };
        if !same_guid(adapter, guid) {
            continue;
        }
        let network = match unsafe { connection.GetNetwork() } {
            Ok(network) => network,
            Err(_) => return ProfileRead::Unreadable,
        };
        let category = match unsafe { network.GetCategory() } {
            Ok(category) => category,
            Err(_) => return ProfileRead::Unreadable,
        };
        return if category == NLM_NETWORK_CATEGORY_PUBLIC {
            ProfileRead::Public
        } else {
            ProfileRead::NotPublic
        };
    }
    ProfileRead::Pending
}

impl Platform for WinPlatform {
    fn probe(&mut self) -> ProbeState {
        if let Some(state) = self.caps {
            return state;
        }
        let Some(receiver) = self.probe.as_ref() else {
            return ProbeState::Pending;
        };
        match receiver.try_recv() {
            Ok(caps) => {
                self.caps = Some(ProbeState::Ready(caps));
                ProbeState::Ready(caps)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ProbeState::Pending,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => ProbeState::Pending,
        }
    }

    fn snapshot(&mut self) -> Vec<AdapterInfo> {
        address::adapters()
    }

    fn start_group(&mut self, ssid: &str, psk: &str) -> Result<(), PlatformError> {
        if self.group.is_some() {
            self.release();
        }
        let group = HostGroup::start(ssid, psk).map_err(platform_error)?;
        self.group = Some(group);
        Ok(())
    }

    fn group_state(&mut self) -> GroupState {
        match self.group.as_mut() {
            Some(group) => {
                let state = group.poll();
                match state {
                    GroupState::Aborted(2) => GroupState::Aborted(2),
                    GroupState::Aborted(1) => GroupState::Aborted(1),
                    other => other,
                }
            }
            None => GroupState::Stopped,
        }
    }

    fn group_address(&mut self, before: &[AdapterInfo]) -> Option<GroupAddress> {
        let now = address::adapters();
        let selected = address::select_group_adapter(before, &now);
        self.address = selected.as_ref().map(|(_, address)| *address);
        self.guid = selected.map(|(guid, _)| guid);
        self.address
    }

    fn group_is_public(&mut self, address: GroupAddress) -> ProfileRead {
        // Only the adapter chosen for this group: another adapter can hold the
        // same address (Mobile Hotspot also uses 192.168.137.1).
        match (self.address, self.guid.as_deref()) {
            (Some(chosen), Some(guid)) if chosen == address => profile_of(guid),
            _ => ProfileRead::Pending,
        }
    }

    fn listen(&mut self, address: GroupAddress) -> Result<u16, PlatformError> {
        let (listener, port) = listener::bind(address).map_err(|_| PlatformError::Bind)?;
        self.listener = Some(listener);
        Ok(port)
    }

    fn accept(&mut self) -> Option<WifiSocket> {
        self.listener.as_mut().and_then(GroupListener::accept)
    }

    fn stop_listening(&mut self) {
        self.listener = None;
    }

    fn release(&mut self) {
        self.listener = None;
        self.group.take();
        self.address = None;
        self.guid = None;
    }
}

impl Drop for WinPlatform {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.release();
    }
}
