//! Real-WAN parallel-vs-serial hydrate measurement (the production R2 data plane).
//!
//! Pushes a multi-chunk file to a live server, then hydrates that same file on two FRESH
//! devices (empty chunk cache) — serial (concurrency=1) vs parallel (concurrency=P) —
//! timing each. Unlike the synthetic-latency unit bench, this measures the parallel-
//! hydration win against REAL object-storage round-trips.
//!
//!   SERVER_URL=http://15.204.216.249 cargo run --release --example parallel_hydrate_wan
//!   # knobs: PHW_SIZE_KB (default 3072), PHW_PAR (default 8)

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn fresh(tag: &str, cfg: ChunkConfig) -> SqliteProvider {
    let base = std::env::temp_dir().join(format!("maker-phw-{tag}-{}", uniq()));
    std::fs::create_dir_all(&base).unwrap();
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(base.join("cache")).unwrap()), cfg).unwrap()
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let size = std::env::var("PHW_SIZE_KB").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(3072) * 1024;
    let par: usize = std::env::var("PHW_PAR").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    // Small chunks so a modest file yields many of them — the regime where per-chunk RTT
    // dominates and parallelism matters most (also what a block-delta'd CAD file looks like).
    let cfg = ChunkConfig { min: 16 * 1024, avg: 32 * 1024, max: 64 * 1024 };

    // High-entropy, position-dependent bytes so every chunk is distinct (nothing dedups away).
    let content: Vec<u8> = (0..size)
        .map(|i| {
            let x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            ((x >> 29) ^ x) as u8
        })
        .collect();

    // --- Device A: chunk + push to live storage --------------------------------------
    let a = fresh("A", cfg);
    let dir = a.mkdir(ROOT_INO, "Bench").unwrap();
    let f = a.create(dir.ino, "model.stl").unwrap();
    a.write(f.ino, 0, &content).unwrap();
    a.flush(f.ino).unwrap();

    let mut client_a = ServerClient::new(&server);
    client_a.enroll(&format!("phw-{}@example.com", uniq()), "correct horse battery staple", "A").expect("register A");
    let io_a = PresignedIO::new(&client_a);
    SyncEngine::push_outbox(&a, &client_a, &io_a).expect("A push"); // upload chunks to storage

    let a_node = a.node_id_for(f.ino).unwrap().unwrap();
    let n_chunks = a.node_manifest_hashes(&a_node).unwrap().len();
    println!(
        "pushed {} KB as {} chunks (~{} KB avg) to {}",
        size / 1024, n_chunks, cfg.avg / 1024, server
    );

    // A fresh device pulls the metadata, then hydrates the file at the given concurrency.
    let hydrate_time = |label: &str, concurrency: usize| -> std::time::Duration {
        let b = fresh(label, ChunkConfig::default());
        let dev = client_a.create_device(&format!("B-{label}")).expect("device");
        let client = ServerClient::with_token(&server, dev.device_token);
        let io = PresignedIO::new(&client);
        // pull_changes = metadata-only (NO proactive hydrate), so the timed hydrate below
        // does the real fetching. sync_tick would auto-hydrate in-scope files first.
        let cur = SyncEngine::pull_changes(&b, &client, b.cursor().unwrap()).expect("B pull");
        b.set_cursor(cur).unwrap();
        let bench = b.lookup(ROOT_INO, "Bench").expect("Bench");
        let bf = b.lookup(bench.ino, "model.stl").expect("model.stl");
        let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

        let hashes = b.node_manifest_hashes(&node_id).unwrap();
        let unique: std::collections::HashSet<_> = hashes.iter().collect();
        let missing = unique.iter().filter(|h| !b.has_chunk(h)).count();
        println!("  [{label}] manifest entries={} unique={} missing_before={}", hashes.len(), unique.len(), missing);

        let t = Instant::now();
        SyncEngine::hydrate_concurrent(&b, &client, &io, &node_id, concurrency).expect("hydrate");
        let dt = t.elapsed();
        let got = b.read(bf.ino, 0, content.len() as u32).expect("read");
        assert_eq!(got, content, "{label}: byte-for-byte");
        dt
    };

    println!("\nhydrating the same {n_chunks} chunks from storage on fresh devices (empty cache):");
    let serial = hydrate_time("serial", 1);
    println!("  serial   (P=1):  {:>7.2} s", serial.as_secs_f64());
    let parallel = hydrate_time("parallel", par);
    println!("  parallel (P={par}):  {:>7.2} s", parallel.as_secs_f64());

    let speedup = serial.as_secs_f64() / parallel.as_secs_f64().max(1e-6);
    println!(
        "\nspeedup: {:.1}x   (~{} ms/chunk serial, {} chunks)",
        speedup,
        serial.as_millis() as usize / n_chunks.max(1),
        n_chunks
    );
}
