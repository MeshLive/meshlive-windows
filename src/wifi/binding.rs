//! Host-side `BWU1` binding and the first sealed `BIND`/`READY` records on one
//! accepted socket. Never blocks; all deadlines use the caller's monotonic ms.

use super::{
    keys,
    limits::{
        BIND_FRAME2_DEADLINE_MS, BIND_READY_DEADLINE_MS, READY_FAMILY_CAP_KBPS,
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
pub enum BindFailure {
    Frame2Timeout,
    Frame2Format,
    BadPre,
    WrongTicket,
    BadPoint,
    Crypto,
    BindMismatch,
    BindRead,
    ReadyTimeout,
    Closed,
    Stalled,
    /// W4G guest: the host's frame 1 or frame 3 came late or malformed.
    Frame1Timeout,
    Frame1Format,
    Frame3Timeout,
    Frame3Format,
}
impl BindFailure {
    pub fn name(self) -> &'static str {
        match self {
            Self::Frame2Timeout => "frame2",
            Self::Frame2Format => "frame2_format",
            Self::BadPre => "pre",
            Self::WrongTicket => "ticket",
            Self::BadPoint => "point",
            Self::Crypto => "ecdh",
            Self::BindMismatch => "bind_check",
            Self::BindRead => "bind_read",
            Self::ReadyTimeout => "ready",
            Self::Closed => "closed",
            Self::Stalled => "stalled",
            Self::Frame1Timeout => "frame1",
            Self::Frame1Format => "frame1_format",
            Self::Frame3Timeout => "frame3",
            Self::Frame3Format => "frame3_format",
        }
    }
}

pub struct Bound {
    pub socket: WifiSocket,
    pub hop_out: Sealer,
    pub hop_in: HopReader,
    pub peer_ready: wire::Ready,
    /// Bytes read after the peer's `READY`, for the first session read.
    pub leftover: Vec<u8>,
}
impl std::fmt::Debug for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bound")
            .field("socket", &"<redacted>")
            .field("hop_out", &"<redacted>")
            .field("hop_in", &"<redacted>")
            .field("peer_ready", &self.peer_ready)
            .field("leftover", &self.leftover.len())
            .finish()
    }
}

pub enum Step {
    Pending,
    Done(Box<Bound>),
    Failed(BindFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Frame2,
    Bind,
    Ready,
}

pub struct HostBinding {
    socket: Option<WifiSocket>,
    uw: Zeroizing<[u8; 32]>,
    ticket: [u8; 8],
    challenge: [u8; 16],
    accepted_ms: i64,
    stage: Stage,
    buffer: Vec<u8>,
    frame1: [u8; 21],
    consumed: bool,
    hop_out: Option<Sealer>,
    hop_in: Option<HopReader>,
}
impl std::fmt::Debug for HostBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostBinding")
            .field("socket", &"<redacted>")
            .field("uw", &"<redacted>")
            .field("ticket", &crate::wifi::log::tag(&self.ticket))
            .field("challenge", &"<redacted>")
            .field("accepted_ms", &self.accepted_ms)
            .field("stage", &self.stage)
            .field("buffer", &self.buffer.len())
            .field("consumed", &self.consumed)
            .field("hop_out", &"<redacted>")
            .field("hop_in", &"<redacted>")
            .finish()
    }
}

impl HostBinding {
    /// Sends frame 1 (the fresh 16-byte challenge) at once.
    pub fn new(
        socket: WifiSocket,
        uw: &[u8; 32],
        ticket: [u8; 8],
        accepted_ms: i64,
    ) -> Result<Self, BindFailure> {
        let nonce = crypto::random_nonce().map_err(|_| BindFailure::Crypto)?;
        let challenge: [u8; 16] = nonce[..16].try_into().expect("sixteen bytes");
        let frame1 = wire::frame1(&challenge);
        let mut socket = socket;
        socket
            .write(&frame1, Instant::now() + Duration::from_millis(100))
            .map_err(map_socket)?;
        Ok(Self {
            socket: Some(socket),
            uw: Zeroizing::new(*uw),
            ticket,
            challenge,
            accepted_ms,
            stage: Stage::Frame2,
            buffer: Vec::new(),
            frame1,
            consumed: false,
            hop_out: None,
            hop_in: None,
        })
    }

    pub fn ticket_consumed(&self) -> bool {
        self.consumed
    }

    pub fn poll(&mut self, now_ms: i64) -> Step {
        match self.stage {
            Stage::Frame2 => self.poll_frame2(now_ms),
            Stage::Bind => self.poll_bind(now_ms),
            Stage::Ready => self.poll_ready(now_ms),
        }
    }

    fn poll_frame2(&mut self, now_ms: i64) -> Step {
        let needed = wire::BWU1_FRAME2_BYTES.saturating_sub(self.buffer.len());
        if needed > 0 {
            if let Err(failure) = self.read_more(needed) {
                return Step::Failed(failure);
            }
        }
        if self.buffer.len() < wire::BWU1_FRAME2_BYTES {
            if now_ms - self.accepted_ms >= BIND_FRAME2_DEADLINE_MS {
                return Step::Failed(BindFailure::Frame2Timeout);
            }
            return Step::Pending;
        }
        let frame2_bytes: [u8; wire::BWU1_FRAME2_BYTES] = self.buffer[..wire::BWU1_FRAME2_BYTES]
            .try_into()
            .expect("126 bytes");
        self.buffer.drain(..wire::BWU1_FRAME2_BYTES);
        let frame2 = match wire::parse_frame2(&frame2_bytes) {
            Ok(frame2) => frame2,
            Err(_) => return Step::Failed(BindFailure::Frame2Format),
        };
        // Verify `pre` first, before the ticket is compared or consumed and
        // before the point is validated.
        let expected = match keys::pre(&self.uw, &self.ticket, &self.challenge, &frame2.e_g, &frame2.n_g) {
            Ok(expected) => expected,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        if !keys::eq16(&expected, &frame2.pre) {
            return Step::Failed(BindFailure::BadPre);
        }
        if frame2.ticket.ct_eq(&self.ticket).unwrap_u8() == 0 {
            return Step::Failed(BindFailure::WrongTicket);
        }
        self.consumed = true;
        let spki = match keys::spki_of(&frame2.e_g) {
            Some(spki) => spki,
            None => return Step::Failed(BindFailure::BadPoint),
        };
        if crypto::validate_agreement_public(&spki).is_err() {
            return Step::Failed(BindFailure::BadPoint);
        }
        let ephemeral = match crypto::Ephemeral::generate() {
            Ok(ephemeral) => ephemeral,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let host_spki = match ephemeral.public_key() {
            Ok(spki) => spki,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let e_h = keys::point_of(&host_spki);
        let nonce = match crypto::random_nonce() {
            Ok(nonce) => nonce,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let n_h: [u8; 32] = nonce;
        let frame3 = wire::frame3(&e_h, &n_h);
        let th = match keys::transcript_hash(&self.frame1, &frame2_bytes, &frame3) {
            Ok(th) => th,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let dh = match ephemeral.agree(&spki) {
            Ok(dh) => dh,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let bind_keys = match keys::bind_keys(&self.uw, &self.ticket, &th, &dh) {
            Ok(keys) => keys,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        if let Err(failure) = self.write_frame3(&frame3) {
            return Step::Failed(failure);
        }
        let policy = Policy {
            wifi: true,
            ..Policy::default()
        };
        let mut hop_out = match Sealer::new(Layer::Hop, &bind_keys.h2g, policy) {
            Ok(sealer) => sealer,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let hop_in = match HopReader::new(&bind_keys.g2h, policy) {
            Ok(reader) => reader,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        let bind = Bwf1::Bind(wire::Bind {
            role: wire::role::HOST,
            ticket: self.ticket,
        });
        let Ok(plain) = bind.encode() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let sealed = match hop_out.seal(wire::HOP_TYPE, 0, &plain) {
            Ok(sealed) => sealed,
            Err(_) => return Step::Failed(BindFailure::Crypto),
        };
        if let Err(failure) = self.write_bytes(&sealed) {
            return Step::Failed(failure);
        }
        self.hop_out = Some(hop_out);
        self.hop_in = Some(hop_in);
        self.stage = Stage::Bind;
        self.poll_bind(now_ms)
    }

    fn poll_bind(&mut self, now_ms: i64) -> Step {
        if now_ms - self.accepted_ms >= BIND_READY_DEADLINE_MS {
            return Step::Failed(BindFailure::BindRead);
        }
        let record = match self.read_record() {
            ReadRecord::Pending => match self.read_more(4096) {
                Ok(()) => match self.read_record() {
                    ReadRecord::Record(record) => record,
                    ReadRecord::Pending => return Step::Pending,
                    ReadRecord::Closed => return Step::Failed(BindFailure::BindRead),
                    ReadRecord::Malformed => return Step::Failed(BindFailure::BindRead),
                },
                Err(BindFailure::Closed) => return Step::Failed(BindFailure::BindRead),
                Err(BindFailure::Stalled) => return Step::Failed(BindFailure::BindRead),
                Err(_) => return Step::Failed(BindFailure::BindRead),
            },
            ReadRecord::Record(record) => record,
            ReadRecord::Closed | ReadRecord::Malformed => return Step::Failed(BindFailure::BindRead),
        };
        if record.header.kind != wire::HOP_TYPE {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let bind = match Bwf1::decode(&record.plaintext) {
            Ok(Bwf1::Bind(bind)) => bind,
            _ => return Step::Failed(BindFailure::BindMismatch),
        };
        if bind.role != wire::role::GUEST || bind.ticket.ct_eq(&self.ticket).unwrap_u8() == 0 {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let ready = Bwf1::Ready(wire::Ready {
            recv_ceiling_kbps: READY_RECV_CEILING_KBPS,
            family_cap_kbps: READY_FAMILY_CAP_KBPS,
            transport_budget_kbps: READY_TRANSPORT_BUDGET_KBPS,
        });
        let Some(hop_out) = self.hop_out.as_mut() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let Ok(plain) = ready.encode() else {
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
        if now_ms - self.accepted_ms >= BIND_READY_DEADLINE_MS {
            return Step::Failed(BindFailure::ReadyTimeout);
        }
        let record = match self.read_record() {
            ReadRecord::Pending => match self.read_more(4096) {
                Ok(()) => match self.read_record() {
                    ReadRecord::Record(record) => record,
                    ReadRecord::Pending => return Step::Pending,
                    ReadRecord::Closed => return Step::Failed(BindFailure::Closed),
                    ReadRecord::Malformed => return Step::Failed(BindFailure::Stalled),
                },
                Err(BindFailure::Closed) => return Step::Failed(BindFailure::Closed),
                Err(failure) => return Step::Failed(failure),
            },
            ReadRecord::Record(record) => record,
            ReadRecord::Closed => return Step::Failed(BindFailure::Closed),
            ReadRecord::Malformed => return Step::Failed(BindFailure::Stalled),
        };
        if record.header.kind != wire::HOP_TYPE {
            return Step::Failed(BindFailure::BindMismatch);
        }
        let peer_ready = match Bwf1::decode(&record.plaintext) {
            Ok(Bwf1::Ready(ready)) => ready,
            _ => return Step::Failed(BindFailure::BindMismatch),
        };
        let Some(hop_out) = self.hop_out.take() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let Some(hop_in) = self.hop_in.take() else {
            return Step::Failed(BindFailure::Crypto);
        };
        let Some(socket) = self.socket.take() else {
            return Step::Failed(BindFailure::Closed);
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

    fn read_record(&mut self) -> ReadRecord {
        let Some(hop_in) = self.hop_in.as_mut() else {
            return ReadRecord::Malformed;
        };
        let mut input = self.buffer.as_slice();
        let result = hop_in.read(&mut input, 4096);
        let consumed = self.buffer.len() - input.len();
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        match result {
            Ok(Some(record)) => ReadRecord::Record(record),
            Ok(None) => ReadRecord::Pending,
            Err(_) => ReadRecord::Malformed,
        }
    }

    fn write_frame3(&mut self, frame3: &[u8; wire::BWU1_FRAME3_BYTES]) -> Result<(), BindFailure> {
        self.write_bytes(frame3)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), BindFailure> {
        let Some(socket) = self.socket.as_mut() else {
            return Err(BindFailure::Closed);
        };
        socket
            .write(bytes, Instant::now() + Duration::from_millis(100))
            .map_err(map_socket)
    }
}

enum ReadRecord {
    Pending,
    Record(crate::records::Record),
    Closed,
    Malformed,
}

fn map_socket(error: crate::bluetooth::carrier::Error) -> BindFailure {
    match error {
        crate::bluetooth::carrier::Error::Timeout => BindFailure::Stalled,
        crate::bluetooth::carrier::Error::Closed => BindFailure::Closed,
        _ => BindFailure::Stalled,
    }
}

#[cfg(test)]
pub(crate) mod test_guest {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    pub(crate) struct Guest {
        pub stream: TcpStream,
        pub uw: [u8; 32],
        pub ticket: [u8; 8],
        pub challenge: [u8; 16],
        pub frame1: [u8; 21],
        pub e_g: [u8; 65],
        pub n_g: [u8; 32],
        ephemeral: Option<crypto::Ephemeral>,
        sealer: Option<Sealer>,
        reader: Option<HopReader>,
    }
    impl Guest {
        pub(crate) fn new(mut stream: TcpStream, uw: &[u8; 32], ticket: [u8; 8]) -> Self {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut frame1 = [0u8; wire::BWU1_FRAME1_BYTES];
            stream.read_exact(&mut frame1).expect("host frame 1");
            let challenge = wire::parse_frame1(&frame1).expect("frame 1");
            let ephemeral = crypto::Ephemeral::generate().expect("ephemeral");
            let e_g = keys::point_of(&ephemeral.public_key().expect("public key"));
            let n_g = crypto::random_nonce().expect("nonce");
            Self {
                stream,
                uw: *uw,
                ticket,
                challenge,
                frame1,
                e_g,
                n_g,
                ephemeral: Some(ephemeral),
                sealer: None,
                reader: None,
            }
        }

        pub(crate) fn frame2_bytes(&self, ticket_field: [u8; 8], pre_ticket: [u8; 8], point: [u8; 65]) -> [u8; wire::BWU1_FRAME2_BYTES] {
            let pre = keys::pre(&self.uw, &pre_ticket, &self.challenge, &point, &self.n_g).expect("pre");
            wire::frame2(&ticket_field, &point, &self.n_g, &pre)
        }

        pub(crate) fn send_frame2(&mut self) {
            let bytes = self.frame2_bytes(self.ticket, self.ticket, self.e_g);
            self.stream.write_all(&bytes).expect("frame 2");
        }

        pub(crate) fn send_frame2_for(&mut self, ticket_field: [u8; 8], pre_ticket: [u8; 8], point: [u8; 65]) {
            let bytes = self.frame2_bytes(ticket_field, pre_ticket, point);
            self.stream.write_all(&bytes).expect("frame 2");
        }

        pub(crate) fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).expect("raw bytes");
        }

        pub(crate) fn read_frame3(&mut self) {
            let mut frame3 = [0u8; wire::BWU1_FRAME3_BYTES];
            self.stream.read_exact(&mut frame3).expect("frame 3");
            let (e_h, n_h) = wire::parse_frame3(&frame3).expect("frame 3 parse");
            let th = keys::transcript_hash(&self.frame1, &self.frame2_bytes(self.ticket, self.ticket, self.e_g), &frame3)
                .expect("transcript");
            let spki = keys::spki_of(&e_h).expect("host point");
            let dh = self
                .ephemeral
                .take()
                .expect("ephemeral")
                .agree(&spki)
                .expect("agreement");
            let keys = keys::bind_keys(&self.uw, &self.ticket, &th, &dh).expect("bind keys");
            let policy = Policy {
                wifi: true,
                ..Policy::default()
            };
            self.sealer = Some(Sealer::new(Layer::Hop, &keys.g2h, policy).expect("guest sealer"));
            self.reader = Some(HopReader::new(&keys.h2g, policy).expect("guest reader"));
            let _ = n_h;
        }

        pub(crate) fn send_bind(&mut self, role: u8, ticket: [u8; 8]) {
            let record = Bwf1::Bind(wire::Bind { role, ticket });
            self.write_record(wire::HOP_TYPE, &record.encode().unwrap());
        }

        pub(crate) fn send_ready(&mut self) {
            let record = Bwf1::Ready(wire::Ready {
                recv_ceiling_kbps: 1100,
                family_cap_kbps: 0,
                transport_budget_kbps: 1100,
            });
            self.write_record(wire::HOP_TYPE, &record.encode().unwrap());
        }

        pub(crate) fn send_ready_and(&mut self, kind: u8, plaintext: &[u8]) -> Vec<u8> {
            let ready = self.seal_record(wire::HOP_TYPE, &Bwf1::Ready(wire::Ready {
                recv_ceiling_kbps: 1100,
                family_cap_kbps: 0,
                transport_budget_kbps: 1100,
            }).encode().unwrap());
            let extra = self.seal_record(kind, plaintext);
            let mut bytes = ready;
            bytes.extend_from_slice(&extra);
            self.stream.write_all(&bytes).expect("ready with extra");
            extra
        }

        pub(crate) fn write_record(&mut self, kind: u8, plaintext: &[u8]) -> Vec<u8> {
            let sealed = self.seal_record(kind, plaintext);
            self.stream.write_all(&sealed).expect("write record");
            sealed
        }

        pub(crate) fn seal_record(&mut self, kind: u8, plaintext: &[u8]) -> Vec<u8> {
            let Some(sealer) = self.sealer.as_mut() else {
                panic!("no guest sealer");
            };
            sealer.seal(kind, 0, plaintext).expect("seal")
        }

        pub(crate) fn read_record(&mut self) -> crate::records::Record {
            let mut header = [0u8; 17];
            self.stream.read_exact(&mut header).expect("record header");
            let size = u32::from_be_bytes(header[13..17].try_into().expect("size")) as usize;
            let mut body = vec![0u8; size];
            self.stream.read_exact(&mut body).expect("record body");
            let mut encoded = header.to_vec();
            encoded.extend_from_slice(&body);
            let mut input = encoded.as_slice();
            self.reader
                .as_mut()
                .expect("guest reader")
                .read(&mut input, usize::MAX)
                .expect("open record")
                .expect("record")
        }

        pub(crate) fn complete(&mut self) {
            self.send_frame2();
            self.read_frame3();
            self.send_bind(wire::role::GUEST, self.ticket);
            let host_bind = self.read_record();
            assert_eq!(host_bind.header.kind, wire::HOP_TYPE);
            self.send_ready();
            let host_ready = self.read_record();
            assert_eq!(host_ready.header.kind, wire::HOP_TYPE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_guest::Guest;
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    fn pair(uw: &[u8; 32], ticket: [u8; 8]) -> (HostBinding, Guest) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let guest_stream = TcpStream::connect(address).unwrap();
        let (host_stream, _) = listener.accept().unwrap();
        let binding =
            HostBinding::new(WifiSocket::from_stream(host_stream).unwrap(), uw, ticket, 0).unwrap();
        let guest = Guest::new(guest_stream, uw, ticket);
        (binding, guest)
    }

    fn failure(step: Step) -> Option<BindFailure> {
        match step {
            Step::Failed(failure) => Some(failure),
            _ => None,
        }
    }

    fn expect(step: Step, expected: BindFailure) {
        assert_eq!(failure(step), Some(expected), "expected {}", expected.name());
    }

    /// Polls until frame 2 has been verified and the ticket consumed.
    fn drive_consumed(binding: &mut HostBinding, now_ms: i64) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Step::Failed(failure) = binding.poll(now_ms) {
                panic!("binding failed early: {}", failure.name());
            }
            if binding.ticket_consumed() {
                return;
            }
            if Instant::now() > deadline {
                panic!("frame 2 was not verified");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn drive_done(binding: &mut HostBinding, now_ms: i64) -> Box<Bound> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match binding.poll(now_ms) {
                Step::Pending => {
                    if Instant::now() > deadline {
                        panic!("binding did not finish");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Step::Done(bound) => return bound,
                Step::Failed(failure) => panic!("binding failed: {}", failure.name()),
            }
        }
    }

    #[test]
    fn a_guest_with_the_right_secret_binds_and_the_two_sides_can_talk_over_the_new_keys() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        guest.send_bind(wire::role::GUEST, [9; 8]);
        let host_bind = guest.read_record();
        assert_eq!(host_bind.header.kind, wire::HOP_TYPE);
        let _ = binding.poll(0);
        guest.send_ready();
        let host_ready = guest.read_record();
        assert_eq!(host_ready.header.kind, wire::HOP_TYPE);
        let mut bound = drive_done(&mut binding, 0);
        assert_eq!(bound.peer_ready.recv_ceiling_kbps, 1100);
        assert!(binding.ticket_consumed());
        let sealed = bound.hop_out.seal(6, 0, b"hello").unwrap();
        bound
            .socket
            .write(&sealed, Instant::now() + Duration::from_millis(100))
            .unwrap();
        let record = guest.read_record();
        assert_eq!(record.header.kind, 6);
        assert_eq!(record.plaintext, b"hello");
    }

    #[test]
    fn a_wrong_pre_closes_silently_and_never_consumes_the_ticket() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        let mut wrong = [7; 32];
        wrong[0] = 8;
        guest.uw = wrong;
        guest.send_frame2();
        let step = binding.poll(0);
        expect(step, BindFailure::BadPre);
        assert!(!binding.ticket_consumed());
        drop(binding);
        let mut rest = Vec::new();
        guest.stream.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn a_frame_two_recorded_for_another_challenge_does_not_verify() {
        let (_, mut first) = pair(&[7; 32], [9; 8]);
        let old = first.frame2_bytes([9; 8], [9; 8], first.e_g);
        drop(first);
        let (mut binding, mut second) = pair(&[7; 32], [9; 8]);
        second.send_raw(&old);
        expect(binding.poll(0), BindFailure::BadPre);
    }

    #[test]
    fn a_valid_pre_with_another_ticket_fails_without_consuming() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2_for([1, 2, 3, 4, 5, 6, 7, 8], [9; 8], guest.e_g);
        expect(binding.poll(0), BindFailure::WrongTicket);
        assert!(!binding.ticket_consumed());
    }

    #[test]
    fn a_point_off_the_curve_fails_after_pre_verified_and_the_ticket_is_burned() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        let mut point = [0u8; 65];
        point[0] = 4;
        guest.send_frame2_for([9; 8], [9; 8], point);
        expect(binding.poll(0), BindFailure::BadPoint);
        assert!(binding.ticket_consumed());
    }

    #[test]
    fn frame_two_later_than_two_seconds_is_refused() {
        let (mut binding, _guest) = pair(&[7; 32], [9; 8]);
        expect(binding.poll(2_000), BindFailure::Frame2Timeout);
    }

    #[test]
    fn bind_and_ready_must_finish_within_five_seconds_of_accept() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        drop(guest);
        expect(binding.poll(5_000), BindFailure::BindRead);

        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        guest.send_bind(wire::role::GUEST, [9; 8]);
        let _ = guest.read_record();
        let _ = binding.poll(0);
        expect(binding.poll(5_000), BindFailure::ReadyTimeout);
    }

    #[test]
    fn the_host_seals_its_bind_and_ready_as_bwf1_hop_records() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        let host_bind = guest.read_record();
        assert_eq!(host_bind.header.kind, wire::HOP_TYPE);
        guest.send_bind(wire::role::GUEST, [9; 8]);
        guest.send_ready();
        let _bound = drive_done(&mut binding, 0);
        let host_ready = guest.read_record();
        assert_eq!(host_ready.header.kind, wire::HOP_TYPE);
    }

    #[test]
    fn a_bind_sealed_under_another_hop_type_is_refused() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        let bind = Bwf1::Bind(wire::Bind {
            role: wire::role::GUEST,
            ticket: [9; 8],
        });
        guest.write_record(6, &bind.encode().unwrap());
        std::thread::sleep(Duration::from_millis(20));
        expect(binding.poll(0), BindFailure::BindMismatch);
    }

    #[test]
    fn each_bind_failure_names_the_android_stage_token() {
        for (failure, name) in [
            (BindFailure::Frame2Timeout, "frame2"),
            (BindFailure::Frame2Format, "frame2_format"),
            (BindFailure::BadPre, "pre"),
            (BindFailure::WrongTicket, "ticket"),
            (BindFailure::BadPoint, "point"),
            (BindFailure::Crypto, "ecdh"),
            (BindFailure::BindMismatch, "bind_check"),
            (BindFailure::BindRead, "bind_read"),
            (BindFailure::ReadyTimeout, "ready"),
            (BindFailure::Closed, "closed"),
            (BindFailure::Stalled, "stalled"),
        ] {
            assert_eq!(failure.name(), name);
        }
    }

    #[test]
    fn a_bind_with_the_wrong_role_or_ticket_is_refused() {
        for (role, ticket) in [(2, [9; 8]), (1, [1; 8])] {
            let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
            guest.send_frame2();
            drive_consumed(&mut binding, 0);
            guest.read_frame3();
            guest.send_bind(role, ticket);
            expect(binding.poll(0), BindFailure::BindMismatch);
        }
    }

    #[test]
    fn a_guest_that_sends_ready_before_bind_is_refused() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        guest.send_ready();
        expect(binding.poll(0), BindFailure::BindMismatch);
    }

    #[test]
    fn bytes_after_the_guests_ready_are_kept_for_the_session() {
        let (mut binding, mut guest) = pair(&[7; 32], [9; 8]);
        guest.send_frame2();
        drive_consumed(&mut binding, 0);
        guest.read_frame3();
        guest.send_bind(wire::role::GUEST, [9; 8]);
        let _ = guest.read_record();
        let extra = guest.send_ready_and(6, b"after-ready");
        std::thread::sleep(Duration::from_millis(20));
        let bound = drive_done(&mut binding, 0);
        assert_eq!(bound.leftover, extra);
    }
}

