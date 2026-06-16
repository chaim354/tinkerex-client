//! Make-available-offline + offline-readiness primitives — **hermetic** (no server).
//!
//! The byte machinery (selective sync + parallel hydration) already exists; what was
//! missing was a way to (a) deliberately pre-download a chosen folder for an offline
//! workshop and (b) *verify* it's fully local before disconnecting. These primitives close
//! that gap:
//!
//!   * [`SyncEngine::ensure_available_offline`] — fully hydrate a node and, if it's a
//!     directory, its entire subtree (every descendant file's chunks). Returns a summary.
//!   * [`SyncEngine::offline_readiness`] — without network, report whether a node/subtree
//!     is fully local (total vs present chunks + the still-online-only files).
//!
//! These run with a `SharedDirIO` "cloud" (content-addressed, `needs_urls() == false`), so
//! the `ServerClient` is constructed but never actually called — proving the logic without
//! a live server. Server-backed end-to-end coverage (pin survives a pull) lives in
//! `tests/integration.rs`.

use std::collections::HashMap;

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{ChunkIO, SharedDirIO, SyncEngine};

const TINY: ChunkConfig = ChunkConfig {
    min: 64,
    avg: 256,
    max: 1024,
};

fn uniq() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    )
}

fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("oa-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 {
        Vec::new()
    } else {
        p.read(ino, 0, size as u32).unwrap()
    }
}

/// Distinct incompressible bytes so FastCDC yields MANY chunks per file.
fn make_body(n: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; n];
    let mut state = 0x9E3779B97F4A7C15u64 ^ seed;
    for b in v.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = (state & 0xff) as u8;
    }
    v
}

/// A `ServerClient` that is constructed but never reached (the `SharedDirIO` cloud ignores
/// URLs, so hydrate skips the presign round-trip). Points at an unroutable address so any
/// accidental network call would fail loudly rather than silently pass.
fn offline_client() -> ServerClient {
    ServerClient::new("http://127.0.0.1:1") // port 1 — never bound; never actually called
}

/// Build a "fresh device" whose local tree mirrors a multi-file subtree but whose chunk
/// cache is EMPTY (the just-pulled-placeholder state), plus a `cloud` (`SharedDirIO`)
/// holding all the real bytes. Returns `(device, cloud, layout)` where `layout` maps a
/// logical label → (node_id, ino, original bytes) on the fresh device.
struct Layout {
    /// node_id of the `Jobs` directory (the subtree we'll take offline).
    jobs_dir: String,
    /// The files under Jobs/, by label.
    files: HashMap<&'static str, (String, u64, Vec<u8>)>,
    /// A file OUTSIDE Jobs/ (under Archive/) — must NOT be touched by a Jobs-scoped op.
    archive_file: (String, u64, Vec<u8>),
}

fn fresh_device_with_subtree() -> (SqliteProvider, SharedDirIO, Layout) {
    // Source device authors the tree + pushes every chunk into the shared cloud.
    let src = dev();
    let jobs = src.mkdir(ROOT_INO, "Jobs").unwrap();
    let clientx = src.mkdir(jobs.ino, "ClientX").unwrap();
    let sub = src.mkdir(clientx.ino, "sub").unwrap();

    let bodies = [
        ("top", jobs.ino, "a.stl", make_body(8_000, 1)),
        ("part", clientx.ino, "part.step", make_body(12_000, 2)),
        ("deep", sub.ino, "deep.lbrn2", make_body(6_000, 3)),
    ];
    for (_, parent, name, body) in &bodies {
        let f = src.create(*parent, name).unwrap();
        src.write(f.ino, 0, body).unwrap();
        src.flush(f.ino).unwrap();
    }
    // An out-of-subtree file under Archive/.
    let archive = src.mkdir(ROOT_INO, "Archive").unwrap();
    let archive_body = make_body(5_000, 9);
    let af = src.create(archive.ino, "old.stl").unwrap();
    src.write(af.ino, 0, &archive_body).unwrap();
    src.flush(af.ino).unwrap();

    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("oa-cloud-{}", uniq()))).unwrap();
    for n in src.export_tree().unwrap() {
        for h in src.node_manifest_hashes(&n.node_id).unwrap() {
            cloud
                .put(&h, &HashMap::new(), src.chunk_get(&h).unwrap())
                .unwrap();
        }
    }

    // Fresh device: same tree (export → apply), EMPTY chunk cache.
    let dst = dev();
    for n in src.export_tree().unwrap() {
        dst.apply_remote(&n).unwrap();
    }

    let d_jobs = dst.lookup(ROOT_INO, "Jobs").unwrap();
    let d_clientx = dst.lookup(d_jobs.ino, "ClientX").unwrap();
    let d_sub = dst.lookup(d_clientx.ino, "sub").unwrap();
    let mut files = HashMap::new();
    for (label, parent_ino, name, body) in [
        ("top", d_jobs.ino, "a.stl", bodies[0].3.clone()),
        ("part", d_clientx.ino, "part.step", bodies[1].3.clone()),
        ("deep", d_sub.ino, "deep.lbrn2", bodies[2].3.clone()),
    ] {
        let ent = dst.lookup(parent_ino, name).unwrap();
        let id = dst.node_id_for(ent.ino).unwrap().unwrap();
        files.insert(label, (id, ent.ino, body));
    }
    let d_archive = dst.lookup(ROOT_INO, "Archive").unwrap();
    let d_af = dst.lookup(d_archive.ino, "old.stl").unwrap();
    let archive_file = (
        dst.node_id_for(d_af.ino).unwrap().unwrap(),
        d_af.ino,
        archive_body,
    );

    let layout = Layout {
        jobs_dir: dst.node_id_for(d_jobs.ino).unwrap().unwrap(),
        files,
        archive_file,
    };
    (dst, cloud, layout)
}

// ---- offline_readiness (NO network) -------------------------------------------------

#[test]
fn offline_readiness_reports_partial_before_and_full_after() {
    let (b, cloud, layout) = fresh_device_with_subtree();

    // BEFORE any hydration: nothing local. The readiness report must say "not ready",
    // 0 present chunks, and list ALL three Jobs files as still-online-only.
    let before = SyncEngine::offline_readiness(&b, &layout.jobs_dir).unwrap();
    assert_eq!(before.total_files, 3, "three files under Jobs");
    assert_eq!(before.ready_files, 0, "none ready yet");
    assert!(before.total_chunks > 3, "multi-chunk files");
    assert_eq!(before.present_chunks, 0, "no chunks cached yet");
    assert!(!before.is_ready(), "subtree is NOT ready for offline");
    let mut missing: Vec<String> = before
        .missing_files
        .iter()
        .map(|f| f.path.clone())
        .collect();
    missing.sort();
    assert_eq!(
        missing,
        vec![
            "Jobs/ClientX/part.step",
            "Jobs/ClientX/sub/deep.lbrn2",
            "Jobs/a.stl"
        ]
    );

    // Hydrate just ONE file (part.step) directly. Now readiness is PARTIAL: 1/3 ready,
    // some chunks present, that one file no longer in the missing list.
    let client = offline_client();
    let part_id = &layout.files["part"].0;
    SyncEngine::hydrate(&b, &client, &cloud, part_id).unwrap();
    let partial = SyncEngine::offline_readiness(&b, &layout.jobs_dir).unwrap();
    assert_eq!(partial.ready_files, 1, "one file fully local now");
    assert!(
        partial.present_chunks > 0 && partial.present_chunks < partial.total_chunks,
        "partial chunk coverage"
    );
    assert!(!partial.is_ready(), "still not fully ready");
    assert!(
        !partial.missing_files.iter().any(|f| &f.node_id == part_id),
        "the hydrated file dropped off the missing list"
    );

    // Hydrate the rest → 100% ready. is_ready() true, no missing files.
    for label in ["top", "deep"] {
        SyncEngine::hydrate(&b, &client, &cloud, &layout.files[label].0).unwrap();
    }
    let after = SyncEngine::offline_readiness(&b, &layout.jobs_dir).unwrap();
    assert_eq!(after.ready_files, 3);
    assert_eq!(after.present_chunks, after.total_chunks);
    assert!(after.missing_files.is_empty());
    assert!(after.is_ready(), "Jobs is safe to take offline");
}

/// `offline_readiness` must touch NO network — it's the pre-disconnect check you run while
/// possibly already offline. We point the client at an unroutable address AND never pass it
/// (the API takes only the provider), so a network call is structurally impossible.
#[test]
fn offline_readiness_needs_no_network() {
    let (b, _cloud, layout) = fresh_device_with_subtree();
    // No client argument at all → cannot reach the network. Returns instantly.
    let r = SyncEngine::offline_readiness(&b, &layout.jobs_dir).unwrap();
    assert_eq!(r.total_files, 3);
}

// ---- ensure_available_offline (hydrate a whole subtree) -----------------------------

#[test]
fn ensure_available_offline_hydrates_a_whole_subtree() {
    let (b, cloud, layout) = fresh_device_with_subtree();
    let client = offline_client();

    // Pre-condition: nothing under Jobs is local.
    assert_eq!(
        SyncEngine::offline_readiness(&b, &layout.jobs_dir)
            .unwrap()
            .present_chunks,
        0
    );

    let summary =
        SyncEngine::ensure_available_offline(&b, &client, &cloud, &layout.jobs_dir, 4).unwrap();

    // Every Jobs file hydrated; the summary counts files + bytes and has no failures.
    assert_eq!(summary.files_hydrated, 3, "all three Jobs files hydrated");
    assert!(summary.failures.is_empty(), "no failures");
    let expected_bytes: u64 = ["top", "part", "deep"]
        .iter()
        .map(|l| layout.files[l].2.len() as u64)
        .sum();
    assert_eq!(
        summary.bytes_hydrated, expected_bytes,
        "bytes accounted across the subtree"
    );

    // Verified: readiness is now 100% and every file reads back byte-for-byte offline.
    assert!(SyncEngine::offline_readiness(&b, &layout.jobs_dir)
        .unwrap()
        .is_ready());
    for label in ["top", "part", "deep"] {
        let (_, ino, body) = &layout.files[label];
        assert_eq!(
            &read_all(&b, *ino),
            body,
            "{label} reads back byte-for-byte from local cache"
        );
    }

    // Scope discipline: the Archive file OUTSIDE Jobs was NOT hydrated.
    let (arch_id, _, _) = &layout.archive_file;
    assert!(
        b.node_manifest_hashes(arch_id)
            .unwrap()
            .iter()
            .any(|h| !b.has_chunk(h)),
        "out-of-subtree Archive file stays a placeholder"
    );
}

/// Idempotent: a second `ensure_available_offline` over an already-resident subtree fetches
/// nothing (0 files hydrated, 0 bytes) and still reports 100% ready.
#[test]
fn ensure_available_offline_is_idempotent() {
    let (b, cloud, layout) = fresh_device_with_subtree();
    let client = offline_client();

    let first =
        SyncEngine::ensure_available_offline(&b, &client, &cloud, &layout.jobs_dir, 4).unwrap();
    assert_eq!(first.files_hydrated, 3);

    let second =
        SyncEngine::ensure_available_offline(&b, &client, &cloud, &layout.jobs_dir, 4).unwrap();
    assert_eq!(second.files_hydrated, 0, "nothing left to hydrate");
    assert_eq!(second.bytes_hydrated, 0);
    assert!(second.failures.is_empty());
    assert!(SyncEngine::offline_readiness(&b, &layout.jobs_dir)
        .unwrap()
        .is_ready());
}

/// Works on a single FILE too (not only a folder).
#[test]
fn ensure_available_offline_on_a_single_file() {
    let (b, cloud, layout) = fresh_device_with_subtree();
    let client = offline_client();
    let (deep_id, deep_ino, deep_body) = &layout.files["deep"];

    let summary = SyncEngine::ensure_available_offline(&b, &client, &cloud, deep_id, 4).unwrap();
    assert_eq!(summary.files_hydrated, 1);
    assert!(SyncEngine::offline_readiness(&b, deep_id)
        .unwrap()
        .is_ready());
    assert_eq!(&read_all(&b, *deep_ino), deep_body);
}
