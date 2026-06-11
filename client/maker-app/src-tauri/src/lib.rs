//! Tauri backend for the Maker client.
//!
//! Three commands drive the native sync daemon: `sync_start` registers this device,
//! mounts the filesystem, and starts the sync loop + nudge; `sync_stop` unmounts;
//! `sync_status` reports state. The mount backend is chosen per-OS — all expose the same
//! `start`/`stop`/`is_running`/`mountpoint` API:
//! - **macOS:** WebDAV + native `mount_webdav` (FUSE-T can't mount on macOS 26+).
//! - **Linux:** FUSE (`fuser`).
//! - **Windows:** WinFsp (drive letter; needs `winfsp-x64.dll` on the search path).

use std::sync::Mutex;

use tauri::State;

#[cfg(target_os = "macos")]
type Mount = sync_engine::WebdavMount;
#[cfg(target_os = "linux")]
type Mount = sync_engine::Daemon;
#[cfg(target_os = "windows")]
type Mount = sync_engine::WinfspMount;

/// Holds the running mount (or `None` when stopped).
#[derive(Default)]
struct AppState(Mutex<Option<Mount>>);

/// Register this device, mount at `mountpoint`, and start syncing against `server_url`.
/// Local state (SQLite tree + chunk cache) lives under the system temp dir.
#[tauri::command]
fn sync_start(
    state: State<AppState>,
    server_url: String,
    email: String,
    mountpoint: String,
) -> Result<String, String> {
    use sync_client::ServerClient;
    use sync_engine::DaemonConfig;

    let mut guard = state.0.lock().unwrap();
    if guard.as_ref().is_some_and(|m| m.is_running()) {
        return Err("already mounted — unmount first".into());
    }

    // Register this device to obtain a bearer token (same email → same user/library).
    let mut client = ServerClient::new(&server_url);
    let reg = client
        .register(&email, "maker-app")
        .map_err(|e| format!("register failed: {e}"))?;

    // Per-device local state under a stable temp location.
    let base = std::env::temp_dir().join("maker-app");
    let db = base.join("tree.db");
    let chunks = base.join("chunks");
    std::fs::create_dir_all(&chunks).map_err(|e| format!("chunk dir: {e}"))?;
    // On Windows the mount point is a drive letter (e.g. "Z:"), not a directory to create.
    #[cfg(not(target_os = "windows"))]
    std::fs::create_dir_all(&mountpoint).map_err(|e| format!("mountpoint: {e}"))?;

    let cfg = DaemonConfig::new(&mountpoint, db, chunks, &server_url, reg.device_token);
    let mount = Mount::start(cfg).map_err(|e| format!("mount failed: {e}"))?;
    let mp = mount.mountpoint().display().to_string();
    *guard = Some(mount);
    Ok(format!("mounted at {mp}"))
}

#[tauri::command]
fn sync_stop(state: State<AppState>) -> Result<String, String> {
    let mut guard = state.0.lock().unwrap();
    match guard.take() {
        Some(mut m) => {
            m.stop();
            Ok("unmounted".into())
        }
        None => Err("not mounted".into()),
    }
}

#[tauri::command]
fn sync_status(state: State<AppState>) -> String {
    let guard = state.0.lock().unwrap();
    match guard.as_ref() {
        Some(m) if m.is_running() => format!("mounted at {}", m.mountpoint().display()),
        _ => "unmounted".into(),
    }
}

/// The OS (`"windows"`/`"macos"`/`"linux"`), so the frontend can default the mount point
/// sensibly — a drive letter on Windows, a directory elsewhere.
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![sync_start, sync_stop, sync_status, platform])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
