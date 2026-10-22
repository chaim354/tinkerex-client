//! Client for the Rails sync server (`docs/sync-protocol.md`).
//!
//! Two responsibilities:
//! 1. **Control plane** — a typed HTTP client for the v1 API (register, commit,
//!    finalize, changes, tree, mkdir/move/delete, restore).
//! 2. **Data plane** — chunk upload/download via the **presigned URLs** the server
//!    mints, so bytes go client <-> R2 directly (the client never holds storage
//!    credentials).
//!
//! The push/pull orchestration (drive the outbox / apply the changelog against
//! `fs-provider`'s local SQLite) builds on these primitives — see [`SyncEngine`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod tree;
pub use tree::{LocalNode, LocalTree};

mod maker;
pub use maker::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server {status}: {body}")]
    Server { status: u16, body: String },
    #[error("not authenticated")]
    Unauthenticated,
    #[error("websocket: {0}")]
    Ws(#[from] tungstenite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The server refused this device's token: a **401 carrying the API's JSON error body**,
    /// which is what both the maker app and the sync server answer a revoked, expired or
    /// unknown device token (`{"error":"unauthorized"}`). This is the ONE error a host may
    /// treat as "signed out" (tinkerex#472). A transport failure, a 5xx, a 403, and a 401
    /// without that body (a proxy in front of the server) are not — signing a device out
    /// for being offline would be the opposite bug.
    pub fn is_auth_rejection(&self) -> bool {
        match self {
            Error::Server { status: 401, body } => serde_json::from_str::<serde_json::Value>(body)
                .is_ok_and(|v| v.get("error").is_some_and(|e| e.is_string())),
            _ => false,
        }
    }
}

// ---- wire types -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: String,
    pub offset: u64,
    pub length: u64,
}

/// manifest_id = SHA-256 of the ordered chunk-hash list — must match the server
/// (and the Rust fs-provider).
pub fn manifest_id(manifest: &[ChunkRef]) -> String {
    let mut h = Sha256::new();
    for c in manifest {
        h.update(c.hash.as_bytes());
    }
    hex::encode(h.finalize())
}

/// What the maker app's `POST /v1/devices/login` answers (and what `provision_library` builds
/// from the sync server's admin door). From `device_login`, `device_token` is the MAKER app's.
#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub user_id: i64,
    pub device_id: i64,
    pub device_token: String,
    pub head_seq: i64,
    /// The sync-server grant the maker app brokers for this device (D1, 2026-10-13): where the
    /// sync routes are and the token they accept. `None` BOTH from a server older than D1 (no
    /// field at all) and from one whose library has nothing to sync (`sync: null`) — `library`
    /// tells those two apart, because every D1 server sends it.
    #[serde(default)]
    pub sync: Option<SyncGrant>,
    /// Which library backend the server runs and whether a device syncs against it. `None` only
    /// from a server older than D1.
    #[serde(default)]
    pub library: Option<LibraryInfo>,
}

/// The `sync` object of a login answer: a SIBLING device on the user's own sync-server library,
/// minted by the maker app with the user's bootstrap token and proven with `whoami` before it is
/// handed out. `url` is the sync server as DEVICES reach it (the maker app's `SYNC_PUBLIC_URL`),
/// never the internal alias the maker app itself dials.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncGrant {
    pub url: String,
    pub device_token: String,
    pub user_id: i64,
    pub device_id: i64,
    pub head_seq: i64,
}

/// The `library` object of a login answer.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryInfo {
    /// The maker app's `LIBRARY_BACKEND`: `"share"`, `"s3"` or `"chunk_remote"`.
    pub backend: String,
    /// Whether a device syncs against this library — `chunk_remote` only, today.
    pub syncable: bool,
}

/// `GET /v1/devices/whoami`: the identity behind the current device token and the library's
/// current `head_seq` (a joining device's starting changelog cursor).
#[derive(Debug, Deserialize)]
pub struct WhoAmI {
    pub user_id: i64,
    pub device_id: i64,
    pub head_seq: i64,
}

/// What `POST /v1/admin/libraries` answers on the sync server (chaim354/tinkerex-sync).
#[derive(Debug, Deserialize)]
struct ProvisionedLibrary {
    user_id: i64,
    device_id: i64,
    device_token: String,
}

/// The env var that switches `enroll` to the sync server's admin door — see `enroll`.
pub const ADMIN_TOKEN_ENV: &str = "SYNC_IT_ADMIN_TOKEN";

/// Whether `SYNC_IT_ADMIN_TOKEN` is set: the suites ask this to pick the sync server's routes
/// (a sibling device via `create_device`) over the maker app's password login.
pub fn admin_token_configured() -> bool {
    admin_token_from_env().is_some()
}

/// `SYNC_IT_ADMIN_TOKEN`, if set and non-blank.
fn admin_token_from_env() -> Option<String> {
    std::env::var(ADMIN_TOKEN_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Decode an optional base64 string field into raw bytes — used for symlink targets,
/// which are raw (possibly non-UTF8) path bytes the server carries base64'd over JSON.
/// Absent/null → `None`; a malformed base64 string surfaces as a deserialize error.
fn de_base64_opt<'de, D>(d: D) -> std::result::Result<Option<Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use base64::Engine as _;
    let opt: Option<String> = Option::deserialize(d)?;
    match opt {
        None => Ok(None),
        Some(s) => base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Serialize)]
pub struct CommitBegin {
    pub commit_id: String,
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub base_version_id: Option<String>,
    pub size: u64,
    pub manifest: Vec<ChunkRef>,
    /// POSIX permission bits to persist on the node (preserves the executable bit).
    pub mode: u32,
    /// Real modification time, unix nanoseconds (a `bigint` on the server). Preserves the
    /// true mtime end-to-end instead of stamping the sync time.
    pub mtime: i64,
}

#[derive(Debug, Deserialize)]
pub struct CommitBeginResponse {
    pub commit_id: String,
    pub missing: Vec<String>,
    pub put_urls: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct FinalizeResponse {
    pub version_id: String,
    pub seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct SeqResponse {
    pub seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct DeviceResponse {
    pub device_id: i64,
    pub device_token: String,
}

#[derive(Debug, Deserialize)]
pub struct NodeVersion {
    pub version_id: String,
    pub manifest_id: String,
    pub size: u64,
    pub parent_version_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct VersionsResponse {
    pub versions: Vec<NodeVersion>,
}

#[derive(Debug, Deserialize)]
pub struct ConflictSide {
    pub version_id: String,
    pub size: u64,
    pub created_at: String,
    pub device_name: String,
}

/// One divergent side of a conflict. N-way: a node with 3+ concurrent edits yields MULTIPLE
/// `ConflictInfo` entries sharing a `node_id` (same `current`, different `incoming`) — every
/// divergent incoming edit is surfaced independently so none is silently lost.
#[derive(Debug, Deserialize)]
pub struct ConflictInfo {
    pub node_id: String,
    pub name: Option<String>,
    pub current: Option<ConflictSide>,
    pub incoming: Option<ConflictSide>,
}

#[derive(Debug, Deserialize)]
pub struct ConflictsResponse {
    pub conflicts: Vec<ConflictInfo>,
}

#[derive(Debug, Deserialize)]
pub struct SplitResponse {
    /// First new sibling (kept for 2-way callers).
    pub split_node_id: String,
    /// All new siblings — one per divergent incoming version (N-way split).
    #[serde(default)]
    pub split_node_ids: Vec<String>,
    pub seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct LockStatus {
    pub node_id: String,
    pub held: bool,
    #[serde(default)]
    pub by_me: bool,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocksResponse {
    pub locks: Vec<LockStatus>,
}

#[derive(Debug, Deserialize)]
pub struct NodeView {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub mode: Option<u32>,
    /// Real modification time, unix nanoseconds. `None` from a server that predates mtime
    /// sync (the engine falls back to `now` so freshness still works).
    #[serde(default)]
    pub mtime: Option<i64>,
    /// For `kind == "symlink"`: the link target bytes, base64-decoded from the wire.
    /// `None` for files/dirs (and from a server that predates symlink sync).
    #[serde(default, deserialize_with = "de_base64_opt")]
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    pub seq: i64,
    pub op: String,
    pub node_id: String,
    pub node: Option<NodeView>,
    #[serde(default)]
    pub manifest: Option<Vec<ChunkRef>>,
    pub version: Option<VersionView>,
}

#[derive(Debug, Deserialize)]
pub struct VersionView {
    pub version_id: String,
    pub manifest_id: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub struct ChangesResponse {
    pub changes: Vec<Change>,
    pub head_seq: i64,
    /// The oldest changelog seq the server still retains. If a client's cursor is below
    /// `oldest_seq - 1`, the entries it needs were pruned and it must full-resync.
    /// Defaults to 0 for back-compat with servers that don't report it.
    #[serde(default)]
    pub oldest_seq: i64,
}

#[derive(Debug, Deserialize)]
pub struct TreeNode {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub version_id: Option<String>,
    pub size: Option<u64>,
    #[serde(default)]
    pub manifest: Option<Vec<ChunkRef>>,
    #[serde(default)]
    pub mode: Option<u32>,
    /// Real modification time, unix nanoseconds. `None` from a pre-mtime server.
    #[serde(default)]
    pub mtime: Option<i64>,
    /// For `kind == "symlink"`: the link target bytes, base64-decoded from the wire.
    /// `None` for files/dirs (and from a server that predates symlink sync).
    #[serde(default, deserialize_with = "de_base64_opt")]
    pub symlink_target: Option<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
pub struct TreeResponse {
    pub nodes: Vec<TreeNode>,
    pub head_seq: i64,
    pub next_after_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DownloadResponse {
    pub urls: HashMap<String, String>,
}

/// A single server-side search hit (G9). `size` is present for files, absent for dirs.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    pub node_id: String,
    pub name: String,
    pub kind: String,
    /// Full slash-rooted path, e.g. `/Project/Bracket.STL`.
    pub path: String,
    pub size: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

// ---- the API client -------------------------------------------------------

/// Whether a failed transfer is worth retrying. Transport errors (connection reset,
/// read/connect timeout) and transient server statuses (5xx / 408 / 429) are retryable;
/// a definitive 4xx (e.g. a 403 on an expired presign) is not — re-issuing the same
/// request/URL won't help, so we surface it immediately instead of burning the backoff.
fn is_retryable(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::Server { status, .. } => {
            *status == 408 || *status == 429 || (*status >= 500 && *status < 600)
        }
        _ => false,
    }
}

/// Run `op` with bounded exponential backoff (500ms → 1s → 2s → 4s, capped), retrying
/// only on [`is_retryable`] errors. Used for bulk chunk PUT/GET so a transient network
/// blip on a large upload doesn't fail the whole commit (which would re-list every chunk
/// as missing next tick and re-upload from scratch, leaking orphaned bytes).
fn with_retry<T>(max_attempts: u32, mut op: impl FnMut() -> Result<T>) -> Result<T> {
    let mut delay = std::time::Duration::from_millis(500);
    let cap = std::time::Duration::from_secs(8);
    let mut attempt = 1;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= max_attempts || !is_retryable(&e) {
                    return Err(e);
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(cap);
                attempt += 1;
            }
        }
    }
}

/// Cheap to clone — `reqwest::blocking::Client` shares its connection pool via an
/// internal `Arc`, so the sync daemon hands a clone to each of its threads.
#[derive(Clone)]
pub struct ServerClient {
    base: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
    /// A separate client for bulk chunk transfer (PUT/GET). It has NO whole-request
    /// deadline — a 4 MB chunk on a slow uplink can legitimately exceed the control
    /// plane's 30s cap — only an idle (per-read) timeout so a dead connection still
    /// can't hang forever. Kept distinct so control-plane calls stay snappy.
    http_bulk: reqwest::blocking::Client,
}

impl ServerClient {
    pub fn new(base: impl Into<String>) -> Self {
        // A default request timeout so a stalled connection can't wedge the caller forever — a
        // hung push on the sync thread would otherwise block graceful shutdown (join never
        // returns). 30s is ample for a control-plane call yet bounds the worst case so an
        // unmount can't stall on a dead connection; short-deadline ops (locks) set their own
        // tighter per-request timeout.
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        // Bulk transfer: a generous whole-request timeout (NOT the control plane's 30s — a
        // large chunk over a slow uplink can legitimately take minutes), kept well under the
        // 900s presign TTL so a stuck transfer still surfaces an error rather than blocking a
        // sync thread forever. `connect_timeout` bounds a dead peer at dial time; `with_retry`
        // adds the rest of the robustness (see `upload_chunk` / `download_chunk`).
        let http_bulk = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .unwrap_or_default();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: None,
            http,
            http_bulk,
        }
    }

    pub fn with_token(base: impl Into<String>, token: impl Into<String>) -> Self {
        let mut c = Self::new(base);
        c.token = Some(token.into());
        c
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn authed(
        &self,
        rb: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let token = self.token.as_ref().ok_or(Error::Unauthenticated)?;
        Ok(rb.bearer_auth(token))
    }

    fn json<T: for<'de> Deserialize<'de>>(resp: reqwest::blocking::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp.json()?)
        } else {
            Err(Error::Server {
                status: status.as_u16(),
                body: resp.text().unwrap_or_default(),
            })
        }
    }

    /// For endpoints that return `204 No Content` (or any empty 2xx body) on success — e.g.
    /// the maker library's deletes/removes. Surfaces a non-2xx as `Error::Server` (so a
    /// cross-user 404 or a validation 422 isn't silently swallowed) without trying to parse
    /// a body.
    fn ok_no_content(resp: reqwest::blocking::Response) -> Result<()> {
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(Error::Server {
                status: status.as_u16(),
                body: resp.text().unwrap_or_default(),
            })
        }
    }

    /// Enroll this device by logging in (the Dropbox model): POST /v1/devices/login with the
    /// user's email + password. The server authenticates via Devise (confirmed + unlocked
    /// account required) and returns a device token, which we store on the client. The
    /// password is sent ONCE — over whatever the base URL's scheme is; use https where you can —
    /// and never persisted, only the token is kept.
    ///
    /// That route and that token are the MAKER app's, and this client keeps the maker token:
    /// it is the client for `/v1/maker/*`. Since D1 the answer also carries `sync` — the sync
    /// server's address and a token it accepts — and the SYNC routes (`/v1/tree`,
    /// `/v1/changes`, `/v1/commit`, versions, conflicts, locks) go to `sync.url` with that
    /// token, through a second client the caller builds. `library` says whether there is
    /// anything to sync at all; an answer with neither field is a server older than D1.
    pub fn device_login(
        &mut self,
        email: &str,
        password: &str,
        device_name: &str,
    ) -> Result<RegisterResponse> {
        let resp = self
            .http
            .post(self.url("/v1/devices/login"))
            .json(&serde_json::json!({
                "email": email, "password": password, "device_name": device_name
            }))
            .send()?;
        let reg: RegisterResponse = Self::json(resp)?;
        self.token = Some(reg.device_token.clone());
        Ok(reg)
    }

    /// TEST/DEV-ONLY: create a CONFIRMED password account via the server's signup seam
    /// (POST /v1/test/users, gated by ENABLE_TEST_SIGNUP on the server). Production accounts
    /// are created via the web UI (Devise) instead; this exists so the Rust integration
    /// tests/examples can stand up an account before enrolling devices with `device_login`.
    pub fn create_test_user(&self, email: &str, password: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/v1/test/users"))
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(Error::Server {
                status: status.as_u16(),
                body: resp.text().unwrap_or_default(),
            })
        }
    }

    /// TEST/DEV convenience: create a confirmed account (via the signup seam) then enroll
    /// this device via `device_login`. A near drop-in for the old `register` bootstrap.
    ///
    /// AGAINST THE SYNC SERVER (chaim354/tinkerex-sync) THERE IS NO SIGNUP SEAM AND NO PASSWORD
    /// LOGIN — `/v1/test/users` and `/v1/devices/login` are the maker app's routes, which that
    /// server has never served, so every server-backed suite and example that enrolled this way
    /// could only ever have run against the maker app. Its door is `POST /v1/admin/libraries`,
    /// gated by an operator token (the same `SYNC_ADMIN_TOKEN` the maker app provisions with).
    /// When `SYNC_IT_ADMIN_TOKEN` is set, enrolment goes through that door instead
    /// (`provision_library`): a fresh library per email, `head_seq` read back from `whoami`.
    /// The password is then unused — the sync server has no account to check it against.
    pub fn enroll(
        &mut self,
        email: &str,
        password: &str,
        device_name: &str,
    ) -> Result<RegisterResponse> {
        if let Some(admin) = admin_token_from_env() {
            return self.provision_library(&admin, email, device_name);
        }
        // Best-effort: in dev/test the gated `/v1/test/users` seam creates a confirmed
        // account; on a production server that seam is off (404) and the account is
        // expected to already exist (e.g. pre-created via `kamal app exec`). Either way
        // the device-login below is the real auth — only its failure is fatal.
        let _ = self.create_test_user(email, password);
        self.device_login(email, password, device_name)
    }

    /// Provision a brand-new library on the sync server through its admin door and hold the
    /// first device's token. A repeat call for the same email answers 409 `email_taken` there —
    /// the door never mints a second credential onto an existing library — so callers that need
    /// a second device on the same library use `create_device`, as `two_devices` does.
    pub fn provision_library(
        &mut self,
        admin_token: &str,
        email: &str,
        device_name: &str,
    ) -> Result<RegisterResponse> {
        let resp = self
            .http
            .post(self.url("/v1/admin/libraries"))
            .header("Authorization", format!("Bearer {admin_token}"))
            .json(&serde_json::json!({ "email": email, "name": device_name }))
            .send()?;
        let p: ProvisionedLibrary = Self::json(resp)?;
        self.token = Some(p.device_token.clone());
        let who = self.whoami()?;
        Ok(RegisterResponse {
            user_id: p.user_id,
            device_id: p.device_id,
            device_token: p.device_token,
            head_seq: who.head_seq,
            // This IS the sync server: there is no maker app in between to broker a grant.
            sync: None,
            library: None,
        })
    }

    /// `GET /v1/devices/whoami` for the current token.
    pub fn whoami(&self) -> Result<WhoAmI> {
        let resp = self
            .authed(self.http.get(self.url("/v1/devices/whoami")))?
            .send()?;
        Self::json(resp)
    }

    /// Register another device for the current user (returns a new device token).
    pub fn create_device(&self, name: &str) -> Result<DeviceResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/devices")))?
            .json(&serde_json::json!({ "name": name }))
            .send()?;
        Self::json(resp)
    }

    pub fn commit_begin(&self, req: &CommitBegin) -> Result<CommitBeginResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/commit")))?
            .json(req)
            .send()?;
        Self::json(resp)
    }

    pub fn commit_finalize(&self, commit_id: &str) -> Result<FinalizeResponse> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/commit/{commit_id}/finalize"))),
            )?
            .send()?;
        Self::json(resp)
    }

    pub fn mkdir(
        &self,
        node_id: &str,
        parent_node_id: Option<&str>,
        name: &str,
        mode: u32,
        mtime: i64,
    ) -> Result<SeqResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/nodes")))?
            .json(&serde_json::json!({ "node_id": node_id, "parent_node_id": parent_node_id, "name": name, "mode": mode, "mtime": mtime }))
            .send()?;
        Self::json(resp)
    }

    /// Create/update a symlink node. The target is raw link bytes (a path, possibly
    /// non-UTF8) and rides the control plane base64'd — a symlink has no manifest/chunks,
    /// so this is a single metadata op (mkdir-shaped), not a two-phase content commit.
    pub fn symlink(
        &self,
        node_id: &str,
        parent_node_id: Option<&str>,
        name: &str,
        target: &[u8],
        mode: u32,
        mtime: i64,
    ) -> Result<SeqResponse> {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(target);
        let resp = self
            .authed(self.http.post(self.url("/v1/nodes/symlink")))?
            .json(&serde_json::json!({
                "node_id": node_id, "parent_node_id": parent_node_id, "name": name,
                "symlink_target": encoded, "mode": mode, "mtime": mtime
            }))
            .send()?;
        Self::json(resp)
    }

    pub fn delete_node(&self, node_id: &str) -> Result<SeqResponse> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/nodes/{node_id}"))))?
            .send()?;
        Self::json(resp)
    }

    pub fn restore(&self, node_id: &str, version_id: &str) -> Result<FinalizeResponse> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/nodes/{node_id}/restore"))),
            )?
            .json(&serde_json::json!({ "version_id": version_id }))
            .send()?;
        Self::json(resp)
    }

    /// The version history of a node (newest first) — for the file browser's
    /// versions/restore UI.
    pub fn versions(&self, node_id: &str) -> Result<VersionsResponse> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("/v1/nodes/{node_id}/versions"))),
            )?
            .send()?;
        Self::json(resp)
    }

    /// Best-effort acquire/refresh the advisory edit lock on a node. Short timeout: if we
    /// can't reach the server the caller proceeds locally (offline just opens the cached
    /// copy — conflict detection is the safety net). Returns the lock status.
    pub fn lock(&self, node_id: &str) -> Result<LockStatus> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/nodes/{node_id}/lock"))),
            )?
            .timeout(std::time::Duration::from_secs(2))
            .send()?;
        Self::json(resp)
    }

    /// Release the advisory lock on a node (no-op if this device isn't the holder).
    pub fn unlock(&self, node_id: &str) -> Result<()> {
        self.authed(
            self.http
                .post(self.url(&format!("/v1/nodes/{node_id}/unlock"))),
        )?
        .timeout(std::time::Duration::from_secs(2))
        .send()?;
        Ok(())
    }

    /// All active edit locks (for the file browser's lock indicators).
    pub fn locks(&self) -> Result<LocksResponse> {
        let resp = self.authed(self.http.get(self.url("/v1/locks")))?.send()?;
        Self::json(resp)
    }

    /// Unresolved conflicts (divergent offline edits) with both sides' device + time.
    pub fn conflicts(&self) -> Result<ConflictsResponse> {
        let resp = self
            .authed(self.http.get(self.url("/v1/conflicts")))?
            .send()?;
        Self::json(resp)
    }

    /// Resolve a conflict by keeping one side as current.
    pub fn resolve_conflict_keep(
        &self,
        node_id: &str,
        keep_version_id: &str,
    ) -> Result<SeqResponse> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/nodes/{node_id}/resolve_conflict"))),
            )?
            .json(&serde_json::json!({ "keep_version_id": keep_version_id }))
            .send()?;
        Self::json(resp)
    }

    /// Resolve a conflict by keeping both — the incoming side becomes a new sibling file.
    pub fn resolve_conflict_split(&self, node_id: &str) -> Result<SplitResponse> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/nodes/{node_id}/resolve_conflict"))),
            )?
            .json(&serde_json::json!({ "split": true }))
            .send()?;
        Self::json(resp)
    }

    pub fn changes(&self, since: i64) -> Result<ChangesResponse> {
        self.changes_paged(since, 500)
    }

    /// Like [`changes`](Self::changes) but with an explicit page size. The server caps a
    /// single delta to this many entries, so a caller that's far behind must page until
    /// caught up (see `SyncEngine::pull_changes`). Exposed so tests can force multi-page
    /// pulls with a small `limit` instead of fabricating hundreds of entries.
    pub fn changes_paged(&self, since: i64, limit: i64) -> Result<ChangesResponse> {
        let resp = self
            .authed(self.http.get(self.url("/v1/changes")))?
            .query(&[("since", since.to_string()), ("limit", limit.to_string())])
            .send()?;
        Self::json(resp)
    }

    pub fn tree(&self, after_id: i64) -> Result<TreeResponse> {
        let resp = self
            .authed(self.http.get(self.url("/v1/tree")))?
            .query(&[("after_id", after_id.to_string())])
            .send()?;
        Self::json(resp)
    }

    /// Server-side name/path search (G9), scoped to this device's user. For the web UI and a
    /// fresh device with no local mirror; the desktop app prefers `fs-provider`'s local
    /// `search` (instant + offline). Paginated by `limit`/`offset`.
    pub fn search(&self, query: &str, limit: i64, offset: i64) -> Result<SearchResponse> {
        let resp = self
            .authed(self.http.get(self.url("/v1/search")))?
            .query(&[
                ("q", query.to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ])
            .send()?;
        Self::json(resp)
    }

    pub fn download_urls(&self, hashes: &[String]) -> Result<DownloadResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/chunks/download")))?
            .json(&serde_json::json!({ "hashes": hashes }))
            .send()?;
        Self::json(resp)
    }

    // ---- data plane: direct chunk transfer to/from presigned URLs ----------

    /// PUT a chunk's bytes to a presigned upload URL (returned by `commit_begin`). Uses the
    /// bulk client (no 30s whole-request cap) with bounded retry-with-backoff over transient
    /// failures, reusing the same already-minted presigned URL each attempt.
    pub fn upload_chunk(&self, put_url: &str, data: Vec<u8>) -> Result<()> {
        with_retry(4, || {
            // Clone per attempt: the body is consumed by `send`, and a retry needs the bytes
            // again. The extra copy only happens on the (rare) retry path for the first send
            // reqwest still moves the buffer in.
            let resp = self.http_bulk.put(put_url).body(data.clone()).send()?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(Error::Server {
                    status: status.as_u16(),
                    body: resp.text().unwrap_or_default(),
                })
            }
        })
    }

    /// GET a chunk's bytes from a presigned download URL. Bulk client + bounded
    /// retry-with-backoff (same URL reused) for the same reasons as `upload_chunk`.
    pub fn download_chunk(&self, get_url: &str) -> Result<Vec<u8>> {
        with_retry(4, || {
            let resp = self.http_bulk.get(get_url).send()?;
            let status = resp.status();
            if status.is_success() {
                Ok(resp.bytes()?.to_vec())
            } else {
                Err(Error::Server {
                    status: status.as_u16(),
                    body: resp.text().unwrap_or_default(),
                })
            }
        })
    }

    /// The `/cable` WebSocket URL for this client's device token (`http`→`ws`).
    fn cable_url(&self) -> Result<String> {
        let token = self.token.as_ref().ok_or(Error::Unauthenticated)?;
        let ws_base = self
            .base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Ok(format!("{ws_base}/cable?token={token}"))
    }

    /// Subscribe to the server's `LibraryChannel` and call `on_head_seq(seq)` for each
    /// `{head_seq}` nudge (the "pull now" signal — the server broadcasts it on every
    /// commit). **Blocking**: this is one connection's lifetime; it returns `Ok(())` if
    /// the server closes the stream cleanly, or `Err` on a socket/handshake failure.
    /// The caller reconnects on return — the nudge is a latency optimization over the
    /// cursor-delta poll, which remains the correctness backbone, so a dropped socket is
    /// not fatal. Pings/`confirm_subscription`/other frames are ignored.
    pub fn watch_library(&self, mut on_head_seq: impl FnMut(i64)) -> Result<()> {
        use tungstenite::Message;
        let (mut socket, _resp) = tungstenite::connect(self.cable_url()?)?;
        // Bound the blocking read so a silently-dead socket (NAT drop, laptop sleep) can't
        // wedge here forever — without this the daemon's reconnect loop, which only runs
        // when we RETURN, would never fire. ActionCable emits a liveness ping every ~3s as a
        // TEXT frame (`{"type":"ping"}`), so any read that blocks a full ~3x that window with
        // no frame at all means the connection is gone → return an error to trigger reconnect.
        let liveness = std::time::Duration::from_secs(10);
        set_ws_read_timeout(socket.get_mut(), Some(liveness));
        // ActionCable subscribe: the identifier is a *stringified* JSON object.
        let subscribe = serde_json::json!({
            "command": "subscribe",
            "identifier": "{\"channel\":\"LibraryChannel\"}",
        })
        .to_string();
        socket.send(Message::Text(subscribe))?;
        loop {
            match socket.read() {
                Ok(Message::Text(txt)) => {
                    // Envelope: {"identifier":..,"message":{"head_seq":N}}. Only act on
                    // frames that carry a head_seq (skip welcome/confirm/the liveness ping).
                    // Simply receiving any frame — including the ActionCable ping — resets the
                    // read timeout, which is our liveness signal.
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                        if let Some(seq) = v
                            .get("message")
                            .and_then(|m| m.get("head_seq"))
                            .and_then(|s| s.as_i64())
                        {
                            on_head_seq(seq);
                        }
                    }
                }
                Ok(Message::Ping(p)) => socket.send(Message::Pong(p))?,
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(e) => {
                    // A read timeout (WouldBlock/TimedOut) with no server ping in the window
                    // means the connection is dead; any other error is a real socket failure.
                    // Either way we return so the caller re-subscribes on a fresh connection —
                    // the cursor-delta poll is the correctness backbone, so a dropped nudge
                    // socket is never fatal.
                    return Err(e.into());
                }
            }
        }
    }
}

/// Set a read timeout on the (optionally TLS-wrapped) WebSocket's underlying TCP socket.
/// A `None` clears it. Best-effort: an unknown stream variant is left untouched.
fn set_ws_read_timeout(
    stream: &mut tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    dur: Option<std::time::Duration>,
) {
    use tungstenite::stream::MaybeTlsStream;
    match stream {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(dur);
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.get_ref().set_read_timeout(dur);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_id_matches_server_formula() {
        // SHA-256 of the concatenated hex hash strings, in order.
        let m = vec![
            ChunkRef {
                hash: "aa".into(),
                offset: 0,
                length: 1,
            },
            ChunkRef {
                hash: "bb".into(),
                offset: 1,
                length: 1,
            },
        ];
        let expected = hex::encode(Sha256::digest(b"aabb"));
        assert_eq!(manifest_id(&m), expected);
    }

    #[test]
    fn url_join_trims_trailing_slash() {
        let c = ServerClient::new("http://localhost:3001/");
        assert_eq!(c.url("/v1/tree"), "http://localhost:3001/v1/tree");
    }

    #[test]
    fn retryable_covers_transport_and_transient_server_errors() {
        // Transient server statuses → retry.
        for status in [500u16, 502, 503, 504, 408, 429] {
            assert!(
                is_retryable(&Error::Server {
                    status,
                    body: String::new()
                }),
                "status {status} should be retryable"
            );
        }
        // Definitive 4xx (expired presign 403, bad request 400, not found 404) → do not retry.
        for status in [400u16, 403, 404, 409, 422] {
            assert!(
                !is_retryable(&Error::Server {
                    status,
                    body: String::new()
                }),
                "status {status} should NOT be retryable"
            );
        }
        // Auth is a hard stop, not a transient blip.
        assert!(!is_retryable(&Error::Unauthenticated));
    }

    #[test]
    fn with_retry_returns_first_success_without_sleeping() {
        let mut calls = 0;
        let out: Result<u32> = with_retry(4, || {
            calls += 1;
            Ok(7)
        });
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls, 1, "a success must not retry");
    }

    #[test]
    fn with_retry_gives_up_immediately_on_non_retryable() {
        let mut calls = 0;
        let out: Result<u32> = with_retry(4, || {
            calls += 1;
            Err(Error::Server {
                status: 403,
                body: "expired".into(),
            })
        });
        assert!(out.is_err());
        assert_eq!(calls, 1, "a non-retryable error must not burn the backoff");
    }

    #[test]
    fn with_retry_exhausts_attempts_on_persistent_retryable() {
        // 2 attempts with a retryable error → exactly 2 calls (one initial + one retry),
        // then surfaces the error. Kept at 2 so the test's backoff sleep stays short.
        let mut calls = 0;
        let out: Result<u32> = with_retry(2, || {
            calls += 1;
            Err(Error::Server {
                status: 503,
                body: "busy".into(),
            })
        });
        assert!(out.is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    fn search_response_deserializes_server_shape() {
        // Locks the wire contract with Sync::Search's JSON: a file carries a size, a dir
        // has size = null (deserialized to None).
        let body = r#"{"results":[
            {"node_id":"n1","name":"Bracket.STL","kind":"file","path":"/Project/Bracket.STL","size":12},
            {"node_id":"n2","name":"Project","kind":"dir","path":"/Project","size":null}
        ]}"#;
        let resp: SearchResponse = serde_json::from_str(body).unwrap();
        assert_eq!(resp.results.len(), 2);
        assert_eq!(resp.results[0].path, "/Project/Bracket.STL");
        assert_eq!(resp.results[0].size, Some(12));
        assert_eq!(resp.results[1].kind, "dir");
        assert_eq!(resp.results[1].size, None);
    }

    /// A one-shot HTTP stub for the two requests `provision_library` makes, in order: the admin
    /// door (asserting the OPERATOR bearer) and whoami (asserting the freshly minted DEVICE
    /// bearer). Anything else, or the wrong bearer, answers 500 so the test fails on the response
    /// rather than hanging.
    fn provisioning_stub(admin: &'static str, minted: &'static str) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let expectations = [
                (
                    "POST /v1/admin/libraries",
                    admin,
                    format!("{{\"user_id\":42,\"device_id\":7,\"device_token\":\"{minted}\"}}"),
                    "201 Created",
                ),
                (
                    "GET /v1/devices/whoami",
                    minted,
                    "{\"user_id\":42,\"device_id\":7,\"head_seq\":9}".to_string(),
                    "200 OK",
                ),
            ];
            for (want_line, want_bearer, body, status) in expectations {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut bearer_ok = false;
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end().to_string();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line
                        .strip_prefix("authorization: ")
                        .or_else(|| line.strip_prefix("Authorization: "))
                    {
                        bearer_ok = v == format!("Bearer {want_bearer}");
                    }
                    if let Some(v) = line
                        .strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                    {
                        content_length = v.parse().unwrap_or(0);
                    }
                }
                let mut discard = vec![0u8; content_length];
                reader.read_exact(&mut discard).unwrap();
                let ok = request_line.starts_with(want_line) && bearer_ok;
                let (status, body) = if ok {
                    (status, body)
                } else {
                    (
                        "500 Internal Server Error",
                        format!("unexpected {request_line:?} bearer_ok={bearer_ok}"),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn provision_library_takes_the_admin_door_then_reads_head_seq_from_whoami() {
        let base = provisioning_stub("op-token", "minted-device-token");
        let mut c = ServerClient::new(&base);
        let reg = c
            .provision_library("op-token", "fresh@example.com", "device-A")
            .expect("provisioning");
        assert_eq!(reg.user_id, 42);
        assert_eq!(reg.device_id, 7);
        assert_eq!(reg.device_token, "minted-device-token");
        assert_eq!(
            reg.head_seq, 9,
            "head_seq comes from whoami, not the admin response"
        );
        assert_eq!(
            c.token.as_deref(),
            Some("minted-device-token"),
            "the client now holds the minted token"
        );
    }

    #[test]
    fn a_blank_admin_token_env_means_the_maker_path() {
        // The only test that touches this env var, so no other test can observe the window.
        std::env::set_var(ADMIN_TOKEN_ENV, "   ");
        assert!(admin_token_from_env().is_none(), "blank is unset");
        std::env::set_var(ADMIN_TOKEN_ENV, " op-token ");
        assert_eq!(
            admin_token_from_env().as_deref(),
            Some("op-token"),
            "trimmed"
        );
        std::env::remove_var(ADMIN_TOKEN_ENV);
        assert!(admin_token_from_env().is_none());
    }

    // ---- the login answer's shape (D1, 2026-10-13) ----
    //
    // The maker app's POST /v1/devices/login gained `library` (always) and `sync` (a grant on a
    // syncable deployment, null otherwise). A server older than D1 sends neither, and must still
    // parse: a client that refused the old answer could not sign in to the server it shipped with.

    /// What every D1 login answer carries beside the pre-D1 four fields.
    const NEW_LOGIN_ANSWER: &str = r#"{
        "user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":42,
        "library":{"backend":"chunk_remote","syncable":true},
        "sync":{"url":"https://sync.tinkerex.example","device_token":"sync-tok",
                "user_id":8,"device_id":21,"head_seq":5}
    }"#;

    #[test]
    fn an_old_login_answer_parses_with_no_grant_and_no_library() {
        let reg: RegisterResponse = serde_json::from_str(
            r#"{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":42}"#,
        )
        .expect("the pre-D1 answer must still parse");
        assert_eq!(reg.device_token, "maker-tok");
        assert_eq!(reg.head_seq, 42);
        assert!(reg.sync.is_none());
        assert!(reg.library.is_none());
    }

    #[test]
    fn a_new_login_answer_carries_the_library_and_the_sync_grant() {
        let reg: RegisterResponse = serde_json::from_str(NEW_LOGIN_ANSWER).unwrap();
        let library = reg.library.expect("library");
        assert_eq!(library.backend, "chunk_remote");
        assert!(library.syncable);
        let grant = reg.sync.expect("sync grant");
        assert_eq!(grant.url, "https://sync.tinkerex.example");
        assert_eq!(grant.device_token, "sync-tok");
        assert_eq!((grant.user_id, grant.device_id, grant.head_seq), (8, 21, 5));
        // Two devices on two servers: the maker token is not the grant's.
        assert_eq!(reg.device_token, "maker-tok");
    }

    #[test]
    fn a_non_syncable_answer_parses_with_a_null_grant() {
        let reg: RegisterResponse = serde_json::from_str(
            r#"{"user_id":3,"device_id":11,"device_token":"maker-tok","head_seq":0,
                "library":{"backend":"share","syncable":false},"sync":null}"#,
        )
        .unwrap();
        assert!(reg.sync.is_none());
        let library = reg.library.expect("library is sent on every deployment");
        assert_eq!(library.backend, "share");
        assert!(!library.syncable);
    }

    /// A one-request HTTP stub: answers `status` + `body` when the request line starts with
    /// `want_line`, and 500 otherwise, so a wrong route fails on the response instead of hanging.
    fn one_shot_stub(want_line: &'static str, status: &'static str, body: &'static str) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_ascii_lowercase();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.strip_prefix("content-length: ") {
                    content_length = v.parse().unwrap_or(0);
                }
            }
            let mut discard = vec![0u8; content_length];
            reader.read_exact(&mut discard).unwrap();
            let (status, body) = if request_line.starts_with(want_line) {
                (status, body.to_string())
            } else {
                (
                    "500 Internal Server Error",
                    format!("unexpected {request_line:?}"),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            reader.get_mut().write_all(response.as_bytes()).unwrap();
        });
        format!("http://{addr}")
    }

    #[test]
    fn device_login_keeps_the_maker_token_and_leaves_the_grant_to_the_caller() {
        // This client is the MAKER app's client (`/v1/maker/*` takes the maker token); the
        // sync routes go to `sync.url` through a client the caller builds from the grant.
        let base = one_shot_stub("POST /v1/devices/login", "201 Created", NEW_LOGIN_ANSWER);
        let mut c = ServerClient::new(&base);
        let reg = c
            .device_login("ada@example.com", "pw", "laptop")
            .expect("login");
        assert_eq!(
            c.token(),
            Some("maker-tok"),
            "the client keeps the maker token"
        );
        assert_eq!(reg.sync.expect("grant").device_token, "sync-tok");
    }

    // ---- a server refusing this device's token (tinkerex#472) ----

    const REFUSED: &str = r#"{"error":"unauthorized"}"#;

    #[test]
    fn an_auth_rejection_is_a_401_carrying_the_apis_json_error() {
        // What both the maker app (V1::Authentication) and the sync server (ApiError::
        // Unauthorized) answer a revoked, expired or unknown device token.
        assert!(Error::Server {
            status: 401,
            body: REFUSED.into()
        }
        .is_auth_rejection());
    }

    #[test]
    fn nothing_but_that_401_is_an_auth_rejection() {
        // Signing a device out on anything else would sign it out for being offline. A proxy's
        // 401 (HTML, or no body) is not the server refusing the token; nor is any other status.
        for body in [
            "<html><body>401 Authorization Required</body></html>",
            "",
            "unauthorized",
        ] {
            assert!(
                !Error::Server {
                    status: 401,
                    body: body.into()
                }
                .is_auth_rejection(),
                "a 401 with body {body:?} is not the API's refusal"
            );
        }
        for status in [400u16, 403, 404, 408, 429, 500, 502, 503, 504] {
            assert!(
                !Error::Server {
                    status,
                    body: REFUSED.into()
                }
                .is_auth_rejection(),
                "status {status}"
            );
        }
        // No token configured locally is a programming error, not the server's verdict.
        assert!(!Error::Unauthenticated.is_auth_rejection());
        // Network down: connection refused.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            drop(l);
            url
        };
        let down = ServerClient::with_token(closed, "tok")
            .whoami()
            .unwrap_err();
        assert!(matches!(down, Error::Http(_)), "{down}");
        assert!(!down.is_auth_rejection());
    }

    #[test]
    fn a_revoked_token_reads_as_an_auth_rejection_on_both_servers_routes() {
        let sync = one_shot_stub("GET /v1/devices/whoami", "401 Unauthorized", REFUSED);
        let e = ServerClient::with_token(sync, "dead").whoami().unwrap_err();
        assert!(e.is_auth_rejection(), "{e}");

        let maker = one_shot_stub("GET /v1/maker/tags", "401 Unauthorized", REFUSED);
        let e = ServerClient::with_token(maker, "dead")
            .list_tags()
            .unwrap_err();
        assert!(e.is_auth_rejection(), "{e}");
    }
}
