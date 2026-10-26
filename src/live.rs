//! Going live: the source side of a stream with Android's performance rules.
//! Encoded once by the (external) encoder, every packet is published to all
//! admitted children through the fan-out; each child link is paced by its own
//! `LinkPacer` (seeded from the GATT prior, then the child's reports), only an
//! audio record at the head skips the pacer, a child that stays behind is
//! withdrawn (LOCAL_CAPACITY) without slowing the encoder, and admission stays
//! inside the measured budget. Pause clears queued media and repeats PAUSED;
//! resume queues LIVE before media; End clears media and queues ENDED, then
//! waits up to 5 s for acknowledgements. Two ordinary Opus units are paired
//! into one BTA1 record like Android. Signed radio preservation and its distinct
//! queue/health policy are opt-in; radio is never repacked in those arms.
//! Capture and encoding are outside.
use crate::{
    bluetooth::pacer::{self, LinkPacer},
    fanout::{Admission, ChildHealth, FanOut, Item, Offer, RadioHealth},
    link::Liveness,
    media::{self, AUDIO, AUDIO_BATCH},
    session::{self, Control, SourceLink},
};

pub const LIVE: u8 = 1;
pub const PAUSED: u8 = 2;
pub const ENDED: u8 = 3;
pub const PAUSED_REPEAT_NS: u64 = 2_000_000_000;
pub const END_GRACE_NS: u64 = 5_000_000_000;
pub const WITHDRAW_ACK_NS: u64 = 4_000_000_000;
const MAX_AUDIO_PAIR_GAP_US: i64 = 1_000_000;
const MAX_AUDIO_UNIT: usize = 16 * 1024;
pub fn signed_radio_enabled() -> bool { std::env::var("MESHLIVE_RADIO_SIGNED_RECORDS").as_deref() == Ok("1") }
pub fn radio_queue_enabled() -> bool { std::env::var("MESHLIVE_RADIO_QUEUE_PARITY").as_deref() == Ok("1") }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotAdmitted,
    UnknownChild,
    Ended,
    Media(media::Error),
    Session(session::Error),
}
impl From<session::Error> for Error {
    fn from(error: session::Error) -> Self {
        Self::Session(error)
    }
}

/// `BTA1` | delta (4) | len (2) | unit | len (2) | unit, flags AUDIO|AUDIO_BATCH.
pub fn pair_audio(first: media::Packet<'_>, second: media::Packet<'_>) -> Result<Vec<u8>, Error> {
    let delta = second.pts_us - first.pts_us;
    if first.flags != AUDIO
        || second.flags != AUDIO
        || first.payload.len() > MAX_AUDIO_UNIT
        || second.payload.len() > MAX_AUDIO_UNIT
        || !(0..=MAX_AUDIO_PAIR_GAP_US).contains(&delta)
    {
        return Err(Error::Media(media::Error::Audio));
    }
    let mut payload = Vec::with_capacity(12 + first.payload.len() + second.payload.len());
    payload.extend_from_slice(b"BTA1");
    payload.extend_from_slice(&(delta as i32).to_be_bytes());
    for unit in [first.payload, second.payload] {
        payload.extend_from_slice(&(unit.len() as u16).to_be_bytes());
        payload.extend_from_slice(unit);
    }
    media::Packet {
        flags: AUDIO | AUDIO_BATCH,
        pts_us: first.pts_us,
        payload: &payload,
    }
    .encode()
    .map_err(Error::Media)
}

/// What the owner should do for one child now.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing queued.
    Idle,
    /// The pacer holds the head (video) record this long.
    Wait(u64),
    /// Write these bytes, then report the write start/end to `written`.
    Write(Vec<u8>),
    /// Write this withdrawal, wait for its acknowledgement (at most 4 s), then close.
    Withdraw(Vec<u8>),
    /// Close this child's link now (dead, withdrawn, or ended).
    Close,
}

struct Child<K> {
    key: K,
    link: SourceLink,
    pacer: LinkPacer,
    health: ChildHealth,
    radio_health: Option<RadioHealth>,
    radio_overloaded: bool,
    liveness: Liveness,
    withdrawn_at: Option<u64>,
    ended_ack: bool,
}

pub struct Source<K> {
    fan: FanOut<K>,
    children: Vec<Child<K>>,
    admission: Admission<K>,
    header: Item,
    state: u8,
    media_state: u8,
    ended_at: Option<u64>,
    last_paused_ns: u64,
    /// A child joined or the stream resumed: a keyframe now (never delayed).
    join_keyframe: bool,
    /// A viewer asked for recovery: subject to the encoder's request backoff.
    recovery_keyframe: bool,
    pending_audio: Option<(i64, Vec<u8>)>,
    audio_muted: bool,
    preserve_signed_audio: bool,
    radio_queue: bool,
    radio_age_shed: bool,
    radio_shed: u64,
}

/// Bounded queue facts; no media, keys or peer identifiers.
#[derive(Debug, Clone, Copy)]
pub struct QueueSnapshot {
    pub records: usize,
    pub bytes: usize,
    pub age_ms: u64,
    pub head_audio: bool,
    pub pace_wait_ns: u64,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyframeRequests {
    pub join: bool,
    pub recovery: bool,
}

impl<K: PartialEq + Clone> Source<K> {
    /// `header` is the encoded BTV3/BTL1 stream header; `budget_kbps` the
    /// measured aggregate this source may admit.
    pub fn new(header: Vec<u8>, budget_kbps: u32) -> Option<Self> {
        let radio_queue = header.starts_with(b"BTL1") && radio_queue_enabled();
        let preserve_signed_audio = header.starts_with(b"BTL1") && (signed_radio_enabled() || radio_queue);
        let header = Item::header(header);
        let mut fan = FanOut::default();
        fan.set_preamble(vec![header.clone()]);
        Some(Self {
            fan,
            children: Vec::new(),
            admission: Admission::new(budget_kbps)?,
            header,
            state: LIVE,
            media_state: LIVE,
            ended_at: None,
            last_paused_ns: 0,
            join_keyframe: false,
            recovery_keyframe: false,
            pending_audio: None,
            audio_muted: false,
            preserve_signed_audio,
            radio_queue,
            radio_age_shed: std::env::var("MESHLIVE_RADIO_AGE_SHED").as_deref() != Ok("0"),
            radio_shed: 0,
        })
    }
    fn refresh_preamble(&mut self) {
        let mut preamble = vec![self.header.clone()];
        let state = if self.state == LIVE { self.media_state } else { self.state };
        if state != LIVE {
            preamble.push(Item::lifecycle(state));
        }
        self.fan.set_preamble(preamble);
    }
    /// Admits an authenticated child inside the budget. It starts at the next
    /// keyframe; the encoder is asked for one.
    pub fn admit(
        &mut self,
        key: K,
        link: SourceLink,
        expected_kbps: u32,
        measured_link_kbps: Option<u32>,
        now_ns: u64,
    ) -> Result<(), Error> {
        if self.ended_at.is_some() {
            return Err(Error::Ended);
        }
        let decision = self
            .admission
            .try_admit(key.clone(), expected_kbps)
            .ok_or(Error::NotAdmitted)?;
        if !decision.admitted {
            return Err(Error::NotAdmitted);
        }
        self.children.retain(|c| c.key != key);
        let mut pacer = LinkPacer::new();
        pacer.seed(now_ns, pacer::seed_kbps(measured_link_kbps, None));
        self.children.push(Child {
            key: key.clone(),
            link,
            pacer,
            health: ChildHealth::default(),
            radio_health: self.radio_queue.then(RadioHealth::default),
            radio_overloaded: false,
            liveness: Liveness::new(now_ns),
            withdrawn_at: None,
            ended_ack: false,
        });
        self.join_keyframe |= self.fan.add_child(key, now_ns);
        Ok(())
    }
    pub fn children(&self) -> usize {
        self.children.len()
    }
    /// Whole source records removed across listener queues since the last
    /// observation. The owner logs this count without media or peer IDs.
    pub fn take_radio_shed(&mut self) -> u64 { std::mem::take(&mut self.radio_shed) }
    pub fn queue_snapshot(&mut self, key: &K, now_ns: u64) -> Option<QueueSnapshot> {
        let child = self.children.iter().find(|c| c.key == *key)?;
        let queue = self.fan.queue(key)?;
        Some(QueueSnapshot { records: queue.len(), bytes: queue.queued_bytes(),
            age_ms: queue.backlog_age_ms(now_ns), head_audio: queue.head_is_audio(),
            pace_wait_ns: child.pacer.wait_ns(now_ns) })
    }
    /// True once per request: the encoder should emit a keyframe.
    pub fn take_keyframe_request(&mut self) -> bool {
        let requests = self.take_keyframe_requests();
        requests.join || requests.recovery
    }
    /// The pending requests split by kind, cleared: joins (and resume) must be
    /// served at once; viewer recovery requests go through the backoff.
    pub fn take_keyframe_requests(&mut self) -> KeyframeRequests {
        KeyframeRequests {
            join: std::mem::take(&mut self.join_keyframe),
            recovery: std::mem::take(&mut self.recovery_keyframe),
        }
    }
    /// One encoded packet from the encoder (codec config, video or audio).
    pub fn publish(&mut self, packet: Vec<u8>, now_ns: u64) -> Result<(), Error> {
        if self.ended_at.is_some() {
            return Err(Error::Ended);
        }
        let parsed = media::Packet::decode(&packet, media::MAX_PAYLOAD).map_err(Error::Media)?;
        // Paused: only configuration is still tracked for later joiners.
        if self.state == PAUSED && parsed.flags & media::CONFIG == 0 {
            return Ok(());
        }
        if self.audio_muted && parsed.flags & AUDIO != 0 && parsed.flags & media::CONFIG == 0 {
            return Ok(());
        }
        // BTL1 signs flags, timestamp, sequence and body before fan-out.
        // Pairing an already signed record would invalidate that signature.
        if parsed.flags == AUDIO && !self.preserve_signed_audio {
            let Some((pts, first)) = self.pending_audio.take() else {
                self.pending_audio = Some((parsed.pts_us, parsed.payload.to_vec()));
                return Ok(());
            };
            let first = media::Packet {
                flags: AUDIO,
                pts_us: pts,
                payload: &first,
            };
            let paired = pair_audio(first, parsed)?;
            self.fan.publish(Item::media(paired).unwrap(), now_ns);
            return Ok(());
        }
        let item = Item::media(packet).ok_or(Error::Media(media::Error::Size))?;
        let audio = item.radio_source_audio();
        let offered = self.fan.publish(item, now_ns);
        if self.radio_queue {
            for (key, offer) in offered {
                if !matches!(offer, Offer::Queued | Offer::StartedAtKeyFrame) { continue; }
                let child = self.children.iter_mut().find(|c| c.key == key).unwrap();
                let queue = self.fan.queue(&key).unwrap();
                let shed = if self.radio_age_shed { queue.shed_radio(now_ns) } else { 0 };
                self.radio_shed += shed as u64;
                let health = child.radio_health.as_mut().unwrap();
                health.observe(audio, shed, now_ns / 1_000_000);
                if health.should_withdraw(queue.backlog_age_ms(now_ns), queue.queued_bytes(), now_ns / 1_000_000) {
                    child.radio_overloaded = true;
                    queue.close();
                }
            }
        }
        Ok(())
    }
    fn flush_audio(&mut self, now_ns: u64) {
        if let Some((pts, unit)) = self.pending_audio.take() {
            let packet = media::Packet {
                flags: AUDIO,
                pts_us: pts,
                payload: &unit,
            }
            .encode()
            .unwrap();
            self.fan.publish(Item::media(packet).unwrap(), now_ns);
        }
    }
    /// Video-source mute: finish the pre-mute unit in order, then suppress
    /// microphone media while retaining configuration. Radio owns its mute.
    pub fn mute_audio(&mut self, muted: bool, now_ns: u64) {
        if self.ended_at.is_some() || self.audio_muted == muted {
            return;
        }
        if muted {
            self.flush_audio(now_ns);
        }
        self.audio_muted = muted;
    }
    /// Radio owns signed silence; Mute also removes already signed records
    /// that have not left a child's queue. In-flight Bluetooth bytes remain.
    pub fn discard_radio_audio(&mut self) {
        if self.ended_at.is_some() || !self.header.bytes.starts_with(b"BTL1") { return; }
        let keys: Vec<K> = self.children.iter().map(|c| c.key.clone()).collect();
        for key in keys {
            if let Some(queue) = self.fan.queue(&key) { queue.clear_audio(); }
        }
    }
    /// Camera off removes video not yet sealed for a child. A current write
    /// finishes before the source worker can apply this transition.
    pub fn discard_video(&mut self) {
        if self.ended_at.is_some() || !self.header.bytes.starts_with(b"BTV3") { return; }
        let keys: Vec<K> = self.children.iter().map(|c| c.key.clone()).collect();
        for key in keys { if let Some(queue) = self.fan.queue(&key) { queue.clear_video(); } }
        self.join_keyframe = true;
    }
    /// Optional video-source notices (Android live_media_state). The owner
    /// calls this only with the switch on; capture/mute still work without it.
    pub fn set_media_state(&mut self, muted: bool, camera_off: bool, now_ns: u64) {
        if self.ended_at.is_some() || !self.header.bytes.starts_with(b"BTV3") {
            return;
        }
        let state = match (muted, camera_off) { (false, false) => LIVE, (true, false) => 4, (false, true) => 5, (true, true) => 6 };
        if self.media_state == state { return; }
        self.media_state = state;
        if self.state == LIVE { self.fan.publish(Item::lifecycle(state), now_ns); }
        self.refresh_preamble();
    }
    fn clear_media(&mut self) {
        let keys: Vec<K> = self.children.iter().map(|c| c.key.clone()).collect();
        for key in keys {
            if let Some(queue) = self.fan.queue(&key) {
                queue.clear_media();
            }
        }
    }
    pub fn pause(&mut self, now_ns: u64) {
        if self.state != LIVE || self.ended_at.is_some() {
            return;
        }
        self.flush_audio(now_ns);
        self.clear_media();
        self.state = PAUSED;
        self.last_paused_ns = now_ns;
        self.fan.publish(Item::lifecycle(PAUSED), now_ns);
        self.refresh_preamble();
    }
    pub fn resume(&mut self, now_ns: u64) {
        if self.state != PAUSED || self.ended_at.is_some() {
            return;
        }
        self.state = LIVE;
        self.fan.publish(Item::lifecycle(LIVE), now_ns);
        if self.media_state != LIVE { self.fan.publish(Item::lifecycle(self.media_state), now_ns); }
        self.refresh_preamble();
        self.join_keyframe = true;
    }
    pub fn end(&mut self, now_ns: u64) {
        if self.ended_at.is_some() {
            return;
        }
        self.pending_audio = None;
        self.clear_media();
        self.state = ENDED;
        self.ended_at = Some(now_ns);
        self.fan.publish(Item::lifecycle(ENDED), now_ns);
    }
    /// Periodic housekeeping: repeat PAUSED every 2 s.
    pub fn tick(&mut self, now_ns: u64) {
        if self.state == PAUSED && now_ns.saturating_sub(self.last_paused_ns) >= PAUSED_REPEAT_NS {
            self.last_paused_ns = now_ns;
            self.fan.publish(Item::lifecycle(PAUSED), now_ns);
        }
    }
    /// One authenticated control record from a child.
    pub fn on_control(&mut self, key: &K, control: Control, now_ns: u64) -> Result<(), Error> {
        let child = self
            .children
            .iter_mut()
            .find(|c| c.key == *key)
            .ok_or(Error::UnknownChild)?;
        child.liveness.on_received(now_ns);
        match control {
            // Android: delivery-only repeats capacity fields; it is not a new
            // capacity measurement or an on-time seed-hold observation.
            Control::Heartbeat(Some(report)) if report.delivery_only => {}
            Control::Heartbeat(Some(report)) if report.arrival_kbps > 0 => {
                child
                    .pacer
                    .set_measured_kbps(now_ns, u32::from(report.arrival_kbps));
            }
            Control::Heartbeat(Some(report)) if !report.link_late => {
                // On time but unmeasured: hold the seed; raise it while this side backs up.
                let backlog = self.fan.queue(key).map_or(0, |q| q.backlog_age_ms(now_ns));
                child.pacer.hold_seed(now_ns, Some(backlog));
            }
            Control::Heartbeat(_) => {}
            Control::KeyFrameRequest => self.recovery_keyframe = true,
            Control::EndAck => child.ended_ack = true,
            Control::WithdrawalAck => child.withdrawn_at = Some(0),
        }
        Ok(())
    }
    /// What to do next for `key`. The owner writes, then calls `written`.
    pub fn step(&mut self, key: &K, now_ns: u64) -> Result<Step, Error> {
        let index = self
            .children
            .iter()
            .position(|c| c.key == *key)
            .ok_or(Error::UnknownChild)?;
        let (dead, withdrawn, ended_ack) = {
            let c = &self.children[index];
            (c.liveness.dead(now_ns), c.withdrawn_at, c.ended_ack)
        };
        if dead {
            return Ok(self.close(index));
        }
        if let Some(at) = withdrawn {
            // Acknowledged (0) or out of time: close only this child.
            return Ok(if at == 0 || now_ns.saturating_sub(at) >= WITHDRAW_ACK_NS {
                self.close(index)
            } else {
                Step::Wait(WITHDRAW_ACK_NS - now_ns.saturating_sub(at))
            });
        }
        if let Some(ended) = self.ended_at {
            let empty = self.fan.queue(key).is_none_or(|q| q.is_empty());
            if empty && (ended_ack || now_ns.saturating_sub(ended) >= END_GRACE_NS) {
                return Ok(self.close(index));
            }
        }
        let ending = self.ended_at.is_some();
        let Self { fan, children, .. } = self;
        let child = &mut children[index];
        let queue = fan.queue(key).ok_or(Error::UnknownChild)?;
        let unhealthy = if let Some(health) = child.radio_health.as_mut() {
            child.radio_overloaded || health.should_withdraw(queue.backlog_age_ms(now_ns), queue.queued_bytes(), now_ns / 1_000_000)
        } else { child.health.should_withdraw(queue.backlog_age_ms(now_ns), now_ns) };
        if child.radio_overloaded || (!ending && unhealthy) {
            child.withdrawn_at = Some(now_ns.max(1));
            queue.close();
            return Ok(Step::Withdraw(child.link.withdraw()?));
        }
        if queue.is_empty() {
            return Ok(Step::Idle);
        }
        let wait = child.pacer.wait_ns(now_ns);
        if wait > 0 && !queue.head_is_audio() {
            return Ok(Step::Wait(wait.min(crate::fanout::PACE_SLICE_NS)));
        }
        let item = queue.take().unwrap();
        Ok(Step::Write(child.link.seal(&item)?))
    }
    /// Charges a finished write from its start (blocking GATT writes).
    pub fn written(&mut self, key: &K, bytes: usize, started_ns: u64, now_ns: u64) {
        if let Some(child) = self.children.iter_mut().find(|c| c.key == *key) {
            child.pacer.on_sent(bytes, started_ns, now_ns);
        }
    }
    fn close(&mut self, index: usize) -> Step {
        let child = self.children.remove(index);
        self.admission.release(&child.key);
        self.fan.remove_child(&child.key);
        Step::Close
    }
    /// The child link failed (EOF, write error, disconnect): close only it.
    pub fn lost(&mut self, key: &K) {
        if let Some(index) = self.children.iter().position(|c| c.key == *key) {
            self.close(index);
        }
    }
}

#[cfg(test)]
mod tests;
