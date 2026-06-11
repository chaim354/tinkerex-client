# Building / running the WinFsp adapter (Windows)

`fs-winfsp` is the Windows counterpart to `fs-fuse`: it exposes the same
`fs_provider::Provider` as a real drive, via **WinFsp** (instead of FUSE).

> **Status: written, not yet built on Windows.** The maintainer's box is macOS,
> where `winfsp` doesn't compile, so this adapter is a faithful first cut against the
> real `winfsp` 0.13 API that **needs a Windows build pass**. The `winfsp-windows` CI
> job (`.github/workflows/rust-client.yml`) compiles it on every push; expect to fix a
> few things on the first green. Known gaps are marked `LIMITATION:` in `src/lib.rs`
> (chiefly: no truncate/shrink — the `Provider` has no truncate yet).

## Prerequisites (Windows)

1. **WinFsp** — https://winfsp.dev → run the installer (driver **and** SDK; the SDK
   headers/libs are what `winfsp-sys` builds against). Or `choco install winfsp`.
2. **Rust** (MSVC toolchain) — https://rustup.rs
3. **LLVM/clang** — `winfsp-sys` uses `bindgen`, which needs `libclang`:
   `choco install llvm`, then set `LIBCLANG_PATH=C:\Program Files\LLVM\bin`.

## Build

```powershell
cd client\fs-winfsp
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --bins
```
If `winfsp-sys` can't find WinFsp, point it at the install dir (registry usually
suffices after the installer; otherwise set the `WINFSP`/install-dir env it reports).

## Run the standalone mount

```powershell
# Mount the local-first provider at drive Z: (or a directory path).
.\target\debug\maker-mount-win.exe Z: C:\maker\tree.db C:\maker\chunks
```
`Z:` should appear in Explorer. Create a folder, drop a file in, open it, rename,
delete. Ctrl-C to unmount (WinFsp also auto-unmounts on process exit).

This proves the adapter against the local tree + chunk store — the same first step
`fs-fuse`'s `maker-mount` was for FUSE. **Once this is solid**, wiring it into the sync
daemon is a small follow-up: in `sync-engine`'s `Daemon`, swap the FUSE mount for
`fs_winfsp::mount_blocking` under `cfg(windows)`, and drop the `cfg(not(windows))` gate
on the Tauri app's `sync-engine` dependency so the mount commands light up on Windows.

## Quick smoke checklist (what to verify on Windows)

- [ ] `cargo build --bins` succeeds (fix any API drift the compiler reports).
- [ ] `maker-mount-win Z: …` mounts; `Z:` is visible in Explorer.
- [ ] `mkdir Z:\proj`, `echo hi > Z:\proj\a.txt`, `type Z:\proj\a.txt` round-trips.
- [ ] `dir Z:\proj` lists entries (incl. `.`/`..`); rename + delete work.
- [ ] Overwriting an existing file is the known-weak path (no truncate yet) — note
      behavior so we can prioritize a `Provider::truncate`.
