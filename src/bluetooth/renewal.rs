//! Design SR (in-call link renewal) on Windows: the transport seam, the
//! per-attempt server bookkeeping and the owner that performs the state
//! machine's actions (`call_renewal::Action`). The wire rules are PROTOCOL
//! "Experimental in-call link renewal"; nothing here adds a byte.
//!
//! Nothing is logged that identifies the call: no key, ticket, UUID or
//! address (the address kind is read from its two top bits only).
use crate::{
    bluetooth::{
        central::Central,
        peripheral::Peripheral,
        scan::{Route, UuidScanner},
    },
    call_renewal::{self as sr, Action, Opening},
    dial::{Dialed, Link},
    framing::HopReader,
};
use std::{
    collections::VecDeque,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

/// Why a transport step failed (a stable word, never a code, address or UUID).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure(pub &'static str);

/// The scanner's state, polled once per worker tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPoll {
    Pending,
    /// The derived advert was seen; the value is the address kind (`rpa`, `static`, `other`).
    Found(&'static str),
    Failed,
}

/// Everything the renewal needs from the radio. `WinRtTransport` is the
/// production implementation; unit tests use a fake with in-memory links.
pub trait Transport {
    /// Starts scanning for `uuid` and returns at once; `scan_poll` reports.
    fn scan_start(&mut self, uuid: [u8; 16], until: Instant) -> Result<(), Failure>;
    fn scan_poll(&mut self) -> ScanPoll;
    fn scan_stop(&mut self);
    /// Connects as central to the last sighting. Blocks at most until `until`.
    fn connect(&mut self, until: Instant) -> Result<Link, Failure>;
    /// Registers the renewal GATT server (the armed side).
    fn register(&mut self, uuid: [u8; 16], until: Instant) -> Result<(), Failure>;
    /// Starts the advert of the registered server.
    fn advertise(&mut self, until: Instant) -> Result<(), Failure>;
    fn stop_advert(&mut self);
    /// A phone subscribed to the renewal server: its link (the server is handed over).
    fn accept(&mut self) -> Result<Option<Link>, Failure>;
    /// A connection was dropped: serve the next one (same UUID, same window).
    fn next_connection(&mut self) -> Result<(), Failure>;
    /// Stops the scan, the advert and the server.
    fn close_all(&mut self);
}

/// What the renewal advert is: one fresh connectable legacy set carrying only
/// the derived UUID (no announcement, no manufacturer or service data, empty
/// scan response), non-discoverable. The address is the OS's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvertPlan {
    pub uuids: Vec<[u8; 16]>,
    pub connectable: bool,
    pub announcement: bool,
    pub service_data: Vec<u8>,
    pub non_discoverable: bool,
    pub until_ms: u64,
}
impl AdvertPlan {
    pub fn new(uuid: [u8; 16], until_ms: u64) -> Self {
        Self { uuids: vec![uuid], connectable: true, announcement: false, service_data: Vec::new(), non_discoverable: true, until_ms }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ServerError {
    Closed,
    /// Eight connections were challenged already.
    Exhausted,
    /// No valid PROOF within 1,000 ms of the challenge (a squatter).
    WrongProof,
}

/// The bookkeeping of one attempt's server: at most 8 challenged connections,
/// each with one second to prove the key. The server of another attempt is
/// never touched by this one.
pub struct Server {
    uuid: [u8; 16],
    until_ms: u64,
    challenged: u8,
    open: bool,
    current_since_ms: Option<u64>,
}
impl Server {
    pub fn new(uuid: [u8; 16], until_ms: u64) -> Self {
        Self { uuid, until_ms, challenged: 0, open: true, current_since_ms: None }
    }
    pub fn id(&self) -> [u8; 16] {
        self.uuid
    }
    pub fn until_ms(&self) -> u64 {
        self.until_ms
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn close(&mut self) {
        self.open = false;
    }
    pub fn challenged(&self) -> u8 {
        self.challenged
    }
    /// A client subscribed: it is challenged now. Never more than 8 per attempt.
    pub fn challenge(&mut self, now_ms: u64) -> Result<(), ServerError> {
        if !self.open {
            return Err(ServerError::Closed);
        }
        if self.challenged >= sr::MAX_CHALLENGES {
            return Err(ServerError::Exhausted);
        }
        self.challenged += 1;
        self.current_since_ms = Some(now_ms);
        Ok(())
    }
    /// The connection's proof outcome. Valid and inside 1,000 ms passes; anything else drops it.
    pub fn outcome(&mut self, now_ms: u64, valid: bool) -> Result<(), ServerError> {
        let since = self.current_since_ms.take().ok_or(ServerError::Closed)?;
        if valid && now_ms.saturating_sub(since) < sr::PROOF_MS {
            Ok(())
        } else {
            Err(ServerError::WrongProof)
        }
    }
}

// ---------------------------------------------------------------------------
// The opening over a link's GATT byte stream (RD11): fixed-length frames, then BMS1.
// ---------------------------------------------------------------------------

/// Reads until `buffer` holds `need` bytes, or `deadline`.
fn fill(link: &Link, buffer: &mut Vec<u8>, need: usize, deadline: Instant) -> Result<(), Failure> {
    while buffer.len() < need {
        let now = Instant::now();
        if now >= deadline {
            return Err(Failure("timeout"));
        }
        let wait = (deadline - now).min(Duration::from_millis(50));
        if crate::dial::pull_link(link, buffer, wait).is_err() {
            return Err(Failure("closed"));
        }
    }
    Ok(())
}

/// The offerer's side: CHALLENGE in, PROOF out, CONFIRM in. Returns the bytes
/// read past CONFIRM (the hop reader's).
pub fn open_as_offerer(link: &mut Link, opening: &mut Opening, deadline: Instant) -> Result<Vec<u8>, Failure> {
    let mut buffer = Vec::new();
    fill(link, &mut buffer, sr::CHALLENGE_LEN, deadline)?;
    let challenge = sr::parse_challenge(&buffer[..sr::CHALLENGE_LEN]).ok_or(Failure("challenge"))?;
    buffer.drain(..sr::CHALLENGE_LEN);
    let proof = opening.proof(&challenge).ok_or(Failure("proof"))?;
    link.write(&proof, deadline).map_err(|_| Failure("write"))?;
    fill(link, &mut buffer, sr::CONFIRM_LEN, deadline)?;
    let confirmed = opening.accept_confirm(&buffer[..sr::CONFIRM_LEN]);
    buffer.drain(..sr::CONFIRM_LEN);
    if confirmed { Ok(buffer) } else { Err(Failure("confirm")) }
}

/// The armed side of one connection: CHALLENGE out, PROOF in (1,000 ms), CONFIRM out.
pub fn open_as_armed(link: &mut Link, opening: &mut Opening, now: Instant) -> Result<Vec<u8>, Failure> {
    let deadline = now + Duration::from_millis(sr::PROOF_MS);
    link.write(&sr::challenge_frame(&opening.challenge()), deadline).map_err(|_| Failure("write"))?;
    let mut buffer = Vec::new();
    fill(link, &mut buffer, sr::PROOF_LEN, deadline)?;
    let proved = opening.accept_proof(&buffer[..sr::PROOF_LEN]);
    buffer.drain(..sr::PROOF_LEN);
    if !proved {
        return Err(Failure("proof"));
    }
    let confirm = opening.confirm().ok_or(Failure("confirm"))?;
    link.write(&confirm, deadline.max(Instant::now() + Duration::from_millis(500))).map_err(|_| Failure("write"))?;
    Ok(buffer)
}

/// The first BMS1 record of a new link (BIND must be it).
fn first_record(link: &Link, buffer: &mut Vec<u8>, hop_in: &mut HopReader, deadline: Instant) -> Result<crate::records::Record, Failure> {
    loop {
        let mut input = buffer.as_slice();
        let record = hop_in.read(&mut input, 1024);
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        match record {
            Ok(Some(record)) => return Ok(record),
            Ok(None) => {}
            Err(_) => return Err(Failure("hop")),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Failure("timeout"));
        }
        if crate::dial::pull_link(link, buffer, (deadline - now).min(Duration::from_millis(50))).is_err() {
            return Err(Failure("closed"));
        }
    }
}

fn random16() -> Result<[u8; 16], Failure> {
    let random = crate::crypto::random_nonce().map_err(|_| Failure("rng"))?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&random[..16]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// The owner.
// ---------------------------------------------------------------------------

enum Io {
    Idle,
    Scanning,
    Serving { server: Server, opening: Opening },
}

/// One call's renewal I/O: performs the state machine's actions on a
/// `Transport`. Lives with the call; one worker thread drives it.
pub struct RenewalIo {
    transport: Box<dyn Transport>,
    io: Io,
    /// Tests: report the hop as GATT whatever link kind the call holds.
    #[cfg(test)]
    pub(crate) force_gatt: bool,
}
impl Default for RenewalIo {
    fn default() -> Self {
        Self::new(Box::new(WinRtTransport::default()))
    }
}

type Trace<'a> = &'a mut dyn FnMut(&str, &str);

impl RenewalIo {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            io: Io::Idle,
            #[cfg(test)]
            force_gatt: false,
        }
    }

    /// One worker tick. `video` is whether the call sends video (the new link
    /// asks for the video preset). `Some(reason)`: the call is over (link lost).
    pub fn step(&mut self, dialed: &mut Dialed, video: bool, trace: Trace) -> Option<&'static str> {
        let now = dialed.now_ns();
        #[allow(unused_mut)]
        let mut env = dialed.renewal_env();
        #[cfg(test)]
        if self.force_gatt {
            env.gatt = true;
        }
        let mut end = None;
        let pending = dialed.session.renewal_tick(now, env);
        self.run(pending, dialed, video, &mut *trace, &mut end);
        let polled = self.poll(dialed, video, &mut *trace);
        self.run(polled, dialed, video, &mut *trace, &mut end);
        // With the old link given up and nothing else to read, the worker would spin.
        if matches!(dialed.link, Link::Closed) && dialed.wifi.link.is_none() && dialed.session.renewal_running() {
            std::thread::sleep(Duration::from_millis(5));
        }
        end
    }

    /// The old Bluetooth link failed (read, write or service) while a
    /// renewal owned the call's liveness: it is given up and the renewal
    /// decides the call's fate.
    pub fn note_old_link_failed(&mut self, dialed: &mut Dialed, what: &str, trace: Trace) {
        trace("call_renewal", &format!("role={} | phase=link_dead | reason={what}", dialed.session.renewal_role()));
        dialed.retire_old_link();
        dialed.session.renewal_old_link_failed();
    }

    /// Anything still running stops with the call.
    pub fn stop(&mut self) {
        self.transport.close_all();
        self.io = Io::Idle;
    }

    fn run(&mut self, first: Vec<Action>, dialed: &mut Dialed, video: bool, trace: Trace, end: &mut Option<&'static str>) {
        let mut queue: VecDeque<Action> = first.into();
        while let Some(action) = queue.pop_front() {
            let follow = self.apply(action, dialed, video, &mut *trace, end);
            queue.extend(follow);
        }
    }

    fn deadline(dialed: &Dialed, until_ms: u64) -> Instant {
        let now_ms = dialed.now_ns() / 1_000_000;
        Instant::now() + Duration::from_millis(until_ms.saturating_sub(now_ms))
    }

    fn apply(&mut self, action: Action, dialed: &mut Dialed, video: bool, trace: Trace, end: &mut Option<&'static str>) -> Vec<Action> {
        let now = dialed.now_ns();
        match action {
            Action::Log { role, phase, reason, attempt } => {
                let ms = (now / 1_000_000).saturating_sub(dialed.session.renewal_started_ms());
                // The advert is non-discoverable (IsDiscoverable false); its address is the OS's.
                let extra = if phase == "armed" { " | nonDiscoverable=true" } else { "" };
                trace("call_renewal", &format!("role={role} | phase={phase} | reason={reason} | ms={ms} | attempt={attempt}{extra}"));
                Vec::new()
            }
            // The session queued every RENEW record itself.
            Action::Send(_) | Action::Reassert => Vec::new(),
            Action::Scan { uuid, until_ms } => match self.transport.scan_start(uuid, Self::deadline(dialed, until_ms)) {
                Ok(()) => {
                    self.io = Io::Scanning;
                    Vec::new()
                }
                Err(Failure(why)) => {
                    trace("call_renewal", &format!("role=offerer | phase=failed | reason=scan_{why}"));
                    dialed.session.renewal_scan_failed(now)
                }
            },
            Action::RegisterServer { uuid, until_ms } => {
                let ok = match self.transport.register(uuid, Self::deadline(dialed, until_ms)) {
                    Ok(()) => true,
                    Err(Failure(why)) => {
                        trace("call_renewal", &format!("role=armed | phase=failed | reason=registration_{why}"));
                        false
                    }
                };
                dialed.session.renewal_registered(dialed.now_ns(), ok)
            }
            Action::Advertise { uuid, until_ms } => {
                let ok = match self.transport.advertise(Self::deadline(dialed, until_ms)) {
                    Ok(()) => true,
                    Err(Failure(why)) => {
                        trace("call_renewal", &format!("role=armed | phase=failed | reason=advertising_{why}"));
                        false
                    }
                };
                if ok {
                    let key = dialed.session.renewal_key();
                    let first = random16();
                    if let (Some(key), Ok(challenge)) = (key, first) {
                        let opening = Opening::armed(key, dialed.session.renewal_call_id(), dialed.session.renewal_ticket(), challenge);
                        self.io = Io::Serving { server: Server::new(uuid, until_ms), opening };
                    }
                }
                dialed.session.renewal_advertising(dialed.now_ns(), ok)
            }
            Action::StopAdvert => {
                self.transport.stop_advert();
                Vec::new()
            }
            Action::CloseOldLink => {
                self.transport.scan_stop();
                dialed.retire_old_link();
                Vec::new()
            }
            Action::Connect { until_ms, .. } => {
                let until = Self::deadline(dialed, until_ms);
                self.connect(dialed, until, video, trace)
            }
            Action::Stop => {
                self.stop();
                Vec::new()
            }
            Action::EndCall(reason) => {
                *end = Some(reason);
                Vec::new()
            }
        }
    }

    fn connect_failed(dialed: &mut Dialed, why: &str, trace: Trace) -> Vec<Action> {
        trace("call_renewal", &format!("role=offerer | phase=connect_timeout | reason={why}"));
        let now = dialed.now_ns();
        dialed.session.renewal_connect_failed(now)
    }

    /// The offerer's connect, opening and BIND (the old link is already closed).
    fn connect(&mut self, dialed: &mut Dialed, until: Instant, video: bool, trace: Trace) -> Vec<Action> {
        trace("call_renewal", "role=offerer | phase=connecting | reason=");
        let mut link = match self.transport.connect(until) {
            Ok(link) => link,
            Err(Failure(why)) => return Self::connect_failed(dialed, why, trace),
        };
        let (Some(key), Ok(nonce)) = (dialed.session.renewal_key(), random16()) else {
            return Self::connect_failed(dialed, "key", trace);
        };
        let mut opening = Opening::offerer(key, dialed.session.renewal_call_id(), dialed.session.renewal_ticket(), nonce);
        let leftover = match open_as_offerer(&mut link, &mut opening, until) {
            Ok(leftover) => leftover,
            Err(Failure(why)) => return Self::connect_failed(dialed, why, trace),
        };
        trace("call_renewal", "role=offerer | phase=proved | reason=");
        let Some(keys) = opening.hop_keys() else { return Self::connect_failed(dialed, "keys", trace) };
        let Ok((mut hop_out, hop_in)) = dialed.session.renewal_hop_layer(true, &keys) else {
            return Self::connect_failed(dialed, "hop", trace);
        };
        let now = dialed.now_ns();
        // The 12 s bound is checked inside: a late BIND is never sent.
        let Ok(bind) = dialed.session.renewal_seal_bind(&mut hop_out, now) else {
            return Self::connect_failed(dialed, "bound", trace);
        };
        if link.write(&bind, until).is_err() {
            return Self::connect_failed(dialed, "bind_write", trace);
        }
        let silence = dialed.session.renewal_silence_ms(now);
        let actions = dialed.bind_renewed_link(link, leftover, hop_out, hop_in, video, &mut *trace);
        trace("call_renewal_silence", &format!("role=offerer | ms={silence}"));
        self.stop();
        actions
    }

    /// Per tick: the scanner's answer and, for the armed side, a phone that subscribed.
    fn poll(&mut self, dialed: &mut Dialed, video: bool, trace: Trace) -> Vec<Action> {
        let now = dialed.now_ns();
        let mode = match self.io {
            Io::Idle => 0,
            Io::Scanning => 1,
            Io::Serving { .. } => 2,
        };
        match mode {
            1 => match self.transport.scan_poll() {
                ScanPoll::Pending => Vec::new(),
                ScanPoll::Found(kind) => {
                    trace("call_renewal", &format!("role=offerer | phase=found | reason= | addressKind={kind}"));
                    dialed.session.renewal_found(now)
                }
                ScanPoll::Failed => {
                    trace("call_renewal", "role=offerer | phase=failed | reason=scan");
                    self.io = Io::Idle;
                    dialed.session.renewal_scan_failed(now)
                }
            },
            2 => match self.transport.accept() {
                Ok(Some(link)) => self.serve(link, dialed, video, trace),
                Ok(None) => Vec::new(),
                Err(Failure(why)) => {
                    trace("call_renewal", &format!("role=armed | phase=failed | reason=accept_{why}"));
                    dialed.session.renewal_failed("accept")
                }
            },
            _ => Vec::new(),
        }
    }

    /// The armed side of one connection to its server.
    fn serve(&mut self, mut link: Link, dialed: &mut Dialed, video: bool, trace: Trace) -> Vec<Action> {
        let Io::Serving { mut server, mut opening } = std::mem::replace(&mut self.io, Io::Idle) else {
            return Vec::new();
        };
        let now_ms = dialed.now_ns() / 1_000_000;
        if server.challenge(now_ms).is_err() {
            trace("call_renewal", "role=armed | phase=failed | reason=connections");
            self.transport.close_all();
            return dialed.session.renewal_failed("connections");
        }
        let opened = open_as_armed(&mut link, &mut opening, Instant::now());
        let valid = opened.is_ok();
        let judged = server.outcome(dialed.now_ns() / 1_000_000, valid);
        let mut leftover = match (opened, judged) {
            (Ok(leftover), Ok(())) => leftover,
            _ => {
                // A squatter, or a proof that came too late: dropped, the next one is challenged afresh.
                drop(link);
                trace("call_renewal", "role=armed | phase=refused | reason=squatter");
                // A new server needs up to the 3 s registration bound; it must be on air, if at all,
                // inside the 7 s advert window, which is never extended or put back on air.
                if dialed.now_ns() / 1_000_000 + sr::REGISTRATION_MS > server.until_ms() {
                    self.transport.close_all();
                    return Vec::new();
                }
                let Ok(fresh) = random16() else { return dialed.session.renewal_failed("rng") };
                if !opening.next_connection(fresh, dialed.now_ns() / 1_000_000) || self.transport.next_connection().is_err() {
                    self.transport.close_all();
                    return dialed.session.renewal_failed("connections");
                }
                self.io = Io::Serving { server, opening };
                return Vec::new();
            }
        };
        trace("call_renewal", "role=armed | phase=proved | reason=");
        // BIND must be the first BMS1 record of this connection, once, for this ticket.
        let Some(keys) = opening.hop_keys() else { return dialed.session.renewal_failed("keys") };
        let Ok((hop_out, mut hop_in)) = dialed.session.renewal_hop_layer(false, &keys) else {
            return dialed.session.renewal_failed("hop");
        };
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let accepted = match first_record(&link, &mut leftover, &mut hop_in, deadline) {
            Ok(record) if record.header.kind == 5 => dialed.session.renewal_accept_bind(&record.plaintext, dialed.now_ns()),
            _ => false,
        };
        if !accepted {
            drop(link);
            self.transport.close_all();
            trace("call_renewal", "role=armed | phase=failed | reason=bind");
            return dialed.session.renewal_failed("bind");
        }
        let silence = dialed.session.renewal_silence_ms(dialed.now_ns());
        let actions = dialed.bind_renewed_link(link, leftover, hop_out, hop_in, video, &mut *trace);
        trace("call_renewal_silence", &format!("role=armed | ms={silence}"));
        self.transport.close_all();
        actions
    }
}

// ---------------------------------------------------------------------------
// The WinRT transport.
// ---------------------------------------------------------------------------

fn failure(error: crate::bluetooth::Error) -> Failure {
    use crate::bluetooth::Error as E;
    Failure(match error {
        E::AdvertisingAborted => "aborted",
        E::AdvertisingIncomplete => "incomplete",
        E::Timeout => "timeout",
        E::NotResponding => "not_responding",
        E::Cancelled => "cancelled",
        E::Unsupported => "unsupported",
        E::Radio(_) => "radio",
        _ => "native",
    })
}

/// Scans with a UUID-filtered watcher, connects as central, and serves the
/// renewal as a dedicated peripheral (`Form::Renewal`). One server and one
/// scanner at a time; each has its own cancel flag, so ending one never
/// cancels another or the live call's link.
#[derive(Default)]
pub struct WinRtTransport {
    scanner: Option<UuidScanner>,
    found: Option<Route>,
    server: Option<Peripheral>,
    uuid: Option<[u8; 16]>,
}
impl Transport for WinRtTransport {
    fn scan_start(&mut self, uuid: [u8; 16], _until: Instant) -> Result<(), Failure> {
        self.found = None;
        self.scanner = Some(UuidScanner::start(uuid).map_err(failure)?);
        Ok(())
    }
    fn scan_poll(&mut self) -> ScanPoll {
        let Some(scanner) = self.scanner.as_ref() else { return ScanPoll::Failed };
        match scanner.poll() {
            Ok(Some((route, kind))) => {
                self.found = Some(route);
                self.scanner = None;
                ScanPoll::Found(kind)
            }
            Ok(None) => ScanPoll::Pending,
            Err(_) => {
                self.scanner = None;
                ScanPoll::Failed
            }
        }
    }
    fn scan_stop(&mut self) {
        self.scanner = None;
    }
    fn connect(&mut self, until: Instant) -> Result<Link, Failure> {
        let route = self.found.take().ok_or(Failure("no_sighting"))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        Central::connect_until(&route, cancelled, until).map(Link::Central).map_err(failure)
    }
    fn register(&mut self, uuid: [u8; 16], _until: Instant) -> Result<(), Failure> {
        self.uuid = Some(uuid);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.server = Some(Peripheral::create_renewal(uuid, cancelled).map_err(failure)?);
        Ok(())
    }
    fn advertise(&mut self, _until: Instant) -> Result<(), Failure> {
        self.server.as_mut().ok_or(Failure("no_server"))?.start().map_err(failure)
    }
    fn stop_advert(&mut self) {
        if let Some(server) = self.server.as_mut() {
            server.stop_announcement();
        }
    }
    fn accept(&mut self) -> Result<Option<Link>, Failure> {
        if self.server.as_ref().is_some_and(|server| server.has_client()) {
            Ok(self.server.take().map(Link::Peripheral))
        } else {
            Ok(None)
        }
    }
    fn next_connection(&mut self) -> Result<(), Failure> {
        let uuid = self.uuid.ok_or(Failure("no_server"))?;
        self.register(uuid, Instant::now())?;
        self.advertise(Instant::now())
    }
    fn close_all(&mut self) {
        self.scanner = None;
        self.found = None;
        if let Some(mut server) = self.server.take() {
            server.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_renewal_advert_is_connectable_legacy_and_carries_only_the_derived_uuid() {
        let plan = AdvertPlan::new([0x11; 16], 7_000);
        assert!(plan.connectable);
        assert!(!plan.announcement);
        assert!(plan.service_data.is_empty());
        assert!(plan.non_discoverable);
        assert_eq!(plan.uuids, vec![[0x11; 16]]);
        assert_eq!(plan.until_ms, 7_000);
    }

    #[test]
    fn a_squatter_blocks_at_most_eight_connections_and_one_second_each() {
        let mut server = Server::new([0x10; 16], 12_000);
        for i in 0..8u64 {
            let at = 1_000 + i * 1_001;
            assert_eq!(server.challenge(at), Ok(()));
            assert_eq!(server.outcome(at + 1, false), Err(ServerError::WrongProof), "wrong proof");
            assert_eq!(u64::from(server.challenged()), i + 1);
        }
        assert_eq!(server.challenge(9_000), Err(ServerError::Exhausted));
        // A right proof that arrives after a second is a squatter's too.
        let mut slow = Server::new([0x10; 16], 12_000);
        slow.challenge(0).unwrap();
        assert_eq!(slow.outcome(1_000, true), Err(ServerError::WrongProof));
        slow.challenge(2_000).unwrap();
        assert_eq!(slow.outcome(2_999, true), Ok(()));
    }

    #[test]
    fn a_second_attempt_never_closes_the_live_server() {
        let mut first = Server::new([0x10; 16], 12_000);
        let second = Server::new([0x20; 16], 12_000);
        assert_ne!(first.id(), second.id());
        first.close();
        assert!(!first.is_open());
        assert!(second.is_open());
        assert_eq!(first.challenge(0), Err(ServerError::Closed));
    }

    fn loopback() -> (crate::wifi::socket::WifiSocket, crate::wifi::socket::WifiSocket) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (
            crate::wifi::socket::WifiSocket::from_stream(client).unwrap(),
            crate::wifi::socket::WifiSocket::from_stream(server).unwrap(),
        )
    }

    #[test]
    fn the_opening_runs_over_a_link_byte_stream_in_both_directions() {
        // The same fixed-length frames as on GATT, over a loopback socket pair.
        let (offerer_side, armed_side) = loopback();
        let mut offerer_link = Link::Wifi(offerer_side);
        let key = sr::k_renew(&[1; 32], &[2; 32]).unwrap();
        let mut armed = Opening::armed(key.clone(), [4; 16], [5; 8], [6; 16]);
        let mut offerer = Opening::offerer(key, [4; 16], [5; 8], [7; 16]);
        let deadline = Instant::now() + Duration::from_secs(5);
        // A `Link` stays on the thread that made it.
        let armed_thread = std::thread::spawn(move || {
            let mut armed_link = Link::Wifi(armed_side);
            let left = open_as_armed(&mut armed_link, &mut armed, Instant::now()).unwrap();
            (left, armed.hop_keys().unwrap())
        });
        let left = open_as_offerer(&mut offerer_link, &mut offerer, deadline).unwrap();
        let (armed_left, armed_keys) = armed_thread.join().unwrap();
        assert!(left.is_empty() && armed_left.is_empty());
        assert_eq!(offerer.hop_keys().unwrap(), armed_keys);
    }
}
