//! Design QV source-side bytes and authenticated admission. No radio or capture.
use crate::{
    bmh2::{self, Capabilities, Hello, Pending, Purpose, Role, SigningIdentity},
    bluetooth::advertise, discovery::{Announcement, ACCEPTING, GATT, RECOVERY, STREAM, PAUSED},
    framing::{HandshakeReader, HopReader, frame_handshake}, labels,
    records::{Layer, Opener, Policy, Sealer}, rotating::{self, OpeningEntry, Selection},
    session::{SourceLink, SourceControl}, stream_watch::{debug, pull, read_hop}, verify_scan::Carrier,
};
use std::{sync::atomic::AtomicBool, time::{Duration, Instant}};

/// Go live is on by default; `MESHLIVE_GO_LIVE=0` turns it off (a test override).
pub fn enabled() -> bool { crate::stream_watch::switch_on(std::env::var("MESHLIVE_GO_LIVE").ok().as_deref()) }
pub fn media_state_enabled() -> bool { std::env::var("MESHLIVE_LIVE_MEDIA_STATE").ok().as_deref() == Some("1") }
pub fn camera_fence_enabled() -> bool { std::env::var("MESHLIVE_SOURCE_CAMERA_FENCE").as_deref() != Ok("0") }
pub const EXPOSURE: &str = "While you're live, nearby Bluetooth scanners can recognise this computer from day to day.";

/// User choices applied before a new source worker opens capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InitialMedia { pub muted: bool, pub camera_off: bool }

/// Radio capture belongs to an admitted listener, never to an unanswered offer.
pub struct CaptureLifecycle { radio: bool, running: bool }
impl CaptureLifecycle {
    pub fn new(radio: bool) -> Self { Self { radio, running: false } }
    pub fn running(&self) -> bool { self.running }
    /// Some(true/false) asks the microphone owner to start/stop exactly once.
    pub fn update(&mut self, paused: bool, has_viewer: bool) -> Option<bool> {
        let running = !paused && (!self.radio || has_viewer);
        if self.running == running { return None; }
        self.running = running;
        Some(running)
    }
}

/// Keep queued microphone input silent until the capture thread has closed
/// its old device and acknowledged the latest requested restart.
#[derive(Default)]
pub struct CaptureFence { generation: u64, waiting: bool }
impl CaptureFence {
    pub fn restart(&mut self) -> u64 {
        self.generation += 1;
        self.waiting = true;
        self.generation
    }
    /// An Unmute selected while paused must not reopen the microphone.
    pub fn unmute(&mut self, paused: bool) -> Option<u64> { (!paused).then(|| self.restart()) }
    pub fn acknowledged(&mut self, generation: u64) {
        if generation == self.generation { self.waiting = false; }
    }
    pub fn ready(&self) -> bool { !self.waiting }
    pub fn scrub(&self, muted: bool, pcm: &mut [i16]) {
        if muted || self.waiting { pcm.fill(0); }
    }
}

/// Independent clocks: evicting an inbound stranger cannot shorten RDI silence.
#[derive(Default)]
pub struct OfferWindow { silent_until: Option<u64>, inbound_since: Option<u64> }
impl OfferWindow {
    pub fn rotate(&mut self, now_ms: u64, random: u32) { self.silent_until = Some(now_ms.saturating_add(advertise::boundary_silence_ms(random))); }
    pub fn can_announce(&self) -> bool { self.silent_until.is_none() }
    pub fn end_silence(&mut self, now_ms: u64) -> bool {
        if self.silent_until.is_some_and(|until| now_ms >= until) { self.silent_until = None; true } else { false }
    }
    pub fn close_inbound(&mut self, connected: bool, now_ms: u64) -> bool {
        if !connected { self.inbound_since = None; return false; }
        let since = *self.inbound_since.get_or_insert(now_ms);
        now_ms.saturating_sub(since) >= 15_000
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings { pub width: u32, pub height: u32, pub fps: u32, pub kbps: u32, pub codec: u8, pub radio_preset: u8 }
impl Default for Settings {
    fn default() -> Self { Self { width: 426, height: 240, fps: 15, kbps: 200, codec: 2, radio_preset: 2 } }
}
impl Settings {
    pub fn valid(self) -> bool {
        [(426, 240), (640, 360), (854, 480)].contains(&(self.width, self.height)) && [15, 24, 30].contains(&self.fps)
            && (100..=1500).contains(&self.kbps) && self.kbps % 25 == 0 && [2, 3].contains(&self.codec) && (1..=3).contains(&self.radio_preset)
    }
    pub fn video_header(self) -> crate::media::VideoHeader {
        crate::media::VideoHeader { width: self.width as u16, height: self.height as u16, fps: self.fps as u8, codec: self.codec, quarter_turns: 0, opus: true }
    }
    pub fn encoder(self) -> crate::video::EncoderSettings {
        crate::video::EncoderSettings { width: self.width, height: self.height, fps: self.fps, codec: self.codec, bitrate_kbps: self.kbps, keyframe_interval_s: 60 }
    }
}

pub fn offer(settings: Settings, radio: bool, entry: &OpeningEntry, logical: u32, paused: bool, full: bool) -> Result<Announcement, String> {
    if !settings.valid() || entry.role != rotating::ROLE_STREAM_OFFER { return Err("Invalid live settings".into()); }
    Ok(Announcement { version: 5, prefix: entry.rdi, flags: STREAM | GATT | if paused { PAUSED } else { 0 },
        summary: if radio { 0x40 } else if settings.codec == 2 { 1 } else { 2 }, token: advertise::route_token(logical, &entry.rdi).map_err(debug)?,
        cost: 0, sequence: if full { 0x80 } else { 0 }, width: if radio { 0 } else { settings.width as u16 },
        height: if radio { 0 } else { settings.height as u16 }, fps: if radio { 0 } else { settings.fps as u8 },
        bitrate: if radio { advertise::rounded_bitrate(match settings.radio_preset { 1 => 16, 2 => 24, _ => 48 }) } else { settings.kbps as u16 }, recovery: None })
}

pub fn accepts(request: &Announcement, tokens: &advertise::TokenGrace, elapsed_ms: u64) -> bool {
    request.version == 5 && request.summary == 0 && request.is_gatt() && tokens.accepts(request.token, elapsed_ms)
        && (request.flags == ACCEPTING | GATT || request.flags == ACCEPTING | GATT | RECOVERY && request.recovery.is_some_and(|h| h != [0; 6]))
}

pub struct Child { pub link: SourceLink, pub control: SourceControl, pub buffer: Vec<u8>, pub node: [u8; 16] }

pub fn admit<C: Carrier, I: SigningIdentity>(
    c: &mut C, identity: &I, request: &Announcement, entries: &[OpeningEntry], epoch: u32,
    settings: Settings, radio: bool, stream: [u8; 16], title: &str, deadline: Instant, cancel: &AtomicBool,
) -> Result<Child, String> {
    let own: [u8; 16] = crate::crypto::sha256(&identity.public_key().map_err(debug)?).map_err(debug)?[..16].try_into().unwrap();
    if request.version != 5 || !request.is_gatt() || request.summary != 0 || request.flags & (ACCEPTING | STREAM) != ACCEPTING
        || !settings.valid() || entries.is_empty() || entries.iter().any(|e| e.role != rotating::ROLE_STREAM_OFFER || e.epoch.abs_diff(epoch) > rotating::EPOCH_TOLERANCE
            || !e.opens(&e.rdi, &own, rotating::ROLE_STREAM_OFFER).unwrap_or(false)) {
        return Err("Source opening isn't ready".into());
    }
    let codec = if radio { 0 } else if settings.codec == 2 { 1 } else { 2 };
    let caps = Capabilities { transport: 0x0c, encode_codecs: codec, decode_codecs: 0, maximum_bitrate_kbps: settings.kbps as u16, maximum_children: 0 };
    let pending = Pending::start(identity, Role::Initiator, caps, Purpose::Link).map_err(debug)?;
    c.send(&frame_handshake(&pending.hello().encode()).map_err(debug)?, deadline)?;
    let mut buffer = Vec::new(); let mut framing = HandshakeReader::default();
    let hello = loop {
        let mut input = buffer.as_slice(); let hello = framing.read(&mut input).map_err(debug)?;
        let used = buffer.len() - input.len(); buffer.drain(..used);
        if let Some(hello) = hello { break Hello::decode(&hello).map_err(debug)?; }
        pull(c, &mut buffer, deadline, cancel)?;
    };
    let link = pending.finish(&hello, |peer| {
        if peer.node_id == own || peer.capabilities.transport & 8 == 0 { return Err(bmh2::Error::PeerRejected); }
        crate::identity_gate::admit(&peer.node_id, &peer.fingerprint).map_err(|_| bmh2::Error::PeerRejected)?;
        Ok(())
    }).map_err(debug)?;
    let policy = Policy { require_opening: true, ..Policy::default() };
    let mut hop_out = Sealer::new(Layer::Hop, link.send_key(), policy).map_err(debug)?;
    let mut hop_in = HopReader::new(link.receive_key(), policy).map_err(debug)?;
    c.send(&hop_out.seal(13, 0, &rotating::encode_bdo1(entries).map_err(debug)?).map_err(debug)?, deadline)?;
    let opening = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
    let peer_entries = rotating::decode_bdo1(&opening.plaintext).map_err(debug)?;
    if rotating::check_selection(&request.prefix, true, rotating::ROLE_VIEWER_REQUEST, &link.peer().node_id, Some(&peer_entries), epoch).map_err(debug)? != Selection::Pass {
        return Err("Viewer didn't open its request".into());
    }
    let caps = Capabilities { transport: 4 | if labels::enabled() { labels::CAPABILITY_BIT } else { 0 }, ..caps };
    let pending = Pending::start(identity, Role::Initiator, caps, Purpose::Endpoint).map_err(debug)?;
    c.send(&hop_out.seal(4, 0, &pending.hello().encode()).map_err(debug)?, deadline)?;
    let hello = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
    if hello.header.kind != 4 { return Err("Expected viewer endpoint handshake".into()); }
    let endpoint = pending.finish(&Hello::decode(&hello.plaintext).map_err(debug)?, |peer| {
        if peer.fingerprint != link.peer().fingerprint || !radio && peer.capabilities.decode_codecs & codec == 0 { return Err(bmh2::Error::PeerRejected); }
        Ok(())
    }).map_err(debug)?;
    let negotiated = caps.transport & endpoint.peer().capabilities.transport & labels::CAPABILITY_BIT != 0;
    let mut content = Sealer::new(Layer::Endpoint, endpoint.send_key(), Policy { labels: negotiated, ..Default::default() }).map_err(debug)?;
    let mesh = radio || link.peer().capabilities.maximum_children > 0;
    if negotiated {
        let binding = if mesh { stream } else { [0; 16] };
        // A title that cannot be signed (too long, not canonical) is omitted,
        // as on Android; the viewer still joins.
        let label: Vec<_> = if title.is_empty() { Vec::new() } else {
            labels::sign(identity, labels::KIND_TITLE, 1, labels::BINDING_STREAM, &binding, title).ok().into_iter().collect()
        };
        c.send(&hop_out.seal(5, 0, &content.seal(10, 0, &labels::container(&label)).map_err(debug)?).map_err(debug)?, deadline)?;
    }
    Ok(Child { node: endpoint.peer().node_id, buffer,
        link: SourceLink::continue_link(hop_out, content, mesh.then_some((own, stream))),
        control: SourceControl::continue_link(hop_in, Opener::new(Layer::Endpoint, endpoint.receive_key(), Policy::default()).map_err(debug)?) })
}

#[cfg(test)]
mod tests;
