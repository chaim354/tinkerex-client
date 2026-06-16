#!/usr/bin/env bash
#
# External conformance: **pjdfstest** against our FUSE mount — the POSIX filesystem
# semantics suite that JuiceFS/CephFS gate on. We run only the subset of operations our
# sync mount actually implements (rename / unlink / mkdir / rmdir / truncate / open /
# chmod — permission *bits* round-trip + sync, so the chmod mode-bit subtests pass; only
# chown/owner subtests are out of scope). The suite's chown/symlink/hardlink/xattr tests
# are intentionally out of scope (we don't model ownership or links), so this job is
# **informational** — it prints a conformance map of what we pass, not a hard gate.
#
# Linux only. Requires: libfuse + fusermount, autotools + a C compiler, git, prove
# (perl), a Rust toolchain. pjdfstest's setuid/owner tests want root; run under sudo for
# the fullest signal (the harness still runs unprivileged, just with more skips).
#
#   client/test-conformance/pjdfstest-fuse.sh
#
set -uo pipefail   # NOT -e: this is informational; we tally results, never abort early

cd "$(dirname "$0")/../.."   # repo root
MNT="$(mktemp -d)"
WORK="$(mktemp -d)"
MOUNT_PID=""
SUBSET=(rename unlink mkdir rmdir truncate open chmod)

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

echo "==> building pjdfstest"
git clone --depth 1 https://github.com/pjd/pjdfstest "$WORK/pjd" >/dev/null 2>&1
( cd "$WORK/pjd" && autoreconf -ifs >/dev/null 2>&1 && ./configure >/dev/null 2>&1 && make pjdfstest >/dev/null 2>&1 ) \
  || { echo "FAIL: pjdfstest didn't build"; exit 1; }

echo "==> running the supported subset (informational conformance map)"
echo "    op        passed / total subtests"
echo "    --------  -----------------------------------------"
cd "$MNT"
for op in "${SUBSET[@]}"; do
  dir="$WORK/pjd/tests/$op"
  [ -d "$dir" ] || { printf "    %-8s  (no tests)\n" "$op"; continue; }
  # Each .t locates the pjdfstest binary relative to itself (../../pjdfstest), and the
  # filesystem under test is the CWD ($MNT). Parse the TAP totals so this is a real
  # pass/total map, not just FAIL — perms/owner tests are expected to fail (see below).
  out=$(prove -rv "$dir" 2>&1) || true
  total=$(printf '%s\n' "$out" | grep -oE 'Tests=[0-9]+' | head -1 | grep -oE '[0-9]+')
  failed=$(printf '%s\n' "$out" | grep -oE 'Failed: [0-9]+' | awk '{s+=$2} END{print s+0}')
  if [ -n "${total:-}" ]; then
    printf "    %-8s  %s / %s passed\n" "$op" "$((total - ${failed:-0}))" "$total"
  else
    printf "    %-8s  %s\n" "$op" "$(printf '%s\n' "$out" | grep -Ei 'All tests|Result' | tail -1)"
  fi
  # Per-.t-file failure breakdown (each .t covers a known aspect: 00=basic, then
  # perms/owner/errno/link cases). Shows WHERE the misses concentrate.
  printf '%s\n' "$out" | grep -E "/${op}/[0-9]+\.t .*Failed: [1-9]" | sed -E 's/^/        /' || true
  # A small sample of the actual failed assertions, so real bugs (wrong errno on a
  # path we DO support) can be told apart from expected perms/owner gaps.
  printf '%s\n' "$out" | grep -E "^not ok" | head -8 | sed -E "s#^#        ${op}: #" || true
done

echo
echo "INFORMATIONAL: pjdfstest subset complete. Failures in perms/owner/link tests are"
echo "expected — our mount doesn't expose POSIX permissions or links. Use this as a"
echo "conformance map for the operations we DO implement."
