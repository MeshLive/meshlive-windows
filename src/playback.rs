//! Exclusive, event-driven WASAPI output. Opening/starting is explicit and never
//! performed by UI discovery, module initialization or the unit tests.
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::*,
        Media::Audio::*,
        System::{Com::*, Threading::*},
    },
    core::PCWSTR,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Closed,
    Buffer,
    Capacity,
    Cancelled,
    Timeout,
    Native(i32),
}
impl From<windows::core::Error> for Error {
    fn from(value: windows::core::Error) -> Self {
        Self::Native(value.code().0)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Audio output error: {self:?}")
    }
}
impl std::error::Error for Error {}
fn check(deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Error::Timeout)
    } else {
        Ok(())
    }
}

pub trait RenderDevice {
    fn available(&mut self) -> Result<usize, Error>;
    fn write(&mut self, samples: &[i16]) -> Result<(), Error>;
    fn wait(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error>;
}
/// No extra PCM queue or allocation. The caller retains its bounded unit until
/// the whole write succeeds; any partial error drops/stops the device, no retry.
pub struct Playback<D: RenderDevice> {
    device: Option<D>,
}
impl<D: RenderDevice> Playback<D> {
    /// The open device (None after a failed write closed it).
    pub fn device(&self) -> Option<&D> {
        self.device.as_ref()
    }
    pub fn new(device: D) -> Self {
        Self {
            device: Some(device),
        }
    }
    pub fn write(
        &mut self,
        samples: &[i16],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if self.device.is_none() {
            return Err(Error::Closed);
        }
        let result = self.write_inner(samples, deadline, cancel);
        if result.is_err() {
            self.device = None;
        }
        result
    }
    fn write_inner(
        &mut self,
        mut samples: &[i16],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if samples.len() > crate::opus::Decoder::MAX_SAMPLES {
            return Err(Error::Buffer);
        }
        let device = self.device.as_mut().ok_or(Error::Closed)?;
        while !samples.is_empty() {
            check(deadline, cancel)?;
            let count = device.available()?.min(samples.len());
            if count == 0 {
                device.wait(deadline, cancel)?;
                continue;
            }
            device.write(&samples[..count])?;
            samples = &samples[count..];
        }
        check(deadline, cancel)
    }
}

struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn new() -> Result<Self, Error> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()?;
        }
        Ok(Self(PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 1,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 96_000,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}
/// Experimental, off by default: `MESHLIVE_AUDIOCLIENT3=1` opens call output
/// with IAudioClient3 at the engine's minimum shared period (low-latency
/// shared mode, the device mix format converted here). Any failure falls
/// back to the standard path and says why in `describe`.
pub const LOW_LATENCY_SWITCH: &str = "MESHLIVE_AUDIOCLIENT3";
pub fn low_latency_requested() -> bool {
    std::env::var(LOW_LATENCY_SWITCH).ok().as_deref() == Some("1")
}

/// The device mix format the low-latency path writes (mono is copied to every channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mix {
    channels: usize,
    float: bool,
}
impl Mix {
    fn frame_bytes(self) -> usize {
        self.channels * if self.float { 4 } else { 2 }
    }
    /// Mono 16-bit PCM in this layout.
    fn convert(self, samples: &[i16], out: &mut Vec<u8>) {
        out.clear();
        for &s in samples {
            for _ in 0..self.channels {
                if self.float {
                    out.extend_from_slice(&(f32::from(s) / 32768.0).to_le_bytes());
                } else {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

/// Create on its dedicated output worker, not the Bluetooth MTA or UI thread.
/// COM initialization is owned here; an incompatible apartment is an error.
pub struct Wasapi {
    // Interfaces must release before the event handle and COM apartment.
    render: IAudioRenderClient,
    client: IAudioClient,
    event: Event,
    _apartment: Apartment,
    frames: u32,
    started: bool,
    /// Some: the low-latency path in the device mix format.
    mix: Option<Mix>,
    converted: Vec<u8>,
    period_frames: u32,
    fallback: Option<String>,
    /// Stream category and ducking opt-out (calls: communications).
    category: String,
    ducking: String,
    /// The endpoint, and whether it is also the console default render
    /// endpoint (the one a system echo canceller most likely uses as its
    /// reference; a mismatch can leave echo uncancelled).
    endpoint: String,
    endpoint_id: String,
    /// Why a call's output went to this endpoint (`speaker_open reason=`).
    reason: Option<&'static str>,
}
impl Wasapi {
    /// What was opened: path, engine period, buffer and stream latency.
    pub fn describe(&self) -> String {
        let latency_ms = unsafe { self.client.GetStreamLatency() }.map_or(-1.0, |l| l as f64 / 10_000.0);
        format!(
            "mode={} period_frames={} buffer_frames={} stream_latency_ms={latency_ms:.2} mix={:?} category={} ducking_optout={} {}{}{}",
            if self.mix.is_some() { "audioclient3" } else { "standard" },
            self.period_frames,
            self.frames,
            self.mix,
            self.category,
            self.ducking,
            self.endpoint,
            self.reason.map_or(String::new(), |r| format!(" reason={r}")),
            self.fallback.as_deref().map_or(String::new(), |f| format!(" fallback={f}"))
        )
    }
    /// The opened endpoint's id.
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }
    /// Frames queued in the engine buffer now.
    pub fn queued_frames(&self) -> Result<u32, Error> {
        Ok(unsafe { self.client.GetCurrentPadding()? })
    }
    /// Opens one path in the default category (`open_with(low_latency, false)`).
    pub fn open(low_latency: bool) -> Result<Self, Error> {
        Self::open_with(low_latency, false)
    }
    /// Opens one path: `low_latency` uses IAudioClient3 (no fallback here).
    /// `call`: a voice call's output (AudioCategory_Communications, so the
    /// endpoint's echo canceller gets this stream as its reference, and no
    /// ducking of other apps); otherwise the default category.
    pub fn open_with(low_latency: bool, call: bool) -> Result<Self, Error> {
        Self::open_endpoint(low_latency, call, None)
    }
    /// As `open_with`, on endpoint `id` (None: the communications default).
    pub fn open_endpoint(low_latency: bool, call: bool, id: Option<&str>) -> Result<Self, Error> {
        let apartment = Apartment::new()?;
        let event = Event(unsafe { CreateEventW(None, false, false, PCWSTR::null())? });
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)? };
        let endpoint = match id {
            Some(id) => unsafe { enumerator.GetDevice(&windows::core::HSTRING::from(id))? },
            None => unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eCommunications)? },
        };
        let endpoint_id = crate::capture::endpoint_id(&endpoint);
        let mut category = "default".to_owned();
        let console = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok().map(|d| crate::capture::endpoint_id(&d));
        let endpoint_text = format!(
            "endpoint=\"{}\" console_default={}",
            crate::capture::friendly_name(&endpoint),
            console.as_deref().map_or("unknown".to_owned(), |c| (c == crate::capture::endpoint_id(&endpoint)).to_string())
        );
        let (client, mix, period_frames) = if low_latency {
            let client3: IAudioClient3 = unsafe { endpoint.Activate(CLSCTX_INPROC_SERVER, None)? };
            if call {
                category = crate::capture::set_communications(&windows::core::Interface::cast(&client3)?);
            }
            let format = unsafe { client3.GetMixFormat()? };
            let opened = (|| -> Result<(Mix, u32), Error> {
                // SAFETY: the engine returned a valid WAVEFORMATEX(-EXTENSIBLE).
                let (tag, channels, rate, bits) = unsafe {
                    let f = std::ptr::read_unaligned(format);
                    (f.wFormatTag, f.nChannels, f.nSamplesPerSec, f.wBitsPerSample)
                };
                let sub = if tag == 0xFFFE {
                    // WAVEFORMATEXTENSIBLE SubFormat Data1 (offset 24): 1 PCM, 3 IEEE float.
                    unsafe { std::ptr::read_unaligned(format.cast::<u8>().add(24).cast::<u32>()) as u16 }
                } else {
                    tag
                };
                let mix = match (sub, bits) {
                    (3, 32) => Mix { channels: channels as usize, float: true },
                    (1, 16) => Mix { channels: channels as usize, float: false },
                    _ => return Err(Error::Native(AUDCLNT_E_UNSUPPORTED_FORMAT.0)),
                };
                if rate != 48_000 || channels == 0 {
                    return Err(Error::Native(AUDCLNT_E_UNSUPPORTED_FORMAT.0));
                }
                let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
                unsafe {
                    client3.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)?;
                    client3.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, format, None)?;
                }
                Ok((mix, min))
            })();
            unsafe { CoTaskMemFree(Some(format.cast())) };
            let (mix, period) = opened?;
            let client: IAudioClient = windows::core::Interface::cast(&client3)?;
            (client, Some(mix), period)
        } else {
            let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_INPROC_SERVER, None)? };
            if call {
                category = crate::capture::set_communications(&client);
            }
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | AUDCLNT_STREAMFLAGS_NOPERSIST;
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 400_000, 0, &format(), None)? };
            let mut period = 0i64;
            unsafe { client.GetDevicePeriod(Some(&mut period), None)? };
            (client, None, (period * 48 / 10_000) as u32)
        };
        unsafe { client.SetEventHandle(event.0)? };
        let ducking = if call { crate::capture::opt_out_of_ducking(&client) } else { "n/a".into() };
        let frames = unsafe { client.GetBufferSize()? };
        if frames == 0 {
            return Err(Error::Capacity);
        }
        let render = unsafe { client.GetService()? };
        let converted = Vec::with_capacity(mix.map_or(0, |m| m.frame_bytes() * frames as usize));
        Ok(Self {
            render,
            client,
            event,
            _apartment: apartment,
            frames,
            started: false,
            mix,
            converted,
            period_frames,
            fallback: None,
            category,
            ducking,
            endpoint: endpoint_text,
            endpoint_id,
            reason: None,
        })
    }
    /// Shared mode only. Native format conversion maps mono PCM to the device's
    /// mix format; it does not alter the default device, volume or OS settings.
    /// Actual output starts on the first nonempty write, after priming its buffer.
    pub fn open_default() -> Result<Playback<Self>, Error> {
        Self::open_switched(false, None)
    }
    /// A voice call's output (communications category) on `target` (an
    /// endpoint id and why), else the communications default. If the target
    /// cannot be opened the communications default is used. Windows'
    /// defaults are never changed.
    pub fn open_call(target: Option<(&str, &'static str)>) -> Result<Playback<Self>, Error> {
        if let Some((id, reason)) = target {
            if let Ok(mut playback) = Self::open_switched(true, Some(id)) {
                if let Some(device) = playback.device.as_mut() {
                    device.reason = Some(reason);
                }
                return Ok(playback);
            }
        }
        let mut playback = Self::open_switched(true, None)?;
        if let Some(device) = playback.device.as_mut() {
            device.reason = Some("comms_default");
        }
        Ok(playback)
    }
    fn open_switched(call: bool, id: Option<&str>) -> Result<Playback<Self>, Error> {
        if low_latency_requested() {
            match Self::open_endpoint(true, call, id) {
                Ok(device) => return Ok(Playback::new(device)),
                Err(error) => {
                    let mut device = Self::open_endpoint(false, call, id)?;
                    device.fallback = Some(format!("{error:?}"));
                    return Ok(Playback::new(device));
                }
            }
        }
        Self::open_endpoint(false, call, id).map(Playback::new)
    }
}
impl RenderDevice for Wasapi {
    fn available(&mut self) -> Result<usize, Error> {
        let padding = unsafe { self.client.GetCurrentPadding()? };
        Ok(self.frames.checked_sub(padding).ok_or(Error::Capacity)? as usize)
    }
    fn write(&mut self, samples: &[i16]) -> Result<(), Error> {
        if samples.is_empty() {
            return Ok(());
        }
        if samples.len() > crate::opus::Decoder::MAX_SAMPLES || samples.len() > self.available()? {
            return Err(Error::Capacity);
        }
        let count = samples.len() as u32;
        // The low-latency path converts before the engine buffer is acquired.
        let (bytes, len) = match self.mix {
            Some(mix) => {
                mix.convert(samples, &mut self.converted);
                (self.converted.as_ptr(), self.converted.len())
            }
            None => (samples.as_ptr().cast::<u8>(), samples.len() * 2),
        };
        let output = unsafe { self.render.GetBuffer(count)? };
        if output.is_null() {
            unsafe {
                let _ = self
                    .render
                    .ReleaseBuffer(count, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
            }
            return Err(Error::Native(E_POINTER.0));
        }
        // One PCM copy into the engine-owned buffer. No Rust allocation or
        // fallible operation is allowed between acquiring and releasing it.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes, output, len);
            self.render.ReleaseBuffer(count, 0)?;
        }
        if !self.started {
            unsafe {
                self.client.Start()?;
            }
            self.started = true;
        }
        Ok(())
    }
    fn wait(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
        check(deadline, cancel)?;
        let millis = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 50) as u32;
        match unsafe { WaitForSingleObject(self.event.0, millis) } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => check(deadline, cancel),
            WAIT_FAILED => Err(windows::core::Error::from_thread().into()),
            _ => Err(Error::Capacity),
        }
    }
}
impl Drop for Wasapi {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc, time::Duration};
    #[derive(Default)]
    struct State {
        writes: Vec<Vec<i16>>,
        waits: usize,
        drops: usize,
    }
    struct Fake {
        state: Rc<RefCell<State>>,
        available: usize,
        fail_after: Option<usize>,
        cancel_wait: bool,
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            self.state.borrow_mut().drops += 1;
        }
    }
    impl RenderDevice for Fake {
        fn available(&mut self) -> Result<usize, Error> {
            Ok(self.available)
        }
        fn write(&mut self, samples: &[i16]) -> Result<(), Error> {
            let mut state = self.state.borrow_mut();
            if self.fail_after == Some(state.writes.len()) {
                return Err(Error::Native(-1));
            }
            state.writes.push(samples.to_vec());
            self.available -= samples.len();
            Ok(())
        }
        fn wait(&mut self, _: Instant, cancel: &AtomicBool) -> Result<(), Error> {
            self.state.borrow_mut().waits += 1;
            self.available = 3;
            if self.cancel_wait {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }
    #[test]
    fn the_low_latency_path_copies_mono_into_every_mix_channel() {
        let mut out = Vec::new();
        Mix { channels: 2, float: true }.convert(&[16_384, -32_768], &mut out);
        let f: Vec<f32> = out.chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        assert_eq!(f, vec![0.5, 0.5, -1.0, -1.0]);
        Mix { channels: 2, float: false }.convert(&[7, -1], &mut out);
        assert_eq!(out, vec![7, 0, 7, 0, 0xff, 0xff, 0xff, 0xff]);
        assert!(!low_latency_requested() || std::env::var(LOW_LATENCY_SWITCH).is_ok());
    }
    fn fake(fail_after: Option<usize>) -> (Playback<Fake>, Rc<RefCell<State>>) {
        let state = Rc::new(RefCell::new(State::default()));
        (
            Playback::new(Fake {
                state: state.clone(),
                available: 3,
                fail_after,
                cancel_wait: false,
            }),
            state,
        )
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }
    #[test]
    fn pcm_order_buffer_waits_and_single_owner_cleanup() {
        let (mut playback, state) = fake(None);
        let cancel = AtomicBool::new(false);
        playback
            .write(&[1, 2, 3, 4, 5, 6, 7], deadline(), &cancel)
            .unwrap();
        assert_eq!(
            state.borrow().writes,
            [vec![1, 2, 3], vec![4, 5, 6], vec![7]]
        );
        assert_eq!(state.borrow().waits, 2);
        drop(playback);
        assert_eq!(state.borrow().drops, 1);
    }
    #[test]
    fn partial_failure_cancellation_deadline_and_oversize_close_without_retry() {
        let (mut playback, state) = fake(Some(1));
        let cancel = AtomicBool::new(false);
        assert_eq!(
            playback.write(&[1, 2, 3, 4], deadline(), &cancel),
            Err(Error::Native(-1))
        );
        assert_eq!(state.borrow().writes, [vec![1, 2, 3]]);
        assert_eq!(state.borrow().drops, 1);
        assert_eq!(
            playback.write(&[4], deadline(), &cancel),
            Err(Error::Closed)
        );
        let (mut playback, state) = fake(None);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            playback.write(&[1], deadline(), &cancel),
            Err(Error::Cancelled)
        );
        assert!(state.borrow().writes.is_empty());
        assert_eq!(state.borrow().drops, 1);
        let (mut playback, state) = fake(None);
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            playback.write(&[1], Instant::now(), &cancel),
            Err(Error::Timeout)
        );
        assert!(state.borrow().writes.is_empty());
        let (mut playback, state) = fake(None);
        assert_eq!(
            playback.write(&[0; 5761], deadline(), &cancel),
            Err(Error::Buffer)
        );
        assert!(state.borrow().writes.is_empty());
    }
    #[test]
    fn native_format_is_mono_pcm_without_opening_audio_device() {
        let f = format();
        assert_eq!(
            (
                f.wFormatTag,
                f.nChannels,
                f.nSamplesPerSec,
                f.nAvgBytesPerSec,
                f.nBlockAlign,
                f.wBitsPerSample,
                f.cbSize
            ),
            (1, 1, 48_000, 96_000, 2, 16, 0)
        );
    }

    #[test]
    fn cancellation_while_buffer_is_full_prevents_the_next_pcm_copy() {
        let (mut playback, state) = fake(None);
        playback.device.as_mut().unwrap().cancel_wait = true;
        let cancel = AtomicBool::new(false);
        assert_eq!(
            playback.write(&[1, 2, 3, 4], deadline(), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(state.borrow().writes, [vec![1, 2, 3]]);
        assert_eq!(state.borrow().drops, 1);
        assert_eq!(
            playback.write(&[4], deadline(), &cancel),
            Err(Error::Closed)
        );
    }
}
