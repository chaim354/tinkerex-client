//! Layered self-test for the Windows CfAPI client — isolates *which* layer fails when the
//! mount misbehaves, using the real code paths (no Cloud Filter API, so it runs anywhere):
//!
//!   1. **Control plane** — pull the tree from the server (proves the URL + token + HTTP API).
//!   2. **Storage** — hydrate the first file's chunks via presigned GET (proves the Windows
//!      box can reach the object store at CHUNK_ENDPOINT and download real bytes).
//!
//! If (1) fails → wrong URL / bad token / server down. If (2) fails → the chunk store
//! (SeaweedFS `192.168.1.7:8333`) isn't reachable from this machine. If both pass but
//! Explorer still hangs → it's purely a CfAPI integration issue, not network/storage.
//!
//!   maker-doctor <server_url> <device_token>
//!   # or: SERVER_URL=… TOKEN=… cargo run --example doctor

use fs_provider::{LocalChunkStore, Provider, SqliteProvider};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args.next().or_else(|| std::env::var("SERVER_URL").ok())
        .expect("usage: maker-doctor <server_url> <device_token>  (or SERVER_URL/TOKEN env)");
    let token = args.next().or_else(|| std::env::var("TOKEN").ok())
        .expect("missing device token (arg 2 or TOKEN env)");

    let client = ServerClient::with_token(&server, &token);
    let tag = std::process::id();
    let db = std::env::temp_dir().join(format!("maker-doctor-{tag}.db"));
    let chunks = std::env::temp_dir().join(format!("maker-doctor-{tag}-chunks"));
    let provider = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).expect("chunk dir")))
        .expect("open local db");

    println!("== Maker CfAPI doctor ==");
    println!("server: {server}");

    // 1. Control plane: pull the tree (HTTP API + token).
    println!("\n[1/2] control plane — pulling the tree…");
    let head = match SyncEngine::pull_snapshot(&provider, &client) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  ✗ FAILED: {e}");
            eprintln!("  → check the SERVER_URL (no :3001 for the deployed server — it's port 80),");
            eprintln!("    that the box can reach it, and that the device token is valid.");
            std::process::exit(1);
        }
    };
    let nodes = provider.export_tree().expect("export");
    let files: Vec<_> = nodes.iter().filter(|n| n.kind == fs_provider::Kind::File && n.manifest_id.is_some()).collect();
    println!("  ✓ OK — head_seq={head}, {} node(s):", nodes.len());
    for n in &nodes {
        let kind = if n.kind == fs_provider::Kind::Dir { "dir " } else { "file" };
        let size: u64 = n.chunks.iter().map(|c| c.length).sum();
        println!("      {kind} {:30} {size} bytes", n.name);
    }
    if nodes.is_empty() {
        println!("  ⚠ the library is EMPTY — push files via the Mac app/mac_host first,");
        println!("    otherwise Explorer will just show an empty folder.");
        return;
    }

    // 2. Storage: hydrate the first file's bytes (presigned GET → object store).
    println!("\n[2/2] storage — hydrating the first file's bytes…");
    let Some(file) = files.first() else {
        println!("  ⚠ no files (only dirs) — nothing to hydrate. Control plane is healthy.");
        return;
    };
    let io = PresignedIO::new(&client);
    if let Err(e) = SyncEngine::hydrate(&provider, &client, &io, &file.node_id) {
        eprintln!("  ✗ FAILED hydrating {}: {e}", file.name);
        eprintln!("  → the chunk store (CHUNK_ENDPOINT, e.g. SeaweedFS 192.168.1.7:8333) is likely");
        eprintln!("    not reachable from this machine. Try: curl http://192.168.1.7:8333  from here.");
        std::process::exit(2);
    }
    let ino = provider.ino_for_node_id(&file.node_id).unwrap().unwrap();
    let size: u64 = file.chunks.iter().map(|c| c.length).sum();
    let bytes = provider.read(ino, 0, size as u32).expect("read after hydrate");
    let head16: Vec<u8> = bytes.iter().take(16).copied().collect();
    println!("  ✓ OK — hydrated {} → {} bytes (first {:?})", file.name, bytes.len(), head16);

    println!("\n✓ ALL CHECKS PASSED — control plane + storage reachable from this machine.");
    println!("  If Explorer still hangs on the CfAPI mount, the problem is in the CfAPI layer");
    println!("  (the maker-cfsyncd console diagnostics will show which callback fires).");
}
