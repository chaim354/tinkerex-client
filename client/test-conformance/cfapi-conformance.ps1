# Live Windows CfAPI conformance - the rigorous round-trip (the CfAPI analogue of
# `mount-conformance`). Where `cfapi-smoke.ps1` proves the write path works at all (one of
# each op), this stresses it: MANY files of varied sizes (incl. > the FastCDC chunk size, so
# multi-chunk + dedup), NESTED directories, and rapid create / edit / delete / rename churn -
# then asserts the surviving tree converged on the server BYTE-FOR-BYTE by hydrating every file
# on a FRESH device (`verify_tree`) and comparing SHA-256s against what we wrote.
#
# Usage:
#   ./client/test-conformance/cfapi-conformance.ps1 [-Server http://192.168.1.95] [-Files 24]
# Requires: rust toolchain, WinFsp + LLVM (sync-engine pulls fs-winfsp), network to $Server
# and to the chunk store (SeaweedFS) for hydration. Exits non-zero on any mismatch.
param(
  [string]$Server = $(if ($env:SERVER_URL) { $env:SERVER_URL } else { "http://192.168.1.95" }),
  [int]$Files = 24
)
$ErrorActionPreference = "Stop"
$failures = @()
function Check([string]$name, [bool]$ok) {
  if ($ok) { Write-Host "  PASS  $name" -ForegroundColor Green }
  else { Write-Host "  FAIL  $name" -ForegroundColor Red; $script:failures += $name }
}
function Sha256OfBytes([byte[]]$bytes) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString("x2") }) -join ""
}

$repo   = (Resolve-Path "$PSScriptRoot\..\..").Path
$engine = "$repo\client\sync-engine\Cargo.toml"
$runid  = [System.Guid]::NewGuid().ToString("N").Substring(0, 8)
$root   = "$env:USERPROFILE\MakerCF-conf-$runid"
$db     = "$env:TEMP\maker-cf-conf-$runid.db"
$chunks = "$env:TEMP\maker-cf-conf-$runid-chunks"
$log    = "$env:TEMP\cfapi-conf-$runid.log"

Write-Host "== CfAPI conformance == server=$Server root=$root run=$runid files=$Files"

# Kill leftover daemons from prior runs first (a zombie maker-cfsyncd keeps a live new-file
# watcher that can pollute results); then sweep leftover roots.
Get-Process maker-cfsyncd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-ChildItem "$env:USERPROFILE\MakerCF-conf-*" -Directory -ErrorAction SilentlyContinue |
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "==> Building maker-cfsyncd + examples (mac_host, verify_tree)"
& cargo build --manifest-path $engine --bin maker-cfsyncd --example mac_host --example verify_tree
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$cfsyncd = "$repo\client\sync-engine\target\debug\maker-cfsyncd.exe"

# --- 1. Seed a fresh library + mint a device token (one seed file so enumeration is non-empty) -
Write-Host "==> Seeding library via mac_host"
$env:SERVER_URL = $Server
$env:EMAIL = "conf-$runid@example.com"
$seed = & cargo run --quiet --manifest-path $engine --example mac_host 2>&1
$tokenLine = $seed | Select-String -Pattern '^WINDOWS_DEVICE_TOKEN=(.+)$'
if (-not $tokenLine) { $seed | Out-Host; throw "could not parse WINDOWS_DEVICE_TOKEN from mac_host output" }
$token = $tokenLine.Matches[0].Groups[1].Value.Trim()
Write-Host "  got device token ($($token.Length) chars)"

# verify-tree on a FRESH device: emits 'file  <path>\t<sha256>' / 'dir   <path>' per node.
function VerifyTree { & cargo run --quiet --manifest-path $engine --example verify_tree -- $Server $token 2>$null }

# --- 2. Mount the CfAPI sync root (background) ------------------------------------------------
Write-Host "==> Mounting CfAPI sync root (maker-cfsyncd) -> $log"
$proc = Start-Process -FilePath $cfsyncd -ArgumentList @($root, $db, $chunks, $Server, $token) `
  -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 6   # register + pull the snapshot

# $expect: library-relative forward-slash path -> expected sha256 (or $null once deleted).
$expect = @{}
try {
  # --- 3. Build a rigorous workload THROUGH the mount --------------------------------------
  # Varied sizes incl. >chunk-size (FastCDC avg is a few KiB, so 256 KiB+ spans many chunks),
  # nested dirs, and binary + text content. We write deterministic pseudo-random bytes so the
  # hash is reproducible and content-addressing/dedup is genuinely exercised.
  Write-Host "==> Writing $Files files across nested dirs (varied sizes)"
  $sizes = @(0, 1, 100, 4096, 65536, 262144, 1048576)   # incl. empty, sub-chunk, multi-chunk
  New-Item -ItemType Directory -Path (Join-Path $root "sub") -Force -EA SilentlyContinue | Out-Null
  New-Item -ItemType Directory -Path (Join-Path $root "sub\deep") -Force -EA SilentlyContinue | Out-Null
  for ($i = 0; $i -lt $Files; $i++) {
    $dir = switch ($i % 3) { 0 { $root } 1 { Join-Path $root "sub" } 2 { Join-Path $root "sub\deep" } }
    $rel = switch ($i % 3) { 0 { "" } 1 { "sub/" } 2 { "sub/deep/" } }
    $name = "f$i.bin"
    $size = $sizes[$i % $sizes.Count]
    # Deterministic bytes: byte k = (i*131 + k*167) mod 256. Reproducible + non-trivial.
    $bytes = New-Object byte[] $size
    for ($k = 0; $k -lt $size; $k++) { $bytes[$k] = [byte](($i * 131 + $k * 167) % 256) }
    [System.IO.File]::WriteAllBytes((Join-Path $dir $name), $bytes)
    $expect["$rel$name"] = Sha256OfBytes $bytes
  }
  Start-Sleep -Seconds 12  # let the watcher + sync loop capture + push the creates

  # --- 4. Churn: edit some, delete some, rename some ---------------------------------------
  Write-Host "==> Churn: edit / delete / rename"
  # Edit f0 (root) - append, recompute its expected hash.
  $f0 = Join-Path $root "f0.bin"
  if (Test-Path $f0) {
    $orig = [System.IO.File]::ReadAllBytes($f0)
    $extra = [System.Text.Encoding]::ASCII.GetBytes("edit-$runid")
    $new = New-Object byte[] ($orig.Length + $extra.Length)
    [Array]::Copy($orig, $new, $orig.Length); [Array]::Copy($extra, 0, $new, $orig.Length, $extra.Length)
    [System.IO.File]::WriteAllBytes($f0, $new)
    $expect["f0.bin"] = Sha256OfBytes $new
  }
  # Delete f3 (sub/f3.bin) - drop it from expectations.
  $del = Join-Path $root "sub\f3.bin"
  if (Test-Path $del) { Remove-Item $del -Force -EA SilentlyContinue; $expect.Remove("sub/f3.bin") }
  # Rename f1 (sub/f1.bin) -> sub/renamed-$runid.bin - move its expected hash to the new key.
  $rsrc = Join-Path $root "sub\f1.bin"
  if (Test-Path $rsrc) {
    Rename-Item $rsrc -NewName "renamed-$runid.bin" -EA SilentlyContinue
    $expect["sub/renamed-$runid.bin"] = $expect["sub/f1.bin"]; $expect.Remove("sub/f1.bin")
  }
  # Settle generously before unmount: CfAPI delivers its NOTIFY callbacks lazily, so give the
  # churn (edit/delete/rename) time to fire + push during normal sync (the patient drain backstops).
  Start-Sleep -Seconds 25  # let the churn capture + push
}
finally {
  # --- 5. Graceful stop (sentinel) so the final drain completes ----------------------------
  if ($proc -and -not $proc.HasExited) {
    New-Item -ItemType File -Path "$db.stop" -Force -EA SilentlyContinue | Out-Null
    if (-not $proc.WaitForExit(40000)) {
      Write-Host "  daemon did not exit on the stop sentinel within 40s - forcing"
      Stop-Process -Id $proc.Id -Force -EA SilentlyContinue
    }
  }
  Start-Sleep -Seconds 2
  Write-Host "==> maker-cfsyncd stdout ($log):"
  if (Test-Path $log) { Get-Content $log | Out-Host } else { Write-Host "  (no stdout captured)" }
  if ((Test-Path "$log.err") -and (Get-Item "$log.err").Length -gt 0) {
    Write-Host "==> maker-cfsyncd stderr:"; Get-Content "$log.err" | Out-Host
  }
}

# --- 6. Verify the server tree byte-for-byte on a fresh device --------------------------------
Write-Host "==> Verifying server tree (hydrate + hash on a fresh device)"
$rows = VerifyTree
$actual = @{}   # path -> sha256 for files the server has
foreach ($line in $rows) {
  if ($line -match '^file\s+(.+?)\t([0-9a-f<].*)$') { $actual[$Matches[1]] = $Matches[2] }
}
Write-Host "  server has $($actual.Count) file(s); expected $($expect.Count)"

# Every expected file must be present on the server with the exact bytes we wrote.
foreach ($path in ($expect.Keys | Sort-Object)) {
  $want = $expect[$path]
  $got  = $actual[$path]
  Check "present + byte-exact: $path" ($got -eq $want)
  if ($got -ne $want -and $got) { Write-Host "      want=$want got=$got" -ForegroundColor DarkYellow }
}
# The deleted file must be gone; the renamed source must be gone.
Check "deleted file absent: sub/f3.bin"            (-not $actual.ContainsKey("sub/f3.bin"))
Check "renamed source absent: sub/f1.bin"          (-not $actual.ContainsKey("sub/f1.bin"))

# --- Verdict ---------------------------------------------------------------------------------
if ($failures.Count -gt 0) {
  Write-Host "`nCONFORMANCE FAILED: $($failures.Count) check(s) failed" -ForegroundColor Red
  Write-Host "daemon log: $log (+ .err)"
  exit 1
}
Write-Host "`nCONFORMANCE PASSED - $($expect.Count) files across nested dirs, varied sizes, churn; byte-for-byte on a fresh device" -ForegroundColor Green
exit 0
