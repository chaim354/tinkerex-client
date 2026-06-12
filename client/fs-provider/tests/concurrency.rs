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
            assert_eq!(read_all(&p, ino), want, "t{t}-f{i} content intact under concurrency");
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
    assert_eq!(tree_snapshot(&replica), tree_snapshot(&writer), "replica converged after concurrent drain");
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
            store.put(hash, &bytes).expect("concurrent put of same hash must succeed");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(store.has(hash));
    assert_eq!(store.get(hash).unwrap(), bytes, "stored bytes intact after the race");
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
