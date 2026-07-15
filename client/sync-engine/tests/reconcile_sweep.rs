//! Regression for BUG #4: `pull_snapshot` must SWEEP nodes the server no longer holds live.
//!
//! A device that full-resyncs (cursor below the pruned `oldest_seq`, or a race-dropped node)
//! applies the live snapshot and jumps the cursor to head — skipping the delete changelog
//! entries. Without a sweep, every file deleted while this device was away survives as a
//! **ghost** that can resurrect if edited. `SyncEngine::sweep_absent` (the factored-out core of
//! the snapshot reconcile) removes those ghosts — but must preserve never-pushed local nodes
//! (pending outbox), which are local intent rather than ghosts.

use std::collections::HashSet;

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_engine::SyncEngine;

const TINY: ChunkConfig = ChunkConfig {
    min: 64,
    avg: 256,
    max: 1024,
};

fn provider() -> (SqliteProvider, tempfile_dir::Dir) {
    let dir = tempfile_dir::Dir::new();
    let p =
        SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir.path()).unwrap()), TINY).unwrap();
    (p, dir)
}

fn write_file(p: &SqliteProvider, parent: u64, name: &str, body: &[u8]) -> u64 {
    let f = p.create(parent, name).unwrap();
    p.write(f.ino, 0, body).unwrap();
    p.flush(f.ino).unwrap();
    f.ino
}

#[test]
fn sweep_removes_ghosts_keeps_pending_and_prevents_resurrection() {
    let (p, _dir) = provider();

    // Three files. `keep.bin` is live on the server; `ghost.bin` was deleted on the server
    // while we were away (absent from the snapshot); `local.bin` is a fresh local create we
    // haven't pushed yet (pending outbox).
    let keep = write_file(&p, ROOT_INO, "keep.bin", b"live on server");
    let ghost = write_file(&p, ROOT_INO, "ghost.bin", b"deleted while away");
    let local = write_file(&p, ROOT_INO, "local.bin", b"never pushed");

    let keep_id = p.node_id_for(keep).unwrap().unwrap();
    let ghost_id = p.node_id_for(ghost).unwrap().unwrap();
    let local_id = p.node_id_for(local).unwrap().unwrap();

    // Simulate the push state: keep.bin + ghost.bin were pushed (drain their outbox); local.bin
    // stays pending.
    p.outbox_clear(&keep_id).unwrap();
    p.outbox_clear(&ghost_id).unwrap();
    assert!(
        p.outbox_list()
            .unwrap()
            .iter()
            .any(|(n, _)| n == &local_id),
        "local.bin remains pending (never pushed)"
    );

    // The server's live set from the snapshot: keep.bin is present; local.bin's node_id also
    // appears once its create is pushed, but here it hasn't been, so the snapshot lacks it —
    // exactly the case the pending-preserve guard covers. ghost.bin is absent (deleted).
    let mut live_ids = HashSet::new();
    live_ids.insert(keep_id.clone());

    SyncEngine::sweep_absent(&p, &live_ids).unwrap();

    // Ghost removed.
    assert!(
        p.lookup(ROOT_INO, "ghost.bin").is_err(),
        "a server-deleted ghost must be swept"
    );
    // Live node kept.
    assert!(p.lookup(ROOT_INO, "keep.bin").is_ok(), "a live node is kept");
    // Never-pushed local node preserved (NOT swept just because the snapshot lacks it).
    assert!(
        p.lookup(ROOT_INO, "local.bin").is_ok(),
        "a pending (never-pushed) local node must NOT be swept"
    );

    // Resurrection guard: the swept-then-untouched ghost must not come back. It has no outbox
    // entry (the delete was inbound), so a push drain would send nothing for it.
    assert!(
        !p.outbox_list()
            .unwrap()
            .iter()
            .any(|(n, _)| n == &ghost_id),
        "the swept ghost must not be re-enqueued (no resurrection)"
    );
}

/// A self-deleting temp dir (no external dep).
mod tempfile_dir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);
    pub struct Dir(PathBuf);
    impl Dir {
        pub fn new() -> Self {
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("synceng-sweep-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
