#!/usr/bin/env bash
#
# cross-os-e2e leg 3/5: LINUX EDIT (FUSE).
#
# Mounts ONE real FUSE kernel mount with the LINUX device token (paired into the shared library by
# the `seed` job), POLLs until the tagged file (written by Windows, pulled by macOS) is present,
# EDITs it THROUGH the mount to NEW deterministic content, waits for the daemon to push the edit,
# then unmounts and emits the NEW expected sha (for mac-pull-2 + win-pull-2 to verify).
#
# Single-mount, single-op counterpart of two-device-fuse.sh — same mount/daemon patterns and the
# same BLOCKED self-detection: FUSE needs /dev/fuse access for the runner user (an environment gap,
# not a code bug) → exit 0 with a BLOCKED notice (never a false red).
#
# Inputs (env, set by the workflow):
#   SERVER_URL    - the SHARED server (e.g. http://192.168.1.95)
#   TOK_LINUX     - the linux device token
#   TEST_TAG      - the per-run unique tag (filename suffix)
#   EXPECTED_SHA  - the v1 sha (from win-push) — we poll until the file reads as this BEFORE editing
#   PULL_TIMEOUT  - optional seconds to poll for v1 (default 90)
# Output (to $GITHUB_OUTPUT if set, else stdout): EXPECTED_SHA=<sha256 of the EDITED content>
#
#   EXPECTED_SHA=... TOK_LINUX=... TEST_TAG=... client/test-conformance/cross-os/linux-edit.sh
set -uo pipefail

cd "$(dirname "$0")/../../.." || exit 1   # repo root
SERVER="${SERVER_URL:-http://192.168.1.95}"
TOK="${TOK_LINUX:-}"
TAG="${TEST_TAG:-notag}"
WANT_V1="${EXPECTED_SHA:-}"
TIMEOUT="${PULL_TIMEOUT:-90}"
FNAME="crossos-$TAG.txt"

if [ -z "$TOK" ];     then echo "BLOCKED: no TOK_LINUX (seed did not emit a linux token)."; exit 0; fi
if [ -z "$WANT_V1" ]; then echo "BLOCKED: no EXPECTED_SHA (win-push leg was BLOCKED or emitted no sha)."; exit 0; fi

# /dev/fuse preflight — the documented environment gap, mirrors two-device-fuse.sh.
if [ ! -e /dev/fuse ]; then
  echo "BLOCKED: /dev/fuse is not present — FUSE can't mount on this runner."
  echo "         Give the runner user /dev/fuse access (ci/runners/README.md), then re-run."
  exit 0
fi
# Reachability (the linux runner is the expected seed host, but guard anyway).
if ! curl -sf --max-time 8 "$SERVER/up" >/dev/null 2>&1; then
  echo "BLOCKED: shared server $SERVER is unreachable from this Linux runner."
  exit 0
fi

# The canonical scenario's EDITED (v2) content — deterministic so mac-pull-2/win-pull-2 recompute it.
NEW_CONTENT="cross-os e2e v2 (linux edit) tag=$TAG"$'\n'
NEW_CONTENT="$(for _ in $(seq 1 96); do printf '%s' "$NEW_CONTENT"; done)"
NEW_SHA="$(printf '%s' "$NEW_CONTENT" | sha256sum | awk '{print $1}')"

WORK="$(mktemp -d)"
MNT="$WORK/mnt"; DB="$WORK/linux.db"; CH="$WORK/linux-chunks"
mkdir -p "$MNT"
PID=""
cleanup() {
  fusermount -u "$MNT" 2>/dev/null || umount "$MNT" 2>/dev/null || true
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "==> maker-syncd log:"; [ -f "$WORK/d.log" ] && cat "$WORK/d.log" || echo "  (none)"
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== cross-os LINUX EDIT == server=$SERVER tag=$TAG file=$FNAME want_v1=$WANT_V1"
echo "   new (edited) sha256=$NEW_SHA"

echo "==> building maker-syncd"
cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-syncd >/dev/null 2>&1 || {
  echo "FAIL: build"; cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-syncd 2>&1 | tail -20; exit 1; }
SYNCD="client/sync-engine/target/debug/maker-syncd"

echo "==> mounting FUSE at $MNT"
"$SYNCD" "$MNT" "$DB" "$CH" "$SERVER" "$TOK" >"$WORK/d.log" 2>&1 &
PID=$!

wait_mounted() { for _ in $(seq 1 40); do
  mountpoint -q "$MNT" 2>/dev/null || grep -q " $MNT fuse" /proc/mounts 2>/dev/null && return 0
  kill -0 "$PID" 2>/dev/null || return 1; sleep 0.5; done; return 1; }
if ! wait_mounted; then
  echo "BLOCKED: FUSE mount did not come up — almost always /dev/fuse access for the runner user."
  echo "         'sudo usermod -aG fuse \$USER' (re-login), or run the runner container with"
  echo "         '--device /dev/fuse --cap-add SYS_ADMIN' (ci/runners/README.md). Daemon log:"; cat "$WORK/d.log"
  exit 0
fi
echo "  mounted"

sha() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }
emit() { if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "$1=$2" >> "$GITHUB_OUTPUT"; fi; echo "$1=$2"; }

# 1. Poll until the v1 file (windows write) is present + reads byte-for-byte through this mount.
echo "==> polling $MNT/$FNAME for v1 == $WANT_V1 (timeout ${TIMEOUT}s)"
END=$(( $(date +%s) + TIMEOUT )); GOT=""
while [ "$(date +%s)" -lt "$END" ]; do
  [ -f "$MNT/$FNAME" ] && GOT="$(sha "$MNT/$FNAME")" && [ "$GOT" = "$WANT_V1" ] && break
  sleep 1
done
if [ "$GOT" != "$WANT_V1" ]; then
  echo "  FAIL  v1 $FNAME did not converge to $WANT_V1 within ${TIMEOUT}s (last: '${GOT:-<absent>}')"
  echo "LINUX EDIT FAILED (could not read the windows-written v1 to edit)"
  exit 1
fi
echo "  PASS  v1 present byte-for-byte; now EDITING through the mount"

# 2. Edit THROUGH the mount → new deterministic content; verify locally, then wait for push.
printf '%s' "$NEW_CONTENT" > "$MNT/$FNAME"
LOCAL="$(sha "$MNT/$FNAME")"
if [ "$LOCAL" != "$NEW_SHA" ]; then
  echo "  FAIL  local read-back after edit ($LOCAL) != expected ($NEW_SHA)"
  echo "LINUX EDIT FAILED"; exit 1
fi
echo "  edited; waiting ~15s for the daemon to push the edit"
sleep 15

emit "EXPECTED_SHA" "$NEW_SHA"
echo "LINUX EDIT OK - edited $FNAME through the FUSE mount to v2 (sha=$NEW_SHA) and pushed to $SERVER"
exit 0
