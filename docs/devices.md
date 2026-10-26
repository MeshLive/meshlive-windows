# Supported PCs

Which Windows PCs MeshLive runs on, what has been tested, and how to report yours. Phone compatibility is listed in
[meshlive-android's devices page](https://github.com/MeshLive/meshlive-android/blob/main/docs/devices.md).

Every PC is held to the same acceptance checks ([testing](dev/testing.md)). A PC that is not listed here is not
known to fail; it has simply not been reported yet.

## Windows versions

| Windows | Calling out | Receiving calls | Watch / Go live (experimental) | Tested |
| --- | --- | --- | --- | --- |
| Windows 11 24H2 or newer, x64 | Yes | Yes, on an allowlisted adapter | Yes, on an allowlisted adapter | Yes (one laptop) |
| Windows 11 before 24H2, x64 | Expected | No: the app needs the extended-advertising API added in build 26100 | No | No |
| Windows 10 2004 or newer, x64 | Build target | No | No | No |
| Windows on ARM64 | Build target | Same rules as x64 | Same rules as x64 | No; not built yet |

## Bluetooth adapters

Calling a phone needs an adapter that supports the Bluetooth LE central role, which almost every adapter does.

Being **called** (and the Watch and Go live experiments) needs the PC to announce itself. Windows does not let an app
read or choose the address it advertises from, so MeshLive announces only on adapter families whose address rotation
has been measured: the address must change whenever the rotating ID changes, so a fixed address cannot link your
IDs together. This is a fail-closed allowlist in `src/qw_policy.rs`, the Windows counterpart of Android's
device-family policy registry. Each entry records its matcher, behaviour, sources, evidence and confidence.

| Family | Matcher | Evidence | Confidence |
| --- | --- | --- | --- |
| Intel USB Bluetooth adapters on Windows 11 24H2+ | USB vendor ID `8087`, Windows build 26100 or newer | Two Android raw observers: only the connectable extended advertisement carries the payload, every provider start uses a new random address, Windows rotates it after 15 minutes, and no address carried two payloads | One adapter measured; other Intel adapters inferred |

On an adapter that is not listed, Settings > Calls explains why the PC does not announce itself, and calling out
still works. To measure a new family, a developer can set `MESHLIVE_QW_UNLISTED=1` (never for everyday use) and follow
the on-air check in [testing](dev/testing.md#on-air-checks).

Known adapter limit: on the measured Intel adapter Windows keeps only **one** announcing GATT service on air at a time,
so watching or going live pauses the call announcement until you leave
([architecture](dev/architecture.md#one-announcing-provider-at-a-time)).

## Tested hardware

| PC | Windows | Bluetooth | Result |
| --- | --- | --- | --- |
| Development laptop (Intel Core with Intel UHD graphics and an NVIDIA RTX 4060 Laptop GPU) | Windows 11 24H2 or newer, x64 | Intel USB adapter (`8087`) | Calls in both directions with several Android phone models; bounded Watch and Go live runs. Details in the [README status](../README.md#status) |

Video encoders seen on that laptop: hardware AV1 (NVIDIA) and hardware VP9 (Intel) through Media Foundation, plus
the bundled software AV1 (libaom) and VP9 (libvpx) encoders. The Intel hardware AV1 encoder is registered by its
driver but cannot be opened on that GPU, so the app skips it. See [video encoding](dev/video-encoding.md).

## Report your PC

Use the [PC test report](https://github.com/MeshLive/meshlive-windows/issues/new?template=device-report.yml): Windows
version and build, the Bluetooth adapter (Device Manager name and USB vendor ID), what you tried with which phone, and
what worked. Reports where everything worked are just as useful. Do not post Bluetooth addresses, serial numbers or
anything that identifies you.
