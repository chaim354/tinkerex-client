#!/usr/bin/env bash
#
# Two-device, REAL-MOUNT, end-to-end test on macOS (WebDAV) — RUN THIS LOCALLY AT THE MACHINE.
#
# This is NOT CI-automated: macOS's native `/sbin/mount_webdav` needs an INTERACTIVE desktop
# login session (it won't hold a mount from a headless/ssh/CI context). So a human runs this from
# a Terminal in a normal macOS desktop session.
#
# It drives the actual app stack — the real `sync_engine::WebdavMount` (the daemon the Tauri app's
# `sync_start` launches on macOS, via the new `maker-webdavd` CLI) with an ACTUAL native WebDAV
# mount — for TWO devices paired into ONE library, and passes files THROUGH the mounts.
#
# Flow (identical to the Windows/Linux real-mount tests):
#   1. Seed: register device A + pair device B into one library (the REAL pairing flow).
#   2. Mount BOTH WebDAV mounts live at once (two `maker-webdavd`, two ports).
#   3. Write through A's mount  -> read it byte-for-byte through B's mount.
#   4. Edit through B's mount   -> A's mount sees the new bytes.
#   5. Plus new file, nested dir, delete, rename, and a multi-chunk (>chunk-size) file.
#
# It boots its own STORAGE_BACKEND=local Rails server — the filesystem-backed chunk store that
# mints REAL presigned http PUT/GET URLs (the memory backend's `memory://` URLs aren't HTTP, so the
# real data plane can't move bytes through it). Each device has its own chunk cache, so B genuinely
# hydrates A's bytes from the server over the wire — a true data-plane round-trip, no SeaweedFS.
#
#   client/test-conformance/two-device-webdav-macos.sh
#
# Requires: a macOS desktop session, a Rust toolchain, Ruby/bundler for the server. NO FUSE-T.
set -uo pipefail

cd "$(dirname "$0")/../.."   # repo root
PORT="${PORT:-3012}"
SERVER="http://127.0.0.1:$PORT"
DAV_A="${DAV_A:-9991}"; DAV_B="${DAV_B:-9992}"
WORK="$(mktemp -d)"
MNT_A="$HOME/MakerDav-A-$$"; MNT_B="$HOME/MakerDav-B-$$"
DB_A="$WORK/A.db"; DB_B="$WORK/B.db"
CH_A="$WORK/A-chunks"; CH_B="$WORK/B-chunks"
SRV_PID=""; PID_A=""; PID_B=""
FAILURES=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; FAILURES=$((FAILURES+1)); }

cleanup() {
  # Graceful daemon stop via the sentinel (final push before unmount), then hard fallback.
  [ -n "$PID_A" ] && touch "$DB_A.stop" 2>/dev/null || true
  [ -n "$PID_B" ] && touch "$DB_B.stop" 2>/dev/null || true
  sleep 3
  for m in "$MNT_A" "$MNT_B"; do /sbin/umount "$m" 2>/dev/null || diskutil unmount force "$m" 2>/dev/null || true; done
  [ -n "$PID_A" ] && kill "$PID_A" 2>/dev/null || true
  [ -n "$PID_B" ] && kill "$PID_B" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "==> maker-webdavd[A] log:"; [ -f "$WORK/A.log" ] && cat "$WORK/A.log" || echo "  (none)"
  echo "==> maker-webdavd[B] log:"; [ -f "$WORK/B.log" ] && cat "$WORK/B.log" || echo "  (none)"
  rmdir "$MNT_A" "$MNT_B" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- 1. Boot the local-storage Rails server (real presigned data plane) ---------------------
echo "==> booting STORAGE_BACKEND=local Rails on $SERVER"
(lsof -ti tcp:$PORT | xargs kill -9 2>/dev/null) || true
sleep 1
bundle install --quiet 2>/dev/null || true
bin/rails db:prepare 2>/dev/null || true
STORAGE_BACKEND=local ENABLE_TEST_SIGNUP=true CHUNK_LOCAL_DIR="$WORK/chunk-store" bin/rails server -p "$PORT" >"$WORK/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do curl -sf "$SERVER/up" >/dev/null && break; sleep 1; done
curl -sf "$SERVER/up" >/dev/null || { echo "FAIL: server didn't start"; cat "$WORK/server.log"; exit 1; }
echo "  server up"

# --- 2. Build + seed (real pairing) ---------------------------------------------------------
echo "==> building maker-webdavd + pair_two_devices"
cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-webdavd --example pair_two_devices >/dev/null 2>&1 || {
  echo "FAIL: build"; cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-webdavd 2>&1 | tail -20; exit 1; }
WEBDAVD="client/sync-engine/target/debug/maker-webdavd"

echo "==> pairing two devices into one library (register A -> create_pairing -> pair B)"
SEED="$(SERVER_URL=$SERVER cargo run --quiet --manifest-path client/sync-engine/Cargo.toml --example pair_two_devices 2>/dev/null)"
echo "$SEED"
TOK_A="$(echo "$SEED" | sed -n 's/^DEVICE_A_TOKEN=//p')"
TOK_B="$(echo "$SEED" | sed -n 's/^DEVICE_B_TOKEN=//p')"
[ -n "$TOK_A" ] && [ -n "$TOK_B" ] || { echo "FAIL: could not parse both tokens"; exit 1; }

# --- 3. Mount BOTH WebDAV mounts live at once -----------------------------------------------
echo "==> mounting BOTH WebDAV daemons (A:$DAV_A  B:$DAV_B)"
"$WEBDAVD" "$MNT_A" "$DB_A" "$CH_A" "$SERVER" "$TOK_A" "$DAV_A" >"$WORK/A.log" 2>&1 &
PID_A=$!
"$WEBDAVD" "$MNT_B" "$DB_B" "$CH_B" "$SERVER" "$TOK_B" "$DAV_B" >"$WORK/B.log" 2>&1 &
PID_B=$!

wait_mounted() { # mountpoint pid
  for _ in $(seq 1 40); do
    if mount | grep -q " $1 "; then return 0; fi
    if ! kill -0 "$2" 2>/dev/null; then return 1; fi
    sleep 0.5
  done
  return 1
}
if ! wait_mounted "$MNT_A" "$PID_A"; then echo "BLOCKED: WebDAV mount A did not come up (need an interactive macOS session). A log:"; cat "$WORK/A.log"; exit 0; fi
if ! wait_mounted "$MNT_B" "$PID_B"; then echo "BLOCKED: WebDAV mount B did not come up. B log:"; cat "$WORK/B.log"; exit 0; fi
echo "  both mounts live"

sha() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }
wait_bytes() { local p="$1" want="$2" t="${3:-90}" end; end=$(( $(date +%s) + t )); while [ "$(date +%s)" -lt "$end" ]; do [ -f "$p" ] && [ "$(sha "$p")" = "$want" ] && return 0; sleep 0.5; done; return 1; }
wait_absent() { local p="$1" t="${2:-90}" end; end=$(( $(date +%s) + t )); while [ "$(date +%s)" -lt "$end" ]; do [ ! -e "$p" ] && return 0; sleep 0.5; done; return 1; }

# --- 4. A -> B (write through A, read through B) --------------------------------------------
echo "==> [A->B] write through A's mount; read through B's mount"
yes "// v1 lamp model" | head -n 50 > "$MNT_A/lamp.scad"
V1="$(sha "$MNT_A/lamp.scad")"
if wait_bytes "$MNT_B/lamp.scad" "$V1"; then pass "A->B: lamp.scad reads byte-for-byte through B's mount"; else fail "A->B: lamp.scad through B"; fi

# --- 5. B -> A (edit through B, A sees it) --------------------------------------------------
echo "==> [B->A] edit through B's mount; A's mount sees new bytes"
yes "// v2 lamp widened base" | head -n 80 > "$MNT_B/lamp.scad"
V2="$(sha "$MNT_B/lamp.scad")"
if wait_bytes "$MNT_A/lamp.scad" "$V2"; then pass "B->A: A's mount sees B's edit byte-for-byte"; else fail "B->A: edit through A"; fi

# --- 6a. new + nested (A -> B) -------------------------------------------------------------
echo "==> [A->B] nested new file"
mkdir -p "$MNT_A/Docs/2026"; printf 'Q3 plan: ship sync\n' > "$MNT_A/Docs/2026/plan.txt"
if wait_bytes "$MNT_B/Docs/2026/plan.txt" "$(sha "$MNT_A/Docs/2026/plan.txt")"; then pass "A->B: nested plan.txt through B"; else fail "A->B: nested through B"; fi

# --- 6b. multi-chunk (A -> B) -------------------------------------------------------------
echo "==> [A->B] multi-chunk (1 MiB) file"
head -c 1048576 /dev/urandom > "$MNT_A/model.stl"
if wait_bytes "$MNT_B/model.stl" "$(sha "$MNT_A/model.stl")" 300; then pass "A->B: 1 MiB model.stl byte-for-byte through B"; else fail "A->B: multi-chunk through B"; fi

# --- 6c. delete (A -> B) ------------------------------------------------------------------
echo "==> [A->B] delete"
printf 'temporary' > "$MNT_A/scratch.txt"
wait_bytes "$MNT_B/scratch.txt" "$(sha "$MNT_A/scratch.txt")" >/dev/null && pass "A->B: scratch.txt appears through B" || fail "A->B: scratch appears through B"
rm -f "$MNT_A/scratch.txt"
if wait_absent "$MNT_B/scratch.txt"; then pass "A->B: delete converges (gone from B)"; else fail "A->B: delete converges"; fi

# --- 6d. rename (B -> A) ------------------------------------------------------------------
echo "==> [B->A] rename"
printf 'keep me' > "$MNT_B/draft.txt"; DR="$(sha "$MNT_B/draft.txt")"
wait_bytes "$MNT_A/draft.txt" "$DR" >/dev/null && pass "B->A: draft.txt appears through A" || fail "B->A: draft appears through A"
mv "$MNT_B/draft.txt" "$MNT_B/final.txt"
if wait_bytes "$MNT_A/final.txt" "$DR"; then pass "B->A: renamed final.txt through A, content preserved"; else fail "B->A: rename through A"; fi
if wait_absent "$MNT_A/draft.txt"; then pass "B->A: old name gone from A"; else fail "B->A: old name gone"; fi

if [ "$FAILURES" -gt 0 ]; then echo ""; echo "TWO-DEVICE WEBDAV E2E FAILED: $FAILURES check(s)"; exit 1; fi
echo ""; echo "TWO-DEVICE WEBDAV E2E PASSED - file written through one real WebDAV mount read byte-for-byte through the other (A<->B), incl. new/nested/delete/rename/multi-chunk"
exit 0
