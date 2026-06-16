//! Selective-sync (subscription) hermetic tests.
//!
//! These exercise the per-device subscription policy + the proactive-hydration gate
//! entirely through the provider's public API — the same surface the sync engine consults.
//! The invariant under test throughout: **the tree stays complete** (metadata for every
//! node is always present locally, so navigation + convergence are unaffected); only
//! whether bytes are *proactively* kept resident changes.

mod common;
use common::*;

use fs_provider::{Provider, SubRule};

/// With no rules set, every node is in scope (the backward-compatible default) — selective
/// sync is strictly opt-in.
#[test]
fn default_is_everything_in_scope() {
    let p = provider();
    let ino = write_file(&p, ROOT, "a.bin", b"hello");
    let node_id = p.node_id_for(ino).unwrap().unwrap();

    assert_eq!(p.subscription_default().unwrap(), SubRule::Include);
    assert!(p.proactively_hydrate(&node_id).unwrap(), "no rules → in scope");
    assert!(p.proactively_hydrate_ino(ino).unwrap());
}

/// Unsubscribing a folder takes its whole subtree out of scope; subscribing a child back
/// in overrides the ancestor for that child (closest-ancestor-wins).
#[test]
fn subtree_scope_and_nearest_ancestor_wins() {
    let p = provider();
    let archive = p.mkdir(ROOT, "archive").unwrap();
    let sub = p.mkdir(archive.ino, "2019").unwrap();
    let deep = write_file(&p, sub.ino, "old.stl", b"big old file");
    let active = write_file(&p, ROOT, "active.stl", b"current");

    let archive_id = p.node_id_for(archive.ino).unwrap().unwrap();
    let sub_id = p.node_id_for(sub.ino).unwrap().unwrap();
    let deep_id = p.node_id_for(deep).unwrap().unwrap();
    let active_id = p.node_id_for(active).unwrap().unwrap();

    // Unsubscribe the whole archive subtree.
    p.unsubscribe(&archive_id).unwrap();
    assert!(!p.proactively_hydrate(&deep_id).unwrap(), "deep file inherits archive's exclude");
    assert!(!p.proactively_hydrate(&sub_id).unwrap(), "intermediate dir is excluded too");
    assert!(p.proactively_hydrate(&active_id).unwrap(), "a sibling outside archive stays in scope");

    // Re-subscribe the nested 2019 folder — the closer rule wins for that subtree.
    p.subscribe(&sub_id).unwrap();
    assert!(p.proactively_hydrate(&deep_id).unwrap(), "2019's include overrides archive's exclude");
    assert!(!p.proactively_hydrate(&archive_id).unwrap(), "archive itself still excluded");
}

/// The "subscribe to a few folders" shape: flip the default to Exclude, then include only
/// the folders you want.
#[test]
fn default_exclude_then_include_selected() {
    let p = provider();
    let projects = p.mkdir(ROOT, "projects").unwrap();
    let a = write_file(&p, projects.ino, "a.stl", b"a");
    let archive = p.mkdir(ROOT, "archive").unwrap();
    let b = write_file(&p, archive.ino, "b.stl", b"b");

    p.set_subscription_default(SubRule::Exclude).unwrap();
    let a_id = p.node_id_for(a).unwrap().unwrap();
    let b_id = p.node_id_for(b).unwrap().unwrap();
    assert!(!p.proactively_hydrate(&a_id).unwrap(), "default exclude → out of scope");
    assert!(!p.proactively_hydrate(&b_id).unwrap());

    let projects_id = p.node_id_for(projects.ino).unwrap().unwrap();
    p.subscribe(&projects_id).unwrap();
    assert!(p.proactively_hydrate(&a_id).unwrap(), "explicitly-included folder is in scope");
    assert!(!p.proactively_hydrate(&b_id).unwrap(), "everything else still excluded");
}

/// `proactive_hydration_targets` is the engine's post-pull work list: it lists in-scope
/// files with missing chunks and EXCLUDES out-of-scope files (they stay placeholders).
#[test]
fn proactive_targets_skips_out_of_scope() {
    // A creates files (chunks in A's store); B gets the metadata only and starts with an
    // EMPTY local chunk store (separate from A's) — so every pulled file is initially a
    // placeholder with missing chunks. B's target list then reflects its subscription policy.
    use fs_provider::{ChunkConfig, LocalChunkStore, SqliteProvider};
    let tiny = ChunkConfig { min: 64, avg: 256, max: 1024 };
    let dir_a = tempdir::Dir::new();
    let dir_b = tempdir::Dir::new();
    let a = SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir_a.path()).unwrap()), tiny).unwrap();
    let b = SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir_b.path()).unwrap()), tiny).unwrap();

    let active = a.mkdir(ROOT, "active").unwrap();
    let active_id = a.node_id_for(active.ino).unwrap().unwrap();
    write_file(&a, active.ino, "f1.bin", &vec![1u8; 4096]);

    let archive = a.mkdir(ROOT, "archive").unwrap();
    let archive_id = a.node_id_for(archive.ino).unwrap().unwrap();
    write_file(&a, archive.ino, "f2.bin", &vec![2u8; 4096]);

    // Replicate the full tree to B (metadata only — B has no chunk bytes yet).
    replicate_tree(&a, &b);

    // B excludes the archive subtree.
    b.unsubscribe(&archive_id).unwrap();
    b.subscribe(&active_id).unwrap();

    let targets = b.proactive_hydration_targets().unwrap();
    // Find the node_ids B sees for the two files.
    let f1 = b.export_tree().unwrap().into_iter().find(|n| n.name == "f1.bin").unwrap().node_id;
    let f2 = b.export_tree().unwrap().into_iter().find(|n| n.name == "f2.bin").unwrap().node_id;

    assert!(targets.contains(&f1), "in-scope file is a proactive-hydration target");
    assert!(!targets.contains(&f2), "out-of-scope file is NOT proactively hydrated");

    // The tree is complete on B regardless of scope — the out-of-scope file's metadata is
    // still present and navigable (tree completeness preserved).
    let tree = b.export_tree().unwrap();
    assert!(tree.iter().any(|n| n.name == "f1.bin"));
    assert!(
        tree.iter().any(|n| n.name == "f2.bin"),
        "out-of-scope file's metadata is still present (tree completeness)"
    );
}

/// On-demand read of an out-of-scope file must still work (graceful degradation) — the
/// gate only governs *proactive* hydration, never an actual read.
#[test]
fn on_demand_read_ignores_subscription() {
    let (a, b, _dir) = shared_store_pair();
    let archive = a.mkdir(ROOT, "archive").unwrap();
    let body = vec![7u8; 4096];
    write_file(&a, archive.ino, "doc.bin", &body);
    replicate_tree(&a, &b);

    let archive_id = b.export_tree().unwrap().into_iter().find(|n| n.name == "archive").unwrap().node_id;
    b.unsubscribe(&archive_id).unwrap();

    let doc_id = b.export_tree().unwrap().into_iter().find(|n| n.name == "doc.bin").unwrap().node_id;
    assert!(!b.proactively_hydrate(&doc_id).unwrap(), "out of scope: not proactively hydrated");

    // It's NOT in the proactive list…
    assert!(!b.proactive_hydration_targets().unwrap().contains(&doc_id));

    // …but the chunks ARE in the shared store, so an on-demand read (the engine's
    // ungated `hydrate` path → the provider `read`) still returns the bytes. The shared
    // store IS the chunk source here, so `read` reassembles from it directly.
    let doc_ino = b.ino_for_node_id(&doc_id).unwrap().unwrap();
    let out = read_all(&b, doc_ino);
    assert_eq!(out, body, "on-demand read of an out-of-scope file still works");
}

/// Unsubscribing a *hydrated* subtree and then dehydrating it reclaims the cached bytes,
/// while leaving the tree + manifests intact (so it re-hydrates on demand) and NEVER
/// touching chunks a still-in-scope file shares.
#[test]
fn dehydrate_subtree_reclaims_only_unshared_chunks() {
    let p = provider();

    let archive = p.mkdir(ROOT, "archive").unwrap();
    let archive_id = p.node_id_for(archive.ino).unwrap().unwrap();
    // A file unique to the archive.
    let unique_body = vec![42u8; 4096];
    let arch_file = write_file(&p, archive.ino, "unique.bin", &unique_body);

    // A file in the archive whose content is IDENTICAL to an in-scope file → they share
    // chunks (content-addressed dedup). The shared chunks must survive dehydration.
    let shared_body = vec![9u8; 4096];
    write_file(&p, archive.ino, "shared_copy.bin", &shared_body);
    write_file(&p, ROOT, "active_copy.bin", &shared_body); // in scope, same content

    // Capture the chunk hashes.
    let unique_hashes = node_manifest_hashes_of(&p, arch_file);
    let active_id =
        p.export_tree().unwrap().into_iter().find(|n| n.name == "active_copy.bin").unwrap().node_id;
    let active_ino = p.ino_for_node_id(&active_id).unwrap().unwrap();
    let shared_hashes = node_manifest_hashes_of(&p, active_ino);

    // All chunks are locally present (we just wrote them).
    for h in unique_hashes.iter().chain(shared_hashes.iter()) {
        assert!(p.has_chunk(h), "chunk should be cached before dehydration");
    }

    // Unsubscribe + dehydrate the archive subtree.
    p.unsubscribe(&archive_id).unwrap();
    let dropped = p.dehydrate_subtree(&archive_id).unwrap();
    assert!(dropped > 0, "some archive-only chunks were dropped");

    // Archive-only chunks gone…
    for h in &unique_hashes {
        assert!(!p.has_chunk(h), "archive-only chunk {h} should be dropped");
    }
    // …but the shared chunks (referenced by the in-scope active_copy.bin) survive.
    for h in &shared_hashes {
        assert!(p.has_chunk(h), "shared chunk {h} must NOT be dropped (in-scope file needs it)");
    }

    // Tree completeness: the archive file's metadata + manifest are still present (it's a
    // placeholder now, not deleted) — proven by it still being in the exported tree with a
    // manifest_id.
    let arch_node = p.export_tree().unwrap().into_iter().find(|n| n.name == "unique.bin").unwrap();
    assert!(arch_node.manifest_id.is_some(), "dehydrated file keeps its manifest (placeholder)");

    // And the in-scope copy still reads fine (its chunks were preserved).
    assert_eq!(read_all(&p, active_ino), shared_body);
}

/// Subscriptions are keyed by stable node_id, so they survive a rename/move of the
/// subtree root (a rename keeps node_id).
#[test]
fn subscription_survives_rename() {
    let p = provider();
    let dir = p.mkdir(ROOT, "old_name").unwrap();
    let dir_id = p.node_id_for(dir.ino).unwrap().unwrap();
    let f = write_file(&p, dir.ino, "f.bin", b"data");
    let f_id = p.node_id_for(f).unwrap().unwrap();

    p.unsubscribe(&dir_id).unwrap();
    assert!(!p.proactively_hydrate(&f_id).unwrap());

    // Rename the dir — node_id is stable, so the exclude still applies.
    p.rename(ROOT, "old_name", ROOT, "new_name").unwrap();
    assert!(!p.proactively_hydrate(&f_id).unwrap(), "exclude survives the rename (node_id stable)");
}

/// `clear_subscription` reverts a node to inheriting from its ancestry / the default.
#[test]
fn clear_subscription_reverts_to_default() {
    let p = provider();
    let f = write_file(&p, ROOT, "f.bin", b"x");
    let f_id = p.node_id_for(f).unwrap().unwrap();

    p.unsubscribe(&f_id).unwrap();
    assert!(!p.proactively_hydrate(&f_id).unwrap());
    p.clear_subscription(&f_id).unwrap();
    assert!(p.proactively_hydrate(&f_id).unwrap(), "back to the default (include)");
}

// ---- pin-for-offline (the workshop "always keep this resident") --------------------

/// Pinning a subtree forces it in-scope for proactive hydration **regardless of the device
/// default** — even when the default is Exclude (the "subscribe to a few folders" device)
/// and nothing else included it. This is the "make available offline" guarantee: a pinned
/// folder is always kept resident.
#[test]
fn pin_forces_in_scope_even_under_default_exclude() {
    let p = provider();
    let jobs = p.mkdir(ROOT, "jobs").unwrap();
    let f = write_file(&p, jobs.ino, "client.step", b"cad");
    let jobs_id = p.node_id_for(jobs.ino).unwrap().unwrap();
    let f_id = p.node_id_for(f).unwrap().unwrap();

    // Device is in "online-only by default" mode.
    p.set_subscription_default(SubRule::Exclude).unwrap();
    assert!(!p.proactively_hydrate(&f_id).unwrap(), "default exclude → out of scope");
    assert!(!p.is_pinned(&jobs_id).unwrap(), "not pinned yet");

    // Pin the jobs folder → its whole subtree is now in scope.
    p.pin(&jobs_id).unwrap();
    assert!(p.is_pinned(&jobs_id).unwrap(), "jobs is pinned");
    assert!(p.proactively_hydrate(&f_id).unwrap(), "a pinned subtree is forced in scope");
    assert!(p.pinned_nodes().unwrap().contains(&jobs_id), "pin shows up in the pin list");
}

/// A pin overrides an EXCLUDED ancestor (closest-rule-wins), so you can keep one job folder
/// resident while its parent archive is online-only.
#[test]
fn pin_overrides_an_excluded_ancestor() {
    let p = provider();
    let archive = p.mkdir(ROOT, "archive").unwrap();
    let job = p.mkdir(archive.ino, "active_job").unwrap();
    let f = write_file(&p, job.ino, "part.stl", b"part");
    let archive_id = p.node_id_for(archive.ino).unwrap().unwrap();
    let job_id = p.node_id_for(job.ino).unwrap().unwrap();
    let f_id = p.node_id_for(f).unwrap().unwrap();

    p.unsubscribe(&archive_id).unwrap(); // exclude the whole archive
    assert!(!p.proactively_hydrate(&f_id).unwrap(), "inherits archive's exclude");

    p.pin(&job_id).unwrap(); // pin the nested job
    assert!(p.proactively_hydrate(&f_id).unwrap(), "the closer pin overrides the excluded ancestor");
    assert!(!p.proactively_hydrate(&archive_id).unwrap(), "archive itself still excluded");
}

/// `unpin` removes the pin → the subtree reverts to inheriting from its ancestry / default.
#[test]
fn unpin_reverts_to_inherited_scope() {
    let p = provider();
    let f = write_file(&p, ROOT, "f.bin", b"x");
    let f_id = p.node_id_for(f).unwrap().unwrap();

    p.set_subscription_default(SubRule::Exclude).unwrap();
    p.pin(&f_id).unwrap();
    assert!(p.proactively_hydrate(&f_id).unwrap(), "pinned → in scope");
    assert!(p.is_pinned(&f_id).unwrap());

    p.unpin(&f_id).unwrap();
    assert!(!p.is_pinned(&f_id).unwrap(), "no longer pinned");
    assert!(!p.proactively_hydrate(&f_id).unwrap(), "reverts to default exclude");
    assert!(p.pinned_nodes().unwrap().is_empty(), "pin list cleared");
}

/// A pin is keyed by stable node_id, so it survives a rename/move of the pinned subtree
/// root — exactly like a subscription.
#[test]
fn pin_survives_rename() {
    let p = provider();
    let dir = p.mkdir(ROOT, "old").unwrap();
    let dir_id = p.node_id_for(dir.ino).unwrap().unwrap();
    let f = write_file(&p, dir.ino, "f.bin", b"data");
    let f_id = p.node_id_for(f).unwrap().unwrap();

    p.set_subscription_default(SubRule::Exclude).unwrap();
    p.pin(&dir_id).unwrap();
    assert!(p.proactively_hydrate(&f_id).unwrap());

    p.rename(ROOT, "old", ROOT, "renamed").unwrap();
    assert!(p.is_pinned(&dir_id).unwrap(), "pin survives the rename (node_id stable)");
    assert!(p.proactively_hydrate(&f_id).unwrap(), "still forced in scope after rename");
}

/// A pin is **per-device local state** — it must NEVER leak into the synced tree (it lives
/// in the local-only `subscriptions` table, never pushed). Pinning enqueues nothing to the
/// outbox and changes nothing in the exported tree.
#[test]
fn pin_is_local_only_never_synced() {
    let p = provider();
    let dir = p.mkdir(ROOT, "d").unwrap();
    let dir_id = p.node_id_for(dir.ino).unwrap().unwrap();
    // Drain whatever the mkdir queued so we measure only the pin's effect.
    p.outbox_clear(&dir_id).unwrap();
    let tree_before = p.export_tree().unwrap();

    p.pin(&dir_id).unwrap();

    assert!(p.outbox_list_topo().unwrap().is_empty(), "pinning enqueues nothing to the outbox");
    assert_eq!(p.export_tree().unwrap(), tree_before, "pinning does not change the synced tree");
}

// ---- small local helpers (the suite's common mod has no node-id manifest helper) ----

/// The chunk hashes of a file by inode, via the provider's public node-id API.
fn node_manifest_hashes_of(p: &fs_provider::SqliteProvider, ino: u64) -> Vec<String> {
    let node_id = p.node_id_for(ino).unwrap().unwrap();
    p.node_manifest_hashes(&node_id).unwrap()
}
