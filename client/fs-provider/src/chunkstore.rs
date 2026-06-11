//! Where chunk bytes live. Content-addressed (keyed by hex SHA-256).
//!
//! [`LocalChunkStore`] is a local directory — used for tests and the local cache.
//! The managed backend will be an R2/S3 implementation that fetches via presigned
//! GET and uploads via presigned PUT; it implements this same trait, so the
//! [`Provider`](crate::Provider) doesn't change.

use std::path::PathBuf;

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
        // Write to a temp file then rename, so a concurrent reader never sees a
        // half-written chunk.
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    fn has(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }
}
