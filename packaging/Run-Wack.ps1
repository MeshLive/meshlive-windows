# Windows App Certification Kit on a built MeshLive MSIX. Needs an ELEVATED
# PowerShell (appcert.exe refuses to run otherwise), so the user runs it:
#   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\Run-Wack.ps1 -Msix <path to .msix> [-Thumbprint <test cert>]
# Steps: sign the MSIX with the local test certificate (packaging\New-TestCertificate.ps1;
# WACK installs the package, and Windows installs only signed packages), trust that
# certificate in LocalMachine\TrustedPeople, remove a Developer Mode registration of
# the same identity, run `appcert test`, and write report.xml plus a short summary
# next to the MSIX. The test certificate is for this check only; the Store signs
# the real package. Remove it afterwards from Cert:\LocalMachine\TrustedPeople if wanted.
param(
    [Parameter(Mandatory = $true)][string]$Msix,
    [string]$Thumbprint = ""
)
$ErrorActionPreference = 'Stop'
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) { throw 'Run this from an elevated PowerShell (Run as administrator).' }
$msix = (Resolve-Path -LiteralPath $Msix).Path
$out = Split-Path -Parent $msix
$identity = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'identity.json') | ConvertFrom-Json
$sdk = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64'
$appcert = 'C:\Program Files (x86)\Windows Kits\10\App Certification Kit\appcert.exe'
if (-not (Test-Path -LiteralPath $appcert)) { throw "Windows App Certification Kit not found: $appcert (install the Windows SDK component)." }

if (-not $Thumbprint) {
    $existing = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $identity.publisher -and $_.NotAfter -gt (Get-Date) } | Select-Object -First 1
    if (-not $existing) { throw "No test certificate for '$($identity.publisher)'. Run packaging\New-TestCertificate.ps1 first (as the same user)." }
    $Thumbprint = $existing.Thumbprint
}
$cert = Get-Item -LiteralPath "Cert:\CurrentUser\My\$Thumbprint"
$signed = Join-Path $out ([IO.Path]::GetFileNameWithoutExtension($msix) + '.wack-signed.msix')
Copy-Item -LiteralPath $msix -Destination $signed -Force
& (Join-Path $sdk 'signtool.exe') sign /fd SHA256 /sha1 $Thumbprint $signed | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'signtool failed.' }
$cer = Join-Path $env:TEMP 'meshlive-wack-test.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null

Get-AppxPackage -Name $identity.name -AllUsers -ErrorAction SilentlyContinue | Remove-AppxPackage -AllUsers -ErrorAction SilentlyContinue

$report = Join-Path $out 'wack-report.xml'
& $appcert reset | Out-Null
& $appcert test -appxpackagepath $signed -reportoutputpath $report
$code = $LASTEXITCODE
if (Test-Path -LiteralPath $report) {
    [xml]$xml = Get-Content -Raw -LiteralPath $report
    $overall = $xml.REPORT.OVERALL_RESULT
    $lines = @("overall=$overall")
    foreach ($test in $xml.SelectNodes('//TEST')) {
        $lines += "{0} | {1}" -f $test.RESULT.'#cdata-section', $test.NAME
    }
    $lines | Set-Content -Encoding utf8 (Join-Path $out 'wack-summary.txt')
    $lines
}
"appcert exit $code; report: $report"
