//! Client-side caching & freshness. Properties the mount depends on:
//!  1. A hydrated file supports random writes (the write path seeds from committed
//!     content — the adapter's open-for-write hydrates the whole file so this can't miss).
//!  2. A pulled remote change updates the content (new manifest → reader re-hydrates) and
//!     carries the source device's **real** `mtime` verbatim (Option B — see
//!     `docs/plan-real-mtime-and-invalidation.md`). The OS cache is dropped *explicitly*
//!     after a content-changing pull (`apply_remote` returns `content_changed`, the adapter
//!     calls `invalidate(ino)`) — we no longer bump mtime to `now` to force the re-read,
//!     which destroyed the file's true modification time and broke build tools.

mod common;
use common::*;

use fs_provider::Provider;

#[test]
fn flush_sets_a_nonzero_mtime() {
    let p = provider();
    let ino = write_file(&p, ROOT, "f.bin", b"hello");
    assert!(p.getattr(ino).unwrap().mtime > 0, "a committed file carries a real mtime, not 0");
}

#[test]
fn remote_update_serves_fresh_content_and_carries_real_mtime() {
    // B has the file cached. A overwrites it. After B applies A's new manifest (what a
    // pull does), B must read the NEW content and carry A's real edit mtime — never the
    // stale copy, and never B's local sync time. (Shared chunk store stands in for the
    // cloud, so B can read A's chunks.)
    let (a, b, _dir) = shared_store_pair();
    let a_ino = write_file(&a, ROOT, "doc.txt", b"version one");
    replicate_tree(&a, &b);

    let b_ino = b.lookup(ROOT, "doc.txt").unwrap().ino;
    let manifest1 = node_named(&b, "doc.txt").manifest_id;
    assert_eq!(read_all(&b, b_ino), b"version one");

    // A overwrites with different content (truncate+write → no stale tail).
    a.truncate(a_ino, 0).unwrap();
    a.write(a_ino, 0, b"a wholly different second version").unwrap();
    a.flush(a_ino).unwrap();
    let a_mtime = a.getattr(a_ino).unwrap().mtime; // A's real edit time
    replicate_tree(&a, &b); // B's pull applies the new manifest in place

    assert_ne!(manifest1, node_named(&b, "doc.txt").manifest_id, "new content → new manifest");
    assert_eq!(
        b.getattr(b_ino).unwrap().mtime, a_mtime,
        "B carries A's real edit mtime, not B's sync time"
    );
    assert_eq!(read_all(&b, b_ino), b"a wholly different second version", "B reads fresh, not stale");
}

#[test]
fn pulled_node_keeps_the_source_mtime_not_now() {
    // Option B's core property: a pull writes the *source device's* real mtime verbatim,
    // never "now". So a file authored long ago shows its true age on every device (the
    // old behavior stamped sync time, which broke build tools). We force an obviously-old
    // mtime on A, sync to B, and assert B reads back that exact value.
    let (a, b, _dir) = shared_store_pair();
    let a_ino = write_file(&a, ROOT, "old.txt", b"authored in 2001");

    // 2001-09-09T01:46:40Z in unix nanoseconds — far from any "now".
    let old_mtime: i64 = 1_000_000_000 * 1_000_000_000;
    a.set_mtime(a_ino, old_mtime).unwrap();
    assert_eq!(a.getattr(a_ino).unwrap().mtime, old_mtime, "set_mtime sticks locally");
    assert_eq!(node_named(&a, "old.txt").mtime, old_mtime, "real mtime is exported for sync");

    replicate_tree(&a, &b);
    let b_ino = b.lookup(ROOT, "old.txt").unwrap().ino;
    assert_eq!(
        b.getattr(b_ino).unwrap().mtime, old_mtime,
        "B sees the source mtime verbatim — not the sync time"
    );
}

#[test]
fn apply_remote_reports_content_change_for_cache_invalidation() {
    // The adapter must drop its OS cache when a pull repoints an existing inode at new
    // content (we no longer bump mtime to force that). `apply_remote` returns that signal.
    let (a, b, _dir) = shared_store_pair();
    let a_ino = write_file(&a, ROOT, "doc.txt", b"v1");
    // First apply: brand-new node on B → nothing cached → no invalidation needed.
    let first = b.apply_remote(&node_named(&a, "doc.txt")).unwrap();
    assert!(!first.content_changed, "a new node has nothing cached to invalidate");
    assert!(first.inserted, "a brand-new node on B → a fresh INSERT (create a placeholder)");
    let b_ino = b.lookup(ROOT, "doc.txt").unwrap().ino;
    assert_eq!(first.ino, b_ino, "outcome carries the local inode");

    // Re-apply the SAME content (metadata-only churn) → not a content change.
    let same = b.apply_remote(&node_named(&a, "doc.txt")).unwrap();
    assert!(!same.content_changed, "same manifest → no invalidation");
    assert!(!same.inserted, "the node already exists → an UPDATE, not an INSERT");

    // A overwrites; the new manifest differs → existing inode must be invalidated.
    a.truncate(a_ino, 0).unwrap();
    a.write(a_ino, 0, b"a different, longer second version").unwrap();
    a.flush(a_ino).unwrap();
    let changed = b.apply_remote(&node_named(&a, "doc.txt")).unwrap();
    assert!(changed.content_changed, "new manifest on an existing inode → invalidate");
    assert!(!changed.inserted, "a content change to an existing node is not an insert");
    assert_eq!(changed.ino, b_ino, "same inode, repointed");
}

#[test]
fn mode_is_preserved_set_and_synced() {
    // The executable bit must stick through chmod, be exported for sync, and round-trip
    // to another device via apply_remote (so `chmod +x` on one device shows up on others).
    let (a, b, _dir) = shared_store_pair();
    let ino = write_file(&a, ROOT, "build.sh", b"#!/bin/sh\necho hi");
    assert_eq!(a.getattr(ino).unwrap().mode, 0o644, "default file mode");

    a.set_mode(ino, 0o755).unwrap();
    assert_eq!(a.getattr(ino).unwrap().mode, 0o755, "chmod +x sticks locally");
    assert_eq!(node_named(&a, "build.sh").mode, 0o755, "mode is exported for sync");

    replicate_tree(&a, &b);
    let b_ino = b.lookup(ROOT, "build.sh").unwrap().ino;
    assert_eq!(b.getattr(b_ino).unwrap().mode, 0o755, "executable bit synced to device B");
}

#[test]
fn mode_survives_a_reopen() {
    use fs_provider::{LocalChunkStore, SqliteProvider};
    let dir = tempdir::Dir::new();
    let db = dir.path().join("m.db");
    let chunks = dir.path().join("chunks");
    std::fs::create_dir_all(&chunks).unwrap();
    {
        let p = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
        let ino = write_file(&p, ROOT, "run", b"x");
        p.set_mode(ino, 0o755).unwrap();
    }
    let p2 = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
    let ino = p2.lookup(ROOT, "run").unwrap().ino;
    assert_eq!(p2.getattr(ino).unwrap().mode, 0o755, "mode persists across restart");
}

#[test]
fn ingest_bytes_replaces_content_and_enqueues_sync() {
    // The CfAPI local-edit capture path: the OS owns the on-disk bytes, so the adapter
    // re-ingests an edited file's whole content as the node's new version. Must replace
    // the content (no stale tail), bump mtime, and enqueue the outbox like a flush.
    let p = provider();
    let ino = write_file(&p, ROOT, "doc.txt", b"the original, longer content");
    let node_id = node_named(&p, "doc.txt").node_id;
    let m1 = p.getattr(ino).unwrap().mtime;
    p.outbox_clear(&node_id).unwrap(); // drain the create

    p.ingest_bytes(ino, b"shorter").unwrap();

    assert_eq!(read_all(&p, ino), b"shorter", "content replaced, no stale tail");
    assert_eq!(p.getattr(ino).unwrap().size, 7);
    assert!(p.getattr(ino).unwrap().mtime >= m1, "mtime advances on ingest");
    assert_eq!(
        p.outbox_list().unwrap(),
        vec![(node_id, "upsert".to_string())],
        "ingest enqueues the node for push"
    );

    // A bad inode is an error, not a silent no-op.
    assert!(p.ingest_bytes(999_999, b"x").is_err());
}

#[test]
fn bind_library_wipes_a_different_librarys_stale_state() {
    // A reused local DB must not mix two libraries: binding it to a different library_id
    // wipes the stale tree (the bug behind the Windows "cursor 14 > head_seq 5" + duplicate
    // placeholders). Same id → kept; different id → wiped; chunk cache untouched.
    let p = provider();
    let _ = write_file(&p, ROOT, "a.bin", b"library one file");
    p.set_cursor(42).unwrap();

    // First bind → no prior library tag, but there's data → reports a wipe and clears it.
    assert!(p.bind_library(1001).unwrap(), "data present, no tag → wiped to adopt the library");
    assert!(p.export_tree().unwrap().is_empty(), "tree wiped");
    assert_eq!(p.cursor().unwrap(), 0, "cursor reset");

    // Re-bind to the SAME library → no wipe.
    let _ = write_file(&p, ROOT, "b.bin", b"library 1001 file");
    p.set_cursor(7).unwrap();
    assert!(!p.bind_library(1001).unwrap(), "same library → kept as-is");
    assert_eq!(p.export_tree().unwrap().len(), 1, "tree preserved");
    assert_eq!(p.cursor().unwrap(), 7, "cursor preserved");

    // Bind to a DIFFERENT library → wipe.
    assert!(p.bind_library(2002).unwrap(), "different library → wiped");
    assert!(p.export_tree().unwrap().is_empty(), "stale tree gone");
    assert_eq!(p.cursor().unwrap(), 0, "cursor reset for the new library");
}

#[test]
fn random_mid_file_write_works_on_a_hydrated_file() {
    // Once the file's chunks are local (the adapter hydrates the whole file on
    // open-for-write), a seek-and-write into the middle reconstructs correctly — the
    // write path seeds the staging buffer from the committed content and overlays.
    let (a, b, _dir) = shared_store_pair();
    write_file(&a, ROOT, "part.bin", &vec![1u8; 2000]);
    replicate_tree(&a, &b);

    let b_ino = b.lookup(ROOT, "part.bin").unwrap().ino; // B shares A's chunks (hydrated)
    b.write(b_ino, 500, &[9u8; 10]).unwrap();
    b.flush(b_ino).unwrap();

    let out = read_all(&b, b_ino);
    assert_eq!(out.len(), 2000, "size preserved");
    assert_eq!(&out[500..510], &[9u8; 10], "the random write landed");
    assert_eq!(out[0], 1, "surrounding content intact");
}
