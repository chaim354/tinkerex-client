//! The sync daemon. Bridges `fs-provider` (local SQLite tree + chunk cache) and
//! `sync-client` (the server API) to **push** local changes and **pull** remote
//! ones, with on-demand **hydration** of chunk bytes.
//!
//! Chunk bytes move through a pluggable [`ChunkIO`]:
//! - [`SharedDirIO`] — a shared local directory (test/dev stand-in for the cloud).
//! - [`PresignedIO`] — PUT/GET to the presigned URLs the server mints (R2/SeaweedFS).
//!
//! Metadata always flows through the real server API. `sync_once` is the loop body.

use std::collections::HashMap;
use std::path::PathBuf;

use fs_provider::{ChunkLoc, Kind, SqliteProvider, SyncNode};
use sync_client::{Change, ChunkRef, CommitBegin, ServerClient, TreeNode};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fs: {0}")]
    Fs(#[from] fs_provider::FsError),
    #[error("api: {0}")]
    Api(#[from] sync_client::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("no presigned url for {0}")]
    NoUrl(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Moves chunk bytes to/from "the cloud".
pub trait ChunkIO {
    /// Upload `hash`'s bytes. `put_urls` are from `commit_begin` (presigned PUTs).
    fn put(&self, hash: &str, put_urls: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()>;
    /// Download `hash`'s bytes. `get_url` is from `/chunks/download` (or `None`).
    fn get(&self, hash: &str, get_url: Option<&str>) -> Result<Vec<u8>>;
}

/// The cloud is a local directory. Stand-in for R2/SeaweedFS in tests/dev; ignores
/// the presigned URLs and keys bytes by content hash.
pub struct SharedDirIO {
    dir: PathBuf,
}

impl SharedDirIO {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }
    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(hash)
    }
}

impl ChunkIO for SharedDirIO {
    fn put(&self, hash: &str, _urls: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()> {
        std::fs::write(self.path(hash), bytes)?;
        Ok(())
    }
    fn get(&self, hash: &str, _url: Option<&str>) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path(hash))?)
    }
}

/// PUT/GET to the presigned URLs the server mints — the production transport
/// (R2 / SeaweedFS S3). Bytes go client ⇄ storage directly.
pub struct PresignedIO<'a> {
    client: &'a ServerClient,
}

impl<'a> PresignedIO<'a> {
    pub fn new(client: &'a ServerClient) -> Self {
        Self { client }
    }
}

impl ChunkIO for PresignedIO<'_> {
    fn put(&self, hash: &str, put_urls: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()> {
        let url = put_urls.get(hash).ok_or_else(|| Error::NoUrl(hash.to_string()))?;
        self.client.upload_chunk(url, bytes)?;
        Ok(())
    }
    fn get(&self, hash: &str, get_url: Option<&str>) -> Result<Vec<u8>> {
        let url = get_url.ok_or_else(|| Error::NoUrl(hash.to_string()))?;
        Ok(self.client.download_chunk(url)?)
    }
}

pub struct SyncEngine;

impl SyncEngine {
    /// Push the whole local tree to the server, uploading missing chunks via `io`.
    pub fn push(provider: &SqliteProvider, client: &ServerClient, io: &dyn ChunkIO) -> Result<()> {
        for node in provider.export_tree()? {
            match node.kind {
                Kind::Dir => {
                    client.mkdir(&node.node_id, node.parent_node_id.as_deref(), &node.name)?;
                }
                Kind::File => {
                    let manifest: Vec<ChunkRef> = node
                        .chunks
                        .iter()
                        .map(|c| ChunkRef { hash: c.hash.clone(), offset: c.offset, length: c.length })
                        .collect();
                    let size = node.chunks.iter().map(|c| c.length).sum();
                    let commit_id = uuid::Uuid::new_v4().to_string();
                    let begin = client.commit_begin(&CommitBegin {
                        commit_id: commit_id.clone(),
                        node_id: node.node_id.clone(),
                        parent_node_id: node.parent_node_id.clone(),
                        name: node.name.clone(),
                        base_version_id: None,
                        size,
                        manifest,
                    })?;
                    for hash in &begin.missing {
                        let bytes = provider.chunk_get(hash)?;
                        io.put(hash, &begin.put_urls, bytes)?;
                    }
                    client.commit_finalize(&commit_id)?;
                }
            }
        }
        Ok(())
    }

    /// Initial sync: apply the full-tree snapshot (paginated) to the local provider.
    pub fn pull_snapshot(provider: &SqliteProvider, client: &ServerClient) -> Result<i64> {
        let mut after = 0;
        let mut head = 0;
        loop {
            let tree = client.tree(after)?;
            head = tree.head_seq;
            for n in &tree.nodes {
                provider.apply_remote(&tree_node_to_sync(n))?;
            }
            match tree.next_after_id {
                Some(id) if !tree.nodes.is_empty() => after = id,
                _ => break,
            }
        }
        Ok(head)
    }

    /// Incremental: apply changelog entries since `cursor`. Returns the new cursor.
    pub fn pull_changes(provider: &SqliteProvider, client: &ServerClient, cursor: i64) -> Result<i64> {
        let resp = client.changes(cursor)?;
        for ch in &resp.changes {
            if let Some(sn) = change_to_sync(ch) {
                provider.apply_remote(&sn)?;
            }
            // NOTE: 'delete' tombstones are not yet applied locally (basic daemon).
        }
        Ok(resp.head_seq)
    }

    /// Hydrate a file's chunks from the cloud into the local cache.
    pub fn hydrate(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
    ) -> Result<()> {
        let hashes = provider.node_manifest_hashes(node_id)?;
        let missing: Vec<String> = hashes.into_iter().filter(|h| !provider.has_chunk(h)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let urls = client.download_urls(&missing)?.urls;
        for h in &missing {
            let bytes = io.get(h, urls.get(h).map(String::as_str))?;
            provider.chunk_put(h, &bytes)?;
        }
        Ok(())
    }

    /// One loop iteration: push local changes, then pull remote ones.
    pub fn sync_once(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        cursor: i64,
    ) -> Result<i64> {
        Self::push(provider, client, io)?;
        Self::pull_changes(provider, client, cursor)
    }
}

fn tree_node_to_sync(n: &TreeNode) -> SyncNode {
    let manifest = n.manifest.clone().unwrap_or_default();
    let manifest_id = (!manifest.is_empty()).then(|| sync_client::manifest_id(&manifest));
    let chunks = manifest
        .into_iter()
        .map(|c| ChunkLoc { hash: c.hash, offset: c.offset, length: c.length })
        .collect();
    SyncNode {
        node_id: n.node_id.clone(),
        parent_node_id: n.parent_node_id.clone(),
        name: n.name.clone(),
        kind: if n.kind == "dir" { Kind::Dir } else { Kind::File },
        manifest_id,
        chunks,
    }
}

fn change_to_sync(ch: &Change) -> Option<SyncNode> {
    if ch.op == "delete" {
        return None;
    }
    let view = ch.node.as_ref()?;
    let manifest = ch.manifest.clone().unwrap_or_default();
    let manifest_id = (!manifest.is_empty()).then(|| sync_client::manifest_id(&manifest));
    let chunks = manifest
        .into_iter()
        .map(|c| ChunkLoc { hash: c.hash, offset: c.offset, length: c.length })
        .collect();
    Some(SyncNode {
        node_id: view.node_id.clone(),
        parent_node_id: view.parent_node_id.clone(),
        name: view.name.clone(),
        kind: if view.kind == "dir" { Kind::Dir } else { Kind::File },
        manifest_id,
        chunks,
    })
}
