# cross-os-e2e leg 1/5: WINDOWS PUSH (CfAPI).
#
# Mounts ONE real Cloud Filter sync root with the WINDOWS device token (paired into the shared
# library by the `seed` job), writes a KNOWN tagged file with deterministic content THROUGH the
# mount, waits until the sync daemon has pushed it to the SHARED server, then unmounts.
#
# This is the single-mount, single-op counterpart of two-device-cfapi.ps1 - same mount/daemon
# patterns and the same BLOCKED self-detection (CfAPI enumerates empty / errors 0x8007017C under a
# Session-0 service account; that's an environment wall, not a failure -> exit 0 with a BLOCKED
# notice so the continue-on-error job never false-reds).
#
# Inputs (env, set by the workflow from the seed outputs):
#   SERVER_URL   - the SHARED server (e.g. http://192.168.1.95)
#   TOK_WIN      - the windows device token
#   TEST_TAG     - the per-run unique tag (filename suffix)
# Output (to $GITHUB_OUTPUT if set, else stdout): EXPECTED_SHA=<sha256 of the content written>
#
#   ./client/test-conformance/cross-os/win-push.ps1
param(
  [string]$Server = $(if ($env:SERVER_URL) { $env:SERVER_URL } else { "http://192.168.1.95" }),
  [string]$TokWin = $env:TOK_WIN,
  [string]$Tag    = $env:TEST_TAG,
  # Seconds to poll the server for the written file to LAND (finalized commit). The push is
  # async (chunk upload → finalize); allow generous WAN slack so a slow-but-OK push isn't
  # mistaken for a dropped one.
  [int]   $VerifyTimeout = $(if ($env:VERIFY_TIMEOUT) { [int]$env:VERIFY_TIMEOUT } else { 60 })
)
$ErrorActionPreference = "Stop"

if (-not $TokWin) { Write-Host "BLOCKED: no TOK_WIN (seed job did not emit a windows token)." -ForegroundColor Yellow; exit 0 }
if (-not $Tag)    { $Tag = "notag" }

$repo   = (Resolve-Path "$PSScriptRoot\..\..\..").Path
$engine = "$repo\client\sync-engine\Cargo.toml"
$runid  = [System.Guid]::NewGuid().ToString("N").Substring(0, 8)

$root   = "$env:USERPROFILE\MakerCF-xos-win-$runid"
$db     = "$env:TEMP\maker-xos-win-$runid.db"
$chunks = "$env:TEMP\maker-xos-win-$runid-chunks"
$log    = "$env:TEMP\xos-win-$runid.log"
$fname  = "crossos-$Tag.txt"

# The canonical scenario's v1 content (deterministic; the mac-pull-1 leg recomputes the same SHA).
$content = "cross-os e2e v1 (windows write) tag=$Tag`n" * 64
$bytes   = [System.Text.Encoding]::UTF8.GetBytes($content)
$sha     = ([System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes) | ForEach-Object { $_.ToString("x2") }) -join ""

Write-Host "== cross-os WIN PUSH == server=$Server tag=$Tag file=$fname run=$runid"
Write-Host "   expected sha256=$sha"

# Sweep leftover daemons + roots from any prior run (a zombie watcher pollutes results; a
# hard-killed CfAPI root stays registered, so never reuse a folder).
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
    New-Item -ItemType File -Path "$dbpath.stop" -Force -EA SilentlyContinue | Out-Null   # graceful: final push
    if (-not $proc.WaitForExit(40000)) { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue }
  }
}
function EmitOutput([string]$key, [string]$val) {
  if ($env:GITHUB_OUTPUT) { "$key=$val" | Out-File -FilePath $env:GITHUB_OUTPUT -Encoding utf8 -Append }
  Write-Host "$key=$val"
}

# Poll GET /v1/tree (Bearer device token) until the windows-written node has ACTUALLY
# landed on the SHARED server: a node named $fname with a finalized version (version_id +
# a non-empty manifest) of the right byte size. This is the anti-false-green check — a
# push whose chunk upload 403'd never finalizes a commit, so the node is absent (or
# version-less), and this returns $false. Returns the version_id on success, $null on
# timeout. (Same auth header + tree shape the sibling pull legs rely on.)
function WaitForServerLanding([string]$server, [string]$token, [string]$name, [int]$wantSize, [int]$timeoutSec) {
  $headers  = @{ Authorization = "Bearer $token" }
  $deadline = (Get-Date).AddSeconds($timeoutSec)
  $lastSeen = "<absent>"
  while ((Get-Date) -lt $deadline) {
    try {
      $afterId = 0
      while ($true) {
        $resp = Invoke-RestMethod -Method Get -Uri "$server/v1/tree?after_id=$afterId&limit=1000" `
                  -Headers $headers -TimeoutSec 10
        foreach ($n in $resp.nodes) {
          if ($n.name -eq $name) {
            $hasManifest = ($n.PSObject.Properties.Name -contains "manifest") -and ($n.manifest) -and ($n.manifest.Count -gt 0)
            $hasVersion  = ($n.PSObject.Properties.Name -contains "version_id") -and $n.version_id
            $size        = if ($n.PSObject.Properties.Name -contains "size") { [int]$n.size } else { -1 }
            $lastSeen    = "version_id=$($n.version_id) size=$size manifestChunks=$($n.manifest.Count)"
            if ($hasVersion -and $hasManifest -and $size -eq $wantSize) { return $n.version_id }
          }
        }
        if ($resp.next_after_id) { $afterId = [int]$resp.next_after_id } else { break }
      }
    } catch {
      $lastSeen = "tree query error: $($_.Exception.Message)"
    }
    Start-Sleep -Seconds 2
  }
  Write-Host "   (last server view of '$name': $lastSeen)" -ForegroundColor DarkYellow
  return $null
}

Write-Host "==> Mounting CfAPI sync root at $root (log -> $log)"
$proc = Start-Process -FilePath $cfsyncd -ArgumentList @($root, $db, $chunks, $Server, $TokWin) `
  -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 8   # register + pull the (empty) snapshot

# BLOCKED self-detection: Session-0 service account can't drive Cloud Filter.
$cloudInvalid = $false
foreach ($lg in @($log, "$log.err")) {
  if ((Test-Path $lg) -and (Select-String -Path $lg -Pattern '0x8007017C|cloud operation is invalid' -Quiet)) { $cloudInvalid = $true }
}
if ($cloudInvalid -or $proc.HasExited) {
  Write-Host "BLOCKED: CfAPI not usable (Session 0 / 0x8007017C, or daemon exited) - the Windows runner is NOT in an interactive desktop session." -ForegroundColor Yellow
  Write-Host "         Run the runner in an interactive login session (ci/runners/README.md) to exercise this leg."
  if (Test-Path $log) { Get-Content $log | Out-Host }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) { Get-Content "$log.err" | Out-Host }
  StopDaemon $proc $db
  exit 0
}

$failed = $false
$landed = $false
try {
  Write-Host "==> Writing $fname through the mount"
  try { [System.IO.File]::WriteAllBytes((Join-Path $root $fname), $bytes) }
  catch {
    if ("$($_.Exception.Message)" -match 'cloud operation is invalid|0x8007017C') {
      Write-Host "BLOCKED: write through the mount failed with 0x8007017C (Session 0)." -ForegroundColor Yellow
      StopDaemon $proc $db
      exit 0
    }
    throw
  }
  # Give the sync loop a moment to start pushing (nudge is ~tens of ms; the daemon stays
  # up for the server-landing poll below, which is the real gate).
  Write-Host "==> Giving the daemon ~5s to begin the push"
  Start-Sleep -Seconds 5

  # THE anti-false-green gate: do NOT trust that the LOCAL write succeeded. Verify the file
  # actually LANDED on the shared server (node + finalized version + manifest of the right
  # size). A chunk upload that 403'd leaves the commit unfinalized → never lands → fails
  # here instead of being silently "left queued, will retry" and reported green.
  Write-Host "==> Verifying the push LANDED on $Server (GET /v1/tree, timeout ${VerifyTimeout}s)"
  $landedVersion = WaitForServerLanding $Server $TokWin $fname $bytes.Length $VerifyTimeout
  if (-not $landedVersion) {
    $landed = $false
    Write-Host "  PUSH DID NOT LAND (chunk upload failed?) - '$fname' never finalized on $Server within ${VerifyTimeout}s." -ForegroundColor Red
  } else {
    $landed = $true
    Write-Host "  LANDED  '$fname' present on the server as version $landedVersion (size=$($bytes.Length))" -ForegroundColor Green
  }
}
catch { $failed = $true; Write-Host "  ERROR: $($_.Exception.Message)" -ForegroundColor Red }
finally {
  Write-Host "==> Stopping daemon gracefully (final push completes)"
  StopDaemon $proc $db
  Start-Sleep -Seconds 3
  if (Test-Path $log) { Get-Content $log | Out-Host }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) { Write-Host "-- stderr --"; Get-Content "$log.err" | Out-Host }
}

if ($failed) { Write-Host "WIN PUSH FAILED" -ForegroundColor Red; exit 1 }
if (-not $landed) {
  Write-Host "WIN PUSH FAILED - PUSH DID NOT LAND (chunk upload failed?)" -ForegroundColor Red
  exit 1
}
EmitOutput "EXPECTED_SHA" $sha
Write-Host "WIN PUSH OK - wrote $fname (sha=$sha) through the CfAPI mount and VERIFIED it landed on $Server (version $landedVersion)" -ForegroundColor Green
exit 0
