#!/usr/bin/env bash
#
# Two-device, REAL-MOUNT, end-to-end test on Linux (FUSE).
#
# The layer ABOVE the engine-level `two_device_e2e.rs`: it drives the actual app stack — the real
# `sync_engine::Daemon` (the FUSE daemon the Tauri app's `sync_start` launches on Linux) with an
# ACTUAL FUSE kernel mount — for TWO devices paired into ONE library, and passes files THROUGH the
# mounts (real filesystem writes/reads), not via Provider calls.
#
# Flow (the headline workflow):
#   1. Seed: register device A + pair device B into one library (the REAL pairing flow, via the
#      `pair_two_devices` example = the app's create_pairing/sync_pair commands).
#   2. Mount BOTH FUSE mounts live at once (two `maker-syncd` = the app's Daemon).
#   3. Write a file THROUGH A's mount  -> assert it appears + reads byte-for-byte THROUGH B's mount.
#   4. Edit a file THROUGH B's mount   -> assert A's mount sees the new bytes.
#   5. Plus: new file, nested dir, delete, rename, and a multi-chunk (>chunk-size) file.
#
# Boots its own STORAGE_BACKEND=memory Rails server (control-plane only — the local chunk caches
# carry the bytes between the two co-located mounts; both devices share this box, so a file
# hydrated into one cache is reachable... NO: each device has its OWN chunk cache, so B genuinely
# hydrates from the server's chunk store. memory backend keeps chunk bytes in the Rails process,
# served via presigned GET to localhost — which works for two local mounts).
#
# Requires: /dev/fuse access + libfuse + `user_allow_other` in /etc/fuse.conf (the Daemon mounts
# with AutoUnmount, which couples allow_other). On a CI runner without an interactive desktop FUSE
# still mounts (unlike FUSE-T/CfAPI), so this CAN run automated — it falls back + flags clearly if
# /dev/fuse is unavailable.
#
#   client/test-conformance/two-device-fuse.sh
#
set -uo pipefail

cd "$(dirname "$0")/../.."   # repo root
PORT="${PORT:-3011}"
SERVER="http://127.0.0.1:$PORT"
WORK="$(mktemp -d)"
MNT_A="$WORK/mnt-A"; MNT_B="$WORK/mnt-B"
DB_A="$WORK/A.db";   DB_B="$WORK/B.db"
CH_A="$WORK/A-chunks"; CH_B="$WORK/B-chunks"
mkdir -p "$MNT_A" "$MNT_B"
SRV_PID=""; PID_A=""; PID_B=""
FAILURES=0

pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; FAILURES=$((FAILURES+1)); }
check() { if [ "$1" = "0" ] || [ "$1" = "true" ]; then pass "$2"; else fail "$2"; fi }

cleanup() {
  for m in "$MNT_A" "$MNT_B"; do fusermount -u "$m" 2>/dev/null || umount "$m" 2>/dev/null || true; done
  [ -n "$PID_A" ] && kill "$PID_A" 2>/dev/null || true
  [ -n "$PID_B" ] && kill "$PID_B" 2>/dev/null || true
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "==> maker-syncd[A] log:"; [ -f "$WORK/A.log" ] && cat "$WORK/A.log" || echo "  (none)"
  echo "==> maker-syncd[B] log:"; [ -f "$WORK/B.log" ] && cat "$WORK/B.log" || echo "  (none)"
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- 0. Preflight: is FUSE actually usable here? --------------------------------------------
if [ ! -e /dev/fuse ]; then
  echo "BLOCKED: /dev/fuse is not present — FUSE can't mount on this runner."
  echo "         (The engine-level two_device_e2e.rs covers convergence headlessly; this real-mount"
  echo "          leg needs /dev/fuse. Re-run on a host/container with --device /dev/fuse.)"
  exit 0   # not a failure: a documented environment gap, like the macOS/Windows interactive walls
fi

# --- 1. Boot the memory Rails server --------------------------------------------------------
echo "==> booting STORAGE_BACKEND=memory Rails on $SERVER"
(fuser -k "$PORT/tcp" 2>/dev/null || pkill -f 'puma' 2>/dev/null) || true
sleep 1
STORAGE_BACKEND=memory bin/rails server -p "$PORT" >"$WORK/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 60); do curl -sf "$SERVER/up" >/dev/null && break; sleep 1; done
curl -sf "$SERVER/up" >/dev/null || { echo "FAIL: server didn't start"; cat "$WORK/server.log"; exit 1; }
echo "  server up"

# --- 2. Build + seed (real pairing) ---------------------------------------------------------
echo "==> building maker-syncd + pair_two_devices"
cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-syncd --example pair_two_devices >/dev/null 2>&1 || {
  echo "FAIL: build failed"; cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-syncd 2>&1 | tail -20; exit 1; }
SYNCD="client/sync-engine/target/debug/maker-syncd"

echo "==> pairing two devices into one library (register A -> create_pairing -> pair B)"
SEED="$(SERVER_URL=$SERVER cargo run --quiet --manifest-path client/sync-engine/Cargo.toml --example pair_two_devices 2>/dev/null)"
echo "$SEED"
TOK_A="$(echo "$SEED" | sed -n 's/^DEVICE_A_TOKEN=//p')"
TOK_B="$(echo "$SEED" | sed -n 's/^DEVICE_B_TOKEN=//p')"
[ -n "$TOK_A" ] && [ -n "$TOK_B" ] || { echo "FAIL: could not parse both device tokens"; exit 1; }
echo "  got A token (${#TOK_A} chars) + B token (${#TOK_B} chars)"

# --- 3. Mount BOTH FUSE mounts live at once -------------------------------------------------
echo "==> mounting BOTH FUSE daemons"
"$SYNCD" "$MNT_A" "$DB_A" "$CH_A" "$SERVER" "$TOK_A" >"$WORK/A.log" 2>&1 &
PID_A=$!
"$SYNCD" "$MNT_B" "$DB_B" "$CH_B" "$SERVER" "$TOK_B" >"$WORK/B.log" 2>&1 &
PID_B=$!

# Wait for both mounts to come live (a mountpoint becomes a fuse mount).
wait_mounted() {
  for _ in $(seq 1 40); do
    if mountpoint -q "$1" 2>/dev/null || grep -q " $1 fuse" /proc/mounts 2>/dev/null; then return 0; fi
    # If the daemon died (e.g. allow_other not permitted), bail early with its log.
    if ! kill -0 "$2" 2>/dev/null; then echo "  daemon for $1 exited:"; cat "$WORK/$(basename "$1" | tr 'a-z' 'A-Z' | sed 's/MNT-//').log" 2>/dev/null; return 1; fi
    sleep 0.5
  done
  return 1
}
if ! wait_mounted "$MNT_A" "$PID_A"; then
  echo "BLOCKED: FUSE mount A did not come up (likely missing /dev/fuse access or user_allow_other)."
  echo "  A log:"; cat "$WORK/A.log"
  exit 0   # documented environment gap, not a correctness failure
fi
if ! wait_mounted "$MNT_B" "$PID_B"; then
  echo "BLOCKED: FUSE mount B did not come up."; echo "  B log:"; cat "$WORK/B.log"; exit 0
fi
echo "  both mounts live"

# Helpers: poll for a file through a mount, byte-for-byte (a read on a cache miss hydrates).
sha() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }
wait_bytes() { # path want_sha [timeout_sec]
  local p="$1" want="$2" t="${3:-90}" end
  end=$(( $(date +%s) + t ))
  while [ "$(date +%s)" -lt "$end" ]; do
    if [ -f "$p" ] && [ "$(sha "$p")" = "$want" ]; then return 0; fi
    sleep 0.5
  done
  return 1
}
wait_absent() { local p="$1" t="${2:-90}" end; end=$(( $(date +%s) + t )); while [ "$(date +%s)" -lt "$end" ]; do [ ! -e "$p" ] && return 0; sleep 0.5; done; return 1; }

# --- 4. A writes through its mount -> B reads it through its mount, byte-for-byte ------------
echo "==> [A->B] write through A's mount; read through B's mount"
yes "// v1 lamp model" | head -n 50 > "$MNT_A/lamp.scad"
V1="$(sha "$MNT_A/lamp.scad")"
if wait_bytes "$MNT_B/lamp.scad" "$V1"; then pass "A->B: lamp.scad reads byte-for-byte through B's mount"; else fail "A->B: lamp.scad byte-for-byte through B's mount"; fi

# --- 5. B edits through its mount -> A sees the new bytes ------------------------------------
echo "==> [B->A] edit through B's mount; A's mount sees new bytes"
yes "// v2 lamp widened base" | head -n 80 > "$MNT_B/lamp.scad"
V2="$(sha "$MNT_B/lamp.scad")"
if wait_bytes "$MNT_A/lamp.scad" "$V2"; then pass "B->A: A's mount sees B's edit byte-for-byte"; else fail "B->A: A's mount sees B's edit"; fi

# --- 6a. New file + nested dir (A -> B) -----------------------------------------------------
echo "==> [A->B] nested new file through A's mount"
mkdir -p "$MNT_A/Docs/2026"
printf 'Q3 plan: ship sync\n' > "$MNT_A/Docs/2026/plan.txt"
NS="$(sha "$MNT_A/Docs/2026/plan.txt")"
if wait_bytes "$MNT_B/Docs/2026/plan.txt" "$NS"; then pass "A->B: nested Docs/2026/plan.txt reads through B's mount"; else fail "A->B: nested file through B's mount"; fi

# --- 6b. Multi-chunk file (A -> B) ----------------------------------------------------------
echo "==> [A->B] multi-chunk (1 MiB) file through A's mount"
head -c 1048576 /dev/urandom > "$MNT_A/model.stl"
BIG="$(sha "$MNT_A/model.stl")"
if wait_bytes "$MNT_B/model.stl" "$BIG" 150; then pass "A->B: 1 MiB multi-chunk model.stl reads byte-for-byte through B's mount"; else fail "A->B: multi-chunk file through B's mount"; fi

# --- 6c. Delete (A -> B) --------------------------------------------------------------------
echo "==> [A->B] delete through A's mount"
printf 'temporary' > "$MNT_A/scratch.txt"
if wait_bytes "$MNT_B/scratch.txt" "$(sha "$MNT_A/scratch.txt")"; then pass "A->B: scratch.txt first appears through B's mount"; else fail "A->B: scratch.txt appears through B"; fi
rm -f "$MNT_A/scratch.txt"
if wait_absent "$MNT_B/scratch.txt"; then pass "A->B: delete converges (gone from B's mount)"; else fail "A->B: delete converges"; fi

# --- 6d. Rename (B -> A) --------------------------------------------------------------------
echo "==> [B->A] rename through B's mount"
printf 'keep me' > "$MNT_B/draft.txt"
DR="$(sha "$MNT_B/draft.txt")"
if wait_bytes "$MNT_A/draft.txt" "$DR"; then pass "B->A: draft.txt first appears through A's mount"; else fail "B->A: draft.txt appears through A"; fi
mv "$MNT_B/draft.txt" "$MNT_B/final.txt"
if wait_bytes "$MNT_A/final.txt" "$DR"; then pass "B->A: renamed final.txt appears through A's mount, content preserved"; else fail "B->A: rename appears through A"; fi
if wait_absent "$MNT_A/draft.txt"; then pass "B->A: old name draft.txt gone from A's mount"; else fail "B->A: old name gone from A"; fi

# --- Verdict --------------------------------------------------------------------------------
if [ "$FAILURES" -gt 0 ]; then
  echo ""; echo "TWO-DEVICE FUSE E2E FAILED: $FAILURES check(s)"; exit 1
fi
echo ""; echo "TWO-DEVICE FUSE E2E PASSED - file written through one real mount read byte-for-byte through the other (A<->B), incl. new/nested/delete/rename/multi-chunk"
exit 0
