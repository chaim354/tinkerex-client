#!/usr/bin/env bash
#
# Gold-standard signal: run REAL `git` against a REAL FUSE kernel mount of the
# local-first Provider (`maker-mount` over `SqliteProvider`).
#
# This is the answer to "does a git repository work inside the Maker Server mounted
# sync filesystem?" — exercised by the actual `git` binary, not a simulation.
#
# Runs inside Linux (the only OS `fs-fuse`/`fuser` links on). On a macOS/Windows dev box
# use the Docker wrapper: `client/test-conformance/git-on-fuse-docker.sh`.
#
# Requires: a FUSE driver (libfuse3 / fuse3), /dev/fuse, CAP_SYS_ADMIN, cargo, git.
#
# Exit 0 = every asserted git workflow worked through the mount.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# CLIENT is the dir holding the `fs-fuse` + `fs-provider` crates. Default = parent of this
# script's dir (repo layout: client/test-conformance/..); override with CLIENT_DIR (the
# Docker wrapper sets it to the bind-mounted crate root).
CLIENT="${CLIENT_DIR:-$(cd "$HERE/.." && pwd)}"
MNT="${MNT:-/tmp/git-fuse-mnt}"
DB="${DB:-/tmp/git-fuse.db}"
CHUNKS="${CHUNKS:-/tmp/git-fuse-chunks}"

cleanup() {
  set +e
  fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null
  [ -n "${MOUNT_PID:-}" ] && kill "$MOUNT_PID" 2>/dev/null
  rm -rf "$DB" "$CHUNKS"
}
trap cleanup EXIT

rm -rf "$DB" "$CHUNKS"
mkdir -p "$MNT" "$CHUNKS"

echo "== building maker-mount (fs-fuse) =="
( cd "$CLIENT/fs-fuse" && cargo build --bin maker-mount )
BIN="$CLIENT/fs-fuse/target/debug/maker-mount"

echo "== mounting at $MNT =="
# Headless: drop AutoUnmount/DefaultPermissions (fuser 0.17 rejects AutoUnmount under the
# Owner ACL in a bare container). We unmount explicitly in cleanup().
MAKER_MOUNT_NO_AUTOUNMOUNT=1 "$BIN" "$MNT" "$DB" "$CHUNKS" >/tmp/mount.log 2>&1 &
MOUNT_PID=$!

# Wait for the mount to come live: the root dir of an empty provider has no entries, so
# `ls` succeeding isn't enough — poll for the mountpoint itself (or a recognizable error).
for _ in $(seq 1 75); do
  if mountpoint -q "$MNT" 2>/dev/null; then break; fi
  if ! kill -0 "$MOUNT_PID" 2>/dev/null; then echo "mount process exited early:"; cat /tmp/mount.log; exit 1; fi
  sleep 0.2
done
if ! mountpoint -q "$MNT" 2>/dev/null; then
  echo "FAIL: mount never came live. mount log:"; cat /tmp/mount.log
  exit 1
fi
echo "mount is live."

cd "$MNT"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; exit 1; }

echo "== git init / add / commit / log / status / diff / branch =="
git init -q repo
cd repo
echo "hello world" > README.md
mkdir -p src
echo "fn main() {}" > src/main.rs
git add .
git commit -q -m "initial commit"
git log --oneline | grep -q "initial commit" && pass "init+add+commit+log" || fail "commit/log"

git status --porcelain | grep -q . && fail "status: working tree should be clean" || pass "status clean"

echo "more" >> README.md
git diff --stat | grep -q "README.md" && pass "diff sees change" || fail "diff"
git add README.md && git commit -q -m "edit readme"

git branch feature
git checkout -q feature
echo "feature work" > feature.txt
git add feature.txt && git commit -q -m "feature commit"
git checkout -q main 2>/dev/null || git checkout -q master
test ! -f feature.txt && pass "checkout switches tree (feature.txt absent on main)" || fail "checkout"
git checkout -q feature
test -f feature.txt && pass "checkout back restores file" || fail "checkout back"

echo "== symlink: add + commit + checkout (the MVP feature) =="
git checkout -q main 2>/dev/null || git checkout -q master
ln -s README.md link-to-readme
git add link-to-readme
git commit -q -m "add symlink"
# Round-trip the symlink through a fresh checkout target inside the mount.
[ -L link-to-readme ] && pass "symlink is a symlink (lstat)" || fail "symlink lstat"
[ "$(readlink link-to-readme)" = "README.md" ] && pass "readlink target correct" || fail "readlink"
git ls-files -s link-to-readme | grep -q "^120000" && pass "git records mode 120000" || fail "git symlink mode"

echo "== git clone --local (hardlink path; degrades gracefully if unsupported) =="
cd "$MNT"
if git clone -q "$MNT/repo" cloned 2>clone.err; then
  ( cd cloned && git log --oneline | grep -q "initial commit" ) && pass "clone --local worked" || fail "clone content"
else
  echo "NOTE: 'git clone --local' failed (expected if hardlinks unsupported); retrying with --no-hardlinks"
  git clone -q --no-hardlinks "$MNT/repo" cloned2 && pass "clone --no-hardlinks worked (hardlink fallback)" || fail "clone --no-hardlinks"
fi

echo "ALL GIT-ON-FUSE ASSERTIONS PASSED"
