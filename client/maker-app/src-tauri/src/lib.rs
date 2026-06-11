//! Tauri backend for the Maker client.
//!
//! Three commands drive the native sync daemon ([`sync_engine::Daemon`]): `sync_start`
//! registers this device, mounts the filesystem, and starts the sync loop + nudge;
//! `sync_stop` unmounts; `sync_status` reports state. The mount backend is Unix-only
//! (FUSE via `fuser`); on Windows the commands return a "not supported yet" error
//! (pending the winfsp-rs adapter — CLAUDE.md §12 step 4) but the app still launches.

use std::sync::Mutex;

use tauri::State;

/// Holds the running daemon (or `None` when stopped). On Windows there is no `Daemon`
/// type to hold, so the state is just a marker.
#[cfg(not(target_os = "windows"))]
#[derive(Default)]
struct AppState(Mutex<Option<sync_engine::Daemon>>);
#[cfg(target_os = "windows")]
#[derive(Default)]
struct AppState;

/// Register this device, mount at `mountpoint`, and start syncing against `server_url`.
/// Local state (SQLite tree + chunk cache) lives under the system temp dir.
#[tauri::command]
fn sync_start(
    state: State<AppState>,
    server_url: String,
    email: String,
    mountpoint: String,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = (state, server_url, email, mountpoint);
        Err("Mounting isn't supported on Windows yet — it needs the winfsp-rs adapter \
             (CLAUDE.md §12 step 4). The sync engine itself is cross-platform; only the \
             FUSE mount is Unix-only today."
            .into())
    }
    #[cfg(not(target_os = "windows"))]
    {
        use sync_client::ServerClient;
        use sync_engine::{Daemon, DaemonConfig};

        let mut guard = state.0.lock().unwrap();
        if guard.as_ref().is_some_and(|d| d.is_running()) {
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
        std::fs::create_dir_all(&mountpoint).map_err(|e| format!("mountpoint: {e}"))?;

        let cfg = DaemonConfig::new(&mountpoint, db, chunks, &server_url, reg.device_token);
        let daemon = Daemon::start(cfg).map_err(|e| format!("mount failed: {e}"))?;
        let mp = daemon.mountpoint().display().to_string();
        *guard = Some(daemon);
        Ok(format!("mounted at {mp}"))
    }
}

#[tauri::command]
fn sync_stop(state: State<AppState>) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let _ = state;
        Err("not mounted (Windows mount unsupported)".into())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut guard = state.0.lock().unwrap();
        match guard.take() {
            Some(mut d) => {
                d.stop();
                Ok("unmounted".into())
            }
            None => Err("not mounted".into()),
        }
    }
}

#[tauri::command]
fn sync_status(state: State<AppState>) -> String {
    #[cfg(target_os = "windows")]
    {
        let _ = state;
        "unsupported on Windows (no FUSE adapter yet)".into()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let guard = state.0.lock().unwrap();
        match guard.as_ref() {
            Some(d) if d.is_running() => format!("mounted at {}", d.mountpoint().display()),
            _ => "unmounted".into(),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![sync_start, sync_stop, sync_status])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
