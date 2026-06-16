//! The daemon **loop primitives** end-to-end: a local write enqueues the outbox, a
//! `sync_tick` drains it to the server, and a second device's `sync_tick` pulls + reads
//! the bytes back. This is what the running `maker-syncd` loop does each interval —
//! here driven explicitly so it can assert each step.
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example daemon_loop
//!
//! Set CHUNK_DIR to a shared dir to stand in for the cloud (else a temp dir). With a
//! live SeaweedFS server + `PresignedIO` this same flow is the real transport.

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

fn uniq() -> String {
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn provider(tag: &str) -> SqliteProvider {
    let base = std::env::temp_dir().join(format!("maker-loop-{tag}-{}", uniq()));
    std::fs::create_dir_all(&base).unwrap();
    let db = base.join("meta.db");
    let cache = base.join("cache");
    SqliteProvider::open(&db, Box::new(LocalChunkStore::new(cache).unwrap())).unwrap()
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let cloud_dir = std::env::var("CHUNK_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join(format!("maker-cloud-{}", uniq()))
            .to_string_lossy()
            .into()
    });
    let io = SharedDirIO::new(&cloud_dir).unwrap();
    println!("cloud (shared chunk store) = {cloud_dir}");

    // --- Device A: a local write enqueues the outbox (no explicit sync calls) ---
    let a = provider("A");
    let dir = a.mkdir(ROOT_INO, "Projects").unwrap();
    let f = a.create(dir.ino, "box.lbrn2").unwrap();
    let content: Vec<u8> = b"LightBurn project payload \x00\x01\x02 "
        .iter()
        .cycle()
        .take(40_000)
        .copied()
        .collect();
    a.write(f.ino, 0, &content).unwrap();
    a.flush(f.ino).unwrap();
    let outbox = a.outbox_list().unwrap();
    assert_eq!(
        outbox.len(),
        2,
        "mkdir + flush enqueued the dir and the file"
    );
    println!(
        "✓ A: local write enqueued {} outbox entries (dir + file)",
        outbox.len()
    );

    // --- Device A: one sync_tick drains the outbox to the server + cloud --------
    let mut client_a = ServerClient::new(&server);
    client_a
        .enroll(
            &format!("loop-{}@example.com", uniq()),
            "correct horse battery staple",
            "device-A",
        )
        .expect("register A");
    let cursor_a =
        SyncEngine::sync_tick(&a, &client_a, &io, a.cursor().unwrap()).expect("A sync_tick");
    a.set_cursor(cursor_a).unwrap();
    assert!(
        a.outbox_list().unwrap().is_empty(),
        "outbox drained after push"
    );
    let n_chunks = std::fs::read_dir(&cloud_dir).unwrap().count();
    println!(
        "✓ A: sync_tick drained the outbox → {n_chunks} chunk(s) in the cloud, cursor {cursor_a}"
    );

    // --- Device B: second device of the same user; sync_tick pulls the tree -----
    let b = provider("B");
    let dev_b = client_a.create_device("device-B").expect("device B");
    let client_b = ServerClient::with_token(&server, dev_b.device_token);

    let cursor_b =
        SyncEngine::sync_tick(&b, &client_b, &io, b.cursor().unwrap()).expect("B sync_tick");
    b.set_cursor(cursor_b).unwrap();
    let proj = b.lookup(ROOT_INO, "Projects").expect("B pulled Projects");
    let bf = b.lookup(proj.ino, "box.lbrn2").expect("B pulled box.lbrn2");
    assert_eq!(bf.size, content.len() as u64);
    println!(
        "✓ B: sync_tick pulled the tree (cursor {cursor_b}); box.lbrn2 is {} bytes",
        bf.size
    );

    // --- Device B: hydrate on demand, then read + verify byte-for-byte ----------
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    assert!(
        !b.has_chunk(&b.node_manifest_hashes(&node_id).unwrap()[0]),
        "B's cache is empty pre-hydrate"
    );
    SyncEngine::hydrate(&b, &client_b, &io, &node_id).expect("hydrate");
    let got = b.read(bf.ino, 0, content.len() as u32).expect("read on B");
    assert_eq!(got, content, "A -> cloud -> B byte-for-byte");
    println!(
        "✓ B: hydrated + read box.lbrn2 — {} bytes match A exactly",
        got.len()
    );

    println!("\nDAEMON LOOP OK — outbox push + delta pull + on-demand hydrate, all via sync_tick.");
}
