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
#[cfg(target_os = "linux")] // only the FUSE EngineHydrator holds an Arc
use std::sync::Arc;

use fs_provider::{ChunkLoc, Kind, SqliteProvider, SyncNode};
use sync_client::{Change, ChunkRef, CommitBegin, ServerClient, TreeNode};

mod daemon;
pub use daemon::{DaemonConfig, WebdavMount, DEFAULT_POLL};

/// Client-side bandwidth throttling: a [`ThrottledChunkIO`](throttle::ThrottledChunkIO)
/// decorator that token-bucket rate-limits the bytes any [`ChunkIO`] transfers (separate
/// up/down caps; unlimited by default). See `docs/research-sync-competitors.md` §4 #6.
pub mod throttle;
pub use throttle::{RateLimiter, ThrottledChunkIO};
#[cfg(target_os = "linux")]
pub use daemon::Daemon;
#[cfg(target_os = "windows")]
pub use daemon::{CfApiMount, WinfspMount};

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

/// Active OS-cache invalidation. When a **pull** repoints an *existing* inode at new
/// content, the engine calls [`invalidate`](Invalidate::invalidate) so the adapter drops
/// the kernel's cached attrs + data for that inode and the next read re-fetches the new
/// manifest's chunks. This replaces the old `mtime = now`-on-pull bump (which forced a
/// re-read but destroyed the file's real modification time, breaking build tools).
///
/// Best-effort, and only FUSE has a strong impl today ([`EngineInvalidate`], via the kernel
/// `notify_inval_inode`). WebDAV (macOS) and WinFsp pass `None` and lean on short metadata
/// timeouts for now; CfAPI / macOS File Provider slot in here later (`CfUpdatePlaceholder`
/// / `signalEnumerator`) — the same two primitives, so this is the migration seam.
pub trait Invalidate: Send + Sync {
    fn invalidate(&self, ino: u64);

    /// A **pull inserted a brand-new node** into the local tree (not a content change to an
    /// existing one). The adapter should *create* a fresh placeholder for it so it appears
    /// without the user re-navigating — e.g. CfAPI's `CfCreatePlaceholders` in the node's
    /// parent directory. Default no-op: FUSE/WebDAV/WinFsp surface new nodes via their normal
    /// `readdir`, so only the on-demand-population adapters (CfAPI / File Provider) override it.
    fn created(&self, _ino: u64) {}

    /// A **pull removed a node** from the local tree (a `delete`, or a recursive subtree sweep).
    /// The adapter must remove the now-stale on-disk placeholder so the deletion converges — e.g.
    /// CfAPI's `std::fs::remove_file`/`remove_dir_all` at the node's last-known path. Called
    /// **BEFORE** the tree row is gone is impossible (the path lookup needs it), so the engine
    /// resolves the ino first and passes it here AFTER the delete; an on-demand adapter keeps an
    /// ino→path index it can still read. Default no-op: FUSE/WebDAV/WinFsp drop the entry via
    /// their normal `readdir` (the row is gone), so only on-demand-population adapters override.
    fn removed(&self, _ino: u64) {}

    /// A **pull moved/renamed an existing node** ((parent, name) changed; same stable node_id).
    /// The old on-disk path is now stale and the new one doesn't exist yet. An on-demand adapter
    /// must remove the OLD-path placeholder (resolved from its ino→path index, still holding the
    /// pre-move path) and create the NEW-path one. The changelog models a move as an upsert (not
    /// delete+create), so neither `created` nor `invalidate` fires — this is the only hook for it.
    /// Default no-op: FUSE/WebDAV/WinFsp re-enumerate the new name live.
    fn moved(&self, _ino: u64) {}
}

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
    /// (Idempotent; used by the examples/tests. The running daemon uses
    /// [`push_outbox`](Self::push_outbox) for incremental pushes.)
    pub fn push(provider: &SqliteProvider, client: &ServerClient, io: &dyn ChunkIO) -> Result<()> {
        for node in provider.export_tree()? {
            push_node(provider, client, io, &node)?;
        }
        Ok(())
    }

    /// Incremental push: drain the local outbox (dirty `node_id`s queued by FUSE
    /// writes), sending only what changed. Order is **parents-first, topologically** (via
    /// [`outbox_list_topo`](SqliteProvider::outbox_list_topo)) — NOT raw insertion order.
    /// Insertion order is unsafe because a move re-`enqueue`s the child but keeps its older
    /// outbox `id`, so a file moved into a dir created after it would otherwise be committed
    /// before its new parent, making a pulling device's `apply_remote` hit `NotFound` and
    /// abort the pull (the BUG #1 sync-stall). The SQLite lock is **never held across WAN
    /// I/O** — each provider call locks and releases on its own — so the metadata path stays
    /// local.
    ///
    /// **Per-node error isolation:** one node that fails to push (a transient server/network
    /// error, a conflict, a missing chunk) must NOT block the *other* queued nodes. A naive
    /// `?` on the first failure would abort the whole drain, leaving an edit/delete/rename
    /// queued behind a stuck sibling forever (the loop kept retrying the same failing node
    /// every tick and never reached the rest). Instead we attempt each entry independently: a
    /// success clears its outbox row; a failure is logged and **left queued** for the next
    /// tick's retry. The drain returns `Ok` even with failures (they're transient + retried);
    /// only an outbox read/clear DB error — a local-state problem — propagates.
    pub fn push_outbox(provider: &SqliteProvider, client: &ServerClient, io: &dyn ChunkIO) -> Result<()> {
        let entries = provider.outbox_list_topo()?;
        if !entries.is_empty() {
            eprintln!("push_outbox: {} queued entr{}", entries.len(), if entries.len() == 1 { "y" } else { "ies" });
        }
        for (node_id, op) in entries {
            let result: Result<()> = if op == "delete" {
                match client.delete_node(&node_id) {
                    Ok(_) => Ok(()),
                    // The node may never have reached the server (created then deleted
                    // before this drained). Treat "unknown" as already-gone.
                    Err(sync_client::Error::Server { status: 404, .. }) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            } else {
                // 'upsert'. If the node vanished between enqueue and now (a delete races
                // ahead), there's nothing to send — just drop the stale entry.
                match provider.sync_node(&node_id) {
                    Ok(Some(node)) => push_node(provider, client, io, &node),
                    Ok(None) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            };
            match result {
                Ok(()) => {
                    eprintln!("push_outbox: pushed node={node_id} op={op}");
                    provider.outbox_clear(&node_id)?;
                }
                // Leave the entry queued so the next tick retries it; don't let it block
                // the siblings still in this drain.
                Err(e) => {
                    eprintln!("push_outbox: node={node_id} op={op} failed (left queued, will retry): {e}");
                }
            }
        }
        Ok(())
    }

    /// Initial sync: apply the full-tree snapshot (paginated) to the local provider.
    pub fn pull_snapshot(provider: &SqliteProvider, client: &ServerClient) -> Result<i64> {
        let mut after = 0;
        let mut head;
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
    /// Pages through the delta — the server caps each response, so a device that's many
    /// changes behind (the offline-reconnect case) **must** loop or it silently skips
    /// every entry past the first page. Uses the default page size.
    pub fn pull_changes(provider: &SqliteProvider, client: &ServerClient, cursor: i64) -> Result<i64> {
        Self::pull_changes_paged(provider, client, cursor, 500)
    }

    /// [`pull_changes`](Self::pull_changes) with an explicit page size (for tests that
    /// want to force multi-page pulls cheaply). Loops `changes(cursor, page)` until the
    /// device is caught up, advancing the cursor to the **last applied entry's seq** each
    /// round — never to the global `head_seq`, which would jump past un-applied entries.
    pub fn pull_changes_paged(
        provider: &SqliteProvider,
        client: &ServerClient,
        cursor: i64,
        page: i64,
    ) -> Result<i64> {
        Self::pull_changes_paged_inval(provider, client, cursor, page, None)
    }

    /// [`pull_changes_paged`](Self::pull_changes_paged) that additionally invalidates the
    /// OS cache of any inode a pull repoints at new content (see [`Invalidate`]). The daemon
    /// + freshness paths pass the FUSE invalidator; the plain examples/tests pass `None`.
    pub fn pull_changes_paged_inval(
        provider: &SqliteProvider,
        client: &ServerClient,
        mut cursor: i64,
        page: i64,
        inval: Option<&dyn Invalidate>,
    ) -> Result<i64> {
        let mut checked_floor = false;
        // Upserts whose parent isn't present locally *yet* are buffered here and retried —
        // NOT applied immediately (which would `NotFound` and, in the old code, abort the
        // whole pull via `?`). Under a move-vs-recursive-delete interleaving a changelog
        // upsert can transiently reference a parent the device just swept; the parent's own
        // (re-establishing) upsert may arrive later in the same delta — possibly on a later
        // page — so we retry after each page and again at end-of-pull. Any still-parentless
        // node after the full delta is genuinely unreachable on the authoritative tree (its
        // ancestor was deleted there too); we drop it and lean on the full-tree reconcile
        // backstop as the correctness floor (BUG #2). Idempotent: a replayed upsert is an
        // in-place update, never a duplicate.
        let mut deferred: Vec<SyncNode> = Vec::new();

        // BUG #2 convergence face — the sticky reconcile flag. A pull can silently remove a
        // local node that carries un-synced local state (its outbox upsert is still pending):
        //   - `apply_remote`'s name-collision sweep displaces it when a remote upsert claims its
        //     `(parent, name)` — `ApplyOutcome::displaced_pending`; or
        //   - `apply_remote_delete`'s recursive subtree sweep removes it as a descendant of a
        //     deleted ancestor — `apply_remote_delete_signaled` returns `true`.
        // Either way the displaced node is typically STILL LIVE on the server (under its prior
        // name / new parent — the server keys nodes by node_id and never tombstoned it), but the
        // changelog delta can't re-add it locally: its queued outbox upsert pushes as a no-op
        // once the row is gone. So we must fall back to the full-tree reconcile (`pull_snapshot`,
        // the documented correctness floor) to re-establish it. We persist a `needs_resync` flag
        // (sticky across ticks) so the reconcile is retried until it converges — then clear it.
        //
        // Honor a flag set by a PREVIOUS tick up front.
        if provider.needs_resync().unwrap_or(false) {
            let head = Self::pull_snapshot(provider, client)?;
            let _ = provider.set_needs_resync(false);
            return Ok(head);
        }
        // Set when THIS pull clobbers a pending local node (see above).
        let mut needs_reconcile = false;
        loop {
            let resp = client.changes_paged(cursor, page)?;

            // Full-resync backstop: if the changelog was pruned past our cursor, the
            // delta has a hole (we'd silently skip the pruned entries). Detect it on the
            // first round via `oldest_seq` and fall back to a full snapshot instead. A
            // fresh device (cursor 0, nothing pruned → oldest_seq ≤ 1) never trips this.
            if !checked_floor {
                checked_floor = true;
                if resp.oldest_seq > cursor + 1 {
                    return Self::pull_snapshot(provider, client);
                }
            }

            if resp.changes.is_empty() {
                // Caught up. Final retry of any still-buffered upserts (a parent may have
                // landed in the last page); `dropped` is true if any remained unresolvable.
                let dropped = retry_deferred(provider, &mut deferred, inval, true);
                // A dropped (still-parentless) upsert OR a clobbered-pending displacement
                // means the changelog delta did not fully converge the local tree to the
                // server. Fall back to the documented reconcile floor so the missing/displaced
                // node is re-added (it's still live on the server). Mark dirty FIRST so the
                // reconcile is retried on the next tick if it can't fully converge yet (the
                // racing peer's push may not have landed); `pull_snapshot` clears it.
                if needs_reconcile || dropped {
                    let _ = provider.set_needs_resync(true);
                    let head = Self::pull_snapshot(provider, client)?;
                    let _ = provider.set_needs_resync(false);
                    return Ok(head);
                }
                return Ok(resp.head_seq.max(cursor));
            }
            for ch in &resp.changes {
                if apply_change(provider, ch, inval, &mut deferred)? {
                    needs_reconcile = true;
                }
            }
            // A parent applied this page may unblock a buffered child — retry now (mid-page:
            // unresolved nodes stay buffered for later pages, nothing dropped).
            retry_deferred(provider, &mut deferred, inval, false);
            // Advance only as far as we actually applied; loop for the next page.
            cursor = resp.changes.last().map(|c| c.seq).unwrap_or(cursor);
        }
    }

    /// Hydrate a file's **whole** content from the cloud into the local cache. The policy for
    /// a read that needs every byte (mmap, read-entire-file) and — critically — for an
    /// **open-for-write** (a random write seeds its staging buffer from the committed content,
    /// so a partial hydrate would leave the unfetched regions wrong). Equivalent to
    /// [`hydrate_range`](Self::hydrate_range) over `(0, u64::MAX)`.
    pub fn hydrate(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
    ) -> Result<()> {
        Self::hydrate_range(provider, client, io, node_id, 0, u64::MAX)
    }

    /// **Progressive / range hydration**: fetch only the chunks overlapping `[offset, offset +
    /// size)` — the subset a read at that range actually needs — instead of the whole file. A
    /// header read of a multi-GB file thus pulls a handful of chunks, not gigabytes.
    ///
    /// The range→chunk mapping is [`SqliteProvider::manifest_range_hashes`] (the manifest
    /// already records each chunk's offset/length); we then fetch only the *missing* ones via
    /// presigned GET, exactly as the whole-file path does. A subsequent
    /// [`Provider::read`](fs_provider::Provider::read) of that range assembles from the now-local
    /// chunks. `size == u64::MAX` (or any range covering EOF) degenerates to a full hydrate, so
    /// [`hydrate`](Self::hydrate) is just this with the whole-file range.
    ///
    /// Policy note: callers pick whole-vs-range by *read intent* — the read path passes the
    /// touched `(offset, size)`; write-open and mmap/read-whole paths call [`hydrate`](Self::hydrate).
    pub fn hydrate_range(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
        offset: u64,
        size: u64,
    ) -> Result<()> {
        let hashes = provider.manifest_range_hashes(node_id, offset, size)?;
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

    /// **Proactive (selective-sync-gated) hydration.** After a pull, eagerly hydrate the
    /// chunks of every file that is *in scope* for this device — i.e. the files
    /// [`SqliteProvider::proactive_hydration_targets`] returns (in-scope files with missing
    /// chunks). **Out-of-scope subtrees are skipped entirely**: they stay dehydrated
    /// placeholders and are fetched only on demand by [`hydrate`](Self::hydrate) (which is
    /// deliberately *not* gated, so an out-of-scope file still opens — graceful degradation,
    /// the OneDrive online-only model).
    ///
    /// This is the single place the subscription gate touches the hydration path; the
    /// per-file [`hydrate`](Self::hydrate) and the on-demand FUSE/CfAPI read hooks are
    /// unchanged. Per-file error isolation: one file that fails to hydrate (transient
    /// network) is logged and skipped, never blocking the rest — the next tick retries it.
    pub fn proactive_hydrate(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
    ) -> Result<()> {
        for node_id in provider.proactive_hydration_targets()? {
            if let Err(e) = Self::hydrate(provider, client, io, &node_id) {
                eprintln!("proactive_hydrate: node {node_id} failed (will retry next tick): {e}");
            }
        }
        Ok(())
    }

    /// **Subscribe** a subtree (by `node_id`) for proactive hydration on this device, then
    /// immediately hydrate the now-in-scope files. The local tree already holds the subtree's
    /// metadata (the full tree always syncs), so this only changes whether its bytes are kept
    /// resident.
    pub fn subscribe(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
    ) -> Result<()> {
        provider.subscribe(node_id)?;
        Self::proactive_hydrate(provider, client, io)
    }

    /// **Unsubscribe** a subtree (by `node_id`): stop proactively hydrating it and reclaim
    /// the bytes it was holding (dropping only chunks no in-scope file shares). The metadata
    /// stays (placeholders), so the subtree is still navigable and re-hydrates on demand.
    /// Returns the number of chunks reclaimed.
    pub fn unsubscribe(provider: &SqliteProvider, node_id: &str) -> Result<usize> {
        provider.unsubscribe(node_id)?;
        Ok(provider.dehydrate_subtree(node_id)?)
    }

    /// One whole-tree loop iteration: push the full tree, then pull remote changes.
    /// (Retained for the examples; the daemon uses [`sync_tick`](Self::sync_tick).)
    pub fn sync_once(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        cursor: i64,
    ) -> Result<i64> {
        Self::push(provider, client, io)?;
        Self::pull_changes(provider, client, cursor)
    }

    /// The daemon's loop body: drain the outbox (incremental push), then pull the
    /// changelog delta since `cursor`. Returns the new cursor to persist. This is the
    /// reusable tick the background sync thread calls on each interval.
    pub fn sync_tick(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        cursor: i64,
    ) -> Result<i64> {
        Self::sync_tick_inval(provider, client, io, cursor, None)
    }

    /// [`sync_tick`](Self::sync_tick) that invalidates the OS cache of any inode a pull
    /// repoints at new content (see [`Invalidate`]). The daemon's sync loop passes the FUSE
    /// invalidator so a same-size remote edit can't keep serving stale cached bytes.
    pub fn sync_tick_inval(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        cursor: i64,
        inval: Option<&dyn Invalidate>,
    ) -> Result<i64> {
        Self::push_outbox(provider, client, io)?;
        let new_cursor = Self::pull_changes_paged_inval(provider, client, cursor, 500, inval)?;
        // Selective sync: after applying the metadata delta, eagerly hydrate the *in-scope*
        // files (the gate lives in `proactive_hydration_targets`). Out-of-scope subtrees are
        // left as placeholders and hydrate on demand. Best-effort — a hydration failure is
        // logged inside and never fails the tick (the cursor still advances; next tick retries).
        if let Err(e) = Self::proactive_hydrate(provider, client, io) {
            eprintln!("sync_tick: proactive hydrate pass failed (will retry next tick): {e}");
        }
        Ok(new_cursor)
    }
}

/// Push a single node (dir → `mkdir`; file → commit_begin → upload missing chunks →
/// finalize). Shared by whole-tree [`push`](SyncEngine::push) and incremental
/// [`push_outbox`](SyncEngine::push_outbox).
fn push_node(
    provider: &SqliteProvider,
    client: &ServerClient,
    io: &dyn ChunkIO,
    node: &SyncNode,
) -> Result<()> {
    match node.kind {
        Kind::Dir => {
            client.mkdir(&node.node_id, node.parent_node_id.as_deref(), &node.name, node.mode, node.mtime)?;
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
                // The version this edit was based on — the server flags a conflict if it's
                // no longer current (a concurrent edit slipped in, e.g. while offline).
                base_version_id: node.version_id.clone(),
                size,
                manifest,
                mode: node.mode,
                mtime: node.mtime,
            })?;
            for hash in &begin.missing {
                let bytes = provider.chunk_get(hash)?;
                io.put(hash, &begin.put_urls, bytes)?;
            }
            let fin = client.commit_finalize(&commit_id)?;
            // Record the new server version so a follow-up edit bases on it (not the now
            // stale prior version, which would falsely conflict with our own push).
            provider.set_node_version(&node.node_id, &fin.version_id)?;
        }
    }
    Ok(())
}

/// Bridges the FUSE read path to the sync engine: on a chunk-cache miss the adapter
/// calls [`Hydrate::hydrate`], which fetches the file's chunks from the cloud via
/// presigned GET. Holds `Arc` clones shared with the mount + sync threads.
/// (FUSE only — macOS's `WebdavMount` uses an inline closure hydrator.)
#[cfg(target_os = "linux")]
pub struct EngineHydrator {
    provider: Arc<SqliteProvider>,
    client: ServerClient,
}

#[cfg(target_os = "linux")]
impl EngineHydrator {
    pub fn new(provider: Arc<SqliteProvider>, client: ServerClient) -> Self {
        Self { provider, client }
    }
}

#[cfg(target_os = "linux")]
impl EngineHydrator {
    /// Resolve `ino` → node_id (logging + swallowing the not-found / error cases the way the
    /// best-effort hydrate hooks do), shared by the whole-file and range entry points.
    fn node_id_or_log(&self, ino: u64) -> Option<String> {
        match self.provider.node_id_for(ino) {
            Ok(Some(id)) => Some(id),
            Ok(None) => None,
            Err(e) => {
                eprintln!("hydrate: node_id lookup for ino {ino} failed: {e}");
                None
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl fs_fuse::Hydrate for EngineHydrator {
    fn hydrate(&self, ino: u64) {
        let Some(node_id) = self.node_id_or_log(ino) else { return };
        let io = PresignedIO::new(&self.client);
        if let Err(e) = SyncEngine::hydrate(&self.provider, &self.client, &io, &node_id) {
            eprintln!("hydrate: ino {ino} (node {node_id}) failed: {e}");
        }
    }

    /// Range hydration: fetch only the chunks overlapping the read's `[offset, size)` (the
    /// header-of-a-huge-file case), not the whole file.
    fn hydrate_range(&self, ino: u64, offset: u64, size: u32) {
        let Some(node_id) = self.node_id_or_log(ino) else { return };
        let io = PresignedIO::new(&self.client);
        if let Err(e) =
            SyncEngine::hydrate_range(&self.provider, &self.client, &io, &node_id, offset, size as u64)
        {
            eprintln!("hydrate_range: ino {ino} (node {node_id}) off={offset} len={size} failed: {e}");
        }
    }
}

/// Active OS-cache invalidation over the FUSE kernel channel. Holds the session's
/// [`fuser::Notifier`] and, on a content-changing pull, calls `Notifier::inval_inode` so the
/// kernel drops its cached attrs + data for that inode (the next read re-fetches the new
/// manifest). This is what lets us preserve the real mtime instead of bumping it to force
/// a re-read.
///
/// The notifier only exists *after* the mount is up (`spawn_mount2` → `BackgroundSession`),
/// but this hook is wired into the freshness adapter + sync loop *before* the mount — a
/// construction cycle. We break it with a [`OnceLock`](std::sync::OnceLock): build the hook
/// empty, share it into both paths, then [`set_notifier`](EngineInvalidate::set_notifier)
/// once mounted. Before that (no inode is cached yet) `invalidate` is a harmless no-op.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct EngineInvalidate {
    notifier: Arc<std::sync::OnceLock<fuser::Notifier>>,
}

#[cfg(target_os = "linux")]
impl EngineInvalidate {
    pub fn new() -> Self {
        Self { notifier: Arc::new(std::sync::OnceLock::new()) }
    }
    /// Inject the FUSE notifier once the session is mounted (idempotent).
    pub fn set_notifier(&self, notifier: fuser::Notifier) {
        let _ = self.notifier.set(notifier);
    }
}

#[cfg(target_os = "linux")]
impl Invalidate for EngineInvalidate {
    fn invalidate(&self, ino: u64) {
        if let Some(n) = self.notifier.get() {
            // offset 0 + len 0 = invalidate the inode's attributes and all cached data.
            // fuser 0.17: inval_inode now takes a strongly-typed `INodeNo` (was a bare u64
            // in 0.14) and offset/len are `i64`.
            if let Err(e) = n.inval_inode(fuser::INodeNo(ino), 0, 0) {
                eprintln!("invalidate: inval_inode({ino}) failed: {e}");
            }
        }
    }
}

/// Freshness-on-access: on a file open, pull the changelog delta so the local tree
/// reflects the latest before serving (the read then hydrates the new manifest's chunks
/// on miss). Throttled so frequent opens don't hammer the server — the background loop +
/// nudge already keep us mostly current, so this only matters in the gap between ticks.
#[cfg(target_os = "linux")]
pub struct EngineFreshness {
    provider: Arc<SqliteProvider>,
    client: ServerClient,
    throttle: std::time::Duration,
    last: std::sync::Mutex<Option<std::time::Instant>>,
    /// Invalidate the OS cache for any inode this open-time pull repoints at new content —
    /// the open's own read would otherwise be served the stale cached bytes. Shares the one
    /// invalidator with the background sync loop so whichever pull applies a change first is
    /// the one that invalidates (the other then sees `content_changed == false`).
    inval: Option<Arc<dyn Invalidate>>,
}

#[cfg(target_os = "linux")]
impl EngineFreshness {
    pub fn new(provider: Arc<SqliteProvider>, client: ServerClient, inval: Option<Arc<dyn Invalidate>>) -> Self {
        Self { provider, client, throttle: std::time::Duration::from_millis(750), last: std::sync::Mutex::new(None), inval }
    }
}

/// Best-effort cross-device advisory lock over the server. Acquire on open-for-write,
/// release on close — both **non-blocking** (a background thread does the short-timeout
/// HTTP, so an offline open never waits; it just opens the local cached copy). The lock
/// is advisory: it lets connected clients avoid colliding, and conflict detection handles
/// whatever the lock can't (offline edits, ignored locks).
#[cfg(target_os = "linux")]
pub struct EngineRemoteLock {
    provider: Arc<SqliteProvider>,
    client: ServerClient,
}

#[cfg(target_os = "linux")]
impl EngineRemoteLock {
    pub fn new(provider: Arc<SqliteProvider>, client: ServerClient) -> Self {
        Self { provider, client }
    }
}

#[cfg(target_os = "linux")]
impl fs_fuse::RemoteLock for EngineRemoteLock {
    fn acquire(&self, ino: u64) {
        if let Ok(Some(node_id)) = self.provider.node_id_for(ino) {
            let client = self.client.clone();
            std::thread::spawn(move || {
                let _ = client.lock(&node_id); // best-effort; offline/held just no-ops
            });
        }
    }
    fn release(&self, ino: u64) {
        if let Ok(Some(node_id)) = self.provider.node_id_for(ino) {
            let client = self.client.clone();
            std::thread::spawn(move || {
                let _ = client.unlock(&node_id);
            });
        }
    }
}

#[cfg(target_os = "linux")]
impl fs_fuse::Freshness for EngineFreshness {
    fn refresh(&self) {
        {
            // Skip if we pulled very recently — the loop/nudge has us covered.
            let mut last = self.last.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < self.throttle) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let cursor = self.provider.cursor().unwrap_or(0);
        match SyncEngine::pull_changes_paged_inval(&self.provider, &self.client, cursor, 500, self.inval.as_deref()) {
            // Advance the cursor monotonically — the sync loop may also be advancing it
            // concurrently; a regression would just re-pull an already-applied (idempotent)
            // delta, so max() is the safe reconciliation.
            Ok(new) => {
                let cur = self.provider.cursor().unwrap_or(0);
                let _ = self.provider.set_cursor(cur.max(new));
            }
            Err(e) => eprintln!("freshness: catch-up pull failed: {e}"),
        }
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
        version_id: n.version_id.clone(),
        mode: n.mode.unwrap_or(if n.kind == "dir" { 0o755 } else { 0o644 }),
        // A server that predates mtime sync sends `None` — fall back to `now` so the kernel
        // still sees a fresh stamp (the old behavior); a real synced mtime is applied verbatim.
        mtime: n.mtime.unwrap_or_else(now_nanos),
    }
}

/// Apply one changelog entry to the local tree. The server soft-deletes, so a `delete`
/// change still carries the node (with `deleted: true`); treat either signal as a delete
/// and remove the node — and any descendants — locally. A *move* is not a delete: the
/// server models it as op `move` on the same stable node_id with a new parent/name, so it
/// falls through to `apply_remote` and updates the existing row in place (no duplicate).
/// Returns `true` if applying this change clobbered a local node that still had pending
/// (un-synced) outbox state — the caller must then reconcile (see `displaced_pending`).
fn apply_change(
    provider: &SqliteProvider,
    ch: &Change,
    inval: Option<&dyn Invalidate>,
    deferred: &mut Vec<SyncNode>,
) -> Result<bool> {
    // A `conflict` change records a divergent version that is NOT current — the node's
    // content is unchanged, so it must not be applied to the tree. It's surfaced to the
    // user via GET /v1/conflicts and resolved there.
    if ch.op == "conflict" {
        return Ok(false);
    }
    let deleted = ch.node.as_ref().is_some_and(|n| n.deleted);
    if ch.op == "delete" || deleted {
        // Resolve the node's ino BEFORE the delete removes its row — an on-demand-population
        // adapter (CfAPI) needs it to find + remove the on-disk placeholder (its ino→path index
        // still holds the pre-delete path). FUSE/WebDAV/WinFsp ignore `removed` (the row vanishing
        // is enough). The recursive subtree sweep is covered by removing the top node's dir on disk.
        let removed_ino = provider.ino_for_node_id(&ch.node_id).ok().flatten();
        // A recursive delete that sweeps a descendant carrying un-synced local state (e.g. a
        // node moved out of this subtree on the server, or a local move not yet pushed) is a
        // potential silent loss — signal a reconcile (the descendant is still live on the
        // server). The target node's own removal is authorized by this entry.
        let signaled = provider.apply_remote_delete_signaled(&ch.node_id)?;
        if let (Some(inval), Some(ino)) = (inval, removed_ino) {
            // The placeholder removal runs AFTER the tree delete so a concurrent read can't
            // re-create it; the adapter resolves the path from its (still-populated) index.
            inval.removed(ino);
        }
        return Ok(signaled);
    } else if let Some(sn) = change_to_sync(ch) {
        // Defer (don't abort) if the parent isn't present locally yet — see BUG #2. The
        // parent's own upsert may arrive later in this same delta; `retry_deferred` reapplies.
        return apply_upsert(provider, sn, inval, deferred);
    }
    Ok(false)
}

/// Apply one upsert `SyncNode`, deferring it (buffering for retry) instead of erroring if
/// its parent isn't present locally yet. Any *other* provider error still propagates. On a
/// successful apply, fires the appropriate OS-cache invalidation hook.
/// Returns `true` if the apply clobbered a pending local node (`displaced_pending`).
fn apply_upsert(
    provider: &SqliteProvider,
    sn: SyncNode,
    inval: Option<&dyn Invalidate>,
    deferred: &mut Vec<SyncNode>,
) -> Result<bool> {
    match provider.apply_remote(&sn) {
        Ok(outcome) => {
            if let Some(inval) = inval {
                if outcome.inserted {
                    // A brand-new node: create a fresh placeholder so it appears without the
                    // user re-navigating the directory (CfAPI on-demand population marks a dir
                    // fully populated after its first enumerate). No-op for FUSE/WebDAV/WinFsp.
                    inval.created(outcome.ino);
                } else if outcome.moved {
                    // A rename/move: the OLD-path placeholder is stale and the NEW one doesn't
                    // exist yet. The adapter removes the old (from its still-current index) and
                    // creates the new. A content change AND a move in the same upsert is handled
                    // by `moved` re-creating a fresh (dehydrated) placeholder, so a separate
                    // invalidate would be redundant. No-op for FUSE/WebDAV/WinFsp.
                    inval.moved(outcome.ino);
                } else if outcome.content_changed {
                    // An existing inode repointed at new content: drop the OS cache so the next
                    // read re-fetches (we no longer bump mtime to force this). FUSE-strong today.
                    inval.invalidate(outcome.ino);
                }
            }
            Ok(outcome.displaced_pending)
        }
        // Parent not present locally yet → buffer for retry within this pull rather than
        // aborting the whole pull. `apply_remote` only returns `NotFound` for a missing
        // parent (the upsert itself is keyed by stable node_id, so the node's own absence is
        // an INSERT, not a NotFound).
        Err(fs_provider::FsError::NotFound) => {
            deferred.push(sn);
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Re-apply buffered (parentless) upserts, looping until no further progress — a parent
/// applied this round may unblock a child. Returns `true` if any node remained unresolvable
/// at the fixed point.
///
/// `final_pass` distinguishes the two call sites:
///   - mid-page (`false`): leftover unresolvable nodes are RETAINED in `deferred` — their
///     parent may arrive on a LATER page — and `false` is returned (no decision yet).
///   - end-of-pull (`true`): leftovers are genuinely unresolvable from the delta; they are
///     dropped and `true` is returned so the caller reconciles from the full tree
///     (`pull_snapshot`, the correctness floor) rather than silently losing them (BUG #2).
fn retry_deferred(
    provider: &SqliteProvider,
    deferred: &mut Vec<SyncNode>,
    inval: Option<&dyn Invalidate>,
    final_pass: bool,
) -> bool {
    if deferred.is_empty() {
        return false;
    }
    let mut progress = true;
    while progress && !deferred.is_empty() {
        progress = false;
        let pending = std::mem::take(deferred);
        for sn in pending {
            match provider.apply_remote(&sn) {
                Ok(outcome) => {
                    progress = true;
                    if let Some(inval) = inval {
                        if outcome.inserted {
                            inval.created(outcome.ino);
                        } else if outcome.moved {
                            inval.moved(outcome.ino);
                        } else if outcome.content_changed {
                            inval.invalidate(outcome.ino);
                        }
                    }
                }
                Err(fs_provider::FsError::NotFound) => deferred.push(sn),
                // A non-parent error on a deferred node: log + drop (don't poison the pull).
                Err(e) => eprintln!("retry_deferred: dropping node after error: {e}"),
            }
        }
    }
    if !final_pass {
        // Mid-page: keep unresolved nodes buffered for a later page; no drop, no signal.
        return false;
    }
    // Whatever remains is unresolvable from the delta alone — drop it and tell the caller to
    // reconcile (don't silently lose it: it may still be live on the authoritative tree).
    let dropped = !deferred.is_empty();
    deferred.clear();
    dropped
}

fn change_to_sync(ch: &Change) -> Option<SyncNode> {
    let view = ch.node.as_ref()?;
    // Deletes are applied by the caller (`apply_remote_delete`); never reconstruct a
    // tombstoned node here.
    if ch.op == "delete" || view.deleted {
        return None;
    }
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
        version_id: ch.version.as_ref().map(|v| v.version_id.clone()),
        mode: view.mode.unwrap_or(if view.kind == "dir" { 0o755 } else { 0o644 }),
        mtime: view.mtime.unwrap_or_else(now_nanos),
    })
}

/// Wall-clock nanoseconds — the fallback mtime when a (pre-mtime) server omits one, so the
/// kernel still gets a fresh stamp. Mirrors `fs_provider::now_nanos` (private there).
fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod apply_tests {
    //! The pull-apply → `Invalidate`-seam wiring (the third live CfAPI bug: deletes + renames
    //! never told the receiving on-demand-population adapter to remove the stale placeholder).
    //! These drive `apply_change` directly with a recording mock, so the engine-side dispatch is
    //! verified on any host (the CfAPI FFI that actually removes the on-disk placeholder is
    //! Windows-only and verified live).
    use super::*;
    use fs_provider::{ChunkConfig, LocalChunkStore};
    use std::sync::Mutex;
    use sync_client::NodeView;

    const TINY: ChunkConfig = ChunkConfig { min: 64, avg: 256, max: 1024 };

    fn provider() -> SqliteProvider {
        SqliteProvider::in_memory(Box::new(LocalChunkStore::temp().unwrap()), TINY).unwrap()
    }

    #[derive(Default)]
    struct RecordingInval {
        created: Mutex<Vec<u64>>,
        invalidated: Mutex<Vec<u64>>,
        removed: Mutex<Vec<u64>>,
        moved: Mutex<Vec<u64>>,
    }
    impl Invalidate for RecordingInval {
        fn invalidate(&self, ino: u64) {
            self.invalidated.lock().unwrap().push(ino);
        }
        fn created(&self, ino: u64) {
            self.created.lock().unwrap().push(ino);
        }
        fn removed(&self, ino: u64) {
            self.removed.lock().unwrap().push(ino);
        }
        fn moved(&self, ino: u64) {
            self.moved.lock().unwrap().push(ino);
        }
    }

    fn upsert_change(node_id: &str, parent: Option<&str>, name: &str) -> Change {
        Change {
            seq: 0,
            op: "upsert".into(),
            node_id: node_id.into(),
            node: Some(NodeView {
                node_id: node_id.into(),
                parent_node_id: parent.map(|s| s.into()),
                name: name.into(),
                kind: "file".into(),
                deleted: false,
                mode: Some(0o644),
                mtime: Some(0),
            }),
            manifest: None,
            version: None,
        }
    }

    fn delete_change(node_id: &str) -> Change {
        Change { seq: 0, op: "delete".into(), node_id: node_id.into(), node: None, manifest: None, version: None }
    }

    /// A pulled `delete` must resolve the node's ino BEFORE removing the row and fire
    /// `removed(ino)` so an on-demand adapter can drop the stale on-disk placeholder.
    #[test]
    fn delete_fires_removed_with_the_node_ino() {
        let p = provider();
        let mut deferred = Vec::new();
        let inval = RecordingInval::default();

        // Seed a node via a pulled upsert, then look up its ino.
        apply_change(&p, &upsert_change("N", None, "a.txt"), Some(&inval), &mut deferred).unwrap();
        let ino = p.ino_for_node_id("N").unwrap().unwrap();
        assert_eq!(*inval.created.lock().unwrap(), vec![ino], "the insert fired created");

        // Now delete it — removed(ino) must fire with the SAME ino, and the row is gone.
        apply_change(&p, &delete_change("N"), Some(&inval), &mut deferred).unwrap();
        assert_eq!(*inval.removed.lock().unwrap(), vec![ino], "delete fired removed with the node's ino");
        assert!(p.ino_for_node_id("N").unwrap().is_none(), "node removed from the tree");
    }

    /// A pulled upsert that relocates an EXISTING node (new parent/name, same node_id) must fire
    /// `moved(ino)` — not `created`/`invalidate` — so the adapter removes the OLD-path placeholder
    /// and creates the NEW one. (A plain insert fires `created`; an in-place re-upsert fires
    /// neither.)
    #[test]
    fn rename_and_move_fire_moved_not_created_or_invalidate() {
        let p = provider();
        let mut deferred = Vec::new();
        let inval = RecordingInval::default();

        apply_change(&p, &upsert_change("N", None, "old.txt"), Some(&inval), &mut deferred).unwrap();
        let ino = p.ino_for_node_id("N").unwrap().unwrap();

        // Rename within the root: a move.
        apply_change(&p, &upsert_change("N", None, "new.txt"), Some(&inval), &mut deferred).unwrap();
        // Move into a subdir (seed the dir first via its own insert).
        apply_change(
            &p,
            &Change { op: "upsert".into(), ..{
                let mut c = upsert_change("D", None, "d");
                if let Some(n) = c.node.as_mut() { n.kind = "dir".into(); }
                c
            } },
            Some(&inval),
            &mut deferred,
        )
        .unwrap();
        apply_change(&p, &upsert_change("N", Some("D"), "new.txt"), Some(&inval), &mut deferred).unwrap();

        let moved = inval.moved.lock().unwrap().clone();
        assert_eq!(moved, vec![ino, ino], "rename + reparent each fired moved(ino)");
        // The relocations must NOT have been misreported as content invalidations.
        assert!(inval.invalidated.lock().unwrap().is_empty(), "a move is not a content invalidate");
        // created fired only for the two INSERTs (N's first appearance + the dir D), never for a move.
        assert_eq!(inval.created.lock().unwrap().len(), 2, "created only for the two inserts");
    }
}
