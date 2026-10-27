# Architecture

How MeshLive for Windows is built. The wire format and the security rules are not defined here: they are shared by
every MeshLive app and owned by
[meshlive-android's protocol reference](https://github.com/MeshLive/meshlive-android/blob/main/docs/dev/protocol/README.md)
(cited below as PROTOCOL). Identifiers such as `CF-CALL-24` refer to the Android app's conformance contract.

## Principles

- **Native Rust, Windows APIs only.** egui on Direct3D 11 through the official `windows` bindings, with AccessKit for
  accessibility. Direct3D feature level 10_0 is the minimum, with the WARP software device as fallback. No wgpu,
  Direct3D 12, WebView, .NET runtime or engine bridge. Media Foundation (camera, video codecs), WASAPI (audio), CNG
  (cryptography and the identity key) and DPAPI (files at rest) are operating-system APIs. libopus, libaom and libvpx
  are built from pinned sources with the same static CRT as the app.
- **Same protocol and security as Android.** Byte-for-byte conformance with the Android app is checked by frozen test
  vectors. Authentication, ordered encryption, encode-once fan-out, per-child queues, measured link pacing, admission
  and overload guards are ported, not redesigned. Bluetooth LE GATT is the only transport (no Classic, no LE L2CAP).
- **Fail closed.** A capability that cannot be proven (an advertising role, an adapter's address privacy, a codec) is
  reported as unavailable with its reason, never silently replaced by a weaker path.
- **Experiments are switchable.** Every experiment is off by default and listed in [switches.md](switches.md).

## Module map

| Area | Modules | Responsibility |
| --- | --- | --- |
| Cryptography | `crypto.rs`, `record_crypto.rs`, `identity.rs`, `sealed.rs` | CNG SHA-256, HMAC, HKDF, P-256 and AES-256-GCM; the persistent non-exportable identity key; DPAPI sealing of stores |
| Records and handshakes | `records.rs`, `framing.rs`, `bmh2.rs`, `bmh3/`, `identity_gate.rs` | BMS1 hop and BME1 endpoint records, bounded ingress (one message per read), BMH2 hellos and key agreement, BMH3 identity-hiding handshake with reveal and abuse guards |
| Discovery | `discovery.rs`, `rotating.rs`, `epoch_store.rs`, `pc_announcement.rs`, `presence.rs`, `cadence.rs`, `copied_id.rs` | Android's manufacturer and compact announcement formats, rotating IDs (RDI) and openings, the persisted monotonic epoch, the PC call-offer announcement, presence leases for contacts, how long a row may go unheard, the copied-ID warning |
| Bluetooth | `bluetooth/` (`scan`, `central`, `peripheral`, `carrier`, `gr1`, `pacer`, `advertise`, `gate`, `radio_watch`), `qw_policy.rs` | WinRT scanner, central and GATT server; the Android GATT chunk carrier and lost-chunk recovery; link pacing; what goes on air; the capability gate; Bluetooth on/off; the adapter privacy allowlist |
| Calls | `call.rs`, `call_session.rs`, `dial.rs`, `incoming.rs`, `callee.rs`, `call_audio.rs`, `call_camera.rs`, `call_video.rs`, `call_rate.rs`, `call_presence.rs`, `blocking.rs`, `keep_awake.rs`, `notify.rs` | Call signalling and state machine, outgoing and incoming setup, the call's audio and camera threads, video rate control, presence exchange inside a call, blocking, keeping the PC awake, incoming-call notifications |
| Streams | `live.rs`, `fanout.rs`, `session.rs`, `link.rs`, `mesh.rs`, `watch.rs`, `audio_live.rs`, `stream_watch.rs`, `stream_source.rs`, `viewer_admission.rs`, `stream_backend.rs`, `source_backend.rs` | Going live (admission, fan-out, per-child pacing and withdrawal), stream links and hop-local control, forwarded-stream grants, admitted watch ingress, signed audio-only radio (BTL1), the Watch and Go live workers |
| Media | `media.rs`, `opus.rs`, `playout.rs`, `listen.rs`, `viewer.rs`, `time_stretch.rs`, `playback.rs`, `output.rs`, `capture.rs`, `audio_devices.rs`, `generated.rs`, `video/` | BTV3 and Opus framing, libopus, the shared playout clock, audio and video receive paths, catch-up by speed, WASAPI output and capture, endpoint choice, generated test inputs, Media Foundation decode and encode |
| Contacts and identity | `contacts.rs`, `contact_backend.rs`, `claims.rs`, `labels.rs`, `names.rs`, `profile.rs`, `safety_code.rs`, `bvmid2.rs`, `qr_display.rs`, `qr_encode.rs`, `qr_scan.rs`, `verify_scan.rs` | Verified contacts, signed name labels, name rules, the PC's own name, safety codes, verification codes shown as QR and scanned by webcam |
| App | `main.rs`, `ui.rs`, `ui/`, `radio_backend.rs`, `graphics.rs`, `tray.rs`, `startup.rs`, `settings_store.rs`, `app_mode.rs`, `crash.rs`, `diagnostics.rs`, `wipe.rs`, `display_state.rs` | Window and screens, the Bluetooth owner behind the UI, the D3D11 renderer, tray icon, Start with Windows, saved settings, the developer review profile, crash reports, diagnostics export, Delete everything |
| Test harness | `phone_checks.rs`, `phone_checks/`, `ui/native_call_check.rs`, `ui/test_hooks.rs` | Opt-in checks with a real phone, compiled into the unit-test binary or the `test-hooks` build only ([testing](testing.md)) |

`lib.rs` exposes the protocol core as the `meshlive` library; `main.rs` is the app. The examples in `examples/` are
developer probes built on the same library ([testing](testing.md#developer-probes)).

## The interface

The UI is a single scaling column per page with an icon navigation rail: Live, Calls, Contacts and Settings, the same
screens and wording as the Android app, in the same black-and-white design (`ui/theme.rs`, Manrope type, the Android
app's icons drawn from its vector paths). `ui/model.rs` holds pure screen state machines; `ui/backend.rs` is the seam
to the core: the UI sends `Command`s and polls `Update`s. `radio_backend.rs` implements it over the real Bluetooth
owner; `--offline` runs the UI without the radio, and `--demo` adds clearly labelled sample people and streams.

Every control has an accessible name. Narrow or zoomed windows keep the navigation rail. Headings use the installed
Segoe UI Semibold when present, falling back to the bundled fonts; no OS font file is redistributed. The renderer is
a vendored, locally modified copy of `egui-directx11` (`vendor/`), with shaders compiled offline as shader model 4.
Native WARP pixel tests check geometry and clipping at 1x, 2x and combined user and display scaling.

## Bluetooth roles and the capability gate

In MeshLive the callee and the viewer are the advertising GATT server; callers and sources connect as central. So
receiving calls, watching and going live need the peripheral role. A role flag alone never proves it: the first
explicit advertisement start does (Started, or Aborted, incomplete, timeout or native refusal, latched until the user
asks again). An adapter without it keeps **Call a phone** and shows the plain reason. There is no Classic, fixed-ID or
cleartext fallback and no background retry (`bluetooth/gate.rs`).

The read-only adapter snapshot runs on a bounded worker. Off, disabled, absent, denied, unknown and unsupported states
block operations; the app never requests access or changes the radio state itself. Bluetooth off and on are observed
through `Windows.Devices.Radios.Radio.StateChanged`, wrapped in Android's adapter epoch (a growing on-delay and
crash-loop guidance after three offs in 60 s).

**Scanning** is active, because Android carries its 22-byte announcement in the scan response, which a passive
Windows scan never requests. A bounded join (256 addresses, parts at most 2 s old) applies Android's merged-record
rules: the control service UUID is required (or a compact UUID), manufacturer data wins, malformed manufacturer data
never falls through, and version 5 or compact version 2 offers are used only with rotating IDs. Addresses are
ephemeral routing hints with no serialization or debug output.

**The GATT carrier** matches Android: a type byte plus a signed 32-bit big-endian sequence, values of MTU minus three
capped at 512 bytes, 4096-entry reorder and history bounds, and a 1 MiB, 4096-chunk ready queue. Invalid input or
overflow closes the pipe. Each direction has one exclusive writer; a failed or cancelled write faults the direction
without retry. Byte pacing follows Android's `LinkPacer`. Lost-chunk recovery (`GR1`) and the 1-second fail-fast for a
gap are on by default, as on Android. The central discovers services uncached, installs notification and disconnect
handlers before subscribing, and uses a 12 s setup and 8 s write bound. The peripheral accepts only its first
subscribed session, rejects foreign writers and oversize values, and targets notifications to that subscriber.

**Announcing.** Windows does not let an app read or set its advertising address, so the PC announces itself only on
an adapter family on the measured allowlist (`qw_policy.rs`; see [Supported PCs](../devices.md#bluetooth-adapters)).
By default the call offer is a compact version 2 service UUID, which phones without extended scanning can see; the
extended form is an Alternative because two providers put one rotating ID on air from two addresses. Advertising is
never discoverable, so the computer's name never goes on air.

### One announcing provider at a time

On the measured adapter (extended advertising supported, 160 bytes of advertisement data), Windows keeps exactly one
announcing `GattServiceProvider` on air. A second announcing provider reports `Aborted` for its whole life, whatever
its service UUID, data size, or legacy or extended form. The data service (connectable, no data) does not count.
Whenever another provider starts or stops on the adapter, even in another app, the provider on air reads `Aborted`
for 10 to 40 ms and recovers; the call listener therefore treats the offer as dropped only after 1 s off air
(`callee::confirm_off_air`). Measured with `examples/provider_capacity.rs` (12 cases plus 6 stop and restart runs).

Consequence: one announcing provider at a time, chosen by mode. Calls mode announces the call offer. Watch and Go
live announce the PC viewer request or stream offer and pause the call offer until you leave
([design record](design/watch-and-go-live.md)). Other adapters may allow more; re-measure before assuming either way.

### Phones paired with this PC

When Windows holds a pairing (bond) with a phone, it resolves that phone's private address to its identity address and
connects as a bonded device, encrypted with the stored bond key, so the phone's Bluetooth stack knows which PC is
calling, even though MeshLive runs as a stranger above it. If the phone no longer holds a matching key, the link drops
at encryption. The central records `paired=` for every connection, and the call screen explains how to remove the
pairing. The app never pairs, unpairs or changes pairings.

## Calls

A call is a direct GATT link carrying BMH3 (or BMH2 for peers without the BMH3 hint), then authenticated call records
(`call.rs`, `call_session.rs`). The caller connects as central (`dial.rs`), the callee answers on the peripheral
(`incoming.rs`, `callee.rs`). Strangers meet throwaway keys; a contact is dialled with the contact tag from its
presence lease and the real identity at both layers, and the callee's first end-to-end record must be its opening.
The writer orders controls before audio before video, and large video frames can travel as fragments.

Media is admitted only between Accept and the end of the call (the call media gate, on by default). Mute discards
microphone audio still queued for sending, and Unmute waits for fresh capture (the capture fence, on by default).
Ring timeouts follow PROTOCOL: the caller cancels 60 s after Ringing, and the callee's own safety stop follows at 65 s.
A silent peer for 8 s ends the call. During a call the backend holds a named power request
(`PowerSetRequest(PowerRequestSystemRequired)`) so an idle PC keeps the link alive. Failed setups are retried once on
a fresh sighting within 15 s, because WinRT has no PHY control to copy Android's 2M-then-1M retry. Decoder failures in
a call recreate the decoder from the newest format, at most twice.

Call audio has its own thread that owns WASAPI capture and output (communications category, so Windows applies the
endpoint's echo cancellation where the driver offers it). Call video encodes on its own thread with its own D3D11
device and a newest-only picture slot, and decodes on its own thread, so neither blocks the Bluetooth worker. Rate
control ports Android's `LinkRateController`.

## Streams (beta)

Watch and Go live are on by default (`MESHLIVE_WATCH=0`, `MESHLIVE_GO_LIVE=0` turn them off); on a PC without the
needed adapter or Windows version the Live tab names what is missing. The PC joins an Android stream by
advertising a viewer request; the phone dials it, then the usual BMH2 link, rotating-ID opening and endpoint
handshake follow (`stream_watch.rs`, `watch.rs`). The PC checks the authenticated original source and pins it across
route replacements, and plays video through the shared playout clock and audio-only radio after verifying every
signed record (`audio_live.rs`). It signs `maxChildren = 0`, so it never forwards.

Going live (`live.rs`, `fanout.rs`, `stream_source.rs`) encodes once and fans out through per-child ordered queues
with Android's withdrawal rules. The PC admits one initial viewer: the reservation is a planning limit, not a measured
adapter capacity. The radio source opens its microphone only after an authenticated listener joins and closes it
after the last one leaves. Camera off, Pause and End release the camera and the encoder (the source camera fence, on
by default). The full design and its gates are in the [design record](design/watch-and-go-live.md).

## Video

**Decoding.** `video/decoder.rs` is a synchronous, D3D11-aware Media Foundation transform backend on the UI's D3D11
device (one device helper shared by the UI and the video worker, with multithread protection). It enumerates
hardware decoders first, then software ones, refuses asynchronous or non-D3D11-aware candidates, selects NV12, bounds
input to 4 MiB, and drains each access unit before the next. It requests `MF_LOW_LATENCY`. Before every input,
`video/bitstream.rs` checks the picture sizes of every VP9 frame (including hidden and intra-only frames and
superframes) and every AV1 sequence and frame header against the stream's declared maximum, so an opaque decoder is
never asked to allocate an unexpected picture. AV1 frame-size overrides, switch frames and tile lists are refused
rather than guessed. `video/format.rs` ports Android's `DecoderFormatTracker` (configuration changes at keyframes,
newest-configuration recovery, bounded pending configurations).

**Display.** `video/surface.rs` converts decoded NV12 (8-bit SDR, BT.601 or BT.709, full or limited range, explicit
chroma siting and crop) to an RGBA texture egui can draw, on the same device, with no CPU readback. Decoder texture
arrays cannot be bound directly as shader resources (and direct NV12 array views produced wrong plane bytes in local
hardware and WARP tests), so array or decoder-only input takes one GPU copy into a reused NV12 texture before the
colour pass. A copy-free path through the D3D11 video processor is future work.

**Encoding.** Hardware AV1 and VP9 through Media Foundation, then the bundled software AV1 (libaom) and VP9 (libvpx),
filtered by the peer's decode mask, in Android's order. A failed open is reported, never a silent switch. Details and
measurements: [video-encoding.md](video-encoding.md).

The test clips in `tests/fixtures/*.ivf` are three original synthetic 64x64 frames per codec, encoded losslessly;
`video-lossless.json` holds their packet and decoded-frame hashes. No captured or third-party footage is used.

## Storage

Everything lives under `%LOCALAPPDATA%\MeshLive` (the review profile uses its own folder next to the executable).

- **Identity:** a non-exportable, signing-only P-256 CNG key in the current user's store, named
  `io.github.meshlive.app.identity.v1`. Opening never replaces a key: it is created only when none exists, and any
  other failure is returned instead of being papered over with a new identity.
- **Verified contacts:** Android's `BVC1` format, one entry per fingerprint and per 6-byte prefix, at most 1024, in
  `identity-verification\verified-contacts.bin`. A contact is verified only when the confirmed fingerprint equals the
  one the current end-to-end handshake proved.
- **Presence leases:** Android's `BPS1` version 2 (random day keys, received leases, the per-contact switch) in
  `identity-verification\presence-leases.bin`, with an unsealed generation counter next to it.
- **Settings, profile name, blocked callers:** their own sealed files.

Stores are sealed with DPAPI in the current user's scope with a per-store entropy label, written atomically, and never
overwritten implicitly: a damaged store stays latched until it is reset, and a transient failure retries with a backoff
from 2 s up to 60 s. Name rules follow Android's (NFC, at most 64 bytes, no control, format, private-use or
noncharacter code points).

**Delete everything** (`wipe.rs`, CF-PRIV-26) deletes the identity key and every known file, including crash reports
and dumps. It never deletes through a reparse point and leaves unknown links untraversed; a failed or interrupted wipe
keeps Bluetooth, identity and stores closed on the next start until the wipe is retried successfully.

## Verification codes

The PC shows its own `BVMID2` code as a QR code (`qr_display.rs`, `qr_encode.rs`) and scans a phone's code from the
webcam or as pasted text (`qr_scan.rs`, `verify_scan.rs`). Scanning follows PROTOCOL "Verification code BVMID2": a
10 s search for call offers carrying the code's rotating ID, a BMH3 session bound to the code's fingerprint, and the
PC's real key presented only after the other side proved that fingerprint. The completed handshake is the
verification; the shield is stored only after the user confirms they scanned that person's screen. Presence leases are
exchanged inside the same scoped session (one offer and one lease per direction).

## Crash reports and diagnostics

A Rust panic or a native exception writes a short text report (a ring of five) and a minidump (three rotating slots),
locally only. On the next start the app offers to share the text through the user's own email app; names, keys,
codes and paths are scrubbed. Minidumps are never exported. **Export diagnostics** saves a text report with the app
version, Windows build, fixed on/off settings and scrubbed crash text, and no raw traces or device descriptions. The
optional trace (`MESHLIVE_TRACE=1`) stays on the PC and is deleted by Delete everything.
