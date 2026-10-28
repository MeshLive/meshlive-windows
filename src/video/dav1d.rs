//! Design AD, Windows part (design decision): the pinned dav1d 1.5.3
//! software AV1 decoder, used only when no Media Foundation AV1 decoder
//! starts. It runs inside `MfFrames`, so every operation of a session (send,
//! drain, flush, close, reconfigure) happens in order on the one thread that
//! owns that `MfFrames` (the session's decode thread): that is the serial pump.
//!
//! Bounds (all fail closed or push back, never grow):
//! - pending input: at most 4 temporal units and 1 MiB. A full queue answers
//!   "not accepted" like an MFT, so the receive queue upstream overflows,
//!   drops to the next keyframe and asks the sender for one (CF-MEDIA-44);
//! - at most 64 OBUs per temporal unit;
//! - session envelope: sequence profile 0, 8-bit, 4:2:0, the sequence
//!   header's maximum frame size inside the BTV3 header per axis, and
//!   `frame_size_limit` = the header's pixel count, at most 1920x1088;
//! - output: a pool of 4 NV12 textures. A picture finding no free texture is
//!   dropped and counted.
//!
//! AV1 decode is advertised (decode mask bit 1) only after the startup check
//! (`readiness`): the embedded lossless fixture decodes hash-exact and the
//! embedded 854x480/30 or 640x360/15 benchmark decodes at least 1.5 times
//! real time. `MESHLIVE_DAV1D=0` switches the fallback off.
use super::bitstream::obu;
use super::*;
use crate::media::VideoHeader;
use std::{
    collections::VecDeque,
    ffi::{CStr, c_char, c_int},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use std::result::Result;
use windows::{Win32::Graphics::Dxgi::Common::*, core::IUnknown};

pub const SWITCH: &str = "MESHLIVE_DAV1D";
pub const MAX_PENDING_UNITS: usize = 4;
pub const MAX_PENDING_BYTES: usize = 1 << 20;
pub const MAX_OBUS_PER_UNIT: usize = 64;
/// 1920x1088: the largest session a software decode is allowed.
pub const FRAME_SIZE_LIMIT: u32 = 2_088_960;
pub const THREADS: u32 = 2;
pub const FRAME_DELAY: u32 = 1;
pub const OUTPUT_POOL: usize = 4;
/// The benchmark must decode at least this many times faster than real time.
pub const REAL_TIME_MARGIN: f64 = 1.5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sequence {
    pub profile: u32,
    pub max_width: u32,
    pub max_height: u32,
    /// 0 I400, 1 I420, 2 I422, 3 I444.
    pub layout: u32,
    pub high_bitdepth: u32,
    /// 0 limited, 1 full.
    pub color_range: u32,
    pub matrix: u32,
    /// 0 unknown, 1 vertical (left), 2 colocated (top-left).
    pub chroma: u32,
}
#[repr(C)]
struct RawPicture {
    planes: [*const u8; 3],
    stride_y: isize,
    stride_uv: isize,
    width: u32,
    height: u32,
    layout: u32,
    bpc: u32,
    pts: i64,
    sequence: Sequence,
}
#[repr(C)]
struct NativeContext {
    _opaque: [u8; 0],
}
// Linked by build.rs (static, unbundled, like libvpx).
unsafe extern "C" {
    fn meshlive_dav1d_version() -> *const c_char;
    fn meshlive_dav1d_parse_sequence(data: *const u8, size: usize, out: *mut Sequence) -> c_int;
    fn meshlive_dav1d_open(threads: u32, frame_delay: u32, frame_size_limit: u32) -> *mut NativeContext;
    fn meshlive_dav1d_close(native: *mut NativeContext);
    fn meshlive_dav1d_flush(native: *mut NativeContext);
    fn meshlive_dav1d_send(native: *mut NativeContext, data: *const u8, size: usize, pts: i64) -> c_int;
    fn meshlive_dav1d_resend(native: *mut NativeContext) -> c_int;
    fn meshlive_dav1d_get(native: *mut NativeContext, out: *mut RawPicture) -> c_int;
}

/// dav1d's version string, e.g. "1.5.3".
pub fn version() -> String {
    unsafe { CStr::from_ptr(meshlive_dav1d_version()) }.to_string_lossy().into_owned()
}
/// Default on; only an explicit "0" turns the fallback off.
pub fn switched_on() -> bool {
    std::env::var(SWITCH).ok().as_deref() != Some("0")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dav1dError {
    /// Outside the session envelope (profile, depth, layout or size).
    Envelope,
    /// More than 64 OBUs, more than 1 MiB, or not a valid OBU sequence.
    Unit,
    Native(i32),
}
impl From<Dav1dError> for DecodeError {
    fn from(value: Dav1dError) -> Self {
        match value {
            Dav1dError::Envelope => DecodeError::Unsupported,
            Dav1dError::Unit => DecodeError::Input,
            Dav1dError::Native(code) => DecodeError::Native(code),
        }
    }
}

/// dav1d's own sequence header parser (not a MeshLive scanner). None: these
/// bytes carry no sequence header.
pub fn parse_sequence(data: &[u8]) -> Result<Option<Sequence>, Dav1dError> {
    if data.is_empty() {
        return Ok(None);
    }
    let mut out = Sequence::default();
    match unsafe { meshlive_dav1d_parse_sequence(data.as_ptr(), data.len(), &mut out) } {
        0 => Ok(Some(out)),
        1 => Ok(None),
        code => Err(Dav1dError::Native(code)),
    }
}

/// What a session may decode, from its BTV3 header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    pub width: u32,
    pub height: u32,
    pub pixels: u32,
}
impl Envelope {
    pub fn for_header(header: VideoHeader) -> Result<Self, Dav1dError> {
        let (width, height) = (u32::from(header.width), u32::from(header.height));
        let pixels = width * height;
        if header.codec != 3 || width == 0 || height == 0 || pixels > FRAME_SIZE_LIMIT {
            return Err(Dav1dError::Envelope);
        }
        Ok(Self { width, height, pixels })
    }
    /// Profile 0 (Main), 8-bit, 4:2:0, and a maximum frame size that fits
    /// the header per axis.
    pub fn admits(self, sequence: &Sequence) -> Result<(), Dav1dError> {
        if sequence.profile != 0
            || sequence.high_bitdepth != 0
            || sequence.layout != 1
            || sequence.max_width == 0
            || sequence.max_height == 0
            || sequence.max_width > self.width
            || sequence.max_height > self.height
        {
            return Err(Dav1dError::Envelope);
        }
        Ok(())
    }
}

/// The OBUs of one temporal unit: their count (at most 64) and whether one
/// is a sequence header. One linear pass, no copies.
pub fn scan_unit(unit: &[u8]) -> Result<(usize, bool), Dav1dError> {
    if unit.is_empty() || unit.len() > MAX_PENDING_BYTES {
        return Err(Dav1dError::Unit);
    }
    let (mut input, mut count, mut sequence) = (unit, 0, false);
    while !input.is_empty() {
        let (kind, _) = obu(&mut input).map_err(|_| Dav1dError::Unit)?;
        count += 1;
        if count > MAX_OBUS_PER_UNIT {
            return Err(Dav1dError::Unit);
        }
        sequence |= kind == 1;
    }
    Ok((count, sequence))
}

/// Pending input, bounded by units and bytes. Every entry carries the
/// generation it was queued in; a flush or reconfigure bumps the generation
/// and older work is discarded, never decoded.
#[derive(Default)]
pub struct InputQueue {
    units: VecDeque<(u64, Vec<u8>, i64)>,
    bytes: usize,
    generation: u64,
}
impl InputQueue {
    /// False: full (the caller pushes back); the unit was not taken.
    pub fn offer(&mut self, unit: &[u8], pts: i64) -> bool {
        if self.units.len() >= MAX_PENDING_UNITS || self.bytes + unit.len() > MAX_PENDING_BYTES {
            return false;
        }
        self.bytes += unit.len();
        self.units.push_back((self.generation, unit.to_vec(), pts));
        true
    }
    /// The oldest unit of the current generation (older ones are dropped).
    pub fn take(&mut self) -> Option<(Vec<u8>, i64)> {
        while let Some((generation, unit, pts)) = self.units.pop_front() {
            self.bytes -= unit.len();
            if generation == self.generation {
                return Some((unit, pts));
            }
        }
        None
    }
    pub fn discard(&mut self) -> usize {
        self.generation += 1;
        let dropped = self.units.len();
        self.units.clear();
        self.bytes = 0;
        dropped
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn len(&self) -> usize {
        self.units.len()
    }
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// One dav1d context, I420 pictures in system memory.
pub struct Native {
    context: *mut NativeContext,
    /// The context holds a unit it has not fully consumed (EAGAIN).
    holding: bool,
}
// Used only through `&mut self` on its owning thread.
unsafe impl Send for Native {}
impl Drop for Native {
    fn drop(&mut self) {
        unsafe { meshlive_dav1d_close(self.context) }
    }
}
/// A decoded picture, valid until the next call on its `Native`.
pub struct Picture<'a> {
    raw: RawPicture,
    _owner: std::marker::PhantomData<&'a mut Native>,
}
impl Picture<'_> {
    pub fn size(&self) -> (u32, u32) {
        (self.raw.width, self.raw.height)
    }
    pub fn pts(&self) -> i64 {
        self.raw.pts
    }
    pub fn sequence(&self) -> Sequence {
        self.raw.sequence
    }
    fn plane(&self, index: usize, row: u32, bytes: u32) -> &[u8] {
        let stride = if index == 0 { self.raw.stride_y } else { self.raw.stride_uv };
        // SAFETY: dav1d's 8-bit plane holds `height` (luma) or `height/2`
        // rounded up (4:2:0 chroma) rows of `stride` bytes, valid until the
        // next call on the owning context, which this borrow prevents.
        unsafe { std::slice::from_raw_parts(self.raw.planes[index].offset(stride * row as isize), bytes as usize) }
    }
    /// Tight I420 (Y, U, V).
    pub fn i420(&self) -> Vec<u8> {
        let (w, h) = self.size();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut out = Vec::with_capacity((w * h + 2 * cw * ch) as usize);
        for y in 0..h {
            out.extend_from_slice(self.plane(0, y, w));
        }
        for index in [1, 2] {
            for y in 0..ch {
                out.extend_from_slice(self.plane(index, y, cw));
            }
        }
        out
    }
    /// NV12 with row pitch `pitch` and `rows` luma rows (the texture's
    /// even height), into `out` (resized; padding is left as written).
    pub fn nv12(&self, pitch: usize, rows: usize, out: &mut Vec<u8>) {
        let (w, h) = self.size();
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        out.resize(pitch * rows * 3 / 2, 0);
        for y in 0..h as usize {
            out[y * pitch..y * pitch + w as usize].copy_from_slice(self.plane(0, y as u32, w));
        }
        let uv = pitch * rows;
        for y in 0..ch {
            let (u, v) = (self.plane(1, y as u32, cw as u32), self.plane(2, y as u32, cw as u32));
            let row = &mut out[uv + y * pitch..uv + y * pitch + 2 * cw];
            for x in 0..cw {
                row[2 * x] = u[x];
                row[2 * x + 1] = v[x];
            }
        }
    }
}
pub enum Sent {
    Consumed,
    /// dav1d still holds the rest: take a picture, then `resend`.
    Again,
}
impl Native {
    pub fn open(frame_size_limit: u32) -> Result<Self, Dav1dError> {
        let context = unsafe { meshlive_dav1d_open(THREADS, FRAME_DELAY, frame_size_limit.min(FRAME_SIZE_LIMIT)) };
        if context.is_null() {
            return Err(Dav1dError::Native(-1));
        }
        Ok(Self { context, holding: false })
    }
    fn sent(&mut self, status: c_int) -> Result<Sent, Dav1dError> {
        match status {
            0 => {
                self.holding = false;
                Ok(Sent::Consumed)
            }
            1 => {
                self.holding = true;
                Ok(Sent::Again)
            }
            code => {
                self.holding = false;
                Err(Dav1dError::Native(code))
            }
        }
    }
    pub fn send(&mut self, unit: &[u8], pts: i64) -> Result<Sent, Dav1dError> {
        if self.holding {
            return Ok(Sent::Again);
        }
        let status = unsafe { meshlive_dav1d_send(self.context, unit.as_ptr(), unit.len(), pts) };
        self.sent(status)
    }
    pub fn resend(&mut self) -> Result<Sent, Dav1dError> {
        let status = unsafe { meshlive_dav1d_resend(self.context) };
        self.sent(status)
    }
    pub fn holding(&self) -> bool {
        self.holding
    }
    pub fn picture(&mut self) -> Result<Option<Picture<'_>>, Dav1dError> {
        let mut raw = RawPicture {
            planes: [std::ptr::null(); 3],
            stride_y: 0,
            stride_uv: 0,
            width: 0,
            height: 0,
            layout: 0,
            bpc: 0,
            pts: 0,
            sequence: Sequence::default(),
        };
        match unsafe { meshlive_dav1d_get(self.context, &mut raw) } {
            0 => {
                if raw.bpc != 8 || raw.layout != 1 || raw.width == 0 || raw.height == 0 || raw.planes.iter().any(|p| p.is_null()) {
                    return Err(Dav1dError::Envelope);
                }
                Ok(Some(Picture { raw, _owner: std::marker::PhantomData }))
            }
            1 => Ok(None),
            code => Err(Dav1dError::Native(code)),
        }
    }
    pub fn flush(&mut self) {
        unsafe { meshlive_dav1d_flush(self.context) };
        self.holding = false;
    }
}

/// Decodes whole temporal units synchronously (startup check, benchmark,
/// tests): every picture is handed to `each`.
pub fn decode_all<'u>(
    native: &mut Native,
    units: impl IntoIterator<Item = (&'u [u8], i64)>,
    mut each: impl FnMut(&Picture<'_>),
) -> Result<usize, Dav1dError> {
    let mut count = 0;
    for (unit, pts) in units {
        let mut sent = native.send(unit, pts)?;
        loop {
            if let Some(picture) = native.picture()? {
                each(&picture);
                count += 1;
            }
            match sent {
                Sent::Consumed => break,
                Sent::Again => sent = native.resend()?,
            }
        }
    }
    // Drain what the context still holds (none with frame delay 1).
    while let Some(picture) = native.picture()? {
        each(&picture);
        count += 1;
    }
    Ok(count)
}

/// IVF (DKIF) frames: (payload, timestamp ticks). None when malformed.
pub fn ivf_frames(data: &[u8]) -> Option<(u16, u16, u32, u32, Vec<(&[u8], u64)>)> {
    if data.len() < 32 || &data[..4] != b"DKIF" || &data[8..12] != b"AV01" {
        return None;
    }
    let width = u16::from_le_bytes([data[12], data[13]]);
    let height = u16::from_le_bytes([data[14], data[15]]);
    let rate = u32::from_le_bytes(data[16..20].try_into().ok()?);
    let scale = u32::from_le_bytes(data[20..24].try_into().ok()?);
    let mut frames = Vec::new();
    let mut position = usize::from(u16::from_le_bytes([data[6], data[7]]));
    while position < data.len() {
        let size = u32::from_le_bytes(data.get(position..position + 4)?.try_into().ok()?) as usize;
        let ticks = u64::from_le_bytes(data.get(position + 4..position + 12)?.try_into().ok()?);
        position += 12;
        frames.push((data.get(position..position + size)?, ticks));
        position += size;
    }
    Some((width, height, rate, scale, frames))
}

// ------------------------------------------------------------ startup check

/// The lossless 64x64 AV1 fixture (tests/fixtures/video-lossless.json) and
/// the I420 SHA-256 of each source frame it must reproduce exactly.
const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/video-av1.ivf");
const FIXTURE_I420: [&str; 3] = [
    "cddb5d009a15d6123d1e54228075308ef05d78e8351d4a3a81a356ce82be53f1",
    "3618a9fa010330e11f1271be957c04db15b6ea376dac68a0d28803a2a5b967ca",
    "0edbd612869a83f8d61f8a441e4a4fb2f73ba7e788e91e0450db0eca88fb6f62",
];
/// One second each of MeshLive's own generated pattern, encoded by the
/// bundled libaom at phone settings (examples/dav1d_fixtures.rs).
pub const BENCHMARKS: [(&str, &[u8]); 2] = [
    ("854x480/30", include_bytes!("../../tests/fixtures/dav1d-bench-854x480.ivf")),
    ("640x360/15", include_bytes!("../../tests/fixtures/dav1d-bench-640x360.ivf")),
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// True when the embedded fixture decodes to exactly its source pixels.
pub fn fixture_decodes_exactly() -> bool {
    let Some((64, 64, _, _, frames)) = ivf_frames(FIXTURE) else { return false };
    let Ok(mut native) = Native::open(64 * 64) else { return false };
    let mut hashes = Vec::new();
    let decoded = decode_all(&mut native, frames.iter().map(|(f, t)| (*f, *t as i64)), |p| {
        hashes.push(crate::crypto::sha256(&p.i420()).map(|h| hex(&h)).unwrap_or_default());
    });
    decoded == Ok(3) && hashes.iter().zip(FIXTURE_I420).all(|(a, b)| a == b) && hashes.len() == 3
}

#[derive(Debug, Clone)]
pub struct Speed {
    pub label: &'static str,
    pub pixels: u32,
    /// Media seconds per wall-clock second.
    pub real_time: f64,
}
/// Decodes `ivf` twice (two seconds of media) and reports how many times
/// faster than real time that ran. None when it does not decode.
pub fn benchmark(label: &'static str, ivf: &[u8]) -> Option<Speed> {
    let (width, height, rate, scale, frames) = ivf_frames(ivf)?;
    if frames.is_empty() || rate == 0 || scale == 0 {
        return None;
    }
    let pixels = u32::from(width) * u32::from(height);
    let media = frames.len() as f64 * f64::from(scale) / f64::from(rate) * 2.0;
    let mut native = Native::open(pixels).ok()?;
    let start = Instant::now();
    for _ in 0..2 {
        let decoded = decode_all(&mut native, frames.iter().map(|(f, t)| (*f, *t as i64)), |_| {}).ok()?;
        if decoded != frames.len() {
            return None;
        }
        native.flush();
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-6);
    Some(Speed { label, pixels, real_time: media / elapsed })
}

#[derive(Debug, Clone)]
pub struct Readiness {
    pub switched_on: bool,
    pub fixture_exact: bool,
    pub speeds: Vec<Speed>,
    /// The largest benchmark size that ran fast enough (0: none).
    pub envelope_pixels: u32,
    pub check_ms: u64,
}
impl Readiness {
    pub fn ready(&self) -> bool {
        self.switched_on && self.fixture_exact && self.envelope_pixels > 0
    }
    /// The startup check from its measured parts (pure, for tests).
    pub fn from_parts(switched_on: bool, fixture_exact: bool, speeds: Vec<Speed>, check_ms: u64) -> Self {
        let envelope_pixels = speeds
            .iter()
            .filter(|s| s.real_time >= REAL_TIME_MARGIN)
            .map(|s| s.pixels)
            .max()
            .unwrap_or(0);
        Self { switched_on, fixture_exact, speeds, envelope_pixels, check_ms }
    }
    pub fn line(&self) -> String {
        let speeds: Vec<String> = self.speeds.iter().map(|s| format!("{}={:.1}x", s.label, s.real_time)).collect();
        format!(
            "decoder=dav1d version={} ready={} switch={} fixture_exact={} speed=[{}] envelope_pixels={} check_ms={}",
            version(),
            self.ready(),
            self.switched_on,
            self.fixture_exact,
            speeds.join(" "),
            self.envelope_pixels,
            self.check_ms
        )
    }
}
/// The startup check, run once per process (on first use, off the UI
/// thread: the decode-mask probe calls it only when Media Foundation has no
/// AV1 decoder).
static READY: OnceLock<Readiness> = OnceLock::new();
pub fn readiness() -> &'static Readiness {
    READY.get_or_init(|| {
        let start = Instant::now();
        if !switched_on() {
            return Readiness::from_parts(false, false, Vec::new(), 0);
        }
        let exact = fixture_decodes_exactly();
        let speeds = if exact { BENCHMARKS.iter().filter_map(|(l, d)| benchmark(l, d)).collect() } else { Vec::new() };
        Readiness::from_parts(true, exact, speeds, start.elapsed().as_millis() as u64)
    })
}
pub fn ready() -> bool {
    readiness().ready()
}

// ------------------------------------------------------- fallback selection

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    MediaFoundation,
    Dav1d,
    None,
}
/// Which decoder a session uses: Media Foundation whenever its decoder
/// starts; dav1d only for AV1, only when MF did not start and the startup
/// check passed.
pub fn choose(codec: u8, mf_started: bool, dav1d_ready: bool) -> Choice {
    if mf_started {
        Choice::MediaFoundation
    } else if codec == 3 && dav1d_ready {
        Choice::Dav1d
    } else {
        Choice::None
    }
}
/// The AV1 bit of the advertised decode mask.
pub fn advertise_av1(mf_present: bool, dav1d_ready: bool) -> bool {
    mf_present || dav1d_ready
}

// ------------------------------------------------------------- statistics

static INPUT_DROPS: AtomicU64 = AtomicU64::new(0);
static OUTPUT_DROPS: AtomicU64 = AtomicU64::new(0);
static SESSIONS: AtomicU64 = AtomicU64::new(0);
static LAST_SESSION: Mutex<Option<String>> = Mutex::new(None);

/// decode ms p50/p95 of a window of samples (microseconds).
pub fn percentiles(samples: &[u32]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| f64::from(sorted[((sorted.len() - 1) as f64 * q).round() as usize]) / 1000.0;
    (at(0.5), at(0.95))
}
/// Process totals and the last finished session, for the trace log.
pub fn summary() -> String {
    format!(
        "{} sessions={} input_drops={} output_drops={} last=[{}]",
        READY.get().map_or_else(|| "decoder=dav1d check=not_run".to_owned(), Readiness::line),
        SESSIONS.load(Ordering::Relaxed),
        INPUT_DROPS.load(Ordering::Relaxed),
        OUTPUT_DROPS.load(Ordering::Relaxed),
        LAST_SESSION.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
    )
}

// ----------------------------------------------------------- the session

struct Slot {
    texture: ID3D11Texture2D,
    sample: IMFSample,
}
/// Whether anything besides the pool still holds this sample (a picture
/// being presented). The sample's own reference count is the only owner
/// signal Media Foundation gives a CPU-filled pool.
fn in_use(sample: &IMFSample) -> bool {
    let unknown: &IUnknown = sample.into();
    // SAFETY: a balanced AddRef/Release on a live interface; Release
    // returns the remaining count.
    let remaining = unsafe {
        (unknown.vtable().AddRef)(unknown.as_raw());
        (unknown.vtable().Release)(unknown.as_raw())
    };
    remaining > 1
}

pub struct Dav1dSession<'a> {
    owner: &'a DecodeDevice,
    native: Native,
    envelope: Envelope,
    queue: InputQueue,
    /// After open or a flush, nothing decodes until a keyframe.
    awaiting_key: bool,
    pool: Vec<Slot>,
    pool_size: (u32, u32),
    staging: Vec<u8>,
    decode_us: VecDeque<u32>,
    pending_since: Option<Instant>,
    input_drops: u64,
    output_drops: u64,
    pictures: u64,
}
impl Drop for Dav1dSession<'_> {
    fn drop(&mut self) {
        let samples: Vec<u32> = self.decode_us.iter().copied().collect();
        let (p50, p95) = percentiles(&samples);
        let line = format!(
            "{}x{} pictures={} decode_ms_p50={p50:.2} p95={p95:.2} input_drops={} output_drops={}",
            self.envelope.width, self.envelope.height, self.pictures, self.input_drops, self.output_drops
        );
        if let Ok(mut last) = LAST_SESSION.lock() {
            *last = Some(line);
        }
    }
}
impl<'a> Dav1dSession<'a> {
    /// `config`: the newest codec configuration OBUs (checked against the
    /// envelope now; fed again before every keyframe by the caller).
    pub fn open(owner: &'a DecodeDevice, header: VideoHeader, config: Option<&[u8]>) -> Result<Self, Dav1dError> {
        let envelope = Envelope::for_header(header)?;
        if let Some(sequence) = config.map(parse_sequence).transpose()?.flatten() {
            envelope.admits(&sequence)?;
        }
        let native = Native::open(envelope.pixels)?;
        SESSIONS.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            owner,
            native,
            envelope,
            queue: InputQueue::default(),
            awaiting_key: true,
            pool: Vec::new(),
            pool_size: (0, 0),
            staging: Vec::new(),
            decode_us: VecDeque::new(),
            pending_since: None,
            input_drops: 0,
            output_drops: 0,
            pictures: 0,
        })
    }
    pub fn envelope(&self) -> Envelope {
        self.envelope
    }
    pub fn queued(&self) -> (usize, usize) {
        (self.queue.len() + usize::from(self.native.holding()), self.queue.bytes())
    }
    pub fn drops(&self) -> (u64, u64) {
        (self.input_drops, self.output_drops)
    }
    /// False: not accepted (pending input is at its bound): pull, then offer
    /// the same unit again. Errors close the session (fail closed).
    pub fn push(&mut self, unit: &[u8], pts_us: i64, key: bool) -> Result<bool, Dav1dError> {
        let (_, has_sequence) = scan_unit(unit)?;
        if has_sequence {
            let sequence = parse_sequence(unit)?.ok_or(Dav1dError::Unit)?;
            self.envelope.admits(&sequence)?;
        }
        if self.awaiting_key {
            if !key {
                // Nothing to decode it against: wait for the keyframe the
                // receive path is already asking for.
                self.input_drops += 1;
                INPUT_DROPS.fetch_add(1, Ordering::Relaxed);
                return Ok(true);
            }
            self.awaiting_key = false;
        }
        let pending = self.queue.len() + usize::from(self.native.holding());
        if pending >= MAX_PENDING_UNITS || !self.queue.offer(unit, pts_us) {
            return Ok(false);
        }
        self.pump()?;
        Ok(true)
    }
    /// Hands queued units to dav1d until it holds one it cannot take yet.
    fn pump(&mut self) -> Result<(), Dav1dError> {
        if self.native.holding() {
            if let Sent::Again = self.native.resend()? {
                return Ok(());
            }
        }
        while let Some((unit, pts)) = self.queue.take() {
            self.pending_since.get_or_insert_with(Instant::now);
            if let Sent::Again = self.native.send(&unit, pts)? {
                break;
            }
        }
        Ok(())
    }
    /// Discontinuity: drop queued input (the generation changes), flush
    /// dav1d and wait for the next keyframe.
    pub fn flush(&mut self) {
        let dropped = self.queue.discard() as u64;
        self.input_drops += dropped;
        INPUT_DROPS.fetch_add(dropped, Ordering::Relaxed);
        self.native.flush();
        self.awaiting_key = true;
        self.pending_since = None;
    }
    pub fn generation(&self) -> u64 {
        self.queue.generation()
    }
    /// The next decoded picture as an NV12 texture sample on the decode
    /// device, or None.
    pub fn pull(&mut self) -> Result<Option<(IMFSample, OutputFormat, i64)>, DecodeError> {
        loop {
            let started = self.pending_since;
            let picture = self.native.picture()?;
            let Some(picture) = picture else {
                // Nothing out: let dav1d take more input, then try once more.
                if self.native.holding() || !self.queue.is_empty() {
                    self.pump()?;
                    if !self.native.holding() {
                        continue;
                    }
                }
                return Ok(None);
            };
            if let Some(t) = started {
                if self.decode_us.len() == 256 {
                    self.decode_us.pop_front();
                }
                self.decode_us.push_back(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
            }
            self.pictures += 1;
            let (width, height) = picture.size();
            if width > self.envelope.width || height > self.envelope.height {
                return Err(DecodeError::Format);
            }
            let format = output_format(&picture.sequence())?;
            let pts = picture.pts();
            let slot = Self::free_slot(self.owner, &mut self.pool, &mut self.pool_size, width, height)?;
            let Some(index) = slot else {
                drop(picture);
                self.output_drops += 1;
                OUTPUT_DROPS.fetch_add(1, Ordering::Relaxed);
                self.pending_since = None;
                self.pump()?;
                continue;
            };
            let (tw, th) = self.pool_size;
            picture.nv12(tw as usize, th as usize, &mut self.staging);
            drop(picture);
            let slot = &self.pool[index];
            unsafe {
                let context = self.owner.gpu.GetImmediateContext()?;
                context.UpdateSubresource(
                    &slot.texture,
                    0,
                    None,
                    self.staging.as_ptr().cast(),
                    tw,
                    tw * th * 3 / 2,
                );
                slot.sample.SetSampleTime(pts.checked_mul(10).ok_or(DecodeError::Input)?)?;
            }
            let sample = slot.sample.clone();
            self.pending_since = None;
            // Let dav1d take what it still holds before the caller presents.
            self.pump()?;
            let crop = Crop { left: 0, top: 0, width, height };
            return Ok(Some((sample, OutputFormat { crop, ..format }, pts.saturating_mul(10))));
        }
    }
    /// The index of a free pool texture of this size, creating the pool (4)
    /// on first use or a size change. None: all 4 are still being presented.
    fn free_slot(
        owner: &DecodeDevice,
        pool: &mut Vec<Slot>,
        size: &mut (u32, u32),
        width: u32,
        height: u32,
    ) -> Result<Option<usize>, DecodeError> {
        let wanted = (width.next_multiple_of(2), height.next_multiple_of(2));
        if *size != wanted {
            // Textures still being presented keep themselves alive.
            pool.clear();
            *size = wanted;
            for _ in 0..OUTPUT_POOL {
                let mut texture = None;
                unsafe {
                    owner.gpu.CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: wanted.0,
                            Height: wanted.1,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_NV12,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                            ..Default::default()
                        },
                        None,
                        Some(&mut texture),
                    )?;
                }
                let texture = texture.ok_or(DecodeError::Format)?;
                let sample = unsafe {
                    let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    sample
                };
                pool.push(Slot { texture, sample });
            }
        }
        Ok(pool.iter().position(|slot| !in_use(&slot.sample)))
    }
}

/// Colour metadata from the sequence header, refused like the MF path when
/// explicitly unsupported.
pub fn output_format(sequence: &Sequence) -> Result<OutputFormat, DecodeError> {
    let colour = match sequence.matrix {
        1 => Some(Colour::Bt709),
        5 | 6 => Some(Colour::Bt601),
        2 => None,
        _ => return Err(DecodeError::Unsupported),
    };
    let range = Some(if sequence.color_range == 1 { Range::Full } else { Range::Limited });
    let chroma = match sequence.chroma {
        1 => Some(ChromaLocation::Left),
        2 => Some(ChromaLocation::TopLeft),
        _ => None,
    };
    Ok(OutputFormat {
        crop: Crop { left: 0, top: 0, width: sequence.max_width, height: sequence.max_height },
        colour,
        range,
        chroma,
    })
}


#[cfg(test)]
mod tests;
