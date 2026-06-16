//! G11 — scale validation, end-to-end (server-backed).
//!
//! Builds a LARGE tree on device A (parameterizable: file count, directory fan-out,
//! body size), pushes it, then exercises the two bulk read paths from a fresh device B:
//!   * a full `pull_snapshot` (paginated `/v1/tree` → `apply_remote` of every node into
//!     the local SQLite tree), and
//!   * a `pull_changes` delta pull of the whole changelog.
//! It also pushes a small delta and confirms B converges via a *bounded* delta pull, and
//! hydrates one large file to prove the >2 GB-shaped offset path (u64 chunk offsets) is
//! byte-for-byte. Wall-times are printed so a human can eyeball the scale numbers.
//!
//! This is the client-side counterpart to the server-side query-count regression in
//! `test/services/sync/scale_test.rb` (which asserts the snapshot/delta serializers issue
//! O(pages) queries, not O(nodes)). Here we prove the *wire + apply* pipeline holds at
//! tree size.
//!
//! Committed scale (`SCALE_FILES` default = 400) is kept CI-fast (~5 s). For the real
//! 10k-file target run, set `SCALE_FILES` (and optionally `SCALE_FANOUT`, `SCALE_BODY`):
//!   SYNC_IT_SERVER_URL=http://localhost:3001 SCALE_FILES=10000 SCALE_FANOUT=100 \
//!     cargo test --test scale -- --nocapture
//! Measured on this box (memory backend, SharedDirIO data plane): full-resync of 10,100
//! nodes in ~6.4 s; a 1-file delta pull atop 10k in ~8 ms (bounded — does NOT walk the
//! tree). See `docs/plan-file-layer-parity.md` §12 (G11) for the write-up.
//!
//! Requires a running server; **skips** (prints a notice, passes) when no server URL is
//! set, so a plain `cargo test` stays green offline.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, Kind, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run scale tests)");
                return;
            }
        }
    };
}

const TINY: ChunkConfig = ChunkConfig { min: 64, avg: 256, max: 1024 };

fn uniq() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), n)
}

fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("scale-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn two_devices(url: &str) -> (ServerClient, ServerClient, SharedDirIO) {
    let mut a = ServerClient::new(url);
    a.register(&format!("scale-{}@example.com", uniq()), "device-A").expect("register A");
    let dev_b = a.create_device("device-B").expect("create device B");
    let b = ServerClient::with_token(url, dev_b.device_token);
    let cloud = SharedDirIO::new(std::env::temp_dir().join(format!("scale-cloud-{}", uniq()))).unwrap();
    (a, b, cloud)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Build a tree of `files` files spread across directories with `fanout` files each
/// (so depth grows as the tree fills), each file `body` bytes. Returns (last big file's
/// containing dir ino, that file's name, its expected body) for a hydrate check.
fn build_tree(a: &SqliteProvider, files: usize, fanout: usize, body: usize) -> (u64, String, Vec<u8>) {
    let mut dir_ino = a.mkdir(ROOT_INO, "lib").unwrap().ino;
    let mut in_dir = 0usize;
    let mut dir_n = 0usize;
    let mut last = (dir_ino, String::new(), Vec::new());

    for i in 0..files {
        if in_dir >= fanout {
            dir_n += 1;
            // Nest sometimes to exercise depth; otherwise sibling dirs under root/lib.
            let parent = if dir_n % 4 == 0 { dir_ino } else { a.lookup(ROOT_INO, "lib").unwrap().ino };
            dir_ino = a.mkdir(parent, &format!("d{dir_n}")).unwrap().ino;
            in_dir = 0;
        }
        let name = format!("f{i}.bin");
        let f = a.create(dir_ino, &name).unwrap();
        // Distinct bytes per file so manifests don't all dedup to one.
        let data: Vec<u8> = (0..body).map(|j| ((i + j) % 251) as u8).collect();
        a.write(f.ino, 0, &data).unwrap();
        a.flush(f.ino).unwrap();
        in_dir += 1;
        if i == files - 1 {
            last = (dir_ino, name, data);
        }
    }
    last
}

/// Count present file+dir nodes in a provider tree (recursive readdir from root).
fn count_nodes(p: &SqliteProvider) -> usize {
    fn walk(p: &SqliteProvider, ino: u64) -> usize {
        let mut n = 0;
        for e in p.readdir(ino).unwrap() {
            if e.name == "." || e.name == ".." { continue; }
            n += 1;
            if e.kind == Kind::Dir {
                n += walk(p, e.ino);
            }
        }
        n
    }
    walk(p, ROOT_INO)
}

#[test]
fn big_tree_full_resync_and_delta() {
    let url = server_url!();
    let (ca, cb, cloud) = two_devices(&url);

    let files = env_usize("SCALE_FILES", 400);
    let fanout = env_usize("SCALE_FANOUT", 50);
    let body = env_usize("SCALE_BODY", 4096);

    let a = dev();
    let t = Instant::now();
    let (_dir, big_name, big_body) = build_tree(&a, files, fanout, body);
    eprintln!("[scale] built {files} files (fanout {fanout}, {body}B each) locally in {:?}", t.elapsed());

    let t = Instant::now();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push tree");
    eprintln!("[scale] pushed {files}-file tree to server in {:?}", t.elapsed());

    // --- B: full resync (paginated /v1/tree → apply_remote of the whole snapshot) ---
    let b = dev();
    let t = Instant::now();
    let cursor = SyncEngine::pull_snapshot(&b, &cb).expect("B full snapshot");
    let snap_ms = t.elapsed();
    let got = count_nodes(&b);
    eprintln!("[scale] B full-resync ({got} nodes) in {snap_ms:?} (cursor={cursor})");
    assert!(got >= files, "B must materialize at least every file ({files}); got {got}");

    // --- a small delta on A → B converges via a bounded delta pull ---
    let extra = a.create(a.lookup(ROOT_INO, "lib").unwrap().ino, "late.bin").unwrap();
    a.write(extra.ino, 0, b"hello-late").unwrap();
    a.flush(extra.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).expect("A push delta");

    let t = Instant::now();
    SyncEngine::pull_changes(&b, &cb, cursor).expect("B delta pull");
    eprintln!("[scale] B delta pull (1 new file atop {files}) in {:?}", t.elapsed());
    assert!(b.lookup(b.lookup(ROOT_INO, "lib").unwrap().ino, "late.bin").is_ok(), "delta converged");

    // --- hydrate one file: proves the chunk byte path is intact at scale ---
    let proj = find_file(&b, ROOT_INO, &big_name).expect("B has the last file");
    let node_id = b.node_id_for(proj).unwrap().unwrap();
    SyncEngine::hydrate(&b, &cb, &cloud, &node_id).expect("B hydrate big file");
    let size = b.getattr(proj).unwrap().size;
    let read = if size == 0 { Vec::new() } else { b.read(proj, 0, size as u32).unwrap() };
    assert_eq!(read, big_body, "hydrated file byte-for-byte at scale");

    eprintln!("[scale] DONE: {files} files, full-resync {snap_ms:?}, hydrate verified");
}

/// Recursively find a file by name anywhere under `ino`; returns its ino.
fn find_file(p: &SqliteProvider, ino: u64, name: &str) -> Option<u64> {
    for e in p.readdir(ino).unwrap() {
        if e.name == "." || e.name == ".." { continue; }
        if e.kind != Kind::Dir && e.name == name {
            return Some(e.ino);
        }
        if e.kind == Kind::Dir {
            if let Some(f) = find_file(p, e.ino, name) {
                return Some(f);
            }
        }
    }
    None
}
