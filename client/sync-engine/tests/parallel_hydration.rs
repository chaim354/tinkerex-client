//! Parallel chunk hydration — correctness + speedup, throttle-compat under parallelism.
//!
//! The data-plane root cause (`docs/r2-migration.md` §5): chunk GETs are serial, so a
//! 100-chunk file pays ~100 × per-chunk RTT to object storage. `SyncEngine::hydrate*`
//! now fetch a file's missing chunks over a bounded worker pool, then the file assembles
//! in manifest order from the (content-addressed) local cache.
//!
//! These suites are split:
//!   * **Hermetic** (no server): a synthetic-latency `ChunkIO` proves parallel wall-time
//!     << serial, that the chunk cache ends up byte-identical regardless of fetch order,
//!     all-or-nothing error semantics, and that the shared token bucket still caps the
//!     aggregate rate under parallel workers. These always run.
//!   * **Server-backed** (skips without `SYNC_IT_SERVER_URL`/`SERVER_URL`): a real
//!     push → parallel-hydrate → byte-for-byte read against the live changelog.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::throttle::{RateLimiter, ThrottledChunkIO};
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

const TINY: ChunkConfig = ChunkConfig {
    min: 64,
    avg: 256,
    max: 1024,
};

fn uniq() -> String {
    use std::sync::atomic::AtomicU64;
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

fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("ph-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 {
        Vec::new()
    } else {
        p.read(ino, 0, size as u32).unwrap()
    }
}

/// Build a provider whose local tree describes a multi-chunk file but whose chunk cache is
/// EMPTY (a freshly-pulled placeholder), and return `(provider, node_id, original_bytes,
/// cloud)` — `cloud` is a `SharedDirIO` holding the real bytes (the "object store"). This
/// is the exact state a `hydrate` faces: metadata present, bytes remote.
fn placeholder_with_remote_bytes(body: &[u8]) -> (SqliteProvider, String, Vec<u8>, SharedDirIO) {
    // A "source" device writes the file and pushes its chunks into the shared cloud.
    let src = dev();
    let f = src.create(ROOT_INO, "huge.bin").unwrap();
    src.write(f.ino, 0, body).unwrap();
    src.flush(f.ino).unwrap();
    let node_id = src.node_id_for(f.ino).unwrap().unwrap();
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("ph-cloud-{}", uniq()))).unwrap();
    for h in src.node_manifest_hashes(&node_id).unwrap() {
        let bytes = src.chunk_get(&h).unwrap();
        cloud.put(&h, &HashMap::new(), bytes).unwrap();
    }

    // A "fresh" device with the SAME tree (export → apply) but an EMPTY chunk cache.
    let dst = dev();
    for n in src.export_tree().unwrap() {
        dst.apply_remote(&n).unwrap();
    }
    let dst_node = dst
        .node_id_for(dst.lookup(ROOT_INO, "huge.bin").unwrap().ino)
        .unwrap()
        .unwrap();
    (dst, dst_node, body.to_vec(), cloud)
}

/// Distinct incompressible bytes so FastCDC yields MANY chunks (so concurrency matters).
fn make_body(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    let mut state = 0x9E3779B97F4A7C15u64;
    for b in v.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = (state & 0xff) as u8;
    }
    v
}

/// A `ChunkIO` decorator that sleeps `delay` on every `get` (and records concurrency) —
/// the synthetic stand-in for object-store round-trip latency. Wraps a real inner store so
/// the bytes are correct; only the timing is artificial.
struct LatentIO<I: ChunkIO> {
    inner: I,
    delay: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    gets: AtomicUsize,
    fail_hash: Mutex<Option<String>>,
}

impl<I: ChunkIO> LatentIO<I> {
    fn new(inner: I, delay: Duration) -> Self {
        Self {
            inner,
            delay,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            fail_hash: Mutex::new(None),
        }
    }
    /// Make `get` of this exact hash always fail (to test all-or-nothing semantics).
    fn fail_on(&self, hash: &str) {
        *self.fail_hash.lock().unwrap() = Some(hash.to_string());
    }
    fn peak_concurrency(&self) -> usize {
        self.max_in_flight.load(Ordering::Relaxed)
    }
    fn get_count(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }
}

impl<I: ChunkIO> ChunkIO for LatentIO<I> {
    fn put(
        &self,
        hash: &str,
        urls: &HashMap<String, String>,
        bytes: Vec<u8>,
    ) -> sync_engine::Result<()> {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
        self.gets.fetch_add(1, Ordering::Relaxed); // reuse the counter for puts in the push test
        std::thread::sleep(self.delay);
        let r = self.inner.put(hash, urls, bytes);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        r
    }
    fn get(&self, hash: &str, url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
        self.gets.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(self.delay);
        let fail = self.fail_hash.lock().unwrap().as_deref() == Some(hash);
        let r = if fail {
            Err(sync_engine::Error::NoUrl(format!(
                "injected get failure: {hash}"
            )))
        } else {
            self.inner.get(hash, url)
        };
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        r
    }
    fn needs_urls(&self) -> bool {
        self.inner.needs_urls()
    }
}

// =====================================================================================
// Hermetic: parallel is faster, correct, and bounded
// =====================================================================================

/// The headline: with artificial per-chunk latency, a parallel hydrate's wall-time is far
/// below the serial sum (≈ serial / P, modulo pool overhead). Proves the round-trips overlap.
#[test]
fn parallel_hydrate_is_much_faster_than_serial() {
    let body = make_body(40_000); // tiny CDC → dozens of chunks
    let delay = Duration::from_millis(25);

    // --- serial (concurrency 1) ---
    let (dst_s, node_s, orig, cloud_s) = placeholder_with_remote_bytes(&body);
    let io_s = LatentIO::new(cloud_s, delay);
    let t = Instant::now();
    SyncEngine::hydrate_concurrent(&dst_s, &dummy_client(), &io_s, &node_s, 1)
        .expect("serial hydrate");
    let serial = t.elapsed();
    let chunks = io_s.get_count();
    assert!(
        chunks >= 8,
        "test needs many chunks to be meaningful, got {chunks}"
    );
    assert_eq!(
        read_all(&dst_s, dst_s.lookup(ROOT_INO, "huge.bin").unwrap().ino),
        orig,
        "serial bytes ok"
    );
    assert_eq!(
        io_s.peak_concurrency(),
        1,
        "serial path never overlaps fetches"
    );

    // --- parallel (concurrency 8) ---
    let (dst_p, node_p, _orig, cloud_p) = placeholder_with_remote_bytes(&body);
    let io_p = LatentIO::new(cloud_p, delay);
    let t = Instant::now();
    SyncEngine::hydrate_concurrent(&dst_p, &dummy_client(), &io_p, &node_p, 8)
        .expect("parallel hydrate");
    let parallel = t.elapsed();
    assert_eq!(
        read_all(&dst_p, dst_p.lookup(ROOT_INO, "huge.bin").unwrap().ino),
        body,
        "parallel bytes ok"
    );

    eprintln!(
        "parallel_hydrate: {chunks} chunks @ {:?}/get — serial {serial:?}, parallel(P=8) {parallel:?} \
         (peak concurrency {})",
        delay,
        io_p.peak_concurrency()
    );
    assert!(
        io_p.peak_concurrency() >= 2,
        "workers must actually overlap"
    );
    // Generous bound to avoid CI flakiness: parallel must be at least ~2.5× faster.
    assert!(
        parallel.mul_f64(2.5) < serial,
        "parallel not meaningfully faster: serial {serial:?} vs parallel {parallel:?}"
    );
}

/// Concurrency is bounded by the configured pool size — never more than P fetches in flight.
#[test]
fn parallel_hydrate_respects_the_concurrency_bound() {
    let body = make_body(40_000);
    let (dst, node, _orig, cloud) = placeholder_with_remote_bytes(&body);
    let io = LatentIO::new(cloud, Duration::from_millis(15));
    let p = 4;
    SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, p).expect("hydrate");
    assert!(
        io.peak_concurrency() <= p,
        "peak {} exceeded the pool bound {p}",
        io.peak_concurrency()
    );
    assert!(
        io.peak_concurrency() >= 2,
        "should have actually run several at once"
    );
}

/// All-or-nothing: if any chunk fails after the fetch, the whole hydrate errors — we never
/// silently leave a half-hydrated file (the read would then serve wrong/short bytes).
#[test]
fn parallel_hydrate_fails_whole_if_any_chunk_fails() {
    let body = make_body(40_000);
    let (dst, node, _orig, cloud) = placeholder_with_remote_bytes(&body);
    let io = LatentIO::new(cloud, Duration::from_millis(2));
    // Fail one of the file's chunks.
    let hashes = dst.node_manifest_hashes(&node).unwrap();
    io.fail_on(&hashes[hashes.len() / 2]);
    let r = SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, 8);
    assert!(r.is_err(), "a failed chunk must fail the whole hydrate");
}

/// A worker panic surfaces as an error, not a hang/deadlock.
#[test]
fn parallel_hydrate_worker_panic_surfaces_as_error() {
    let body = make_body(40_000);
    let (dst, node, _orig, cloud) = placeholder_with_remote_bytes(&body);
    let io = PanicIO {
        inner: cloud,
        panic_on: dst.node_manifest_hashes(&node).unwrap()[0].clone(),
    };
    let r = SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, 8);
    assert!(
        r.is_err(),
        "a panicking worker must yield an Err, not deadlock"
    );
}

struct PanicIO {
    inner: SharedDirIO,
    panic_on: String,
}
impl ChunkIO for PanicIO {
    fn put(
        &self,
        hash: &str,
        urls: &HashMap<String, String>,
        bytes: Vec<u8>,
    ) -> sync_engine::Result<()> {
        self.inner.put(hash, urls, bytes)
    }
    fn get(&self, hash: &str, url: Option<&str>) -> sync_engine::Result<Vec<u8>> {
        if hash == self.panic_on {
            panic!("boom in worker");
        }
        self.inner.get(hash, url)
    }
    fn needs_urls(&self) -> bool {
        self.inner.needs_urls()
    }
}

/// Idempotent: a second hydrate (chunks now cached) is a no-op and stays byte-correct.
#[test]
fn parallel_hydrate_is_idempotent() {
    let body = make_body(20_000);
    let (dst, node, orig, cloud) = placeholder_with_remote_bytes(&body);
    let io = LatentIO::new(cloud, Duration::from_millis(1));
    SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, 8).unwrap();
    let first = io.get_count();
    SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, 8).unwrap();
    assert_eq!(
        io.get_count(),
        first,
        "second hydrate fetched nothing (all cached)"
    );
    assert_eq!(
        read_all(&dst, dst.lookup(ROOT_INO, "huge.bin").unwrap().ino),
        orig
    );
}

// =====================================================================================
// Throttle still caps under parallelism (shared token bucket)
// =====================================================================================

/// The download cap must hold even with P workers pulling concurrently: a shared
/// `RateLimiter` (one Arc) gates them all, so the aggregate rate ≈ the cap, NOT P × cap.
#[test]
fn parallel_hydrate_respects_the_shared_download_cap() {
    // Build a file whose total chunk bytes are known, then hydrate under a tight cap and
    // assert wall-time ≈ totalBytes/rate even though P=8 workers run concurrently.
    let body = make_body(60_000);
    let (dst, node, _orig, cloud) = placeholder_with_remote_bytes(&body);

    let total: usize = dst
        .node_manifest_hashes(&node)
        .unwrap()
        .iter()
        .map(|h| cloud_len(&cloud, h))
        .sum();

    let rate = (total as u64) / 2; // ~2 s worth of bytes total
    let down = Arc::new(RateLimiter::new(rate));
    // Near-zero per-get latency so the TIMING is dominated by the throttle, not sleeps.
    let throttled = ThrottledChunkIO::new(cloud, None, Some(down));
    let io = LatentIO::new(throttled, Duration::from_millis(0));

    let t = Instant::now();
    SyncEngine::hydrate_concurrent(&dst, &dummy_client(), &io, &node, 8).expect("hydrate");
    let elapsed = t.elapsed();

    // burst defaults to 1 s of bytes → roughly (total - rate)/rate seconds expected.
    let expected = Duration::from_secs_f64((total as f64 - rate as f64) / rate as f64);
    eprintln!(
        "shared-cap parallel hydrate: total={total}B rate={rate}B/s P=8 → {elapsed:?} (expected ≳ {expected:?})",
    );
    assert!(
        elapsed >= expected.mul_f64(0.7),
        "parallel workers bypassed the shared cap: {elapsed:?} < {expected:?} (cap not global!)"
    );
}

/// The length of a chunk as stored in the cloud (the SharedDirIO file size).
fn cloud_len(cloud: &SharedDirIO, hash: &str) -> usize {
    cloud.get(hash, None).map(|v| v.len()).unwrap_or(0)
}

/// A throwaway client — the hermetic hydrate path doesn't presign (it only would if a
/// missing chunk needed a real URL; `SharedDirIO`/`LatentIO` ignore URLs). The engine
/// still calls `download_urls`, so the client must be present but is never actually hit
/// because... see note in the hermetic helper. We point it at a bogus URL: the hermetic
/// tests that exercise the network are the server-backed ones below.
fn dummy_client() -> ServerClient {
    ServerClient::new("http://127.0.0.1:0")
}

// =====================================================================================
// Server-backed: real push → parallel hydrate → byte-for-byte
// =====================================================================================

macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run server-backed parallel hydrate)");
                return;
            }
        }
    };
}

#[test]
fn server_backed_parallel_hydrate_is_byte_for_byte() {
    let url = server_url!();
    let mut ca = ServerClient::new(&url);
    ca.register(&format!("ph-{}@example.com", uniq()), "device-A")
        .expect("register A");
    let dev_b = ca.create_device("device-B").expect("create B");
    let cb = ServerClient::with_token(&url, dev_b.device_token);
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("ph-it-cloud-{}", uniq()))).unwrap();

    let a = dev();
    let f = a.create(ROOT_INO, "model.stl").unwrap();
    let body = make_body(50_000);
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push(&a, &ca, &cloud).expect("A push");

    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let bf = b.lookup(ROOT_INO, "model.stl").unwrap();
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

    // Parallel hydrate over the real changelog/presign control plane.
    SyncEngine::hydrate_concurrent(&b, &cb, &cloud, &node_id, 8).expect("B parallel hydrate");
    assert_eq!(
        read_all(&b, bf.ino),
        body,
        "A → cloud → B parallel-hydrated byte-for-byte"
    );
}

/// The **push** path also uploads a file's missing chunks in parallel: with artificial
/// per-PUT latency, several uploads overlap (peak concurrency > 1) and the pushed bytes are
/// byte-for-byte readable back on a second device. Server-backed (push needs commit_begin/
/// finalize); `SharedDirIO` (needs_urls=false) ignores the presigned PUT URLs.
#[test]
fn server_backed_parallel_push_uploads_overlap() {
    let url = server_url!();
    let mut ca = ServerClient::new(&url);
    ca.register(&format!("ph-push-{}@example.com", uniq()), "device-A")
        .expect("register A");
    let dev_b = ca.create_device("device-B").expect("create B");
    let cb = ServerClient::with_token(&url, dev_b.device_token);
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("ph-push-cloud-{}", uniq()))).unwrap();
    let io = LatentIO::new(cloud, Duration::from_millis(20));

    let a = dev();
    let f = a.create(ROOT_INO, "big.bin").unwrap();
    let body = make_body(50_000); // many chunks → many PUTs
    a.write(f.ino, 0, &body).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push(&a, &ca, &io).expect("A parallel push");

    eprintln!(
        "parallel_push: {} PUTs, peak upload concurrency {}",
        io.get_count(),
        io.peak_concurrency()
    );
    assert!(
        io.peak_concurrency() >= 2,
        "parallel push should overlap uploads, got {}",
        io.peak_concurrency()
    );

    // And the bytes round-trip: B pulls + hydrates from the same cloud, byte-for-byte.
    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).expect("B snapshot");
    let bf = b.lookup(ROOT_INO, "big.bin").unwrap();
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    SyncEngine::hydrate(&b, &cb, &io, &node_id).expect("B hydrate");
    assert_eq!(
        read_all(&b, bf.ino),
        body,
        "parallel-pushed bytes round-trip byte-for-byte"
    );
}
