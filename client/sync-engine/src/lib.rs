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
use sha2::{Digest, Sha256};
use sync_client::{Change, ChunkRef, CommitBegin, ServerClient, TreeNode};

mod daemon;
pub use daemon::{DaemonConfig, Debounce, WebdavMount, DEFAULT_HYDRATE_CONCURRENCY, DEFAULT_POLL};

/// Client-side bandwidth throttling: a [`ThrottledChunkIO`](throttle::ThrottledChunkIO)
/// decorator that token-bucket rate-limits the bytes any [`ChunkIO`] transfers (separate
/// up/down caps; unlimited by default). See `docs/research-sync-competitors.md` §4 #6.
pub mod throttle;
#[cfg(target_os = "linux")]
pub use daemon::Daemon;
#[cfg(target_os = "windows")]
pub use daemon::{CfApiMount, WinfspMount};
pub use throttle::{RateLimiter, ThrottledChunkIO};

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
    /// A chunk fetched from remote object storage failed its content-address check: the
    /// sha256 of the bytes that came back is not the hash they were fetched for. Means the
    /// storage returned corrupt/truncated/wrong bytes (bit rot, a partial download, a stale
    /// or wrong object). The hydrate is failed all-or-nothing and the bad bytes are NEVER
    /// written into the local cache, so a read can never serve corrupt content. `expected` is
    /// the manifest hash; `got` is the sha256 actually computed over the returned bytes.
    #[error("chunk corrupt: expected {expected}, got {got} (storage returned bytes that don't match the content hash)")]
    ChunkCorrupt { expected: String, got: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The outcome of [`SyncEngine::ensure_available_offline`] — a one-shot "download this
/// folder/file for offline use" pass over a subtree.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OfflineSummary {
    /// How many files had missing chunks fetched this call (a re-run over an already-resident
    /// subtree reports 0 — idempotency is observable).
    pub files_hydrated: usize,
    /// Total bytes actually fetched this call (the previously-missing chunks).
    pub bytes_hydrated: u64,
    /// Per-file failures as `(path, error)` — a file that couldn't be hydrated (transient
    /// network, a missing chunk). The rest of the subtree is still attempted; the caller
    /// should re-run if this is non-empty. Empty ⇒ the whole subtree is now resident.
    pub failures: Vec<(String, String)>,
}

impl OfflineSummary {
    /// Whether the pass completed with no per-file failures (the subtree is now resident).
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The result of [`SyncEngine::offline_readiness`] — the pre-disconnect "is this safe to
/// take offline?" report, computed with **no network** from the local tree + chunk cache.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OfflineReadiness {
    /// Total files in the node/subtree.
    pub total_files: usize,
    /// Files whose every chunk is already local (fully downloaded).
    pub ready_files: usize,
    /// Distinct chunks across the subtree (deduped — a shared chunk counts once).
    pub total_chunks: usize,
    /// Distinct chunks already present in the local cache.
    pub present_chunks: usize,
    /// The files **not yet fully downloaded** (path + node_id) — what's still online-only.
    pub missing_files: Vec<fs_provider::SubtreeFile>,
}

impl OfflineReadiness {
    /// `true` iff every chunk of every file is local — i.e. the node/subtree can be opened
    /// and read with no network. (An empty subtree — no files — is trivially ready.)
    pub fn is_ready(&self) -> bool {
        self.present_chunks == self.total_chunks && self.missing_files.is_empty()
    }
}

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
///
/// **`Send + Sync`** is required so the engine can move a file's chunks **concurrently** over
/// a bounded worker pool (see [`hydrate_concurrent`](SyncEngine::hydrate_concurrent) /
/// parallel push): every production transport already is (`PresignedIO` borrows a `Send +
/// Sync` `ServerClient`; `SharedDirIO` is a path; `ThrottledChunkIO` adds `Arc<RateLimiter>`),
/// and a test transport just needs interior mutability via `Mutex`/atomics rather than
/// `RefCell`.
pub trait ChunkIO: Send + Sync {
    /// Upload `hash`'s bytes. `put_urls` are from `commit_begin` (presigned PUTs).
    fn put(&self, hash: &str, put_urls: &HashMap<String, String>, bytes: Vec<u8>) -> Result<()>;
    /// Download `hash`'s bytes. `get_url` is from `/chunks/download` (or `None`).
    fn get(&self, hash: &str, get_url: Option<&str>) -> Result<Vec<u8>>;

    /// Whether `get`/`put` need the per-chunk presigned URLs from the server. The production
    /// [`PresignedIO`] does (default `true`); a content-addressed local store ([`SharedDirIO`])
    /// ignores the URL and overrides this to `false`, which lets a hydrate **skip the
    /// `/chunks/download` presign round-trip entirely** (and lets the engine be exercised
    /// hermetically, with no server, in tests).
    fn needs_urls(&self) -> bool {
        true
    }
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
    /// Keyed by content hash; the presigned URL is ignored, so skip the presign call.
    fn needs_urls(&self) -> bool {
        false
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
        let url = put_urls
            .get(hash)
            .ok_or_else(|| Error::NoUrl(hash.to_string()))?;
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
    pub fn push_outbox(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
    ) -> Result<()> {
        // No debounce by default — drain everything ready (window 0 == disabled). The daemon
        // loop calls `push_outbox_debounced` with its configured window.
        Self::push_outbox_debounced(provider, client, io, 0, 0)
    }

    /// [`push_outbox`](Self::push_outbox) with a **G5 debounce/coalesce window**. An `upsert`
    /// that is still inside its debounce window (last touched < `window_ms` ago AND first
    /// queued < `max_age_ms` ago) is **left queued** this tick so a flurry of rapid saves to
    /// one big CAD file collapses into a single push of the final content (see
    /// `docs/torn-writes-and-batching.md` §4). Deletes/renames are never debounced. With
    /// `window_ms == 0` this is exactly the eager drain. On unmount/quiesce the caller passes
    /// `window_ms == 0` (or [`flush_outbox`](Self::flush_outbox)) to force the final push.
    pub fn push_outbox_debounced(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        window_ms: i64,
        max_age_ms: i64,
    ) -> Result<()> {
        let entries = provider.outbox_ready_topo(window_ms, max_age_ms)?;
        // Snapshot each entry's `enqueued_at` NOW, at drain time — before any WAN I/O. The
        // clear after a successful push is gated on this captured value so a mount write that
        // re-enqueues the same node mid-upload (bumping its `enqueued_at`) is NOT thrown away
        // by the clear (see `SqliteProvider::outbox_clear_upto`).
        let enqueued_at: std::collections::HashMap<String, i64> = provider
            .outbox_list_timed()?
            .into_iter()
            .map(|(nid, _, enq, _)| (nid, enq))
            .collect();
        if !entries.is_empty() {
            eprintln!(
                "push_outbox: {} queued entr{}",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" }
            );
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
                    // Clear only up to the timestamp we captured at drain time: a re-enqueue
                    // during the WAN push (newer `enqueued_at`) survives for the next tick.
                    match enqueued_at.get(&node_id) {
                        Some(&enq) => provider.outbox_clear_upto(&node_id, enq)?,
                        // No captured stamp (shouldn't happen — it came from the same snapshot);
                        // fall back to the unconditional clear rather than leaking the row.
                        None => provider.outbox_clear(&node_id)?,
                    }
                }
                // Leave the entry queued so the next tick retries it; don't let it block
                // the siblings still in this drain.
                Err(e) => {
                    eprintln!(
                        "push_outbox: node={node_id} op={op} failed (left queued, will retry): {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Force-drain the **entire** outbox ignoring any debounce window — the
    /// flush-on-close/unmount path so a debounced final edit can't be left un-pushed when
    /// the daemon stops. Equivalent to `push_outbox` (window 0).
    pub fn flush_outbox(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
    ) -> Result<()> {
        Self::push_outbox_debounced(provider, client, io, 0, 0)
    }

    /// Initial sync / reconcile floor: apply the full-tree snapshot (paginated) to the local
    /// provider, then **sweep** any local node the snapshot no longer holds live.
    ///
    /// The sweep is essential (BUG #4): `pull_snapshot` is also the reconcile backstop for a
    /// device that full-resyncs (its cursor fell below the server's pruned `oldest_seq`, or a
    /// racing pull dropped a node). Applying live nodes *only* — and jumping the cursor to head,
    /// skipping the delete changelog entries — leaves every file that was deleted while this
    /// device was away as a **ghost**: still present locally, absent from the server. Editing a
    /// ghost pushes it back and re-creates it on every device (a resurrection). So after applying
    /// the snapshot we remove every local node whose `node_id` isn't in the live set — mirroring
    /// the fault harness's `reconcile_full` ("SWEEP FIRST"). We PRESERVE nodes that still carry a
    /// pending outbox entry (never-pushed local creates/edits): those aren't ghosts, they're
    /// local intent the server hasn't seen yet, and the queued push will surface them.
    pub fn pull_snapshot(provider: &SqliteProvider, client: &ServerClient) -> Result<i64> {
        let mut after = 0;
        let mut head;
        let mut live_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let tree = client.tree(after)?;
            head = tree.head_seq;
            for n in &tree.nodes {
                live_ids.insert(n.node_id.clone());
                provider.apply_remote(&tree_node_to_sync(n))?;
            }
            match tree.next_after_id {
                Some(id) if !tree.nodes.is_empty() => after = id,
                _ => break,
            }
        }
        Self::sweep_absent(provider, &live_ids)?;
        Ok(head)
    }

    /// Remove every local node whose `node_id` is NOT in `live_ids` (the server's live set from
    /// a full snapshot), EXCEPT nodes that still carry a pending outbox entry (never-pushed local
    /// creates/edits — local intent, not a ghost). The reconcile sweep behind
    /// [`pull_snapshot`](Self::pull_snapshot); factored out so it's unit-testable without a
    /// server. Deletes are recursive + idempotent (`apply_remote_delete`), and never touch the
    /// outbox (inbound change).
    pub fn sweep_absent(
        provider: &SqliteProvider,
        live_ids: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let pending: std::collections::HashSet<String> = provider
            .outbox_list()?
            .into_iter()
            .map(|(nid, _)| nid)
            .collect();
        for n in provider.export_tree()? {
            if !live_ids.contains(&n.node_id) && !pending.contains(&n.node_id) {
                provider.apply_remote_delete(&n.node_id)?;
            }
        }
        Ok(())
    }

    /// Incremental: apply changelog entries since `cursor`. Returns the new cursor.
    /// Pages through the delta — the server caps each response, so a device that's many
    /// changes behind (the offline-reconnect case) **must** loop or it silently skips
    /// every entry past the first page. Uses the default page size.
    pub fn pull_changes(
        provider: &SqliteProvider,
        client: &ServerClient,
        cursor: i64,
    ) -> Result<i64> {
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
                // landed in the last page). `RetryOutcome::diverged` folds BOTH of that pass's
                // signals — a node left unresolvable and dropped, AND a displacement one of its
                // applies caused. The second was discarded entirely until 2026-08-19.
                let retry = retry_deferred(provider, &mut deferred, inval, true);
                // A dropped (still-parentless) upsert OR a clobbered-pending displacement
                // means the changelog delta did not fully converge the local tree to the
                // server. Fall back to the documented reconcile floor so the missing/displaced
                // node is re-added (it's still live on the server). Mark dirty FIRST so the
                // reconcile is retried on the next tick if it can't fully converge yet (the
                // racing peer's push may not have landed); `pull_snapshot` clears it.
                if raise_reconcile_floor(provider, needs_reconcile || retry.diverged()) {
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
            // unresolved nodes stay buffered for later pages, nothing dropped). Its return was
            // DISCARDED until 2026-08-19, which lost a mid-page displacement outright: `dropped`
            // is false here by design (leftovers are retained, not decided), but a
            // `displaced_pending` is a real, permanent divergence and must survive to the
            // end-of-pull decision above.
            if retry_deferred(provider, &mut deferred, inval, false).diverged() {
                needs_reconcile = true;
            }
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

    /// [`hydrate`](Self::hydrate) over a **bounded worker pool** of `concurrency` threads —
    /// the whole-file case of [`hydrate_range_concurrent`](Self::hydrate_range_concurrent).
    pub fn hydrate_concurrent(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
        concurrency: usize,
    ) -> Result<()> {
        Self::hydrate_range_concurrent(provider, client, io, node_id, 0, u64::MAX, concurrency)
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
        // The serial path is just the concurrent one with a pool of 1.
        Self::hydrate_range_concurrent(provider, client, io, node_id, offset, size, 1)
    }

    /// **Parallel range hydration.** Fetch the chunks overlapping `[offset, offset + size)`
    /// over a **bounded worker pool** of `concurrency` threads instead of one at a time, then
    /// write each into the local content-addressed cache. The serial round-trip latency to
    /// object storage (~80 ms/chunk to R2) is the bottleneck a 100-chunk file pays serially
    /// (~10 s); overlapping `P` fetches cuts that to ≈ serial/`P` (see `docs/r2-migration.md`
    /// §5). `concurrency <= 1` (or a single missing chunk) runs inline with no thread spawn.
    ///
    /// **Correctness is identical to the serial path:**
    /// - *Order doesn't matter for hydration* — chunks are content-addressed and cached by
    ///   hash; the file is assembled in manifest order later by [`Provider::read`]. Each worker
    ///   independently fetches a hash and writes it to the cache (the chunk store is `Send +
    ///   Sync`; the provider's cache writes don't take the SQLite tree lock).
    /// - *All-or-nothing* — if ANY chunk fails (or a worker panics), the whole call returns
    ///   `Err` and we never claim success on a partially-hydrated file (a read would otherwise
    ///   serve wrong/short bytes). The first error wins; remaining work is abandoned.
    /// - *Dedup / idempotency* — already-cached chunks are filtered out up front, so a repeat
    ///   hydrate fetches nothing; a duplicate hash in the range is fetched once.
    /// - *Throttle* — `io` carries its own (shared `Arc<RateLimiter>`) token bucket, so all
    ///   workers draw from one global bucket and the aggregate download rate still respects the
    ///   cap (it is NOT multiplied by `P`).
    ///
    /// The presign step (`/chunks/download`) is **batched once** for all missing chunks (one
    /// round-trip), exactly as before — only the byte transfers are parallelized.
    pub fn hydrate_range_concurrent(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
        offset: u64,
        size: u64,
        concurrency: usize,
    ) -> Result<()> {
        let hashes = provider.manifest_range_hashes(node_id, offset, size)?;
        let missing: Vec<String> = hashes
            .into_iter()
            .filter(|h| !provider.has_chunk(h))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        // One batched presign for every missing chunk (unchanged) — unless the transport is a
        // content-addressed local store that ignores URLs (SharedDirIO / hermetic tests).
        let urls = if io.needs_urls() {
            client.download_urls(&missing)?.urls
        } else {
            HashMap::new()
        };
        // Each item fetches → VERIFIES the bytes hash to `h` → writes its chunk to the local
        // cache → drops the bytes (so we hold at most `P` chunk buffers at once). Order is
        // irrelevant; assembly is by hash. The verify is the integrity gate: object storage
        // (R2/SeaweedFS) returning a corrupt/truncated/wrong object would otherwise be cached
        // and served as file content undetected. We check here (where REMOTE bytes enter the
        // cache), not on every local read (hot path) nor on the local-write/commit path (those
        // bytes were just hashed to derive the key). A mismatch fails the whole hydrate
        // (all-or-nothing) so a read never serves corrupt content.
        parallel_for_each(&missing, concurrency, |h| {
            let bytes = io.get(h, urls.get(h).map(String::as_str))?;
            verify_chunk(h, &bytes)?;
            provider.chunk_put(h, &bytes)?;
            Ok(())
        })
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

    /// **Pin a subtree for offline + download it now.** Records the pin (forcing the subtree
    /// in scope for proactive hydration across pulls — see [`SqliteProvider::pin`]) and then
    /// immediately makes it available offline via [`ensure_available_offline`](Self::ensure_available_offline).
    /// The combination is the "make this folder available offline" action: it both downloads
    /// the current contents AND keeps the folder resident as new files arrive (the daemon's
    /// proactive-hydration pass honors the pin every tick). Returns the download summary.
    pub fn pin(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
        concurrency: usize,
    ) -> Result<OfflineSummary> {
        provider.pin(node_id)?;
        Self::ensure_available_offline(provider, client, io, node_id, concurrency)
    }

    /// **Unpin a subtree**, optionally reclaiming its cached bytes. Removes the pin so the
    /// subtree stops being force-kept resident (it reverts to inheriting its ancestry / the
    /// device default). When `reclaim` is true, the now out-of-scope subtree's cached chunks
    /// are dropped (dedup-safe — a chunk a still-in-scope file shares is never dropped),
    /// returning the number reclaimed; when false, returns 0 and leaves the bytes (they'll be
    /// reclaimed by a later dehydrate / GC). The tree + manifests stay, so the subtree is still
    /// navigable and re-hydrates on demand.
    pub fn unpin(provider: &SqliteProvider, node_id: &str, reclaim: bool) -> Result<usize> {
        provider.unpin(node_id)?;
        if reclaim {
            Ok(provider.dehydrate_subtree(node_id)?)
        } else {
            Ok(0)
        }
    }

    /// **Make a node available offline.** Fully hydrate `node_id` and — if it's a directory —
    /// every descendant file's chunks, so the whole subtree is resident on local disk before
    /// the device goes offline (the workshop / makerspace / client-site scenario). Each file
    /// is hydrated over the bounded worker pool (`concurrency` threads per file). Idempotent:
    /// already-local chunks are skipped, so a re-run over a resident subtree fetches nothing.
    ///
    /// Returns an [`OfflineSummary`]: how many files were (partly or wholly) hydrated, the
    /// bytes brought down, and any per-file [`failures`](OfflineSummary::failures). **Per-file
    /// error isolation:** one file that fails (transient network, a missing chunk) is recorded
    /// in `failures` and does NOT abort the rest — so a single bad file can't leave the rest of
    /// the offline set un-downloaded. The caller should re-run (or check
    /// [`offline_readiness`](Self::offline_readiness)) if `failures` is non-empty.
    ///
    /// This is the engine primitive behind a future "make available offline" app button / the
    /// `pin` CLI command. It does NOT itself set a subscription rule — pair it with
    /// [`subscribe`](Self::subscribe) (or the pin flag) to keep the subtree resident across
    /// pulls; on its own it's a one-shot "download these now".
    pub fn ensure_available_offline(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        node_id: &str,
        concurrency: usize,
    ) -> Result<OfflineSummary> {
        let mut summary = OfflineSummary::default();
        for file in provider.subtree_files(node_id)? {
            // Count only the bytes actually fetched this call (the still-missing chunks), so a
            // re-run over a resident subtree reports 0 bytes (idempotency is observable).
            let hashes = provider.node_manifest_hashes(&file.node_id)?;
            let missing: Vec<String> = hashes
                .iter()
                .filter(|h| !provider.has_chunk(h))
                .cloned()
                .collect();
            if missing.is_empty() {
                continue; // already fully resident — nothing to do
            }
            match Self::hydrate_concurrent(provider, client, io, &file.node_id, concurrency) {
                Ok(()) => {
                    summary.files_hydrated += 1;
                    // Sum the bytes of the chunks that were missing (now fetched). Read each
                    // from the now-populated cache; a chunk that somehow still isn't present
                    // (shouldn't happen on Ok) contributes 0 rather than erroring the summary.
                    for h in &missing {
                        if let Ok(bytes) = provider.chunk_get(h) {
                            summary.bytes_hydrated += bytes.len() as u64;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("ensure_available_offline: {} failed: {e}", file.path);
                    summary.failures.push((file.path.clone(), e.to_string()));
                }
            }
        }
        Ok(summary)
    }

    /// **Offline-readiness check — the pre-disconnect "are these files safe to take offline?"**
    /// Without any network, report whether `node_id` (a file or a whole subtree) is fully
    /// resident on local disk: total vs locally-present chunks, how many files are completely
    /// local, and the list of files **not yet fully downloaded**. A file/subtree is ready iff
    /// every chunk of every descendant file is in the local cache
    /// ([`is_ready`](OfflineReadiness::is_ready)).
    ///
    /// Takes only the provider — **no client, no `ChunkIO`** — so it is structurally incapable
    /// of touching the network (it consults the local SQLite tree + chunk store only,
    /// `node_manifest_hashes` + `has_chunk`). That's the point: you run it right before you
    /// pull the plug, possibly already on a flaky connection.
    pub fn offline_readiness(provider: &SqliteProvider, node_id: &str) -> Result<OfflineReadiness> {
        let mut r = OfflineReadiness::default();
        // Distinct chunks across the subtree (content-addressed + deduped) so a chunk shared by
        // two files counts once — present_chunks/total_chunks then read as a true coverage %.
        let mut total: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
        for file in provider.subtree_files(node_id)? {
            r.total_files += 1;
            let hashes = provider.node_manifest_hashes(&file.node_id)?;
            let mut all_local = true;
            for h in &hashes {
                total.insert(h.clone());
                if provider.has_chunk(h) {
                    present.insert(h.clone());
                } else {
                    all_local = false;
                }
            }
            if all_local {
                r.ready_files += 1;
            } else {
                r.missing_files.push(file);
            }
        }
        r.total_chunks = total.len();
        r.present_chunks = present.len();
        Ok(r)
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
        Self::sync_tick_debounced(provider, client, io, cursor, inval, 0, 0)
    }

    /// [`sync_tick_inval`](Self::sync_tick_inval) with the **G5 debounce window** applied to
    /// the push leg. The daemon loop passes its configured `(window_ms, max_age_ms)` so rapid
    /// successive saves to one file coalesce into a single push. The pull leg is unchanged
    /// (pulls are always applied promptly). `window_ms == 0` disables the debounce.
    pub fn sync_tick_debounced(
        provider: &SqliteProvider,
        client: &ServerClient,
        io: &dyn ChunkIO,
        cursor: i64,
        inval: Option<&dyn Invalidate>,
        window_ms: i64,
        max_age_ms: i64,
    ) -> Result<i64> {
        Self::push_outbox_debounced(provider, client, io, window_ms, max_age_ms)?;
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

/// Verify a fetched chunk is content-addressed-correct: `sha256(bytes)` (hex) must equal the
/// `expected` hash it was fetched for. This is the SAME hash function the chunk addressing uses
/// everywhere — `fs-provider`'s commit path (`hex(Sha256::digest(bytes))`) and `sync-client`'s
/// `manifest_id` — so an honest chunk always matches and only genuine storage corruption (bit
/// rot / a truncated download / a wrong object) is rejected. Returns [`Error::ChunkCorrupt`] on
/// mismatch; the caller then never caches the bad bytes and fails the hydrate.
fn verify_chunk(expected: &str, bytes: &[u8]) -> Result<()> {
    let got = hex::encode(Sha256::digest(bytes));
    if got != expected {
        return Err(Error::ChunkCorrupt {
            expected: expected.to_string(),
            got,
        });
    }
    Ok(())
}

/// Default concurrency for parallel chunk **uploads** during a push (mirrors
/// [`daemon::DEFAULT_HYDRATE_CONCURRENCY`] on the download side). A push commits one file at a
/// time, so this bounds the in-flight PUTs of that file's missing chunks.
const DEFAULT_PUSH_CONCURRENCY: usize = 8;

/// Run `f` over every item of `items` across a **bounded worker pool** of `concurrency`
/// threads, with all-or-nothing error + panic semantics. The single concurrency primitive
/// shared by parallel hydrate (chunk GETs) and parallel push (chunk PUTs).
///
/// Guarantees:
/// - **Bounded:** at most `min(concurrency, items.len())` items run at once; `<= 1` (or a
///   single item) runs **inline** with no thread spawn.
/// - **All-or-nothing:** the FIRST item to error stops new work being claimed (a cooperative
///   `failed` flag) and that error is returned; on success every item ran.
/// - **Panic-safe:** an item closure that panics is caught and surfaced as an `Err`, never a
///   parent-thread unwind or a hang (which a raw `thread::scope` re-raise would be).
/// - **Borrow-friendly:** uses `thread::scope`, so `f` may borrow non-`'static` state (the
///   provider, the io, the URL map) without `Arc` cloning. `f` must be `Sync` (shared `&f`).
fn parallel_for_each<T, F>(items: &[T], concurrency: usize, f: F) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    if items.is_empty() {
        return Ok(());
    }
    let workers = concurrency.max(1).min(items.len());
    // One item / no real pool → run inline (preserves the exact serial behavior + error).
    if workers <= 1 {
        for it in items {
            f(it)?;
        }
        return Ok(());
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_err: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);
    let n = items.len();

    // Run one item, catching a panic so a misbehaving closure fails the batch as an `Err`
    // rather than unwinding into the parent (which `thread::scope` would otherwise re-raise).
    let run_one = |idx: usize| -> Result<()> {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&items[idx])));
        match caught {
            Ok(r) => r,
            Err(_) => Err(Error::Io(std::io::Error::other(format!(
                "parallel worker panicked on item {idx}"
            )))),
        }
    };

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let failed = &failed;
            let first_err = &first_err;
            let run_one = &run_one;
            scope.spawn(move || loop {
                // Stop claiming work once any worker has failed (cooperative early-out).
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= n {
                    return;
                }
                if let Err(e) = run_one(idx) {
                    failed.store(true, Ordering::Relaxed);
                    let mut slot = first_err.lock().unwrap_or_else(|p| p.into_inner());
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                    return;
                }
            });
        }
    });

    if let Some(e) = first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    Ok(())
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
            client.mkdir(
                &node.node_id,
                node.parent_node_id.as_deref(),
                &node.name,
                node.mode,
                node.mtime,
            )?;
        }
        Kind::Symlink => {
            // A symlink is metadata-only — its target is the whole content (tiny inline bytes,
            // no manifest/chunks), so it commits in one metadata op (mkdir-shaped), carrying
            // the link target on the wire. The server stores it on the node; B recreates the
            // link via apply_remote → SqliteProvider readlink.
            let target = node.symlink_target.clone().unwrap_or_default();
            client.symlink(
                &node.node_id,
                node.parent_node_id.as_deref(),
                &node.name,
                &target,
                node.mode,
                node.mtime,
            )?;
        }
        Kind::File => {
            let manifest: Vec<ChunkRef> = node
                .chunks
                .iter()
                .map(|c| ChunkRef {
                    hash: c.hash.clone(),
                    offset: c.offset,
                    length: c.length,
                })
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
            // Upload the missing chunks **in parallel** over the same bounded worker pool as
            // hydration. The uploads are independent and content-addressed (a re-PUT of
            // identical bytes is idempotent), and conflict handling lives entirely in
            // `commit_begin`/`commit_finalize` — the byte transfers between them have no
            // ordering or conflict semantics — so parallelizing them is safe and symmetric with
            // the download path. `begin.put_urls` is read-only and shared; the throttle's shared
            // token bucket still caps the aggregate UPLOAD rate.
            parallel_for_each(&begin.missing, DEFAULT_PUSH_CONCURRENCY, |hash| {
                let bytes = provider.chunk_get(hash)?;
                io.put(hash, &begin.put_urls, bytes)?;
                Ok(())
            })?;
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
    /// Chunk-fetch concurrency (bounded worker pool); 0/1 = serial.
    concurrency: usize,
}

#[cfg(target_os = "linux")]
impl EngineHydrator {
    /// Serial hydrator (back-compat; one chunk at a time).
    pub fn new(provider: Arc<SqliteProvider>, client: ServerClient) -> Self {
        Self::with_concurrency(provider, client, 1)
    }

    /// Hydrator with a bounded chunk-fetch worker pool of `concurrency` (0/1 = serial).
    pub fn with_concurrency(
        provider: Arc<SqliteProvider>,
        client: ServerClient,
        concurrency: usize,
    ) -> Self {
        Self {
            provider,
            client,
            concurrency,
        }
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
        let Some(node_id) = self.node_id_or_log(ino) else {
            return;
        };
        let io = PresignedIO::new(&self.client);
        if let Err(e) = SyncEngine::hydrate_concurrent(
            &self.provider,
            &self.client,
            &io,
            &node_id,
            self.concurrency,
        ) {
            eprintln!("hydrate: ino {ino} (node {node_id}) failed: {e}");
        }
    }

    /// Range hydration: fetch only the chunks overlapping the read's `[offset, size)` (the
    /// header-of-a-huge-file case), not the whole file — also over the bounded worker pool.
    fn hydrate_range(&self, ino: u64, offset: u64, size: u32) {
        let Some(node_id) = self.node_id_or_log(ino) else {
            return;
        };
        let io = PresignedIO::new(&self.client);
        if let Err(e) = SyncEngine::hydrate_range_concurrent(
            &self.provider,
            &self.client,
            &io,
            &node_id,
            offset,
            size as u64,
            self.concurrency,
        ) {
            eprintln!(
                "hydrate_range: ino {ino} (node {node_id}) off={offset} len={size} failed: {e}"
            );
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
        Self {
            notifier: Arc::new(std::sync::OnceLock::new()),
        }
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
    pub fn new(
        provider: Arc<SqliteProvider>,
        client: ServerClient,
        inval: Option<Arc<dyn Invalidate>>,
    ) -> Self {
        Self {
            provider,
            client,
            throttle: std::time::Duration::from_millis(750),
            last: std::sync::Mutex::new(None),
            inval,
        }
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
        match SyncEngine::pull_changes_paged_inval(
            &self.provider,
            &self.client,
            cursor,
            500,
            self.inval.as_deref(),
        ) {
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
        .map(|c| ChunkLoc {
            hash: c.hash,
            offset: c.offset,
            length: c.length,
        })
        .collect();
    SyncNode {
        node_id: n.node_id.clone(),
        parent_node_id: n.parent_node_id.clone(),
        name: n.name.clone(),
        kind: kind_from_str(&n.kind),
        manifest_id,
        chunks,
        version_id: n.version_id.clone(),
        mode: n
            .mode
            .unwrap_or(if n.kind == "dir" { 0o755 } else { 0o644 }),
        // A server that predates mtime sync sends `None` — fall back to `now` so the kernel
        // still sees a fresh stamp (the old behavior); a real synced mtime is applied verbatim.
        mtime: n.mtime.unwrap_or_else(now_nanos),
        // A symlink carries its target inline (decoded from base64 by sync-client); files/dirs
        // and pre-symlink servers send `None`.
        symlink_target: n.symlink_target.clone(),
    }
}

/// Map a server `kind` string to the provider's [`Kind`]. Unknown kinds fall back to
/// `File` (the historical default), so an older client degrades rather than panics.
fn kind_from_str(kind: &str) -> Kind {
    match kind {
        "dir" => Kind::Dir,
        "symlink" => Kind::Symlink,
        _ => Kind::File,
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
        return Ok(matches!(
            apply_upsert(provider, sn, inval, deferred)?,
            Applied::Clean {
                displaced_pending: true
            }
        ));
    }
    Ok(false)
}

/// What [`apply_upsert`] did with one remote upsert — the engine's SINGLE reading of the
/// provider's six-field [`fs_provider::ApplyOutcome`].
///
/// It exists so `retry_deferred` can DELEGATE to `apply_upsert` rather than re-implement it: a
/// plain `bool` cannot tell "applied" from "buffered for retry", and that distinction is what
/// drives the retry loop's `progress` flag. Before this type there were two hand-written
/// readings of `ApplyOutcome` and the second (`retry_deferred`'s) silently dropped
/// `displaced_pending` — a clobbered pending local node whose reconcile floor therefore never
/// fired, i.e. permanent silent loss on the one path that skipped the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Applied {
    /// Applied to the local tree. `displaced_pending` is the provider's own signal that the
    /// collision sweep hard-deleted a DIFFERENT local node that still carried a pending outbox
    /// entry — the delta can never re-add it, so the caller MUST reach the reconcile floor.
    Clean { displaced_pending: bool },
    /// Parent not present locally yet (`NotFound`) or applying now would create a cycle
    /// (`InvalidArgument`) → the node was pushed into `deferred` for retry. Nothing applied.
    Deferred,
    /// BUG #1: the node itself carries an un-pushed local edit, so `apply_remote` deliberately
    /// left the local row untouched. **Not a reconcile signal and not progress** — nothing
    /// changed, the local node is intact and still live, and its queued push is what surfaces
    /// the conflict server-side. Treating this as a divergence would turn every conflicting
    /// edit into a full-tree resync.
    SkippedDirtyLocal,
}

/// Apply one upsert `SyncNode`, deferring it (buffering for retry) instead of erroring if
/// its parent isn't present locally yet. Any *other* provider error still propagates. On a
/// successful apply, fires the appropriate OS-cache invalidation hook.
///
/// **This is the ONLY caller of `apply_remote` for an upsert.** `retry_deferred` delegates here
/// rather than keeping a second copy of the dispatch.
fn apply_upsert(
    provider: &SqliteProvider,
    sn: SyncNode,
    inval: Option<&dyn Invalidate>,
    deferred: &mut Vec<SyncNode>,
) -> Result<Applied> {
    match provider.apply_remote(&sn) {
        Ok(outcome) => {
            if outcome.dirty_local {
                // BUG #1: the node carries an un-pushed local edit whose manifest differs from
                // this incoming one. `apply_remote` deliberately left the local row untouched
                // (it would otherwise clobber the local edit + its base version, and the later
                // push would silently fast-forward). DEFER the remote upsert: apply nothing, fire
                // no cache invalidation, and leave the local edit queued in the outbox so its push
                // sends the local content against its stale base → the server records a real
                // conflict (keep/split) instead of a silent loss. Not a reconcile signal (the
                // local node is intact and still live).
                return Ok(Applied::SkippedDirtyLocal);
            }
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
            Ok(Applied::Clean {
                displaced_pending: outcome.displaced_pending,
            })
        }
        // Parent not present locally yet (`NotFound`) OR applying this re-parent now would
        // create a cycle (`InvalidArgument` — the new parent is the node itself or one of its
        // descendants) → buffer for retry within this pull rather than aborting it. Both are
        // transient *delta-interleaving* states: a later entry in the same delta usually moves
        // the parent into place / out of the would-be loop, after which the deferred upsert
        // applies cleanly (`retry_deferred`). If neither resolves by end-of-pull the node is
        // dropped and the caller falls back to the full-tree reconcile floor (`pull_snapshot`),
        // which re-applies the authoritative (acyclic) server snapshot — never leaving a detached
        // cycle or an orphaned subtree locally. `apply_remote` returns `NotFound` only for a
        // missing parent and `InvalidArgument` only for a would-be cycle (the node's own absence
        // is an INSERT, not an error).
        Err(fs_provider::FsError::NotFound | fs_provider::FsError::InvalidArgument) => {
            deferred.push(sn);
            Ok(Applied::Deferred)
        }
        Err(e) => Err(e.into()),
    }
}

/// Raise the sticky reconcile floor when this pull failed to converge the local tree, and say
/// whether the caller must now run `pull_snapshot`. One statement, one place, so a test can
/// drive the whole decision without a server — and so the flag can never be set by one caller
/// and read by another that composed the condition differently.
fn raise_reconcile_floor(provider: &SqliteProvider, diverged: bool) -> bool {
    if !diverged {
        return false;
    }
    let _ = provider.set_needs_resync(true);
    true
}

/// What one [`retry_deferred`] pass concluded. Two independent signals, both meaning "the
/// changelog delta alone did not converge this device" — see [`Self::diverged`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RetryOutcome {
    /// A buffered upsert stayed unresolvable at the fixed point and was dropped. Only ever
    /// true on the final pass; mid-page leftovers are RETAINED for a later page, not decided.
    dropped: bool,
    /// An apply during this pass clobbered a pending local node ([`Applied::Clean`]'s
    /// `displaced_pending`). Discarded entirely before 2026-08-19 — the displaced row is hard
    /// deleted, its queued outbox upsert then pushes as a no-op, and without this the loss is
    /// silent and permanent.
    displaced_pending: bool,
}

impl RetryOutcome {
    /// Did this pass leave the local tree diverged from the server? Either signal alone is
    /// enough; both are answered by the reconcile floor (`pull_snapshot`).
    fn diverged(&self) -> bool {
        self.dropped || self.displaced_pending
    }
}

/// Re-apply buffered (parentless / would-cycle) upserts, looping until no further progress — a
/// parent applied this round may unblock a child. Every apply goes through [`apply_upsert`], so
/// there is exactly ONE reading of `ApplyOutcome` in this crate; this function's whole job is
/// the fixed point plus the two signals in [`RetryOutcome`].
///
/// `final_pass` distinguishes the two call sites:
///   - mid-page (`false`): leftover unresolvable nodes are RETAINED in `deferred` — their
///     parent may arrive on a LATER page — so `dropped` stays false (no decision yet).
///     `displaced_pending` is still reported: a displacement already happened and is permanent.
///   - end-of-pull (`true`): leftovers are genuinely unresolvable from the delta; they are
///     dropped and `dropped` is true so the caller reconciles from the full tree
///     (`pull_snapshot`, the correctness floor) rather than silently losing them (BUG #2).
fn retry_deferred(
    provider: &SqliteProvider,
    deferred: &mut Vec<SyncNode>,
    inval: Option<&dyn Invalidate>,
    final_pass: bool,
) -> RetryOutcome {
    let mut out = RetryOutcome::default();
    if deferred.is_empty() {
        return out;
    }
    let mut progress = true;
    while progress && !deferred.is_empty() {
        progress = false;
        // `apply_upsert` re-pushes anything still unresolvable into this same (now emptied)
        // vec, which is exactly the buffering this loop wants — do NOT also push here.
        let pending = std::mem::take(deferred);
        for sn in pending {
            match apply_upsert(provider, sn, inval, deferred) {
                Ok(Applied::Clean { displaced_pending }) => {
                    progress = true;
                    out.displaced_pending |= displaced_pending;
                }
                // Still parent-not-present, or still would-cycle (a later page / a yet-to-retry
                // sibling may resolve either) → already re-buffered by `apply_upsert`.
                Ok(Applied::Deferred) => {}
                // BUG #1: nothing was applied and the local row is intact, so this is neither
                // progress nor a divergence. The node leaves the buffer (a retry would take the
                // same branch forever); its queued push surfaces the conflict server-side.
                Ok(Applied::SkippedDirtyLocal) => {}
                // A non-parent error on a deferred node: log + drop (don't poison the pull).
                // `apply_upsert` propagates these with `?`; the direct path wants that, but here
                // a transient DB error must not abort a pull that is otherwise converging.
                Err(e) => eprintln!("retry_deferred: dropping node after error: {e}"),
            }
        }
    }
    if !final_pass {
        // Mid-page: keep unresolved nodes buffered for a later page; no drop.
        return out;
    }
    // Whatever remains is unresolvable from the delta alone — drop it and tell the caller to
    // reconcile (don't silently lose it: it may still be live on the authoritative tree).
    out.dropped = !deferred.is_empty();
    deferred.clear();
    out
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
        .map(|c| ChunkLoc {
            hash: c.hash,
            offset: c.offset,
            length: c.length,
        })
        .collect();
    Some(SyncNode {
        node_id: view.node_id.clone(),
        parent_node_id: view.parent_node_id.clone(),
        name: view.name.clone(),
        kind: kind_from_str(&view.kind),
        manifest_id,
        chunks,
        version_id: ch.version.as_ref().map(|v| v.version_id.clone()),
        mode: view
            .mode
            .unwrap_or(if view.kind == "dir" { 0o755 } else { 0o644 }),
        mtime: view.mtime.unwrap_or_else(now_nanos),
        // A symlink change carries its target inline (decoded from base64 by sync-client).
        symlink_target: view.symlink_target.clone(),
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
    use fs_provider::{ChunkConfig, LocalChunkStore, Provider};
    use std::sync::Mutex;
    use sync_client::{ChunkRef, NodeView};

    const TINY: ChunkConfig = ChunkConfig {
        min: 64,
        avg: 256,
        max: 1024,
    };

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
                symlink_target: None,
            }),
            manifest: None,
            version: None,
        }
    }

    fn delete_change(node_id: &str) -> Change {
        Change {
            seq: 0,
            op: "delete".into(),
            node_id: node_id.into(),
            node: None,
            manifest: None,
            version: None,
        }
    }

    /// A pulled `delete` must resolve the node's ino BEFORE removing the row and fire
    /// `removed(ino)` so an on-demand adapter can drop the stale on-disk placeholder.
    #[test]
    fn delete_fires_removed_with_the_node_ino() {
        let p = provider();
        let mut deferred = Vec::new();
        let inval = RecordingInval::default();

        // Seed a node via a pulled upsert, then look up its ino.
        apply_change(
            &p,
            &upsert_change("N", None, "a.txt"),
            Some(&inval),
            &mut deferred,
        )
        .unwrap();
        let ino = p.ino_for_node_id("N").unwrap().unwrap();
        assert_eq!(
            *inval.created.lock().unwrap(),
            vec![ino],
            "the insert fired created"
        );

        // Now delete it — removed(ino) must fire with the SAME ino, and the row is gone.
        apply_change(&p, &delete_change("N"), Some(&inval), &mut deferred).unwrap();
        assert_eq!(
            *inval.removed.lock().unwrap(),
            vec![ino],
            "delete fired removed with the node's ino"
        );
        assert!(
            p.ino_for_node_id("N").unwrap().is_none(),
            "node removed from the tree"
        );
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

        apply_change(
            &p,
            &upsert_change("N", None, "old.txt"),
            Some(&inval),
            &mut deferred,
        )
        .unwrap();
        let ino = p.ino_for_node_id("N").unwrap().unwrap();

        // Rename within the root: a move.
        apply_change(
            &p,
            &upsert_change("N", None, "new.txt"),
            Some(&inval),
            &mut deferred,
        )
        .unwrap();
        // Move into a subdir (seed the dir first via its own insert).
        apply_change(
            &p,
            &Change {
                op: "upsert".into(),
                ..{
                    let mut c = upsert_change("D", None, "d");
                    if let Some(n) = c.node.as_mut() {
                        n.kind = "dir".into();
                    }
                    c
                }
            },
            Some(&inval),
            &mut deferred,
        )
        .unwrap();
        apply_change(
            &p,
            &upsert_change("N", Some("D"), "new.txt"),
            Some(&inval),
            &mut deferred,
        )
        .unwrap();

        let moved = inval.moved.lock().unwrap().clone();
        assert_eq!(
            moved,
            vec![ino, ino],
            "rename + reparent each fired moved(ino)"
        );
        // The relocations must NOT have been misreported as content invalidations.
        assert!(
            inval.invalidated.lock().unwrap().is_empty(),
            "a move is not a content invalidate"
        );
        // created fired only for the two INSERTs (N's first appearance + the dir D), never for a move.
        assert_eq!(
            inval.created.lock().unwrap().len(),
            2,
            "created only for the two inserts"
        );
    }

    fn dir_upsert(node_id: &str, parent: Option<&str>, name: &str) -> Change {
        let mut c = upsert_change(node_id, parent, name);
        if let Some(n) = c.node.as_mut() {
            n.kind = "dir".into();
        }
        c
    }

    /// Walk every node's parent chain; return false if any node can't reach ROOT within a bound
    /// (it's in a cycle / orphaned). The post-pull invariant: the local tree is fully reachable.
    fn tree_is_acyclic_reachable(p: &SqliteProvider) -> bool {
        let nodes = p.export_tree().unwrap();
        let parent_of: std::collections::HashMap<String, Option<String>> = nodes
            .iter()
            .map(|n| (n.node_id.clone(), n.parent_node_id.clone()))
            .collect();
        for n in &nodes {
            let mut cur = n.parent_node_id.clone();
            let mut hops = 0;
            while let Some(pid) = cur {
                hops += 1;
                if hops > 10_000 {
                    return false;
                }
                cur = parent_of.get(&pid).cloned().flatten();
            }
        }
        true
    }

    /// A cyclic re-parent upsert (X under Y, then Y under X) must NOT propagate as an error that
    /// aborts the whole pull, and must NOT mutate the tree into a detached cycle. Instead it is
    /// DEFERRED (buffered for retry, mirroring the parent-not-present discipline). When the delta
    /// drains and the cycle is still unresolvable, `retry_deferred(final_pass=true)` DROPS it and
    /// returns `true` so the caller falls back to the reconcile floor (`pull_snapshot`). The tree
    /// stays fully reachable throughout.
    #[test]
    fn cyclic_reparent_upsert_is_deferred_then_dropped_for_reconcile_floor() {
        let p = provider();
        let mut deferred = Vec::new();

        // Seed X and Y as siblings under ROOT.
        apply_change(&p, &dir_upsert("X", None, "X"), None, &mut deferred).unwrap();
        apply_change(&p, &dir_upsert("Y", None, "Y"), None, &mut deferred).unwrap();
        assert!(deferred.is_empty(), "clean sibling inserts don't defer");

        // X.parent := Y — clean (Y is not below X). Tree: ROOT → Y → X.
        let r1 = apply_change(&p, &dir_upsert("X", Some("Y"), "X"), None, &mut deferred).unwrap();
        assert!(!r1, "a clean reparent doesn't signal reconcile");
        assert!(deferred.is_empty(), "a clean reparent isn't deferred");

        // Y.parent := X — now a CYCLE (X is below Y). Must be DEFERRED, not Err, not applied.
        let r2 = apply_change(&p, &dir_upsert("Y", Some("X"), "Y"), None, &mut deferred)
            .expect("a cyclic reparent must be deferred, not propagated as an error");
        assert!(
            !r2,
            "deferring doesn't itself signal reconcile (the drop does)"
        );
        assert_eq!(deferred.len(), 1, "the cyclic upsert is buffered for retry");
        assert!(
            tree_is_acyclic_reachable(&p),
            "tree stays reachable while the cycle is deferred"
        );

        // End-of-pull drain: the cycle can't resolve from the delta alone → dropped, reconcile.
        let dropped = retry_deferred(&p, &mut deferred, None, true).dropped;
        assert!(
            dropped,
            "an unresolvable cyclic upsert is dropped → caller hits the reconcile floor"
        );
        assert!(
            deferred.is_empty(),
            "deferred is cleared after the final pass"
        );
        assert!(
            tree_is_acyclic_reachable(&p),
            "tree stays reachable (no detached cycle)"
        );
    }

    /// A cyclic interleaving that RESOLVES mid-delta: X under Y is deferred while it would cycle,
    /// then a later entry moves X back out from under Y, so on retry the deferred upsert applies
    /// cleanly — NO reconcile floor needed. This is the common transient-interleaving case.
    #[test]
    fn cyclic_reparent_resolved_by_a_later_delta_entry_needs_no_floor() {
        let p = provider();
        let mut deferred = Vec::new();

        // ROOT → Y → X  (X already under Y).
        apply_change(&p, &dir_upsert("Y", None, "Y"), None, &mut deferred).unwrap();
        apply_change(&p, &dir_upsert("X", Some("Y"), "X"), None, &mut deferred).unwrap();
        assert!(deferred.is_empty());

        // Y.parent := X would cycle → deferred.
        apply_change(&p, &dir_upsert("Y", Some("X"), "Y"), None, &mut deferred).unwrap();
        assert_eq!(deferred.len(), 1, "cyclic upsert buffered");

        // A LATER entry resolves it: move X back under ROOT (out from under Y). Now Y-under-X is
        // no longer a cycle (X is no longer Y's ancestor).
        apply_change(&p, &dir_upsert("X", None, "X"), None, &mut deferred).unwrap();

        // Retry the buffered cyclic upsert: it now applies cleanly. final_pass=true, but nothing
        // remains → no drop, no reconcile floor.
        let dropped = retry_deferred(&p, &mut deferred, None, true).dropped;
        assert!(!dropped, "the interleaving resolved → no reconcile floor");
        assert!(deferred.is_empty());
        assert!(
            tree_is_acyclic_reachable(&p),
            "converged to a reachable tree"
        );

        // Y is now under X, X under ROOT — exactly the resolved authoritative shape.
        let nodes = p.export_tree().unwrap();
        let y = nodes.iter().find(|n| n.node_id == "Y").unwrap();
        let x = nodes.iter().find(|n| n.node_id == "X").unwrap();
        assert_eq!(y.parent_node_id.as_deref(), Some("X"), "Y settled under X");
        assert!(x.parent_node_id.is_none(), "X settled under ROOT");
    }

    /// A DEEPER cycle (A → B → C, then C re-parented under A) is rejected + deferred just the
    /// same — the ancestor walk is not limited to the immediate parent.
    #[test]
    fn deep_cyclic_reparent_is_deferred() {
        let p = provider();
        let mut deferred = Vec::new();

        // ROOT → A → B → C.
        apply_change(&p, &dir_upsert("A", None, "A"), None, &mut deferred).unwrap();
        apply_change(&p, &dir_upsert("B", Some("A"), "B"), None, &mut deferred).unwrap();
        apply_change(&p, &dir_upsert("C", Some("B"), "C"), None, &mut deferred).unwrap();
        assert!(deferred.is_empty());

        // A.parent := C — A is C's grand-ancestor, so this closes a 3-node loop. Deferred.
        apply_change(&p, &dir_upsert("A", Some("C"), "A"), None, &mut deferred).unwrap();
        assert_eq!(
            deferred.len(),
            1,
            "the deep cyclic upsert is buffered, not applied"
        );
        assert!(tree_is_acyclic_reachable(&p), "tree stays reachable");

        let dropped = retry_deferred(&p, &mut deferred, None, true).dropped;
        assert!(dropped, "still unresolvable → reconcile floor");
        assert!(tree_is_acyclic_reachable(&p));
    }

    /// **F-51.** A deferred upsert that APPLIES on retry can still clobber a pending local node —
    /// and `retry_deferred` used to discard that `displaced_pending`, so the reconcile floor never
    /// fired and the displaced node was lost permanently (its queued outbox upsert pushes as a
    /// no-op once the row is hard-deleted; see `ApplyOutcome::displaced_pending`).
    ///
    /// The interleaving is ordinary, not exotic: `apply_upsert` defers on `InvalidArgument` (a
    /// would-be cycle) as well as `NotFound`, and a cycle-deferred node ALREADY EXISTS locally —
    /// so when a later delta entry breaks the cycle, the retry lands the node on a `(parent, name)`
    /// a local create is holding with an un-pushed outbox entry.
    ///
    /// Asserts the persistent `needs_resync` FLAG, not a returned bool: that flag is what a
    /// subsequent tick reads, and it is nil-until-written, so a regression cannot leave it
    /// coincidentally right.
    #[test]
    fn a_displacement_on_the_deferred_retry_path_raises_the_reconcile_floor() {
        let p = provider();
        let mut deferred = Vec::new();

        // ROOT → X → Y, both from the server.
        apply_change(&p, &dir_upsert("X", None, "X"), None, &mut deferred).unwrap();
        apply_change(&p, &dir_upsert("Y", Some("X"), "Y"), None, &mut deferred).unwrap();
        let y_ino = p.ino_for_node_id("Y").unwrap().expect("Y is local");

        // A LOCAL file at (Y, "x") with an un-pushed outbox upsert — un-synced local intent.
        let f = p.create(y_ino, "x").unwrap();
        p.write(f.ino, 0, b"local work").unwrap();
        p.flush(f.ino).unwrap();
        let f_id = p.node_id_for(f.ino).unwrap().unwrap();
        assert!(
            p.outbox_list()
                .unwrap()
                .iter()
                .any(|(id, op)| id == &f_id && op == "upsert"),
            "precondition: the local file's create is queued (un-pushed)"
        );

        // Delta entry 1: X moves under Y and takes the name "x". Y is X's descendant right now,
        // so this would CYCLE → deferred, nothing applied, no signal yet.
        let r1 = apply_change(&p, &dir_upsert("X", Some("Y"), "x"), None, &mut deferred).unwrap();
        assert!(!r1, "the deferral itself signals nothing");
        assert_eq!(deferred.len(), 1, "the cyclic upsert is buffered");

        // Delta entry 2: Y moves out to ROOT, breaking the cycle. Applies cleanly, no collision.
        let r2 = apply_change(&p, &dir_upsert("Y", None, "Y"), None, &mut deferred).unwrap();
        assert!(!r2, "moving Y out displaces nothing");

        // End-of-pull retry: X now applies under Y as "x" — and its collision sweep hard-deletes
        // the local (pending) file that held that name.
        let retry = retry_deferred(&p, &mut deferred, None, true);
        assert!(
            !retry.dropped,
            "the cycle resolved, so nothing was dropped — `dropped` alone cannot carry this"
        );
        assert!(
            retry.displaced_pending,
            "the retry clobbered a pending local node and must SAY SO"
        );
        assert!(
            p.ino_for_node_id(&f_id).unwrap().is_none(),
            "the local file really was swept out of the tree"
        );

        // The caller's own decision, through the one shared reading of it.
        assert!(
            raise_reconcile_floor(&p, retry.diverged()),
            "a displacement means the delta did not converge → reconcile"
        );
        assert!(
            p.needs_resync().unwrap(),
            "the sticky reconcile floor must be raised, or the displaced node is lost for good"
        );
    }

    /// The other half of the same rule: a BUG #1 dirty-local skip is NOT a divergence. Nothing was
    /// applied and the local row is intact, so raising the reconcile floor here would turn every
    /// conflicting edit into a full-tree resync (and `pull_snapshot` would overwrite the very edit
    /// the skip exists to preserve).
    #[test]
    fn a_dirty_local_skip_is_not_a_reconcile_signal() {
        let p = provider();
        let mut deferred = Vec::new();
        apply_change(&p, &dir_upsert("D", None, "D"), None, &mut deferred).unwrap();
        let d_ino = p.ino_for_node_id("D").unwrap().unwrap();

        // A local file with an un-pushed edit.
        let f = p.create(d_ino, "f.txt").unwrap();
        p.write(f.ino, 0, b"my local edit").unwrap();
        p.flush(f.ino).unwrap();
        let f_id = p.node_id_for(f.ino).unwrap().unwrap();

        // The server sends a DIFFERENT manifest for that same node → dirty_local.
        let mut ch = upsert_change(&f_id, Some("D"), "f.txt");
        ch.manifest = Some(vec![ChunkRef {
            hash: "0".repeat(64),
            offset: 0,
            length: 7,
        }]);
        // The narrowest true thing: the handler must NAME this outcome, because `retry_deferred`
        // branches on the variant. Reported as `Clean` it becomes "progress" there, and reported
        // as `Clean { displaced_pending: true }` it becomes a full-tree resync on every
        // conflicting edit — which would overwrite the very local edit the skip exists to keep.
        let sn = change_to_sync(&ch).unwrap();
        assert_eq!(
            apply_upsert(&p, sn, None, &mut deferred).unwrap(),
            Applied::SkippedDirtyLocal,
            "an un-pushed local edit is SKIPPED, not applied, deferred, or displaced"
        );

        let signaled = apply_change(&p, &ch, None, &mut deferred).unwrap();
        assert!(!signaled, "a dirty-local skip is not a reconcile signal");
        assert!(
            deferred.is_empty(),
            "a dirty-local skip is not buffered for retry either"
        );
        assert!(
            !raise_reconcile_floor(&p, signaled),
            "nothing diverged, so no floor"
        );
        assert!(
            !p.needs_resync().unwrap(),
            "a conflicting local edit must NOT trigger a full-tree resync"
        );
        let size = p.getattr(f.ino).unwrap().size as u32;
        assert_eq!(
            p.read(f.ino, 0, size).unwrap(),
            b"my local edit",
            "the local edit is preserved untouched"
        );
    }
}

#[cfg(test)]
mod hydrate_integrity_tests {
    //! Chunk-integrity verification on the hydrate (fetch-from-cloud) path. Chunks are
    //! content-addressed — the manifest hash IS the sha256 of the bytes — so a corrupt /
    //! truncated / wrong byte returned by remote object storage must be **rejected** before
    //! it is written into the local cache and served as file content (silent data corruption).
    //! Verification lives only on the fetch path; the local-write/commit path already hashed
    //! its own bytes to derive the key, and every-read verification would be a hot-path cost.
    use super::*;
    use fs_provider::{ChunkConfig, LocalChunkStore, Provider};
    use std::sync::Mutex;

    const TINY: ChunkConfig = ChunkConfig {
        min: 64,
        avg: 256,
        max: 1024,
    };

    fn provider() -> SqliteProvider {
        SqliteProvider::in_memory(Box::new(LocalChunkStore::temp().unwrap()), TINY).unwrap()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// Seed a single-chunk file node in the local tree (a manifest pointing at `hash`) WITHOUT
    /// the chunk bytes in the local store — exactly the post-pull state a hydrate fetches. Uses
    /// `apply_remote`, the real pull-apply path. Returns the node_id.
    fn seed_placeholder(p: &SqliteProvider, content: &[u8]) -> String {
        let hash = sha256_hex(content);
        let manifest_id = {
            let mut mh = Sha256::new();
            mh.update(hash.as_bytes());
            hex::encode(mh.finalize())
        };
        let node = SyncNode {
            node_id: "F".into(),
            parent_node_id: None,
            name: "f.bin".into(),
            kind: Kind::File,
            manifest_id: Some(manifest_id),
            chunks: vec![ChunkLoc {
                hash,
                offset: 0,
                length: content.len() as u64,
            }],
            version_id: Some("v1".into()),
            mode: 0o644,
            mtime: 0,
            symlink_target: None,
        };
        p.apply_remote(&node).unwrap();
        "F".into()
    }

    /// A transport that returns the requested chunk's bytes verbatim — the honest cloud.
    /// `needs_urls() == false` so the engine never calls the (offline) server.
    struct HonestIO {
        chunks: HashMap<String, Vec<u8>>,
    }
    impl ChunkIO for HonestIO {
        fn put(&self, _h: &str, _u: &HashMap<String, String>, _b: Vec<u8>) -> Result<()> {
            Ok(())
        }
        fn get(&self, hash: &str, _url: Option<&str>) -> Result<Vec<u8>> {
            Ok(self.chunks.get(hash).cloned().unwrap_or_default())
        }
        fn needs_urls(&self) -> bool {
            false
        }
    }

    /// A transport that returns CORRUPTED bytes for a chunk — bytes whose sha256 does NOT match
    /// the hash they were fetched for (simulates storage corruption / a truncated download / a
    /// wrong object). Records which hashes it served so the test can assert nothing leaked.
    struct CorruptingIO {
        served: Mutex<Vec<String>>,
    }
    impl ChunkIO for CorruptingIO {
        fn put(&self, _h: &str, _u: &HashMap<String, String>, _b: Vec<u8>) -> Result<()> {
            Ok(())
        }
        fn get(&self, hash: &str, _url: Option<&str>) -> Result<Vec<u8>> {
            self.served.lock().unwrap().push(hash.to_string());
            Ok(b"corrupted-bytes-do-not-match-the-hash".to_vec())
        }
        fn needs_urls(&self) -> bool {
            false
        }
    }

    /// RED: a fetched chunk whose bytes don't hash to the expected (manifest) hash must be
    /// REJECTED — the hydrate fails with a corruption error, the bad bytes are NOT written into
    /// the local cache, and a subsequent read therefore never serves wrong content.
    #[test]
    fn corrupt_fetched_chunk_is_rejected_and_not_cached() {
        let p = provider();
        let content = b"the real file content, repeated to span a chunk boundary maybe... \
                        and a bit more so the manifest is non-trivial";
        let node_id = seed_placeholder(&p, content);
        let expected_hash = sha256_hex(content);
        assert!(
            !p.has_chunk(&expected_hash),
            "precondition: chunk not yet local"
        );

        let client = ServerClient::new("http://127.0.0.1:0"); // never contacted (needs_urls=false)
        let io = CorruptingIO {
            served: Mutex::new(Vec::new()),
        };

        let err = SyncEngine::hydrate(&p, &client, &io, &node_id)
            .expect_err("hydrate must FAIL when the cloud returns bytes that don't match the hash");

        match err {
            Error::ChunkCorrupt { expected, .. } => {
                assert_eq!(
                    expected, expected_hash,
                    "the error names the expected manifest hash"
                );
            }
            other => panic!("expected Error::ChunkCorrupt, got {other:?}"),
        }

        // The corrupt bytes must NOT have been written into the content-addressed cache.
        assert!(
            !p.has_chunk(&expected_hash),
            "corrupt bytes must never enter the local cache under the chunk's hash"
        );
        // And the transport was actually exercised (we really fetched + rejected).
        assert_eq!(
            io.served.lock().unwrap().len(),
            1,
            "the corrupt chunk was fetched once"
        );
    }

    /// GREEN companion: a correct chunk (bytes hash to the requested hash) hydrates fine and is
    /// cached, so verification does not break the honest path.
    #[test]
    fn correct_fetched_chunk_hydrates_and_is_cached() {
        let p = provider();
        let content = b"honest bytes that hash to their own content address";
        let node_id = seed_placeholder(&p, content);
        let hash = sha256_hex(content);

        let mut chunks = HashMap::new();
        chunks.insert(hash.clone(), content.to_vec());
        let io = HonestIO { chunks };
        let client = ServerClient::new("http://127.0.0.1:0");

        SyncEngine::hydrate(&p, &client, &io, &node_id).expect("a correct chunk hydrates");
        assert!(
            p.has_chunk(&hash),
            "the verified chunk is now in the local cache"
        );
        // And the file reads back byte-for-byte.
        let ino = p.ino_for_node_id(&node_id).unwrap().unwrap();
        let got = p.read(ino, 0, content.len() as u32).unwrap();
        assert_eq!(
            got, content,
            "the hydrated file reads back its real content"
        );
    }
}
