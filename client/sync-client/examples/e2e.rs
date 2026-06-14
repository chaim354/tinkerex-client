//! End-to-end control-plane test: Rust client -> live Rails sync server.
//!
//! Exercises register -> mkdir -> commit(begin/finalize) -> changes -> tree ->
//! restore, asserting the client and server speak the same protocol. Chunk *bytes*
//! aren't uploaded here (the server's memory store returns non-HTTP URLs and finalize
//! doesn't verify) — this proves the control plane.
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example e2e

use std::time::{SystemTime, UNIX_EPOCH};

use sync_client::{ChunkRef, CommitBegin, ServerClient};

fn uniq() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{n:x}")
}

fn main() {
    let base = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let mut c = ServerClient::new(&base);

    let email = format!("e2e-{}@example.com", uniq());
    let reg = c.register(&email, "rust-client").expect("register");
    assert_eq!(reg.head_seq, 0, "fresh user starts at seq 0");
    println!("✓ registered {} (device {})", reg.user_id, reg.device_id);

    // mkdir Projects
    let dir = format!("dir-{}", uniq());
    let r = c.mkdir(&dir, None, "Projects", 0o755, 0).expect("mkdir");
    assert_eq!(r.seq, 1);
    println!("✓ mkdir Projects (seq {})", r.seq);

    // commit a file under Projects
    let node = format!("node-{}", uniq());
    let hash = "a".repeat(64);
    let manifest = vec![ChunkRef { hash: hash.clone(), offset: 0, length: 12 }];
    let commit_id = format!("commit-{}", uniq());
    let begin = c
        .commit_begin(&CommitBegin {
            commit_id: commit_id.clone(),
            node_id: node.clone(),
            parent_node_id: Some(dir.clone()),
            name: "box.lbrn2".into(),
            base_version_id: None,
            size: 12,
            manifest: manifest.clone(), mode: 0o644, mtime: 0,
        })
        .expect("commit begin");
    assert_eq!(begin.missing, vec![hash.clone()], "the one chunk is missing");
    assert!(begin.put_urls.contains_key(&hash), "got a presigned PUT url");
    println!("✓ commit begin: {} missing, presigned PUTs returned", begin.missing.len());

    // (a real client would PUT the chunk to begin.put_urls[hash] here)

    let fin1 = c.commit_finalize(&commit_id).expect("finalize");
    assert!(!fin1.version_id.is_empty());
    assert_eq!(fin1.seq, 2);
    println!("✓ finalized v1 (seq {})", fin1.seq);

    // second version of the same file
    let commit_id2 = format!("commit-{}", uniq());
    let manifest2 = vec![ChunkRef { hash: "b".repeat(64), offset: 0, length: 20 }];
    c.commit_begin(&CommitBegin {
        commit_id: commit_id2.clone(),
        node_id: node.clone(),
        parent_node_id: Some(dir.clone()),
        name: "box.lbrn2".into(),
        base_version_id: Some(fin1.version_id.clone()),
        size: 20,
        manifest: manifest2, mode: 0o644, mtime: 0,
    })
    .expect("begin2");
    let fin2 = c.commit_finalize(&commit_id2).expect("finalize2");
    println!("✓ finalized v2 (seq {})", fin2.seq);

    // pull changes from 0
    let changes = c.changes(0).expect("changes");
    assert_eq!(changes.head_seq, 3);
    let put = changes.changes.iter().find(|ch| ch.node_id == node && ch.op == "put").expect("file change present");
    assert!(put.manifest.is_some(), "put change carries a manifest");
    println!("✓ pulled {} changes (head_seq {})", changes.changes.len(), changes.head_seq);

    // full tree
    let tree = c.tree(0).expect("tree");
    assert!(tree.nodes.iter().any(|n| n.name == "Projects" && n.kind == "dir"));
    let file = tree.nodes.iter().find(|n| n.node_id == node).expect("file in tree");
    assert_eq!(file.size, Some(20), "tree shows the latest version size");
    println!("✓ tree snapshot has Projects/ and box.lbrn2 (size {:?})", file.size);

    // restore v1
    let restored = c.restore(&node, &fin1.version_id).expect("restore");
    println!("✓ restored v1 -> new version {} (seq {})", &restored.version_id[..8], restored.seq);

    println!("\nE2E OK — Rust client and Rails server speak the same protocol.");
}
