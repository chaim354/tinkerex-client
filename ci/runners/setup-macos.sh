#!/usr/bin/env bash
# Register a self-hosted Actions runner on this Mac against chaim354/tinkerex-client.
#
# SELF-HOSTED RUNNERS REGISTER PER REPOSITORY, NOT PER ORG. A runner registered against
# chaim354/tinkerex is invisible here, so each physical box needs a SECOND runner service even
# though it already runs one for that repo.
#
# READ THIS BEFORE ASSUMING A RUNNER IS ALL THAT IS MISSING: as of 2026-08-21 this repository has
# ZERO workflows. The Stage 1 split (tinkerex commit 7d4d386) DELETED build-app.yml,
# install-app-on-runners.yml, cross-os-e2e.yml and rust-client.yml from tinkerex, saying they
# "move wholesale to tinkerex-client's own CI" -- and that half never happened. ~1,018 lines and
# ~24 jobs of Rust/Tauri CI exist in neither repo. They are recoverable from tinkerex at
# `git show 7d4d386^:.github/workflows/<name>.yml`, and their `client/...` path references still
# resolve here because the extraction deliberately kept the client/ prefix. A runner registered
# by this script will sit idle until those workflows are restored.
#
# REPO_URL IS HARDCODED ON PURPOSE, mirroring tinkerex's copy: taking the repo as an argument
# makes registering the wrong box against the wrong repo a one-character typo.
#
#   gh api -X POST repos/chaim354/tinkerex-client/actions/runners/registration-token --jq .token
#   ./ci/runners/setup-macos.sh <THAT_TOKEN>

set -euo pipefail

TOKEN="${1:?Usage: setup-macos.sh <REGISTRATION_TOKEN>}"
REPO_URL="https://github.com/chaim354/tinkerex-client"
RUNNER_DIR="${RUNNER_DIR:-$HOME/actions-runner-tinkerex-client}"
LABELS="self-hosted,macos,maker-mac"

command -v brew >/dev/null 2>&1 || { echo "Homebrew required: https://brew.sh"; exit 1; }

echo "==> Installing prereqs via Homebrew"
brew install jq mingw-w64 rclone || true   # mingw-w64 = linker for the windows-gnu cross-check

echo "==> Installing Rust (rustup) if missing + the Windows cross-target"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi
rustup target add x86_64-pc-windows-gnu

# arch: Apple Silicon → arm64, Intel → x64
ARCH="$( [ "$(uname -m)" = "arm64" ] && echo arm64 || echo x64 )"

echo "==> Downloading the latest GitHub Actions runner (osx-$ARCH)"
mkdir -p "$RUNNER_DIR" && cd "$RUNNER_DIR"
VERSION="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | sed 's/^v//')"
TARBALL="actions-runner-osx-${ARCH}-${VERSION}.tar.gz"
if [ ! -f "$TARBALL" ]; then
  curl -fsSL -o "$TARBALL" \
    "https://github.com/actions/runner/releases/download/v${VERSION}/${TARBALL}"
fi
tar xzf "$TARBALL"

echo "==> Configuring the runner (labels: $LABELS)"
./config.sh --unattended --replace \
  --url "$REPO_URL" --token "$TOKEN" \
  --name "$(hostname -s)-maker-mac" --labels "$LABELS"

echo "==> Installing + starting the runner service (launchd)"
./svc.sh install
./svc.sh start

echo "==> Done. NOTE: fs-fuse on macOS needs the FUSE-T pkg-config alias (client/fs-fuse/BUILD.md (in this repo))"
echo "    only if a job builds fs-fuse here; the cross-check + fs-webdav build do not."
echo "    Verify: gh api repos/chaim354/tinkerex-client/actions/runners --jq '.runners[].name'"
