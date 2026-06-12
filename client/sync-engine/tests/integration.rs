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

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

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

// ---- last-writer-wins ---------------------------------------------------------------

#[test]
fn concurrent_edits_resolve_last_writer_wins() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    // A creates a file and both devices sync to it.
    let a = dev();
    let f = a.create(ROOT_INO, "shared.txt").unwrap();
    a.write(f.ino, 0, b"original").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push");

    let b = dev();
    let b_cur = SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let bf = b.lookup(ROOT_INO, "shared.txt").unwrap();

    // Both edit the *same* node, A first then B → B is the last writer.
    a.truncate(f.ino, 0).unwrap();
    a.write(f.ino, 0, b"edited by A").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push edit");

    b.truncate(bf.ino, 0).unwrap();
    b.write(bf.ino, 0, b"edited by B is the winner").unwrap();
    b.flush(bf.ino).unwrap();
    SyncEngine::push_outbox(&b, &cb, &cloud).expect("B push edit");

    // A pulls the changelog delta since its snapshot point. The delta carries A's own
    // edit then B's (later) edit in seq order, so the node converges onto B's version.
    // LWW is a *metadata* property — assert A's manifest now equals B's winning
    // manifest (content-addressed, so equal manifest_id ⇒ identical bytes). The
    // data-plane (hydrate→read) is covered separately by the roundtrip/hydrate tests.
    SyncEngine::pull_changes(&a, &ca, b_cur).expect("A pulls B's edit");
    let a_node = a.node_id_for(f.ino).unwrap().unwrap();
    let a_manifest = a.sync_node(&a_node).unwrap().unwrap().manifest_id;
    let b_manifest = b.sync_node(&a_node).unwrap().unwrap().manifest_id;
    assert_eq!(a_manifest, b_manifest, "LWW: A converged onto B's later (winning) version");
    // Sanity: B (the winner) can read its own content directly.
    assert_eq!(read_all(&b, bf.ino), b"edited by B is the winner");
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
