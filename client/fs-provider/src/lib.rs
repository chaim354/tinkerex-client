//! Local-first filesystem provider.
//!
//! This is the platform-agnostic core every mount adapter sits on top of:
//! - a **local SQLite tree** (instant `lookup`/`getattr`/`readdir` — no WAN), and
//! - a **content-addressed chunk store** (files are ordered lists of content-hashed
//!   chunks; reads hydrate chunks on demand, writes chunk-and-dedup via FastCDC).
//!
//! The [`Provider`] trait is the seam: a FUSE filesystem (`fuser`/`winfsp-rs`), a
//! Windows CfAPI provider, or a macOS File Provider extension each translate OS
//! callbacks into these calls. The bytes come from a [`ChunkStore`] — a local dir
//! today, R2/S3 (via presigned GET/PUT) later — so the same logic runs everywhere.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub mod chunkstore;
pub use chunkstore::{ChunkStore, LocalChunkStore};

pub mod locks;
pub use locks::{LockManager, LockRange};

pub mod subscriptions;
pub use subscriptions::SubRule;

pub const ROOT_INO: u64 = 1;

/// The default permission bits for a freshly created node (no umask modeling — the
/// mount runs as the user, who owns everything).
fn default_mode(kind: Kind) -> u32 {
    match kind {
        Kind::Dir => 0o755,
        Kind::File => 0o644,
    }
}

/// Wall-clock **nanoseconds** since the epoch — the real `mtime` stamp for a *local*
/// modification (a new write with no caller-supplied time). A pulled remote change carries
/// the source device's real mtime instead (see [`SqliteProvider::apply_remote`]); we no
/// longer stamp `now` on pull, which used to destroy the true modification time (breaking
/// build tools that key on it). Cache freshness after a pull is handled by an explicit
/// adapter `invalidate(ino)` (the `content_changed` flag [`apply_remote`] returns), not an
/// mtime bump. Nanos resolution matches what build tools and the future CfAPI/File-Provider
/// metadata want; stored as i64 (a `bigint` on the wire/server — unix-nanos in 2026
/// overflow a 32-bit int).
fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
        }
    }
    fn from_str(s: &str) -> Kind {
        if s == "dir" {
            Kind::Dir
        } else {
            Kind::File
        }
    }
}

/// Attributes returned to the mount layer (maps onto FUSE attr / CfAPI placeholder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub kind: Kind,
    pub size: u64,
    /// Real modification time, **unix nanoseconds** (see [`now_nanos`]). Preserved + synced
    /// so a file's true mtime is stable across devices (not the sync time).
    pub mtime: i64,
    /// POSIX permission bits (e.g. `0o644`, `0o755`). Preserved + synced so the
    /// executable bit (and other mode bits) survive a chmod and propagate across devices.
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub kind: Kind,
}

/// A chunk's location within a file (sync export).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLoc {
    pub hash: String,
    pub offset: u64,
    pub length: u64,
}

/// A sync-ready node descriptor: the stable server id, its parent's id, and (for
/// files) the manifest. This is what the sync engine pushes to the server — the
/// glue between the local inode tree and the server's UUID-keyed tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncNode {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub kind: Kind,
    pub manifest_id: Option<String>,
    pub chunks: Vec<ChunkLoc>,
    /// The server version this node currently reflects. On **pull** it's the version the
    /// server reports (stored locally); on **push** it's sent as the commit's
    /// `base_version_id` so the server can detect a conflicting concurrent edit. `None`
    /// for dirs and never-synced nodes.
    pub version_id: Option<String>,
    /// POSIX permission bits, synced so the executable bit survives across devices.
    pub mode: u32,
    /// Real modification time (unix nanoseconds). Synced so the true mtime — not the sync
    /// time — is what every device sees. On **push** it's the local node's mtime; on
    /// **pull** it's the source device's mtime, applied verbatim by [`SqliteProvider::apply_remote`].
    pub mtime: i64,
}

/// What [`SqliteProvider::apply_remote`] did, so the caller can refresh the OS cache. A
/// pull that **repointed an existing node at new content** must explicitly invalidate the
/// kernel's data cache for that inode (we no longer bump mtime to force a re-read). A brand
/// new node, or a metadata-only change, has nothing cached to invalidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// The local inode the node maps to (for the adapter's `invalidate(ino)`).
    pub ino: u64,
    /// The node already existed and its manifest (content) changed — invalidate its cache.
    pub content_changed: bool,
    /// The node did not exist locally before this pull (a fresh INSERT, not an UPDATE). The
    /// adapter should *create* a fresh placeholder for it (e.g. CfAPI `CfCreatePlaceholders`)
    /// rather than invalidate an existing one — there's nothing cached to invalidate yet.
    pub inserted: bool,
    /// The collision sweep displaced a *different* local node at this `(parent, name)` that
    /// still had a **pending outbox entry** — i.e. un-synced local intent (typically a local
    /// rename/create into a name a concurrent remote also claimed). The remote is
    /// authoritative so the incoming node won the name, but the displaced node's local edit
    /// was clobbered: it is no longer in the tree, yet its outbox entry will push as a no-op
    /// (the row is gone), so the changelog delta alone can never re-add it. The pull caller
    /// must trigger the full-tree **reconcile** (`pull_snapshot`) to reconverge — the
    /// displaced node is still live on the server under its prior name and reconcile re-adds
    /// it there (the server's collision adjudication is the conflict's authoritative
    /// resolution). Without this signal the loss would be silent + permanent.
    pub displaced_pending: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("not found")]
    NotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("already exists")]
    AlreadyExists,
    #[error("directory not empty")]
    NotEmpty,
    #[error("chunk missing: {0}")]
    ChunkMissing(String),
    #[error("locked (would block)")]
    WouldBlock,
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, FsError>;

/// FastCDC chunking parameters. Large defaults suit big maker files; tests shrink them.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            min: 256 * 1024,
            avg: 1024 * 1024,
            max: 4 * 1024 * 1024,
        }
    }
}

/// The seam. Every platform mount adapter calls only these methods.
pub trait Provider: Send + Sync {
    fn lookup(&self, parent: u64, name: &str) -> Result<Attr>;
    fn getattr(&self, ino: u64) -> Result<Attr>;
    fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>>;
    /// Hydrate `size` bytes at `offset` (from in-progress writes or the chunk store).
    fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>>;
    fn create(&self, parent: u64, name: &str) -> Result<Attr>;
    fn mkdir(&self, parent: u64, name: &str) -> Result<Attr>;
    fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32>;
    /// Finalize writeback: chunk the staged content, store + dedup, update the tree.
    fn flush(&self, ino: u64) -> Result<()>;
    /// Set permission bits (the kernel's `chmod`). Default no-op; `SqliteProvider`
    /// persists + syncs it so the executable bit survives and propagates.
    fn set_mode(&self, _ino: u64, _mode: u32) -> Result<()> {
        Ok(())
    }
    /// Set the modification time (unix nanoseconds — the kernel's `setattr(mtime)`, e.g. a
    /// `utimensat`/`cp -p`). Default no-op; `SqliteProvider` persists + syncs it so the real
    /// mtime survives and propagates instead of being overwritten with the sync time.
    fn set_mtime(&self, _ino: u64, _mtime: i64) -> Result<()> {
        Ok(())
    }
    /// Resize the file to exactly `size` bytes (the kernel's `setattr`/`O_TRUNC`).
    /// Shrinking drops any content past the new EOF (so an overwrite-with-shorter
    /// leaves no stale tail); growing zero-fills. The new length is what `flush`
    /// will commit and what the sync push will export.
    fn truncate(&self, ino: u64, size: u64) -> Result<()>;
    fn unlink(&self, parent: u64, name: &str) -> Result<()>;
    fn rmdir(&self, parent: u64, name: &str) -> Result<()>;
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()>;

    // ---- advisory locking (POSIX byte-range; local to this device) -------------------
    // Defaults are a no-op "always succeeds" — a provider without lock support behaves as
    // if nothing is ever locked. `SqliteProvider` overrides with real tracking.

    /// `F_SETLK`: acquire `lock`. `Err(WouldBlock)` if another owner holds a conflict.
    fn set_lock(&self, _ino: u64, _lock: LockRange) -> Result<()> {
        Ok(())
    }
    /// `F_GETLK`: the conflicting lock a `set_lock` would hit, or `None` if free.
    fn test_lock(&self, _ino: u64, _probe: LockRange) -> Result<Option<LockRange>> {
        Ok(None)
    }
    /// `F_UNLCK`: release the owner's locks over `[start, end]`.
    fn unlock(&self, _ino: u64, _owner: u64, _start: u64, _end: u64) -> Result<()> {
        Ok(())
    }
    /// Release every lock an owner holds on `ino` (on file close).
    fn release_locks(&self, _ino: u64, _owner: u64) -> Result<()> {
        Ok(())
    }
}

/// SQLite-backed [`Provider`]. Tree in SQLite, bytes in a [`ChunkStore`].
pub struct SqliteProvider {
    conn: Mutex<Connection>,
    store: Box<dyn ChunkStore>,
    /// In-progress writes, buffered in memory until `flush`. (Production streams to a
    /// staging file; in-memory keeps the foundation simple and fully testable.)
    staging: Mutex<HashMap<u64, Vec<u8>>>,
    chunk_cfg: ChunkConfig,
    /// Local advisory file locks (POSIX byte-range), honored between processes on this
    /// device. Lives in memory: locks are inherently per-mount and released on close.
    locks: LockManager,
}

impl SqliteProvider {
    pub fn open(db_path: &Path, store: Box<dyn ChunkStore>) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        Self::init(conn, store, ChunkConfig::default())
    }

    pub fn in_memory(store: Box<dyn ChunkStore>, chunk_cfg: ChunkConfig) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, store, chunk_cfg)
    }

    fn init(conn: Connection, store: Box<dyn ChunkStore>, chunk_cfg: ChunkConfig) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nodes (
                ino         INTEGER PRIMARY KEY,
                parent_ino  INTEGER,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                size        INTEGER NOT NULL DEFAULT 0,
                mtime       INTEGER NOT NULL DEFAULT 0,  -- real mtime, unix NANOSECONDS (preserved + synced)
                manifest_id TEXT,
                node_id     TEXT,
                version_id  TEXT,         -- server version this node reflects (commit base)
                mode        INTEGER       -- POSIX permission bits (preserved + synced)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_ino, name);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_node_id ON nodes(node_id);
            CREATE TABLE IF NOT EXISTS manifest_chunks (
                manifest_id TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                chunk_hash  TEXT NOT NULL,
                offset      INTEGER NOT NULL,
                length      INTEGER NOT NULL,
                PRIMARY KEY (manifest_id, seq)
            );
            -- Sync outbox: node_ids dirtied by local writes, drained by the sync
            -- engine's push. Insertion-order `id` preserves parent-before-child order
            -- (a child can't be created before its parent exists in the tree).
            CREATE TABLE IF NOT EXISTS outbox (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id TEXT NOT NULL UNIQUE,
                op      TEXT NOT NULL        -- 'upsert' | 'delete'
            );
            -- Misc sync state (the changelog cursor lives here under key 'cursor').
            CREATE TABLE IF NOT EXISTS sync_state (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );",
        )?;
        // Migrate older DBs that predate these columns (ignore "duplicate column").
        let _ = conn.execute("ALTER TABLE nodes ADD COLUMN version_id TEXT", []);
        let _ = conn.execute("ALTER TABLE nodes ADD COLUMN mode INTEGER", []);
        // Selective-sync subscription state (per-device hydration policy; never synced).
        Self::init_subscriptions(&conn)?;
        // Ensure the root directory (ino 1) exists.
        conn.execute(
            "INSERT OR IGNORE INTO nodes(ino, parent_ino, name, kind, node_id) VALUES (?1, NULL, '', 'dir', 'root')",
            params![ROOT_INO as i64],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            store,
            staging: Mutex::new(HashMap::new()),
            chunk_cfg,
            locks: LockManager::new(),
        })
    }

    fn attr_row(conn: &Connection, ino: u64) -> Result<Attr> {
        conn.query_row(
            "SELECT ino, kind, size, mtime, mode FROM nodes WHERE ino = ?1",
            params![ino as i64],
            |r| {
                let kind = Kind::from_str(&r.get::<_, String>(1)?);
                Ok(Attr {
                    ino: r.get::<_, i64>(0)? as u64,
                    kind,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get::<_, i64>(3)?,
                    mode: r.get::<_, Option<i64>>(4)?.map(|m| m as u32).unwrap_or_else(|| default_mode(kind)),
                })
            },
        )
        .optional()?
        .ok_or(FsError::NotFound)
    }

    /// Assemble the full byte content of a committed file from its manifest chunks.
    fn assemble(&self, conn: &Connection, manifest_id: &str) -> Result<Vec<u8>> {
        let mut stmt = conn.prepare(
            "SELECT chunk_hash, length FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![manifest_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (hash, len) = row?;
            let bytes = self.store.get(&hash).map_err(|_| FsError::ChunkMissing(hash))?;
            // Trust the manifest length; chunk store returns the whole chunk.
            out.extend_from_slice(&bytes[..len.min(bytes.len())]);
        }
        Ok(out)
    }

    /// Assemble **only** the bytes of `[offset, offset + size)` from a committed manifest,
    /// touching only the chunks that range overlaps. This is what makes progressive hydration
    /// pay off: a header read of a multi-GB file assembles from the one or two chunks it spans,
    /// so it needs only *those* chunks cached (the rest can stay un-hydrated) — whereas
    /// [`assemble`](Self::assemble) reads every chunk and so requires the whole file local.
    ///
    /// A `ChunkMissing` for an overlapping chunk propagates (the read path hydrates that range
    /// and retries — same contract as before, just scoped to the range). Bytes past EOF are
    /// simply absent from the result (the caller clamps).
    fn assemble_range(&self, conn: &Connection, manifest_id: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let req_end = offset.saturating_add(size as u64);
        let mut stmt = conn.prepare(
            "SELECT chunk_hash, offset, length FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![manifest_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as usize,
            ))
        })?;
        let mut out = vec![0u8; (req_end - offset) as usize];
        let mut filled_end = offset; // highest contiguous byte position we've produced
        for row in rows {
            let (hash, c_off, c_len) = row?;
            let c_end = c_off.saturating_add(c_len as u64);
            // Skip chunks entirely outside the request.
            if c_end <= offset || c_off >= req_end {
                continue;
            }
            let bytes = self.store.get(&hash).map_err(|_| FsError::ChunkMissing(hash))?;
            let bytes = &bytes[..c_len.min(bytes.len())];
            // Overlap of [c_off, c_end) with [offset, req_end), copied into the output buffer.
            let copy_start = c_off.max(offset);
            let copy_end = c_end.min(req_end);
            let src_lo = (copy_start - c_off) as usize;
            let src_hi = (copy_end - c_off) as usize;
            let dst_lo = (copy_start - offset) as usize;
            // The chunk store may hold fewer bytes than the manifest claims only on corruption;
            // guard the slice so we never panic (the missing tail reads as zero / clamps).
            if src_hi <= bytes.len() {
                let span = src_hi - src_lo;
                out[dst_lo..dst_lo + span].copy_from_slice(&bytes[src_lo..src_hi]);
                filled_end = filled_end.max(copy_end);
            }
        }
        // Clamp to the bytes actually backed by chunks (don't return zero-padding past EOF).
        let valid = (filled_end.saturating_sub(offset)) as usize;
        out.truncate(valid);
        Ok(out)
    }

    /// Current content of `ino`: in-progress staging buffer if open for write, else
    /// the committed content assembled from chunks, else empty.
    fn current_content(&self, ino: u64) -> Result<Vec<u8>> {
        if let Some(buf) = self.staging.lock().unwrap().get(&ino) {
            return Ok(buf.clone());
        }
        let conn = self.conn.lock().unwrap();
        let manifest: Option<String> = conn
            .query_row(
                "SELECT manifest_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(FsError::NotFound)?;
        match manifest {
            Some(m) => self.assemble(&conn, &m),
            None => Ok(Vec::new()),
        }
    }

    fn insert_node(&self, parent: u64, name: &str, kind: Kind) -> Result<Attr> {
        let conn = self.conn.lock().unwrap();
        // parent must exist and be a directory
        let pkind: String = conn
            .query_row(
                "SELECT kind FROM nodes WHERE ino = ?1",
                params![parent as i64],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(FsError::NotFound)?;
        if Kind::from_str(&pkind) != Kind::Dir {
            return Err(FsError::NotADirectory);
        }
        let node_id = uuid::Uuid::new_v4().to_string();
        let mode = default_mode(kind);
        let mtime = now_nanos();
        let res = conn.execute(
            "INSERT INTO nodes(parent_ino, name, kind, node_id, mode, mtime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![parent as i64, name, kind.as_str(), node_id, mode as i64, mtime],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                return Err(FsError::AlreadyExists)
            }
            Err(e) => return Err(e.into()),
        }
        let ino = conn.last_insert_rowid() as u64;
        // Dirs are sync-ready on creation; files wait until `flush` has a manifest.
        if kind == Kind::Dir {
            Self::enqueue(&conn, &node_id, "upsert")?;
        }
        Ok(Attr {
            ino,
            kind,
            size: 0,
            mtime,
            mode,
        })
    }

    /// Record the server version a node now reflects — called after a successful push
    /// (the finalize response's version_id) so the next edit's commit `base` is current
    /// and the node doesn't conflict with its own just-pushed version.
    pub fn set_node_version(&self, node_id: &str, version_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET version_id = ?1 WHERE node_id = ?2",
            params![version_id, node_id],
        )?;
        Ok(())
    }

    /// The stable server id for a local inode (`None` if the inode is gone).
    pub fn node_id_for(&self, ino: u64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT node_id FROM nodes WHERE ino = ?1", params![ino as i64], |r| r.get(0))
            .optional()?)
    }

    /// Export the whole tree as sync-ready descriptors for pushing to the server.
    /// Excludes the root; a top-level node's `parent_node_id` is `None`.
    pub fn export_tree(&self) -> Result<Vec<SyncNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT n.node_id, n.name, n.kind, n.manifest_id,
                    CASE WHEN n.parent_ino = ?1 THEN NULL ELSE p.node_id END, n.version_id, n.mode, n.mtime
             FROM nodes n LEFT JOIN nodes p ON p.ino = n.parent_ino
             WHERE n.ino != ?1
             ORDER BY n.ino",
        )?;
        let rows = stmt.query_map(params![ROOT_INO as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (node_id, name, kind_s, manifest_id, parent_node_id, version_id, mode, mtime) = row?;
            let kind = Kind::from_str(&kind_s);
            let chunks = match &manifest_id {
                Some(m) => Self::manifest_rows(&conn, m)?,
                None => Vec::new(),
            };
            out.push(SyncNode {
                node_id,
                parent_node_id,
                name,
                kind,
                manifest_id,
                chunks,
                version_id,
                mode: mode.map(|m| m as u32).unwrap_or_else(|| default_mode(kind)),
                mtime,
            });
        }
        Ok(out)
    }

    fn manifest_rows(conn: &Connection, manifest_id: &str) -> Result<Vec<ChunkLoc>> {
        let mut stmt = conn.prepare(
            "SELECT chunk_hash, offset, length FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![manifest_id], |r| {
            Ok(ChunkLoc {
                hash: r.get::<_, String>(0)?,
                offset: r.get::<_, i64>(1)? as u64,
                length: r.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- sync apply (pull) ------------------------------------------------

    /// Apply a node pulled from the server into the local tree (upsert by node_id).
    /// Parents must already be applied (the server returns them parent-first).
    ///
    /// Writes the **source device's** `mtime` verbatim (not `now`) so a file's real
    /// modification time is identical on every device. Returns an [`ApplyOutcome`] telling
    /// the caller whether an *existing* node was repointed at new content — if so, the
    /// adapter must `invalidate(ino)` to drop the OS data cache (we no longer rely on an
    /// mtime bump to force the re-read).
    pub fn apply_remote(&self, node: &SyncNode) -> Result<ApplyOutcome> {
        let conn = self.conn.lock().unwrap();
        let parent_ino: i64 = match &node.parent_node_id {
            None => ROOT_INO as i64,
            Some(pid) => conn
                .query_row("SELECT ino FROM nodes WHERE node_id = ?1", params![pid], |r| r.get(0))
                .optional()?
                .ok_or(FsError::NotFound)?,
        };
        if let Some(mid) = &node.manifest_id {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM manifest_chunks WHERE manifest_id = ?1)",
                params![mid],
                |r| r.get(0),
            )?;
            if !exists {
                for (i, c) in node.chunks.iter().enumerate() {
                    conn.execute(
                        "INSERT INTO manifest_chunks(manifest_id, seq, chunk_hash, offset, length)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![mid, i as i64, c.hash, c.offset as i64, c.length as i64],
                    )?;
                }
            }
        }
        let size: i64 = node.chunks.iter().map(|c| c.length as i64).sum();
        // Look up the prior row (ino + manifest) so we can both reuse the inode on update
        // and decide whether the content actually changed (→ cache invalidation).
        let prior: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT ino, manifest_id FROM nodes WHERE node_id = ?1",
                params![node.node_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Displace any *different* node already occupying this (parent, name): the remote
        // snapshot/changelog is authoritative, so the incoming node wins. Without this, applying
        // a snapshot in which a name was reused (e.g. a delete+create, or two devices racing a
        // create/rename to the same path) hits the `UNIQUE(parent_ino, name)` index and the whole
        // pull fails. Recursively sweeping the displaced subtree mirrors the local POSIX
        // rename-replace path. (The displaced node's own changelog delete, if any, arrives
        // separately and is then a no-op.)
        //
        // BUT first detect whether any node we're about to sweep has a **pending outbox entry**:
        // that's un-synced local intent (a local rename/create into this name that hasn't been
        // pushed). Hard-deleting it silently loses that local node — and because the displaced
        // row vanishes, its queued outbox `upsert` later pushes as a no-op, so the changelog
        // delta can never re-add it. We still let the authoritative remote win the name, but we
        // signal `displaced_pending` so the pull caller triggers the reconcile backstop that
        // re-adds the displaced node (still live on the server under its prior name).
        let displaced_pending: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM outbox WHERE node_id IN (
                         WITH RECURSIVE sub(ino, node_id) AS (
                             SELECT ino, node_id FROM nodes
                               WHERE parent_ino = ?1 AND name = ?2 AND node_id != ?3
                             UNION ALL
                             SELECT n.ino, n.node_id FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                         )
                         SELECT node_id FROM sub
                     )
                 )",
                params![parent_ino, node.name, node.node_id],
                |r| r.get(0),
            )?;
        conn.execute(
            "DELETE FROM nodes WHERE ino IN (
                 WITH RECURSIVE sub(ino) AS (
                     SELECT ino FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND node_id != ?3
                     UNION ALL
                     SELECT n.ino FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                 )
                 SELECT ino FROM sub
             )",
            params![parent_ino, node.name, node.node_id],
        )?;
        let ino = if let Some((ino, _)) = &prior {
            conn.execute(
                "UPDATE nodes SET parent_ino=?1, name=?2, kind=?3, manifest_id=?4, size=?5, mtime=?6, version_id=?7, mode=?8 WHERE node_id=?9",
                params![parent_ino, node.name, node.kind.as_str(), node.manifest_id, size, node.mtime, node.version_id, (node.mode & 0o7777) as i64, node.node_id],
            )?;
            *ino as u64
        } else {
            conn.execute(
                "INSERT INTO nodes(parent_ino, name, kind, node_id, manifest_id, size, mtime, version_id, mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![parent_ino, node.name, node.kind.as_str(), node.node_id, node.manifest_id, size, node.mtime, node.version_id, (node.mode & 0o7777) as i64],
            )?;
            conn.last_insert_rowid() as u64
        };
        // Content changed iff the node already existed and its manifest moved (a same-size
        // edit included — that's exactly the case the old mtime-bump heuristic could miss).
        let content_changed = match &prior {
            Some((_, old_manifest)) => *old_manifest != node.manifest_id,
            None => false,
        };
        let inserted = prior.is_none();
        Ok(ApplyOutcome { ino, content_changed, inserted, displaced_pending })
    }

    /// Apply a delete pulled from the server: remove the node identified by its stable
    /// `node_id` (and any descendants) from the local tree. Idempotent — a no-op if the
    /// node is already gone (the server may delete a node this device never saw, or a
    /// parent delete may arrive before/after its children). Does **not** touch the
    /// outbox: this is an inbound change, not a local one, so it must never echo back.
    ///
    /// Orphan-safe: a directory delete sweeps the whole subtree via a recursive CTE, so
    /// even if per-child deletes don't arrive (or arrive out of order) the local tree
    /// stays consistent. Manifest/chunk rows are left for GC, matching the local
    /// `unlink`/`rmdir` path.
    pub fn apply_remote_delete(&self, node_id: &str) -> Result<()> {
        self.apply_remote_delete_signaled(node_id).map(|_| ())
    }

    /// [`apply_remote_delete`](Self::apply_remote_delete) that also reports whether the
    /// recursive sweep removed a **descendant** (a node other than `node_id` itself) that
    /// still had a **pending outbox entry** — i.e. un-synced local intent. That descendant may
    /// have been *moved out* of this subtree on the authoritative tree (so it's still live on
    /// the server under a new parent) while this device — applying a parent-delete before the
    /// descendant's re-parent upsert, or holding an un-pushed local move — sweeps it locally.
    /// The descendant's queued outbox upsert will then push as a no-op (its row is gone), so
    /// the changelog delta can never re-add it. The pull caller must reconcile from the full
    /// tree (the documented correctness floor) when this returns `true`. (The target `node_id`
    /// itself is excluded: deleting it IS authorized by this delete entry.) This is the
    /// recursive-delete face of BUG #2's convergence loss, dual to
    /// [`ApplyOutcome::displaced_pending`].
    pub fn apply_remote_delete_signaled(&self, node_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        // Detect, BEFORE deleting, whether any node in the to-be-swept subtree OTHER than the
        // explicit target has a pending outbox entry.
        let swept_pending: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM outbox WHERE node_id IN (
                     WITH RECURSIVE sub(ino, node_id) AS (
                         SELECT ino, node_id FROM nodes WHERE node_id = ?1
                         UNION ALL
                         SELECT n.ino, n.node_id FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                     )
                     SELECT node_id FROM sub WHERE node_id != ?1
                 )
             )",
            params![node_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM nodes WHERE ino IN (
                 WITH RECURSIVE sub(ino) AS (
                     SELECT ino FROM nodes WHERE node_id = ?1
                     UNION ALL
                     SELECT n.ino FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                 )
                 SELECT ino FROM sub
             )",
            params![node_id],
        )?;
        Ok(swept_pending)
    }

    pub fn ino_for_node_id(&self, node_id: &str) -> Result<Option<u64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT ino FROM nodes WHERE node_id = ?1", params![node_id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .map(|i| i as u64))
    }

    /// The chunk hashes whose byte range overlaps `[offset, offset + size)` — the SUBSET of a
    /// node's manifest needed to satisfy a read at that range. This is the core of **progressive
    /// / range hydration**: a read of just a file's header (or any slice) maps to only the few
    /// chunks covering it, so the hydrator fetches those instead of the whole (possibly
    /// multi-GB) file. Hashes are returned in manifest order and **deduplicated** (a chunk that
    /// dedups to the same hash appears once), so the caller fetches each distinct chunk at most
    /// once.
    ///
    /// A chunk spanning `[c.offset, c.offset + c.length)` overlaps the request iff
    /// `c.offset < offset + size && offset < c.offset + c.length`. A zero-length request (`size
    /// == 0`) overlaps nothing → empty. An empty manifest (no content, or a never-flushed node)
    /// → empty. A range past EOF → empty (no chunk overlaps). The whole-file case is just
    /// `manifest_range_hashes(id, 0, u64::MAX)` and returns every distinct chunk, matching
    /// [`node_manifest_hashes`](Self::node_manifest_hashes) (modulo dedup).
    pub fn manifest_range_hashes(&self, node_id: &str, offset: u64, size: u64) -> Result<Vec<String>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mid: Option<String> = conn
            .query_row("SELECT manifest_id FROM nodes WHERE node_id = ?1", params![node_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        let mid = match mid {
            None => return Ok(Vec::new()),
            Some(m) => m,
        };
        // The requested range's exclusive end, saturating so `u64::MAX` (whole-file) can't wrap.
        let req_end = offset.saturating_add(size);
        let mut stmt = conn.prepare(
            "SELECT chunk_hash, offset, length FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![mid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let (hash, c_off, c_len) = row?;
            let c_end = c_off.saturating_add(c_len);
            // Half-open overlap test: the chunk and the request share at least one byte.
            if c_off < req_end && offset < c_end && seen.insert(hash.clone()) {
                out.push(hash);
            }
        }
        Ok(out)
    }

    /// The ordered chunk hashes for a node's current manifest (for hydration).
    pub fn node_manifest_hashes(&self, node_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mid: Option<String> = conn
            .query_row("SELECT manifest_id FROM nodes WHERE node_id = ?1", params![node_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        match mid {
            None => Ok(Vec::new()),
            Some(m) => {
                let mut stmt =
                    conn.prepare("SELECT chunk_hash FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq")?;
                let rows = stmt.query_map(params![m], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<std::result::Result<_, _>>()?)
            }
        }
    }

    // ---- chunk cache access (for the sync daemon to move bytes) -----------

    pub fn chunk_get(&self, hash: &str) -> Result<Vec<u8>> {
        self.store.get(hash).map_err(FsError::Io)
    }
    pub fn chunk_put(&self, hash: &str, data: &[u8]) -> Result<()> {
        self.store.put(hash, data).map_err(FsError::Io)
    }
    pub fn has_chunk(&self, hash: &str) -> bool {
        self.store.has(hash)
    }

    // ---- sync outbox + cursor (the daemon's push queue & changelog position) ----

    /// Enqueue `node_id` as dirty. Called under the caller's held conn lock/tx so the
    /// outbox entry is atomic with the tree change that dirtied it. Re-enqueueing a
    /// node coalesces to its latest `op` (keeps the original insertion order).
    fn enqueue(conn: &Connection, node_id: &str, op: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO outbox(node_id, op) VALUES (?1, ?2)
             ON CONFLICT(node_id) DO UPDATE SET op = excluded.op",
            params![node_id, op],
        )?;
        Ok(())
    }

    /// The pending outbox entries `(node_id, op)` in insertion order (parents first).
    pub fn outbox_list(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT node_id, op FROM outbox ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The pending outbox entries `(node_id, op)` **topologically ordered — parents before
    /// children**, so an `upsert` is never drained before the parent it references.
    ///
    /// Insertion order alone is NOT a safe drain order: a `rename`/move re-`enqueue`s the
    /// moved node, but the outbox `ON CONFLICT(node_id) DO UPDATE` keeps the node's ORIGINAL
    /// insertion `id`. So moving a file into a directory created *after* it leaves the child
    /// ahead of its new parent in `outbox_list` order. Draining in that order commits the
    /// child first → the server changelog records the child before its parent → a pulling
    /// device's `apply_remote` hits `NotFound` for the missing parent and aborts the pull
    /// (the sync-stall BUG #1 found by the fault-injection harness). Ordering parents-first
    /// here closes that.
    ///
    /// Any node whose parent is *also* queued is emitted after that parent. Nodes whose
    /// parent is not queued (root-parented, already on the server, or vanished) are ready
    /// immediately. Deletes carry no parent dependency (the server delete is recursive, so
    /// relative order among deletes is harmless) and are treated as ready. A pathological
    /// cycle (which the tree's acyclic invariant forbids) degrades gracefully: any unplaced
    /// leftovers are appended in their original insertion order.
    pub fn outbox_list_topo(&self) -> Result<Vec<(String, String)>> {
        let entries = self.outbox_list()?;
        if entries.len() < 2 {
            return Ok(entries);
        }
        // Map each queued upsert node_id -> its current parent_node_id (None = root-parented
        // or vanished). Deletes don't participate in the dependency (recursive server-side).
        let mut parent_of: HashMap<String, Option<String>> = HashMap::new();
        for (nid, op) in &entries {
            if op != "delete" {
                if let Ok(Some(n)) = self.sync_node(nid) {
                    parent_of.insert(nid.clone(), n.parent_node_id.clone());
                }
            }
        }
        let queued: std::collections::HashSet<&String> = entries.iter().map(|(n, _)| n).collect();
        let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ordered: Vec<(String, String)> = Vec::with_capacity(entries.len());
        let mut progress = true;
        while ordered.len() < entries.len() && progress {
            progress = false;
            for (nid, op) in &entries {
                if placed.contains(nid) {
                    continue;
                }
                // Ready unless this node's parent is another still-unplaced queued node.
                let ready = match parent_of.get(nid) {
                    Some(Some(pid)) => !queued.contains(pid) || placed.contains(pid),
                    _ => true, // root-parented, a delete, or vanished
                };
                if ready {
                    ordered.push((nid.clone(), op.clone()));
                    placed.insert(nid.clone());
                    progress = true;
                }
            }
        }
        // Cycle fallback (shouldn't happen — the tree is acyclic): append any leftover.
        for (nid, op) in &entries {
            if !placed.contains(nid) {
                ordered.push((nid.clone(), op.clone()));
            }
        }
        Ok(ordered)
    }

    /// Remove one outbox entry once it has been pushed to the server.
    pub fn outbox_clear(&self, node_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM outbox WHERE node_id = ?1", params![node_id])?;
        Ok(())
    }

    /// The local changelog cursor (highest applied `seq`); 0 if never synced.
    pub fn cursor(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM sync_state WHERE key = 'cursor'", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// Bind this local DB to a `library_id` (a stable per-library/account id). If the DB was
    /// last used by a **different** library, its stale tree state — nodes, manifests, outbox,
    /// and the cursor — is wiped so a reused DB path can't mix two libraries (which produced
    /// duplicate/colliding placeholders + a stale cursor). The content-addressed chunk store
    /// is left intact (safe to share — keyed by hash). Returns `true` if it wiped a different
    /// library's state, `false` if the DB already belonged to this library (kept as-is).
    pub fn bind_library(&self, library_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let current: Option<i64> = conn
            .query_row("SELECT value FROM sync_state WHERE key = 'library'", [], |r| r.get(0))
            .optional()?;
        if current == Some(library_id) {
            return Ok(false); // same library — keep the local cache + cursor
        }
        let had_nodes: i64 =
            conn.query_row("SELECT COUNT(*) FROM nodes WHERE parent_ino IS NOT NULL", [], |r| r.get(0))?;
        conn.execute("DELETE FROM nodes WHERE parent_ino IS NOT NULL", [])?;
        conn.execute("DELETE FROM manifest_chunks", [])?;
        conn.execute("DELETE FROM outbox", [])?;
        conn.execute("DELETE FROM sync_state", [])?; // clears the old cursor too
        conn.execute("INSERT INTO sync_state(key, value) VALUES ('library', ?1)", params![library_id])?;
        Ok(had_nodes > 0)
    }

    pub fn set_cursor(&self, cursor: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES ('cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![cursor],
        )?;
        Ok(())
    }

    /// Whether this device is flagged for a full-tree reconcile (`pull_snapshot`) on the next
    /// sync tick. Set when a pull's `apply_remote` collision sweep clobbered a local node that
    /// still had un-synced local state (`ApplyOutcome::displaced_pending`): the changelog delta
    /// alone can never re-add that node, so the engine must reconcile. The flag is **sticky**
    /// (persisted in `sync_state`) because a single reconcile may not converge — the displaced
    /// node's authoritative state may not have reached the server yet (the racing peer's push
    /// is what surfaces it). The engine keeps reconciling each tick until the local tree
    /// matches the server, then clears the flag. Default `false`.
    pub fn needs_resync(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM sync_state WHERE key = 'needs_resync'", [], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0)
            != 0)
    }

    pub fn set_needs_resync(&self, needs: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES ('needs_resync', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![needs as i64],
        )?;
        Ok(())
    }

    /// A single node as a sync-ready descriptor (incremental push). `None` if the inode
    /// no longer exists (e.g. created then deleted before a push drained the outbox).
    pub fn sync_node(&self, node_id: &str) -> Result<Option<SyncNode>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT n.name, n.kind, n.manifest_id,
                        CASE WHEN n.parent_ino = ?2 THEN NULL ELSE p.node_id END, n.version_id, n.mode, n.mtime
                 FROM nodes n LEFT JOIN nodes p ON p.ino = n.parent_ino
                 WHERE n.node_id = ?1",
                params![node_id, ROOT_INO as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let (name, kind_s, manifest_id, parent_node_id, version_id, mode, mtime) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        let kind = Kind::from_str(&kind_s);
        let chunks = match &manifest_id {
            Some(m) => Self::manifest_rows(&conn, m)?,
            None => Vec::new(),
        };
        Ok(Some(SyncNode {
            node_id: node_id.to_string(),
            parent_node_id,
            name,
            kind,
            manifest_id,
            chunks,
            version_id,
            mode: mode.map(|m| m as u32).unwrap_or_else(|| default_mode(kind)),
            mtime,
        }))
    }

    // ---- CfAPI callback helpers (local-origin node-id operations) ---------------
    //
    // CfAPI callbacks receive a cloud root-relative path or a node identity, not the
    // (parent_ino, name) pair the Provider trait uses. These three helpers bridge the
    // gap and are usable cross-platform (no Windows-only code here).

    /// Local-origin recursive delete: remove the node identified by `node_id` (and its
    /// entire subtree) from the local tree, and enqueue a `"delete"` outbox entry so the
    /// sync loop pushes the removal to the server. Idempotent: if the node does not exist
    /// the call is a silent no-op and nothing is written to the outbox.
    ///
    /// Contrast with [`apply_remote_delete`]: that method is for *inbound* server deletes
    /// and deliberately skips the outbox (to avoid echoing the change back). This method is
    /// for *outbound* local deletes captured by a CfAPI callback.
    pub fn delete_by_node_id(&self, node_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Recursive CTE delete of the entire subtree (same as apply_remote_delete).
        // `changed` tells us whether anything was actually deleted — if zero the node
        // wasn't present so we skip the outbox write (idempotent: no-op on missing ids).
        let changed = tx.execute(
            "DELETE FROM nodes WHERE ino IN (
                 WITH RECURSIVE sub(ino) AS (
                     SELECT ino FROM nodes WHERE node_id = ?1
                     UNION ALL
                     SELECT n.ino FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                 )
                 SELECT ino FROM sub
             )",
            params![node_id],
        )?;
        if changed > 0 {
            // Enqueue a delete for the sync loop — the subtree children are implicitly
            // deleted on the server when the parent is deleted (the server also does a
            // recursive sweep), so a single outbox entry for the root of the deleted
            // subtree is sufficient.
            Self::enqueue(&tx, node_id, "delete")?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Resolve a library-relative POSIX path (e.g. `"a/b/c.txt"`) to `(parent_ino,
    /// leaf_name)`, creating any missing intermediate directories via `mkdir`. The **leaf
    /// is not created** — the caller does that (e.g. via `create`). A top-level path
    /// (`"foo.txt"`) returns `(ROOT_INO, "foo.txt")`. Empty input or a bare `"/"` returns
    /// `Err(NotFound)`.
    ///
    /// Returns `Err(NotADirectory)` if an intermediate path component exists as a file
    /// rather than a directory (e.g. `"foo.txt/bar.txt"` when `"foo.txt"` is a file).
    ///
    /// This function does **not** normalize `.` or `..` — the caller must pass a clean
    /// POSIX path.
    ///
    /// This is the CfAPI bridge for new-file capture: a `NOTIFY_FILE_CLOSE` callback
    /// delivers a root-relative OS path; `resolve_path` maps it to the tree location so
    /// the adapter can call `create`/`ingest_bytes` with the correct `(parent_ino, name)`.
    pub fn resolve_path(&self, rel: &str) -> Result<(u64, String)> {
        // Split on '/' and drop empty components (leading slash, trailing slash, "//").
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Err(FsError::NotFound);
        }
        // The leaf is the last component — we stop before creating it.
        let (dirs, leaf) = parts.split_at(parts.len() - 1);
        let leaf = leaf[0].to_string();
        // Walk (and if necessary create) each intermediate directory.
        let mut current_ino = ROOT_INO;
        for &component in dirs {
            current_ino = match self.lookup(current_ino, component) {
                Ok(attr) => {
                    if attr.kind != Kind::Dir {
                        return Err(FsError::NotADirectory);
                    }
                    attr.ino
                }
                Err(FsError::NotFound) => self.mkdir(current_ino, component)?.ino,
                Err(e) => return Err(e),
            };
        }
        Ok((current_ino, leaf))
    }

    /// Look up the node's current `(parent_ino, name)` by `node_id`, then delegate to
    /// the existing [`Provider::rename`] implementation (which handles POSIX replace,
    /// enqueues an `"upsert"`, etc.). Returns `Err(NotFound)` if the `node_id` is
    /// unknown.
    ///
    /// This is the CfAPI bridge for rename/move capture: a `NOTIFY_RENAME` callback
    /// delivers both the source cloud-file identity (CF_PLACEHOLDER_BASIC_INFO →
    /// `node_id`) and the new root-relative path (call `resolve_path` on it first).
    pub fn rename_by_node_id(&self, node_id: &str, new_parent: u64, new_name: &str) -> Result<()> {
        // Look up the current (parent_ino, name) for this node_id.
        let (old_parent, old_name) = {
            let conn = self.conn.lock().unwrap();
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT parent_ino, name FROM nodes WHERE node_id = ?1",
                    params![node_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                Some((p, n)) => (p as u64, n),
                None => return Err(FsError::NotFound),
            }
        };
        // Delegate to the existing rename implementation.
        self.rename(old_parent, &old_name, new_parent, new_name)
    }

    /// Resolve an inode to its `(parent_ino, name)`. Returns `Ok(None)` for the root (which
    /// has no parent) or an unknown inode. This is the inbound CfAPI bridge for **new remote
    /// nodes**: when a pull inserts a node, the adapter needs its parent directory (to locate
    /// the on-disk dir) and its leaf name (to build the placeholder) — both of which the
    /// `Attr`/`DirEntry` surface omits. Cross-platform (no Windows-only code).
    pub fn parent_and_name(&self, ino: u64) -> Result<Option<(u64, String)>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(Option<i64>, String)> = conn
            .query_row(
                "SELECT parent_ino, name FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            // A NULL parent_ino is the root row — it has no parent directory.
            Some((Some(p), name)) => Some((p as u64, name)),
            _ => None,
        })
    }

    /// Replace a node's entire content with `data`, then commit it (chunk + manifest +
    /// outbox), as if the file had been written and flushed. This is the **CfAPI
    /// local-edit capture** entry point: under Cloud Filter API the OS owns the on-disk
    /// bytes, so when a watcher detects the user edited a (real) file, the adapter reads
    /// those bytes off disk and re-ingests them here as the node's new version — the
    /// counterpart to a FUSE `write`+`flush`, which CfAPI never routes through us. Stamps a
    /// fresh real mtime and enqueues the outbox, so the normal push path uploads it.
    pub fn ingest_bytes(&self, ino: u64, data: &[u8]) -> Result<()> {
        // Validate the inode exists first — `flush` is a no-op on a missing stage entry, so
        // a bad ino would otherwise silently do nothing.
        self.getattr(ino)?;
        self.staging.lock().unwrap().insert(ino, data.to_vec());
        self.flush(ino)
    }
}

impl Provider for SqliteProvider {
    fn lookup(&self, parent: u64, name: &str) -> Result<Attr> {
        let conn = self.conn.lock().unwrap();
        let ino: Option<i64> = conn
            .query_row(
                "SELECT ino FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![parent as i64, name],
                |r| r.get(0),
            )
            .optional()?;
        match ino {
            Some(i) => Self::attr_row(&conn, i as u64),
            None => Err(FsError::NotFound),
        }
    }

    fn getattr(&self, ino: u64) -> Result<Attr> {
        // If open for write, report the live staged size.
        if let Some(buf) = self.staging.lock().unwrap().get(&ino) {
            let conn = self.conn.lock().unwrap();
            let mut a = Self::attr_row(&conn, ino)?;
            a.size = buf.len() as u64;
            return Ok(a);
        }
        let conn = self.conn.lock().unwrap();
        Self::attr_row(&conn, ino)
    }

    fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT ino, name, kind FROM nodes WHERE parent_ino = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![ino as i64], |r| {
            Ok(DirEntry {
                ino: r.get::<_, i64>(0)? as u64,
                name: r.get::<_, String>(1)?,
                kind: Kind::from_str(&r.get::<_, String>(2)?),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        // Open for write → serve from the in-memory staging buffer (the live, uncommitted
        // content). Slice the requested range out of it.
        if let Some(buf) = self.staging.lock().unwrap().get(&ino) {
            let start = (offset as usize).min(buf.len());
            let end = (start + size as usize).min(buf.len());
            return Ok(buf[start..end].to_vec());
        }
        // Committed file → assemble **only the chunks the range overlaps**, not the whole file.
        // This is what lets progressive hydration serve a header read from just the first
        // chunk(s): a `ChunkMissing` here is scoped to the requested range, so the mount's
        // read-path hydrates *that range* and retries (instead of needing the whole file).
        let conn = self.conn.lock().unwrap();
        let manifest: Option<String> = conn
            .query_row("SELECT manifest_id FROM nodes WHERE ino = ?1", params![ino as i64], |r| r.get(0))
            .optional()?
            .ok_or(FsError::NotFound)?;
        match manifest {
            Some(m) => self.assemble_range(&conn, &m, offset, size),
            None => Ok(Vec::new()),
        }
    }

    fn create(&self, parent: u64, name: &str) -> Result<Attr> {
        let attr = self.insert_node(parent, name, Kind::File)?;
        // Open it for write immediately (FUSE create → open).
        self.staging.lock().unwrap().insert(attr.ino, Vec::new());
        Ok(attr)
    }

    fn mkdir(&self, parent: u64, name: &str) -> Result<Attr> {
        self.insert_node(parent, name, Kind::Dir)
    }

    fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let mut staging = self.staging.lock().unwrap();
        if !staging.contains_key(&ino) {
            drop(staging);
            // Seed the staging buffer with current committed content so partial
            // writes to an existing file are correct.
            let seed = self.current_content(ino)?;
            staging = self.staging.lock().unwrap();
            staging.entry(ino).or_insert(seed);
        }
        let buf = staging.get_mut(&ino).unwrap();
        let off = offset as usize;
        if off + data.len() > buf.len() {
            buf.resize(off + data.len(), 0);
        }
        buf[off..off + data.len()].copy_from_slice(data);
        Ok(data.len() as u32)
    }

    fn flush(&self, ino: u64) -> Result<()> {
        let buf = match self.staging.lock().unwrap().remove(&ino) {
            Some(b) => b,
            None => return Ok(()), // nothing staged
        };

        // Chunk with FastCDC, store + dedup, build a content-addressed manifest.
        let mut chunk_rows: Vec<(usize, String, usize, usize)> = Vec::new(); // seq, hash, offset, len
        let chunker =
            fastcdc::v2020::FastCDC::new(&buf, self.chunk_cfg.min, self.chunk_cfg.avg, self.chunk_cfg.max);
        for (seq, c) in chunker.enumerate() {
            let bytes = &buf[c.offset..c.offset + c.length];
            let hash = hex::encode(Sha256::digest(bytes));
            self.store.put(&hash, bytes)?;
            chunk_rows.push((seq, hash, c.offset, c.length));
        }
        // Manifest id = hash of the ordered chunk-hash list (identical files dedup).
        let mut mh = Sha256::new();
        for (_, h, _, _) in &chunk_rows {
            mh.update(h.as_bytes());
        }
        let manifest_id = hex::encode(mh.finalize());

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM manifest_chunks WHERE manifest_id = ?1",
            params![manifest_id],
        )?;
        for (seq, hash, offset, len) in &chunk_rows {
            tx.execute(
                "INSERT INTO manifest_chunks(manifest_id, seq, chunk_hash, offset, length)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![manifest_id, *seq as i64, hash, *offset as i64, *len as i64],
            )?;
        }
        tx.execute(
            "UPDATE nodes SET size = ?1, manifest_id = ?2, mtime = ?4 WHERE ino = ?3",
            params![buf.len() as i64, manifest_id, ino as i64, now_nanos()],
        )?;
        // Mark the file dirty for sync, atomically with its new manifest.
        if let Some(node_id) =
            tx.query_row("SELECT node_id FROM nodes WHERE ino = ?1", params![ino as i64], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten()
        {
            Self::enqueue(&tx, &node_id, "upsert")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn truncate(&self, ino: u64, size: u64) -> Result<()> {
        // If the file is already open for write, resize the live staging buffer in
        // place; the eventual `flush`/`release` commits the new length. Shrinking here
        // is what clears the stale tail an overwrite-with-shorter would otherwise leave
        // (the kernel issues this `setattr(size=0)` on an `O_TRUNC` open, before the
        // shorter `write`).
        {
            let mut staging = self.staging.lock().unwrap();
            if let Some(buf) = staging.get_mut(&ino) {
                buf.resize(size as usize, 0);
                return Ok(());
            }
        }
        // Truncating to empty discards everything, so it must NOT read the old content
        // first — that would assemble the committed chunks and fail on a pulled-but-
        // unhydrated placeholder (the `O_TRUNC` overwrite of a not-yet-downloaded file,
        // a routine mount operation). Short-circuit to an empty buffer.
        if size == 0 {
            self.staging.lock().unwrap().insert(ino, Vec::new());
            return self.flush(ino);
        }
        // Otherwise (a bare `truncate(n)` to a non-zero size): seed from the committed
        // content, resize, and commit immediately so the new length — and its outbox
        // entry — is durable even when no write follows. Growing/partial-shrink needs
        // the prefix bytes [0, n), so this path does require the file to be hydrated.
        let mut content = self.current_content(ino)?;
        content.resize(size as usize, 0);
        self.staging.lock().unwrap().insert(ino, content);
        self.flush(ino)
    }

    fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row(
                "SELECT node_id FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND kind = 'file'",
                params![parent as i64, name],
                |r| r.get(0),
            )
            .optional()?;
        let n = conn.execute(
            "DELETE FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND kind = 'file'",
            params![parent as i64, name],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        if let Some(id) = node_id {
            Self::enqueue(&conn, &id, "delete")?;
        }
        Ok(())
    }

    fn rmdir(&self, parent: u64, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let ino: i64 = conn
            .query_row(
                "SELECT ino FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND kind = 'dir'",
                params![parent as i64, name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(FsError::NotFound)?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM nodes WHERE parent_ino = ?1", params![ino], |r| {
                r.get(0)
            })?;
        if count > 0 {
            return Err(FsError::NotEmpty);
        }
        let node_id: Option<String> = conn
            .query_row("SELECT node_id FROM nodes WHERE ino = ?1", params![ino], |r| r.get(0))
            .optional()?;
        conn.execute("DELETE FROM nodes WHERE ino = ?1", params![ino])?;
        if let Some(id) = node_id {
            Self::enqueue(&conn, &id, "delete")?;
        }
        Ok(())
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Source must exist.
        let src: Option<(i64, String)> = tx
            .query_row(
                "SELECT ino, node_id FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![parent as i64, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (src_ino, src_id) = src.ok_or(FsError::NotFound)?;

        // POSIX rename atomically *replaces* an occupied destination (the atomic-save
        // pattern: write `f.tmp`, rename `f.tmp` → `f`). Without this the unique
        // (parent, name) index turns every such save into a constraint error.
        let dst: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT ino, node_id, kind FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![newparent as i64, newname],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((dst_ino, dst_id, dst_kind)) = dst {
            if dst_ino == src_ino {
                return Ok(()); // rename onto itself — no-op
            }
            // A directory destination must be empty (ENOTEMPTY otherwise); a file is
            // simply displaced. The displaced node is tombstoned so the deletion
            // propagates on push (otherwise other devices keep a duplicate).
            if Kind::from_str(&dst_kind) == Kind::Dir {
                let kids: i64 = tx
                    .query_row("SELECT COUNT(*) FROM nodes WHERE parent_ino = ?1", params![dst_ino], |r| r.get(0))?;
                if kids > 0 {
                    return Err(FsError::NotEmpty);
                }
            }
            tx.execute("DELETE FROM nodes WHERE ino = ?1", params![dst_ino])?;
            Self::enqueue(&tx, &dst_id, "delete")?;
        }

        tx.execute(
            "UPDATE nodes SET parent_ino = ?1, name = ?2 WHERE ino = ?3",
            params![newparent as i64, newname, src_ino],
        )?;
        Self::enqueue(&tx, &src_id, "upsert")?;
        tx.commit()?;
        Ok(())
    }

    fn set_mode(&self, ino: u64, mode: u32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row("SELECT node_id FROM nodes WHERE ino = ?1", params![ino as i64], |r| r.get(0))
            .optional()?;
        let n = conn.execute(
            "UPDATE nodes SET mode = ?1 WHERE ino = ?2",
            params![(mode & 0o7777) as i64, ino as i64],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        // Propagate the metadata change (re-commit/re-mkdir carries the new mode).
        if let Some(id) = node_id {
            Self::enqueue(&conn, &id, "upsert")?;
        }
        Ok(())
    }

    fn set_mtime(&self, ino: u64, mtime: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row("SELECT node_id FROM nodes WHERE ino = ?1", params![ino as i64], |r| r.get(0))
            .optional()?;
        let n = conn.execute(
            "UPDATE nodes SET mtime = ?1 WHERE ino = ?2",
            params![mtime, ino as i64],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        // Propagate the metadata change so the real mtime (e.g. from `cp -p`) reaches the
        // other devices — the re-commit carries the new mtime alongside the manifest.
        if let Some(id) = node_id {
            Self::enqueue(&conn, &id, "upsert")?;
        }
        Ok(())
    }

    fn set_lock(&self, ino: u64, lock: LockRange) -> Result<()> {
        self.locks.set(ino, lock).map_err(|_| FsError::WouldBlock)
    }
    fn test_lock(&self, ino: u64, probe: LockRange) -> Result<Option<LockRange>> {
        Ok(self.locks.test(ino, probe))
    }
    fn unlock(&self, ino: u64, owner: u64, start: u64, end: u64) -> Result<()> {
        self.locks.unlock(ino, owner, start, end);
        Ok(())
    }
    fn release_locks(&self, ino: u64, owner: u64) -> Result<()> {
        self.locks.release_owner(ino, owner);
        Ok(())
    }
}

/// Shared ownership of a provider. The FUSE mount (`fuser::mount2` takes the adapter by
/// value and blocks) and the background sync loop each hold an `Arc` clone of the same
/// provider; every method is `&self`, so delegation is a straight deref.
impl<P: Provider + ?Sized> Provider for std::sync::Arc<P> {
    fn lookup(&self, parent: u64, name: &str) -> Result<Attr> {
        (**self).lookup(parent, name)
    }
    fn getattr(&self, ino: u64) -> Result<Attr> {
        (**self).getattr(ino)
    }
    fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        (**self).readdir(ino)
    }
    fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        (**self).read(ino, offset, size)
    }
    fn create(&self, parent: u64, name: &str) -> Result<Attr> {
        (**self).create(parent, name)
    }
    fn mkdir(&self, parent: u64, name: &str) -> Result<Attr> {
        (**self).mkdir(parent, name)
    }
    fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32> {
        (**self).write(ino, offset, data)
    }
    fn flush(&self, ino: u64) -> Result<()> {
        (**self).flush(ino)
    }
    fn truncate(&self, ino: u64, size: u64) -> Result<()> {
        (**self).truncate(ino, size)
    }
    fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        (**self).unlink(parent, name)
    }
    fn rmdir(&self, parent: u64, name: &str) -> Result<()> {
        (**self).rmdir(parent, name)
    }
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        (**self).rename(parent, name, newparent, newname)
    }
    fn set_mode(&self, ino: u64, mode: u32) -> Result<()> {
        (**self).set_mode(ino, mode)
    }
    fn set_mtime(&self, ino: u64, mtime: i64) -> Result<()> {
        (**self).set_mtime(ino, mtime)
    }
    fn set_lock(&self, ino: u64, lock: LockRange) -> Result<()> {
        (**self).set_lock(ino, lock)
    }
    fn test_lock(&self, ino: u64, probe: LockRange) -> Result<Option<LockRange>> {
        (**self).test_lock(ino, probe)
    }
    fn unlock(&self, ino: u64, owner: u64, start: u64, end: u64) -> Result<()> {
        (**self).unlock(ino, owner, start, end)
    }
    fn release_locks(&self, ino: u64, owner: u64) -> Result<()> {
        (**self).release_locks(ino, owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> SqliteProvider {
        // Tiny chunks so small test files split into many chunks.
        let store = Box::new(LocalChunkStore::temp().unwrap());
        SqliteProvider::in_memory(store, ChunkConfig { min: 64, avg: 256, max: 1024 }).unwrap()
    }

    #[test]
    fn mkdir_lookup_readdir() {
        let p = provider();
        let d = p.mkdir(ROOT_INO, "proj").unwrap();
        assert_eq!(d.kind, Kind::Dir);
        assert_eq!(p.lookup(ROOT_INO, "proj").unwrap().ino, d.ino);
        let entries = p.readdir(ROOT_INO).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "proj");
    }

    #[test]
    fn write_chunks_and_read_back() {
        let p = provider();
        let f = p.create(ROOT_INO, "part.bin").unwrap();
        // ~4 KB → multiple chunks at the tiny test sizes.
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(p.write(f.ino, 0, &data).unwrap(), data.len() as u32);
        p.flush(f.ino).unwrap();

        // size committed
        assert_eq!(p.getattr(f.ino).unwrap().size, 4096);
        // full read reassembles exactly
        let full = p.read(f.ino, 0, 4096).unwrap();
        assert_eq!(full, data);
        // a middle range matches
        let mid = p.read(f.ino, 1000, 500).unwrap();
        assert_eq!(mid, &data[1000..1500]);
        // read past EOF clamps
        assert_eq!(p.read(f.ino, 4000, 500).unwrap(), &data[4000..4096]);
    }

    #[test]
    fn identical_content_dedups_manifest() {
        let p = provider();
        let a = p.create(ROOT_INO, "a.bin").unwrap();
        let b = p.create(ROOT_INO, "b.bin").unwrap();
        let data = vec![7u8; 3000];
        p.write(a.ino, 0, &data).unwrap();
        p.flush(a.ino).unwrap();
        p.write(b.ino, 0, &data).unwrap();
        p.flush(b.ino).unwrap();

        let conn = p.conn.lock().unwrap();
        let ma: String = conn
            .query_row("SELECT manifest_id FROM nodes WHERE ino=?1", params![a.ino as i64], |r| r.get(0))
            .unwrap();
        let mb: String = conn
            .query_row("SELECT manifest_id FROM nodes WHERE ino=?1", params![b.ino as i64], |r| r.get(0))
            .unwrap();
        assert_eq!(ma, mb, "identical content → identical (deduped) manifest");
    }

    #[test]
    fn partial_overwrite_of_existing_file() {
        let p = provider();
        let f = p.create(ROOT_INO, "x.bin").unwrap();
        p.write(f.ino, 0, &vec![1u8; 2000]).unwrap();
        p.flush(f.ino).unwrap();
        // overwrite bytes 500..510 of the committed file
        p.write(f.ino, 500, &[9u8; 10]).unwrap();
        p.flush(f.ino).unwrap();
        let out = p.read(f.ino, 0, 2000).unwrap();
        assert_eq!(out.len(), 2000);
        assert_eq!(&out[500..510], &[9u8; 10]);
        assert_eq!(out[0], 1);
        assert_eq!(out[1999], 1);
    }

    #[test]
    fn overwrite_with_shorter_leaves_no_stale_tail() {
        // The §12.4 bug: overwrite a file with fewer bytes than before. The kernel does
        // this as `setattr(size=0)` (truncate on `O_TRUNC` open) then a short `write`.
        // Without a truncate path the committed manifest kept the old trailing chunks,
        // so reads returned a stale tail / wrong size.
        let p = provider();
        let f = p.create(ROOT_INO, "x.bin").unwrap();
        p.write(f.ino, 0, &vec![1u8; 2000]).unwrap();
        p.flush(f.ino).unwrap();
        assert_eq!(p.getattr(f.ino).unwrap().size, 2000);

        // Overwrite-with-shorter, exactly as the kernel sequences it.
        p.truncate(f.ino, 0).unwrap();
        p.write(f.ino, 0, &vec![9u8; 100]).unwrap();
        p.flush(f.ino).unwrap();

        assert_eq!(p.getattr(f.ino).unwrap().size, 100, "size reflects only the new content");
        // A generous read returns *exactly* the new 100 bytes — no stale tail past EOF.
        let out = p.read(f.ino, 0, 4096).unwrap();
        assert_eq!(out.len(), 100, "no chunks past the new EOF");
        assert_eq!(out, vec![9u8; 100]);
    }

    #[test]
    fn truncate_grows_with_zero_fill_and_shrinks_committed_file() {
        let p = provider();
        let f = p.create(ROOT_INO, "x.bin").unwrap();
        p.write(f.ino, 0, &vec![7u8; 500]).unwrap();
        p.flush(f.ino).unwrap();

        // Shrink a *committed* (not-open-for-write) file: commits immediately.
        p.truncate(f.ino, 200).unwrap();
        assert_eq!(p.getattr(f.ino).unwrap().size, 200);
        assert_eq!(p.read(f.ino, 0, 4096).unwrap(), vec![7u8; 200]);

        // Grow back: the gap zero-fills.
        p.truncate(f.ino, 350).unwrap();
        assert_eq!(p.getattr(f.ino).unwrap().size, 350);
        let out = p.read(f.ino, 0, 4096).unwrap();
        assert_eq!(out.len(), 350);
        assert_eq!(&out[..200], &vec![7u8; 200][..]);
        assert_eq!(&out[200..], &vec![0u8; 150][..], "grown region reads back as zeros");
    }

    #[test]
    fn export_tree_yields_sync_nodes() {
        let p = provider();
        let dir = p.mkdir(ROOT_INO, "proj").unwrap();
        let f = p.create(dir.ino, "part.bin").unwrap();
        p.write(f.ino, 0, &vec![5u8; 1500]).unwrap();
        p.flush(f.ino).unwrap();

        let nodes = p.export_tree().unwrap();
        assert_eq!(nodes.len(), 2);

        let dir_n = nodes.iter().find(|n| n.name == "proj").unwrap();
        assert!(dir_n.parent_node_id.is_none(), "top-level dir has no parent");
        assert_eq!(dir_n.kind, Kind::Dir);

        let file_n = nodes.iter().find(|n| n.name == "part.bin").unwrap();
        assert_eq!(file_n.parent_node_id.as_ref(), Some(&dir_n.node_id),
                   "file's parent_node_id is the dir's node_id");
        assert!(file_n.manifest_id.is_some());
        assert!(!file_n.chunks.is_empty(), "file carries its manifest chunks");
    }

    #[test]
    fn apply_remote_reconstructs_tree_and_content() {
        // A and B share the temp chunk store (same process) — so once A flushes,
        // B can read content after applying A's exported tree. This mirrors the
        // real daemon where the shared store is the cloud (R2/SeaweedFS).
        let a = provider();
        let dir = a.mkdir(ROOT_INO, "proj").unwrap();
        let f = a.create(dir.ino, "x.bin").unwrap();
        let data = vec![9u8; 800];
        a.write(f.ino, 0, &data).unwrap();
        a.flush(f.ino).unwrap();
        let exported = a.export_tree().unwrap();

        let b = provider();
        for n in &exported {
            b.apply_remote(n).unwrap();
        }

        let proj = b.lookup(ROOT_INO, "proj").unwrap();
        assert_eq!(proj.kind, Kind::Dir);
        let xb = b.lookup(proj.ino, "x.bin").unwrap();
        assert_eq!(xb.size, 800);

        let file_node = exported.iter().find(|n| n.name == "x.bin").unwrap();
        let hashes = b.node_manifest_hashes(&file_node.node_id).unwrap();
        assert_eq!(hashes.len(), file_node.chunks.len());
        assert!(b.ino_for_node_id(&file_node.node_id).unwrap().is_some());

        // content round-trips (chunks present in the shared store)
        assert_eq!(b.read(xb.ino, 0, 800).unwrap(), data);
    }

    #[test]
    fn node_ids_are_unique_and_stable() {
        let p = provider();
        let a = p.mkdir(ROOT_INO, "a").unwrap();
        let b = p.mkdir(ROOT_INO, "b").unwrap();
        let ida = p.node_id_for(a.ino).unwrap().unwrap();
        let idb = p.node_id_for(b.ino).unwrap().unwrap();
        assert_ne!(ida, idb);
        assert_eq!(p.node_id_for(a.ino).unwrap().unwrap(), ida, "stable across calls");
    }

    #[test]
    fn outbox_tracks_local_changes() {
        let p = provider();
        // mkdir enqueues an upsert immediately.
        let dir = p.mkdir(ROOT_INO, "proj").unwrap();
        let dir_id = p.node_id_for(dir.ino).unwrap().unwrap();
        // create alone does NOT enqueue (no manifest yet); flush does.
        let f = p.create(dir.ino, "part.bin").unwrap();
        let file_id = p.node_id_for(f.ino).unwrap().unwrap();
        assert_eq!(p.outbox_list().unwrap(), vec![(dir_id.clone(), "upsert".into())]);
        p.write(f.ino, 0, &vec![1u8; 1000]).unwrap();
        p.flush(f.ino).unwrap();
        // Parent dir enqueued before the file (insertion order preserved).
        assert_eq!(
            p.outbox_list().unwrap(),
            vec![(dir_id.clone(), "upsert".into()), (file_id.clone(), "upsert".into())]
        );
        // A delete coalesces the file's entry to 'delete' (no duplicate row).
        p.unlink(dir.ino, "part.bin").unwrap();
        assert_eq!(
            p.outbox_list().unwrap(),
            vec![(dir_id.clone(), "upsert".into()), (file_id.clone(), "delete".into())]
        );
        // Draining clears entries.
        p.outbox_clear(&dir_id).unwrap();
        p.outbox_clear(&file_id).unwrap();
        assert!(p.outbox_list().unwrap().is_empty());
    }

    #[test]
    fn cursor_round_trips() {
        let p = provider();
        assert_eq!(p.cursor().unwrap(), 0, "defaults to 0");
        p.set_cursor(42).unwrap();
        assert_eq!(p.cursor().unwrap(), 42);
        p.set_cursor(99).unwrap();
        assert_eq!(p.cursor().unwrap(), 99, "upserts in place");
    }

    #[test]
    fn sync_node_matches_export_tree() {
        let p = provider();
        let dir = p.mkdir(ROOT_INO, "proj").unwrap();
        let f = p.create(dir.ino, "x.bin").unwrap();
        p.write(f.ino, 0, &vec![5u8; 1500]).unwrap();
        p.flush(f.ino).unwrap();
        let file_id = p.node_id_for(f.ino).unwrap().unwrap();
        let from_export = p.export_tree().unwrap().into_iter().find(|n| n.node_id == file_id).unwrap();
        let from_single = p.sync_node(&file_id).unwrap().unwrap();
        assert_eq!(from_single, from_export, "single-node export equals whole-tree export");
        assert!(p.sync_node("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn unlink_and_rename() {
        let p = provider();
        p.create(ROOT_INO, "tmp.bin").unwrap();
        p.flush(p.lookup(ROOT_INO, "tmp.bin").unwrap().ino).unwrap();
        p.rename(ROOT_INO, "tmp.bin", ROOT_INO, "final.bin").unwrap();
        assert!(p.lookup(ROOT_INO, "tmp.bin").is_err());
        assert!(p.lookup(ROOT_INO, "final.bin").is_ok());
        p.unlink(ROOT_INO, "final.bin").unwrap();
        assert!(p.lookup(ROOT_INO, "final.bin").is_err());
    }

    #[test]
    fn apply_remote_delete_removes_node_and_subtree() {
        // Build proj/{x.bin, sub/y.bin} on A, replicate to B.
        let a = provider();
        let proj = a.mkdir(ROOT_INO, "proj").unwrap();
        let x = a.create(proj.ino, "x.bin").unwrap();
        a.write(x.ino, 0, &vec![1u8; 500]).unwrap();
        a.flush(x.ino).unwrap();
        let sub = a.mkdir(proj.ino, "sub").unwrap();
        let y = a.create(sub.ino, "y.bin").unwrap();
        a.write(y.ino, 0, &vec![2u8; 500]).unwrap();
        a.flush(y.ino).unwrap();

        let b = provider();
        for n in &a.export_tree().unwrap() {
            b.apply_remote(n).unwrap();
        }
        let x_id = a.node_id_for(x.ino).unwrap().unwrap();
        let proj_id = a.node_id_for(proj.ino).unwrap().unwrap();
        let sub_id = a.node_id_for(sub.ino).unwrap().unwrap();
        let y_id = a.node_id_for(y.ino).unwrap().unwrap();

        // Delete a single file: gone on B, siblings untouched.
        b.apply_remote_delete(&x_id).unwrap();
        assert!(b.ino_for_node_id(&x_id).unwrap().is_none());
        assert!(b.ino_for_node_id(&y_id).unwrap().is_some(), "sibling subtree intact");

        // Idempotent: deleting again (and deleting an unknown id) is a no-op.
        b.apply_remote_delete(&x_id).unwrap();
        b.apply_remote_delete("never-existed").unwrap();

        // Delete the parent dir: the whole subtree (sub/ and y.bin) goes with it,
        // even though no per-child delete was applied — no orphaned rows.
        b.apply_remote_delete(&proj_id).unwrap();
        assert!(b.ino_for_node_id(&proj_id).unwrap().is_none());
        assert!(b.ino_for_node_id(&sub_id).unwrap().is_none(), "nested dir swept");
        assert!(b.ino_for_node_id(&y_id).unwrap().is_none(), "nested file swept");
        assert!(b.export_tree().unwrap().is_empty(), "tree fully emptied");
    }

    /// A pull whose collision sweep would clobber a locally-renamed-but-not-yet-pushed node
    /// must SIGNAL it (`displaced_pending`) so the caller can reconcile — otherwise the local
    /// node is silently + permanently lost (it's still live on the server under its prior
    /// name, but the changelog delta can never re-add it: its stale outbox upsert pushes as a
    /// no-op once the row is gone). This is BUG #2's convergence face, isolated to the unit.
    #[test]
    fn apply_remote_collision_over_pending_local_node_signals_displaced() {
        let p = provider();
        let dir = p.mkdir(ROOT_INO, "d").unwrap();
        let dir_id = p.node_id_for(dir.ino).unwrap().unwrap();

        // Local node F created + renamed to "g" — a pending, un-pushed local rename.
        let f = p.create(dir.ino, "f").unwrap();
        p.write(f.ino, 0, b"local").unwrap();
        p.flush(f.ino).unwrap();
        let f_id = p.node_id_for(f.ino).unwrap().unwrap();
        p.rename(dir.ino, "f", dir.ino, "g").unwrap();
        assert!(
            p.outbox_list().unwrap().iter().any(|(id, op)| id == &f_id && op == "upsert"),
            "precondition: F's local rename is queued (un-pushed)"
        );

        // A remote pull brings in a DIFFERENT node R that claims the same path (dir, "g") —
        // a concurrent create/rename on another device that the server made authoritative.
        let remote = SyncNode {
            node_id: "remote-R".into(),
            parent_node_id: Some(dir_id.clone()),
            name: "g".into(),
            kind: Kind::File,
            manifest_id: None,
            chunks: vec![],
            version_id: Some("vR".into()),
            mode: 0o644,
            mtime: 0,
        };
        let outcome = p.apply_remote(&remote).unwrap();

        // The remote won the name (authoritative) — and we were told the displaced local node
        // had pending state, so the caller knows to reconcile.
        assert!(outcome.displaced_pending, "must flag that a pending local node was clobbered");
        assert!(p.ino_for_node_id("remote-R").unwrap().is_some(), "remote node present at the path");
        assert!(p.ino_for_node_id(&f_id).unwrap().is_none(), "displaced local node is gone from the tree");

        // A plain collision over a node with NO pending outbox state must NOT raise the flag
        // (the ordinary authoritative name-reuse case — no local intent was lost).
        let other = p.mkdir(ROOT_INO, "e").unwrap();
        let other_id = p.node_id_for(other.ino).unwrap().unwrap();
        let pulled = SyncNode {
            node_id: "remote-S".into(),
            parent_node_id: Some(other_id.clone()),
            name: "h".into(),
            kind: Kind::File,
            manifest_id: None,
            chunks: vec![],
            version_id: Some("v1".into()),
            mode: 0o644,
            mtime: 0,
        };
        p.apply_remote(&pulled).unwrap();
        p.outbox_clear("remote-S").unwrap(); // simulate it being a clean (non-local) row
        // A re-pull at the same path with a different id, no pending outbox entry on remote-S:
        let pulled2 = SyncNode { node_id: "remote-T".into(), ..pulled.clone() };
        let o2 = p.apply_remote(&pulled2).unwrap();
        assert!(!o2.displaced_pending, "no pending local state → no displaced_pending signal");
    }

    /// Convergence harness: drive a deterministic pseudo-random stream of create /
    /// overwrite / delete / move ops on a source provider, replicate each step's outbox
    /// to a replica via `apply_remote`/`apply_remote_delete` (exactly what the daemon's
    /// pull does), and assert the two trees stay byte-identical after **every** op. This
    /// is the mechanical backstop that would have caught the dropped-delete and
    /// duplicated-move bugs.
    #[test]
    fn convergence_under_random_ops() {
        struct E {
            ino: u64,
            parent: u64,
            name: String,
            dir: bool,
        }

        fn snapshot(p: &SqliteProvider) -> Vec<(String, Option<String>, String, String, Option<String>)> {
            let mut v: Vec<_> = p
                .export_tree()
                .unwrap()
                .into_iter()
                .map(|n| (n.node_id, n.parent_node_id, n.name, n.kind.as_str().to_string(), n.manifest_id))
                .collect();
            v.sort();
            v
        }

        fn mirror(src: &SqliteProvider, dst: &SqliteProvider) {
            for (node_id, op) in src.outbox_list().unwrap() {
                if op == "delete" {
                    dst.apply_remote_delete(&node_id).unwrap();
                } else if let Some(n) = src.sync_node(&node_id).unwrap() {
                    dst.apply_remote(&n).unwrap();
                }
                src.outbox_clear(&node_id).unwrap();
            }
        }

        let src = provider();
        let dst = provider();
        let mut entries: Vec<E> = Vec::new();
        let mut ctr = 0u64;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };

        for iter in 0..250 {
            // Candidate parent dirs = root + every live directory.
            let dirs: Vec<u64> = std::iter::once(ROOT_INO)
                .chain(entries.iter().filter(|e| e.dir).map(|e| e.ino))
                .collect();
            let files: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| !e.dir).map(|(i, _)| i).collect();

            match next() % 6 {
                0 => {
                    // mkdir
                    let parent = dirs[next() % dirs.len()];
                    ctr += 1;
                    let name = format!("d{ctr}");
                    let a = src.mkdir(parent, &name).unwrap();
                    entries.push(E { ino: a.ino, parent, name, dir: true });
                }
                1 => {
                    // create + write + flush (a committed file)
                    let parent = dirs[next() % dirs.len()];
                    ctr += 1;
                    let name = format!("f{ctr}");
                    let a = src.create(parent, &name).unwrap();
                    let len = 64 + next() % 600;
                    let data: Vec<u8> = (0..len).map(|i| (i ^ ctr as usize) as u8).collect();
                    src.write(a.ino, 0, &data).unwrap();
                    src.flush(a.ino).unwrap();
                    entries.push(E { ino: a.ino, parent, name, dir: false });
                }
                2 => {
                    // overwrite an existing file with a *new, often shorter* body, the
                    // way the kernel does it on `O_TRUNC`: truncate to 0, then write. The
                    // small `len` ceiling makes shrink the common case, exercising the
                    // stale-tail path. We assert the source committed exactly the new
                    // bytes (no tail), and the snapshot's content-addressed `manifest_id`
                    // proves the replica converges on that same content.
                    if !files.is_empty() {
                        let e = &entries[files[next() % files.len()]];
                        let len = 16 + next() % 200;
                        let data: Vec<u8> = (0..len).map(|i| (i.wrapping_add(iter)) as u8).collect();
                        src.truncate(e.ino, 0).unwrap();
                        src.write(e.ino, 0, &data).unwrap();
                        src.flush(e.ino).unwrap();
                        assert_eq!(
                            src.read(e.ino, 0, 4096).unwrap(),
                            data,
                            "overwrite-with-shorter left a stale tail at iter {iter}"
                        );
                    }
                }
                3 => {
                    // delete a file
                    if !files.is_empty() {
                        let e = entries.remove(files[next() % files.len()]);
                        src.unlink(e.parent, &e.name).unwrap();
                    }
                }
                4 => {
                    // rmdir an empty directory (no tracked children)
                    let empty: Vec<usize> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.dir && !entries.iter().any(|c| c.parent == e.ino))
                        .map(|(i, _)| i)
                        .collect();
                    if !empty.is_empty() {
                        let e = entries.remove(empty[next() % empty.len()]);
                        src.rmdir(e.parent, &e.name).unwrap();
                    }
                }
                _ => {
                    // move a file to a (possibly different) directory under a fresh name
                    if !files.is_empty() {
                        let i = files[next() % files.len()];
                        let newparent = dirs[next() % dirs.len()];
                        ctr += 1;
                        let newname = format!("f{ctr}");
                        let (op, on) = (entries[i].parent, entries[i].name.clone());
                        src.rename(op, &on, newparent, &newname).unwrap();
                        entries[i].parent = newparent;
                        entries[i].name = newname;
                    }
                }
            }

            mirror(&src, &dst);
            assert_eq!(snapshot(&src), snapshot(&dst), "replicas diverged after iter {iter}");
        }

        // Sanity: the harness actually built and tore down a non-trivial tree.
        assert!(!entries.is_empty(), "expected a surviving tree to compare");
    }
}
