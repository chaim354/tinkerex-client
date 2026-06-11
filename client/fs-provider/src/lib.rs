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

pub const ROOT_INO: u64 = 1;

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
    pub mtime: i64,
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
    fn unlink(&self, parent: u64, name: &str) -> Result<()>;
    fn rmdir(&self, parent: u64, name: &str) -> Result<()>;
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()>;
}

/// SQLite-backed [`Provider`]. Tree in SQLite, bytes in a [`ChunkStore`].
pub struct SqliteProvider {
    conn: Mutex<Connection>,
    store: Box<dyn ChunkStore>,
    /// In-progress writes, buffered in memory until `flush`. (Production streams to a
    /// staging file; in-memory keeps the foundation simple and fully testable.)
    staging: Mutex<HashMap<u64, Vec<u8>>>,
    chunk_cfg: ChunkConfig,
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
                mtime       INTEGER NOT NULL DEFAULT 0,
                manifest_id TEXT,
                node_id     TEXT
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
        })
    }

    fn attr_row(conn: &Connection, ino: u64) -> Result<Attr> {
        conn.query_row(
            "SELECT ino, kind, size, mtime FROM nodes WHERE ino = ?1",
            params![ino as i64],
            |r| {
                Ok(Attr {
                    ino: r.get::<_, i64>(0)? as u64,
                    kind: Kind::from_str(&r.get::<_, String>(1)?),
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get::<_, i64>(3)?,
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
        let res = conn.execute(
            "INSERT INTO nodes(parent_ino, name, kind, node_id) VALUES (?1, ?2, ?3, ?4)",
            params![parent as i64, name, kind.as_str(), node_id],
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
            mtime: 0,
        })
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
                    CASE WHEN n.parent_ino = ?1 THEN NULL ELSE p.node_id END
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
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (node_id, name, kind_s, manifest_id, parent_node_id) = row?;
            let chunks = match &manifest_id {
                Some(m) => Self::manifest_rows(&conn, m)?,
                None => Vec::new(),
            };
            out.push(SyncNode {
                node_id,
                parent_node_id,
                name,
                kind: Kind::from_str(&kind_s),
                manifest_id,
                chunks,
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
    pub fn apply_remote(&self, node: &SyncNode) -> Result<()> {
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
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE node_id = ?1)",
            params![node.node_id],
            |r| r.get(0),
        )?;
        if exists {
            conn.execute(
                "UPDATE nodes SET parent_ino=?1, name=?2, kind=?3, manifest_id=?4, size=?5 WHERE node_id=?6",
                params![parent_ino, node.name, node.kind.as_str(), node.manifest_id, size, node.node_id],
            )?;
        } else {
            conn.execute(
                "INSERT INTO nodes(parent_ino, name, kind, node_id, manifest_id, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![parent_ino, node.name, node.kind.as_str(), node.node_id, node.manifest_id, size],
            )?;
        }
        Ok(())
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

    pub fn set_cursor(&self, cursor: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES ('cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![cursor],
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
                        CASE WHEN n.parent_ino = ?2 THEN NULL ELSE p.node_id END
                 FROM nodes n LEFT JOIN nodes p ON p.ino = n.parent_ino
                 WHERE n.node_id = ?1",
                params![node_id, ROOT_INO as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let (name, kind_s, manifest_id, parent_node_id) = match row {
            Some(r) => r,
            None => return Ok(None),
        };
        let chunks = match &manifest_id {
            Some(m) => Self::manifest_rows(&conn, m)?,
            None => Vec::new(),
        };
        Ok(Some(SyncNode {
            node_id: node_id.to_string(),
            parent_node_id,
            name,
            kind: Kind::from_str(&kind_s),
            manifest_id,
            chunks,
        }))
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
        let content = self.current_content(ino)?;
        let start = (offset as usize).min(content.len());
        let end = (start + size as usize).min(content.len());
        Ok(content[start..end].to_vec())
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
            "UPDATE nodes SET size = ?1, manifest_id = ?2 WHERE ino = ?3",
            params![buf.len() as i64, manifest_id, ino as i64],
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
        let conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row(
                "SELECT node_id FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![parent as i64, name],
                |r| r.get(0),
            )
            .optional()?;
        let n = conn.execute(
            "UPDATE nodes SET parent_ino = ?1, name = ?2 WHERE parent_ino = ?3 AND name = ?4",
            params![newparent as i64, newname, parent as i64, name],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        if let Some(id) = node_id {
            Self::enqueue(&conn, &id, "upsert")?;
        }
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
    fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        (**self).unlink(parent, name)
    }
    fn rmdir(&self, parent: u64, name: &str) -> Result<()> {
        (**self).rmdir(parent, name)
    }
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        (**self).rename(parent, name, newparent, newname)
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
}
