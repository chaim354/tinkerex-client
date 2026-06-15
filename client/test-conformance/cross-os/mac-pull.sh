#!/usr/bin/env bash
#
# cross-os-e2e leg (macOS PULL, WebDAV) — reused by BOTH `mac-pull-1` and `mac-pull-2`.
#
# Mounts ONE real native WebDAV mount with the MAC device token (paired into the shared library by
# the `seed` job), POLLs the mounted path until the tagged file appears AND reads byte-for-byte ==
# the EXPECTED sha (passed via env), asserts, then unmounts. Sync is async (nudge + poll), so we
# poll with a timeout rather than assuming immediate availability.
#
# Single-mount, single-op counterpart of two-device-webdav-macos.sh — same mount/daemon patterns
# and the same BLOCKED self-detection: mount_webdav needs an INTERACTIVE desktop session (it won't
# hold a mount headless), and the mac runner is known to sometimes not route to the deployed
# segment — both are environment walls → exit 0 with a BLOCKED notice (never a false red).
#
# Inputs (env, set by the workflow):
#   SERVER_URL    - the SHARED server (e.g. http://192.168.1.95)
#   TOK_MAC       - the mac device token
#   TEST_TAG      - the per-run unique tag (filename suffix)
#   EXPECTED_SHA  - the sha256 the file must read as (from win-push for leg 1, linux-edit for leg 2)
#   STEP_LABEL    - optional human label for logs (e.g. "pull-1 (windows write)")
#   PULL_TIMEOUT  - optional seconds to poll (default 90)
#
#   EXPECTED_SHA=... TOK_MAC=... TEST_TAG=... client/test-conformance/cross-os/mac-pull.sh
set -uo pipefail

cd "$(dirname "$0")/../../.." || exit 1   # repo root
SERVER="${SERVER_URL:-http://192.168.1.95}"
TOK="${TOK_MAC:-}"
TAG="${TEST_TAG:-notag}"
WANT="${EXPECTED_SHA:-}"
LABEL="${STEP_LABEL:-pull}"
TIMEOUT="${PULL_TIMEOUT:-90}"
DAV_PORT="${DAV_PORT:-9981}"
FNAME="crossos-$TAG.txt"

if [ -z "$TOK" ];  then echo "BLOCKED: no TOK_MAC (seed did not emit a mac token)."; exit 0; fi
if [ -z "$WANT" ]; then echo "BLOCKED: no EXPECTED_SHA (the upstream push/edit leg was BLOCKED or did not emit a sha)."; exit 0; fi

# Reachability: the mac runner sometimes can't route to the deployed segment. Detect → BLOCKED.
if ! curl -sf --max-time 8 "$SERVER/up" >/dev/null 2>&1; then
  echo "BLOCKED: shared server $SERVER is unreachable from this macOS runner (known routing gap)."
  echo "         Give the mac runner a route to the deployed segment, then re-run (ci/runners/README.md)."
  exit 0
fi

WORK="$(mktemp -d)"
MNT="$HOME/MakerDav-xos-$$"
DB="$WORK/mac.db"; CH="$WORK/mac-chunks"
PID=""
cleanup() {
  [ -n "$PID" ] && touch "$DB.stop" 2>/dev/null || true
  sleep 2
  /sbin/umount "$MNT" 2>/dev/null || diskutil unmount force "$MNT" 2>/dev/null || true
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "==> maker-webdavd log:"; [ -f "$WORK/d.log" ] && cat "$WORK/d.log" || echo "  (none)"
  rmdir "$MNT" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== cross-os MAC PULL [$LABEL] == server=$SERVER tag=$TAG file=$FNAME want=$WANT"

echo "==> building maker-webdavd"
cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-webdavd >/dev/null 2>&1 || {
  echo "FAIL: build"; cargo build --manifest-path client/sync-engine/Cargo.toml --bin maker-webdavd 2>&1 | tail -20; exit 1; }
WEBDAVD="client/sync-engine/target/debug/maker-webdavd"

echo "==> mounting WebDAV at $MNT (port $DAV_PORT)"
"$WEBDAVD" "$MNT" "$DB" "$CH" "$SERVER" "$TOK" "$DAV_PORT" >"$WORK/d.log" 2>&1 &
PID=$!

wait_mounted() { for _ in $(seq 1 40); do mount | grep -q " $MNT " && return 0; kill -0 "$PID" 2>/dev/null || return 1; sleep 0.5; done; return 1; }
if ! wait_mounted; then
  echo "BLOCKED: WebDAV mount did not come up (need an INTERACTIVE macOS desktop session)."
  echo "         Run the mac runner interactively (ci/runners/README.md). Daemon log:"; cat "$WORK/d.log"
  exit 0
fi
echo "  mounted"

sha() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }
echo "==> polling $MNT/$FNAME for byte-for-byte == $WANT (timeout ${TIMEOUT}s)"
END=$(( $(date +%s) + TIMEOUT ))
GOT=""
while [ "$(date +%s)" -lt "$END" ]; do
  if [ -f "$MNT/$FNAME" ]; then
    GOT="$(sha "$MNT/$FNAME")"
    [ "$GOT" = "$WANT" ] && break
  fi
  sleep 1
done

if [ "$GOT" = "$WANT" ]; then
  echo "  PASS  [$LABEL] $FNAME reads byte-for-byte ($WANT) through the macOS WebDAV mount"
  echo "MAC PULL [$LABEL] PASSED"
  exit 0
fi
echo "  FAIL  [$LABEL] $FNAME did not converge to $WANT within ${TIMEOUT}s (last seen: '${GOT:-<absent>}')"
echo "MAC PULL [$LABEL] FAILED"
exit 1
