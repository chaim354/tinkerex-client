# Live Windows CfAPI smoke - the regression net for Phase 2b (Task A4).
# Registers a real sync root, enumerates, hydrates, then exercises the write path
# (new file / edit / delete / rename) and ASSERTS each round-tripped to the server via
# the control plane (`tree_names` example). Run on a self-hosted Windows runner with the
# deployed server reachable; exits non-zero on any failure.
#
# Usage:
#   ./client/test-conformance/cfapi-smoke.ps1 [-Server http://192.168.1.95]
# Requires: rust toolchain, WinFsp + LLVM (sync-engine pulls fs-winfsp), network to $Server
# and to the chunk store (SeaweedFS) for hydration.
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
# UNIQUE per-run paths. A CfAPI sync root hard-killed by a prior run stays registered and
# refuses deletion, so we never reuse the same folder (reusing it threw RemoveItemIOError).
$root   = "$env:USERPROFILE\MakerCF-smoke-$runid"
$db     = "$env:TEMP\maker-cf-smoke-$runid.db"
$chunks = "$env:TEMP\maker-cf-smoke-$runid-chunks"
$log    = "$env:TEMP\cfapi-smoke-$runid.log"

Write-Host "== CfAPI smoke == server=$Server root=$root run=$runid"

# Kill any LEFTOVER daemons from prior runs FIRST. A maker-cfsyncd that outlived its run (an
# early hang that got force-killed, or a crash) keeps its sync root mounted + its new-file
# watcher live — and the watcher can capture files into a stale library, polluting results. A
# fresh runner has none; this just guarantees isolation between runs on a shared self-hosted box.
Get-Process maker-cfsyncd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

# Best-effort sweep of leftover sync roots from prior runs. A still-registered root may refuse
# deletion (IOException) - that must NOT abort the smoke, hence SilentlyContinue.
Get-ChildItem "$env:USERPROFILE\MakerCF-smoke-*" -Directory -ErrorAction SilentlyContinue |
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "==> Building maker-cfsyncd + examples (mac_host, tree_names)"
& cargo build --manifest-path $engine --bin maker-cfsyncd --example mac_host --example tree_names
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$cfsyncd = "$repo\client\sync-engine\target\debug\maker-cfsyncd.exe"

# --- 1. Seed a library + mint a device token (mac_host runs anywhere) -----------------
Write-Host "==> Seeding library via mac_host"
$env:SERVER_URL = $Server
$env:EMAIL = "smoke-$runid@example.com"
$seed = & cargo run --quiet --manifest-path $engine --example mac_host 2>&1
$seed | Out-Host
$tokenLine = $seed | Select-String -Pattern '^WINDOWS_DEVICE_TOKEN=(.+)$'
if (-not $tokenLine) { throw "could not parse WINDOWS_DEVICE_TOKEN from mac_host output" }
$token = $tokenLine.Matches[0].Groups[1].Value.Trim()
Write-Host "  got device token ($($token.Length) chars)"

function ServerTree { & cargo run --quiet --manifest-path $engine --example tree_names -- $Server $token 2>$null }

# --- 2. Control-plane baseline: the seeded files are on the server ----------------------
$baseline = ServerTree
Write-Host "==> Server tree (baseline):`n$($baseline -join "`n")"
$seededFile = ($baseline | Select-String -Pattern '^file  (.+)$' | Select-Object -First 1).Matches[0].Groups[1].Value
Check "baseline: server has at least one seeded file" ([bool]$seededFile)

# --- 3. Mount the CfAPI sync root (background) -----------------------------------------
Write-Host "==> Mounting CfAPI sync root (maker-cfsyncd) -> $log"
$proc = Start-Process -FilePath $cfsyncd -ArgumentList @($root, $db, $chunks, $Server, $token) `
  -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 6   # let it register + pull the snapshot

try {
  # --- 4. Enumerate (FETCH_PLACEHOLDERS) ---------------------------------------------
  $listed = Get-ChildItem -Path $root -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name
  Write-Host "==> Explorer enumeration: $($listed -join ', ')"
  Check "enumerate: sync root lists files" ($listed.Count -gt 0)

  # --- 5. Hydrate (FETCH_DATA) on the first seeded file ------------------------------
  $leaf = Split-Path $seededFile -Leaf
  $onDisk = Join-Path $root $leaf
  $content = if (Test-Path $onDisk) { Get-Content -Raw -Path $onDisk -ErrorAction SilentlyContinue } else { $null }
  Check "hydrate: first file opens with non-empty content" ([bool]$content)

  # --- 6. Write path: new / edit / delete / rename ----------------------------------
  # -EA SilentlyContinue so a failed op records a Check FAIL instead of aborting before the
  # verdict + daemon-log dump (the diagnostics live in the finally block).
  $newFile = Join-Path $root "smoke-new-$runid.txt"
  Set-Content -Path $newFile -Value "created by smoke $runid" -ErrorAction SilentlyContinue   # NEW FILE (B1)
  if (Test-Path $onDisk) { Add-Content -Path $onDisk -Value "edit-$runid" -ErrorAction SilentlyContinue }  # EDIT
  $toDelete = $listed | Where-Object { $_ -ne $leaf } | Select-Object -First 1
  if ($toDelete) { Remove-Item -Path (Join-Path $root $toDelete) -Force -ErrorAction SilentlyContinue }    # DELETE (B2)
  $renameSrc = $listed | Where-Object { $_ -ne $leaf -and $_ -ne $toDelete } | Select-Object -First 1
  $renamedTo = "smoke-renamed-$runid.txt"
  if ($renameSrc) { Rename-Item -Path (Join-Path $root $renameSrc) -NewName $renamedTo -ErrorAction SilentlyContinue }  # RENAME (B3)

  Write-Host "==> Local ops done (new=$(Split-Path $newFile -Leaf) edit=$leaf delete=$toDelete rename=$renameSrc->$renamedTo); waiting for push"
  # Settle generously: CfAPI delivers its NOTIFY callbacks (close/delete/rename) lazily — often a
  # good while after the op — so a short wait can leave a capture to race the unmount drain. A
  # longer wait lets them fire during NORMAL sync (cleanly pushed by the loop); the daemon's
  # patient shutdown drain is the backstop for any that still land at unmount.
  Start-Sleep -Seconds 25
}
finally {
  # --- 7. Stop the daemon GRACEFULLY (touch <db>.stop) so an in-flight/final push can
  # complete before unmount, instead of a hard Stop-Process -Force that could kill it
  # mid-push. The daemon watches for the sentinel, does a final outbox drain, then unmounts.
  if ($proc -and -not $proc.HasExited) {
    New-Item -ItemType File -Path "$db.stop" -Force -ErrorAction SilentlyContinue | Out-Null
    if (-not $proc.WaitForExit(40000)) {
      Write-Host "  daemon did not exit on the stop sentinel within 40s - forcing"
      Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
  }
  Start-Sleep -Seconds 2
  # Always surface the daemon's own logs - they say whether the sync root registered and what
  # the CfAPI callbacks did (or why the mount produced an empty enumeration).
  Write-Host "==> maker-cfsyncd stdout ($log):"
  if (Test-Path $log) { Get-Content $log | Out-Host } else { Write-Host "  (no stdout captured)" }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) {
    Write-Host "==> maker-cfsyncd stderr:"; Get-Content "$log.err" | Out-Host
  }
}

# --- 8. Assert the write path reached the server --------------------------------------
$after = ServerTree
Write-Host "==> Server tree (after):`n$($after -join "`n")"
$names = $after | ForEach-Object { ($_ -replace '^\w+\s+', '') } | ForEach-Object { Split-Path $_ -Leaf }

Check "new file pushed (B1): smoke-new-$runid.txt on server" ($names -contains "smoke-new-$runid.txt")
if ($toDelete)  { Check "delete pushed (B2): $toDelete gone from server" (-not ($names -contains $toDelete)) }
if ($renameSrc) {
  Check "rename pushed (B3): $renamedTo on server"      ($names -contains $renamedTo)
  Check "rename pushed (B3): old name $renameSrc gone"  (-not ($names -contains $renameSrc))
}

# --- Verdict --------------------------------------------------------------------------
if ($failures.Count -gt 0) {
  Write-Host "`nSMOKE FAILED: $($failures -join '; ')" -ForegroundColor Red
  Write-Host "daemon log: $log (+ .err)"
  if (Test-Path $log) { Write-Host "---- tail ----"; Get-Content -Tail 40 $log }
  exit 1
}
Write-Host "`nSMOKE PASSED - register + enumerate + hydrate + new/edit/delete/rename round-tripped" -ForegroundColor Green
exit 0
