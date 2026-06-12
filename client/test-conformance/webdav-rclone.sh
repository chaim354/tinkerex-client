#!/usr/bin/env bash
#
# External conformance: drive our WebDAV surface (`fs-webdav` over the local-first
# Provider) with **rclone** — the same tool real users would sync with. rclone copies a
# tree in, then `check --download` re-reads every file and compares bytes, so this
# exercises our read AND write paths through chunking + the content-addressed store with
# an independent implementation. Then a modify + `sync` + re-check proves delta updates.
#
# Cross-platform (macOS + Linux). Requires: rclone on PATH, a Rust toolchain.
#
#   client/test-conformance/webdav-rclone.sh
#
set -euo pipefail

cd "$(dirname "$0")/../.."   # repo root
PORT="${PORT:-19988}"
WORK="$(mktemp -d)"
DATA="$WORK/data"
DB="$WORK/maker.db"
CHUNKS="$WORK/chunks"
SRV_PID=""

# Define the rclone remote "dav" via env (avoids connection-string escaping of the URL's
# own colons). `dav:tree` then targets a `tree/` subdir of our WebDAV root.
export RCLONE_CONFIG_DAV_TYPE=webdav
export RCLONE_CONFIG_DAV_URL="http://127.0.0.1:$PORT"
export RCLONE_CONFIG_DAV_VENDOR=other
REMOTE="dav:tree"

cleanup() {
  if [ -n "$SRV_PID" ]; then
    kill "$SRV_PID" 2>/dev/null || true
    wait "$SRV_PID" 2>/dev/null || true   # absorb the shell's "Terminated" notice
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

command -v rclone >/dev/null || { echo "FAIL: rclone not on PATH"; exit 1; }

echo "==> building maker-webdav"
cargo build --manifest-path client/fs-webdav/Cargo.toml --bin maker-webdav >/dev/null
BIN="client/fs-webdav/target/debug/maker-webdav"

echo "==> starting WebDAV server on 127.0.0.1:$PORT"
"$BIN" "127.0.0.1:$PORT" "$DB" "$CHUNKS" >"$WORK/server.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 50); do
  if curl -s -o /dev/null "http://127.0.0.1:$PORT/"; then break; fi
  sleep 0.2
done
curl -s -o /dev/null "http://127.0.0.1:$PORT/" || { echo "FAIL: server didn't start"; cat "$WORK/server.log"; exit 1; }

echo "==> generating test data (varied sizes incl. empty, 1B, KB, MB)"
mkdir -p "$DATA/sub/deep"
: > "$DATA/empty.bin"                                   # 0 bytes
printf 'x' > "$DATA/one.bin"                            # 1 byte
head -c 1000      /dev/urandom > "$DATA/small.bin"      # < a chunk
head -c 300000    /dev/urandom > "$DATA/medium.bin"     # many chunks
head -c 4000000   /dev/urandom > "$DATA/large.bin"      # MBs
head -c 50000     /dev/urandom > "$DATA/sub/a.bin"
head -c 120000    /dev/urandom > "$DATA/sub/deep/b.bin"
cp "$DATA/medium.bin" "$DATA/sub/dup.bin"               # identical content → dedup path

echo "==> rclone copy → WebDAV"
rclone copy "$DATA" "$REMOTE"

echo "==> rclone check --download (byte-for-byte round-trip)"
rclone check --download "$DATA" "$REMOTE"

echo "==> delta: modify, append, delete, re-sync, re-check"
head -c 250000 /dev/urandom > "$DATA/medium.bin"       # overwrite (different length)
printf 'more'  >> "$DATA/small.bin"                     # append
rm "$DATA/one.bin"                                      # delete
rclone sync "$DATA" "$REMOTE"                           # propagate incl. the deletion
rclone check --download "$DATA" "$REMOTE"

echo "==> listing parity"
LOCAL_N=$(find "$DATA" -type f | wc -l | tr -d ' ')
REMOTE_N=$(rclone ls "$REMOTE" | wc -l | tr -d ' ')
[ "$LOCAL_N" = "$REMOTE_N" ] || { echo "FAIL: file count $LOCAL_N local vs $REMOTE_N remote"; exit 1; }

echo "PASS: rclone ⇄ WebDAV round-trip + delta verified ($REMOTE_N files)"
