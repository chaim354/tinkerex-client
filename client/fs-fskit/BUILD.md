# Building / running the FSKit mount (macOS) — and the when-the-account-arrives checklist

`fs-fskit` is the **native, kext-free macOS mount** over the same `Provider` — the
principled peer of `fs-fuse` (Linux FUSE) and `fs-cfapi` (Windows Cloud Filter). It uses
Apple's **FSKit** framework (`FSUnaryFileSystem`/`FSVolume`), the FUSE successor: a full
userspace filesystem the kernel mounts, with **hydrate-on-read** baked into the Rust bridge.
See `docs/plan-macos-fskit-migration.md` for the design and the FSKit-vs-File-Provider call.

> **Status — SKELETON. The live mount is account-gated and does NOT build/run yet.**
> The Rust side that *can* build *does*: the cross-platform pure logic and the FFI bridge
> are written + **unit-tested** (`cargo test -p fs-fskit --features ffi`, 23 tests, run on
> any OS). The Swift `.appex`, its `Info.plist`/entitlements, signing, notarization, and
> system registration need the **FSKit SDK + a paid Apple Developer account** and are
> **stubbed** with `TODO(dev-account)` markers (`swift/`). `FsKitMount::start` returns
> `MountError::NeedsDeveloperAccount` today. **macOS ships WebDAV (`fs-webdav`) until this
> lands** — keep it behind a cargo feature as the fallback (the same WinFsp→CfAPI pattern).

## What compiles + is tested today (no account needed)

```sh
# Pure logic + FFI bridge dispatch, against an in-memory SqliteProvider (no Swift/FSKit):
cargo test --manifest-path client/fs-fskit/Cargo.toml --features ffi

# Default (pure logic only — ItemId<->ino, item_type, decide_change):
cargo test --manifest-path client/fs-fskit/Cargo.toml
```

- **`src/lib.rs`** — `ItemId` ↔ `ino` mapping (root pinned to `ROOT_INO`, `0` = invalid
  sentinel), `item_type(Kind)`, and `decide_change(RemoteNodeEvent) -> FsKitChange` (the
  active-invalidation decision: data vs metadata vs removed vs none — the FSKit analogue of
  FUSE `notify_inval_inode` / CfAPI `PlaceholderAction`).
- **`src/ffi.rs`** (`--features ffi`) — `FsKitBridge` over `Arc<SqliteProvider>` + an
  optional `Hydrate` hook; dispatches every FSKit op to the `Provider`
  (`lookup`/`getattr`/`enumerate`/`read`-with-hydrate-on-miss-retry/`create`/`mkdir`/`write`/
  `flush`/`truncate`/`unlink`/`rmdir`/`rename`), an `errno()` mapping, and the
  `into_raw`/`from_raw`/`free_raw` opaque-handle lifecycle. Sketched `#[no_mangle] extern
  "C"` shims (`fskit_lookup`/`_read`/`_free`) use plain C scalars + byte buffers.
- **`src/mount.rs`** — `FsKitMount` lifecycle shape (`start`/`is_running`/`stop`/`Drop`)
  mirroring `WebdavMount`/`CfApiMount`. `start` builds the shared provider, then refuses.

## What is stubbed (needs the account + SDK)

Everything under **`swift/`**, all marked `TODO(dev-account)`:
- `MakerFSExtension.swift` — `FSUnaryFileSystem` + `FSVolume.Operations` conformances.
- `Bridge.swift` — `@_silgen_name` decls of the Rust `fskit_*` C ABI + the `RustBridge` wrapper.
- `Info.plist` — extension-point + principal-class declaration.
- `MakerFS.entitlements` — FSKit + App Sandbox entitlements.

---

# CHECKLIST — when the paid Apple Developer account arrives

Do these roughly in order. **Step 0 first** — it de-risks the whole effort.

### 0. De-risk the greyed-toggle / signing issue with a hello-world extension
FUSE-T's FSKit build couldn't mount on macOS 26 (silent fail; the System-Settings extension
toggle was greyed — see `docs/plan-macos-fskit-migration.md` §7.1). That is most likely an
**entitlement/signing/provisioning** problem that would bite our extension identically.
**Before touching the bridge**, build the minimal Apple "FSKitSample"/hello-world extension
(or a trivial one of our own) that mounts an **empty** read-only volume, sign it with the new
Developer ID, and confirm it (a) appears + can be enabled in System Settings ▸ General ▸
Login Items & Extensions ▸ File System Extensions, and (b) actually mounts. If it doesn't,
fix signing/provisioning here — not against our full code.

### 1. Install the toolchain
- **Xcode** (the version that ships the **FSKit** SDK — Sequoia 15.4+ / macOS 26 era) on the
  self-hosted macOS runner + your dev machine.
- Confirm `import FSKit` resolves in a scratch Swift file.

### 2. Set your real identifiers + signing
- Replace every `TODO(dev-account)` placeholder in `swift/Info.plist` +
  `swift/MakerFS.entitlements`:
  - `CFBundleIdentifier` → your reverse-DNS id under your Team (e.g. `com.<you>.maker.MakerFS`).
  - the FSKit extension-point id + the `com.apple.developer.fskit.fsmodule` entitlement key —
    **confirm the exact strings against the installed SDK** (the stubs are best-guess).
  - the app group (`group.com.<you>.maker`) for host-app ↔ extension XPC.
- In your provisioning profile, **request/authorize the FSKit entitlement** (it is privileged).
- Set the Developer ID signing identity for the `.appex` and the host app.

### 3. Create the Xcode targets
- A **File System Extension** target (the `.appex`) using `swift/*.swift` + `Info.plist` +
  `MakerFS.entitlements`.
- A host **app** target that embeds the `.appex` (extensions ship inside an app). The Tauri
  `client/maker-app` bundle is the natural host — embed the `.appex` in its `.app`.
- Link the **Rust staticlib**: add a `crate-type = ["staticlib"]` (or `cdylib`) target to
  `client/fs-fskit/Cargo.toml`, build it with `--features ffi` for the mac target(s)
  (`aarch64-apple-darwin`, `x86_64-apple-darwin`), and add the resulting `.a`/`.dylib` +
  a bridging header to the extension target so the `@_silgen_name` `fskit_*` symbols resolve.

### 4. Finish the FFI surface (Rust + Swift together)
- Upgrade `src/ffi.rs`'s sketched scalar/buffer shims to the **FSKit-struct-typed**
  signatures (FSItem identifiers, the reply-buffer/`replyHandler` shapes) now that the SDK
  headers are in hand. Add the missing shims: `fskit_getattr`, `fskit_enumerate`,
  `fskit_create`, `fskit_mkdir`, `fskit_write`, `fskit_flush`, `fskit_truncate`,
  `fskit_unlink`, `fskit_rmdir`, `fskit_rename`. Each just calls the matching `FsKitBridge`
  method (already real + tested) and translates the `Result` via `errno()`.
- Fill in the `swift/Bridge.swift` decls + wrappers to match, and the
  `MakerFSExtension.swift` `FSVolume.Operations` methods (the comment block is the map).

### 5. Wire into the engine (mirror the other mounts)
- In `client/sync-engine/src/daemon.rs`, add an `FsKitMount` (copy `WebdavMount`: build the
  shared `Arc<SqliteProvider>` + `bind_library`, spawn `sync_loop` + `nudge_loop`, build the
  hydrate closure). Replace `fs_fskit::FsKitMount::start`'s `NeedsDeveloperAccount` early
  return with: build `ffi::FsKitBridge` → `into_raw` → hand the pointer to the registered
  `.appex` (via XPC/app-group) → return the live `Self`. On `stop`/`Drop`: signal the
  extension to unmount, then `free_raw` + join the threads.
- Export it from `sync-engine`'s `lib.rs` behind `#[cfg(target_os = "macos")]`, and in
  `client/maker-app/src-tauri/src/lib.rs` flip the macOS `Mount` alias from
  `sync_engine::WebdavMount` to `sync_engine::FsKitMount` **behind a cargo feature**
  (`fskit-backend`), keeping WebDAV as the default/fallback for one release + for macOS
  versions below the FSKit floor.
- Wire **active invalidation**: on a pulled change the engine computes
  `fs_fskit::decide_change(...)` and signals the extension to purge the kernel cache for the
  item (the FSKit analogue of FUSE `notify_inval_inode`) — making macOS `invalidate(ino)` a
  real call instead of the WebDAV timeout no-op.

### 6. Register + run the extension
- Build + sign the host app embedding the `.appex`. Launch it once so macOS discovers the
  extension, then enable it in **System Settings ▸ General ▸ Login Items & Extensions ▸ File
  System Extensions**.
- Notarize for distribution (`notarytool`) once it works locally.

### 7. Smoke-test against the deployed server
You need a running sync server + a device token (register via the API or the Maker app).
Bring up the FSKit volume (via the app, or a `maker-fskitd` binary added in step 5) pointed
at the deployed Rails server, then verify — the **same checks the CfAPI smoke uses**:

1. **Populate.** The library tree appears at the mount point in Finder; folders, filenames,
   sizes, and **modification dates** match the server (real mtime is synced end-to-end).
2. **Hydrate on read.** Open a file; the bytes are fetched on demand (the Rust bridge's
   hydrate-on-miss → presigned GET → chunk cache → retry). Confirm a **hash match** against
   the source on another device.
3. **Write capture.** Create/edit/delete/rename a file in the mount; confirm it pushes and
   appears on another device (the direct `Provider` write path — no NOTIFY/ACK dance).
4. **Remote-change invalidation.** Edit the file on another device; within a sync tick the
   kernel cache is purged and the next read shows the **new** content (the `decide_change`
   active invalidation).
5. **Teardown.** Unmount via the app; the volume disappears and the bridge is freed.

Then run the **external conformance** suites already in `client/test-conformance/` against
the FSKit mount (the macOS analogue of the FUSE/WebDAV runs): `rclone` round-trip + `fsx`
data-integrity. Add a `macos-fskit-smoke` CI job on the self-hosted macOS runner (the peer of
`windows-smoke` / `mount-conformance`) that mounts the volume + asserts a write round-trips to
the server.

### 8. Decide the macOS version floor + keep WebDAV
FSKit is recent (Sequoia 15.4+, mature in macOS 26). Setting the floor drops older users, so
**keep WebDAV as the fallback** behind the `fskit-backend` feature gate (a config choice, not
a fork) until FSKit's third-party stability is proven on the deployment base.
