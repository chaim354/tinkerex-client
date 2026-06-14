//! Tests for the three `SqliteProvider` helpers that CfAPI callbacks (Windows-only) will
//! call: `delete_by_node_id`, `resolve_path`, and `rename_by_node_id`. These are pure
//! cross-platform Rust, fully testable on macOS/Linux.

mod common;
use common::*;

use fs_provider::{FsError, Provider};

// ---- delete_by_node_id -----------------------------------------------------------

/// Deleting a single file removes it from the tree and enqueues a "delete" in the outbox.
#[test]
fn delete_by_node_id_removes_file_and_enqueues_delete() {
    let p = provider();
    let ino = write_file(&p, ROOT, "foo.txt", b"hello");
    let node_id = p.node_id_for(ino).unwrap().unwrap();

    p.delete_by_node_id(&node_id).unwrap();

    // Tree: gone.
    assert!(p.ino_for_node_id(&node_id).unwrap().is_none());
    assert!(matches!(p.lookup(ROOT, "foo.txt"), Err(FsError::NotFound)));

    // Outbox: a "delete" entry for the node_id (there may also be the prior "upsert"
    // from write_file's flush, coalesced to "delete" on the same node_id).
    let outbox = p.outbox_list().unwrap();
    assert!(
        outbox.iter().any(|(id, op)| id == &node_id && op == "delete"),
        "expected a delete entry for {node_id} in outbox, got {outbox:?}"
    );
}

/// Deleting a directory removes it AND its entire child subtree, enqueuing "delete" for
/// the root of the deleted subtree (the top-level dir's node_id).
#[test]
fn delete_by_node_id_removes_dir_and_subtree() {
    let p = provider();
    let d = p.mkdir(ROOT, "proj").unwrap();
    let child_ino = write_file(&p, d.ino, "child.txt", b"data");
    let sub = p.mkdir(d.ino, "sub").unwrap();
    let deep_ino = write_file(&p, sub.ino, "deep.txt", b"deep");

    let dir_id = p.node_id_for(d.ino).unwrap().unwrap();
    let child_id = p.node_id_for(child_ino).unwrap().unwrap();
    let sub_id = p.node_id_for(sub.ino).unwrap().unwrap();
    let deep_id = p.node_id_for(deep_ino).unwrap().unwrap();

    p.delete_by_node_id(&dir_id).unwrap();

    // Everything in the subtree is gone.
    assert!(p.ino_for_node_id(&dir_id).unwrap().is_none(), "dir itself gone");
    assert!(p.ino_for_node_id(&child_id).unwrap().is_none(), "direct child gone");
    assert!(p.ino_for_node_id(&sub_id).unwrap().is_none(), "nested dir gone");
    assert!(p.ino_for_node_id(&deep_id).unwrap().is_none(), "deep file gone");

    // At minimum the top-level dir_id has a "delete" outbox entry.
    let outbox = p.outbox_list().unwrap();
    assert!(
        outbox.iter().any(|(id, op)| id == &dir_id && op == "delete"),
        "expected delete for dir_id in outbox, got {outbox:?}"
    );
}

/// Calling delete_by_node_id on a nonexistent node_id is a no-op (idempotent): no
/// error, and nothing is written to the outbox.
#[test]
fn delete_by_node_id_nonexistent_is_noop() {
    let p = provider();
    // No nodes created — outbox is empty.
    p.delete_by_node_id("does-not-exist").unwrap();
    assert!(p.outbox_list().unwrap().is_empty(), "no outbox entry for nonexistent node");
}

/// Calling delete_by_node_id twice on the same (already-deleted) node is idempotent.
#[test]
fn delete_by_node_id_idempotent_after_first_delete() {
    let p = provider();
    let ino = write_file(&p, ROOT, "f.txt", b"x");
    let node_id = p.node_id_for(ino).unwrap().unwrap();

    p.delete_by_node_id(&node_id).unwrap();
    // Second call: node is already gone — should not error, no new outbox entry.
    let outbox_before = p.outbox_list().unwrap();
    p.delete_by_node_id(&node_id).unwrap();
    let outbox_after = p.outbox_list().unwrap();
    assert_eq!(outbox_before, outbox_after, "second delete must not change the outbox");
}

// ---- resolve_path ---------------------------------------------------------------

/// A simple one-component path resolves to (ROOT_INO, "name"), with no directory
/// created (the leaf is not created by resolve_path).
#[test]
fn resolve_path_top_level_file_returns_root_ino() {
    let p = provider();
    let (parent_ino, leaf) = p.resolve_path("readme.txt").unwrap();
    assert_eq!(parent_ino, ROOT, "top-level file's parent is root");
    assert_eq!(leaf, "readme.txt");
    // No directories were created in the tree.
    assert!(p.export_tree().unwrap().is_empty(), "resolve_path must not create the leaf");
}

/// A multi-component path creates the intermediate directories and returns the parent
/// of the leaf.
#[test]
fn resolve_path_creates_intermediate_dirs() {
    let p = provider();
    let (parent_ino, leaf) = p.resolve_path("a/b/c.txt").unwrap();
    assert_eq!(leaf, "c.txt");

    // a must exist at root.
    let a = p.lookup(ROOT, "a").unwrap();
    assert_eq!(a.kind, fs_provider::Kind::Dir);

    // b must exist under a.
    let b = p.lookup(a.ino, "b").unwrap();
    assert_eq!(b.kind, fs_provider::Kind::Dir);

    // parent_ino must be b's inode.
    assert_eq!(parent_ino, b.ino, "returned parent_ino is the deepest dir");
}

/// Calling resolve_path twice with the same path is stable: the second call returns the
/// same (parent_ino, leaf) without creating duplicates.
#[test]
fn resolve_path_is_stable_on_repeat_call() {
    let p = provider();
    let first = p.resolve_path("x/y/z.stl").unwrap();
    let second = p.resolve_path("x/y/z.stl").unwrap();
    assert_eq!(first, second, "resolve_path must be idempotent");

    // Only two directories total (x and y) — no duplicates.
    let tree = p.export_tree().unwrap();
    let dirs: Vec<_> = tree.iter().filter(|n| n.kind == fs_provider::Kind::Dir).collect();
    assert_eq!(dirs.len(), 2, "exactly two intermediate dirs (x, y)");
}

/// Empty string and "/" are invalid path inputs — must return NotFound (or similar
/// error indicating the input makes no sense as a file path).
#[test]
fn resolve_path_empty_or_slash_is_error() {
    let p = provider();
    assert!(
        matches!(p.resolve_path(""), Err(FsError::NotFound)),
        "empty string must be an error"
    );
    assert!(
        matches!(p.resolve_path("/"), Err(FsError::NotFound)),
        "bare slash must be an error"
    );
}

// ---- rename_by_node_id ----------------------------------------------------------

/// Move a file from its current location to a new parent+name: the same node_id is
/// reachable at the new location, the old name is gone, and an "upsert" is enqueued.
#[test]
fn rename_by_node_id_moves_to_new_parent_and_name() {
    let p = provider();
    let src = write_file(&p, ROOT, "old.txt", b"content");
    let node_id = p.node_id_for(src).unwrap().unwrap();

    let dst_dir = p.mkdir(ROOT, "dst").unwrap();

    p.rename_by_node_id(&node_id, dst_dir.ino, "new.txt").unwrap();

    // Old location is gone.
    assert!(matches!(p.lookup(ROOT, "old.txt"), Err(FsError::NotFound)));

    // New location exists and has the same node_id.
    let new_attr = p.lookup(dst_dir.ino, "new.txt").unwrap();
    assert_eq!(
        p.node_id_for(new_attr.ino).unwrap().unwrap(),
        node_id,
        "node_id is stable across rename"
    );

    // Outbox has an "upsert" entry for this node (rename enqueues upsert via the
    // existing Provider::rename path).
    let outbox = p.outbox_list().unwrap();
    assert!(
        outbox.iter().any(|(id, op)| id == &node_id && op == "upsert"),
        "expected upsert for {node_id} in outbox after rename, got {outbox:?}"
    );
}

/// rename_by_node_id with an unknown node_id returns NotFound.
#[test]
fn rename_by_node_id_unknown_node_id_is_not_found() {
    let p = provider();
    let err = p.rename_by_node_id("ghost-id", ROOT, "anywhere.txt");
    assert!(matches!(err, Err(FsError::NotFound)), "expected NotFound, got {err:?}");
}

// ---- additional pinning tests (review fixes) ------------------------------------

/// If an intermediate path component is a FILE (not a directory), resolve_path must
/// return Err(NotADirectory) immediately rather than walking into the file's inode
/// and yielding an opaque error later.
#[test]
fn resolve_path_intermediate_file_is_not_a_directory() {
    let p = provider();
    // Create a top-level file named "foo.txt".
    write_file(&p, ROOT, "foo.txt", b"contents");
    // Attempting to treat "foo.txt" as a directory component must fail fast.
    let err = p.resolve_path("foo.txt/bar.txt");
    assert!(
        matches!(err, Err(FsError::NotADirectory)),
        "expected NotADirectory when intermediate component is a file, got {err:?}"
    );
}

/// rename_by_node_id onto itself (same parent + same name) must succeed and leave the
/// node in place — it delegates to Provider::rename which short-circuits on dst==src.
#[test]
fn rename_by_node_id_onto_self_is_ok() {
    let p = provider();
    let ino = write_file(&p, ROOT, "self.txt", b"data");
    let node_id = p.node_id_for(ino).unwrap().unwrap();

    // Rename to the same parent and same name — must not error.
    p.rename_by_node_id(&node_id, ROOT, "self.txt").unwrap();

    // The node must still be found at the original location with the same node_id.
    let attr = p.lookup(ROOT, "self.txt").unwrap();
    assert_eq!(
        p.node_id_for(attr.ino).unwrap().unwrap(),
        node_id,
        "node_id must be stable after a rename-onto-self"
    );
}

// ---- parent_and_name (CfAPI "new remote node" bridge) ---------------------------

/// A top-level file resolves to (ROOT, name); a nested file resolves to (its dir's ino,
/// leaf name). This is what the CfAPI `created` hook uses to locate a new node's parent
/// directory + build its placeholder.
#[test]
fn parent_and_name_returns_parent_ino_and_leaf() {
    let p = provider();
    let top = write_file(&p, ROOT, "top.txt", b"x");
    let (pi, name) = p.parent_and_name(top).unwrap().unwrap();
    assert_eq!(pi, ROOT, "top-level file's parent is root");
    assert_eq!(name, "top.txt");

    let dir = p.mkdir(ROOT, "d").unwrap();
    let nested = write_file(&p, dir.ino, "n.txt", b"y");
    let (pi2, name2) = p.parent_and_name(nested).unwrap().unwrap();
    assert_eq!(pi2, dir.ino, "nested file's parent is its directory");
    assert_eq!(name2, "n.txt");
}

/// The root inode has no parent → None (not an error). An unknown inode is also None.
#[test]
fn parent_and_name_root_and_unknown_are_none() {
    let p = provider();
    assert!(p.parent_and_name(ROOT).unwrap().is_none(), "root has no parent");
    assert!(p.parent_and_name(999_999).unwrap().is_none(), "unknown ino → None");
}
