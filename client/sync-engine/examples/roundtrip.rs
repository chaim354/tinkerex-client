//! Full end-to-end: device A -> live Rails server -> device B, with **real chunk
//! bytes** moving through a shared "cloud" store (stands in for SeaweedFS/R2 — set
//! CHUNK_DIR, or a temp dir is used). Proves both planes: metadata via the server's
//! changelog, bytes via the chunk store, content verified on B.
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example roundtrip

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn provider(tag: &str) -> SqliteProvider {
    let base = std::env::temp_dir().join(format!("maker-rt-{tag}-{}", uniq()));
    let db = base.join("meta.db");
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    SqliteProvider::open(&db, Box::new(LocalChunkStore::new(cache).unwrap())).unwrap()
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let cloud_dir = std::env::var("CHUNK_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("maker-cloud-{}", uniq())).to_string_lossy().into());
    let io = SharedDirIO::new(&cloud_dir).unwrap();
    println!("cloud (shared chunk store) = {cloud_dir}");

    // --- Device A: build a real local tree with content ------------------------
    let a = provider("A");
    let dir = a.mkdir(ROOT_INO, "Projects").unwrap();
    let f = a.create(dir.ino, "box.lbrn2").unwrap();
    let content: Vec<u8> = b"LightBurn project payload \x00\x01\x02 ".iter().cycle().take(40_000).copied().collect();
    a.write(f.ino, 0, &content).unwrap();
    a.flush(f.ino).unwrap();
    println!("✓ device A created Projects/box.lbrn2 ({} bytes, chunked locally)", content.len());

    // --- Device A: register + push (uploads chunks to the cloud) ---------------
    let mut client_a = ServerClient::new(&server);
    client_a.enroll(&format!("rt-{}@example.com", uniq()), "correct horse battery staple", "device-A").expect("register A");
    SyncEngine::push(&a, &client_a, &io).expect("push");
    let n_chunks = std::fs::read_dir(&cloud_dir).unwrap().count();
    println!("✓ device A pushed tree + uploaded {n_chunks} chunk(s) to the cloud");

    // --- Device B: second device, separate local store -------------------------
    let b = provider("B");
    let dev_b = client_a.create_device("device-B").expect("device B");
    let client_b = ServerClient::with_token(&server, dev_b.device_token);

    let head = SyncEngine::pull_snapshot(&b, &client_b).expect("pull");
    println!("✓ device B pulled snapshot (head_seq {head})");

    // B sees the tree
    let proj = b.lookup(ROOT_INO, "Projects").expect("B has Projects");
    let bf = b.lookup(proj.ino, "box.lbrn2").expect("B has box.lbrn2");
    assert_eq!(bf.size, content.len() as u64);

    // B's cache is empty until hydration
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();
    assert!(!b.node_manifest_hashes(&node_id).unwrap().is_empty(), "B knows the manifest");

    // --- Device B: hydrate from the cloud, then read + verify ------------------
    SyncEngine::hydrate(&b, &client_b, &io, &node_id).expect("hydrate");
    let got = b.read(bf.ino, 0, content.len() as u32).expect("read on B");
    assert_eq!(got, content, "content round-trips A -> cloud -> B byte-for-byte");
    println!("✓ device B hydrated + read box.lbrn2 — {} bytes match A exactly", got.len());

    println!("\nROUNDTRIP OK — real bytes A -> cloud -> B; metadata via the live server.");
}
