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
                manifest_id TEXT
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_parent_name ON nodes(parent_ino, name);
            CREATE TABLE IF NOT EXISTS manifest_chunks (
                manifest_id TEXT NOT NULL,
                seq         INTEGER NOT NULL,
                chunk_hash  TEXT NOT NULL,
                offset      INTEGER NOT NULL,
                length      INTEGER NOT NULL,
                PRIMARY KEY (manifest_id, seq)
            );",
        )?;
        // Ensure the root directory (ino 1) exists.
        conn.execute(
            "INSERT OR IGNORE INTO nodes(ino, parent_ino, name, kind) VALUES (?1, NULL, '', 'dir')",
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
        let res = conn.execute(
            "INSERT INTO nodes(parent_ino, name, kind) VALUES (?1, ?2, ?3)",
            params![parent as i64, name, kind.as_str()],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                return Err(FsError::AlreadyExists)
            }
            Err(e) => return Err(e.into()),
        }
        let ino = conn.last_insert_rowid() as u64;
        Ok(Attr {
            ino,
            kind,
            size: 0,
            mtime: 0,
        })
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
        tx.commit()?;
        Ok(())
    }

    fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND kind = 'file'",
            params![parent as i64, name],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
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
        conn.execute("DELETE FROM nodes WHERE ino = ?1", params![ino])?;
        Ok(())
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE nodes SET parent_ino = ?1, name = ?2 WHERE parent_ino = ?3 AND name = ?4",
            params![newparent as i64, newname, parent as i64, name],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        Ok(())
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
