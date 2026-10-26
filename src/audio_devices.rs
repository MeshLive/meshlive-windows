//! Audio endpoints as they really are on this PC, chosen deliberately and
//! never changed (volume, mute, defaults and privacy are read only).
//!
//! Why: an audio enhancer (e.g. FxSound) installs a virtual render device
//! that becomes the default and forwards to the real speakers, and can leave
//! virtual or silent endpoints behind. A call must still find a microphone
//! that delivers signal and must not depend on the user changing settings.
//! - `list`: every active capture and render endpoint with id, friendly
//!   name, form factor, enumerator (HDAUDIO/USB/BTH... = real, ROOT/SWD =
//!   software or virtual), state, mix format, mute, volume, default roles
//!   and stream latency.
//! - `open_best_microphone`: the user's choice, else the default
//!   communications capture endpoint unless there is evidence against it
//!   (SILENT flags, no packets, muted, privacy Deny; silence alone is not
//!   evidence, DSP noise gates output zeros), else the best other endpoint.
//! - `DeviceWatch`: IMMNotificationClient; an owner reopens its endpoint
//!   when the default changed or its device went away, without ending the call.
use crate::capture::{Level, Microphone, SignalMeter};
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::{PKEY_Device_ContainerId, PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName},
        Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE},
        Media::Audio::{Endpoints::IAudioEndpointVolume, *},
        System::Com::{StructuredStorage::*, *},
    },
    core::{PCWSTR, implement},
};

fn scoped_com() -> bool { std::env::var("MESHLIVE_AUDIO_COM_SCOPE").as_deref() == Ok("1") }
struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn enter(scoped: bool) -> windows::core::Result<Option<Self>> {
        let status = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        // The retained arm keeps its earlier initialization ownership. An
        // existing MTA is borrowed without a matching Uninitialize call.
        if !scoped || status == RPC_E_CHANGED_MODE { return Ok(None); }
        status.ok()?;
        Ok(Some(Self(PhantomData)))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) { unsafe { CoUninitialize(); } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Capture,
    Render,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub flow: Flow,
    pub form_factor: Option<u32>,
    pub enumerator: String,
    pub state: u32,
    pub mix_format: String,
    pub muted: Option<bool>,
    pub volume: Option<f32>,
    pub default_communications: bool,
    pub default_console: bool,
    pub latency_ms: Option<f64>,
    /// The physical device this endpoint belongs to (PKEY_Device_ContainerId,
    /// "{GUID}"); empty when unknown. A laptop's built-in microphone and
    /// speakers share one container; a headset's are another.
    pub container: String,
}
impl Endpoint {
    pub fn is_virtual(&self) -> bool {
        is_virtual(&self.enumerator, &self.name)
    }
    pub fn line(&self) -> String {
        format!(
            "fact=audio_endpoint flow={:?} name=\"{}\" enumerator={} virtual={} form_factor={} state={} default_comms={} default_console={} mix_format=\"{}\" muted={} volume={} device_period_ms={} container={} id={}",
            self.flow,
            self.name,
            self.enumerator,
            self.is_virtual(),
            self.form_factor.map_or("unknown".into(), |f| f.to_string()),
            self.state,
            self.default_communications,
            self.default_console,
            self.mix_format,
            self.muted.map_or("unknown".into(), |m| m.to_string()),
            self.volume.map_or("unknown".into(), |v| format!("{v:.2}")),
            self.latency_ms.map_or("unknown".into(), |l| format!("{l:.1}")),
            if self.container.is_empty() { "unknown" } else { &self.container },
            self.id
        )
    }
}

/// Software or virtual endpoint (pure): its device was enumerated by the
/// root or software-device bus rather than a hardware bus, or it is a known
/// enhancer by name. Hardware buses: HDAUDIO, USB, BTHENUM/BTHHFENUM,
/// INTELAUDIO, ACPI, PCI...
pub fn is_virtual(enumerator: &str, name: &str) -> bool {
    let e = enumerator.to_ascii_uppercase();
    let n = name.to_ascii_lowercase();
    e == "ROOT" || e == "SWD" || e.is_empty() || n.contains("fxsound") || n.contains("virtual") || n.contains("cable")
}

fn property_string(store: &windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let text = PropVariantToStringAlloc(&value).ok();
        let out = text.and_then(|t| {
            let s = t.to_string().ok();
            CoTaskMemFree(Some(t.0.cast()));
            s
        });
        let _ = PropVariantClear(&mut value);
        out
    }
}
fn device_id(device: &IMMDevice) -> String {
    unsafe { device.GetId() }
        .ok()
        .and_then(|id| {
            let text = unsafe { id.to_string() }.ok();
            unsafe { CoTaskMemFree(Some(id.0.cast())) };
            text
        })
        .unwrap_or_default()
}

/// Every active endpoint (call on a thread with COM initialized; `Microphone`
/// and the playback owner use single-threaded apartments).
pub fn list() -> Vec<Endpoint> {
    list_with_scope(scoped_com())
}
fn list_with_scope(scoped: bool) -> Vec<Endpoint> {
    let Ok(_apartment) = Apartment::enter(scoped) else { return Vec::new() };
    let mut out = Vec::new();
    let Ok(enumerator) = (unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }) else {
        return out;
    };
    let default_id = |flow: EDataFlow, role: ERole| unsafe { enumerator.GetDefaultAudioEndpoint(flow, role) }.ok().map(|d| device_id(&d));
    for (flow, kind) in [(eCapture, Flow::Capture), (eRender, Flow::Render)] {
        let comms = default_id(flow, eCommunications);
        let console = default_id(flow, eConsole);
        let Ok(collection) = (unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }) else { continue };
        let count = unsafe { collection.GetCount() }.unwrap_or(0);
        for i in 0..count {
            let Ok(device) = (unsafe { collection.Item(i) }) else { continue };
            let id = device_id(&device);
            let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok();
            let name = store.as_ref().and_then(|s| property_string(s, &PKEY_Device_FriendlyName)).unwrap_or_default();
            let enumerator_name = store.as_ref().and_then(|s| property_string(s, &PKEY_Device_EnumeratorName)).unwrap_or_default();
            let container = store.as_ref().and_then(|s| property_string(s, &PKEY_Device_ContainerId)).unwrap_or_default().to_ascii_lowercase();
            let form_factor = store
                .as_ref()
                .and_then(|s| property_string(s, &PKEY_AudioEndpoint_FormFactor))
                .and_then(|v| v.parse().ok());
            let client = unsafe { device.Activate::<IAudioClient>(CLSCTX_INPROC_SERVER, None) }.ok();
            let mix_format = client
                .as_ref()
                .and_then(|c| unsafe { c.GetMixFormat() }.ok())
                .map(|f| {
                    let w = unsafe { *f };
                    unsafe { CoTaskMemFree(Some(f.cast())) };
                    let (rate, channels, bits) = (w.nSamplesPerSec, w.nChannels, w.wBitsPerSample);
                    format!("{rate} Hz {channels} ch {bits} bit")
                })
                .unwrap_or_else(|| "unknown".into());
            let latency_ms = client.as_ref().and_then(|c| {
                let mut period = 0i64;
                unsafe { c.GetDevicePeriod(Some(&mut period), None) }.ok().map(|_| period as f64 / 10_000.0)
            });
            let volume = unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_INPROC_SERVER, None) }.ok();
            out.push(Endpoint {
                default_communications: comms.as_deref() == Some(id.as_str()),
                default_console: console.as_deref() == Some(id.as_str()),
                id,
                name,
                flow: kind,
                form_factor,
                enumerator: enumerator_name,
                state: unsafe { device.GetState() }.map_or(0, |s| s.0),
                mix_format,
                muted: volume.as_ref().and_then(|v| unsafe { v.GetMute() }.ok()).map(|m| m.as_bool()),
                volume: volume.as_ref().and_then(|v| unsafe { v.GetMasterVolumeLevelScalar() }.ok()),
                latency_ms,
                container,
            });
        }
    }
    out
}

/// The order in which capture endpoints are tried (pure): the default
/// communications device, the default console device, then the other real
/// microphones; virtual ones last.
pub fn capture_order(endpoints: &[Endpoint]) -> Vec<String> {
    let mut captures: Vec<&Endpoint> = endpoints.iter().filter(|e| e.flow == Flow::Capture).collect();
    captures.sort_by_key(|e| (!e.default_communications, !e.default_console, e.is_virtual()));
    captures.into_iter().map(|e| e.id.clone()).collect()
}

/// A probe found a live input (pure): packets arrived, Windows did not flag
/// them SILENT, and the window has an analogue noise floor.
pub fn usable(meter: &SignalMeter, packets: u64, silent_packets: u64) -> bool {
    crate::capture::classify(meter, packets, silent_packets).usable()
}

/// Why call output went to an endpoint (logged on `speaker_open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderReason {
    /// The real (not virtual) render endpoint of the microphone's device.
    SameContainer,
    /// The communications default render endpoint.
    CommsDefault,
}
impl RenderReason {
    pub fn word(self) -> &'static str {
        match self {
            RenderReason::SameContainer => "same_container",
            RenderReason::CommsDefault => "comms_default",
        }
    }
}
/// The call's render endpoint (pure): the render endpoint in the same device
/// container as the microphone (`mic`: its endpoint id, else the
/// communications default capture endpoint), so the device's echo canceller
/// hears what the call plays. Virtual endpoints (enhancers such as FxSound)
/// never qualify; among several, the communications default, then the
/// console default, then the first. Otherwise None: the communications
/// default render endpoint is used. Windows' defaults are never changed.
pub fn call_render(endpoints: &[Endpoint], mic: Option<&str>) -> Option<(String, RenderReason)> {
    let capture = endpoints.iter().filter(|e| e.flow == Flow::Capture);
    let mic = match mic {
        Some(id) => capture.clone().find(|e| e.id == id),
        None => capture.clone().find(|e| e.default_communications),
    }?;
    if mic.container.is_empty() {
        return None;
    }
    endpoints
        .iter()
        .filter(|e| e.flow == Flow::Render && e.container == mic.container && !e.is_virtual())
        .min_by_key(|e| (!e.default_communications, !e.default_console))
        .map(|e| (e.id.clone(), RenderReason::SameContainer))
}

/// Keep the communications default (pure)? Yes unless there is evidence it
/// cannot deliver audio: no packets, SILENT flags, the endpoint muted, or a
/// privacy Deny. Silence alone is not evidence: a DSP noise gate outputs
/// zeros in a quiet room (measured: capture-speak-1, the gated default gave
/// +66 dB for speech while the "livelier" array gave +17.6 dB).
pub fn keep_default(level: Level, muted: bool, denied: bool) -> bool {
    !denied && !muted && !matches!(level, Level::NoPackets | Level::Flagged)
}
/// Order of preference among the other endpoints (pure): real before
/// virtual, then a measured level class, then ambient RMS as the last
/// tiebreak (unreliable with DSP gates). No packets or SILENT flags never
/// qualify.
pub fn best_probe(probes: &[(String, Level, bool, f64)]) -> Option<String> {
    let rank = |l: Level| match l {
        Level::Ok => 3,
        Level::VeryQuiet => 2,
        Level::Flat => 1,
        Level::NoPackets | Level::Flagged => 0,
    };
    probes
        .iter()
        .filter(|(_, l, _, _)| rank(*l) > 0)
        .max_by(|a, b| (!a.2).cmp(&!b.2).then(rank(a.1).cmp(&rank(b.1))).then(a.3.total_cmp(&b.3)))
        .map(|(id, ..)| id.clone())
}

/// Opens the microphone. `choice`: an endpoint id the user picked (used as
/// is). Otherwise the default communications capture endpoint, kept unless
/// `keep_default` finds evidence against it; then the other endpoints by
/// `best_probe`. Returns the microphone with one log line per endpoint
/// probed, or the measured verdict of each.
pub fn open_best_microphone(probe: Duration, choice: Option<&str>) -> Result<(Microphone, Vec<String>), (String, Vec<String>)> {
    let mut log = Vec::new();
    if let Some(id) = choice {
        return match Microphone::open(Some(id), false) {
            Ok(m) => {
                log.push(format!("mic_selected id={id} reason=chosen by the user"));
                Ok((m, log))
            }
            Err(e) => Err((format!("the chosen microphone could not be opened: {e:?}"), log)),
        };
    }
    let endpoints = list();
    let order = capture_order(&endpoints);
    let mut probes: Vec<(String, Level, bool, f64)> = Vec::new();
    let mut verdicts: Vec<(String, bool, String, Level)> = Vec::new();
    let privacy = crate::capture::privacy_facts();
    let denied = privacy.iter().any(|l| l.contains("value=Deny"));
    let mut opened: Vec<(String, Microphone)> = Vec::new();
    for id in &order {
        let endpoint = endpoints.iter().find(|e| &e.id == id);
        let name = endpoint.map(|e| e.name.clone()).unwrap_or_default();
        let is_virtual = endpoint.is_some_and(Endpoint::is_virtual);
        let is_default = endpoint.is_some_and(|e| e.default_communications);
        let mut mic = match Microphone::open(Some(id), false) {
            Ok(m) => m,
            Err(e) => {
                log.push(format!("mic_candidate name=\"{name}\" open_failed={e:?}"));
                continue;
            }
        };
        // The judged window starts after SignalMeter's skipped start-up.
        let until = Instant::now() + probe + Duration::from_millis(20 * u64::from(SignalMeter::SKIP_FRAMES));
        let mut meter = SignalMeter::default();
        while Instant::now() < until {
            match mic.next_frame(until.max(Instant::now() + Duration::from_millis(30))) {
                Ok(frame) => meter.push(&frame),
                Err(_) => break,
            }
        }
        let level = crate::capture::classify(&meter, mic.packets, mic.silent_packets);
        let muted = endpoint.and_then(|e| e.muted) == Some(true);
        let verdict = crate::capture::mic_verdict(&meter, mic.packets, mic.silent_packets, &privacy, muted);
        log.push(format!(
            "mic_candidate name=\"{name}\" virtual={is_virtual} default_comms={is_default} {} packets={} silent_packets={} level={level:?} verdict=\"{verdict}\"",
            meter.summary(),
            mic.packets,
            mic.silent_packets
        ));
        verdicts.push((name.clone(), is_virtual, verdict, level));
        if is_default {
            if keep_default(level, muted, denied) {
                log.push(format!(
                    "mic_selected name=\"{name}\" level={level:?} reason=communications default (no SILENT flags, packets arriving, not muted, no privacy Deny; quiet or gated is not dead)"
                ));
                return Ok((mic, log));
            }
            log.push(format!("mic_default_rejected name=\"{name}\" level={level:?} muted={muted} privacy_deny={denied}"));
            continue;
        }
        probes.push((id.clone(), if muted { Level::Flagged } else { level }, is_virtual, meter.rms_dbfs()));
        opened.push((id.clone(), mic));
    }
    if let Some(best) = best_probe(&probes) {
        if let Some(index) = opened.iter().position(|(id, _)| *id == best) {
            let (_, mic) = opened.swap_remove(index);
            let name = endpoints.iter().find(|e| e.id == best).map(|e| e.name.clone()).unwrap_or_default();
            let level = probes.iter().find(|p| p.0 == best).map(|p| p.1);
            log.push(format!("mic_selected name=\"{name}\" level={level:?} reason=the communications default showed evidence against it; best other endpoint"));
            if matches!(level, Some(Level::VeryQuiet | Level::Flat)) {
                log.push(format!("mic_warning name=\"{name}\" quiet or gated at selection"));
            }
            return Ok((mic, log));
        }
    }
    let each: Vec<String> = verdicts.iter().map(|(name, v, verdict, _)| format!("{name}{}: {verdict}", if *v { " (virtual)" } else { "" })).collect();
    let reason = if each.is_empty() { "no capture endpoint could be opened".to_owned() } else { each.join(" | ") };
    Err((format!("microphone blocked or muted: {reason}"), log))
}

// ---------------------------------------------------------------- device changes

#[implement(IMMNotificationClient)]
struct Notifications {
    changed: Arc<AtomicBool>,
}
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Raised when a default endpoint changed or a device went away; the owner
/// reopens its endpoint (and the call goes on).
pub struct DeviceWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    pub changed: Arc<AtomicBool>,
    // COM objects above must release before this thread's apartment owner.
    _apartment: Option<Apartment>,
}
impl DeviceWatch {
    pub fn start() -> windows::core::Result<Self> {
        Self::start_with_scope(scoped_com())
    }
    fn start_with_scope(scoped: bool) -> windows::core::Result<Self> {
        let apartment = Apartment::enter(scoped)?;
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)? };
        let changed = Arc::new(AtomicBool::new(false));
        let client: IMMNotificationClient = Notifications { changed: changed.clone() }.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        Ok(Self { enumerator, client, changed, _apartment: apartment })
    }
    /// True once per change.
    pub fn take(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
}
impl Drop for DeviceWatch {
    fn drop(&mut self) {
        let _ = unsafe { self.enumerator.UnregisterEndpointNotificationCallback(&self.client) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_enumeration_and_watch_release_their_sta_owners() {
        std::thread::spawn(|| {
            for _ in 0..3 {
                drop(list_with_scope(true));
                unsafe {
                    CoInitializeEx(None, COINIT_MULTITHREADED).ok().expect("enumeration must release its STA initialization");
                    CoUninitialize();
                }
                drop(DeviceWatch::start_with_scope(true).unwrap());
                unsafe {
                    CoInitializeEx(None, COINIT_MULTITHREADED).ok().expect("watch must release COM after unregistering and dropping its objects");
                    CoUninitialize();
                }
            }
        }).join().unwrap();
    }
    #[test]
    fn audio_scope_borrows_an_existing_mta_without_uninitializing_it() {
        std::thread::spawn(|| {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap(); }
            drop(list_with_scope(true));
            drop(DeviceWatch::start_with_scope(true).unwrap());
            assert_eq!(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }, RPC_E_CHANGED_MODE,
                "the caller still owns its MTA");
            unsafe { CoUninitialize(); }
            let sta = Apartment::enter(true).unwrap();
            assert!(sta.is_some(), "the caller's balanced cleanup allows a fresh STA");
        }).join().unwrap();
    }
    #[test]
    fn the_unscoped_audio_control_retains_its_sta_initialization() {
        std::thread::spawn(|| {
            drop(Apartment::enter(false).unwrap());
            assert_eq!(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }, RPC_E_CHANGED_MODE);
            // Explicit cleanup belongs to this negative fixture only.
            unsafe { CoUninitialize(); }
        }).join().unwrap();
    }
    fn endpoint(name: &str, enumerator: &str, comms: bool, console: bool) -> Endpoint {
        Endpoint {
            id: name.into(),
            name: name.into(),
            flow: Flow::Capture,
            form_factor: None,
            enumerator: enumerator.into(),
            state: 1,
            mix_format: String::new(),
            muted: None,
            volume: None,
            default_communications: comms,
            default_console: console,
            latency_ms: None,
            container: String::new(),
        }
    }
    fn in_container(name: &str, enumerator: &str, flow: Flow, comms: bool, console: bool, container: &str) -> Endpoint {
        Endpoint { flow, container: container.into(), ..endpoint(name, enumerator, comms, console) }
    }
    #[test]
    fn call_output_follows_the_microphones_device_and_never_a_virtual_enhancer() {
        const LAPTOP: &str = "{00000000-0000-0000-ffff-ffffffffffff}";
        const HEADSET: &str = "{11111111-0000-0000-0000-000000000001}";
        let list = vec![
            in_container("array", "HDAUDIO", Flow::Capture, true, true, LAPTOP),
            in_container("headset mic", "BTHHFENUM", Flow::Capture, false, false, HEADSET),
            // A software audio enhancer (FxSound) as the communications default render endpoint.
            in_container("FxSound Speakers", "ROOT", Flow::Render, true, false, LAPTOP),
            in_container("speakers", "HDAUDIO", Flow::Render, false, true, LAPTOP),
            in_container("headset", "BTHHFENUM", Flow::Render, false, false, HEADSET),
        ];
        assert_eq!(call_render(&list, None), Some(("speakers".into(), RenderReason::SameContainer)), "the comms default mic's device");
        assert_eq!(call_render(&list, Some("headset mic")), Some(("headset".into(), RenderReason::SameContainer)));
        assert_eq!(call_render(&list, Some("array")).map(|r| r.1.word()), Some("same_container"));
        // Several real renders in the container: comms default, then console default.
        let mut two = list.clone();
        two.push(in_container("hdmi", "HDAUDIO", Flow::Render, false, false, LAPTOP));
        assert_eq!(call_render(&two, None).unwrap().0, "speakers");
        two[3].default_console = false;
        two[4] = in_container("hdmi", "HDAUDIO", Flow::Render, true, false, HEADSET);
        assert_eq!(call_render(&two, None).unwrap().0, "speakers", "only renders of the same container count");
        // No match: the communications default (None).
        assert_eq!(call_render(&list[..3], None), None, "only a virtual render shares the container");
        assert_eq!(call_render(&list, Some("unplugged")), None);
        let unknown = vec![in_container("array", "HDAUDIO", Flow::Capture, true, true, ""), in_container("speakers", "HDAUDIO", Flow::Render, false, true, "")];
        assert_eq!(call_render(&unknown, None), None, "an unknown container never matches");
        assert_eq!(RenderReason::CommsDefault.word(), "comms_default");
    }
    #[test]
    fn virtual_endpoints_are_recognised_and_tried_last() {
        assert!(is_virtual("ROOT", "FxSound Speakers"));
        assert!(is_virtual("SWD", "Something"));
        assert!(is_virtual("HDAUDIO", "FxSound Speakers"), "by name too");
        assert!(!is_virtual("HDAUDIO", "Microphone Array"));
        assert!(!is_virtual("USB", "Headset Microphone"));
        let list = vec![
            endpoint("virtual mic", "ROOT", false, false),
            endpoint("usb", "USB", false, false),
            endpoint("array", "HDAUDIO", false, true),
            endpoint("comms", "BTHENUM", true, false),
        ];
        assert_eq!(capture_order(&list), vec!["comms", "array", "usb", "virtual mic"]);
        assert!(!usable(&SignalMeter::default(), 150, 0));
        // The communications default stays on silence; only evidence moves it.
        assert!(keep_default(Level::Flat, false, false), "a gated default is quiet, not dead");
        assert!(keep_default(Level::VeryQuiet, false, false));
        assert!(!keep_default(Level::Flagged, false, false));
        assert!(!keep_default(Level::NoPackets, false, false));
        assert!(!keep_default(Level::Ok, true, false), "muted");
        assert!(!keep_default(Level::Ok, false, true), "privacy Deny");
        let probes = vec![
            ("flagged".to_owned(), Level::Flagged, false, -50.0),
            ("gated".to_owned(), Level::Flat, false, -97.0),
            ("array".to_owned(), Level::VeryQuiet, false, -81.0),
            ("usb".to_owned(), Level::VeryQuiet, false, -84.0),
            ("virtual".to_owned(), Level::Ok, true, -40.0),
        ];
        assert_eq!(best_probe(&probes).as_deref(), Some("array"), "real first, then class, RMS last");
        assert_eq!(best_probe(&probes[..2]).as_deref(), Some("gated"), "a gated real endpoint still qualifies");
        assert_eq!(best_probe(&probes[..1]), None);
        assert_eq!(best_probe(&probes[4..]).as_deref(), Some("virtual"));
    }
}
