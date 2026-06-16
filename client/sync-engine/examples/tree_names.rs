//! Dump the server-side tree as one library-relative path per line — the assertion oracle for
//! the Windows CfAPI smoke (`client/test-conformance/cfapi-smoke.ps1`). Runs ANYWHERE (control
//! plane only, no CfAPI), so the smoke can do its local file ops on Windows and then assert what
//! actually reached the server, byte-independent.
//!
//!   tree-names <server_url> <device_token>
//!   # or: SERVER_URL=… TOKEN=… cargo run --example tree_names
//!
//! Output: one path per line, files and dirs, e.g.
//!   dir   notes
//!   file  notes/todo.txt
//!   file  hello.txt
//! Exit 0 on a successful pull (even if empty); exit 1 if the control-plane pull fails.

use fs_provider::{Kind, LocalChunkStore, SqliteProvider};
use std::collections::HashMap;
use sync_client::ServerClient;
use sync_engine::SyncEngine;

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .or_else(|| std::env::var("SERVER_URL").ok())
        .expect("usage: tree-names <server_url> <device_token>  (or SERVER_URL/TOKEN env)");
    let token = args
        .next()
        .or_else(|| std::env::var("TOKEN").ok())
        .expect("missing device token (arg 2 or TOKEN env)");

    let client = ServerClient::with_token(&server, &token);
    let tag = std::process::id();
    let db = std::env::temp_dir().join(format!("maker-treenames-{tag}.db"));
    let chunks = std::env::temp_dir().join(format!("maker-treenames-{tag}-chunks"));
    let provider = SqliteProvider::open(
        &db,
        Box::new(LocalChunkStore::new(&chunks).expect("chunk dir")),
    )
    .expect("open local db");

    if let Err(e) = SyncEngine::pull_snapshot(&provider, &client) {
        eprintln!("tree-names: control-plane pull FAILED: {e}");
        std::process::exit(1);
    }

    let nodes = provider.export_tree().expect("export_tree");
    // node_id -> (name, parent_node_id) so we can reconstruct full paths.
    let by_id: HashMap<String, (String, Option<String>)> = nodes
        .iter()
        .map(|n| {
            (
                n.node_id.clone(),
                (n.name.clone(), n.parent_node_id.clone()),
            )
        })
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

    let mut lines: Vec<String> = nodes
        .iter()
        .map(|n| {
            let kind = if n.kind == Kind::Dir { "dir " } else { "file" };
            format!("{kind}  {}", path_of(&n.node_id))
        })
        .collect();
    lines.sort();
    for l in lines {
        println!("{l}");
    }
}
