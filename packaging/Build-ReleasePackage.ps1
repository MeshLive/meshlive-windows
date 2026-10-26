# Builds the MeshLive MSIX from a release meshlive.exe (docs/dev/building.md).
#   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\Build-ReleasePackage.ps1 [-Build] [-CertificateThumbprint <sha1>] [-Register]
# -Build                 build meshlive.exe first through scripts\Build.ps1
# -CertificateThumbprint sign the .msix (local sideload tests only; the Store
#                        signs Store packages itself). The certificate subject
#                        must equal the manifest publisher (packaging\identity.json).
# -Register              register the unpacked layout for this user (Developer
#                        Mode; no certificate needed) to test the packaged identity.
# Output: a new folder with layout\, MeshLive_<version>_x64.msix and hashes.
[CmdletBinding()]
param(
    [string]$OutputPath = "",
    [string]$Exe = "",
    [string]$CertificateThumbprint = "",
    [switch]$Build,
    [switch]$Register
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $OutputPath) { $OutputPath = Join-Path $repo ".artifacts\release\msix-$(Get-Date -Format 'yyyyMMdd-HHmmss')" }
$out = [IO.Path]::GetFullPath($OutputPath)
if ((Test-Path -LiteralPath $out) -and (Get-ChildItem -LiteralPath $out -Force | Select-Object -First 1)) {
    throw "Release output must be new or empty: $out"
}
New-Item -ItemType Directory -Force -Path $out | Out-Null

if ($Build) {
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repo 'scripts\Build.ps1') -Action Build -Configuration Release *> (Join-Path $out 'build.log')
    if ($LASTEXITCODE -ne 0) { throw "Release build failed ($LASTEXITCODE); see $out\build.log" }
}
if (-not $Exe) { $Exe = Join-Path $repo '.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe' }
if (-not (Test-Path -LiteralPath $Exe)) { throw "meshlive.exe not found: $Exe (use -Build)" }
# The test-hooks build must never be packaged.
if (Select-String -LiteralPath $Exe -Pattern 'MESHLIVE_TEST_DIAL' -SimpleMatch -Quiet) {
    throw "This meshlive.exe contains the test hooks (built with --features test-hooks); package a release build."
}

$sdk = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64'
$makeappx = Join-Path $sdk 'makeappx.exe'
$signtool = Join-Path $sdk 'signtool.exe'
if (-not (Test-Path -LiteralPath $makeappx)) { throw "Windows SDK makeappx.exe not found: $makeappx" }

$identity = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'identity.json') | ConvertFrom-Json
$cargoVersion = (Select-String -LiteralPath (Join-Path $repo 'Cargo.toml') -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Matches[0].Groups[1].Value
$version = "$cargoVersion.0"

$layout = Join-Path $out 'layout'
New-Item -ItemType Directory -Force -Path $layout | Out-Null
Copy-Item -LiteralPath $Exe -Destination (Join-Path $layout 'meshlive.exe')
foreach ($doc in 'THIRD_PARTY_NOTICES.md', 'LICENSE') {
    $path = Join-Path $repo $doc
    if (Test-Path -LiteralPath $path) { Copy-Item -LiteralPath $path -Destination $layout }
}
# The bundled fonts' licences (SIL OFL 1.1) travel with the exe that embeds them.
foreach ($font in @(@('assets\fonts\OFL.txt', 'OFL-NotoSans.txt'), @('assets\fonts\Manrope-OFL.txt', 'OFL-Manrope.txt'), @('licenses\Apache-2.0.txt', 'Apache-2.0.txt'))) {
    Copy-Item -LiteralPath (Join-Path $repo $font[0]) -Destination (Join-Path $layout $font[1])
}
& python -B (Join-Path $PSScriptRoot 'make_assets.py') (Join-Path $repo 'assets\icon.png') (Join-Path $layout 'Assets')
if ($LASTEXITCODE -ne 0) { throw 'Asset generation failed (needs Python with Pillow).' }

$manifest = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'AppxManifest.xml')
$manifest = $manifest.Replace('{{IDENTITY_NAME}}', $identity.name).Replace('{{IDENTITY_PUBLISHER}}', $identity.publisher)
$manifest = $manifest.Replace('{{PUBLISHER_DISPLAY_NAME}}', $identity.publisherDisplayName).Replace('{{VERSION}}', $version)
if ($manifest -match '\{\{') { throw 'An unfilled {{token}} is left in the manifest.' }
[IO.File]::WriteAllText((Join-Path $layout 'AppxManifest.xml'), $manifest, (New-Object Text.UTF8Encoding($false)))

$msix = Join-Path $out "MeshLive_${version}_x64.msix"
& $makeappx pack /d $layout /p $msix /o | Out-File -Encoding utf8 (Join-Path $out 'makeappx.log')
if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE); see $out\makeappx.log" }

if ($CertificateThumbprint) {
    $thumb = $CertificateThumbprint.Replace(' ', '').ToUpperInvariant()
    $cert = Get-Item -LiteralPath "Cert:\CurrentUser\My\$thumb" -ErrorAction SilentlyContinue
    if (-not $cert) { throw "Signing certificate not found in CurrentUser\My: $thumb" }
    if ($cert.Subject -ne $identity.publisher) { throw "Certificate subject '$($cert.Subject)' does not match the manifest publisher '$($identity.publisher)'." }
    & $signtool sign /fd SHA256 /sha1 $thumb $msix | Out-File -Encoding utf8 (Join-Path $out 'signtool.log')
    if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE); see $out\signtool.log" }
}

if ($Register) {
    # Developer Mode loose-file registration: the packaged identity without a certificate.
    Add-AppxPackage -Register (Join-Path $layout 'AppxManifest.xml')
    Write-Output "registered layout=$layout"
}

Get-ChildItem -LiteralPath $out -File | Where-Object Extension -in '.msix' | ForEach-Object {
    [pscustomobject]@{ Path = $_.FullName; Length = $_.Length; SHA256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
}
[pscustomobject]@{ Path = (Join-Path $layout 'meshlive.exe'); SHA256 = (Get-FileHash -LiteralPath (Join-Path $layout 'meshlive.exe') -Algorithm SHA256).Hash }
