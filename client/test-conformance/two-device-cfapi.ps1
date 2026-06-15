# Two-device, REAL-MOUNT, end-to-end test on Windows (CfAPI).
#
# This is the layer ABOVE the engine-level `two_device_e2e.rs`: it drives the actual app stack —
# the real `sync_engine::CfApiMount` (the daemon the Tauri app's `sync_start` command launches) with
# an ACTUAL Cloud Filter kernel mount — for TWO devices paired into ONE library, and passes files
# THROUGH the mounts (filesystem writes/reads in the sync-root folders), not via Provider calls.
#
# Flow (the headline workflow, app-command-level):
#   1. Seed: register device A + pair device B into one library (the REAL pairing flow, via the
#      `pair_two_devices` example = the app's create_pairing/sync_pair commands).
#   2. Mount BOTH sync roots live at once (two `maker-cfsyncd` = the app's CfApiMount).
#   3. Write a file THROUGH device A's mount  -> assert it appears + reads byte-for-byte THROUGH B's mount.
#   4. Edit a file THROUGH device B's mount   -> assert device A's mount sees the new bytes.
#   5. Plus: new file, nested dir, delete, rename, and a multi-chunk (>chunk-size) file — each
#      written through one mount and verified through the other.
#
# Requires an INTERACTIVE Windows login session: CfAPI/Cloud Files enumerates empty under the
# Session-0 service account, so this is on-demand / nightly + continue-on-error in CI (it gates
# nothing) and PASSES only on the interactive self-hosted Windows runner. The deployed server
# (S3/SeaweedFS chunk backend) must be reachable so the *other* device can hydrate the bytes.
#
# Usage:
#   ./client/test-conformance/two-device-cfapi.ps1 [-Server http://192.168.1.95]
param(
  [string]$Server = $(if ($env:SERVER_URL) { $env:SERVER_URL } else { "http://192.168.1.95" })
)
$ErrorActionPreference = "Stop"
$failures = @()
function Check([string]$name, [bool]$ok) {
  if ($ok) { Write-Host "  PASS  $name" -ForegroundColor Green }
  else { Write-Host "  FAIL  $name" -ForegroundColor Red; $script:failures += $name }
}

$repo   = (Resolve-Path "$PSScriptRoot\..\..").Path
$engine = "$repo\client\sync-engine\Cargo.toml"
$runid  = [System.Guid]::NewGuid().ToString("N").Substring(0, 8)

# UNIQUE per-run paths per device. A CfAPI sync root hard-killed by a prior run stays registered and
# refuses deletion, so we never reuse a folder.
$rootA   = "$env:USERPROFILE\MakerCF-2dev-A-$runid"
$rootB   = "$env:USERPROFILE\MakerCF-2dev-B-$runid"
$dbA     = "$env:TEMP\maker-2dev-A-$runid.db"
$dbB     = "$env:TEMP\maker-2dev-B-$runid.db"
$chunksA = "$env:TEMP\maker-2dev-A-$runid-chunks"
$chunksB = "$env:TEMP\maker-2dev-B-$runid-chunks"
$logA    = "$env:TEMP\2dev-cfapi-A-$runid.log"
$logB    = "$env:TEMP\2dev-cfapi-B-$runid.log"

Write-Host "== Two-device CfAPI real-mount E2E == server=$Server run=$runid"
Write-Host "   A root=$rootA"
Write-Host "   B root=$rootB"

# Kill leftover daemons (a zombie maker-cfsyncd keeps a live watcher that can pollute results), then
# sweep leftover roots from prior runs.
Get-Process maker-cfsyncd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-ChildItem "$env:USERPROFILE\MakerCF-2dev-*" -Directory -ErrorAction SilentlyContinue |
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "==> Building maker-cfsyncd + pair_two_devices"
& cargo build --manifest-path $engine --bin maker-cfsyncd --example pair_two_devices
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$cfsyncd = "$repo\client\sync-engine\target\debug\maker-cfsyncd.exe"

# --- 1. Seed: register A + pair B into one library (the REAL pairing flow) --------------------
Write-Host "==> Pairing two devices into one library (register A -> create_pairing -> pair B)"
$env:SERVER_URL = $Server
# Relax ErrorActionPreference for the native cargo call: any stderr (a stray cargo progress line)
# would otherwise abort under 'Stop'. We assert on the parsed tokens below, so a non-fatal stderr
# line is fine.
$prevEAP = $ErrorActionPreference; $ErrorActionPreference = "Continue"
$seed = & cargo run --quiet --manifest-path $engine --example pair_two_devices 2>&1
$ErrorActionPreference = $prevEAP
$seed | Out-Host
$tokA = ($seed | Select-String -Pattern '^DEVICE_A_TOKEN=(.+)$').Matches[0].Groups[1].Value.Trim()
$tokB = ($seed | Select-String -Pattern '^DEVICE_B_TOKEN=(.+)$').Matches[0].Groups[1].Value.Trim()
if (-not $tokA -or -not $tokB) { throw "could not parse both device tokens from pair_two_devices" }
Write-Host "  got A token ($($tokA.Length) chars) + B token ($($tokB.Length) chars)"

# Helpers ------------------------------------------------------------------------------------
function MountDevice([string]$root, [string]$db, [string]$chunks, [string]$token, [string]$log) {
  Start-Process -FilePath $cfsyncd -ArgumentList @($root, $db, $chunks, $Server, $token) `
    -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
}
function StopDevice($proc, [string]$db, [string]$log) {
  if ($proc -and -not $proc.HasExited) {
    New-Item -ItemType File -Path "$db.stop" -Force -EA SilentlyContinue | Out-Null
    if (-not $proc.WaitForExit(40000)) {
      Write-Host "  daemon ($db) did not exit on the stop sentinel within 40s - forcing"
      Stop-Process -Id $proc.Id -Force -EA SilentlyContinue
    }
  }
}
function Sha256OfFile([string]$path) {
  if (-not (Test-Path $path)) { return $null }
  (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower()
}
function Sha256OfBytes([byte[]]$bytes) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString("x2") }) -join ""
}
# Write bytes THROUGH a mount; if CfAPI rejects it with the Session-0 "cloud operation is invalid"
# error, raise a sentinel the top-level catch turns into a clean BLOCKED (not a raw stack trace).
function WriteThroughMount([string]$path, [byte[]]$bytes) {
  try { [System.IO.File]::WriteAllBytes($path, $bytes) }
  catch {
    if ("$($_.Exception.Message)" -match 'cloud operation is invalid|0x8007017C') {
      throw "CFAPI_SESSION0_BLOCKED"
    }
    throw
  }
}
# Poll for a file to appear THROUGH a mount and read byte-for-byte (CfAPI hydrates on open). The
# sync is nudge-driven (~tens of ms) but CfAPI enumeration + hydration is lazy, so we poll.
function WaitForBytes([string]$path, [string]$wantSha, [int]$timeoutSec = 90) {
  $deadline = (Get-Date).AddSeconds($timeoutSec)
  while ((Get-Date) -lt $deadline) {
    if (Test-Path $path) {
      $got = Sha256OfFile $path     # opening the placeholder triggers FETCH_DATA (hydrate)
      if ($got -eq $wantSha) { return $true }
    }
    Start-Sleep -Milliseconds 750
  }
  return $false
}
function WaitForAbsent([string]$path, [int]$timeoutSec = 90) {
  $deadline = (Get-Date).AddSeconds($timeoutSec)
  while ((Get-Date) -lt $deadline) {
    if (-not (Test-Path $path)) { return $true }
    Start-Sleep -Milliseconds 750
  }
  return $false
}

# --- 2. Mount BOTH sync roots live at once ---------------------------------------------------
Write-Host "==> Mounting BOTH CfAPI sync roots (A -> $logA, B -> $logB)"
$procA = MountDevice $rootA $dbA $chunksA $tokA $logA
$procB = MountDevice $rootB $dbB $chunksB $tokB $logB
Start-Sleep -Seconds 8   # register + pull the (empty) snapshot on both

# Preflight: CfAPI's Cloud Filter platform only operates in an INTERACTIVE desktop session. When
# the self-hosted runner runs as a service (Session 0 / NETWORK SERVICE), FETCH_PLACEHOLDERS
# returns 0 entries and TRANSFER_PLACEHOLDERS / a write fails with 0x8007017C "The cloud operation
# is invalid". That's an environment wall, not a product/test failure — detect it and exit 0 with a
# BLOCKED notice (so the continue-on-error job reads clearly), exactly like the FUSE script's
# /dev/fuse check. To make this PASS, run the Windows runner in an interactive login session
# (ci/runners/README).
function StopBoth { StopDevice $procA $dbA $logA; StopDevice $procB $dbB $logB }
$cloudInvalid = $false
foreach ($lg in @($logA, "$logA.err", $logB, "$logB.err")) {
  if ((Test-Path $lg) -and (Select-String -Path $lg -Pattern '0x8007017C|cloud operation is invalid' -Quiet)) { $cloudInvalid = $true }
}
if ($cloudInvalid) {
  Write-Host "BLOCKED: CfAPI reports 0x8007017C (the cloud operation is invalid) — the Windows runner is NOT in an interactive desktop session (Session 0)." -ForegroundColor Yellow
  Write-Host "         Cloud Filter can't enumerate/transfer under the service account. Run the runner in an interactive login session to exercise this leg."
  Write-Host "         (The engine-level two_device_e2e.rs proves convergence headlessly; this script needs the real interactive mount.)"
  StopBoth
  exit 0
}

try {
  Check "A mount is running"  (-not $procA.HasExited)
  Check "B mount is running"  (-not $procB.HasExited)

  # --- 3. A writes THROUGH its mount -> B reads it THROUGH its mount, byte-for-byte ----------
  Write-Host "==> [A->B] write a file through A's mount; read it through B's mount"
  $v1 = [System.Text.Encoding]::UTF8.GetBytes(("// v1 lamp model`n" * 50))
  WriteThroughMount (Join-Path $rootA "lamp.scad") $v1
  $v1sha = Sha256OfBytes $v1
  Check "A->B: lamp.scad appears + reads byte-for-byte through B's mount" `
        (WaitForBytes (Join-Path $rootB "lamp.scad") $v1sha)

  # --- 4. B edits THROUGH its mount -> A sees the new bytes THROUGH its mount ----------------
  Write-Host "==> [B->A] edit the file through B's mount; A's mount sees the new bytes"
  $v2 = [System.Text.Encoding]::UTF8.GetBytes(("// v2 lamp widened base`n" * 80))
  WriteThroughMount (Join-Path $rootB "lamp.scad") $v2
  $v2sha = Sha256OfBytes $v2
  Check "B->A: A's mount sees B's edited bytes byte-for-byte" `
        (WaitForBytes (Join-Path $rootA "lamp.scad") $v2sha)

  # --- 5a. New file + nested dir (A -> B) ----------------------------------------------------
  Write-Host "==> [A->B] new file in a nested dir through A's mount"
  New-Item -ItemType Directory -Path (Join-Path $rootA "Docs\2026") -Force -EA SilentlyContinue | Out-Null
  $nested = [System.Text.Encoding]::UTF8.GetBytes("Q3 plan: ship sync`n")
  WriteThroughMount (Join-Path $rootA "Docs\2026\plan.txt") $nested
  Check "A->B: nested Docs/2026/plan.txt appears + reads through B's mount" `
        (WaitForBytes (Join-Path $rootB "Docs\2026\plan.txt") (Sha256OfBytes $nested))

  # --- 5b. Multi-chunk file (A -> B): > the FastCDC chunk size, so it spans many chunks ------
  Write-Host "==> [A->B] multi-chunk (1 MiB) file through A's mount"
  $big = New-Object byte[] 1048576
  for ($k = 0; $k -lt $big.Length; $k++) { $big[$k] = [byte](($k * 167 + 13) % 256) }
  WriteThroughMount (Join-Path $rootA "model.stl") $big
  Check "A->B: 1 MiB multi-chunk model.stl reads byte-for-byte through B's mount" `
        (WaitForBytes (Join-Path $rootB "model.stl") (Sha256OfBytes $big) 150)

  # --- 5c. Delete (A -> B) ------------------------------------------------------------------
  Write-Host "==> [A->B] delete a file through A's mount"
  $scratch = [System.Text.Encoding]::UTF8.GetBytes("temporary")
  WriteThroughMount (Join-Path $rootA "scratch.txt") $scratch
  Check "A->B: scratch.txt first appears through B's mount" `
        (WaitForBytes (Join-Path $rootB "scratch.txt") (Sha256OfBytes $scratch))
  Remove-Item -Path (Join-Path $rootA "scratch.txt") -Force -EA SilentlyContinue
  Check "A->B: delete converges (scratch.txt gone from B's mount)" `
        (WaitForAbsent (Join-Path $rootB "scratch.txt"))

  # --- 5d. Rename (B -> A) ------------------------------------------------------------------
  Write-Host "==> [B->A] rename a file through B's mount"
  $draft = [System.Text.Encoding]::UTF8.GetBytes("keep me")
  WriteThroughMount (Join-Path $rootB "draft.txt") $draft
  Check "B->A: draft.txt first appears through A's mount" `
        (WaitForBytes (Join-Path $rootA "draft.txt") (Sha256OfBytes $draft))
  Rename-Item -Path (Join-Path $rootB "draft.txt") -NewName "final.txt" -EA SilentlyContinue
  Check "B->A: renamed final.txt appears through A's mount, content preserved" `
        (WaitForBytes (Join-Path $rootA "final.txt") (Sha256OfBytes $draft))
  Check "B->A: old name draft.txt gone from A's mount" `
        (WaitForAbsent (Join-Path $rootA "draft.txt"))
}
catch {
  if ("$($_.Exception.Message)" -eq "CFAPI_SESSION0_BLOCKED") {
    Write-Host "BLOCKED: a write through the mount failed with 0x8007017C — the Windows runner is NOT in an interactive desktop session (Session 0)." -ForegroundColor Yellow
    Write-Host "         Cloud Filter can't transfer under the service account. Run the runner interactively to exercise this leg."
    StopBoth
    exit 0
  }
  throw
}
finally {
  # --- 6. Graceful stop of BOTH daemons (sentinel) so final pushes complete -----------------
  Write-Host "==> Stopping both daemons gracefully"
  StopDevice $procA $dbA $logA
  StopDevice $procB $dbB $logB
  Start-Sleep -Seconds 2
  foreach ($pair in @(@($logA, "A"), @($logB, "B"))) {
    $log = $pair[0]; $who = $pair[1]
    Write-Host "==> maker-cfsyncd[$who] stdout ($log):"
    if (Test-Path $log) { Get-Content $log | Out-Host } else { Write-Host "  (no stdout captured)" }
    if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) {
      Write-Host "==> maker-cfsyncd[$who] stderr:"; Get-Content "$log.err" | Out-Host
    }
  }
}

# --- Verdict --------------------------------------------------------------------------------
if ($failures.Count -gt 0) {
  Write-Host "`nTWO-DEVICE CfAPI E2E FAILED: $($failures -join '; ')" -ForegroundColor Red
  exit 1
}
Write-Host "`nTWO-DEVICE CfAPI E2E PASSED - file written through one real mount read byte-for-byte through the other (A<->B), incl. new/nested/delete/rename/multi-chunk" -ForegroundColor Green
exit 0
