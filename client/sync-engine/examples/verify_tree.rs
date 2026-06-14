//! Byte-for-byte server-tree oracle for the CfAPI conformance harness
//! (`client/test-conformance/cfapi-conformance.ps1`). Like `tree_names`, but it also **hydrates
//! every file from the chunk store on a FRESH device** and prints its content hash — so the
//! harness can assert not just that the right paths reached the server, but that their bytes
//! round-tripped intact through chunking + the content store, independent of the box that wrote
//! them. Runs anywhere (control plane + presigned GET, no CfAPI).
//!
//!   verify-tree <server_url> <device_token>
//!   # or: SERVER_URL=… TOKEN=… cargo run --example verify_tree
//!
//! Output: one line per node, sorted, files carrying a sha256 of their hydrated bytes:
//!   dir   proj
//!   file  proj/part.bin  <sha256-hex>
//!   file  hello.txt      <sha256-hex>
//! Exit 0 on success; exit 1 if the control-plane pull fails (a per-file hydrate failure is
//! reported inline as `<HYDRATE-FAILED>` but does not abort — the harness flags the mismatch).

use fs_provider::{Kind, LocalChunkStore, Provider, SqliteProvider};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .or_else(|| std::env::var("SERVER_URL").ok())
        .expect("usage: verify-tree <server_url> <device_token>  (or SERVER_URL/TOKEN env)");
    let token = args
        .next()
        .or_else(|| std::env::var("TOKEN").ok())
        .expect("missing device token (arg 2 or TOKEN env)");

    let client = ServerClient::with_token(&server, &token);
    let tag = std::process::id();
    let db = std::env::temp_dir().join(format!("maker-verify-{tag}.db"));
    let chunks = std::env::temp_dir().join(format!("maker-verify-{tag}-chunks"));
    let provider = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).expect("chunk dir")))
        .expect("open local db");
    let io = PresignedIO::new(&client);

    if let Err(e) = SyncEngine::pull_snapshot(&provider, &client) {
        eprintln!("verify-tree: control-plane pull FAILED: {e}");
        std::process::exit(1);
    }

    let nodes = provider.export_tree().expect("export_tree");
    // node_id -> (name, parent_node_id) so we can reconstruct full paths.
    let by_id: HashMap<String, (String, Option<String>)> = nodes
        .iter()
        .map(|n| (n.node_id.clone(), (n.name.clone(), n.parent_node_id.clone())))
        .collect();
    let path_of = |start: &str| -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut id = start.to_string();
        while let Some((name, parent)) = by_id.get(&id) {
            parts.push(name.clone());
            match parent {
                Some(p) => id = p.clone(),
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    };

    let mut lines: Vec<String> = Vec::new();
    for n in &nodes {
        let path = path_of(&n.node_id);
        if n.kind == Kind::Dir {
            lines.push(format!("dir   {path}"));
            continue;
        }
        // Hydrate this file's chunks on this fresh device, then read + hash the bytes.
        let hash = match SyncEngine::hydrate(&provider, &client, &io, &n.node_id) {
            Ok(()) => match provider.ino_for_node_id(&n.node_id) {
                Ok(Some(ino)) => {
                    let size = provider.getattr(ino).map(|a| a.size).unwrap_or(0);
                    match provider.read(ino, 0, size as u32) {
                        Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
                        Err(e) => format!("<READ-FAILED: {e}>"),
                    }
                }
                _ => "<NODE-MISSING>".to_string(),
            },
            Err(e) => format!("<HYDRATE-FAILED: {e}>"),
        };
        lines.push(format!("file  {path}\t{hash}"));
    }
    lines.sort();
    for l in lines {
        println!("{l}");
    }
}
