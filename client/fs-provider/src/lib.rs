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

/// POSIX `NAME_MAX`: the maximum length, **in bytes**, of a single path component. A
/// longer name fails with `ENAMETOOLONG` (pjdfstest `<op>/02.t`). The kernel enforces this
/// per component (bytes, not chars), so we mirror it on every node-creating/-renaming path.
pub const NAME_MAX: usize = 255;

/// Reject a component longer than [`NAME_MAX`] bytes with [`FsError::NameTooLong`]
/// (ENAMETOOLONG). Shared by `create`/`mkdir` (via `insert_node`) and `rename`.
fn check_name_len(name: &str) -> Result<()> {
    if name.len() > NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

/// The default permission bits for a freshly created node (no umask modeling — the
/// mount runs as the user, who owns everything).
fn default_mode(kind: Kind) -> u32 {
    match kind {
        Kind::Dir => 0o755,
        Kind::File => 0o644,
        // Symlinks are conventionally 0o777 (the mode is ignored on most systems — the
        // link's permissions come from the target — but git records 120000 and tools
        // expect lrwxrwxrwx, so report 0o777 here).
        Kind::Symlink => 0o777,
    }
}

/// Escape the LIKE wildcards `%` and `_` (and the escape char `\` itself) so a user's
/// search query is matched literally, not as a glob. Pair with `ESCAPE '\'` in the SQL.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

/// Wall-clock now in unix milliseconds — the unit the G5 outbox debounce records.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A symbolic link. The link target (an arbitrary byte path) is stored out-of-band in
    /// the node's `symlink_target` column, read back via [`SqliteProvider::readlink`]. Git
    /// stores symlinks (mode 120000) and `git checkout` materializes them, so this is the
    /// one "special" POSIX kind the sync engine models (FIFOs/sockets/devices stay non-goals
    /// — they have no portable cross-device meaning; a symlink target is just bytes).
    Symlink,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Symlink => "symlink",
        }
    }
    fn from_str(s: &str) -> Kind {
        match s {
            "dir" => Kind::Dir,
            "symlink" => Kind::Symlink,
            _ => Kind::File,
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
    /// Access time, **unix nanoseconds**. **Device-local** — set by `utimensat`/`set_atime`
    /// and reported by `getattr`, but deliberately NOT carried in the sync export (every
    /// device has its own access pattern; syncing atime would be both wrong and chatty).
    /// Defaults to the node's mtime when never explicitly set.
    pub atime: i64,
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
    /// For `kind == Symlink`: the link target bytes (a path), synced verbatim so the link
    /// resolves identically on every device. `None` for files and dirs. This is a symlink's
    /// entire content — there's no manifest/chunks for a link (the target is tiny inline data).
    pub symlink_target: Option<Vec<u8>>,
}

/// Options for [`SqliteProvider::search`]. `limit`/`offset` paginate the (path-ordered)
/// result set so a huge tree never returns an unbounded list. Defaults: a generous cap,
/// from the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOpts {
    /// Max results to return (cap; the local tree could be large).
    pub limit: usize,
    /// Skip the first `offset` results (paginate alongside `limit`).
    pub offset: usize,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            limit: 200,
            offset: 0,
        }
    }
}

/// A single search hit: enough to render a result row and navigate to the node without a
/// second round-trip. `path` is the full slash-rooted path (e.g. `/Project/Bracket.STL`),
/// reconstructed from the parent chain. `size`/`mtime` come straight from the local row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub node_id: String,
    /// Full path from the library root, leading slash, e.g. `/a/b/file.stl`.
    pub path: String,
    /// The basename (leaf component), e.g. `file.stl`.
    pub name: String,
    pub kind: Kind,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Real modification time, unix nanoseconds (0 if never set).
    pub mtime: i64,
}

/// A file within a subtree, as enumerated by [`SqliteProvider::subtree_files`] — the
/// work-list unit for "make this folder available offline" + the pre-disconnect readiness
/// report. Directories are not represented (only files carry chunks to hydrate/verify).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeFile {
    /// The file's stable `node_id` (survives renames/moves) — the key the hydrate +
    /// verification primitives take.
    pub node_id: String,
    /// The file's path relative to (and *including*) the subtree root's own name, e.g.
    /// querying `Jobs` yields `Jobs/ClientX/part.step`. A file root yields just its name.
    /// Human-readable for the "still-online-only" list in a readiness report.
    pub path: String,
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
    /// The node already existed and this apply **moved/renamed** it — its `(parent, name)`
    /// changed, so its on-disk path is now different. For on-demand-population adapters (CfAPI /
    /// macOS File Provider) the OLD-path placeholder must be removed and a NEW-path one created;
    /// the changelog models a move as an upsert on the same stable `node_id` (not a delete +
    /// create), so neither `inserted` nor `content_changed` catches it. `false` for an INSERT (a
    /// brand-new node has no old path) and for an in-place content/metadata change. FUSE/WebDAV/
    /// WinFsp ignore this (they enumerate live from the tree).
    pub moved: bool,
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
    /// The **incoming node itself** had an un-pushed local edit — a pending outbox `upsert`
    /// whose manifest differs from the incoming one — so `apply_remote` **left the local row
    /// untouched** rather than overwriting the local manifest + base `version_id`. Overwriting
    /// would silently destroy the local edit: the later push would then read the remote's
    /// manifest, fast-forward on the remote's base, and the server would never record the
    /// conflict. By preserving the local edit (manifest + its original base version), the queued
    /// push sends the local content against its *stale* base, so the server records a real
    /// conflict (keep/split) instead. The pull caller should treat this upsert as **deferred**
    /// (not applied) and let the outbox push surface the conflict server-side. `false` for a
    /// clean apply (no pending local edit, or the incoming manifest matches the local one).
    pub dirty_local: bool,
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
    /// The arguments are structurally invalid for the operation — currently raised when a
    /// `rename` would move a directory into its own subtree (a directory cannot become its
    /// own ancestor; POSIX maps this to **EINVAL**). Mapped to `EINVAL` by the adapters.
    #[error("invalid argument")]
    InvalidArgument,
    /// A single path component exceeds `NAME_MAX` (255 bytes) — POSIX **ENAMETOOLONG**.
    /// Mapped to `ENAMETOOLONG` by the adapters.
    #[error("file name too long")]
    NameTooLong,
    #[error("chunk missing: {0}")]
    ChunkMissing(String),
    #[error("locked (would block)")]
    WouldBlock,
    /// The operation targets a node kind the sync engine deliberately does not model —
    /// symlinks, FIFOs, sockets, and device (block/char) nodes (see CLAUDE.md / the
    /// parity-roadmap POSIX non-goals). The adapters reject these cleanly (mapped to
    /// `EPERM`) so an attempt fails with a deterministic errno rather than corrupting the
    /// tree (which only stores `File`/`Dir`) or panicking.
    #[error("operation not supported (special files are not synced)")]
    NotSupported,
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
    /// Set the access time (unix nanoseconds — the kernel's `setattr(atime)`, e.g. a
    /// `utimensat`). Default no-op; `SqliteProvider` persists it **device-locally** (atime
    /// is access state, never synced — see [`Attr::atime`]).
    fn set_atime(&self, _ino: u64, _atime: i64) -> Result<()> {
        Ok(())
    }
    /// Resize the file to exactly `size` bytes (the kernel's `setattr`/`O_TRUNC`).
    /// Shrinking drops any content past the new EOF (so an overwrite-with-shorter
    /// leaves no stale tail); growing zero-fills. The new length is what `flush`
    /// will commit and what the sync push will export.
    fn truncate(&self, ino: u64, size: u64) -> Result<()>;

    /// Begin an overwrite: stage an EMPTY buffer for `ino` **without committing**, so the last
    /// committed version stays the durable fallback until the full new body is flushed. This is
    /// what an overwriting open (a truncating WebDAV PUT) uses instead of `truncate(ino, 0)`:
    /// `truncate(ino, 0)` on an unstaged file commits an empty version *immediately*, so an
    /// aborted/short PUT would recover to 0 bytes (and an empty version could sync mid-PUT).
    /// Staging-without-commit means an aborted write (see [`discard`](Self::discard)) reverts to
    /// the PRIOR committed content. Default: fall back to `truncate(ino, 0)` (safe for providers
    /// that don't stage); `SqliteProvider` overrides with the stage-without-commit behavior.
    fn begin_overwrite(&self, ino: u64) -> Result<()> {
        self.truncate(ino, 0)
    }

    /// Discard any in-progress (unflushed) staged write for `ino`, reverting to the last
    /// committed version. Default no-op; `SqliteProvider` drops the staging buffer. The WebDAV
    /// adapter calls this from a write handle's `Drop` when the handle was never successfully
    /// flushed (a client disconnect mid-PUT), so stale staging can't shadow committed content.
    fn discard(&self, _ino: u64) -> Result<()> {
        Ok(())
    }

    fn unlink(&self, parent: u64, name: &str) -> Result<()>;
    fn rmdir(&self, parent: u64, name: &str) -> Result<()>;
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()>;

    /// Create a symbolic link `name` under `parent`, pointing at `target` (raw bytes — a
    /// path, stored verbatim). The git-checkout path (mode-120000 entries). Default returns
    /// `NotSupported` (an adapter/provider without link support behaves as before — EPERM);
    /// `SqliteProvider` stores + syncs the target.
    fn symlink(&self, _parent: u64, _name: &str, _target: &[u8]) -> Result<Attr> {
        Err(FsError::NotSupported)
    }
    /// Read a symlink's target bytes (`readlink(2)`). Default `NotSupported`; `SqliteProvider`
    /// returns the stored target (and `NotSupported` if `ino` is not a symlink).
    fn readlink(&self, _ino: u64) -> Result<Vec<u8>> {
        Err(FsError::NotSupported)
    }

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
                -- AUTOINCREMENT so a deleted rowid is NEVER reused. A plain INTEGER PRIMARY
                -- KEY reuses the rowid of a just-deleted highest node, and a reused ino could
                -- inherit stale in-memory state (staging bytes / advisory locks) keyed by ino.
                -- We also purge that state on every removal path (belt and braces), but not
                -- reusing the ino closes the hazard structurally.
                ino         INTEGER PRIMARY KEY AUTOINCREMENT,
                parent_ino  INTEGER,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                size        INTEGER NOT NULL DEFAULT 0,
                mtime       INTEGER NOT NULL DEFAULT 0,  -- real mtime, unix NANOSECONDS (preserved + synced)
                atime       INTEGER NOT NULL DEFAULT 0,  -- access time, unix NANOSECONDS (DEVICE-LOCAL, never synced)
                manifest_id TEXT,
                node_id     TEXT,
                version_id  TEXT,         -- server version this node reflects (commit base)
                mode        INTEGER,      -- POSIX permission bits (preserved + synced)
                symlink_target BLOB       -- for kind='symlink': the link target bytes (NULL otherwise)
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
                op      TEXT NOT NULL,       -- 'upsert' | 'delete'
                -- G5 debounce bookkeeping (unix millis): enqueued_at is refreshed on
                -- every re-enqueue (the last touch, used to detect quiescence), while
                -- first_enqueued_at is set once and preserved (the max-age cap, so a
                -- file saved continuously still eventually syncs). 0 on rows from older
                -- DBs; the debounce treats a 0 as ready-now (no debounce info, so push).
                enqueued_at       INTEGER NOT NULL DEFAULT 0,
                first_enqueued_at INTEGER NOT NULL DEFAULT 0
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
        // atime column on an existing DB (device-local access time; 0 → falls back to mtime).
        let _ = conn.execute(
            "ALTER TABLE nodes ADD COLUMN atime INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Symlink support (git checkout of a symlinked path); NULL on older rows + non-links.
        let _ = conn.execute("ALTER TABLE nodes ADD COLUMN symlink_target BLOB", []);
        // G5 debounce columns on an existing outbox (no-op on a fresh DB).
        let _ = conn.execute(
            "ALTER TABLE outbox ADD COLUMN enqueued_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE outbox ADD COLUMN first_enqueued_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
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
            "SELECT ino, kind, size, mtime, mode, atime FROM nodes WHERE ino = ?1",
            params![ino as i64],
            |r| {
                let kind = Kind::from_str(&r.get::<_, String>(1)?);
                let mtime = r.get::<_, i64>(3)?;
                // atime defaults to mtime when never explicitly set (a fresh node, or an
                // older DB predating the column) — a sane "last touched" value, and what a
                // noatime-style mount would report.
                let atime = match r.get::<_, Option<i64>>(5)? {
                    Some(a) if a > 0 => a,
                    _ => mtime,
                };
                Ok(Attr {
                    ino: r.get::<_, i64>(0)? as u64,
                    kind,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime,
                    atime,
                    mode: r
                        .get::<_, Option<i64>>(4)?
                        .map(|m| m as u32)
                        .unwrap_or_else(|| default_mode(kind)),
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
            let bytes = self
                .store
                .get(&hash)
                .map_err(|_| FsError::ChunkMissing(hash))?;
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
    fn assemble_range(
        &self,
        conn: &Connection,
        manifest_id: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
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
            let bytes = self
                .store
                .get(&hash)
                .map_err(|_| FsError::ChunkMissing(hash))?;
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
    /// The [`Kind`] of an inode, or `NotFound` if it's gone. The shared guard for the
    /// content ops (`read`/`write`/`truncate`): they operate on file bytes, so a directory
    /// or symlink ino reaching them is a misuse that must fail with the kernel's errno
    /// rather than silently corrupt the tree (stamping a file manifest onto a dir/symlink
    /// row, or returning an empty buffer for a directory).
    fn kind_of(&self, ino: u64) -> Result<Kind> {
        let conn = self.conn.lock().unwrap();
        let k: Option<String> = conn
            .query_row(
                "SELECT kind FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
            .optional()?;
        k.map(|s| Kind::from_str(&s)).ok_or(FsError::NotFound)
    }

    /// Drop the in-memory per-inode state — the staging buffer and any advisory locks — for
    /// inodes whose tree rows have just been removed (unlink/rmdir/rename-replace/remote
    /// delete/collision sweep). The rowids may be reused by a future node; without this purge a
    /// reused ino could serve a dead file's staged bytes or inherit a phantom lock. Call it
    /// AFTER the row delete commits, with the inos collected BEFORE the delete.
    fn purge_inode_state(&self, inos: &[i64]) {
        if inos.is_empty() {
            return;
        }
        let mut staging = self.staging.lock().unwrap();
        for &ino in inos {
            staging.remove(&(ino as u64));
            self.locks.purge(ino as u64);
        }
    }

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
        check_name_len(name)?;
        let mut conn = self.conn.lock().unwrap();
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
        // The INSERT and the (dir) outbox enqueue land in ONE transaction so a crash can't
        // create the node without its sync intent (or vice versa).
        let tx = conn.transaction()?;
        // Seed atime = mtime at creation so it has a concrete value; thereafter set_mtime
        // never disturbs atime and set_atime never disturbs mtime (the UTIME_OMIT contract).
        let res = tx.execute(
            "INSERT INTO nodes(parent_ino, name, kind, node_id, mode, mtime, atime) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![parent as i64, name, kind.as_str(), node_id, mode as i64, mtime],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(FsError::AlreadyExists)
            }
            Err(e) => return Err(e.into()),
        }
        let ino = tx.last_insert_rowid() as u64;
        // Dirs are sync-ready on creation; files wait until `flush` has a manifest.
        if kind == Kind::Dir {
            Self::enqueue(&tx, &node_id, "upsert")?;
        }
        tx.commit()?;
        Ok(Attr {
            ino,
            kind,
            size: 0,
            mtime,
            atime: mtime, // a fresh node's atime starts at its creation/mtime
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
            .query_row(
                "SELECT node_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Export the whole tree as sync-ready descriptors for pushing to the server.
    /// Excludes the root; a top-level node's `parent_node_id` is `None`.
    pub fn export_tree(&self) -> Result<Vec<SyncNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT n.node_id, n.name, n.kind, n.manifest_id,
                    CASE WHEN n.parent_ino = ?1 THEN NULL ELSE p.node_id END, n.version_id, n.mode, n.mtime,
                    n.symlink_target
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
                r.get::<_, Option<Vec<u8>>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                node_id,
                name,
                kind_s,
                manifest_id,
                parent_node_id,
                version_id,
                mode,
                mtime,
                symlink_target,
            ) = row?;
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
                symlink_target,
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

    // ---- local search (G9) ------------------------------------------------

    /// Instant, offline search of the **local** SQLite tree by name or full path.
    ///
    /// Local-first: this touches only the device's own metadata — never the WAN and never
    /// the chunk store — so an online-only (never-hydrated) node is still found. The match
    /// is a case-insensitive substring of either the basename *or* the full slash-rooted
    /// path (so a query of a parent folder name finds its descendants). Results are ordered
    /// by path (stable) and paginated by [`SearchOpts`]. The root is never a hit.
    ///
    /// `%`/`_` in the query are treated literally (escaped for the `LIKE`).
    pub fn search(&self, query: &str, opts: &SearchOpts) -> Result<Vec<SearchHit>> {
        let conn = self.conn.lock().unwrap();
        // A recursive CTE reconstructs each node's full path (parent_ino chain) in one pass,
        // then we filter on the lowercased path. The root (ino = ROOT_INO) seeds with an
        // empty path and is excluded from the output by the `depth > 0` guard.
        let pattern = format!("%{}%", escape_like(&query.to_lowercase()));
        let mut stmt = conn.prepare(
            "WITH RECURSIVE paths(ino, parent_ino, name, kind, size, mtime, node_id, path, depth) AS (
                 SELECT ino, parent_ino, name, kind, size, mtime, node_id, '', 0
                   FROM nodes WHERE ino = ?1
                 UNION ALL
                 SELECT n.ino, n.parent_ino, n.name, n.kind, n.size, n.mtime, n.node_id,
                        paths.path || '/' || n.name, paths.depth + 1
                   FROM nodes n JOIN paths ON n.parent_ino = paths.ino
                  WHERE n.ino != ?1
             )
             SELECT node_id, path, name, kind, size, mtime FROM paths
             WHERE depth > 0 AND LOWER(path) LIKE ?2 ESCAPE '\\'
             ORDER BY path
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![
                ROOT_INO as i64,
                pattern,
                opts.limit as i64,
                opts.offset as i64
            ],
            |r| {
                let kind = Kind::from_str(&r.get::<_, String>(3)?);
                Ok(SearchHit {
                    node_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    path: r.get::<_, String>(1)?,
                    name: r.get::<_, String>(2)?,
                    kind,
                    size: r.get::<_, i64>(4)? as u64,
                    mtime: r.get::<_, i64>(5)?,
                })
            },
        )?;
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
        let mut conn = self.conn.lock().unwrap();
        // Wrap the entire apply in ONE transaction. The manifest-chunk insert loop below runs
        // under an `EXISTS` guard, so a crash partway through (with no transaction) would leave
        // a PARTIAL manifest that the guard then treats as "already present" forever → the node
        // reads back permanently truncated (silent data loss). A transaction makes the whole
        // apply — manifest rows, collision sweep, and the node upsert — atomic.
        let tx = conn.transaction()?;
        let parent_ino: i64 = match &node.parent_node_id {
            None => ROOT_INO as i64,
            Some(pid) => tx
                .query_row(
                    "SELECT ino FROM nodes WHERE node_id = ?1",
                    params![pid],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(FsError::NotFound)?,
        };
        // Look up the prior row (ino + manifest + symlink target) so we can both reuse the
        // inode on update and decide whether the content actually changed (→ cache invalidation).
        let prior: Option<(i64, Option<String>, i64, String, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT ino, manifest_id, parent_ino, name, symlink_target FROM nodes WHERE node_id = ?1",
                params![node.node_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        // BUG #1 (dirty-local guard): if the node ALREADY exists locally, carries a pending
        // outbox `upsert` (an un-pushed local edit), AND the incoming manifest differs from the
        // local one, then overwriting the row here would silently destroy the local edit and its
        // base `version_id` — the later push would fast-forward on the remote's base and the
        // server would never record the conflict. Leave the local row completely untouched and
        // signal `dirty_local`; the queued push then sends the local content against its stale
        // base, so the server records a real conflict (keep/split). No mutation has happened yet
        // (the tx rolls back on drop), so this is a clean early return.
        if let Some((prior_ino, old_manifest, _, _, _)) = &prior {
            if *old_manifest != node.manifest_id {
                let pending_upsert: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM outbox WHERE node_id = ?1 AND op = 'upsert')",
                    params![node.node_id],
                    |r| r.get(0),
                )?;
                if pending_upsert {
                    return Ok(ApplyOutcome {
                        ino: *prior_ino as u64,
                        content_changed: false,
                        moved: false,
                        inserted: false,
                        displaced_pending: false,
                        dirty_local: true,
                    });
                }
            }
        }
        if let Some(mid) = &node.manifest_id {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM manifest_chunks WHERE manifest_id = ?1)",
                params![mid],
                |r| r.get(0),
            )?;
            if !exists {
                for (i, c) in node.chunks.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO manifest_chunks(manifest_id, seq, chunk_hash, offset, length)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![mid, i as i64, c.hash, c.offset as i64, c.length as i64],
                    )?;
                }
            }
        }
        // A symlink's "size" is its target length (it has no chunks); a file's is the sum of
        // its chunk lengths.
        let size: i64 = match (node.kind, &node.symlink_target) {
            (Kind::Symlink, Some(t)) => t.len() as i64,
            _ => node.chunks.iter().map(|c| c.length as i64).sum(),
        };
        // Cycle guard (pull path): refuse to re-parent an *existing* node under itself or one
        // of its own descendants — that would detach the node + its subtree into a cycle that's
        // unreachable from ROOT (orphaned data + an infinite parent-walk). The authoritative
        // server tree is always acyclic, so a cyclic upsert only arises from a transient
        // *delta-interleaving* on the receiving side (e.g. "X.parent:=Y" and "Y.parent:=X" in the
        // same delta, applied before the other's re-parent lands). We reject WITHOUT mutating the
        // row (it stays where it was → still reachable) and surface `InvalidArgument`; the pull
        // loop mirrors its parent-not-present discipline — DEFER + retry (a later entry in the
        // same delta usually resolves the interleaving), and if it still can't resolve, fall back
        // to the full-tree reconcile floor (`pull_snapshot`), which re-applies the acyclic server
        // snapshot. Mirrors the local-`rename` EINVAL guard ("a dir can't become its own
        // ancestor"). New INSERTs can't form a cycle (a fresh node has no descendants yet), so we
        // only check on a re-parent of an existing node.
        if let Some((self_ino, _, old_parent, _, _)) = &prior {
            if *old_parent != parent_ino
                && self.ino_is_ancestor_or_self(&tx, *self_ino, parent_ino)?
            {
                return Err(FsError::InvalidArgument);
            }
        }
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
        let displaced_pending: bool = tx.query_row(
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
        // Collect the displaced subtree's inodes BEFORE the sweep so their in-memory
        // staging/lock state can be purged after commit (their rowids are freed for reuse).
        let swept_inos: Vec<i64> = {
            let mut stmt = tx.prepare(
                "WITH RECURSIVE sub(ino) AS (
                     SELECT ino FROM nodes WHERE parent_ino = ?1 AND name = ?2 AND node_id != ?3
                     UNION ALL
                     SELECT n.ino FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                 )
                 SELECT ino FROM sub",
            )?;
            let v: Vec<i64> = stmt
                .query_map(params![parent_ino, node.name, node.node_id], |r| {
                    r.get::<_, i64>(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
            v
        };
        tx.execute(
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
        let ino = if let Some((ino, _, _, _, _)) = &prior {
            tx.execute(
                "UPDATE nodes SET parent_ino=?1, name=?2, kind=?3, manifest_id=?4, size=?5, mtime=?6, version_id=?7, mode=?8, symlink_target=?10 WHERE node_id=?9",
                params![parent_ino, node.name, node.kind.as_str(), node.manifest_id, size, node.mtime, node.version_id, (node.mode & 0o7777) as i64, node.node_id, node.symlink_target],
            )?;
            *ino as u64
        } else {
            tx.execute(
                "INSERT INTO nodes(parent_ino, name, kind, node_id, manifest_id, size, mtime, version_id, mode, symlink_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![parent_ino, node.name, node.kind.as_str(), node.node_id, node.manifest_id, size, node.mtime, node.version_id, (node.mode & 0o7777) as i64, node.symlink_target],
            )?;
            tx.last_insert_rowid() as u64
        };
        // Content changed iff the node already existed and its manifest moved (a same-size
        // edit included — that's exactly the case the old mtime-bump heuristic could miss) OR,
        // for a symlink (which has no manifest), its target bytes changed.
        let content_changed = match &prior {
            Some((_, old_manifest, _, _, old_target)) => {
                *old_manifest != node.manifest_id || *old_target != node.symlink_target
            }
            None => false,
        };
        // Moved/renamed iff the node already existed and its (parent, name) changed. The
        // changelog models a move as an upsert on the same stable node_id, so this is the only
        // signal that an on-demand-population adapter's old-path placeholder is now stale.
        let moved = match &prior {
            Some((_, _, old_parent, old_name, _)) => {
                *old_parent != parent_ino || *old_name != node.name
            }
            None => false,
        };
        let inserted = prior.is_none();
        tx.commit()?;
        // The collision sweep freed the displaced subtree's rowids — purge their in-memory
        // staging/lock state so a future node reusing an ino can't inherit stale bytes/locks.
        self.purge_inode_state(&swept_inos);
        Ok(ApplyOutcome {
            ino,
            content_changed,
            inserted,
            moved,
            displaced_pending,
            dirty_local: false,
        })
    }

    /// Would re-parenting a node under `cand` (the proposed new parent ino) create a cycle —
    /// i.e. is `node_ino` equal to `cand` or an ancestor of `cand`? Walk `cand`'s parent chain
    /// up to ROOT (NULL parent); if it passes through `node_ino`, placing `node_ino` *above*
    /// `cand` would close a loop. Bounded by the tree depth (the chain is acyclic *before* this
    /// would-be mutation, which is exactly the invariant the caller preserves by rejecting).
    /// Mirrors the ancestor walk in local `rename`'s "dir into its own subtree" EINVAL guard.
    fn ino_is_ancestor_or_self(
        &self,
        conn: &rusqlite::Connection,
        node_ino: i64,
        cand: i64,
    ) -> Result<bool> {
        let mut cur = cand;
        loop {
            if cur == node_ino {
                return Ok(true);
            }
            if cur == ROOT_INO as i64 {
                return Ok(false);
            }
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT parent_ino FROM nodes WHERE ino = ?1",
                    params![cur],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match parent {
                Some(p) => cur = p,
                None => return Ok(false), // reached a top-level node (NULL parent → ROOT)
            }
        }
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
        let swept_inos: Vec<i64>;
        let swept_pending: bool;
        {
            let conn = self.conn.lock().unwrap();
            // Detect, BEFORE deleting, whether any node in the to-be-swept subtree OTHER than the
            // explicit target has a pending outbox entry.
            swept_pending = conn.query_row(
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
            // Collect the subtree's inodes BEFORE deleting so we can purge their in-memory
            // staging/lock state (their rowids are freed for reuse by the delete).
            let mut stmt = conn.prepare(
                "WITH RECURSIVE sub(ino) AS (
                     SELECT ino FROM nodes WHERE node_id = ?1
                     UNION ALL
                     SELECT n.ino FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                 )
                 SELECT ino FROM sub",
            )?;
            swept_inos = stmt
                .query_map(params![node_id], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<_, _>>()?;
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
        }
        self.purge_inode_state(&swept_inos);
        Ok(swept_pending)
    }

    pub fn ino_for_node_id(&self, node_id: &str) -> Result<Option<u64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT ino FROM nodes WHERE node_id = ?1",
                params![node_id],
                |r| r.get::<_, i64>(0),
            )
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
    pub fn manifest_range_hashes(
        &self,
        node_id: &str,
        offset: u64,
        size: u64,
    ) -> Result<Vec<String>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mid: Option<String> = conn
            .query_row(
                "SELECT manifest_id FROM nodes WHERE node_id = ?1",
                params![node_id],
                |r| r.get::<_, Option<String>>(0),
            )
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
            .query_row(
                "SELECT manifest_id FROM nodes WHERE node_id = ?1",
                params![node_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match mid {
            None => Ok(Vec::new()),
            Some(m) => {
                let mut stmt = conn.prepare(
                    "SELECT chunk_hash FROM manifest_chunks WHERE manifest_id = ?1 ORDER BY seq",
                )?;
                let rows = stmt.query_map(params![m], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<std::result::Result<_, _>>()?)
            }
        }
    }

    /// Enumerate every **file** under a subtree (by `node_id`), returning each file's stable
    /// `node_id` + its path relative to (and including) the subtree root's own name. This is
    /// the work-list for "make this folder available offline" (which files' chunks to
    /// hydrate) and the pre-disconnect readiness report (which files to verify are local).
    ///
    /// A **file** root resolves to just that one file. A **directory** root resolves to every
    /// file at any depth beneath it (directories themselves are omitted — only files carry
    /// chunks). An empty directory, or an unknown `node_id`, yields an empty list (not an
    /// error — the caller is asking "what's under here", and "nothing" is a valid answer).
    /// Results are ordered by path.
    pub fn subtree_files(&self, node_id: &str) -> Result<Vec<SubtreeFile>> {
        let conn = self.conn.lock().unwrap();
        let root_ino: Option<i64> = conn
            .query_row(
                "SELECT ino FROM nodes WHERE node_id = ?1",
                params![node_id],
                |r| r.get(0),
            )
            .optional()?;
        let root_ino = match root_ino {
            None => return Ok(Vec::new()),
            Some(i) => i,
        };
        // Recursive CTE: seed with the subtree root (path = its own name), descend by
        // parent_ino building each node's path. Emit only files; the root itself is emitted
        // iff it is a file (a directory root has depth-0 kind 'dir', filtered out).
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(ino, node_id, name, kind, path) AS (
                 SELECT ino, node_id, name, kind, name FROM nodes WHERE ino = ?1
                 UNION ALL
                 SELECT n.ino, n.node_id, n.name, n.kind, sub.path || '/' || n.name
                   FROM nodes n JOIN sub ON n.parent_ino = sub.ino
                  WHERE n.ino != ?1
             )
             SELECT node_id, path FROM sub
             WHERE kind = 'file' AND node_id IS NOT NULL
             ORDER BY path",
        )?;
        let rows = stmt.query_map(params![root_ino], |r| {
            Ok(SubtreeFile {
                node_id: r.get::<_, String>(0)?,
                path: r.get::<_, String>(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
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
        let now = now_millis();
        // On insert: stamp both timestamps to `now`. On a re-enqueue of the same node
        // (a rapid re-save): refresh `enqueued_at` (the last-touch the debounce watches
        // for quiescence) but PRESERVE `first_enqueued_at` (the max-age cap floor, so a
        // continuously re-saved file still flushes once it has waited long enough).
        conn.execute(
            "INSERT INTO outbox(node_id, op, enqueued_at, first_enqueued_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(node_id) DO UPDATE SET op = excluded.op, enqueued_at = excluded.enqueued_at",
            params![node_id, op, now],
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

    /// The raw outbox entries with their debounce timestamps `(node_id, op, enqueued_at,
    /// first_enqueued_at)` (unix millis). Exposed for the engine's debounce gate + tests.
    pub fn outbox_list_timed(&self) -> Result<Vec<(String, String, i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node_id, op, enqueued_at, first_enqueued_at FROM outbox ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// **G5 debounce drain order.** Like [`outbox_list_topo`](Self::outbox_list_topo) but
    /// filters out `upsert`s that are still inside their debounce window, so a flurry of
    /// rapid saves to one file coalesces into a single push of the *final* content instead
    /// of N pushes of intermediate states.
    ///
    /// An `upsert` is **held** (skipped this tick, left queued — coalescing with any further
    /// save) while BOTH:
    ///   - it was last touched (`enqueued_at`) less than `window_ms` ago (still being saved
    ///     actively — wait for quiescence), AND
    ///   - its `first_enqueued_at` is less than `max_age_ms` ago (the hard cap: never hold a
    ///     node longer than this, so a file saved every second still eventually syncs).
    ///
    /// `delete`s are **never** held (low-volume, user-visible — push promptly, per the design
    /// in `docs/torn-writes-and-batching.md` §4). A held parent also holds its queued
    /// descendants so the parents-first invariant of the topo order is preserved (a child is
    /// never drained ahead of a parent that's still debouncing). `window_ms == 0` disables
    /// the debounce entirely (identical to `outbox_list_topo`). A `0` timestamp (older-DB
    /// row) counts as "ready now".
    pub fn outbox_ready_topo(
        &self,
        window_ms: i64,
        max_age_ms: i64,
    ) -> Result<Vec<(String, String)>> {
        if window_ms <= 0 {
            return self.outbox_list_topo();
        }
        let now = now_millis();
        let timed = self.outbox_list_timed()?;

        // Eligibility per node: a delete or a quiesced/aged-out upsert is eligible; a still-hot
        // upsert is held.
        let mut held: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (nid, op, enq, first) in &timed {
            if op == "delete" {
                continue;
            }
            // enq == 0 (legacy row) → ready. Otherwise hold while hot AND under the cap.
            let still_hot = *enq != 0 && now.saturating_sub(*enq) < window_ms;
            let under_cap = *first == 0 || now.saturating_sub(*first) < max_age_ms;
            if still_hot && under_cap {
                held.insert(nid.clone());
            }
        }
        if held.is_empty() {
            return self.outbox_list_topo();
        }

        // Propagate the hold down the tree: any queued node whose ancestor chain passes
        // through a held node is also held (don't drain a child before its still-debouncing
        // parent). Build child->parent over the queued upserts, then mark transitively.
        let mut parent_of: HashMap<String, Option<String>> = HashMap::new();
        for (nid, op, _, _) in &timed {
            if op != "delete" {
                if let Ok(Some(n)) = self.sync_node(nid) {
                    parent_of.insert(nid.clone(), n.parent_node_id.clone());
                }
            }
        }
        // Iterate to a fixpoint (cheap — the queue is small): a node becomes held if its
        // parent is held.
        let mut changed = true;
        while changed {
            changed = false;
            for (nid, op, _, _) in &timed {
                if op == "delete" || held.contains(nid) {
                    continue;
                }
                if let Some(Some(pid)) = parent_of.get(nid) {
                    if held.contains(pid) {
                        held.insert(nid.clone());
                        changed = true;
                    }
                }
            }
        }

        let ready: Vec<(String, String)> = self
            .outbox_list_topo()?
            .into_iter()
            .filter(|(nid, _)| !held.contains(nid))
            .collect();
        Ok(ready)
    }

    /// Remove one outbox entry once it has been pushed to the server. **Unconditional** —
    /// use [`outbox_clear_upto`](Self::outbox_clear_upto) from the WAN drain, which is
    /// re-enqueue-safe.
    pub fn outbox_clear(&self, node_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM outbox WHERE node_id = ?1", params![node_id])?;
        Ok(())
    }

    /// Remove an outbox entry only if it has **not been re-enqueued since** `enqueued_at`
    /// (the timestamp captured when the drain read the row). The sync-engine drain reads an
    /// entry, spends seconds in WAN I/O pushing it, then clears it — but a mount write during
    /// that window re-enqueues the same node, bumping its `enqueued_at` to a newer value.
    /// An unconditional `DELETE ... WHERE node_id = ?` would throw that newer dirty mark away
    /// → the just-made edit would never sync (a silent data-loss window). Gating the delete on
    /// `enqueued_at <= ?` deletes only the row the drain actually pushed; a re-enqueued (newer)
    /// row survives for the next tick. A `0` legacy timestamp on the captured side still clears
    /// its own `0` row (`0 <= 0`).
    pub fn outbox_clear_upto(&self, node_id: &str, enqueued_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM outbox WHERE node_id = ?1 AND enqueued_at <= ?2",
            params![node_id, enqueued_at],
        )?;
        Ok(())
    }

    /// The local changelog cursor (highest applied `seq`); 0 if never synced.
    pub fn cursor(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'cursor'",
                [],
                |r| r.get(0),
            )
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
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'library'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if current == Some(library_id) {
            return Ok(false); // same library — keep the local cache + cursor
        }
        let had_nodes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_ino IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        conn.execute("DELETE FROM nodes WHERE parent_ino IS NOT NULL", [])?;
        conn.execute("DELETE FROM manifest_chunks", [])?;
        conn.execute("DELETE FROM outbox", [])?;
        conn.execute("DELETE FROM sync_state", [])?; // clears the old cursor too
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES ('library', ?1)",
            params![library_id],
        )?;
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
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'needs_resync'",
                [],
                |r| r.get::<_, i64>(0),
            )
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
                        CASE WHEN n.parent_ino = ?2 THEN NULL ELSE p.node_id END, n.version_id, n.mode, n.mtime,
                        n.symlink_target
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
                        r.get::<_, Option<Vec<u8>>>(7)?,
                    ))
                },
            )
            .optional()?;
        let (name, kind_s, manifest_id, parent_node_id, version_id, mode, mtime, symlink_target) =
            match row {
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
            symlink_target,
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
        // Validate the inode exists AND is a file first — `flush` is a no-op on a missing
        // stage entry (a bad ino would silently do nothing), and ingesting bytes onto a
        // directory or symlink would stamp a file manifest onto it, corrupting the tree
        // (same hazard the write/truncate kind-guards close). dir -> EISDIR, symlink -> EINVAL.
        match self.kind_of(ino)? {
            Kind::File => {}
            Kind::Dir => return Err(FsError::IsADirectory),
            Kind::Symlink => return Err(FsError::InvalidArgument),
        }
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
        // opendir(2)/readdir on a non-directory is ENOTDIR; on a missing inode, ENOENT.
        // Without this the bare `parent_ino = ?` query returned an empty Vec for a file,
        // a symlink, or a non-existent ino alike — masking the kind/existence error as an
        // empty directory. (ROOT_INO is always a directory and is allowed.)
        if ino != ROOT_INO {
            match self.kind_of(ino)? {
                Kind::Dir => {}
                _ => return Err(FsError::NotADirectory),
            }
        }
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
        // Guard the node kind: read() is for file bytes. A directory → EISDIR (POSIX
        // read(2) on a directory fd); a symlink → EINVAL (you readlink() a symlink, not
        // read() it). Without this a directory read returned an empty buffer (wrong errno)
        // and a symlink read returned its target bytes (a structural misuse).
        match self.kind_of(ino)? {
            Kind::File => {}
            Kind::Dir => return Err(FsError::IsADirectory),
            Kind::Symlink => return Err(FsError::InvalidArgument),
        }
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
            .query_row(
                "SELECT manifest_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
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
        // Guard the node kind: write() is for file bytes. A directory → EISDIR; a symlink →
        // EINVAL. Without this a write+flush stamped a file manifest onto the dir/symlink
        // row, corrupting the tree (a symlink lost its target; a dir became a file).
        match self.kind_of(ino)? {
            Kind::File => {}
            Kind::Dir => return Err(FsError::IsADirectory),
            Kind::Symlink => return Err(FsError::InvalidArgument),
        }
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
        let chunker = fastcdc::v2020::FastCDC::new(
            &buf,
            self.chunk_cfg.min,
            self.chunk_cfg.avg,
            self.chunk_cfg.max,
        );
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
        if let Some(node_id) = tx
            .query_row(
                "SELECT node_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        {
            Self::enqueue(&tx, &node_id, "upsert")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn truncate(&self, ino: u64, size: u64) -> Result<()> {
        // POSIX: truncate() on a directory is EISDIR. Reject before touching staging/chunks
        // (a dir has no manifest; without this the size=0 path would commit an empty file
        // manifest onto the directory row). A missing inode is ENOENT.
        {
            let conn = self.conn.lock().unwrap();
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM nodes WHERE ino = ?1",
                    params![ino as i64],
                    |r| r.get(0),
                )
                .optional()?;
            match kind.map(|k| Kind::from_str(&k)) {
                None => return Err(FsError::NotFound),
                Some(Kind::Dir) => return Err(FsError::IsADirectory),
                // A symlink reaching truncate is a structural misuse — committing a file
                // manifest here would silently destroy the link target. EINVAL.
                Some(Kind::Symlink) => return Err(FsError::InvalidArgument),
                Some(Kind::File) => {}
            }
        }
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

    fn begin_overwrite(&self, ino: u64) -> Result<()> {
        // Stage an empty buffer WITHOUT committing (contrast `truncate(ino, 0)`, which commits
        // an empty version immediately for an unstaged file). The last committed manifest stays
        // the durable content until the full new body is flushed, so an aborted/short PUT
        // recovers the PRIOR content (via `discard`) rather than 0 bytes, and no empty
        // intermediate version can sync. Same kind guards as `truncate`/`write`.
        match self.kind_of(ino)? {
            Kind::File => {}
            Kind::Dir => return Err(FsError::IsADirectory),
            Kind::Symlink => return Err(FsError::InvalidArgument),
        }
        self.staging.lock().unwrap().insert(ino, Vec::new());
        Ok(())
    }

    fn discard(&self, ino: u64) -> Result<()> {
        // Drop the in-progress staged write; the last committed version remains the content.
        self.staging.lock().unwrap().remove(&ino);
        Ok(())
    }

    fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        // Look the target up by (parent, name) WITHOUT a kind filter, so we can return the
        // POSIX-correct errno: a missing entry is ENOENT, but a *directory* is EISDIR
        // (`unlink` of a dir must not masquerade as "not found"). Linux maps this to EISDIR;
        // the mount adapters surface it as such.
        let target: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT ino, node_id, kind FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![parent as i64, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (ino, node_id, kind) = target.ok_or(FsError::NotFound)?;
        if Kind::from_str(&kind) == Kind::Dir {
            return Err(FsError::IsADirectory);
        }
        // Delete the row and enqueue the tombstone in ONE transaction so a crash can't leave
        // the row removed but the delete un-queued (or vice versa). Delete by `node_id`, NOT
        // by `kind = 'file'`: a SYMLINK (which passed the IsADirectory guard above) matched
        // zero rows under the old `kind = 'file'` filter, so `rm symlink` removed NOTHING yet
        // still enqueued a delete — leaving a ghost row that then blocked re-creating the name.
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM nodes WHERE node_id = ?1", params![node_id])?;
        Self::enqueue(&tx, &node_id, "delete")?;
        tx.commit()?;
        // The rowid is now free to be reused — drop any staged bytes / locks keyed to it.
        self.purge_inode_state(&[ino]);
        Ok(())
    }

    fn rmdir(&self, parent: u64, name: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        // Resolve without a kind filter for the POSIX-correct errno: a missing entry is
        // ENOENT, but `rmdir` of a *file* (non-directory) is ENOTDIR — it must not look
        // like "not found".
        let (ino, kind): (i64, String) = conn
            .query_row(
                "SELECT ino, kind FROM nodes WHERE parent_ino = ?1 AND name = ?2",
                params![parent as i64, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(FsError::NotFound)?;
        if Kind::from_str(&kind) != Kind::Dir {
            return Err(FsError::NotADirectory);
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_ino = ?1",
            params![ino],
            |r| r.get(0),
        )?;
        if count > 0 {
            return Err(FsError::NotEmpty);
        }
        let node_id: Option<String> = conn
            .query_row(
                "SELECT node_id FROM nodes WHERE ino = ?1",
                params![ino],
                |r| r.get(0),
            )
            .optional()?;
        // Row delete + tombstone enqueue atomically (crash-consistency).
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM nodes WHERE ino = ?1", params![ino])?;
        if let Some(id) = &node_id {
            Self::enqueue(&tx, id, "delete")?;
        }
        tx.commit()?;
        self.purge_inode_state(&[ino]);
        Ok(())
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> Result<()> {
        // The *new* name is the one being written; the old name already passed this check
        // when its node was created, so only the destination component needs validating.
        check_name_len(newname)?;
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
        let src_kind = Kind::from_str(&tx.query_row(
            "SELECT kind FROM nodes WHERE ino = ?1",
            params![src_ino],
            |r| r.get::<_, String>(0),
        )?);

        // The destination *parent* must exist and be a directory — otherwise the bare
        // UPDATE below would reparent the node under a non-existent or non-directory ino,
        // silently orphaning it (it would vanish from every readdir while still holding a
        // row). POSIX: a missing prefix component is ENOENT; a file used as a directory is
        // ENOTDIR.
        let pkind: Option<String> = tx
            .query_row(
                "SELECT kind FROM nodes WHERE ino = ?1",
                params![newparent as i64],
                |r| r.get(0),
            )
            .optional()?;
        match pkind {
            None => return Err(FsError::NotFound),
            Some(k) if Kind::from_str(&k) != Kind::Dir => return Err(FsError::NotADirectory),
            Some(_) => {}
        }

        // A directory cannot be moved into its own subtree (it would become its own
        // ancestor — a cycle). POSIX: EINVAL. Walk the destination parent's ancestor chain
        // up to the root; if the source is on it (or IS the destination parent), reject.
        if src_kind == Kind::Dir {
            let mut cur = newparent as i64;
            loop {
                if cur == src_ino {
                    return Err(FsError::InvalidArgument);
                }
                let parent: Option<i64> = tx
                    .query_row(
                        "SELECT parent_ino FROM nodes WHERE ino = ?1",
                        params![cur],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                match parent {
                    Some(p) => cur = p,
                    None => break, // reached the root (NULL parent)
                }
            }
        }

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
        // Track a displaced (POSIX-replaced) destination inode so its in-memory staging/lock
        // state can be purged after the commit (its rowid is now free to be reused).
        let mut displaced_ino: Option<i64> = None;
        if let Some((dst_ino, dst_id, dst_kind_s)) = dst {
            if dst_ino == src_ino {
                return Ok(()); // rename onto itself — no-op
            }
            let dst_kind = Kind::from_str(&dst_kind_s);
            // POSIX cross-type rules: the source and destination must agree on directory-ness.
            //   - file  → existing directory : EISDIR
            //   - dir   → existing non-dir    : ENOTDIR
            //   - dir   → existing directory  : allowed only if the dest is empty (else ENOTEMPTY)
            // A symlink is a non-directory (like a file) for rename purposes.
            match (src_kind, dst_kind) {
                (Kind::File | Kind::Symlink, Kind::Dir) => return Err(FsError::IsADirectory),
                (Kind::Dir, Kind::File | Kind::Symlink) => return Err(FsError::NotADirectory),
                (Kind::Dir, Kind::Dir) => {
                    let kids: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM nodes WHERE parent_ino = ?1",
                        params![dst_ino],
                        |r| r.get(0),
                    )?;
                    if kids > 0 {
                        return Err(FsError::NotEmpty);
                    }
                }
                // non-dir onto non-dir: plain atomic replace (the atomic-save path)
                (Kind::File | Kind::Symlink, Kind::File | Kind::Symlink) => {}
            }
            // The displaced node is tombstoned so the deletion propagates on push
            // (otherwise other devices keep a duplicate).
            tx.execute("DELETE FROM nodes WHERE ino = ?1", params![dst_ino])?;
            Self::enqueue(&tx, &dst_id, "delete")?;
            displaced_ino = Some(dst_ino);
        }

        tx.execute(
            "UPDATE nodes SET parent_ino = ?1, name = ?2 WHERE ino = ?3",
            params![newparent as i64, newname, src_ino],
        )?;
        Self::enqueue(&tx, &src_id, "upsert")?;
        tx.commit()?;
        // The displaced destination's rowid is free now — drop its staged bytes / locks.
        if let Some(ino) = displaced_ino {
            self.purge_inode_state(&[ino]);
        }
        Ok(())
    }

    /// Create a **symlink** named `name` under `parent`, pointing at `target` (arbitrary
    /// bytes — a path). This is the git-checkout path: a repo storing a 120000 entry has its
    /// working-tree link materialized via `symlink(2)`. Sync-ready on creation (no manifest
    /// — the target *is* the content), so it's enqueued to the outbox immediately like a dir.
    fn symlink(&self, parent: u64, name: &str, target: &[u8]) -> Result<Attr> {
        // Same NAME_MAX (ENAMETOOLONG) rule as create/mkdir/rename — symlink(2) is a
        // node-creating op and the kernel enforces NAME_MAX on its component too. Without
        // this the over-long name fell through to SQLite as a generic Db error.
        check_name_len(name)?;
        let mut conn = self.conn.lock().unwrap();
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
        let mode = default_mode(Kind::Symlink);
        let mtime = now_nanos();
        let size = target.len() as i64;
        // INSERT + outbox enqueue in one transaction (crash-consistency).
        let tx = conn.transaction()?;
        let res = tx.execute(
            "INSERT INTO nodes(parent_ino, name, kind, node_id, mode, mtime, size, symlink_target)
             VALUES (?1, ?2, 'symlink', ?3, ?4, ?5, ?6, ?7)",
            params![
                parent as i64,
                name,
                node_id,
                mode as i64,
                mtime,
                size,
                target
            ],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(FsError::AlreadyExists)
            }
            Err(e) => return Err(e.into()),
        }
        let ino = tx.last_insert_rowid() as u64;
        Self::enqueue(&tx, &node_id, "upsert")?;
        tx.commit()?;
        Ok(Attr {
            ino,
            kind: Kind::Symlink,
            size: size as u64,
            mtime,
            atime: mtime,
            mode,
        })
    }

    /// Read a symlink's target bytes (the `readlink(2)` path). `NotSupported` if the inode is
    /// not a symlink (an EINVAL-shaped error — `readlink` on a regular file), `NotFound` if gone.
    fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(String, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT kind, symlink_target FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Err(FsError::NotFound),
            Some((kind, _)) if Kind::from_str(&kind) != Kind::Symlink => Err(FsError::NotSupported),
            Some((_, target)) => Ok(target.unwrap_or_default()),
        }
    }

    fn set_mode(&self, ino: u64, mode: u32) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row(
                "SELECT node_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
            .optional()?;
        // UPDATE + outbox enqueue atomically so a crash can't persist the new mode without
        // the sync intent that propagates it (or enqueue without the mode change).
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE nodes SET mode = ?1 WHERE ino = ?2",
            params![(mode & 0o7777) as i64, ino as i64],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        // Propagate the metadata change (re-commit/re-mkdir carries the new mode).
        if let Some(id) = &node_id {
            Self::enqueue(&tx, id, "upsert")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn set_mtime(&self, ino: u64, mtime: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let node_id: Option<String> = conn
            .query_row(
                "SELECT node_id FROM nodes WHERE ino = ?1",
                params![ino as i64],
                |r| r.get(0),
            )
            .optional()?;
        // UPDATE + outbox enqueue atomically (crash-consistency).
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE nodes SET mtime = ?1 WHERE ino = ?2",
            params![mtime, ino as i64],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        // Propagate the metadata change so the real mtime (e.g. from `cp -p`) reaches the
        // other devices — the re-commit carries the new mtime alongside the manifest.
        if let Some(id) = &node_id {
            Self::enqueue(&tx, id, "upsert")?;
        }
        tx.commit()?;
        Ok(())
    }

    fn set_atime(&self, ino: u64, atime: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE nodes SET atime = ?1 WHERE ino = ?2",
            params![atime, ino as i64],
        )?;
        if n == 0 {
            return Err(FsError::NotFound);
        }
        // Deliberately NOT enqueued to the outbox: atime is device-local access state, not
        // synced (unlike set_mtime, which propagates the real modification time). This is
        // the whole reason atime lives in a separate column the export ignores.
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
    fn begin_overwrite(&self, ino: u64) -> Result<()> {
        (**self).begin_overwrite(ino)
    }
    fn discard(&self, ino: u64) -> Result<()> {
        (**self).discard(ino)
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
    fn symlink(&self, parent: u64, name: &str, target: &[u8]) -> Result<Attr> {
        (**self).symlink(parent, name, target)
    }
    fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        (**self).readlink(ino)
    }
    fn set_mode(&self, ino: u64, mode: u32) -> Result<()> {
        (**self).set_mode(ino, mode)
    }
    fn set_mtime(&self, ino: u64, mtime: i64) -> Result<()> {
        (**self).set_mtime(ino, mtime)
    }
    fn set_atime(&self, ino: u64, atime: i64) -> Result<()> {
        (**self).set_atime(ino, atime)
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
        SqliteProvider::in_memory(
            store,
            ChunkConfig {
                min: 64,
                avg: 256,
                max: 1024,
            },
        )
        .unwrap()
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
            .query_row(
                "SELECT manifest_id FROM nodes WHERE ino=?1",
                params![a.ino as i64],
                |r| r.get(0),
            )
            .unwrap();
        let mb: String = conn
            .query_row(
                "SELECT manifest_id FROM nodes WHERE ino=?1",
                params![b.ino as i64],
                |r| r.get(0),
            )
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

        assert_eq!(
            p.getattr(f.ino).unwrap().size,
            100,
            "size reflects only the new content"
        );
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
        assert_eq!(
            &out[200..],
            &vec![0u8; 150][..],
            "grown region reads back as zeros"
        );
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
        assert!(
            dir_n.parent_node_id.is_none(),
            "top-level dir has no parent"
        );
        assert_eq!(dir_n.kind, Kind::Dir);

        let file_n = nodes.iter().find(|n| n.name == "part.bin").unwrap();
        assert_eq!(
            file_n.parent_node_id.as_ref(),
            Some(&dir_n.node_id),
            "file's parent_node_id is the dir's node_id"
        );
        assert!(file_n.manifest_id.is_some());
        assert!(
            !file_n.chunks.is_empty(),
            "file carries its manifest chunks"
        );
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
        assert_eq!(
            p.node_id_for(a.ino).unwrap().unwrap(),
            ida,
            "stable across calls"
        );
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
        assert_eq!(
            p.outbox_list().unwrap(),
            vec![(dir_id.clone(), "upsert".into())]
        );
        p.write(f.ino, 0, &vec![1u8; 1000]).unwrap();
        p.flush(f.ino).unwrap();
        // Parent dir enqueued before the file (insertion order preserved).
        assert_eq!(
            p.outbox_list().unwrap(),
            vec![
                (dir_id.clone(), "upsert".into()),
                (file_id.clone(), "upsert".into())
            ]
        );
        // A delete coalesces the file's entry to 'delete' (no duplicate row).
        p.unlink(dir.ino, "part.bin").unwrap();
        assert_eq!(
            p.outbox_list().unwrap(),
            vec![
                (dir_id.clone(), "upsert".into()),
                (file_id.clone(), "delete".into())
            ]
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
        let from_export = p
            .export_tree()
            .unwrap()
            .into_iter()
            .find(|n| n.node_id == file_id)
            .unwrap();
        let from_single = p.sync_node(&file_id).unwrap().unwrap();
        assert_eq!(
            from_single, from_export,
            "single-node export equals whole-tree export"
        );
        assert!(p.sync_node("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn unlink_and_rename() {
        let p = provider();
        p.create(ROOT_INO, "tmp.bin").unwrap();
        p.flush(p.lookup(ROOT_INO, "tmp.bin").unwrap().ino).unwrap();
        p.rename(ROOT_INO, "tmp.bin", ROOT_INO, "final.bin")
            .unwrap();
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
        assert!(
            b.ino_for_node_id(&y_id).unwrap().is_some(),
            "sibling subtree intact"
        );

        // Idempotent: deleting again (and deleting an unknown id) is a no-op.
        b.apply_remote_delete(&x_id).unwrap();
        b.apply_remote_delete("never-existed").unwrap();

        // Delete the parent dir: the whole subtree (sub/ and y.bin) goes with it,
        // even though no per-child delete was applied — no orphaned rows.
        b.apply_remote_delete(&proj_id).unwrap();
        assert!(b.ino_for_node_id(&proj_id).unwrap().is_none());
        assert!(
            b.ino_for_node_id(&sub_id).unwrap().is_none(),
            "nested dir swept"
        );
        assert!(
            b.ino_for_node_id(&y_id).unwrap().is_none(),
            "nested file swept"
        );
        assert!(b.export_tree().unwrap().is_empty(), "tree fully emptied");
    }

    /// `subtree_files` enumerates every FILE node under a subtree root (by node_id),
    /// returning each file's stable `node_id` + its path relative to the subtree root. This
    /// is the work-list backbone for "make this folder available offline" (which files to
    /// hydrate) and for the pre-disconnect readiness report (which files to check). A file
    /// root returns just itself; directories are not listed (only files carry chunks).
    #[test]
    fn subtree_files_enumerates_every_file_under_a_dir() {
        let p = provider();
        // Jobs/{a.stl, ClientX/{part.step, sub/deep.bin}}, plus an Archive sibling NOT under Jobs.
        let jobs = p.mkdir(ROOT_INO, "Jobs").unwrap();
        let a = p.create(jobs.ino, "a.stl").unwrap();
        p.write(a.ino, 0, &vec![1u8; 300]).unwrap();
        p.flush(a.ino).unwrap();
        let clientx = p.mkdir(jobs.ino, "ClientX").unwrap();
        let part = p.create(clientx.ino, "part.step").unwrap();
        p.write(part.ino, 0, &vec![2u8; 300]).unwrap();
        p.flush(part.ino).unwrap();
        let sub = p.mkdir(clientx.ino, "sub").unwrap();
        let deep = p.create(sub.ino, "deep.bin").unwrap();
        p.write(deep.ino, 0, &vec![3u8; 300]).unwrap();
        p.flush(deep.ino).unwrap();

        let archive = p.mkdir(ROOT_INO, "Archive").unwrap();
        let old = p.create(archive.ino, "old.stl").unwrap();
        p.write(old.ino, 0, &vec![4u8; 300]).unwrap();
        p.flush(old.ino).unwrap();

        let jobs_id = p.node_id_for(jobs.ino).unwrap().unwrap();
        let files = p.subtree_files(&jobs_id).unwrap();

        // Exactly the three files under Jobs (no dirs, nothing from Archive).
        let mut paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "Jobs/ClientX/part.step",
                "Jobs/ClientX/sub/deep.bin",
                "Jobs/a.stl"
            ]
        );
        // Each carries the real file node_id (resolvable back to an inode).
        for f in &files {
            assert!(
                p.ino_for_node_id(&f.node_id).unwrap().is_some(),
                "file node_id resolves"
            );
            assert!(
                !p.node_manifest_hashes(&f.node_id).unwrap().is_empty(),
                "file has a manifest"
            );
        }
    }

    /// A FILE root resolves to just that one file (so "available offline" works on a single
    /// file, not only a folder).
    #[test]
    fn subtree_files_on_a_file_returns_just_that_file() {
        let p = provider();
        let f = p.create(ROOT_INO, "solo.stl").unwrap();
        p.write(f.ino, 0, &vec![9u8; 300]).unwrap();
        p.flush(f.ino).unwrap();
        let id = p.node_id_for(f.ino).unwrap().unwrap();
        let files = p.subtree_files(&id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].node_id, id);
        assert_eq!(files[0].path, "solo.stl");
    }

    /// An empty directory (or an unknown node_id) yields no files — not an error.
    #[test]
    fn subtree_files_empty_dir_and_unknown_id_are_empty() {
        let p = provider();
        let empty = p.mkdir(ROOT_INO, "Empty").unwrap();
        let empty_id = p.node_id_for(empty.ino).unwrap().unwrap();
        assert!(
            p.subtree_files(&empty_id).unwrap().is_empty(),
            "no files under an empty dir"
        );
        assert!(
            p.subtree_files("never-existed").unwrap().is_empty(),
            "unknown id → empty"
        );
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
            p.outbox_list()
                .unwrap()
                .iter()
                .any(|(id, op)| id == &f_id && op == "upsert"),
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
            symlink_target: None,
        };
        let outcome = p.apply_remote(&remote).unwrap();

        // The remote won the name (authoritative) — and we were told the displaced local node
        // had pending state, so the caller knows to reconcile.
        assert!(
            outcome.displaced_pending,
            "must flag that a pending local node was clobbered"
        );
        assert!(
            p.ino_for_node_id("remote-R").unwrap().is_some(),
            "remote node present at the path"
        );
        assert!(
            p.ino_for_node_id(&f_id).unwrap().is_none(),
            "displaced local node is gone from the tree"
        );

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
            symlink_target: None,
        };
        p.apply_remote(&pulled).unwrap();
        p.outbox_clear("remote-S").unwrap(); // simulate it being a clean (non-local) row
                                             // A re-pull at the same path with a different id, no pending outbox entry on remote-S:
        let pulled2 = SyncNode {
            node_id: "remote-T".into(),
            ..pulled.clone()
        };
        let o2 = p.apply_remote(&pulled2).unwrap();
        assert!(
            !o2.displaced_pending,
            "no pending local state → no displaced_pending signal"
        );
    }

    /// `apply_remote` must flag `moved` when a pulled upsert relocates an EXISTING node
    /// (new parent and/or name, same stable node_id) — and NOT for an INSERT or an in-place
    /// content/metadata change. This is the signal an on-demand-population adapter (CfAPI)
    /// needs to remove the old-path placeholder + create the new one (the third live bug:
    /// renames left the old name lingering on the receiving mount).
    #[test]
    fn apply_remote_flags_moved_only_on_relocation() {
        let p = provider();
        let d1 = p.mkdir(ROOT_INO, "d1").unwrap();
        let d1_id = p.node_id_for(d1.ino).unwrap().unwrap();
        let d2 = p.mkdir(ROOT_INO, "d2").unwrap();
        let d2_id = p.node_id_for(d2.ino).unwrap().unwrap();

        // INSERT: a brand-new node — moved must be false (no old path).
        let mk = |node_id: &str, parent: &str, name: &str| SyncNode {
            node_id: node_id.into(),
            parent_node_id: Some(parent.into()),
            name: name.into(),
            kind: Kind::File,
            manifest_id: None,
            chunks: vec![],
            version_id: Some("v1".into()),
            mode: 0o644,
            mtime: 0,
            symlink_target: None,
        };
        let o_insert = p.apply_remote(&mk("R", &d1_id, "a.txt")).unwrap();
        assert!(o_insert.inserted && !o_insert.moved, "INSERT is not a move");

        // In-place metadata change (same parent + name): not a move.
        let o_same = p.apply_remote(&mk("R", &d1_id, "a.txt")).unwrap();
        assert!(
            !o_same.inserted && !o_same.moved,
            "same (parent,name) is not a move"
        );

        // RENAME (same parent, new name): moved.
        let o_rename = p.apply_remote(&mk("R", &d1_id, "b.txt")).unwrap();
        assert!(
            !o_rename.inserted && o_rename.moved,
            "rename within a dir is a move"
        );

        // MOVE (new parent, possibly same name): moved.
        let o_move = p.apply_remote(&mk("R", &d2_id, "b.txt")).unwrap();
        assert!(!o_move.inserted && o_move.moved, "reparent is a move");
        assert_eq!(
            p.parent_and_name(o_move.ino).unwrap().unwrap().0,
            d2.ino,
            "now under d2"
        );
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

        fn snapshot(
            p: &SqliteProvider,
        ) -> Vec<(String, Option<String>, String, String, Option<String>)> {
            let mut v: Vec<_> = p
                .export_tree()
                .unwrap()
                .into_iter()
                .map(|n| {
                    (
                        n.node_id,
                        n.parent_node_id,
                        n.name,
                        n.kind.as_str().to_string(),
                        n.manifest_id,
                    )
                })
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
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };

        for iter in 0..250 {
            // Candidate parent dirs = root + every live directory.
            let dirs: Vec<u64> = std::iter::once(ROOT_INO)
                .chain(entries.iter().filter(|e| e.dir).map(|e| e.ino))
                .collect();
            let files: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.dir)
                .map(|(i, _)| i)
                .collect();

            match next() % 6 {
                0 => {
                    // mkdir
                    let parent = dirs[next() % dirs.len()];
                    ctr += 1;
                    let name = format!("d{ctr}");
                    let a = src.mkdir(parent, &name).unwrap();
                    entries.push(E {
                        ino: a.ino,
                        parent,
                        name,
                        dir: true,
                    });
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
                    entries.push(E {
                        ino: a.ino,
                        parent,
                        name,
                        dir: false,
                    });
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
                        let data: Vec<u8> =
                            (0..len).map(|i| (i.wrapping_add(iter)) as u8).collect();
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
            assert_eq!(
                snapshot(&src),
                snapshot(&dst),
                "replicas diverged after iter {iter}"
            );
        }

        // Sanity: the harness actually built and tore down a non-trivial tree.
        assert!(!entries.is_empty(), "expected a surviving tree to compare");
    }
}
