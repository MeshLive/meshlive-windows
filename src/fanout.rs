//! Sender-side fan-out with Android's performance rules: a source encodes once
//! and every direct child gets the same shared bytes (`Arc`, never copied) in
//! its own strictly ordered queue; nothing a child can use is dropped (latency
//! instead of loss), only video before a joining child's first keyframe; a
//! child that stays behind is withdrawn instead of slowing the source; a new
//! child is admitted only inside the measured aggregate budget; the writer is
//! paced by `LinkPacer`, and only an audio record at the head may skip the wait.
//! A relay queues unchanged upstream ciphertext through the same queue, so
//! relaying adds no re-encode or re-seal. Ports Android `RelayChildQueue`,
//! `RelayChildHealthPolicy`, `LinkAdmissionController`, `awaitLinkCredit` and the
//! late-joiner `VideoConfigBatch` cache. The opt-in BTL1 source policy sheds
//! only whole signed audio records; video and opaque relay queues keep this arm.
use crate::{bluetooth::pacer::LinkPacer, media};
use std::{collections::VecDeque, sync::Arc};

/// What a queue needs to know about one record; the bytes are opaque here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Class {
    pub audio: bool,
    pub key_frame: bool,
    pub config: bool,
    /// Stream header, lifecycle and other non-media records: never skipped.
    pub control: bool,
    pub discontinuity: bool,
}
impl Class {
    /// From the inner BTV3 packet flags.
    pub fn media(flags: u8) -> Self {
        Self {
            audio: flags & media::AUDIO != 0,
            key_frame: flags & media::KEY_FRAME != 0,
            config: flags & media::CONFIG != 0,
            control: flags & media::ROTATION != 0,
            discontinuity: flags & media::DISCONTINUITY != 0,
        }
    }
    pub fn control() -> Self {
        Self {
            control: true,
            ..Self::default()
        }
    }
    fn is_video_frame(self) -> bool {
        !self.audio && !self.control && !self.config
    }
}

/// One encoded (or, at a relay, still encrypted) record shared by every child.
#[derive(Clone, Debug)]
pub struct Item {
    pub bytes: Arc<[u8]>,
    pub class: Class,
    /// BME1 content type a source seals it as (1 header, 2 media, 4 lifecycle);
    /// 0 for opaque relay ciphertext that is copied unchanged.
    pub kind: u8,
}
impl Item {
    /// Opaque bytes (a relay's unchanged upstream ciphertext).
    pub fn new(bytes: impl Into<Arc<[u8]>>, class: Class) -> Self {
        Self {
            bytes: bytes.into(),
            class,
            kind: 0,
        }
    }
    /// One encoded BTV3 packet; its class comes from its own flags byte.
    pub fn media(packet: impl Into<Arc<[u8]>>) -> Option<Self> {
        let bytes: Arc<[u8]> = packet.into();
        let flags = *bytes.get(4)?;
        Some(Self {
            class: Class::media(flags),
            bytes,
            kind: 2,
        })
    }
    /// The stream header (BTV3 video or BTL1 audio-only).
    pub fn header(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
            class: Class::control(),
            kind: 1,
        }
    }
    /// Lifecycle state: LIVE 1, PAUSED 2, ENDED 3; optional video notices 4-6.
    pub fn lifecycle(state: u8) -> Self {
        Self {
            bytes: Arc::from([state].as_slice()),
            class: Class::control(),
            kind: 4,
        }
    }
    /// Source packet namespace only. Never infer eligibility from relay bytes
    /// or a loose AUDIO bit test (config and other flag combinations survive).
    pub fn radio_source_audio(&self) -> bool {
        self.kind == 2 && self.bytes.get(4).is_some_and(|&flags|
            flags == media::AUDIO || flags == (media::AUDIO | media::AUDIO_BATCH))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Queued,
    /// Joined mid-stream: video before the first keyframe cannot be decoded there.
    WaitingForJoinKeyFrame,
    StartedAtKeyFrame,
    Closed,
}

struct Queued {
    item: Item,
    at_ns: u64,
}

/// One child's strictly ordered queue. Video uses backlog as withdrawal
/// evidence; the opt-in radio source owner also applies shedding and a byte bound.
#[derive(Default)]
pub struct ChildQueue {
    items: VecDeque<Queued>,
    bytes: usize,
    waiting_for_key_frame: bool,
    closed: bool,
    skipped_before_join: u64,
}
impl ChildQueue {
    pub fn joining() -> Self {
        Self {
            waiting_for_key_frame: true,
            ..Self::default()
        }
    }
    pub fn offer(&mut self, item: Item, now_ns: u64) -> Offer {
        if self.closed {
            return Offer::Closed;
        }
        let class = item.class;
        if self.waiting_for_key_frame && class.is_video_frame() {
            if !class.key_frame {
                self.skipped_before_join += 1;
                return Offer::WaitingForJoinKeyFrame;
            }
            self.waiting_for_key_frame = false;
            self.push(item, now_ns);
            return Offer::StartedAtKeyFrame;
        }
        self.push(item, now_ns);
        Offer::Queued
    }
    fn push(&mut self, item: Item, now_ns: u64) {
        self.bytes += item.bytes.len();
        self.items.push_back(Queued {
            item,
            at_ns: now_ns,
        });
    }
    pub fn take(&mut self) -> Option<Item> {
        let queued = self.items.pop_front()?;
        self.bytes -= queued.item.bytes.len();
        Some(queued.item)
    }
    /// Only the head decides whether the pacer may be skipped: the child
    /// rejects a sequence that goes backwards, so audio cannot overtake video.
    pub fn head_is_audio(&self) -> bool {
        self.items.front().is_some_and(|q| q.item.class.audio)
    }
    pub fn backlog_age_ms(&self, now_ns: u64) -> u64 {
        self.items
            .front()
            .map_or(0, |q| now_ns.saturating_sub(q.at_ns) / 1_000_000)
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }
    /// Android AudioLiveQueuePolicy: after 3 s, retain the newest 1.5 s.
    /// Drop before sealing, never a partial record or an in-flight write.
    pub fn shed_radio(&mut self, now_ns: u64) -> usize {
        let age_ms = |q: &Queued| now_ns.saturating_sub(q.at_ns) / 1_000_000;
        if self.items.iter().find(|q| q.item.radio_source_audio()).is_none_or(|q| age_ms(q) <= 3_000) {
            return 0;
        }
        let before = self.items.len();
        self.items.retain(|q| !q.item.radio_source_audio() || age_ms(q) <= 1_500);
        self.bytes = self.items.iter().map(|q| q.item.bytes.len()).sum();
        before - self.items.len()
    }
    pub fn skipped_before_join(&self) -> u64 {
        self.skipped_before_join
    }
    /// Pause/End: drop queued media; stream header, lifecycle and codec
    /// configuration stay in order.
    pub fn clear_media(&mut self) {
        self.items
            .retain(|q| q.item.class.control || q.item.class.config);
        self.bytes = self.items.iter().map(|q| q.item.bytes.len()).sum();
    }
    /// Radio Mute: discard unsent audio, preserving setup and lifecycle order.
    pub fn clear_audio(&mut self) {
        self.items.retain(|q| !q.item.class.audio || q.item.class.config || q.item.class.control);
        self.bytes = self.items.iter().map(|q| q.item.bytes.len()).sum();
    }
    /// Camera off: retain audio/setup/control order; video resumes at a keyframe.
    pub fn clear_video(&mut self) {
        self.items.retain(|q| q.item.class.audio || q.item.class.config || q.item.class.control);
        self.bytes = self.items.iter().map(|q| q.item.bytes.len()).sum();
        self.waiting_for_key_frame = true;
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.items.clear();
        self.bytes = 0;
    }
}

/// Android `RelayChildHealthPolicy` (no-loss arm): withdraw a child whose
/// backlog stays beyond what a viewer's playout can absorb.
#[derive(Default)]
pub struct ChildHealth {
    behind_since_ns: Option<u64>,
    samples: VecDeque<(u64, u64)>,
}
impl ChildHealth {
    pub const WITHDRAW_BACKLOG_MS: u64 = 2_500;
    pub const SUSTAINED_NS: u64 = 3_000_000_000;
    pub const HARD_BACKLOG_MS: u64 = 6_000;
    pub const TREND_WINDOW_NS: u64 = 3_000_000_000;
    pub const TREND_MIN_BACKLOG_MS: u64 = 1_500;
    pub const TREND_GROWTH_MS: u64 = 500;
    pub fn should_withdraw(&mut self, backlog_ms: u64, now_ns: u64) -> bool {
        if backlog_ms >= Self::HARD_BACKLOG_MS {
            return true;
        }
        self.samples.push_back((now_ns, backlog_ms));
        while self.samples.len() >= 2
            && now_ns.saturating_sub(self.samples[1].0) >= Self::TREND_WINDOW_NS
        {
            self.samples.pop_front();
        }
        let (then_ns, then_ms) = self.samples[0];
        let peak = self.samples.iter().map(|s| s.1).max().unwrap_or(0);
        if now_ns.saturating_sub(then_ns) >= Self::TREND_WINDOW_NS
            && backlog_ms >= Self::TREND_MIN_BACKLOG_MS
            && backlog_ms.saturating_sub(then_ms) >= Self::TREND_GROWTH_MS
            && backlog_ms >= peak
        {
            return true;
        }
        if backlog_ms < Self::WITHDRAW_BACKLOG_MS {
            self.behind_since_ns = None;
            return false;
        }
        let since = *self.behind_since_ns.get_or_insert(now_ns);
        now_ns.saturating_sub(since) >= Self::SUSTAINED_NS
    }
}

/// Android AudioLiveChildHealth, independent of video's no-loss policy.
#[derive(Default)]
pub struct RadioHealth {
    samples: VecDeque<(u64, u64, u64)>,
    queued: u64,
    shed: u64,
}
impl RadioHealth {
    /// Normal-memory Android sender bound; checked after every enqueue.
    pub const MAX_BYTES: usize = 8 * 1024 * 1024;
    pub fn observe(&mut self, queued: bool, shed: usize, now_ms: u64) {
        if queued || shed > 0 {
            self.samples.push_back((now_ms, u64::from(queued), shed as u64));
            self.queued += u64::from(queued);
            self.shed += shed as u64;
        }
        self.prune(now_ms);
    }
    fn prune(&mut self, now_ms: u64) {
        while self.samples.front().is_some_and(|s| now_ms.saturating_sub(s.0) > 10_000) {
            let (_, queued, shed) = self.samples.pop_front().unwrap();
            self.queued -= queued;
            self.shed -= shed;
        }
    }
    pub fn should_withdraw(&mut self, age_ms: u64, bytes: usize, now_ms: u64) -> bool {
        self.prune(now_ms);
        age_ms > 30_000 || bytes > Self::MAX_BYTES || (self.queued >= 10 && self.shed * 5 > self.queued)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionDecision {
    pub admitted: bool,
    pub reserved_kbps: u32,
    pub available_kbps: u32,
}
/// Android `LinkAdmissionController`: admit a child only inside the measured aggregate budget.
pub struct Admission<K> {
    maximum_kbps: u32,
    inbound_kbps: u32,
    children: Vec<(K, u32)>,
}
impl<K: PartialEq> Admission<K> {
    pub fn new(maximum_kbps: u32) -> Option<Self> {
        (maximum_kbps > 0).then(|| Self {
            maximum_kbps,
            inbound_kbps: 0,
            children: Vec::new(),
        })
    }
    pub fn set_inbound_kbps(&mut self, kbps: u32) {
        self.inbound_kbps = kbps;
    }
    pub fn try_admit(&mut self, child: K, expected_kbps: u32) -> Option<AdmissionDecision> {
        if expected_kbps == 0 {
            return None;
        }
        let own = self
            .children
            .iter()
            .find(|(k, _)| *k == child)
            .map_or(0, |c| c.1);
        let used = self.inbound_kbps + self.children.iter().map(|c| c.1).sum::<u32>() - own;
        let available = self.maximum_kbps.saturating_sub(used);
        let admitted = expected_kbps <= available;
        if admitted {
            self.children.retain(|(k, _)| *k != child);
            self.children.push((child, expected_kbps));
        }
        Some(AdmissionDecision {
            admitted,
            reserved_kbps: used,
            available_kbps: available,
        })
    }
    pub fn release(&mut self, child: &K) {
        self.children.retain(|(k, _)| k != child);
    }
    pub fn reserved_kbps(&self) -> u32 {
        self.inbound_kbps + self.children.iter().map(|c| c.1).sum::<u32>()
    }
}

pub const PACE_SLICE_NS: u64 = 5_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Credit {
    pub slept_ns: u64,
    /// Wait skipped because the head record was audio.
    pub exempted_ns: u64,
}
/// Android `awaitLinkCredit`: wait for the pacer in 5 ms slices; audio at the
/// head is never held.
pub fn link_credit(
    pacer: &LinkPacer,
    mut now_ns: impl FnMut() -> u64,
    head_is_audio: impl Fn() -> bool,
    running: impl Fn() -> bool,
    mut sleep_ns: impl FnMut(u64),
) -> Credit {
    let mut slept_ns = 0;
    while running() {
        let wait = pacer.wait_ns(now_ns());
        if wait == 0 {
            break;
        }
        if head_is_audio() {
            return Credit {
                slept_ns,
                exempted_ns: wait,
            };
        }
        let slice = wait.min(PACE_SLICE_NS);
        sleep_ns(slice);
        slept_ns += slice;
    }
    Credit {
        slept_ns,
        exempted_ns: 0,
    }
}

/// Codec configuration a late joiner needs: only the current encoder's video
/// configuration (a config after video media starts a new batch; a
/// discontinuity keyframe without new configuration drops it), deduplicated,
/// at most four records; audio configuration is kept.
#[derive(Default)]
pub struct ConfigCache {
    items: Vec<Item>,
    media_since_config: bool,
}
impl ConfigCache {
    pub const MAX: usize = 4;
    pub fn observe(&mut self, item: &Item) {
        let class = item.class;
        if class.config {
            if !class.audio {
                if self.media_since_config {
                    self.items.retain(|i| i.class.audio);
                }
                self.media_since_config = false;
            }
            if !self
                .items
                .iter()
                .any(|i| i.class == class && i.bytes == item.bytes)
            {
                if self.items.len() == Self::MAX {
                    self.items.remove(0);
                }
                self.items.push(item.clone());
            }
        } else if class.is_video_frame() {
            if class.key_frame && class.discontinuity && self.media_since_config {
                self.items.retain(|i| i.class.audio);
            }
            self.media_since_config = true;
        }
    }
    pub fn items(&self) -> &[Item] {
        &self.items
    }
}

/// Encode once, fan out to every admitted child.
pub struct FanOut<K> {
    children: Vec<(K, ChildQueue)>,
    config: ConfigCache,
    /// Records every joiner gets first (stream header, current lifecycle state).
    preamble: Vec<Item>,
}
impl<K: PartialEq + Clone> Default for FanOut<K> {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            config: ConfigCache::default(),
            preamble: Vec::new(),
        }
    }
}
impl<K: PartialEq + Clone> FanOut<K> {
    pub fn set_preamble(&mut self, items: Vec<Item>) {
        self.preamble = items;
    }
    /// Adds a child at the live edge: preamble and current configuration first,
    /// then video from the next keyframe. Returns true when the caller must
    /// request a keyframe from its encoder or parent.
    pub fn add_child(&mut self, child: K, now_ns: u64) -> bool {
        self.remove_child(&child);
        let mut queue = ChildQueue::joining();
        for item in self.preamble.iter().chain(self.config.items()) {
            queue.offer(item.clone(), now_ns);
        }
        self.children.push((child, queue));
        true
    }
    pub fn remove_child(&mut self, child: &K) -> bool {
        let before = self.children.len();
        self.children.retain(|(k, _)| k != child);
        before != self.children.len()
    }
    /// One encoded record for all children; the bytes are shared, not copied.
    pub fn publish(&mut self, item: Item, now_ns: u64) -> Vec<(K, Offer)> {
        self.config.observe(&item);
        self.children
            .iter_mut()
            .map(|(k, q)| (k.clone(), q.offer(item.clone(), now_ns)))
            .collect()
    }
    pub fn queue(&mut self, child: &K) -> Option<&mut ChildQueue> {
        self.children
            .iter_mut()
            .find(|(k, _)| k == child)
            .map(|(_, q)| q)
    }
    pub fn children(&self) -> impl Iterator<Item = &K> {
        self.children.iter().map(|(k, _)| k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    fn video(byte: u8, key: bool) -> Item {
        let flags = if key { media::KEY_FRAME } else { 0 };
        Item::new(vec![byte; 100], Class::media(flags))
    }
    fn audio(byte: u8) -> Item {
        Item::new(vec![byte; 10], Class::media(media::AUDIO))
    }
    fn config(byte: u8, audio: bool) -> Item {
        let flags = media::CONFIG | if audio { media::AUDIO } else { 0 };
        Item::new(vec![byte; 4], Class::media(flags))
    }
    #[test]
    fn radio_shedding_keeps_boundaries_setup_and_other_namespaces_unchanged() {
        let source_audio = |flags| Item::media(media::Packet { flags, pts_us: 0, payload: &[1, 2, 3] }.encode().unwrap()).unwrap();
        let mut queue = ChildQueue::default();
        let mut retained = vec![Item::header(b"BTL1".as_slice()), Item::lifecycle(2), Item::lifecycle(1)];
        for flags in 0..=u8::MAX {
            if flags != media::AUDIO && flags != (media::AUDIO | media::AUDIO_BATCH) { retained.push(source_audio(flags)); }
        }
        // Identical AUDIO-looking bytes in the opaque relay namespace survive.
        retained.push(Item::new(source_audio(media::AUDIO).bytes, Class::media(media::AUDIO)));
        for item in &retained { queue.offer(item.clone(), 0); }
        queue.offer(source_audio(media::AUDIO), 0);
        queue.offer(source_audio(media::AUDIO | media::AUDIO_BATCH), 1_500 * MS);
        let edge = source_audio(media::AUDIO);
        queue.offer(edge.clone(), 1_501 * MS);
        assert_eq!(queue.shed_radio(3 * S), 0, "exactly three seconds does not shed");
        assert_eq!(queue.shed_radio(3 * S + MS), 2, "only complete records older than 1.5 seconds leave");
        retained.push(edge);
        assert_eq!(queue.queued_bytes(), retained.iter().map(|i| i.bytes.len()).sum::<usize>());
        for expected in retained {
            let actual = queue.take().unwrap();
            assert!(Arc::ptr_eq(&actual.bytes, &expected.bytes), "retained bytes and their order never change");
        }
        assert!(queue.is_empty());
    }
    #[test]
    fn radio_health_uses_strict_rate_age_memory_and_rolling_window_limits() {
        let mut health = RadioHealth::default();
        for _ in 0..9 { health.observe(true, 0, 0); }
        health.observe(false, 2, 0);
        assert!(!health.should_withdraw(0, 0, 0), "fewer than ten queued records");
        health.observe(true, 0, 0);
        assert!(!health.should_withdraw(30_000, RadioHealth::MAX_BYTES, 0), "exact age, byte and 20% limits stay admitted");
        assert!(health.should_withdraw(30_001, 0, 0));
        assert!(health.should_withdraw(0, RadioHealth::MAX_BYTES + 1, 0));
        health.observe(false, 1, 0);
        assert!(health.should_withdraw(0, 0, 10_000), "the exact ten-second boundary remains in the window");
        assert!(!health.should_withdraw(0, 0, 10_001), "expired queued and shed samples leave together");
        for _ in 0..10 { health.observe(true, 0, 10_001); }
        assert!(!health.should_withdraw(0, 0, 10_001), "old shedding cannot penalise a recovered listener");
    }
    #[test]
    fn one_encoded_record_reaches_every_child_without_a_copy_and_in_order() {
        let mut fan = FanOut::default();
        fan.add_child(1, 0);
        fan.add_child(2, 0);
        let key = video(1, true);
        fan.publish(key.clone(), 0);
        fan.publish(audio(2), 0);
        fan.publish(video(3, false), 0);
        for child in [1, 2] {
            let q = fan.queue(&child).unwrap();
            let first = q.take().unwrap();
            assert!(Arc::ptr_eq(&first.bytes, &key.bytes), "shared, not copied");
            assert!(q.head_is_audio());
            assert_eq!(q.take().unwrap().bytes[0], 2);
            assert!(!q.head_is_audio());
            assert_eq!(q.take().unwrap().bytes[0], 3);
            assert!(q.take().is_none());
        }
        assert_eq!(Arc::strong_count(&key.bytes), 1);
    }
    #[test]
    fn a_joiner_gets_preamble_and_current_config_then_waits_only_for_video() {
        let mut fan = FanOut::default();
        fan.set_preamble(vec![Item::new(vec![9], Class::control())]);
        fan.publish(config(1, true), 0);
        fan.publish(config(2, false), 0);
        fan.publish(video(3, true), 0);
        assert!(fan.add_child(7, 0));
        let offers = [
            fan.publish(video(4, false), 0),
            fan.publish(audio(5), 0),
            fan.publish(video(6, true), 0),
            fan.publish(video(7, false), 0),
        ];
        assert_eq!(offers[0], vec![(7, Offer::WaitingForJoinKeyFrame)]);
        assert_eq!(offers[1], vec![(7, Offer::Queued)]);
        assert_eq!(offers[2], vec![(7, Offer::StartedAtKeyFrame)]);
        assert_eq!(offers[3], vec![(7, Offer::Queued)]);
        let q = fan.queue(&7).unwrap();
        let got: Vec<u8> = std::iter::from_fn(|| q.take())
            .map(|i| i.bytes[0])
            .collect();
        assert_eq!(got, vec![9, 1, 2, 5, 6, 7]);
        assert_eq!(q.skipped_before_join(), 1);
    }
    #[test]
    fn late_joiners_get_only_the_current_encoders_configuration() {
        let mut cache = ConfigCache::default();
        cache.observe(&config(1, true));
        cache.observe(&config(2, false));
        cache.observe(&config(2, false));
        cache.observe(&video(0, true));
        // A restarted encoder: its configuration replaces the old video configuration.
        cache.observe(&config(3, false));
        let kept: Vec<u8> = cache.items().iter().map(|i| i.bytes[0]).collect();
        assert_eq!(kept, vec![1, 3]);
        // A discontinuity keyframe without new configuration drops the stale one.
        cache.observe(&video(0, false));
        let restart = Item::new(
            vec![0; 10],
            Class::media(media::KEY_FRAME | media::DISCONTINUITY),
        );
        cache.observe(&restart);
        let kept: Vec<u8> = cache.items().iter().map(|i| i.bytes[0]).collect();
        assert_eq!(kept, vec![1]);
        for byte in 10..20 {
            cache.observe(&config(byte, true));
        }
        assert_eq!(cache.items().len(), ConfigCache::MAX);
    }
    #[test]
    fn health_withdraws_a_child_that_stays_behind_but_absorbs_a_burst() {
        let mut p = ChildHealth::default();
        assert!((0..20).all(|t| !p.should_withdraw(1_800, t * S)));
        let mut p = ChildHealth::default();
        assert!(!p.should_withdraw(2_600, 0));
        assert!(!p.should_withdraw(2_900, 2 * S));
        assert!(p.should_withdraw(3_200, 3 * S));
        let mut p = ChildHealth::default();
        p.should_withdraw(2_600, 0);
        assert!(!p.should_withdraw(1_000, 2 * S));
        assert!(!p.should_withdraw(1_200, 3 * S));
        assert!(!p.should_withdraw(1_300, 5 * S));
        assert!(ChildHealth::default().should_withdraw(6_000, 0));
        let mut p = ChildHealth::default();
        let growth = [300, 700, 1_100, 1_400, 1_640];
        assert!(
            growth
                .iter()
                .enumerate()
                .any(|(t, ms)| p.should_withdraw(*ms, t as u64 * S))
        );
    }
    #[test]
    fn admission_stays_inside_the_measured_budget() {
        let mut a = Admission::new(800).unwrap();
        a.set_inbound_kbps(300);
        assert!(a.try_admit(1, 300).unwrap().admitted);
        let d = a.try_admit(2, 300).unwrap();
        assert!(!d.admitted);
        assert_eq!((d.reserved_kbps, d.available_kbps), (600, 200));
        // Re-admitting the same child replaces its own reservation.
        assert!(a.try_admit(1, 500).unwrap().admitted);
        assert_eq!(a.reserved_kbps(), 800);
        a.release(&1);
        assert!(a.try_admit(2, 500).unwrap().admitted);
        assert!(a.try_admit(3, 0).is_none());
        assert!(Admission::<u8>::new(0).is_none());
    }
    #[test]
    fn only_audio_at_the_head_skips_the_link_pacer() {
        let now = std::cell::Cell::new(S);
        let charged = || {
            let mut pacer = LinkPacer::new();
            pacer.set_measured_kbps(now.get(), 800);
            pacer.on_sent(12_000, now.get(), now.get());
            pacer
        };
        let mut q = ChildQueue::default();
        q.offer(video(1, true), 0);
        q.offer(audio(2), 0);
        let pacer = charged();
        let mut sleeps = 0;
        let credit = link_credit(
            &pacer,
            || now.get(),
            || q.head_is_audio(),
            || true,
            |ns| {
                sleeps += 1;
                now.set(now.get() + ns);
            },
        );
        assert_eq!(
            credit,
            Credit {
                slept_ns: 50 * MS,
                exempted_ns: 0
            }
        );
        assert_eq!(sleeps, 10);
        q.take();
        let pacer = charged();
        let credit = link_credit(
            &pacer,
            || now.get(),
            || q.head_is_audio(),
            || true,
            |_| panic!("audio waited"),
        );
        assert_eq!(
            credit,
            Credit {
                slept_ns: 0,
                exempted_ns: 50 * MS
            }
        );
    }
    #[test]
    fn backlog_age_comes_from_the_oldest_queued_record_and_close_drops_everything() {
        let mut q = ChildQueue::default();
        assert_eq!(q.backlog_age_ms(5 * S), 0);
        q.offer(video(1, true), S);
        q.offer(video(2, false), 2 * S);
        assert_eq!(q.backlog_age_ms(3 * S), 2_000);
        assert_eq!(q.queued_bytes(), 200);
        q.take();
        assert_eq!(q.backlog_age_ms(3 * S), 1_000);
        q.close();
        assert_eq!((q.len(), q.queued_bytes()), (0, 0));
        assert_eq!(q.offer(video(3, true), 0), Offer::Closed);
    }
}
