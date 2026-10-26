//! Shared by the call tools (`call_phone`, `answer_phone`): logging, the
//! app's stores for the in-call lease exchange, and the in-call loop (media,
//! presence, a `stats` line every second, hang-up after `seconds`).
#![allow(dead_code)]
use meshlive::{
    call::Reason,
    call_session::Notice,
    call_video::CallVideo,
    dial::{self, Dialed},
    generated::tone_20ms,
    media::{AUDIO, CONFIG, Packet},
    opus,
    video::{Backend, DecodeDevice, MfFrames},
    viewer::DiscardFrames,
};
use std::{cell::Cell, rc::Rc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
pub fn log(phase: &str, detail: &str) {
    println!("t={} phase={phase} {detail}", now_ms());
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}
pub type Res<T> = Result<T, String>;
pub fn e<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

/// The app's contact and presence stores for the in-call lease exchange.
pub struct Stores {
    pub contacts: meshlive::contacts::ContactStore,
    pub vault: meshlive::presence::Vault,
    pub claims: meshlive::claims::ClaimsStore,
    pub epoch: u32,
}
impl Stores {
    pub fn open() -> Res<Self> {
        let dir = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir).ok_or("no data folder")?;
        let mut vault = meshlive::presence::Vault::open_default(&dir);
        let epoch = dial::wall_epoch().ok_or("clock before 1970")?;
        log("presence_store", &format!("persistent={}", vault.ensure_loaded(epoch)));
        Ok(Self {
            contacts: meshlive::contacts::ContactStore::open_default(&dir),
            claims: meshlive::claims::ClaimsStore::open_default(&dir),
            vault,
            epoch,
        })
    }
    pub fn hooks(&mut self) -> meshlive::verify_scan::StoreHooks<'_> {
        meshlive::verify_scan::StoreHooks {
            contacts: &mut self.contacts,
            vault: &mut self.vault,
            claims: Some(&mut self.claims),
            epoch: self.epoch,
            now_ms: now_ms() as i64,
        }
    }
}

pub struct CallOptions<'a> {
    /// Decode the peer's video natively (counted, not shown).
    pub video: bool,
    pub owner: Option<&'a DecodeDevice>,
    pub video_backend: Option<String>,
    /// Media seconds before this side hangs up.
    pub seconds: u64,
    /// Dial start (caller) or subscriber time (callee), for the timings.
    pub started: Instant,
    /// This PC placed the call (a callee never cancels for no answer).
    pub outgoing: bool,
    pub stores: Option<Stores>,
    /// Notices already produced (a callee's Accept returns StartMedia).
    pub initial: Vec<Notice>,
}

/// The call from the first record to the hang-up.
pub fn run_call(dialed: &mut Dialed, options: CallOptions) -> Res<()> {
    let CallOptions { video, owner, video_backend, seconds, started: dial_started, outgoing, mut stores, initial } = options;
    let mut pending = initial;
    let video_backend = video_backend.as_deref();
    let mut leases = meshlive::call_presence::CallPresence::new();
    let mut leases_logged = 0;
    if let (Some(s), Some((node, fingerprint))) = (stores.as_mut(), dialed.peer_real) {
        for record in leases.identified(node, fingerprint, &mut s.hooks()) {
            dialed.session.send_discovery(record);
        }
    }
    // A callee rang when it was answered; a caller learns it from Ringing.
    let mut rang: Option<Instant> = if outgoing { None } else { Some(Instant::now()) };
    let start = dialed.start;
    let clock = || start.elapsed().as_nanos() as u64;
    let mut pictures: Option<CallVideo> = None;
    if let Some(choice) = video_backend {
        dialed.video_plan.retain(|(_, b)| match choice {
            "hardware" => *b == Backend::Hardware,
            _ => *b != Backend::Hardware,
        });
        log("video_backend", &format!("choice={choice} encoders={}", dialed.video_plan.len()));
    }
    // The phone's video: decoded (counted) in a video call, discarded otherwise.
    let decoded = Rc::new(Cell::new(0u64));
    let counter = decoded.clone();
    let mut mf: Option<MfFrames<_>> = None;
    let mut discard = DiscardFrames::default();
    let mut decode_failed = false;
    let mut next_stats = Instant::now() + Duration::from_secs(1);
    let mut encoder = opus::Encoder::new().map_err(e)?;
    // Real capture, only when asked for.
    let use_camera = std::env::args().any(|a| a == "--camera");
    // The first capture endpoint that delivers signal (virtual or silent
    // ones are skipped); a device change reopens it without ending the call.
    // `--mic-device ID` (an id from `capture_check --list`) is the user's pick.
    // Otherwise the communications default; a switch happens only on a
    // device event (default changed, device removed), never on silence.
    let mic_device: Option<String> = std::env::args().skip_while(|a| a != "--mic-device").nth(1);
    let open_mic = || match meshlive::audio_devices::open_best_microphone(Duration::from_secs(1), mic_device.as_deref()) {
        Ok((m, tried)) => {
            for line in tried {
                log("mic_probe", &line);
            }
            log("mic_start", &format!("endpoint={}", m.endpoint_id));
            Some(m)
        }
        Err((verdict, tried)) => {
            for line in tried {
                log("mic_probe", &line);
            }
            log("mic_unavailable", &verdict);
            None
        }
    };
    let want_mic = std::env::args().any(|a| a == "--mic");
    let mut mic = if want_mic { open_mic() } else { None };
    let devices = if want_mic { meshlive::audio_devices::DeviceWatch::start().ok() } else { None };
    let mut media_from: Option<Instant> = None;
    let mut audio_frame = 0u32;
    let mut hung_up = false;
    let mut heard = 0usize;
    let mut pcm = [0i16; opus::Decoder::MAX_SAMPLES];
    loop {
        dialed.flush().map_err(|error| error.to_string())?;
        if hung_up {
            log("closed", "hang-up flushed");
            break;
        }
        if let Some(from) = media_from {
            // One 20 ms Opus frame per 20 ms of wall time: the microphone with
            // --mic, else a quiet generated 440 Hz tone (-40 dBFS).
            if devices.as_ref().is_some_and(|d| d.take()) {
                log("mic_reopen", "reason=audio device change");
                mic = open_mic();
            }
            while u128::from(audio_frame) * 20 < from.elapsed().as_millis() {
                let pcm = match mic.as_mut() {
                    Some(m) => match m.next_frame(Instant::now() + Duration::from_millis(40)) {
                        Ok(frame) => frame,
                        Err(_) => break,
                    },
                    None => tone_20ms(audio_frame, 440.0, 330.0),
                };
                let packet = encoder.encode(&pcm).map_err(e)?;
                let pts = i64::from(audio_frame) * 20_000;
                let _ = dialed.session
                    .send(
                        Packet {
                            flags: AUDIO,
                            pts_us: pts,
                            payload: &packet,
                        }
                        .encode()
                        .map_err(e)?,
                    )
                    .map_err(e)?;
                audio_frame += 1;
            }
            if let Some(pictures) = pictures.as_mut() {
                pictures.tick(&mut dialed.session).map_err(e)?;
                if let Some(line) = pictures.rate_tick(&dialed.session) {
                    log("rate", &line);
                }
            }
            if from.elapsed() >= Duration::from_secs(seconds) {
                dialed.session.hang_up(Reason::Normal).map_err(e)?;
                let (sent, refused, dropped) =
                    pictures.as_ref().map_or((0, 0, 0), |p| (p.sent, p.refused, p.dropped));
                log(
                    "hang_up",
                    &format!(
                        "audio_frames_sent={audio_frame} pcm_samples_heard={heard} video_packets_sent={sent} video_packets_refused={refused} video_packets_dropped={dropped} peer_frames_decoded={} peer_inputs_discarded={}",
                        decoded.get(),
                        discard.inputs
                    ),
                );
                for fact in dialed.link.stack_facts() {
                    log("stack_fact", &fact);
                }
                hung_up = true;
                continue;
            }
        }
        {
            let mut notices = std::mem::take(&mut pending);
            notices.extend(dialed.pump_input(Duration::from_millis(5)).map_err(|error| {
                log("link_lost", &dialed.link.loss_context());
                error.to_string()
            })?);
            if let Some(s) = stores.as_mut() {
                let mut out = Vec::new();
                for notice in &notices {
                    if let Notice::PeerRevealed { node_id, fingerprint } = notice {
                        out.extend(leases.identified(*node_id, *fingerprint, &mut s.hooks()));
                    }
                }
                for record in dialed.session.take_presence_records() {
                    out.extend(leases.on_record(&record, &mut s.hooks()));
                }
                if dialed.session.answered() {
                    out.extend(leases.answered(&mut s.hooks()));
                }
                for record in out {
                    dialed.session.send_discovery(record);
                }
                for line in &leases.log[leases_logged..] {
                    log("presence", line);
                }
                leases_logged = leases.log.len();
            }
            for message in dialed.session.take_remote() {
                if message.signal == meshlive::call::Signal::Ringing && rang.is_none() {
                    rang = Some(Instant::now());
                    log("ringing", &format!("dial_to_ring_ms={}", dial_started.elapsed().as_millis()));
                }
            }
            for notice in notices {
                log("notice", &format!("{notice:?}"));
                if let Notice::StartMedia { .. } = notice {
                    log(
                        "active",
                        &format!(
                            "ring_to_active_ms={} dial_to_active_ms={}",
                            rang.map_or(-1, |r| r.elapsed().as_millis() as i64),
                            dial_started.elapsed().as_millis()
                        ),
                    );
                }
                match notice {
                    Notice::StartMedia { camera } => {
                        dialed.session.media_ready().map_err(e)?;
                        if let (true, Some(owner)) = (camera, owner) {
                            match CallVideo::open(owner, &dialed.header, &dialed.video_plan) {
                                Ok((mut p, name)) => {
                                    log(
                                        "video_start",
                                        &format!("encoder=\"{name}\" codec={} source={}", dialed.header.codec, if use_camera { "camera" } else { "generated" }),
                                    );
                                    if use_camera {
                                        // The first camera, covered or pointed away; pictures are encoded and dropped.
                                        match meshlive::capture::Camera::open_first(u32::from(dialed.header.width), u32::from(dialed.header.height)) {
                                            Ok(mut cam) => p.use_frames(Box::new(move || cam.next_frame(Instant::now() + Duration::from_millis(100)).ok())),
                                            Err(error) => log("camera_unavailable", &format!("{error:?}")),
                                        }
                                    }
                                    pictures = Some(p);
                                }
                                Err(error) => log("video_unavailable", &format!("{error:?}")),
                            }
                        }
                        let config = encoder.config();
                        dialed.session
                            .send(
                                Packet {
                                    flags: AUDIO | CONFIG,
                                    pts_us: 0,
                                    payload: &config,
                                }
                                .encode()
                                .map_err(e)?,
                            )
                            .map_err(e)?;
                        media_from = Some(Instant::now());
                    }
                    Notice::PeerHeader(header) => {
                        // A video call decodes the peer's pictures natively.
                        if let (true, Some(owner)) = (video, owner) {
                            let counter = counter.clone();
                            mf = Some(MfFrames::new(owner, header, move |_, _| counter.set(counter.get() + 1)));
                        }
                    }
                    Notice::KeyFrameRequested => {
                        if let Some(p) = pictures.as_mut() {
                            p.keyframe_requested(clock());
                        }
                    }
                    Notice::Ended(reason) => {
                        log("ended_by_peer", &format!("{reason:?}"));
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        let now = clock() as i64;
        let pumped = match (mf.as_mut(), decode_failed) {
            (Some(decoder), false) => dialed.session.pump_video(decoder, now, 20_000_000),
            _ => dialed.session.pump_video(&mut discard, now, 20_000_000),
        };
        if let Err(error) = pumped {
            // A local decoder failure never ends the call: discard from now on.
            let reason = mf.as_ref().and_then(|m| m.last_error.clone()).unwrap_or_default();
            log("peer_video_decode_failed", &format!("{error:?} reason=\"{reason}\""));
            decode_failed = true;
        }
        if Instant::now() >= next_stats {
            next_stats += Duration::from_secs(1);
            dialed.link.drain_events(&mut |p, d| log(p, d));
            dialed.link.recheck_preset(&mut |p, d| log(p, d));
            let st = dialed.session.stats();
            // The peer's latest report and this side's measurements (CF-MEDIA-22..25).
            if let Some((_, r)) = dialed.session.take_peer_reports().last() {
                log(
                    "peer_report",
                    &format!(
                        "version={} arrival_kbps={} link_late={} added_delay_ms={} delivery={:?}",
                        if r.delivery.is_some() { 3 } else { 2 },
                        r.arrival_kbps,
                        r.link_late,
                        r.added_delay_ms,
                        r.delivery.map(|d| (d.received_bytes, d.clock_ms))
                    ),
                );
            }
            log(
                "link_measure",
                &format!(
                    "own_arrival_samples={} delivered_kbps={:?} peer_queue_ms={:?} written_bytes={}",
                    dialed.session.arrival().samples(),
                    dialed.session.delivery().delivered_kbps(),
                    dialed.session.delivery().queue_ms(),
                    dialed.session.delivery().written()
                ),
            );
            let (sent, refused, dropped) =
                pictures.as_ref().map_or((0, 0, 0), |p| (p.sent, p.refused, p.dropped));
            log(
                "stats",
                &format!(
                    "audio_sent={} video_sent={} audio_queue={} video_queue={} video_queued_total={sent} video_refused={refused} video_dropped={dropped} rx_video_queue={} rx_overflows={} rx_skipped={} peer_frames_decoded={} peer_inputs_discarded={}",
                    st.audio_sent,
                    st.video_sent,
                    st.audio_queued,
                    st.video_queued,
                    st.video_rx_queued,
                    st.video_overflows,
                    st.video_rx_skipped,
                    decoded.get(),
                    discard.inputs
                ),
            );
        }
        heard += {
            let mut n = 0;
            dialed.session
                .audio()
                .pump(clock() as i64, 0, &mut pcm, |_, s| n += s.len())
                .map_err(e)?;
            n
        };
        if outgoing && media_from.is_none() && start.elapsed() > Duration::from_secs(90) {
            dialed.session.cancel().map_err(e)?;
            log("cancel", "not answered within 90 s");
            hung_up = true;
        }
    }
    drop(pictures);
    drop(mf);
    log("done", "no camera, microphone or audio output was opened");
    Ok(())
}

