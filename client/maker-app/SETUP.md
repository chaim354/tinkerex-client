# Run the Tinkerex desktop app (Tauri) and mount a synced drive

The app signs this device in to a Tinkerex server, mounts the local-first filesystem, and
runs the sync daemon (push on save, pull on a `{head_seq}` nudge / poll, hydrate on read)
against the sync server that sign-in names — all in one process. The mount backend is
chosen per platform:

- **macOS:** **WebDAV** — the app serves the tree over localhost and mounts it with the
  built-in `/sbin/mount_webdav`. **No FUSE, no kext, no FSKit** (FUSE-T can't mount on
  macOS 26). The drive shows in Finder under *Locations* as "Tinkerex". This is the default
  and it just works.
- **Linux:** **FUSE** (`fuser`) — mounts natively.
- **Windows:** **CfAPI** (driverless Cloud Filter — the default; no WinFsp/GPL driver, no
  `winfsp-x64.dll`). The mount point is a **folder** (e.g. `%USERPROFILE%\Tinkerex`), not a drive
  letter. The legacy WinFsp (drive-letter) backend is still available with
  `--features winfsp-backend`. (Live smoke + byte-for-byte conformance green — Phase 2b B5.)
  See the **Windows** section below for build/run + the two-device real-mount test.

The backend lives in `src-tauri/src/lib.rs` (commands `sync_login`/`sync_stop`/
`sync_status`) via a per-OS `Mount` alias — `sync_engine::WebdavMount` on macOS,
`sync_engine::Daemon` (FUSE) on Linux, `sync_engine::CfApiMount` on Windows.

> **macOS build note:** macOS needs nothing FUSE-related, to build or to run. `fuser` is now a
> Linux-only dependency of `sync-engine`, and the app crate builds and tests on macOS with no
> FUSE-T and no pkg-config alias (measured 2026-10-13). The FUSE-T alias below matters only if
> you build `client/fs-fuse` itself on macOS.

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

### 2. Have a Tinkerex server to sign in to

The app signs in to a **Tinkerex server** — the web app,
[chaim354/tinkerex](https://github.com/chaim354/tinkerex) — with an account on it. When that
server's `LIBRARY_BACKEND` is `chunk_remote`, its sign-in answer carries a grant for its sync
server ([chaim354/tinkerex-sync](https://github.com/chaim354/tinkerex-sync)), and the app syncs
against that directly. On `share` or `s3` there is nothing to sync: the app says so in one
sentence and mounts nothing. This repo has no server to start; running one is that repo's
business (its `docs/INSTALL.md`, and its CLAUDE.md §13 for a local pair).

### 3. Launch the app

```sh
cd client/maker-app
cargo tauri dev
```
First build pulls Tauri + the sync engine (a minute or two); then a window opens.
There's no `npm install` — the frontend is plain HTML served from `src/`.

### 4. Sign in, mount and test

In the window (it opens empty):
1. **Tinkerex server** — the server's address, as you would type it in a browser. The
   password goes over whatever that address uses, so use an `https://` one where you can; a
   LAN server's plain `http://` address works too.
2. **Account email** and **password** — an account on that server; the *same* account is
   the *same* library on every device.
3. **Mount point** — a directory, `~/Tinkerex` by default (created if missing).
4. Click **Sign in & sync**. Status flips to `mounted at …/Tinkerex`.

Then, in Finder / a terminal:
```sh
echo "hello tinkerex" > ~/Tinkerex/notes.txt      # save → pushed to the sync server
ls ~/Tinkerex
cat ~/Tinkerex/notes.txt                          # read → hydrates from storage
```
Click **Unmount** to tear it down cleanly.

> Local state (credentials, prefs, and each account's SQLite tree + chunk cache) lives under
> the platform data dir's `maker-app/` — `~/Library/Application Support/maker-app/` on macOS,
> `~/.local/share/maker-app/` on Linux (README.md has the details). Quit the app and delete it
> to start fresh.

### Two-device demo (one box)

Run a second instance pointed at a *different* mount point but the *same* email,
and watch a file made in one appear in the other (nudge-driven, ~tens of ms):
```sh
# second terminal
cd client/maker-app && cargo tauri dev
# mount this one at ~/Tinkerex2, same email → same library
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
- **`login failed: …`** — the server isn't answering at the address in the form, or the
  email/password is wrong (`401`). A sentence saying the server keeps its files in a shared
  folder or a storage bucket means that server has nothing to sync (step 2).

---

## Windows

**The mount path IS wired** (this section supersedes the older "not available yet" note). The
default Windows backend is **CfAPI** (driverless Cloud Filter — no WinFsp/GPL driver; the mount
point is a **folder**, `%USERPROFILE%\Tinkerex` by default, not a drive letter). The app's
`sync_login`/`sync_stop`/`sync_status` commands drive `sync_engine::CfApiMount` exactly as they
drive `Daemon`/`WebdavMount` on Linux/macOS. The legacy WinFsp (drive-letter) backend is still
available with `--features winfsp-backend`.

> **CfAPI needs an INTERACTIVE desktop session.** Cloud Filter refuses operations under a Session-0
> service account (FETCH_PLACEHOLDERS enumerates empty; writes fail with `0x8007017C` "the cloud
> operation is invalid"). Run the app / daemon from a normal interactive login — not from a service
> or a detached/headless context.

Build deps: LLVM/libclang (for `winfsp-sys` bindgen, pulled transitively) + WinFsp. Build & run:
```powershell
cd client\maker-app
cargo tauri dev
```

CLI daemon (no GUI), and the two-device real-mount test:
```powershell
# one CfAPI daemon (a sync root folder):
cargo build --manifest-path ..\sync-engine\Cargo.toml --bin maker-cfsyncd
..\sync-engine\target\debug\maker-cfsyncd.exe %USERPROFILE%\MakerCF tree.db chunks http://localhost:3001 <device_token>

# TWO real CfAPI sync roots, A<->B byte-for-byte through the mounts (needs the deployed S3 server):
..\..\client\test-conformance\two-device-cfapi.ps1 -Server http://192.168.1.95
```

## Library (catalog over the synced files)

The **Library** tab is a findable catalog: items (with type, tags, instructions),
collections, and tasks, all stored server-side and reached over `/v1/maker/*`. Files are
linked by the synced tree's `node_id` (the file picker reuses the file browser). Switch
to it with the **Sync | Library** nav at the top; it loads once the connection form has a
server URL + account email (the same creds the Sync tab uses).

What you can do in v1:
- **Catalog:** browse the grid, search (debounced), filter by type and by tag chip,
  toggle ★ favorite.
- **Item detail:** view instructions (markdown-lite), attachments grouped by role, and
  tags. Create/edit an item, add/remove tags (tag removal uses the per-item tagging id),
  attach a synced file (the picker lists the file browser's files and guesses a role from
  the extension) and detach it, or delete the item.
- **Collections:** create, open to see members, add/remove items.
- **Tasks:** a standalone todo/doing/done board — add a task, advance it (`→`), reopen a
  done task (`↺`), or delete it.

**Previews seam:** item cards render a type-icon placeholder today. When the *Previews*
pillar lands (server-rendered thumbnails per `(node_id, version_id)`), the grid card and
the detail header set `background-image` from a `preview_url` on the item/attachment JSON —
no other UI change. The placeholder div (`.lib-thumb`) is the single insertion point.
