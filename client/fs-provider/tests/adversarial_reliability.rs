//! Adversarial RED-tests probing reliability gaps in `fs-provider` not covered by the
//! existing hermetic suites (chunking/convergence/concurrency/caching/range_hydration).
//!
//! The discipline (per the overnight brief): each test is written to *try to break* a
//! correctness claim. A test that stays green confirms robustness; a test that exposes a
//! real defect is either fixed (and flipped green) or left `#[ignore]`'d with a precise
//! repro + expected-vs-actual comment. We never weaken an assertion to force green.
//!
//! Target areas here (provider level, all hermetic, all run on macOS):
//!   - partial hydration / `read` range assembly under missing middle chunks
//!   - large logical offsets (u64 / i64 sparse boundaries)
//!   - torn writes: a dropped `flush` (crash mid-commit) must not corrupt committed state
//!   - concurrent writers racing the SAME file
//!   - rename/move corner cases (rename onto self via different name path, cycle attempts)
//!   - `set_mtime` / metadata-only mutations and the outbox

mod common;
use common::*;

use std::sync::Arc;
use std::thread;

use fs_provider::{ChunkConfig, FsError, LocalChunkStore, Provider, SqliteProvider};

// =======================================================================================
// AREA 1: partial hydration — `read` range assembly when only SOME chunks are local.
//
// `assemble_range` is the progressive-hydration hot path: a range read must touch only the
// chunks it overlaps, returning `ChunkMissing(hash)` for an overlapping chunk that is not
// cached so the mount can hydrate exactly that range. We probe the boundaries:
//   (a) a read fully inside a present chunk succeeds even when LATER chunks are absent;
//   (b) a read overlapping a missing chunk reports ChunkMissing (not silent zero-fill);
//   (c) a read starting exactly at a chunk boundary.
// =======================================================================================

/// Build a committed file on `src`, then create a `replica` with its OWN empty chunk store
/// holding only the manifest (a placeholder). Returns (replica, node_id, all_hashes, body).
fn placeholder_replica(body: &[u8]) -> (SqliteProvider, tempdir::Dir, String, Vec<String>) {
    let src = provider();
    write_file(&src, ROOT, "big.bin", body);
    let node = node_named(&src, "big.bin");
    let hashes: Vec<String> = node.chunks.iter().map(|c| c.hash.clone()).collect();

    let dir = tempdir::Dir::new();
    let replica = SqliteProvider::in_memory(
        Box::new(LocalChunkStore::new(dir.path()).unwrap()),
        ChunkConfig { min: 64, avg: 256, max: 1024 },
    )
    .unwrap();
    replica.apply_remote(&node).unwrap();
    let node_id = node.node_id.clone();
    // Copy each chunk's bytes from src so the test can selectively seed them into replica.
    (replica, dir, node_id, hashes)
}

#[test]
fn range_read_of_present_prefix_succeeds_while_tail_chunks_absent() {
    // A header read (first N bytes) must succeed once just the first chunk(s) are cached,
    // EVEN IF the rest of the file is not hydrated. This is the whole point of progressive
    // hydration. We hydrate ONLY the first chunk and read inside it.
    let body: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let src = provider();
    write_file(&src, ROOT, "big.bin", &body);
    let node = node_named(&src, "big.bin");
    let first_hash = node.chunks[0].hash.clone();
    let first_len = node.chunks[0].length; // bytes covered by chunk 0
    let first_bytes = src.chunk_get(&first_hash).unwrap();

    let (replica, _d, _nid, _h) = placeholder_replica(&body);
    let ino = replica.lookup(ROOT, "big.bin").unwrap().ino;
    // Seed only chunk 0 into the replica's store.
    replica.chunk_put(&first_hash, &first_bytes).unwrap();

    // Read strictly inside chunk 0 — must succeed and match.
    let want = &body[0..(first_len as usize).min(body.len()).min(100)];
    let got = replica
        .read(ino, 0, want.len() as u32)
        .expect("range read inside the one hydrated chunk must succeed");
    assert_eq!(got, want, "prefix bytes match the source");
}

#[test]
fn range_read_overlapping_missing_chunk_reports_chunk_missing_not_zerofill() {
    // A read that overlaps a NOT-yet-hydrated chunk must surface `ChunkMissing` so the mount
    // hydrates it — it must NOT silently return zero-filled or truncated bytes (silent data
    // corruption). We hydrate chunk 0 only, then read a range that reaches into chunk 1+.
    let body: Vec<u8> = (0..6000u32).map(|i| ((i * 7) % 251) as u8).collect();
    let src = provider();
    write_file(&src, ROOT, "big.bin", &body);
    let node = node_named(&src, "big.bin");
    assert!(node.chunks.len() >= 3, "need a multi-chunk file for this probe");
    let first_hash = node.chunks[0].hash.clone();
    let first_bytes = src.chunk_get(&first_hash).unwrap();

    let (replica, _d, _nid, _h) = placeholder_replica(&body);
    let ino = replica.lookup(ROOT, "big.bin").unwrap().ino;
    replica.chunk_put(&first_hash, &first_bytes).unwrap();

    // Read the whole file: it overlaps the still-missing chunk 1, so it must error
    // (ChunkMissing), never return a short/zeroed buffer pretending to be the content.
    let r = replica.read(ino, 0, body.len() as u32);
    match r {
        Err(FsError::ChunkMissing(_)) => { /* correct */ }
        Err(other) => panic!("expected ChunkMissing, got {other:?}"),
        Ok(bytes) => panic!(
            "BUG: read returned {} bytes instead of ChunkMissing — silent corruption \
             (a partially-hydrated file must not serve fabricated bytes)",
            bytes.len()
        ),
    }
}

// =======================================================================================
// AREA 2: large logical offsets — sparse writes and reads near u64/i64 boundaries.
//
// The manifest stores chunk offset/length as i64. A sparse write at a very large offset, or
// a read at a huge offset, must not panic, wrap, or corrupt. We avoid OOM by keeping the
// actual written region tiny but the OFFSET large.
// =======================================================================================

#[test]
fn read_far_past_eof_returns_empty_not_panic() {
    // A read whose offset is well past EOF must return an empty slice (clamp), never panic
    // or wrap. Kernels issue such reads at the tail of a file.
    let p = provider();
    let ino = write_file(&p, ROOT, "f.bin", &vec![9u8; 500]);
    // Offset past EOF, and an offset near u64::MAX (overflow bait for `offset + size`).
    for off in [500u64, 10_000, u64::MAX - 10, u64::MAX] {
        let got = p.read(ino, off, 4096).expect("read past EOF must not error/panic");
        assert!(got.is_empty(), "read at offset {off} past EOF returns empty, got {} bytes", got.len());
    }
}

#[test]
fn read_size_overflow_offset_plus_size_does_not_wrap() {
    // offset + size as u64 can wrap if computed naively. Read near the top of the range with
    // a large size; must clamp to EOF (empty here), never panic.
    let p = provider();
    let ino = write_file(&p, ROOT, "f.bin", &vec![1u8; 1000]);
    let got = p.read(ino, u64::MAX - 5, u32::MAX).expect("must not panic on offset+size overflow");
    assert!(got.is_empty());
}

#[test]
fn sparse_write_at_large_offset_within_buffer_is_consistent() {
    // A write at a moderately large offset (1 MiB) into a fresh file zero-fills the hole and
    // the manifest round-trips byte-for-byte. (We keep it to 1 MiB to avoid OOM; the point is
    // hole zero-fill + offset arithmetic, not raw scale.)
    let p = provider();
    let f = p.create(ROOT, "sparse.bin").unwrap();
    let tail = b"END-MARKER";
    let off = 1usize << 20; // 1 MiB
    p.write(f.ino, off as u64, tail).unwrap();
    p.flush(f.ino).unwrap();

    let size = p.getattr(f.ino).unwrap().size;
    assert_eq!(size, (off + tail.len()) as u64, "size spans the hole");
    // The hole reads as zeros, the tail reads as written.
    let whole = read_all(&p, f.ino);
    assert_eq!(whole.len(), off + tail.len());
    assert!(whole[..off].iter().all(|&b| b == 0), "hole is zero-filled");
    assert_eq!(&whole[off..], tail, "tail marker intact");
}

// =======================================================================================
// AREA 3: torn writes — a crash mid-commit (dropped `flush`) must not corrupt the last
// committed state. The staging buffer holds uncommitted bytes; if the process dies before
// `flush`, the file must read back as its LAST committed content, not a half-written mix.
// =======================================================================================

#[test]
fn uncommitted_write_does_not_corrupt_last_committed_content_after_reopen() {
    // Commit v1, start writing v2 into staging, then "crash" (drop without flush) by
    // reopening the on-disk DB+store. The reopened provider must show v1 intact — the
    // uncommitted staging buffer never touched durable state.
    let dir = tempdir::Dir::new();
    let db = dir.path().join("meta.db");
    let chunks = dir.path().join("chunks");
    std::fs::create_dir_all(&chunks).unwrap();

    let v1 = b"committed-version-one".to_vec();
    let ino;
    {
        let p = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
        let f = p.create(ROOT, "doc.txt").unwrap();
        p.write(f.ino, 0, &v1).unwrap();
        p.flush(f.ino).unwrap(); // v1 durable
        ino = f.ino;
        // Begin v2 but DO NOT flush (simulated crash: just drop p with staging dirty).
        p.write(ino, 0, b"HALF-WRITTEN-version-two-never-flushed").unwrap();
    } // p dropped — staging lost, no flush

    // Reopen: the durable state must be exactly v1.
    let p2 = SqliteProvider::open(&db, Box::new(LocalChunkStore::new(&chunks).unwrap())).unwrap();
    let f2 = p2.lookup(ROOT, "doc.txt").unwrap();
    assert_eq!(read_all(&p2, f2.ino), v1, "reopened file shows last committed (v1), not torn v2");
}

#[test]
fn flush_is_all_or_nothing_no_partial_manifest_visible() {
    // After a successful flush the node's manifest + size must be fully consistent: the size
    // equals the assembled byte count and every manifest chunk is present in the store. (A
    // torn flush that committed the size row but not all chunk rows would show here.)
    let p = provider();
    let body: Vec<u8> = (0..9999u32).map(|i| (i % 256) as u8).collect();
    let ino = write_file(&p, ROOT, "x.bin", &body);
    let attr = p.getattr(ino).unwrap();
    assert_eq!(attr.size, body.len() as u64);
    // Re-read must equal body exactly (assembled from committed chunks).
    assert_eq!(read_all(&p, ino), body, "committed content assembles back identically");
}

// =======================================================================================
// AREA 4: concurrent writers racing the SAME file (the existing concurrency suite only
// races DISTINCT files). Two threads opening + writing + flushing the same inode must not
// panic, deadlock, or leave a corrupt manifest — the final content must be ONE of the
// writers' coherent versions (last flush wins), never a byte-level interleave.
// =======================================================================================

#[test]
fn concurrent_writers_to_same_file_leave_a_coherent_version() {
    let p = Arc::new(provider());
    let f = p.create(ROOT, "race.bin").unwrap();
    p.flush(f.ino).unwrap();
    let ino = f.ino;

    // Two distinct, internally-coherent candidate bodies.
    let body_a: Vec<u8> = std::iter::repeat(0xAA).take(5000).collect();
    let body_b: Vec<u8> = std::iter::repeat(0xBB).take(7000).collect();

    let mut handles = Vec::new();
    for body in [body_a.clone(), body_b.clone()] {
        let p = Arc::clone(&p);
        handles.push(thread::spawn(move || {
            for _ in 0..25 {
                p.truncate(ino, 0).unwrap();
                p.write(ino, 0, &body).unwrap();
                p.flush(ino).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // The committed content must be a coherent whole — all 0xAA or all 0xBB, of the matching
    // length — NOT a frankenstein mix (which would prove the staging buffer / flush is not
    // serialized per inode).
    let got = read_all(&p, ino);
    let coherent = got == body_a || got == body_b;
    assert!(
        coherent,
        "BUG: concurrent same-file writers produced an incoherent mix (len={}, first={:#x}, last={:#x})",
        got.len(),
        got.first().copied().unwrap_or(0),
        got.last().copied().unwrap_or(0)
    );
}

// =======================================================================================
// AREA 5: rename / move corner cases beyond the convergence harness.
// =======================================================================================

// BUG FOUND + FIXED: `rename` of a directory into its own descendant created a detached
// CYCLE (a→b→a unreachable from ROOT), losing the subtree and looping any parent-walk. The
// provider returned Ok(()) with no descendant check. FIX: `SqliteProvider::rename` now walks
// the destination parent's ancestor chain and rejects a directory move-into-self/descendant
// with `FsError::InvalidArgument` (→ EINVAL, the POSIX errno), leaving the tree untouched.
// This test pins the fix: the rename is rejected AND the tree stays acyclic + reachable.
#[test]
fn rename_dir_into_its_own_descendant_is_rejected_with_einval() {
    let p = provider();
    let a = p.mkdir(ROOT, "a").unwrap();
    let b = p.mkdir(a.ino, "b").unwrap();
    let c = p.mkdir(b.ino, "c").unwrap();

    // Move `a` under its child `b` (one level): must be EINVAL.
    assert!(
        matches!(p.rename(ROOT, "a", b.ino, "a"), Err(FsError::InvalidArgument)),
        "moving a dir into its direct child must be rejected (EINVAL)"
    );
    // Move `a` under its grandchild `c` (deeper descendant): also EINVAL.
    assert!(
        matches!(p.rename(ROOT, "a", c.ino, "a"), Err(FsError::InvalidArgument)),
        "moving a dir into a deeper descendant must be rejected (EINVAL)"
    );
    // Moving a dir onto a path that is NOT a descendant is still allowed.
    let sib = p.mkdir(ROOT, "sib").unwrap();
    p.rename(ROOT, "a", sib.ino, "a").expect("move into a non-descendant dir is fine");

    // The tree must remain a forest rooted at ROOT with no cycle.
    let nodes = p.export_tree().unwrap();
    let by_id: std::collections::HashMap<String, Option<String>> =
        nodes.iter().map(|n| (n.node_id.clone(), n.parent_node_id.clone())).collect();
    for n in &nodes {
        let mut cur = n.parent_node_id.clone();
        let mut hops = 0;
        while let Some(pid) = cur {
            hops += 1;
            assert!(hops < 1000, "BUG: cycle detected reaching root from node {}", n.node_id);
            cur = by_id.get(&pid).cloned().flatten();
        }
    }
}

// =======================================================================================
// AREA 5b: cycle on the PULL path — `apply_remote` re-parenting under a descendant.
//
// The changelog is applied entry-by-entry; a delta can momentarily present "X's parent is
// now Y" and "Y's parent is now X". Unlike local `rename`, `apply_remote` is authoritative,
// but it must never leave the LOCAL tree in a cyclic state where a subtree is unreachable
// from ROOT (that orphans data + loops any tree walk). We probe whether applying a cyclic
// pair leaves a detached cycle.
// =======================================================================================

/// Walk every node's parent chain; panic if any node cannot reach ROOT within `bound` hops
/// (i.e. it's in a cycle or orphaned). Returns the set of node_ids reachable from ROOT.
fn assert_acyclic_reachable(p: &SqliteProvider, ctx: &str) {
    let nodes = p.export_tree().unwrap();
    let by_id: std::collections::HashMap<String, Option<String>> =
        nodes.iter().map(|n| (n.node_id.clone(), n.parent_node_id.clone())).collect();
    for n in &nodes {
        let mut cur = n.parent_node_id.clone();
        let mut hops = 0;
        while let Some(pid) = cur {
            hops += 1;
            assert!(
                hops < 10_000,
                "{ctx}: BUG — node {} is in a cycle / cannot reach ROOT (orphaned subtree)",
                n.node_id
            );
            cur = by_id.get(&pid).cloned().flatten();
        }
    }
}

#[test]
#[ignore = "RED (documented gap): apply_remote does not guard against re-parenting a dir \
            under its own descendant, so a cyclic changelog interleaving leaves a detached \
            cycle in the LOCAL tree (subtree unreachable from ROOT). See comment for repro. \
            Lower severity than the local-rename cycle: the authoritative server tree is \
            acyclic, so this only manifests on a transient/buggy delta ordering and the \
            reconcile floor (pull_snapshot) would eventually re-apply a consistent snapshot. \
            Left documented rather than fixed tonight: the right fix (cycle-detect + defer in \
            pull_changes_paged, mirroring BUG #2's defer-and-retry) belongs in sync-engine, \
            not a blunt reject in apply_remote (which must stay able to apply any single \
            authoritative snapshot row)."]
fn apply_remote_cyclic_reparent_must_not_orphan_subtree() {
    // Build X and Y as two sibling dirs under ROOT on a source, export their nodes.
    let src = provider();
    let x = src.mkdir(ROOT, "X").unwrap();
    let y = src.mkdir(ROOT, "Y").unwrap();
    let x_node = src.export_tree().unwrap().into_iter().find(|n| n.name == "X").unwrap();
    let y_node = src.export_tree().unwrap().into_iter().find(|n| n.name == "Y").unwrap();
    let _ = (x, y);

    // Replica applies both as-is (siblings under ROOT) — fine.
    let replica = provider();
    replica.apply_remote(&x_node).unwrap();
    replica.apply_remote(&y_node).unwrap();
    assert_acyclic_reachable(&replica, "after sibling apply");

    // Now craft a cyclic delta: X.parent := Y, then Y.parent := X.
    let mut x_under_y = x_node.clone();
    x_under_y.parent_node_id = Some(y_node.node_id.clone());
    let mut y_under_x = y_node.clone();
    y_under_x.parent_node_id = Some(x_node.node_id.clone());

    // Apply both (authoritative). EXPECTED: the local tree must not contain a detached cycle.
    // ACTUAL (the gap): apply_remote happily sets X.parent=Y and Y.parent=X → both unreachable
    // from ROOT, a 2-node cycle → assert_acyclic_reachable panics.
    let _ = replica.apply_remote(&x_under_y);
    let _ = replica.apply_remote(&y_under_x);
    assert_acyclic_reachable(&replica, "after cyclic reparent delta");
}

// =======================================================================================
// AREA 5c: delete-vs-edit race on the PULL path. A device has a pending LOCAL edit
// (un-pushed outbox upsert) on node X; a remote DELETE of X arrives via the changelog.
// `apply_remote_delete` removes X locally and (by design) does NOT signal for the target
// node itself — only for swept descendants. We probe what happens to the pending edit: does
// the device silently drop it, or surface a signal? This documents the actual semantics so a
// regression (or a future product decision to conflict instead) is visible.
// =======================================================================================

#[test]
fn remote_delete_of_a_locally_edited_node_drops_the_pending_edit_silently() {
    // This is a DOCUMENTING test (not a bug claim): it pins the current "remote delete wins,
    // no signal for the target node" behavior so any future change is deliberate. The brief
    // calls out delete-vs-edit races; the gap here is that a user's un-pushed edit to a file
    // another device deleted is discarded locally with NO conflict/signal — defensible, but
    // worth making explicit + visible in the suite.
    let p = provider();
    let f = p.create(ROOT, "doc.txt").unwrap();
    p.write(f.ino, 0, b"v1").unwrap();
    p.flush(f.ino).unwrap();
    let node_id = p.node_id_for(f.ino).unwrap().unwrap();
    // Drain the create from the outbox (it's "synced").
    for (nid, _) in p.outbox_list().unwrap() {
        p.outbox_clear(&nid).unwrap();
    }

    // Local pending EDIT (un-pushed): re-write the file → outbox upsert queued.
    p.truncate(f.ino, 0).unwrap();
    p.write(f.ino, 0, b"my important local edit").unwrap();
    p.flush(f.ino).unwrap();
    assert!(
        p.outbox_list().unwrap().iter().any(|(nid, op)| nid == &node_id && op == "upsert"),
        "the local edit is queued as a pending upsert"
    );

    // A remote DELETE for the SAME node arrives via pull.
    let signaled = p.apply_remote_delete_signaled(&node_id).unwrap();

    // Document the observed semantics:
    //  - the node is gone locally,
    //  - no reconcile signal is raised for the target node (design: delete authorizes it),
    //  - the queued outbox upsert now references a vanished node → it will push as a no-op,
    //    so the local edit is silently discarded.
    assert!(p.lookup(ROOT, "doc.txt").is_err(), "remote delete removed the node locally");
    assert!(!signaled, "no reconcile signal for the target node itself (current design)");
    // The pending upsert's node is gone, so a drain finds nothing to push.
    assert!(
        p.sync_node(&node_id).unwrap().is_none(),
        "DOCUMENTED GAP: the locally-edited node vanished; its pending edit will not be pushed \
         (remote delete wins with no conflict surfaced to the user)"
    );
}

// =======================================================================================
// AREA 6: metadata-only mutation (`set_mtime`) and the outbox.
// =======================================================================================

#[test]
fn set_mtime_persists_and_enqueues_without_changing_content() {
    let p = provider();
    let body = b"content stays put".to_vec();
    let ino = write_file(&p, ROOT, "f.bin", &body);
    // Drain the create from the outbox.
    for (nid, _) in p.outbox_list().unwrap() {
        p.outbox_clear(&nid).unwrap();
    }
    let mid_before = node_named(&p, "f.bin").manifest_id.clone();

    p.set_mtime(ino, 1_700_000_000_000_000_000).unwrap();
    assert_eq!(p.getattr(ino).unwrap().mtime, 1_700_000_000_000_000_000, "mtime persisted in nanos");
    assert_eq!(read_all(&p, ino), body, "content unchanged by set_mtime");
    assert_eq!(node_named(&p, "f.bin").manifest_id, mid_before, "manifest (content addr) unchanged");
    // The metadata change is queued for sync.
    assert!(!p.outbox_list().unwrap().is_empty(), "set_mtime enqueues an upsert for propagation");
}
