//! Synchronous D3D11-aware MFTs only. Access units must already be admitted and
//! authenticated; this worker does not replace source/record/session policy.
use super::bitstream::SizeGuard;
use super::*;
use crate::media::{MAX_PAYLOAD, VideoHeader};
use std::mem::ManuallyDrop;
use windows::{
    Win32::{
        Foundation::{E_FAIL, E_NOTIMPL},
        System::Variant::VARIANT,
    },
    core::{Error as NativeError, GUID},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Closed,
    Header,
    Input,
    Unsupported,
    Format,
    Bitstream(BitstreamError),
    Native(i32),
}
impl From<NativeError> for DecodeError {
    fn from(value: NativeError) -> Self {
        Self::Native(value.code().0)
    }
}
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Video decode error: {self:?}")
    }
}
impl std::error::Error for DecodeError {}
type DecodeResult<T> = std::result::Result<T, DecodeError>;

/// HRESULT 0 means requested and read back successfully. Failure/unsupported
/// controls stay visible; neither these statuses nor DXGI output prove latency
/// or hardware acceleration. Output size checks remain mandatory regardless.
#[derive(Debug, Clone, Copy)]
pub struct CodecControls {
    pub low_latency_codec: i32,
    pub low_latency_attribute: i32,
    pub maximum_width: i32,
    pub maximum_height: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct OutputFormat {
    pub crop: Crop,
    pub colour: Option<Colour>,
    pub range: Option<Range>,
    pub chroma: Option<ChromaLocation>,
}
/// Borrowing the decoder prevents input/flush/reconfiguration while this frame
/// is being presented. Keep its MF sample alive while using any native buffers.
pub struct DecodedFrame<'a> {
    sample: IMFSample,
    owner: &'a DecodeDevice,
    pub format: OutputFormat,
    pub timestamp_100ns: i64,
}
impl DecodedFrame<'_> {
    pub fn nv12(&self) -> Result<Nv12Frame<'_>> {
        self.owner.nv12_frame(self.sample.clone(), self.format.crop)
    }
    /// Native video processing/diagnostics must retain this sample, not just its
    /// texture, until all use finishes. The buffer can belong to a decoder pool.
    pub fn sample(&self) -> &IMFSample {
        &self.sample
    }
}
struct Transform {
    native: IMFTransform,
    activation: IMFActivate,
}
impl Drop for Transform {
    fn drop(&mut self) {
        unsafe {
            let _ = self.native.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .native
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activation.ShutdownObject();
        }
    }
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

/// Exclusive MTA worker. Drain pull() to None before pushing another unit.
/// No internal compressed/frame queue; native failure discards the transform.
/// Async MFTs and CPU-only output need separate implementations, not polling.
pub struct Decoder<'a> {
    inner: Option<Transform>,
    owner: &'a DecodeDevice,
    header: VideoHeader,
    input_id: u32,
    output_id: u32,
    accepting: bool,
    ending: bool,
    controls: CodecControls,
    sizes: SizeGuard,
}
impl<'a> Decoder<'a> {
    pub fn open(owner: &'a DecodeDevice, header: VideoHeader) -> DecodeResult<Self> {
        header.encode().map_err(|_| DecodeError::Header)?;
        let subtype = if header.codec == 2 {
            MFVideoFormat_VP90
        } else {
            MFVideoFormat_AV1
        };
        let mut last = DecodeError::Unsupported;
        // Hardware-category MFTs first. Async candidates are explicitly refused
        // here; a synchronous D3D11-aware MFT may still accelerate internally.
        for flags in [
            MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT,
        ] {
            let candidates = Activations::new(subtype, flags | MFT_ENUM_FLAG_SORTANDFILTER)?;
            for index in 0..candidates.count {
                let activation =
                    unsafe { (*candidates.pointer.add(index as usize)).take().unwrap() };
                match Self::activate(owner, header, subtype, activation) {
                    Ok(value) => return Ok(value),
                    Err(error) => last = error,
                }
            }
        }
        Err(last)
    }
    fn activate(
        owner: &'a DecodeDevice,
        header: VideoHeader,
        subtype: GUID,
        activation: IMFActivate,
    ) -> DecodeResult<Self> {
        let native: IMFTransform = unsafe { activation.ActivateObject()? };
        let transform = Transform { native, activation };
        let attributes = unsafe { transform.native.GetAttributes()? };
        if attribute(&attributes, MF_TRANSFORM_ASYNC)?.unwrap_or(0) != 0
            || attribute(&attributes, MF_SA_D3D11_AWARE)?.unwrap_or(0) == 0
        {
            return Err(DecodeError::Unsupported);
        }
        let controls = controls(&transform.native, &attributes, header);
        let (mut inputs, mut outputs) = (0, 0);
        let (mut input_id, mut output_id) = ([0], [0]);
        unsafe {
            transform.native.GetStreamCount(&mut inputs, &mut outputs)?;
            if inputs != 1 || outputs != 1 {
                return Err(DecodeError::Unsupported);
            }
            match transform.native.GetStreamIDs(&mut input_id, &mut output_id) {
                Ok(()) => (),
                Err(e) if e.code() == E_NOTIMPL => {
                    input_id[0] = 0;
                    output_id[0] = 0;
                }
                Err(e) => return Err(e.into()),
            }
            transform
                .native
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, owner.manager.as_raw() as usize)?;
            let input = input_type(header, subtype)?;
            transform.native.SetInputType(input_id[0], &input, 0)?;
        }
        let mut decoder = Self {
            inner: Some(transform),
            owner,
            header,
            input_id: input_id[0],
            output_id: output_id[0],
            accepting: true,
            ending: false,
            controls,
            sizes: SizeGuard::new(header),
        };
        decoder.negotiate()?;
        let native = &decoder.inner.as_ref().unwrap().native;
        unsafe {
            native.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            native.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(decoder)
    }
    pub fn controls(&self) -> CodecControls {
        self.controls
    }
    fn negotiate(&mut self) -> DecodeResult<()> {
        let native = &self.inner.as_ref().ok_or(DecodeError::Closed)?.native;
        for index in 0..64 {
            let media_type = match unsafe { native.GetOutputAvailableType(self.output_id, index) } {
                Ok(t) => t,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(e.into()),
            };
            if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE)? } != MFVideoFormat_NV12 {
                continue;
            }
            format(&media_type, self.header)?;
            unsafe {
                native.SetOutputType(self.output_id, &media_type, 0)?;
                let info = native.GetOutputStreamInfo(self.output_id)?;
                if info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                    == 0
                {
                    return Err(DecodeError::Unsupported);
                }
            }
            return Ok(());
        }
        Err(DecodeError::Unsupported)
    }
    /// False means the unit was NOT accepted: drain output, then retry the same
    /// unit. No retry is allowed after any error; the decoder closes on error.
    pub fn push(&mut self, payload: &[u8], pts_us: i64, key_frame: bool) -> DecodeResult<bool> {
        if self.inner.is_none() {
            return Err(DecodeError::Closed);
        }
        let result = self.push_inner(payload, pts_us, key_frame);
        if result.is_err() {
            self.inner = None;
        }
        result
    }
    fn push_inner(&mut self, payload: &[u8], pts_us: i64, key_frame: bool) -> DecodeResult<bool> {
        if self.ending {
            return Err(DecodeError::Input);
        }
        if !self.accepting {
            return Ok(false);
        }
        let sample = input_sample(payload, pts_us, key_frame, self.header.fps)?;
        // Preflight every picture before the opaque decoder sees its bytes.
        // Backpressure must not commit speculative reference/sequence state.
        let sizes = self
            .sizes
            .inspect(payload)
            .map_err(DecodeError::Bitstream)?;
        match unsafe {
            self.inner
                .as_ref()
                .unwrap()
                .native
                .ProcessInput(self.input_id, &sample, 0)
        } {
            Ok(()) => {
                self.sizes = sizes;
                self.accepting = false;
                Ok(true)
            }
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.accepting = false;
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
    pub fn pull(&mut self) -> DecodeResult<Option<DecodedFrame<'_>>> {
        let result = self.pull_inner();
        if result.is_err() {
            self.inner = None;
        }
        result.map(|output| {
            output.map(|(sample, format, timestamp_100ns)| DecodedFrame {
                sample,
                owner: self.owner,
                format,
                timestamp_100ns,
            })
        })
    }
    fn pull_inner(&mut self) -> DecodeResult<Option<(IMFSample, OutputFormat, i64)>> {
        // Bound repeated native format-change responses in a single operation.
        for _ in 0..4 {
            let native = &self.inner.as_ref().ok_or(DecodeError::Closed)?.native;
            let mut output = Output(MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                ..Default::default()
            });
            let mut status = 0;
            match unsafe {
                native.ProcessOutput(0, std::slice::from_mut(&mut output.0), &mut status)
            } {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    self.accepting = !self.ending;
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
                Ok(()) => (),
            }
            if status != 0 || output.0.dwStatus & !(MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32) != 0
            {
                return Err(DecodeError::Format);
            }
            let sample = output.0.pSample.take().ok_or(DecodeError::Format)?;
            let current = unsafe { native.GetOutputCurrentType(self.output_id)? };
            let format = format(&current, self.header)?;
            let timestamp = unsafe { sample.GetSampleTime()? };
            // Validate actual DXGI format/device/buffer/crop before handing off.
            let _ = self.owner.nv12_frame(sample.clone(), format.crop)?;
            return Ok(Some((sample, format, timestamp)));
        }
        Err(DecodeError::Format)
    }
    pub fn drain(&mut self) -> DecodeResult<()> {
        let native = &self.inner.as_ref().ok_or(DecodeError::Closed)?.native;
        let result = unsafe {
            native
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, self.input_id as usize)
                .and_then(|_| native.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
        };
        if let Err(e) = result {
            self.inner = None;
            return Err(e.into());
        }
        self.ending = true;
        self.accepting = false;
        Ok(())
    }
    /// Session owner must discard its old queued frames and supply a recovery
    /// keyframe/newest configuration. This does not interpret wire CSD records.
    pub fn flush(&mut self) -> DecodeResult<()> {
        let native = &self.inner.as_ref().ok_or(DecodeError::Closed)?.native;
        let result = unsafe {
            native
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .and_then(|_| native.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
        };
        if let Err(e) = result {
            self.inner = None;
            return Err(e.into());
        }
        self.ending = false;
        self.accepting = true;
        self.sizes = SizeGuard::new(self.header);
        Ok(())
    }
}

fn input_type(header: VideoHeader, subtype: GUID) -> DecodeResult<IMFMediaType> {
    header.encode().map_err(|_| DecodeError::Header)?;
    let value = unsafe { MFCreateMediaType()? };
    unsafe {
        value.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        value.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        value.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (header.width as u64) << 32 | header.height as u64,
        )?;
        value.SetUINT64(&MF_MT_FRAME_RATE, (header.fps as u64) << 32 | 1)?;
        value.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1u64 << 32 | 1)?;
        value.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(value)
}
fn input_sample(payload: &[u8], pts_us: i64, key: bool, fps: u8) -> DecodeResult<IMFSample> {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD || !(1..=120).contains(&fps) {
        return Err(DecodeError::Input);
    }
    let time = pts_us.checked_mul(10).ok_or(DecodeError::Input)?;
    let buffer = unsafe { MFCreateMemoryBuffer(payload.len() as u32)? };
    struct Lock<'a>(&'a IMFMediaBuffer);
    impl Drop for Lock<'_> {
        fn drop(&mut self) {
            unsafe {
                let _ = self.0.Unlock();
            }
        }
    }
    let mut destination = std::ptr::null_mut();
    let mut maximum = 0;
    unsafe {
        buffer.Lock(&mut destination, Some(&mut maximum), None)?;
        let lock = Lock(&buffer);
        if (maximum as usize) < payload.len() {
            return Err(DecodeError::Input);
        }
        std::ptr::copy_nonoverlapping(payload.as_ptr(), destination, payload.len());
        drop(lock);
        buffer.SetCurrentLength(payload.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(10_000_000 / fps as i64)?;
        sample.SetUINT32(&MFSampleExtension_CleanPoint, key as u32)?;
        Ok(sample)
    }
}
fn attribute(value: &IMFAttributes, key: GUID) -> DecodeResult<Option<u32>> {
    match unsafe { value.GetUINT32(&key) } {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn format(value: &IMFMediaType, header: VideoHeader) -> DecodeResult<OutputFormat> {
    if unsafe { value.GetGUID(&MF_MT_SUBTYPE)? } != MFVideoFormat_NV12 {
        return Err(DecodeError::Unsupported);
    }
    let size = unsafe { value.GetUINT64(&MF_MT_FRAME_SIZE)? };
    let (width, height) = ((size >> 32) as u32, size as u32);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(DecodeError::Format);
    }
    let mut crop = Crop {
        left: 0,
        top: 0,
        width,
        height,
    };
    let mut blob = [0; std::mem::size_of::<MFVideoArea>()];
    match unsafe { value.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, None) } {
        Ok(()) => {
            if unsafe { value.GetBlobSize(&MF_MT_MINIMUM_DISPLAY_APERTURE)? } != blob.len() as u32 {
                return Err(DecodeError::Format);
            }
            let area: MFVideoArea = unsafe { std::ptr::read_unaligned(blob.as_ptr().cast()) };
            if area.OffsetX.fract != 0
                || area.OffsetY.fract != 0
                || area.OffsetX.value < 0
                || area.OffsetY.value < 0
                || area.Area.cx <= 0
                || area.Area.cy <= 0
            {
                return Err(DecodeError::Unsupported);
            }
            crop = Crop {
                left: area.OffsetX.value as u32,
                top: area.OffsetY.value as u32,
                width: area.Area.cx as u32,
                height: area.Area.cy as u32,
            };
        }
        Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => (),
        Err(e) => return Err(e.into()),
    }
    if crop.width > header.width as u32
        || crop.height > header.height as u32
        || crop.left.checked_add(crop.width).is_none_or(|x| x > width)
        || crop.top.checked_add(crop.height).is_none_or(|y| y > height)
    {
        return Err(DecodeError::Format);
    }
    // Missing/unspecified metadata stays unknown. Explicit unsupported tags
    // refuse instead of being downgraded to "unknown" and guessed by a caller.
    match attribute(value, MF_MT_TRANSFER_FUNCTION)? {
        None | Some(0 | 4 | 5 | 7) => (),
        _ => return Err(DecodeError::Unsupported),
    }
    let colour = match attribute(value, MF_MT_YUV_MATRIX)? {
        Some(1) => Some(Colour::Bt709),
        Some(2) => Some(Colour::Bt601),
        None | Some(0) => None,
        _ => return Err(DecodeError::Unsupported),
    };
    let range = match attribute(value, MF_MT_VIDEO_NOMINAL_RANGE)? {
        Some(1) => Some(Range::Full),
        Some(2) => Some(Range::Limited),
        None | Some(0) => None,
        _ => return Err(DecodeError::Unsupported),
    };
    let chroma = match attribute(value, MF_MT_VIDEO_CHROMA_SITING)?
        .map(|v| v & !MFVideoChromaSubsampling_ProgressiveChroma.0 as u32)
    {
        Some(1) => Some(ChromaLocation::Center),
        Some(5) => Some(ChromaLocation::Left),
        Some(7) => Some(ChromaLocation::TopLeft),
        None | Some(0) => None,
        _ => return Err(DecodeError::Unsupported),
    };
    Ok(OutputFormat {
        crop,
        colour,
        range,
        chroma,
    })
}

fn controls(
    native: &IMFTransform,
    attributes: &IMFAttributes,
    header: VideoHeader,
) -> CodecControls {
    // This is the documented direct-MFT control even without ICodecAPI. A store
    // readback alone does not prove the transform honors it; report behavior.
    let attribute_status = unsafe {
        attributes
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .and_then(|_| attributes.GetUINT32(&MF_LOW_LATENCY))
            .and_then(|value| {
                if value == 1 {
                    Ok(())
                } else {
                    Err(NativeError::from_hresult(E_FAIL))
                }
            })
    }
    .err()
    .map_or(0, |e| e.code().0);
    let api = native.cast::<ICodecAPI>();
    let (low_latency, maximum_width, maximum_height) = match api {
        Err(e) => (e.code().0, e.code().0, e.code().0),
        Ok(api) => {
            let request_bool = || -> Result<()> {
                unsafe {
                    api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))?;
                    if !bool::try_from(&api.GetValue(&CODECAPI_AVLowLatencyMode)?)? {
                        return Err(NativeError::from_hresult(E_FAIL));
                    }
                }
                Ok(())
            };
            let request_size = |key: &GUID, size: i32| -> Result<()> {
                unsafe {
                    api.SetValue(key, &VARIANT::from(size))?;
                    if i32::try_from(&api.GetValue(key)?)? != size {
                        return Err(NativeError::from_hresult(E_FAIL));
                    }
                }
                Ok(())
            };
            let status = |value: Result<()>| value.err().map_or(0, |e| e.code().0);
            (
                status(request_bool()),
                status(request_size(
                    &CODECAPI_AVDecVideoMaxCodedWidth,
                    header.width as i32,
                )),
                status(request_size(
                    &CODECAPI_AVDecVideoMaxCodedHeight,
                    header.height as i32,
                )),
            )
        }
    };
    CodecControls {
        low_latency_codec: low_latency,
        low_latency_attribute: attribute_status,
        maximum_width,
        maximum_height,
    }
}

#[cfg(test)]
mod tests;
