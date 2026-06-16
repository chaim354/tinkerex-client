#!/usr/bin/env bash
#
# Run the **real-mount pjdfstest** conformance suite (`pjdfstest-fuse.sh`) inside a
# Linux **Docker** container — so it works from a macOS dev box, where `fs-fuse`/`fuser`
# don't link and there's no `/dev/fuse`. This builds `fs-fuse` on Linux, mounts it via
# real FUSE, and runs pjdfstest's `rename`/`unlink`/`mkdir`/`rmdir`/`truncate`/`open`/
# `chmod` subset against the kernel mount — the same rules the Provider-level
# `tests/posix_errno.rs` / `tests/posix_times.rs` suites assert hermetically, now proven
# through the real VFS path.
#
# Requirements: Docker with `--cap-add SYS_ADMIN --device /dev/fuse` (FUSE in a container).
# Usage:  client/test-conformance/pjdfstest-docker.sh
#
set -euo pipefail

cd "$(dirname "$0")/../.."   # repo root
REPO="$(pwd)"
IMAGE="maker-pjdfstest:latest"

echo "==> building the pjdfstest Linux image (rust + fuse3 + autotools + perl prove)"
docker build -t "$IMAGE" -f client/test-conformance/Dockerfile.pjdfstest client/test-conformance

echo "==> running pjdfstest against a real FUSE mount of fs-fuse"
# SYS_ADMIN + /dev/fuse let FUSE mount inside the container. The repo is mounted read-only
# at /src; the container copies the crates it needs to a writable /build so cargo can work.
docker run --rm \
  --cap-add SYS_ADMIN \
  --device /dev/fuse \
  --security-opt apparmor:unconfined \
  -v "$REPO":/src:ro \
  "$IMAGE" \
  bash /src/client/test-conformance/pjdfstest-in-container.sh
