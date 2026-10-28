# Tester zip (a development artifact; users install from the Microsoft Store).
#   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\make_zip.ps1 [-Exe <release meshlive.exe>] [-OutputPath <new folder>]
# Contents: meshlive.exe (icon and version embedded), the user guide, privacy,
# LICENSE, THIRD_PARTY_NOTICES; SHA256SUMS next to the zip covers the zip and
# every file in it. Refuses a test-hooks build.
[CmdletBinding()]
param(
    [string]$Exe = "",
    [string]$OutputPath = ""
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Exe) { $Exe = Join-Path $repo '.artifacts\cargo\x86_64-pc-windows-msvc\release\meshlive.exe' }
if (-not (Test-Path -LiteralPath $Exe)) { throw "meshlive.exe not found: $Exe" }
if (Select-String -LiteralPath $Exe -Pattern 'MESHLIVE_TEST_DIAL' -SimpleMatch -Quiet) { throw 'This meshlive.exe contains the test hooks; zip a release build.' }
$version = (Select-String -LiteralPath (Join-Path $repo 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (-not $OutputPath) { $OutputPath = Join-Path $repo ".artifacts\release\zip-$(Get-Date -Format 'yyyyMMdd-HHmmss')" }
$out = [IO.Path]::GetFullPath($OutputPath)
if ((Test-Path -LiteralPath $out) -and (Get-ChildItem -LiteralPath $out -Force | Select-Object -First 1)) { throw "Output must be new or empty: $out" }
$stage = Join-Path $out "MeshLive-$version-windows-x64"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -LiteralPath $Exe -Destination (Join-Path $stage 'meshlive.exe')
foreach ($doc in 'docs\user-guide.md', 'docs\privacy.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md') {
    $path = Join-Path $repo $doc
    if (Test-Path -LiteralPath $path) { Copy-Item -LiteralPath $path -Destination $stage }
}
# The bundled fonts' licences (SIL OFL 1.1) travel with the exe that embeds them.
foreach ($font in @(@('assets\fonts\OFL.txt', 'OFL-NotoSans.txt'), @('assets\fonts\Manrope-OFL.txt', 'OFL-Manrope.txt'), @('licenses\Apache-2.0.txt', 'Apache-2.0.txt'), @('licenses\dav1d-COPYING.txt', 'dav1d-COPYING.txt'))) {
    Copy-Item -LiteralPath (Join-Path $repo $font[0]) -Destination (Join-Path $stage $font[1])
}
$zip = Join-Path $out "MeshLive-$version-windows-x64.zip"
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
$sums = @((Get-ChildItem -LiteralPath $stage -File) + (Get-Item -LiteralPath $zip)) | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
}
[IO.File]::WriteAllLines((Join-Path $out 'SHA256SUMS'), $sums)
Get-Content (Join-Path $out 'SHA256SUMS')
