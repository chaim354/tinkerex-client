//! Regressions for the node-removal fixes:
//!  * BUG #6 — `unlink` of a **symlink** must actually remove it (the old `kind = 'file'`
//!    DELETE filter matched 0 rows for a symlink yet still enqueued a delete → a ghost row).
//!  * BUG #8 — a removed inode must carry no stale in-memory state (staging bytes / advisory
//!    locks) and its rowid must not be reused (AUTOINCREMENT), so a new node can't inherit them.

mod common;
use common::*;

use fs_provider::{FsError, LockRange, Provider};

fn whole(owner: u64) -> LockRange {
    LockRange {
        owner,
        start: 0,
        end: u64::MAX,
        exclusive: true,
        pid: owner as u32,
    }
}

#[test]
fn unlink_removes_a_symlink_and_pushes_the_delete() {
    let p = provider();
    let s = p.symlink(ROOT, "link", b"target/path").unwrap();
    let sid = p.node_id_for(s.ino).unwrap().unwrap();
    // Drain the create enqueue so we can see the delete cleanly.
    p.outbox_clear(&sid).unwrap();

    p.unlink(ROOT, "link").unwrap();

    // Gone locally (the old `kind='file'` filter left it in place).
    assert!(
        matches!(p.lookup(ROOT, "link"), Err(FsError::NotFound)),
        "the symlink row must actually be removed"
    );
    // A delete was enqueued for the sync push.
    assert!(
        p.outbox_list()
            .unwrap()
            .iter()
            .any(|(n, op)| n == &sid && op == "delete"),
        "unlink of a symlink enqueues a delete"
    );
    // The name is free — recreating it as a regular file must not hit AlreadyExists (which it
    // would if a ghost row survived).
    let f = p.create(ROOT, "link").unwrap();
    assert_eq!(p.getattr(f.ino).unwrap().kind, Kind::File);
}

#[test]
fn removed_inode_is_not_reused_and_carries_no_stale_state() {
    let p = provider();

    // A file with staged (unflushed) bytes and a held advisory lock.
    let f = p.create(ROOT, "a.bin").unwrap();
    p.write(f.ino, 0, b"secret-staged-bytes").unwrap(); // sits in the staging buffer
    p.set_lock(f.ino, whole(10)).unwrap();
    let old_ino = f.ino;

    p.unlink(ROOT, "a.bin").unwrap();
    assert!(
        matches!(p.getattr(old_ino), Err(FsError::NotFound)),
        "the inode is gone after unlink"
    );

    // A fresh file must get a DISTINCT inode (AUTOINCREMENT — no rowid reuse), so it can never
    // inherit the removed node's staged bytes or lock.
    let g = p.create(ROOT, "b.bin").unwrap();
    assert_ne!(g.ino, old_ino, "a removed inode must not be reused");
    assert_eq!(
        read_all(&p, g.ino),
        Vec::<u8>::new(),
        "the new file starts empty (no stale staged bytes)"
    );

    // And no phantom lock lingers at the freed inode: probing it as a different owner finds
    // nothing held.
    assert!(
        p.test_lock(old_ino, whole(20)).unwrap().is_none(),
        "no phantom advisory lock survives the removed inode"
    );
}
