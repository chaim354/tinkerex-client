//! Push a NEW VERSION of an existing file in a library — for testing remote-change
//! invalidation on the Windows CfAPI client. Pulls the tree (to find the file's stable
//! node_id + current version), replaces its content, and commits a new version against that
//! node, so other devices see a *content change* on the same file (not a new file).
//!
//!   maker-push-update <server_url> <device_token> <filename> <new text content>
//!   # e.g.
//!   cargo run --example push_update -- http://192.168.1.95 <TOKEN> hello.txt "VERSION TWO"

use fs_provider::{LocalChunkStore, Provider, SqliteProvider};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args.next().expect("usage: push_update <server> <token> <filename> <content…>");
    let token = args.next().expect("missing device token");
    let name = args.next().expect("missing filename");
    let content: String = args.collect::<Vec<_>>().join(" ");
    if content.is_empty() {
        panic!("missing new content (everything after the filename is the new file body)");
    }

    let client = ServerClient::with_token(&server, &token);
    let tag = std::process::id();
    let db = std::env::temp_dir().join(format!("maker-pushupd-{tag}.db"));
    let chunks = std::env::temp_dir().join(format!("maker-pushupd-{tag}-chunks"));
    let provider = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).expect("chunk dir")))
        .expect("open db");

    // Pull the tree so the file's stable node_id + current version are known locally.
    SyncEngine::pull_snapshot(&provider, &client).expect("pull tree (server reachable? token valid?)");
    let node = provider
        .export_tree()
        .expect("export")
        .into_iter()
        .find(|n| n.name == name && n.kind == fs_provider::Kind::File)
        .unwrap_or_else(|| panic!("no file named {name:?} in the library"));
    let ino = provider.ino_for_node_id(&node.node_id).unwrap().unwrap();

    // Replace its content and push a new version (base = the version we just pulled → a clean
    // fast-forward, recorded as a content change on the same node_id).
    provider.ingest_bytes(ino, content.as_bytes()).expect("ingest");
    SyncEngine::push_outbox(&provider, &client, &PresignedIO::new(&client)).expect("push (S3 backend reachable?)");

    println!("✓ updated {name} to a new version ({} bytes): {content:?}", content.len());
    println!("  node_id={}", node.node_id);
    println!("  → on Windows, re-open {name} in MakerCF; it should show the new content.");
}
