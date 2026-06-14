//! Server-backed engine integration — the *real wire contract* (HTTP control plane +
//! changelog), with chunk bytes moving through a shared-dir stand-in for the cloud
//! (`SharedDirIO`). These cover what the hermetic fs-provider suites can't: push/pull/
//! hydrate over the network, last-writer-wins convergence, dedup/idempotency of pushes,
//! and cursor monotonicity.
//!
//! They require a running server and are **skipped** (pass with a printed notice) when
//! `SYNC_IT_SERVER_URL`/`SERVER_URL` is unset, so a plain `cargo test` stays green
//! offline. CI boots `STORAGE_BACKEND=memory` Rails and sets the var — see
//! `.github/workflows/rust-client.yml`.

use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

/// `Some(url)` if a test server is configured, else `None` → the test prints a skip
/// notice and returns. (Macro so the early-return lands in the test body.)
macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run engine integration)");
                return;
            }
        }
    };
}

const TINY: ChunkConfig = ChunkConfig { min: 64, avg: 256, max: 1024 };

fn uniq() -> String {
    // Time alone collides across parallel test threads (same nanosecond) → duplicate
    // emails. A process-wide counter makes it unique regardless of clock resolution.
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), n)
}

/// A device with its own **isolated** local tree + chunk store. (Not `temp()`, which is
/// a per-process shared dir — devices must NOT share a store, or B would never need to
/// hydrate and the data-plane would go untested.)
fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("it-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 { Vec::new() } else { p.read(ino, 0, size as u32).unwrap() }
}

/// Register a fresh library (device A) + a second device B on it; returns both clients
/// and a shared "cloud" chunk store between them.
fn two_devices(url: &str) -> (ServerClient, ServerClient, SharedDirIO) {
    let mut a = ServerClient::new(url);
    a.register(&format!("it-{}@example.com", uniq()), "device-A").expect("register A");
    let dev_b = a.create_device("device-B").expect("create device B");
    let b = ServerClient::with_token(url, dev_b.device_token);
    let cloud = SharedDirIO::new(std::env::temp_dir().join(format!("it-cloud-{}", uniq()))).unwrap();
    (a, b, cloud)
}

// ---- push → pull → hydrate, real bytes ---------------------------------------------

#[test]
fn roundtrip_pushes_and_hydrates_real_bytes() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let dir = a.mkdir(ROOT_INO, "Projects").unwrap();
    let f = a.create(dir.ino, "box.lbrn2").unwrap();
    let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push(&a, &ca, &cloud).expect("A push");

    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let proj = b.lookup(ROOT_INO, "Projects").expect("B has Projects");
    let bf = b.lookup(proj.ino, "box.lbrn2").expect("B has file");
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).expect("B hydrate");
    assert_eq!(read_all(&b, bf.ino), body, "A → cloud → B byte-for-byte");
}

// ---- delete + move propagate via a delta pull --------------------------------------

#[test]
fn delete_and_move_converge_via_delta() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let inbox = a.mkdir(ROOT_INO, "Inbox").unwrap();
    let archive = a.mkdir(ROOT_INO, "Archive").unwrap();
    for (name, body) in [("note.txt", &b"keep"[..]), ("trash.txt", &b"toss"[..])] {
        let f = a.create(inbox.ino, name).unwrap();
        a.write(f.ino, 0, body).unwrap();
        a.flush(f.ino).unwrap();
    }
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A initial push");

    let b = dev();
    let cursor = SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let b_inbox = b.lookup(ROOT_INO, "Inbox").unwrap().ino;
    let b_archive = b.lookup(ROOT_INO, "Archive").unwrap().ino;
    assert!(b.lookup(b_inbox, "trash.txt").is_ok());

    a.unlink(inbox.ino, "trash.txt").unwrap();
    a.rename(inbox.ino, "note.txt", archive.ino, "note.txt").unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push delta");

    SyncEngine::pull_changes(&b, &cb, cursor).expect("B delta pull");
    assert!(b.lookup(b_inbox, "trash.txt").is_err(), "delete propagated");
    assert!(b.lookup(b_archive, "note.txt").is_ok(), "move landed at new path");
    assert!(b.lookup(b_inbox, "note.txt").is_err(), "no duplicate at old path");
}

// ---- sequential edits fast-forward (no false conflict) -----------------------------

#[test]
fn sequential_edits_fast_forward_without_a_conflict() {
    // The non-conflict path now that the client tracks base versions: an edit whose base
    // IS current is a clean put (no conflict). Conflicts only fire on a *stale* base
    // (see offline_divergent_edit_raises_a_conflict). This guards against false positives.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let f = a.create(ROOT_INO, "notes.txt").unwrap();
    a.write(f.ino, 0, b"v1").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // Edit again on the SAME device (base is current after set_node_version) → clean.
    a.truncate(f.ino, 0).unwrap();
    a.write(f.ino, 0, b"v2 sequential").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    assert!(cb.conflicts().unwrap().conflicts.is_empty(), "sequential edits don't conflict");
    // A second device sees the latest, no conflict.
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    assert!(b.lookup(ROOT_INO, "notes.txt").is_ok());
}

// ---- hydration: only-missing + idempotent ------------------------------------------

#[test]
fn hydrate_fetches_only_missing_and_is_idempotent() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let f = a.create(ROOT_INO, "big.bin").unwrap();
    let body: Vec<u8> = (0..20_000u32).map(|i| (i * 31 % 255) as u8).collect();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push(&a, &ca, &cloud).expect("A push");

    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let bf = b.lookup(ROOT_INO, "big.bin").unwrap();
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

    // Before hydration B knows the manifest but holds none of the chunks.
    let hashes = b.node_manifest_hashes(&node_id).unwrap();
    assert!(hashes.len() > 1, "tiny chunks → many hashes");
    assert!(hashes.iter().all(|h| !b.has_chunk(h)), "no chunks cached pre-hydrate");

    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).expect("hydrate");
    assert!(hashes.iter().all(|h| b.has_chunk(h)), "all chunks present post-hydrate");
    assert_eq!(read_all(&b, bf.ino), body);

    // Re-hydrate is a no-op (nothing missing) and content still reads correctly.
    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).expect("re-hydrate is a no-op");
    assert_eq!(read_all(&b, bf.ino), body);
}

// ---- push dedup / idempotency ------------------------------------------------------

#[test]
fn re_push_of_identical_content_dedups_and_stays_consistent() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let f = a.create(ROOT_INO, "f.bin").unwrap();
    let body: Vec<u8> = (0..10_000u32).map(|i| (i % 200) as u8).collect();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();

    // Push twice. The whole-tree push mints a new commit each time; the server must
    // dedup identical chunks (second commit's `missing` is empty) and the tree must
    // stay single-file consistent — no duplicate node, content intact on B.
    SyncEngine::push(&a, &ca, &cloud).expect("push #1");
    SyncEngine::push(&a, &ca, &cloud).expect("push #2 (idempotent content)");

    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    assert_eq!(b.readdir(ROOT_INO).unwrap().iter().filter(|e| e.name == "f.bin").count(), 1, "no dup node");
    let bf = b.lookup(ROOT_INO, "f.bin").unwrap();
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).expect("hydrate");
    assert_eq!(read_all(&b, bf.ino), body);
}

// ---- auth: pairing a second device into an existing library ------------------------

#[test]
fn pairing_adds_a_device_to_an_existing_library() {
    let url = server_url!();
    let email = format!("pair-{}@example.com", uniq());

    // Device A registers (creates the account) and pushes a file.
    let mut ca = ServerClient::new(&url);
    ca.register(&email, "device-A").expect("register A");
    let cloud = SharedDirIO::new(std::env::temp_dir().join(format!("pair-cloud-{}", uniq()))).unwrap();
    let a = dev();
    let f = a.create(ROOT_INO, "paired.txt").unwrap();
    a.write(f.ino, 0, b"hello from A").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push(&a, &ca, &cloud).expect("A push");

    // A issues a pairing code; a brand-new device B redeems it (never sees A's token).
    let code = ca.create_pairing().expect("A issues code").code;
    let mut cb = ServerClient::new(&url);
    cb.pair(&email, &code, "device-B").expect("B pairs in");

    // B is now a real device on the same library — it sees A's file.
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    assert!(b.lookup(ROOT_INO, "paired.txt").is_ok(), "paired device B sees A's library");

    // A reused code can't pair again (single-use).
    assert!(ServerClient::new(&url).pair(&email, &code, "device-C").is_err(), "code is single-use");
}

// ---- file mode (executable bit) propagation ----------------------------------------

#[test]
fn executable_bit_propagates_across_devices() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A creates a script and chmod +x it.
    let a = dev();
    let f = a.create(ROOT_INO, "flash.sh").unwrap();
    a.write(f.ino, 0, b"#!/bin/sh\necho flashing").unwrap();
    a.flush(f.ino).unwrap();
    a.set_mode(f.ino, 0o755).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B pulls → the executable bit came across.
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "flash.sh").unwrap();
    assert_eq!(b.getattr(bf.ino).unwrap().mode, 0o755, "chmod +x on A is visible on B");
}

// ---- offline availability after hydration (cross-platform) --------------------------

/// A chunk transport that's "offline": every fetch fails. Used to prove a read after
/// hydration never touches the network — the bytes come from the local cache.
struct OfflineIO;
impl ChunkIO for OfflineIO {
    fn put(&self, hash: &str, _urls: &HashMap<String, String>, _bytes: Vec<u8>) -> sync_engine::Result<()> {
        Err(sync_engine::Error::NoUrl(format!("offline: {hash}")))
    }
    fn get(&self, hash: &str, _url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        Err(sync_engine::Error::NoUrl(format!("offline: {hash}")))
    }
}

#[test]
fn hydrated_file_stays_readable_offline() {
    // The offline guarantee (same on macOS/Windows/Linux — it's all the shared Provider +
    // engine): once a file is hydrated, its chunks live in the local content-addressed cache,
    // so the user can "download what they need, then go offline" and still read it. We hydrate
    // online, then point the engine at a DEAD server + a chunk source that errors, and confirm
    // a re-hydrate fetches nothing (everything's cached) and the read serves from the cache.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A pushes a multi-chunk file.
    let a = dev();
    let f = a.create(ROOT_INO, "offline.bin").unwrap();
    let body: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B pulls the tree and hydrates the file ONLINE (chunks land in B's local cache).
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "offline.bin").unwrap();
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).unwrap();
    assert_eq!(read_all(&b, bf.ino), body, "online read OK after hydrate");

    // Now go OFFLINE: an unreachable server + a chunk source that errors on every fetch.
    let dead = ServerClient::new("http://127.0.0.1:9"); // nothing listening
    // A re-hydrate must NOT contact the network — all chunks are already cached, so it finds
    // nothing missing and never calls the (failing) transport or the (dead) server.
    SyncEngine::hydrate(&b, &dead, &OfflineIO, &node_id).expect("offline re-hydrate is a no-op");
    // And the file still reads byte-for-byte, served entirely from the local cache.
    assert_eq!(read_all(&b, bf.ino), body, "hydrated file reads OFFLINE from the local cache");
}

// ---- real mtime preservation across devices ----------------------------------------

#[test]
fn real_mtime_propagates_across_devices() {
    // Option B: A's real modification time rides the commit to the server and lands on B
    // verbatim — B sees A's edit time, never B's sync time. (Models
    // `executable_bit_propagates_across_devices`; the wire carries mtime as a bigint.)
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A writes a file and stamps an obviously-old mtime (2001) via setattr/`cp -p`.
    let a = dev();
    let f = a.create(ROOT_INO, "old.txt").unwrap();
    a.write(f.ino, 0, b"authored long ago").unwrap();
    a.flush(f.ino).unwrap();
    let old_mtime: i64 = 1_000_000_000 * 1_000_000_000;
    a.set_mtime(f.ino, old_mtime).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B pulls the snapshot → A's real mtime came across (not the sync time).
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "old.txt").unwrap();
    assert_eq!(b.getattr(bf.ino).unwrap().mtime, old_mtime, "A's real mtime is visible on B, verbatim");
}

// ---- best-effort cross-device advisory lock ----------------------------------------

#[test]
fn best_effort_lock_coordinates_connected_devices() {
    let url = server_url!();
    let (ca, cb, _cloud) = two_devices(&url); // A = "device-A", B = "device-B"
    let node = format!("lock-{}", uniq());

    // A acquires the lock.
    let a = ca.lock(&node).expect("A lock");
    assert!(a.held && a.by_me, "A holds its own lock");

    // B, while connected, sees it held by A — not stolen.
    let b = cb.lock(&node).expect("B lock attempt");
    assert!(b.held && !b.by_me, "B sees A's live lock");
    assert_eq!(b.device_name.as_deref(), Some("device-A"));

    // It shows up in the active-locks list (for the browser's indicators).
    assert!(ca.locks().unwrap().locks.iter().any(|l| l.node_id == node && l.held));

    // A releases → B can now acquire.
    ca.unlock(&node).expect("A unlock");
    let b2 = cb.lock(&node).expect("B re-attempt");
    assert!(b2.by_me, "B acquires after A releases");
}

// ---- conflict: a divergent offline edit is flagged, not silently dropped ----------

#[test]
fn offline_divergent_edit_raises_a_conflict() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A creates a file and pushes (its local node now tracks version V1).
    let a = dev();
    let f = a.create(ROOT_INO, "design.scad").unwrap();
    a.write(f.ino, 0, b"base design").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B (second device) pulls the snapshot — its local node also tracks V1.
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "design.scad").unwrap();

    // A edits + pushes → server's current is now V2.
    a.truncate(f.ino, 0).unwrap();
    a.write(f.ino, 0, b"A's online change").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B edits its copy — still based on V1 (it never saw V2) — and pushes.
    b.truncate(bf.ino, 0).unwrap();
    b.write(bf.ino, 0, b"B's offline change").unwrap();
    b.flush(bf.ino).unwrap();
    SyncEngine::push_outbox(&b, &cb, &cloud).unwrap();

    // The server flagged a conflict (base V1 != current V2) instead of LWW-dropping a edit.
    let conflicts = cb.conflicts().unwrap().conflicts;
    assert_eq!(conflicts.len(), 1, "B's stale-based edit raised a conflict");
    let c = &conflicts[0];
    assert_eq!(c.name.as_deref(), Some("design.scad"));
    assert!(c.current.is_some() && c.incoming.is_some(), "both sides reported (device + time)");

    // Resolve by splitting → a new sibling conflicted-copy node, conflict cleared.
    let split = cb.resolve_conflict_split(&c.node_id).unwrap();
    assert!(!split.split_node_id.is_empty());
    assert!(cb.conflicts().unwrap().conflicts.is_empty(), "resolved after split");
}

// ---- freshness-on-access (the on-open catch-up pull) -------------------------------

#[test]
fn freshness_check_on_access_serves_latest_after_remote_change() {
    // Red-green: this is exactly what the adapter's open-hook freshness check does
    // (EngineFreshness::refresh → pull_changes). RED = a reader that hasn't synced
    // serves a stale copy of a file another device just changed; GREEN = the catch-up
    // pull brings it current before the read.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A publishes v1; B snapshots, hydrates, reads v1.
    let a = dev();
    let f = a.create(ROOT_INO, "live.txt").unwrap();
    a.write(f.ino, 0, b"version one").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let cursor = SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "live.txt").unwrap();
    let node = b.node_id_for(bf.ino).unwrap().unwrap();
    SyncEngine::hydrate(&b, &cb, &cloud, &node).unwrap();
    assert_eq!(read_all(&b, bf.ino), b"version one");

    // A overwrites to v2 and pushes; B's cursor is now behind.
    a.truncate(f.ino, 0).unwrap();
    a.write(f.ino, 0, b"version two is newer").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // RED: with no freshness check, B keeps serving the stale local copy.
    assert_eq!(read_all(&b, bf.ino), b"version one", "stale until B catches up (the gap freshness-on-open closes)");

    // GREEN: the on-access catch-up pull (+ lazy re-hydrate of the new manifest) → latest.
    SyncEngine::pull_changes(&b, &cb, cursor).expect("freshness catch-up pull");
    SyncEngine::hydrate(&b, &cb, &cloud, &node).expect("hydrate the new manifest");
    assert_eq!(read_all(&b, bf.ino), b"version two is newer", "fresh after the on-access pull");
}

// ---- offline reconnect: paging through a large backlog -----------------------------

#[test]
fn pull_changes_pages_through_a_backlog_without_dropping_entries() {
    // The reconnect bug: a device many changes behind must page the changelog. With a
    // single non-looping pull it would apply only the first page and jump its cursor to
    // head_seq, silently losing the rest. Force several pages via a small page size.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let total = 23;
    for i in 0..total {
        a.mkdir(ROOT_INO, &format!("d{i:02}")).unwrap();
    }
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push backlog");

    // B is far behind (cursor 0). Page size 5 → ~5 round-trips.
    let b = dev();
    let head = SyncEngine::pull_changes_paged(&b, &cb, 0, 5).expect("B pages the backlog");
    let names: std::collections::HashSet<String> =
        b.readdir(ROOT_INO).unwrap().into_iter().map(|e| e.name).collect();
    for i in 0..total {
        assert!(names.contains(&format!("d{i:02}")), "d{i:02} must survive paging");
    }
    assert_eq!(names.len(), total, "every backlog entry applied, none dropped");
    assert!(head > 0, "cursor advanced to head");

    // A follow-up pull from the returned cursor is a clean no-op (fully caught up).
    let head2 = SyncEngine::pull_changes_paged(&b, &cb, head, 5).expect("caught-up pull");
    assert_eq!(head2, head, "cursor stable once caught up");
}

// ---- offline writes reconcile on reconnect -----------------------------------------

#[test]
fn offline_writes_reconcile_with_remote_changes_on_reconnect() {
    // B goes offline and keeps writing (outbox accumulates) while A pushes changes B
    // hasn't seen. On reconnect B pushes its queue, then pulls A's delta — the libraries
    // converge: each device's non-conflicting files end up on both.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    a.mkdir(ROOT_INO, "shared").unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let b_cur = SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let b_shared = b.lookup(ROOT_INO, "shared").unwrap().ino;

    // --- B offline: queues local writes; A keeps pushing ---
    let f = b.create(b_shared, "from_b.txt").unwrap();
    b.write(f.ino, 0, b"made offline on B").unwrap();
    b.flush(f.ino).unwrap();

    let a_shared = a.lookup(ROOT_INO, "shared").unwrap().ino;
    let af = a.create(a_shared, "from_a.txt").unwrap();
    a.write(af.ino, 0, b"made on A while B was away").unwrap();
    a.flush(af.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // --- B reconnects: push the queue, then pull the delta ---
    SyncEngine::push_outbox(&b, &cb, &cloud).expect("B flushes its offline queue");
    SyncEngine::pull_changes(&b, &cb, b_cur).expect("B pulls A's delta");
    // And A pulls B's now-pushed work.
    SyncEngine::pull_changes(&a, &ca, b_cur).expect("A pulls B's reconnect push");

    // Both devices now hold both files.
    assert!(b.lookup(b_shared, "from_a.txt").is_ok(), "B got A's offline-era file");
    assert!(b.lookup(b_shared, "from_b.txt").is_ok(), "B kept its own offline file");
    assert!(a.lookup(a_shared, "from_b.txt").is_ok(), "A got B's queued file");
    assert!(a.lookup(a_shared, "from_a.txt").is_ok(), "A kept its own file");
}

// ---- hydrate after a conflict (manifest swapped by a pull) --------------------------

#[test]
fn hydrate_reads_the_winning_version_after_a_conflict() {
    // The path the LWW test sidesteps: after A's node is re-pointed at B's winning
    // manifest by a pull, A must be able to hydrate that new manifest's chunks (which B
    // uploaded) and read B's bytes — proving hydrate follows a manifest swap correctly.
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    let f = a.create(ROOT_INO, "doc.txt").unwrap();
    a.write(f.ino, 0, &vec![b'A'; 5000]).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let b_cur = SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "doc.txt").unwrap();
    let winner: Vec<u8> = (0..6000u32).map(|i| (i % 253) as u8).collect();
    b.truncate(bf.ino, 0).unwrap();
    b.write(bf.ino, 0, &winner).unwrap();
    b.flush(bf.ino).unwrap();
    SyncEngine::push_outbox(&b, &cb, &cloud).unwrap();

    // A pulls B's edit (A had no edit of its own here → unambiguous winner = B).
    SyncEngine::pull_changes(&a, &ca, b_cur).expect("A pulls B's edit");
    let node = a.node_id_for(f.ino).unwrap().unwrap();
    SyncEngine::hydrate(&a, &ca, &cloud, &node).expect("A hydrates the swapped-in manifest");
    assert_eq!(read_all(&a, f.ino), winner, "A reads B's winning content after hydrate");
}

// ---- cursor monotonicity / delta idempotency ---------------------------------------

#[test]
fn cursor_advances_and_redelivered_delta_is_idempotent() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let a = dev();
    a.mkdir(ROOT_INO, "d1").unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let c0 = SyncEngine::pull_snapshot(&b, &cb).expect("snapshot");

    a.mkdir(ROOT_INO, "d2").unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let c1 = SyncEngine::pull_changes(&b, &cb, c0).expect("delta 1");
    assert!(c1 >= c0, "cursor is monotonic ({c0} → {c1})");
    assert!(b.lookup(ROOT_INO, "d2").is_ok());

    // Re-applying the *same* delta (same cursor) must not duplicate or error.
    let snap = b.readdir(ROOT_INO).unwrap().len();
    let c2 = SyncEngine::pull_changes(&b, &cb, c0).expect("re-delivered delta");
    assert_eq!(c2, c1, "head_seq stable on replay");
    assert_eq!(b.readdir(ROOT_INO).unwrap().len(), snap, "replay is idempotent");
}
