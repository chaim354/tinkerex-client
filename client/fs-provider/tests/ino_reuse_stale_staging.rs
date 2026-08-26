//! Regression: a reused inode must not serve a previous file's staged (uncommitted) bytes.
//!
//! `nodes.ino` is `INTEGER PRIMARY KEY` *without* AUTOINCREMENT, so SQLite **reuses**
//! rowids: delete the highest-rowid node and the next INSERT gets that same rowid back.
//! The write path keys its in-memory `staging` buffer by ino and only clears it on
//! `flush`. If a file is opened for write (staging seeded), then its node is removed
//! (unlink / remote delete) WITHOUT clearing staging, a later INSERT that reuses the ino —
//! e.g. a remote create landing via `apply_remote` — inherits the dead file's stale bytes.
//! A `read` of the new node then returns the OLD file's content instead of its real
//! committed manifest: silent cross-file corruption.

mod common;
use common::*;

use fs_provider::Provider;

/// Remote create reusing a dead ino must read its OWN content, not stale staging bytes.
#[test]
fn remote_create_reusing_an_ino_does_not_serve_stale_staging() {
    let (a, b, _dir) = shared_store_pair();

    // On A: author a real file so B has committed chunks to hydrate from.
    let victim_body = b"NEW FILE REAL CONTENT - must be what we read back";
    write_file(&a, ROOT, "newfile", victim_body);
    let new_node = node_named(&a, "newfile");

    // On B: open a local file for write and leave it UNflushed, so staging[ino] holds
    // poison bytes. Make it the highest rowid so its ino is the one SQLite reuses next.
    let doomed = b.create(ROOT, "doomed").unwrap();
    b.write(
        doomed.ino,
        0,
        b"POISON STALE STAGING BYTES THAT MUST NOT LEAK",
    )
    .unwrap();
    // Drop the doomed node's directory row (as a remote delete would) but DON'T flush —
    // staging[doomed.ino] survives. Use unlink to remove the row.
    b.unlink(ROOT, "doomed").unwrap();

    // Now apply the remote file. With ino reuse, the fresh INSERT may grab doomed.ino.
    b.apply_remote(&new_node).unwrap();

    // Find the applied node's ino and read it. It MUST be its own committed content.
    let ino = b.ino_for_node_id(&new_node.node_id).unwrap().unwrap();
    let got = read_all(&b, ino);
    assert_eq!(
        got, victim_body,
        "reused ino served stale staging bytes from the deleted file (ino={ino}, doomed={})",
        doomed.ino
    );
}

/// A locally re-created file (create over a just-deleted highest-rowid ino) must start
/// empty — `create` resets staging, so this is the control that should already pass.
#[test]
fn local_recreate_over_reused_ino_starts_empty() {
    let p = provider();
    let a = p.create(ROOT, "a").unwrap();
    p.write(a.ino, 0, b"stale").unwrap();
    p.unlink(ROOT, "a").unwrap();
    let c = p.create(ROOT, "c").unwrap();
    p.flush(c.ino).unwrap();
    assert_eq!(read_all(&p, c.ino), b"", "recreated file should be empty");
}
