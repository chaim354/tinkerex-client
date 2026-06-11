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
pub struct NodeView {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub deleted: bool,
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
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: None,
            http: reqwest::blocking::Client::new(),
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

    pub fn mkdir(&self, node_id: &str, parent_node_id: Option<&str>, name: &str) -> Result<SeqResponse> {
        let resp = self
            .authed(self.http.post(self.url("/v1/nodes")))?
            .json(&serde_json::json!({ "node_id": node_id, "parent_node_id": parent_node_id, "name": name }))
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

    pub fn changes(&self, since: i64) -> Result<ChangesResponse> {
        let resp = self
            .authed(self.http.get(self.url("/v1/changes")))?
            .query(&[("since", since.to_string())])
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
}
