//! A tested non-blocking listener for the Wi-Fi Direct group interface. std only.

use crate::wifi::{
    address::{self, GroupAddress},
    socket::WifiSocket,
};
use std::net::{IpAddr, SocketAddr, TcpListener};
#[cfg(test)]
use std::net::Ipv4Addr;

pub struct GroupListener {
    listener: TcpListener,
    filter: Box<dyn Fn(SocketAddr) -> bool>,
}

pub fn bind(address: GroupAddress) -> std::io::Result<(GroupListener, u16)> {
    let listener = TcpListener::bind((address.ip, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    Ok((
        GroupListener {
            listener,
            filter: Box::new(move |peer| admit(peer, address)),
        },
        port,
    ))
}

impl GroupListener {
    /// Accepts until `WouldBlock`; a refused peer is dropped and the next is tried.
    pub fn accept(&mut self) -> Option<WifiSocket> {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if !(self.filter)(peer) {
                        drop(stream);
                        continue;
                    }
                    return WifiSocket::from_stream(stream).ok();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return None,
                Err(_) => return None,
            }
        }
    }
}

pub fn admit(peer: SocketAddr, group: GroupAddress) -> bool {
    match peer.ip() {
        IpAddr::V4(ip) => address::same_subnet(group, ip),
        IpAddr::V6(_) => false,
    }
}

#[cfg(test)]
pub(crate) fn bind_for_tests(
    ip: Ipv4Addr,
    admit: fn(SocketAddr) -> bool,
) -> std::io::Result<(GroupListener, u16)> {
    let listener = TcpListener::bind((ip, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    Ok((
        GroupListener {
            listener,
            filter: Box::new(admit),
        },
        port,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        net::TcpStream,
        time::{Duration, Instant},
    };

    #[test]
    fn the_group_listener_never_blocks_when_nobody_connects() {
        let (mut listener, _) = bind_for_tests("127.0.0.1".parse().unwrap(), |_| true).unwrap();
        let started = Instant::now();
        assert!(listener.accept().is_none());
        assert!(started.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn the_group_listener_drops_a_refused_peer_and_keeps_listening() {
        let (mut refusing, port) = bind_for_tests("127.0.0.1".parse().unwrap(), |_| false).unwrap();
        let mut first = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(refusing.accept().is_none());
        let mut byte = [0u8; 1];
        assert_eq!(first.read(&mut byte).unwrap_or(0), 0);
        drop(refusing);

        let (mut accepting, port) = bind_for_tests("127.0.0.1".parse().unwrap(), |_| true).unwrap();
        let _second = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(accepting.accept().is_some());
    }

    #[test]
    fn the_group_listener_hands_out_a_nonblocking_socket_for_an_admitted_peer() {
        let (mut listener, port) = bind_for_tests("127.0.0.1".parse().unwrap(), |_| true).unwrap();
        let _client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let socket = listener.accept().expect("an admitted socket");
        let mut bytes = [0u8; 1];
        assert_eq!(socket.read(&mut bytes, Instant::now()), Err(crate::bluetooth::carrier::Error::Timeout));
    }

    #[test]
    fn admit_refuses_ipv6_and_peers_outside_the_group_subnet() {
        let group = GroupAddress {
            ip: "192.168.137.1".parse().unwrap(),
            prefix: 24,
        };
        assert!(admit("192.168.137.5:5000".parse().unwrap(), group));
        assert!(!admit("192.168.138.5:5000".parse().unwrap(), group));
        assert!(!admit("[::1]:5000".parse().unwrap(), group));
    }
}
