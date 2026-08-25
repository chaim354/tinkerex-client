# CLAUDE.md — tinkerex-client

Project context for an AI coding agent (Claude Code) and human developers.

> ## READ THIS FIRST: this file was inherited, not audited
>
> **Everything below was extracted from `chaim354/tinkerex` CLAUDE.md on 2026-08-25** and has
> **NOT been verified against this repository's current code.** It was written while these crates
> lived inside the Rails repo, and it is a *partial* description — the maker app's CLAUDE.md only
> ever recorded what a Rails-side task needed to know about the client. Crate-level invariants,
> the Tauri app's own structure, and anything learned since extraction are **not here**.
>
> **Treat every rule below as a lead, not as ground truth.** Verify against the code before
> relying on one. As each area is verified, rewrite it in place and delete this warning for that
> section.
>
> Existing per-crate docs are more current than this file and should be read first:
> `client/fs-fuse/BUILD.md`, `client/fs-cfapi/BUILD.md`, `client/fs-winfsp/BUILD.md`,
> `client/fs-fskit/BUILD.md`, `client/maker-app/README.md`, `client/maker-app/SETUP.md`,
> `client/test-conformance/README.md`.

---

## 1. What this is

The **client half** of a local-first file sync engine: Rust crates plus a Tauri desktop app.
Extracted from `chaim354/tinkerex` on 2026-08-20 with its own history (166 commits via
`git filter-repo`), its own CI, and its own runner registration. The `client/` path prefix inside
this repo was kept deliberately.

| crate | role |
|---|---|
| `fs-provider` | Local SQLite tree + FastCDC chunk store. The core: `stat`/`readdir`/`open` answer from local metadata, so **the WAN is never in the metadata path**. |
| `fs-fuse` | Linux mount adapter (`fuser`) |
| `fs-cfapi` | Windows mount adapter — **the default**, driverless Cloud Filter API |
| `fs-winfsp` | Windows alternative behind a feature flag; mounts a **drive letter**, not a path |
| `fs-webdav` | macOS mount adapter (native `mount_webdav`) |
| `fs-fskit` | macOS FSKit bridge — **not a shipping mount path**: the Rust side builds and unit-tests, but the Swift extension that would actually mount it is **stubbed pending a paid Apple Developer membership** |
| `sync-client` | HTTP client for the sync server's v1 API |
| `sync-engine` | The daemon: `push` / `pull` / `hydrate` / `sync_once` |
| `maker-app` | The Tauri desktop app |
| `maker-app-core`, `juicefs-backend` | **Leftovers from the abandoned mount path, reference only** — see §4 |
| `test-conformance` | Battle-tested external tools driven against our real mounts |

All three shipping mounts are live over the same `Provider` and verified byte-for-byte against a
real deployed server.

---

## 2. Architecture

> **A remote-mounted filesystem does NOT work over the WAN.** rclone/JuiceFS mount = synchronous
> metadata round-trips → ~1 s/file (measured). Every production system uses **local metadata +
> async sync + on-demand hydration**. That pivot is the whole reason this architecture exists.
> **Do not go back to a remote mount.**

- **Local state:** the tree (`fs-provider`'s `nodes`/`manifest_chunks`, each node carrying its
  server `node_id`), a local content-addressed chunk cache, and sync state (cursor + outbox).
- **Push:** `export_tree` → per file `commit` (begin → upload missing chunks → finalize); dirs via
  `mkdir`.
- **Pull:** changelog delta since cursor → `apply_remote` into local SQLite → dehydrate changed
  placeholders. The delta is **paged** and full-resyncs when the cursor is below the server's
  `oldest_seq` (pruned-gap detection).
- **Hydrate:** on read, fetch missing chunks via presigned GET → local cache.
- **Conflicts:** each commit sends its `base_version_id`; if it is no longer current the server
  records a conflict (both versions kept) and the client prompts to keep one or split into two
  files.
- **Chunk bytes move client-to-storage directly via presigned URLs — never through Rails.**
- **Change notification:** ActionCable → `{head_seq}` on commit (~39 ms measured).

The client holds **only a device token** (256-bit). No storage credentials — it gets short-lived,
per-object presigned URLs.

---

## 3. Licensing — a hard constraint

- **Permissive only: MIT / Apache-2.0 / BSD.** Our crates are Apache; FastCDC is MIT; SQLite is
  public domain.
- **FUSE drivers (WinFsp GPL, macFUSE) are USER-INSTALLED, never bundled** — so no copyleft ends
  up in our binaries. On Windows prefer native **CfAPI**, which needs no driver at all. This is
  the reason CfAPI is the default rather than WinFsp.

---

## 4. Roads not taken (do not revive)

- **JuiceFS was abandoned:** chatty WAN metadata, no internal ACL (multi-tenancy would need a
  gateway or volume-per-tenant), and FUSE-driver licensing. `juicefs-backend` and
  `maker-app-core` are leftovers kept for reference only.
- **Edge-SQLite (Cloudflare D1, Turso) cannot back JuiceFS** — no driver plus chatty-metadata
  latency. They would fit a coarse-manifest model, but D1/Durable Objects are Cloudflare-locked
  and would break self-hosting.
- **FUSE-T's FSKit build cannot mount on macOS 26** — silent failure, the System Settings toggle
  is greyed out. That is why macOS ships WebDAV.

---

## 5. Build and run

Deps are **strictly target-gated** so each OS compiles only its own.

```sh
# Crate tests (hermetic)
(cd client/fs-provider && cargo test)   # chunking / posix / convergence / concurrency / caching
(cd client/sync-client && cargo test)

# sync-engine integration needs a running sync server (skips cleanly without one).
# The server is chaim354/tinkerex-sync, NOT the maker app -- the maker app has not served
# /v1/commit since embedded chunk was retired on 2026-08-25.
(cd client/sync-engine && SYNC_IT_SERVER_URL=http://localhost:3002 cargo test --test integration)

# Live cross-stack demos (SERVER_URL points at a running sync server, default port 3002)
(cd client/sync-client && SERVER_URL=http://localhost:3002 cargo run --example e2e)
(cd client/sync-client && SERVER_URL=http://localhost:3002 cargo run --example multi_device)
(cd client/sync-client && SERVER_URL=http://localhost:3002 cargo run --example nudge)
(cd client/sync-engine && SERVER_URL=http://localhost:3002 cargo run --example roundtrip)
(cd client/sync-engine && SERVER_URL=http://localhost:3002 cargo run --example daemon_loop)
(cd client/sync-engine && SERVER_URL=http://localhost:3002 cargo run --example presigned_roundtrip)

# External conformance -- real tools against our real mounts
client/test-conformance/webdav-rclone.sh      # rclone <-> WebDAV round-trip (macOS + Linux)
client/test-conformance/fsx-fuse.sh           # fsx data-integrity exerciser <-> FUSE (Linux)
client/test-conformance/pjdfstest-fuse.sh     # pjdfstest POSIX map <-> FUSE (Linux, informational)

# Mount fs-fuse locally
./target/debug/maker-mount ~/MakerDrive /tmp/maker.db /tmp/maker-chunks
```

**Per-OS gotchas:**
- **macOS** → WebDAV. `fs-fuse` on macOS needs FUSE-T plus an `osxfuse`→`fuse-t` pkg-config alias
  and an rpath — see `client/fs-fuse/BUILD.md`.
- **Linux** → FUSE. `auto_unmount` needs `user_allow_other` in `/etc/fuse.conf` plus `/dev/fuse`
  access.
- **Windows** → CfAPI by default. WinFsp needs LLVM/libclang to build and `winfsp-x64.dll` on
  PATH at runtime.

**TWO-DEVICE REAL-MOUNT E2E LIVES PARTLY IN THE OTHER REPO, DELIBERATELY.**
`two-device-fuse.sh` and `two-device-webdav-macos.sh` boot their **own local Rails server** (they
`cd` upward to find `bin/rails`), so they must be run **from the maker app repo's root** even
though the scripts live here — which is also why that repo's `nightly.yml` keeps those two legs,
via a checkout-and-symlink trick that file documents. `two-device-cfapi.ps1` targets the
**deployed** server, needs no local Rails, and lives entirely in this repo's CI.

---

## 6. CI

- **SELF-HOSTED runners only — never GitHub-hosted.** Every job uses
  `runs-on: [self-hosted, linux|macos|windows]`. GitHub bills *only* hosted runners
  (`ubuntu/macos/windows-latest`; macOS 10×, Windows 2×). **Never add `*-latest`.**
- **This repo has ONE registered runner: `charless-MBP-maker-mac` (macOS, ARM64), as of
  2026-08-24.** That is what made the `Rust client` workflow start actually running (~90 s)
  instead of queueing forever. **Verify rather than trusting that date** —
  `gh api repos/:owner/:repo/actions/runners` with an **admin-scoped** token (it 403s from inside
  Actions, since `administration` is not a permission a workflow `GITHUB_TOKEN` can hold).
- **`flasher-maker-linux` and `CHAIMDESK-maker-windows` are registered NOWHERE.** Linux and
  Windows coverage is **skipped, not passing.** A job matching no online runner does not fail — it
  queues 24 h at 0 elapsed and is auto-cancelled, which once left a PR UNSTABLE for five weeks.
  Gate such jobs on a repo variable rather than letting them queue.
- Self-hosted machines are **persistent**: make dep-install steps idempotent and do not assume the
  clean toolchain of a hosted image.

---

## 7. Related repos

| repo | role |
|---|---|
| `chaim354/tinkerex-sync` | The **sync server** these crates sync against — chunk/version/changelog engine. Runs as the `sync` Kamal accessory. |
| `chaim354/tinkerex` | The **maker app** (Rails) — projects, inventory, model library. Owns `docs/sync-protocol.md`, the normative wire spec. It is a *sibling client* of the sync server, not this repo's server. |

Clone siblings alongside this one (`../tinkerex`, `../tinkerex-sync`) — that is the convention
both repos' CI uses.
