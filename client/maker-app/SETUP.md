# Run the Maker client (Tauri app) and mount a synced drive

The app registers a device, mounts the local-first filesystem, and runs the sync
daemon (push on save, pull on a `{head_seq}` nudge / poll, hydrate on read) — all
in one process. Mounting uses **FUSE** (`fuser`): **macOS and Linux today**.
**Windows mounting is not wired yet** (it needs the winfsp-rs adapter, CLAUDE.md
§12 step 4) — the app still builds and launches on Windows, but the mount button
returns a clear "not supported yet" message. See [§Windows](#windows) below.

The backend lives in `src-tauri/src/lib.rs` (commands `sync_start` / `sync_stop` /
`sync_status`), driving `sync_engine::Daemon`.

---

## macOS / Linux — the real test

### 1. Prerequisites

- **Rust** — https://rustup.rs
- **Tauri CLI** — `cargo install tauri-cli --version "^2.0" --locked` (or use a
  global `cargo-tauri` you already have).
- **A FUSE driver:**
  - **macOS:** **FUSE-T** — https://www.fuse-t.org (no kext; no macFUSE).
  - **Linux:** libfuse (`apt install libfuse-dev`, usually present).
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
