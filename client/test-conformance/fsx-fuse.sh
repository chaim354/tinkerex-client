#!/usr/bin/env bash
#
# External conformance: **fsx** against our FUSE mount (`fs-fuse` over the Provider).
# fsx is the filesystem exerciser used by CephFS/XFS/etc. — it drives a long randomized
# stream of write / read / truncate (and would mmap, which we disable) operations against
# a real file and verifies every read against an in-memory "good" copy. This is the
# strongest independent check of our data plane: chunking, offset math, truncate, and the
# content-addressed store, all exercised adversarially.
#
# Linux only (our FUSE path; macOS uses WebDAV). Requires: libfuse + fusermount, a C
# compiler, curl, a Rust toolchain. Run as a user with /dev/fuse access.
#
#   client/test-conformance/fsx-fuse.sh [numops]
#
set -euo pipefail

cd "$(dirname "$0")/../.."   # repo root
NUMOPS="${1:-100000}"
MNT="$(mktemp -d)"
WORK="$(mktemp -d)"
MOUNT_PID=""

unmount() { fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null || true; }
cleanup() {
  unmount
  [ -n "$MOUNT_PID" ] && { kill "$MOUNT_PID" 2>/dev/null || true; wait "$MOUNT_PID" 2>/dev/null || true; }
  rm -rf "$MNT" "$WORK"
}
trap cleanup EXIT

echo "==> building maker-mount (fs-fuse)"
cargo build --manifest-path client/fs-fuse/Cargo.toml --bin maker-mount >/dev/null
BIN="client/fs-fuse/target/debug/maker-mount"

echo "==> mounting at $MNT"
"$BIN" "$MNT" "$WORK/db" "$WORK/chunks" >"$WORK/mount.log" 2>&1 &
MOUNT_PID=$!
for _ in $(seq 1 50); do mountpoint -q "$MNT" && break; sleep 0.2; done
mountpoint -q "$MNT" || { echo "FAIL: mount didn't come up"; cat "$WORK/mount.log"; exit 1; }

echo "==> fetching + building fsx (xfstests/ltp)"
curl -fsSL "https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git/plain/ltp/fsx.c" -o "$WORK/fsx.c"
cc -O2 -D_GNU_SOURCE -o "$WORK/fsx" "$WORK/fsx.c" -lm 2>"$WORK/cc.log" \
  || { echo "FAIL: fsx didn't compile"; cat "$WORK/cc.log"; exit 1; }

# -W / -R disable mmap writes/reads (FUSE mmap is out of scope); the rest is plain
# read/write/truncate, which is exactly our data plane. fsx aborts non-zero on any
# data mismatch, so a clean exit IS the assertion.
echo "==> running fsx ($NUMOPS ops, mmap disabled) inside the mount"
"$WORK/fsx" -N "$NUMOPS" -W -R -l 2000000 "$MNT/fsx.testfile"

echo "PASS: fsx completed $NUMOPS ops with no data mismatch through the FUSE mount"
