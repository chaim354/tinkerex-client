//! High-volume mixed-op stress on one shared `Arc<SqliteProvider>`.
//!
//! The daemon shares a single provider between the mount thread(s) and the sync thread.
//! This drives many threads through a churn of create/write/flush/truncate/rename/unlink/
//! read/readdir against an overlapping name space and asserts the invariants that must hold
//! no matter how the ops interleave:
//!   - no panic, no abort, no deadlock (the test simply completing proves liveness);
//!   - the tree stays structurally sound (every listed node is `lookup`-able and, if a file,
//!     readable — no orphan rows, no dangling manifests, no poisoned mutex);
//!   - content is never torn: any committed file reads back as a length-consistent buffer.
//!
//! Ops that lose a race (a name already taken, a node just unlinked by a peer) must surface
//! as a clean `Err`, never a panic — so every call tolerates the expected error set.

mod common;
use common::*;

use std::sync::Arc;
use std::thread;

use fs_provider::{FsError, Provider, SqliteProvider};

/// A cheap per-thread xorshift so each thread walks a different, deterministic op stream.
fn rng(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Errors a racing op may legitimately hit (a peer got there first). Anything else is a bug.
fn tolerable(e: &FsError) -> bool {
    matches!(
        e,
        FsError::NotFound
            | FsError::AlreadyExists
            | FsError::NotEmpty
            | FsError::IsADirectory
            | FsError::NotADirectory
            | FsError::InvalidArgument
    )
}

#[test]
fn mixed_op_churn_keeps_the_tree_sound() {
    let p = Arc::new(provider());
    // A fixed, shared pool of names so threads actively contend (create/unlink/rename races).
    const NAMES: usize = 24;
    let n_threads = 8;
    let iters = 1500;

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let p = Arc::clone(&p);
        handles.push(thread::spawn(move || {
            let mut st = 0x1234_5678u64.wrapping_add((t as u64 + 1).wrapping_mul(0x9e3779b9));
            for _ in 0..iters {
                let pick = format!("f{}", rng(&mut st) as usize % NAMES);
                let op = rng(&mut st) % 8;
                let result: Result<(), FsError> = (|| {
                    match op {
                        0 => {
                            // create + write + flush
                            let a = p.create(ROOT, &pick)?;
                            let len = 1 + (rng(&mut st) as usize % 800);
                            let body = vec![(t as u8).wrapping_add(len as u8); len];
                            p.write(a.ino, 0, &body)?;
                            p.flush(a.ino)?;
                        }
                        1 => {
                            // overwrite shorter (truncate 0 then write) — stale-tail path
                            if let Ok(a) = p.lookup(ROOT, &pick) {
                                p.truncate(a.ino, 0)?;
                                let len = 1 + (rng(&mut st) as usize % 200);
                                p.write(a.ino, 0, &vec![0xAB; len])?;
                                p.flush(a.ino)?;
                            }
                        }
                        2 => {
                            // partial write into the middle then flush
                            if let Ok(a) = p.lookup(ROOT, &pick) {
                                let off = (rng(&mut st) % 400) as u64;
                                p.write(a.ino, off, b"PATCHED")?;
                                p.flush(a.ino)?;
                            }
                        }
                        3 => {
                            let _ = p.unlink(ROOT, &pick);
                        }
                        4 => {
                            // rename to another pool name (atomic replace path)
                            let dst = format!("f{}", rng(&mut st) as usize % NAMES);
                            let _ = p.rename(ROOT, &pick, ROOT, &dst);
                        }
                        5 => {
                            // read whole file (must never panic / return torn length)
                            if let Ok(a) = p.lookup(ROOT, &pick) {
                                if a.kind == Kind::File {
                                    let got = p.read(a.ino, 0, a.size as u32 + 16)?;
                                    assert!(got.len() <= a.size as usize + 16);
                                }
                            }
                        }
                        6 => {
                            let _ = p.readdir(ROOT)?;
                        }
                        _ => {
                            // mkdir / rmdir churn under a sub-namespace
                            let d = format!("d{}", rng(&mut st) as usize % 6);
                            match rng(&mut st) % 2 {
                                0 => {
                                    let _ = p.mkdir(ROOT, &d);
                                }
                                _ => {
                                    let _ = p.rmdir(ROOT, &d);
                                }
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    assert!(tolerable(&e), "non-tolerable error under churn: {e:?}");
                }
            }
        }));
    }
    for h in handles {
        h.join()
            .expect("a stress thread panicked — race / poisoned lock");
    }

    // Post-condition: the tree is sound. Every entry readdir reports must be lookup-able,
    // and every file must read back a buffer matching its reported size (no torn commit, no
    // orphan row, no dangling manifest).
    let entries = p.readdir(ROOT).unwrap();
    for e in &entries {
        let attr = p
            .lookup(ROOT, &e.name)
            .unwrap_or_else(|_| panic!("readdir listed {} but lookup failed — orphan row", e.name));
        if attr.kind == Kind::File {
            let content = p
                .read(attr.ino, 0, attr.size as u32)
                .unwrap_or_else(|err| panic!("file {} unreadable post-churn: {err:?}", e.name));
            assert_eq!(
                content.len(),
                attr.size as usize,
                "file {} size/content mismatch — torn state",
                e.name
            );
        }
    }

    // And the provider is still fully functional (lock not poisoned).
    let f = p.create(ROOT, "final-sanity").unwrap();
    p.write(f.ino, 0, b"ok").unwrap();
    p.flush(f.ino).unwrap();
    assert_eq!(read_all(&p, f.ino), b"ok");
}

/// A focused race on ONE file: many threads create/overwrite/unlink the same name. The
/// invariant: it never panics, and whatever the final state, a committed read is consistent.
#[test]
fn hammer_a_single_name_no_corruption() {
    let p: Arc<SqliteProvider> = Arc::new(provider());
    let n = 12;
    let mut handles = Vec::new();
    for t in 0..n {
        let p = Arc::clone(&p);
        handles.push(thread::spawn(move || {
            let mut st = (t as u64 + 1) * 0xDEAD_BEEF;
            for _ in 0..500 {
                match rng(&mut st) % 4 {
                    0 => {
                        if let Ok(a) = p.create(ROOT, "hot") {
                            let _ = p.write(a.ino, 0, &vec![t as u8; 64]);
                            let _ = p.flush(a.ino);
                        }
                    }
                    1 => {
                        if let Ok(a) = p.lookup(ROOT, "hot") {
                            let _ = p.truncate(a.ino, 0);
                            let _ = p.write(a.ino, 0, b"short");
                            let _ = p.flush(a.ino);
                        }
                    }
                    2 => {
                        let _ = p.unlink(ROOT, "hot");
                    }
                    _ => {
                        if let Ok(a) = p.lookup(ROOT, "hot") {
                            let _ = p.read(a.ino, 0, 4096);
                        }
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("single-name hammer panicked");
    }
    // Final state is consistent (present-and-readable, or absent — both fine).
    if let Ok(a) = p.lookup(ROOT, "hot") {
        let c = p.read(a.ino, 0, a.size as u32).unwrap();
        assert_eq!(c.len(), a.size as usize);
    }
}
