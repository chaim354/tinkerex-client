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