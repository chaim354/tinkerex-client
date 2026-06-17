//! Tauri backend for the Maker client.
//!
//! Commands drive the native sync daemon. Auth is **token-persisted**: a device enrolls
//! once via device-login (`sync_login`: email + password → a device token, authenticated by
//! Devise on the server), the bearer token is saved under the app dir, and later starts
//! reuse it — we never re-login. Every device of an account logs in independently (the
//! Dropbox model); there are no pairing codes.
//!
//! The mount backend is chosen per-OS — all expose the same `start`/`stop`/`is_running`/
//! `mountpoint` API:
//! - **macOS:** WebDAV + native `mount_webdav` (FUSE-T can't mount on macOS 26+).
//! - **Linux:** FUSE (`fuser`).
//! - **Windows:** WinFsp (drive letter; needs `winfsp-x64.dll` on the search path).

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::State;

#[cfg(target_os = "macos")]
type Mount = sync_engine::WebdavMount;
#[cfg(target_os = "linux")]
type Mount = sync_engine::Daemon;

// ---- WKWebView stale-cache eviction on app update (macOS) ---------------------------
//
// The frontend is the bundled static site (`frontendDist = ../src`) served by WKWebView
// from `tauri://localhost`. WKWebView keeps a persistent on-disk cache (under
// `~/Library/Caches/<bundle-id>` + `~/Library/WebKit/<bundle-id>`) keyed by URL. Across an
// app update the asset bytes change but the URLs (`/index.html`, `/main.js`, …) AND the
// bundle id stay the same — so the webview serves the OLD cached UI until the user manually
// clears the caches dir. We do that purge automatically, exactly once per version bump.
//
// Implementation: a tiny "last-run version" stamp file. On startup (before any window is
// created) compare the running build's version to the stamp; on a mismatch (or first run)
// evict the on-disk WKWebView data for this bundle id, then rewrite the stamp. Everything
// degrades safely — any IO error just logs and lets the app boot (worst case: the user is
// back to the manual clear, never a crash). macOS-only; a no-op everywhere else.
#[cfg(target_os = "macos")]
mod webview_cache {
    use std::path::PathBuf;

    /// Where the "last successfully launched" version stamp lives. Kept next to the app's
    /// other local state (the same `maker-app` base the creds/account dirs use) so it's
    /// per-install and survives across launches but not a full uninstall.
    fn stamp_path() -> PathBuf {
        super::app_base().join("webview-version")
    }

    /// The WKWebView on-disk locations Apple keys by bundle id. Clearing these is exactly the
    /// manual `rm -rf ~/Library/Caches/<bundle-id>` fix, plus the WebKit website-data store
    /// (local/session storage, the back/forward disk cache) so nothing stale survives.
    fn webkit_dirs(bundle_id: &str) -> Vec<PathBuf> {
        let home = match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h),
            None => return Vec::new(),
        };
        let lib = home.join("Library");
        vec![
            lib.join("Caches").join(bundle_id),
            lib.join("WebKit").join(bundle_id),
        ]
    }

    /// Run once on startup, before the window loads. Evicts the WKWebView disk cache iff the
    /// build version changed since the last launch (the update case), then records the current
    /// version. `bundle_id` is the app identifier (`com.chaim.maker-app`); `version` is the
    /// build's `tauri.conf.json` version (`CARGO_PKG_VERSION`, kept in lockstep).
    pub fn evict_if_version_changed(bundle_id: &str, version: &str) {
        let stamp = stamp_path();
        let last = std::fs::read_to_string(&stamp).ok();
        // First run with the stamp absent counts as "changed": harmless (cache is empty or
        // a pre-stamp build's) and self-healing — we just write the stamp and move on.
        if last.as_deref().map(str::trim) == Some(version) {
            return; // same version as last launch — nothing to do, keep the warm cache
        }

        for dir in webkit_dirs(bundle_id) {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => eprintln!(
                    "webview-cache: evicted {} on update -> {version}",
                    dir.display()
                ),
                // Not-found is the normal case (clean install / dir never created) — not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!(
                    "webview-cache: could not evict {} ({e}); continuing",
                    dir.display()
                ),
            }
        }

        if let Some(parent) = stamp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&stamp, version) {
            // Couldn't persist the stamp → we'd re-evict next launch. Annoying, not unsafe.
            eprintln!("webview-cache: could not write version stamp ({e}); continuing");
        }
    }
}
// Windows default: CfAPI — the driverless Cloud Filter path (no WinFsp/GPL driver, mount point is
// a folder not a drive letter). Flipped from WinfspMount after the live smoke + byte-for-byte
// conformance went green (Phase 2b B5). WinFsp stays reachable behind the `winfsp-backend` feature
// for one release.
#[cfg(all(target_os = "windows", not(feature = "winfsp-backend")))]
type Mount = sync_engine::CfApiMount;
#[cfg(all(target_os = "windows", feature = "winfsp-backend"))]
type Mount = sync_engine::WinfspMount;

/// Holds the running mount (or `None` when stopped).
#[derive(Default)]
struct AppState(Mutex<Option<Mount>>);

// ---- credential persistence (register/pair once, reuse the token) ------------------

fn app_base() -> PathBuf {
    std::env::temp_dir().join("maker-app")
}

/// Normalize a user-entered mount path: expand a leading `~`/`~/…` to `$HOME`, and resolve a
/// relative path against `$HOME` (not the `.app`'s `/` working directory). A Windows drive
/// letter (`Z:`) or an already-absolute path passes through unchanged.
#[cfg(not(target_os = "windows"))]
fn expand_mountpoint(input: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    if input == "~" {
        PathBuf::from(home)
    } else if let Some(rest) = input.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else {
        let p = PathBuf::from(input);
        if p.is_absolute() {
            p
        } else {
            PathBuf::from(home).join(p)
        }
    }
}

#[cfg(target_os = "windows")]
fn expand_mountpoint(input: &str) -> PathBuf {
    PathBuf::from(input) // a drive letter like "Z:" — no expansion
}

fn creds_path() -> PathBuf {
    app_base().join("credentials.json")
}

/// Per-account local state dir (the SQLite tree + chunk cache), keyed by `(server, email)` —
/// the same identity the saved token is keyed by. Without this the local tree.db is shared
/// across accounts, so mounting a *different* account shows the previous one's files (its
/// changelog never deletes them). Hashed so the path has no `://`/`@`/separator surprises.
fn account_dir(server: &str, email: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    creds_key(server, email).hash(&mut h);
    app_base().join(format!("acct-{:016x}", h.finish()))
}

fn creds_key(server: &str, email: &str) -> String {
    format!("{server}|{email}")
}

/// The saved device token for this (server, email), if this device has registered/paired.
fn load_token(server: &str, email: &str) -> Option<String> {
    let data = std::fs::read_to_string(creds_path()).ok()?;
    let map: serde_json::Value = serde_json::from_str(&data).ok()?;
    map.get(creds_key(server, email))?
        .as_str()
        .map(str::to_string)
}

fn save_token(server: &str, email: &str, token: &str) {
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(creds_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(
        creds_key(server, email),
        serde_json::Value::String(token.to_string()),
    );
    let _ = std::fs::create_dir_all(app_base());
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(creds_path(), json);
    }
}

// ---- file-layer preferences (bandwidth + progressive hydration) --------------------
//
// These map onto `DaemonConfig` *start-time* options (`with_bandwidth`,
// `with_hydrate_concurrency`), so they're persisted here and applied on the next
// sign-in/mount. Selective-sync (per-folder include/exclude) is NOT here: the engine
// has the API (`SyncEngine::subscribe`/`unsubscribe`) but the mount types don't yet
// expose their `Arc<SqliteProvider>` for a live command — see
// `docs/plan-selective-sync-ui.md`.

/// User preferences that tune the sync daemon. Serialized to `prefs.json` keyed by
/// `(server,email)`. All zero/false = the unchanged defaults.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Prefs {
    /// Download cap in **MB/s** (0 = unlimited).
    #[serde(default)]
    down_mbps: u64,
    /// Upload cap in **MB/s** (0 = unlimited).
    #[serde(default)]
    up_mbps: u64,
    /// Opt-in progressive (range) hydration for large files.
    #[serde(default)]
    progressive_hydration: bool,
}

impl Prefs {
    /// A MB/s cap as bytes/sec for `DaemonConfig::with_bandwidth`. `0` → `None` (unlimited).
    fn cap_bps(mbps: u64) -> Option<u64> {
        if mbps == 0 {
            None
        } else {
            Some(mbps.saturating_mul(1_000_000))
        }
    }

    fn down_bps(&self) -> Option<u64> {
        Self::cap_bps(self.down_mbps)
    }

    fn up_bps(&self) -> Option<u64> {
        Self::cap_bps(self.up_mbps)
    }
}

fn prefs_path() -> PathBuf {
    app_base().join("prefs.json")
}

fn load_prefs(server: &str, email: &str) -> Prefs {
    let data = match std::fs::read_to_string(prefs_path()) {
        Ok(d) => d,
        Err(_) => return Prefs::default(),
    };
    let map: serde_json::Value = match serde_json::from_str(&data) {
        Ok(m) => m,
        Err(_) => return Prefs::default(),
    };
    map.get(creds_key(server, email))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn save_prefs(server: &str, email: &str, prefs: &Prefs) {
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(
        creds_key(server, email),
        serde_json::to_value(prefs).unwrap_or(serde_json::Value::Null),
    );
    let _ = std::fs::create_dir_all(app_base());
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(prefs_path(), json);
    }
}

/// Read this account's saved file-layer prefs (for the Settings UI).
#[tauri::command]
fn get_prefs(server_url: String, email: String) -> Prefs {
    load_prefs(&server_url, &email)
}

/// Persist this account's file-layer prefs. Bandwidth + hydration apply on the next
/// sign-in/mount (they're daemon start-time options). Returns a human note.
#[tauri::command]
fn set_prefs(server_url: String, email: String, prefs: Prefs) -> Result<String, String> {
    save_prefs(&server_url, &email, &prefs);
    Ok("Saved — applies the next time you sign in.".into())
}

// ---- mounting ----------------------------------------------------------------------

/// Mount + start the sync loop with an already-obtained device `token`. Shared by
/// the `sync_login` enrollment path.
fn mount_with_token(
    state: &State<AppState>,
    server_url: &str,
    email: &str,
    mountpoint: &str,
    token: String,
) -> Result<String, String> {
    use sync_engine::DaemonConfig;

    let mut guard = state.0.lock().unwrap();
    if guard.as_ref().is_some_and(|m| m.is_running()) {
        return Err("already mounted — unmount first".into());
    }

    // Per-account local tree + chunk cache, so switching accounts doesn't surface the
    // previous account's files from a shared tree.db.
    let base = account_dir(server_url, email);
    let db = base.join("tree.db");
    let chunks = base.join("chunks");
    std::fs::create_dir_all(&chunks).map_err(|e| format!("chunk dir: {e}"))?;
    // Expand a leading `~` and make the path absolute. The `.app`'s working directory is `/`,
    // so a bare `~/Maker` (which the UI suggests) or a relative path resolves under the
    // read-only root volume → EROFS ("Read-only file system"). Expand it first.
    let mountpoint = expand_mountpoint(mountpoint);
    // On Windows the mount point is a drive letter (e.g. "Z:"), not a directory to create.
    #[cfg(not(target_os = "windows"))]
    std::fs::create_dir_all(&mountpoint).map_err(|e| format!("mountpoint: {e}"))?;

    // Apply the account's saved file-layer prefs (bandwidth caps) at mount time — these
    // are DaemonConfig start-time options, so they take effect on this sign-in.
    let prefs = load_prefs(server_url, email);
    let cfg = DaemonConfig::new(&mountpoint, db, chunks, server_url, token)
        .with_bandwidth(prefs.up_bps(), prefs.down_bps());
    let mount = Mount::start(cfg).map_err(|e| format!("mount failed: {e}"))?;
    let mp = mount.mountpoint().display().to_string();
    *guard = Some(mount);
    Ok(format!("mounted at {mp}"))
}

/// Enroll this device by logging in (email + password) — the Dropbox model — or reuse a
/// saved token, then mount + sync against `server_url`. The server authenticates the human
/// credentials via Devise (a confirmed, unlocked account is required) and returns a device
/// token; the password is sent once over TLS and never stored — only the token is saved.
/// Every device of an account logs in independently (no pairing codes).
#[tauri::command]
fn sync_login(
    state: State<AppState>,
    server_url: String,
    email: String,
    password: String,
    mountpoint: String,
) -> Result<String, String> {
    use sync_client::ServerClient;

    let token = if let Some(t) = load_token(&server_url, &email) {
        t // already enrolled on this device — reuse the saved token, never re-login
    } else {
        let mut client = ServerClient::new(&server_url);
        let reg = client
            .device_login(&email, &password, "maker-app")
            .map_err(|e| format!("login failed: {e}"))?;
        save_token(&server_url, &email, &reg.device_token);
        reg.device_token
    };

    mount_with_token(&state, &server_url, &email, &mountpoint, token)
}

// ---- file browser + versioning (queries the server's synced tree) ------------------

#[derive(serde::Serialize)]
struct BrowseEntry {
    node_id: String,
    parent_node_id: Option<String>,
    name: String,
    kind: String,
    size: u64,
    /// Device name currently editing this file on another device (best-effort lock), if any.
    locked_by: Option<String>,
}

/// List the whole library tree (paginated server-side, flattened here). The frontend
/// builds the folder hierarchy from parent_node_id.
#[tauri::command]
fn browse(server_url: String, email: String) -> Result<Vec<BrowseEntry>, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);

    // Active locks held by OTHER devices → node_id => holder name (best-effort; ignore errors).
    let locked: std::collections::HashMap<String, String> = client
        .locks()
        .map(|r| r.locks)
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.held && !l.by_me)
        .filter_map(|l| l.device_name.map(|d| (l.node_id, d)))
        .collect();

    let mut out = Vec::new();
    let mut after = 0;
    loop {
        let tree = client
            .tree(after)
            .map_err(|e| format!("browse failed: {e}"))?;
        if tree.nodes.is_empty() {
            break;
        }
        for n in &tree.nodes {
            out.push(BrowseEntry {
                node_id: n.node_id.clone(),
                parent_node_id: n.parent_node_id.clone(),
                name: n.name.clone(),
                kind: n.kind.clone(),
                size: n.size.unwrap_or(0),
                locked_by: locked.get(&n.node_id).cloned(),
            });
        }
        match tree.next_after_id {
            Some(id) => after = id,
            None => break,
        }
    }
    Ok(out)
}

#[derive(serde::Serialize)]
struct VersionEntry {
    version_id: String,
    size: u64,
    created_at: String,
}

/// Version history of a file (newest first) — the basis for restore.
#[tauri::command]
fn file_versions(
    server_url: String,
    email: String,
    node_id: String,
) -> Result<Vec<VersionEntry>, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);
    let vs = client
        .versions(&node_id)
        .map_err(|e| format!("versions failed: {e}"))?;
    Ok(vs
        .versions
        .into_iter()
        .map(|v| VersionEntry {
            version_id: v.version_id,
            size: v.size,
            created_at: v.created_at,
        })
        .collect())
}

/// Restore a file to an earlier version (creates a new version reusing the old
/// manifest — lossless). The sync loop then propagates it to every device.
#[tauri::command]
fn restore_version(
    server_url: String,
    email: String,
    node_id: String,
    version_id: String,
) -> Result<String, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);
    client
        .restore(&node_id, &version_id)
        .map_err(|e| format!("restore failed: {e}"))?;
    Ok("restored".into())
}

// ---- conflicts (divergent offline edits) -------------------------------------------

#[derive(serde::Serialize)]
struct ConflictSideOut {
    version_id: String,
    size: u64,
    created_at: String,
    device_name: String,
}

#[derive(serde::Serialize)]
struct ConflictOut {
    node_id: String,
    name: Option<String>,
    current: Option<ConflictSideOut>,
    incoming: Option<ConflictSideOut>,
}

fn side(s: sync_client::ConflictSide) -> ConflictSideOut {
    ConflictSideOut {
        version_id: s.version_id,
        size: s.size,
        created_at: s.created_at,
        device_name: s.device_name,
    }
}

/// Unresolved conflicts, with both sides' device + time + size, for the resolution prompt.
#[tauri::command]
fn conflicts(server_url: String, email: String) -> Result<Vec<ConflictOut>, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);
    let cs = client
        .conflicts()
        .map_err(|e| format!("conflicts failed: {e}"))?;
    Ok(cs
        .conflicts
        .into_iter()
        .map(|c| ConflictOut {
            node_id: c.node_id,
            name: c.name,
            current: c.current.map(side),
            incoming: c.incoming.map(side),
        })
        .collect())
}

/// Resolve a conflict: keep one version as current (the other stays in history).
#[tauri::command]
fn resolve_conflict_keep(
    server_url: String,
    email: String,
    node_id: String,
    version_id: String,
) -> Result<String, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);
    client
        .resolve_conflict_keep(&node_id, &version_id)
        .map_err(|e| format!("resolve failed: {e}"))?;
    Ok("kept".into())
}

/// Resolve a conflict: keep both — the incoming side becomes a new sibling file.
#[tauri::command]
fn resolve_conflict_split(
    server_url: String,
    email: String,
    node_id: String,
) -> Result<String, String> {
    use sync_client::ServerClient;
    let token = load_token(&server_url, &email).ok_or("this device isn't signed in yet")?;
    let client = ServerClient::with_token(&server_url, token);
    client
        .resolve_conflict_split(&node_id)
        .map_err(|e| format!("split failed: {e}"))?;
    Ok("split".into())
}

// ---- maker library (the catalog over the synced tree) ------------------------------

/// Build an authed `ServerClient` from the saved creds for this (server, email), or a
/// human error if this device isn't signed in. Mirrors the inline pattern in `browse`.
fn maker_client(server_url: &str, email: &str) -> Result<sync_client::ServerClient, String> {
    let token = load_token(server_url, email).ok_or("this device isn't signed in yet")?;
    Ok(sync_client::ServerClient::with_token(server_url, token))
}

// -- items --

#[tauri::command]
fn maker_list_items(
    server_url: String,
    email: String,
    item_type: Option<String>,
    tag: Option<String>,
    q: Option<String>,
) -> Result<Vec<sync_client::Item>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_items(item_type.as_deref(), tag.as_deref(), q.as_deref())
        .map_err(|e| format!("list items failed: {e}"))
}

#[tauri::command]
fn maker_get_item(
    server_url: String,
    email: String,
    id: i64,
) -> Result<sync_client::ItemDetail, String> {
    let c = maker_client(&server_url, &email)?;
    c.get_item(id).map_err(|e| format!("open item failed: {e}"))
}

#[tauri::command]
fn maker_create_item(
    server_url: String,
    email: String,
    input: sync_client::ItemInput,
) -> Result<sync_client::ItemDetail, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_item(&input)
        .map_err(|e| format!("create item failed: {e}"))
}

#[tauri::command]
fn maker_update_item(
    server_url: String,
    email: String,
    id: i64,
    input: sync_client::ItemInput,
) -> Result<sync_client::ItemDetail, String> {
    let c = maker_client(&server_url, &email)?;
    c.update_item(id, &input)
        .map_err(|e| format!("update item failed: {e}"))
}

#[tauri::command]
fn maker_delete_item(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_item(id)
        .map_err(|e| format!("delete item failed: {e}"))
}

// -- file attachments --

#[tauri::command]
fn maker_attach_file(
    server_url: String,
    email: String,
    item_id: i64,
    input: sync_client::FileAttachmentInput,
) -> Result<sync_client::FileAttachment, String> {
    let c = maker_client(&server_url, &email)?;
    c.attach_file(item_id, &input)
        .map_err(|e| format!("attach failed: {e}"))
}

#[tauri::command]
fn maker_detach_file(server_url: String, email: String, attachment_id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.detach_file(attachment_id)
        .map_err(|e| format!("detach failed: {e}"))
}

// -- tags & taggings --

#[tauri::command]
fn maker_tag_item(
    server_url: String,
    email: String,
    item_id: i64,
    name: String,
) -> Result<String, String> {
    let c = maker_client(&server_url, &email)?;
    c.tag_item(item_id, &name)
        .map_err(|e| format!("tag failed: {e}"))
}

#[tauri::command]
fn maker_untag(server_url: String, email: String, tagging_id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.untag(tagging_id)
        .map_err(|e| format!("untag failed: {e}"))
}

#[tauri::command]
fn maker_list_tags(server_url: String, email: String) -> Result<Vec<sync_client::Tag>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_tags().map_err(|e| format!("list tags failed: {e}"))
}

// -- collections --

#[tauri::command]
fn maker_list_collections(
    server_url: String,
    email: String,
) -> Result<Vec<sync_client::Collection>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_collections()
        .map_err(|e| format!("list collections failed: {e}"))
}

#[tauri::command]
fn maker_create_collection(
    server_url: String,
    email: String,
    name: String,
    description: Option<String>,
) -> Result<sync_client::Collection, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_collection(&name, description.as_deref())
        .map_err(|e| format!("create collection failed: {e}"))
}

#[tauri::command]
fn maker_collection_items(
    server_url: String,
    email: String,
    collection_id: i64,
) -> Result<Vec<sync_client::CollectionItem>, String> {
    let c = maker_client(&server_url, &email)?;
    c.collection_items(collection_id)
        .map_err(|e| format!("collection items failed: {e}"))
}

#[tauri::command]
fn maker_add_to_collection(
    server_url: String,
    email: String,
    collection_id: i64,
    item_id: i64,
) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.add_to_collection(collection_id, item_id)
        .map_err(|e| format!("add to collection failed: {e}"))
}

#[tauri::command]
fn maker_remove_from_collection(
    server_url: String,
    email: String,
    collection_id: i64,
    item_id: i64,
) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.remove_from_collection(collection_id, item_id)
        .map_err(|e| format!("remove from collection failed: {e}"))
}

// -- tasks --

#[tauri::command]
fn maker_list_tasks(
    server_url: String,
    email: String,
    open_only: bool,
) -> Result<Vec<sync_client::Task>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_tasks(open_only)
        .map_err(|e| format!("list tasks failed: {e}"))
}

#[tauri::command]
fn maker_create_task(
    server_url: String,
    email: String,
    input: sync_client::TaskInput,
) -> Result<sync_client::Task, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_task(&input)
        .map_err(|e| format!("create task failed: {e}"))
}

#[tauri::command]
fn maker_update_task(
    server_url: String,
    email: String,
    id: i64,
    input: sync_client::TaskInput,
) -> Result<sync_client::Task, String> {
    let c = maker_client(&server_url, &email)?;
    c.update_task(id, &input)
        .map_err(|e| format!("update task failed: {e}"))
}

#[tauri::command]
fn maker_delete_task(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_task(id)
        .map_err(|e| format!("delete task failed: {e}"))
}

// -- projects --

#[tauri::command]
fn maker_list_projects(
    server_url: String,
    email: String,
    status: Option<String>,
) -> Result<Vec<sync_client::Project>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_projects(status.as_deref())
        .map_err(|e| format!("list projects failed: {e}"))
}

#[tauri::command]
fn maker_get_project(
    server_url: String,
    email: String,
    id: i64,
) -> Result<sync_client::Project, String> {
    let c = maker_client(&server_url, &email)?;
    c.get_project(id)
        .map_err(|e| format!("open project failed: {e}"))
}

#[tauri::command]
fn maker_create_project(
    server_url: String,
    email: String,
    input: sync_client::ProjectInput,
) -> Result<sync_client::Project, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_project(&input)
        .map_err(|e| format!("create project failed: {e}"))
}

#[tauri::command]
fn maker_update_project(
    server_url: String,
    email: String,
    id: i64,
    input: sync_client::ProjectInput,
) -> Result<sync_client::Project, String> {
    let c = maker_client(&server_url, &email)?;
    c.update_project(id, &input)
        .map_err(|e| format!("update project failed: {e}"))
}

#[tauri::command]
fn maker_delete_project(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_project(id)
        .map_err(|e| format!("delete project failed: {e}"))
}

#[tauri::command]
fn maker_add_build_log(
    server_url: String,
    email: String,
    project_id: i64,
    input: sync_client::BuildLogEntryInput,
) -> Result<sync_client::BuildLogEntry, String> {
    let c = maker_client(&server_url, &email)?;
    c.add_build_log(project_id, &input)
        .map_err(|e| format!("add build-log failed: {e}"))
}

#[tauri::command]
fn maker_delete_build_log(server_url: String, email: String, entry_id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_build_log(entry_id)
        .map_err(|e| format!("delete build-log failed: {e}"))
}

#[tauri::command]
fn maker_attach_file_to_project(
    server_url: String,
    email: String,
    project_id: i64,
    input: sync_client::FileAttachmentInput,
) -> Result<sync_client::FileAttachment, String> {
    let c = maker_client(&server_url, &email)?;
    c.attach_file_to_project(project_id, &input)
        .map_err(|e| format!("attach failed: {e}"))
}

// -- inventory --

#[tauri::command]
fn maker_list_inventory(
    server_url: String,
    email: String,
    category: Option<String>,
    location_id: Option<i64>,
) -> Result<Vec<sync_client::InventoryItem>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_inventory(category.as_deref(), location_id)
        .map_err(|e| format!("list inventory failed: {e}"))
}

#[tauri::command]
fn maker_low_stock(
    server_url: String,
    email: String,
) -> Result<Vec<sync_client::InventoryItem>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_low_stock()
        .map_err(|e| format!("low-stock failed: {e}"))
}

#[tauri::command]
fn maker_get_inventory_item(
    server_url: String,
    email: String,
    id: i64,
) -> Result<sync_client::InventoryItem, String> {
    let c = maker_client(&server_url, &email)?;
    c.get_inventory_item(id)
        .map_err(|e| format!("open item failed: {e}"))
}

#[tauri::command]
fn maker_create_inventory_item(
    server_url: String,
    email: String,
    input: sync_client::InventoryItemInput,
) -> Result<sync_client::InventoryItem, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_inventory_item(&input)
        .map_err(|e| format!("create item failed: {e}"))
}

#[tauri::command]
fn maker_update_inventory_item(
    server_url: String,
    email: String,
    id: i64,
    input: sync_client::InventoryItemInput,
) -> Result<sync_client::InventoryItem, String> {
    let c = maker_client(&server_url, &email)?;
    c.update_inventory_item(id, &input)
        .map_err(|e| format!("update item failed: {e}"))
}

#[tauri::command]
fn maker_delete_inventory_item(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_inventory_item(id)
        .map_err(|e| format!("delete item failed: {e}"))
}

#[tauri::command]
fn maker_adjust_inventory(
    server_url: String,
    email: String,
    id: i64,
    delta: i64,
    reason: Option<String>,
    note: Option<String>,
) -> Result<sync_client::InventoryItem, String> {
    let c = maker_client(&server_url, &email)?;
    c.adjust_inventory(id, delta, reason.as_deref(), note.as_deref())
        .map_err(|e| format!("adjust failed: {e}"))
}

#[tauri::command]
fn maker_inventory_transactions(
    server_url: String,
    email: String,
    id: i64,
) -> Result<Vec<sync_client::InventoryTransaction>, String> {
    let c = maker_client(&server_url, &email)?;
    c.inventory_transactions(id)
        .map_err(|e| format!("transactions failed: {e}"))
}

#[tauri::command]
fn maker_list_locations(
    server_url: String,
    email: String,
) -> Result<Vec<sync_client::Location>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_locations()
        .map_err(|e| format!("list locations failed: {e}"))
}

#[tauri::command]
fn maker_create_location(
    server_url: String,
    email: String,
    name: String,
    description: Option<String>,
    parent_id: Option<i64>,
) -> Result<sync_client::Location, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_location(&name, description.as_deref(), parent_id)
        .map_err(|e| format!("create location failed: {e}"))
}

#[tauri::command]
fn maker_delete_location(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_location(id)
        .map_err(|e| format!("delete location failed: {e}"))
}

#[tauri::command]
fn maker_list_suppliers(
    server_url: String,
    email: String,
) -> Result<Vec<sync_client::Supplier>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_suppliers()
        .map_err(|e| format!("list suppliers failed: {e}"))
}

#[tauri::command]
fn maker_create_supplier(
    server_url: String,
    email: String,
    name: String,
    url: Option<String>,
    notes: Option<String>,
) -> Result<sync_client::Supplier, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_supplier(&name, url.as_deref(), notes.as_deref())
        .map_err(|e| format!("create supplier failed: {e}"))
}

#[tauri::command]
fn maker_delete_supplier(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_supplier(id)
        .map_err(|e| format!("delete supplier failed: {e}"))
}

// -- BOM --

#[tauri::command]
fn maker_list_boms(
    server_url: String,
    email: String,
    project_id: i64,
) -> Result<Vec<sync_client::Bom>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_boms(project_id)
        .map_err(|e| format!("list boms failed: {e}"))
}

#[tauri::command]
fn maker_create_bom(
    server_url: String,
    email: String,
    project_id: i64,
    input: sync_client::BomInput,
) -> Result<sync_client::Bom, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_bom(project_id, &input)
        .map_err(|e| format!("create bom failed: {e}"))
}

#[tauri::command]
fn maker_delete_bom(server_url: String, email: String, id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_bom(id).map_err(|e| format!("delete bom failed: {e}"))
}

#[tauri::command]
fn maker_bom_rollup(
    server_url: String,
    email: String,
    id: i64,
) -> Result<sync_client::BomRollup, String> {
    let c = maker_client(&server_url, &email)?;
    c.bom_rollup(id).map_err(|e| format!("rollup failed: {e}"))
}

#[tauri::command]
fn maker_create_bom_line(
    server_url: String,
    email: String,
    bom_id: i64,
    input: sync_client::BomLineInput,
) -> Result<sync_client::BomLine, String> {
    let c = maker_client(&server_url, &email)?;
    c.create_bom_line(bom_id, &input)
        .map_err(|e| format!("add line failed: {e}"))
}

#[tauri::command]
fn maker_update_bom_line(
    server_url: String,
    email: String,
    line_id: i64,
    input: sync_client::BomLineInput,
) -> Result<sync_client::BomLine, String> {
    let c = maker_client(&server_url, &email)?;
    c.update_bom_line(line_id, &input)
        .map_err(|e| format!("update line failed: {e}"))
}

#[tauri::command]
fn maker_delete_bom_line(server_url: String, email: String, line_id: i64) -> Result<(), String> {
    let c = maker_client(&server_url, &email)?;
    c.delete_bom_line(line_id)
        .map_err(|e| format!("delete line failed: {e}"))
}

// -- previews --

#[tauri::command]
fn maker_list_previews(
    server_url: String,
    email: String,
    node_id: Option<String>,
    version_id: Option<String>,
    kind: Option<String>,
) -> Result<Vec<sync_client::Preview>, String> {
    let c = maker_client(&server_url, &email)?;
    c.list_previews(node_id.as_deref(), version_id.as_deref(), kind.as_deref())
        .map_err(|e| format!("list previews failed: {e}"))
}

#[tauri::command]
fn maker_enqueue_preview(
    server_url: String,
    email: String,
    node_id: String,
    version_id: Option<String>,
    kind: Option<String>,
    filename: Option<String>,
) -> Result<sync_client::Preview, String> {
    let c = maker_client(&server_url, &email)?;
    c.enqueue_preview(
        &node_id,
        version_id.as_deref(),
        kind.as_deref(),
        filename.as_deref(),
    )
    .map_err(|e| format!("enqueue preview failed: {e}"))
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
    // Evict the WKWebView on-disk cache when the app has been updated to a new version, so the
    // new bundled UI shows without a manual `~/Library/Caches/<bundle-id>` clear. Must run
    // BEFORE the window is created (i.e. before `Builder::run`), so the webview never loads the
    // stale assets in the first place. macOS-only; the bundle id + version come from
    // `tauri.conf.json` (`CARGO_PKG_VERSION` is kept in lockstep with the config `version`).
    #[cfg(target_os = "macos")]
    webview_cache::evict_if_version_changed("com.chaim.maker-app", env!("CARGO_PKG_VERSION"));

    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            sync_login,
            sync_stop,
            sync_status,
            platform,
            browse,
            file_versions,
            restore_version,
            conflicts,
            resolve_conflict_keep,
            resolve_conflict_split,
            maker_list_items,
            maker_get_item,
            maker_create_item,
            maker_update_item,
            maker_delete_item,
            maker_attach_file,
            maker_detach_file,
            maker_tag_item,
            maker_untag,
            maker_list_tags,
            maker_list_collections,
            maker_create_collection,
            maker_collection_items,
            maker_add_to_collection,
            maker_remove_from_collection,
            maker_list_tasks,
            maker_create_task,
            maker_update_task,
            maker_delete_task,
            maker_list_projects,
            maker_get_project,
            maker_create_project,
            maker_update_project,
            maker_delete_project,
            maker_add_build_log,
            maker_delete_build_log,
            maker_attach_file_to_project,
            maker_list_inventory,
            maker_low_stock,
            maker_get_inventory_item,
            maker_create_inventory_item,
            maker_update_inventory_item,
            maker_delete_inventory_item,
            maker_adjust_inventory,
            maker_inventory_transactions,
            maker_list_locations,
            maker_create_location,
            maker_delete_location,
            maker_list_suppliers,
            maker_create_supplier,
            maker_delete_supplier,
            maker_list_boms,
            maker_create_bom,
            maker_delete_bom,
            maker_bom_rollup,
            maker_create_bom_line,
            maker_update_bom_line,
            maker_delete_bom_line,
            maker_list_previews,
            maker_enqueue_preview,
            get_prefs,
            set_prefs
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bps_zero_is_unlimited() {
        assert_eq!(Prefs::cap_bps(0), None);
    }

    #[test]
    fn cap_bps_converts_mbps_to_bytes_per_sec() {
        // 5 MB/s = 5_000_000 bytes/sec (DaemonConfig::with_bandwidth wants bytes/sec).
        assert_eq!(Prefs::cap_bps(5), Some(5_000_000));
        assert_eq!(Prefs::cap_bps(1), Some(1_000_000));
    }

    #[test]
    fn cap_bps_saturates_instead_of_overflowing() {
        // A pathological MB/s value must not panic — saturate to u64::MAX.
        assert_eq!(Prefs::cap_bps(u64::MAX), Some(u64::MAX));
    }

    #[test]
    fn prefs_down_and_up_use_their_own_caps() {
        let p = Prefs {
            down_mbps: 10,
            up_mbps: 0,
            progressive_hydration: true,
        };
        assert_eq!(p.down_bps(), Some(10_000_000));
        assert_eq!(p.up_bps(), None); // 0 = unlimited
    }

    #[test]
    fn prefs_round_trip_through_json() {
        // The shape the Settings UI sends as a Tauri arg, and we persist verbatim.
        let p = Prefs {
            down_mbps: 8,
            up_mbps: 2,
            progressive_hydration: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: Prefs = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn prefs_tolerates_absent_keys() {
        // An older/partial prefs blob must still parse to defaults for missing fields.
        let p: Prefs = serde_json::from_str(r#"{"down_mbps":4}"#).unwrap();
        assert_eq!(p.down_mbps, 4);
        assert_eq!(p.up_mbps, 0);
        assert!(!p.progressive_hydration);
    }

    #[test]
    fn default_prefs_are_all_unlimited_off() {
        let p = Prefs::default();
        assert_eq!(p.down_bps(), None);
        assert_eq!(p.up_bps(), None);
        assert!(!p.progressive_hydration);
    }
}
