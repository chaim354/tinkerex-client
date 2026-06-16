//! **Two-device end-to-end** — the headline multi-device workflow, driven through the
//! SAME engine primitives the Tauri app's `Daemon`/`WebdavMount`/`CfApiMount` run on every
//! interval, plus the SAME `ServerClient` API its commands call (`sync_pair`, `file_versions`,
//! `restore_version`, `conflicts`, `resolve_conflict_*`).
//!
//! ## How this maps to the real app stack
//!
//! Two independent client instances (`DeviceCtx` — each with its own SQLite tree, its own
//! content-addressed chunk cache, its own device token) paired to ONE user/library on one
//! server. This is exactly the Tauri app's per-account `(provider, chunk_dir, token)` triple
//! from `maker-app/src-tauri/src/lib.rs::mount_with_token`.
//!
//! Each device's `sync_tick()` here is **literally what `sync_engine::daemon::sync_loop`
//! calls** on every nudge/poll: `SyncEngine::push_outbox` (drain local writes → server +
//! chunk store) then `SyncEngine::pull_changes` (apply the changelog delta). A read after a
//! pull goes through the engine's `SyncEngine::hydrate` — the same call the per-OS adapter's
//! hydrate hook makes on a chunk-cache miss (`fs_fuse::Hydrate` / `fs_webdav::Hydrate` /
//! `fs_cfapi::Hydrate`, all wired in `daemon.rs`).
//!
//! ## Which leg is which (mount vs engine/provider level)
//!
//! These tests cover the **data + control plane end-to-end at the engine/provider level**:
//! a local write through `Provider` (the same SQLite tree the kernel mount writes into via
//! FUSE `flush` / WebDAV PUT / CfAPI `NOTIFY_FILE_CLOSE`), then the real `SyncEngine` loop
//! body + the real `ServerClient` HTTP wire + a `ChunkIO` chunk store (`SharedDirIO`, the
//! `PresignedIO` stand-in). They run **headlessly on all three OSes** because they don't open
//! a kernel mount — the part that can't run headless (FUSE-T / WebDAV / CfAPI all need an
//! interactive session, per CLAUDE.md §12.1 and SETUP.md). The **kernel-mount leg** (the
//! adapter translating syscalls → these same `Provider` calls) is proven separately and
//! interactively by `client/test-conformance/` (cfapi-smoke/conformance on Windows;
//! webdav-rclone + fsx-fuse on mac/Linux) — see the module README. So: full multi-device
//! convergence is automated here on every OS; the kernel-syscall translation is the
//! conformance suites' job.
//!
//! Skips cleanly (prints a notice, passes) when no server is configured — same convention as
//! `integration.rs`. CI boots `STORAGE_BACKEND=memory` Rails and sets `SYNC_IT_SERVER_URL`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::{RegisterResponse, ServerClient};
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

/// `Some(url)` if a test server is configured, else early-return with a skip notice.
macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!(
                    "SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run the two-device e2e)"
                );
                return;
            }
        }
    };
}

/// Small chunks so even a few-KB file is multi-chunk → dedup + per-chunk transfer get real
/// coverage (same as integration.rs).
const TINY: ChunkConfig = ChunkConfig {
    min: 64,
    avg: 256,
    max: 1024,
};

fn uniq() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    )
}

/// Deterministic pseudo-random bytes (an LCG) — low self-similarity, so a file made of these is
/// genuinely many *distinct* content-defined chunks. Used by the dedup test so intra-file dedup
/// of repetitive data doesn't mask the cross-version dedup being measured.
fn prng(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

/// One client device, modeling the app's per-account state: an isolated local tree + chunk
/// cache (`provider`), the server API client bound to this device's token (`client`), and the
/// sync cursor the daemon persists in `sync_state`. NOT shared between devices — exactly like
/// two machines, so B genuinely has to hydrate from the chunk store (the data plane is tested).
struct DeviceCtx {
    name: String,
    provider: SqliteProvider,
    client: ServerClient,
}

impl DeviceCtx {
    /// A brand-new isolated local store + a client on the given token.
    fn new(name: &str, url: &str, token: String) -> Self {
        let dir = std::env::temp_dir().join(format!("e2e-{name}-{}", uniq()));
        let provider =
            SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap();
        let client = ServerClient::with_token(url, token);
        DeviceCtx {
            name: name.into(),
            provider,
            client,
        }
    }

    /// The daemon's loop body: push local outbox, then pull + apply the changelog delta. This
    /// is exactly `sync_engine::daemon::sync_loop`'s per-tick work (`push_outbox` then the
    /// delta pull inside `sync_tick`), with the cursor persisted in `sync_state`.
    fn sync_tick(&self, io: &dyn ChunkIO) {
        let cursor = self.provider.cursor().unwrap();
        let new_cursor = SyncEngine::sync_tick(&self.provider, &self.client, io, cursor)
            .unwrap_or_else(|e| panic!("{} sync_tick: {e}", self.name));
        self.provider.set_cursor(new_cursor).unwrap();
    }

    /// First sync after pairing: pull the whole tree snapshot (what `CfApiMount::start` and a
    /// fresh `Daemon` do before the loop), persisting the cursor.
    fn initial_sync(&self) {
        let head = SyncEngine::pull_snapshot(&self.provider, &self.client)
            .unwrap_or_else(|e| panic!("{} pull_snapshot: {e}", self.name));
        self.provider.set_cursor(head).unwrap();
    }

    /// Read a file's full content the way a mount read does: hydrate any missing chunks via the
    /// engine (the adapter's hydrate hook), then read from the local provider.
    fn read_hydrated(&self, ino: u64, io: &dyn ChunkIO) -> Vec<u8> {
        let node_id = self.provider.node_id_for(ino).unwrap().unwrap();
        SyncEngine::hydrate(&self.provider, &self.client, io, &node_id)
            .unwrap_or_else(|e| panic!("{} hydrate {node_id}: {e}", self.name));
        let size = self.provider.getattr(ino).unwrap().size;
        if size == 0 {
            Vec::new()
        } else {
            self.provider.read(ino, 0, size as u32).unwrap()
        }
    }

    fn node_id(&self, ino: u64) -> String {
        self.provider.node_id_for(ino).unwrap().unwrap()
    }
}

/// Register device A (creates the library), then pair device B into it via the **real pairing
/// flow** (`create_pairing` on A → `pair` on B) — exactly what the app's `create_pairing` +
/// `sync_pair` commands do. Returns both device contexts + a shared "cloud" chunk store.
///
/// (The shared `SharedDirIO` stands in for SeaweedFS/R2; in production the bytes move via
/// `PresignedIO` to object storage. Both implement `ChunkIO`, so the loop code is identical —
/// only the transport differs, which `presigned_roundtrip.rs` exercises against live storage.)
fn register_and_pair(url: &str) -> (DeviceCtx, DeviceCtx, SharedDirIO) {
    let email = format!("e2e-{}@example.com", uniq());

    // Device A creates the (confirmed) account + enrolls (the app's `sync_login` first-run path).
    let password = "correct horse battery staple";
    let mut ca = ServerClient::new(url);
    let reg_a: RegisterResponse = ca.enroll(&email, password, "device-A").expect("enroll A");
    let a = DeviceCtx::new("A", url, reg_a.device_token);

    // Brand-new device B enrolls by logging in with the SAME account credentials (the Dropbox
    // model — each device authenticates independently; no pairing codes).
    let mut cb = ServerClient::new(url);
    let reg_b: RegisterResponse = cb
        .device_login(&email, password, "device-B")
        .expect("B logs in");
    let b = DeviceCtx::new("B", url, reg_b.device_token);

    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("e2e-cloud-{}", uniq()))).unwrap();
    (a, b, cloud)
}

// ============================================================================================
// THE CANONICAL SCENARIO: A writes → B pulls + reads → B edits → A pulls the edit, byte-for-byte
// ============================================================================================

#[test]
fn canonical_roundtrip_a_writes_b_reads_b_edits_a_reads() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    // --- 1. Device A writes a file through its provider (= a mount write) and the daemon pushes.
    let dir = a.provider.mkdir(ROOT_INO, "Projects").unwrap();
    let af = a.provider.create(dir.ino, "lamp.scad").unwrap();
    let v1: Vec<u8> = b"// v1: a parametric lamp\nmodule lamp(){ cube(10); }\n".repeat(120);
    a.provider.write(af.ino, 0, &v1).unwrap();
    a.provider.flush(af.ino).unwrap();
    a.sync_tick(&cloud); // outbox → server + chunk store
    assert!(
        a.provider.outbox_list().unwrap().is_empty(),
        "A's outbox drained after push"
    );

    // --- 2. Device B's daemon pulls + hydrates → byte-for-byte identical.
    b.initial_sync();
    let b_proj = b
        .provider
        .lookup(ROOT_INO, "Projects")
        .expect("B sees Projects");
    let bf = b
        .provider
        .lookup(b_proj.ino, "lamp.scad")
        .expect("B sees lamp.scad");
    assert_eq!(
        b.read_hydrated(bf.ino, &cloud),
        v1,
        "B reads A's v1 byte-for-byte"
    );

    // --- 3. Device B edits the file through its provider and pushes.
    let v2: Vec<u8> =
        b"// v2: B widened the base\nmodule lamp(){ cube([20,20,10]); }\n".repeat(200);
    b.provider.truncate(bf.ino, 0).unwrap();
    b.provider.write(bf.ino, 0, &v2).unwrap();
    b.provider.flush(bf.ino).unwrap();
    b.sync_tick(&cloud);

    // --- 4. Device A's daemon pulls → A sees B's edit byte-for-byte.
    a.sync_tick(&cloud);
    assert_eq!(
        a.read_hydrated(af.ino, &cloud),
        v2,
        "A reads B's v2 edit byte-for-byte"
    );

    // Sanity: no spurious conflict (these were sequential, each based on the current version).
    assert!(
        a.client.conflicts().unwrap().conflicts.is_empty(),
        "clean sequential edits, no conflict"
    );
}

// ============================================================================================
// New file + nested-dir creation propagates A → B
// ============================================================================================

#[test]
fn new_files_and_nested_dirs_propagate() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    // A builds a nested tree with files at multiple depths.
    let docs = a.provider.mkdir(ROOT_INO, "Docs").unwrap();
    let sub = a.provider.mkdir(docs.ino, "2026").unwrap();
    let top = a.provider.create(ROOT_INO, "README.md").unwrap();
    a.provider.write(top.ino, 0, b"# Maker library\n").unwrap();
    a.provider.flush(top.ino).unwrap();
    let deep = a.provider.create(sub.ino, "plan.txt").unwrap();
    a.provider
        .write(deep.ino, 0, b"Q3 plan: ship sync\n")
        .unwrap();
    a.provider.flush(deep.ino).unwrap();
    a.sync_tick(&cloud);

    // B converges on the whole structure.
    b.initial_sync();
    let b_docs = b.provider.lookup(ROOT_INO, "Docs").expect("B has Docs");
    let b_sub = b
        .provider
        .lookup(b_docs.ino, "2026")
        .expect("B has Docs/2026");
    let b_deep = b
        .provider
        .lookup(b_sub.ino, "plan.txt")
        .expect("B has Docs/2026/plan.txt");
    let b_top = b
        .provider
        .lookup(ROOT_INO, "README.md")
        .expect("B has README.md");
    assert_eq!(
        b.read_hydrated(b_deep.ino, &cloud),
        b"Q3 plan: ship sync\n",
        "nested file content"
    );
    assert_eq!(
        b.read_hydrated(b_top.ino, &cloud),
        b"# Maker library\n",
        "top-level file content"
    );
}

// ============================================================================================
// Delete propagation: A deletes → B converges (the file is gone on B)
// ============================================================================================

#[test]
fn delete_propagates_a_to_b() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    let f = a.provider.create(ROOT_INO, "scratch.txt").unwrap();
    a.provider.write(f.ino, 0, b"temporary").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    b.initial_sync();
    assert!(
        b.provider.lookup(ROOT_INO, "scratch.txt").is_ok(),
        "B has the file before the delete"
    );

    // A deletes; B's next tick applies the deletion.
    a.provider.unlink(ROOT_INO, "scratch.txt").unwrap();
    a.sync_tick(&cloud);
    b.sync_tick(&cloud);
    assert!(
        b.provider.lookup(ROOT_INO, "scratch.txt").is_err(),
        "delete converged on B"
    );
}

// ============================================================================================
// Rename / move propagation: A renames + moves → B converges (new path, no dup at old path)
// ============================================================================================

#[test]
fn rename_and_move_propagate_a_to_b() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    let inbox = a.provider.mkdir(ROOT_INO, "Inbox").unwrap();
    let archive = a.provider.mkdir(ROOT_INO, "Archive").unwrap();
    let f = a.provider.create(inbox.ino, "draft.txt").unwrap();
    a.provider.write(f.ino, 0, b"keep me").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    b.initial_sync();
    let b_inbox = b.provider.lookup(ROOT_INO, "Inbox").unwrap().ino;
    let b_archive = b.provider.lookup(ROOT_INO, "Archive").unwrap().ino;
    assert!(
        b.provider.lookup(b_inbox, "draft.txt").is_ok(),
        "B has Inbox/draft.txt initially"
    );

    // A renames-and-moves Inbox/draft.txt → Archive/final.txt.
    a.provider
        .rename(inbox.ino, "draft.txt", archive.ino, "final.txt")
        .unwrap();
    a.sync_tick(&cloud);
    b.sync_tick(&cloud);

    let moved = b
        .provider
        .lookup(b_archive, "final.txt")
        .expect("B sees the file at the new path");
    assert_eq!(
        b.read_hydrated(moved.ino, &cloud),
        b"keep me",
        "content preserved across move"
    );
    assert!(
        b.provider.lookup(b_inbox, "draft.txt").is_err(),
        "no stale copy at the old path"
    );
}

// ============================================================================================
// Larger multi-chunk file + dedup: only changed chunks move on an edit
// ============================================================================================

#[test]
fn large_file_edit_dedups_unchanged_chunks() {
    let url = server_url!();
    // This test counts files in the chunk store to prove dedup, so it owns the dir explicitly
    // (instead of `register_and_pair`'s internal one) — `SharedDirIO` doesn't expose its path.
    let (a, b, _unused) = register_and_pair(&url);
    let cloud_path = std::env::temp_dir().join(format!("e2e-dedup-cloud-{}", uniq()));
    let cloud = SharedDirIO::new(&cloud_path).unwrap();
    let count = || {
        std::fs::read_dir(&cloud_path)
            .map(|d| d.count())
            .unwrap_or(0)
    };

    // A pushes a large (multi-chunk) file: a stable head, then a tail we'll edit. Use
    // low-self-similarity (deterministic PRNG) bytes so the file is genuinely many *distinct*
    // chunks — repetitive data would intra-file-dedup to a handful and mask the cross-version
    // dedup we're measuring.
    let head = prng(0xA11CE, 30_000);
    let tail_v1 = prng(0xB0B, 30_000);
    let mut v1 = head.clone();
    v1.extend_from_slice(&tail_v1);

    let f = a.provider.create(ROOT_INO, "model.stl").unwrap();
    a.provider.write(f.ino, 0, &v1).unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);
    let chunks_after_v1 = count();
    assert!(
        chunks_after_v1 > 1,
        "a >chunk-size file lands as multiple chunks ({chunks_after_v1})"
    );

    // B reads v1.
    b.initial_sync();
    let bf = b.provider.lookup(ROOT_INO, "model.stl").unwrap();
    assert_eq!(b.read_hydrated(bf.ino, &cloud), v1, "B reads v1");

    // A edits ONLY the tail; the head's content-defined chunks are unchanged → dedup: the cloud
    // gains only the new tail chunks, not a fresh copy of the whole file.
    let tail_v2 = prng(0xCAFE, 30_000);
    let mut v2 = head.clone();
    v2.extend_from_slice(&tail_v2);
    a.provider.truncate(f.ino, 0).unwrap();
    a.provider.write(f.ino, 0, &v2).unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);
    let chunks_after_v2 = count();

    // The head is ~half the file; only the changed tail's chunks should be new. So the bytes that
    // moved on the edit are well under a full re-upload (which would ~double the store). This is
    // the "only changed chunks move" guarantee — the FastCDC block-delta win — asserted as a
    // generous bound (≈ the tail, with slack for the head/tail boundary chunk) rather than an
    // exact count, so it's robust to chunker tuning.
    let added = chunks_after_v2 - chunks_after_v1;
    assert!(
        added < chunks_after_v1 * 3 / 4,
        "edit re-used the unchanged head's chunks: added {added} new chunks, but a full re-upload \
         would have added ~{chunks_after_v1}"
    );

    // B converges on v2, byte-for-byte (it only hydrates the chunks it's missing).
    b.sync_tick(&cloud);
    assert_eq!(
        b.read_hydrated(bf.ino, &cloud),
        v2,
        "B reads v2 after a dedup'd edit"
    );
}

// ============================================================================================
// Version history + restore: A restores a prior version → B converges to it
// ============================================================================================

#[test]
fn version_history_and_restore_converge() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    // A publishes v1, then v2 (two versions in history).
    let f = a.provider.create(ROOT_INO, "spec.md").unwrap();
    a.provider.write(f.ino, 0, b"spec v1: original").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    a.provider.truncate(f.ino, 0).unwrap();
    a.provider
        .write(f.ino, 0, b"spec v2: revised heading")
        .unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    let node_id = a.node_id(f.ino);

    // B converges on v2.
    b.initial_sync();
    let bf = b.provider.lookup(ROOT_INO, "spec.md").unwrap();
    assert_eq!(
        b.read_hydrated(bf.ino, &cloud),
        b"spec v2: revised heading",
        "B has v2"
    );

    // The app's `file_versions` command: history newest-first, two entries.
    let versions = a
        .client
        .versions(&node_id)
        .expect("version history")
        .versions;
    assert!(
        versions.len() >= 2,
        "two versions in history (got {})",
        versions.len()
    );

    // Restore to the OLDEST (v1) — the app's `restore_version` command. This mints a NEW version
    // reusing v1's manifest (lossless), which propagates like any edit.
    let oldest = versions.last().unwrap().version_id.clone();
    a.client.restore(&node_id, &oldest).expect("restore to v1");

    // B's daemon pulls the restore and converges back to v1's content, byte-for-byte.
    b.sync_tick(&cloud);
    assert_eq!(
        b.read_hydrated(bf.ino, &cloud),
        b"spec v1: original",
        "B converged to the restored v1 content"
    );
}

// ============================================================================================
// Offline-edit conflict: A and B both edit offline → conflict recorded → keep/split resolution
// ============================================================================================

#[test]
fn offline_divergent_edits_conflict_then_resolve() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    // A creates + pushes a file; B pulls it (both now track version V1).
    let f = a.provider.create(ROOT_INO, "design.scad").unwrap();
    a.provider.write(f.ino, 0, b"base design").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);
    b.initial_sync();
    let bf = b.provider.lookup(ROOT_INO, "design.scad").unwrap();

    // A edits + pushes online → server current is V2.
    a.provider.truncate(f.ino, 0).unwrap();
    a.provider.write(f.ino, 0, b"A online change").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    // B (which never saw V2 — it was "offline") edits its V1-based copy and pushes.
    b.provider.truncate(bf.ino, 0).unwrap();
    b.provider.write(bf.ino, 0, b"B offline change").unwrap();
    b.provider.flush(bf.ino).unwrap();
    b.sync_tick(&cloud);

    // The server flags a conflict (both versions kept) instead of silently dropping B's edit.
    let conflicts = b.client.conflicts().expect("conflicts").conflicts;
    assert_eq!(
        conflicts.len(),
        1,
        "B's stale-based edit raised exactly one conflict"
    );
    let c = &conflicts[0];
    assert_eq!(c.name.as_deref(), Some("design.scad"));
    assert!(
        c.current.is_some() && c.incoming.is_some(),
        "both sides reported (device + time + size)"
    );

    // Resolve by SPLIT (the app's `resolve_conflict_split`): incoming side becomes a sibling.
    let split = b
        .client
        .resolve_conflict_split(&c.node_id)
        .expect("split-resolve");
    assert!(
        !split.split_node_id.is_empty(),
        "split created a new sibling node"
    );
    assert!(
        b.client.conflicts().unwrap().conflicts.is_empty(),
        "conflict cleared after split"
    );

    // Both devices converge on the resolved tree: the original + the split sibling both present.
    a.sync_tick(&cloud);
    b.sync_tick(&cloud);
    let count_scad = |d: &DeviceCtx| {
        d.provider
            .readdir(ROOT_INO)
            .unwrap()
            .iter()
            .filter(|e| e.name.contains("design"))
            .count()
    };
    assert_eq!(
        count_scad(&a),
        2,
        "A sees the original + the conflicted-copy sibling"
    );
    assert_eq!(
        count_scad(&b),
        2,
        "B sees the original + the conflicted-copy sibling"
    );
}

// ============================================================================================
// Best-effort cross-device lock coordination (the lock indicators the browser shows)
// ============================================================================================

#[test]
fn cross_device_lock_is_visible_to_the_other_device() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    let f = a.provider.create(ROOT_INO, "shared.scad").unwrap();
    a.provider.write(f.ino, 0, b"shared").unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);
    b.initial_sync();
    let node_id = a.node_id(f.ino);

    // A acquires the edit lock; B (connected) sees it held by device-A, not stolen.
    let la = a.client.lock(&node_id).expect("A lock");
    assert!(la.held && la.by_me, "A holds its own lock");
    let lb = b.client.lock(&node_id).expect("B lock attempt");
    assert!(lb.held && !lb.by_me, "B sees A's live lock");
    assert_eq!(lb.device_name.as_deref(), Some("device-A"));

    // A releases → B can acquire (the indicator clears).
    a.client.unlock(&node_id).expect("A unlock");
    let lb2 = b.client.lock(&node_id).expect("B re-attempt");
    assert!(lb2.by_me, "B acquires after A releases");
}

// ============================================================================================
// Offline availability: a hydrated file stays readable with no network (cross-platform)
// ============================================================================================

/// A chunk transport that's "offline": every fetch fails.
struct OfflineIO;
impl ChunkIO for OfflineIO {
    fn put(
        &self,
        hash: &str,
        _urls: &HashMap<String, String>,
        _bytes: Vec<u8>,
    ) -> sync_engine::Result<()> {
        Err(sync_engine::Error::NoUrl(format!("offline: {hash}")))
    }
    fn get(&self, hash: &str, _url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        Err(sync_engine::Error::NoUrl(format!("offline: {hash}")))
    }
}

#[test]
fn hydrated_file_reads_offline_on_b() {
    let url = server_url!();
    let (a, b, cloud) = register_and_pair(&url);

    let f = a.provider.create(ROOT_INO, "offline.bin").unwrap();
    let body: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    a.provider.write(f.ino, 0, &body).unwrap();
    a.provider.flush(f.ino).unwrap();
    a.sync_tick(&cloud);

    // B hydrates ONLINE (chunks land in B's local cache).
    b.initial_sync();
    let bf = b.provider.lookup(ROOT_INO, "offline.bin").unwrap();
    assert_eq!(
        b.read_hydrated(bf.ino, &cloud),
        body,
        "online read after hydrate"
    );

    // Now go OFFLINE: re-hydrate must touch nothing (all cached) and the read still serves bytes.
    let node_id = b.node_id(bf.ino);
    SyncEngine::hydrate(&b.provider, &b.client, &OfflineIO, &node_id)
        .expect("offline re-hydrate is a no-op (everything cached)");
    let size = b.provider.getattr(bf.ino).unwrap().size;
    assert_eq!(
        b.provider.read(bf.ino, 0, size as u32).unwrap(),
        body,
        "reads offline from local cache"
    );
}
