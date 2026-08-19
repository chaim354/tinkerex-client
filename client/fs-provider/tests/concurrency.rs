//! Thread-safety of the provider + chunk store. The daemon shares one
//! `Arc<SqliteProvider>` between the mount thread (reads/writes) and the sync thread
//! (outbox drain + apply), so concurrent access must not panic, deadlock, or corrupt
//! the tree — and the content-addressed store must tolerate two threads writing the
//! same chunk hash at once (the dedup race that already bit us once).

mod common;
use common::*;

use std::sync::Arc;
use std::thread;

use fs_provider::{ChunkStore, LocalChunkStore, Provider};

#[test]
fn concurrent_writers_to_distinct_files_all_commit() {
    let p = Arc::new(provider());
    let n_threads = 8;
    let per_thread = 12;

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let p = Arc::clone(&p);
        handles.push(thread::spawn(move || {
            for i in 0..per_thread {
                let name = format!("t{t}-f{i}.bin");
                let body: Vec<u8> = (0..(500 + i * 37)).map(|b| (b ^ (t * 7)) as u8).collect();
                let f = p.create(ROOT, &name).unwrap();
                p.write(f.ino, 0, &body).unwrap();
                p.flush(f.ino).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Every file exists with the exact content its writer produced.
    assert_eq!(p.readdir(ROOT).unwrap().len(), n_threads * per_thread);
    for t in 0..n_threads {
        for i in 0..per_thread {
            let want: Vec<u8> = (0..(500 + i * 37)).map(|b| (b ^ (t * 7)) as u8).collect();
            let ino = p.lookup(ROOT, &format!("t{t}-f{i}.bin")).unwrap().ino;
            assert_eq!(
                read_all(&p, ino),
                want,
                "t{t}-f{i} content intact under concurrency"
            );
        }
    }
}

#[test]
fn concurrent_writes_and_outbox_drain_dont_deadlock_or_lose_data() {
    // One thread writes files; another continuously drains the outbox into a replica
    // (the mount-thread vs sync-thread interleaving). At the end the replica, after a
    // final drain, must match the writer exactly.
    let writer = Arc::new(provider());
    let replica = Arc::new(provider());

    let w = Arc::clone(&writer);
    let producer = thread::spawn(move || {
        for i in 0..60 {
            let f = w.create(ROOT, &format!("f{i}.bin")).unwrap();
            w.write(f.ino, 0, &vec![i as u8; 300 + i]).unwrap();
            w.flush(f.ino).unwrap();
        }
    });

    let w2 = Arc::clone(&writer);
    let r2 = Arc::clone(&replica);
    let syncer = thread::spawn(move || {
        // Spin draining until the producer is well underway; bounded so it always ends.
        for _ in 0..400 {
            for (node_id, op) in w2.outbox_list().unwrap() {
                if op == "delete" {
                    r2.apply_remote_delete(&node_id).unwrap();
                } else if let Some(n) = w2.sync_node(&node_id).unwrap() {
                    // A parent may not have drained yet on the replica; skip & retry.
                    let _ = r2.apply_remote(&n);
                }
                w2.outbox_clear(&node_id).unwrap();
            }
            thread::yield_now();
        }
    });

    producer.join().unwrap();
    syncer.join().unwrap();

    // Final reconciling drain (covers anything the racing syncer skipped on a missing
    // parent — here all files are under root, so this is just the tail).
    sync_to(&writer, &[&replica]);
    assert_eq!(
        tree_snapshot(&replica),
        tree_snapshot(&writer),
        "replica converged after concurrent drain"
    );
}

#[test]
fn chunkstore_tolerates_concurrent_same_hash_writes() {
    // The dedup race: N threads store the identical (hash, bytes) at once. Every put
    // must succeed (the loser of the rename treats it as a dedup win) and the bytes
    // must be intact and readable.
    let dir = tempdir::Dir::new();
    let store = Arc::new(LocalChunkStore::new(dir.path()).unwrap());
    let hash = "deadbeefcafe00112233445566778899aabbccddeeff00112233445566778899";
    let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();

    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let bytes = bytes.clone();
        handles.push(thread::spawn(move || {
            store
                .put(hash, &bytes)
                .expect("concurrent put of same hash must succeed");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(store.has(hash));
    assert_eq!(
        store.get(hash).unwrap(),
        bytes,
        "stored bytes intact after the race"
    );
}

#[test]
fn chunkstore_put_is_idempotent_dedup() {
    let dir = tempdir::Dir::new();
    let store = LocalChunkStore::new(dir.path()).unwrap();
    let hash = "00aa11bb22cc33dd44ee55ff66001122334455667788990011223344556677ab";
    store.put(hash, b"first").unwrap();
    // Content-addressed: a second put for the same hash is a no-op (keeps the first
    // bytes; we never overwrite an existing chunk).
    store.put(hash, b"first").unwrap();
    assert!(store.has(hash));
    assert_eq!(store.get(hash).unwrap(), b"first");
}

// ---------------------------------------------------------------------------
// Lock-order regression: `staging` must be a LEAF lock.
//
// The mount thread and the sync thread share one `Arc<SqliteProvider>`
// (`sync-engine/src/daemon.rs` puts the FUSE session on its own thread), so both of the
// provider's mutexes are contended by design. `getattr` on an inode that is open for
// write used to take `staging` and then, still holding it, `conn`; every caller of
// `purge_inode_state` (`unlink`, `rmdir`, `rename`, `apply_remote`) holds `conn` at
// function scope and then takes `staging`. That is a textbook ABBA inversion and it
// wedges the mount — only a remount clears it.
//
// **These tests must never `join()` their workers.** `std::thread::JoinHandle` has no
// timed join, so joining a deadlocked thread hangs the runner forever, and
// `.github/workflows/rust-client.yml` runs this suite on the single registered
// self-hosted mac. Instead each worker bumps an `AtomicU64` and the test thread watches
// both counters: when neither moves for `FROZEN_TICKS` consecutive samples the test
// PANICS (a red test, seconds long) rather than hanging. Wedged workers stay detached
// and die with the test binary.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fs_provider::{Kind, SqliteProvider, SyncNode};

const TICK: Duration = Duration::from_millis(50);
/// Neither side moving for this many consecutive samples (2 s) is a deadlock, not a
/// slow machine — the reproduced wedge happens in well under one second.
const FROZEN_TICKS: u32 = 40;
const DEADLINE: Duration = Duration::from_secs(30);
/// Iterations each side must complete before the pairing counts as proven live.
const TARGET: u64 = 200;

/// Sample two progress counters until both pass `TARGET`, panicking if they freeze
/// together (deadlock) or the deadline expires. Never touches the worker threads.
fn watch_progress(label: &str, a: &AtomicU64, b: &AtomicU64, stop: &AtomicBool) {
    let start = Instant::now();
    let (mut last_a, mut last_b) = (u64::MAX, u64::MAX);
    let mut frozen = 0u32;
    loop {
        thread::sleep(TICK);
        let (na, nb) = (a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
        if na >= TARGET && nb >= TARGET {
            stop.store(true, Ordering::Relaxed);
            return;
        }
        if na == last_a && nb == last_b {
            frozen += 1;
            if frozen >= FROZEN_TICKS {
                stop.store(true, Ordering::Relaxed);
                panic!(
                    "deadlocked ({label}): getattr={na} {label}={nb} — both counters frozen for {:?}",
                    TICK * FROZEN_TICKS
                );
            }
        } else {
            frozen = 0;
        }
        (last_a, last_b) = (na, nb);
        if start.elapsed() > DEADLINE {
            stop.store(true, Ordering::Relaxed);
            panic!(
                "timed out ({label}) after {:?}: getattr={na} {label}={nb}",
                start.elapsed()
            );
        }
    }
}

/// A file created and written but **never flushed** — so it stays in the staging map and
/// every `getattr` on it takes the staged-size path. Returns its inode.
fn permanently_staged(p: &SqliteProvider, name: &str) -> u64 {
    let f = p.create(ROOT, name).unwrap();
    p.write(f.ino, 0, b"open for write").unwrap();
    f.ino
}

/// `getattr` on a staged inode (staging -> conn) against `unlink` (conn -> staging).
#[test]
fn getattr_on_staged_inode_does_not_deadlock_against_unlink() {
    let p = Arc::new(provider());
    let ino = permanently_staged(&p, "staged.bin");

    let (gets, unlinks) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let (p, gets, stop) = (Arc::clone(&p), Arc::clone(&gets), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Staged length, not the committed row's zero.
                assert_eq!(p.getattr(ino).unwrap().size, b"open for write".len() as u64);
                gets.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    {
        let (p, unlinks, stop) = (Arc::clone(&p), Arc::clone(&unlinks), Arc::clone(&stop));
        thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let name = format!("churn-{i}.bin");
                let f = p.create(ROOT, &name).unwrap();
                p.write(f.ino, 0, b"bytes").unwrap();
                p.flush(f.ino).unwrap();
                p.unlink(ROOT, &name).unwrap();
                unlinks.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        });
    }

    watch_progress("unlink", &gets, &unlinks, &stop);
}

/// The same inversion via `rename`'s displaced-destination purge.
#[test]
fn getattr_on_staged_inode_does_not_deadlock_against_rename_replace() {
    let p = Arc::new(provider());
    let ino = permanently_staged(&p, "staged.bin");

    let (gets, renames) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let (p, gets, stop) = (Arc::clone(&p), Arc::clone(&gets), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                p.getattr(ino).unwrap();
                gets.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    {
        let (p, renames, stop) = (Arc::clone(&p), Arc::clone(&renames), Arc::clone(&stop));
        thread::spawn(move || {
            // A destination that always exists, so every rename DISPLACES a node and
            // therefore reaches `purge_inode_state` with a non-empty ino list.
            write_file(&p, ROOT, "dst.bin", b"old");
            while !stop.load(Ordering::Relaxed) {
                write_file(&p, ROOT, "src.bin", b"new");
                p.rename(ROOT, "src.bin", ROOT, "dst.bin").unwrap();
                renames.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    watch_progress("rename", &gets, &renames, &stop);
}

/// The sync thread's face of it: `apply_remote` reaches `purge_inode_state` through the
/// **name-collision sweep**, not a direct removal. Two remote node ids alternate under one
/// name, so each apply displaces the other and the swept list is never empty (an empty
/// list early-returns before the staging lock and would prove nothing).
#[test]
fn getattr_on_staged_inode_does_not_deadlock_against_apply_remote_collision() {
    let p = Arc::new(provider());
    let ino = permanently_staged(&p, "staged.bin");
    // A local row under the contested name, so the very first apply sweeps something.
    write_file(&p, ROOT, "collide.txt", b"local");

    let remote = |id: &str| SyncNode {
        node_id: id.to_string(),
        parent_node_id: None, // root
        name: "collide.txt".to_string(),
        kind: Kind::File,
        manifest_id: None,
        chunks: Vec::new(),
        version_id: None,
        mode: 0o644,
        mtime: 1,
        symlink_target: None,
    };

    let (gets, applies) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let (p, gets, stop) = (Arc::clone(&p), Arc::clone(&gets), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                p.getattr(ino).unwrap();
                gets.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    {
        let (p, applies, stop) = (Arc::clone(&p), Arc::clone(&applies), Arc::clone(&stop));
        let (a, b) = (remote("remote-a"), remote("remote-b"));
        thread::spawn(move || {
            let mut flip = false;
            while !stop.load(Ordering::Relaxed) {
                p.apply_remote(if flip { &a } else { &b }).unwrap();
                applies.fetch_add(1, Ordering::Relaxed);
                flip = !flip;
            }
        });
    }

    watch_progress("apply_remote", &gets, &applies, &stop);
}
