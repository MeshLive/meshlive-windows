//! `BWF1` hop records and `BWU1` binding frames.
//!
//! Plaintext of a `BMS1` hop record of type 16: `"BWF1" (4) | kind (1) | body`,
//! big-endian. Decode checks run in this order and return the first failure:
//! `Length` (total < 5 or > 96), `Magic`, `Kind` (1 to 11), `BodyLength`,
//! `Field`. `Version` exists for the discovery messages Windows does not use.
//! No panic and no allocation sized by input.

use super::limits::{
    HOST_VALID_MS, MAX_BWF1_PLAINTEXT_BYTES, MAX_FREE_PATHS_IN_CAPS, MIN_5GHZ_MHZ,
    MIN_BWF1_PLAINTEXT_BYTES,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Magic,
    Kind,
    BodyLength,
    Field,
    Version,
}
impl Error {
    pub fn name(self) -> &'static str {
        match self {
            Error::Length => "LENGTH",
            Error::Magic => "MAGIC",
            Error::Kind => "KIND",
            Error::BodyLength => "BODY_LENGTH",
            Error::Field => "FIELD",
            Error::Version => "VERSION",
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

pub const MAGIC: &[u8; 4] = b"BWF1";
/// The `BMS1` hop record type that carries every `BWF1` record, `BIND` and `READY` included
/// (PROTOCOL "Reserved elements"; Android `SecureRecordType.BWF1`).
pub const HOP_TYPE: u8 = 16;
pub mod kind {
    pub const CAPS: u8 = 1;
    pub const HOST: u8 = 2;
    pub const DECLINE: u8 = 3;
    pub const PICK: u8 = 4;
    pub const ARMED: u8 = 5;
    pub const BIND: u8 = 6;
    pub const READY: u8 = 7;
    pub const SWITCH: u8 = 8;
    pub const RESUME: u8 = 9;
    pub const CLOSE: u8 = 10;
    pub const CREDS: u8 = 11;
}
pub mod bearer {
    pub const AWARE: u8 = 1;
    pub const DIRECT: u8 = 2;
}
pub mod caps_bit {
    pub const AWARE: u8 = 1;
    pub const AWARE_ICM: u8 = 2;
    pub const DIRECT_GO: u8 = 4;
    pub const DIRECT_CLIENT: u8 = 8;
}
/// DECLINE reasons = A0 `WifiEligibility.DeclineReason(wire)`.
pub mod decline {
    pub const NOT_ELIGIBLE: u8 = 1;
    pub const BUSY: u8 = 2;
    pub const WIFI_UNAVAILABLE: u8 = 3;
    pub const LIMIT_OR_COOLDOWN: u8 = 4;
    pub const BATTERY_OR_THERMAL: u8 = 5;
    pub const SETUP_FAILED: u8 = 6;
    pub const NOT_5GHZ: u8 = 7;
    pub fn name(reason: u8) -> &'static str {
        match reason {
            NOT_ELIGIBLE => "not_eligible",
            BUSY => "busy",
            WIFI_UNAVAILABLE => "wifi_unavailable",
            LIMIT_OR_COOLDOWN => "limit_or_cooldown",
            BATTERY_OR_THERMAL => "battery_or_thermal",
            SETUP_FAILED => "setup_failed",
            NOT_5GHZ => "not_5ghz",
            _ => "unknown",
        }
    }
}
/// CLOSE reason byte: PROTOCOL defines only 1 = hang-up; any byte is accepted on receive.
pub mod close {
    pub const HANG_UP: u8 = 1;
}
/// BIND roles = A0 `WifiWire.ROLE_GUEST` / `WifiWire.ROLE_HOST`.
pub mod role {
    pub const GUEST: u8 = 1;
    pub const HOST: u8 = 2;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Caps {
    pub bearers: u8,
    pub free_host: u8,
    pub free_guest: u8,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub status: u8,
    pub bearer: u8,
    pub ticket: [u8; 8],
    pub valid_ms: u16,
    pub freq_mhz: u16,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Decline {
    pub ticket: [u8; 8],
    pub reason: u8,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub ticket: [u8; 8],
    pub challenge: [u8; 16],
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Armed {
    pub ticket: [u8; 8],
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub role: u8,
    pub ticket: [u8; 8],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ready {
    pub recv_ceiling_kbps: u16,
    pub family_cap_kbps: u16,
    pub transport_budget_kbps: u16,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    pub generation: u32,
    pub ticket: [u8; 8],
    pub e2e_boundary: u64,
    pub mesh_boundary: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    pub generation: u32,
    pub ticket: [u8; 8],
    pub e2e_next: u64,
    pub mesh_next: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Close {
    pub reason: u8,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Creds {
    pub ticket: [u8; 8],
    pub port: u16,
    pub ipv4: [u8; 4],
}

macro_rules! redacted_debug {
    ($name:ident { $($field:ident),* $(,)? } redact: $($secret:ident),* $(,)?) => {
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut out = f.debug_struct(stringify!($name));
                $(out.field(stringify!($field), &self.$field);)*
                $(out.field(stringify!($secret), &"<redacted>");)*
                out.finish()
            }
        }
    };
}
redacted_debug!(Host { status, bearer, valid_ms, freq_mhz } redact: ticket);
redacted_debug!(Decline { reason } redact: ticket);
redacted_debug!(Pick {} redact: ticket, challenge);
redacted_debug!(Armed {} redact: ticket);
redacted_debug!(Bind { role } redact: ticket);
redacted_debug!(Switch { generation, e2e_boundary, mesh_boundary } redact: ticket);
redacted_debug!(Resume { generation, e2e_next, mesh_next } redact: ticket);
redacted_debug!(Creds {} redact: ticket, ipv4, port);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Bwf1 {
    Caps(Caps),
    Host(Host),
    Decline(Decline),
    Pick(Pick),
    Armed(Armed),
    Bind(Bind),
    Ready(Ready),
    Switch(Switch),
    Resume(Resume),
    Close(Close),
    Creds(Creds),
}
impl std::fmt::Debug for Bwf1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bwf1::Caps(v) => f.debug_tuple("Caps").field(v).finish(),
            Bwf1::Host(v) => f.debug_tuple("Host").field(v).finish(),
            Bwf1::Decline(v) => f.debug_tuple("Decline").field(v).finish(),
            Bwf1::Pick(v) => f.debug_tuple("Pick").field(v).finish(),
            Bwf1::Armed(v) => f.debug_tuple("Armed").field(v).finish(),
            Bwf1::Bind(v) => f.debug_tuple("Bind").field(v).finish(),
            Bwf1::Ready(v) => f.debug_tuple("Ready").field(v).finish(),
            Bwf1::Switch(v) => f.debug_tuple("Switch").field(v).finish(),
            Bwf1::Resume(v) => f.debug_tuple("Resume").field(v).finish(),
            Bwf1::Close(v) => f.debug_tuple("Close").field(v).finish(),
            Bwf1::Creds(v) => f.debug_tuple("Creds").field(v).finish(),
        }
    }
}

fn body_len(kind: u8) -> Option<usize> {
    Some(match kind {
        kind::CAPS => 4,
        kind::HOST => 16,
        kind::DECLINE => 9,
        kind::PICK => 24,
        kind::ARMED => 8,
        kind::BIND => 9,
        kind::READY => 8,
        kind::SWITCH | kind::RESUME => 28,
        kind::CLOSE => 1,
        kind::CREDS => 14,
        _ => return None,
    })
}
fn array<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&src[..N]);
    out
}
fn u16_at(src: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(array(&src[at..at + 2]))
}
fn u32_at(src: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(array(&src[at..at + 4]))
}
fn u64_at(src: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(array(&src[at..at + 8]))
}

impl Bwf1 {
    pub fn kind(&self) -> u8 {
        match self {
            Bwf1::Caps(_) => kind::CAPS,
            Bwf1::Host(_) => kind::HOST,
            Bwf1::Decline(_) => kind::DECLINE,
            Bwf1::Pick(_) => kind::PICK,
            Bwf1::Armed(_) => kind::ARMED,
            Bwf1::Bind(_) => kind::BIND,
            Bwf1::Ready(_) => kind::READY,
            Bwf1::Switch(_) => kind::SWITCH,
            Bwf1::Resume(_) => kind::RESUME,
            Bwf1::Close(_) => kind::CLOSE,
            Bwf1::Creds(_) => kind::CREDS,
        }
    }

    /// Whether every field keeps the rules `decode` enforces (`Err(Field)`
    /// otherwise). Checked in every build, so a release build never writes a
    /// record the peer refuses.
    pub fn validate(&self) -> Result<(), Error> {
        let valid = match self {
            Bwf1::Caps(v) => {
                v.bearers <= 0x0f
                    && v.free_host <= MAX_FREE_PATHS_IN_CAPS
                    && v.free_guest <= MAX_FREE_PATHS_IN_CAPS
            }
            Bwf1::Host(v) => {
                let status = match v.status {
                    1 => (1..=HOST_VALID_MS).contains(&v.valid_ms),
                    0 => v.valid_ms == 0 && v.freq_mhz == 0,
                    _ => false,
                };
                let frequency = match v.bearer {
                    bearer::AWARE => v.freq_mhz == 0,
                    bearer::DIRECT => v.freq_mhz == 0 || v.freq_mhz >= MIN_5GHZ_MHZ,
                    _ => false,
                };
                status && frequency
            }
            Bwf1::Decline(v) => (1..=7).contains(&v.reason),
            Bwf1::Bind(v) => v.role == role::GUEST || v.role == role::HOST,
            Bwf1::Switch(v) => {
                v.generation != 0
                    && v.e2e_boundary <= i64::MAX as u64
                    && v.mesh_boundary <= i64::MAX as u64
            }
            Bwf1::Resume(v) => {
                v.generation != 0 && v.e2e_next <= i64::MAX as u64 && v.mesh_next <= i64::MAX as u64
            }
            Bwf1::Creds(v) => v.port != 0 && v.ipv4 != [0; 4],
            Bwf1::Pick(_) | Bwf1::Armed(_) | Bwf1::Ready(_) | Bwf1::Close(_) => true,
        };
        if valid { Ok(()) } else { Err(Error::Field) }
    }

    /// The whole plaintext `"BWF1" kind body`; `Err(Field)` for a value that
    /// breaks a field rule (`validate`), in release builds too.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut body = Vec::new();
        match self {
            Bwf1::Caps(v) => {
                body.extend_from_slice(&[v.bearers, v.free_host, v.free_guest, 0]);
            }
            Bwf1::Host(v) => {
                body.extend_from_slice(&[v.status, v.bearer]);
                body.extend_from_slice(&v.ticket);
                body.extend_from_slice(&v.valid_ms.to_be_bytes());
                body.extend_from_slice(&v.freq_mhz.to_be_bytes());
                body.extend_from_slice(&[0, 0]);
            }
            Bwf1::Decline(v) => {
                body.extend_from_slice(&v.ticket);
                body.push(v.reason);
            }
            Bwf1::Pick(v) => {
                body.extend_from_slice(&v.ticket);
                body.extend_from_slice(&v.challenge);
            }
            Bwf1::Armed(v) => body.extend_from_slice(&v.ticket),
            Bwf1::Bind(v) => {
                body.push(v.role);
                body.extend_from_slice(&v.ticket);
            }
            Bwf1::Ready(v) => {
                body.extend_from_slice(&v.recv_ceiling_kbps.to_be_bytes());
                body.extend_from_slice(&v.family_cap_kbps.to_be_bytes());
                body.extend_from_slice(&v.transport_budget_kbps.to_be_bytes());
                body.extend_from_slice(&[0, 0]);
            }
            Bwf1::Switch(v) => {
                body.extend_from_slice(&v.generation.to_be_bytes());
                body.extend_from_slice(&v.ticket);
                body.extend_from_slice(&v.e2e_boundary.to_be_bytes());
                body.extend_from_slice(&v.mesh_boundary.to_be_bytes());
            }
            Bwf1::Resume(v) => {
                body.extend_from_slice(&v.generation.to_be_bytes());
                body.extend_from_slice(&v.ticket);
                body.extend_from_slice(&v.e2e_next.to_be_bytes());
                body.extend_from_slice(&v.mesh_next.to_be_bytes());
            }
            Bwf1::Close(v) => body.push(v.reason),
            Bwf1::Creds(v) => {
                body.extend_from_slice(&v.ticket);
                body.extend_from_slice(&v.port.to_be_bytes());
                body.extend_from_slice(&v.ipv4);
            }
        }
        let mut out = Vec::with_capacity(5 + body.len());
        out.extend_from_slice(MAGIC);
        out.push(self.kind());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(plaintext: &[u8]) -> Result<Self, Error> {
        if plaintext.len() < MIN_BWF1_PLAINTEXT_BYTES
            || plaintext.len() > MAX_BWF1_PLAINTEXT_BYTES
        {
            return Err(Error::Length);
        }
        if &plaintext[..4] != MAGIC {
            return Err(Error::Magic);
        }
        let kind = plaintext[4];
        let expected = body_len(kind).ok_or(Error::Kind)?;
        let body = &plaintext[5..];
        if body.len() != expected {
            return Err(Error::BodyLength);
        }
        match kind {
            kind::CAPS => {
                // Forward compatibility: bits 4-7 and the reserved byte are ignored.
                let caps = Caps {
                    bearers: body[0] & 0x0f,
                    free_host: body[1],
                    free_guest: body[2],
                };
                if caps.free_host > MAX_FREE_PATHS_IN_CAPS
                    || caps.free_guest > MAX_FREE_PATHS_IN_CAPS
                {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Caps(caps))
            }
            kind::HOST => {
                let host = Host {
                    status: body[0],
                    bearer: body[1],
                    ticket: array(&body[2..10]),
                    valid_ms: u16_at(body, 10),
                    freq_mhz: u16_at(body, 12),
                };
                if host.status > 1 || (host.bearer != bearer::AWARE && host.bearer != bearer::DIRECT)
                {
                    return Err(Error::Field);
                }
                if host.status == 1 && !(1..=HOST_VALID_MS).contains(&host.valid_ms) {
                    return Err(Error::Field);
                }
                if host.status == 0 && (host.valid_ms != 0 || host.freq_mhz != 0) {
                    return Err(Error::Field);
                }
                if host.bearer == bearer::AWARE && host.freq_mhz != 0 {
                    return Err(Error::Field);
                }
                if host.bearer == bearer::DIRECT
                    && host.freq_mhz != 0
                    && host.freq_mhz < MIN_5GHZ_MHZ
                {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Host(host))
            }
            kind::DECLINE => {
                let decline = Decline {
                    ticket: array(&body[..8]),
                    reason: body[8],
                };
                if !(1..=7).contains(&decline.reason) {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Decline(decline))
            }
            kind::PICK => Ok(Bwf1::Pick(Pick {
                ticket: array(&body[..8]),
                challenge: array(&body[8..24]),
            })),
            kind::ARMED => Ok(Bwf1::Armed(Armed {
                ticket: array(&body[..8]),
            })),
            kind::BIND => {
                let bind = Bind {
                    role: body[0],
                    ticket: array(&body[1..9]),
                };
                if bind.role != role::GUEST && bind.role != role::HOST {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Bind(bind))
            }
            kind::READY => Ok(Bwf1::Ready(Ready {
                recv_ceiling_kbps: u16_at(body, 0),
                family_cap_kbps: u16_at(body, 2),
                transport_budget_kbps: u16_at(body, 4),
            })),
            kind::SWITCH => {
                let switch = Switch {
                    generation: u32_at(body, 0),
                    ticket: array(&body[4..12]),
                    e2e_boundary: u64_at(body, 12),
                    mesh_boundary: u64_at(body, 20),
                };
                if switch.generation == 0
                    || switch.e2e_boundary > i64::MAX as u64
                    || switch.mesh_boundary > i64::MAX as u64
                {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Switch(switch))
            }
            kind::RESUME => {
                let resume = Resume {
                    generation: u32_at(body, 0),
                    ticket: array(&body[4..12]),
                    e2e_next: u64_at(body, 12),
                    mesh_next: u64_at(body, 20),
                };
                if resume.generation == 0
                    || resume.e2e_next > i64::MAX as u64
                    || resume.mesh_next > i64::MAX as u64
                {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Resume(resume))
            }
            kind::CLOSE => Ok(Bwf1::Close(Close { reason: body[0] })),
            kind::CREDS => {
                let creds = Creds {
                    ticket: array(&body[..8]),
                    port: u16_at(body, 8),
                    ipv4: array(&body[10..14]),
                };
                if creds.port == 0 || creds.ipv4 == [0; 4] {
                    return Err(Error::Field);
                }
                Ok(Bwf1::Creds(creds))
            }
            _ => Err(Error::Kind),
        }
    }
}

pub const BWU1: &[u8; 4] = b"BWU1";
pub const BWU1_FRAME1_BYTES: usize = 21;
pub const BWU1_FRAME2_BYTES: usize = 126;
pub const BWU1_FRAME3_BYTES: usize = 102;

pub fn frame1(challenge: &[u8; 16]) -> [u8; 21] {
    let mut out = [0; BWU1_FRAME1_BYTES];
    out[..4].copy_from_slice(BWU1);
    out[4] = 1;
    out[5..].copy_from_slice(challenge);
    out
}
pub fn frame2(ticket: &[u8; 8], e_g: &[u8; 65], n_g: &[u8; 32], pre: &[u8; 16]) -> [u8; 126] {
    let mut out = [0; BWU1_FRAME2_BYTES];
    out[..4].copy_from_slice(BWU1);
    out[4] = 2;
    out[5..13].copy_from_slice(ticket);
    out[13..78].copy_from_slice(e_g);
    out[78..110].copy_from_slice(n_g);
    out[110..126].copy_from_slice(pre);
    out
}
pub fn frame3(e_h: &[u8; 65], n_h: &[u8; 32]) -> [u8; 102] {
    let mut out = [0; BWU1_FRAME3_BYTES];
    out[..4].copy_from_slice(BWU1);
    out[4] = 3;
    out[5..70].copy_from_slice(e_h);
    out[70..102].copy_from_slice(n_h);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Frame2 {
    pub ticket: [u8; 8],
    pub e_g: [u8; 65],
    pub n_g: [u8; 32],
    pub pre: [u8; 16],
}
impl std::fmt::Debug for Frame2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame2")
            .field("ticket", &"<redacted>")
            .field("e_g", &"<redacted>")
            .field("n_g", &"<redacted>")
            .field("pre", &"<redacted>")
            .finish()
    }
}

fn check_frame(bytes: &[u8], size: usize, frame_type: u8) -> Result<(), Error> {
    if bytes.len() != size {
        return Err(Error::Length);
    }
    if &bytes[..4] != BWU1 {
        return Err(Error::Magic);
    }
    if bytes[4] != frame_type {
        return Err(Error::Kind);
    }
    Ok(())
}

pub fn parse_frame1(bytes: &[u8]) -> Result<[u8; 16], Error> {
    check_frame(bytes, BWU1_FRAME1_BYTES, 1)?;
    Ok(array(&bytes[5..21]))
}
pub fn parse_frame2(bytes: &[u8]) -> Result<Frame2, Error> {
    check_frame(bytes, BWU1_FRAME2_BYTES, 2)?;
    let e_g = array::<65>(&bytes[13..78]);
    if e_g[0] != 0x04 {
        return Err(Error::Field);
    }
    Ok(Frame2 {
        ticket: array(&bytes[5..13]),
        e_g,
        n_g: array(&bytes[78..110]),
        pre: array(&bytes[110..126]),
    })
}
pub fn parse_frame3(bytes: &[u8]) -> Result<([u8; 65], [u8; 32]), Error> {
    check_frame(bytes, BWU1_FRAME3_BYTES, 3)?;
    let e_h = array::<65>(&bytes[5..70]);
    if e_h[0] != 0x04 {
        return Err(Error::Field);
    }
    Ok((e_h, array(&bytes[70..102])))
}

/// The hop-record length window checked before any allocation:
/// `(MIN_BWF1_PLAINTEXT_BYTES + 16)..=(MAX_BWF1_PLAINTEXT_BYTES + 16)` = 21..=112.
pub fn accepts_bwf1_ciphertext_length(len: usize) -> bool {
    (MIN_BWF1_PLAINTEXT_BYTES + 16..=MAX_BWF1_PLAINTEXT_BYTES + 16).contains(&len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> [u8; 8] {
        [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18]
    }
    fn plaintext(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(kind);
        out.extend_from_slice(body);
        out
    }
    fn host_body(status: u8, bearer: u8, valid_ms: u16, freq_mhz: u16, reserved: u16) -> Vec<u8> {
        let mut out = vec![status, bearer];
        out.extend_from_slice(&ticket());
        out.extend_from_slice(&valid_ms.to_be_bytes());
        out.extend_from_slice(&freq_mhz.to_be_bytes());
        out.extend_from_slice(&reserved.to_be_bytes());
        out
    }

    #[test]
    fn every_wifi_record_round_trips_through_encode_and_decode() {
        let values = [
            Bwf1::Caps(Caps {
                bearers: caps_bit::AWARE | caps_bit::AWARE_ICM,
                free_host: 1,
                free_guest: 2,
            }),
            Bwf1::Host(Host {
                status: 1,
                bearer: bearer::DIRECT,
                ticket: ticket(),
                valid_ms: 20_000,
                freq_mhz: 0,
            }),
            Bwf1::Decline(Decline {
                ticket: ticket(),
                reason: decline::BUSY,
            }),
            Bwf1::Pick(Pick {
                ticket: ticket(),
                challenge: [3; 16],
            }),
            Bwf1::Armed(Armed { ticket: ticket() }),
            Bwf1::Bind(Bind {
                role: role::HOST,
                ticket: ticket(),
            }),
            Bwf1::Ready(Ready {
                recv_ceiling_kbps: 1100,
                family_cap_kbps: 0,
                transport_budget_kbps: 1100,
            }),
            Bwf1::Switch(Switch {
                generation: 1,
                ticket: ticket(),
                e2e_boundary: 1234,
                mesh_boundary: 0,
            }),
            Bwf1::Resume(Resume {
                generation: 2,
                ticket: ticket(),
                e2e_next: 1235,
                mesh_next: 0,
            }),
            Bwf1::Close(Close {
                reason: close::HANG_UP,
            }),
            Bwf1::Creds(Creds {
                ticket: ticket(),
                port: 40000,
                ipv4: [192, 168, 137, 1],
            }),
        ];
        for value in values {
            let encoded = value.encode().unwrap();
            assert_eq!(&encoded[..4], MAGIC);
            assert_eq!(encoded[4], value.kind());
            assert_eq!(Bwf1::decode(&encoded), Ok(value));
        }
    }

    #[test]
    fn the_decoder_reports_the_first_failing_check_in_the_documented_order() {
        assert_eq!(Bwf1::decode(&[0; 97]), Err(Error::Length));
        assert_eq!(Bwf1::decode(&[0; 4]), Err(Error::Length));
        let mut wrong_magic = plaintext(kind::CAPS, &[0; 4]);
        wrong_magic[0] = b'X';
        assert_eq!(Bwf1::decode(&wrong_magic), Err(Error::Magic));
        assert_eq!(
            Bwf1::decode(&plaintext(0, &[0; 4])),
            Err(Error::Kind)
        );
        assert_eq!(
            Bwf1::decode(&plaintext(12, &[0; 28])),
            Err(Error::Kind)
        );
        assert_eq!(
            Bwf1::decode(&plaintext(kind::CAPS, &[0; 3])),
            Err(Error::BodyLength)
        );
        assert_eq!(
            Bwf1::decode(&plaintext(kind::CAPS, &[0; 5])),
            Err(Error::BodyLength)
        );
        assert_eq!(
            Bwf1::decode(&plaintext(kind::CAPS, &[0, 8, 0, 0])),
            Err(Error::Field)
        );
    }

    #[test]
    fn an_out_of_range_field_is_refused() {
        let mut cases: Vec<Vec<u8>> = vec![
            plaintext(kind::CAPS, &[0, 8, 0, 0]),
            plaintext(kind::CAPS, &[0, 0, 8, 0]),
            plaintext(kind::HOST, &host_body(1, 1, 20_001, 0, 0)),
            plaintext(kind::HOST, &host_body(1, 1, 0, 0, 0)),
            plaintext(kind::HOST, &host_body(0, 1, 5, 0, 0)),
            plaintext(kind::HOST, &host_body(1, 1, 100, 5745, 0)),
            plaintext(kind::HOST, &host_body(1, 2, 100, 2437, 0)),
            plaintext(kind::BIND, &{
                let mut body = vec![0];
                body.extend_from_slice(&ticket());
                body
            }),
            plaintext(kind::BIND, &{
                let mut body = vec![3];
                body.extend_from_slice(&ticket());
                body
            }),
            plaintext(kind::DECLINE, &{
                let mut body = ticket().to_vec();
                body.push(0);
                body
            }),
            plaintext(kind::DECLINE, &{
                let mut body = ticket().to_vec();
                body.push(8);
                body
            }),
            plaintext(kind::SWITCH, &{
                let mut body = 0u32.to_be_bytes().to_vec();
                body.extend_from_slice(&ticket());
                body.extend_from_slice(&0u64.to_be_bytes());
                body.extend_from_slice(&0u64.to_be_bytes());
                body
            }),
            plaintext(kind::CREDS, &{
                let mut body = ticket().to_vec();
                body.extend_from_slice(&0u16.to_be_bytes());
                body.extend_from_slice(&[192, 168, 137, 1]);
                body
            }),
            plaintext(kind::CREDS, &{
                let mut body = ticket().to_vec();
                body.extend_from_slice(&40000u16.to_be_bytes());
                body.extend_from_slice(&[0, 0, 0, 0]);
                body
            }),
        ];
        cases.push(plaintext(kind::CAPS, &[0xf1, 0, 8, 0]));
        for (index, case) in cases.iter().enumerate() {
            assert!(
                Bwf1::decode(case) == Err(Error::Field),
                "case {index} was not refused"
            );
        }
    }

    #[test]
    fn the_encoder_refuses_a_value_the_decoder_would_refuse_in_every_build() {
        let invalid = [
            Bwf1::Caps(Caps {
                bearers: 0x10,
                free_host: 0,
                free_guest: 0,
            }),
            Bwf1::Caps(Caps {
                bearers: 1,
                free_host: MAX_FREE_PATHS_IN_CAPS + 1,
                free_guest: 0,
            }),
            Bwf1::Host(Host {
                status: 2,
                bearer: bearer::DIRECT,
                ticket: ticket(),
                valid_ms: 100,
                freq_mhz: 0,
            }),
            Bwf1::Host(Host {
                status: 1,
                bearer: bearer::DIRECT,
                ticket: ticket(),
                valid_ms: 100,
                freq_mhz: 2437,
            }),
            Bwf1::Decline(Decline {
                ticket: ticket(),
                reason: 0,
            }),
            Bwf1::Bind(Bind {
                role: 3,
                ticket: ticket(),
            }),
            Bwf1::Switch(Switch {
                generation: 0,
                ticket: ticket(),
                e2e_boundary: 1,
                mesh_boundary: 0,
            }),
            Bwf1::Resume(Resume {
                generation: 1,
                ticket: ticket(),
                e2e_next: u64::MAX,
                mesh_next: 0,
            }),
            Bwf1::Creds(Creds {
                ticket: ticket(),
                port: 0,
                ipv4: [192, 168, 137, 1],
            }),
        ];
        for value in invalid {
            assert_eq!(value.encode(), Err(Error::Field), "{value:?}");
        }
    }

    #[test]
    fn ignored_bits_and_reserved_fields_are_dropped() {
        assert_eq!(
            Bwf1::decode(b"BWF1\x01\xf1\x00\x00\x09"),
            Ok(Bwf1::Caps(Caps {
                bearers: 1,
                free_host: 0,
                free_guest: 0,
            }))
        );
        let host = Bwf1::Host(Host {
            status: 1,
            bearer: bearer::DIRECT,
            ticket: ticket(),
            valid_ms: 100,
            freq_mhz: 0,
        });
        let mut bytes = host.encode().unwrap();
        let len = bytes.len();
        bytes[len - 2..].copy_from_slice(&[0, 1]);
        assert_eq!(Bwf1::decode(&bytes), Ok(host));
        let ready = Bwf1::Ready(Ready {
            recv_ceiling_kbps: 1,
            family_cap_kbps: 1,
            transport_budget_kbps: 1,
        });
        let mut bytes = ready.encode().unwrap();
        let len = bytes.len();
        bytes[len - 2..].copy_from_slice(&[0, 1]);
        assert_eq!(Bwf1::decode(&bytes), Ok(ready));
    }

    #[test]
    fn windows_host_offer_is_valid() {
        let host = Bwf1::Host(Host {
            status: 1,
            bearer: bearer::DIRECT,
            ticket: ticket(),
            valid_ms: 20_000,
            freq_mhz: 0,
        });
        assert_eq!(Bwf1::decode(&host.encode().unwrap()), Ok(host));
        let creds = Bwf1::Creds(Creds {
            ticket: ticket(),
            port: 40000,
            ipv4: [192, 168, 137, 1],
        });
        assert_eq!(Bwf1::decode(&creds.encode().unwrap()), Ok(creds));
    }

    #[test]
    fn the_bwu1_frames_have_the_fixed_sizes_and_parse_back() {
        let challenge = [7; 16];
        let f1 = frame1(&challenge);
        assert_eq!(f1.len(), 21);
        assert!(parse_frame1(&f1) == Ok(challenge), "mismatch");

        let e_g = {
            let mut point = [9; 65];
            point[0] = 0x04;
            point
        };
        let n_g = [8; 32];
        let pre = [6; 16];
        let f2 = frame2(&ticket(), &e_g, &n_g, &pre);
        assert_eq!(f2.len(), 126);
        assert_eq!(
            parse_frame2(&f2),
            Ok(Frame2 {
                ticket: ticket(),
                e_g,
                n_g,
                pre,
            })
        );
        let e_h = {
            let mut point = [5; 65];
            point[0] = 0x04;
            point
        };
        let n_h = [4; 32];
        let f3 = frame3(&e_h, &n_h);
        assert_eq!(f3.len(), 102);
        assert_eq!(parse_frame3(&f3), Ok((e_h, n_h)));

        assert_eq!(parse_frame1(&f2), Err(Error::Length));
        let mut wrong_magic = f1;
        wrong_magic[0] = b'X';
        assert_eq!(parse_frame1(&wrong_magic), Err(Error::Magic));
        let mut wrong_type = f1;
        wrong_type[4] = 2;
        assert_eq!(parse_frame1(&wrong_type), Err(Error::Kind));
        let mut off_curve = [0u8; 65];
        off_curve[0] = 0x05;
        let f2_off = frame2(&ticket(), &off_curve, &n_g, &pre);
        assert_eq!(parse_frame2(&f2_off), Err(Error::Field));
    }

    #[test]
    fn the_bwf1_ciphertext_length_window_is_21_to_112() {
        assert!(!accepts_bwf1_ciphertext_length(20));
        assert!(accepts_bwf1_ciphertext_length(21));
        assert!(accepts_bwf1_ciphertext_length(112));
        assert!(!accepts_bwf1_ciphertext_length(113));
        assert_eq!(decline::name(7), "not_5ghz");
        assert_eq!(decline::name(3), "wifi_unavailable");
        assert_eq!(decline::name(0), "unknown");
    }

    #[test]
    fn a_debug_record_never_prints_its_ticket() {
        let ticket = [0x9e, 0x8d, 0x7c, 0x6b, 0x5a, 0x49, 0x38, 0x27];
        let records = [
            Bwf1::Host(Host {
                status: 1,
                bearer: bearer::DIRECT,
                ticket,
                valid_ms: 20_000,
                freq_mhz: 0,
            }),
            Bwf1::Decline(Decline {
                ticket,
                reason: decline::BUSY,
            }),
            Bwf1::Pick(Pick {
                ticket,
                challenge: [3; 16],
            }),
            Bwf1::Armed(Armed { ticket }),
            Bwf1::Bind(Bind {
                role: role::HOST,
                ticket,
            }),
            Bwf1::Switch(Switch {
                generation: 1,
                ticket,
                e2e_boundary: 1234,
                mesh_boundary: 0,
            }),
            Bwf1::Resume(Resume {
                generation: 2,
                ticket,
                e2e_next: 1235,
                mesh_next: 0,
            }),
            Bwf1::Creds(Creds {
                ticket,
                port: 40000,
                ipv4: [192, 168, 137, 1],
            }),
        ];
        for record in records {
            let text = format!("{record:?}");
            for byte in ticket {
                assert!(!text.contains(&format!("{byte:02x}")), "{text}");
                assert!(!text.contains(&format!("{byte}")), "{text}");
            }
            assert!(!text.contains("192"), "{text}");
            assert!(!text.contains("137"), "{text}");
        }
    }
}
