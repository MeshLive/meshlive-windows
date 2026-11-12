//! W4G: the Windows guest platform. Native Wifi joins the phone's group with a temporary
//! profile (`WlanConnect`, `wlan_connection_mode_temporary_profile`: nothing is saved), ACM
//! notifications say when that network connected or dropped, and WinSock connects out from
//! the joined address. Every WLAN call runs on one worker thread; the call worker only sends
//! commands and drains events. No listener, no Firewall rule, and none of the calls that
//! need the Location consent (`WlanScan`, `WlanGetAvailableNetworkList`,
//! `WlanGetNetworkBssList`, the current-connection query, MSM notifications).

use crate::wifi::{
    address::{self, AdapterInfo},
    guest_platform::{self, GuestPlatform, JoinEvent, JoinRequest, RANDOM_MAC},
    platform::{PlatformError, ProbeState},
    probe::{self, WifiCaps},
    socket::WifiSocket,
};
use std::{
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    os::windows::io::FromRawSocket,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::WiFi::{
            L2_NOTIFICATION_DATA, WLAN_CONNECTION_NOTIFICATION_DATA, WLAN_CONNECTION_PARAMETERS,
            WLAN_INTERFACE_INFO_LIST, WLAN_NOTIFICATION_SOURCE_ACM, WLAN_NOTIFICATION_SOURCE_NONE,
            WlanCloseHandle, WlanConnect, WlanDisconnect, WlanEnumInterfaces, WlanFreeMemory,
            WlanOpenHandle, WlanQueryInterface, WlanRegisterNotification, dot11_BSS_type_infrastructure,
            wlan_connection_mode_temporary_profile, wlan_intf_opcode_channel_number,
            wlan_notification_acm_connection_attempt_fail, wlan_notification_acm_connection_complete,
            wlan_notification_acm_disconnected,
        },
        Networking::WinSock::{
            AF_INET, FIONBIO, IN_ADDR, IN_ADDR_0, IPPROTO_TCP, POLLWRNORM, SO_ERROR, SOCK_STREAM,
            SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSADATA, WSAECONNREFUSED,
            WSAEWOULDBLOCK, WSAGetLastError, WSAPOLLFD, WSAPoll, WSAStartup, bind, closesocket, connect,
            getsockopt, ioctlsocket, socket,
        },
    },
    core::{GUID, PCWSTR, PSTR},
};
use zeroize::Zeroizing;

/// A pending connect gives up after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Left joins are remembered this long, to undo a join that completes late.
const LEFT_MEMORY: Duration = Duration::from_secs(60);

enum Command {
    Join { generation: u32, request: JoinRequest },
    Leave { generation: u32 },
    Connect { generation: u32, local: Ipv4Addr, remote: SocketAddrV4 },
}

pub struct WlanGuest {
    cancelled: Arc<AtomicBool>,
    probe: Option<mpsc::Receiver<WifiCaps>>,
    caps: Option<ProbeState>,
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<JoinEvent>,
    guid: Arc<Mutex<Option<String>>>,
}

impl WlanGuest {
    /// Starts the read-only probe and the WLAN worker; nothing joins until `join`.
    pub fn new() -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let probe = Some(probe::spawn(cancelled.clone()));
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let guid = Arc::new(Mutex::new(None));
        let worker_guid = guid.clone();

        let _ = std::thread::Builder::new()
            .name("meshlive-wifi-guest".to_owned())
            .spawn(move || worker(command_rx, event_tx, worker_guid));
        Self {
            cancelled,
            probe,
            caps: None,
            commands,
            events,
            guid,
        }
    }
}

impl Default for WlanGuest {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WlanGuest {
    fn drop(&mut self) {
        // Dropping `commands` ends the worker, which leaves any join first.
        self.cancelled.store(true, Ordering::Release);
    }
}

impl GuestPlatform for WlanGuest {
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
            Err(_) => ProbeState::Pending,
        }
    }

    fn join(&mut self, generation: u32, request: JoinRequest) -> Result<(), PlatformError> {
        join_gate(self.interface_guid().is_some())?;
        self.commands
            .send(Command::Join { generation, request })
            .map_err(|_| PlatformError::Unsupported)
    }

    fn poll(&mut self) -> Vec<JoinEvent> {
        self.events.try_iter().collect()
    }

    fn interface_guid(&mut self) -> Option<String> {
        self.guid.lock().ok().and_then(|guid| guid.clone())
    }

    fn adapters(&mut self) -> Vec<AdapterInfo> {
        address::adapters()
    }

    fn connect(&mut self, generation: u32, local: Ipv4Addr, remote: SocketAddrV4) -> Result<(), PlatformError> {
        self.commands
            .send(Command::Connect {
                generation,
                local,
                remote,
            })
            .map_err(|_| PlatformError::Unsupported)
    }

    fn leave(&mut self, generation: u32) {
        let _ = self.commands.send(Command::Leave { generation });
    }
}

/// The worker has not opened the WLAN handle yet (or has no Wi-Fi interface): not a
/// platform refusal, so no local-fault record; the phone's attempt is declined busy.
fn join_gate(interface_known: bool) -> Result<(), PlatformError> {
    if interface_known {
        Ok(())
    } else {
        Err(PlatformError::InUse)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcmKind {
    Complete,
    Failed,
    Disconnected,
}

/// The ACM notification codes this adapter acts on.
pub(crate) fn acm_kind(code: u32) -> Option<AcmKind> {
    let code = code as i32;
    if code == wlan_notification_acm_connection_complete.0 {
        Some(AcmKind::Complete)
    } else if code == wlan_notification_acm_connection_attempt_fail.0 {
        Some(AcmKind::Failed)
    } else if code == wlan_notification_acm_disconnected.0 {
        Some(AcmKind::Disconnected)
    } else {
        None
    }
}

/// One ACM notification for the worker (the network name stays in memory only).
struct Acm {
    kind: AcmKind,
    ssid: Zeroizing<Vec<u8>>,
    reason: u32,
    /// The interface the notification came from (GUID text): another Wi-Fi adapter's
    /// connections are none of our join's business.
    interface: String,
}

/// What `handle_acm` needs from the WLAN handle (a fake in tests).
trait AcmHost {
    fn interface_text(&self) -> String;
    fn disconnect(&self);
    fn channel(&self) -> Option<u32>;
}

unsafe extern "system" fn on_notification(data: *mut L2_NOTIFICATION_DATA, context: *mut core::ffi::c_void) {
    if data.is_null() || context.is_null() {
        return;
    }
    let data = unsafe { &*data };
    if data.NotificationSource.0 != WLAN_NOTIFICATION_SOURCE_ACM.0 {
        return;
    }
    let Some(kind) = acm_kind(data.NotificationCode) else {
        return;
    };
    let needed = std::mem::offset_of!(WLAN_CONNECTION_NOTIFICATION_DATA, strProfileXml);
    if data.pData.is_null() || (data.dwDataSize as usize) < needed {
        return;
    }
    let connection = unsafe { &*(data.pData as *const WLAN_CONNECTION_NOTIFICATION_DATA) };
    let length = (connection.dot11Ssid.uSSIDLength as usize).min(32);
    let acm = Acm {
        kind,
        ssid: Zeroizing::new(connection.dot11Ssid.ucSSID[..length].to_vec()),
        reason: connection.wlanReasonCode,
        interface: guid_text(data.InterfaceGuid),
    };
    let sender = unsafe { &*(context as *const Mutex<mpsc::Sender<Acm>>) };
    if let Ok(sender) = sender.lock() {
        let _ = sender.send(acm);
    }
}

pub(crate) fn guid_text(guid: GUID) -> String {
    let d = guid.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1, guid.data2, guid.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

struct Wlan {
    handle: HANDLE,
    guid: GUID,
}

impl Wlan {
    /// The WLAN client handle and the first Wi-Fi interface (W1's probe makes the same choice).
    fn open() -> Option<Self> {
        unsafe {
            let mut negotiated = 0u32;
            let mut handle = HANDLE::default();
            if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != 0 {
                return None;
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            let code = WlanEnumInterfaces(handle, None, &mut list);
            let guid = if code == 0 && !list.is_null() && (*list).dwNumberOfItems > 0 {
                Some((*(*list).InterfaceInfo.as_ptr()).InterfaceGuid)
            } else {
                None
            };
            if !list.is_null() {
                WlanFreeMemory(list as *const core::ffi::c_void);
            }
            match guid {
                Some(guid) => Some(Self { handle, guid }),
                None => {
                    let _ = WlanCloseHandle(handle, None);
                    None
                }
            }
        }
    }

    fn register(&self, context: &Mutex<mpsc::Sender<Acm>>) -> bool {
        let context = context as *const Mutex<mpsc::Sender<Acm>> as *const core::ffi::c_void;
        unsafe {
            WlanRegisterNotification(
                self.handle,
                WLAN_NOTIFICATION_SOURCE_ACM,
                true,
                Some(on_notification),
                Some(context),
                None,
                None,
            ) == 0
        }
    }

    fn unregister(&self) {
        unsafe {
            let _ = WlanRegisterNotification(self.handle, WLAN_NOTIFICATION_SOURCE_NONE, true, None, None, None, None);
        }
    }

    /// `WlanConnect` with the temporary profile; the XML and its UTF-16 copy are wiped on return.
    fn connect_temporary(&self, request: &JoinRequest) -> Result<(), u32> {
        let Some(xml) = guest_platform::profile_xml(&request.ssid, &request.psk, RANDOM_MAC) else {
            return Err(87);
        };
        let mut wide = Zeroizing::new(Vec::with_capacity(xml.len() + 1));
        wide.extend(xml.encode_utf16());
        wide.push(0u16);
        let parameters = WLAN_CONNECTION_PARAMETERS {
            wlanConnectionMode: wlan_connection_mode_temporary_profile,
            strProfile: PCWSTR(wide.as_ptr()),
            pDot11Ssid: std::ptr::null_mut(),
            pDesiredBssidList: std::ptr::null_mut(),
            dot11BssType: dot11_BSS_type_infrastructure,
            dwFlags: 0,
        };
        let code = unsafe { WlanConnect(self.handle, &self.guid, &parameters, None) };
        if code == 0 { Ok(()) } else { Err(code) }
    }

    fn disconnect(&self) {
        unsafe {
            let _ = WlanDisconnect(self.handle, &self.guid, None);
        }
    }

    /// The joined channel (not behind the Location consent; W0 finding 5).
    fn channel(&self) -> Option<u32> {
        unsafe {
            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            let code = WlanQueryInterface(
                self.handle,
                &self.guid,
                wlan_intf_opcode_channel_number,
                None,
                &mut size,
                &mut data,
                None,
            );
            let channel = if code == 0 && !data.is_null() && size >= 4 {
                Some(*(data as *const u32))
            } else {
                None
            };
            if !data.is_null() {
                WlanFreeMemory(data);
            }
            channel
        }
    }
}

impl AcmHost for Wlan {
    fn interface_text(&self) -> String {
        guid_text(self.guid)
    }
    fn disconnect(&self) {
        Wlan::disconnect(self)
    }
    fn channel(&self) -> Option<u32> {
        Wlan::channel(self)
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}

struct Join {
    generation: u32,
    ssid: Zeroizing<Vec<u8>>,
    /// A completion or failure arrived for this join.
    settled: bool,
    connected: bool,
    left: Option<Instant>,
}

struct PendingConnect {
    generation: u32,
    socket: SOCKET,
    started: Instant,
}

fn worker(
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<JoinEvent>,
    guid_out: Arc<Mutex<Option<String>>>,
) {
    // Ends only when the sender is gone (`WlanGuest` dropped), after every queued command
    // (a `Leave` among them) was handled.
    let Some(wlan) = Wlan::open() else {
        return;
    };
    if let Ok(mut guid) = guid_out.lock() {
        *guid = Some(guid_text(wlan.guid));
    }
    let (acm_tx, acm_rx) = mpsc::channel::<Acm>();
    let context = Box::new(Mutex::new(acm_tx));
    let registered = wlan.register(&context);
    let mut joins: Vec<Join> = Vec::new();
    let mut pending: Vec<PendingConnect> = Vec::new();
    loop {
        match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(Command::Join { generation, request }) => {
                if !registered {
                    let _ = events.send(JoinEvent::JoinRefused { generation });
                    continue;
                }
                match wlan.connect_temporary(&request) {
                    Ok(()) => joins.push(Join {
                        generation,
                        ssid: Zeroizing::new(request.ssid.as_bytes().to_vec()),
                        settled: false,
                        connected: false,
                        left: None,
                    }),
                    Err(_) => {
                        let _ = events.send(JoinEvent::JoinRefused { generation });
                    }
                }
                // `request` drops here: the name and the password are wiped.
            }
            Ok(Command::Leave { generation }) => {
                for join in joins.iter_mut().filter(|join| join.generation == generation && join.left.is_none()) {
                    if join.connected || !join.settled {
                        wlan.disconnect();
                    }
                    join.left = Some(Instant::now());
                    join.connected = false;
                }
                pending.retain(|connect| {
                    if connect.generation == generation {
                        unsafe {
                            closesocket(connect.socket);
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            Ok(Command::Connect {
                generation,
                local,
                remote,
            }) => match start_connect(local, remote) {
                Ok(socket) => pending.push(PendingConnect {
                    generation,
                    socket,
                    started: Instant::now(),
                }),
                Err(()) => {
                    let _ = events.send(JoinEvent::ConnectFailed { generation });
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        for acm in acm_rx.try_iter() {
            handle_acm(&wlan, &mut joins, &events, acm);
        }
        poll_pending(&mut pending, &events);
        joins.retain(|join| join.left.is_none_or(|at| at.elapsed() < LEFT_MEMORY));
    }
    // Shutdown: leave what is still ours, close sockets, stop notifications, close the handle.
    if joins.iter().any(|join| join.left.is_none() && (join.connected || !join.settled)) {
        wlan.disconnect();
    }
    for connect in pending {
        unsafe {
            closesocket(connect.socket);
        }
    }
    if registered {
        wlan.unregister();
    }
    drop(wlan);
    drop(context);
}

fn handle_acm<W: AcmHost>(wlan: &W, joins: &mut [Join], events: &mpsc::Sender<JoinEvent>, acm: Acm) {
    // Only the interface we joined with: a second adapter reconnecting elsewhere must
    // not clear our `connected` flag (the PC would then never leave the phone's group).
    if !guest_platform::same_guid_text(&acm.interface, &wlan.interface_text()) {
        return;
    }
    let ours = joins.iter().rposition(|join| join.ssid[..] == acm.ssid[..]);
    if acm.kind == AcmKind::Complete && acm.reason == 0 {
        // Another network connected: none of our other joins is connected or pending.
        for (index, join) in joins.iter_mut().enumerate() {
            if Some(index) != ours {
                join.settled = true;
                join.connected = false;
            }
        }
    }
    let Some(index) = ours else {
        return;
    };
    let join = &mut joins[index];
    match acm.kind {
        AcmKind::Complete if acm.reason == 0 => {
            join.settled = true;
            if join.left.is_some() {
                wlan.disconnect();
                let _ = events.send(JoinEvent::LateJoinUndone {
                    generation: join.generation,
                });
            } else {
                join.connected = true;
                let _ = events.send(JoinEvent::Joined {
                    generation: join.generation,
                    channel: wlan.channel(),
                });
            }
        }
        AcmKind::Complete | AcmKind::Failed => {
            if !join.settled {
                join.settled = true;
                if join.left.is_none() {
                    let _ = events.send(JoinEvent::JoinFailed {
                        generation: join.generation,
                    });
                }
            }
        }
        AcmKind::Disconnected => {
            if join.connected {
                join.connected = false;
                if join.left.is_none() {
                    let _ = events.send(JoinEvent::Left {
                        generation: join.generation,
                    });
                }
            }
        }
    }
}

fn poll_pending(pending: &mut Vec<PendingConnect>, events: &mpsc::Sender<JoinEvent>) {
    let mut index = 0;
    while index < pending.len() {
        let connect = &pending[index];
        let generation = connect.generation;
        let result = if connect.started.elapsed() >= CONNECT_TIMEOUT {
            unsafe {
                closesocket(connect.socket);
            }
            ConnectPoll::Failed
        } else {
            poll_connect(connect.socket)
        };
        let event = match result {
            ConnectPoll::Pending => {
                index += 1;
                continue;
            }
            ConnectPoll::Connected(socket) => JoinEvent::Connected { generation, socket },
            ConnectPoll::Refused => JoinEvent::ConnectRefused { generation },
            ConnectPoll::Failed => JoinEvent::ConnectFailed { generation },
        };
        pending.remove(index);
        let _ = events.send(event);
    }
}

fn ensure_winsock() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        unsafe { WSAStartup(0x0202, &mut data) == 0 }
    })
}

pub(crate) fn sockaddr_of(address: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: address.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(address.ip().octets()),
            },
        },
        sin_zero: [0; 8],
    }
}

/// A non-blocking TCP socket bound to `local` (port 0) with its connect to `remote` started.
pub(crate) fn start_connect(local: Ipv4Addr, remote: SocketAddrV4) -> Result<SOCKET, ()> {
    if !ensure_winsock() {
        return Err(());
    }
    let socket = unsafe { socket(AF_INET.0 as i32, SOCK_STREAM, IPPROTO_TCP.0) }.map_err(|_| ())?;
    let fail = |socket: SOCKET| {
        unsafe {
            closesocket(socket);
        }
        Err(())
    };
    let mut nonblocking = 1u32;
    if unsafe { ioctlsocket(socket, FIONBIO, &mut nonblocking) } != 0 {
        return fail(socket);
    }
    let size = std::mem::size_of::<SOCKADDR_IN>() as i32;
    let local_address = sockaddr_of(SocketAddrV4::new(local, 0));
    if unsafe { bind(socket, &local_address as *const SOCKADDR_IN as *const SOCKADDR, size) } != 0 {
        return fail(socket);
    }
    let remote_address = sockaddr_of(remote);
    if unsafe { connect(socket, &remote_address as *const SOCKADDR_IN as *const SOCKADDR, size) } != 0
        && unsafe { WSAGetLastError() } != WSAEWOULDBLOCK
    {
        return fail(socket);
    }
    Ok(socket)
}

pub(crate) enum ConnectPoll {
    Pending,
    Connected(WifiSocket),
    Refused,
    Failed,
}

impl ConnectPoll {
    #[cfg(test)]
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected(_) => "connected",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// One non-blocking look at a started connect. Any result but `Pending` consumes the socket.
pub(crate) fn poll_connect(socket: SOCKET) -> ConnectPoll {
    let mut descriptor = WSAPOLLFD {
        fd: socket,
        events: POLLWRNORM,
        revents: Default::default(),
    };
    let ready = unsafe { WSAPoll(&mut descriptor, 1, 0) };
    if ready == 0 {
        return ConnectPoll::Pending;
    }
    let close = |result: ConnectPoll| {
        unsafe {
            closesocket(socket);
        }
        result
    };
    if ready == SOCKET_ERROR {
        return close(ConnectPoll::Failed);
    }
    let mut error = 0i32;
    let mut length = std::mem::size_of::<i32>() as i32;
    let read = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_ERROR,
            PSTR(&mut error as *mut i32 as *mut u8),
            &mut length,
        )
    };
    if read != 0 {
        return close(ConnectPoll::Failed);
    }
    if error == WSAECONNREFUSED.0 {
        return close(ConnectPoll::Refused);
    }
    if error != 0 {
        return close(ConnectPoll::Failed);
    }
    let stream = unsafe { TcpStream::from_raw_socket(socket.0 as u64) };
    match WifiSocket::from_stream(stream) {
        Ok(socket) => ConnectPoll::Connected(socket),
        Err(_) => ConnectPoll::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_acm_codes_map_to_the_events_the_adapter_acts_on() {
        assert_eq!(acm_kind(wlan_notification_acm_connection_complete.0 as u32), Some(AcmKind::Complete));
        assert_eq!(acm_kind(wlan_notification_acm_connection_attempt_fail.0 as u32), Some(AcmKind::Failed));
        assert_eq!(acm_kind(wlan_notification_acm_disconnected.0 as u32), Some(AcmKind::Disconnected));
        assert_eq!(acm_kind(9_999), None);
    }

    #[test]
    fn a_socket_address_keeps_the_ip_and_port_in_network_order() {
        let address = sockaddr_of(SocketAddrV4::new(Ipv4Addr::new(192, 168, 49, 1), 47_000));
        assert_eq!(address.sin_family, AF_INET);
        assert_eq!(u16::from_be(address.sin_port), 47_000);
        let raw = unsafe { address.sin_addr.S_un.S_addr };
        assert_eq!(raw.to_ne_bytes(), [192, 168, 49, 1]);
    }

    struct FakeWlan {
        guid: &'static str,
        disconnects: std::cell::Cell<u32>,
    }
    impl AcmHost for FakeWlan {
        fn interface_text(&self) -> String {
            self.guid.to_owned()
        }
        fn disconnect(&self) {
            self.disconnects.set(self.disconnects.get() + 1);
        }
        fn channel(&self) -> Option<u32> {
            Some(149)
        }
    }

    const OURS: &str = "{11111111-2222-3333-4444-555555555555}";
    const OTHER: &str = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";

    fn acm(kind: AcmKind, ssid: &[u8], reason: u32, interface: &str) -> Acm {
        Acm {
            kind,
            ssid: Zeroizing::new(ssid.to_vec()),
            reason,
            interface: interface.to_owned(),
        }
    }

    fn join(generation: u32, ssid: &[u8], connected: bool) -> Join {
        Join {
            generation,
            ssid: Zeroizing::new(ssid.to_vec()),
            settled: connected,
            connected,
            left: None,
        }
    }

    #[test]
    fn notifications_from_another_adapter_never_change_our_join() {
        let wlan = FakeWlan {
            guid: OURS,
            disconnects: std::cell::Cell::new(0),
        };
        let (events, received) = mpsc::channel();
        let mut joins = vec![join(1, b"DIRECT-ab", true)];
        // The other adapter reconnects to the home network.
        handle_acm(&wlan, &mut joins, &events, acm(AcmKind::Complete, b"home", 0, OTHER));
        assert!(joins[0].connected, "our group is still joined");
        // The other adapter drops a network that happens to share our name.
        handle_acm(&wlan, &mut joins, &events, acm(AcmKind::Disconnected, b"DIRECT-ab", 0, OTHER));
        assert!(joins[0].connected);
        assert!(received.try_recv().is_err(), "no event for another adapter");
        assert_eq!(wlan.disconnects.get(), 0);
        // The same notifications on our adapter do count.
        handle_acm(&wlan, &mut joins, &events, acm(AcmKind::Complete, b"home", 0, OURS));
        assert!(!joins[0].connected, "Windows moved our adapter to another network");
    }

    #[test]
    fn our_adapters_completed_join_with_reason_zero_is_reported_with_its_channel() {
        let wlan = FakeWlan {
            guid: "11111111-2222-3333-4444-555555555555",
            disconnects: std::cell::Cell::new(0),
        };
        let (events, received) = mpsc::channel();
        let mut joins = vec![join(7, b"DIRECT-ab", false)];
        handle_acm(&wlan, &mut joins, &events, acm(AcmKind::Complete, b"DIRECT-ab", 0, OURS));
        assert!(joins[0].connected);
        match received.try_recv() {
            Ok(JoinEvent::Joined { generation: 7, channel: Some(149) }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_join_before_the_worker_knows_its_interface_is_busy_not_a_local_fault() {
        assert!(matches!(join_gate(false), Err(PlatformError::InUse)));
        assert!(join_gate(true).is_ok());
    }

    #[test]
    fn the_interface_guid_is_spelled_like_the_adapter_name() {
        let guid = GUID::from_u128(0x11111111_2222_3333_4444_555555555555);
        let text = guid_text(guid);
        assert_eq!(text, "{11111111-2222-3333-4444-555555555555}");
        assert!(crate::wifi::guest_platform::same_guid_text(&text, "11111111-2222-3333-4444-555555555555"));
    }

    #[test]
    fn a_connect_bound_to_a_local_address_reaches_its_listener_and_a_closed_port_is_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(open) = listener.local_addr().unwrap() else {
            panic!("ipv4")
        };
        let socket = start_connect(Ipv4Addr::LOCALHOST, open).expect("connect started");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match poll_connect(socket) {
                ConnectPoll::Pending => {
                    assert!(Instant::now() < deadline, "no connect");
                    std::thread::sleep(Duration::from_millis(1));
                }
                ConnectPoll::Connected(_) => break,
                other => panic!("unexpected {}", other.name()),
            }
        }
        let (_accepted, from) = listener.accept().unwrap();
        assert_eq!(from.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), "bound to the local address");
        let closed = {
            let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let std::net::SocketAddr::V4(address) = unused.local_addr().unwrap() else {
                panic!("ipv4")
            };
            address
        };
        let socket = start_connect(Ipv4Addr::LOCALHOST, closed).expect("connect started");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match poll_connect(socket) {
                ConnectPoll::Pending => {
                    assert!(Instant::now() < deadline, "no answer from a closed port");
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => {
                    assert_eq!(result.name(), "refused");
                    break;
                }
            }
        }
    }
}
