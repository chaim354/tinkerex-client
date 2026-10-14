//! Tauri backend for the Tinkerex desktop app.
//!
//! Commands drive the native sync daemon. Auth is **token-persisted**: a device signs in
//! once (`sync_login`: email + password → `POST /v1/devices/login` on the TINKEREX server the
//! user typed, authenticated by Devise there), the credential is saved under the app dir, and
//! later starts reuse it — we never re-login. Since D1 (2026-10-13) that login answers two
//! devices' worth of credential: the maker app's own token, and a `sync` grant naming the sync
//! server and a token it accepts. The sync routes go to the grant (`sync_endpoint`), the maker
//! API to the typed server (`maker_endpoint`). Every device of an account logs in
//! independently (the Dropbox model); there are no pairing codes.
//!
//! The mount backend is chosen per-OS — all expose the same `start`/`stop`/`is_running`/
//! `mountpoint` API:
//! - **macOS:** WebDAV + native `mount_webdav` (FUSE-T can't mount on macOS 26+).
//! - **Linux:** FUSE (`fuser`).
//! - **Windows:** CfAPI (a sync-root folder, no driver); WinFsp behind `winfsp-backend`.

use std::path::{Path, PathBuf};
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
    /// version. `bundle_id` is the app identifier, read off `tauri.conf.json` by `run` (it was a
    /// second, hard-coded copy until D1 renamed it); `version` is the build's `tauri.conf.json`
    /// version (`CARGO_PKG_VERSION`, kept in lockstep).
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

// ---- credential persistence (sign in once, reuse the credential) -------------------

/// The durable per-user local-data directory for ALL persistent state — the credentials
/// (`credentials.json`), prefs (`prefs.json`), and every per-account `tree.db` + chunk
/// cache. **Must not** live in the OS temp dir: `/tmp` is tmpfs on Linux (wiped at reboot)
/// and `$TMPDIR` is purged after a few days on macOS. A wipe would destroy the device token
/// (→ a duplicate device on re-login), the whole local library (a full re-download), and —
/// worst — any outbox entries not yet pushed (offline local edits are gone permanently).
/// Uses the platform local-data dir (`~/Library/Application Support` on macOS,
/// `~/.local/share` on Linux, `%LOCALAPPDATA%` on Windows), falling back to the temp dir
/// only if it can't be determined (better than panicking).
fn app_base() -> PathBuf {
    // Still `maker-app`: renaming it is a migration of every account's state, deferred on purpose (D1).
    dirs::data_local_dir()
        .map(|d| d.join("maker-app"))
        .unwrap_or_else(|| legacy_app_base())
}

/// The old (pre-fix) location: the OS temp dir. Retained so we can migrate state off it.
fn legacy_app_base() -> PathBuf {
    std::env::temp_dir().join("maker-app")
}

/// One-time migration of persistent state from the old OS-temp location to the durable
/// app-data dir. Anything left in temp — including unpushed offline edits in the outbox —
/// is lost on the next reboot/purge, so we move it up front. Best-effort: on any IO error
/// we leave the legacy copy in place (reads still fall back to it) and continue booting.
fn migrate_legacy_state() {
    let new_base = app_base();
    let old_base = legacy_app_base();
    if new_base == old_base || new_base.exists() || !old_base.exists() {
        // Nothing to do: no durable dir available, already migrated / fresh install, or no
        // legacy state to move.
        return;
    }
    if let Some(parent) = new_base.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(&old_base, &new_base) {
        Ok(()) => eprintln!(
            "app-state: migrated {} -> {}",
            old_base.display(),
            new_base.display()
        ),
        // Cross-device rename (e.g. /tmp tmpfs -> $HOME) fails with EXDEV — fall back to a
        // recursive copy, then remove the old tree. If the copy fails, keep the old dir so
        // the read-time fallback in `credential_files`/`load_prefs` can still find the token.
        Err(_) => match copy_dir_all(&old_base, &new_base) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&old_base);
                eprintln!(
                    "app-state: copied {} -> {}",
                    old_base.display(),
                    new_base.display()
                );
            }
            Err(e) => eprintln!("app-state: legacy migration failed ({e}); using fallback reads"),
        },
    }
}

/// Recursively copy a directory tree (used only by the cross-device migration fallback).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Create `dir` (and parents) with owner-only permissions (0700 on Unix). Idempotent; also
/// tightens the mode if the dir already existed with looser perms.
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically write `contents` to `path` with owner-only permissions (0600 on Unix). Writes
/// a sibling temp file (owner-only), fsyncs, then renames over the target — so a crash or
/// full disk mid-write can't leave a truncated/lost credentials file (which would strand
/// every account's token), and the secret is never briefly world-readable (0644) on disk.
fn write_secure_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"))?;
    ensure_private_dir(parent)?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad file name"))?;
    let tmp = parent.join(format!(".{file_name}.tmp"));

    use std::io::Write;
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        // Tighten in case the temp file pre-existed with a looser mode (create won't reset it).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
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
    expand_user_profile(input, std::env::var("USERPROFILE").ok().as_deref())
}

/// The Windows half of `expand_mountpoint`, pure so it is tested on every OS. The sign-in
/// form's Windows default is `%USERPROFILE%\Tinkerex` (D1), and nothing else would expand it —
/// so a leading `%USERPROFILE%` (any case, as Windows reads it) becomes `profile`. Everything
/// else passes through unchanged: a drive letter (`Z:`, WinFsp) or an absolute folder.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn expand_user_profile(input: &str, profile: Option<&str>) -> PathBuf {
    const VAR: &str = "%USERPROFILE%";
    match (input.get(..VAR.len()), profile) {
        (Some(head), Some(profile)) if head.eq_ignore_ascii_case(VAR) => {
            let rest = input[VAR.len()..].trim_start_matches(['\\', '/']);
            if rest.is_empty() {
                PathBuf::from(profile)
            } else {
                PathBuf::from(profile).join(rest)
            }
        }
        _ => PathBuf::from(input),
    }
}

fn creds_path() -> PathBuf {
    app_base().join("credentials.json")
}

/// Per-account local state dir (the SQLite tree + chunk cache), keyed by `(server, email)` —
/// the same identity the saved credential is keyed by, where `server` is the TYPED server even
/// when the sync routes go elsewhere (D1). Without this the local tree.db is shared
/// across accounts, so mounting a *different* account shows the previous one's files (its
/// changelog never deletes them). The key is SHA-256'd (a **stable** hash — unlike
/// `DefaultHasher`, whose output changes across Rust releases and would silently remap an
/// account to a fresh empty dir, forcing a full re-download and stranding unpushed edits) so
/// the path also has no `://`/`@`/separator surprises.
fn account_dir(server: &str, email: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(creds_key(server, email).as_bytes());
    let hex = hex::encode(digest);
    app_base().join(format!("acct-{}", &hex[..16]))
}

/// Normalize the `(server, email)` identity so trivially-equivalent inputs map to the SAME
/// account (dir + token + prefs). Without this, `Email@X.com` vs `email@x.com`, or a
/// trailing-slash server URL, would key different dirs → duplicate device enrollments and a
/// split local tree. The server-URL normalization matches `ServerClient::new`'s
/// `trim_end_matches('/')`; email is lowercased + trimmed (emails are case-insensitive in
/// practice for our login).
fn creds_key(server: &str, email: &str) -> String {
    let server = server.trim().trim_end_matches('/');
    let email = email.trim().to_lowercase();
    format!("{server}|{email}")
}

/// One account's sign-in on this device: the VALUE stored in `credentials.json` under
/// `creds_key(typed server, email)` — the same key as before D1 (2026-10-13).
///
/// Two servers are involved since D1. The TYPED server is the Tinkerex (maker) app: its login
/// mints `maker_token`, which its own `v1/maker/*` API takes. The sync routes live on the sync
/// server, whose address and token arrive in that login's `sync` grant. On disk:
///
/// ```json
/// { "maker_token": "…", "sync_url": "https://sync.example.com", "sync_token": "…" }
/// ```
///
/// A LEGACY value — a bare string, the only shape before D1 — still loads, and is never
/// rewritten: it means "a sync token for the typed server", which is exactly what pasting a
/// token from `tinkerex-sync device issue` into this file produces.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Credential {
    /// The maker app's device token. `None` only for a legacy bare string.
    #[serde(default)]
    maker_token: Option<String>,
    /// Where the sync routes go: the grant's `url`, or the typed server for a legacy value.
    sync_url: String,
    /// The token the sync server accepts from this device.
    sync_token: String,
}

/// What every command answers when this (server, email) has no saved credential.
const NOT_SIGNED_IN: &str = "this device isn't signed in yet";

/// Read one stored value: a bare string is the legacy shape, an object the record, and
/// anything else is unreadable and treated as absent.
fn credential_from_value(typed_server: &str, value: &serde_json::Value) -> Option<Credential> {
    match value {
        serde_json::Value::String(token) => Some(Credential {
            maker_token: None,
            sync_url: typed_server.to_string(),
            sync_token: token.clone(),
        }),
        serde_json::Value::Object(_) => serde_json::from_value(value.clone()).ok(),
        _ => None,
    }
}

/// Where `credentials.json` is read from, in order: the durable app dir, then the temp-dir
/// copy a migration could not move (see `migrate_legacy_state`).
fn credential_files() -> [PathBuf; 2] {
    [creds_path(), legacy_app_base().join("credentials.json")]
}

/// The saved credential for this (server, email) in the first of `files` that has one.
fn load_credential_from(files: &[PathBuf], server: &str, email: &str) -> Option<Credential> {
    let key = creds_key(server, email);
    files.iter().find_map(|path| {
        let data = std::fs::read_to_string(path).ok()?;
        let map: serde_json::Value = serde_json::from_str(&data).ok()?;
        credential_from_value(server, map.get(&key)?)
    })
}

/// Add or replace this (server, email)'s record in the file at `path`, carrying every other
/// account's entry across unchanged — a legacy bare string included, so nothing is migrated.
fn save_credential_to(
    path: &Path,
    server: &str,
    email: &str,
    cred: &Credential,
) -> std::io::Result<()> {
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(creds_key(server, email), serde_json::to_value(cred)?);
    // Owner-only + atomic: both tokens are bearer credentials — never world-readable, and a
    // crash mid-write must not corrupt every account's sign-in.
    write_secure_atomic(path, &serde_json::to_string_pretty(&map)?)
}

fn save_credential(server: &str, email: &str, cred: &Credential) {
    if let Err(e) = save_credential_to(&creds_path(), server, email, cred) {
        eprintln!("save_credential: could not persist credentials ({e})");
    }
}

/// `(url, token)` for the SYNC routes — the tree, versions, restore, conflicts, locks, and the
/// daemon `sync_login` mounts on a saved credential. The ONE place a command learns where they
/// live: the grant's URL since D1, the typed server for a legacy value. `Err` is `NOT_SIGNED_IN`.
fn sync_endpoint(server: &str, email: &str) -> Result<(String, String), String> {
    sync_endpoint_from(&credential_files(), server, email)
}

fn sync_endpoint_from(
    files: &[PathBuf],
    server: &str,
    email: &str,
) -> Result<(String, String), String> {
    let cred = load_credential_from(files, server, email).ok_or(NOT_SIGNED_IN)?;
    Ok((cred.sync_url, cred.sync_token))
}

/// `(url, token)` for the maker app's own API (`v1/maker/*`), which lives on the TYPED server.
/// A legacy value carries no maker token: it was this device's one token for the typed server,
/// used for every route, so it still is.
fn maker_endpoint(server: &str, email: &str) -> Result<(String, String), String> {
    maker_endpoint_from(&credential_files(), server, email)
}

fn maker_endpoint_from(
    files: &[PathBuf],
    server: &str,
    email: &str,
) -> Result<(String, String), String> {
    let cred = load_credential_from(files, server, email).ok_or(NOT_SIGNED_IN)?;
    Ok((
        server.to_string(),
        cred.maker_token.unwrap_or(cred.sync_token),
    ))
}

/// What a login answer means for this device — one pure function, so each arm is a test.
///
/// - `sync: {…}`: a D1 server on a syncable deployment. Sync against the grant; keep the maker
///   token for the maker API.
/// - `library.syncable == false`: a D1 server whose files live in a shared folder or a bucket.
///   Nothing to mount; `Err` is the sentence the sign-in form shows, verbatim.
/// - neither field: a server older than D1, which served the sync routes itself with the same
///   token — what a legacy value means.
fn credential_from_login(
    typed_server: &str,
    reg: &sync_client::RegisterResponse,
) -> Result<Credential, String> {
    match (&reg.sync, &reg.library) {
        (Some(grant), _) => Ok(Credential {
            maker_token: Some(reg.device_token.clone()),
            sync_url: grant.url.clone(),
            sync_token: grant.device_token.clone(),
        }),
        (None, Some(library)) if !library.syncable => Err(not_syncable_sentence(&library.backend)),
        // A D1 server on a syncable deployment answers a grant or a 503, never this; saving
        // it as an older server would point sync at the maker app and reuse that forever.
        (None, Some(library)) => Err(format!(
            "This server's library ({}) syncs, but its sign-in answer carried no sync grant. \
             Nothing was saved; try signing in again.",
            library.backend
        )),
        (None, None) => Ok(Credential {
            maker_token: Some(reg.device_token.clone()),
            sync_url: typed_server.to_string(),
            sync_token: reg.device_token.clone(),
        }),
    }
}

/// The one sentence a library with nothing to sync gets, naming where its files live instead.
fn not_syncable_sentence(backend: &str) -> String {
    match backend {
        "share" => "This server keeps its files in a shared folder (share); there is nothing for \
                    this app to sync. Open the folder from your file manager instead."
            .into(),
        "s3" => "This server keeps its files in a storage bucket (s3); there is nothing for this \
                 app to sync. Open the bucket with your storage provider's tools instead."
            .into(),
        other => format!(
            "This server keeps its files where this app cannot sync them ({other}); there is \
             nothing for this app to sync."
        ),
    }
}

/// The sign-in error the form shows. A 503 `sync_unavailable` is the maker app saying it could
/// not reach its sync server — it enrolled nothing, so the useful words are "try again".
fn login_error_text(e: &sync_client::Error) -> String {
    match e {
        sync_client::Error::Server { status: 503, body } if body.contains("sync_unavailable") => {
            "The Tinkerex server could not reach its sync server, so this device was not signed \
             in. Try again in a minute."
                .into()
        }
        other => format!("login failed: {other}"),
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
    let key = creds_key(server, email);
    for path in [prefs_path(), legacy_app_base().join("prefs.json")] {
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(p) = map.get(&key).and_then(|v| serde_json::from_value(v.clone()).ok()) {
                    return p;
                }
            }
        }
    }
    Prefs::default()
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
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = write_secure_atomic(&prefs_path(), &json) {
            eprintln!("save_prefs: could not persist prefs ({e})");
        }
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

/// Mount + start the sync loop against the sync server at `sync_url`, as `sync_token`.
/// `server_url` is the TYPED server: it keys this account's local tree and prefs exactly as
/// before D1, even though the daemon itself talks to the grant's host.
fn mount_with_grant(
    state: &State<AppState>,
    server_url: &str,
    email: &str,
    mountpoint: &str,
    sync_url: &str,
    sync_token: String,
) -> Result<String, String> {
    use sync_engine::DaemonConfig;

    // Recover a poisoned lock instead of panicking: a prior command that panicked while
    // holding it must not permanently wedge every future mount/unmount.
    let mut guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
    if guard.as_ref().is_some_and(|m| m.is_running()) {
        return Err("already mounted — unmount first".into());
    }
    // Drop any stale, stopped mount BEFORE starting the new one. Its `Drop` may unmount its
    // mountpoint; if we created the new mount first (and it reused the same path), the old
    // one's teardown could unmount the fresh mount out from under us.
    if let Some(old) = guard.take() {
        drop(old);
    }

    // Per-account local tree + chunk cache, so switching accounts doesn't surface the
    // previous account's files from a shared tree.db.
    let base = account_dir(server_url, email);
    let db = base.join("tree.db");
    let chunks = base.join("chunks");
    std::fs::create_dir_all(&chunks).map_err(|e| format!("chunk dir: {e}"))?;
    // Expand a leading `~` and make the path absolute. The `.app`'s working directory is `/`,
    // so a bare `~/Tinkerex` (which the UI suggests) or a relative path resolves under the
    // read-only root volume → EROFS ("Read-only file system"). Expand it first.
    let mountpoint = expand_mountpoint(mountpoint);
    // On Windows the mount point is a drive letter (e.g. "Z:"), not a directory to create.
    #[cfg(not(target_os = "windows"))]
    std::fs::create_dir_all(&mountpoint).map_err(|e| format!("mountpoint: {e}"))?;

    // Apply the account's saved file-layer prefs (bandwidth caps) at mount time — these
    // are DaemonConfig start-time options, so they take effect on this sign-in.
    let prefs = load_prefs(server_url, email);
    let cfg = DaemonConfig::new(&mountpoint, db, chunks, sync_url, sync_token)
        .with_bandwidth(prefs.up_bps(), prefs.down_bps());
    let mount = Mount::start(cfg).map_err(|e| format!("mount failed: {e}"))?;
    let mp = mount.mountpoint().display().to_string();
    *guard = Some(mount);
    Ok(format!("mounted at {mp}"))
}

/// Sign this device in to the Tinkerex server the user typed (email + password) — the Dropbox
/// model — or reuse its saved credential, then mount + sync against the sync server that
/// server's answer named (D1). The server authenticates the human credentials via Devise (a
/// confirmed, unlocked account is required); the password is sent once — over whatever the
/// server's address uses; use https where you can — and never stored, only the credential is.
/// Every device of an account logs in independently (no pairing codes). An `http://` address,
/// typed or granted, is used exactly as given: the self-hosted LAN deployments answer over
/// plain HTTP.
#[tauri::command]
fn sync_login(
    state: State<AppState>,
    server_url: String,
    email: String,
    password: String,
    mountpoint: String,
) -> Result<String, String> {
    use sync_client::ServerClient;

    let (sync_url, sync_token) = match sync_endpoint(&server_url, &email) {
        Ok(endpoint) => endpoint, // already enrolled on this device — reuse it, never re-login
        Err(_not_signed_in) => {
            let mut client = ServerClient::new(&server_url);
            // The name other devices see in conflict prompts and lock badges.
            let reg = client
                .device_login(&email, &password, "Tinkerex desktop")
                .map_err(|e| login_error_text(&e))?;
            // A library with nothing to sync is an Err carrying the sentence the form shows,
            // and saves nothing: the next sign-in asks the server again.
            let cred = credential_from_login(&server_url, &reg)?;
            save_credential(&server_url, &email, &cred);
            // Mount from the credential just minted, not a re-read: a save that failed (it is
            // logged) must not strand a sign-in the server has already accepted.
            (cred.sync_url, cred.sync_token)
        }
    };

    mount_with_grant(
        &state,
        &server_url,
        &email,
        &mountpoint,
        &sync_url,
        sync_token,
    )
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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);

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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);
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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);
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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);
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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);
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
    let (sync_url, sync_token) = sync_endpoint(&server_url, &email)?;
    let client = ServerClient::with_token(sync_url, sync_token);
    client
        .resolve_conflict_split(&node_id)
        .map_err(|e| format!("split failed: {e}"))?;
    Ok("split".into())
}

// ---- maker library (the catalog over the synced tree) ------------------------------

/// Build an authed `ServerClient` for the MAKER API from the saved credential for this
/// (server, email), or a human error if this device isn't signed in. Unlike `browse` and the
/// other sync-route commands, it talks to the typed server with the maker token
/// (`maker_endpoint`), never to the sync grant's host.
fn maker_client(server_url: &str, email: &str) -> Result<sync_client::ServerClient, String> {
    let (url, token) = maker_endpoint(server_url, email)?;
    Ok(sync_client::ServerClient::with_token(url, token))
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
    let mut guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
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
    let guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(m) if m.is_running() => format!("mounted at {}", m.mountpoint().display()),
        _ => "unmounted".into(),
    }
}

/// The OS (`"windows"`/`"macos"`/`"linux"`), so the frontend can default the mount point
/// sensibly — a sync-root folder under the profile on Windows (CfAPI), a directory elsewhere.
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Move any persistent state (device token, prefs, per-account tree.db + chunk cache) off
    // the old OS-temp location onto the durable app-data dir before anything reads it. The
    // temp dir is wiped on reboot / purged after days, taking unpushed offline edits with it.
    migrate_legacy_state();

    // Evict the WKWebView on-disk cache when the app has been updated to a new version, so the
    // new bundled UI shows without a manual `~/Library/Caches/<bundle-id>` clear. Must run
    // BEFORE the window is created (i.e. before `Builder::run`), so the webview never loads the
    // stale assets in the first place. macOS-only; the bundle id + version come from
    // `tauri.conf.json` (`CARGO_PKG_VERSION` is kept in lockstep with the config `version`).
    // The id is read off the generated context, not spelled out here: a hard-coded copy kept
    // naming `com.chaim.maker-app` until D1 renamed the app — the old bundle's cache, not ours.
    let context = tauri::generate_context!();
    #[cfg(target_os = "macos")]
    webview_cache::evict_if_version_changed(
        &context.config().identifier,
        env!("CARGO_PKG_VERSION"),
    );

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
        .run(context)
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

    // ---- account-dir keying (fix #3) ----

    #[test]
    fn creds_key_normalizes_email_case_and_whitespace() {
        assert_eq!(
            creds_key("http://x", "  Email@X.com "),
            creds_key("http://x", "email@x.com")
        );
    }

    #[test]
    fn creds_key_normalizes_trailing_slash_on_server() {
        // Matches ServerClient::new's trim_end_matches('/') so the app and client agree.
        assert_eq!(
            creds_key("http://srv:3001/", "a@b.co"),
            creds_key("http://srv:3001", "a@b.co")
        );
    }

    #[test]
    fn account_dir_maps_equivalent_identities_to_the_same_dir() {
        let a = account_dir("http://srv/", "User@Host.com");
        let b = account_dir("http://srv", "user@host.com");
        assert_eq!(a, b);
    }

    #[test]
    fn account_dir_separates_distinct_accounts() {
        assert_ne!(
            account_dir("http://srv", "a@b.co"),
            account_dir("http://srv", "c@d.co")
        );
    }

    #[test]
    fn account_dir_hash_is_stable_and_hex() {
        // Deterministic across runs (SHA-256, not DefaultHasher) — a stable, filesystem-safe
        // name so an app rebuilt with a newer Rust keeps pointing at the same dir.
        let dir = account_dir("http://srv", "a@b.co");
        let name = dir.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("acct-"));
        let hex = name.strip_prefix("acct-").unwrap();
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        // Recompute: must equal the SHA-256(creds_key)[..16] hex, not vary run to run.
        use sha2::{Digest, Sha256};
        let expected = hex::encode(Sha256::digest(creds_key("http://srv", "a@b.co").as_bytes()));
        assert_eq!(hex, &expected[..16]);
    }

    // ---- secure credential write (fix #2) ----

    #[test]
    #[cfg(unix)]
    fn write_secure_atomic_sets_owner_only_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("maker-secure-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("credentials.json");
        write_secure_atomic(&file, r#"{"k":"v"}"#).unwrap();

        // File is 0600 (owner rw only) and the dir is 0700 (owner rwx only).
        let fmode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "credential file must be owner-only");
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "state dir must be owner-only");

        // Content round-trips, and a rewrite (atomic replace) keeps the tight mode.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), r#"{"k":"v"}"#);
        write_secure_atomic(&file, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second");
        let fmode2 = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode2, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_dir_all_replicates_a_nested_tree() {
        let root = std::env::temp_dir().join(format!("maker-copy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        std::fs::write(src.join("sub/b.txt"), "b").unwrap();

        copy_dir_all(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- the credential record (D1, 2026-10-13) ----
    //
    // Every test reads and writes a credentials.json in a temp dir of its own, through the
    // `_from`/`_to` functions — NEVER the real one under app_base(), which on a developer's
    // machine holds their actual sign-ins.

    const SERVER: &str = "https://tinkerex.example.com";
    const EMAIL: &str = "ada@example.com";

    /// A fresh, private temp dir per test; the test removes it.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tinkerex-creds-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `entries` as credentials.json, exactly as an older build (or a hand edit) left it.
    fn write_raw(file: &Path, entries: &[(String, serde_json::Value)]) {
        let map: serde_json::Map<String, serde_json::Value> = entries.iter().cloned().collect();
        std::fs::write(file, serde_json::Value::Object(map).to_string()).unwrap();
    }

    fn grant_record() -> Credential {
        Credential {
            maker_token: Some("maker-tok".into()),
            sync_url: "https://sync.tinkerex.example.com".into(),
            sync_token: "sync-tok".into(),
        }
    }

    fn answer(json: &str) -> sync_client::RegisterResponse {
        serde_json::from_str(json).expect("a login answer")
    }

    #[test]
    fn a_credential_record_round_trips_through_credentials_json_as_an_object() {
        let dir = scratch("roundtrip");
        let file = dir.join("credentials.json");
        save_credential_to(&file, SERVER, EMAIL, &grant_record()).unwrap();

        // On disk: an object with exactly the three fields, under the same key as before D1.
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            on_disk[creds_key(SERVER, EMAIL)],
            serde_json::json!({
                "maker_token": "maker-tok",
                "sync_url": "https://sync.tinkerex.example.com",
                "sync_token": "sync-tok"
            })
        );
        assert_eq!(
            load_credential_from(std::slice::from_ref(&file), SERVER, EMAIL),
            Some(grant_record())
        );
        // Keyed as before: a trailing slash and the email's case name the same account.
        assert_eq!(
            load_credential_from(&[file], "https://tinkerex.example.com/", "Ada@Example.com"),
            Some(grant_record())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_bare_string_is_a_sync_token_for_the_typed_server() {
        // The only shape before D1, and what a token from `tinkerex-sync device issue` pasted
        // into credentials.json still is. It keeps working, and it is never migrated.
        let dir = scratch("legacy");
        let file = dir.join("credentials.json");
        let typed = "https://sync.tinkerex.home";
        write_raw(&file, &[(creds_key(typed, EMAIL), "legacy-tok".into())]);

        assert_eq!(
            load_credential_from(std::slice::from_ref(&file), typed, EMAIL),
            Some(Credential {
                maker_token: None,
                sync_url: typed.into(),
                sync_token: "legacy-tok".into(),
            })
        );
        assert_eq!(
            sync_endpoint_from(std::slice::from_ref(&file), typed, EMAIL),
            Ok((typed.to_string(), "legacy-tok".to_string()))
        );
        // The maker API keeps getting the one token it always got, at the typed server.
        assert_eq!(
            maker_endpoint_from(&[file], typed, EMAIL),
            Ok((typed.to_string(), "legacy-tok".to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_a_record_leaves_another_accounts_legacy_string_a_string() {
        // "Nothing is migrated on disk": a save rewrites the whole file, so it must carry every
        // other entry across unchanged — a legacy string stays a bare string.
        let dir = scratch("keep-legacy");
        let file = dir.join("credentials.json");
        let other = creds_key("https://sync.tinkerex.home", "old@example.com");
        write_raw(&file, &[(other.clone(), "legacy-tok".into())]);

        save_credential_to(&file, SERVER, EMAIL, &grant_record()).unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(on_disk[&other], serde_json::json!("legacy-tok"));
        assert_eq!(
            load_credential_from(&[file], SERVER, EMAIL),
            Some(grant_record())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_endpoints_split_a_record_between_the_two_servers() {
        let dir = scratch("endpoints");
        let file = dir.join("credentials.json");
        save_credential_to(&file, SERVER, EMAIL, &grant_record()).unwrap();

        // Sync routes: the grant's URL and token. Maker API: the TYPED server, the maker token.
        assert_eq!(
            sync_endpoint_from(std::slice::from_ref(&file), SERVER, EMAIL),
            Ok((
                "https://sync.tinkerex.example.com".to_string(),
                "sync-tok".to_string()
            ))
        );
        assert_eq!(
            maker_endpoint_from(&[file], SERVER, EMAIL),
            Ok((SERVER.to_string(), "maker-tok".to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_credential_is_the_existing_not_signed_in_sentence() {
        let dir = scratch("missing");
        let absent = dir.join("credentials.json"); // never written
        let not_signed_in = Err("this device isn't signed in yet".to_string());
        assert_eq!(
            sync_endpoint_from(std::slice::from_ref(&absent), SERVER, EMAIL),
            not_signed_in
        );
        assert_eq!(maker_endpoint_from(&[absent], SERVER, EMAIL), not_signed_in);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pre_migration_copy_is_still_read_after_the_durable_file() {
        // The pre-D1 loader's two-file fallback, kept: the durable file first, then the temp-dir copy
        // a migration could not move.
        let dir = scratch("two-files");
        let durable = dir.join("credentials.json");
        let legacy_copy = dir.join("legacy-credentials.json");
        write_raw(
            &legacy_copy,
            &[(creds_key(SERVER, EMAIL), "old-tok".into())],
        );
        assert_eq!(
            sync_endpoint_from(&[durable.clone(), legacy_copy.clone()], SERVER, EMAIL),
            Ok((SERVER.to_string(), "old-tok".to_string()))
        );
        save_credential_to(&durable, SERVER, EMAIL, &grant_record()).unwrap();
        assert_eq!(
            load_credential_from(&[durable, legacy_copy], SERVER, EMAIL),
            Some(grant_record())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- what a login answer means (D1) ----

    #[test]
    fn a_grant_answer_syncs_against_the_grant_and_keeps_the_maker_token() {
        let reg = answer(
            r#"{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":42,
                "library":{"backend":"chunk_remote","syncable":true},
                "sync":{"url":"https://sync.tinkerex.example.com","device_token":"sync-tok",
                        "user_id":8,"device_id":21,"head_seq":5}}"#,
        );
        assert_eq!(credential_from_login(SERVER, &reg), Ok(grant_record()));
    }

    #[test]
    fn an_http_grant_url_is_kept_exactly_as_the_server_sent_it() {
        // The self-hosted LAN deployments answer over plain HTTP (measured 2026-10-13: their
        // sync host fails a TLS handshake), so a grant naming `http://` must reach the daemon
        // as-is — never upgraded to https, never refused.
        let typed = "http://tinkerex.home";
        let reg = answer(
            r#"{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":42,
                "library":{"backend":"chunk_remote","syncable":true},
                "sync":{"url":"http://sync.tinkerex.home","device_token":"sync-tok",
                        "user_id":8,"device_id":21,"head_seq":5}}"#,
        );
        let cred = credential_from_login(typed, &reg).expect("an http grant is a grant");
        assert_eq!(cred.sync_url, "http://sync.tinkerex.home");

        let dir = scratch("http-grant");
        let file = dir.join("credentials.json");
        save_credential_to(&file, typed, EMAIL, &cred).unwrap();
        assert_eq!(
            sync_endpoint_from(std::slice::from_ref(&file), typed, EMAIL),
            Ok((
                "http://sync.tinkerex.home".to_string(),
                "sync-tok".to_string()
            ))
        );
        assert_eq!(
            maker_endpoint_from(&[file], typed, EMAIL),
            Ok((typed.to_string(), "maker-tok".to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_library_with_nothing_to_sync_is_one_sentence_naming_where_its_files_are() {
        for (backend, place) in [
            ("share", "in a shared folder (share)"),
            ("s3", "in a storage bucket (s3)"),
        ] {
            let reg = answer(&format!(
                r#"{{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":0,
                    "library":{{"backend":"{backend}","syncable":false}},"sync":null}}"#
            ));
            let err = credential_from_login(SERVER, &reg).expect_err("nothing to mount");
            assert!(
                err.starts_with(&format!(
                    "This server keeps its files {place}; there is nothing for this app to sync."
                )),
                "{err}"
            );
        }
        assert_eq!(
            not_syncable_sentence("share"),
            "This server keeps its files in a shared folder (share); there is nothing for this \
             app to sync. Open the folder from your file manager instead."
        );
    }

    #[test]
    fn an_answer_with_no_sync_field_is_an_older_server_that_syncs_itself() {
        // Before D1 the server answered the four fields and served the sync routes itself, with
        // the same token: the legacy string's meaning, with the maker token kept as well.
        let reg = answer(r#"{"user_id":3,"device_id":11,"device_token":"one-tok","head_seq":42}"#);
        assert_eq!(
            credential_from_login(SERVER, &reg),
            Ok(Credential {
                maker_token: Some("one-tok".into()),
                sync_url: SERVER.into(),
                sync_token: "one-tok".into(),
            })
        );
    }

    #[test]
    fn a_syncable_library_without_a_grant_is_refused_rather_than_saved() {
        // A D1 server on a syncable deployment answers a grant or a 503 — never this. Treating
        // it as an older server would save a credential that points sync at the maker app,
        // which serves no sync routes, and the app would reuse it on every sign-in.
        let reg = answer(
            r#"{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":0,
                "library":{"backend":"chunk_remote","syncable":true},"sync":null}"#,
        );
        assert!(credential_from_login(SERVER, &reg).is_err());
    }

    #[test]
    fn a_503_sync_unavailable_reads_as_try_again() {
        let unavailable = sync_client::Error::Server {
            status: 503,
            body: r#"{"error":"sync_unavailable"}"#.into(),
        };
        assert!(login_error_text(&unavailable).contains("Try again"));
        let refused = sync_client::Error::Server {
            status: 401,
            body: r#"{"error":"invalid_credentials"}"#.into(),
        };
        assert!(login_error_text(&refused).starts_with("login failed: "));
    }

    // ---- the Windows mount default (D1) ----

    #[test]
    fn a_leading_userprofile_expands_to_the_profile_dir() {
        // The sign-in form's Windows default is `%USERPROFILE%\Tinkerex`; nothing else expands it.
        let profile = Some(r"C:\Users\ada");
        let expected = PathBuf::from(r"C:\Users\ada").join("Tinkerex");
        assert_eq!(
            expand_user_profile(r"%USERPROFILE%\Tinkerex", profile),
            expected
        );
        assert_eq!(
            expand_user_profile(r"%userprofile%\Tinkerex", profile),
            expected
        );
        assert_eq!(
            expand_user_profile("%USERPROFILE%", profile),
            PathBuf::from(r"C:\Users\ada")
        );
        // Everything else passes through as before: a drive letter, an absolute path.
        assert_eq!(expand_user_profile("Z:", profile), PathBuf::from("Z:"));
        assert_eq!(
            expand_user_profile(r"D:\Sync", profile),
            PathBuf::from(r"D:\Sync")
        );
        // No profile known: left alone rather than guessed.
        assert_eq!(
            expand_user_profile(r"%USERPROFILE%\Tinkerex", None),
            PathBuf::from(r"%USERPROFILE%\Tinkerex")
        );
    }
}
