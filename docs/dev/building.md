# Building MeshLive for Windows

How to build, test and package the Windows app from source.

## Requirements

- Windows 11 x64 (the only host that has been used to build; ARM64 build tools are not set up).
- Rust 1.88 or newer with the `x86_64-pc-windows-msvc` target (`rustup` default on Windows).
- Visual Studio 2022 or Build Tools with the **Desktop development with C++** workload (MSVC, `cl.exe`, `lib.exe`,
  MSBuild, `vcvarsall.bat`). The scripts find it with `vswhere`.
- Windows SDK **10.0.26100** (the scripts use its `fxc.exe` shader compiler, and packaging uses `makeappx.exe` and
  `signtool.exe` from the same folder).
- Python 3.11 or newer.
- Git for Windows (its `bash.exe` runs libvpx's `configure`, and its `perl.exe` runs libaom's code generator).
- A few gigabytes of free disk space for the downloaded sources, caches and build output.

Nothing is installed system-wide by the scripts: downloads, caches and temporary files stay in the checkout's
`.cache/`, and build output in `.artifacts/` (both are git-ignored).

## Build and test

From PowerShell in the repository root:

```powershell
# Build the pinned native codecs and the app, then run every unit and integration test (release).
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Build.ps1 -Action Test -Configuration Release

# Only build (Debug or Release), or only type-check.
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Build.ps1 -Action Build -Configuration Release
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Build.ps1 -Action Check
```

The app is then at `.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe`. Close it before rebuilding:
`Build.ps1` refuses to overwrite a running executable, and only one build runs at a time.

What `Build.ps1` does:

1. `scripts/native_audio.py` downloads the pinned libopus source and the build-only tools listed in
   `sources.lock.json` (CMake, NASM, GNU make), checks each SHA-256 before extracting, and builds libopus with the
   static CRT. Its `__FILE__` paths are trimmed (`/d1trimfile`), so no build folder ends up in the app.
2. `scripts/native_encoder.py` does the same for libaom 3.14.1 (software AV1) and libvpx 1.17.0 (software VP9).
3. `scripts/native_decoder.py` builds dav1d 1.5.3 (software AV1 decoding, used only when no Media Foundation AV1
   decoder starts) with Meson 1.9.1 and Ninja 1.13.0 run from their pinned wheels, also with trimmed paths.
4. Cargo builds and tests the app with `--locked` (exact versions from `Cargo.lock`) and one job at a time, so the
   build fits on a laptop with limited memory. The release profile links the C runtime statically
   (`.cargo/config.toml`). Release builds also remap the checkout and crate-cache paths
   (`--remap-path-prefix`) and name the PDB without its folder, so the exe embeds no build-machine path.

Tests run with `--test-threads=1`, because several tests own process-wide Windows state (COM apartments, Media
Foundation, the D3D11 device). Normal tests open no camera, microphone, speaker or Bluetooth radio; the D3D11 tests
use the WARP software renderer where no GPU is present.

### Build-time variables

`Build.ps1` sets these; set them yourself only if you run `cargo` directly.

| Variable | What it is |
| --- | --- |
| `MESH_FXC` | Path to the Windows SDK `fxc.exe` used by `build.rs` and the vendored renderer to compile the HLSL shaders |
| `MESH_OPUS_LIB` | Folder holding the static libopus built by `native_audio.py` |
| `MESH_AOM_LIB`, `MESH_VPX_LIB` | Folders holding the static libaom and libvpx built by `native_encoder.py` |
| `MESH_DAV1D_LIB` | Folder holding the static dav1d built by `native_decoder.py` |

Running `cargo` directly after one `Build.ps1` run:

```powershell
$env:MESH_FXC = 'C:/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64/fxc.exe'
$native = "$PWD\.artifacts\native\x86_64-pc-windows-msvc"
$env:MESH_OPUS_LIB = "$native\opus"; $env:MESH_AOM_LIB = "$native\aom"; $env:MESH_VPX_LIB = "$native\vpx"
$env:MESH_DAV1D_LIB = "$native\dav1d"
cargo test --locked --release --target x86_64-pc-windows-msvc --jobs 2 -- --test-threads=1
```

## Run

```powershell
.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe              # the app
.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe --offline    # the UI without the Bluetooth radio
.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe --demo       # with sample people and streams
.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe --warp --feature-level-10 --exit-after 5 --metrics .artifacts\warp-check.json
```

The last line is a bounded rendering check on the WARP software device at Direct3D feature level 10_0. Ctrl+1 to
Ctrl+4 switch pages; Ctrl+plus and Ctrl+minus scale text.

To run a development build next to an installed MeshLive without touching its identity or contacts, set
`MESHLIVE_REVIEW=1` for that process ([switches](switches.md)). It uses a separate identity and data folder next to
the executable.

### Test-hooks build

`--features test-hooks` builds a separate executable with the unattended test controls (`MESHLIVE_TEST_*` in the
[switch register](switches.md)) used to automate calls with phones:

```powershell
cargo build --locked --release --target x86_64-pc-windows-msvc --features test-hooks --target-dir .artifacts\cargo-testhooks
```

Never package or distribute it. The packaging scripts refuse an executable that contains the test hooks.

## Offline checks

```powershell
python -B scripts\check_sources.py                  # frozen assets, fixtures and vendored files match sources.lock.json
python -B scripts\check_sources.py --index          # the same for the staged Git blobs
python -B scripts\check_imports.py .artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe   # PE imports (no unexpected DLLs)
python -B scripts\third_party.py                    # regenerate the crate table in THIRD_PARTY_NOTICES.md
powershell -NoProfile -File scripts\Measure-Idle.ps1 -Label my-idle-run   # idle CPU of the release app over 150 s
```

`scripts/generate_video_fixtures.py` regenerates the synthetic VP9/AV1 test clips in `tests/fixtures/` (it needs a
pinned FFmpeg build and is not part of the normal build); `scripts/check_bmh3_fixture.py` compares the frozen BMH3
vectors with the Android app's Kotlin fixture; `scripts/gen_android_icons.py` regenerates `src/ui/android_icons.rs`
from the Android app's vector drawables.

## Package

### MSIX (Microsoft Store and sideloading)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\Build-ReleasePackage.ps1 -Build
```

This builds the release executable, generates the tile and logo images from `assets/icon.png`
(`packaging/make_assets.py`, needs Pillow), fills `packaging/AppxManifest.xml` from `packaging/identity.json`, and
writes `MeshLive_<version>_x64.msix` plus SHA-256 hashes to a new folder under `.artifacts\release\`.

- `identity.json` holds **placeholders** until the Store name is reserved. Put the Partner Center values there
  (package name and publisher). Never put a personal name in it.
- `-Register` registers the unpacked layout for your user (Developer Mode), so you can test the packaged identity
  without a certificate.
- `-CertificateThumbprint <sha1>` signs the package for local sideload tests. `packaging/New-TestCertificate.ps1`
  creates a self-signed test certificate whose subject matches the placeholder publisher. Never commit a certificate
  or key (`.gitignore` excludes `*.pfx`, `*.cer` and `*.p12`). Store packages are signed by the Store.
- `packaging/Run-Wack.ps1` runs the Windows App Certification Kit on the package (elevated).
- Store submission notes: [packaging/CERTIFICATION.md](../../packaging/CERTIFICATION.md).

### Tester zip

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\make_zip.ps1
```

Packs `meshlive.exe` with the user guide, privacy page, licences and font licences, and writes `SHA256SUMS` for the
zip and every file in it.

## Updating dependencies

- Rust crates are pinned with `=` in `Cargo.toml` and locked in `Cargo.lock`. After an update, run the full test
  suite, then `scripts/third_party.py` to refresh the notices.
- Native sources and build tools are pinned by URL and SHA-256 in `sources.lock.json`. Change the version, URL and
  hash together, and record the reason in the pull request.
- `vendor/egui-directx11` is a vendored, locally modified copy of the upstream renderer; see its `PROVENANCE.md`.
