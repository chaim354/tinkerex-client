#!/usr/bin/env bash
# Stamp a single version into BOTH the Tauri app's Cargo.toml and tauri.conf.json so they
# never drift. This matters for the WKWebView cache fix (client/maker-app/src-tauri/src/lib.rs):
# the on-update cache eviction keys off CARGO_PKG_VERSION (= Cargo.toml's [package] version),
# and Tauri bundles the installer version from tauri.conf.json — if those two disagree, or a
# release reuses the previous version, the cache won't bust on update.
#
# Usage:  scripts/set-app-version.sh 1.2.3
# Called by .github/workflows/build-app.yml on `app-v*` tag builds (version from the tag), and
# runnable by hand for a manual bump. Uses python3 for the edits so it behaves identically on
# the Linux / macOS / Windows(Git-Bash) build runners (sed's `0,/re/` is GNU-only — it silently
# no-ops on macOS BSD sed, which is exactly the kind of drift this script exists to prevent).
set -euo pipefail

V="${1:?usage: set-app-version.sh <x.y.z>}"
# Accept x.y.z plus an optional -prerelease / +build suffix.
if ! [[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+.][0-9A-Za-z.-]+)?$ ]]; then
  echo "set-app-version: '$V' is not a valid semver (expected x.y.z)" >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"   # → src-tauri

PY=python3
command -v python3 >/dev/null 2>&1 || PY=python   # Windows runners expose it as `python`

"$PY" - "$V" "$root/Cargo.toml" "$root/tauri.conf.json" <<'PYEOF'
import re, sys
version, cargo_path, conf_path = sys.argv[1], sys.argv[2], sys.argv[3]

# Cargo.toml: the first `version = "..."` is the [package] version (top of file).
cargo = open(cargo_path, encoding="utf-8").read()
cargo, n = re.subn(r'(?m)^(version\s*=\s*")[^"]*(")', lambda m: m.group(1) + version + m.group(2), cargo, count=1)
assert n == 1, f"no [package] version found in {cargo_path}"
open(cargo_path, "w", encoding="utf-8").write(cargo)

# tauri.conf.json: edit the text (not json.load/dump) to preserve formatting + key order.
conf = open(conf_path, encoding="utf-8").read()
conf, n = re.subn(r'("version"\s*:\s*")[^"]*(")', lambda m: m.group(1) + version + m.group(2), conf, count=1)
assert n == 1, f'no top-level "version" found in {conf_path}'
open(conf_path, "w", encoding="utf-8").write(conf)
PYEOF

echo "set-app-version → $V"
grep -m1 -E '^version *=' "$root/Cargo.toml"
grep -m1 -E '"version":' "$root/tauri.conf.json"
