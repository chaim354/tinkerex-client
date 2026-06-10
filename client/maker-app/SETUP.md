# Real-world Windows test: mount a drive from the Tauri app

Goal: run a Tauri app on Windows, click **Mount**, and get a working `Z:` drive
backed by JuiceFS that Explorer and apps can read/write.

This uses `juicefs mount` (via WinFsp) supervised by the app — the fastest path
to a real drive. The CfAPI provider (no WinFsp) comes later; this proves the
stack first. WinFsp is GPLv3 but you install it yourself for this test, so
there's nothing to redistribute and no licensing issue.

The mount logic is the verified `maker-app-core` crate in this repo. Everything
below runs **on the Windows machine**.

---

## 1. Install the prerequisites (Windows)

1. **WinFsp** — https://winfsp.dev → run the installer. This is the FUSE driver
   `juicefs` loads to expose a drive letter.
2. **JuiceFS** — download `juicefs.exe` from
   https://github.com/juicedata/juicefs/releases, put it somewhere on `PATH`
   (or note its full path).
3. **Rust** — https://rustup.rs (MSVC toolchain).
4. **Tauri CLI**:
   ```powershell
   cargo install tauri-cli --version "^2.0" --locked
   cargo install create-tauri-app --locked
   ```

> Visual Studio C++ Build Tools + WebView2 are needed by Tauri; the rustup and
> Tauri installers prompt for them if missing.

---

## 2. Create a JuiceFS volume (single-box quick test)

Simplest possible backing for a first test: **SQLite metadata + local-folder
storage** — no servers at all.

```powershell
mkdir C:\juicefs\meta, C:\juicefs\data
juicefs format --storage file --bucket C:/juicefs/data sqlite3://C:/juicefs/meta/maker.db maker
```

That `sqlite3://C:/juicefs/meta/maker.db` string is your **meta URL** below.

> Going multi-device later: swap SQLite → Postgres (`postgres://…`) or Redis,
> and the local folder → S3/R2/B2 (`--storage s3 --bucket https://…`). The app
> doesn't change — only the meta URL and the `format` storage flags.

Sanity-check the volume mounts at all, straight from the CLI:

```powershell
juicefs mount sqlite3://C:/juicefs/meta/maker.db Z:
# open Explorer → Z: should exist. Ctrl+C here to unmount, then continue.
```

If `Z:` appears, the hard part works and the app will too.

---

## 3. Scaffold the Tauri app

From `client/` in this repo:

```powershell
cargo create-tauri-app
#  Project name:        maker-app
#  Frontend language:   TypeScript / JavaScript  ->  pick "JavaScript"
#  UI template:         Vanilla
#  package manager:     (none needed for Vanilla)
```

You now have `client/maker-app/` next to `client/maker-app-core/`.

---

## 4. Wire in the controller

**a. Add the dependency** — `client/maker-app/src-tauri/Cargo.toml`:

```toml
[dependencies]
maker-app-core = { path = "../../maker-app-core" }
```

**b. Replace** `client/maker-app/src-tauri/src/lib.rs` with:

```rust
use std::sync::Mutex;

use maker_app_core::{MountConfig, MountController, MountStatus};
use tauri::State;

struct AppState(Mutex<MountController>);

#[tauri::command]
fn mount(state: State<AppState>) -> Result<MountStatus, String> {
    let mut c = state.0.lock().unwrap();
    c.mount().map_err(|e| e.to_string())?;
    Ok(c.status())
}

#[tauri::command]
fn unmount(state: State<AppState>) -> Result<MountStatus, String> {
    let mut c = state.0.lock().unwrap();
    c.unmount().map_err(|e| e.to_string())?;
    Ok(c.status())
}

#[tauri::command]
fn status(state: State<AppState>) -> MountStatus {
    state.0.lock().unwrap().status()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let controller = MountController::new(MountConfig::from_env());
    tauri::Builder::default()
        .manage(AppState(Mutex::new(controller)))
        .invoke_handler(tauri::generate_handler![mount, unmount, status])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**c. Enable the global API** so the plain-HTML frontend can call commands —
in `client/maker-app/src-tauri/tauri.conf.json`, set:

```json
"app": {
  "withGlobalTauri": true
}
```

**d. Replace** `client/maker-app/src/index.html` with:

```html
<!doctype html>
<html>
  <head><meta charset="UTF-8" /><title>Maker — Mount</title></head>
  <body style="font-family: sans-serif; padding: 2rem">
    <h1>Maker — JuiceFS Mount</h1>
    <p>Status: <strong id="state">…</strong> <span id="detail"></span></p>
    <button id="mount">Mount</button>
    <button id="unmount">Unmount</button>
    <script type="module">
      const { invoke } = window.__TAURI__.core;
      const stateEl = document.getElementById("state");
      const detailEl = document.getElementById("detail");
      const render = (s) => {
        stateEl.textContent = `${s.state} (${s.mount_point})`;
        detailEl.textContent = s.detail ? `— ${s.detail}` : "";
      };
      const refresh = async () => render(await invoke("status"));
      document.getElementById("mount").onclick = async () => {
        try { render(await invoke("mount")); } catch (e) { detailEl.textContent = `— ${e}`; }
      };
      document.getElementById("unmount").onclick = async () => {
        try { render(await invoke("unmount")); } catch (e) { detailEl.textContent = `— ${e}`; }
      };
      refresh();
      setInterval(refresh, 2000);
    </script>
  </body>
</html>
```

---

## 5. Run it

Point the controller at your volume (PowerShell, same session you launch from):

```powershell
$env:JUICEFS_META = "sqlite3://C:/juicefs/meta/maker.db"
$env:JUICEFS_MOUNTPOINT = "Z:"
# $env:JUICEFS_BIN = "C:\tools\juicefs.exe"   # only if not on PATH
# $env:JUICEFS_ARGS = "--cache-size 10240"    # optional tuning

cd client\maker-app
cargo tauri dev
```

The window opens → click **Mount** → status flips to `mounted (Z:)` and **`Z:`
appears in Explorer**. Drop a file in, open it, edit it. Click **Unmount** to
tear it down cleanly.

---

## Troubleshooting

- **"juicefs exited immediately: …"** — the app caught the failure; the message
  is juicefs's own stderr. Common causes: WinFsp not installed, `Z:` already in
  use (set `JUICEFS_MOUNTPOINT` to a free letter), or a bad meta URL.
- **Drive doesn't appear but status says mounted** — check the `cargo tauri dev`
  console for `[juicefs] …` log lines.
- **Permission error calling a command** — confirm `withGlobalTauri: true` and
  that all three commands are in `generate_handler!`.
- **Multi-device freshness** — out of scope for this single-box test; that's the
  change-notification channel, validated separately.
