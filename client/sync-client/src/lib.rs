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

#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub user_id: i64,
    pub device_id: i64,
    pub device_token: String,
    pub head_seq: i64,
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
pub struct PairingResponse {
    /// The human-enterable code to type on the new device (shown once).
    pub code: String,
    pub expires_at: String,
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
    pub split_node_id: String,
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

/// Cheap to clone — `reqwest::blocking::Client` shares its connection pool via an
/// internal `Arc`, so the sync daemon hands a clone to each of its threads.
#[derive(Clone)]
pub struct ServerClient {
    base: String,
    token: Option<String>,
    http: reqwest::blocking::Client,
}

impl ServerClient {
    pub fn new(base: impl Into<String>) -> Self {
        // A default request timeout so a stalled connection can't wedge the caller forever — a
        // hung push on the sync thread would otherwise block graceful shutdown (join never
        // returns). 30s is ample for a chunk upload/download yet bounds the worst case so an
        // unmount can't stall on a dead connection; short-deadline ops (locks) set their own
        // tighter per-request timeout.
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: None,
            http,
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

    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::RequestBuilder> {
        let token = self.token.as_ref().ok_or(Error::Unauthenticated)?;
        Ok(rb.bearer_auth(token))
    }

    fn json<T: for<'de> Deserialize<'de>>(resp: reqwest::blocking::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp.json()?)
        } else {
            Err(Error::Server { status: status.as_u16(), body: resp.text().unwrap_or_default() })
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
            Err(Error::Server { status: status.as_u16(), body: resp.text().unwrap_or_default() })
        }
    }

    /// Bootstrap: register a user/device. Stores the returned token on the client.
    pub fn register(&mut self, email: &str, device_name: &str) -> Result<RegisterResponse> {
        let resp = self
            .http
            .post(self.url("/v1/auth/register"))
            .json(&serde_json::json!({ "email": email, "device_name": device_name }))
            .send()?;
        let reg: RegisterResponse = Self::json(resp)?;
        self.token = Some(reg.device_token.clone());
        Ok(reg)
    }

    /// Register another device for the current user (returns a new device token).
    pub fn create_device(&self, name: &str) -> Result<DeviceResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/devices")))?
            .json(&serde_json::json!({ "name": name }))
            .send()?;
        Self::json(resp)
    }

    /// Issue a short-lived pairing code (this device must be authenticated). A *new*
    /// device redeems it via [`pair`](Self::pair) to join this library — without ever
    /// seeing this device's token.
    pub fn create_pairing(&self) -> Result<PairingResponse> {
        let resp = self.authed(self.http.post(self.url("/v1/devices/pairing")))?.send()?;
        Self::json(resp)
    }

    /// Redeem a pairing code on a new device to join an existing library. Stores the
    /// returned token on the client (like `register`).
    pub fn pair(&mut self, email: &str, code: &str, device_name: &str) -> Result<RegisterResponse> {
        let resp = self
            .http
            .post(self.url("/v1/auth/pair"))
            .json(&serde_json::json!({ "email": email, "code": code, "device_name": device_name }))
            .send()?;
        let reg: RegisterResponse = Self::json(resp)?;
        self.token = Some(reg.device_token.clone());
        Ok(reg)
    }

    pub fn commit_begin(&self, req: &CommitBegin) -> Result<CommitBeginResponse> {
        let resp = self.authed(self.http.post(self.url("/v1/commit")))?.json(req).send()?;
        Self::json(resp)
    }

    pub fn commit_finalize(&self, commit_id: &str) -> Result<FinalizeResponse> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/commit/{commit_id}/finalize"))))?
            .send()?;
        Self::json(resp)
    }

    pub fn mkdir(&self, node_id: &str, parent_node_id: Option<&str>, name: &str, mode: u32, mtime: i64) -> Result<SeqResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/nodes")))?
            .json(&serde_json::json!({ "node_id": node_id, "parent_node_id": parent_node_id, "name": name, "mode": mode, "mtime": mtime }))
            .send()?;
        Self::json(resp)
    }

    pub fn delete_node(&self, node_id: &str) -> Result<SeqResponse> {
        let resp = self.authed(self.http.delete(self.url(&format!("/v1/nodes/{node_id}"))))?.send()?;
        Self::json(resp)
    }

    pub fn restore(&self, node_id: &str, version_id: &str) -> Result<FinalizeResponse> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/nodes/{node_id}/restore"))))?
            .json(&serde_json::json!({ "version_id": version_id }))
            .send()?;
        Self::json(resp)
    }

    /// The version history of a node (newest first) — for the file browser's
    /// versions/restore UI.
    pub fn versions(&self, node_id: &str) -> Result<VersionsResponse> {
        let resp = self
            .authed(self.http.get(self.url(&format!("/v1/nodes/{node_id}/versions"))))?
            .send()?;
        Self::json(resp)
    }

    /// Best-effort acquire/refresh the advisory edit lock on a node. Short timeout: if we
    /// can't reach the server the caller proceeds locally (offline just opens the cached
    /// copy — conflict detection is the safety net). Returns the lock status.
    pub fn lock(&self, node_id: &str) -> Result<LockStatus> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/nodes/{node_id}/lock"))))?
            .timeout(std::time::Duration::from_secs(2))
            .send()?;
        Self::json(resp)
    }

    /// Release the advisory lock on a node (no-op if this device isn't the holder).
    pub fn unlock(&self, node_id: &str) -> Result<()> {
        self.authed(self.http.post(self.url(&format!("/v1/nodes/{node_id}/unlock"))))?
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
        let resp = self.authed(self.http.get(self.url("/v1/conflicts")))?.send()?;
        Self::json(resp)
    }

    /// Resolve a conflict by keeping one side as current.
    pub fn resolve_conflict_keep(&self, node_id: &str, keep_version_id: &str) -> Result<SeqResponse> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/nodes/{node_id}/resolve_conflict"))))?
            .json(&serde_json::json!({ "keep_version_id": keep_version_id }))
            .send()?;
        Self::json(resp)
    }

    /// Resolve a conflict by keeping both — the incoming side becomes a new sibling file.
    pub fn resolve_conflict_split(&self, node_id: &str) -> Result<SplitResponse> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/nodes/{node_id}/resolve_conflict"))))?
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

    /// PUT a chunk's bytes to a presigned upload URL (returned by `commit_begin`).
    pub fn upload_chunk(&self, put_url: &str, data: Vec<u8>) -> Result<()> {
        let resp = self.http.put(put_url).body(data).send()?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(Error::Server { status: status.as_u16(), body: resp.text().unwrap_or_default() })
        }
    }

    /// GET a chunk's bytes from a presigned download URL.
    pub fn download_chunk(&self, get_url: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(get_url).send()?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp.bytes()?.to_vec())
        } else {
            Err(Error::Server { status: status.as_u16(), body: resp.text().unwrap_or_default() })
        }
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
        // ActionCable subscribe: the identifier is a *stringified* JSON object.
        let subscribe = serde_json::json!({
            "command": "subscribe",
            "identifier": "{\"channel\":\"LibraryChannel\"}",
        })
        .to_string();
        socket.send(Message::Text(subscribe))?;
        loop {
            match socket.read()? {
                Message::Text(txt) => {
                    // Envelope: {"identifier":..,"message":{"head_seq":N}}. Only act on
                    // frames that carry a head_seq (skip welcome/confirm/ping).
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                        if let Some(seq) =
                            v.get("message").and_then(|m| m.get("head_seq")).and_then(|s| s.as_i64())
                        {
                            on_head_seq(seq);
                        }
                    }
                }
                Message::Ping(p) => socket.send(Message::Pong(p))?,
                Message::Close(_) => return Ok(()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_id_matches_server_formula() {
        // SHA-256 of the concatenated hex hash strings, in order.
        let m = vec![
            ChunkRef { hash: "aa".into(), offset: 0, length: 1 },
            ChunkRef { hash: "bb".into(), offset: 1, length: 1 },
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
}
