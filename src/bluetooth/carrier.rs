//! Android GattVideoTransport carrier. Its sequence numbers are not authentication/replay protection.
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const HISTORY: usize = 4096;
const MAX_BYTES: usize = 1_048_576;
const HEADER: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidValue,
    Sequence,
    Overflow,
    Closed,
    Timeout,
    Cancelled,
    Backend,
    /// Design GR option A: chunk `missing` never arrived while `parked`
    /// later chunks waited `held_ms` (Android `ChunkGapPolicy.reason`).
    Lost { missing: i32, parked: u32, held_ms: u32 },
    /// `GR1` (option C): the reader gave up on a chunk.
    Recovery(super::gr1::Lost),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lost { missing, parked, held_ms } => {
                write!(f, "GATT chunk {missing} was lost ({parked} later chunks waited {held_ms} ms)")
            }
            Self::Recovery(super::gr1::Lost::Gone { missing }) => {
                write!(f, "GATT chunk {missing} was lost and the peer no longer holds it")
            }
            Self::Recovery(super::gr1::Lost::Deadline { missing, requests, held_ms }) => {
                write!(f, "GATT chunk {missing} was lost ({requests} requests, {held_ms} ms)")
            }
            Self::Recovery(super::gr1::Lost::History { missing, parked, bytes }) => write!(
                f,
                "GATT chunk {missing} was lost (the peer no longer holds it: {parked} later chunks, {bytes} bytes)"
            ),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Design GR option A (Android `ChunkGapPolicy`, identical constants): a
/// sequence gap held this long, or behind this many parked chunks, is a chunk
/// lost below the app (one ATT bearer never reorders), so the stream ends at
/// once with its reason instead of waiting for the call's 8 s liveness.
pub const GAP_MS: u64 = 1_000;
pub const GAP_CHUNKS: usize = 128;
/// `MESHLIVE_GATT_GAP_FAST_FAIL=0` is the control arm (default on, as
/// Android's `test_gatt_gap_fast_fail`): keep the old 4,096-chunk wait.
pub fn gap_fast_fail() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MESHLIVE_GATT_GAP_FAST_FAIL").ok().as_deref() != Some("0"))
}
fn gap_lost(held: Duration, parked: usize) -> bool {
    held >= Duration::from_millis(GAP_MS) || parked >= GAP_CHUNKS
}

/// Android: `(mtu - 3).coerceIn(1, 512)`. Windows can report ATT MTUs above
/// 517 (its own maximum is larger); the value is still capped at 512 bytes.
pub fn value_capacity(att_mtu: u16) -> Result<usize, Error> {
    if att_mtu < 23 {
        return Err(Error::InvalidValue);
    }
    Ok(usize::from(att_mtu - 3).min(512).min(value_max()))
}

/// Experimental `MESHLIVE_GATT_VALUE_MAX=20..512` (off: 512): caps every GATT
/// value this PC sends (notifications and writes). 244 keeps a value plus its
/// ATT and L2CAP headers inside one 251-byte LE data PDU, so the receiving
/// host never reassembles ACL fragments. Reason: a Redmi Note 9 Pro (Android
/// 11) dropped a split ACL start fragment ("L2CAP packet too small",
/// "continuation for unknown packet") in 2 of 6 PC -> phone calls at 514-byte
/// values, and the lost chunk stalled the call until the phone's 8 s
/// liveness timeout. A/B (gvm-A/B, 2026-10-20): at 244 the drop still
/// came (1-byte start fragments), so the value size is not the cause; off.
pub fn value_max() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| parse_value_max(std::env::var("MESHLIVE_GATT_VALUE_MAX").ok().as_deref()))
}
fn parse_value_max(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok()).filter(|v| (20..=512).contains(v)).unwrap_or(512)
}

pub fn pacing_interval_ns(bytes: usize, bytes_per_second: u32) -> Result<u64, Error> {
    if bytes_per_second == 0 {
        return Err(Error::InvalidValue);
    }
    let numerator = u64::try_from(bytes)
        .map_err(|_| Error::InvalidValue)?
        .checked_mul(1_000_000_000)
        .ok_or(Error::InvalidValue)?;
    Ok(numerator.div_ceil(u64::from(bytes_per_second)))
}

/// Callback arrival times do not measure air throughput. A rate must be selected by its owner.
#[derive(Default)]
pub struct Pacer {
    next_ns: u64,
}
impl Pacer {
    pub fn reserve(&mut self, now_ns: u64, bytes: usize, rate: u32) -> Result<u64, Error> {
        let ready = now_ns.max(self.next_ns);
        self.next_ns = ready
            .checked_add(pacing_interval_ns(bytes, rate)?)
            .ok_or(Error::InvalidValue)?;
        Ok(ready)
    }
}

struct State {
    ready: VecDeque<Vec<u8>>,
    pending: BTreeMap<i32, Vec<u8>>,
    recent: HashSet<i32>,
    history: VecDeque<i32>,
    next: i32,
    offset: usize,
    bytes: usize,
    closed: bool,
    failure: Option<Error>,
    max_chunks: usize,
    max_bytes: usize,
    /// Arrival time and size of every data value (CF-MEDIA-23), bounded; the
    /// owner takes them. Type-2 control values are not media and are left out.
    arrivals: VecDeque<(Instant, usize)>,
    /// When the current sequence gap opened (the first chunk parked behind a
    /// missing one); None while there is none.
    gap_opened: Option<Instant>,
    /// Design GR option A on (`gap_fast_fail`; tests set it directly).
    fast_fail: bool,
    /// Experimental `GR1` for this link (None while the switch is off).
    recovery: Option<Arc<super::gr1::Recovery>>,
    reader: super::gr1::Reader,
    /// Bytes of the values held in `pending`.
    parked_bytes: usize,
    /// Test hook `MESHLIVE_GATT_DROP_RECEIVED_EVERY=N` (test-hooks builds
    /// only): drop the first copy of every data value whose sequence is
    /// N-1 mod N, as if the host had lost it.
    drop_every: u32,
    dropped_once: VecDeque<i32>,
    /// Lines for the owner's trace (`gatt_debug_drop`).
    notes: Vec<String>,
}

/// One immutable owned buffer per callback. Reader and native callbacks share bounded state.
#[derive(Clone)]
pub struct Pipe(Arc<(Mutex<State>, Condvar)>);
impl Default for Pipe {
    fn default() -> Self {
        Self::with_limits(HISTORY, MAX_BYTES)
    }
}
impl Pipe {
    fn with_limits(max_chunks: usize, max_bytes: usize) -> Self {
        Self(Arc::new((
            Mutex::new(State {
                ready: VecDeque::new(),
                pending: BTreeMap::new(),
                recent: HashSet::new(),
                history: VecDeque::new(),
                next: 0,
                offset: 0,
                bytes: 0,
                closed: false,
                failure: None,
                max_chunks,
                max_bytes,
                arrivals: VecDeque::new(),
                gap_opened: None,
                fast_fail: gap_fast_fail(),
                recovery: None,
                reader: super::gr1::Reader::default(),
                parked_bytes: 0,
                drop_every: 0,
                dropped_once: VecDeque::new(),
                notes: Vec::new(),
            }),
            Condvar::new(),
        )))
    }

    /// The pipe of one GATT link (`link`: "client" or "server"), with `GR1`
    /// when its Experimental switch is on and the receive-drop test hook.
    pub fn for_link(link: &'static str) -> Self {
        let pipe = Self::default();
        {
            let mut state = pipe.0.0.lock().unwrap();
            state.recovery = super::gr1::Recovery::for_link(link);
            state.drop_every = drop_every();
        }
        pipe
    }
    /// The link's `GR1` state, when on.
    pub fn recovery(&self) -> Option<Arc<super::gr1::Recovery>> {
        self.0.0.lock().unwrap().recovery.clone()
    }
    #[cfg(test)]
    fn with_recovery(recovery: Arc<super::gr1::Recovery>) -> Self {
        let pipe = Self::default();
        pipe.0.0.lock().unwrap().recovery = Some(recovery);
        pipe
    }
    /// `(phase, detail)` lines for the owner's trace since the last call:
    /// `gatt_recovery` and `gatt_debug_drop`.
    pub fn take_notes(&self) -> Vec<(&'static str, String)> {
        let (notes, recovery) = {
            let mut state = self.0.0.lock().unwrap();
            (std::mem::take(&mut state.notes), state.recovery.clone())
        };
        let mut out: Vec<(&'static str, String)> = notes.into_iter().map(|n| ("gatt_debug_drop", n)).collect();
        if let Some(recovery) = recovery {
            out.extend(recovery.take_log().into_iter().map(|l| ("gatt_recovery", l)));
        }
        out
    }
    /// `GR1` request rounds and the deadline while nothing arrives (the
    /// link's worker; never an event handler).
    pub fn recovery_tick(&self) {
        let mut state = self.0.0.lock().unwrap();
        if state.closed || !Self::recovering(&state) {
            return;
        }
        if let Err(error) = Self::update_recovery(&mut state) {
            Self::fault(&mut state, Some(error));
            self.0.1.notify_all();
        }
    }
    fn recovering(state: &State) -> bool {
        state.recovery.as_ref().is_some_and(|r| r.peer_hello().is_some())
    }
    /// The reader half: missing ranges, due NACKs and the end rules.
    fn update_recovery(state: &mut State) -> Result<(), Error> {
        let Some(recovery) = state.recovery.clone() else { return Ok(()) };
        let Some(peer) = recovery.peer_hello() else { return Ok(()) };
        let pending: Vec<i32> = state.pending.keys().copied().collect();
        let update = state.reader.update(peer, recovery.value_limit(), state.next, &pending, state.parked_bytes, recovery.now_ns());
        for note in update.notes {
            recovery.note(note);
        }
        if let Some(ranges) = update.nack {
            recovery.send_nack(&ranges);
        }
        if let Some(lost) = update.failure {
            let error = Error::Recovery(lost);
            recovery.note(format!("event=failed | reason={error}"));
            return Err(error);
        }
        Ok(())
    }
    /// A type-2 value: `GR1` when on (parse and enqueue only), else ignored.
    fn on_control(state: &mut State, value: &[u8]) -> Result<(), Error> {
        let Some(recovery) = state.recovery.clone() else { return Ok(()) };
        if !super::gr1::is_gr1(value) {
            return Ok(());
        }
        match super::gr1::decode(value) {
            None => recovery.note(format!("event=ignored | reason=malformed | bytes={}", value.len())),
            Some(super::gr1::Message::Hello(hello)) => recovery.on_hello(hello),
            Some(super::gr1::Message::Nack(ranges)) => recovery.on_nack(&ranges),
            Some(super::gr1::Message::High(sequence)) => {
                let Some(peer) = recovery.peer_hello() else { return Ok(()) };
                let mut notes = Vec::new();
                let hit = state.reader.on_high(peer, sequence, state.next, &mut notes);
                for note in notes {
                    recovery.note(note);
                }
                if hit {
                    if state.gap_opened.is_none() {
                        state.gap_opened = Some(Instant::now());
                    }
                    return Self::update_recovery(state);
                }
            }
            Some(super::gr1::Message::Gone(range)) => {
                if !Self::recovering(state) {
                    return Ok(());
                }
                match state.reader.gone_hit(range) {
                    Some(missing) => {
                        let error = Error::Recovery(super::gr1::Lost::Gone { missing });
                        recovery.note(format!("event=failed | reason={error}"));
                        return Err(error);
                    }
                    None => recovery.note(format!("event=ignored | reason=gone_not_missing | seq={}", range.first)),
                }
            }
        }
        Ok(())
    }

    /// Arrival times and sizes of the notifications since the last call.
    pub fn take_arrivals(&self) -> Vec<(Instant, usize)> {
        self.0.0.lock().unwrap().arrivals.drain(..).collect()
    }
    pub fn offer(&self, packet: &[u8]) -> Result<(), Error> {
        let arrived = Instant::now();
        let mut state = self.0.0.lock().unwrap();
        if state.drop_every > 0 && !state.closed && packet.first() == Some(&1) && packet.len() > HEADER {
            let sequence = i32::from_be_bytes(packet[1..5].try_into().unwrap());
            let every = state.drop_every as i32;
            if sequence.rem_euclid(every) == every - 1 && !state.dropped_once.contains(&sequence) {
                if state.dropped_once.len() >= HISTORY {
                    state.dropped_once.pop_front();
                }
                state.dropped_once.push_back(sequence);
                state.notes.push(format!("seq={sequence} every={every}"));
                return Ok(());
            }
        }
        if packet.first() == Some(&1) {
            if state.arrivals.len() >= 4096 {
                state.arrivals.pop_front();
            }
            state.arrivals.push_back((arrived, packet.len()));
        }
        if state.closed {
            return Err(state.failure.unwrap_or(Error::Closed));
        }
        let result = (|| {
            if packet.is_empty() || packet.len() > 512 {
                return Err(Error::InvalidValue);
            }
            if packet[0] == 2 {
                // Type 2: ignored by older readers; `GR1` when on (parsed and
                // queued here, never sent from this event handler).
                return Self::on_control(&mut state, packet);
            }
            if packet[0] != 1 || packet.len() <= HEADER {
                return Err(Error::InvalidValue);
            }
            let sequence = i32::from_be_bytes(packet[1..5].try_into().unwrap());
            if state.recent.contains(&sequence) || state.pending.contains_key(&sequence) {
                return Ok(());
            }
            let ahead = super::gr1::seq_after(sequence, state.next);
            if ahead > 0 {
                if ahead as usize > HISTORY || state.pending.len() >= HISTORY {
                    return Err(Error::Sequence);
                }
                if state.pending.is_empty() {
                    state.gap_opened = Some(arrived);
                }
                state.parked_bytes += packet.len();
                state.pending.insert(sequence, packet[HEADER..].to_vec());
                // With the peer's HELLO the reader asks for the chunk; its
                // deadline replaces option A's limits.
                if Self::recovering(&state) {
                    return Self::update_recovery(&mut state);
                }
                return Self::check_gap(&state, arrived);
            }
            if sequence != state.next {
                return Err(Error::Sequence);
            }
            Self::accept(&mut state, packet[HEADER..].to_vec())?;
            loop {
                let next = state.next;
                let Some(body) = state.pending.remove(&next) else {
                    break;
                };
                state.parked_bytes = state.parked_bytes.saturating_sub(body.len() + HEADER);
                Self::accept(&mut state, body)?;
            }
            // The missing chunk arrived: a gap still left behind it is a new
            // one with its own clock.
            state.gap_opened = (!state.pending.is_empty()).then_some(arrived);
            if Self::recovering(&state) {
                return Self::update_recovery(&mut state);
            }
            Ok(())
        })();
        if let Err(error) = result {
            Self::fault(&mut state, Some(error));
        }
        self.0.1.notify_all();
        result
    }

    /// Option A: the gap has been held too long or behind too many chunks.
    fn check_gap(state: &State, now: Instant) -> Result<(), Error> {
        if Self::recovering(state) {
            return Ok(());
        }
        let Some(opened) = state.gap_opened.filter(|_| state.fast_fail) else {
            return Ok(());
        };
        let held = now.saturating_duration_since(opened);
        if gap_lost(held, state.pending.len()) {
            return Err(Error::Lost {
                missing: state.next,
                parked: u32::try_from(state.pending.len()).unwrap_or(u32::MAX),
                held_ms: u32::try_from(held.as_millis()).unwrap_or(u32::MAX),
            });
        }
        Ok(())
    }

    fn accept(state: &mut State, bytes: Vec<u8>) -> Result<(), Error> {
        if state.ready.len() >= state.max_chunks || bytes.len() > state.max_bytes - state.bytes {
            return Err(Error::Overflow);
        }
        if state.history.len() >= HISTORY {
            let oldest = state.history.pop_front().unwrap();
            state.recent.remove(&oldest);
        }
        let next = state.next;
        state.history.push_back(next);
        state.recent.insert(next);
        state.next = next.wrapping_add(1);
        state.bytes += bytes.len();
        state.ready.push_back(bytes);
        Ok(())
    }

    fn fault(state: &mut State, error: Option<Error>) {
        if state.closed {
            return;
        }
        state.closed = true;
        state.failure = error;
        state.gap_opened = None;
        state.parked_bytes = 0;
        if let Some(recovery) = &state.recovery {
            recovery.close();
        }
        state.ready.clear();
        state.pending.clear();
        state.history.clear();
        state.recent.clear();
        state.bytes = 0;
        state.offset = 0;
    }

    pub fn abort(&self, error: Error) {
        Self::fault(&mut self.0.0.lock().unwrap(), Some(error));
        self.0.1.notify_all();
    }
    pub fn close(&self) {
        Self::fault(&mut self.0.0.lock().unwrap(), None);
        self.0.1.notify_all();
    }

    pub fn read(
        &self,
        output: &mut [u8],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<usize, Error> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut state = self.0.0.lock().unwrap();
        loop {
            if let Some(error) = state.failure {
                return Err(error);
            }
            if cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            if state.ready.is_empty() {
                if let Err(lost) = Self::check_gap(&state, Instant::now()) {
                    Self::fault(&mut state, Some(lost));
                    return Err(lost);
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            if let Some(first) = state.ready.front() {
                let count = output.len().min(first.len() - state.offset);
                output[..count].copy_from_slice(&first[state.offset..state.offset + count]);
                let end = state.offset + count == first.len();
                state.bytes -= count;
                state.offset += count;
                if end {
                    state.ready.pop_front();
                    state.offset = 0;
                }
                return Ok(count);
            }
            if state.closed {
                return Ok(0);
            }
            let sleep = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10));
            state = self.0.1.wait_timeout(state, sleep).unwrap().0;
        }
    }
}

pub trait LinkIo {
    fn capacity(&self) -> Result<usize, Error>;
    fn send(
        &mut self,
        value: &[u8],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), Error>;
    fn abort(&mut self);
    /// The link's receive pipe when it runs `GR1` (the writer drains its
    /// queue and keeps its history); None by default.
    fn recovery_pipe(&self) -> Option<Pipe> {
        None
    }
}

/// `GR1` test-hook switch (test-hooks builds only; 0 otherwise).
fn drop_every() -> u32 {
    #[cfg(feature = "test-hooks")]
    {
        std::env::var("MESHLIVE_GATT_DROP_RECEIVED_EVERY").ok().and_then(|v| v.trim().parse().ok()).filter(|&n| n >= 2).unwrap_or(0)
    }
    #[cfg(not(feature = "test-hooks"))]
    {
        0
    }
}

/// Exclusive worker ownership serializes whole writes. Partial failure never permits reuse/retry.
#[derive(Default)]
pub struct Writer {
    sequence: i32,
    failed: bool,
}
impl Writer {
    pub fn write(
        &mut self,
        mut bytes: &[u8],
        io: &mut impl LinkIo,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        let recovery = io.recovery_pipe().and_then(|p| p.recovery());
        let result = (|| {
            while !bytes.is_empty() {
                if cancelled.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                let capacity = io.capacity()?;
                if !(20..=512).contains(&capacity) {
                    return Err(Error::InvalidValue);
                }
                let count = bytes.len().min(capacity - HEADER);
                let mut packet = Vec::with_capacity(count + HEADER);
                packet.push(1);
                packet.extend_from_slice(&self.sequence.to_be_bytes());
                packet.extend_from_slice(&bytes[..count]);
                if let Some(recovery) = &recovery {
                    // GR1: requests and resends go before new data; the
                    // history keeps its own copy of every data value.
                    recovery.start(capacity);
                    Self::drain(recovery, io, deadline, cancelled)?;
                    recovery.on_data_sent(self.sequence, &packet);
                }
                io.send(&packet, deadline, cancelled)?;
                if cancelled.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Timeout);
                }
                self.sequence = self.sequence.wrapping_add(1);
                bytes = &bytes[count..];
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
            io.abort();
        }
        result
    }
    /// GR1 while the data writer is idle (the link's worker, every tick):
    /// HELLO, the HIGH probe, the reader's rounds and deadline, and every
    /// queued control value and resend. A no-op while the switch is off.
    pub fn service(&mut self, io: &mut impl LinkIo, deadline: Instant, cancelled: &AtomicBool) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Closed);
        }
        let Some(pipe) = io.recovery_pipe() else { return Ok(()) };
        let Some(recovery) = pipe.recovery() else { return Ok(()) };
        let Ok(capacity) = io.capacity() else { return Ok(()) };
        recovery.start(capacity);
        recovery.tick();
        pipe.recovery_tick();
        let result = Self::drain(&recovery, io, deadline, cancelled);
        if result.is_err() {
            self.failed = true;
            io.abort();
        }
        result
    }
    /// Sends the queued GR1 values (a resend is the stored value unchanged).
    fn drain(recovery: &super::gr1::Recovery, io: &mut impl LinkIo, deadline: Instant, cancelled: &AtomicBool) -> Result<(), Error> {
        while let Some(item) = recovery.pop() {
            let started = recovery.now_ns();
            io.send(&item.value, deadline, cancelled)?;
            recovery.sent(&item, started);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_value_cap_is_off_unless_set_in_range() {
        assert_eq!(parse_value_max(None), 512);
        assert_eq!(parse_value_max(Some("244")), 244);
        assert_eq!(parse_value_max(Some(" 20 ")), 20);
        assert_eq!(parse_value_max(Some("19")), 512, "below the ATT minimum: ignored");
        assert_eq!(parse_value_max(Some("600")), 512);
        assert_eq!(parse_value_max(Some("x")), 512);
    }
    fn packet(sequence: i32, bytes: &[u8]) -> Vec<u8> {
        [vec![1], sequence.to_be_bytes().to_vec(), bytes.to_vec()].concat()
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
    struct Fake {
        pipe: Pipe,
        packets: Vec<Vec<u8>>,
        capacities: VecDeque<usize>,
        fail_at: Option<usize>,
        aborted: bool,
    }
    impl Fake {
        fn new(pipe: Pipe) -> Self {
            Self {
                pipe,
                packets: vec![],
                capacities: VecDeque::from([20]),
                fail_at: None,
                aborted: false,
            }
        }
    }
    impl LinkIo for Fake {
        fn capacity(&self) -> Result<usize, Error> {
            Ok(*self.capacities.front().unwrap())
        }
        fn send(&mut self, value: &[u8], _: Instant, _: &AtomicBool) -> Result<(), Error> {
            if self.fail_at == Some(self.packets.len()) {
                return Err(Error::Backend);
            }
            self.packets.push(value.to_vec());
            if self.capacities.len() > 1 {
                self.capacities.pop_front();
            }
            self.pipe.offer(value)
        }
        fn abort(&mut self) {
            self.aborted = true;
            self.pipe.abort(Error::Backend);
        }
    }
    #[test]
    fn android_chunk_layout_and_exact_pacing_examples_match() {
        assert_eq!(packet(0x01020304, &[7, 8]), [1, 1, 2, 3, 4, 7, 8]);
        assert_eq!(pacing_interval_ns(514, 12000), Ok(42_833_334));
        assert_eq!(pacing_interval_ns(150, 12000), Ok(12_500_000));
        assert!(pacing_interval_ns(1, 0).is_err());
        assert_eq!(value_capacity(23), Ok(20));
        assert_eq!(value_capacity(517), Ok(512));
        assert!(value_capacity(22).is_err());
        for mtu in [518, 527, u16::MAX] {
            assert_eq!(value_capacity(mtu), Ok(512));
        }
        let mut pacer = Pacer::default();
        assert_eq!(pacer.reserve(100, 150, 12000), Ok(100));
        assert_eq!(pacer.reserve(100, 150, 12000), Ok(12_500_100));
        assert_eq!(pacer.reserve(50_000_000, 150, 12000), Ok(50_000_000));
    }
    #[test]
    fn callback_storage_reordering_and_duplicates_preserve_bytes() {
        let pipe = Pipe::default();
        let mut value = packet(0, &[1, 2]);
        pipe.offer(&value).unwrap();
        value.fill(99);
        pipe.offer(&packet(2, &[5, 6])).unwrap();
        pipe.offer(&packet(2, &[5, 6])).unwrap();
        pipe.offer(&packet(1, &[3, 4])).unwrap();
        pipe.offer(&packet(0, &[1, 2])).unwrap();
        let mut bytes = [0; 6];
        let stop = AtomicBool::new(false);
        let mut offset = 0;
        while offset < 6 {
            offset += pipe.read(&mut bytes[offset..], deadline(), &stop).unwrap();
        }
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6]);
    }
    #[test]
    fn loopback_fragmentation_tracks_mtu_changes_without_interleaving() {
        let pipe = Pipe::default();
        let mut io = Fake::new(pipe.clone());
        io.capacities = VecDeque::from([20, 244, 512, 20]);
        let bytes: Vec<_> = (0..16384).map(|i| (i * 31) as u8).collect();
        Writer::default()
            .write(&bytes, &mut io, deadline(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(io.packets[0].len(), 20);
        assert_eq!(io.packets[1].len(), 244);
        assert_eq!(io.packets[2].len(), 512);
        let mut received = vec![0; bytes.len()];
        let mut offset = 0;
        while offset < received.len() {
            let end = (offset + 23).min(received.len());
            offset += pipe
                .read(
                    &mut received[offset..end],
                    deadline(),
                    &AtomicBool::new(false),
                )
                .unwrap();
        }
        assert_eq!(received, bytes);
    }
    #[test]
    fn malformed_values_gaps_and_overflow_permanently_close_and_discard() {
        for bad in [
            vec![],
            vec![0],
            vec![1, 0, 0, 0, 0],
            vec![1; 513],
            packet(4097, &[7]),
            packet(-1, &[7]),
        ] {
            let pipe = Pipe::default();
            assert!(pipe.offer(&bad).is_err());
            assert!(pipe.offer(&packet(0, &[1])).is_err());
            assert!(
                pipe.read(&mut [0; 1], deadline(), &AtomicBool::new(false))
                    .is_err()
            );
        }
        for (chunks, bytes) in [(1, 100), (100, 1)] {
            let pipe = Pipe::with_limits(chunks, bytes);
            pipe.offer(&packet(0, &[1])).unwrap();
            assert_eq!(pipe.offer(&packet(1, &[2])), Err(Error::Overflow));
            assert_eq!(
                pipe.read(&mut [0; 2], deadline(), &AtomicBool::new(false)),
                Err(Error::Overflow)
            );
        }
    }
    #[test]
    fn partial_write_failure_cancellation_and_timeouts_never_retry() {
        let mut io = Fake::new(Pipe::default());
        io.fail_at = Some(1);
        let mut writer = Writer::default();
        assert_eq!(
            writer.write(&[7; 32], &mut io, deadline(), &AtomicBool::new(false)),
            Err(Error::Backend)
        );
        assert!(io.aborted);
        assert_eq!(io.packets.len(), 1);
        assert_eq!(
            writer.write(&[8], &mut io, deadline(), &AtomicBool::new(false)),
            Err(Error::Closed)
        );
        for (stop, until, expected) in [
            (true, deadline(), Error::Cancelled),
            (false, Instant::now(), Error::Timeout),
        ] {
            let mut io = Fake::new(Pipe::default());
            assert_eq!(
                Writer::default().write(&[1], &mut io, until, &AtomicBool::new(stop)),
                Err(expected)
            );
            assert!(io.packets.is_empty());
            assert!(io.aborted);
        }
    }
    fn gap_pipe(fast_fail: bool) -> Pipe {
        let pipe = Pipe::default();
        pipe.0.0.lock().unwrap().fast_fail = fast_fail;
        pipe
    }
    fn backdate_gap(pipe: &Pipe, by: Duration) {
        let mut state = pipe.0.0.lock().unwrap();
        state.gap_opened = state.gap_opened.map(|at| at - by);
    }
    #[test]
    fn a_gap_held_one_second_fails_with_its_reason() {
        let pipe = gap_pipe(true);
        pipe.offer(&packet(0, &[1])).unwrap();
        pipe.offer(&packet(2, &[3])).unwrap();
        backdate_gap(&pipe, Duration::from_millis(GAP_MS));
        let lost = pipe.offer(&packet(3, &[4])).unwrap_err();
        assert!(matches!(lost, Error::Lost { missing: 1, parked: 2, held_ms } if held_ms >= 1000), "{lost:?}");
        assert!(lost.to_string().starts_with("GATT chunk 1 was lost (2 later chunks waited "), "{lost}");
        let stop = AtomicBool::new(false);
        assert_eq!(pipe.read(&mut [0; 4], deadline(), &stop), Err(lost), "the reader gets the reason, not an EOF");
    }
    #[test]
    fn a_held_gap_fails_in_the_reader_when_nothing_more_arrives() {
        let pipe = gap_pipe(true);
        pipe.offer(&packet(0, &[1])).unwrap();
        pipe.offer(&packet(2, &[3])).unwrap();
        let stop = AtomicBool::new(false);
        assert_eq!(pipe.read(&mut [0; 4], deadline(), &stop), Ok(1));
        backdate_gap(&pipe, Duration::from_millis(GAP_MS));
        assert!(matches!(pipe.read(&mut [0; 4], deadline(), &stop), Err(Error::Lost { missing: 1, parked: 1, .. })));
    }
    #[test]
    fn one_hundred_twenty_eight_parked_chunks_fail_before_a_second() {
        let pipe = gap_pipe(true);
        for sequence in 1..GAP_CHUNKS as i32 {
            pipe.offer(&packet(sequence, &[1])).unwrap();
        }
        assert!(matches!(
            pipe.offer(&packet(GAP_CHUNKS as i32, &[1])),
            Err(Error::Lost { missing: 0, parked: 128, .. })
        ));
    }
    #[test]
    fn a_brief_reorder_delivers_and_a_second_gap_has_its_own_clock() {
        let pipe = gap_pipe(true);
        pipe.offer(&packet(1, &[2])).unwrap();
        pipe.offer(&packet(3, &[4])).unwrap();
        backdate_gap(&pipe, Duration::from_millis(900));
        pipe.offer(&packet(0, &[1])).unwrap();
        // Chunk 2 is still missing: its gap starts now, not 900 ms ago.
        assert!(pipe.0.0.lock().unwrap().gap_opened.is_some_and(|at| at.elapsed() < Duration::from_millis(100)));
        pipe.offer(&packet(2, &[3])).unwrap();
        assert!(pipe.0.0.lock().unwrap().gap_opened.is_none());
        let mut bytes = [0; 4];
        let stop = AtomicBool::new(false);
        let mut offset = 0;
        while offset < 4 {
            offset += pipe.read(&mut bytes[offset..], deadline(), &stop).unwrap();
        }
        assert_eq!(bytes, [1, 2, 3, 4]);
    }
    #[test]
    fn the_control_arm_keeps_waiting_and_type_two_is_not_an_arrival() {
        let pipe = gap_pipe(false);
        pipe.offer(&packet(2, &[3])).unwrap();
        backdate_gap(&pipe, Duration::from_secs(5));
        for sequence in 3..300 {
            pipe.offer(&packet(sequence, &[1])).unwrap();
        }
        assert_eq!(pipe.read(&mut [0; 1], Instant::now(), &AtomicBool::new(false)), Err(Error::Timeout));
        let fresh = Pipe::default();
        fresh.offer(&[2, 0x47, 0x52]).unwrap();
        fresh.offer(&packet(0, &[1])).unwrap();
        assert_eq!(fresh.take_arrivals().len(), 1, "a type-2 control value is not a media arrival");
    }
    /// One direction of a fake GATT link: values go to the peer's pipe;
    /// `own` is this side's receive pipe (and GR1 state).
    struct Wire {
        own: Pipe,
        peer: Pipe,
        drop_data: Vec<i32>,
        sent: Vec<Vec<u8>>,
    }
    impl LinkIo for Wire {
        fn capacity(&self) -> Result<usize, Error> {
            Ok(64)
        }
        fn send(&mut self, value: &[u8], _: Instant, _: &AtomicBool) -> Result<(), Error> {
            self.sent.push(value.to_vec());
            if value[0] == 1 {
                let sequence = i32::from_be_bytes(value[1..5].try_into().unwrap());
                if let Some(i) = self.drop_data.iter().position(|&s| s == sequence) {
                    self.drop_data.remove(i);
                    return Ok(());
                }
            }
            let _ = self.peer.offer(value);
            Ok(())
        }
        fn abort(&mut self) {}
        fn recovery_pipe(&self) -> Option<Pipe> {
            Some(self.own.clone())
        }
    }
    fn pair(drop_data: Vec<i32>) -> (Wire, Wire) {
        let a = Pipe::with_recovery(Arc::new(super::super::gr1::Recovery::new("server")));
        let b = Pipe::with_recovery(Arc::new(super::super::gr1::Recovery::new("client")));
        (
            Wire { own: a.clone(), peer: b.clone(), drop_data, sent: vec![] },
            Wire { own: b, peer: a, drop_data: vec![], sent: vec![] },
        )
    }
    fn read_all(pipe: &Pipe, n: usize) -> Vec<u8> {
        let mut out = vec![0; n];
        let mut offset = 0;
        let stop = AtomicBool::new(false);
        while offset < n {
            offset += pipe.read(&mut out[offset..], deadline(), &stop).unwrap();
        }
        out
    }
    #[test]
    fn a_lost_value_is_recovered_byte_for_byte_by_one_nack() {
        let (mut a, mut b) = pair(vec![3]);
        let (mut wa, mut wb) = (Writer::default(), Writer::default());
        let stop = AtomicBool::new(false);
        // HELLOs cross before data (each side's first service).
        wb.service(&mut b, deadline(), &stop).unwrap();
        wa.service(&mut a, deadline(), &stop).unwrap();
        let data: Vec<u8> = (0..59 * 8).map(|i| i as u8).collect();
        wa.write(&data, &mut a, deadline(), &stop).unwrap();
        // B noticed the gap at chunk 4 and queued a NACK; its worker sends it.
        wb.service(&mut b, deadline(), &stop).unwrap();
        let nacks: Vec<_> = b.sent.iter().filter(|v| v[0] == 2 && v[3] == 2).collect();
        assert_eq!(nacks.len(), 1);
        // A's worker answers with the stored value, unchanged.
        wa.service(&mut a, deadline(), &stop).unwrap();
        let original = a.sent.iter().find(|v| v[0] == 1 && v[1..5] == 3i32.to_be_bytes()).unwrap().clone();
        assert_eq!(a.sent.iter().filter(|v| **v == original).count(), 2, "the resend is the original value");
        assert_eq!(read_all(&a.peer, data.len()), data);
        let log: Vec<_> = b.own.take_notes().into_iter().map(|(_, l)| l).collect();
        assert!(log.iter().any(|l| l.starts_with("event=recovered | seq=3 | count=1")), "{log:?}");
        let sender_log: Vec<_> = a.own.take_notes().into_iter().map(|(_, l)| l).collect();
        assert!(sender_log.iter().any(|l| l.starts_with("event=resend | seq=3 | bytes=64 | queuedAheadMs=")), "{sender_log:?}");
    }
    #[test]
    fn without_the_peer_hello_the_reader_keeps_option_a() {
        let recovery = Arc::new(super::super::gr1::Recovery::new("client"));
        let pipe = Pipe::with_recovery(recovery.clone());
        pipe.0.0.lock().unwrap().fast_fail = true;
        pipe.offer(&packet(0, &[1])).unwrap();
        pipe.offer(&packet(2, &[3])).unwrap();
        pipe.recovery_tick();
        assert!(recovery.pop().is_none(), "no NACK without the peer's HELLO");
        backdate_gap(&pipe, Duration::from_millis(GAP_MS));
        assert!(matches!(pipe.offer(&packet(3, &[4])), Err(Error::Lost { missing: 1, .. })), "option A still ends it");
    }
    #[test]
    fn a_hello_and_controls_never_enter_the_byte_stream_and_gone_ends_with_its_reason() {
        let (mut a, mut b) = pair(vec![0]);
        let (mut wa, mut wb) = (Writer::default(), Writer::default());
        let stop = AtomicBool::new(false);
        wb.service(&mut b, deadline(), &stop).unwrap();
        wa.service(&mut a, deadline(), &stop).unwrap();
        wa.write(&[9; 59 * 2], &mut a, deadline(), &stop).unwrap();
        assert_eq!(a.peer.0.0.lock().unwrap().ready.len(), 0, "HELLO is not data");
        // The peer says it no longer holds chunk 0.
        let gone = super::super::gr1::gone(super::super::gr1::SeqRange { first: 0, run: 1 });
        assert!(a.peer.offer(&gone).is_err());
        let error = a.peer.read(&mut [0; 4], deadline(), &stop).unwrap_err();
        assert_eq!(error.to_string(), "GATT chunk 0 was lost and the peer no longer holds it");
    }
    #[test]
    fn the_drop_hook_drops_only_the_first_copy() {
        let pipe = Pipe::default();
        pipe.0.0.lock().unwrap().drop_every = 3;
        for s in 0..3 {
            pipe.offer(&packet(s, &[s as u8])).unwrap();
        }
        assert_eq!(pipe.0.0.lock().unwrap().pending.len(), 0);
        assert_eq!(pipe.0.0.lock().unwrap().next, 2, "chunk 2 dropped");
        pipe.offer(&packet(2, &[2])).unwrap();
        assert_eq!(pipe.0.0.lock().unwrap().next, 3, "its resend gets through");
        assert_eq!(pipe.take_notes(), vec![("gatt_debug_drop", "seq=2 every=3".to_owned())]);
    }
    #[test]
    fn legacy_control_stays_out_of_data_and_closed_readers_wake() {
        let pipe = Pipe::default();
        pipe.offer(&[2, 7]).unwrap();
        assert_eq!(
            pipe.read(&mut [0; 1], Instant::now(), &AtomicBool::new(false)),
            Err(Error::Timeout)
        );
        let other = pipe.clone();
        let reader = std::thread::spawn(move || {
            other.read(&mut [0; 1], deadline(), &AtomicBool::new(false))
        });
        pipe.close();
        assert_eq!(reader.join().unwrap(), Ok(0));
    }
}
