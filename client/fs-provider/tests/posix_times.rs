//! pjdfstest-style **timestamp** semantics (`utimensat`) at the Provider level.
//!
//! pjdfstest's `utimensat` suite checks that atime and mtime can be set independently and
//! read back, and that the `UTIME_NOW` / `UTIME_OMIT` special values behave (set-to-now /
//! leave-unchanged). The kernel resolves those special values *before* they reach the
//! filesystem — `UTIME_OMIT` arrives as "this timestamp not in the setattr", `UTIME_NOW`
//! as the current wall clock — so at the Provider seam the contract is simply:
//!   - `set_mtime(ino, t)` / `set_atime(ino, t)` set that one timestamp to `t` (nanos),
//!   - the other timestamp is left untouched (the UTIME_OMIT case),
//!   - `getattr` reports both.
//!
//! **atime is device-local** (an access time, like the advisory locks) and is deliberately
//! NOT carried in the sync export — syncing it would be wrong (every device has its own
//! access pattern) and chatty. mtime *is* synced (the file's true modification time). The
//! last test pins that split.

mod common;
use common::*;

use fs_provider::Provider;

#[test]
fn set_mtime_and_atime_independently_then_read_both_back() {
    let p = provider();
    let ino = write_file(&p, ROOT, "f", b"x");

    // Set mtime only; atime must be untouched (the UTIME_OMIT-on-atime case).
    let before = p.getattr(ino).unwrap();
    p.set_mtime(ino, 1_000_000_000).unwrap();
    let a = p.getattr(ino).unwrap();
    assert_eq!(a.mtime, 1_000_000_000, "mtime updated");
    assert_eq!(a.atime, before.atime, "atime untouched when only mtime is set");

    // Set atime only; mtime must be untouched (the UTIME_OMIT-on-mtime case).
    p.set_atime(ino, 2_000_000_000).unwrap();
    let a = p.getattr(ino).unwrap();
    assert_eq!(a.atime, 2_000_000_000, "atime updated");
    assert_eq!(a.mtime, 1_000_000_000, "mtime untouched when only atime is set");
}

#[test]
fn set_atime_on_missing_inode_is_not_found() {
    let p = provider();
    assert!(matches!(p.set_atime(999_999, 1), Err(fs_provider::FsError::NotFound)));
}

#[test]
fn atime_is_device_local_and_not_synced_but_mtime_is() {
    // A→B replication carries mtime verbatim (the file's true modification time), but NOT
    // atime — atime is per-device access state. After A sets a distinctive atime and
    // syncs, B's copy must reflect A's mtime, and B's atime must be whatever B has locally
    // (i.e. A's atime is never exported / applied).
    let (a, b, _store) = shared_store_pair();
    let ino = write_file(&a, ROOT, "f", b"hello");
    a.set_mtime(ino, 1_234_500_000_000).unwrap();
    a.set_atime(ino, 9_999_900_000_000).unwrap();

    sync_to(&a, &[&b]);

    let bf = b.lookup(ROOT, "f").unwrap();
    assert_eq!(bf.mtime, 1_234_500_000_000, "mtime synced A->B");
    assert_ne!(bf.atime, 9_999_900_000_000, "atime is device-local, NOT synced from A");
}
