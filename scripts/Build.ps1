param([ValidateSet('Build','Test','Check')][string]$Action = 'Test', [ValidateSet('Debug','Release')][string]$Configuration = 'Debug', [ValidateSet('x86_64-pc-windows-msvc','aarch64-pc-windows-msvc')][string]$Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
foreach ($entry in @{ CARGO_HOME='.cache/cargo'; CARGO_TARGET_DIR='.artifacts/cargo'; TEMP='.cache/temp'; TMP='.cache/temp' }.GetEnumerator()) {
    $value = Join-Path $repo $entry.Value
    New-Item -ItemType Directory -Path $value -Force | Out-Null
    [Environment]::SetEnvironmentVariable($entry.Key,$value,'Process')
}
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_INCREMENTAL = '0'
$env:RUSTUP_NO_UPDATE_CHECK = '1'
$env:MESH_FXC = 'C:/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64/fxc.exe'
if (!(Test-Path -LiteralPath $env:MESH_FXC)) { throw 'Installed Windows SDK shader compiler not found; no installer will be run.' }
$profile = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$executable = [IO.Path]::GetFullPath((Join-Path $env:CARGO_TARGET_DIR "$Target/$profile/meshlive.exe"))
foreach ($process in Get-Process -Name meshlive -ErrorAction SilentlyContinue) {
    if ($process.Path -eq $executable) { throw 'Close this prototype before rebuilding its executable.' }
}
$mutex = New-Object Threading.Mutex($false,'Local\MeshLive.Windows.Build')
$held = $false
try {
    $held = $mutex.WaitOne(0)
    if (!$held) { throw 'Another MeshLive build is running.' }
    Push-Location $repo
    try {
        $saved = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        & python -B scripts/native_audio.py --target $Target 2>&1 | ForEach-Object { "$_" }; $code = $LASTEXITCODE
        $ErrorActionPreference = $saved
        if ($code -ne 0) { throw 'Pinned native codec build failed.' }
        $env:MESH_OPUS_LIB = Join-Path $repo ".artifacts/native/$Target/opus"
        $saved = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        & python -B scripts/native_encoder.py --target $Target 2>&1 | ForEach-Object { "$_" }; $code = $LASTEXITCODE
        $ErrorActionPreference = $saved
        if ($code -ne 0) { throw 'Pinned native video encoder build failed.' }
        $env:MESH_AOM_LIB = Join-Path $repo ".artifacts/native/$Target/aom"
        $env:MESH_VPX_LIB = Join-Path $repo ".artifacts/native/$Target/vpx"
        $saved = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        & python -B scripts/native_decoder.py --target $Target 2>&1 | ForEach-Object { "$_" }; $code = $LASTEXITCODE
        $ErrorActionPreference = $saved
        if ($code -ne 0) { throw 'Pinned native video decoder build failed.' }
        $env:MESH_DAV1D_LIB = Join-Path $repo ".artifacts/native/$Target/dav1d"
        $arguments = @('build','--locked','--target',$Target,'--jobs','1')
        if ($Action -eq 'Check') { $arguments[0] = 'check' }
        if ($Configuration -eq 'Release') {
            $arguments += '--release'
            # No build-machine path in a release exe: remap the checkout and the crate cache, name the PDB without its folder.
            $env:CARGO_ENCODED_RUSTFLAGS = @('-C', 'target-feature=+crt-static', "--remap-path-prefix=$($repo.TrimEnd('\'))\=",
                "--remap-path-prefix=$($env:CARGO_HOME)=cargo", '-C', 'link-arg=/PDBALTPATH:%_PDB%') -join [char]0x1f
        }
        $saved = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        & cargo @arguments 2>&1 | ForEach-Object { "$_" }; $code = $LASTEXITCODE
        $ErrorActionPreference = $saved
        if ($code -ne 0) { throw "cargo failed: $code" }
        if ($Action -eq 'Test') {
            $arguments[0] = 'test'; $arguments += @('--','--test-threads=1')
            $saved = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
            & cargo @arguments 2>&1 | ForEach-Object { "$_" }; $code = $LASTEXITCODE
            $ErrorActionPreference = $saved
            if ($code -ne 0) { throw "cargo tests failed: $code" }
        }
    } finally { Pop-Location }
} finally { if ($held) { $mutex.ReleaseMutex() }; $mutex.Dispose() }
