# Testing

How to run the tests, how to check the app against your own Android phone, and the acceptance gates a feature has to
pass before it is called working. Build requirements are in [building.md](building.md).

Rules that apply to every test:

- **Every PC and every phone meets the same gates.** A fallback is not a pass for the requested path, and a setup
  that only connected is not a media pass.
- **Unmeasured stays unmeasured.** Offline tests, fake radios and test vectors are not evidence that something works
  between real devices. Decoder timestamps are proxies, not screen latency. Picture quality, frame timing, lifecycle,
  sound quality and latency are separate measurements.
- **Keep failures visible.** A later pass does not erase an earlier failure at the same settings; record both.
- **Never change a requested setting** (bitrate, frame rate, size, codec, transport, buffer) to make a test pass.

## Unit and integration tests

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Build.ps1 -Action Test -Configuration Release
```

This runs about 600 tests, one at a time. They need no phone, camera, microphone, speaker or Bluetooth radio:

- **Protocol conformance** against frozen vectors from the Android app (`tests/fixtures/`, `tests/vectors/`): BMS1
  and BME1 records, BMH2 and BMH3 handshakes and key schedules, rotating IDs and openings, compact and manufacturer
  discovery, presence leases, labels, verification codes, BTV3 and BTL1 media framing, forwarded-stream grants, the
  GATT carrier and lost-chunk recovery. `tests/fixtures/README.md` lists where each file comes from and what is
  checked. Fixture values are never edited to make a test pass.
- **Engines with fakes:** the GATT carrier through a fake radio (fragmentation, duplicates, reordering, tampering),
  call and stream state machines, fan-out, pacing, admission, playout, decoder format tracking, rate control.
- **Native Windows APIs without devices:** CNG, DPAPI, WASAPI format handling, Media Foundation decode of the
  synthetic `tests/fixtures/*.ivf` clips, D3D11 rendering and NV12 conversion on the WARP software device, libopus,
  libaom and libvpx round trips.
- **UI:** screens and flows driven through the UI's own methods with a scripted backend, plus pixel checks of
  offscreen renders. `MESH_UI_REVIEW=1` writes those renders as PNGs to `.artifacts/ui-refresh-review/`.

The CI workflow (`.github/workflows/ci.yml`) runs the same build and tests on a Windows runner. Hardware-dependent
tests skip or use WARP when a capability is missing; a green CI run says nothing about Bluetooth.

## Checking with your own phone

You need an Android phone with [MeshLive for Android](https://github.com/MeshLive/meshlive-android) and the PC within
a few metres. Use phones you own, keep the volume low, and point cameras at nothing private: these checks are about
the protocol, not about recording anyone.

### With the app

The simplest check is the app itself: call the phone from **Calls > Nearby**, call the PC from the phone, add each
other as contacts, and try Mute, Camera off and End. Then report what you saw with the
[PC test report](https://github.com/MeshLive/meshlive-windows/issues/new?template=device-report.yml). Turn on
`MESHLIVE_TRACE=1` before starting the app if you want a local trace for a bug report (it stays on your PC; remove
names and IDs before sharing any of it).

Watch and Go live need `MESHLIVE_WATCH=1` (and `MESHLIVE_GO_LIVE=1`) on the PC and, on the phone, Settings > Connection
> **Find PCs** and **PC live streams**, which are off by default. The phone must support extended Bluetooth scanning.

### Opt-in native checks

The unit-test binary contains opt-in checks that drive the normal app workers against a phone you select. They do
nothing unless `MESHLIVE_PHONE_CHECK` is set, so they are skipped in normal test runs. They save only counters and
text results under `.artifacts/phone/`: never audio, pictures, Bluetooth addresses or names. Each mode, and the
variables that select the phone, is listed in the [switch register](switches.md) (rows marked "Native unit-test binary
only"). For example:

```powershell
# Watch a phone's audio-only radio for 60 s. Read the stream's current rotating ID (12 hex digits) from your phone's log.
$env:MESHLIVE_PHONE_CHECK = 'watch-radio'
$env:MESHLIVE_PHONE_STREAM_RDI = '<rotating ID>'
$env:MESHLIVE_PHONE_SECONDS = '60'
cargo test --locked --release --target x86_64-pc-windows-msvc --bin meshlive opt_in_native_stream_phone_check -- --test-threads=1 --nocapture

# A generated-silence voice call to the phone; 'select' lists the call offers heard and waits for your choice.
$env:MESHLIVE_PHONE_CHECK = 'call-voice'
$env:MESHLIVE_PHONE_CALL_RDI = 'select'
cargo test --locked --release --target x86_64-pc-windows-msvc --bin meshlive opt_in_native_voice_call_phone_check -- --test-threads=1 --nocapture
```

(Set the build variables from [building.md](building.md#build-time-variables) first.) The checks never choose a phone
by themselves: you select it by its current rotating ID, route or identity handle, and accepting a call on the phone is
your action. Source checks (`source-radio`, `source-video`, `source-camera`) also need per-run consent variables, and
`source-camera` opens the PC camera only with `MESHLIVE_PHONE_CAMERA_CONSENT=1`.

### Test-hooks build

For unattended calls (dial a contact, auto-answer, hang up after a set time), build the separate test-hooks executable
([building.md](building.md#test-hooks-build)) and use the `MESHLIVE_TEST_*` variables. It is for testing only and must
never be distributed.

### Developer probes

`examples/` holds small console probes built on the same library. Those that use the radio refuse to run without
`--approved`; run them only on test devices you own.

| Probe | What it does |
| --- | --- |
| `bluetooth_probe` | Reads the adapter's capabilities; changes nothing and prints no names or addresses |
| `call_phone`, `answer_phone` | Headless outgoing or incoming call with a generated tone and test pattern |
| `show_code`, `scan_code` | Show or scan a verification code over the radio |
| `observe_adverts`, `qv_gate`, `gatt_matrix`, `provider_capacity`, `radio_check` | On-air measurements: what the PC advertises, how many providers fit, GATT behaviour |
| `video_probe`, `video_decode_probe`, `encode_probe`, `encode_bench` | Codec inventory, decode of the synthetic fixtures, encoder checks and latency |
| `audio_output_probe`, `capture_check`, `render_latency`, `camera_first_frames` | Audio output, capture levels, output latency, first camera frames (camera covered or pointed away) |
| `wipe_install` | Deletes this PC's MeshLive identity and stores (`--approved --confirm WIPE`, or `--dry-run`) |

## Acceptance gates

A feature is called working only after it passes these on real devices, in both directions where that applies, with
the evidence recorded. Each gate holds for every eligible PC and phone.

### Calls

- Voice and video, PC to phone and phone to PC, contact and stranger: rings, connects, media both ways, clean end.
- No media before Accept or after the end; Mute and Unmute send no old queued audio; Camera off and on resumes with a
  fresh keyframe.
- Busy, decline, no answer (Missed call after 60 s), blocked callers, and calling again after each.
- A call across a rotating-ID change and a call with the PC idle.
- The packaged (MSIX) app, separately from the unpackaged build.

### Watch (experimental)

- Each eligible Android source, video and audio-only radio separately, five minutes each: authenticated matching
  source, first decoded media within three seconds, no signature or sequence failures, no unintended parent loss,
  Pause and Resume, End, and rejoin. Leaving, End and parent loss cancel queued PC sound.
- Through an Android relay phone: the original source is kept, and after a deliberate parent loss the same source
  returns with fresh media while a substituted source is rejected.

### Go live (experimental)

- One viewer at a time, five minutes of video and of radio, repeated Start and End: stable paced delivery without
  rate decay or listener loss, Pause and Resume, and the authenticated end acknowledgement within the five-second
  grace. No microphone before a listener joins or after the last one leaves.
- Relayed onward by an Android phone to a second viewer, with the same checks on both legs.
- Viewer-request flood: 50 fresh requests per minute for five minutes while a real viewer keeps requesting; the real
  viewer is admitted within 60 seconds and tracking tables stay bounded.

### On-air checks

- With a raw Bluetooth observer for 30 minutes across two rotating-ID boundaries and provider restarts: the address
  changes with the rotating ID, no name is advertised or readable, one announcing provider, and every address type the
  PC uses is recorded. A new adapter family joins the allowlist only after this check.

**Not yet covered:** sound quality and echo, physical camera and microphone release timing, several viewers at once,
Windows 10, ARM64, and adapters other than the measured Intel family.

## Where things stand

The README's [Status](../../README.md#status) summarises what has passed on real hardware. Open from the gates above:
the packaged app, the Watch three-second startup target, PC streams relayed onward by a phone (a bounded run failed with
capacity withdrawals on the PC's link), intermittent source link stalls, the flood and on-air checks, and all
multi-viewer work.
