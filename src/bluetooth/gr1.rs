//! Experimental `GR1` lost-chunk recovery on the GATT chunk layer
//! (default on since the design decision of 2026-10-21, as Android;
//! `MESHLIVE_GATT_CHUNK_RECOVERY=0` is the off arm; Android
//! `test_gatt_chunk_recovery`, `GattChunkRecovery.kt`). Wire rules: PROTOCOL
//! "GATT byte stream and draft lost-chunk recovery"; reasons and review:
//! DECISIONS "Design GR" option C, revision 1. The constants are part of the
//! contract and identical on every platform.
//!
//! Threading (review 1 item 3): WinRT event handlers only parse and enqueue
//! (`Pipe::offer` calls `on_hello`/`on_nack` and the pipe's own reader); every
//! value leaves through the link's worker (`carrier::Writer`), which drains
//! the queue before each new data chunk and in `service` while idle. Nothing
//! here sends, and no lock is held across a send.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

pub const TYPE: u8 = 2;
const G: u8 = 0x47;
const R: u8 = 0x52;
pub const OP_HELLO: u8 = 1;
pub const OP_NACK: u8 = 2;
pub const OP_GONE: u8 = 3;
pub const OP_HIGH: u8 = 4;
pub const VERSION: u8 = 1;
pub const HISTORY_MS: u32 = 2_000;
pub const HISTORY_VALUES: u32 = 1_024;
pub const HISTORY_BYTES: u32 = 262_144;
pub const HELLO_REPEAT_MS: u64 = 1_000;
pub const ROUND_MS: u64 = 250;
pub const MAX_REQUESTS: u32 = 3;
pub const DEADLINE_MS: u64 = 1_000;
pub const READER_NACKS_PER_SECOND: usize = 20;
pub const RESENDS_PER_SEQUENCE: u32 = 3;
pub const RESEND_BUDGET_PERCENT: u64 = 25;
pub const RESEND_FLOOR_BYTES_PER_SECOND: u64 = 1_024;
pub const HIGH_IDLE_MS: u64 = 200;
pub const HIGH_PER_SECOND: usize = 2;
pub const MAX_OUT_OF_RANGE_NACK_RANGES: u32 = 8;
pub const MAX_RANGES: usize = 8;
pub const MAX_RUN: u32 = 255;
const MS: u64 = 1_000_000;
const SECOND: u64 = 1_000_000_000;

/// Chunk sequences modulo 2^32: positive when `a` comes after `b`.
pub fn seq_after(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

/// On by default (design decision 2026-10-21, as Android);
/// `MESHLIVE_GATT_CHUNK_RECOVERY=0` turns it off.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var("MESHLIVE_GATT_CHUNK_RECOVERY").ok().as_deref()))
}
fn switch(value: Option<&str>) -> bool {
    value != Some("0")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqRange {
    pub first: i32,
    pub run: u32,
}
impl SeqRange {
    pub fn last(&self) -> i32 {
        self.first.wrapping_add(self.run as i32 - 1)
    }
    pub fn overlaps(&self, first: i32, last: i32) -> bool {
        seq_after(self.last(), first) >= 0 && seq_after(last, self.first) >= 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub values: u32,
    pub bytes: u32,
    pub ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Nack(Vec<SeqRange>),
    Gone(SeqRange),
    High(i32),
}

pub fn is_gr1(value: &[u8]) -> bool {
    value.len() >= 4 && value[0] == TYPE && value[1] == G && value[2] == R
}
fn header(op: u8, body: usize) -> Vec<u8> {
    let mut value = vec![0; 4 + body];
    value[..4].copy_from_slice(&[TYPE, G, R, op]);
    value
}
pub fn hello(values: u32, bytes: u32, ms: u32) -> Vec<u8> {
    let mut value = header(OP_HELLO, 9);
    value[4] = VERSION;
    value[5..7].copy_from_slice(&(values as u16).to_be_bytes());
    value[7..11].copy_from_slice(&bytes.to_be_bytes());
    value[11..13].copy_from_slice(&(ms as u16).to_be_bytes());
    value
}
pub fn nack(ranges: &[SeqRange]) -> Vec<u8> {
    assert!((1..=MAX_RANGES).contains(&ranges.len()) && ranges.iter().all(|r| (1..=MAX_RUN).contains(&r.run)));
    let mut value = header(OP_NACK, 1 + 5 * ranges.len());
    value[4] = ranges.len() as u8;
    for (index, range) in ranges.iter().enumerate() {
        value[5 + 5 * index..9 + 5 * index].copy_from_slice(&range.first.to_be_bytes());
        value[9 + 5 * index] = range.run as u8;
    }
    value
}
pub fn gone(range: SeqRange) -> Vec<u8> {
    assert!((1..=MAX_RUN).contains(&range.run));
    let mut value = header(OP_GONE, 5);
    value[4..8].copy_from_slice(&range.first.to_be_bytes());
    value[8] = range.run as u8;
    value
}
pub fn high(sequence: i32) -> Vec<u8> {
    let mut value = header(OP_HIGH, 4);
    value[4..8].copy_from_slice(&sequence.to_be_bytes());
    value
}
/// NACK ranges one value can carry at this ATT value limit (0 while unknown).
pub fn nack_ranges_fitting(value_limit: usize) -> usize {
    if value_limit < 10 { 0 } else { MAX_RANGES.min((value_limit - 5) / 5) }
}
fn u16_at(v: &[u8], at: usize) -> u32 {
    u32::from(u16::from_be_bytes([v[at], v[at + 1]]))
}
fn i32_at(v: &[u8], at: usize) -> i32 {
    i32::from_be_bytes(v[at..at + 4].try_into().unwrap())
}
/// None for anything that is not a well-formed `GR1` message (ignored like
/// any other type-2 value). A HELLO is clamped to this build's own limits.
pub fn decode(value: &[u8]) -> Option<Message> {
    if !is_gr1(value) {
        return None;
    }
    match value[3] {
        OP_HELLO => {
            if value.len() != 13 || value[4] != VERSION {
                return None;
            }
            let values = u16_at(value, 5);
            let bytes = i32_at(value, 7) as u32;
            if values == 0 || bytes == 0 {
                return None;
            }
            Some(Message::Hello(Hello {
                values: values.min(HISTORY_VALUES),
                bytes: bytes.min(HISTORY_BYTES),
                ms: u16_at(value, 11).min(HISTORY_MS),
            }))
        }
        OP_NACK => {
            if value.len() < 5 {
                return None;
            }
            let count = usize::from(value[4]);
            if !(1..=MAX_RANGES).contains(&count) || value.len() != 5 + 5 * count {
                return None;
            }
            let ranges: Vec<SeqRange> =
                (0..count).map(|i| SeqRange { first: i32_at(value, 5 + 5 * i), run: u32::from(value[9 + 5 * i]) }).collect();
            if ranges.iter().any(|r| r.run == 0) { None } else { Some(Message::Nack(ranges)) }
        }
        OP_GONE => {
            if value.len() != 9 || value[8] == 0 {
                return None;
            }
            Some(Message::Gone(SeqRange { first: i32_at(value, 4), run: u32::from(value[8]) }))
        }
        OP_HIGH => (value.len() == 8).then(|| Message::High(i32_at(value, 4))),
        _ => None,
    }
}

/// Data values this side sent, byte for byte (own copies, never changed).
struct Entry {
    sequence: i32,
    value: Box<[u8]>,
    sent_ns: u64,
    resends: u32,
}
pub struct ResendHistory {
    entries: VecDeque<Entry>,
    held_bytes: usize,
    last_sent: Option<i32>,
    max_values: usize,
    max_bytes: usize,
    max_age_ns: u64,
}
impl Default for ResendHistory {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            held_bytes: 0,
            last_sent: None,
            max_values: HISTORY_VALUES as usize,
            max_bytes: HISTORY_BYTES as usize,
            max_age_ns: u64::from(HISTORY_MS) * MS,
        }
    }
}
impl ResendHistory {
    pub fn record(&mut self, sequence: i32, value: &[u8], now_ns: u64) {
        self.entries.push_back(Entry { sequence, value: value.into(), sent_ns: now_ns, resends: 0 });
        self.held_bytes += value.len();
        self.last_sent = Some(sequence);
        while self.entries.len() > self.max_values || self.held_bytes > self.max_bytes {
            self.drop_oldest();
        }
        self.prune(now_ns);
    }
    fn prune(&mut self, now_ns: u64) {
        while self.entries.front().is_some_and(|e| now_ns.saturating_sub(e.sent_ns) > self.max_age_ns) {
            self.drop_oldest();
        }
    }
    fn drop_oldest(&mut self) {
        if let Some(e) = self.entries.pop_front() {
            self.held_bytes -= e.value.len();
        }
    }
    fn find(&mut self, sequence: i32, now_ns: u64) -> Option<&mut Entry> {
        self.prune(now_ns);
        let first = self.entries.front()?.sequence;
        let offset = seq_after(sequence, first);
        if offset < 0 || offset as usize >= self.entries.len() {
            return None;
        }
        if self.entries[offset as usize].sequence == sequence {
            return self.entries.get_mut(offset as usize);
        }
        self.entries.iter_mut().find(|e| e.sequence == sequence)
    }
    pub fn held(&self) -> (usize, usize) {
        (self.entries.len(), self.held_bytes)
    }
}

/// Bytes over a rolling one-second window of ten slots.
#[derive(Default)]
struct RollingBytes {
    counts: [u64; 10],
    epochs: [Option<u64>; 10],
}
impl RollingBytes {
    const BUCKET_NS: u64 = SECOND / 10;
    fn add(&mut self, now_ns: u64, bytes: u64) {
        let epoch = now_ns / Self::BUCKET_NS;
        let slot = (epoch % 10) as usize;
        if self.epochs[slot] != Some(epoch) {
            self.epochs[slot] = Some(epoch);
            self.counts[slot] = 0;
        }
        self.counts[slot] += bytes;
    }
    fn sum(&self, now_ns: u64) -> u64 {
        let epoch = now_ns / Self::BUCKET_NS;
        (0..10).filter(|&s| self.epochs[s].is_some_and(|e| e + 10 > epoch && e <= epoch)).map(|s| self.counts[s]).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hello,
    Nack,
    Gone,
    High,
    Resend,
}
pub struct Outbound {
    pub value: Vec<u8>,
    pub kind: Kind,
    pub sequence: i32,
    pub enqueued_ns: u64,
}

struct Inner {
    outbound: VecDeque<Outbound>,
    history: ResendHistory,
    sent_data: RollingBytes,
    resent: RollingBytes,
    out_of_range_ranges: u32,
    nacks_ignored: bool,
    last_data_sent_ns: u64,
    high_sent_for: Option<i32>,
    high_times: VecDeque<u64>,
    hello_sends: u32,
    hello_first_ns: u64,
    peer_hello: Option<Hello>,
    value_limit: usize,
    closed: bool,
    log: VecDeque<String>,
}

/// One GATT connection's recovery (sender history and queue; the reader
/// half lives in the link's `Pipe`). Shared by the pipe (event handlers) and
/// the writer (the worker).
pub struct Recovery {
    link: &'static str,
    base: Instant,
    inner: Mutex<Inner>,
}
impl Recovery {
    pub fn new(link: &'static str) -> Self {
        Self {
            link,
            base: Instant::now(),
            inner: Mutex::new(Inner {
                outbound: VecDeque::new(),
                history: ResendHistory::default(),
                sent_data: RollingBytes::default(),
                resent: RollingBytes::default(),
                out_of_range_ranges: 0,
                nacks_ignored: false,
                last_data_sent_ns: 0,
                high_sent_for: None,
                high_times: VecDeque::new(),
                hello_sends: 0,
                hello_first_ns: 0,
                peer_hello: None,
                value_limit: 0,
                closed: false,
                log: VecDeque::new(),
            }),
        }
    }
    /// None unless the Experimental switch is on: no behaviour change by default.
    pub fn for_link(link: &'static str) -> Option<Arc<Self>> {
        enabled().then(|| Arc::new(Self::new(link)))
    }
    pub fn now_ns(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }
    fn note_locked(&self, inner: &mut Inner, line: String) {
        if inner.log.len() >= 1024 {
            inner.log.pop_front();
        }
        inner.log.push_back(format!("{line} | link={}", self.link));
    }
    pub fn note(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        self.note_locked(&mut inner, line);
    }
    /// `gatt_recovery` lines for the owner's trace.
    pub fn take_log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.drain(..).collect()
    }
    pub fn peer_hello(&self) -> Option<Hello> {
        self.inner.lock().unwrap().peer_hello
    }
    pub fn value_limit(&self) -> usize {
        self.inner.lock().unwrap().value_limit
    }
    pub fn history_state(&self) -> (usize, usize) {
        self.inner.lock().unwrap().history.held()
    }
    /// The link can send (first writer use): announce HELLO.
    pub fn start(&self, value_limit: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.value_limit = value_limit;
        if inner.hello_sends == 0 && !inner.closed {
            self.enqueue_hello(&mut inner);
        }
    }
    fn enqueue_hello(&self, inner: &mut Inner) {
        let now = self.now_ns();
        inner.hello_sends += 1;
        if inner.hello_sends == 1 {
            inner.hello_first_ns = now;
        }
        inner.outbound.push_front(Outbound {
            value: hello(HISTORY_VALUES, HISTORY_BYTES, HISTORY_MS),
            kind: Kind::Hello,
            sequence: -1,
            enqueued_ns: now,
        });
        let n = inner.hello_sends;
        self.note_locked(inner, format!("event=hello_sent | n={n}"));
    }
    /// The peer's HELLO (event handler; no I/O).
    pub fn on_hello(&self, hello: Hello) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        let first = inner.peer_hello.is_none();
        inner.peer_hello = Some(hello);
        if first {
            self.note_locked(
                &mut inner,
                format!("event=hello | historyValues={} | historyBytes={} | historyMs={}", hello.values, hello.bytes, hello.ms),
            );
        }
    }
    /// The reader asks for missing chunks (under the pipe's lock; enqueue only).
    pub fn send_nack(&self, ranges: &[SeqRange]) {
        let now = self.now_ns();
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        inner.outbound.push_front(Outbound { value: nack(ranges), kind: Kind::Nack, sequence: ranges[0].first, enqueued_ns: now });
    }
    /// A data value is about to leave this side (the worker).
    pub fn on_data_sent(&self, sequence: i32, value: &[u8]) {
        let now = self.now_ns();
        let mut inner = self.inner.lock().unwrap();
        inner.history.record(sequence, value, now);
        inner.sent_data.add(now, value.len() as u64);
        inner.last_data_sent_ns = now;
    }
    /// A NACK from the peer (event handler; enqueue only).
    pub fn on_nack(&self, ranges: &[SeqRange]) {
        let now = self.now_ns();
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        if inner.closed || inner.nacks_ignored {
            return;
        }
        let budget = (inner.sent_data.sum(now) * RESEND_BUDGET_PERCENT / 100).max(RESEND_FLOOR_BYTES_PER_SECOND);
        let (mut queued, mut gone_seqs, mut capped, mut exhausted) = (Vec::new(), Vec::new(), 0, 0);
        for range in ranges {
            let mut out_of_range = false;
            for index in 0..range.run {
                let sequence = range.first.wrapping_add(index as i32);
                match inner.history.last_sent {
                    Some(last) if seq_after(sequence, last) <= 0 => {}
                    _ => {
                        out_of_range = true;
                        continue;
                    }
                }
                let resent_now = inner.resent.sum(now);
                let Some(entry) = inner.history.find(sequence, now) else {
                    gone_seqs.push(sequence);
                    continue;
                };
                if entry.resends >= RESENDS_PER_SEQUENCE {
                    exhausted += 1;
                } else if resent_now + entry.value.len() as u64 > budget {
                    capped += 1;
                } else {
                    entry.resends += 1;
                    let value = entry.value.to_vec();
                    inner.resent.add(now, value.len() as u64);
                    queued.push(Outbound { value, kind: Kind::Resend, sequence, enqueued_ns: now });
                }
            }
            if out_of_range {
                inner.out_of_range_ranges += 1;
                if inner.out_of_range_ranges > MAX_OUT_OF_RANGE_NACK_RANGES {
                    inner.nacks_ignored = true;
                    break;
                }
            }
        }
        let queued_count = queued.len();
        inner.outbound.extend(queued);
        for range in coalesce(&gone_seqs) {
            inner.outbound.push_front(Outbound { value: gone(range), kind: Kind::Gone, sequence: range.first, enqueued_ns: now });
        }
        self.note_locked(
            inner,
            format!(
                "event=nack_received | seq={} | ranges={} | queued={queued_count} | gone={} | exhausted={exhausted}",
                ranges[0].first,
                ranges.len(),
                gone_seqs.len()
            ),
        );
        if capped > 0 {
            self.note_locked(inner, format!("event=capped | values={capped} | budgetBytesPerSecond={budget}"));
        }
        if inner.nacks_ignored {
            let n = inner.out_of_range_ranges;
            self.note_locked(inner, format!("event=nacks_ignored | reason=out_of_range | ranges={n}"));
        }
    }
    /// Timers: the second HELLO and the idle HIGH probe (the worker).
    pub fn tick(&self) {
        let now = self.now_ns();
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return;
        }
        if inner.hello_sends == 1 && now - inner.hello_first_ns >= HELLO_REPEAT_MS * MS {
            self.enqueue_hello(&mut inner);
        }
        if let Some(last) = inner.history.last_sent {
            if inner.peer_hello.is_some() && inner.high_sent_for != Some(last) && now - inner.last_data_sent_ns >= HIGH_IDLE_MS * MS {
                while inner.high_times.front().is_some_and(|&t| now - t >= SECOND) {
                    inner.high_times.pop_front();
                }
                if inner.high_times.len() < HIGH_PER_SECOND {
                    inner.high_times.push_back(now);
                    inner.high_sent_for = Some(last);
                    inner.outbound.push_back(Outbound { value: high(last), kind: Kind::High, sequence: last, enqueued_ns: now });
                }
            }
        }
    }
    /// The next value to send ahead of new data.
    pub fn pop(&self) -> Option<Outbound> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return None;
        }
        inner.outbound.pop_front()
    }
    /// After a queued value left (the worker): its log line.
    pub fn sent(&self, item: &Outbound, started_ns: u64) {
        match item.kind {
            Kind::Resend => self.note(format!(
                "event=resend | seq={} | bytes={} | queuedAheadMs={}",
                item.sequence,
                item.value.len(),
                started_ns.saturating_sub(item.enqueued_ns) / MS
            )),
            Kind::Gone => self.note(format!("event=gone_sent | seq={}", item.sequence)),
            Kind::High => self.note(format!("event=high_sent | seq={}", item.sequence)),
            Kind::Hello | Kind::Nack => {}
        }
    }
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.outbound.clear();
    }
}

fn coalesce(sequences: &[i32]) -> Vec<SeqRange> {
    let mut ranges: Vec<SeqRange> = Vec::new();
    for &sequence in sequences {
        match ranges.last_mut() {
            Some(last) if seq_after(sequence, last.last()) == 1 && last.run < MAX_RUN => last.run += 1,
            _ => ranges.push(SeqRange { first: sequence, run: 1 }),
        }
    }
    ranges
}

/// Why a reader ended the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lost {
    /// More held than the peer's history: it can no longer have the chunk.
    History { missing: i32, parked: u32, bytes: u32 },
    /// Still missing at the deadline.
    Deadline { missing: i32, requests: u32, held_ms: u32 },
    /// The peer answered GONE.
    Gone { missing: i32 },
}

struct Hole {
    first: i32,
    last: i32,
    first_missing_ns: u64,
    requests: u32,
    last_request_ns: u64,
}

/// The reader half, owned by a `Pipe` (called under its lock). Missing
/// chunks are ranges, only within min(peer history, 1,024) of the next
/// expected sequence.
#[derive(Default)]
pub struct Reader {
    holes: Vec<Hole>,
    high_heard: Option<i32>,
    nack_times: VecDeque<u64>,
}
/// What one reader update asks the pipe to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Update {
    pub failure: Option<Lost>,
    pub nack: Option<Vec<SeqRange>>,
    pub notes: Vec<String>,
}
impl Reader {
    fn limit(peer: Hello) -> i32 {
        peer.values.min(HISTORY_VALUES) as i32
    }
    /// A HIGH from the sender; true when it may reveal a missing tail.
    pub fn on_high(&mut self, peer: Hello, sequence: i32, next: i32, notes: &mut Vec<String>) -> bool {
        let distance = seq_after(sequence, next);
        if distance < 0 {
            return false;
        }
        if distance >= Self::limit(peer) {
            notes.push(format!("event=ignored | reason=high_out_of_range | seq={sequence} | next={next}"));
            return false;
        }
        if self.high_heard.is_none_or(|heard| seq_after(sequence, heard) > 0) {
            self.high_heard = Some(sequence);
        }
        true
    }
    /// The first missing sequence a GONE covers, or None.
    pub fn gone_hit(&self, range: SeqRange) -> Option<i32> {
        let hole = self.holes.iter().find(|h| range.overlaps(h.first, h.last))?;
        Some(if seq_after(range.first, hole.first) > 0 { range.first } else { hole.first })
    }
    /// Re-evaluates the missing ranges and the requests due. `pending`: the
    /// held sequences in ascending order.
    pub fn update(
        &mut self,
        peer: Hello,
        value_limit: usize,
        next: i32,
        pending: &[i32],
        parked_bytes: usize,
        now_ns: u64,
    ) -> Update {
        let mut out = Update::default();
        let limit = Self::limit(peer);
        let parked = pending.len();
        let farthest = pending.last().map_or(-1, |&p| seq_after(p, next));
        if parked > 0 && (farthest >= limit || parked >= limit as usize || parked_bytes > peer.bytes as usize) {
            out.failure = Some(Lost::History { missing: next, parked: parked as u32, bytes: parked_bytes as u32 });
            return out;
        }
        if self.high_heard.is_some_and(|h| seq_after(h, next) < 0) {
            self.high_heard = None;
        }
        let mut fresh = Vec::new();
        let mut expected = next;
        for &key in pending {
            if seq_after(key, expected) > 0 {
                fresh.push((expected, key.wrapping_sub(1)));
            }
            expected = key.wrapping_add(1);
        }
        if let Some(heard) = self.high_heard {
            if seq_after(heard, expected) >= 0 {
                fresh.push((expected, heard));
            }
        }
        let max_sequence = next.wrapping_add(limit - 1);
        let old = std::mem::take(&mut self.holes);
        for (first, last) in fresh {
            if seq_after(first, next) >= limit {
                continue;
            }
            let clipped = if seq_after(last, max_sequence) > 0 { max_sequence } else { last };
            let prior = old.iter().find(|h| seq_after(clipped, h.first) >= 0 && seq_after(h.last, first) >= 0);
            self.holes.push(Hole {
                first,
                last: clipped,
                first_missing_ns: prior.map_or(now_ns, |p| p.first_missing_ns),
                requests: prior.map_or(0, |p| p.requests),
                last_request_ns: prior.map_or(0, |p| p.last_request_ns),
            });
        }
        for filled in &old {
            if !self.holes.iter().any(|h| seq_after(h.last, filled.first) >= 0 && seq_after(filled.last, h.first) >= 0) {
                out.notes.push(format!(
                    "event=recovered | seq={} | count={} | waitedMs={} | rounds={}",
                    filled.first,
                    seq_after(filled.last, filled.first) + 1,
                    now_ns.saturating_sub(filled.first_missing_ns) / MS,
                    filled.requests
                ));
            }
        }
        if let Some(hole) = self.holes.iter().find(|h| now_ns.saturating_sub(h.first_missing_ns) >= DEADLINE_MS * MS) {
            out.failure = Some(Lost::Deadline {
                missing: hole.first,
                requests: hole.requests,
                held_ms: (now_ns.saturating_sub(hole.first_missing_ns) / MS) as u32,
            });
            return out;
        }
        self.request_due(value_limit, now_ns, &mut out);
        out
    }
    fn request_due(&mut self, value_limit: usize, now_ns: u64, out: &mut Update) {
        let due: Vec<usize> = (0..self.holes.len())
            .filter(|&i| {
                let h = &self.holes[i];
                h.requests == 0 || (h.requests < MAX_REQUESTS && now_ns.saturating_sub(h.last_request_ns) >= ROUND_MS * MS)
            })
            .collect();
        if due.is_empty() {
            return;
        }
        let fit = nack_ranges_fitting(value_limit);
        if fit == 0 {
            return;
        }
        while self.nack_times.front().is_some_and(|&t| now_ns.saturating_sub(t) >= SECOND) {
            self.nack_times.pop_front();
        }
        if self.nack_times.len() >= READER_NACKS_PER_SECOND {
            return;
        }
        let mut ranges = Vec::new();
        let mut asked = Vec::new();
        for i in due {
            if ranges.len() >= fit {
                break;
            }
            let hole = &self.holes[i];
            let mut start = hole.first;
            let mut left = seq_after(hole.last, hole.first) as u32 + 1;
            while left > 0 && ranges.len() < fit {
                let run = left.min(MAX_RUN);
                ranges.push(SeqRange { first: start, run });
                start = start.wrapping_add(run as i32);
                left -= run;
            }
            asked.push(i);
        }
        let mut round = 0;
        for i in asked {
            let hole = &mut self.holes[i];
            hole.requests += 1;
            hole.last_request_ns = now_ns;
            round = round.max(hole.requests);
        }
        self.nack_times.push_back(now_ns);
        out.notes.push(format!("event=nack | seq={} | ranges={} | round={round}", ranges[0].first, ranges.len()));
        out.nack = Some(ranges);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PEER: Hello = Hello { values: HISTORY_VALUES, bytes: HISTORY_BYTES, ms: HISTORY_MS };

    #[test]
    fn recovery_is_on_unless_switched_off() {
        assert!(switch(None) && switch(Some("1")) && switch(Some("")));
        assert!(!switch(Some("0")));
    }

    #[test]
    fn messages_round_trip_and_malformed_ones_are_ignored() {
        assert_eq!(hello(1024, 262_144, 2000), [2, 0x47, 0x52, 1, 1, 0x04, 0x00, 0x00, 0x04, 0x00, 0x00, 0x07, 0xd0]);
        assert_eq!(decode(&hello(1024, 262_144, 2000)), Some(Message::Hello(PEER)));
        // A larger claim is clamped; 0 values or 0 bytes is ignored.
        assert_eq!(decode(&hello(4000, 9_000_000, 60_000)), Some(Message::Hello(PEER)));
        assert_eq!(decode(&hello(0, 1, 1)), None);
        assert_eq!(decode(&hello(1, 0, 1)), None);
        let mut v2 = hello(1, 1, 1);
        v2[4] = 2;
        assert_eq!(decode(&v2), None);
        let ranges = vec![SeqRange { first: 7, run: 1 }, SeqRange { first: -2, run: 255 }];
        assert_eq!(nack(&ranges)[..10], [2, 0x47, 0x52, 2, 2, 0, 0, 0, 7, 1]);
        assert_eq!(decode(&nack(&ranges)), Some(Message::Nack(ranges)));
        assert_eq!(decode(&[2, 0x47, 0x52, 2, 0]), None, "count 0");
        assert_eq!(decode(&[2, 0x47, 0x52, 2, 1, 0, 0, 0, 1, 0]), None, "run 0");
        assert_eq!(decode(&[2, 0x47, 0x52, 2, 1, 0, 0, 0, 1]), None, "short");
        assert_eq!(decode(&gone(SeqRange { first: 9, run: 3 })), Some(Message::Gone(SeqRange { first: 9, run: 3 })));
        assert_eq!(decode(&high(-5)), Some(Message::High(-5)));
        assert_eq!(decode(&[2, 0x47, 0x52, 9, 0, 0, 0, 0]), None, "unknown op");
        assert_eq!(decode(&[2, 7]), None);
        assert_eq!(nack_ranges_fitting(20), 3);
        assert_eq!(nack_ranges_fitting(512), 8);
        assert_eq!(nack_ranges_fitting(0), 0);
    }

    #[test]
    fn history_limits_by_count_bytes_and_age() {
        let mut h = ResendHistory { max_values: 3, max_bytes: 10, ..ResendHistory::default() };
        for s in 0..5 {
            h.record(s, &[s as u8; 3], 0);
        }
        assert_eq!(h.held(), (3, 9));
        assert!(h.find(1, 0).is_none() && h.find(4, 0).is_some());
        h.record(5, &[0; 9], 0);
        assert_eq!(h.held(), (1, 9), "bytes bound drops the oldest");
        assert!(h.find(5, u64::from(HISTORY_MS) * MS + 1).is_none(), "older than 2 s");
    }

    #[test]
    fn a_nack_resends_the_stored_bytes_and_answers_gone_and_limits_hold() {
        let r = Recovery::new("server");
        for s in 0..4 {
            r.on_data_sent(s, &[1, 0, 0, 0, s as u8, 0xAA]);
        }
        r.on_nack(&[SeqRange { first: 2, run: 1 }]);
        let item = r.pop().unwrap();
        assert_eq!((item.kind, item.value.as_slice()), (Kind::Resend, &[1, 0, 0, 0, 2, 0xAA][..]));
        // Each sequence is resent at most three times.
        for _ in 0..5 {
            r.on_nack(&[SeqRange { first: 2, run: 1 }]);
        }
        let mut resends = 0;
        while let Some(i) = r.pop() {
            resends += usize::from(i.kind == Kind::Resend);
        }
        assert_eq!(resends, 2);
        // Above the last sequence sent: skipped; after 8 such ranges NACKs are ignored.
        for _ in 0..9 {
            r.on_nack(&[SeqRange { first: 50, run: 1 }]);
        }
        r.on_nack(&[SeqRange { first: 1, run: 1 }]);
        assert!(r.pop().is_none(), "NACKs ignored for the rest of the connection");
        assert!(r.take_log().iter().any(|l| l.starts_with("event=nacks_ignored")));
    }

    #[test]
    fn resends_are_capped_at_a_quarter_of_the_bytes_sent_with_a_floor() {
        let r = Recovery::new("client");
        for s in 0..40 {
            r.on_data_sent(s, &[0; 500]);
        }
        // 20,000 bytes sent: the budget is 5,000 bytes, so 10 resends of 500.
        r.on_nack(&[SeqRange { first: 0, run: 40 }]);
        let mut resends = 0;
        while let Some(i) = r.pop() {
            resends += usize::from(i.kind == Kind::Resend);
        }
        assert_eq!(resends, 10);
        assert!(r.take_log().iter().any(|l| l.contains("event=capped | values=30 | budgetBytesPerSecond=5000")));
        let quiet = Recovery::new("client");
        quiet.on_data_sent(0, &[0; 600]);
        quiet.on_data_sent(1, &[0; 600]);
        quiet.on_nack(&[SeqRange { first: 0, run: 2 }]);
        let mut n = 0;
        while quiet.pop().is_some() {
            n += 1;
        }
        assert_eq!(n, 1, "the 1,024 B/s floor lets one 600-byte value through");
    }

    #[test]
    fn the_reader_asks_rounds_at_250_ms_and_fails_at_the_deadline() {
        let mut reader = Reader::default();
        let u = reader.update(PEER, 512, 5, &[7, 8], 2, 0);
        assert_eq!(u.nack, Some(vec![SeqRange { first: 5, run: 2 }]));
        assert_eq!(reader.update(PEER, 512, 5, &[7, 8, 9], 3, 100 * MS).nack, None, "not again within 250 ms");
        assert!(reader.update(PEER, 512, 5, &[7, 8, 9], 3, 250 * MS).nack.is_some());
        assert!(reader.update(PEER, 512, 5, &[7, 8, 9], 3, 500 * MS).nack.is_some());
        assert_eq!(reader.update(PEER, 512, 5, &[7, 8, 9], 3, 750 * MS).nack, None, "three requests at most");
        let end = reader.update(PEER, 512, 5, &[7, 8, 9], 3, 1000 * MS);
        assert_eq!(end.failure, Some(Lost::Deadline { missing: 5, requests: 3, held_ms: 1000 }));
    }

    #[test]
    fn a_filled_hole_is_recovered_and_a_tail_loss_is_found_by_high() {
        let mut reader = Reader::default();
        reader.update(PEER, 512, 5, &[6], 1, 0);
        let u = reader.update(PEER, 512, 7, &[], 0, 40 * MS);
        assert_eq!(u.notes, vec!["event=recovered | seq=5 | count=1 | waitedMs=40 | rounds=1".to_owned()]);
        let mut notes = Vec::new();
        assert!(reader.on_high(PEER, 9, 7, &mut notes));
        assert_eq!(reader.update(PEER, 512, 7, &[], 0, 50 * MS).nack, Some(vec![SeqRange { first: 7, run: 3 }]));
        assert!(!reader.on_high(PEER, 7 + 1024, 7, &mut notes), "beyond the history: ignored");
        assert!(notes[0].starts_with("event=ignored | reason=high_out_of_range"));
        assert_eq!(reader.gone_hit(SeqRange { first: 8, run: 1 }), Some(8));
        assert_eq!(reader.gone_hit(SeqRange { first: 20, run: 1 }), None);
    }

    #[test]
    fn more_held_than_the_peer_history_fails_at_once() {
        let mut reader = Reader::default();
        let small = Hello { values: 4, bytes: 100, ms: 2000 };
        let pending: Vec<i32> = (1..=4).collect();
        assert_eq!(reader.update(small, 512, 0, &pending, 8, 0).failure, Some(Lost::History { missing: 0, parked: 4, bytes: 8 }));
        assert_eq!(reader.update(small, 512, 0, &[1], 101, 0).failure, Some(Lost::History { missing: 0, parked: 1, bytes: 101 }));
    }

    #[test]
    fn sequence_arithmetic_is_modulo_two_to_the_32() {
        assert_eq!(seq_after(i32::MIN, i32::MAX), 1);
        assert_eq!(SeqRange { first: i32::MAX, run: 2 }.last(), i32::MIN);
        assert_eq!(coalesce(&[i32::MAX, i32::MIN, 3]), vec![SeqRange { first: i32::MAX, run: 2 }, SeqRange { first: 3, run: 1 }]);
    }
}

#[cfg(test)]
mod vectors {
    use super::*;
    use serde_json::Value;
    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }
    /// The shared `gr1_v1` vectors (Android replays the same file).
    #[test]
    fn shared_gr1_v1_vectors_decode_and_reencode() {
        let doc: Value = serde_json::from_str(include_str!("../../tests/vectors/gr1_v1.json")).unwrap();
        let c = &doc["constants"];
        assert_eq!(c["historyValues"], HISTORY_VALUES);
        assert_eq!(c["historyBytes"], HISTORY_BYTES);
        assert_eq!(c["historyMs"], HISTORY_MS);
        assert_eq!(c["deadlineMs"], DEADLINE_MS);
        assert_eq!(c["roundMs"], ROUND_MS);
        assert_eq!(c["resendBudgetPercent"], RESEND_BUDGET_PERCENT);
        assert_eq!(c["resendFloorBytesPerSecond"], RESEND_FLOOR_BYTES_PER_SECOND);
        assert_eq!(c["gapFastFailMs"], crate::bluetooth::carrier::GAP_MS);
        assert_eq!(c["gapFastFailChunks"], crate::bluetooth::carrier::GAP_CHUNKS);
        for pair in doc["nackRangesFitting"].as_array().unwrap() {
            assert_eq!(nack_ranges_fitting(pair[0].as_u64().unwrap() as usize), pair[1].as_u64().unwrap() as usize);
        }
        for v in doc["valid"].as_array().unwrap() {
            let raw = bytes(v["hex"].as_str().unwrap());
            let name = v["name"].as_str().unwrap();
            let decoded = decode(&raw).unwrap_or_else(|| panic!("{name}"));
            match (v["op"].as_str().unwrap(), decoded) {
                ("hello", Message::Hello(h)) => {
                    assert_eq!((h.values, h.bytes, h.ms), (
                        v["historyValues"].as_u64().unwrap() as u32,
                        v["historyBytes"].as_u64().unwrap() as u32,
                        v["historyMs"].as_u64().unwrap() as u32
                    ), "{name}");
                    if name == "hello_default" {
                        assert_eq!(hello(h.values, h.bytes, h.ms), raw);
                    }
                }
                ("nack", Message::Nack(ranges)) => {
                    let want: Vec<SeqRange> = v["ranges"].as_array().unwrap().iter()
                        .map(|r| SeqRange { first: r[0].as_i64().unwrap() as i32, run: r[1].as_u64().unwrap() as u32 }).collect();
                    assert_eq!(ranges, want, "{name}");
                    assert_eq!(nack(&ranges), raw, "{name}");
                }
                ("gone", Message::Gone(range)) => {
                    assert_eq!((range.first, range.run), (v["first"].as_i64().unwrap() as i32, v["run"].as_u64().unwrap() as u32));
                    assert_eq!(gone(range), raw);
                }
                ("high", Message::High(sequence)) => {
                    assert_eq!(sequence, v["sequence"].as_i64().unwrap() as i32);
                    assert_eq!(high(sequence), raw);
                }
                (op, other) => panic!("{name}: {op} decoded as {other:?}"),
            }
        }
        for v in doc["ignored"].as_array().unwrap() {
            assert_eq!(decode(&bytes(v["hex"].as_str().unwrap())), None, "{}", v["name"]);
        }
    }
}
