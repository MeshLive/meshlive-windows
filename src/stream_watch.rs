//! Design QV rev 4: stream admission, request bytes and hop-local reports.
//! All radio activation remains an explicit worker operation.
use crate::video::codecs;
use crate::{
    bmh2::{self, Capabilities, Hello, Pending, Purpose, Role, SigningIdentity},
    bluetooth::{advertise, scan::Sighting},
    discovery::{Announcement, ACCEPTING, GATT, STREAM},
    framing::{HandshakeReader, HopReader, frame_handshake},
    labels, link, mesh,
    records::{Layer, Opener, Policy, Record, Sealer},
    rotating::{self, OpeningEntry, Selection}, session::ViewerUplink,
    verify_scan::Carrier, watch::Reader,
};
use std::{sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use zeroize::Zeroizing;

/// Watch is on by default; `MESHLIVE_WATCH=0` turns it off (a test override).
pub fn enabled() -> bool { switch_on(std::env::var("MESHLIVE_WATCH").ok().as_deref()) }
/// A default-on feature switch: only an explicit "0" turns it off.
pub fn switch_on(value: Option<&str>) -> bool { value != Some("0") }

/// What every "live streams can't run here" text starts with.
pub const LIVE_NEEDS: &str = "Live streams need Windows 11 version 24H2 or newer and a Bluetooth adapter that can advertise.";
pub const LIVE_SWITCHED_OFF: &str = "Live streams are switched off on this PC (MESHLIVE_WATCH=0).";
pub const BLUETOOTH_OFF: &str = "Bluetooth is off. Turn Bluetooth on to watch or go live.";

/// Why Watch and Go live can't run on this PC (None: they can). `policy` is
/// the announcement policy verdict (`callee::policy`): the same Windows
/// version and adapter checks receiving calls has. Pure, for the Live page.
pub fn unsupported_reason(
    enabled: bool,
    caps: &crate::bluetooth::Capabilities,
    policy: Result<&'static str, &'static str>,
) -> Option<String> {
    use crate::bluetooth::Radio;
    if !enabled {
        return Some(LIVE_SWITCHED_OFF.into());
    }
    if caps.radio != Radio::On {
        return Some(format!("{}. Turn Bluetooth on to watch or go live.", caps.status()));
    }
    if policy == Err(crate::qw_policy::OLD_WINDOWS) {
        return Some(format!("{LIVE_NEEDS} This PC has an older version of Windows. Calling a phone still works."));
    }
    let adapter = caps.require(true).is_ok() && caps.central == Some(true) && caps.extended_advertising == Some(true);
    if !adapter {
        return Some(format!(
            "{LIVE_NEEDS} This PC's Bluetooth adapter can't advertise the way live streams need. Calling a phone still works."
        ));
    }
    policy.err().map(|reason| format!("{LIVE_NEEDS} {reason}"))
}
pub fn idle_scan_enabled() -> bool { std::env::var("MESHLIVE_STREAM_IDLE_SCAN").ok().as_deref() == Some("1") }
pub fn route_retry_enabled() -> bool { std::env::var("MESHLIVE_WATCH_ROUTE_RETRY").as_deref() == Ok("1") }

/// Authenticated video lifecycle notices. Radio mute remains local-only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackState { pub paused: bool, pub source_muted: bool, pub camera_off: bool }
impl PlaybackState {
    pub fn from_record(state: u8) -> Option<Self> {
        matches!(state, 1 | 2 | 4..=6).then_some(Self {
            paused: state == 2,
            source_muted: matches!(state, 4 | 6),
            camera_off: matches!(state, 5 | 6),
        })
    }
    /// Camera off can send nothing when muted or the header has no audio. Disconnects and failed
    /// viewer heartbeats still close the carrier; no parent heartbeat is added.
    pub fn timed_out(self, radio: bool, has_audio: bool, silence: Duration) -> bool {
        silence >= Duration::from_secs(6) && (radio || !(self.camera_off && (self.source_muted || !has_audio)))
    }
}

pub fn decode_mask() -> u8 {
    probe_decode_mask().unwrap_or(0)
}
pub fn probe_decode_mask() -> windows::core::Result<u8> {
    crate::video::device(false, false).and_then(|(gpu, _, _)| crate::video::DecodeDevice::new(&gpu))
        .and_then(|_owner| crate::video::decode_mask_probe())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target { pub logical: u32, pub summary: u8 }
impl Target {
    pub fn from(a: &Announcement) -> Self { Self { logical: advertise::logical_token(a.token, &a.prefix), summary: a.summary } }
    pub fn matches(self, a: &Announcement) -> bool { a.flags & STREAM != 0 && self == Self::from(a) }
}

/// Why this PC can't watch an offer (None: it can), in the words the Live
/// row shows. A version 2 offer is a privacy policy, not a limit (Design QV
/// R7): joining it means advertising this PC's fixed ID to everyone nearby,
/// so the phone has to use Android's "Private Bluetooth ID" (rotating IDs).
pub const NEEDS_PRIVATE_ID: &str = "Ask them to turn on Private Bluetooth ID";
/// Design HC (the cost of its stream order): an HEVC stream on a PC without Microsoft's
/// HEVC Video Extensions (or with the HEVC master off).
pub const NEEDS_HEVC: &str = "This PC can't decode HEVC: it needs Microsoft's HEVC Video Extensions";
pub fn unavailable(a: &Announcement, decode_mask: u8) -> Option<&'static str> {
    if a.version != 5 { Some(NEEDS_PRIVATE_ID) }
    else if !a.is_gatt() { Some("Not reachable from this PC") }
    else if a.summary != 0x40 && codecs::summary_to_hello(a.summary) & decode_mask & codecs::HELLO_CODECS == 0 {
        Some(if a.summary & codecs::SUMMARY_HEVC != 0 && decode_mask & codecs::HELLO_HEVC == 0 { NEEDS_HEVC } else { "This PC can't decode this stream" })
    }
    else { None }
}

pub fn request(offer: &Announcement, entry: &OpeningEntry, hint: Option<[u8; 6]>) -> Result<Announcement, String> {
    if offer.version != 5 || !offer.is_gatt() || offer.flags & STREAM == 0 || entry.role != rotating::ROLE_VIEWER_REQUEST {
        return Err("Not a rotating GATT stream".into());
    }
    let mut a = Announcement { version: 5, prefix: entry.rdi, flags: ACCEPTING | GATT,
        summary: 0, token: offer.token, cost: offer.cost, sequence: 0, width: 0, height: 0, fps: 0,
        bitrate: offer.bitrate, recovery: None };
    if let Some(hint) = hint { a = advertise::with_recovery(&a, hint).map_err(debug)?; }
    Ok(a)
}

pub(crate) fn debug(e: impl std::fmt::Debug) -> String { format!("{e:?}") }
pub(crate) fn pull<C: Carrier>(c: &mut C, buffer: &mut Vec<u8>, deadline: Instant, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) { return Err("Stopped".into()); }
    if Instant::now() >= deadline { return Err("Stream handshake timed out".into()); }
    if buffer.len() > crate::media::MAX_PAYLOAD + 4096 { return Err("Stream ingress limit".into()); }
    c.receive(buffer, Duration::from_millis(20)).map(|_| ())
}
pub(crate) fn read_hop<C: Carrier>(c: &mut C, buffer: &mut Vec<u8>, reader: &mut HopReader, deadline: Instant, cancel: &AtomicBool) -> Result<Record, String> {
    loop {
        let mut input = buffer.as_slice();
        let record = reader.read(&mut input, 4096).map_err(debug)?;
        let used = buffer.len() - input.len(); buffer.drain(..used);
        if let Some(record) = record { return Ok(record); }
        pull(c, buffer, deadline, cancel)?;
    }
}

pub struct Joined {
    pub reader: Reader,
    pub uplink: ViewerUplink,
    pub buffer: Vec<u8>,
    pub source: [u8; 16],
    pub parent: [u8; 16],
    pub recovery_key: Option<Zeroizing<[u8; 32]>>,
    pub codec_mask: u8,
    labels: Option<Vec<u8>>,
}
impl Joined {
    pub fn title_after_header(&mut self) -> Option<String> {
        let container = self.labels.take()?;
        // Only a direct non-mesh stream has no stream ID and uses zero.
        // A mesh/forwarded zero ID gets no labels (PROTOCOL Peer labels).
        let stream = match self.reader.stream_id() {
            Some(stream) if stream == [0; 16] => return None,
            Some(stream) => stream,
            None => [0; 16],
        };
        labels::parse_container(&container).ok()?.into_iter().filter_map(|part| labels::decode(part).ok())
            .filter(|label| label.kind == labels::KIND_TITLE
                && labels::verify(label, &self.source, labels::BINDING_STREAM, &stream).is_ok())
            // The newest title wins, as in Android's LabelInbox.
            .max_by_key(|label| label.sequence).map(|label| label.text)
    }
}

/// The parent always connects and initiates. A rotating hop must open exactly
/// the selected role-1 ID; endpoint and link identities must be the same.
pub fn join<C: Carrier, I: SigningIdentity>(
    c: &mut C, identity: &I, offer: &Announcement, entries: &[OpeningEntry], epoch: u32,
    decode_mask: u8, pinned_source: Option<[u8; 16]>, cancel: &AtomicBool,
) -> Result<Joined, String> {
    if unavailable(offer, decode_mask).is_some() { return Err("Stream isn't joinable".into()); }
    let own: [u8; 16] = crate::crypto::sha256(&identity.public_key().map_err(debug)?).map_err(debug)?[..16].try_into().unwrap();
    if entries.is_empty() || entries.iter().any(|e| e.role != rotating::ROLE_VIEWER_REQUEST || e.epoch.abs_diff(epoch) > rotating::EPOCH_TOLERANCE)
        || entries.iter().any(|e| !e.opens(&e.rdi, &own, rotating::ROLE_VIEWER_REQUEST).unwrap_or(false)) {
        return Err("Viewer opening is stale or invalid".into());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut buffer = Vec::new(); let mut framing = HandshakeReader::default();
    let hello = loop {
        let mut input = buffer.as_slice(); let value = framing.read(&mut input).map_err(debug)?;
        let used = buffer.len() - input.len(); buffer.drain(..used);
        if let Some(value) = value { break Hello::decode(&value).map_err(debug)?; }
        pull(c, &mut buffer, deadline, cancel)?;
    };
    let caps = Capabilities { transport: 0x0c, encode_codecs: 0, decode_codecs: decode_mask, maximum_bitrate_kbps: 1500, maximum_children: 0 };
    let pending = Pending::start(identity, Role::Responder, caps, Purpose::Link).map_err(debug)?;
    let reply = frame_handshake(&pending.hello().encode()).map_err(debug)?;
    let link = pending.finish(&hello, |peer| {
        if peer.node_id == own || peer.capabilities.transport & 8 == 0 { return Err(bmh2::Error::PeerRejected); }
        crate::identity_gate::admit(&peer.node_id, &peer.fingerprint).map_err(|_| bmh2::Error::PeerRejected)?;
        Ok(())
    }).map_err(debug)?;
    c.send(&reply, deadline)?;
    let policy = Policy { require_opening: true, ..Policy::default() };
    let mut hop_in = HopReader::new(link.receive_key(), policy).map_err(debug)?;
    let mut hop_out = Sealer::new(Layer::Hop, link.send_key(), policy).map_err(debug)?;
    let opening = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
    let parent_entries = rotating::decode_bdo1(&opening.plaintext).map_err(debug)?;
    if rotating::check_selection(&offer.prefix, true, rotating::ROLE_STREAM_OFFER, &link.peer().node_id, Some(&parent_entries), epoch).map_err(debug)? != Selection::Pass {
        return Err("Parent did not open the selected stream offer".into());
    }
    c.send(&hop_out.seal(13, 0, &rotating::encode_bdo1(entries).map_err(debug)?).map_err(debug)?, deadline)?;
    let remote = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
    if remote.header.kind != 4 { return Err("Expected endpoint handshake".into()); }
    let hello = Hello::decode(&remote.plaintext).map_err(debug)?;
    let caps = Capabilities { transport: 4 | if labels::enabled() { labels::CAPABILITY_BIT } else { 0 }, ..caps };
    let pending = Pending::start(identity, Role::Responder, caps, Purpose::Endpoint).map_err(debug)?;
    let reply = pending.hello().encode();
    let endpoint = pending.finish(&hello, |peer| {
        if peer.fingerprint != link.peer().fingerprint { return Err(bmh2::Error::PeerRejected); }
        crate::identity_gate::admit(&peer.node_id, &peer.fingerprint).map_err(|_| bmh2::Error::PeerRejected)?;
        Ok(())
    }).map_err(debug)?;
    c.send(&hop_out.seal(4, 0, &reply).map_err(debug)?, deadline)?;
    let negotiated = caps.transport & endpoint.peer().capabilities.transport & labels::CAPABILITY_BIT != 0;
    let mut content = Opener::new(Layer::Endpoint, endpoint.receive_key(), Policy { labels: negotiated, ..Policy::default() }).map_err(debug)?;
    let mut source = endpoint.peer().node_id;
    let mut grant = None; let mut recovery_key = None;
    if offer.cost != 0 {
        let r = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
        if r.header.kind != 5 { return Err("Expected forwarded grant".into()); }
        let g = content.open(&r.plaintext).map_err(debug)?;
        if g.header.kind != 7 { return Err("Expected BFG1".into()); }
        let value = mesh::Grant::decode(&g.plaintext).map_err(debug)?;
        source = *value.source();
        crate::identity_gate::admit_source(&source).map_err(debug)?;
        recovery_key = Some(advertise::recovery_key(value.key()).map_err(debug)?);
        grant = Some(value);
    }
    if pinned_source.is_some_and(|pinned| pinned != source) { return Err("Authenticated stream source changed".into()); }
    let mut labels = None;
    if negotiated {
        let r = read_hop(c, &mut buffer, &mut hop_in, deadline, cancel)?;
        if r.header.kind != 5 { return Err("Expected labels".into()); }
        let record = content.open(&r.plaintext).map_err(debug)?;
        if record.header.kind != 10 { return Err("Expected negotiated labels".into()); }
        labels = Some(record.plaintext.to_vec());
    }
    let audio = offer.summary == 0x40;
    let reader = match grant {
        Some(grant) => Reader::forwarded(hop_in, grant, audio, true).map_err(debug)?,
        None => Reader::direct(hop_in, content, source, audio, true),
    };
    let uplink = ViewerUplink::continue_link(hop_out, Sealer::new(Layer::Endpoint, endpoint.send_key(), Policy::default()).map_err(debug)?);
    // Hello bits throughout: the summary is mapped through the one table (a
    // relayed offer has no source hello to intersect with).
    let codec_mask = decode_mask & codecs::summary_to_hello(offer.summary)
        & if offer.cost == 0 { endpoint.peer().capabilities.encode_codecs } else { codecs::HELLO_CODECS };
    Ok(Joined { reader, uplink, buffer, source, parent: link.peer().node_id, recovery_key, codec_mask, labels })
}

#[derive(Default)]
pub struct Reports { last: Option<(u64, u64, bool, u16, u64)> }
impl Reports {
    pub fn next(&mut self, now_ms: u64, trains: u64, received: u64, kbps: u16, late: bool, added_ms: u16) -> Option<link::Report> {
        let (at, reported, was_late, delay, bytes) = self.last.unwrap_or((0, 0, false, 0, 0));
        if now_ms.saturating_sub(at) < 1000 { return None; }
        let delay_changed = delay.abs_diff(added_ms) >= 50;
        let on_time = trains == 0 && !late && received > bytes;
        let changed = trains > reported || was_late != late || delay_changed;
        if !(changed || on_time) || !(kbps > 0 || was_late != late || delay_changed || on_time) { return None; }
        self.last = Some((now_ms, trains, late, added_ms, received));
        Some(link::Report { arrival_kbps: kbps, link_late: late, added_delay_ms: added_ms, delivery: None, delivery_only: false })
    }
}

/// Bounded route board: never evicts an actively heard stream to admit a flood.
#[derive(Default)]
pub struct Offers { rows: Vec<(u32, Sighting)>, next: u32, suppressed: Vec<((u64, [u8; 6], u32), Instant)> }
impl Offers {
    fn key(s: &Sighting) -> (u64, [u8; 6], u32) { (s.route.address_key(), s.announcement.prefix, s.announcement.token) }
    pub fn routes(&self) -> impl Iterator<Item = &Sighting> { self.rows.iter().map(|(_, s)| s) }
    /// Only the attempted advertising route is delayed, never its source or
    /// logical stream. Retains the worker's existing bounded retry penalties.
    pub fn suppress(&mut self, s: &Sighting, now: Instant, delay: Duration) {
        if self.suppressed.len() == 64 { self.suppressed.remove(0); }
        self.suppressed.push((Self::key(s), now + delay));
    }
    pub fn next_route(&self, target: Target, mask: u8, pinned: bool, now: Instant) -> Option<&Sighting> {
        self.routes().filter(|s| target.matches(&s.announcement)
            && now.saturating_duration_since(s.received) <= Duration::from_secs(10)
            && unavailable(&s.announcement, mask).is_none()
            && (s.announcement.sequence & 0x80 == 0 || pinned && s.announcement.cost == 0)
            && !self.suppressed.iter().any(|(key, until)| *key == Self::key(s) && *until > now))
            .min_by_key(|s| s.announcement.cost)
    }
    pub fn observe(&mut self, s: Sighting) {
        self.prune(Instant::now());
        if s.announcement.flags & STREAM == 0 { return; }
        if let Some((_, old)) = self.rows.iter_mut().find(|(_, old)| old.route.address_key() == s.route.address_key()
            && old.announcement.prefix == s.announcement.prefix && old.announcement.token == s.announcement.token) { *old = s; return; }
        let existing = self.rows.iter().find(|(_, old)| Target::from(&old.announcement) == Target::from(&s.announcement)).map(|(id, _)| *id);
        if self.rows.len() >= 256 || existing.is_none() && self.rows.iter().map(|(id, _)| *id).collect::<std::collections::BTreeSet<_>>().len() >= 64 { return; }
        let id = existing.unwrap_or_else(|| { self.next = self.next.wrapping_add(1).max(1); self.next });
        self.rows.push((id, s));
    }
    pub fn prune(&mut self, now: Instant) {
        self.rows.retain(|(_, s)| now.saturating_duration_since(s.received) <= Duration::from_secs(12));
        self.suppressed.retain(|(_, until)| *until > now);
    }
    pub fn best(&self, id: u32, mask: u8) -> Option<&Sighting> {
        self.rows.iter().filter(|(h, _)| *h == id).map(|(_, s)| s).min_by_key(|s| (unavailable(&s.announcement, mask).is_some() || s.announcement.sequence & 0x80 != 0, s.announcement.cost))
    }
    pub fn rows(&self, mask: u8) -> Vec<(u32, &Sighting)> {
        self.rows.iter().map(|(id, _)| *id).collect::<std::collections::BTreeSet<_>>().into_iter()
            .filter_map(|id| self.best(id, mask).map(|s| (id, s))).collect()
    }
}

#[cfg(test)]
mod tests;
