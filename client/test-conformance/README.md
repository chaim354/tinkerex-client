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

## Running locally

```sh
# macOS or Linux — needs rclone on PATH:
client/test-conformance/webdav-rclone.sh

# Linux only — needs libfuse + fusermount, a C compiler, curl:
client/test-conformance/fsx-fuse.sh [numops]      # default 100000
client/test-conformance/pjdfstest-fuse.sh         # needs autotools + git + prove
```

## CI

The `mount-conformance` job in `.github/workflows/rust-client.yml` runs all three on an
Ubuntu runner: `webdav-rclone` and `fsx-fuse` gate the build; `pjdfstest-fuse` runs
`continue-on-error` (informational map).

## Notes / caveats

- These cover the **mount**, not distributed convergence. Two devices diverging under
  concurrent edits is what `convergence.rs` (ownCloud-`FakeFolder`-style) covers.
- pjdfstest will report failures in chmod/chown/symlink/hardlink/xattr tests — **expected**:
  our sync mount intentionally doesn't expose POSIX permissions or links.
- fsx runs with mmap disabled (`-W -R`) — FUSE mmap is out of scope for now.
- fsx (Apple's exerciser) is **APSL-2.0** — copyleft, so we **fetch it at test time** from
  FreeBSD's standalone copy rather than vendoring it, keeping our repo/artifacts permissive
  (§9). Downloading + running a tool in CI is not redistribution.
