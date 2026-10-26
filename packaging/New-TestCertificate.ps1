# Creates a self-signed code-signing certificate for LOCAL sideload tests of
# the MSIX only (the Store signs Store packages itself). The subject equals
# the placeholder publisher in packaging\identity.json. The certificate and
# its key stay in CurrentUser\My; the public .cer goes to %TEMP% for the
# TrustedPeople import, which needs an elevated prompt the user approves.
# Never commit a certificate or key (.gitignore excludes *.pfx and *.cer).
$ErrorActionPreference = 'Stop'
$identity = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'identity.json') | ConvertFrom-Json
$cert = New-SelfSignedCertificate -Type Custom -Subject $identity.publisher -KeyUsage DigitalSignature `
    -FriendlyName 'MeshLive local test signing (not for distribution)' -CertStoreLocation 'Cert:\CurrentUser\My' `
    -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}') -NotAfter (Get-Date).AddMonths(3)
$cer = Join-Path $env:TEMP 'meshlive-test-signing.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Write-Output "thumbprint=$($cert.Thumbprint) cer=$cer"
Write-Output "To trust it for sideloading (elevated): Import-Certificate -FilePath `"$cer`" -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
