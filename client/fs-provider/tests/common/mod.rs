//! Shared helpers for the fs-provider integration suites.
//!
//! These drive the provider through its **public** API only (the same surface the mount
//! adapters and sync engine use), so the suites double as a contract spec for any future
//! `Provider`/`ChunkStore` implementation — the rclone-`fstests` idea: one suite, run
//! against the backend.

#![allow(dead_code, unused_imports)]

use fs_provider::{ChunkConfig, ChunkLoc, LocalChunkStore, Provider, SqliteProvider, SyncNode, ROOT_INO};

/// Tiny CDC parameters so even small test files split into many chunks — this is what
/// makes the dedup / block-delta properties observable at KB scale instead of MB.
pub const TINY: ChunkConfig = ChunkConfig { min: 64, avg: 256, max: 1024 };

/// An in-memory provider with the tiny chunk config (a fresh temp chunk store each time).
pub fn provider() -> SqliteProvider {
    SqliteProvider::in_memory(Box::new(LocalChunkStore::temp().unwrap()), TINY).unwrap()
}

/// An in-memory provider with explicit chunk parameters.
pub fn provider_with(cfg: ChunkConfig) -> SqliteProvider {
    SqliteProvider::in_memory(Box::new(LocalChunkStore::temp().unwrap()), cfg).unwrap()
}

/// Two providers that **share one on-disk chunk store** — the store stands in for the
/// cloud, so once A flushes, B can hydrate/read after applying A's exported tree. Returns
/// `(a, b, chunk_dir)`; keep `chunk_dir` alive for the test's duration.
pub fn shared_store_pair() -> (SqliteProvider, SqliteProvider, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let a = SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir.path()).unwrap()), TINY).unwrap();
    let b = SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir.path()).unwrap()), TINY).unwrap();
    (a, b, dir)
}

/// Create a file under `parent`, write `body`, flush. Returns its inode.
pub fn write_file(p: &SqliteProvider, parent: u64, name: &str, body: &[u8]) -> u64 {
    let f = p.create(parent, name).unwrap();
    if !body.is_empty() {
        p.write(f.ino, 0, body).unwrap();
    }
    p.flush(f.ino).unwrap();
    f.ino
}

/// The whole content of a file (a generous read clamped to EOF by the provider).
pub fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 {
        return Vec::new();
    }
    p.read(ino, 0, size as u32).unwrap()
}

/// The ordered chunk list for a file at `parent/name`, via `export_tree` (public API).
pub fn chunks_of(p: &SqliteProvider, name: &str) -> Vec<ChunkLoc> {
    node_named(p, name).chunks
}

/// The set of chunk hashes for a file (for dedup / block-delta overlap assertions).
pub fn chunk_hashes(p: &SqliteProvider, name: &str) -> std::collections::HashSet<String> {
    chunks_of(p, name).into_iter().map(|c| c.hash).collect()
}

/// The exported `SyncNode` for a top-level node by name (panics if absent).
pub fn node_named(p: &SqliteProvider, name: &str) -> SyncNode {
    p.export_tree().unwrap().into_iter().find(|n| n.name == name).unwrap_or_else(|| panic!("no node named {name}"))
}

/// Apply every node A exported into B (parents first — `export_tree` orders by ino).
pub fn replicate_tree(from: &SqliteProvider, to: &SqliteProvider) {
    for n in &from.export_tree().unwrap() {
        to.apply_remote(n).unwrap();
    }
}

/// Drain `source`'s outbox and apply each change to every replica — exactly what the
/// daemon's push+pull does, minus the wire (the ownCloud `FakeFolder` idea). Parents
/// precede children (outbox insertion order), so applies never miss a parent.
pub fn sync_to(source: &SqliteProvider, replicas: &[&SqliteProvider]) {
    for (node_id, op) in source.outbox_list().unwrap() {
        for r in replicas {
            if op == "delete" {
                r.apply_remote_delete(&node_id).unwrap();
            } else if let Some(n) = source.sync_node(&node_id).unwrap() {
                r.apply_remote(&n).unwrap();
            }
        }
        source.outbox_clear(&node_id).unwrap();
    }
}

/// A normalized, order-independent snapshot of a tree for cross-replica equality:
/// `(node_id, parent_node_id, name, is_dir, manifest_id)` sorted. Content equality is
/// implied by `manifest_id` (content-addressed), so this also asserts byte convergence.
pub fn tree_snapshot(p: &SqliteProvider) -> Vec<(String, Option<String>, String, bool, Option<String>)> {
    let mut v: Vec<_> = p
        .export_tree()
        .unwrap()
        .into_iter()
        .map(|n| (n.node_id, n.parent_node_id, n.name, n.kind == fs_provider::Kind::Dir, n.manifest_id))
        .collect();
    v.sort();
    v
}

pub use fs_provider::Kind;
pub const ROOT: u64 = ROOT_INO;

/// A self-deleting temp directory (no external dep; uses the OS temp dir + pid + a
/// process-unique counter, since the build box forbids `Date.now`-style entropy in some
/// contexts — a monotonic atomic is enough for uniqueness within a run).
pub mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("fsprov-test-{}-{}", std::process::id(), n));
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
