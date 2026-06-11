//! Like `daemon_loop`, but chunk bytes move through **real presigned PUT/GET** to live
//! S3-compatible storage (SeaweedFS/R2) via `PresignedIO` — never through Rails. This
//! is the production data plane; run it with the server booted against `STORAGE_BACKEND=s3`.
//!
//!   # terminal 1 (server pointed at SeaweedFS):
//!   STORAGE_BACKEND=s3 CHUNK_ENDPOINT=http://192.168.1.7:8333 CHUNK_BUCKET=my-bucket \
//!     CHUNK_REGION=us-east-1 CHUNK_ACCESS_KEY_ID=admin CHUNK_SECRET_ACCESS_KEY=secret \
//!     bin/rails server -p 3001
//!   # terminal 2:
//!   SERVER_URL=http://localhost:3001 cargo run --example presigned_roundtrip

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn provider(tag: &str) -> SqliteProvider {
    let base = std::env::temp_dir().join(format!("maker-presigned-{tag}-{}", uniq()));
    std::fs::create_dir_all(&base).unwrap();
    let db = base.join("meta.db");
    let cache = base.join("cache");
    SqliteProvider::open(&db, Box::new(LocalChunkStore::new(cache).unwrap())).unwrap()
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());

    // --- Device A: build a local tree, push through PresignedIO -----------------
    let a = provider("A");
    let dir = a.mkdir(ROOT_INO, "Projects").unwrap();
    let f = a.create(dir.ino, "box.lbrn2").unwrap();
    let content: Vec<u8> =
        b"LightBurn over real S3 \x00\x01\x02\x03 ".iter().cycle().take(40_000).copied().collect();
    a.write(f.ino, 0, &content).unwrap();
    a.flush(f.ino).unwrap();

    let mut client_a = ServerClient::new(&server);
    client_a.register(&format!("presigned-{}@example.com", uniq()), "device-A").expect("register A");
    let io_a = PresignedIO::new(&client_a);
    let cursor_a = SyncEngine::sync_tick(&a, &client_a, &io_a, a.cursor().unwrap()).expect("A sync_tick");
    a.set_cursor(cursor_a).unwrap();
    println!("✓ A: pushed via presigned PUT to live storage (cursor {cursor_a})");

    // --- Device B: pull + hydrate via presigned GET, verify bytes ---------------
    let b = provider("B");
    let dev_b = client_a.create_device("device-B").expect("device B");
    let client_b = ServerClient::with_token(&server, dev_b.device_token);
    let io_b = PresignedIO::new(&client_b);

    let cursor_b = SyncEngine::sync_tick(&b, &client_b, &io_b, b.cursor().unwrap()).expect("B sync_tick");
    b.set_cursor(cursor_b).unwrap();
    let proj = b.lookup(ROOT_INO, "Projects").expect("B pulled Projects");
    let bf = b.lookup(proj.ino, "box.lbrn2").expect("B pulled box.lbrn2");
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

    SyncEngine::hydrate(&b, &client_b, &io_b, &node_id).expect("hydrate via presigned GET");
    let got = b.read(bf.ino, 0, content.len() as u32).expect("read on B");
    assert_eq!(got, content, "A -> live S3 -> B byte-for-byte");
    println!("✓ B: hydrated via presigned GET — {} bytes match A exactly", got.len());

    println!("\nPRESIGNED ROUNDTRIP OK — real bytes through live S3-compatible storage.");
}
