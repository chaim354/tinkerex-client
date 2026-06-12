//! Multi-replica convergence — the ownCloud/Nextcloud `FakeFolder` pattern, plus
//! Dropbox-style randomized state-machine checking: drive random create/overwrite/
//! delete/move/replace ops on a writer, fan the changelog out to N replicas, and assert
//! every replica's tree is byte-identical to the writer after **every** op (content
//! equality is implied by the content-addressed `manifest_id` in the snapshot).
//!
//! Also covers the properties a sync core must hold beyond convergence: idempotent
//! re-apply, durability across a process restart (reopen from disk), and replication of
//! the atomic-save replace-rename.

mod common;
use common::*;

use fs_provider::{LocalChunkStore, Provider, SqliteProvider};
use proptest::prelude::*;

// ---- model-based multi-replica convergence -----------------------------------------

struct Entry {
    ino: u64,
    parent: u64,
    name: String,
    dir: bool,
}

/// Run `iters` random ops against a writer, syncing to `n_replicas` after each, and
/// assert convergence every step. Returns the number of surviving nodes (so the caller
/// can assert the run did non-trivial work).
fn run_convergence(seed: u64, n_replicas: usize, iters: usize) -> usize {
    let writer = provider();
    let replicas: Vec<SqliteProvider> = (0..n_replicas).map(|_| provider()).collect();
    let refs: Vec<&SqliteProvider> = replicas.iter().collect();

    let mut entries: Vec<Entry> = Vec::new();
    let mut ctr = 0u64;
    let mut state = seed ^ 0x9e3779b97f4a7c15;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };

    for iter in 0..iters {
        let dirs: Vec<u64> = std::iter::once(ROOT).chain(entries.iter().filter(|e| e.dir).map(|e| e.ino)).collect();
        let files: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| !e.dir).map(|(i, _)| i).collect();

        match next() % 7 {
            0 => {
                let parent = dirs[next() % dirs.len()];
                ctr += 1;
                let name = format!("d{ctr}");
                let a = writer.mkdir(parent, &name).unwrap();
                entries.push(Entry { ino: a.ino, parent, name, dir: true });
            }
            1 => {
                let parent = dirs[next() % dirs.len()];
                ctr += 1;
                let name = format!("f{ctr}");
                let a = writer.create(parent, &name).unwrap();
                let body: Vec<u8> = (0..(40 + next() % 500)).map(|i| (i ^ ctr as usize) as u8).collect();
                writer.write(a.ino, 0, &body).unwrap();
                writer.flush(a.ino).unwrap();
                entries.push(Entry { ino: a.ino, parent, name, dir: false });
            }
            2 => {
                if let Some(&i) = files.get(next() % files.len().max(1)).filter(|_| !files.is_empty()) {
                    let e = &entries[i];
                    let body: Vec<u8> = (0..(20 + next() % 300)).map(|j| j.wrapping_add(iter) as u8).collect();
                    writer.truncate(e.ino, 0).unwrap();
                    writer.write(e.ino, 0, &body).unwrap();
                    writer.flush(e.ino).unwrap();
                }
            }
            3 => {
                if !files.is_empty() {
                    let e = entries.remove(files[next() % files.len()]);
                    writer.unlink(e.parent, &e.name).unwrap();
                }
            }
            4 => {
                let empty: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.dir && !entries.iter().any(|c| c.parent == e.ino))
                    .map(|(i, _)| i)
                    .collect();
                if !empty.is_empty() {
                    let e = entries.remove(empty[next() % empty.len()]);
                    writer.rmdir(e.parent, &e.name).unwrap();
                }
            }
            5 => {
                // move a file to a (possibly different) dir under a fresh name
                if !files.is_empty() {
                    let i = files[next() % files.len()];
                    let newparent = dirs[next() % dirs.len()];
                    ctr += 1;
                    let newname = format!("f{ctr}");
                    let (op, on) = (entries[i].parent, entries[i].name.clone());
                    writer.rename(op, &on, newparent, &newname).unwrap();
                    entries[i].parent = newparent;
                    entries[i].name = newname;
                }
            }
            _ => {
                // atomic-save replace: rename a file *over* another existing file
                if files.len() >= 2 {
                    let si = files[next() % files.len()];
                    let mut di = files[next() % files.len()];
                    if di == si {
                        di = files[(files.iter().position(|&x| x == si).unwrap() + 1) % files.len()];
                    }
                    if di != si {
                        let src_ino = entries[si].ino;
                        let dst_ino = entries[di].ino;
                        let (sp, sn) = (entries[si].parent, entries[si].name.clone());
                        let (dp, dn) = (entries[di].parent, entries[di].name.clone());
                        writer.rename(sp, &sn, dp, &dn).unwrap();
                        // The displaced dst node is gone; src now lives at dst's path.
                        entries.retain(|e| e.ino != dst_ino);
                        let s = entries.iter_mut().find(|e| e.ino == src_ino).unwrap();
                        s.parent = dp;
                        s.name = dn;
                    }
                }
            }
        }

        sync_to(&writer, &refs);
        let w = tree_snapshot(&writer);
        for (k, r) in replicas.iter().enumerate() {
            assert_eq!(tree_snapshot(r), w, "replica {k} diverged after iter {iter} (seed {seed})");
        }
    }
    entries.len()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Three replicas stay identical to the writer across a random op stream (seeded so
    /// proptest can shrink to a minimal failing seed).
    #[test]
    fn three_replicas_converge(seed in any::<u64>()) {
        run_convergence(seed, 3, 120);
    }
}

#[test]
fn convergence_does_nontrivial_work() {
    // A sanity floor so the proptest above can't pass by building nothing.
    let mut built_something = false;
    for seed in 0..8u64 {
        if run_convergence(seed, 2, 150) > 0 {
            built_something = true;
        }
    }
    assert!(built_something, "harness should leave a surviving tree for at least one seed");
}

// ---- idempotency / durability ------------------------------------------------------

#[test]
fn apply_remote_is_idempotent() {
    let (a, b, _dir) = shared_store_pair();
    let d = a.mkdir(ROOT, "proj").unwrap();
    write_file(&a, d.ino, "x.bin", b"hello world");
    let nodes = a.export_tree().unwrap();

    for n in &nodes {
        b.apply_remote(n).unwrap();
    }
    let once = tree_snapshot(&b);
    // Applying the very same changes again must be a no-op (re-delivered changelog
    // entries / a replayed pull must not duplicate or corrupt anything).
    for n in &nodes {
        b.apply_remote(n).unwrap();
    }
    assert_eq!(tree_snapshot(&b), once, "re-applying the same nodes is idempotent");
}

#[test]
fn state_persists_across_reopen() {
    // Durability: a provider backed by an on-disk DB + chunk dir must reconstruct its
    // tree and content after a process restart (drop + reopen the same paths).
    let dir = tempdir::Dir::new();
    let db = dir.path().join("meta.db");
    let chunks = dir.path().join("chunks");
    std::fs::create_dir_all(&chunks).unwrap();

    let body: Vec<u8> = (0..5000u32).map(|i| (i % 97) as u8).collect();
    let (proj_id, file_id) = {
        let p = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
        let d = p.mkdir(ROOT, "proj").unwrap();
        let ino = write_file(&p, d.ino, "part.bin", &body);
        (p.node_id_for(d.ino).unwrap().unwrap(), p.node_id_for(ino).unwrap().unwrap())
    };

    // Reopen from the same files.
    let p2 = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
    let proj = p2.lookup(ROOT, "proj").unwrap();
    assert_eq!(p2.node_id_for(proj.ino).unwrap().unwrap(), proj_id, "dir id survives restart");
    let f = p2.lookup(proj.ino, "part.bin").unwrap();
    assert_eq!(p2.node_id_for(f.ino).unwrap().unwrap(), file_id, "file id survives restart");
    assert_eq!(read_all(&p2, f.ino), body, "content survives restart (chunks on disk)");
}

#[test]
fn truncate_to_zero_works_on_unhydrated_placeholder() {
    // A device that pulled a file's manifest but never downloaded its bytes (a
    // placeholder) must still be able to O_TRUNC-overwrite it — the kernel issues
    // truncate(0) before the new write. truncate(0) must NOT try to read the (absent)
    // base content.
    let src = provider();
    write_file(&src, ROOT, "f.bin", &vec![7u8; 4000]);
    let node = node_named(&src, "f.bin");

    // Replica with its OWN empty store → it holds the manifest but none of the chunks.
    let dir = tempdir::Dir::new();
    let replica = SqliteProvider::in_memory(
        Box::new(LocalChunkStore::new(dir.path()).unwrap()),
        fs_provider::ChunkConfig { min: 64, avg: 256, max: 1024 },
    )
    .unwrap();
    replica.apply_remote(&node).unwrap();
    let ino = replica.lookup(ROOT, "f.bin").unwrap().ino;
    assert!(replica.read(ino, 0, 10).is_err(), "placeholder bytes are genuinely absent");

    // O_TRUNC overwrite: truncate to empty (must succeed without the base), then write.
    replica.truncate(ino, 0).expect("truncate(0) must not require hydration");
    replica.write(ino, 0, b"rewritten").unwrap();
    replica.flush(ino).unwrap();
    assert_eq!(read_all(&replica, ino), b"rewritten");
}

#[test]
fn replica_converges_on_replace_rename() {
    // The atomic-save replace must replicate: the displaced node disappears on the
    // replica and the surviving entry carries the source's content — no duplicate.
    let writer = provider();
    let replica = provider();
    write_file(&writer, ROOT, "note.txt", b"old longer body");
    write_file(&writer, ROOT, "note.txt.tmp", b"new");
    sync_to(&writer, &[&replica]);
    assert_eq!(tree_snapshot(&replica), tree_snapshot(&writer));

    writer.rename(ROOT, "note.txt.tmp", ROOT, "note.txt").unwrap();
    sync_to(&writer, &[&replica]);

    assert_eq!(tree_snapshot(&replica), tree_snapshot(&writer), "replica converged on the replace");
    let n = replica.readdir(ROOT).unwrap().iter().filter(|e| e.name == "note.txt").count();
    assert_eq!(n, 1, "no duplicate note.txt on the replica");
    assert!(replica.lookup(ROOT, "note.txt.tmp").is_err(), "tmp gone on replica");
}
