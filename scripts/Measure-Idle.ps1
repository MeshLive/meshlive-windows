param([ValidatePattern('^[a-z0-9-]+$')][string]$Label = 'x64-idle-v2')
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$exe = Join-Path $repo '.artifacts/cargo/x86_64-pc-windows-msvc/release/meshlive.exe'
$output = Join-Path $repo ".artifacts/feasibility/$Label"
if (Test-Path -LiteralPath $output) { throw 'Use a fresh label; do not overwrite a prior run.' }
New-Item -ItemType Directory -Path $output -Force | Out-Null
$uiMetrics = Join-Path $output 'x64-idle-window.json'
$binaryHash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
# This starts only the owned prototype; it does not inspect or change other windows.
$app = Start-Process -FilePath $exe -WorkingDirectory $repo -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $output 'startup-stderr.txt') -ArgumentList @('--exit-after','150','--metrics',('"' + $uiMetrics + '"'))
Write-Output 'Owned prototype started; warming up for 20 seconds.'
Start-Sleep -Seconds 20
$app.Refresh()
if ($app.HasExited) { throw 'Prototype exited before the idle measurement.' }
$cpuStart = $app.TotalProcessorTime.TotalSeconds
$watch = [Diagnostics.Stopwatch]::StartNew()
$privateStart = $app.PrivateMemorySize64
$workingStart = $app.WorkingSet64
Write-Output 'Measuring 120 seconds of idle process CPU; other user workloads are uncontrolled.'
Start-Sleep -Seconds 120
$app.Refresh()
if ($app.HasExited) { throw 'Prototype exited during the idle measurement.' }
$elapsed = $watch.Elapsed.TotalSeconds
$cpu = $app.TotalProcessorTime.TotalSeconds - $cpuStart
$result = [ordered]@{
    scope = 'Release x64 UI idle only; radio and media inactive'
    executable_sha256 = $binaryHash
    warmup_seconds = 20
    elapsed_seconds = $elapsed
    cpu_seconds = $cpu
    cpu_percent_one_logical_cpu = 100 * $cpu / $elapsed
    private_bytes_start = $privateStart
    private_bytes_end = $app.PrivateMemorySize64
    working_set_bytes_start = $workingStart
    working_set_bytes_end = $app.WorkingSet64
    os_version = [Environment]::OSVersion.Version.ToString()
    other_workloads = 'Uncontrolled; no other application stopped or changed'
}
$result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'x64-idle-process.json') -Encoding UTF8
if (!$app.WaitForExit(20000)) { throw 'Owned prototype did not finish its bounded run; verify its exact process before cleanup.' }
Get-Content -LiteralPath (Join-Path $output 'x64-idle-process.json')
Get-Content -LiteralPath $uiMetrics
