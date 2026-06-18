//! A `flush` whose chunk upload fails must NOT lose the staged edit.
//!
//! `flush` takes the staging buffer out of the map up front, then chunks + `store.put`s
//! each piece, then commits the manifest in a DB transaction. If a `put` fails (storage
//! full, a network blip mid-finalize, a presigned-URL timeout) the staged buffer must be
//! preserved so the edit can be retried — otherwise the user's write is silently dropped
//! while the node keeps its OLD content. Apps rarely retry a failed flush, so dropping it
//! here is real data loss.
//!
//! We drive this with a fault-injecting ChunkStore that fails the next `put`, flush, then
//! recover (stop failing) and flush again — the content must commit, not be lost.

mod common;
use common::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fs_provider::{ChunkStore, Provider, SqliteProvider};

/// A store that fails every `put` while `fail` is set, delegating to an inner local store.
struct FlakyStore {
    inner: fs_provider::LocalChunkStore,
    fail: Arc<AtomicBool>,
}

impl ChunkStore for FlakyStore {
    fn get(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        self.inner.get(hash)
    }
    fn put(&self, hash: &str, data: &[u8]) -> std::io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected put failure",
            ));
        }
        self.inner.put(hash, data)
    }
    fn has(&self, hash: &str) -> bool {
        self.inner.has(hash)
    }
}

#[test]
fn failed_flush_preserves_the_staged_edit_for_retry() {
    let dir = tempdir::Dir::new();
    let fail = Arc::new(AtomicBool::new(false));
    let store = FlakyStore {
        inner: fs_provider::LocalChunkStore::new(dir.path()).unwrap(),
        fail: Arc::clone(&fail),
    };
    let p = SqliteProvider::in_memory(Box::new(store), TINY).unwrap();

    // Commit an initial version cleanly so the node has a known-good prior manifest.
    let f = p.create(ROOT, "doc.txt").unwrap();
    p.write(f.ino, 0, b"version one").unwrap();
    p.flush(f.ino).unwrap();
    assert_eq!(read_all(&p, f.ino), b"version one");

    // Now stage a new edit and make the upload fail.
    p.write(
        f.ino,
        0,
        b"version two is much longer than version one here",
    )
    .unwrap();
    fail.store(true, Ordering::SeqCst);
    let res = p.flush(f.ino);
    assert!(res.is_err(), "flush should surface the upload failure");

    // The edit must NOT be lost: either it's still staged (a read shows the new bytes) or a
    // retry recommits it. Recover the store and flush again — the new content must land.
    fail.store(false, Ordering::SeqCst);
    p.flush(f.ino).unwrap();
    assert_eq!(
        read_all(&p, f.ino),
        b"version two is much longer than version one here",
        "staged edit was lost after a failed flush — silent data loss"
    );
}

#[test]
fn read_after_failed_flush_does_not_show_partial_or_old_then_lose_new() {
    // A read taken between the failed flush and the successful retry must reflect the live
    // staged edit (the user's in-progress content), never a torn mix.
    let dir = tempdir::Dir::new();
    let fail = Arc::new(AtomicBool::new(false));
    let store = FlakyStore {
        inner: fs_provider::LocalChunkStore::new(dir.path()).unwrap(),
        fail: Arc::clone(&fail),
    };
    let p = SqliteProvider::in_memory(Box::new(store), TINY).unwrap();

    let f = p.create(ROOT, "x").unwrap();
    p.write(f.ino, 0, b"AAAA").unwrap();
    p.flush(f.ino).unwrap();

    p.write(f.ino, 0, b"BBBBBBBB").unwrap();
    fail.store(true, Ordering::SeqCst);
    let _ = p.flush(f.ino); // fails

    // The staged edit should still be observable (not silently reverted to "AAAA").
    let mid = read_all(&p, f.ino);
    assert_eq!(
        mid, b"BBBBBBBB",
        "in-progress edit vanished after a failed flush"
    );

    fail.store(false, Ordering::SeqCst);
    p.flush(f.ino).unwrap();
    assert_eq!(read_all(&p, f.ino), b"BBBBBBBB");
}
