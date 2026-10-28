//! Explicit Watch worker. The radio backend stops its call provider before
//! starting this worker and keeps it paused until the user leaves Watch (F1).
use crate::ui::{backend::{PictureHandle, Update}, model::WatchEvent};
use meshlive::{
    bluetooth::{advertise, peripheral::{Form, Peripheral}, scan::{Scanner, ScanOptions, Sighting}},
    bmh2::SigningIdentity, stream_watch::{self, Target, Offers}, rotating::OpeningEntry,
    verify_scan::Carrier, watch::{Event, Header},
};
use std::{path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::Sender}, thread::JoinHandle, time::{Duration, Instant}};

pub struct Task {
    cancel: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Task {
    pub fn watch(dir: PathBuf, selected: Sighting, updates: Sender<Update>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false)); let muted = Arc::new(AtomicBool::new(false));
        let (stop, mute) = (cancel.clone(), muted.clone());
        let failed = updates.clone();
        let worker = std::thread::Builder::new().name("meshlive-watch".into()).spawn(move || {
            if let Err(error) = run(dir, selected, &stop, &mute, &updates) {
                let _ = updates.send(Update::WatchNote(Some(format!("Couldn't watch. {error}"))));
                let _ = updates.send(Update::Watch(WatchEvent::GaveUp));
            }
        }).ok();
        if worker.is_none() { cancel.store(true, Ordering::Release); let _ = failed.send(Update::Watch(WatchEvent::GaveUp)); }
        Self { cancel, muted, worker }
    }
    pub fn mute(&self, muted: bool) { self.muted.store(muted, Ordering::Release); }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

fn debug(error: impl std::fmt::Debug) -> String { format!("{error:?}") }
fn wait(stop: &AtomicBool, duration: Duration) {
    let until = Instant::now() + duration;
    while !stop.load(Ordering::Acquire) && Instant::now() < until { std::thread::sleep(Duration::from_millis(20)); }
}

fn epoch(owner: &mut meshlive::epoch_store::EpochOwner, vault: &mut meshlive::presence::Vault) -> Result<u32, String> {
    let elapsed = meshlive::epoch_store::system_clocks().0;
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(debug)?.as_millis() as u64;
    let reading = owner.read_persisted(wall, elapsed).ok_or("Couldn't persist the discovery clock.")?;
    if reading.new_generation && vault.ensure_loaded(reading.epoch) { vault.bump(reading.epoch); }
    Ok(reading.epoch)
}

fn entry(vault: &mut meshlive::presence::Vault, node: &[u8; 16], epoch: u32) -> Result<OpeningEntry, String> {
    let state = vault.announcing(epoch).0;
    let key = state.keys.get(&meshlive::presence::day_of(epoch)).ok_or("Presence keys aren't ready.")?;
    OpeningEntry::own(key, node, epoch, meshlive::rotating::ROLE_VIEWER_REQUEST).map_err(debug)
}

fn run(dir: PathBuf, selected: Sighting, stop: &Arc<AtomicBool>, muted: &AtomicBool, out: &Sender<Update>) -> Result<(), String> {
    meshlive::callee::policy(&mut |_, _| {}).map_err(str::to_owned)?;
    let _lease = meshlive::callee::AnnouncementLease::acquire().ok_or("MeshLive is already announcing in another window.")?;
    let (identity, _) = meshlive::identity::Identity::open_or_create().map_err(debug)?;
    let node = meshlive::crypto::sha256(&identity.public_key().map_err(debug)?).map_err(debug)?[..16].try_into().unwrap();
    let mut epochs = meshlive::epoch_store::EpochOwner::open(&dir, meshlive::epoch_store::system_clocks().1);
    let mut vault = meshlive::presence::Vault::open_default(&dir);
    let mask = stream_watch::decode_mask();
    if let Some(reason) = stream_watch::unavailable(&selected.announcement, mask) { return Err(reason.into()); }
    let idle_scan = stream_watch::idle_scan_enabled();
    let route_retry = stream_watch::route_retry_enabled();
    let mut scanner = Some(Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(debug)?);
    let target = Target::from(&selected.announcement);
    let mut offers = Offers::default(); offers.observe(selected);
    let mut pin = None;
    let mut recovery: Option<(zeroize::Zeroizing<[u8; 32]>, [u8; 16])> = None;
    let mut unanswered = 0;
    while !stop.load(Ordering::Acquire) {
        if scanner.is_none() { scanner = Some(Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(debug)?); }
        let now = Instant::now();
        offers.prune(now);
        match scanner.as_ref().unwrap().receive(now + Duration::from_millis(20), stop) {
            Ok(s) => if target.matches(&s.announcement) { offers.observe(s); },
            Err(meshlive::bluetooth::Error::Timeout) => {},
            Err(error) => return Err(format!("Bluetooth search stopped. {error:?}")),
        }
        let chosen = offers.next_route(target, mask, pin.is_some(), Instant::now()).cloned();
        let Some(chosen) = chosen else { continue };
        let e = epoch(&mut epochs, &mut vault)?;
        let mut current = entry(&mut vault, &node, e)?;
        let mut previous: Option<(OpeningEntry, Instant)> = None;
        let make_request = |e: &OpeningEntry| {
            let hint = recovery.as_ref().map(|(key, failed)| advertise::recovery_hint(key, failed, target.logical, &e.rdi, chosen.announcement.token)).transpose().map_err(debug)?;
            let request = stream_watch::request(&chosen.announcement, e, hint)?;
            meshlive::pc_announcement::wrap(1, &request).map_err(debug)
        };
        // Peripheral::close cancels its own carrier. A lost/no-answer attempt
        // must not cancel the logical Watch and its later route attempts.
        let mut peripheral = Peripheral::create_stream(make_request(&current)?, Arc::new(AtomicBool::new(false))).map_err(debug)?;
        peripheral.start().map_err(debug)?;
        let _ = out.send(Update::AdvertisingProof(Ok(())));
        let requested = Instant::now();
        let mut accepted = false;
        while !stop.load(Ordering::Acquire) && requested.elapsed() < Duration::from_secs(15) {
            if peripheral.has_client() { accepted = true; break; }
            let e = epoch(&mut epochs, &mut vault)?;
            if e != current.epoch {
                peripheral.stop_announcement();
                previous = Some((current.clone(), Instant::now()));
                let random = u32::from_be_bytes(meshlive::crypto::random_nonce().map_err(debug)?[..4].try_into().unwrap());
                wait(stop, Duration::from_millis(advertise::boundary_silence_ms(random)));
                current = entry(&mut vault, &node, e)?;
                if !stop.load(Ordering::Acquire) { peripheral.start_announcement(Form::PcStream(make_request(&current)?)).map_err(debug)?; }
            }
            wait(stop, Duration::from_millis(20));
        }
        if stop.load(Ordering::Acquire) { return Ok(()); }
        if !accepted {
            unanswered += 1;
            if route_retry { offers.suppress(&chosen, Instant::now(), Duration::from_secs(1)); }
            #[cfg(test)] eprintln!("PHONE_CHECK watch_unanswered cost={} retry_route={route_retry}", chosen.announcement.cost);
            if unanswered >= 2 { let _ = out.send(Update::WatchNote(Some("Couldn't join. Update MeshLive on that phone, and turn on Find PCs and PC live streams.".into()))); }
            continue;
        }
        let mut entries = vec![current.clone()];
        if let Some((old, off_air)) = previous.filter(|(_, at)| at.elapsed() < Duration::from_secs(60)) { let _ = off_air; entries.push(old); }
        let joined = stream_watch::join(&mut peripheral, &identity, &chosen.announcement, &entries, epoch(&mut epochs, &mut vault)?, mask, pin, stop);
        let result = match joined {
            Ok(mut joined) => {
                if idle_scan { scanner = None; }
                peripheral.stop_announcement();
                let r = play(&mut peripheral, &mut joined, &mut pin, chosen.announcement.cost, stop, muted, out);
                if pin.is_some() { unanswered = 0; recovery = joined.recovery_key.take().map(|key| (key, joined.parent)); }
                r
            }
            Err(error) => { let _ = out.send(Update::WatchNote(Some(format!("Joining again. {error}")))); Ok(End::Lost) }
        };
        peripheral.close();
        let end = match result { Ok(end) => end, Err(error) => {
            let _ = out.send(Update::WatchNote(Some(format!("Signal lost. {error}")))); End::Lost
        }};
        if matches!(end, End::Ended) { let _ = out.send(Update::Watch(WatchEvent::Ended)); return Ok(()); }
        if stop.load(Ordering::Acquire) { return Ok(()); }
        let _ = out.send(Update::Watch(WatchEvent::Lost));
        let delay = match end { End::Marker => 120, End::Withdrawn => 30, _ => 1 };
        offers.suppress(&chosen, Instant::now(), Duration::from_secs(delay));
    }
    Ok(())
}

enum End { Ended, Withdrawn, Lost, Marker }

fn send_watch_uplink(p: &mut impl Carrier, bytes: &[u8], last_up: &mut Instant, window_seconds: Option<u64>) -> Result<(), String> {
    let now = Instant::now();
    let deadline = if let Some(seconds) = window_seconds {
        *last_up + Duration::from_secs(seconds)
    } else { now + Duration::from_secs(1) };
    p.send(bytes, deadline)?;
    if window_seconds.is_some() { *last_up = Instant::now(); }
    Ok(())
}

fn play(p: &mut Peripheral, joined: &mut stream_watch::Joined, pin: &mut Option<[u8; 16]>, hops: u8,
    stop: &Arc<AtomicBool>, muted: &AtomicBool, out: &Sender<Update>) -> Result<End, String> {
    let started = Instant::now(); let mut last_data = started; let mut last_up = started;
    let uplink_window = match std::env::var("MESHLIVE_WATCH_UPLINK_BUDGET").as_deref() {
        Ok("6") => Some(6), // PROTOCOL: GATT direct viewer or receiver-node child.
        Ok("1") => Some(8), // Retained earlier experiment; the parent's six-second bound still applies.
        _ => None,
    };
    let send_periodic = |p: &mut Peripheral, bytes: &[u8], last_up: &mut Instant| {
        #[cfg(test)] let sent = Instant::now();
        let result = send_watch_uplink(p, bytes, last_up, uplink_window);
        #[cfg(test)] crate::phone_checks::watch_uplink_result(sent.elapsed().as_millis() as u64, result.is_err());
        result
    };
    let mut clock = meshlive::playout::Profile::Auto.clock();
    let mut audio = meshlive::listen::AudioReceiver::<meshlive::opus::Decoder>::default();
    let mut video: Option<meshlive::viewer::VideoReceiver> = None;
    let mut decoder = None;
    let pictures = Arc::new(meshlive::video::PictureSlot::default());
    let mut reports = stream_watch::Reports::default(); let mut meter = meshlive::call_rate::ArrivalMeter::default();
    let mut received = 0; let mut state = stream_watch::PlaybackState::default(); let mut radio = false; let mut recoveries = 0;
    let mut has_audio = true;
    let mut requested_key = started.checked_sub(Duration::from_secs(2)).unwrap_or(started);
    let mut pcm = vec![0i16; meshlive::opus::Decoder::MAX_SAMPLES];
    // Drop sound before joining the video decoder on every return path.
    let sound = meshlive::call_audio::CallAudio::start_watch(meshlive::call_audio::gain_from_env(), stop.clone());
    while !stop.load(Ordering::Acquire) {
        p.service().map_err(debug)?;
        let now_ns = started.elapsed().as_nanos() as i64;
        for (at, bytes) in p.take_arrivals() { meter.on_arrival(at.saturating_duration_since(started).as_nanos() as u64, bytes); }
        if let (Some(video), Some(decoder)) = (video.as_mut(), decoder.as_mut()) {
            if video.feed_due(decoder, now_ns, &clock, 20_000_000).is_err() {
                recoveries += 1; if recoveries > 2 { return Err("The video decoder couldn't recover.".into()); }
                video.recover().map_err(debug)?;
            }
        }
        if !state.paused {
            audio.pump(now_ns, 20_000_000, &mut pcm, |_, pcm| {
                #[cfg(test)]
                { crate::phone_checks::media_received(); crate::phone_checks::AUDIO_SAMPLES.fetch_add(pcm.len() as u64, Ordering::Relaxed); }
                sound.play(if muted.load(Ordering::Acquire) { vec![0; pcm.len()] } else { pcm.to_vec() });
            }).map_err(debug)?;
        }
        for _ in 0..32 {
            if video.as_ref().is_some_and(|v| !v.can_accept()) { break; }
            let mut input = joined.buffer.as_slice();
            let event = joined.reader.read(&mut input, started.elapsed().as_millis() as u64);
            let used = joined.buffer.len() - input.len(); joined.buffer.drain(..used);
            let event = match event { Err(meshlive::watch::Error::Marker) => return Ok(End::Marker), other => other.map_err(debug)? };
            let Some(event) = event else { break };
            last_data = Instant::now();
            match event {
                Event::Header(header) => {
                    #[cfg(test)] crate::phone_checks::WATCH_HEADER_AUDIO.store(u64::from(match header { Header::Audio { .. } => true, Header::Video(h) => h.opus }), Ordering::Release);
                    #[cfg(test)] if hops > 0 && joined.source != joined.parent {
                        crate::phone_checks::WATCH_DISTINCT_RELAY_HEADERS.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Header::Video(h) = header {
                        let bit = meshlive::video::codecs::hello_bit(h.codec);
                        if joined.codec_mask & bit == 0 { return Err("The authenticated stream codec wasn't offered or decodable.".into()); }
                    }
                    *pin = Some(joined.source);
                    let _ = out.send(Update::WatchNote(None));
                    let _ = out.send(Update::Watch(WatchEvent::Connected));
                    let _ = out.send(Update::WatchRoute(hops));
                    if let Some(title) = joined.title_after_header() { let _ = out.send(Update::WatchTitle(title)); }
                    match header {
                        Header::Audio { .. } => { radio = true; clock = meshlive::playout::Profile::Radio.clock(); },
                        Header::Video(header) => {
                            has_audio = header.opus;
                            pictures.set_turns(header.quarter_turns);
                            video = Some(meshlive::viewer::VideoReceiver::new(header));
                            let slot = pictures.clone();
                            decoder = Some(meshlive::viewer::DecodeThread::media_foundation(header, move |pts, frame| {
                                match frame.nv12().and_then(|frame_nv12| frame_nv12.read_rgba(frame.format.colour, frame.format.range)) {
                                    Ok((w, h, rgba)) => {
                                        #[cfg(test)]
                                        { crate::phone_checks::media_received(); crate::phone_checks::VIDEO_FRAMES.fetch_add(1, Ordering::Relaxed); }
                                        slot.publish(w, h, rgba, pts);
                                    }, Err(_) => slot.read_failed(),
                                }
                            }));
                            let _ = out.send(Update::CallPictures(PictureHandle(pictures.clone())));
                            // Design AD: whether a dav1d fallback was checked or used (MF stays first).
                            if header.codec == 3 { crate::radio_backend::trace("av1_decoder", &meshlive::video::dav1d::summary()); }
                        }
                    }
                }
                Event::Media(packet) => {
                    let packet = packet.packet(); received += packet.payload.len() as u64;
                    if packet.flags & meshlive::media::ROTATION != 0 { pictures.set_turns(packet.rotation().map_err(debug)?); }
                    else if !state.paused && packet.flags & meshlive::media::AUDIO != 0 {
                        audio.on_packet(packet, now_ns, &mut clock, |csd| meshlive::opus::Decoder::new(csd, meshlive::opus::Profile::StreamOrCall)).map_err(debug)?;
                    } else if !state.paused { if let Some(video) = video.as_mut() {
                        video.on_packet(packet, now_ns, &mut clock).map_err(debug)?;
                        // Design HC: au_reject / au_unusual lines.
                        for line in video.take_au_log() { crate::radio_backend::trace("video_au", &line); }
                    } }
                }
                Event::State(3) => {
                    sound.stop_now();
                    p.send(&joined.uplink.end_ack().map_err(debug)?, Instant::now() + Duration::from_millis(300))
                        .map_err(|e| { #[cfg(test)] eprintln!("PHONE_CHECK watch_uplink_failed stage=end_ack"); e })?;
                    return Ok(End::Ended);
                }
                Event::State(value @ (1 | 2 | 4..=6)) => {
                    state = stream_watch::PlaybackState::from_record(value).ok_or("Invalid stream state.")?;
                    if state.paused { audio.clear_queued(); if let Some(v) = video.as_mut() { v.overflow(); } }
                    let _ = out.send(Update::Watch(WatchEvent::SourceState(state)));
                }
                Event::Withdrawn => {
                    sound.stop_now();
                    p.send(&joined.uplink.withdrawal_ack().map_err(debug)?, Instant::now() + Duration::from_millis(300))
                        .map_err(|e| { #[cfg(test)] eprintln!("PHONE_CHECK watch_uplink_failed stage=withdrawal_ack"); e })?;
                    return Ok(End::Withdrawn);
                }
                Event::State(_) => {}
            }
        }
        let added = (clock.current_delay_ms() - if radio { 600 } else { 250 }).clamp(0, 65535) as u16;
        let rate = meter.kbps(now_ns as u64);
        if let Some(report) = reports.next(started.elapsed().as_millis() as u64, meter.trains(), received, rate, clock.is_saturated(now_ns), added) {
            send_periodic(p, &joined.uplink.heartbeat(Some(&report)).map_err(debug)?, &mut last_up)?; last_up = Instant::now();
        } else if last_up.elapsed() >= Duration::from_secs(2) {
            send_periodic(p, &joined.uplink.heartbeat(None).map_err(debug)?, &mut last_up)?; last_up = Instant::now();
        }
        if !radio && !state.paused && !state.camera_off && video.as_ref().is_some_and(|v| v.awaiting_keyframe()) && requested_key.elapsed() >= Duration::from_secs(2) {
            send_periodic(p, &joined.uplink.keyframe_request().map_err(debug)?, &mut last_up)?; requested_key = Instant::now();
        }
        if state.timed_out(radio, has_audio, last_data.elapsed()) { return Ok(End::Lost); }
        if joined.buffer.len() > meshlive::media::MAX_PAYLOAD + 4096 { return Err("Stream ingress limit.".into()); }
        if video.as_ref().is_none_or(|v| v.can_accept()) {
            if p.receive(&mut joined.buffer, Duration::from_millis(10))? { last_data = Instant::now(); }
        } else { wait(stop, Duration::from_millis(5)); }
    }
    Ok(End::Lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshlive::{framing::HopReader, records::{Layer, Opener, Policy, Sealer}, session::{Control, SourceControl, ViewerUplink}};
    struct DelayedUplink { ready: Instant, sent: Vec<Vec<u8>> }
    impl Carrier for DelayedUplink {
        fn send(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), String> {
            if deadline < self.ready { return Err("Timeout".into()); }
            self.sent.push(bytes.to_vec()); Ok(())
        }
        fn receive(&mut self, _: &mut Vec<u8>, _: Duration) -> Result<bool, String> { Ok(false) }
    }
    #[test]
    fn a_slow_uplink_keeps_authenticated_order_only_inside_the_selected_liveness_window() {
        for (window, age, succeeds) in [(None, 2, false), (Some(8), 2, true), (Some(8), 7, false),
            (Some(6), 2, true), (Some(6), 5, false), (Some(8), 5, true)] {
            let now = Instant::now();
            let mut p = DelayedUplink { ready: now + Duration::from_millis(1500), sent: Vec::new() };
            let mut up = ViewerUplink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(), Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let mut source = SourceControl::continue_link(HopReader::new(&[1; 32], Policy::default()).unwrap(), Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let heartbeat = up.heartbeat(None).unwrap();
            let mut last_up = now - Duration::from_secs(age);
            let result = send_watch_uplink(&mut p, &heartbeat, &mut last_up, window);
            assert_eq!(result.is_ok(), succeeds);
            if succeeds {
                assert!(matches!(source.read(&mut p.sent[0].as_slice()).unwrap(), Some(Control::Heartbeat(None))));
                // Continue the same sealed sequence; no retry or reseal of the slow heartbeat.
                p.ready = Instant::now();
                p.send(&up.end_ack().unwrap(), Instant::now() + Duration::from_millis(300)).unwrap();
                assert!(matches!(source.read(&mut p.sent[1].as_slice()).unwrap(), Some(Control::EndAck)));
            } else { assert!(p.sent.is_empty(), "an expired budget cannot submit a record"); }
        }
    }

    #[test]
    fn a_successful_keyframe_request_refreshes_the_optional_budget_for_the_next_control() {
        for window in [None, Some(6), Some(8)] {
            let now = Instant::now();
            let mut last_up = now - Duration::from_secs(window.unwrap_or(6) - 1);
            let mut p = DelayedUplink { ready: now, sent: Vec::new() };
            let mut up = ViewerUplink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(), Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let mut source = SourceControl::continue_link(HopReader::new(&[1; 32], Policy::default()).unwrap(), Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            send_watch_uplink(&mut p, &up.keyframe_request().unwrap(), &mut last_up, window).unwrap();
            assert!(matches!(source.read(&mut p.sent[0].as_slice()).unwrap(), Some(Control::KeyFrameRequest)));
            p.ready = Instant::now() + Duration::from_millis(1500);
            let result = send_watch_uplink(&mut p, &up.heartbeat(None).unwrap(), &mut last_up, window);
            assert_eq!(result.is_ok(), window.is_some());
            if result.is_ok() {
                assert!(matches!(source.read(&mut p.sent[1].as_slice()).unwrap(), Some(Control::Heartbeat(None))));
                p.ready = Instant::now();
                p.send(&up.end_ack().unwrap(), Instant::now() + Duration::from_millis(300)).unwrap();
                assert!(matches!(source.read(&mut p.sent[2].as_slice()).unwrap(), Some(Control::EndAck)));
            } else { assert_eq!(p.sent.len(), 1, "no retry or reseal after the baseline deadline fails"); }
        }
    }
}
