//! Chunk-store latency benchmark — measures the REAL data-plane round-trips that gate
//! sync usability against ANY S3-compatible store (R2 / SeaweedFS / MinIO) or the
//! built-in `local` backend, via the same presigned-URL path production uses.
//!
//! What it measures, per chunk size, over a small batch (so per-op overhead shows):
//!   1. presign-PUT call     — `POST /v1/commit` round-trip to mint a presigned PUT URL
//!   2. PUT round-trip       — uploading the chunk bytes to that URL (client ⇄ storage)
//!   3. presign-GET call     — `POST /v1/chunks/download` round-trip to mint a GET URL
//!   4. GET round-trip       — downloading the chunk bytes from that URL
//!   5. end-to-end hydrate   — presign-GET call + GET (what a read-on-miss actually pays)
//!
//! Bytes never flow through Rails — only the presign calls do; the PUT/GET hit storage
//! directly. This is the production transport (`PresignedIO` / `ServerClient`).
//!
//! Run it against whatever the *server* is pointed at. The server's STORAGE_BACKEND +
//! CHUNK_* env decides the store; this client only needs SERVER_URL. To benchmark R2,
//! boot the server with the R2 CHUNK_* env (see docs/r2-migration.md) — NO client change.
//!
//!   # baseline: server on the `local` backend (real bytes, on-box, no object store)
//!   STORAGE_BACKEND=local bin/rails server -p 3001
//!   SERVER_URL=http://localhost:3001 cargo run --release --example chunk_latency
//!
//!   # against R2 (the migration target): point the SERVER at R2, run the SAME command
//!   STORAGE_BACKEND=s3 CHUNK_ENDPOINT=https://<acct>.r2.cloudflarestorage.com \
//!     CHUNK_BUCKET=<bucket> CHUNK_REGION=auto \
//!     CHUNK_ACCESS_KEY_ID=<id> CHUNK_SECRET_ACCESS_KEY=<key> bin/rails server -p 3001
//!   SERVER_URL=http://localhost:3001 cargo run --release --example chunk_latency
//!
//! Knobs (env): SERVER_URL, BENCH_SIZES (comma KB list, e.g. "64,1024,4096"),
//! BENCH_BATCH (ops per size, default 8).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use sync_client::{ChunkRef, CommitBegin, ServerClient};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

/// Distinct, incompressible bytes per (size, index) so every op is a genuine round-trip
/// (a fresh content-addressed hash → never deduped away, never a cache hit).
fn make_bytes(size: usize, salt: u64) -> Vec<u8> {
    let mut v = vec![0u8; size];
    let mut state = salt.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
    for b in v.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = (state & 0xff) as u8;
    }
    v
}

fn hash_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Percentile from a sorted slice of millis (nearest-rank).
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

struct Stats {
    label: &'static str,
    min: f64,
    med: f64,
    p95: f64,
    max: f64,
    mean: f64,
    n: usize,
}

fn summarize(label: &'static str, mut samples: Vec<f64>) -> Stats {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let mean = if n == 0 { 0.0 } else { samples.iter().sum::<f64>() / n as f64 };
    Stats {
        label,
        min: samples.first().copied().unwrap_or(0.0),
        med: pct(&samples, 50.0),
        p95: pct(&samples, 95.0),
        max: samples.last().copied().unwrap_or(0.0),
        mean,
        n,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn bench_size(client: &ServerClient, size_kb: usize, batch: usize) {
    let size = size_kb * 1024;
    let mut presign_put = Vec::new();
    let mut put_rt = Vec::new();
    let mut presign_get = Vec::new();
    let mut get_rt = Vec::new();
    let mut hydrate_e2e = Vec::new();

    for i in 0..batch {
        let bytes = make_bytes(size, (size_kb as u64) << 20 | i as u64);
        let hash = hash_hex(&bytes);

        // --- (1) presign PUT: a commit_begin that returns a presigned PUT for this chunk
        let commit_id = uniq();
        let manifest = vec![ChunkRef { hash: hash.clone(), offset: 0, length: size as u64 }];
        let t = Instant::now();
        let begin = client
            .commit_begin(&CommitBegin {
                commit_id: commit_id.clone(),
                node_id: uniq(),
                parent_node_id: None, // attaches to the user's root
                name: format!("bench-{size_kb}k-{i}-{}.bin", uniq()),
                base_version_id: None,
                size: size as u64,
                manifest,
                mode: 0o644,
                mtime: 0,
            })
            .expect("commit_begin");
        presign_put.push(ms(t.elapsed()));

        let put_url = begin
            .put_urls
            .get(&hash)
            .cloned()
            .expect("server should presign a PUT for a never-before-seen chunk");

        // --- (2) PUT round-trip: upload the bytes to storage directly
        let t = Instant::now();
        client.upload_chunk(&put_url, bytes.clone()).expect("upload_chunk");
        put_rt.push(ms(t.elapsed()));

        // Finalize so the chunk is owned by this user (commit_download is ownership-gated).
        client.commit_finalize(&commit_id).expect("commit_finalize");

        // --- (3) presign GET: mint a presigned download URL for the chunk
        let t = Instant::now();
        let dl = client.download_urls(std::slice::from_ref(&hash)).expect("download_urls");
        let presign_get_ms = ms(t.elapsed());
        presign_get.push(presign_get_ms);
        let get_url = dl.urls.get(&hash).cloned().expect("server should presign a GET");

        // --- (4) GET round-trip: download the bytes from storage directly
        let t = Instant::now();
        let got = client.download_chunk(&get_url).expect("download_chunk");
        let get_ms = ms(t.elapsed());
        get_rt.push(get_ms);
        assert_eq!(got.len(), size, "round-tripped byte count must match");
        assert_eq!(hash_hex(&got), hash, "round-tripped bytes must be identical");

        // --- (5) end-to-end hydrate = presign-GET call + GET (the read-on-miss cost)
        hydrate_e2e.push(presign_get_ms + get_ms);
    }

    let rows = [
        summarize("presign-PUT (commit_begin)", presign_put),
        summarize("PUT round-trip", put_rt),
        summarize("presign-GET (chunks/download)", presign_get),
        summarize("GET round-trip", get_rt),
        summarize("hydrate e2e (presign+GET)", hydrate_e2e),
    ];

    println!("\n=== chunk size {size_kb} KB  (batch n={batch}) ===");
    println!(
        "{:<32} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "metric (ms)", "min", "med", "p95", "max", "mean"
    );
    for s in &rows {
        println!(
            "{:<32} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1}",
            s.label, s.min, s.med, s.p95, s.max, s.mean
        );
    }
    // Throughput off the median PUT/GET (a coarse but useful read of bytes/s).
    let put_med = rows[1].med;
    let get_med = rows[3].med;
    let mb = size as f64 / (1024.0 * 1024.0);
    if put_med > 0.0 {
        println!(
            "  PUT ~{:.1} MB/s   GET ~{:.1} MB/s (median)",
            mb / (put_med / 1000.0),
            mb / (get_med / 1000.0)
        );
    }
    let _ = rows[0].n; // n is part of Stats for reporting; silence dead-field lint
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let sizes: Vec<usize> = std::env::var("BENCH_SIZES")
        .unwrap_or_else(|_| "64,1024,4096".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let batch: usize =
        std::env::var("BENCH_BATCH").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

    println!("chunk_latency: server={server}  sizes={sizes:?} KB  batch={batch}");
    println!("(measures the presigned PUT/GET data plane the server is configured for —");
    println!(" point the SERVER at R2/SeaweedFS/local via STORAGE_BACKEND + CHUNK_*.)");

    let mut client = ServerClient::new(&server);
    let reg = client
        .register(&format!("bench-{}@example.com", uniq()), "bench-device")
        .expect("register (is the server up at SERVER_URL?)");
    println!("registered user_id={} (fresh library)", reg.user_id);

    for &kb in &sizes {
        bench_size(&client, kb, batch);
    }

    println!("\nDONE. Re-run with the SERVER pointed at R2 to compare (no client change).");
    println!("Tune: BENCH_SIZES=64,256,1024,4096 BENCH_BATCH=16");
}
