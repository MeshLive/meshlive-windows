# Design QV: a Windows PC watches Android live streams and audio-only radio (draft 2026-10-20, revision 2 after review 1, no wire code)

**2026-10-22 implementation note:** Android's design records (`docs/dev/decisions/` in meshlive-android), Design QV
revision 4, supersedes this retained revision 2 draft. Kind 1/2 use the full
23-byte forms. The default-off Watch and Go live experiments use F1, with only
one announcing provider. Current implementation status and the open W5-W8
checks are in [testing.md](../testing.md). This historical draft is preserved.

**W1 follow-up (2026-10-21).** This PC's adapter keeps one announcing provider on air ([architecture](../architecture.md#one-announcing-provider-at-a-time)). Watch and live mode therefore announce kind 1 or 2 under `c8d5a10a` on the only provider while the call offer pauses (option F1, accepted; no wire change).

**Status.** Draft for review. Nothing here is built. Revision 2 answers review 1 (R1 to R9); each finding is marked **[R#]** where the text changed, and the table at the end lists them.

- The Windows libraries it reuses exist and are unit-tested (listed under "Windows side").
- Every new on-air form is behind default-off switches until the review and gates V1 to V3 pass.
- Scope: viewer only. The PC signs `maxChildren = 0`, never forwards, never goes live and never relays in this design.

Android citations are `file:line` under `app/src/main/java/com/example/bluetoothtest/`, from the Android working tree of 2026-10-20 (accurate to about ±3 lines). "MA" means `MainActivity.kt`.

## Problem

A viewer joins a stream by **advertising a viewer request**. The parent (source or forwarder) scans for requests, picks one and **dials the viewer** (PROTOCOL "Experimental rotating discovery IDs", Phase 1b items 2 to 5; CF-RELAY-04; CF-LINK-37).

- On GATT the viewer is the GATT server: it hosts `c8d5a105` (write `c8d5a103`, notify `c8d5a104`).
- The parent connects as central and initiates the BMH2 link handshake. Stream hops use **BMH2 only**; BMH3 is used for calls only.
- Android code: `AndroidVideoReceiver.kt:507-545` (`receiveGattLoop`), `GattVideoTransport.kt:1303-1351,1534-1600` (`GattVideoServer`), `BluetoothVideoSender.kt:262-285,327-332`.

So a PC can watch only if an Android parent can **hear the PC's request and connect to it**.

Windows cannot send Android's request form. That form is a legacy connectable advert with `c8d5a101` in the primary PDU and the 22-byte `0xFFFF` manufacturer announcement in the scan response (`GattVideoTransport.kt:1335-1351`, `StableAdvertisingSet.kt:495-512`). The Design QW measurements found three blockers:

- The publisher refuses 128-bit service UUIDs.
- A manufacturer-data advert can only be non-connectable.
- A `GattServiceProvider` advert carries only its own service UUID, plus service data on extended advertising.

The PC's only proven on-air form is the Design QW call offer: service data under `c8d5a10a`, 9 bytes, kind 0 only. The QW review (R1) ruled that viewer requests "need their own design".

Scanning is not the problem. The Windows scanner already reads Android's stream offers: versions 2 and 5, compact v2 and the audio-only marker (`src/discovery.rs`, `src/bluetooth/scan.rs`).

## What Windows must scan (no wire change)

1. **Stream offers.** Versions 2 and 5 and compact v2 (version 5 uses the role-1 RDI; its route token is the logical token XOR mask(RDI)). Each offer carries a codec summary. The audio-only marker is `0x40` with zero geometry (CF-MEDIA-80).
   - Group rows by `LogicalStreamTarget(logical token, summary)`. Pick the best route: joinable first, then fewest hops.
   - **[R5]** A route is joinable only if its offer is a GATT offer (flag `0x08` set, `0x10` clear): the PC's request is always a GATT request, and a source selects only requests of its own transport (`StreamGattFallback.acceptsViewerRequest`). LE CoC and Classic routes are listed as "not reachable from this PC".
   - Drop offers older than 12 s. Dial only routes heard within 10 s, and discard a route after a failed attempt (CF-DISC-41, CF-RELAY-05).
   - Show codecs the PC cannot decode as dimmed and not joinable. The decode mask comes from Media Foundation (CF-HS-02).
   - Android: `MA:6935-6996` (`refreshNearbyStreams`), `MA:7008-7040` (staleness and suppression), `StreamDiscovery.kt:23,74-127`, `MA:7126-7260` (rows and title precedence).
2. **Title pages, version 3** (optional in the first build). Reassemble by (token, digest, count) within 30 s. At most 3 pages and 256 tokens; a contested title is blanked for 60 s (CF-DISC-43, CF-DISC-45).
   - Android: `TitlePage.kt:27-157`, decoded before the per-address dedup (`BleL2capControl.kt:597-609`).
   - The stream-title text rule (48 bytes) already exists in `src/names.rs`.
3. **Full and paused bits are hints only.** The full bit (`SEQUENCE_SOURCE_FULL`) and the paused bit are untrusted hints (CF-RELAY-03, CF-LIFE-03). A full source is not selected (`MA:10252-10262`), except to reconnect to the same source after a loss, as Android does **[R9]**.

## What Windows must advertise

### Recommended: option C, the compact viewer-request UUID as the provider's service UUID (no Android change)

Android already decodes iPhone compact viewer requests (kind 1) in the source's request scanner, and compact discovery is **on by default**:

- default: `ConnectivityFeatureDefaults.IPHONE_DISCOVERY = true` (`ConnectivityFeatures.kt:25`);
- the masked `C8D6…` service-UUID filter: `CompactDiscoveryAnnouncement.kt:93-94,150-160` and `BleL2capControl.kt:528-545`;
- the request scan and accept predicate: `MA:6667-6770`, accept at `:6730-6738`.

Two Design QW measurements suggest Windows can produce the iPhone form:

- Gate 2: **Windows emits a legacy UUID-list advert alongside the extended payload advert, from a different random address.**
- Gate 1: the legacy advert carries the provider's service UUID in the 128-bit UUID list.

So a `GattServiceProvider` whose service UUID **is** the compact viewer-request UUID should put the compact viewer-request UUID on air in a form Android's compact filter matches. Default Android sources would then find it and connect to the PC's `c8d5a105` data service, with no Android change at all. **[R4]** This is not radio-level indistinguishable from an iPhone: Windows adds its own UUIDs (QW gate 1 saw `0x180A`) and sends a legacy and an extended advert at once from two addresses. The PC logs its form as "compact viewer", and nothing in this design depends on looking like an iPhone.

**The UUID.** PROTOCOL "Experimental iPhone compact discovery", version 2 (rotating), kind 1, with these fields:

- prefix: the PC's role-2 RDI for the current epoch;
- bit 59 (GATT): 1;
- codec bits: 0 (a version 5 request carries summary 0);
- route cost and route-token bits 30..0: copied from the newest selected offer;
- bits 18..12: the bitrate in 25 kbps units;
- bits 11..0: zero.

**What already exists.** The encoder is in `advertise::compact_uuid` and `discovery::compact`. The provider form `Form::CompactAlias` (`src/bluetooth/peripheral.rs`) is Experimental, was built for call offers, and has never been shown to reach a phone.

**Rotation [R4].** Break-before-make, as Android item 6: at an epoch boundary while waiting, the request provider stops, stays silent for a uniform 1-8 s, and starts as a new provider with the new UUID (new address). The role-2 ID is pinned when a connection is accepted and stays openable for 60 s after it left the air (Phase 1b item 7). It stops when a parent is admitted. The data service stays up. Within one epoch the RDI is the same on every address Windows uses (including its own 15-minute address change), so those addresses are linkable to each other for that epoch, as on Android.

**Limits.** The same as an iPhone viewer's:

- No recovery hint (the compact form has no field for it). A PC that loses a forwarder parent sends an ordinary request.
- Only version 5 offers are joinable. **[R7]** This is a privacy policy, not a technical limit: compact version 1 can carry the PC's fixed prefix for a version 2 offer, but that would broadcast a fixed identifier, so the PC does not do it.
- A phone with "Find iPhones" turned off cannot see the PC.

**What must be proven (gate V1):**

- the legacy UUID-list advert with a compact UUID is **connectable**;
- a connection made through it reaches the `c8d5a105` provider;
- its address changes with every restart;
- Android sources accept the request.

- the exact UUID bytes appear on air (raw observer, not only an advertising status).

Gate 1 saw the UUID list only for the `c8d5a10a` UUID. QW item 2 showed status 4 with a compact UUID, but no phone was observing at the time.

### Fallback: option A, PC announcement kind 1 (needs an Android change)

If V1 fails, use the same service-data form as Design QW: UUID `c8d5a10a`, connectable extended advert, secondary PHY LE 1M. Byte 0 keeps the version and kind nibbles. Today's strict Android decoder checks the length first, so a 14- or 20-byte payload is rejected as `Length`, before its kind is read **[R9]** (`PcAnnouncement.kt:98-104`); either way an old phone drops it safely.

```text
byte 0       version (7..4) = 1, kind (3..0) = 1 viewer request
bytes 1..6   the viewer's role-2 RDI for the current epoch
byte 7       flags: bit 0 GATT (required), bit 3 route recovery (only with a hint); other bits zero
byte 8       bits 0..2 zero (summary 0), bits 3..6 route cost copied from the offer, bit 7 zero
bytes 9..12  route token copied from the newest selected offer (big endian, bit 31 set)
byte 13      bitrate in 25 kbps units (display only)
bytes 14..19 recovery hint, present exactly when bit 3 is set (length 20), else absent (length 14)
```

**Android receiving side.** Decoding plus a mapping in `ScanRecordForms` (`PcAnnouncement.kt:200-215`): kind 1 becomes an ordinary version 5 viewer-request `DiscoveryAnnouncement`. This sits behind "Find PCs" (`test_find_pcs`, default off) plus a new Experimental switch.

**Cost:** only sources with "Find PCs" on can see the PC, and only through extended scanning (Design QW amendment 1).

**Gain:** a recovery hint.

**Size:** 35 or 41 bytes on air. Extended advertising has room, but nothing above 9 bytes has ever been observed (gate V1A).

### Why the compact form's fields are enough

Every admission rule is unchanged and keys on fields both forms carry:

- the transport filter (`StreamGattFallback.acceptsViewerRequest`);
- the route-token window: the current token plus two retired tokens, each for 60 s (`RotatingStreams.kt:19-72`; `MA:613,6622,6733`);
- admission keyed by the announced bytes plus advertising address (`r:<6 bytes>@<address>`, `BluetoothVideoBroadcaster.kt:736-749`), then by node ID; a second link from the same node replaces the first (`:539-560,713`);
- the capacity cooldown of 30 s (`SourceCapacityRejectionPolicy.kt`; CF-RELAY-21) and the GATT pool backoff (`BluetoothVideoBroadcaster.kt:162-251`);
- request staleness of 12 s (`MA:6830,13798`);
- the role-2 BDO1 opening check and pins, within the two-epoch window.

### Rejected alternatives

- **The PC dials the parent.** No Android parent accepts an inbound viewer connection; the parent always dials (CF-RELAY-04). Changing that would be a protocol change for every phone.
- **A discoverable advert.** It broadcasts the PC's friendly name (QW option B).
- **A phone tethering media to the PC.** USB and Wi-Fi are development transports only.

## After the connection (no wire change; the PC is the listener and responder)

1. **Accept.** The PC accepts one central within 15 s (`CONNECTION_WAIT_TIMEOUT_MS`, `AndroidVideoReceiver.kt:1862`).
   - Source side: connect timeout 12 s, 2 GATT open attempts, MTU 517 requested.
   - Chunks are `type 1 | seq u32 | payload`, with payload = min(MTU−3, 512) − 5.
   - Media goes out as writes without response to `c8d5a103`. The PC sends its uplink as notifications on `c8d5a104` (`GattVideoTransport.kt:550-552,1514-1521,1747-1752`).
   - The PC already serves this direction for incoming calls (the `Peripheral` carrier used by `incoming::respond` and `verify_scan`).
2. **Link handshake (BMH2 responder).**
   - Link hello bit 3 is set on LE stream hops when rotating IDs are on and `maxChildren <= 1` (`RotatingStreams.kt:124-134`).
   - With both hellos carrying it, the first hop record each way is `BDO1`: the parent sends its role-1 entries, the PC its pinned role-2 entries (`RotatingDiscoveryRuntime.kt:392-413`).
   - The PC checks the parent's role-1 entry against the six bytes of the offer it selected (RV15, two-epoch window; `AndroidVideoReceiver.kt:755-806`).
3. **Endpoint handshake.**
   - The PC sends its real decode mask, `maxChildren = 0`, and bit 5 when labels are on.
   - The parent checks the stream's codec against the mask (audio-only is exempt).
   - For version 5, the endpoint peer must equal the link peer (`BluetoothVideoSender.kt:380-440`, `AndroidVideoReceiver.kt:785-799`).
4. **Forwarded stream.** The first endpoint record is the 68-byte `BFG1` grant (`GROUP_KEY`). The PC checks:
   - the grant's stream ID against the `BMV1` stream ID;
   - the pinned source on a replacement route;
   - the verified-contact gate on the source node ID.

   Media then opens with the granted key (`ReceiverStreamForwarder.kt:592-600`, `AndroidVideoReceiver.kt:806-842`; CF-RELAY-07, CF-REC-34). Windows already has this in `src/mesh.rs` and `watch::Reader::forwarded`.
5. **Labels [R6].** Only when both endpoint hellos carry bit 5 and the link peer is the endpoint peer: then exactly one `LABELS` record arrives at a fixed position, before the header from a direct source, right after `GROUP_KEY` from a forwarder, and it may hold zero labels. Without bit 5 on both sides the PC expects no `LABELS` record, and any record at that position is read as the next one.
   - It is a kind 2 title bound to the stream ID (or the zero ID), verified after the header (`BluetoothVideoSender.kt:471-481`, `AndroidVideoReceiver.kt:817-838`).
   - The receive code exists (`src/labels.rs`). The PC sends no labels on a stream hop.
6. **Header.** `BTV3` or `BTL1`, dispatched by magic (`StreamProtocol.kt:198-233`). The offer's marker and the header kind must agree, or the link fails closed (`AudioLiveProtocol.kt:127`).
   - **[R1]** A mismatch penalises only the attempted route (advertising address, announced prefix, route token) for 2 min. The PC never penalises the node ID: the advert is unauthenticated, so a node-wide penalty would let anyone who copies a source's RDI and token with the other marker get the honest source banned. Android's code also penalises only the route (it passes no node, `MainActivity.kt` about 11886). PROTOCOL's node rule disagrees with the code; reconciling it belongs to PROTOCOL's owner.
   - `BTL1` checks: the signer block (the identity's node ID equals the authenticated source, then the certificate), per-record signatures, `seq`, the Opus 20 ms unit rule, ingress budgets and clock rules (`AudioLiveProtocol.kt:20-190,238-290,483-552`; CF-MEDIA-80/82, CF-REC-07). Windows: `src/audio_live.rs`.
7. **Media.**
   - Direct `BMS1` type 5 or mesh `BMV1`, never both in one session (CF-REC-13; `watch.rs`).
   - Video: Media Foundation decode with keyframe gating and discontinuities (`viewer.rs`; CF-MEDIA-53/54).
   - Audio: on the shared playout clock (`listen.rs`, `playout.rs`), with the call path's catch-up speed (`time_stretch`, CF-MEDIA-68).
   - Output: `playback.rs`, including the reopen after a write timeout.
8. **Uplink** (`session::ViewerUplink`). The PC sends:
   - **[R2]** a link report every 2 s: version 2 (6 bytes: arrival-train kbps, flags with "link late", added delay ms) from the ArrivalMeter already used by calls (`call_rate.rs`) and `link.rs`. Android's source pacer needs these: without capacity reports its rate decays after about 4 s (`AndroidVideoReceiver.kt` about 1190, `LinkSelfMeasurement.kt` about 538). Version 1 only before the first report is measurable; version 3 only under the experimental stream delivery reports, which need every phone in the stream to accept it;
   - keyframe requests as an empty BME1 type 5 (never for audio-only);
   - `STREAM_END_ACK` after `ENDED`;
   - `ROUTE_CONTROL_ACK` within 4 s of a withdrawal.

   A parent never sends heartbeats. A silent GATT link closes after 6 s (`AndroidVideoReceiver.kt:1133-1190,1887-1908`; CF-LIFE-01, CF-REC-30).
9. **Lifecycle.**
   - `PAUSED` (repeated every 2 s) keeps the link.
   - `ENDED` returns the PC to the list and that broadcast is never retried.
   - An unexpected loss keeps the last frame and searches for the same logical stream: break-before-make, same authenticated source.
   - A capacity withdrawal suppresses the route for 30 s (CF-LIFE-03/05/06, CF-RELAY-12).
   - Media-state records 4 to 6 are shown.

## Windows side

Already built (unit-tested; the server side is used by incoming calls; never on air for streams):

- discovery and advertising: `discovery.rs`, `bluetooth/scan.rs`, `bluetooth/peripheral.rs` (provider, one subscriber, admission slot, `Form::CompactAlias`), `advertise.rs` (compact encoders);
- handshakes: `rotating.rs` (role-2 RDI, BDO1, pins), the `bmh2.rs` responder;
- the viewer pipeline: `watch.rs`, `viewer.rs`, `listen.rs`, `playout.rs`, `session.rs` (viewer uplink), `link.rs`, `mesh.rs`, `audio_live.rs`, `labels.rs`, `time_stretch.rs`, `playback.rs`, the video surface.

Missing, in build order:

1. **The viewer-request provider** (option C, compact kind-1 UUID): start, rotate while waiting, stop on admission. Behind `MESHLIVE_WATCH=1` (Experimental, default off) and the existing `qw_policy` allowlist.
2. **The join state machine.** Select an offer, request, accept one central within 15 s, then BMH2 responder with role-2 BDO1, endpoint responder, reader pipeline, suppression windows and the recovery loop.
3. **A Watch tab.** Rows, freshness, dimmed unsupported codecs, title board.
4. **Stream output.** Audio and video from the reader to `playback` and the surface.
5. **A headless `watch_phone` tool** for the phone gates, like `answer_phone`.

Option A would add a kind-1 codec with shared vectors (Rust, Kotlin, Swift) and the Android `PcAnnouncement`/`ScanRecordForms` change behind its own switch.

## Gaps and open questions

1. **Option C on air (blocking).** Is the legacy UUID-list advert connectable, and does a connection through it reach `c8d5a105`? If not, use option A and its Android change.
2. **Two announcements at once (answered by review 1: pause first).** While the PC waits for a parent, its call-offer announcement is paused (the PC is not callable for at most the 15 s wait). Keeping both on air (two providers, up to four adverts) is a later change after gate V2 measures it.
3. **The class marker (answered by review 1: acceptable).** Reusing the compact form is acceptable; the PC logs it as "compact viewer" and claims no radio-level indistinguishability **[R4]**.
4. **No recovery hint under option C (answered: acceptable initially).** After losing a forwarder, the PC queues behind ordinary requests and may wait for a capacity slot.
5. **Version 2 offers are not joinable (answered: acceptable privacy policy) [R7].** A source with rotating IDs off, or inside a Visible-by-handle window, cannot be watched from a PC: a request for it would have to carry the PC's fixed prefix.
6. **Forwarding from the PC is out of scope.** A PC stream offer needs about 22 bytes (token, sequence, geometry) and its own design.
7. **Audio-only defaults [R9].** Audio-only is on by default in the Android code (`AudioLiveConfig.kt:8-10`, `ENABLED = true`), and PROTOCOL already says so (revision 1 misread a stale heading). The PC's `BTL1` reader has no switch and fails closed.
8. **Media-state records 4 to 6.** Older readers drop the link on them; the PC reads them.
9. **Address residual, as on Android [R4].** A PC that waits across a boundary keeps its GATT service. Windows' own 15-minute address timer can split one epoch across two addresses; the shared RDI links those two addresses for that epoch (it links nothing across epochs).
10. **Source-side flood bounds [R3].** Admission resistance is inherited from Android, not proven: fresh RDI/address pairs bypass per-request cooldowns and use pre-authentication slots, and the rejection maps have no size bound or expiry sweep (`BluetoothVideoBroadcaster.kt`, `SourceCapacityRejectionPolicy.kt`). Gate V8 measures the bound; an Android-side cap is its own fix item, not part of this design.

## Test plan

Gates run in order; each is a hard stop.

- **V1, option C on air (Windows plus two Android raw observers, no app).**
  - Tool: `qv_gate --approved --minutes 20 --restart-at 600` (examples/qv_gate.rs). It publishes `c8d5a105` and a compact kind-1 request provider (1 byte of service data, LE 1M secondary), logs the exact UUID and its on-air LSB-first bytes, both providers' status events, and every subscription and write on each service with the peer tag, then restarts the request once with a new RDI and token.
  - Record the AD bytes of both adverts, the PDU type, the connectable bit, the address type and its rotation, and the status events.
  - Then make a plain GATT connect from a phone (nRF Connect) to the legacy advert's address and check that `c8d5a105` is listed.
  - Then subscribe to `c8d5a104` and write one byte to `c8d5a103` from nRF Connect: the tool must log `data_c8d5a105 event=subscribed` and `event=write` for that peer.
  - Pass (all required; an advertising status alone never passes): the observers show the exact UUID bytes logged by the tool; the PDU is connectable; the phone's connection lists `c8d5a105` and its subscribe and write reach the data service; the address changes at the restart; observers are unbonded to the PC. Per adapter: the result holds only for the adapter it ran on (`qw_policy` allowlist).
- **V1A, only if V1 fails.** Option A sizes (14 and 20 bytes) on the extended advert.
- **V2, two providers.** Call offer and request on air at once, both connectable. If this fails, the request pauses the call offer.
- **V3, Android source acceptance** (one phone, default settings, logs only). The source's request scan decodes the PC's compact request and dials it. Evidence:
  - the `compact` counters;
  - a `rotid_open: role=viewer` result;
  - a BMH2 link with the headless `watch_phone` responder.
- **V4, Windows unit tests (before V3) [R8].** The join state machine with fakes:
  - the 15 s accept window and one central;
  - role-2 BDO1;
  - the BFG1 checks;
  - the marker/header mismatch penalty;
  - `ENDED` versus an unexpected loss;
  - the suppression windows;
  - negative cases: a BDO1 that does not open the selected ID, a stale pin, a token outside the grace window, a BFG1 with the wrong stream ID or source, a central that connects and stays silent, a second central during a join, recovery when the source is full, and switch-off preservation (with `MESHLIVE_WATCH` off nothing new is advertised or scanned);
  - the v2 report cadence and contents (R2).
- **V5, phones, direct stream [R8].**
  - Every eligible test phone in turn as the source. VP9 and AV1 where offered, at the source's own settings, 5 min each. TESTING's startup gate applies (first picture within 3 s of the link), not the 15 s accept window.
  - The PC joins within the window and plays. It survives pause and resume, stops on `ENDED`, its keyframe requests are answered, and its heartbeats hold the link.
  - Audio-only radio at the Voice and Music presets on two phones.
  - A labelled, planned kill of the source: the PC searches, and does not retry after `ENDED`.
- **V6, phones, forwarded stream.** Source, then a forwarding phone, then the PC. Check:
  - the BFG1 grant;
  - the title label is byte-identical;
  - a replacement route keeps the original source;
  - after a forwarder loss, the PC's ordinary request is admitted.
- **V7, privacy.** An observer timeline over 30 min and two boundaries:
  - the request's address changes with its RDI;
  - no name goes on air;
  - nothing fixed is on air beyond what an iPhone request also shows.
- **V8, negative cases.**
  - A copied request from another address fails at the opening and does not starve the PC.
  - **[R3]** A flood: 50 compact requests per minute from fresh RDI/address pairs for 5 min against one source; record the source's pre-authentication slot use, its rejection-map size and whether the real PC viewer is still admitted within 60 s.
  - A version 2 offer row says why it cannot be joined.

Measurements stay separate: join time, frame-timing proxies, audio continuity and lifecycle are each their own result. Decoder timestamps are proxies, not screen latency.

## Review 1 findings and where they are addressed

| # | Finding | Addressed in revision 2 |
|---|---|---|
| R1 | Node-wide mismatch penalty lets an attacker ban an honest source | Header item 6: route-only penalty, as Android's code |
| R2 | Viewer must send v2 capacity reports | Uplink item 8: v2 reports every 2 s from the ArrivalMeter |
| R3 | Flood resistance inherited, not proven | Open question 10; V8 flood measurement |
| R4 | "Exactly like an iPhone" / "links nothing" overclaims; rotation details | Option C text, Rotation, open questions 3 and 9 |
| R5 | Transport in the join filter | Scan item 1 |
| R6 | LABELS only when negotiated; may be empty | After-connection item 5 |
| R7 | v5-only is a privacy policy | Limits, open question 5 |
| R8 | Gate order and coverage | V4 before V3 with negative cases; V5 every eligible phone and the 3 s startup gate |
| R9 | Length before Kind; full-source reconnection; audio-only already documented | Option A text, scan item 3, open question 7 |
