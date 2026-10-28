//! Opt-in native phone checks through the normal stream workers. No ADB,
//! device addresses/serials, images, recordings or installed-package operations.
use crate::ui::{backend::Update, model::{LiveEvent, LiveKind, WatchEvent}};
use std::{sync::{atomic::{AtomicU64, Ordering}, mpsc}, time::{Duration, Instant}};

pub static AUDIO_SAMPLES: AtomicU64 = AtomicU64::new(0);
pub static VIDEO_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static WATCH_HEADER_AUDIO: AtomicU64 = AtomicU64::new(2);
pub static WATCH_DISTINCT_RELAY_HEADERS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_WRITES: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_REPORTS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_END_ACKS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_WRITE_US: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_DELIVERY_REPORTS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_RECEIVED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_RECEIVER_CLOCK_MS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_GENERATED_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_CAMERA_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_QUEUE_MS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_QUEUE_RECORDS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_PACE_MS: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_WRITE_US: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_CAPTURE_US: AtomicU64 = AtomicU64::new(0);
pub static SOURCE_MAX_CAPTURE_FRAMES: AtomicU64 = AtomicU64::new(0);
static CHECK_STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static FIRST_MEDIA_MS: AtomicU64 = AtomicU64::new(0);
static LAST_MEDIA_MS: AtomicU64 = AtomicU64::new(0);
static MAX_MEDIA_GAP_MS: AtomicU64 = AtomicU64::new(0);
static MAX_WATCH_UPLINK_MS: AtomicU64 = AtomicU64::new(0);
pub fn watch_uplink_result(ms: u64, failed: bool) {
    if CHECK_STARTED.get().is_none() { return; }
    MAX_WATCH_UPLINK_MS.fetch_max(ms, Ordering::Relaxed);
    if failed { eprintln!("PHONE_CHECK watch_uplink_failed stage=periodic elapsed_ms={ms}"); }
}
pub fn media_received() {
    let Some(born) = CHECK_STARTED.get() else { return };
    let ms = born.elapsed().as_millis() as u64 + 1;
    let last = LAST_MEDIA_MS.swap(ms, Ordering::AcqRel);
    if last == 0 { FIRST_MEDIA_MS.store(ms, Ordering::Release); }
    else { MAX_MEDIA_GAP_MS.fetch_max(ms.saturating_sub(last), Ordering::Relaxed); }
}

struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);
struct Store;
impl Drop for Store { fn drop(&mut self) { meshlive::identity_gate::set_store_dir(None); } }
impl Env {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let before = values.iter().map(|(key, value)| {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value); } (*key, old)
        }).collect(); Self(before)
    }
}
impl Drop for Env {
    fn drop(&mut self) { for (key, value) in &self.0 { unsafe { match value { Some(value) => std::env::set_var(key, value), None => std::env::remove_var(key) } } } }
}

#[test]
fn opt_in_native_stream_phone_check() {
    let Ok(mode) = std::env::var("MESHLIVE_PHONE_CHECK") else { return };
    if mode.starts_with("call-") { return; }
    assert!(matches!(mode.as_str(), "watch-radio" | "watch-video" | "source-radio" | "source-video" | "source-camera" | "stream-offers"));
    let _mta = meshlive::bluetooth::MtaUsage::for_process().expect("Process MTA");
    if mode == "stream-offers" { observe_stream_offers(); return; }
    let seconds = std::env::var("MESHLIVE_PHONE_SECONDS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60).clamp(20, 330);
    let single_radio = std::env::var("MESHLIVE_PHONE_RADIO_SINGLE").as_deref() == Ok("1");
    let require_relay = std::env::var("MESHLIVE_PHONE_RELAY").as_deref() == Ok("1");
    let require_recovery = std::env::var("MESHLIVE_PHONE_RELAY_RECOVERY").as_deref() == Ok("1");
    if require_relay { assert_eq!(mode, "watch-radio", "Relay check requires radio Watch"); }
    if require_recovery {
        assert!(require_relay && std::env::var("MESHLIVE_PHONE_FLOW").as_deref() != Ok("1"),
            "Controlled recovery requires relay Watch, separate from the uninterrupted flow");
    }
    if single_radio { assert!(mode == "source-radio" && meshlive::live::signed_radio_enabled(), "Single-unit radio requires signed-record preservation"); }
    let source_no_labels = std::env::var("MESHLIVE_PHONE_SOURCE_NO_LABELS").as_deref() == Ok("1");
    if source_no_labels { assert_eq!(mode, "source-radio", "Metadata control requires radio source mode"); }
    let mut flags = vec![("MESHLIVE_WATCH", "1"), ("MESHLIVE_GO_LIVE", "1"), ("MESHLIVE_PLAYBACK_GAIN", "0")];
    if source_no_labels { flags.push(("MESHLIVE_LABELS", "0")); }
    let _env = Env::set(&flags);
    for counter in [&SOURCE_CAMERA_FRAMES, &SOURCE_MAX_QUEUE_MS, &SOURCE_MAX_QUEUE_RECORDS, &SOURCE_MAX_PACE_MS,
        &SOURCE_MAX_WRITE_US, &SOURCE_MAX_CAPTURE_US, &SOURCE_MAX_CAPTURE_FRAMES] { counter.store(0, Ordering::Release); }
    for counter in [&AUDIO_SAMPLES, &VIDEO_FRAMES, &SOURCE_WRITES, &SOURCE_REPORTS, &SOURCE_END_ACKS, &SOURCE_WRITE_US, &SOURCE_BYTES, &SOURCE_DELIVERY_REPORTS, &SOURCE_RECEIVED_BYTES, &SOURCE_RECEIVER_CLOCK_MS, &MAX_WATCH_UPLINK_MS, &SOURCE_GENERATED_FRAMES] { counter.store(0, Ordering::Release); }
    WATCH_HEADER_AUDIO.store(2, Ordering::Release);
    WATCH_DISTINCT_RELAY_HEADERS.store(0, Ordering::Release);
    let dir = std::env::current_dir().unwrap().join(".artifacts/phone/host"); std::fs::create_dir_all(&dir).unwrap();
    meshlive::identity_gate::set_store_dir(Some(dir.clone()));
    let _store = Store;
    let (out, updates) = mpsc::channel();
    let born = Instant::now();
    let mut connected = false; let mut paused = false; let mut resumed = false; let mut ended = false; let mut losses = 0;
    let mut viewer_active = false; let mut viewer_since = None;
    let mut watch_states = 0u64;
    let (mut direct_headers, mut relay_headers, mut max_hops) = (0u64, 0u64, 0u8);
    let (mut loss_at_ms, mut recovered_at_ms, mut samples_at_loss, mut samples_at_recovery) = (None, None, None, None);
    let mut camera_off_since: Option<Instant> = None; let mut both_off_since: Option<Instant> = None;
    let mut camera_off_ms = 0u128; let mut both_off_ms = 0u128;
    let mut failure = None;
    let camera_cycle = std::env::var("MESHLIVE_PHONE_CAMERA_CYCLE").as_deref() == Ok("1");
    if camera_cycle { assert!(matches!(mode.as_str(), "source-camera" | "source-video") && meshlive::stream_source::camera_fence_enabled() && meshlive::stream_source::media_state_enabled()); }
    let mut camera_off = false; let mut camera_on = false;
    let mut off_baseline = None; let mut on_baseline = 0; let mut off_inputs = 0;
    let video_inputs = || if mode == "source-camera" { SOURCE_CAMERA_FRAMES.load(Ordering::Acquire) } else { SOURCE_GENERATED_FRAMES.load(Ordering::Acquire) };
    let (watch, source) = if mode.starts_with("watch-") {
        use meshlive::bluetooth::scan::{Scanner, ScanOptions};
        let prefix = std::env::var("MESHLIVE_PHONE_STREAM_RDI").ok().map(|prefix| {
            assert!(prefix.len() == 12 && prefix.bytes().all(|b| b.is_ascii_hexdigit()));
            (0..12).step_by(2).map(|i| u8::from_str_radix(&prefix[i..i+2], 16).unwrap()).collect::<Vec<_>>()
        });
        let token = std::env::var("MESHLIVE_PHONE_STREAM_TOKEN").ok().map(|token| u32::from_str_radix(token.trim_start_matches("0x"), 16).unwrap());
        assert!(prefix.is_some() || token.is_some(), "Select the approved source's current RDI or route token from a controlled observation");
        let scanner = Scanner::start(ScanOptions { rotating: true, compact: false }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30); let stop = std::sync::atomic::AtomicBool::new(false);
        let selected = loop {
            assert!(Instant::now() < deadline, "Approved source wasn't discovered");
            match scanner.receive(Instant::now() + Duration::from_millis(100), &stop) {
                Ok(s) if prefix.as_ref().is_none_or(|p| s.announcement.prefix == p.as_slice()) && token.is_none_or(|t| s.announcement.token == t) && s.announcement.flags & meshlive::discovery::STREAM != 0
                    && (s.announcement.summary == 0x40) == (mode == "watch-radio") => break s,
                Ok(_) | Err(meshlive::bluetooth::Error::Timeout) => {}, Err(error) => panic!("Scanner: {error:?}"),
            }
        };
        drop(scanner); eprintln!("PHONE_CHECK discovered mode={mode} elapsed_ms={}", born.elapsed().as_millis());
        let mask = meshlive::stream_watch::probe_decode_mask();
        let offer = format!("version={} flags={} summary={} sequence={} decode_mask={mask:?}\n", selected.announcement.version, selected.announcement.flags, selected.announcement.summary, selected.announcement.sequence);
        std::fs::write(dir.parent().unwrap().join(format!("{mode}-offer.txt")), &offer).unwrap();
        eprintln!("PHONE_CHECK offer {offer}");
        assert_eq!(selected.announcement.sequence & 0x80, 0, "Approved source is advertising full: {offer}");
        CHECK_STARTED.set(Instant::now()).unwrap();
        (Some(crate::stream_backend::Task::watch(dir.clone(), selected, out)), None)
    } else {
        // Mirrors the source UI's per-session public-address consent. This
        // opt-in is never accepted as a product or persistent consent.
        assert_eq!(std::env::var("MESHLIVE_PHONE_SOURCE_CONSENT").ok().as_deref(), Some("1"), "Confirm the source exposure notice for this run");
        eprintln!("PHONE_CHECK consent: {}", meshlive::stream_source::EXPOSURE);
        let initial = meshlive::stream_source::InitialMedia { muted: true, ..Default::default() };
        if mode == "source-camera" { assert_eq!(std::env::var("MESHLIVE_PHONE_CAMERA_CONSENT").as_deref(), Ok("1"), "Explicit camera check required"); }
        let kind = if matches!(mode.as_str(), "source-video" | "source-camera") { LiveKind::Video } else { LiveKind::AudioOnly };
        // Design HC: MESHLIVE_PHONE_SOURCE_CODEC=2|3|4|5 picks the source codec
        // (default VP9, as before); 4/5 need their master switch on.
        let mut settings = meshlive::stream_source::Settings::default();
        if let Ok(codec) = std::env::var("MESHLIVE_PHONE_SOURCE_CODEC") {
            settings.codec = codec.parse().expect("MESHLIVE_PHONE_SOURCE_CODEC is 2, 3, 4 or 5");
            assert!(settings.valid(), "codec {} isn't valid here (HEVC/H.264 are off with MESHLIVE_HEVC=0 / MESHLIVE_H264=0, or this PC has no hardware encoder for them)", settings.codec);
        }
        eprintln!("PHONE_CHECK source_codec={}", settings.codec);
        let task = crate::source_backend::Task::start(dir.clone(), kind, String::new(), settings, initial, out);
        (None, Some(task))
    };
    let started = Instant::now(); let mut last_report = started;
    while started.elapsed() < Duration::from_secs(seconds) {
        match updates.recv_timeout(Duration::from_millis(100)) {
            Ok(Update::Watch(WatchEvent::Connected)) => {
                if losses > 0 && samples_at_recovery.is_none() {
                    recovered_at_ms = Some(started.elapsed().as_millis());
                    samples_at_recovery = Some(AUDIO_SAMPLES.load(Ordering::Acquire));
                }
                connected = true; eprintln!("PHONE_CHECK admitted elapsed_ms={}", born.elapsed().as_millis());
            },
            Ok(Update::Watch(WatchEvent::Lost)) => {
                if losses == 0 {
                    loss_at_ms = Some(started.elapsed().as_millis());
                    samples_at_loss = Some(AUDIO_SAMPLES.load(Ordering::Acquire));
                }
                losses += 1;
            },
            Ok(Update::WatchRoute(hops)) => {
                if hops == 0 { direct_headers += 1; } else { relay_headers += 1; }
                max_hops = max_hops.max(hops);
            },
            Ok(Update::Watch(WatchEvent::Paused(value))) => { if value { paused = true; } else if paused { resumed = true; } },
            Ok(Update::Watch(WatchEvent::SourceState(state))) => {
                let value = if state.paused { 2 } else if state.camera_off { if state.source_muted { 6 } else { 5 } } else if state.source_muted { 4 } else { 1 };
                watch_states |= 1 << value;
                for (active, since, longest) in [(state.camera_off && !state.source_muted, &mut camera_off_since, &mut camera_off_ms),
                    (state.camera_off && state.source_muted, &mut both_off_since, &mut both_off_ms)] {
                    if active { since.get_or_insert_with(Instant::now); }
                    else if let Some(begin) = since.take() { *longest = (*longest).max(begin.elapsed().as_millis()); }
                }
                if state.paused { paused = true; } else if paused { resumed = true; }
                eprintln!("PHONE_CHECK source_state={value} elapsed_ms={} frames={}", started.elapsed().as_millis(), VIDEO_FRAMES.load(Ordering::Acquire));
            },
            Ok(Update::Watch(WatchEvent::Ended)) => { ended = true; break; },
            Ok(Update::WatchNote(Some(note)) | Update::LiveNote(Some(note))) => eprintln!("PHONE_CHECK note={note}"),
            Ok(Update::Watch(WatchEvent::GaveUp)) | Ok(Update::Live(LiveEvent::Failed)) => { failure = Some("Native worker failed"); break; },
            Ok(Update::Live(LiveEvent::Viewers(n))) if n > 0 => {
                connected = true; viewer_active = true; viewer_since.get_or_insert_with(Instant::now);
                eprintln!("PHONE_CHECK viewer_admitted elapsed_ms={}", born.elapsed().as_millis());
            },
            Ok(Update::Live(LiveEvent::Viewers(0))) if viewer_active => { viewer_active = false; losses += 1; },
            _ => {},
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            let progress = format!("seconds={} audio_samples={} video_frames={} source_writes={} source_reports={} source_bytes={} delivery_reports={} received_bytes={} receiver_clock_ms={} losses={} last_media_ms={} watch_states={watch_states}\n", started.elapsed().as_secs(), AUDIO_SAMPLES.load(Ordering::Acquire), VIDEO_FRAMES.load(Ordering::Acquire), SOURCE_WRITES.load(Ordering::Acquire), SOURCE_REPORTS.load(Ordering::Acquire), SOURCE_BYTES.load(Ordering::Acquire), SOURCE_DELIVERY_REPORTS.load(Ordering::Acquire), SOURCE_RECEIVED_BYTES.load(Ordering::Acquire), SOURCE_RECEIVER_CLOCK_MS.load(Ordering::Acquire), losses, LAST_MEDIA_MS.load(Ordering::Acquire));
            let recovered_audio_samples = samples_at_recovery.map_or(0, |at| AUDIO_SAMPLES.load(Ordering::Acquire).saturating_sub(at));
            let progress = format!("{progress}direct_headers={direct_headers} relay_headers={relay_headers} max_hops={max_hops} recovered_audio_samples={recovered_audio_samples}\n");
            std::fs::write(dir.parent().unwrap().join(format!("{mode}-progress.txt")), &progress).unwrap();
            eprintln!("PHONE_CHECK progress {progress}");
            last_report = Instant::now();
        }
        if let Some(source) = source.as_ref() {
            if let Some(age) = viewer_since.map(|at| at.elapsed().as_secs()) {
                if age >= 20 && !paused { source.control(crate::source_backend::Control::Pause(true)); paused = true; }
                if age >= 25 && paused && !resumed { source.control(crate::source_backend::Control::Pause(false)); resumed = true; }
                if camera_cycle {
                    if age >= 40 && !camera_off { source.control(crate::source_backend::Control::Camera(false)); camera_off = true; }
                    if age >= 45 && off_baseline.is_none() { off_baseline = Some(video_inputs()); }
                    if age >= 50 && !camera_on {
                        off_inputs = video_inputs().saturating_sub(off_baseline.unwrap());
                        on_baseline = video_inputs(); source.control(crate::source_backend::Control::Camera(true)); camera_on = true;
                    }
                }
            }
        }
    }
    let viewer_seconds = viewer_since.map_or(0, |at| at.elapsed().as_secs());
    camera_off_ms = camera_off_ms.max(camera_off_since.map_or(0, |at| at.elapsed().as_millis()));
    both_off_ms = both_off_ms.max(both_off_since.map_or(0, |at| at.elapsed().as_millis()));
    let stopping = Instant::now();
    drop(watch); drop(source); meshlive::identity_gate::set_store_dir(None);
    let stop_ms = stopping.elapsed().as_millis();
    let samples = AUDIO_SAMPLES.load(Ordering::Acquire); let frames = VIDEO_FRAMES.load(Ordering::Acquire);
    let writes = SOURCE_WRITES.load(Ordering::Acquire); let reports = SOURCE_REPORTS.load(Ordering::Acquire);
    let end_acks = SOURCE_END_ACKS.load(Ordering::Acquire);
    let result = format!("mode={mode}\nconnected={connected}\nseconds={}\naudio_samples={samples}\nvideo_frames={frames}\nsource_writes={writes}\nsource_reports={reports}\nsource_end_acks={end_acks}\nsource_write_us={}\nfirst_media_ms={}\nmax_media_gap_ms={}\nlosses={losses}\npaused={paused}\nresumed={resumed}\nended={ended}\nfailure={failure:?}\n", started.elapsed().as_secs(), SOURCE_WRITE_US.load(Ordering::Acquire), FIRST_MEDIA_MS.load(Ordering::Acquire), MAX_MEDIA_GAP_MS.load(Ordering::Acquire));
    let result = format!("{result}source_bytes={}\ndelivery_reports={}\nreceived_bytes={}\nreceiver_clock_ms={}\nviewer_seconds={viewer_seconds}\nstop_ms={stop_ms}\n", SOURCE_BYTES.load(Ordering::Acquire), SOURCE_DELIVERY_REPORTS.load(Ordering::Acquire), SOURCE_RECEIVED_BYTES.load(Ordering::Acquire), SOURCE_RECEIVER_CLOCK_MS.load(Ordering::Acquire));
    let result = format!("{result}watch_states={watch_states}\ncamera_off_ms={camera_off_ms}\nboth_off_ms={both_off_ms}\n");
    let result = format!("{result}max_watch_uplink_ms={}\n", MAX_WATCH_UPLINK_MS.load(Ordering::Acquire));
    let result = format!("{result}watch_header_audio={}\n", WATCH_HEADER_AUDIO.load(Ordering::Acquire));
    let result = format!("{result}single_unit_radio={single_radio}\n");
    let result = format!("{result}source_no_labels={source_no_labels}\n");
    let result = format!("{result}require_relay={require_relay}\ndirect_headers={direct_headers}\nrelay_headers={relay_headers}\nmax_hops={max_hops}\ndistinct_relay_headers={}\n", WATCH_DISTINCT_RELAY_HEADERS.load(Ordering::Acquire));
    let recovered_audio_samples = samples_at_recovery.map_or(0, |at| samples.saturating_sub(at));
    let result = format!("{result}require_recovery={require_recovery}\nloss_at_ms={loss_at_ms:?}\nrecovered_at_ms={recovered_at_ms:?}\nsamples_at_loss={samples_at_loss:?}\nrecovered_audio_samples={recovered_audio_samples}\n");
    let result = format!("{result}generated_video_inputs={}\n", SOURCE_GENERATED_FRAMES.load(Ordering::Acquire));
    let result = format!("{result}camera_video_inputs={}\n", SOURCE_CAMERA_FRAMES.load(Ordering::Acquire));
    let result = format!("{result}camera_cycle={camera_cycle}\ncamera_off_requested={camera_off}\ncamera_on_requested={camera_on}\nvideo_inputs_during_settled_camera_off={off_inputs}\n");
    let result = format!("{result}max_source_queue_ms={}\nmax_source_queue_records={}\nmax_source_pace_ms={}\nmax_source_write_us={}\nmax_source_capture_us={}\nmax_source_capture_frames={}\n",
        SOURCE_MAX_QUEUE_MS.load(Ordering::Acquire), SOURCE_MAX_QUEUE_RECORDS.load(Ordering::Acquire), SOURCE_MAX_PACE_MS.load(Ordering::Acquire),
        SOURCE_MAX_WRITE_US.load(Ordering::Acquire), SOURCE_MAX_CAPTURE_US.load(Ordering::Acquire), SOURCE_MAX_CAPTURE_FRAMES.load(Ordering::Acquire));
    std::fs::write(dir.parent().unwrap().join(format!("{mode}-result.txt")), &result).unwrap(); eprintln!("PHONE_CHECK result\n{result}");
    assert!(failure.is_none() && connected, "{result}");
    if require_relay {
        assert!(relay_headers > 0 && direct_headers == 0 && max_hops == 1
            && WATCH_DISTINCT_RELAY_HEADERS.load(Ordering::Acquire) > 0, "{result}");
    }
    if require_recovery {
        assert!(losses == 1 && relay_headers >= 2 && WATCH_DISTINCT_RELAY_HEADERS.load(Ordering::Acquire) >= 2
            && samples_at_loss.is_some_and(|n| n >= 48_000) && recovered_audio_samples >= 96_000
            && recovered_at_ms.zip(loss_at_ms).is_some_and(|(back, lost)| back >= lost) && ended, "{result}");
    }
    if std::env::var("MESHLIVE_PHONE_NO_AUDIO").as_deref() == Ok("1") {
        assert!(mode == "watch-video" && WATCH_HEADER_AUDIO.load(Ordering::Acquire) == 0 && samples == 0, "{result}");
    }
    assert!(match mode.as_str() { "watch-radio" => samples >= 48_000, "watch-video" => frames >= 5, _ => writes >= 10 && reports >= 3 && end_acks > 0 && losses == 0 }, "{result}");
    if mode == "source-video" { assert!(SOURCE_GENERATED_FRAMES.load(Ordering::Acquire) >= 30, "{result}"); }
    if mode == "source-camera" { assert!(SOURCE_CAMERA_FRAMES.load(Ordering::Acquire) >= 30, "{result}"); }
    if camera_cycle { assert!(camera_off && camera_on && off_baseline.is_some() && off_inputs == 0 && video_inputs() > on_baseline, "{result}"); }
    if std::env::var("MESHLIVE_PHONE_MEDIA_STATE").as_deref() == Ok("1") {
        let expected = (1 << 1) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6);
        assert!(mode == "watch-video" && watch_states & expected == expected && camera_off_ms >= 8_000
            && both_off_ms >= 8_000 && ended && losses == 0, "{result}");
    }
    if std::env::var("MESHLIVE_PHONE_FLOW").as_deref() == Ok("1") {
        if mode.starts_with("watch-") {
            assert!(LAST_MEDIA_MS.load(Ordering::Acquire) >= 300_000 && losses == 0 && ended, "{result}");
            if mode == "watch-video" { assert!(paused && resumed, "{result}"); }
        } else {
            assert!(viewer_seconds >= 300 && viewer_active && paused && resumed, "{result}");
        }
    }
}

/// Passive public metadata only. The operator binds a target through the
/// approved phone's normal source stop/restart before any Watch connection.
fn observe_stream_offers() {
    use meshlive::{bluetooth::scan::{Scanner, ScanOptions}, discovery::{STREAM, GATT, CLASSIC}};
    let scanner = Scanner::start(ScanOptions { rotating: true, compact: true }).expect("Offer scanner");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let video = std::env::var("MESHLIVE_PHONE_OFFER_VIDEO").as_deref() == Ok("1");
    let mut offers = std::collections::BTreeMap::new();
    let mut truncated = false;
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        match scanner.receive(Instant::now() + Duration::from_millis(100), &stop) {
            Ok(s) if s.announcement.version == 5
                && (if video { s.announcement.stream_kind() == meshlive::discovery::StreamKind::Video } else { s.announcement.summary == 0x40 })
                && s.announcement.flags & (STREAM | GATT | CLASSIC) == STREAM | GATT => {
                let a = s.announcement;
                let key = (a.prefix, a.token);
                if offers.len() < 64 || offers.contains_key(&key) { offers.insert(key, a.cost); }
                else { truncated = true; }
            }
            Ok(_) | Err(meshlive::bluetooth::Error::Timeout) => {},
            Err(error) => panic!("Offer scanner: {error:?}"),
        }
    }
    drop(scanner);
    let mut result = format!("observed=true\ntruncated={truncated}\n");
    for ((rdi, token), cost) in offers {
        let rdi: String = rdi.iter().map(|b| format!("{b:02x}")).collect();
        result.push_str(&format!("rdi={rdi} token={token:08x} cost={cost}\n"));
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".artifacts/phone");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stream-offers.txt"), result).unwrap();
}

mod call;
mod ui;
