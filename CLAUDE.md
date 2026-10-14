# CLAUDE.md — tinkerex-client

Project context for an AI coding agent (Claude Code) and human developers.

> **Provenance.** This file was seeded from `chaim354/tinkerex`'s CLAUDE.md on 2026-08-25 (that
> repo only ever recorded the *Rails-side* view of these crates) and **audited against this
> repository's code the same day**. Every inherited claim held: the crate roles, `fs-cfapi` as the
> Windows default, the `winfsp-backend` opt-out, the FSKit stub, the FUSE-T/macOS-26 note, the
> per-OS build gotchas, Apache-2.0 across the crates, and all six conformance scripts. What the
> seed was missing — the workspace layout, the real test counts, the four CI workflows — is filled
> in below, and **two stale comments found in `sync-engine` are recorded in §0.**
>
> Still thin, and honestly so: this file describes the crates from the *outside*. Crate-internal
> invariants (what `fs-provider`'s SQLite schema guarantees, how `sync-engine` orders a pull) are
> not here. The per-crate `BUILD.md`/`README.md` files are more current on their own subjects.

## 0. Orientation — what will trip you up

1. **THERE IS NO CARGO WORKSPACE.** Eleven crates, eleven `Cargo.toml` files, **no `[workspace]`
   root** — so there is no `cargo test --all`, no shared `Cargo.lock`, and no shared `target/`.
   Every command is `cd client/<crate> && cargo …`. Every cheatsheet line below reflects that; it
   is not a stylistic choice you can shorten.
2. **`fs-cfapi` IS the shipping Windows mount, and two comments in `sync-engine` still say
   otherwise.** `maker-app/src-tauri/src/lib.rs:106-111` sets `type Mount = sync_engine::CfApiMount`
   by default, with `WinfspMount` behind `--features winfsp-backend`, and its comment records the
   flip as done "after the live smoke + byte-for-byte" verification. But
   **`sync-engine/Cargo.toml` still says "WinfspMount stays the default until CfApiMount is proven
   on real hardware", and `sync-engine/src/daemon.rs:189` still says "Windows uses `WinfspMount`."**
   Both are stale. Believe the Tauri app; the sync-engine comments describe the pre-flip world.
   *(Found by this audit, 2026-08-25. Left in place because editing them was outside its scope.)*
3. **`cargo test` in `client/fs-fuse` FAILS ON macOS, by design, and that is not a broken
   checkout.** `fuser`'s build script probes pkg-config for `fuse`, and FUSE-T ships `fuse-t.pc`
   instead — the alias fix is in `client/fs-fuse/BUILD.md`. The other crates test clean on macOS.
   (`BUILD.md` says `fuser` 0.14; the manifest pins 0.17. Minor, but it is the file you will be
   reading while fixing this.)
4. **`fs-webdav` is deliberately NOT target-gated**, unlike the other mount crates. It is a WebDAV
   *server* over `fs-provider` — "works on any OS with a WebDAV client" — and only the
   `mount_webdav` call is macOS-specific. Do not "fix" it into a `cfg(target_os = "macos")` block.

---

## 1. What this is

The **client half** of a local-first file sync engine: Rust crates plus a Tauri desktop app.
Extracted from `chaim354/tinkerex` on 2026-08-20 with its own history (166 commits via
`git filter-repo`), its own CI, and its own runner. The `client/` path prefix was kept deliberately.

| crate | role |
|---|---|
| `fs-provider` | Local SQLite tree + FastCDC chunk store. The core: `stat`/`readdir`/`open` answer from local metadata, so **the WAN is never in the metadata path**. |
| `fs-fuse` | Linux mount (`fuser`). Gated to `cfg(target_os = "linux")` **by its consumer**, `sync-engine` — its own manifest is unconditional. |
| `fs-cfapi` | Windows mount — **the shipping default**. Driverless Cloud Filter API; mount point is a folder, not a drive letter. "Replaces the GPL-WinFsp path." |
| `fs-winfsp` | Windows legacy, behind the Tauri app's `winfsp-backend` feature, "kept for one release post-flip". Mounts a **drive letter**. Needs `winfsp-x64.dll` on PATH — the crate deliberately does not enable winfsp-sys's `system` feature, so it builds with bundled headers. |
| `fs-webdav` | macOS mount (native `mount_webdav`). Cross-platform by construction — see §0.4. |
| `fs-fskit` | macOS FSKit bridge — **not a shipping mount path**. The Rust side builds and unit-tests; the Swift extension is **stubbed pending a paid Apple Developer membership**. |
| `sync-client` | HTTP client for the sync server's v1 API. |
| `sync-engine` | The daemon: `push` / `pull` / `hydrate` / `sync_once`, plus the per-OS mount selection. |
| `maker-app` | The Tauri desktop app, product name **Tinkerex**, identifier `com.tinkerex.desktop` since D1 (crate lives at `maker-app/src-tauri`; its on-disk data dir is still `maker-app`, a rename deferred on purpose). |
| `maker-app-core`, `juicefs-backend` | **Leftovers from the abandoned mount path, reference only** — §4. |
| `test-conformance` | Battle-tested external tools driven against our real mounts. |

All three shipping mounts are live over the same `Provider` and verified byte-for-byte against a
real deployed server.
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
---

## 5. Build and run

**No workspace (§0.1) — every command is per-crate.** Measured on macOS, 2026-08-25:

| crate | `cargo test` |
|---|---|
| `fs-provider` | **83 passing** across 6 test binaries (chunking, posix, convergence, concurrency, caching) |
| `sync-engine` | **92 passing** |
| `sync-client` | **43 passing** (2026-10-13; was 39) |
| `fs-webdav` | **8 passing** |
| `fs-fuse` | **build failure on macOS by design** — §0.3 |
| `maker-app/src-tauri` | **27 passing** (2026-10-13), plus `node --test client/maker-app/test` **26 passing** — neither runs in CI (§6) |

**The server-backed `sync-engine` suites run against `chaim354/tinkerex-sync` only with two env
knobs, and until 2026-09-30 could not run against it at all.** Every suite and example enrolled
through `ServerClient::enroll` → `POST /v1/test/users` + `POST /v1/devices/login`, which are the
MAKER APP's routes; the sync server never served them, so the line this file used to carry here
("the server is chaim354/tinkerex-sync") described a run nobody could have made. Now:
`SYNC_IT_ADMIN_TOKEN=<the server's SYNC_ADMIN_TOKEN>` switches `enroll` to the sync server's own
door (`POST /v1/admin/libraries`, one fresh library per email, `head_seq` read back from
`whoami`), and a second device joins as a sibling through `create_device` instead of a password
login. The server must be booted with **`SYNC_VERIFY_FINALIZE=0`**: these suites move chunk bytes
through `SharedDirIO`, never the server's store, and finalize's default verify refuses a commit
whose bytes it cannot see. Measured against the Rust server (tinkerex-sync `crates/tinkerex-sync`,
booted by the maker repo's `SYNC_IMPL=rust bin/sync-server`), Linux, 2026-09-30:

| suite | result |
|---|---|
| `integration` | 25 passing |
| `fault_injection` | 11 passing |
| `two_device_e2e` | 9 passing |
| `parallel_hydration` | 8 passing |
| `offline_availability` | 5 passing |
| `adversarial_sync`, `concurrency_load`, `multifiletype_conformance` | 4 passing each |
| `scale`, `reconcile_sweep` | 1 passing each |

`device_login_adds_a_device_to_an_existing_library` prints SKIPPED under the admin token: password
login is the maker app's, and the run's log says so rather than passing silently.

**THE DESKTOP APP SIGNS IN TO THE MAKER APP AND SYNCS AGAINST THE GRANT IT IS HANDED (D1,
2026-10-13).** It used to post `POST /v1/devices/login` to the typed URL and sync against that same
URL, which no live server could answer: the maker app serves the login and no sync route, the sync
server the reverse. The typed URL is now the Tinkerex (maker) server, whose login answer adds
`library: {backend, syncable}` and, when syncable, `sync: {url, device_token, user_id, device_id,
head_seq}` — a sibling device on the user's own sync-server library. `credentials.json` holds
`{maker_token, sync_url, sync_token}` under the unchanged `(typed server, email)` key; sync-route
commands resolve through `sync_endpoint`, the `maker_*` ones through `maker_endpoint`, and the
local tree stays keyed by the typed server. **A legacy bare-string value still means "a sync token
for the typed server" and is never rewritten** (the `tinkerex-sync device issue` workaround keeps
working); an answer with neither field is an older server, read the same way; `syncable: false`
mounts nothing and says why in one sentence. `http://` grant URLs are used as given: the LAN
deployments are plain HTTP. `device_login` still keeps the MAKER token on its client. **The
suites' two modes above are unchanged** — their maker-app mode does not follow the grant.

```sh
(cd client/fs-provider && cargo test)
(cd client/sync-client  && cargo test)
(cd client/maker-app/src-tauri && cargo test)   # the desktop app's backend -- not in CI
node --test client/maker-app/test               # its JS, from the repo root -- not in CI

# sync-engine integration needs a running sync server (skips cleanly without one).
# Against chaim354/tinkerex-sync (the Rust binary or the Ruby host), boot it with
# SYNC_VERIFY_FINALIZE=0 and pass its admin token -- see the paragraph above.
(cd client/sync-engine && SYNC_IT_SERVER_URL=http://localhost:3002 SYNC_IT_ADMIN_TOKEN=... cargo test --test integration)
# Against the maker app (which serves /v1/test/users + /v1/devices/login), no admin token:
(cd client/sync-engine && SYNC_IT_SERVER_URL=http://localhost:3000 cargo test --test integration)

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
- **macOS** → WebDAV. `fs-fuse` needs FUSE-T plus an `osxfuse`→`fuse-t` pkg-config alias and an
  rpath — `client/fs-fuse/BUILD.md` (§0.3).
- **Linux** → FUSE. `auto_unmount` needs `user_allow_other` in `/etc/fuse.conf` plus `/dev/fuse`.
- **Windows** → CfAPI (§0.2). WinFsp needs LLVM/libclang to build and `winfsp-x64.dll` on PATH.

**TWO-DEVICE REAL-MOUNT E2E LIVES PARTLY IN THE OTHER REPO, DELIBERATELY.**
`two-device-fuse.sh` and `two-device-webdav-macos.sh` boot their **own local Rails server** (they
`cd` upward to find `bin/rails`), so they must be run **from the maker app repo's root** even
though the scripts live here — which is why that repo's `nightly.yml` keeps those legs.
`two-device-cfapi.ps1` targets the **deployed** server, needs no local Rails, and lives entirely in
this repo's CI.

---

## 6. CI — four workflows, and most of them cannot run

- **SELF-HOSTED runners only, never `*-latest`.** GitHub bills only hosted runners; self-hosted are
  free. Verified: no `*-latest` appears in any workflow here.

| workflow | trigger | runner |
|---|---|---|
| `rust-client.yml` | push, PR, dispatch (one slice on demand) | macOS — **the only one that actually runs** |
| `build-app.yml` | push, dispatch | `[self-hosted, linux]` |
| `install-app-on-runners.yml` | `workflow_run`, dispatch | `[self-hosted, linux]` |
| `cross-os-e2e.yml` | dispatch only — **its nightly schedule is commented out**, gated on `CI_RUNNERS` | all three OSes, `continue-on-error` |

- **ONE runner is registered: `charless-MBP-maker-mac` (macOS, ARM64), online.** Verify rather than
  trust: `gh api repos/chaim354/tinkerex-client/actions/runners`, with an **admin-scoped** token —
  `administration` is not a permission a workflow `GITHUB_TOKEN` can hold, so it 403s from inside
  Actions.
- **Consequence: the three linux/windows workflows queue-and-cancel rather than fail.** A job
  matching no online runner sits 24 h at 0 elapsed and is auto-cancelled, which reads as UNSTABLE,
  not red — the failure mode that once left a PR stuck for five weeks in the other repo. **Linux
  and Windows coverage is skipped, not passing.**
- Self-hosted machines are **persistent**: make dep-install steps idempotent and do not assume the
  clean toolchain of a hosted image.

---

## 7. Related repos

| repo | role |
|---|---|
| `chaim354/tinkerex-sync` | The **sync server** these crates sync against — chunk/version/changelog engine, 26-endpoint v1 API. Runs as the `sync` Kamal accessory. Has its own CLAUDE.md. Being rewritten in Rust (`crates/tinkerex-sync` there, graded by a recorded corpus); its `/cable` accepts this client's `LibraryChannel` subscription with no `Origin` header, which the Ruby host refused — so the nudge starts working for the daemon at that cutover. |
| `chaim354/tinkerex` | The **maker app** (Rails) — projects, inventory, model library. Owns `docs/sync-protocol.md`, the normative wire spec. A *sibling client* of the sync server, not this repo's server. |

Clone siblings alongside this one (`../tinkerex`, `../tinkerex-sync`) — the convention both repos'
CI uses.
