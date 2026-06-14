# Building / running the CfAPI mount (Windows)

`fs-cfapi` is the **driverless** Windows mount: it uses the OS's **Cloud Filter API**
(`cldapi.dll`, the API OneDrive uses) instead of the GPL WinFsp driver. Files live in a
normal **sync-root folder** under your profile (not a drive letter), as placeholders that
**hydrate on demand** when read in Explorer.

> **Status — Phase 1 (download/read half), runtime-UNVERIFIED on real hardware.** The FFI
> **typechecks for the Windows target** (built on macOS via
> `cargo check --target x86_64-pc-windows-gnu`, and the `cfapi-windows` CI job), but its
> *runtime* behavior against a live sync root needs the smoke test below. Local-edit→upload capture is
> **Phase 2** (a USN-journal watcher → `Provider::ingest_bytes` → push). See
> `docs/plan-windows-cfapi-migration.md`. The cross-platform pure logic (`FileIdentity`,
> FILETIME, placeholder decisions) IS unit-tested (`cargo test -p fs-cfapi`, runs anywhere).

## Requirements
- **Windows 10 1709+ / Windows 11** (Cloud Filter API). NTFS volume.
- A Rust toolchain (MSVC). **No driver, no SDK, no LLVM** — unlike WinFsp, CfAPI needs none
  of that; the `windows` crate provides the bindings and `cldapi.dll` ships with Windows.

## Build
```powershell
# Pure-logic tests (also run on macOS/Linux CI):
cargo test  --manifest-path client/fs-cfapi/Cargo.toml

# The standalone local mount runner + the engine's CfAPI sync daemon:
cargo build --manifest-path client/fs-cfapi/Cargo.toml --bins        # maker-cfapi
cargo build --manifest-path client/sync-engine/Cargo.toml --bins     # maker-cfsyncd (full sync)
```

## Smoke test on a real Windows machine

You need a running sync server + a device token (register a device via the API or the Maker
app). Then run the **CfAPI sync daemon** against an empty folder:

```powershell
# <root> must be an empty, non-existent-or-empty folder under your profile (NTFS), e.g.:
$root = "$env:USERPROFILE\MakerCF"
cargo run --manifest-path client/sync-engine/Cargo.toml --bin maker-cfsyncd -- `
  $root `
  "$env:TEMP\maker-cf.db" `
  "$env:TEMP\maker-cf-chunks" `
  "http://<server-host>:3001" `
  "<device-token>"
```

What to verify (Phase 1):

1. **Populate.** Open `%USERPROFILE%\MakerCF` in **File Explorer**. The library tree appears,
   with the cloud/online-only overlay on files (placeholders — metadata only, no bytes yet).
   Folders + filenames + sizes + **modification dates** match the server (mtime is synced).
2. **Hydrate on read.** Double-click a file (or `type`/`Get-Content` it). Explorer shows a
   brief "downloading" state; the bytes are fetched on demand (presigned GET → chunk cache →
   served via `CfExecuteOperation`), and the file opens with the correct content. Confirm a
   hash match against the source on another device.
3. **Remote-change invalidation.** Edit the same file on **another device** and let it sync.
   Within a sync tick (nudge/poll) the placeholder is **dehydrated** (`CfUpdatePlaceholder`),
   so the next read on Windows re-fetches and shows the **new** content — without the app
   bumping mtime (the Option B cache-drop, CfAPI-side).
4. **Teardown.** Press Enter in the console: the sync root is **disconnected + unregistered**
   (`CfDisconnectSyncRoot` / `CfUnregisterSyncRoot`); the folder's special status clears.

> A bare local mount (no server) is also available — `maker-cfapi <root> <db> <chunks>` —
> which serves only already-cached chunks (a miss errors); useful to test registration +
> populate in isolation.

## Known gaps (flagged in `src/win.rs`)
- **Phase 2 (edits) — DONE:** editing an existing synced file is captured via
  `NOTIFY_FILE_CLOSE_COMPLETION` (content-compared against the provider to filter our own
  hydration writes) → `Provider::ingest_bytes` → the sync loop pushes it. To test: open a
  synced file in `MakerCF`, edit + save, and confirm it appears on another device. The console
  prints `cfapi: local edit captured …`.
- **Phase 2b (not yet):** *new* files dropped into the folder, *deletes*, and *renames* aren't
  captured yet (new files have no node identity; delete/rename need the pre-op ACK callbacks).
- On-demand directory population (`FETCH_PLACEHOLDERS`) is disabled — Phase 1 fully populates
  the tree up front (simpler; fine for typical maker libraries).
- The Ctrl-C handler is a stub; use the Enter prompt (or kill the process — `Drop` still
  unregisters the root on a clean exit).
- Ownership/POSIX mode are intentionally not synced on Windows (no NTFS meaning).
