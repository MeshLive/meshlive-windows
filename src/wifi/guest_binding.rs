//! W4G: the guest side of `BWU1` and the first sealed `BIND`/`READY` records on the
//! socket this PC opened to the phone's `CREDS` address. Never blocks; every deadline
//! uses the caller's monotonic ms. The keys come only from the call's `UW`, the HOST
//! ticket and this socket's transcript (PROTOCOL "Binding").

use super::{
    binding::{BindFailure, Bound, Step},
    keys,
    limits::{
        BIND_READY_DEADLINE_MS, GUEST_FRAME_DEADLINE_MS, READY_FAMILY_CAP_KBPS,
        READY_RECV_CEILING_KBPS, READY_TRANSPORT_BUDGET_KBPS,
    },
    socket::WifiSocket,
    wire::{self, Bwf1},
};
use crate::{
    crypto,
    framing::HopReader,
    records::{Layer, Policy, Sealer},
};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Frame1,
    Frame3,
    Bind,
    Ready,
    /// Failed or finished: the socket is gone and nothing is ever written again.
    Over,
}

pub struct GuestBinding {
    socket: Option<WifiSocket>,
    uw: Zeroizing<[u8; 32]>,
    ticket: [u8; 8],
    connected_ms: i64,
    frame2_ms: i64,
    stage: Stage,
    buffer: Vec<u8>,
    frame1: [u8; wire::BWU1_FRAME1_BYTES],
    frame2: [u8; wire::BWU1_FRAME2_BYTES],
    ephemeral: Option<crypto::Ephemeral>,
    hop_out: Option<Sealer>,
    hop_in: Option<HopReader>,
    /// Frame 3 verified and the carrier keys derived; stays true after `Done`.
    keys_derived: bool,
}
impl std::fmt::Debug for GuestBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestBinding")
            .field("socket", &"<redacted>")
            .field("uw", &"<redacted>")
            .field("ticket", &crate::wifi::log::tag(&self.ticket))
            .field("stage", &self.stage)
            .field("buffered", &self.buffer.len())
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl GuestBinding {
    /// Takes the socket connected to the phone at `connected_ms`; writes nothing yet.
    pub fn new(socket: WifiSocket, uw: &[u8; 32], ticket: [u8; 8], connected_ms: i64) -> Self {
        Self {
            socket: Some(socket),
            uw: Zeroizing::new(*uw),
            ticket,
            connected_ms,
            frame2_ms: connected_ms,
            stage: Stage::Frame1,
            buffer: Vec::new(),
            frame1: [0; wire::BWU1_FRAME1_BYTES],
            frame2: [0; wire::BWU1_FRAME2_BYTES],
            ephemeral: None,
            hop_out: None,
            hop_in: None,
            keys_derived: false,
        }
    }

    /// Frame 3 verified and the carrier keys derived (`BIND` written): the attempt may be
    /// registered with the cutover now, like the host's ticket consumption.
    pub fn keys_ready(&self) -> bool {
        self.keys_derived
    }

    pub fn poll(&mut self, now_ms: i64) -> Step {
        let step = match self.stage {
            Stage::Frame1 => self.poll_frame1(now_ms),
            Stage::Frame3 => self.poll_frame3(now_ms),
            Stage::Bind => self.poll_bind(now_ms),
            Stage::Ready => self.poll_ready(now_ms),
            Stage::Over => Step::Failed(BindFailure::Closed),
        };
        if !matches!(step, Step::Pending) {
            // Failed: the socket closes silently now. Done: the socket left with `Bound`.
            self.stage = Stage::Over;
            self.socket = None;
            self.ephemeral = None;
        }
        step
    }

    fn poll_frame1(&mut self, now_ms: i64) -> Step {
        let needed = wire::BWU1_FRAME1_BYTES.saturating_sub(self.buffer.len());
        if needed > 0 {
            if let Err(failure) = self.read_more(needed) {
                return Step::Failed(failure);
            }
        }
        if self.buffer.len() < wire::BWU1_FRAME1_BYTES {
            if now_ms - self.connected_ms >= GUEST_FRAME_DEADLINE_MS {
                return Step::Failed(BindFailure::Frame1Timeout);
            }
            return Step::Pending;
        }
        let frame1: [u8; wire::BWU1_FRAME1_BYTES] = self.buffer[..wire::BWU1_FRAME1_BYTES]
            .try_into()
            .expect("21 bytes");
        self.buffer.drain(..wire::BWU1_FRAME1_BYTES);
        let challenge = match wire::parse_frame1(&frame1) {
            Ok(challenge) => challenge,
            Err(_) => return Step::Failed(BindFailure::Frame1Format),
        };
        let ephemeral = match crypto::Ephemeral::generate() {
            Ok(ephemeral) => ephemeral,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let spki = match ephemeral.public_key() {
            Ok(spki) => spki,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let e_g = keys::point_of(&spki);
        let n_g = match crypto::random_nonce() {
            Ok(nonce) => nonce,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let pre = match keys::pre(&self.uw, &self.ticket, &challenge, &e_g, &n_g) {
            Ok(pre) => pre,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let frame2 = wire::frame2(&self.ticket, &e_g, &n_g, &pre);
        if let Err(failure) = self.write_bytes(&frame2) {
            return Step::Failed(failure);
        }
        self.frame1 = frame1;
        self.frame2 = frame2;
        self.ephemeral = Some(ephemeral);
        self.frame2_ms = now_ms;
        self.stage = Stage::Frame3;
        self.poll_frame3(now_ms)
    }

    fn poll_frame3(&mut self, now_ms: i64) -> Step {
        let needed = wire::BWU1_FRAME3_BYTES.saturating_sub(self.buffer.len());
        if needed > 0 {
            if let Err(failure) = self.read_more(needed) {
                return Step::Failed(failure);
            }
        }
        if self.buffer.len() < wire::BWU1_FRAME3_BYTES {
            if now_ms - self.frame2_ms >= GUEST_FRAME_DEADLINE_MS
                || now_ms - self.connected_ms >= BIND_READY_DEADLINE_MS
            {
                return Step::Failed(BindFailure::Frame3Timeout);
            }
            return Step::Pending;
        }
        let frame3: [u8; wire::BWU1_FRAME3_BYTES] = self.buffer[..wire::BWU1_FRAME3_BYTES]
            .try_into()
            .expect("102 bytes");
        self.buffer.drain(..wire::BWU1_FRAME3_BYTES);
        let (e_h, _n_h) = match wire::parse_frame3(&frame3) {
            Ok(parts) => parts,
            Err(_) => return Step::Failed(BindFailure::Frame3Format),
        };
        // The host point is validated before any agreement (as in BMH3).
        let Some(host_spki) = keys::spki_of(&e_h) else {
            return Step::Failed(BindFailure::BadPoint);
        };
        if crypto::validate_agreement_public(&host_spki).is_err() {
            return Step::Failed(BindFailure::BadPoint);
        }
        let th = match keys::transcript_hash(&self.frame1, &self.frame2, &frame3) {
            Ok(th) => th,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let Some(ephemeral) = self.ephemeral.take() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let dh = match ephemeral.agree(&host_spki) {
            Ok(dh) => dh,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let bind_keys = match keys::bind_keys(&self.uw, &self.ticket, &th, &dh) {
            Ok(keys) => keys,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let policy = Policy {
            wifi: true,
            ..Policy::default()
        };
        let mut hop_out = match Sealer::new(Layer::Hop, &bind_keys.g2h, policy) {
            Ok(sealer) => sealer,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let hop_in = match HopReader::new(&bind_keys.h2g, policy) {
            Ok(reader) => reader,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let bind = Bwf1::Bind(wire::Bind {
            role: wire::role::GUEST,
            ticket: self.ticket,
        });
        let Ok(plain) = bind.encode() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let sealed = match hop_out.seal(wire::HOP_TYPE, 0, &plain) {
            Ok(sealed) => sealed,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        self.hop_out = Some(hop_out);
        self.hop_in = Some(hop_in);
        self.keys_derived = true;
        if let Err(failure) = self.write_bytes(&sealed) {
            return Step::Failed(failure);
        }
        self.stage = Stage::Bind;
        self.poll_bind(now_ms)
    }

    fn poll_bind(&mut self, now_ms: i64) -> Step {
        if now_ms - self.connected_ms >= BIND_READY_DEADLINE_MS {
            return Step::Failed(BindFailure::BindRead);
        }
        let record = match self.next_record() {
            Ok(Some(record)) => record,
            Ok(None) => return Step::Pending,
            Err(_) => return Step::Failed(BindFailure::BindRead),
        };
        if record.header.kind != wire::HOP_TYPE {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let bind = match Bwf1::decode(&record.plaintext) {
            Ok(Bwf1::Bind(bind)) => bind,
            _ => return Step::Failed(BindFailure::BindMismatch),
        };
        if bind.role != wire::role::HOST || bind.ticket.ct_eq(&self.ticket).unwrap_u8() == 0 {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let ready = Bwf1::Ready(wire::Ready {
            recv_ceiling_kbps: READY_RECV_CEILING_KBPS,
            family_cap_kbps: READY_FAMILY_CAP_KBPS,
            transport_budget_kbps: READY_TRANSPORT_BUDGET_KBPS,
        });
        let Ok(plain) = ready.encode() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let Some(hop_out) = self.hop_out.as_mut() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let sealed = match hop_out.seal(wire::HOP_TYPE, 0, &plain) {
            Ok(sealed) => sealed,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        if let Err(failure) = self.write_bytes(&sealed) {
            return Step::Failed(failure);
        }
        self.stage = Stage::Ready;
        self.poll_ready(now_ms)
    }

    fn poll_ready(&mut self, now_ms: i64) -> Step {
        if now_ms - self.connected_ms >= BIND_READY_DEADLINE_MS {
            return Step::Failed(BindFailure::ReadyTimeout);
        }
        let record = match self.next_record() {
            Ok(Some(record)) => record,
            Ok(None) => return Step::Pending,
            Err(failure) => return Step::Failed(failure),
        };
        if record.header.kind != wire::HOP_TYPE {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let peer_ready = match Bwf1::decode(&record.plaintext) {
            Ok(Bwf1::Ready(ready)) => ready,
            _ => return Step::Failed(BindFailure::BindMismatch),
        };
        let (Some(hop_out), Some(hop_in), Some(socket)) =
            (self.hop_out.take(), self.hop_in.take(), self.socket.take())
        else {
            return Step::Failed(BindFailure::Crypto);
        };
        let leftover = std::mem::take(&mut self.buffer);
        Step::Done(Box::new(Bound {
            socket,
            hop_out,
            hop_in,
            peer_ready,
            leftover,
        }))
    }

    /// The next complete record: from the buffer, else after one non-blocking read.
    fn next_record(&mut self) -> Result<Option<crate::records::Record>, BindFailure> {
        if let Some(record) = self.buffered_record()? {
            return Ok(Some(record));
        }
        self.read_more(4096)?;
        self.buffered_record()
    }

    fn buffered_record(&mut self) -> Result<Option<crate::records::Record>, BindFailure> {
        let Some(hop_in) = self.hop_in.as_mut() else {
            return Err(BindFailure::Crypto);
        };
        let mut input = self.buffer.as_slice();
        let result = hop_in.read(&mut input, 4096);
        let consumed = self.buffer.len() - input.len();
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        result.map_err(|_| BindFailure::Stalled)
    }

    fn read_more(&mut self, limit: usize) -> Result<(), BindFailure> {
        if limit == 0 {
            return Ok(());
        }
        let mut chunk = [0u8; 4096];
        let want = limit.min(chunk.len());
        let Some(socket) = self.socket.as_ref() else {
            return Err(BindFailure::Closed);
        };
        match socket.read(&mut chunk[..want], Instant::now()) {
            Ok(0) => Err(BindFailure::Closed),
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                Ok(())
            }
            Err(crate::bluetooth::carrier::Error::Timeout) => Ok(()),
            Err(crate::bluetooth::carrier::Error::Closed) => Err(BindFailure::Closed),
            Err(_) => Err(BindFailure::Stalled),
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), BindFailure> {
        let Some(socket) = self.socket.as_mut() else {
            return Err(BindFailure::Closed);
        };
        socket
            .write(bytes, Instant::now() + Duration::from_millis(100))
            .map_err(|error| match error {
                crate::bluetooth::carrier::Error::Closed => BindFailure::Closed,
                _ => BindFailure::Stalled,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::binding::HostBinding;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    const TICKET: [u8; 8] = [9; 8];

    fn sockets() -> (TcpStream, WifiSocket) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let guest = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (host, _) = listener.accept().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        (host, WifiSocket::from_stream(guest).unwrap())
    }

    /// Polls until the step is not `Pending` (at most 300 ms of real time: loopback bytes
    /// arrive well within it); `now_ms` stays fixed.
    fn settle(binding: &mut GuestBinding, now_ms: i64) -> Step {
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match binding.poll(now_ms) {
                Step::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                step => return step,
            }
        }
    }

    fn failure(step: Step) -> Option<BindFailure> {
        match step {
            Step::Failed(failure) => Some(failure),
            _ => None,
        }
    }

    /// The real host binding against the guest, both polled on this thread.
    fn bind_with_host(host_uw: [u8; 32], guest_uw: [u8; 32]) -> (Step, Option<Box<Bound>>, bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let guest_stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (host_stream, _) = listener.accept().unwrap();
        let mut host =
            Some(HostBinding::new(WifiSocket::from_stream(host_stream).unwrap(), &host_uw, TICKET, 0).unwrap());
        let mut guest = GuestBinding::new(WifiSocket::from_stream(guest_stream).unwrap(), &guest_uw, TICKET, 0);
        assert!(!guest.keys_ready());
        let mut host_bound = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut now_ms = 0;
        loop {
            if let Some(binding) = host.as_mut() {
                match binding.poll(0) {
                    Step::Pending => {}
                    Step::Done(bound) => {
                        host_bound = Some(bound);
                        host = None;
                    }
                    Step::Failed(_) => host = None,
                }
            }
            match guest.poll(now_ms) {
                Step::Pending => {}
                step => {
                    let keys_ready = guest.keys_ready();
                    // Let the host read the guest's READY before the test looks at it.
                    for _ in 0..200 {
                        if host_bound.is_some() || host.is_none() {
                            break;
                        }
                        if let Some(binding) = host.as_mut() {
                            if let Step::Done(bound) = binding.poll(0) {
                                host_bound = Some(bound);
                                host = None;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    return (step, host_bound, keys_ready);
                }
            }
            if Instant::now() > deadline {
                // The host is gone: let the guest's own deadlines decide.
                now_ms += 1_000;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Reads exactly one sealed record (17-byte header, size at bytes 13..17) and opens it.
    fn read_one(socket: &WifiSocket, reader: &mut HopReader) -> crate::records::Record {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut bytes = Vec::new();
        loop {
            let mut wanted = 17;
            if bytes.len() >= 17 {
                wanted = 17 + u32::from_be_bytes(bytes[13..17].try_into().unwrap()) as usize;
                if bytes.len() >= wanted {
                    let mut input = &bytes[..wanted];
                    return reader.read(&mut input, usize::MAX).unwrap().expect("record");
                }
            }
            assert!(Instant::now() < deadline, "no record");
            let mut chunk = vec![0u8; wanted - bytes.len()];
            match socket.read(&mut chunk, Instant::now() + Duration::from_millis(50)) {
                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                Err(crate::bluetooth::carrier::Error::Timeout) => {}
                Err(error) => panic!("read failed: {error:?}"),
            }
        }
    }

    /// A raw host for the misbehaving cases: frame 1, frame 2, frame 3 and its own keys.
    struct RawHost {
        stream: TcpStream,
        frame1: [u8; wire::BWU1_FRAME1_BYTES],
    }
    impl RawHost {
        fn start(guest_uw: [u8; 32]) -> (Self, GuestBinding) {
            let (stream, guest_socket) = sockets();
            let mut host = Self {
                stream,
                frame1: wire::frame1(&[0x33; 16]),
            };
            let frame1 = host.frame1;
            host.stream.write_all(&frame1).unwrap();
            (host, GuestBinding::new(guest_socket, &guest_uw, TICKET, 0))
        }
        fn read_frame2(&mut self) -> [u8; wire::BWU1_FRAME2_BYTES] {
            let mut frame2 = [0u8; wire::BWU1_FRAME2_BYTES];
            self.stream.read_exact(&mut frame2).unwrap();
            frame2
        }
        /// Answers frame 2 with a valid frame 3 and returns the host's sealer and reader.
        fn answer(&mut self, uw: &[u8; 32]) -> (Sealer, HopReader) {
            let frame2 = self.read_frame2();
            let parsed = wire::parse_frame2(&frame2).unwrap();
            let ephemeral = crypto::Ephemeral::generate().unwrap();
            let e_h = keys::point_of(&ephemeral.public_key().unwrap());
            let frame3 = wire::frame3(&e_h, &[0x44; 32]);
            self.stream.write_all(&frame3).unwrap();
            let th = keys::transcript_hash(&self.frame1, &frame2, &frame3).unwrap();
            let dh = ephemeral.agree(&keys::spki_of(&parsed.e_g).unwrap()).unwrap();
            let keys = keys::bind_keys(uw, &TICKET, &th, &dh).unwrap();
            let policy = Policy {
                wifi: true,
                ..Policy::default()
            };
            (
                Sealer::new(Layer::Hop, &keys.h2g, policy).unwrap(),
                HopReader::new(&keys.g2h, policy).unwrap(),
            )
        }
    }

    #[test]
    fn a_guest_binds_with_the_real_host_and_both_talk_over_the_new_keys() {
        let (step, host_bound, keys_ready) = bind_with_host([7; 32], [7; 32]);
        let Step::Done(mut guest_bound) = step else {
            panic!("guest failed: {:?}", failure(step).map(BindFailure::name));
        };
        assert!(keys_ready);
        let mut host_bound = host_bound.expect("the host finished BIND and READY too");
        let close = Bwf1::Close(wire::Close { reason: 1 }).encode().unwrap();
        let sealed = guest_bound.hop_out.seal(wire::HOP_TYPE, 0, &close).unwrap();
        guest_bound
            .socket
            .write(&sealed, Instant::now() + Duration::from_millis(500))
            .unwrap();
        let record = read_one(&host_bound.socket, &mut host_bound.hop_in);
        assert_eq!(record.header.kind, wire::HOP_TYPE);
        assert_eq!(Bwf1::decode(&record.plaintext).unwrap(), Bwf1::Close(wire::Close { reason: 1 }));
    }

    #[test]
    fn a_host_with_another_uw_never_gives_the_guest_a_carrier() {
        let (step, host_bound, keys_ready) = bind_with_host([8; 32], [7; 32]);
        assert!(host_bound.is_none());
        assert!(!keys_ready, "no keys without the host's frame 3");
        assert!(
            matches!(
                failure(step),
                Some(BindFailure::Closed | BindFailure::Stalled | BindFailure::Frame3Timeout)
            ),
            "the guest gives up and never binds"
        );
    }

    #[test]
    fn the_hosts_frame_1_must_arrive_within_two_seconds_of_connect() {
        let (_host, guest_socket) = sockets();
        let mut guest = GuestBinding::new(guest_socket, &[7; 32], TICKET, 0);
        assert!(matches!(guest.poll(GUEST_FRAME_DEADLINE_MS - 1), Step::Pending));
        assert_eq!(failure(guest.poll(GUEST_FRAME_DEADLINE_MS)), Some(BindFailure::Frame1Timeout));
        assert_eq!(failure(guest.poll(0)), Some(BindFailure::Closed), "a failed binding stays failed");
    }

    #[test]
    fn a_malformed_frame_1_fails_and_no_frame_2_is_written() {
        let (mut host, guest_socket) = sockets();
        let mut bad = wire::frame1(&[0x33; 16]);
        bad[4] = 2;
        host.write_all(&bad).unwrap();
        let mut guest = GuestBinding::new(guest_socket, &[7; 32], TICKET, 0);
        assert_eq!(failure(settle(&mut guest, 0)), Some(BindFailure::Frame1Format));
        host.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut byte = [0u8; 1];
        assert!(!matches!(host.read(&mut byte), Ok(1)), "nothing follows a bad frame 1");
    }

    #[test]
    fn frame_2_carries_the_ticket_and_a_pre_bound_to_the_hosts_challenge() {
        let (mut host, mut guest) = RawHost::start([7; 32]);
        assert!(matches!(settle(&mut guest, 0), Step::Pending));
        let frame2 = wire::parse_frame2(&host.read_frame2()).unwrap();
        assert_eq!(frame2.ticket, TICKET);
        let expected = keys::pre(&[7; 32], &TICKET, &[0x33; 16], &frame2.e_g, &frame2.n_g).unwrap();
        assert!(keys::eq16(&expected, &frame2.pre));
    }

    #[test]
    fn the_hosts_frame_3_must_arrive_within_two_seconds_of_frame_2() {
        let (mut host, mut guest) = RawHost::start([7; 32]);
        assert!(matches!(settle(&mut guest, 100), Step::Pending));
        let _ = host.read_frame2();
        assert!(matches!(guest.poll(100 + GUEST_FRAME_DEADLINE_MS - 1), Step::Pending));
        assert_eq!(
            failure(guest.poll(100 + GUEST_FRAME_DEADLINE_MS)),
            Some(BindFailure::Frame3Timeout)
        );
    }

    #[test]
    fn a_host_point_off_the_curve_fails_before_any_bind_is_written() {
        let (mut host, mut guest) = RawHost::start([7; 32]);
        assert!(matches!(settle(&mut guest, 0), Step::Pending));
        let _ = host.read_frame2();
        let mut off_curve = [0xff; 65];
        off_curve[0] = 4;
        host.stream.write_all(&wire::frame3(&off_curve, &[0x44; 32])).unwrap();
        assert_eq!(failure(settle(&mut guest, 0)), Some(BindFailure::BadPoint));
        assert!(!guest.keys_ready());
        host.stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut byte = [0u8; 1];
        assert!(!matches!(host.stream.read(&mut byte), Ok(1)), "no BIND after a bad point");
    }

    #[test]
    fn a_host_bind_with_the_wrong_role_ticket_or_hop_type_is_refused() {
        for (hop_type, role, ticket) in [
            (wire::HOP_TYPE, wire::role::GUEST, TICKET),
            (wire::HOP_TYPE, wire::role::HOST, [1; 8]),
            (6, wire::role::HOST, TICKET),
        ] {
            let (mut host, mut guest) = RawHost::start([7; 32]);
            assert!(matches!(settle(&mut guest, 0), Step::Pending));
            let (mut sealer, _reader) = host.answer(&[7; 32]);
            let bind = Bwf1::Bind(wire::Bind { role, ticket }).encode().unwrap();
            host.stream.write_all(&sealer.seal(hop_type, 0, &bind).unwrap()).unwrap();
            assert_eq!(
                failure(settle(&mut guest, 0)),
                Some(BindFailure::BindMismatch),
                "type {hop_type} role {role}"
            );
            assert!(guest.keys_ready(), "frame 3 was valid");
        }
    }

    #[test]
    fn the_guest_writes_its_bind_without_waiting_for_the_hosts() {
        let (mut host, mut guest) = RawHost::start([7; 32]);
        assert!(matches!(settle(&mut guest, 0), Step::Pending));
        let (_sealer, mut reader) = host.answer(&[7; 32]);
        assert!(matches!(settle(&mut guest, 0), Step::Pending));
        let mut header = [0u8; 17];
        host.stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[13..17].try_into().unwrap()) as usize;
        let mut body = vec![0u8; size];
        host.stream.read_exact(&mut body).unwrap();
        let mut encoded = header.to_vec();
        encoded.extend_from_slice(&body);
        let mut input = encoded.as_slice();
        let record = reader.read(&mut input, usize::MAX).unwrap().unwrap();
        assert_eq!(record.header.kind, wire::HOP_TYPE);
        assert_eq!(
            Bwf1::decode(&record.plaintext).unwrap(),
            Bwf1::Bind(wire::Bind {
                role: wire::role::GUEST,
                ticket: TICKET
            })
        );
    }

    #[test]
    fn bind_and_ready_must_finish_within_five_seconds_of_connect() {
        let (mut host, mut guest) = RawHost::start([7; 32]);
        assert!(matches!(settle(&mut guest, 0), Step::Pending));
        let _keys = host.answer(&[7; 32]);
        assert!(matches!(settle(&mut guest, 1_000), Step::Pending));
        assert!(matches!(guest.poll(BIND_READY_DEADLINE_MS - 1), Step::Pending));
        assert_eq!(failure(guest.poll(BIND_READY_DEADLINE_MS)), Some(BindFailure::BindRead));
    }

    #[test]
    fn the_debug_text_names_no_key_ticket_or_socket() {
        let (_host, guest_socket) = sockets();
        let guest = GuestBinding::new(guest_socket, &[0x7e; 32], [0x5a; 8], 0);
        let text = format!("{guest:?}");
        assert!(!text.contains("5a5a5a5a"), "{text}");
        assert!(!text.contains("7e7e7e7e"), "{text}");
        assert!(!text.contains("126"), "{text}");
    }
}
