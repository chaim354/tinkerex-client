//! Regression for BUG #2 in the rust-sync-core batch: `outbox_clear` after a WAN push must
//! not delete a NEWER re-enqueued edit. The sync-engine drain reads an outbox entry, spends
//! seconds in WAN I/O, then clears it — but a mount write during that window re-enqueues the
//! same node (bumping `enqueued_at`). An unconditional `DELETE ... WHERE node_id = ?` throws
//! that newer dirty mark away → the edit never syncs. The conditional clear
//! (`outbox_clear_upto(node, captured_enqueued_at)`) only deletes the row the drain actually
//! pushed, so the re-enqueued edit survives.

mod common;
use common::*;

use fs_provider::Provider;

/// Read the `enqueued_at` timestamp for a node's current outbox row.
fn enqueued_at(p: &fs_provider::SqliteProvider, node_id: &str) -> Option<i64> {
    p.outbox_list_timed()
        .unwrap()
        .into_iter()
        .find(|(n, _, _, _)| n == node_id)
        .map(|(_, _, enq, _)| enq)
}

#[test]
fn conditional_clear_preserves_a_reenqueued_edit() {
    let p = provider();

    // First edit → outbox entry #1 with enqueued_at = t1.
    let f = p.create(ROOT, "part.step").unwrap();
    p.write(f.ino, 0, b"v1").unwrap();
    p.flush(f.ino).unwrap();
    let node_id = p.node_id_for(f.ino).unwrap().unwrap();
    let t1 = enqueued_at(&p, &node_id).expect("entry queued after first flush");

    // The engine snapshots (node_id, t1) and heads off to do WAN I/O. Meanwhile a mount write
    // lands and re-enqueues the SAME node — enqueued_at advances to t2 > t1. Sleep to guarantee
    // a distinct millisecond stamp.
    std::thread::sleep(std::time::Duration::from_millis(3));
    p.write(f.ino, 0, b"v2-newer").unwrap();
    p.flush(f.ino).unwrap();
    let t2 = enqueued_at(&p, &node_id).expect("still queued after re-enqueue");
    assert!(t2 > t1, "re-enqueue must bump enqueued_at ({t2} > {t1})");

    // The drain now clears the row it pushed — but conditionally, only up to the timestamp it
    // captured (t1). Because the row was re-enqueued to t2 > t1, the clear must be a NO-OP.
    p.outbox_clear_upto(&node_id, t1).unwrap();
    assert_eq!(
        enqueued_at(&p, &node_id),
        Some(t2),
        "the newer re-enqueued edit must survive a clear keyed to the older drained timestamp"
    );

    // A clear keyed to the current (t2) stamp — i.e. a drain that actually pushed the latest —
    // does remove it.
    p.outbox_clear_upto(&node_id, t2).unwrap();
    assert_eq!(
        enqueued_at(&p, &node_id),
        None,
        "clearing up to the current timestamp drains the row"
    );
}
