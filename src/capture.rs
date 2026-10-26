//! Real capture for calls (test policy since 2026-10-19: short tests with the
//! camera covered or pointed away, quiet room, every run logged, nothing kept
//! beyond the measurement). Explicit and opt-in: nothing here opens a device
//! unless an owner calls `open_*`; unit tests touch only the pure helpers.
//!
//! - `Microphone`: WASAPI shared-mode capture from the default communications
//!   endpoint, 48 kHz mono 16-bit (the engine converts), event driven; whole
//!   20 ms frames (960 samples) for the Opus encoder.
//! - `Camera`: Media Foundation source reader on the first video capture
//!   device, NV12 at the call size (advanced video processing scales and
//!   converts), synchronous reads.
//! Samples and pictures live only in memory and are dropped after use.
// The COM interfaces declared here keep their Windows method names.
#![allow(non_snake_case)]
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Media::{Audio::*, MediaFoundation::*},
        System::{Com::*, Threading::*},
    },
    core::PCWSTR,
};

pub const FRAME_SAMPLES: usize = 960;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NoDevice,
    Timeout,
    Native(i32),
}
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Self::Native(e.code().0)
    }
}

struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn new() -> Result<Self, Error> {
        // The same apartment kind as the WASAPI render owner (playback), so both
        // can live on one thread (a mode change would fail).
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()? };
        Ok(Self(PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

// ---------------------------------------------------------------- communications processing

/// IAudioEffectsManager (audioclient.h, Windows 11 22H2 and later): the
/// effects the audio engine applies to one stream in its current processing
/// mode. Declared here because the pinned `windows` metadata predates it.
#[windows_core::interface("4460b3ae-4b44-4527-8676-7548a8acd260")]
unsafe trait IAudioEffectsManager: windows_core::IUnknown {
    fn RegisterAudioEffectsChangedNotificationCallback(&self, client: *mut core::ffi::c_void) -> windows_core::HRESULT;
    fn UnregisterAudioEffectsChangedNotificationCallback(&self, client: *mut core::ffi::c_void) -> windows_core::HRESULT;
    fn GetAudioEffects(&self, effects: *mut *mut AUDIO_EFFECT, count: *mut u32) -> windows_core::HRESULT;
    fn SetAudioEffectState(&self, id: windows_core::GUID, state: AUDIO_EFFECT_STATE) -> windows_core::HRESULT;
}

/// AUDIO_EFFECT_TYPE_* (ksmedia.h) the call log names.
const EFFECT_NAMES: [(u128, &str); 6] = [
    (0x6f64adbe_8211_11e2_8c70_2c27d7f001fa, "aec"),
    (0x6f64adbf_8211_11e2_8c70_2c27d7f001fa, "ns"),
    (0x6f64adc0_8211_11e2_8c70_2c27d7f001fa, "agc"),
    (0x6f64adc1_8211_11e2_8c70_2c27d7f001fa, "beamforming"),
    (0x6f64add0_8211_11e2_8c70_2c27d7f001fa, "deep_ns"),
    (0x6f64adcf_8211_11e2_8c70_2c27d7f001fa, "far_field_beamforming"),
];
const AEC_EFFECT: u128 = 0x6f64adbe_8211_11e2_8c70_2c27d7f001fa;

/// What the capture endpoint reports about echo cancellation for this stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aec {
    /// The engine applies an AEC effect to this stream.
    On,
    /// An AEC effect is present but switched off.
    Off,
    /// The effect list has no AEC.
    Absent,
    /// The effect list could not be read (Windows before 11 22H2, or the
    /// driver does not report it): the endpoint may still cancel echo.
    Unknown,
}

/// Judges an effect list (pure): `(effect type, on)` pairs.
pub fn aec_state(effects: &[(u128, bool)]) -> Aec {
    match effects.iter().find(|(id, _)| *id == AEC_EFFECT) {
        Some((_, true)) => Aec::On,
        Some((_, false)) => Aec::Off,
        None => Aec::Absent,
    }
}
/// The effect list as `name:on|off` words (pure); unknown types by GUID.
pub fn effect_words(effects: &[(u128, bool)]) -> String {
    let words: Vec<String> = effects
        .iter()
        .map(|(id, on)| {
            let name = EFFECT_NAMES
                .iter()
                .find(|(known, _)| known == id)
                .map_or_else(|| format!("{:?}", windows_core::GUID::from_u128(*id)), |(_, n)| (*n).to_owned());
            format!("{name}:{}", if *on { "on" } else { "off" })
        })
        .collect();
    if words.is_empty() { "none".into() } else { words.join(",") }
}

/// Marks a stream as a voice call before `Initialize` (AudioCategory_Communications),
/// so Windows runs the endpoint's communications processing (AEC and noise
/// suppression where the driver or Windows offers them). Returns the log word.
pub(crate) fn set_communications(client: &IAudioClient) -> String {
    let result = windows_core::Interface::cast::<IAudioClient2>(client).and_then(|client2| {
        let properties = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AudioCategory_Communications,
            Options: AUDCLNT_STREAMOPTIONS_NONE,
        };
        unsafe { client2.SetClientProperties(&properties) }
    });
    match result {
        Ok(()) => "communications".into(),
        Err(e) => format!("default(set_failed={:#010x})", e.code().0),
    }
}
/// After `Initialize`, before `Start`: this app's call stream does not lower
/// other apps' sound (Windows' default communications ducking), so a call
/// never changes what else the PC plays. Returns the log word.
pub(crate) fn opt_out_of_ducking(client: &IAudioClient) -> String {
    let session = unsafe { client.GetService::<IAudioSessionControl>() }
        .and_then(|s| windows_core::Interface::cast::<IAudioSessionControl2>(&s))
        .and_then(|s| unsafe { s.SetDuckingPreference(true) });
    // Windows 11 22H2+: the per-stream option as well (absent before).
    let stream = windows_core::Interface::cast::<IAudioClientDuckingControl>(client)
        .and_then(|d| unsafe { d.SetDuckingOptionsForCurrentStream(AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS) });
    format!(
        "session:{},stream:{}",
        if session.is_ok() { "ok" } else { "unavailable" },
        if stream.is_ok() { "ok" } else { "unavailable" }
    )
}
/// After `Initialize`: the stream's effect list and AEC state, or why unknown.
fn stream_effects(client: &IAudioClient) -> (Aec, String) {
    let manager = match unsafe { client.GetService::<IAudioEffectsManager>() } {
        Ok(m) => m,
        Err(e) => return (Aec::Unknown, format!("unavailable({:#010x})", e.code().0)),
    };
    let mut list: *mut AUDIO_EFFECT = std::ptr::null_mut();
    let mut count = 0u32;
    let hr = unsafe { manager.GetAudioEffects(&mut list, &mut count) };
    if hr.is_err() {
        return (Aec::Unknown, format!("unreadable({:#010x})", hr.0));
    }
    let effects: Vec<(u128, bool)> = if list.is_null() {
        Vec::new()
    } else {
        // SAFETY: the engine returned `count` entries, freed with CoTaskMemFree.
        let slice = unsafe { std::slice::from_raw_parts(list, count as usize) };
        let v = slice.iter().map(|e| (e.id.to_u128(), e.state == AUDIO_EFFECT_STATE_ON)).collect();
        unsafe { CoTaskMemFree(Some(list.cast())) };
        v
    };
    (aec_state(&effects), effect_words(&effects))
}

// ---------------------------------------------------------------- microphone

pub struct Microphone {
    // Interfaces release before the event handle and the apartment.
    capture: IAudioCaptureClient,
    client: IAudioClient,
    event: HANDLE,
    pending: Vec<i16>,
    /// Packets received and those Windows flagged SILENT (it delivers silent
    /// packets when microphone access is denied to the app).
    pub packets: u64,
    pub silent_packets: u64,
    /// The opened endpoint, for the facts.
    pub endpoint_id: String,
    /// Echo cancellation the endpoint reports for this stream.
    pub aec: Aec,
    /// One `audio_effects` log line: category, effects, ducking.
    pub effects: String,
    _apartment: Apartment,
}
impl Microphone {
    pub fn open_default() -> Result<Self, Error> {
        Self::open(None, false)
    }
    /// `id`: an endpoint id (`audio_devices::list`), None for the default
    /// communications capture endpoint. `loopback`: capture what a RENDER
    /// endpoint plays (WASAPI loopback), for the acoustic-free latency check.
    pub fn open(id: Option<&str>, loopback: bool) -> Result<Self, Error> {
        Self::open_with(id, loopback, !loopback)
    }
    /// As `open`; `call`: a communications stream (false only for
    /// measurements, e.g. the echo A/B's baseline).
    pub fn open_with(id: Option<&str>, loopback: bool, call: bool) -> Result<Self, Error> {
        let apartment = Apartment::new()?;
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)? };
        let endpoint = match id {
            Some(id) => unsafe { enumerator.GetDevice(&windows::core::HSTRING::from(id)) },
            None => unsafe { enumerator.GetDefaultAudioEndpoint(if loopback { eRender } else { eCapture }, eCommunications) },
        }
        .map_err(|_| Error::NoDevice)?;
        let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_INPROC_SERVER, None)? };
        // A call's microphone is a communications stream (echo cancellation);
        // loopback (a measurement) keeps the default category.
        let category = if call { set_communications(&client) } else { "default".to_owned() };
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 1,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 96_000,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | AUDCLNT_STREAMFLAGS_NOPERSIST;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
        unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &format, None)?;
            client.SetEventHandle(event)?;
        }
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        let (aec, effects) = stream_effects(&client);
        let ducking = if call { opt_out_of_ducking(&client) } else { "n/a".to_owned() };
        unsafe { client.Start()? };
        let endpoint_id = endpoint_id(&endpoint);
        let effects = format!(
            "audio_effects flow=capture endpoint=\"{}\" category={category} aec={aec:?} effects={effects} ducking_optout={ducking}",
            friendly_name(&endpoint)
        );
        Ok(Self {
            capture,
            client,
            event,
            pending: Vec::new(),
            packets: 0,
            silent_packets: 0,
            endpoint_id,
            aec,
            effects,
            _apartment: apartment,
        })
    }
    /// The next whole 20 ms frame (960 samples), waiting at most until `deadline`.
    pub fn next_frame(&mut self, deadline: Instant) -> Result<[i16; FRAME_SAMPLES], Error> {
        loop {
            if self.pending.len() >= FRAME_SAMPLES {
                let mut frame = [0i16; FRAME_SAMPLES];
                frame.copy_from_slice(&self.pending[..FRAME_SAMPLES]);
                self.pending.drain(..FRAME_SAMPLES);
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let wait = deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, 50) as u32;
            let _ = unsafe { WaitForSingleObject(self.event, wait) } == WAIT_OBJECT_0;
            loop {
                let packet = unsafe { self.capture.GetNextPacketSize()? };
                if packet == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
                self.packets += 1;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    self.silent_packets += 1;
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    self.pending.extend(std::iter::repeat_n(0i16, frames as usize));
                } else {
                    let samples = unsafe { std::slice::from_raw_parts(data.cast::<i16>(), frames as usize) };
                    self.pending.extend_from_slice(samples);
                }
                unsafe { self.capture.ReleaseBuffer(frames)? };
            }
        }
    }
}
impl Drop for Microphone {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
        self.pending.fill(0);
    }
}

// ---------------------------------------------------------------- facts (read only)

pub(crate) fn endpoint_id(device: &IMMDevice) -> String {
    unsafe { device.GetId() }
        .ok()
        .and_then(|id| {
            let text = unsafe { id.to_string() }.ok();
            unsafe { CoTaskMemFree(Some(id.0.cast())) };
            text
        })
        .unwrap_or_else(|| "unknown".into())
}
pub(crate) fn friendly_name(device: &IMMDevice) -> String {
    use windows::Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
    };
    (|| -> windows::core::Result<String> {
        let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
        let mut value = unsafe { store.GetValue(&PKEY_Device_FriendlyName)? };
        let text = unsafe { PropVariantToStringAlloc(&value)? };
        let name = unsafe { text.to_string() }.unwrap_or_default();
        unsafe {
            CoTaskMemFree(Some(text.0.cast()));
            let _ = PropVariantClear(&mut value);
        }
        Ok(name)
    })()
    .unwrap_or_else(|_| "unknown".into())
}
/// One line per capture-endpoint fact: the default communications and
/// console capture endpoints (id, name, mix format, mute, volume). Read only:
/// nothing is changed.
pub fn capture_endpoint_facts() -> Vec<String> {
    let mut lines = Vec::new();
    let Ok(_apartment) = Apartment::new() else { return lines };
    let Ok(enumerator) = (unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }) else {
        return lines;
    };
    for (role_name, role) in [("communications", eCommunications), ("console", eConsole)] {
        match unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, role) } {
            Ok(device) => {
                let format = unsafe { device.Activate::<IAudioClient>(CLSCTX_INPROC_SERVER, None) }
                    .ok()
                    .and_then(|c| unsafe { c.GetMixFormat() }.ok())
                    .map(|f| {
                        let w = unsafe { *f };
                        unsafe { CoTaskMemFree(Some(f.cast())) };
                        let (rate, channels, bits) = (w.nSamplesPerSec, w.nChannels, w.wBitsPerSample);
                        format!("{rate} Hz {channels} ch {bits} bit")
                    })
                    .unwrap_or_else(|| "unknown".into());
                let volume = unsafe {
                    device.Activate::<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume>(CLSCTX_INPROC_SERVER, None)
                };
                let (muted, level) = match volume {
                    Ok(v) => (
                        unsafe { v.GetMute() }.map_or("unknown".into(), |m| m.as_bool().to_string()),
                        unsafe { v.GetMasterVolumeLevelScalar() }.map_or("unknown".into(), |l| format!("{l:.2}")),
                    ),
                    Err(_) => ("unknown".into(), "unknown".into()),
                };
                let state = unsafe { device.GetState() }.map_or(0, |s| s.0);
                lines.push(format!(
                    "fact=capture_endpoint role={role_name} id={} name=\"{}\" mix_format=\"{format}\" state={state} (1 active) muted={muted} volume_scalar={level}",
                    endpoint_id(&device),
                    friendly_name(&device)
                ));
            }
            Err(e) => lines.push(format!("fact=capture_endpoint role={role_name} none hresult={:#010x}", e.code().0)),
        }
    }
    lines
}
/// The microphone privacy switches as Windows stores them (read only):
/// "Microphone access", "Let apps access", "Let desktop apps access".
pub fn privacy_facts() -> Vec<String> {
    consent_facts("microphone", "mic_privacy")
}
/// The camera's privacy switches (read only), as `privacy_facts`.
pub fn camera_privacy_facts() -> Vec<String> {
    consent_facts("webcam", "camera_privacy")
}
fn consent_facts(capability: &str, fact: &str) -> Vec<String> {
    use windows::{
        Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        core::HSTRING,
    };
    let read = |root: HKEY, path: &str| -> String {
        let mut buffer = [0u16; 64];
        let mut size = (buffer.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(root, &HSTRING::from(path), &HSTRING::from("Value"), RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size))
        };
        if status.is_ok() {
            String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)])
        } else {
            format!("unset({})", status.0)
        }
    };
    let base = format!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{capability}");
    vec![
        format!("fact={fact} scope=machine value={}", read(HKEY_LOCAL_MACHINE, &base)),
        format!("fact={fact} scope=user_apps value={}", read(HKEY_CURRENT_USER, &base)),
        format!("fact={fact} scope=user_desktop_apps value={}", read(HKEY_CURRENT_USER, &format!(r"{base}\NonPackaged"))),
    ]
}
/// Judges a capture window as real room signal or not (pure). Why: a single
/// loudest 20 ms frame is fooled by the start-up transient of an open jack
/// input (-64 dBFS once, then digital silence). So the first
/// `SKIP_FRAMES` (200 ms) are ignored, the DC offset is removed, and the
/// window is judged by its RMS plus the spread of its frame levels: a dead
/// or constant input is near-silent and flat; a room is neither.
#[derive(Debug, Clone, Default)]
pub struct SignalMeter {
    seen: u32,
    sum: f64,
    sum_sq: f64,
    samples: u64,
    frame_dbfs: Vec<f64>,
}
impl SignalMeter {
    pub const SKIP_FRAMES: u32 = 10;
    /// At or below this window RMS (DC removed) an input is digitally flat:
    /// a 16-bit path with no analogue noise sits near -96..-101 dBFS. A
    /// live microphone always has an analogue noise floor above it.
    pub const FLAT_DBFS: f64 = -90.0;
    /// Below this a live input is "very quiet" (a MEMS array in a quiet room
    /// can sit at about -80..-85 dBFS); it is still usable, with a warning.
    pub const QUIET_DBFS: f64 = -80.0;
    /// A frame-level spread (p90 - p10) at or below this is a constant
    /// signal, not a noise floor.
    pub const MIN_VARIANCE_DB: f64 = 0.05;

    pub fn push(&mut self, frame: &[i16]) {
        self.seen += 1;
        if self.seen <= Self::SKIP_FRAMES || frame.is_empty() {
            return;
        }
        let n = frame.len() as f64;
        let mean = frame.iter().map(|&s| f64::from(s)).sum::<f64>() / n;
        let ac = frame.iter().map(|&s| (f64::from(s) - mean).powi(2)).sum::<f64>() / n;
        self.frame_dbfs.push(to_dbfs(ac));
        for &s in frame {
            self.sum += f64::from(s);
            self.sum_sq += f64::from(s) * f64::from(s);
        }
        self.samples += frame.len() as u64;
    }
    /// Frames judged (after the skipped start).
    pub fn frames(&self) -> usize {
        self.frame_dbfs.len()
    }
    /// Window RMS with the DC offset removed.
    pub fn rms_dbfs(&self) -> f64 {
        if self.samples == 0 {
            return -120.0;
        }
        let n = self.samples as f64;
        let mean = self.sum / n;
        to_dbfs((self.sum_sq / n - mean * mean).max(0.0))
    }
    /// The DC offset as a fraction of full scale.
    pub fn dc(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.sum / self.samples as f64 / 32768.0 }
    }
    /// p90 - p10 of the per-frame levels, in dB.
    pub fn spread_db(&self) -> f64 {
        if self.frame_dbfs.len() < 2 {
            return 0.0;
        }
        let mut v = self.frame_dbfs.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        at(0.9) - at(0.1)
    }
    /// A live input: above the digital floor with any variance.
    pub fn plausible(&self) -> bool {
        self.frames() >= 10 && self.rms_dbfs() > Self::FLAT_DBFS && self.spread_db() > Self::MIN_VARIANCE_DB
    }
    fn quantile(&self, q: f64) -> f64 {
        if self.frame_dbfs.is_empty() {
            return -120.0;
        }
        let mut v = self.frame_dbfs.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        v[((v.len() - 1) as f64 * q).round() as usize]
    }
    /// The noise floor: the 10th percentile of the frame levels.
    pub fn floor_dbfs(&self) -> f64 {
        self.quantile(0.1)
    }
    /// The loudest judged frame.
    pub fn peak_dbfs(&self) -> f64 {
        self.quantile(1.0)
    }
    pub fn summary(&self) -> String {
        format!(
            "window_rms_dbfs={:.1} spread_db={:.1} dc={:.4} frames_judged={} plausible={}",
            self.rms_dbfs(),
            self.spread_db(),
            self.dc(),
            self.frames(),
            self.plausible()
        )
    }
}
fn to_dbfs(mean_square: f64) -> f64 {
    if mean_square <= 0.0 {
        return -120.0;
    }
    (20.0 * (mean_square.sqrt() / 32768.0).log10()).max(-120.0)
}

/// What a capture window measured (pure, from the counts and the meter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    NoPackets,
    /// 90 % or more of the packets carried Windows' SILENT flag.
    Flagged,
    /// Digitally flat: zeros or a constant. NOT proof of a dead device: a
    /// DSP noise gate (measured on this PC's Realtek microphone, floor
    /// -97 dBFS) outputs zeros in a quiet room and opens for speech.
    Flat,
    /// Live, below `QUIET_DBFS`: usable with a warning.
    VeryQuiet,
    Ok,
}
impl Level {
    pub fn usable(self) -> bool {
        matches!(self, Level::VeryQuiet | Level::Ok)
    }
}
pub fn classify(meter: &SignalMeter, packets: u64, silent_packets: u64) -> Level {
    if packets == 0 {
        Level::NoPackets
    } else if silent_packets * 10 >= packets * 9 {
        Level::Flagged
    } else if !meter.plausible() {
        Level::Flat
    } else if meter.rms_dbfs() < SignalMeter::QUIET_DBFS {
        Level::VeryQuiet
    } else {
        Level::Ok
    }
}

/// The plain verdict for a capture window (pure). It states only what the
/// counts, the meter and the read-only facts show.
pub fn mic_verdict(meter: &SignalMeter, packets: u64, silent_packets: u64, privacy: &[String], muted: bool) -> String {
    let denied = privacy.iter().any(|l| l.contains("value=Deny"));
    let privacy_note = if denied { "; the privacy registry says Deny" } else { "" };
    let measured = format!("window_rms_dbfs={:.1} spread_db={:.2} packets={packets} silent_packets={silent_packets}", meter.rms_dbfs(), meter.spread_db());
    match classify(meter, packets, silent_packets) {
        Level::NoPackets => "mic silent: no audio packets arrived (the endpoint did not run, or another app holds it exclusively)".into(),
        Level::Flagged => format!(
            "mic silent: Windows flagged {silent_packets} of {packets} packets SILENT{}",
            if denied { "; the privacy registry says Deny" } else { " (access withheld or the device disabled; the privacy registry does not say Deny)" }
        ),
        Level::Flat if muted => format!("mic silent: the endpoint is muted (read only, not changed) {measured}"),
        Level::Flat => format!(
            "mic quiet or gated: digital silence, no SILENT flags {measured} (a DSP noise gate outputs zeros in a quiet room and opens for speech; this alone does not mean the microphone is dead){privacy_note}"
        ),
        Level::VeryQuiet => format!("mic very quiet: live noise floor but low level {measured} (quiet room or low gain)"),
        Level::Ok => format!("mic ok: {measured}"),
    }
}

// ---------------------------------------------------------------- camera

pub struct Camera {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    pub width: u32,
    pub height: u32,
    _apartment: Apartment,
}
impl Camera {
    /// The first video capture device, delivering NV12 at `width` x `height`.
    pub fn open_first(width: u32, height: u32) -> Result<Self, Error> {
        let apartment = Apartment::new()?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)? };
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 1)? };
        let attributes = attributes.ok_or(Error::NoDevice)?;
        unsafe { attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)? };
        let mut devices: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe { MFEnumDeviceSources(&attributes, &mut devices, &mut count)? };
        if count == 0 || devices.is_null() {
            return Err(Error::NoDevice);
        }
        let list = unsafe { std::slice::from_raw_parts_mut(devices, count as usize) };
        let first = list[0].clone().ok_or(Error::NoDevice)?;
        for entry in list.iter_mut() {
            entry.take();
        }
        unsafe { CoTaskMemFree(Some(devices.cast())) };
        let source: IMFMediaSource = unsafe { first.ActivateObject()? };
        let mut options = None;
        unsafe { MFCreateAttributes(&mut options, 1)? };
        let options = options.ok_or(Error::NoDevice)?;
        unsafe { options.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)? };
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &options)? };
        let media = unsafe { MFCreateMediaType()? };
        unsafe {
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            media.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(width) << 32) | u64::from(height))?;
            reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &media)?;
        }
        Ok(Self { reader, source, width, height, _apartment: apartment })
    }
    /// The next picture as packed NV12 (`width * height * 3 / 2` bytes).
    pub fn next_frame(&mut self, deadline: Instant) -> Result<Vec<u8>, Error> {
        let expected = (self.width * self.height * 3 / 2) as usize;
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let mut flags = 0u32;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )?
            };
            let Some(sample) = sample else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            let mut pointer = std::ptr::null_mut();
            let mut length = 0u32;
            let data = unsafe {
                buffer.Lock(&mut pointer, None, Some(&mut length))?;
                let data = std::slice::from_raw_parts(pointer, length as usize).to_vec();
                buffer.Unlock()?;
                data
            };
            if data.len() >= expected {
                let mut picture = data;
                picture.truncate(expected);
                return Ok(picture);
            }
        }
    }
}
impl Drop for Camera {
    fn drop(&mut self) {
        unsafe {
            let _ = self.source.Shutdown();
            let _ = MFShutdown();
        }
    }
}

// ---------------------------------------------------------------- voice capture DSP (Experimental)

/// Experimental, off by default: `MESHLIVE_VOICE_DSP=1` captures call audio
/// through Windows' Voice Capture DSP (CLSID_CWMAudioAEC, SINGLE_CHANNEL_AEC,
/// source mode) instead of the endpoint path, for endpoints whose
/// communications processing has no echo canceller. The DSP opens the
/// default capture and render devices itself and outputs 16 kHz, which is
/// upsampled to 48 kHz here. Any failure falls back to the endpoint path.
pub const VOICE_DSP_SWITCH: &str = "MESHLIVE_VOICE_DSP";
pub fn voice_dsp_requested() -> bool {
    std::env::var(VOICE_DSP_SWITCH).ok().as_deref() == Some("1")
}

/// DMO_MEDIA_TYPE (mediaobj.h).
#[repr(C)]
struct DmoMediaType {
    majortype: windows_core::GUID,
    subtype: windows_core::GUID,
    fixed_size_samples: windows_core::BOOL,
    temporal_compression: windows_core::BOOL,
    sample_size: u32,
    formattype: windows_core::GUID,
    unk: *mut core::ffi::c_void,
    cb_format: u32,
    pb_format: *mut u8,
}
/// DMO_OUTPUT_DATA_BUFFER (mediaobj.h).
#[repr(C)]
struct DmoOutputDataBuffer {
    buffer: *mut core::ffi::c_void,
    status: u32,
    timestamp: i64,
    timelength: i64,
}
const DMO_OUTPUT_DATA_BUFFERF_INCOMPLETE: u32 = 0x0100_0000;
/// WMAAECMA_E_NO_ACTIVE_RENDER_STREAM: nothing plays on the render device yet.
const NO_ACTIVE_RENDER_STREAM: i32 = 0x87CC_000Au32 as i32;
/// FORMAT_WaveFormatEx (uuids.h).
const FORMAT_WAVEFORMATEX: windows_core::GUID = windows_core::GUID::from_u128(0x05589f81_c356_11ce_bf01_00aa0055595a);
/// MFPKEY_WMAAECMA_* (wmcodecdsp.h): one GUID, PID_FIRST_USABLE (2) onwards.
const WMAAECMA: windows_core::GUID = windows_core::GUID::from_u128(0x6f52c567_0360_4bd2_9617_ccbf1421c939);
const PID_SYSTEM_MODE: u32 = 2;
const PID_DMO_SOURCE_MODE: u32 = 3;
/// The DSP's output rate (it supports 8, 11.025, 16 and 22.05 kHz).
const DSP_RATE: u32 = 16_000;

#[windows_core::interface("59eff8b9-938c-4a26-82f2-95cb84cdc837")]
unsafe trait IMediaBuffer: windows_core::IUnknown {
    fn SetLength(&self, length: u32) -> windows_core::HRESULT;
    fn GetMaxLength(&self, length: *mut u32) -> windows_core::HRESULT;
    fn GetBufferAndLength(&self, buffer: *mut *mut u8, length: *mut u32) -> windows_core::HRESULT;
}
#[windows_core::interface("d8ad0f58-5494-4102-97c5-ec798e59bcf4")]
unsafe trait IMediaObject: windows_core::IUnknown {
    fn GetStreamCount(&self, inputs: *mut u32, outputs: *mut u32) -> windows_core::HRESULT;
    fn GetInputStreamInfo(&self, index: u32, flags: *mut u32) -> windows_core::HRESULT;
    fn GetOutputStreamInfo(&self, index: u32, flags: *mut u32) -> windows_core::HRESULT;
    fn GetInputType(&self, index: u32, type_index: u32, media: *mut DmoMediaType) -> windows_core::HRESULT;
    fn GetOutputType(&self, index: u32, type_index: u32, media: *mut DmoMediaType) -> windows_core::HRESULT;
    fn SetInputType(&self, index: u32, media: *const DmoMediaType, flags: u32) -> windows_core::HRESULT;
    fn SetOutputType(&self, index: u32, media: *const DmoMediaType, flags: u32) -> windows_core::HRESULT;
    fn GetInputCurrentType(&self, index: u32, media: *mut DmoMediaType) -> windows_core::HRESULT;
    fn GetOutputCurrentType(&self, index: u32, media: *mut DmoMediaType) -> windows_core::HRESULT;
    fn GetInputSizeInfo(&self, index: u32, size: *mut u32, lookahead: *mut u32, alignment: *mut u32) -> windows_core::HRESULT;
    fn GetOutputSizeInfo(&self, index: u32, size: *mut u32, alignment: *mut u32) -> windows_core::HRESULT;
    fn GetInputMaxLatency(&self, index: u32, latency: *mut i64) -> windows_core::HRESULT;
    fn SetInputMaxLatency(&self, index: u32, latency: i64) -> windows_core::HRESULT;
    fn Flush(&self) -> windows_core::HRESULT;
    fn Discontinuity(&self, index: u32) -> windows_core::HRESULT;
    fn AllocateStreamingResources(&self) -> windows_core::HRESULT;
    fn FreeStreamingResources(&self) -> windows_core::HRESULT;
    fn GetInputStatus(&self, index: u32, flags: *mut u32) -> windows_core::HRESULT;
    fn ProcessInput(&self, index: u32, buffer: *mut core::ffi::c_void, flags: u32, time: i64, length: i64) -> windows_core::HRESULT;
    fn ProcessOutput(&self, flags: u32, count: u32, buffers: *mut DmoOutputDataBuffer, status: *mut u32) -> windows_core::HRESULT;
    fn Lock(&self, lock: i32) -> windows_core::HRESULT;
}

/// The output buffer the DSP writes into (one, reused).
#[windows_core::implement(IMediaBuffer)]
struct DmoBuffer {
    data: std::cell::UnsafeCell<Box<[u8]>>,
    capacity: u32,
    length: std::cell::Cell<u32>,
}
impl IMediaBuffer_Impl for DmoBuffer_Impl {
    unsafe fn SetLength(&self, length: u32) -> windows_core::HRESULT {
        if length > self.capacity {
            return windows::Win32::Foundation::E_INVALIDARG;
        }
        self.length.set(length);
        windows::Win32::Foundation::S_OK
    }
    unsafe fn GetMaxLength(&self, length: *mut u32) -> windows_core::HRESULT {
        if length.is_null() {
            return windows::Win32::Foundation::E_POINTER;
        }
        unsafe { *length = self.capacity };
        windows::Win32::Foundation::S_OK
    }
    unsafe fn GetBufferAndLength(&self, buffer: *mut *mut u8, length: *mut u32) -> windows_core::HRESULT {
        unsafe {
            if !buffer.is_null() {
                *buffer = (&mut *self.data.get()).as_mut_ptr();
            }
            if !length.is_null() {
                *length = self.length.get();
            }
        }
        windows::Win32::Foundation::S_OK
    }
}

/// 3x upsampling (16 kHz to 48 kHz) by a polyphase windowed-sinc low-pass
/// (pure): cutoff 7.2 kHz, 16 taps per phase, unity gain in the pass band.
pub struct Upsample3 {
    phases: [[f32; Upsample3::TAPS]; 3],
    history: [f32; Upsample3::TAPS],
}
impl Default for Upsample3 {
    fn default() -> Self {
        Self::new()
    }
}
impl Upsample3 {
    const TAPS: usize = 16;
    pub fn new() -> Self {
        use std::f64::consts::{PI, TAU};
        let n = Self::TAPS * 3;
        let centre = (n - 1) as f64 / 2.0;
        let cutoff = 7_200.0 / 48_000.0;
        let mut h = vec![0.0f64; n];
        for (i, v) in h.iter_mut().enumerate() {
            let x = i as f64 - centre;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (TAU * cutoff * x).sin() / (PI * x) };
            let phase = i as f64 / (n - 1) as f64;
            let blackman = 0.42 - 0.5 * (TAU * phase).cos() + 0.08 * (2.0 * TAU * phase).cos();
            *v = sinc * blackman;
        }
        let sum: f64 = h.iter().sum();
        let mut phases = [[0.0f32; Self::TAPS]; 3];
        for (i, v) in h.iter().enumerate() {
            // Zero stuffing keeps a third of the level: each phase sums to 1.
            phases[i % 3][i / 3] = (v * 3.0 / sum) as f32;
        }
        Self { phases, history: [0.0; Self::TAPS] }
    }
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &x in input {
            self.history.copy_within(0..Self::TAPS - 1, 1);
            self.history[0] = f32::from(x);
            for phase in &self.phases {
                let y: f32 = phase.iter().zip(&self.history).map(|(c, s)| c * s).sum();
                out.push(y.round().clamp(-32_768.0, 32_767.0) as i16);
            }
        }
    }
}

fn prop_i32(value: i32) -> windows::Win32::System::Com::StructuredStorage::PROPVARIANT {
    let mut p = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
    // SAFETY: a plain VT_I4 value in a zeroed PROPVARIANT (nothing to clear).
    unsafe {
        (*p.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_I4;
        (*p.Anonymous.Anonymous).Anonymous.lVal = value;
    }
    p
}
fn prop_bool(value: bool) -> windows::Win32::System::Com::StructuredStorage::PROPVARIANT {
    let mut p = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
    // SAFETY: a plain VT_BOOL value in a zeroed PROPVARIANT (nothing to clear).
    unsafe {
        (*p.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_BOOL;
        (*p.Anonymous.Anonymous).Anonymous.boolVal = windows::Win32::Foundation::VARIANT_BOOL(if value { -1 } else { 0 });
    }
    p
}

/// Call capture through the Voice Capture DSP (see `VOICE_DSP_SWITCH`).
pub struct VoiceDsp {
    // Interfaces release before the apartment.
    dmo: IMediaObject,
    buffer: IMediaBuffer,
    upsample: Upsample3,
    pending: Vec<i16>,
    scratch: Vec<i16>,
    /// Output calls refused because nothing played yet, and the silent
    /// 20 ms frames sent meanwhile.
    pub no_render: u64,
    pub silence_frames: u64,
    pub packets: u64,
    last_frame: Instant,
    /// One `audio_effects` log line.
    pub effects: String,
    _apartment: Apartment,
}
impl VoiceDsp {
    pub fn open() -> Result<Self, Error> {
        use windows::Win32::{Foundation::PROPERTYKEY, UI::Shell::PropertiesSystem::IPropertyStore};
        let apartment = Apartment::new()?;
        let dmo: IMediaObject = unsafe { CoCreateInstance(&CWMAudioAEC, None, CLSCTX_INPROC_SERVER)? };
        let store: IPropertyStore = windows_core::Interface::cast(&dmo)?;
        unsafe {
            store.SetValue(&PROPERTYKEY { fmtid: WMAAECMA, pid: PID_SYSTEM_MODE }, &prop_i32(SINGLE_CHANNEL_AEC.0))?;
            store.SetValue(&PROPERTYKEY { fmtid: WMAAECMA, pid: PID_DMO_SOURCE_MODE }, &prop_bool(true))?;
        }
        let mut format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 1,
            nSamplesPerSec: DSP_RATE,
            nAvgBytesPerSec: DSP_RATE * 2,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let media = DmoMediaType {
            majortype: MFMediaType_Audio,
            subtype: MFAudioFormat_PCM,
            fixed_size_samples: true.into(),
            temporal_compression: false.into(),
            sample_size: 2,
            formattype: FORMAT_WAVEFORMATEX,
            unk: std::ptr::null_mut(),
            cb_format: std::mem::size_of::<WAVEFORMATEX>() as u32,
            pb_format: (&raw mut format).cast(),
        };
        unsafe {
            dmo.SetOutputType(0, &media, 0).ok()?;
            dmo.AllocateStreamingResources().ok()?;
        }
        let buffer: IMediaBuffer = DmoBuffer {
            data: std::cell::UnsafeCell::new(vec![0u8; DSP_RATE as usize * 2].into_boxed_slice()),
            capacity: DSP_RATE * 2,
            length: std::cell::Cell::new(0),
        }
        .into();
        Ok(Self {
            dmo,
            buffer,
            upsample: Upsample3::new(),
            pending: Vec::new(),
            scratch: Vec::new(),
            no_render: 0,
            silence_frames: 0,
            packets: 0,
            last_frame: Instant::now(),
            effects: format!("audio_effects flow=capture endpoint=voice_capture_dsp category=dsp aec=On effects=dsp:single_channel_aec rate_hz={DSP_RATE}"),
            _apartment: apartment,
        })
    }
    /// Pulls what the DSP has; false when nothing plays yet.
    fn pull(&mut self) -> Result<bool, Error> {
        loop {
            let mut out = DmoOutputDataBuffer {
                buffer: windows_core::Interface::as_raw(&self.buffer),
                status: 0,
                timestamp: 0,
                timelength: 0,
            };
            let mut status = 0u32;
            let hr = unsafe {
                let _ = self.buffer.SetLength(0);
                self.dmo.ProcessOutput(0, 1, &mut out, &mut status)
            };
            if hr.0 == NO_ACTIVE_RENDER_STREAM {
                self.no_render += 1;
                return Ok(false);
            }
            hr.ok()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0u32;
            unsafe { self.buffer.GetBufferAndLength(&mut data, &mut length).ok()? };
            if length > 0 && !data.is_null() {
                self.packets += 1;
                // SAFETY: the DSP wrote `length` bytes of 16-bit PCM into our buffer.
                let samples = unsafe { std::slice::from_raw_parts(data.cast::<i16>(), length as usize / 2) };
                self.scratch.clear();
                self.upsample.process(samples, &mut self.scratch);
                self.pending.extend_from_slice(&self.scratch);
            }
            if out.status & DMO_OUTPUT_DATA_BUFFERF_INCOMPLETE == 0 {
                return Ok(true);
            }
        }
    }
    /// The next whole 20 ms frame at 48 kHz, waiting at most until `deadline`.
    /// Before anything plays the DSP gives no output; silence is then sent
    /// at the real-time pace so the call's audio clock keeps running.
    pub fn next_frame(&mut self, deadline: Instant) -> Result<[i16; FRAME_SAMPLES], Error> {
        loop {
            if self.pending.len() >= FRAME_SAMPLES {
                let mut frame = [0i16; FRAME_SAMPLES];
                frame.copy_from_slice(&self.pending[..FRAME_SAMPLES]);
                self.pending.drain(..FRAME_SAMPLES);
                self.last_frame = Instant::now();
                return Ok(frame);
            }
            if !self.pull()? && self.last_frame.elapsed() >= Duration::from_millis(20) {
                self.silence_frames += 1;
                self.last_frame += Duration::from_millis(20);
                return Ok([0; FRAME_SAMPLES]);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for VoiceDsp {
    fn drop(&mut self) {
        unsafe {
            let _ = self.dmo.FreeStreamingResources();
        }
        self.pending.fill(0);
    }
}

// ---------------------------------------------------------------- measurements (pure)

/// RMS level of 16-bit samples in dBFS (-inf for silence, clamped to -120).
pub fn rms_dbfs(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return -120.0;
    }
    let mean = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / samples.len() as f64;
    if mean <= 0.0 {
        return -120.0;
    }
    (20.0 * (mean.sqrt() / 32768.0).log10()).max(-120.0)
}
/// Mean luma (Y plane) of an NV12 picture, 0..255.
pub fn mean_luma(nv12: &[u8], width: usize, height: usize) -> f64 {
    let y = &nv12[..(width * height).min(nv12.len())];
    if y.is_empty() {
        return 0.0;
    }
    y.iter().map(|v| f64::from(*v)).sum::<f64>() / y.len() as f64
}
/// The first sample index whose absolute value reaches `threshold` (a tone's
/// onset in a captured stream), for an acoustic loopback latency estimate.
pub fn onset(samples: &[i16], threshold: i16) -> Option<usize> {
    samples.iter().position(|s| s.unsigned_abs() >= threshold.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn levels_luma_and_onset_are_measured_without_keeping_anything() {
        assert_eq!(rms_dbfs(&[0; 960]), -120.0);
        let full: Vec<i16> = (0..960).map(|i| if i % 2 == 0 { 32767 } else { -32767 }).collect();
        assert!(rms_dbfs(&full) > -0.01);
        let quiet: Vec<i16> = full.iter().map(|s| s / 100).collect();
        assert!((rms_dbfs(&quiet) + 40.0).abs() < 0.1);
        let mut picture = vec![16u8; 4 * 2 * 3 / 2];
        picture[..8].fill(235);
        assert_eq!(mean_luma(&picture, 4, 2), 235.0);
        let mut stream = vec![0i16; 1000];
        stream[480] = 9000;
        assert_eq!(onset(&stream, 4000), Some(480));
        assert_eq!(onset(&[0; 10], 4000), None);
    }

    #[test]
    fn the_effect_list_says_whether_echo_is_cancelled() {
        const NS: u128 = 0x6f64adbf_8211_11e2_8c70_2c27d7f001fa;
        assert_eq!(aec_state(&[(NS, true), (AEC_EFFECT, true)]), Aec::On);
        assert_eq!(aec_state(&[(AEC_EFFECT, false)]), Aec::Off);
        assert_eq!(aec_state(&[(NS, true)]), Aec::Absent);
        assert_eq!(aec_state(&[]), Aec::Absent);
        assert_eq!(effect_words(&[(AEC_EFFECT, true), (NS, false)]), "aec:on,ns:off");
        assert_eq!(effect_words(&[]), "none");
        assert!(effect_words(&[(1, true)]).ends_with(":on"));
        assert!(!voice_dsp_requested() || std::env::var(VOICE_DSP_SWITCH).is_ok());
    }
    fn tone(rate: f64, hz: f64, n: usize, amplitude: f64) -> Vec<i16> {
        (0..n).map(|i| ((i as f64 / rate * hz * std::f64::consts::TAU).sin() * amplitude) as i16).collect()
    }
    /// The level of `hz` in `x` (48 kHz), by correlation, in dB relative to `amplitude`.
    fn level_db(x: &[i16], hz: f64, amplitude: f64) -> f64 {
        let (mut s, mut c) = (0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            let t = i as f64 / 48_000.0 * hz * std::f64::consts::TAU;
            s += f64::from(*v) * t.sin();
            c += f64::from(*v) * t.cos();
        }
        let a = 2.0 * (s * s + c * c).sqrt() / x.len() as f64;
        20.0 * (a / amplitude).log10()
    }
    #[test]
    fn the_dsp_upsampler_triples_the_rate_keeps_voice_and_removes_images() {
        let mut up = Upsample3::new();
        let mut out = Vec::new();
        up.process(&[1000; 320], &mut out);
        assert_eq!(out.len(), 960, "20 ms at 16 kHz is 20 ms at 48 kHz");
        assert!(out[200..].iter().all(|v| (999..=1001).contains(v)), "unity DC gain");
        // A 1 kHz voice tone keeps its level; its 15 kHz image is removed.
        let mut up = Upsample3::new();
        let mut out = Vec::new();
        up.process(&tone(16_000.0, 1_000.0, 16_000, 8_000.0), &mut out);
        let settled = &out[480..];
        assert!(level_db(settled, 1_000.0, 8_000.0).abs() < 0.5, "{}", level_db(settled, 1_000.0, 8_000.0));
        assert!(level_db(settled, 15_000.0, 8_000.0) < -50.0, "{}", level_db(settled, 15_000.0, 8_000.0));
        // 3.4 kHz (the top of telephone speech) is still within 1 dB.
        let mut up = Upsample3::new();
        let mut out = Vec::new();
        up.process(&tone(16_000.0, 3_400.0, 16_000, 8_000.0), &mut out);
        assert!(level_db(&out[480..], 3_400.0, 8_000.0).abs() < 1.0);
        // Full scale never wraps.
        let mut up = Upsample3::new();
        let mut out = Vec::new();
        let square: Vec<i16> = (0..320).map(|i| if (i / 8) % 2 == 0 { 32_767 } else { -32_768 }).collect();
        up.process(&square, &mut out);
        assert_eq!(out.len(), 960);
    }

    /// Device check, run by hand (`--ignored`): opens the communications
    /// microphone and the voice DSP for half a second each and prints what
    /// they report. Nothing is kept.
    #[test]
    #[ignore = "opens the microphone"]
    fn device_check_communications_capture_and_voice_dsp() {
        println!("baseline {}", Microphone::open_with(None, false, false).expect("microphone").effects);
        // Opened only (output starts at the first write): nothing is played.
        let target = crate::audio_devices::call_render(&crate::audio_devices::list(), None);
        match crate::playback::Wasapi::open_call(target.as_ref().map(|(id, r)| (id.as_str(), r.word()))) {
            Ok(render) => println!("render {}", render.device().map(|d| d.describe()).unwrap_or_default()),
            Err(e) => println!("render open_failed={e:?}"),
        }
        let mut mic = Microphone::open(None, false).expect("microphone");
        println!("{}", mic.effects);
        let until = Instant::now() + Duration::from_millis(500);
        let mut frames = 0;
        while Instant::now() < until {
            if mic.next_frame(Instant::now() + Duration::from_millis(30)).is_ok() {
                frames += 1;
            }
        }
        println!("endpoint frames={frames} packets={} silent={}", mic.packets, mic.silent_packets);
        drop(mic);
        match VoiceDsp::open() {
            Ok(mut dsp) => {
                let until = Instant::now() + Duration::from_millis(500);
                let mut result = Ok(());
                let mut frames = 0;
                while Instant::now() < until && result.is_ok() {
                    result = dsp.next_frame(Instant::now() + Duration::from_millis(30)).map(|_| frames += 1).or_else(|e| if e == Error::Timeout { Ok(()) } else { Err(e) });
                }
                println!("voice_dsp frames={frames} packets={} no_render={} silence={} result={result:?}", dsp.packets, dsp.no_render, dsp.silence_frames);
            }
            Err(e) => println!("voice_dsp open_failed={e:?}"),
        }
    }

    fn noise(seed: &mut u32, amplitude: f64) -> i16 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((f64::from(*seed >> 8) / f64::from(1u32 << 24) - 0.5) * 2.0 * amplitude) as i16
    }
    fn meter(frames: &[(f64, i16)]) -> SignalMeter {
        let mut seed = 7;
        let mut m = SignalMeter::default();
        for &(amplitude, dc) in frames {
            let f: Vec<i16> = (0..960).map(|_| noise(&mut seed, amplitude).saturating_add(dc)).collect();
            m.push(&f);
        }
        m
    }
    #[test]
    fn a_silent_microphone_gets_a_verdict_that_follows_the_data() {
        let deny = vec!["fact=mic_privacy scope=user_desktop_apps value=Deny".to_owned()];
        let flat = meter(&[(0.0, 0); 60]);
        assert!(mic_verdict(&flat, 150, 150, &deny, false).contains("150 of 150 packets SILENT; the privacy registry says Deny"));
        assert!(mic_verdict(&flat, 150, 150, &[], false).contains("does not say Deny"));
        assert!(mic_verdict(&flat, 150, 0, &[], true).contains("muted"));
        let v = mic_verdict(&flat, 150, 0, &[], false);
        assert!(v.contains("quiet or gated") && v.contains("does not mean the microphone is dead") && !v.contains("Deny"), "{v}");
        assert!(mic_verdict(&flat, 0, 0, &[], false).contains("no audio packets"));
        // A room: varying levels around -55 dBFS.
        let room: Vec<(f64, i16)> = (0..60).map(|i| (40.0 + f64::from(i % 7) * 15.0, 0)).collect();
        assert!(mic_verdict(&meter(&room), 150, 0, &[], false).starts_with("mic ok"));
    }
    #[test]
    fn a_startup_transient_or_a_dc_offset_is_flat_and_a_quiet_array_is_live() {
        // The jack case: one loud start-up burst, then digital silence.
        let mut jack = vec![(3000.0, 0); 3];
        jack.extend(vec![(0.4, 0); 60]);
        let m = meter(&jack);
        assert_eq!(classify(&m, 150, 0), Level::Flat, "{}", m.summary());
        // A large constant DC offset with no AC content.
        let m = meter(&[(0.0, 2000); 60]);
        assert_eq!(classify(&m, 150, 0), Level::Flat, "{}", m.summary());
        // A quiet MEMS array: a noise floor near -81 dBFS is live, very quiet.
        let m = meter(&[(5.0, 0); 60]);
        assert!(m.rms_dbfs() > -86.0 && m.rms_dbfs() < -78.0, "{}", m.summary());
        assert_eq!(classify(&m, 150, 0), Level::VeryQuiet, "{}", m.summary());
        assert!(mic_verdict(&m, 150, 0, &[], false).starts_with("mic very quiet"));
        assert_eq!(classify(&meter(&[(400.0, 0); 60]), 150, 0), Level::Ok);
        // Floor and peak for the speech check.
        let mut speech = vec![(5.0, 0); 40];
        speech.extend(vec![(3000.0, 0); 5]);
        let m = meter(&speech);
        assert!(m.peak_dbfs() - m.floor_dbfs() > 40.0, "{}", m.summary());
    }
}
