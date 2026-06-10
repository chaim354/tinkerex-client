use std::time::SystemTime;

use async_trait::async_trait;

/// Metadata for a single filesystem entry, mapped onto what a CfAPI
/// `FETCH_PLACEHOLDERS` / File Provider enumeration needs: name, dir flag,
/// size, mtime, and an opaque version tag for change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Opaque per-version identity (WebDAV ETag now; could be a content hash
    /// from the control plane later). Used to decide whether a hydrated
    /// placeholder is stale and must be re-fetched.
    pub etag: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// The only surface the native on-demand providers depend on.
///
/// Keep this minimal and storage-agnostic: every method must be expressible
/// both over WebDAV (HTTP) and over libjfs (FFI). Anything CfAPI-specific or
/// HTTP-specific belongs in the provider or the impl, not here.
#[async_trait]
pub trait JuiceFsBackend: Send + Sync {
    /// Stat a single path. `NotFound` if it does not exist.
    async fn stat(&self, path: &str) -> Result<Metadata>;

    /// List the direct children of a directory (excludes the directory itself).
    /// Drives `FETCH_PLACEHOLDERS` / replicated-extension enumeration.
    async fn readdir(&self, path: &str) -> Result<Vec<Metadata>>;

    /// Read `len` bytes starting at `offset`. Drives `FETCH_DATA` hydration.
    /// Returns fewer bytes than requested only at end-of-file.
    async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>>;

    /// Write the full contents of `path` (creating or replacing it).
    /// Drives writeback when the OS hands back a locally-modified file.
    async fn write(&self, path: &str, data: &[u8]) -> Result<()>;
}
