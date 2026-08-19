//! Like `daemon_loop`, but chunk bytes move through **real presigned PUT/GET** to live
//! S3-compatible storage (SeaweedFS/R2) via `PresignedIO` — never through Rails. This
//! is the production data plane; run it with the server booted against `STORAGE_BACKEND=s3`.
//!
//!   # terminal 1 (server pointed at SeaweedFS):
//!   STORAGE_BACKEND=s3 CHUNK_ENDPOINT=http://192.168.1.8:8333 CHUNK_BUCKET=my-bucket \
//!     CHUNK_REGION=us-east-1 CHUNK_ACCESS_KEY_ID=admin CHUNK_SECRET_ACCESS_KEY=secret \
//!     bin/rails server -p 3001
//!   # terminal 2:
//!   SERVER_URL=http://localhost:3001 cargo run --example presigned_roundtrip

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

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
    let content: Vec<u8> = b"LightBurn over real S3 \x00\x01\x02\x03 "
        .iter()
        .cycle()
        .take(40_000)
        .copied()
        .collect();
    a.write(f.ino, 0, &content).unwrap();
    a.flush(f.ino).unwrap();

    let mut client_a = ServerClient::new(&server);
    client_a
        .enroll(
            &format!("presigned-{}@example.com", uniq()),
            "correct horse battery staple",
            "device-A",
        )
        .expect("register A");
    let io_a = PresignedIO::new(&client_a);
    let cursor_a =
        SyncEngine::sync_tick(&a, &client_a, &io_a, a.cursor().unwrap()).expect("A sync_tick");
    a.set_cursor(cursor_a).unwrap();
    println!("✓ A: pushed via presigned PUT to live storage (cursor {cursor_a})");

    // --- Device B: pull + hydrate via presigned GET, verify bytes ---------------
    let b = provider("B");
    let dev_b = client_a.create_device("device-B").expect("device B");
    let client_b = ServerClient::with_token(&server, dev_b.device_token);
    let io_b = PresignedIO::new(&client_b);

    let cursor_b =
        SyncEngine::sync_tick(&b, &client_b, &io_b, b.cursor().unwrap()).expect("B sync_tick");
    b.set_cursor(cursor_b).unwrap();
    let proj = b.lookup(ROOT_INO, "Projects").expect("B pulled Projects");
    let bf = b.lookup(proj.ino, "box.lbrn2").expect("B pulled box.lbrn2");
    let node_id = b.node_id_for(bf.ino).unwrap().unwrap();

    SyncEngine::hydrate(&b, &client_b, &io_b, &node_id).expect("hydrate via presigned GET");
    let got = b.read(bf.ino, 0, content.len() as u32).expect("read on B");
    assert_eq!(got, content, "A -> live S3 -> B byte-for-byte");
    println!(
        "✓ B: hydrated via presigned GET — {} bytes match A exactly",
        got.len()
    );

    // --- Which store did those bytes actually go to? ----------------------------------
    // The round trip above already proves the bytes LANDED: the example enrolls a fresh
    // user every run and dedup is per-user, so every chunk is first-seen, and the server's
    // finalize verification (SYNC_VERIFY_FINALIZE, ON by default) HEADs its own store before
    // accepting the commit. What it does NOT show is WHICH store that is — a green run says
    // "the store the server is configured with", which is exactly the question a cutover asks.
    //
    // S3ChunkStore sets force_path_style, so a presigned URL is
    // https://<host>/<bucket>/chunks/<user>/<hash>?X-Amz-... — origin and first path segment
    // name the endpoint AND the bucket. Print ONLY those two. The query string carries the
    // SigV4 signature and must never reach a log or a transcript (CLAUDE.md §14g: a redirected
    // `op read` printed a live PAT once and forced a rotation).
    let hashes = b.node_manifest_hashes(&node_id).expect("B manifest hashes");
    let first = hashes.first().expect("the file has at least one chunk");
    let urls = client_b
        .download_urls(std::slice::from_ref(first))
        .expect("presign a download URL");
    let url = urls
        .urls
        .values()
        .next()
        .expect("a presigned GET url")
        .clone();
    let mut seg = url.split('?').next().unwrap_or_default().splitn(5, '/');
    let scheme = seg.next().unwrap_or_default();
    let _empty = seg.next();
    let host = seg.next().unwrap_or_default();
    let bucket = seg.next().unwrap_or_default();
    println!("✓ store identity (from the presigned GET; query string withheld): {scheme}//{host} bucket={bucket}");

    println!("\nPRESIGNED ROUNDTRIP OK — real bytes through live S3-compatible storage.");
}
