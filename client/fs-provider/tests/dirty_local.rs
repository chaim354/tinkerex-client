//! Regression for BUG #1: a pull must NOT clobber an un-pushed local edit.
//!
//! When a node has a pending outbox `upsert` (a local edit not yet pushed) and a pull delivers
//! a remote upsert for that SAME node with a DIFFERENT manifest, `apply_remote` used to
//! overwrite the local manifest AND its base `version_id` unconditionally. That silently
//! destroyed the local edit: the later push would read the (now remote) manifest, fast-forward
//! on the remote's base, and the server would never record the conflict. The fix leaves the
//! local row untouched and returns `dirty_local`, so the queued push surfaces the real conflict.

mod common;
use common::*;

use fs_provider::{Provider, SyncNode};

#[test]
fn pull_over_a_pending_local_edit_preserves_it_and_signals_dirty_local() {
    let p = provider();

    // The target file, edited locally (manifest A) and queued in the outbox but NOT pushed.
    let f = p.create(ROOT, "cad.step").unwrap();
    p.write(f.ino, 0, b"AAAA-original-local-edit").unwrap();
    p.flush(f.ino).unwrap();
    let node_id = p.node_id_for(f.ino).unwrap().unwrap();
    let node_a = p.sync_node(&node_id).unwrap().unwrap();
    let manifest_a = node_a.manifest_id.clone();
    assert!(
        p.outbox_list()
            .unwrap()
            .iter()
            .any(|(n, op)| n == &node_id && op == "upsert"),
        "the local edit is queued (pending upsert) but un-pushed"
    );

    // Produce a real, DIFFERENT manifest B (chunks land in the same store) via a throwaway file.
    let g = p.create(ROOT, "throwaway.bin").unwrap();
    p.write(g.ino, 0, b"BBBB-concurrent-remote-version-different-bytes")
        .unwrap();
    p.flush(g.ino).unwrap();
    let node_b = p
        .sync_node(&p.node_id_for(g.ino).unwrap().unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(manifest_a, node_b.manifest_id, "A and B differ");

    // The incoming remote upsert: same node_id, but pointing at manifest B and a new version.
    let incoming = SyncNode {
        node_id: node_id.clone(),
        parent_node_id: node_a.parent_node_id.clone(),
        name: node_a.name.clone(),
        kind: Kind::File,
        manifest_id: node_b.manifest_id.clone(),
        chunks: node_b.chunks.clone(),
        version_id: Some("remote-v2".to_string()),
        mode: node_a.mode,
        mtime: node_a.mtime,
        symlink_target: None,
    };

    let outcome = p.apply_remote(&incoming).unwrap();
    assert!(
        outcome.dirty_local,
        "pending local edit + differing incoming manifest must signal dirty_local"
    );
    assert!(
        !outcome.content_changed && !outcome.inserted && !outcome.moved,
        "nothing was applied — no invalidation signals fire"
    );

    // The local edit survived: content, manifest, and base version_id are all unchanged.
    assert_eq!(
        read_all(&p, f.ino),
        b"AAAA-original-local-edit".to_vec(),
        "local content must NOT be clobbered by the pull"
    );
    let after = p.sync_node(&node_id).unwrap().unwrap();
    assert_eq!(after.manifest_id, manifest_a, "manifest A preserved");
    assert_eq!(
        after.version_id, node_a.version_id,
        "base version_id preserved (not overwritten with the remote's)"
    );
}

#[test]
fn pull_applies_normally_when_there_is_no_pending_local_edit() {
    // Control: the SAME differing-manifest pull, but with NO pending outbox upsert, must apply
    // cleanly (dirty_local false, content becomes B) — the guard must not block ordinary pulls.
    let p = provider();
    let f = p.create(ROOT, "cad.step").unwrap();
    p.write(f.ino, 0, b"AAAA-original").unwrap();
    p.flush(f.ino).unwrap();
    let node_id = p.node_id_for(f.ino).unwrap().unwrap();
    let node_a = p.sync_node(&node_id).unwrap().unwrap();

    // Drain the outbox — the local edit is now "pushed", no pending intent.
    p.outbox_clear(&node_id).unwrap();

    let g = p.create(ROOT, "throwaway.bin").unwrap();
    p.write(g.ino, 0, b"BBBB-remote-different").unwrap();
    p.flush(g.ino).unwrap();
    let node_b = p
        .sync_node(&p.node_id_for(g.ino).unwrap().unwrap())
        .unwrap()
        .unwrap();

    let incoming = SyncNode {
        node_id: node_id.clone(),
        parent_node_id: node_a.parent_node_id.clone(),
        name: node_a.name.clone(),
        kind: Kind::File,
        manifest_id: node_b.manifest_id.clone(),
        chunks: node_b.chunks.clone(),
        version_id: Some("remote-v2".to_string()),
        mode: node_a.mode,
        mtime: node_a.mtime,
        symlink_target: None,
    };
    let outcome = p.apply_remote(&incoming).unwrap();
    assert!(!outcome.dirty_local, "no pending edit → normal apply");
    assert!(outcome.content_changed, "the manifest was repointed to B");
    assert_eq!(
        read_all(&p, f.ino),
        b"BBBB-remote-different".to_vec(),
        "with no local edit, the pull applies the remote content"
    );
}
