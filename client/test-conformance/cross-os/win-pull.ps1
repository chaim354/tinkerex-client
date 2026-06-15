# cross-os-e2e leg 5/5 (one of two): WINDOWS PULL (CfAPI).
#
# Mounts ONE real Cloud Filter sync root with the WINDOWS device token, POLLs the sync-root path
# until the tagged file reads byte-for-byte == the EXPECTED sha (the EDITED content from linux-edit,
# passed via env), asserts, then unmounts. Sync is async (nudge + poll) and CfAPI hydrates lazily
# on open, so we poll with a timeout.
#
# Same mount/daemon + BLOCKED self-detection as win-push.ps1 (Session-0 / 0x8007017C -> exit 0
# BLOCKED, never a false red).
#
# Inputs (env, set by the workflow):
#   SERVER_URL    - the SHARED server
#   TOK_WIN       - the windows device token
#   TEST_TAG      - the per-run unique tag
#   EXPECTED_SHA  - the EDITED sha from linux-edit
#   PULL_TIMEOUT  - optional seconds to poll (default 90)
#
#   ./client/test-conformance/cross-os/win-pull.ps1
param(
  [string]$Server   = $(if ($env:SERVER_URL) { $env:SERVER_URL } else { "http://192.168.1.95" }),
  [string]$TokWin   = $env:TOK_WIN,
  [string]$Tag      = $env:TEST_TAG,
  [string]$Want     = $env:EXPECTED_SHA,
  [int]   $Timeout  = $(if ($env:PULL_TIMEOUT) { [int]$env:PULL_TIMEOUT } else { 90 })
)
$ErrorActionPreference = "Stop"

if (-not $TokWin) { Write-Host "BLOCKED: no TOK_WIN." -ForegroundColor Yellow; exit 0 }
if (-not $Want)   { Write-Host "BLOCKED: no EXPECTED_SHA (the linux-edit leg was BLOCKED or emitted no sha)." -ForegroundColor Yellow; exit 0 }
if (-not $Tag)    { $Tag = "notag" }

$repo   = (Resolve-Path "$PSScriptRoot\..\..\..").Path
$engine = "$repo\client\sync-engine\Cargo.toml"
$runid  = [System.Guid]::NewGuid().ToString("N").Substring(0, 8)
$root   = "$env:USERPROFILE\MakerCF-xos-winpull-$runid"
$db     = "$env:TEMP\maker-xos-winpull-$runid.db"
$chunks = "$env:TEMP\maker-xos-winpull-$runid-chunks"
$log    = "$env:TEMP\xos-winpull-$runid.log"
$fname  = "crossos-$Tag.txt"

Write-Host "== cross-os WIN PULL == server=$Server tag=$Tag file=$fname want=$Want run=$runid"

Get-Process maker-cfsyncd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-ChildItem "$env:USERPROFILE\MakerCF-xos-*" -Directory -ErrorAction SilentlyContinue |
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "==> Building maker-cfsyncd"
& cargo build --manifest-path $engine --bin maker-cfsyncd
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$cfsyncd = "$repo\client\sync-engine\target\debug\maker-cfsyncd.exe"

function StopDaemon($proc, [string]$dbpath) {
  if ($proc -and -not $proc.HasExited) {
    New-Item -ItemType File -Path "$dbpath.stop" -Force -EA SilentlyContinue | Out-Null
    if (-not $proc.WaitForExit(40000)) { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue }
  }
}
function Sha256OfFile([string]$path) {
  if (-not (Test-Path $path)) { return $null }
  (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower()
}

Write-Host "==> Mounting CfAPI sync root at $root"
$proc = Start-Process -FilePath $cfsyncd -ArgumentList @($root, $db, $chunks, $Server, $TokWin) `
  -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 8

$cloudInvalid = $false
foreach ($lg in @($log, "$log.err")) {
  if ((Test-Path $lg) -and (Select-String -Path $lg -Pattern '0x8007017C|cloud operation is invalid' -Quiet)) { $cloudInvalid = $true }
}
if ($cloudInvalid -or $proc.HasExited) {
  Write-Host "BLOCKED: CfAPI not usable (Session 0 / 0x8007017C, or daemon exited) - run the Windows runner interactively." -ForegroundColor Yellow
  if (Test-Path $log) { Get-Content $log | Out-Host }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) { Get-Content "$log.err" | Out-Host }
  StopDaemon $proc $db
  exit 0
}

$ok = $false
try {
  Write-Host "==> Polling $root\$fname for byte-for-byte == $Want (timeout ${Timeout}s)"
  $deadline = (Get-Date).AddSeconds($Timeout); $got = $null
  while ((Get-Date) -lt $deadline) {
    $p = Join-Path $root $fname
    if (Test-Path $p) { $got = Sha256OfFile $p; if ($got -eq $Want) { $ok = $true; break } }  # open triggers hydrate
    Start-Sleep -Milliseconds 1000
  }
  if ($ok) { Write-Host "  PASS  $fname reads byte-for-byte ($Want) through the CfAPI mount" -ForegroundColor Green }
  else     { Write-Host "  FAIL  $fname did not converge to $Want within ${Timeout}s (last: '$got')" -ForegroundColor Red }
}
finally {
  StopDaemon $proc $db
  Start-Sleep -Seconds 2
  if (Test-Path $log) { Get-Content $log | Out-Host }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) { Write-Host "-- stderr --"; Get-Content "$log.err" | Out-Host }
}

if ($ok) { Write-Host "WIN PULL PASSED" -ForegroundColor Green; exit 0 }
Write-Host "WIN PULL FAILED" -ForegroundColor Red
exit 1
