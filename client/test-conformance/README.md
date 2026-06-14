# External conformance suites

Our own tests (fs-provider hermetic suites, sync-engine integration, the convergence
harness) are necessary but self-authored. These scripts add **independent, battle-tested
external tools** that the serious filesystems (JuiceFS, CephFS, gcsfuse) gate on, pointed
at our real mount surfaces. They validate the *filesystem layer*; distributed-sync
convergence stays the job of the model-based harness in `fs-provider/tests/convergence.rs`
(there is no off-the-shelf suite for that — every sync project rolls its own).

| Script | Tool | Surface | Gating? | What it proves |
|---|---|---|---|---|
| `webdav-rclone.sh` | **rclone** | `fs-webdav` (macOS path) | ✅ gating | Copy a tree in, `check --download` every file byte-for-byte, then modify/append/delete + `sync` + re-check. Exercises read+write through chunking + the content store with an independent client. Cross-platform. |
| `fsx-fuse.sh` | **fsx** (xfstests) | `fs-fuse` (Linux path) | ✅ gating | A long randomized write/read/**truncate** stream verified against an in-memory model — the strongest adversarial check of the data plane. Linux only. |
| `pjdfstest-fuse.sh` | **pjdfstest** | `fs-fuse` (Linux path) | ℹ️ informational | POSIX semantics for the ops we implement (rename/unlink/mkdir/rmdir/truncate/open). The suite's perms/owner/link tests are out of scope (we don't expose them), so this is a conformance *map*, not a gate. |
| `cfapi-smoke.ps1` | self-authored | `fs-cfapi` (Windows path) | ℹ️ on-demand/nightly | Live CfAPI write-path proof: register → enumerate → hydrate → one each of new/edit/delete/rename, each asserted to round-trip to the deployed server via the `tree_names` oracle. Needs an interactive Windows session + the deployed server. |
| `cfapi-conformance.ps1` | self-authored | `fs-cfapi` (Windows path) | ℹ️ on-demand/nightly | The rigorous CfAPI round-trip (the Windows analogue of `mount-conformance`): many files of varied sizes (incl. > chunk size → multi-chunk + dedup), nested dirs, and create/edit/delete/rename churn through the real Cloud Filter mount — asserted **byte-for-byte** by hydrating every file on a fresh device (`verify_tree`) and comparing SHA-256s. |

## Running locally

```sh
# macOS or Linux — needs rclone on PATH:
client/test-conformance/webdav-rclone.sh

# Linux only — needs libfuse + fusermount, a C compiler, curl:
client/test-conformance/fsx-fuse.sh [numops]      # default 100000
client/test-conformance/pjdfstest-fuse.sh         # needs autotools + git + prove
```

```powershell
# Windows only (self-hosted runner, interactive session) — needs the deployed server + chunk store:
client/test-conformance/cfapi-smoke.ps1        [-Server http://192.168.1.95]
client/test-conformance/cfapi-conformance.ps1  [-Server http://192.168.1.95] [-Files 24]
```

## CI

The `mount-conformance` job in `.github/workflows/rust-client.yml` runs the three FUSE/WebDAV
suites on an Ubuntu runner: `webdav-rclone` and `fsx-fuse` gate the build; `pjdfstest-fuse` runs
`continue-on-error` (informational map).

The CfAPI suites run on the self-hosted Windows runner — `nightly.yml` (`windows-smoke` +
`cfapi-conformance`), and on-demand from `rust-client.yml` (`gh workflow run rust-client.yml -f
target=cfapi` for the smoke, `-f target=cfapi-conf` for the conformance). Both are
`continue-on-error`: CfAPI needs an interactive Windows login session to enumerate, so they're a
live regression net, not a build gate.

## Relationship to the two-device end-to-end suite

These scripts cover the **kernel-mount leg** (syscalls → adapter → `Provider`) — the part that
needs an interactive session and can't run headless. The complementary
`client/sync-engine/tests/two_device_e2e.rs` covers **distributed multi-device convergence**
(the headline A→B→edit→A roundtrip + delete/move/rename/dedup/restore/conflict) at the
provider/engine level — the same `SyncEngine` loop + `ServerClient` API the Tauri app's `Daemon`
runs, minus the kernel mount. That suite runs **headlessly on every OS** (per-PR on Linux in the
`engine-integration` job; nightly on mac/Windows vs the deployed server in `e2e-{linux,macos,windows}`).
Together: the e2e suite proves the sync plane converges on all three OSes; these scripts prove the
mount surface translates syscalls correctly on each.

## Notes / caveats

- These cover the **mount**, not distributed convergence. Two devices diverging under
  concurrent edits is what `convergence.rs` (ownCloud-`FakeFolder`-style) and the
  `two_device_e2e.rs` end-to-end suite cover.
- pjdfstest will report failures in chmod/chown/symlink/hardlink/xattr tests — **expected**:
  our sync mount intentionally doesn't expose POSIX permissions or links.
- fsx runs with mmap disabled (`-W -R`) — FUSE mmap is out of scope for now.
- fsx (Apple's exerciser) is **APSL-2.0** — copyleft, so we **fetch it at test time** from
  FreeBSD's standalone copy rather than vendoring it, keeping our repo/artifacts permissive
  (§9). Downloading + running a tool in CI is not redistribution.
