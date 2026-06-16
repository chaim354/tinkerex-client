#!/usr/bin/env bash
#
# Run the real git-on-FUSE gold-standard test (git-on-fuse.sh) inside a Linux Docker
# container — so it works from a macOS/Windows dev box where `fs-fuse`/`fuser` can't link.
#
# Needs: Docker with --cap-add SYS_ADMIN --device /dev/fuse (FUSE inside the container).
#
# It bind-mounts the two crates the binary needs (fs-fuse + fs-provider) read-only and
# builds inside the container against a container-local target dir (so the host's macOS
# target/ is never used for a Linux build).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLIENT="$(cd "$HERE/.." && pwd)"
IMAGE="rust:1-bookworm"

echo "== pulling $IMAGE (once) =="
docker pull -q "$IMAGE" >/dev/null

# Copy the crate sources into a throwaway build context so the container can write its own
# Cargo.lock/target without touching the host tree.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/client"
cp -R "$CLIENT/fs-fuse" "$WORK/client/fs-fuse"
cp -R "$CLIENT/fs-provider" "$WORK/client/fs-provider"
cp "$HERE/git-on-fuse.sh" "$WORK/git-on-fuse.sh"
# Drop any host build artifacts.
rm -rf "$WORK/client/fs-fuse/target" "$WORK/client/fs-provider/target"

docker run --rm \
  --cap-add SYS_ADMIN --device /dev/fuse --security-opt apparmor:unconfined \
  -v "$WORK:/work" -w /work \
  -e CLIENT_DIR=/work/client \
  -e MNT=/tmp/git-fuse-mnt -e DB=/tmp/git-fuse.db -e CHUNKS=/tmp/git-fuse-chunks \
  "$IMAGE" bash -c '
    set -e
    export PATH="/usr/local/cargo/bin:$PATH"
    apt-get update -qq && apt-get install -y -qq fuse3 libfuse3-dev git >/dev/null
    echo "user_allow_other" >> /etc/fuse.conf || true
    bash /work/git-on-fuse.sh
  '
