//! Explicit QV source worker. Calls remain paused for this task's lifetime.
use crate::ui::{backend::Update, model::{LiveEvent, LiveKind}};
use meshlive::{bluetooth::{advertise, central::Central, peripheral::{Form, Peripheral}, scan::{Scanner, ScanOptions}},
    bmh2::SigningIdentity, live::{Source, Step}, media::{self, Packet}, rotating::OpeningEntry,
    stream_source::{self, Settings}, verify_scan::Carrier};
use std::{path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, Sender, Receiver}}, thread::JoinHandle, time::{Duration, Instant}};

/// Android's BluetoothVideoBroadcaster.CONNECTION_TIMEOUT_MS: one viewer's connect and handshake.
const ADMISSION_BUDGET: Duration = Duration::from_secs(12);

pub enum Control { Pause(bool), Mute(bool), Camera(bool) }
pub struct Task { quit: Arc<AtomicBool>, dial_cancel: Arc<AtomicBool>, dialing: Arc<AtomicBool>, controls: Sender<Control>, thread: Option<JoinHandle<()>> }
impl Task {
    pub fn start(dir: PathBuf, kind: LiveKind, title: String, settings: Settings, initial: stream_source::InitialMedia, out: Sender<Update>) -> Self {
        let quit = Arc::new(AtomicBool::new(false)); let dial_cancel = Arc::new(AtomicBool::new(false)); let dialing = Arc::new(AtomicBool::new(false));
        let (controls, inbox) = mpsc::channel(); let (q, c, d) = (quit.clone(), dial_cancel.clone(), dialing.clone());
        let failed = out.clone();
        let thread = std::thread::Builder::new().name("meshlive-source".into()).spawn(move || {
            if let Err(error) = run(dir, kind == LiveKind::AudioOnly, title, settings, initial, &q, &c, &d, inbox, &out) {
                let note = if error == "AdvertisingAborted" { "Windows couldn't announce this stream. Turn Bluetooth off and on, then try Go live again.".into() } else { format!("Broadcast stopped. {error}") };
                let _ = out.send(Update::LiveNote(Some(note)));
                let _ = out.send(Update::Live(LiveEvent::Failed));
            }
        }).ok();
        if thread.is_none() { let _ = failed.send(Update::Live(LiveEvent::Failed)); }
        Self { quit, dial_cancel, dialing, controls, thread }
    }
    pub fn control(&self, control: Control) {
        if matches!(control, Control::Pause(true)) && self.dialing.load(Ordering::Acquire) { self.dial_cancel.store(true, Ordering::Release); }
        let _ = self.controls.send(control);
    }
    pub fn finished(&self) -> bool { self.thread.as_ref().is_none_or(|t| t.is_finished()) }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        if self.dialing.load(Ordering::Acquire) { self.dial_cancel.store(true, Ordering::Release); }
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
fn debug(e: impl std::fmt::Debug) -> String { format!("{e:?}") }

#[cfg(test)]
fn phone_source_ready(entry: &OpeningEntry) -> Result<(), String> {
    let mode = std::env::var("MESHLIVE_PHONE_CHECK").unwrap_or_default();
    if !matches!(mode.as_str(), "source-radio" | "source-video" | "source-camera") { return Ok(()); }
    let label = meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(&entry.rdi)).ok_or("Source label")?.name;
    std::fs::write(format!(".artifacts/phone/{mode}-ready.txt"), format!("ready=true\nlabel={label}\n")).map_err(debug)
}
#[cfg(test)]
fn generated_nv12(settings: Settings, frame: u32) -> Vec<u8> {
    let (w, h) = (settings.width as usize, settings.height as usize);
    let mut pixels = vec![128; w * h * 3 / 2];
    for y in 0..h { for x in 0..w {
        pixels[y * w + x] = if (x + frame as usize * 8) % w < 40 { 210 } else { 40 };
    } }
    pixels
}
fn capture_control(running: Option<bool>, sound: Option<&meshlive::call_audio::CallAudio>, fence: &mut stream_source::CaptureFence) {
    if let (Some(running), Some(sound)) = (running, sound) {
        if running { sound.restart_capture(fence.restart()); }
        else { sound.stop_capture(); }
    }
}
struct Child<C = Central> { carrier: C, control: meshlive::session::SourceControl, buffer: Vec<u8>, node: [u8; 16], last: Instant }
fn entry(vault: &mut meshlive::presence::Vault, node: &[u8; 16], epoch: u32) -> Result<OpeningEntry, String> {
    let state = vault.announcing(epoch).0;
    OpeningEntry::own(state.keys.get(&meshlive::presence::day_of(epoch)).ok_or("Presence keys aren't ready.")?, node, epoch, meshlive::rotating::ROLE_STREAM_OFFER).map_err(debug)
}
fn epoch(owner: &mut meshlive::epoch_store::EpochOwner, vault: &mut meshlive::presence::Vault) -> Result<u32, String> {
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(debug)?.as_millis() as u64;
    let reading = owner.read_persisted(wall, meshlive::epoch_store::system_clocks().0).ok_or("Couldn't persist the discovery clock.")?;
    if reading.new_generation && vault.ensure_loaded(reading.epoch) { vault.bump(reading.epoch); }
    Ok(reading.epoch)
}
fn announce(settings: Settings, radio: bool, current: &OpeningEntry, logical: u32, paused: bool, full: bool) -> Result<Vec<u8>, String> {
    Ok(meshlive::pc_announcement::wrap(2, &stream_source::offer(settings, radio, current, logical, paused, full)?).map_err(debug)?.to_vec())
}
/// Capture closes first. Its synchronous cleanup must not consume the
/// terminal acknowledgement window before ENDED has even been queued.
fn end_after_capture(source: &mut Source<[u8; 16]>, requested: (Instant, u64), fresh_clock: bool,
    stop_capture: impl FnOnce() -> (Instant, u64)) -> Instant {
    let stopped = stop_capture();
    let (at, ns) = if fresh_clock { stopped } else { requested };
    source.end(ns);
    at
}
fn open_encoder<'a>(owner: &'a meshlive::video::DecodeDevice, settings: Settings, out: &Sender<Update>) -> Result<meshlive::video::SourceEncoder<'a>, String> {
    let encoder = meshlive::video::VideoEncoder::open(owner, settings.encoder(), meshlive::video::Backend::Hardware)
        .or_else(|e| if settings.codec == 2 { meshlive::video::VideoEncoder::open(owner, settings.encoder(), meshlive::video::Backend::SoftwareVp9) } else { Err(e) }).map_err(debug)?;
    let _ = out.send(Update::LiveNote(Some(format!("{} · {} × {} · {} fps · {} kbps. One viewer while adapter checks are pending.", encoder.name(), settings.width, settings.height, settings.fps, settings.kbps))));
    Ok(meshlive::video::SourceEncoder::new(encoder, 60))
}
fn run(dir: PathBuf, radio: bool, title: String, settings: Settings, initial: stream_source::InitialMedia, quit: &AtomicBool, dial_cancel: &Arc<AtomicBool>, dialing: &AtomicBool,
    controls: Receiver<Control>, out: &Sender<Update>) -> Result<(), String> {
    if !stream_source::enabled() || !settings.valid() || title.len() > 64 || title.chars().any(char::is_control) { return Err("Invalid live settings.".into()); }
    meshlive::callee::policy(&mut |_, _| {}).map_err(str::to_owned)?;
    let _lease = meshlive::callee::AnnouncementLease::acquire().ok_or("MeshLive is already announcing in another window.")?;
    let generated_video = cfg!(test) && std::env::var("MESHLIVE_PHONE_CHECK").as_deref() == Ok("source-video");
    let single_radio = cfg!(test) && radio && std::env::var("MESHLIVE_PHONE_RADIO_SINGLE").as_deref() == Ok("1");
    #[cfg(not(test))]
    let (identity, _) = meshlive::identity::Identity::open_or_create().map_err(debug)?;
    #[cfg(test)]
    let identity = if matches!(std::env::var("MESHLIVE_PHONE_CHECK").as_deref(), Ok("source-radio" | "source-video" | "source-camera")) {
        meshlive::identity::Identity::ephemeral()
    } else { meshlive::identity::Identity::open_or_create().map(|(identity, _)| identity) }.map_err(debug)?;
    let node = meshlive::crypto::sha256(&identity.public_key().map_err(debug)?).map_err(debug)?[..16].try_into().unwrap();
    let stream: [u8; 16] = meshlive::crypto::random_nonce().map_err(debug)?[..16].try_into().unwrap();
    let logical = u32::from_be_bytes(stream[..4].try_into().unwrap()) & 0x7fff_ffff;
    if logical == 0 { return Err("Couldn't create a stream token.".into()); }
    let owner = if radio { None } else { Some(meshlive::video::device(false, false).and_then(|(gpu, _, _)| meshlive::video::DecodeDevice::new(&gpu)).map_err(debug)?) };
    let mut encoder = if radio { None } else { Some(open_encoder(owner.as_ref().unwrap(), settings, out)?) };
    let camera_fence = stream_source::camera_fence_enabled();
    if camera_fence && initial.camera_off { encoder = None; }
    let mut opus = if radio { meshlive::opus::Encoder::radio(settings.radio_preset) } else { meshlive::opus::Encoder::new() }.map_err(debug)?;
    let mut signer = if radio { Some(meshlive::audio_live::Signer::new(&identity, stream, settings.radio_preset).map_err(debug)?) } else { None };
    let header = if let Some(signer) = signer.as_ref() { signer.header().to_vec() } else { settings.video_header().encode().map_err(debug)?.to_vec() };
    // W6 has not established an aggregate capacity. Restrict this experiment
    // to its first child; this reservation is a planned load, not a measurement.
    let planned = if radio { 80 } else { settings.kbps + 64 };
    let mut source = Source::new(header, planned).ok_or("Invalid stream budget.")?;
    if !radio { source.mute_audio(initial.muted, 0); }
    let media_notices = !radio && stream_source::media_state_enabled();
    if media_notices { source.set_media_state(initial.muted, initial.camera_off, 0); }
    let config = Packet { flags: 9, pts_us: 0, payload: &opus.config() }.encode().map_err(debug)?;
    let config = if let Some(signer) = signer.as_mut() { signer.packet(Packet::decode(&config, media::MAX_PAYLOAD).map_err(debug)?).map_err(debug)? } else { config };
    source.publish(config, 0).map_err(debug)?;
    let mut epochs = meshlive::epoch_store::EpochOwner::open(&dir, meshlive::epoch_store::system_clocks().1);
    let mut vault = meshlive::presence::Vault::open_default(&dir);
    let e = epoch(&mut epochs, &mut vault)?;
    let mut current = entry(&mut vault, &node, e)?; let mut previous: Option<(OpeningEntry, Instant)> = None;
    let mut tokens = advertise::TokenGrace::default(); let born = Instant::now();
    let mut provider_cancel = Arc::new(AtomicBool::new(false));
    let mut provider = Peripheral::create_stream(announce(settings, radio, &current, logical, false, false)?.try_into().unwrap(), provider_cancel.clone()).map_err(debug)?;
    provider.start().map_err(debug)?;
    #[cfg(test)] phone_source_ready(&current)?;
    tokens.advertise(advertise::route_token(logical, &current.rdi).map_err(debug)?, 0);
    let idle_scan = meshlive::stream_watch::idle_scan_enabled();
    let mut scanner = Some(Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(debug)?);
    let mut sound = Some(meshlive::call_audio::CallAudio::start(0.0));
    let mut camera = (!radio && !initial.camera_off && !generated_video).then(|| meshlive::call_camera::CallCamera::start(settings.width, settings.height));
    #[cfg(test)] let mut generated_frame = 0u32;
    let mut child: Option<Child> = None; let mut paused = false; let mut muted = initial.muted; let mut camera_on = !initial.camera_off;
    let mut audio_batch = meshlive::audio_live::Batcher::new(std::env::var("MESHLIVE_RADIO_BATCH10").as_deref() == Ok("1"));
    audio_batch.mute(muted);
    let mut capture_fence = stream_source::CaptureFence::default();
    let mut capture = stream_source::CaptureLifecycle::new(radio);
    let mut audio_pts = 0; let mut next_picture = Instant::now();
    let mut requests = meshlive::viewer_admission::Requests::default(); let mut full = false; let mut window = stream_source::OfferWindow::default(); let mut ended = None;
    let fresh_end_clock = std::env::var("MESHLIVE_SOURCE_END_CLOCK").as_deref() == Ok("1");
    let ready_writes = std::env::var("MESHLIVE_SOURCE_READY_WRITES").as_deref() == Ok("1");
    let _ = out.send(Update::AdvertisingProof(Ok(()))); let _ = out.send(Update::Live(LiveEvent::Started));
    loop {
        let now = Instant::now(); let ns = born.elapsed().as_nanos() as u64;
        if quit.load(Ordering::Acquire) && ended.is_none() {
            ended = Some(end_after_capture(&mut source, (now, ns), fresh_end_clock, || {
                camera = None; if camera_fence { encoder = None; } sound = None; audio_batch.clear(); provider.stop_announcement();
                (Instant::now(), born.elapsed().as_nanos() as u64)
            }));
        }
        if ended.is_some_and(|at| child.is_none() || at.elapsed() >= Duration::from_secs(5)) { break; }
        let mut changed = false;
        if ended.is_none() {
            for control in controls.try_iter() {
                match control {
                    Control::Pause(value) if value != paused => {
                        paused = value; audio_batch.clear(); changed = true;
                        capture_control(capture.update(paused, child.is_some()), sound.as_ref(), &mut capture_fence);
                        if paused { camera = None; if camera_fence { encoder = None; } source.pause(ns); }
                        else { if !radio && camera_on && !generated_video { camera = Some(meshlive::call_camera::CallCamera::start(settings.width, settings.height)); } audio_pts = born.elapsed().as_micros() as i64; source.resume(ns); }
                    }
                    Control::Mute(value) => {
                        if muted && !value && capture.running() { if let Some(fence) = capture_fence.unmute(paused) { if let Some(s) = sound.as_ref() { s.restart_capture(fence); } } }
                        muted = value; audio_batch.mute(value);
                        if !radio { source.mute_audio(value, ns); } else if value { source.discard_radio_audio(); }
                        if media_notices { source.set_media_state(muted, !camera_on, ns); }
                    }
                    Control::Camera(value) => { camera_on = value; camera = None; if !value && camera_fence { encoder = None; source.discard_video(); } if value && !paused && !radio { if !generated_video { camera = Some(meshlive::call_camera::CallCamera::start(settings.width, settings.height)); } if let Some(e) = encoder.as_mut() { e.request_keyframe(ns, true); } } if media_notices { source.set_media_state(muted, !camera_on, ns); } }
                    _ => {}
                }
            }
            if camera_fence && !radio && !paused && camera_on && encoder.is_none() {
                encoder = Some(open_encoder(owner.as_ref().unwrap(), settings, out)?);
            }
            if let Some(running) = capture.update(paused, child.is_some()) {
                audio_batch.clear();
                capture_control(Some(running), sound.as_ref(), &mut capture_fence);
            }
            if let Some(sound) = sound.as_ref() {
                #[cfg(test)] let capture_started = Instant::now();
                #[cfg(test)] let mut capture_frames = 0;
                for event in sound.events() {
                    match event {
                        meshlive::call_audio::Event::MicUnavailable(reason) => return Err(reason),
                        meshlive::call_audio::Event::CaptureRestarted(fence) => capture_fence.acknowledged(fence),
                        meshlive::call_audio::Event::Frame(mut pcm) if capture.running() => {
                            #[cfg(test)] { capture_frames += 1; }
                            capture_fence.scrub(muted, &mut pcm[..]);
                            let unit = opus.encode(&pcm).map_err(debug)?;
                            if let Some(signer) = signer.as_mut() {
                                let batch = if single_radio { Some(Packet { flags: media::AUDIO, pts_us: audio_pts, payload: &unit }.encode().map_err(debug)?) }
                                    else { audio_batch.push(audio_pts, unit).map_err(debug)? };
                                if let Some(batch) = batch {
                                    source.publish(signer.packet(Packet::decode(&batch, media::MAX_PAYLOAD).map_err(debug)?).map_err(debug)?, ns).map_err(debug)?;
                                }
                            } else { source.publish(Packet { flags: 8, pts_us: audio_pts, payload: &unit }.encode().map_err(debug)?, ns).map_err(debug)?; }
                            audio_pts += 20_000;
                        }
                        _ => {}
                    }
                }
                #[cfg(test)] {
                    crate::phone_checks::SOURCE_MAX_CAPTURE_US.fetch_max(capture_started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    crate::phone_checks::SOURCE_MAX_CAPTURE_FRAMES.fetch_max(capture_frames, Ordering::Relaxed);
                }
            }
            let shed = source.take_radio_shed();
            if shed > 0 {
                crate::radio_backend::trace("audio_live_shed", &format!("removed={shed}"));
                #[cfg(test)] eprintln!("PHONE_CHECK audio_live_shed removed={shed}");
            }
            if let Some(camera) = camera.as_ref() {
                for event in camera.events() { if let meshlive::call_camera::Event::Unavailable(reason) = event { return Err(reason); } }
            }
            if !radio && !paused && camera_on && now >= next_picture {
                    let picture = camera.as_ref().and_then(|camera| camera.latest());
                    #[cfg(test)] let picture = if generated_video {
                        let picture = generated_nv12(settings, generated_frame);
                        generated_frame = generated_frame.wrapping_add(1);
                        Some(picture)
                    } else { picture };
                    if let (Some(picture), Some(encoder)) = (picture, encoder.as_mut()) {
                        encoder.encode_into(&mut source, &picture, born.elapsed().as_micros() as i64, ns, now + Duration::from_millis(200)).map_err(debug)?;
                        #[cfg(test)] if generated_video { crate::phone_checks::SOURCE_GENERATED_FRAMES.fetch_add(1, Ordering::Relaxed); }
                        #[cfg(test)] if !generated_video { crate::phone_checks::SOURCE_CAMERA_FRAMES.fetch_add(1, Ordering::Relaxed); }
                        next_picture = Instant::now() + Duration::from_micros(1_000_000 / u64::from(settings.fps));
                    }
            }
            let e = epoch(&mut epochs, &mut vault)?;
            if e != current.epoch {
                provider.stop_announcement(); previous = Some((current.clone(), Instant::now()));
                let random = u32::from_be_bytes(meshlive::crypto::random_nonce().map_err(debug)?[..4].try_into().unwrap());
                // Silence only the announcement. Existing media keeps running.
                current = entry(&mut vault, &node, e)?;
                // Keep the event loop servicing media during the boundary gap.
                window.rotate(born.elapsed().as_millis() as u64, random); changed = true;
            }
            if full != child.is_some() { full = child.is_some(); changed = true; }
            // No request can gain a slot while the initial viewer occupies it.
            // Reopen discovery only after that link releases the slot.
            if idle_scan {
                if full { scanner = None; }
                else if scanner.is_none() { scanner = Some(Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(debug)?); }
            }
            if changed { provider.stop_announcement(); }
            let rotation_due = window.end_silence(born.elapsed().as_millis() as u64);
            if changed || rotation_due {
                // This initial-viewer experiment has no remaining admission
                // capacity. Keep its offer silent instead of restarting an
                // announcing provider while the admitted link is active.
                if window.can_announce() && !full {
                    provider.start_announcement(Form::PcStream(announce(settings, radio, &current, logical, paused, full)?.try_into().unwrap())).map_err(debug)?;
                    #[cfg(test)] phone_source_ready(&current)?;
                    tokens.advertise(advertise::route_token(logical, &current.rdi).map_err(debug)?, born.elapsed().as_millis() as u64);
                }
            }
            // Never handshake with an unsolicited inbound central. Recreate
            // only this provider after its 15 s slot budget.
            if window.close_inbound(provider.has_client(), born.elapsed().as_millis() as u64) {
                provider.close(); provider_cancel = Arc::new(AtomicBool::new(false));
                provider = Peripheral::create_stream(announce(settings, radio, &current, logical, paused, full)?.try_into().unwrap(), provider_cancel.clone()).map_err(debug)?;
                if window.can_announce() && !full { provider.start().map_err(debug)?; } else { provider.start_silent().map_err(debug)?; }
                window.close_inbound(false, born.elapsed().as_millis() as u64);
            }
            // Collect a bounded scan round before choosing the oldest request.
            // Keep deferred requests available when the global window reopens.
            for _ in 0..512 {
              match scanner.as_ref().map_or(Err(meshlive::bluetooth::Error::Timeout), |scan| scan.receive(Instant::now() + Duration::from_millis(1), quit)) {
                Ok(request) if request.received.elapsed() <= Duration::from_secs(12)
                    && stream_source::accepts(&request.announcement, &tokens, born.elapsed().as_millis() as u64) => {
                    requests.observe(request, Instant::now());
                }
                Ok(_) => {}
                Err(meshlive::bluetooth::Error::Timeout) => break,
                Err(_) if quit.load(Ordering::Acquire) => break,
                Err(error) => return Err(format!("Bluetooth search stopped. {error:?}")),
              }
            }
            if child.is_none() {
                if let Some(request) = requests.next(Instant::now()).filter(|s| stream_source::accepts(&s.announcement, &tokens, born.elapsed().as_millis() as u64)) {
                        dial_cancel.store(false, Ordering::Release); dialing.store(true, Ordering::Release);
                        if quit.load(Ordering::Acquire) { dialing.store(false, Ordering::Release); continue; }
                        requests.attempted(Instant::now());
                        // One budget for connect and handshake, like Android's 12 s receiver
                        // connection timeout, so a silent requester cannot hold admission longer.
                        let admission_deadline = Instant::now() + ADMISSION_BUDGET;
                        let connected = Central::connect_until(&request.route, dial_cancel.clone(), admission_deadline);
                        requests.connected(connected.as_ref().map(|_| ()).map_err(|e| *e), Instant::now());
                        let result = connected.map_err(debug).and_then(|mut carrier| {
                            if std::env::var("MESHLIVE_STREAM_ACK_WRITES").as_deref() == Ok("1") { carrier.acknowledged_writes().map_err(debug)?; }
                            #[cfg(test)]
                            eprintln!("PHONE_CHECK source_link {}", carrier.stack_facts().join(" | "));
                            let mut entries = vec![current.clone()]; if let Some((old, _)) = previous.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(60)) { entries.push(old.clone()); }
                            let admitted = stream_source::admit(&mut carrier, &identity, &request.announcement, &entries, e, settings, radio, stream, &title, admission_deadline, quit)?;
                            source.admit(admitted.node, admitted.link, planned, None, born.elapsed().as_nanos() as u64).map_err(debug)?;
                            Ok(Child { carrier, control: admitted.control, buffer: admitted.buffer, node: admitted.node, last: Instant::now() })
                        });
                        dialing.store(false, Ordering::Release);
                        if let Ok(joined) = result { child = Some(joined); let _ = out.send(Update::Live(LiveEvent::Viewers(1))); }
                        else { requests.failed(&request, Instant::now()); }
                        // Do not transmit stale queued capture accumulated during setup.
                        if let Some(s) = sound.as_ref() { for event in s.events() { match event {
                            meshlive::call_audio::Event::CaptureRestarted(fence) => capture_fence.acknowledged(fence),
                            meshlive::call_audio::Event::MicUnavailable(reason) => return Err(reason),
                            _ => {}
                        } } } audio_pts = born.elapsed().as_micros() as i64; audio_batch.clear();
                }
            }
        }
        source.tick(ns);
        if let Some(active) = child.as_mut() {
            #[cfg(test)] let recovery_started = Instant::now();
            let result = active.carrier.service().map_err(|error| {
                #[cfg(test)] eprintln!("PHONE_CHECK source_io_failed stage=recovery_service elapsed_ms={}", recovery_started.elapsed().as_millis());
                debug(error)
            }).and_then(|_| service(active, &mut source, born));
            #[cfg(test)]
            if result != Ok(true) { eprintln!("PHONE_CHECK source_service={result:?} native_error={:?}", active.carrier.native_error()); }
            if result.is_err() || result == Ok(false) { source.lost(&active.node); child = None; let _ = out.send(Update::Live(LiveEvent::Viewers(0))); }
        }
        let writable = ready_writes && child.as_ref().and_then(|c| source.queue_snapshot(&c.node, born.elapsed().as_nanos() as u64))
            .is_some_and(|q| q.records > 0 && (q.head_audio || q.pace_wait_ns == 0));
        if !writable { std::thread::sleep(Duration::from_millis(2)); }
    }
    provider.close(); Ok(())
}
fn service<C: Carrier>(child: &mut Child<C>, source: &mut Source<[u8; 16]>, born: Instant) -> Result<bool, String> {
    if child.buffer.len() > 4096 { return Err("Oversized viewer control.".into()); }
    for _ in 0..16 {
        let mut input = child.buffer.as_slice(); let control = child.control.read(&mut input).map_err(debug)?;
        let used = child.buffer.len() - input.len(); child.buffer.drain(..used);
        if let Some(control) = control {
            #[cfg(test)]
            if matches!(control, meshlive::session::Control::Heartbeat(Some(_))) { crate::phone_checks::SOURCE_REPORTS.fetch_add(1, Ordering::Relaxed); }
            #[cfg(test)]
            if let meshlive::session::Control::Heartbeat(Some(report)) = &control {
                if let Some(delivery) = report.delivery {
                    crate::phone_checks::SOURCE_DELIVERY_REPORTS.fetch_add(1, Ordering::Relaxed);
                    crate::phone_checks::SOURCE_RECEIVED_BYTES.store(u64::from(delivery.received_bytes), Ordering::Release);
                    crate::phone_checks::SOURCE_RECEIVER_CLOCK_MS.store(u64::from(delivery.clock_ms), Ordering::Release);
                    eprintln!("PHONE_CHECK delivery received_bytes={} clock_ms={} arrival_kbps={} late={} delay_ms={}", delivery.received_bytes, delivery.clock_ms, report.arrival_kbps, report.link_late, report.added_delay_ms);
                }
            }
            #[cfg(test)]
            if matches!(control, meshlive::session::Control::EndAck) { crate::phone_checks::SOURCE_END_ACKS.fetch_add(1, Ordering::Relaxed); }
            child.last = Instant::now(); source.on_control(&child.node, control, born.elapsed().as_nanos() as u64).map_err(debug)?;
        } else {
            // Writes can outlast the heartbeat interval. Consume already
            // queued feedback before deciding that this viewer went silent.
            if !child.carrier.receive(&mut child.buffer, Duration::from_millis(1))? { break; }
            if child.buffer.len() > 4096 { return Err("Oversized viewer control.".into()); }
        }
    }
    if child.last.elapsed() >= Duration::from_secs(6) {
        #[cfg(test)]
        eprintln!("PHONE_CHECK source_close=viewer_control_timeout");
        return Ok(false);
    }
    let now_ns = born.elapsed().as_nanos() as u64;
    #[cfg(test)] let pending = source.queue_snapshot(&child.node, now_ns);
    #[cfg(test)] if let Some(pending) = pending {
        crate::phone_checks::SOURCE_MAX_QUEUE_MS.fetch_max(pending.age_ms, Ordering::Relaxed);
        crate::phone_checks::SOURCE_MAX_QUEUE_RECORDS.fetch_max(pending.records as u64, Ordering::Relaxed);
        crate::phone_checks::SOURCE_MAX_PACE_MS.fetch_max(pending.pace_wait_ns / 1_000_000, Ordering::Relaxed);
    }
    let step = source.step(&child.node, now_ns).map_err(debug)?;
    #[cfg(test)]
    if matches!(step, Step::Withdraw(_)) { eprintln!("PHONE_CHECK source_withdraw=local_capacity queue={pending:?}"); }
    match step {
        Step::Write(bytes) | Step::Withdraw(bytes) => {
            let start = born.elapsed().as_nanos() as u64;
            child.carrier.send(&bytes, Instant::now() + Duration::from_secs(5)).map_err(|error| {
                #[cfg(test)] eprintln!("PHONE_CHECK source_io_failed stage=record_write elapsed_ms={}", (born.elapsed().as_nanos() as u64 - start) / 1_000_000);
                error
            })?;
            #[cfg(test)]
            {
                crate::phone_checks::SOURCE_WRITES.fetch_add(1, Ordering::Relaxed);
                crate::phone_checks::SOURCE_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                crate::phone_checks::SOURCE_WRITE_US.fetch_add((born.elapsed().as_nanos() as u64 - start) / 1000, Ordering::Relaxed);
                crate::phone_checks::SOURCE_MAX_WRITE_US.fetch_max((born.elapsed().as_nanos() as u64 - start) / 1000, Ordering::Relaxed);
            }
            source.written(&child.node, bytes.len(), start, born.elapsed().as_nanos() as u64);
        }
        Step::Close => return Ok(false), _ => {}
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshlive::{framing::HopReader, records::{Layer, Policy, Sealer, Opener}, session::{SourceLink, SourceControl, ViewerUplink}};
    use std::collections::VecDeque;
    #[derive(Default)]
    struct Queued { incoming: VecDeque<Vec<u8>>, written: Vec<Vec<u8>> }
    impl Carrier for Queued {
        fn send(&mut self, bytes: &[u8], _: Instant) -> Result<(), String> { self.written.push(bytes.to_vec()); Ok(()) }
        fn receive(&mut self, bytes: &mut Vec<u8>, _: Duration) -> Result<bool, String> {
            let Some(next) = self.incoming.pop_front() else { return Ok(false) };
            bytes.extend(next); Ok(true)
        }
    }
    #[test]
    fn slow_capture_cleanup_preserves_the_window_for_authenticated_end_ack() {
        use meshlive::watch::{Event, Reader};
        for (fresh, reply) in [(false, "valid"), (true, "valid"), (true, "missing"), (true, "forged")] {
            let requested = (Instant::now(), 1_000_000_000);
            let stopped = (requested.0 + Duration::from_secs(10), 11_000_000_000);
            let node = [5; 16];
            let mut source = Source::new(Settings::default().video_header().encode().unwrap().to_vec(), 1_000).unwrap();
            source.admit(node, SourceLink::new(&[3; 32], &[4; 32]).unwrap(), 300, None, requested.1).unwrap();
            let mut capture_open = true;
            let ended = end_after_capture(&mut source, requested, fresh, || { capture_open = false; stopped });
            assert!(!capture_open, "capture must close before the terminal window starts");
            if !fresh {
                assert!(stopped.0.duration_since(ended) >= Duration::from_secs(5), "the retained arm consumes its grace in cleanup");
                continue;
            }
            assert!(stopped.0.duration_since(ended).is_zero(), "slow cleanup cannot predate ENDED's grace");
            // A valid queued heartbeat keeps this already-admitted viewer alive.
            let mut up = ViewerUplink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(), Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let mut control = SourceControl::continue_link(HopReader::new(&[1; 32], Policy::default()).unwrap(), Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let heartbeat = up.heartbeat(None).unwrap();
            source.on_control(&node, control.read(&mut heartbeat.as_slice()).unwrap().unwrap(), stopped.1).unwrap();
            let mut viewer = Reader::direct(HopReader::new(&[3; 32], Policy::default()).unwrap(), Opener::new(Layer::Endpoint, &[4; 32], Policy::default()).unwrap(), node, false, true);
            let mut now = stopped.1; let mut saw_end = false;
            for _ in 0..8 {
                match source.step(&node, now).unwrap() {
                    Step::Write(bytes) => {
                        source.written(&node, bytes.len(), now, now);
                        match viewer.read(&mut bytes.as_slice(), now / 1_000_000).unwrap() {
                            Some(Event::State(meshlive::live::ENDED)) => { saw_end = true; break; }
                            Some(Event::Header(_)) => {},
                            _ => panic!("Unexpected media after capture stopped"),
                        }
                    }
                    Step::Wait(wait) => now += wait,
                    _ => panic!("ENDED was not delivered"),
                }
            }
            assert!(saw_end);
            let reply_at = stopped.1 + 4_000_000_000;
            if reply != "missing" {
                let mut ack = up.end_ack().unwrap();
                if reply == "forged" { *ack.last_mut().unwrap() ^= 1; }
                let received = control.read(&mut ack.as_slice());
                if reply == "valid" { source.on_control(&node, received.unwrap().unwrap(), reply_at).unwrap(); }
                else { assert!(received.is_err(), "forgery cannot acknowledge End"); }
            }
            if reply == "valid" { assert_eq!(source.step(&node, reply_at).unwrap(), Step::Close); }
            else {
                assert_eq!(source.step(&node, reply_at).unwrap(), Step::Idle);
                assert_eq!(source.step(&node, stopped.1 + meshlive::live::END_GRACE_NS).unwrap(), Step::Close, "the grace remains bounded");
            }
        }
    }
    #[test]
    fn queued_authenticated_feedback_survives_a_long_write_but_silence_or_forgery_does_not() {
        for case in 0..4 {
            let born = Instant::now() - Duration::from_secs(7);
            let node = [5; 16];
            let mut source = Source::new(Settings::default().video_header().encode().unwrap().to_vec(), 1_000).unwrap();
            source.admit(node, SourceLink::new(&[3; 32], &[4; 32]).unwrap(), 300, None, born.elapsed().as_nanos() as u64).unwrap();
            let mut control = ViewerUplink::continue_link(Sealer::new(Layer::Hop, &[1; 32], Policy::default()).unwrap(), Sealer::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let mut heartbeat = control.heartbeat(None).unwrap();
            if case == 3 { *heartbeat.last_mut().unwrap() ^= 1; }
            let incoming = match case {
                0 | 3 => VecDeque::from([heartbeat]),
                1 => VecDeque::from([heartbeat[..9].to_vec(), heartbeat[9..].to_vec()]),
                _ => VecDeque::new(),
            };
            let old = Instant::now() - Duration::from_secs(6);
            let control = SourceControl::continue_link(HopReader::new(&[1; 32], Policy::default()).unwrap(), Opener::new(Layer::Endpoint, &[2; 32], Policy::default()).unwrap());
            let mut child = Child { carrier: Queued { incoming, ..Default::default() }, control, buffer: Vec::new(), node, last: old };
            let result = service(&mut child, &mut source, born);
            match case {
                0 | 1 => {
                    assert_eq!(result, Ok(true), "queued valid feedback must be read before the silent-link deadline");
                    assert!(child.last > old);
                    assert!(!child.carrier.written.is_empty(), "the admitted viewer still receives its stream header");
                }
                2 => { assert_eq!(result, Ok(false)); assert!(child.carrier.written.is_empty()); assert_eq!(child.last, old); }
                _ => { assert!(result.is_err()); assert!(child.carrier.written.is_empty()); assert_eq!(child.last, old, "forgery cannot refresh liveness"); }
            }
        }
    }
}
