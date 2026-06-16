//! G9 — local instant search over the SQLite tree.
//!
//! The local-first promise: search hits the device's own SQLite tree directly — no WAN,
//! fully offline. Matches a substring of the basename *or* the full path,
//! case-insensitively, and never returns a node that's been removed from the tree
//! (the client tree hard-removes on delete, so there are no tombstones to leak).

mod common;
use common::*;

use fs_provider::{Kind, Provider, SearchOpts};

/// Match on basename, case-insensitive, with the full reconstructed path returned.
#[test]
fn matches_basename_case_insensitive() {
    let p = provider();
    let proj = p.mkdir(ROOT, "Project").unwrap();
    write_file(&p, proj.ino, "Bracket.STL", b"solid");
    write_file(&p, ROOT, "notes.txt", b"hi");

    // lowercase query finds the mixed-case name
    let hits = p.search("bracket", &SearchOpts::default()).unwrap();
    assert_eq!(hits.len(), 1, "exactly one match, got {hits:?}");
    let h = &hits[0];
    assert_eq!(h.name, "Bracket.STL");
    assert_eq!(h.kind, Kind::File);
    assert_eq!(h.path, "/Project/Bracket.STL", "full path reconstructed");
    assert_eq!(h.size, 5);
    assert!(h.node_id.len() > 0);
}

/// Match on an interior path component (the folder name), not just the basename.
#[test]
fn matches_on_path_component() {
    let p = provider();
    let proj = p.mkdir(ROOT, "RobotArm").unwrap();
    write_file(&p, proj.ino, "servo.step", b"x");
    write_file(&p, ROOT, "loose.step", b"y");

    // "robotarm" matches the file under it (by path) but not the loose one
    let hits = p.search("robotarm", &SearchOpts::default()).unwrap();
    let paths: Vec<_> = hits.iter().map(|h| h.path.clone()).collect();
    assert!(
        paths.contains(&"/RobotArm".to_string()),
        "the dir itself matches: {paths:?}"
    );
    assert!(
        paths.contains(&"/RobotArm/servo.step".to_string()),
        "child matches by path: {paths:?}"
    );
    assert!(
        !paths.contains(&"/loose.step".to_string()),
        "unrelated file excluded"
    );
}

/// A directory is matchable and returned with kind=Dir and size 0.
#[test]
fn matches_directories() {
    let p = provider();
    p.mkdir(ROOT, "Enclosure").unwrap();
    let hits = p.search("enclosure", &SearchOpts::default()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, Kind::Dir);
    assert_eq!(hits[0].path, "/Enclosure");
}

/// Deleting a node removes it from search results (no tombstones on the client tree).
#[test]
fn excludes_deleted_nodes() {
    let p = provider();
    let ino = write_file(&p, ROOT, "gone.scad", b"z");
    let node_id = p.node_id_for(ino).unwrap().unwrap();
    assert_eq!(p.search("gone", &SearchOpts::default()).unwrap().len(), 1);

    p.delete_by_node_id(&node_id).unwrap();
    assert_eq!(
        p.search("gone", &SearchOpts::default()).unwrap().len(),
        0,
        "deleted node not found"
    );
}

/// Works on a deeply nested tree and reconstructs the whole path.
#[test]
fn deep_nesting_full_path() {
    let p = provider();
    let a = p.mkdir(ROOT, "a").unwrap();
    let b = p.mkdir(a.ino, "b").unwrap();
    let c = p.mkdir(b.ino, "c").unwrap();
    write_file(&p, c.ino, "deep.gcode", b"g");

    let hits = p.search("deep.gcode", &SearchOpts::default()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/a/b/c/deep.gcode");
}

/// An online-only (never-hydrated) node is still found — search is metadata-only, no bytes.
#[test]
fn finds_online_only_node_without_hydration() {
    // B applies A's exported tree but never hydrates the chunk bytes; the node exists in
    // B's metadata tree, so search must find it without touching the chunk store.
    let (a, b, _dir) = shared_store_pair();
    write_file(&a, ROOT, "archived.3mf", b"big-cold-file");
    replicate_tree(&a, &b);

    // B has the node in its tree but no local bytes — search is metadata-only.
    let hits = b.search("archived", &SearchOpts::default()).unwrap();
    assert_eq!(hits.len(), 1, "online-only node is searchable: {hits:?}");
    assert_eq!(hits[0].path, "/archived.3mf");
}

/// The root itself is never returned as a hit (its name is empty).
#[test]
fn never_returns_root() {
    let p = provider();
    write_file(&p, ROOT, "x.txt", b"a");
    // An empty query would otherwise match everything including root.
    let hits = p.search("", &SearchOpts::default()).unwrap();
    assert!(
        hits.iter().all(|h| h.node_id != "root"),
        "root excluded: {hits:?}"
    );
    assert!(hits.iter().all(|h| !h.path.is_empty()));
}

/// Results are capped by the limit and offset paginates deterministically.
#[test]
fn respects_limit_and_offset() {
    let p = provider();
    for i in 0..10 {
        write_file(&p, ROOT, &format!("part{i:02}.bin"), b"x");
    }
    let page1 = p
        .search(
            "part",
            &SearchOpts {
                limit: 3,
                offset: 0,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page1.len(), 3, "limit respected");

    let page2 = p
        .search(
            "part",
            &SearchOpts {
                limit: 3,
                offset: 3,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page2.len(), 3);

    // No overlap between the two pages (stable ordering).
    let p1: std::collections::HashSet<_> = page1.iter().map(|h| &h.node_id).collect();
    let p2: std::collections::HashSet<_> = page2.iter().map(|h| &h.node_id).collect();
    assert!(p1.is_disjoint(&p2), "pages don't overlap");
}

/// Search is reachable when the provider is held behind an `Arc` (the daemon shares one
/// `Arc<SqliteProvider>`), via deref to the concrete method.
#[test]
fn reachable_through_arc() {
    use std::sync::Arc;
    let p = Arc::new(provider());
    write_file(&p, ROOT, "z.txt", b"a");
    let hits = p.search("z.txt", &SearchOpts::default()).unwrap();
    assert!(hits.iter().any(|h| h.name == "z.txt"));
}
