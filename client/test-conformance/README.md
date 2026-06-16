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
| `pjdfstest-fuse.sh` | **pjdfstest** | `fs-fuse` (Linux path) | ℹ️ informational | POSIX semantics for the ops we implement (rename/unlink/mkdir/rmdir/truncate/open/**chmod**). The suite's chown/symlink/hardlink/xattr tests are out of scope (we don't model ownership or links), so this is a conformance *map*, not a gate. The errno paths (ENOENT/EEXIST/ENOTDIR/EISDIR/ENOTEMPTY/ENAMETOOLONG/EINVAL), the rename cross-type rules, and the utimensat atime/mtime semantics are asserted hermetically in `fs-provider/tests/posix_errno.rs` + `posix_times.rs` (run on any OS); this script proves the same rules through the real kernel VFS. |
| `pjdfstest-docker.sh` | **pjdfstest** (in Docker) | `fs-fuse` (Linux path, via Linux container) | ℹ️ on-demand | Runs `pjdfstest-in-container.sh` inside a Linux container (`Dockerfile.pjdfstest`) so the real-mount pjdfstest map can be produced **from a macOS dev box** (where `fuser` doesn't link / there's no `/dev/fuse`). Needs `--cap-add SYS_ADMIN --device /dev/fuse`. Same subset + map as `pjdfstest-fuse.sh`, plus a grand-total line. |
| `cfapi-smoke.ps1` | self-authored | `fs-cfapi` (Windows path) | ℹ️ on-demand/nightly | Live CfAPI write-path proof: register → enumerate → hydrate → one each of new/edit/delete/rename, each asserted to round-trip to the deployed server via the `tree_names` oracle. Needs an interactive Windows session + the deployed server. |
| `cfapi-conformance.ps1` | self-authored | `fs-cfapi` (Windows path) | ℹ️ on-demand/nightly | The rigorous CfAPI round-trip (the Windows analogue of `mount-conformance`): many files of varied sizes (incl. > chunk size → multi-chunk + dedup), nested dirs, and create/edit/delete/rename churn through the real Cloud Filter mount — asserted **byte-for-byte** by hydrating every file on a fresh device (`verify_tree`) and comparing SHA-256s. |
| `two-device-cfapi.ps1` | self-authored | `fs-cfapi` × 2 (Windows) | ℹ️ on-demand/nightly | **Two real CfAPI mounts at once, app-to-app.** Pairs device A + device B into one library (the real `create_pairing`/`pair` flow), mounts BOTH sync roots live (two `maker-cfsyncd` = the app's `CfApiMount`), then passes a file **through A's mount** → reads it **byte-for-byte through B's mount**, edits **through B** → A sees it, plus new/nested/delete/rename/multi-chunk. The layer above `two_device_e2e.rs`. Interactive Windows session + deployed server required. |
| `two-device-fuse.sh` | self-authored | `fs-fuse` × 2 (Linux) | ℹ️ on-demand/nightly (CI-attemptable) | The Linux analogue: two real FUSE mounts (two `maker-syncd` = the app's `Daemon`) paired into one library, A↔B byte-for-byte **through the mounts**. Self-contained (boots a memory Rails server). FUSE can mount in CI with `/dev/fuse`, so this real-mount leg is automated; it exits 0 with a `BLOCKED` notice if `/dev/fuse`/`allow_other` is unavailable. |
| `two-device-webdav-macos.sh` | self-authored | `fs-webdav` × 2 (macOS) | 🖐️ manual (interactive) | The macOS analogue: two real native WebDAV mounts (two `maker-webdavd` = the app's `WebdavMount`) paired into one library, A↔B byte-for-byte **through the mounts**. **NOT CI-automated** — `mount_webdav` needs an interactive desktop session; a human runs it locally. |

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
client/test-conformance/cfapi-smoke.ps1         [-Server http://192.168.1.95]
client/test-conformance/cfapi-conformance.ps1   [-Server http://192.168.1.95] [-Files 24]
client/test-conformance/two-device-cfapi.ps1    [-Server http://192.168.1.95]   # TWO real mounts, A<->B
```

```sh
# Two-device REAL-MOUNT end-to-end (a file through ONE mount, read through ANOTHER device's mount):
client/test-conformance/two-device-fuse.sh             # Linux, two real FUSE mounts (self-contained)
client/test-conformance/two-device-webdav-macos.sh     # macOS, two real WebDAV mounts (run AT the machine — interactive)
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

There are **three** layers, increasing in fidelity:

1. **Engine-level** — `client/sync-engine/tests/two_device_e2e.rs`: distributed multi-device
   convergence (A→B→edit→A + delete/move/rename/dedup/restore/conflict) at the provider/engine
   level — the same `SyncEngine` loop + `ServerClient` API the Tauri app's `Daemon` runs, **minus
   the kernel mount**. Runs **headlessly on every OS** (per-PR on Linux in `engine-integration`;
   nightly on mac/Windows vs the deployed server in `e2e-{linux,macos,windows}`).

2. **Single-mount kernel leg** — `cfapi-{smoke,conformance}.ps1` / `webdav-rclone.sh` /
   `fsx-fuse.sh` / `pjdfstest-fuse.sh`: the syscall→adapter→`Provider` translation on ONE real
   mount, asserted via a control-plane oracle (a fresh headless device).

3. **Two-mount, app-to-app kernel leg** — `two-device-{cfapi.ps1,fuse.sh,webdav-macos.sh}`: the
   highest-fidelity test — TWO real kernel mounts live at once (the same `CfApiMount`/`Daemon`/
   `WebdavMount` the Tauri app's `sync_start` launches), paired via the **real** pairing flow, with
   a file passed **through one mount** and read **byte-for-byte through the other**. This is the
   only leg that exercises syscall→adapter→`Provider`→sync→`Provider`→adapter→syscall end-to-end
   between two devices. Interactive-session-bound on macOS (manual) and Windows (CfAPI); FUSE is
   CI-automatable on Linux.

Together: layer 1 proves the sync plane converges on all three OSes headlessly; layer 2 proves each
mount translates syscalls correctly; layer 3 proves the full app-to-app path through two real mounts.

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
