//! The outbox topological drain must NEVER emit a node before the parent it references.
//!
//! This is the changelog-ordering invariant whose violation caused a real sync-stall bug (a
//! child committed before its parent → a pulling device's apply_remote hits NotFound and
//! aborts the pull). `outbox_list_topo` orders parents-first even when a move re-enqueues a
//! child whose new parent was created later (so insertion order has the child ahead of its
//! parent). These tests build adversarial orderings and assert the invariant holds, then
//! prove convergence when a replica applies the drain in exactly that order.

mod common;
use common::*;

use std::collections::HashMap;

use fs_provider::{Provider, SqliteProvider};

/// Assert that in `order`, every node appears AFTER its parent (when the parent is also in
/// the list). Uses the provider's current tree to resolve each node's parent_node_id.
fn assert_parents_first(p: &SqliteProvider, order: &[(String, String)]) {
    // node_id -> parent_node_id (from a fresh export).
    let parent_of: HashMap<String, Option<String>> = p
        .export_tree()
        .unwrap()
        .into_iter()
        .map(|n| (n.node_id, n.parent_node_id))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for (nid, op) in order {
        if op != "delete" {
            if let Some(Some(pid)) = parent_of.get(nid) {
                // If the parent is anywhere in this drain, it must already be seen.
                let parent_in_drain = order.iter().any(|(n, o)| n == pid && o != "delete");
                if parent_in_drain {
                    assert!(
                        seen.contains(pid),
                        "child {nid} drained before its parent {pid}"
                    );
                }
            }
        }
        seen.insert(nid.clone());
    }
}

#[test]
fn move_into_a_later_created_dir_keeps_parents_first() {
    let p = provider();
    // Create a file FIRST (low insertion id), then a dir (higher id), then move the file
    // into the dir. The move re-enqueues the file but keeps its original (earlier) outbox
    // id — so insertion order puts the child ahead of its parent.
    let f = p.create(ROOT, "early.bin").unwrap();
    p.write(f.ino, 0, b"data").unwrap();
    p.flush(f.ino).unwrap();
    let d = p.mkdir(ROOT, "laterdir").unwrap();
    p.rename(ROOT, "early.bin", d.ino, "early.bin").unwrap();

    let topo = p.outbox_list_topo().unwrap();
    assert_parents_first(&p, &topo);

    // And a replica draining in topo order converges (no NotFound abort). Re-do the same on
    // a shared-store pair so bytes hydrate.
    let (src, repl, _d2) = shared_store_pair();
    let f2 = src.create(ROOT, "early.bin").unwrap();
    src.write(f2.ino, 0, b"data").unwrap();
    src.flush(f2.ino).unwrap();
    let d2 = src.mkdir(ROOT, "laterdir").unwrap();
    src.rename(ROOT, "early.bin", d2.ino, "early.bin").unwrap();
    for (nid, op) in src.outbox_list_topo().unwrap() {
        if op == "delete" {
            repl.apply_remote_delete(&nid).unwrap();
        } else if let Some(n) = src.sync_node(&nid).unwrap() {
            repl.apply_remote(&n)
                .expect("topo-ordered apply must not hit a missing parent");
        }
    }
    assert_eq!(tree_snapshot(&src), tree_snapshot(&repl));
}

#[test]
fn deep_chain_built_bottom_up_drains_parents_first() {
    let p = provider();
    // Build a chain of dirs, but create the DEEPEST content first by repeatedly creating a
    // dir then moving the prior subtree under it — maximizing out-of-order insertion ids.
    // Simpler robust variant: create N dirs at root, then chain them by moving each under
    // the previous (each move re-enqueues an early node under a late parent).
    let mut inos = vec![ROOT];
    for i in 0..12 {
        let d = p.mkdir(ROOT, &format!("d{i}")).unwrap();
        inos.push(d.ino);
    }
    // Chain: move d{i} under d{i-1} (from the deepest up, so parents are created-later refs).
    for i in (1..12).rev() {
        p.rename(ROOT, &format!("d{i}"), inos[i], &format!("d{i}"))
            .unwrap();
    }
    let topo = p.outbox_list_topo().unwrap();
    assert_parents_first(&p, &topo);

    // Convergence: a fresh replica applying in topo order reproduces the chain.
    let repl = provider();
    for (nid, op) in &topo {
        if op == "delete" {
            repl.apply_remote_delete(nid).unwrap();
        } else if let Some(n) = p.sync_node(nid).unwrap() {
            repl.apply_remote(&n)
                .expect("deep chain: topo apply must not miss a parent");
        }
    }
    assert_eq!(tree_snapshot(&p), tree_snapshot(&repl));
}
