//! Multi-device convergence: two devices of one user sync through the server's
//! changelog. Device A builds a tree; device B initial-syncs via the tree snapshot,
//! then converges on A's later change via a delta pull. The headline feature — n
//! devices staying consistent — demonstrated end-to-end.
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example multi_device

use std::time::{SystemTime, UNIX_EPOCH};

use sync_client::{ChunkRef, CommitBegin, LocalTree, ServerClient};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn commit_file(c: &ServerClient, node_id: &str, parent: Option<&str>, name: &str, hash: &str, size: u64) {
    let commit_id = format!("c-{}", uniq());
    c.commit_begin(&CommitBegin {
        commit_id: commit_id.clone(),
        node_id: node_id.into(),
        parent_node_id: parent.map(String::from),
        name: name.into(),
        base_version_id: None,
        size,
        manifest: vec![ChunkRef { hash: hash.into(), offset: 0, length: size }],
    })
    .expect("begin");
    c.commit_finalize(&commit_id).expect("finalize");
}

fn main() {
    let base = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());

    // --- Device A: build a tree -------------------------------------------------
    let mut a = ServerClient::new(&base);
    a.register(&format!("multi-{}@example.com", uniq()), "device-A").expect("register A");

    let dir = format!("d-{}", uniq());
    a.mkdir(&dir, None, "Projects").expect("mkdir");
    let f1 = format!("f-{}", uniq());
    commit_file(&a, &f1, Some(&dir), "box.lbrn2", &"a".repeat(64), 12);
    let f2 = format!("f-{}", uniq());
    commit_file(&a, &f2, None, "README.txt", &"b".repeat(64), 5);
    println!("✓ device A built a tree (Projects/box.lbrn2 + README.txt)");

    // --- Device B: a second device of the SAME user -----------------------------
    let dev_b = a.create_device("device-B").expect("create device B");
    let b = ServerClient::with_token(&base, dev_b.device_token);

    // initial sync via the snapshot
    let snap = b.tree(0).expect("tree");
    let mut tree = LocalTree::from_snapshot(&snap.nodes, snap.head_seq);
    assert!(tree.live().any(|n| n.name == "Projects" && n.kind == "dir"));
    assert_eq!(tree.path_of(&f1).as_deref(), Some("/Projects/box.lbrn2"));
    assert!(tree.get(&f2).expect("readme present").manifest.is_some());
    println!("✓ device B initial-synced {} nodes (cursor {})", tree.nodes.len(), tree.cursor);

    // --- Device A changes; Device B converges via a delta pull ------------------
    a.delete_node(&f2).expect("delete on A");
    let delta = b.changes(tree.cursor).expect("changes");
    tree.apply_all(&delta.changes);
    assert!(tree.get(&f2).is_none(), "B converged: README.txt is gone");
    assert_eq!(tree.cursor, delta.head_seq);
    assert_eq!(tree.path_of(&f1).as_deref(), Some("/Projects/box.lbrn2"), "the rest of the tree is intact");
    println!("✓ device B converged on A's delete (cursor {} == head_seq {})", tree.cursor, delta.head_seq);

    println!("\nMULTI-DEVICE OK — two devices converge through the server changelog.");
}
