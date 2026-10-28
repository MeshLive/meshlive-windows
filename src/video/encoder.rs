//! Media Foundation hardware VP9/AV1 (and, Design HC, HEVC/H.264) encoding
//! from NV12 frames. Hardware
//! encoder MFTs are asynchronous: they are unlocked explicitly and driven by
//! their NeedInput/HaveOutput events, polled with a bounded deadline (never a
//! blocking wait). Settings follow Android's encoder rules: constant bitrate,
//! low latency, a keyframe at most every 60 s and on request. The input is
//! system-memory NV12, so no capture device or camera is involved; the caller
//! supplies frames (the generated test pattern in tests and probes).
use super::DecodeDevice;
use super::annexb::{Codec, Gate, Packetiser, session_level};
use super::codecs::Switches;
use std::{
    mem::ManuallyDrop,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Graphics::Direct3D11::ID3D11Texture2D,
        Media::MediaFoundation::*,
        System::{Com::CoTaskMemFree, Variant::VARIANT},
    },
    core::{GUID, Interface},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// No usable encoder for this codec on this PC.
    Unsupported,
    Input,
    Timeout,
    Closed,
    Native(i32),
}
impl From<windows::core::Error> for EncodeError {
    fn from(error: windows::core::Error) -> Self {
        Self::Native(error.code().0)
    }
}
impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Video encode error: {self:?}")
    }
}
impl std::error::Error for EncodeError {}
type Result<T> = std::result::Result<T, EncodeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSettings {
    /// 2 = VP9, 3 = AV1, 4 = H.264, 5 = HEVC (the BTV3 codec numbers).
    pub codec: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Android sends a safety keyframe every 60 s; others are on request.
    pub keyframe_interval_s: u32,
}
impl EncoderSettings {
    fn subtype(&self) -> Result<GUID> {
        match self.codec {
            2 => Ok(MFVideoFormat_VP90),
            3 => Ok(MFVideoFormat_AV1),
            4 => Ok(MFVideoFormat_H264),
            5 => Ok(MFVideoFormat_HEVC),
            _ => Err(EncodeError::Input),
        }
    }
    /// Design HC: the Media Foundation profiles to try, in order (HEVC Main;
    /// H.264 High, Main, then Constrained Baseline), and the session level
    /// (the lowest covering one, `annexb::session_level`) unless its control
    /// arm is off. None for VP9 and AV1.
    fn hc_profiles(&self) -> Option<(&'static [u32], Option<u32>)> {
        let codec = Codec::of(self.codec)?;
        let profiles: &'static [u32] = match codec {
            Codec::Hevc => &[eAVEncH265VProfile_Main_420_8.0 as u32],
            Codec::H264 => &[eAVEncH264VProfile_High.0 as u32, eAVEncH264VProfile_Main.0 as u32, eAVEncH264VProfile_ConstrainedBase.0 as u32],
        };
        let level = Switches::current()
            .session_level
            .then(|| session_level(codec, self.width, self.height, self.fps, self.bitrate_kbps * 1000));
        Some((profiles, level))
    }
    pub(crate) fn frame_bytes(&self) -> usize {
        (self.width * self.height * 3 / 2) as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub key_frame: bool,
}

struct Output(MFT_OUTPUT_DATA_BUFFER);
impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.0.pSample);
            ManuallyDrop::drop(&mut self.0.pEvents);
        }
    }
}

pub struct Encoder<'a> {
    owner: &'a DecodeDevice,
    /// True when the MFT took the owner's DXGI device manager (texture input).
    d3d: bool,
    native: Option<IMFTransform>,
    activation: IMFActivate,
    events: IMFMediaEventGenerator,
    api: Option<ICodecAPI>,
    settings: EncoderSettings,
    name: String,
    need_input: u32,
    output: Vec<EncodedUnit>,
    draining: bool,
    controls: Vec<(&'static str, i32)>,
    inputs: u64,
    /// Design HC: the output profile and level that the MFT accepted.
    profile_level: Option<(u32, Option<u32>)>,
}
impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        if let Some(native) = self.native.take() {
            unsafe {
                let _ = native.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
        unsafe {
            let _ = self.activation.ShutdownObject();
        }
    }
}

/// Hardware encoders for NV12 input and this codec, best first; with `luid`,
/// only those on that adapter (MFTEnum2 + MFT_ENUM_ADAPTER_LUID, the
/// documented filter: MFTEnumEx results carry no adapter).
fn candidates_on(subtype: GUID, luid: Option<i64>) -> Result<Vec<IMFActivate>> {
    let Some(luid) = luid else { return candidates(subtype) };
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.ok_or(EncodeError::Unsupported)?;
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid.to_le_bytes())? };
    let mut pointer: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attributes,
            &mut pointer,
            &mut count,
        )?;
    }
    Ok(take_activations(pointer, count))
}
fn take_activations(pointer: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    let mut out = Vec::with_capacity(count as usize);
    if !pointer.is_null() {
        unsafe {
            for index in 0..count as usize {
                if let Some(activation) = (*pointer.add(index)).take() {
                    out.push(activation);
                }
            }
            CoTaskMemFree(Some(pointer.cast()));
        }
    }
    out
}
fn candidates(subtype: GUID) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut pointer: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut pointer,
            &mut count,
        )?;
    }
    Ok(take_activations(pointer, count))
}

/// One enumerated hardware encoder MFT (read-only facts, not a successful open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareCandidate {
    /// Position among the MFTs of this adapter (Media Foundation's order).
    pub index: usize,
    pub name: String,
    /// The adapter the MFT runs on.
    pub adapter_luid: i64,
    pub adapter: String,
}
/// Every hardware encoder MFT for `codec`, per hardware adapter.
pub fn hardware_candidates(codec: u8) -> Result<Vec<HardwareCandidate>> {
    let subtype = EncoderSettings { codec, width: 0, height: 0, fps: 0, bitrate_kbps: 0, keyframe_interval_s: 0 }
        .subtype()?;
    let mut out = Vec::new();
    for (luid, adapter) in super::encode_d3d::adapters()? {
        for (index, a) in candidates_on(subtype, Some(luid))?.iter().enumerate() {
            out.push(HardwareCandidate { index, name: friendly_name(a), adapter_luid: luid, adapter: adapter.clone() });
        }
    }
    Ok(out)
}

/// Names of the hardware encoders this PC offers for `codec` (read-only).
pub fn inventory(codec: u8) -> Result<Vec<String>> {
    let subtype = EncoderSettings {
        codec,
        width: 0,
        height: 0,
        fps: 0,
        bitrate_kbps: 0,
        keyframe_interval_s: 0,
    }
    .subtype()?;
    Ok(candidates(subtype)?.iter().map(friendly_name).collect())
}
fn friendly_name(activation: &IMFActivate) -> String {
    let mut pointer = windows::core::PWSTR::null();
    let mut length = 0;
    unsafe {
        if activation
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pointer, &mut length)
            .is_ok()
        {
            let name = pointer.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pointer.0 as *const _));
            return name;
        }
    }
    String::new()
}

/// Opt-in diagnostics (MESH_ENCODE_TRACE): which open step an MFT refused.
fn trace_open(name: &str, step: &str, error: &windows::core::Error) {
    if std::env::var_os("MESH_ENCODE_TRACE").is_some() {
        eprintln!("encoder open: {name}: {step} failed {:#010x}", error.code().0);
    }
}

/// The output type with the Design HC profile and level, when given.
fn output_type(subtype: GUID, s: &EncoderSettings, profile_level: Option<(u32, Option<u32>)>) -> Result<IMFMediaType> {
    let value = video_type(subtype, s)?;
    if let Some((profile, level)) = profile_level {
        unsafe {
            value.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            if let Some(level) = level {
                value.SetUINT32(&MF_MT_MPEG2_LEVEL, level)?;
            }
        }
    }
    Ok(value)
}
fn video_type(subtype: GUID, s: &EncoderSettings) -> Result<IMFMediaType> {
    let value = unsafe { MFCreateMediaType()? };
    unsafe {
        value.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        value.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        value.SetUINT64(&MF_MT_FRAME_SIZE, (s.width as u64) << 32 | s.height as u64)?;
        value.SetUINT64(&MF_MT_FRAME_RATE, (s.fps as u64) << 32 | 1)?;
        value.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1u64 << 32 | 1)?;
        value.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if subtype != MFVideoFormat_NV12 {
            value.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate_kbps * 1000)?;
        }
    }
    Ok(value)
}

impl<'a> Encoder<'a> {
    pub fn open(owner: &'a DecodeDevice, settings: EncoderSettings) -> Result<Self> {
        if settings.width == 0
            || settings.height == 0
            || settings.width % 2 != 0
            || settings.height % 2 != 0
            || !(1..=120).contains(&settings.fps)
            || settings.bitrate_kbps == 0
        {
            return Err(EncodeError::Input);
        }
        let subtype = settings.subtype()?;
        // Encoders on the owner's adapter first (they can take its textures
        // without a copy), then every other one in Media Foundation's order.
        let own = super::encode_d3d::device_luid(&owner.gpu);
        let mut list: Vec<(IMFActivate, bool)> = Vec::new();
        if own.is_some() {
            list.extend(candidates_on(subtype, own)?.into_iter().map(|a| (a, true)));
        }
        list.extend(candidates(subtype)?.into_iter().map(|a| (a, own.is_none())));
        let mut last = EncodeError::Unsupported;
        for (activation, same_adapter) in list {
            match Self::activate(owner, settings, subtype, activation, same_adapter) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
    /// Forced selection of one enumerated MFT. Texture input only when the
    /// owner's device is on the candidate's adapter.
    pub fn open_candidate(owner: &'a DecodeDevice, settings: EncoderSettings, candidate: &HardwareCandidate) -> Result<Self> {
        let subtype = settings.subtype()?;
        let activation = candidates_on(subtype, Some(candidate.adapter_luid))?
            .into_iter()
            .nth(candidate.index)
            .ok_or(EncodeError::Unsupported)?;
        let same = super::encode_d3d::device_luid(&owner.gpu) == Some(candidate.adapter_luid);
        Self::activate(owner, settings, subtype, activation, same)
    }
    /// True when input textures go to the MFT directly (no CPU copy).
    pub fn texture_input(&self) -> bool {
        self.d3d
    }
    /// Opens with texture input when possible; an MFT that fails with the
    /// D3D manager is shut down and opened again for system-memory input.
    fn activate(
        owner: &'a DecodeDevice,
        settings: EncoderSettings,
        subtype: GUID,
        activation: IMFActivate,
        same_adapter: bool,
    ) -> Result<Self> {
        match Self::activate_once(owner, settings, subtype, activation.clone(), same_adapter) {
            Err(_) if same_adapter => {
                unsafe {
                    let _ = activation.ShutdownObject();
                }
                Self::activate_once(owner, settings, subtype, activation, false)
            }
            result => result,
        }
    }
    fn activate_once(
        owner: &'a DecodeDevice,
        settings: EncoderSettings,
        subtype: GUID,
        activation: IMFActivate,
        same_adapter: bool,
    ) -> Result<Self> {
        let name = friendly_name(&activation);
        let native: IMFTransform =
            unsafe { activation.ActivateObject() }.inspect_err(|e| trace_open(&name, "ActivateObject", e))?;
        let attributes = unsafe { native.GetAttributes()? };
        unsafe {
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        // D3D11 input: only for a D3D11-aware MFT on the owner's adapter (an MFT
        // on another GPU cannot use this device). If the MFT refuses the
        // manager, frames go in as system memory (`encode_texture` reads back).
        let aware = unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        let d3d = aware
            && same_adapter
            && unsafe { native.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, owner.manager.as_raw() as usize) }.is_ok();
        let events: IMFMediaEventGenerator = native.cast()?;
        // Rate control and GOP go before the media types (Microsoft encoder
        // docs: some encoders read them only when the output type is set).
        // Each result is kept: an ignored control is reported, not assumed.
        let api = native.cast::<ICodecAPI>().ok();
        let mut controls = Vec::new();
        if let Some(api) = &api {
            let gop = settings.keyframe_interval_s.max(1) * settings.fps;
            let bits = settings.bitrate_kbps * 1000;
            let wanted: [(&'static str, &GUID, VARIANT); 6] = [
                // Constant bitrate (eAVEncCommonRateControlMode_CBR = 0), as Android.
                ("rate control CBR", &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(0u32)),
                ("mean bitrate", &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bits)),
                ("max bitrate", &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bits)),
                // Android's libaom rate buffer: 1 s (NativeAomTuning.rateBufferMs).
                ("buffer 1 s", &CODECAPI_AVEncCommonBufferSize, VARIANT::from(bits)),
                ("low latency", &CODECAPI_AVLowLatencyMode, VARIANT::from(true)),
                ("GOP size", &CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop)),
            ];
            for (label, id, value) in wanted {
                let code = unsafe { api.SetValue(id, &value) }.map_or_else(|e| e.code().0, |_| 0);
                controls.push((label, code));
            }
            // Design HC: no B-frames (one picture per access unit, no
            // reordering) and closed GOPs; the emitted NAL types are checked
            // by the packetiser, not assumed from these results. Measured
            // 2026-10-28: NVIDIA's H.264 and HEVC MFTs reject both setters
            // (E_INVALIDARG), yet emit neither B-frames nor reordering at low
            // latency; the start check and the receiver gate verify output.
            if settings.hc_profiles().is_some() {
                let hc: [(&'static str, &GUID, VARIANT); 2] = [
                    ("no B-frames", &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32)),
                    ("closed GOP", &CODECAPI_AVEncMPVGOPOpen, VARIANT::from(false)),
                ];
                for (label, id, value) in hc {
                    let code = unsafe { api.SetValue(id, &value) }.map_or_else(|e| e.code().0, |_| 0);
                    controls.push((label, code));
                }
            }
        }
        // Design HC: the first profile (then level) the MFT accepts; VP9/AV1
        // keep today's single attempt.
        let attempts: Vec<Option<(u32, Option<u32>)>> = match settings.hc_profiles() {
            None => vec![None],
            Some((profiles, level)) => level
                .into_iter()
                .map(Some)
                .chain([None])
                .flat_map(|level| profiles.iter().map(move |&p| Some((p, level))))
                .collect(),
        };
        let mut profile_level = None;
        let mut accepted = false;
        let mut last = None;
        for attempt in attempts {
            let output = output_type(subtype, &settings, attempt)?;
            unsafe {
                let _ = output.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, settings.keyframe_interval_s.max(1) * settings.fps);
            }
            match unsafe { native.SetOutputType(0, &output, 0) } {
                Ok(()) => {
                    profile_level = attempt;
                    accepted = true;
                    break;
                }
                Err(e) => {
                    trace_open(&name, "SetOutputType", &e);
                    last = Some(e);
                }
            }
        }
        if !accepted {
            return Err(last.map_or(EncodeError::Unsupported, EncodeError::from));
        }
        if let Some((profile, level)) = profile_level {
            controls.push(("profile", profile as i32));
            controls.push(("level", level.map_or(-1, |l| l as i32)));
        }
        unsafe {
            native
                .SetInputType(0, &video_type(MFVideoFormat_NV12, &settings)?, 0)
                .inspect_err(|e| trace_open(&name, "SetInputType", e))?;
        }
        unsafe {
            native
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .inspect_err(|e| trace_open(&name, "BEGIN_STREAMING", e))?;
            native
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .inspect_err(|e| trace_open(&name, "START_OF_STREAM", e))?;
        }
        Ok(Self {
            owner,
            d3d,
            native: Some(native),
            activation,
            events,
            api,
            settings,
            name,
            need_input: 0,
            controls,
            inputs: 0,
            output: Vec::new(),
            draining: false,
            profile_level,
        })
    }
    /// The output type's sequence header (Design HC: the parameter sets of
    /// an H.264/HEVC MFT), when the MFT publishes one.
    pub fn sequence_header(&self) -> Option<Vec<u8>> {
        let native = self.native.as_ref()?;
        let current = unsafe { native.GetOutputCurrentType(0) }.ok()?;
        let size = unsafe { current.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
        let mut blob = vec![0; size as usize];
        unsafe { current.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.ok()?;
        Some(blob)
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }
    /// Each ICodecAPI control tried at open and its HRESULT (0 = accepted).
    pub fn controls(&self) -> &[(&'static str, i32)] {
        &self.controls
    }
    /// Handles one pending event; `Ok(None)` when none is available yet.
    fn pump_event(&mut self) -> Result<Option<MF_EVENT_TYPE>> {
        let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let kind = MF_EVENT_TYPE(unsafe { event.GetType()? } as i32);
        let status = unsafe { event.GetStatus()? };
        if status.is_err() {
            return Err(EncodeError::Native(status.0));
        }
        if kind == METransformNeedInput {
            self.need_input += 1;
        } else if kind == METransformHaveOutput {
            self.collect()?;
        }
        Ok(Some(kind))
    }
    fn collect(&mut self) -> Result<()> {
        let native = self.native.as_ref().ok_or(EncodeError::Closed)?;
        for _ in 0..4 {
            let mut output = Output(MFT_OUTPUT_DATA_BUFFER::default());
            let mut status = 0;
            match unsafe { native.ProcessOutput(0, std::slice::from_mut(&mut output.0), &mut status) }
            {
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let subtype = self.settings.subtype()?;
                    unsafe { native.SetOutputType(0, &output_type(subtype, &self.settings, self.profile_level)?, 0)? };
                    continue;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            let sample = output.0.pSample.take().ok_or(EncodeError::Native(0))?;
            let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
            let mut pointer = std::ptr::null_mut();
            let mut length = 0;
            let data = unsafe {
                buffer.Lock(&mut pointer, None, Some(&mut length))?;
                let data = std::slice::from_raw_parts(pointer, length as usize).to_vec();
                buffer.Unlock()?;
                data
            };
            let time = unsafe { sample.GetSampleTime().unwrap_or(0) };
            let key = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) } != 0;
            if !data.is_empty() {
                self.output.push(EncodedUnit {
                    data,
                    pts_us: time / 10,
                    key_frame: key,
                });
            }
            return Ok(());
        }
        Err(EncodeError::Native(0))
    }
    /// Encodes one NV12 frame; returns every unit the encoder finished so far.
    pub fn encode(&mut self, nv12: &[u8], pts_us: i64, force_key: bool, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        if self.native.is_none() || self.draining {
            return Err(EncodeError::Closed);
        }
        if nv12.len() != self.settings.frame_bytes() {
            return Err(EncodeError::Input);
        }
        let result = self.encode_inner(nv12, pts_us, force_key, deadline);
        if result.is_err() {
            self.native = None;
        }
        result
    }
    fn force(&self, force_key: bool) {
        // The first input is a keyframe anyway; forcing it as well made one
        // encoder emit a second keyframe right after it.
        if force_key && self.inputs > 0 {
            if let Some(api) = &self.api {
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
        }
    }
    fn wait_for_input(&mut self, deadline: Instant) -> Result<()> {
        while self.need_input == 0 {
            if self.pump_event()?.is_none() {
                if Instant::now() >= deadline {
                    return Err(EncodeError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
    fn encode_inner(&mut self, nv12: &[u8], pts_us: i64, force_key: bool, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        self.wait_for_input(deadline)?;
        self.force(force_key);
        let buffer = unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut pointer = std::ptr::null_mut();
            buffer.Lock(&mut pointer, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), pointer, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            buffer
        };
        self.submit(&buffer, pts_us)
    }
    fn submit(&mut self, buffer: &IMFMediaBuffer, pts_us: i64) -> Result<Vec<EncodedUnit>> {
        let sample = unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(buffer)?;
            sample.SetSampleTime(pts_us.checked_mul(10).ok_or(EncodeError::Input)?)?;
            sample.SetSampleDuration(10_000_000 / self.settings.fps as i64)?;
            sample
        };
        let native = self.native.as_ref().ok_or(EncodeError::Closed)?;
        unsafe { native.ProcessInput(0, &sample, 0)? };
        self.need_input -= 1;
        self.inputs += 1;
        // Collect whatever is ready without waiting.
        while self.pump_event()?.is_some() {}
        Ok(std::mem::take(&mut self.output))
    }
    /// Encodes one NV12 texture of the owner's device. With texture input the
    /// MFT reads it on the GPU (no CPU copy); the texture must stay unchanged
    /// until its unit comes back (use a pool). Otherwise it is read back once
    /// into system memory (the fallback for MFTs that refuse D3D input).
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        pts_us: i64,
        force_key: bool,
        deadline: Instant,
    ) -> Result<Vec<EncodedUnit>> {
        if !self.d3d {
            let nv12 = super::encode_d3d::read_nv12(&self.owner.gpu, texture, subresource, &self.settings)?;
            return self.encode(&nv12, pts_us, force_key, deadline);
        }
        if self.native.is_none() || self.draining {
            return Err(EncodeError::Closed);
        }
        let result = (|| {
            self.wait_for_input(deadline)?;
            self.force(force_key);
            let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, subresource, false)? };
            self.submit(&buffer, pts_us)
        })();
        if result.is_err() {
            self.native = None;
        }
        result
    }
    /// Waits (bounded, polling) until at least one unit is ready; for latency
    /// measurement and for callers that want each picture's output in turn.
    pub fn wait_output(&mut self, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        while self.output.is_empty() {
            if self.pump_event()?.is_none() {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        Ok(std::mem::take(&mut self.output))
    }
    /// Changes the CBR target without a restart (ICodecAPI mean bit rate).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if kbps == 0 {
            return Err(EncodeError::Input);
        }
        let api = self.api.as_ref().ok_or(EncodeError::Unsupported)?;
        unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(kbps * 1000))? };
        self.settings.bitrate_kbps = kbps;
        Ok(())
    }
    /// End of input: returns the remaining units after the encoder drained.
    pub fn finish(&mut self, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        let native = self.native.as_ref().ok_or(EncodeError::Closed)?;
        unsafe {
            native.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            native.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.draining = true;
        loop {
            match self.pump_event()? {
                Some(kind) if kind == METransformDrainComplete => break,
                Some(_) => {}
                None => {
                    if Instant::now() >= deadline {
                        return Err(EncodeError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(std::mem::take(&mut self.output))
    }
}

/// Which implementation encodes a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// A Media Foundation hardware encoder MFT.
    Hardware,
    /// The pinned static libaom AV1 encoder (CPU).
    SoftwareAv1,
    /// The pinned static libvpx VP9 encoder (CPU).
    SoftwareVp9,
}

/// What this PC can encode (read-only enumeration for VP9/AV1, where
/// activation may still fail; Design HC encoders only after `hc_hardware`'s
/// start check, and only with their master on).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Availability {
    pub av1_hardware: Vec<String>,
    pub vp9_hardware: Vec<String>,
    pub hevc_hardware: Vec<String>,
    pub h264_hardware: Vec<String>,
    /// libaom version; always built in.
    pub av1_software: String,
    /// libvpx version; always built in.
    pub vp9_software: String,
}
pub fn availability() -> Result<Availability> {
    Ok(Availability {
        av1_hardware: inventory(3)?,
        vp9_hardware: inventory(2)?,
        hevc_hardware: hc_hardware(5),
        h264_hardware: hc_hardware(4),
        av1_software: super::encode_aom::version(),
        vp9_software: super::encode_vpx::version(),
    })
}

/// Design HC components this process found unusable (`no_idr`, `reorder`,
/// an unsupported bitstream): never offered again until restart.
static INELIGIBLE: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub fn mark_ineligible(name: &str) {
    let mut list = INELIGIBLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !list.iter().any(|n| n == name) {
        list.push(name.to_owned());
    }
}
fn ineligible(name: &str) -> bool {
    INELIGIBLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().any(|n| n == name)
}

/// Design HC: the hardware encoder of `codec` (4 H.264, 5 HEVC) that this
/// PC really starts, as a one-entry list; empty while the codec's master is
/// off or nothing passes. Checked once per process on its own thread
/// (`hc_check`); a component later marked ineligible drops out.
pub fn hc_hardware(codec: u8) -> Vec<String> {
    if !Switches::current().codec_on(codec) {
        return Vec::new();
    }
    match hc_checked(codec) {
        Some(Ok(name)) if !ineligible(name) => vec![name.clone()],
        _ => Vec::new(),
    }
}
/// The cached `hc_check` result (None: not an HC codec).
pub fn hc_checked(codec: u8) -> Option<&'static std::result::Result<String, String>> {
    static H264: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    static HEVC: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    let cell = match codec {
        4 => &H264,
        5 => &HEVC,
        _ => return None,
    };
    Some(cell.get_or_init(|| std::thread::spawn(move || hc_check(codec)).join().unwrap_or_else(|_| Err("check panicked".into()))))
}
/// The start check behind `hc_hardware`: a hardware MFT opens at 426x240/15
/// on a fresh device, and its pictures (the third forced) come out as access
/// units the packetiser accepts and the receiver's gate passes, with at
/// least two IDRs. Returns the encoder's name, or why it failed.
pub fn hc_check(codec: u8) -> std::result::Result<String, String> {
    let kind = Codec::of(codec).ok_or("not an HC codec")?;
    let (gpu, _, _) = super::device(false, false).map_err(|e| format!("device {e}"))?;
    let owner = DecodeDevice::new(&gpu).map_err(|e| format!("media foundation {e}"))?;
    let settings = EncoderSettings { codec, width: 426, height: 240, fps: 15, bitrate_kbps: 200, keyframe_interval_s: 60 };
    let mut encoder = Encoder::open(&owner, settings).map_err(|e| format!("open {e:?}"))?;
    let name = encoder.name().to_owned();
    let mut packetiser = Packetiser::new(kind);
    if let Some(header) = encoder.sequence_header() {
        packetiser.on_codec_config(&header);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut units = Vec::new();
    for i in 0..4u32 {
        let picture = crate::generated::pattern_nv12(426, 240, i);
        units.extend(encoder.encode(&picture, i64::from(i) * 66_667, i == 2, deadline).map_err(|e| format!("{name}: encode {e:?}"))?);
        units.extend(encoder.wait_output(Instant::now() + Duration::from_millis(500)).map_err(|e| format!("{name}: output {e:?}"))?);
    }
    units.extend(encoder.finish(deadline).map_err(|e| format!("{name}: drain {e:?}"))?);
    let mut gate = Gate::new(kind, 426, 240);
    let (mut keys, mut pictures) = (0, 0);
    for unit in &units {
        let packet = match packetiser.on_access_unit(&unit.data, unit.pts_us) {
            Ok(packet) => packet,
            Err(super::annexb::PacketiseError::NoSlice) => continue,
            Err(e) => return Err(format!("{name}: {}", e.label())),
        };
        gate.check(&packet.data, packet.key).map_err(|r| format!("{name}: {}", r.line(kind)))?;
        keys += usize::from(packet.key);
        pictures += 1;
    }
    if pictures < 3 || keys < 2 {
        return Err(format!("{name}: {pictures} pictures, {keys} IDRs (a forced keyframe that is not an IDR?)"));
    }
    Ok(name)
}

/// BTV3 codec bits of a decode mask: bit 0 VP9, bit 1 AV1 (the offer's codec
/// mask order: VP9 then AV1). Design HC's bits: `codecs::HELLO_*`.
pub const MASK_VP9: u8 = 1;
pub const MASK_AV1: u8 = 2;
/// Decode-mask bit 7 (CF-MEDIA-50): the peer prefers VP9; a sender that can
/// encode VP9 sends VP9.
pub const DECODE_PREFERS_VP9: u8 = super::codecs::DECODE_PREFERS_VP9;

/// Android's encoder order: hardware first, AV1 before VP9 when both exist,
/// then the optimized software AV1 encoder; software VP9 (Windows only,
/// Android has none) comes last, for VP9-only peers on PCs without a hardware
/// VP9 encoder. Only codecs in the peer's decode mask are eligible; an empty
/// plan means no common codec (the offer is hidden).
pub fn plan(peer_decode_mask: u8, available: &Availability) -> Vec<(u8, Backend)> {
    let mut out = Vec::new();
    if peer_decode_mask & MASK_AV1 != 0 && !available.av1_hardware.is_empty() {
        out.push((3, Backend::Hardware));
    }
    if peer_decode_mask & MASK_VP9 != 0 && !available.vp9_hardware.is_empty() {
        out.push((2, Backend::Hardware));
    }
    if peer_decode_mask & MASK_AV1 != 0 {
        out.push((3, Backend::SoftwareAv1));
    }
    if peer_decode_mask & MASK_VP9 != 0 {
        out.push((2, Backend::SoftwareVp9));
    }
    // CF-MEDIA-50: "prefers VP9" puts VP9 first (this PC always has libvpx).
    if peer_decode_mask & DECODE_PREFERS_VP9 != 0 && peer_decode_mask & MASK_VP9 != 0 {
        out.sort_by_key(|(codec, _)| u8::from(*codec != 2));
    }
    out
}

/// One encoder for one source, hardware or software, with one interface.
pub enum VideoEncoder<'a> {
    Hardware(Encoder<'a>),
    Software(super::encode_aom::AomEncoder),
    SoftwareVp9(super::encode_vpx::VpxEncoder),
}
impl<'a> VideoEncoder<'a> {
    /// Opens `backend` for `settings.codec`. A hardware open that fails is an
    /// error, never a silent switch: the caller walks `plan` and shows the
    /// selected encoder (Android never changes the selection silently).
    pub fn open(owner: &'a DecodeDevice, settings: EncoderSettings, backend: Backend) -> Result<Self> {
        match backend {
            Backend::Hardware => Encoder::open(owner, settings).map(Self::Hardware),
            Backend::SoftwareAv1 => super::encode_aom::AomEncoder::open(settings).map(Self::Software),
            Backend::SoftwareVp9 => super::encode_vpx::VpxEncoder::open(settings).map(Self::SoftwareVp9),
        }
    }
    /// The first entry of `plan` that opens, with the failures it skipped.
    pub fn open_planned(
        owner: &'a DecodeDevice,
        mut settings: EncoderSettings,
        plan: &[(u8, Backend)],
    ) -> Result<(Self, Vec<(u8, Backend, EncodeError)>)> {
        let mut skipped = Vec::new();
        for &(codec, backend) in plan {
            settings.codec = codec;
            match Self::open(owner, settings, backend) {
                Ok(encoder) => return Ok((encoder, skipped)),
                Err(error) => skipped.push((codec, backend, error)),
            }
        }
        Err(skipped.last().map_or(EncodeError::Unsupported, |s| s.2))
    }
    pub fn backend(&self) -> Backend {
        match self {
            Self::Hardware(_) => Backend::Hardware,
            Self::Software(_) => Backend::SoftwareAv1,
            Self::SoftwareVp9(_) => Backend::SoftwareVp9,
        }
    }
    pub fn name(&self) -> String {
        match self {
            Self::Hardware(e) => e.name().to_owned(),
            Self::Software(_) => format!("libaom {} (software AV1)", super::encode_aom::version()),
            Self::SoftwareVp9(_) => format!("libvpx {} (software VP9)", super::encode_vpx::version()),
        }
    }
    pub fn settings(&self) -> EncoderSettings {
        match self {
            Self::Hardware(e) => e.settings(),
            Self::Software(e) => e.settings(),
            Self::SoftwareVp9(e) => e.settings(),
        }
    }
    pub fn encode(&mut self, nv12: &[u8], pts_us: i64, force_key: bool, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        match self {
            Self::Hardware(e) => e.encode(nv12, pts_us, force_key, deadline),
            Self::Software(e) => e.encode(nv12, pts_us, force_key),
            Self::SoftwareVp9(e) => e.encode(nv12, pts_us, force_key),
        }
    }
    /// One NV12 texture of `gpu` (the owner's device): straight to a hardware
    /// encoder with texture input, otherwise read back once (software
    /// encoders, MFTs that refuse D3D input).
    pub fn encode_texture(
        &mut self,
        gpu: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        texture: &ID3D11Texture2D,
        subresource: u32,
        pts_us: i64,
        force_key: bool,
        deadline: Instant,
    ) -> Result<Vec<EncodedUnit>> {
        if let Self::Hardware(e) = self {
            return e.encode_texture(texture, subresource, pts_us, force_key, deadline);
        }
        let nv12 = super::encode_d3d::read_nv12(gpu, texture, subresource, &self.settings())?;
        self.encode(&nv12, pts_us, force_key, deadline)
    }
    /// True when textures reach the encoder without a CPU copy.
    pub fn texture_input(&self) -> bool {
        matches!(self, Self::Hardware(e) if e.texture_input())
    }
    pub fn finish(&mut self, deadline: Instant) -> Result<Vec<EncodedUnit>> {
        match self {
            Self::Hardware(e) => e.finish(deadline),
            Self::Software(e) => e.finish(),
            Self::SoftwareVp9(e) => e.finish(),
        }
    }
    /// Effective-rate change without a restart (Android's rate adjuster).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        match self {
            Self::Hardware(e) => e.set_bitrate(kbps),
            Self::Software(e) => e.set_bitrate(kbps),
            Self::SoftwareVp9(e) => e.set_bitrate(kbps),
        }
    }
}
