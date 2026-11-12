//! A non-blocking accepted Wi-Fi socket: `TCP_NODELAY`, a bounded outgoing
//! buffer, a write-stall rule and the oldest-unsent age used by the quality
//! rule. std + WinSock only; no WinRT.

use crate::{
    bluetooth::carrier::Error,
    wifi::limits::{CARRIER_DEAD_STALL_MS, OUT_BUFFER_MAX_BYTES, SOCKET_SEND_BUFFER_BYTES},
};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{Ipv4Addr, Shutdown, TcpStream},
    os::windows::io::AsRawSocket,
    time::{Duration, Instant},
};
use windows::Win32::Networking::WinSock::{
    POLLRDNORM, SOCKET, SOCKET_ERROR, SO_KEEPALIVE, SO_SNDBUF, SOL_SOCKET, WSAPOLLFD, WSAPoll,
    setsockopt,
};

pub struct WifiSocket {
    stream: TcpStream,
    out: Vec<u8>,
    out_sent: usize,
    sent_total: u64,
    /// `(end offset after this write, queued at)`, one entry per write.
    queued: VecDeque<(u64, Instant)>,
    /// Where the first queued record starts (WP6NG: `discard_unstarted`).
    queued_start: u64,
    progress: Instant,
    /// The write side failed or the peer closed it: no more writes.
    closed: bool,
    /// `shutdown` ran: no more reads either. A failed write alone still lets
    /// the bytes the peer already sent be read.
    shut: bool,
    #[cfg(test)]
    force_stall: bool,
    /// Test only: the kernel takes nothing (records stay unsent).
    #[cfg(test)]
    hold: bool,
}

impl WifiSocket {
    /// Takes an accepted stream: non-blocking, `TCP_NODELAY`, send buffer 256 KiB, keep-alive off.
    pub fn from_stream(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        let raw = SOCKET(stream.as_raw_socket() as usize);
        let send_buffer = (SOCKET_SEND_BUFFER_BYTES as i32).to_ne_bytes();
        let keepalive = 0i32.to_ne_bytes();
        unsafe {
            if setsockopt(raw, SOL_SOCKET, SO_SNDBUF, Some(&send_buffer)) == SOCKET_ERROR {
                return Err(std::io::Error::last_os_error());
            }
            if setsockopt(raw, SOL_SOCKET, SO_KEEPALIVE, Some(&keepalive)) == SOCKET_ERROR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self {
            stream,
            out: Vec::new(),
            out_sent: 0,
            sent_total: 0,
            queued: VecDeque::new(),
            queued_start: 0,
            progress: Instant::now(),
            closed: false,
            shut: false,
            #[cfg(test)]
            force_stall: false,
            #[cfg(test)]
            hold: false,
        })
    }

    /// Waits up to `deadline` for bytes (WSAPoll on POLLRDNORM; 0 wait = one try).
    /// `Ok(0)` is never returned: a closed peer is `Err(Closed)`.
    pub fn read(
        &self,
        out: &mut [u8],
        deadline: Instant,
    ) -> Result<usize, crate::bluetooth::carrier::Error> {
        if self.shut {
            return Err(Error::Closed);
        }
        // Rounded up: a wait under 1 ms still waits (0 ms is one try, by design).
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_micros().div_ceil(1_000).min(i32::MAX as u128) as i32;
        let mut descriptor = WSAPOLLFD {
            fd: SOCKET(self.stream.as_raw_socket() as usize),
            events: POLLRDNORM,
            revents: Default::default(),
        };
        let ready = unsafe { WSAPoll(&mut descriptor, 1, timeout_ms) };
        if ready == SOCKET_ERROR {
            return Err(Error::Backend);
        }
        if ready == 0 {
            return Err(Error::Timeout);
        }
        match (&self.stream).read(out) {
            Ok(0) => Err(Error::Closed),
            Ok(count) => Ok(count),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Err(Error::Timeout)
            }
            Err(_) => Err(Error::Backend),
        }
    }

    /// Age of the oldest written record whose bytes the kernel has not accepted yet.
    /// `None` when nothing waits.
    pub fn oldest_unsent_age(&self, now: Instant) -> Option<Duration> {
        self.queued
            .iter()
            .find(|(end, _)| *end > self.sent_total)
            .map(|(_, queued_at)| now.saturating_duration_since(*queued_at))
    }

    /// Appends to the outgoing buffer and tries to send at once. Never blocks.
    pub fn write(
        &mut self,
        bytes: &[u8],
        _deadline: Instant,
    ) -> Result<(), crate::bluetooth::carrier::Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.out.len() - self.out_sent + bytes.len() > OUT_BUFFER_MAX_BYTES {
            return Err(Error::Overflow);
        }
        if self.out.len() == self.out_sent {
            self.progress = Instant::now();
        }
        if self.queued.is_empty() {
            self.queued_start = self.sent_total + (self.out.len() - self.out_sent) as u64;
        }
        self.out.extend_from_slice(bytes);
        self.queued
            .push_back((self.sent_total + (self.out.len() - self.out_sent) as u64, Instant::now()));
        self.service_at(Instant::now())
    }

    /// WP6NG finding 7: drops every queued record the kernel has not started
    /// to take; a record already partly sent is kept whole, so the stream
    /// stays framed. Later writes are unaffected. Returns how many whole
    /// records were dropped (WP6NH: the owner retires a carrier that lost any).
    pub fn discard_unstarted(&mut self) -> usize {
        while let Some(&(end, _)) = self.queued.front() {
            if end > self.sent_total {
                break;
            }
            self.queued_start = end;
            self.queued.pop_front();
        }
        let waiting = self.queued.len();
        let keep = match self.queued.front() {
            Some(&(end, _)) if self.sent_total > self.queued_start => {
                self.queued.truncate(1);
                (end - self.sent_total) as usize
            }
            _ => {
                self.queued.clear();
                0
            }
        };
        self.out.truncate(self.out_sent + keep);
        waiting - self.queued.len()
    }
    /// Sends what the kernel accepts. `Err(Timeout)` when data waited 2.5 s with no progress.
    pub fn service(&mut self) -> Result<(), crate::bluetooth::carrier::Error> {
        self.service_at(Instant::now())
    }

    pub(crate) fn service_at(
        &mut self,
        now: Instant,
    ) -> Result<(), crate::bluetooth::carrier::Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        #[cfg(test)]
        if self.force_stall {
            return Err(Error::Timeout);
        }
        #[cfg(test)]
        if self.hold {
            return Ok(());
        }
        if self.out_sent >= self.out.len() {
            self.out.clear();
            self.out_sent = 0;
            return Ok(());
        }
        match (&self.stream).write(&self.out[self.out_sent..]) {
            Ok(0) => {
                self.closed = true;
                Err(Error::Closed)
            }
            Ok(count) => {
                self.out_sent += count;
                self.sent_total += count as u64;
                self.progress = now;
                while self
                    .queued
                    .front()
                    .is_some_and(|(end, _)| *end <= self.sent_total)
                {
                    if let Some((end, _)) = self.queued.pop_front() {
                        self.queued_start = end;
                    }
                }
                if self.out_sent > 64 * 1024 {
                    self.out.drain(..self.out_sent);
                    self.out_sent = 0;
                }
                Ok(())
            }
            // Interrupted sent nothing: like WouldBlock, it is tried again.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                if now.saturating_duration_since(self.progress)
                    >= Duration::from_millis(CARRIER_DEAD_STALL_MS as u64)
                {
                    Err(Error::Timeout)
                } else {
                    Ok(())
                }
            }
            Err(_) => {
                self.closed = true;
                Err(Error::Closed)
            }
        }
    }

    /// Used by the listener filter in W3.
    pub fn peer_in_subnet(&self, ip: Ipv4Addr, prefix: u8) -> bool {
        let Ok(peer) = self.stream.peer_addr() else {
            return false;
        };
        let std::net::IpAddr::V4(peer) = peer.ip() else {
            return false;
        };
        let prefix = prefix.min(32);
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        (u32::from(ip) & mask) == (u32::from(peer) & mask)
    }

    /// Both directions, idempotent; `Drop` calls it.
    pub fn shutdown(&mut self) {
        if !self.shut {
            let _ = self.stream.shutdown(Shutdown::Both);
            self.closed = true;
            self.shut = true;
        }
    }

    #[cfg(test)]
    pub(crate) fn force_stall(&mut self) {
        self.force_stall = true;
    }

    /// Test only: while held, the kernel takes nothing; release sends at the next service.
    #[cfg(test)]
    pub(crate) fn hold_sends(&mut self, hold: bool) {
        self.hold = hold;
    }
}

impl Drop for WifiSocket {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests;
