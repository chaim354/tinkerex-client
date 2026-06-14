# Run the Maker client (Tauri app) and mount a synced drive

The app registers a device, mounts the local-first filesystem, and runs the sync
daemon (push on save, pull on a `{head_seq}` nudge / poll, hydrate on read) — all
in one process. The mount backend is chosen per platform:

- **macOS:** **WebDAV** — the app serves the tree over localhost and mounts it with the
  built-in `/sbin/mount_webdav`. **No FUSE, no kext, no FSKit** (FUSE-T can't mount on
  macOS 26). The drive shows in Finder under *Locations* as "Maker". This is the default
  and it just works.
- **Linux:** **FUSE** (`fuser`) — mounts natively.
- **Windows:** **CfAPI** (driverless Cloud Filter — the default; no WinFsp/GPL driver, no
  `winfsp-x64.dll`). The mount point is a **folder** (e.g. `%USERPROFILE%\MakerCF`), not a drive
  letter. The legacy WinFsp (drive-letter) backend is still available with
  `--features winfsp-backend`. (Live smoke + byte-for-byte conformance green — Phase 2b B5.)
  NOTE: the rest of this doc predates this flip and still describes Windows as unwired — it needs
  a fuller refresh.

The backend lives in `src-tauri/src/lib.rs` (commands `sync_start`/`sync_stop`/
`sync_status`) via a per-OS `Mount` alias — `sync_engine::WebdavMount` on macOS,
`sync_engine::Daemon` (FUSE) on Linux.

> **macOS build note:** at runtime macOS needs nothing FUSE-related (WebDAV is native).
> But `cargo tauri dev` currently still *compiles* the FUSE `fuser` crate (used on Linux,
> pulled in unconditionally), so the FUSE-T pkg-config alias below is needed to **build**
> on macOS — not to run. (Follow-up: gate `fuser` out of macOS builds.)

---

## macOS / Linux — the real test

### 1. Prerequisites

- **Rust** — https://rustup.rs
- **Tauri CLI** — `cargo install tauri-cli --version "^2.0" --locked` (or use a
  global `cargo-tauri` you already have).
- **A FUSE driver:**
  - **macOS:** **FUSE-T** — https://www.fuse-t.org (no kext; no macFUSE).
  - **Linux:** libfuse (`apt install libfuse-dev`, usually present).
- **Linux only — Tauri's system libraries + libfuse** (Debian/Ubuntu names; see
  https://tauri.app/start/prerequisites for other distros):
  ```sh
  sudo apt update && sudo apt install -y \
    libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
    libfuse-dev pkg-config fuse
  ```
  No pkg-config alias or rpath dance on Linux — libfuse ships the standard `fuse.pc`,
  and `fuser` links it directly. (Skip the macOS FUSE-T block below.)
- **Linux only — allow `allow_other`.** The mount uses `auto_unmount` (clean teardown
  if the app dies), which `fuser`/libfuse couple with `allow_other`, and Debian/Ubuntu
  gate that behind `/etc/fuse.conf`. Without it, Mount fails with *"option allow_other
  only allowed if 'user_allow_other' is set"*. One-time:
  ```sh
  echo 'user_allow_other' | sudo tee -a /etc/fuse.conf
  ```
  Also make sure you can reach `/dev/fuse` (`sudo usermod -aG fuse $USER`, then re-login,
  if a mount returns `permission denied (os error 13)`).
- **macOS only — the FUSE-T pkg-config alias.** `fuser` probes pkg-config for the
  macFUSE names; FUSE-T ships different ones, so alias them once per shell
  (verbatim from `client/fs-fuse/BUILD.md`):
  ```sh
  mkdir -p /tmp/fuse-pc
  for n in osxfuse fuse; do
    cat > /tmp/fuse-pc/$n.pc <<EOF
  prefix=/usr/local
  libdir=\${prefix}/lib
  includedir=\${prefix}/include/fuse
  Name: $n
  Description: FUSE-T compat alias
  Version: 2.9.7
  Libs: -L\${libdir} -Wl,-rpath,\${libdir} -lfuse-t
  Cflags: -I\${includedir}
  EOF
  done
  export PKG_CONFIG_PATH=/tmp/fuse-pc:/usr/local/lib/pkgconfig
  ```
  (The FUSE-T runtime rpath is baked in by `src-tauri/.cargo/config.toml`.)
- **macOS — FUSE-T is no longer needed to run** (the app mounts via WebDAV). This note
  only matters if you specifically want the `fs-fuse`/`maker-mount` path on macOS: FUSE-T
  ≥1.1 defaults to an experimental **FSKit** backend on macOS 26+ that silently never
  mounts (the third-party FSKit extension toggle in System Settings → Login Items &
  Extensions → File System Extensions is frequently greyed/broken). For raw FUSE on
  macOS, install the **NFS-based** FUSE-T (the plain `fuse-t` pkg from
  https://github.com/macos-fuse-t/fuse-t/releases, **not** the `*-fskit` one). The app
  itself sidesteps all of this with WebDAV, so you can ignore it.

### 2. Start the sync server (from the repo root, separate terminal)

```sh
STORAGE_BACKEND=memory bin/rails server -p 3001
```
`memory` keeps chunk bytes in-process — fine for a single-box test. For real
object storage, boot with `STORAGE_BACKEND=s3 …` instead (see CLAUDE.md §13).

### 3. Launch the app

```sh
cd client/maker-app
# macOS: keep PKG_CONFIG_PATH exported from step 1 in this shell
cargo tauri dev
```
First build pulls Tauri + the sync engine (a minute or two); then a window opens.
There's no `npm install` — the frontend is plain HTML served from `src/`.

### 4. Mount and test

In the window:
1. **Sync server URL** — `http://localhost:3001` (default)
2. **Account email** — any email; the *same* email is the *same* library across
   devices.
3. **Mount point** — a directory, e.g. `/tmp/MakerDrive` (created if missing).
4. Click **Mount & sync**. Status flips to `mounted at /tmp/MakerDrive`.

Then, in Finder / a terminal:
```sh
echo "hello maker" > /tmp/MakerDrive/notes.txt      # save → pushed to the server
ls /tmp/MakerDrive
cat /tmp/MakerDrive/notes.txt                        # read → hydrates from storage
```
Click **Unmount** to tear it down cleanly.

> Local state (the SQLite tree + chunk cache) lives under
> `$TMPDIR/maker-app/` — delete it to start fresh.

### Two-device demo (one box)

Run a second instance pointed at a *different* mount point but the *same* email,
and watch a file made in one appear in the other (nudge-driven, ~tens of ms):
```sh
# second terminal (server + PKG_CONFIG_PATH already set)
cd client/maker-app && cargo tauri dev
# mount this one at /tmp/MakerDrive2, same email → same library
```
(For a pure-CLI version of the same thing without the GUI, use the `maker-syncd`
binary — see CLAUDE.md §13.)

### Troubleshooting

- **`could not find system library 'fuse'` at build** — the pkg-config alias
  isn't exported in this shell (step 1).
- **`Library not loaded: @rpath/libfuse-t.dylib`** — FUSE-T isn't installed, or
  you're running a binary built before `src-tauri/.cargo/config.toml` existed
  (`cargo clean -p maker-app` and rebuild).
- **Mount button errors / nothing appears** — FUSE-T needs an interactive GUI
  session; it won't hold a mount from a headless/ssh context. Run the app from a
  normal desktop session. Check the `cargo tauri dev` console for daemon logs.
- **`register failed`** — the server isn't up on the URL in the form (step 2).

---

## Windows

The mount path is **not available yet**. `fs-fuse` is built on `fuser`
(Linux/macOS only); the Windows mount adapter (`winfsp-rs`, CLAUDE.md §12 step 4)
isn't written. The app is gated so it still **builds and launches** on Windows —
`cargo tauri dev` works — but **Mount & sync** returns:

> Mounting isn't supported on Windows yet — it needs the winfsp-rs adapter.

You can still exercise the **sync engine** itself on Windows (push/pull/hydrate,
no mount) via the cross-platform example flows against the server:
```powershell
cd client\sync-engine
$env:SERVER_URL = "http://localhost:3001"
cargo run --example daemon_loop          # outbox → push → pull → hydrate
cargo run --example presigned_roundtrip  # real bytes through S3-compatible storage
```
When the winfsp-rs adapter lands, the same `Daemon` + Tauri commands light up on
Windows with no frontend changes.
