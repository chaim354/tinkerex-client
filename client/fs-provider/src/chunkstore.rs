//! Where chunk bytes live. Content-addressed (keyed by hex SHA-256).
//!
//! [`LocalChunkStore`] is a local directory — used for tests and the local cache.
//! The managed backend will be an R2/S3 implementation that fetches via presigned
//! GET and uploads via presigned PUT; it implements this same trait, so the
//! [`Provider`](crate::Provider) doesn't change.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process counter making each in-flight temp file unique (see `put`).
static TMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// A content-addressed blob store. `hash` is the hex SHA-256 of `data`.
pub trait ChunkStore: Send + Sync {
    fn get(&self, hash: &str) -> std::io::Result<Vec<u8>>;
    /// Idempotent: storing a hash that already exists is a no-op (this is the dedup).
    fn put(&self, hash: &str, data: &[u8]) -> std::io::Result<()>;
    fn has(&self, hash: &str) -> bool;
}

/// Chunks as files under a directory, sharded by the first two hex chars.
pub struct LocalChunkStore {
    root: PathBuf,
}

impl LocalChunkStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// A fresh store in a unique temp dir (for tests).
    pub fn temp() -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("fsprov-chunks-{}", std::process::id()));
        // Unique-enough per process; tests in one binary share it harmlessly (CAS).
        Self::new(dir)
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        if hash.len() >= 2 {
            self.root.join(&hash[0..2]).join(hash)
        } else {
            self.root.join(hash)
        }
    }
}

impl ChunkStore for LocalChunkStore {
    fn get(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.path_for(hash))
    }

    fn put(&self, hash: &str, data: &[u8]) -> std::io::Result<()> {
        let p = self.path_for(hash);
        if p.exists() {
            return Ok(()); // dedup: already stored
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a *unique* temp file then rename, so a concurrent reader never sees
        // a half-written chunk. The temp name must be unique per writer: two threads
        // storing the same chunk hash (CAS dedup, identical content) would otherwise
        // share one temp path and race on the rename (the loser hits ENOENT). The sync
        // daemon writes this store from multiple threads (flush + hydrate), so this is
        // load-bearing, not theoretical.
        let nonce = TMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = p.with_extension(format!("tmp.{}.{}", std::process::id(), nonce));
        std::fs::write(&tmp, data)?;
        match std::fs::rename(&tmp, &p) {
            Ok(()) => Ok(()),
            // A concurrent writer landed the same chunk first — that's the dedup win,
            // not an error. Drop our temp copy and treat it as stored.
            Err(_) if p.exists() => {
                let _ = std::fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn has(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }
}
