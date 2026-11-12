//! The small platform boundary between the attempt owner and WinRT/Win32.

use crate::wifi::{
    address::{AdapterInfo, GroupAddress},
    group::GroupState,
    probe::WifiCaps,
    socket::WifiSocket,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformError {
    /// `WiFiDirectError` value.
    Radio(i32),
    InUse,
    Native(i32),
    Bind,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState {
    Pending,
    Ready(WifiCaps),
}

/// F-5: the group network's Firewall profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileRead {
    Public,
    NotPublic,
    Pending,
    Unreadable,
}

pub trait Platform {
    /// Non-blocking; `Ready` once the probe thread answered.
    fn probe(&mut self) -> ProbeState;
    fn snapshot(&mut self) -> Vec<AdapterInfo>;
    fn start_group(&mut self, ssid: &str, psk: &str) -> Result<(), PlatformError>;
    fn group_state(&mut self) -> GroupState;
    fn group_address(&mut self, before: &[AdapterInfo]) -> Option<GroupAddress>;
    fn group_is_public(&mut self, address: GroupAddress) -> ProfileRead;
    fn listen(&mut self, address: GroupAddress) -> Result<u16, PlatformError>;
    fn accept(&mut self) -> Option<WifiSocket>;
    fn stop_listening(&mut self);
    fn release(&mut self);
}
