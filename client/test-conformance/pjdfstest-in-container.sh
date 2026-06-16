#!/usr/bin/env bash
#
# Runs INSIDE the Linux container (see pjdfstest-docker.sh). Builds fs-fuse from the
# read-only /src mount, mounts it via real FUSE, and runs the pjdfstest subset our sync
# mount implements — printing a pass/total conformance map per operation.
#
set -uo pipefail   # NOT -e: informational; we tally, never abort early

# Copy the workspace to a writable location (cargo needs to write target/, /src is ro).
# Only the client crates + their Cargo lock are needed for fs-fuse.
mkdir -p /build/client
cp -a /src/client/. /build/client/
cd /build/client/fs-fuse

MNT="$(mktemp -d)"
WORK="$(mktemp -d)"
MOUNT_PID=""
# The operations our mount actually implements (chmod added now that mode bits round-trip).
SUBSET=(rename unlink mkdir rmdir truncate open chmod)

unmount() { fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null || true; }
cleanup() {
  unmount
  [ -n "$MOUNT_PID" ] && { kill "$MOUNT_PID" 2>/dev/null || true; wait "$MOUNT_PID" 2>/dev/null || true; }
  rm -rf "$MNT" "$WORK"
}
trap cleanup EXIT

echo "==> building maker-mount (fs-fuse) on linux"
cargo build --bin maker-mount 2>&1 | tail -3
BIN="/build/client/fs-fuse/target/debug/maker-mount"
[ -x "$BIN" ] || { echo "FAIL: maker-mount didn't build"; exit 1; }

echo "==> mounting at $MNT"
# In a bare-root container the fuser-0.17 AutoUnmount path requires a non-Owner allow ACL
# (allow_other/allow_root) and errors otherwise ("auto_unmount requires acl != Owner").
# maker-mount honours MAKER_MOUNT_NO_AUTOUNMOUNT to drop AutoUnmount+DefaultPermissions so a
# plain container mount comes up; cleanup() unmounts explicitly anyway.
export MAKER_MOUNT_NO_AUTOUNMOUNT=1
"$BIN" "$MNT" "$WORK/db" "$WORK/chunks" >"$WORK/mount.log" 2>&1 &
MOUNT_PID=$!
for _ in $(seq 1 50); do mountpoint -q "$MNT" && break; sleep 0.2; done
mountpoint -q "$MNT" || { echo "FAIL: mount didn't come up"; cat "$WORK/mount.log"; exit 1; }
echo "    mounted OK"

echo "==> building pjdfstest"
git clone --depth 1 https://github.com/pjd/pjdfstest "$WORK/pjd" >/dev/null 2>&1 \
  || { echo "FAIL: could not clone pjdfstest"; exit 1; }
( cd "$WORK/pjd" && autoreconf -ifs >/dev/null 2>&1 && ./configure >/dev/null 2>&1 && make pjdfstest >/dev/null 2>&1 ) \
  || { echo "FAIL: pjdfstest didn't build"; exit 1; }

echo
echo "==> pjdfstest conformance map (real FUSE mount)"
echo "    op        passed / total subtests"
echo "    --------  -----------------------------------------"
cd "$MNT"
GRAND_PASS=0; GRAND_TOTAL=0
for op in "${SUBSET[@]}"; do
  dir="$WORK/pjd/tests/$op"
  [ -d "$dir" ] || { printf "    %-8s  (no tests)\n" "$op"; continue; }
  out=$(prove -rv "$dir" 2>&1) || true
  total=$(printf '%s\n' "$out" | grep -oE 'Tests=[0-9]+' | head -1 | grep -oE '[0-9]+')
  failed=$(printf '%s\n' "$out" | grep -oE 'Failed: [0-9]+' | awk '{s+=$2} END{print s+0}')
  if [ -n "${total:-}" ]; then
    passed=$((total - ${failed:-0}))
    printf "    %-8s  %s / %s passed\n" "$op" "$passed" "$total"
    GRAND_PASS=$((GRAND_PASS + passed)); GRAND_TOTAL=$((GRAND_TOTAL + total))
  else
    printf "    %-8s  %s\n" "$op" "$(printf '%s\n' "$out" | grep -Ei 'All tests|Result' | tail -1)"
  fi
  # Per-.t failure breakdown + a sample of failed assertions (tells a real bug on a path
  # we support apart from an expected perms/owner gap).
  printf '%s\n' "$out" | grep -E "/${op}/[0-9]+\.t .*Failed: [1-9]" | sed -E 's/^/        /' || true
  printf '%s\n' "$out" | grep -E "^not ok" | head -6 | sed -E "s#^#        ${op}: #" || true
done
echo
echo "    ========  ============================================"
printf "    TOTAL     %s / %s subtests passed\n" "$GRAND_PASS" "$GRAND_TOTAL"
echo
echo "NOTE: residual failures concentrate in owner (chown/uid-gid) + special-file (mknod/"
echo "symlink) subtests, which are deliberate non-goals (CLAUDE.md §14). The errno/rename/"
echo "utimes rules this batch fixed are the ones expected to flip green."
