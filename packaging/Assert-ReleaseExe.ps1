# Refuses a meshlive.exe that a release package must not contain (both
# packagers run it; src/main.rs tests it):
# - a test-hooks build (it carries the MESHLIVE_TEST_DIAL hook);
# - any build that reads the protection-off control arms: a debug build or a
#   release build with debug assertions on (audit SEC-13). The exe carries
#   src/control_arms.rs BUILD_MARKER, "...=SEALED" only when it reads none.
#   powershell -NoProfile -ExecutionPolicy Bypass -File packaging\Assert-ReleaseExe.ps1 -Exe <meshlive.exe>
# Exit code 0: the exe may be packaged; otherwise it says why and exits 1.
param([Parameter(Mandatory = $true)][string]$Exe)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "meshlive.exe not found: $Exe" }
# Latin-1 keeps one character per byte, so an ASCII marker is found as is.
$text = [Text.Encoding]::GetEncoding(28591).GetString([IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $Exe).Path))
if ($text.Contains('MESHLIVE_TEST_DIAL')) {
    throw "This meshlive.exe contains the test hooks (built with --features test-hooks); package a release build."
}
$marker = 'MESHLIVE-PROTECTION-ARMS='
if ($text.Contains($marker + 'READABLE') -or -not $text.Contains($marker + 'SEALED')) {
    throw "This meshlive.exe reads the protection-off control arms (a debug build, debug assertions on, or no release marker); package a release build."
}
"release exe ok: $Exe"
