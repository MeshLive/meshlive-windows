# Video encoding

How the Windows app encodes video for calls and Go live, and what was measured on the development laptop. Decoding
and display are described in [architecture.md](architecture.md#video).

## Overview

- Hardware encoders are Media Foundation transforms (MFTs). They are asynchronous by design, so that path stays
  event-driven (bounded, non-blocking polling).
- Software AV1 is the pinned libaom 3.14.1 (the line the Android app builds) and software VP9 the pinned libvpx
  1.17.0, both behind a small C boundary (`src/video/encode_aom_shim.c`, `encode_vpx_shim.c`).
- `VideoEncoder` gives hardware and software one interface; `plan` orders the candidates as Android does;
  `KeyframePolicy` ports Android's `KeyFrameRequestBackoff` with a 60 s safety keyframe and coalescing;
  `SourceEncoder` encodes once per source, emits BTV3 packets and marks a discontinuity on size-changing restarts.

## Measured on the development laptop (2026-10-19, `examples/encode_probe --generated`)

- MFTs (all hardware/async; no software VP9/AV1 MFT): NVIDIA AV1 Encoder MFT, Intel Hardware Accelerated
  AV1 Encoder MFT (x2), Intel Hardware VP9 Encoder MFT (x2). Every ICodecAPI control (CBR, mean/max bitrate,
  1 s buffer, low latency, GOP) returned S_OK on NVIDIA AV1 and Intel VP9. The Intel AV1 MFT was not opened
  (NVIDIA sorts first).
- 60 textured frames at 15 FPS, keyframes requested at 37 (3 coalesced requests): every encoder gives one
  unit per picture, source timestamps, keyframes exactly [0, 37], all 60 decoded by the MF decoder.
  NVIDIA AV1 640x360/400: 457 kbps total, 354 steady, PSNR min 36.1 dB. Intel VP9 640x360/400: 510 total,
  439 steady, min 36.2 dB, 4.7 ms/frame. libaom 426x240/200: 204/210, min 32.9; 640x360/400: 413/376,
  min 34.6; 854x480/600: 619/571, min 34.7 (1.5-3.1 ms/frame).
- libvpx 1.17.0 software VP9: 426x240/200: 203 total/195 steady, min
  35.0 dB; 640x360/400: 407/399, min 35.0; 854x480/600: 624/603, min 34.6 (1.3-2.9 ms/frame); no superframes.
- Probe (plain pattern, 30 frames, 2 sealed viewers through the watch reader):
  identical media both viewers, PSNR min 50.8 (NVIDIA AV1), 43.2 (libaom), 49.1 (Intel VP9), 41.3 (libvpx) dB.
- Findings fixed: (1) NVIDIA AV1 writes the sequence header only on its first keyframe -> `SourceEncoder`
  re-inserts the newest one after the temporal delimiter of every keyframe (encode_bitstream.rs); the key flag
  comes from the bitstream, not MFSampleExtension_CleanPoint. (2) Codec controls set after the media types
  were ignored (GOP 30 observed) -> set before SetOutputType, plus MF_MT_MAX_KEYFRAME_SPACING. (3) Async
  output latency made the policy force a second keyframe at frame 1 -> forced inputs count immediately.
  (4) Without max bitrate/buffer NVIDIA overshot CBR by 30%. (5) An early probe truncated pts to the source
  frame index (frame 1 compared with frame 0) -> rounded.

## D3D11 input and forced selection (encode_d3d.rs, encode_tests_d3d.rs)

- MFTEnumEx results carry no adapter, so `hardware_candidates` enumerates per adapter with MFTEnum2 +
  MFT_ENUM_ADAPTER_LUID. The development laptop: NVIDIA RTX 4060 Laptop -> NVIDIA AV1; Intel UHD -> Intel AV1 (x2), Intel VP9 (x2).
- Texture input: a D3D11-aware MFT on the owner's adapter gets MFT_MESSAGE_SET_D3D_MANAGER and
  MFCreateDXGISurfaceBuffer samples; an open that fails with the manager is retried without it; an MFT without
  texture input gets one readback (`read_nv12`). `Encoder::open` prefers MFTs on the owner's adapter.
- GPU RGBA -> NV12: D3D11 video processor, BT.601 limited output, 8-texture pool; luma PSNR vs the BT.601
  formula 64.1 dB (NVIDIA), 65.9 dB (Intel).
- Forced selection, 640x360/400, 60 frames, both inputs: NVIDIA AV1 (texture: 458 kbps, min 37.2 dB) and both
  Intel VP9 registrations (texture: 479 kbps, min 37.7 dB) pass all checks. Both Intel AV1 registrations fail
  at ActivateObject (0x80004005, E_FAIL) with and without D3D: registered by the driver, not creatable on that
  iGPU (Xe-LP UHD has no AV1 encode block). Enumeration is not capability; `open` skips them.

## Latency and CPU (`examples/encode_bench --generated`)

640x360, 400 kbps CBR, 60 s real-time paced per run. Latency = submission to its unit back; CPU = whole bench
process in % of one logical core (20 cores; includes the bench's own polling and the GPU convert submission).
Other work may have run concurrently; numbers are from one laptop, one run.

| Encoder (input) | fps | kbps | p50 ms | p95 ms | max ms | CPU % core |
| --- | --- | --- | --- | --- | --- | --- |
| NVIDIA AV1 (D3D11 texture) | 15 / 30 | 407 / 409 | 2.38 / 2.52 | 5.18 / 4.20 | 31.5 / 33.0 | 0.5 / 0.7 |
| NVIDIA AV1 (system memory) | 15 / 30 | 408 / 410 | 1.86 / 1.65 | 2.56 / 2.28 | 70.2 / 3.2 | 1.3 / 0.7 |
| Intel VP9 (D3D11 texture) | 15 / 30 | 420 / 423 | 4.10 / 4.10 | 5.73 / 4.80 | 70.1 / 9.2 | 1.8 / 4.0 |
| Intel VP9 (system memory) | 15 / 30 | 440 / 438 | 4.12 / 4.19 | 5.30 / 8.45 | 36.3 / 33.5 | 1.0 / 3.4 |
| libaom 3.14.1 | 15 / 30 | 395 / 437 | 3.28 / 4.22 | 5.56 / 7.54 | 12.5 / 30.4 | 8.2 / 23.6 |
| libvpx 1.17.0 | 15 / 30 | 398 / 393 | 2.77 / 2.30 | 4.80 / 4.32 | 24.0 / 8.5 | 7.7 / 12.8 |

All runs: one unit per picture (900 / 1800). Intel AV1 does not open (see above).

## Decisions

- Codec order = Android ARCHITECTURE "Encoders": hardware first, AV1 before VP9, then software AV1, filtered
  by the peer's decode mask (VP9 = bit 0, AV1 = bit 1, `MeshControlMessages.CODEC_*`). A failed open is
  reported, never a silent switch; `open_planned` returns the skipped entries for display.
- libaom settings mirror Android `NativeAomTuning.DEFAULT` + `nativeCpuUsed`/`nativeThreadCount`: real-time,
  CBR, lag 0, AQ 3, PSNR tuning, CDEF, 3 refs, WebRTC tool profile, no frame drop, keyframes only forced.
  Output is sent unchanged: temporal delimiter + in-band sequence header on every keyframe (Android's native
  path sends no config packet either).
- Pinned the official v3.14.1 release tarball; it equals the v3.14.1 tag commit 03087864cf4b (tag object
  33c921ef). Android's reference checkout (0006e96, labelled 3.14.1 in its notices) is a later main commit:
  266 files differ from the release (line endings ignored); the difference is recorded for review.
- libvpx: Android builds none (MediaCodec VP9 only: CBR, no B-frames, keyframes on request + 60 s, not
  error-resilient). Pinned the latest release 1.17.0 (= tag commit 6df3ec34). Settings: WebRTC real-time VP9
  (q 2..52, AQ 3, row MT, tile columns by threads, static threshold 1, speed 6/7/8 by size) with Android's 1 s
  rate buffer, no frame drop, no auto alt-ref, lag 0 (no superframes). Order: hardware AV1, hardware VP9,
  software AV1, software VP9 (AV1 before VP9 among software, as Android).
- libvpx build: upstream configure (Git bash) + pinned GNU make 4.4.1 (ezwinports) + MSBuild /m:1, build dir
  beside the source (short relative path); the project generator's 13 KB argument list was silently truncated
  between native make and sh, so its exact `make -n` command runs from a script file. libvpx links unbundled
  (`static:-bundle`): bundled into the rlib, its members were not found at the final link.
- No checksum files are published for the libaom bucket or NASM; hashes are pinned from HTTPS downloads.
- `live::Source` merges join and recovery keyframe requests into one flag, so `encode_into` treats them as
  forced (no backoff). Splitting the flag would let recovery requests use Android's backoff.

## Open items

- Hardware MFT AV1 sequence header in-band: verified only on the encoders of the development laptop.
- Hardware AV1 opens can fail on other PCs; Go live with AV1 then reports the failure instead of switching codec,
  while calls keep their software fallback. No hardware AV1 pass is claimed beyond the laptop above.
