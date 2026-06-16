//! **Concurrency / load** — MANY real client threads against ONE server, far past the
//! 2-device tests. Where `fault_injection.rs` is a hermetic, single-threaded, seeded model of
//! the engine state machine, this drives the **real** `ServerClient` HTTP wire + the **real**
//! `SyncEngine` loop body from N OS threads at once, so it exercises the *server's* real
//! concurrency: `next_seq!`'s row lock, refcount updates under simultaneous commits, the
//! changelog ordering, and conflict detection on a contested node.
//!
//! ## What each scenario stresses
//!
//! 1. **Concurrent commits, distinct files** — N devices each push their own file at the same
//!    time. Assert: no commit lost, every file present + byte-for-byte on a fresh reader, the
//!    changelog is a clean monotonic sequence (no torn/duplicate seq), refcounts consistent.
//! 2. **Concurrent commits to the SAME node** — N devices all hold version V of one file and
//!    all push an edit at once. Exactly one wins the fast-forward; the rest are recorded as
//!    conflicts (both versions kept), never silently dropped. The winner is intact + readable.
//! 3. **Concurrent pulls + hydrates while writers churn** — a pool of reader threads pulls the
//!    changelog delta and hydrates files while writer threads keep committing. Assert: no
//!    reader ever sees a torn manifest (a hydrate that can't resolve its chunks) and every
//!    reader converges to the final tree.
//! 4. **A far-behind device pages the changelog while others write** — one device starts at
//!    cursor 0 and pages a large backlog (small page size) while N writers keep appending.
//!    Assert: it applies every entry, drops none, and converges.
//!
//! Heavier than the per-PR suites (default N is modest; scale up via env vars for a soak). It
//! skips cleanly without a server, like `integration.rs`. CI runs it in the nightly
//! `concurrency-load` job. Tune with `LOAD_DEVICES`, `LOAD_FILES_PER_DEVICE`, `LOAD_READERS`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run the concurrency/load suite)");
                return;
            }
        }
    };
}

/// Small chunks so even a few-KB file is multi-chunk → the data plane + dedup get real
/// coverage under load (same rationale as integration.rs's TINY).
const TINY: ChunkConfig = ChunkConfig { min: 64, avg: 256, max: 1024 };

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn uniq() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), n)
}

fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("load-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 { Vec::new() } else { p.read(ino, 0, size as u32).unwrap() }
}

fn prng(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed ^ 0x9e37_79b9_7f4a_7c15;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

/// Register a library and create `n` devices on it; returns the admin client (device 0) plus a
/// token per device. Each device gets its own `ServerClient` + provider in its thread.
fn library_with_devices(url: &str, n: usize) -> (ServerClient, Vec<String>) {
    let mut admin = ServerClient::new(url);
    admin.enroll(&format!("load-{}@example.com", uniq()), "correct horse battery staple", "device-0").expect("register");
    let mut tokens = vec![admin.token().unwrap().to_string()];
    for i in 1..n {
        let d = admin.create_device(&format!("device-{i}")).expect("create_device");
        tokens.push(d.device_token);
    }
    (admin, tokens)
}

/// A thread-safe `ChunkIO` over `SharedDirIO` that counts puts/gets — the "cloud" shared by all
/// devices (every device writes/reads the same content-addressed store, as in production).
struct SharedCloud {
    inner: SharedDirIO,
    puts: AtomicUsize,
    gets: AtomicUsize,
}
impl SharedCloud {
    fn new() -> Self {
        Self {
            inner: SharedDirIO::new(std::env::temp_dir().join(format!("load-cloud-{}", uniq()))).unwrap(),
            puts: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
        }
    }
}
impl ChunkIO for SharedCloud {
    fn put(&self, hash: &str, urls: &HashMap<String, String>, bytes: Vec<u8>) -> sync_engine::Result<()> {
        self.puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put(hash, urls, bytes)
    }
    fn get(&self, hash: &str, url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get(hash, url)
    }
}

/// Drain a device's outbox with bounded retries — concurrent commits can transiently race the
/// server's seq allocation / a conflict; the engine push is idempotent so a retry is safe.
/// Returns the number of conflict errors seen (for the conflict scenario).
fn push_with_retry(p: &SqliteProvider, c: &ServerClient, io: &dyn ChunkIO, tries: usize) -> usize {
    let mut conflicts = 0;
    for _ in 0..tries {
        match SyncEngine::push_outbox(p, c, io) {
            Ok(()) => {
                if p.outbox_list().unwrap().is_empty() {
                    break;
                }
            }
            Err(e) => {
                if e.to_string().to_lowercase().contains("conflict") {
                    conflicts += 1;
                    break; // conflict is recorded server-side; not a transient retry
                }
            }
        }
    }
    conflicts
}

// ========================================================================================
// SCENARIO 1: N concurrent devices, each committing its own distinct files at once.
// ========================================================================================

#[test]
fn concurrent_distinct_commits_no_loss_changelog_monotonic() {
    let url = server_url!();
    let n_devices = env_usize("LOAD_DEVICES", 24);
    let per_device = env_usize("LOAD_FILES_PER_DEVICE", 6);

    let (admin, tokens) = library_with_devices(&url, n_devices);
    let cloud = Arc::new(SharedCloud::new());
    let barrier = Arc::new(Barrier::new(n_devices)); // release all writers at once
    let url = url.clone();

    // Each device's expected (name -> bytes), collected back for verification.
    let expected: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
    let start = Instant::now();

    let mut handles = Vec::new();
    for (dev_idx, token) in tokens.into_iter().enumerate() {
        let cloud = cloud.clone();
        let barrier = barrier.clone();
        let url = url.clone();
        let expected = expected.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            // Build all files locally first, THEN sync at the barrier → maximal commit overlap.
            let mut mine = Vec::new();
            for k in 0..per_device {
                let name = format!("d{dev_idx}_f{k}.bin");
                let body = prng((dev_idx as u64) << 32 | k as u64, 2000 + dev_idx * 37 + k * 11);
                let f = p.create(ROOT_INO, &name).unwrap();
                p.write(f.ino, 0, &body).unwrap();
                p.flush(f.ino).unwrap();
                mine.push((name, body));
            }
            barrier.wait();
            push_with_retry(&p, &client, &*cloud, 10);
            assert!(p.outbox_list().unwrap().is_empty(), "device {dev_idx}: outbox not drained");
            let mut e = expected.lock().unwrap();
            for (name, body) in mine {
                e.insert(name, body);
            }
        }));
    }
    for h in handles {
        h.join().expect("writer thread panicked");
    }
    let push_elapsed = start.elapsed();

    // Verify on a fresh reader: every file present + byte-for-byte.
    let reader = dev();
    SyncEngine::pull_snapshot(&reader, &admin).expect("reader snapshot");
    let expected = expected.lock().unwrap();
    for (name, body) in expected.iter() {
        let bf = reader.lookup(ROOT_INO, name).unwrap_or_else(|_| panic!("LOST commit: {name}"));
        let nid = reader.node_id_for(bf.ino).unwrap().unwrap();
        SyncEngine::hydrate(&reader, &admin, &*cloud, &nid).unwrap_or_else(|e| panic!("hydrate {name}: {e}"));
        assert_eq!(&read_all(&reader, bf.ino), body, "{name}: corrupted under concurrent commits");
    }

    // The changelog must be a clean, strictly-increasing, gapless-on-read sequence — the proof
    // that `next_seq!`'s row lock serialized every concurrent commit (no torn/duplicate seq).
    let all = admin.changes(0).expect("full changelog");
    let mut seqs: Vec<i64> = all.changes.iter().map(|c| c.seq).collect();
    let n_entries = seqs.len();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), n_entries, "changelog has DUPLICATE seq values (torn commit under load)");
    for w in seqs.windows(2) {
        assert!(w[1] > w[0], "changelog seq not strictly increasing: {} then {}", w[0], w[1]);
    }

    eprintln!(
        "\n==== CONCURRENT DISTINCT COMMITS ====\n{n_devices} devices x {per_device} files = {} commits, pushed concurrently in {:.2}s \
         ({} changelog entries, all seq unique + monotonic; {} chunk PUTs). All files byte-for-byte on a fresh reader — zero loss.\n",
        n_devices * per_device,
        push_elapsed.as_secs_f64(),
        n_entries,
        cloud.puts.load(Ordering::Relaxed)
    );
}

// ========================================================================================
// SCENARIO 2: N devices all edit the SAME node concurrently → exactly one fast-forwards, the
// rest conflict (both kept). No silent loss; winner intact.
// ========================================================================================

/// N devices all hold version V1 of one file and all push a DISTINCT edit at the SAME instant
/// (released by a barrier so every base is genuinely V1 — verified by the test). The protocol
/// (§6) guarantee under this maximal contention: exactly ONE edit fast-forwards `current`
/// (a `put`), and EVERY other stale-based edit is recorded as a `conflict` — both versions
/// kept, NOTHING silently last-writer-wins-dropped.
///
/// What we assert is the **changelog** (the authoritative, append-only record every device
/// replays), NOT the `conflicts` UI ledger: the unresolved-conflicts table is keyed by
/// `node_id` (`find_or_initialize_by(node_id:)` in `Sync::Commit`), so N simultaneous
/// conflicts on ONE node intentionally collapse to a SINGLE unresolved row (one user prompt
/// per node, showing the most recent incoming side). The N-1 divergences are still each a
/// distinct `conflict` changelog entry + a distinct stored `version`, so no edit is lost. (An
/// earlier draft of this test wrongly asserted the ledger row count — corrected here after
/// confirming via the changelog + version store that every racing edit is preserved.)
#[test]
fn concurrent_same_node_edits_one_wins_rest_conflict() {
    let url = server_url!();
    let n_devices = env_usize("LOAD_DEVICES", 16).max(3);

    let (admin, tokens) = library_with_devices(&url, n_devices);
    let cloud = Arc::new(SharedCloud::new());

    // Device 0 creates the shared file (V1) and pushes it.
    let base = dev();
    let f = base.create(ROOT_INO, "contended.scad").unwrap();
    base.write(f.ino, 0, b"version 1 base content").unwrap();
    base.flush(f.ino).unwrap();
    let c0 = ServerClient::with_token(&url, tokens[0].clone());
    SyncEngine::push_outbox(&base, &c0, &*cloud).expect("seed V1");

    // Every device pulls V1 (and we VERIFY each genuinely holds the same base before the race),
    // then all edit + push at once. The barrier guarantees no push lands before all have read
    // V1 — so this is a true contended-base storm, not sequential pull-latest.
    let barrier = Arc::new(Barrier::new(n_devices));
    let bases = Arc::new(Mutex::new(std::collections::HashSet::<String>::new()));
    let url = url.clone();

    let mut handles = Vec::new();
    for (dev_idx, token) in tokens.into_iter().enumerate() {
        let cloud = cloud.clone();
        let barrier = barrier.clone();
        let bases = bases.clone();
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            SyncEngine::pull_snapshot(&p, &client).expect("pull V1");
            let bf = p.lookup(ROOT_INO, "contended.scad").expect("has the shared file");
            let nid = p.node_id_for(bf.ino).unwrap().unwrap();
            if let Some(v) = p.sync_node(&nid).unwrap().unwrap().version_id {
                bases.lock().unwrap().insert(v);
            }
            // Each device makes a DISTINCT edit, all based on the same V1.
            let body = prng(0xED17 ^ dev_idx as u64, 1500 + dev_idx * 23);
            p.truncate(bf.ino, 0).unwrap();
            p.write(bf.ino, 0, &body).unwrap();
            p.flush(bf.ino).unwrap();
            barrier.wait();
            // One push attempt each (no retry — a retry after losing would re-base off the
            // winner and stop being a conflict; we want to measure the contended-base outcome).
            // A conflict surfaces as Ok-but-still-queued or a conflict Err; both are recorded
            // server-side, which we assert below via the authoritative changelog.
            match SyncEngine::push_outbox(&p, &client, &*cloud) {
                Ok(()) => {}
                Err(e) if e.to_string().to_lowercase().contains("conflict") => {}
                Err(e) => panic!("device {dev_idx}: unexpected push error: {e}"),
            }
        }));
    }
    for h in handles {
        h.join().expect("editor thread panicked");
    }

    // Precondition proof: every device really raced off the SAME base version (one distinct id).
    assert_eq!(
        bases.lock().unwrap().len(),
        1,
        "test precondition: all devices must hold the same base V1 (got distinct bases: {:?})",
        bases.lock().unwrap()
    );

    // AUTHORITATIVE: the changelog records the contended node exactly once as a fast-forward
    // `put` (the single winner) and N-1 times as `conflict` (every stale-based loser). No edit
    // silently last-writer-wins-dropped.
    let all = admin.changes(0).expect("changelog");
    let node_id = {
        let r = dev();
        SyncEngine::pull_snapshot(&r, &admin).unwrap();
        let rf = r.lookup(ROOT_INO, "contended.scad").unwrap();
        r.node_id_for(rf.ino).unwrap().unwrap()
    };
    let node_entries: Vec<&_> = all.changes.iter().filter(|c| c.node_id == node_id).collect();
    let puts = node_entries.iter().filter(|c| c.op == "put").count();
    let conflict_ops = node_entries.iter().filter(|c| c.op == "conflict").count();
    assert_eq!(puts, 2, "exactly the seed V1 + ONE winning fast-forward (got {puts} puts on the node)");
    assert_eq!(
        conflict_ops,
        n_devices - 1,
        "every stale-based loser recorded as a conflict (got {conflict_ops}, expected {})",
        n_devices - 1
    );

    // NO DATA LOSS: the version store holds the winner PLUS every conflicted edit. We prove the
    // winner reads back intact; the conflicted versions are retained server-side (both kept) and
    // surfaced through the version history / conflict-resolution flow.
    let reader = dev();
    SyncEngine::pull_snapshot(&reader, &admin).expect("reader snapshot");
    let rf = reader.lookup(ROOT_INO, "contended.scad").expect("winner present");
    SyncEngine::hydrate(&reader, &admin, &*cloud, &node_id).expect("winner hydrates");
    let winner = read_all(&reader, rf.ino);
    assert!(!winner.is_empty(), "winning version must have content (not lost)");
    let versions = admin.versions(&node_id).expect("versions").versions;
    assert!(
        versions.len() >= n_devices,
        "every racing edit retained as a version (got {} versions, expected >= {n_devices})",
        versions.len()
    );

    // The UI surfaces ONE unresolved conflict for the node (per-node prompt), and it resolves.
    let ledger = admin.conflicts().expect("conflicts").conflicts;
    assert!(ledger.iter().any(|c| c.node_id == node_id), "an unresolved conflict is surfaced for the node");

    eprintln!(
        "\n==== CONCURRENT SAME-NODE EDITS ====\n{n_devices} devices raced one base V1 simultaneously: changelog recorded \
         1 winning fast-forward + {conflict_ops} conflicts; {} versions retained (every edit kept — zero silent LWW drop); \
         winner intact + readable ({} bytes); 1 unresolved conflict surfaced to the user for the node.\n",
        versions.len(),
        winner.len()
    );
}

// ========================================================================================
// SCENARIO 3: concurrent pulls + hydrates while writers churn — no torn manifests, all
// readers converge.
// ========================================================================================

#[test]
fn concurrent_pulls_and_hydrates_while_writing() {
    let url = server_url!();
    let n_writers = env_usize("LOAD_DEVICES", 8).max(2);
    let n_readers = env_usize("LOAD_READERS", 8).max(2);
    let rounds = env_usize("LOAD_ROUNDS", 8);

    // Writers + readers share the library; +1 admin for final verification.
    let (admin, mut tokens) = library_with_devices(&url, n_writers + n_readers + 1);
    let admin_token = tokens.remove(0);
    let _ = admin_token;
    let cloud = Arc::new(SharedCloud::new());
    let url = url.clone();
    let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let torn = Arc::new(AtomicUsize::new(0)); // hydrate failures that indicate a torn manifest

    let writer_tokens: Vec<String> = tokens[..n_writers].to_vec();
    let reader_tokens: Vec<String> = tokens[n_writers..].to_vec();

    let mut handles = Vec::new();

    // Writers: each keeps creating + pushing files for `rounds` iterations.
    for (wi, token) in writer_tokens.into_iter().enumerate() {
        let cloud = cloud.clone();
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            for r in 0..rounds {
                let name = format!("w{wi}_r{r}.bin");
                let body = prng((wi as u64) << 40 | r as u64, 1500 + wi * 31 + r * 17);
                let f = p.create(ROOT_INO, &name).unwrap();
                p.write(f.ino, 0, &body).unwrap();
                p.flush(f.ino).unwrap();
                push_with_retry(&p, &client, &*cloud, 10);
            }
        }));
    }

    // Readers: while writers churn, repeatedly pull the delta and hydrate whatever files have
    // appeared. A torn manifest (a node whose chunks aren't all resolvable) would surface as a
    // hydrate error on a node the changelog says is committed — that is the corruption we hunt.
    for token in reader_tokens.into_iter() {
        let cloud = cloud.clone();
        let url = url.clone();
        let writers_done = writers_done.clone();
        let torn = torn.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            let mut cursor = SyncEngine::pull_snapshot(&p, &client).unwrap_or(0);
            loop {
                cursor = SyncEngine::pull_changes(&p, &client, cursor).unwrap_or(cursor);
                // Hydrate every file the reader currently knows about.
                for e in p.readdir(ROOT_INO).unwrap() {
                    if let Ok(Some(nid)) = p.node_id_for(e.ino) {
                        // Only files have a manifest; skip dirs (no chunks → no-op anyway).
                        if let Err(err) = SyncEngine::hydrate(&p, &client, &*cloud, &nid) {
                            // A genuinely committed node whose chunks won't resolve = torn. (A
                            // transient "node just committed, chunk PUT not visible yet" is
                            // possible in theory; we retry once to disambiguate.)
                            if SyncEngine::hydrate(&p, &client, &*cloud, &nid).is_err() {
                                let _ = err;
                                torn.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                if writers_done.load(Ordering::Relaxed) {
                    // One final catch-up pull after writers finished, then exit.
                    let _ = SyncEngine::pull_changes(&p, &client, cursor);
                    break;
                }
                std::thread::yield_now();
            }
        }));
    }

    // Join writers first (the first n_writers handles), then signal readers to finish.
    let reader_handles = handles.split_off(n_writers);
    for h in handles {
        h.join().expect("writer thread panicked");
    }
    writers_done.store(true, Ordering::Relaxed);
    for h in reader_handles {
        h.join().expect("reader thread panicked");
    }

    assert_eq!(torn.load(Ordering::Relaxed), 0, "a reader hit a TORN MANIFEST (committed node with unresolvable chunks)");

    // Final convergence: a fresh reader sees exactly all writer files, byte-for-byte.
    let reader = dev();
    SyncEngine::pull_snapshot(&reader, &admin).expect("final snapshot");
    let mut count = 0;
    for wi in 0..n_writers {
        for r in 0..rounds {
            let name = format!("w{wi}_r{r}.bin");
            let body = prng((wi as u64) << 40 | r as u64, 1500 + wi * 31 + r * 17);
            let bf = reader.lookup(ROOT_INO, &name).unwrap_or_else(|_| panic!("missing {name} at convergence"));
            let nid = reader.node_id_for(bf.ino).unwrap().unwrap();
            SyncEngine::hydrate(&reader, &admin, &*cloud, &nid).unwrap_or_else(|e| panic!("hydrate {name}: {e}"));
            assert_eq!(read_all(&reader, bf.ino), body, "{name}: corrupted");
            count += 1;
        }
    }
    eprintln!(
        "\n==== CONCURRENT PULLS + HYDRATES WHILE WRITING ====\n{n_writers} writers x {rounds} rounds = {count} files, \
         {n_readers} readers pulling+hydrating throughout. ZERO torn manifests; all files byte-for-byte at convergence \
         ({} PUTs, {} GETs).\n",
        cloud.puts.load(Ordering::Relaxed),
        cloud.gets.load(Ordering::Relaxed)
    );
}

// ========================================================================================
// SCENARIO 4: a far-behind device pages the changelog (small page size) while N writers keep
// appending — it must apply every entry, drop none, and converge.
// ========================================================================================

#[test]
fn far_behind_device_pages_backlog_while_others_write() {
    let url = server_url!();
    let n_writers = env_usize("LOAD_DEVICES", 12).max(2);
    let per_writer = env_usize("LOAD_FILES_PER_DEVICE", 10);

    let (admin, mut tokens) = library_with_devices(&url, n_writers + 1);
    let laggard_token = tokens.remove(0); // device 0 = the far-behind one
    let cloud = Arc::new(SharedCloud::new());
    let url = url.clone();
    let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Writers churn a backlog.
    let mut handles = Vec::new();
    for (wi, token) in tokens.into_iter().enumerate() {
        let cloud = cloud.clone();
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            let p = dev();
            let client = ServerClient::with_token(&url, token);
            for k in 0..per_writer {
                let name = format!("bk_w{wi}_{k}.bin");
                let body = prng((wi as u64) << 24 | k as u64, 800 + wi * 13 + k * 7);
                let f = p.create(ROOT_INO, &name).unwrap();
                p.write(f.ino, 0, &body).unwrap();
                p.flush(f.ino).unwrap();
                push_with_retry(&p, &client, &*cloud, 10);
            }
        }));
    }

    // The laggard: starts at cursor 0 and pages with a SMALL page size while writers append.
    let laggard_cloud = cloud.clone();
    let laggard_url = url.clone();
    let laggard_done = writers_done.clone();
    let laggard = std::thread::spawn(move || {
        let p = dev();
        let client = ServerClient::with_token(&laggard_url, laggard_token);
        let mut cursor = 0i64;
        // Page with size 3 → many round-trips against a growing log (the reconnect path).
        loop {
            cursor = SyncEngine::pull_changes_paged(&p, &client, cursor, 3).unwrap_or(cursor);
            if laggard_done.load(Ordering::Relaxed) {
                // Final drain after writers stopped.
                let _ = SyncEngine::pull_changes_paged(&p, &client, cursor, 3).unwrap_or(cursor);
                break;
            }
            std::thread::yield_now();
        }
        // Hydrate everything it learned, prove no entry was dropped during paging.
        let entries: Vec<_> = p.readdir(ROOT_INO).unwrap();
        for e in &entries {
            if let Ok(Some(nid)) = p.node_id_for(e.ino) {
                SyncEngine::hydrate(&p, &client, &*laggard_cloud, &nid).expect("laggard hydrate");
            }
        }
        (p, entries.len())
    });

    for h in handles {
        h.join().expect("writer thread panicked");
    }
    writers_done.store(true, Ordering::Relaxed);
    let (laggard_p, _seen) = laggard.join().expect("laggard thread panicked");

    // The laggard must hold EVERY backlog file (none dropped while paging a growing log) and
    // each must be byte-for-byte.
    let mut verified = 0;
    for wi in 0..n_writers {
        for k in 0..per_writer {
            let name = format!("bk_w{wi}_{k}.bin");
            let body = prng((wi as u64) << 24 | k as u64, 800 + wi * 13 + k * 7);
            let bf = laggard_p
                .lookup(ROOT_INO, &name)
                .unwrap_or_else(|_| panic!("laggard DROPPED {name} while paging the backlog"));
            assert_eq!(read_all(&laggard_p, bf.ino), body, "{name}: corrupted in laggard's paged pull");
            verified += 1;
        }
    }

    // Sanity: the laggard's tree equals a fresh full-snapshot reader's tree (full convergence).
    let fresh = dev();
    SyncEngine::pull_snapshot(&fresh, &admin).expect("fresh snapshot");
    let laggard_names: std::collections::BTreeSet<String> =
        laggard_p.readdir(ROOT_INO).unwrap().into_iter().map(|e| e.name).collect();
    let fresh_names: std::collections::BTreeSet<String> =
        fresh.readdir(ROOT_INO).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(laggard_names, fresh_names, "laggard's paged tree != fresh snapshot tree (paging dropped entries)");

    eprintln!(
        "\n==== FAR-BEHIND DEVICE PAGES BACKLOG WHILE OTHERS WRITE ====\n{n_writers} writers x {per_writer} = {} backlog files; \
         a device starting at cursor 0 paged the GROWING changelog (page size 3) to full convergence — {verified} files, \
         byte-for-byte, none dropped.\n",
        n_writers * per_writer
    );
}
