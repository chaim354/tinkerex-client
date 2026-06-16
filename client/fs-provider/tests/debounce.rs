//! G5 — change-batching / outbox debounce.
//!
//! A flurry of rapid saves to one file must coalesce into a SINGLE pending push of the
//! *final* content (not N pushes of intermediate states), while never stalling sync
//! indefinitely: a node held past the max-age cap flushes, deletes are never held, and a
//! window of 0 disables the debounce entirely.
//!
//! These tests exercise `SqliteProvider::outbox_ready_topo(window_ms, max_age_ms)` — the
//! drain-order gate the sync engine's `push_outbox_debounced` calls each tick. They use
//! short real-time windows + small sleeps so they stay hermetic (no server, no clock
//! injection) yet deterministic.

mod common;
use common::*;
use fs_provider::{Provider, ROOT_INO};

use std::thread::sleep;
use std::time::Duration;

/// Helper: the node_id of an inode (the stable id the outbox keys on).
fn node_id(p: &fs_provider::SqliteProvider, ino: u64) -> String {
    p.node_id_for(ino).unwrap().expect("node has a node_id")
}

#[test]
fn rapid_saves_coalesce_to_one_pending_push_of_final_bytes() {
    let p = provider();
    let f = p.create(ROOT_INO, "model.f3d").unwrap();

    // Five rapid "saves" of the same file in quick succession (the big-CAD-autosave burst).
    for i in 0..5u8 {
        let body = vec![i; 4096]; // each save overwrites with new content
        p.truncate(f.ino, 0).unwrap();
        p.write(f.ino, 0, &body).unwrap();
        p.flush(f.ino).unwrap(); // each flush enqueues an upsert (re-touches the outbox row)
    }
    let id = node_id(&p, f.ino);

    // The outbox already coalesces same-node edits to ONE row (the design's free win):
    // five flushes → a single (node_id, upsert) entry.
    let raw = p.outbox_list().unwrap();
    assert_eq!(
        raw,
        vec![(id.clone(), "upsert".to_string())],
        "five saves → one outbox row"
    );

    // With a debounce window WIDER than the burst, the node is still "hot" (just touched),
    // so the ready set is EMPTY — we hold it, coalescing with any further save.
    let ready = p.outbox_ready_topo(10_000, 60_000).unwrap();
    assert!(
        ready.is_empty(),
        "a freshly-saved node is held inside the debounce window"
    );

    // Once the node has been quiet longer than the window, it becomes ready and will push
    // exactly once — carrying the FINAL bytes (the manifest of the last write).
    sleep(Duration::from_millis(120));
    let ready = p.outbox_ready_topo(100, 60_000).unwrap();
    assert_eq!(
        ready,
        vec![(id.clone(), "upsert".to_string())],
        "quiesced node flushes once"
    );

    // And the content it will push is the LAST save (value 4), proving no intermediate
    // state was committed and the final state is not lost.
    let node = p.sync_node(&id).unwrap().unwrap();
    let bytes = read_all(&p, f.ino);
    assert_eq!(bytes, vec![4u8; 4096], "final content preserved");
    assert!(node.manifest_id.is_some());
}

#[test]
fn max_age_cap_flushes_a_continuously_resaved_file() {
    let p = provider();
    let f = p.create(ROOT_INO, "spinner.scad").unwrap();
    p.write(f.ino, 0, b"v0").unwrap();
    p.flush(f.ino).unwrap();
    let id = node_id(&p, f.ino);

    // Keep re-saving so the node never goes quiet for the window. With a max-age cap of 0
    // (already past the cap), the node must flush DESPITE being hot — sync can't stall.
    sleep(Duration::from_millis(30));
    for _ in 0..3 {
        p.write(f.ino, 0, b"vN").unwrap();
        p.flush(f.ino).unwrap(); // re-touches enqueued_at (still hot)
    }
    // window huge (always hot) but max_age 0 → the cap fires, node is ready.
    let ready = p.outbox_ready_topo(10_000, 0).unwrap();
    assert_eq!(
        ready,
        vec![(id, "upsert".to_string())],
        "max-age cap flushes a hot node"
    );
}

#[test]
fn deletes_are_never_debounced() {
    let p = provider();
    let f = p.create(ROOT_INO, "scrap.stl").unwrap();
    p.write(f.ino, 0, b"junk").unwrap();
    p.flush(f.ino).unwrap();
    let id = node_id(&p, f.ino);
    p.unlink(ROOT_INO, "scrap.stl").unwrap(); // enqueues a delete (just now)

    // Even with a very wide window, the delete is ready immediately (low-volume,
    // user-visible — push promptly).
    let ready = p.outbox_ready_topo(60_000, 600_000).unwrap();
    assert_eq!(
        ready,
        vec![(id, "delete".to_string())],
        "deletes bypass the debounce"
    );
}

#[test]
fn window_zero_disables_debounce() {
    let p = provider();
    let f = p.create(ROOT_INO, "a.txt").unwrap();
    p.write(f.ino, 0, b"hi").unwrap();
    p.flush(f.ino).unwrap();
    let id = node_id(&p, f.ino);

    // window 0 == the eager pre-G5 drain: ready immediately even though just saved.
    let ready = p.outbox_ready_topo(0, 0).unwrap();
    assert_eq!(ready, vec![(id, "upsert".to_string())]);
    // And it's identical to the un-debounced topo order.
    assert_eq!(ready, p.outbox_list_topo().unwrap());
}

#[test]
fn held_parent_holds_its_queued_child() {
    // A held (still-debouncing) directory must also hold a queued child so the parents-first
    // drain invariant isn't violated (a child never drains ahead of its still-hot parent).
    let p = provider();
    let d = p.mkdir(ROOT_INO, "dir").unwrap();
    let f = p.create(d.ino, "child.txt").unwrap();
    p.write(f.ino, 0, b"x").unwrap();
    p.flush(f.ino).unwrap();
    // Both dir + child are freshly queued (hot). A wide window holds the parent → child too.
    let ready = p.outbox_ready_topo(10_000, 600_000).unwrap();
    assert!(ready.is_empty(), "a hot parent holds its queued child");
}
