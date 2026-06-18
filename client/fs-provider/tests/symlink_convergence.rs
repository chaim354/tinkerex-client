//! Symlink convergence + lifecycle across replicas.
//!
//! Symlinks are the one "special" POSIX kind the sync engine models (git checkout
//! materializes 120000 entries). `tree_snapshot` proves file/dir content via the
//! content-addressed `manifest_id`, but a symlink has no manifest — its content IS the
//! target bytes — so convergence of links must be asserted via `readlink`. This suite
//! drives the full link lifecycle (create → retarget → rename → unlink → recreate) through
//! the outbox→apply pipeline and asserts every replica's link targets match the writer,
//! catching exactly the class of bug the `unlink kind='file'` defect was (a link the
//! writer dropped that a replica kept, or vice-versa).

mod common;
use common::*;

use fs_provider::Provider;

/// All (name → target) symlinks directly under ROOT, for cross-replica equality.
fn links_under_root(p: &impl Provider) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = p
        .readdir(ROOT)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == Kind::Symlink)
        .map(|e| {
            let attr = p.lookup(ROOT, &e.name).unwrap();
            (e.name, p.readlink(attr.ino).unwrap())
        })
        .collect();
    v.sort();
    v
}

#[test]
fn symlink_lifecycle_converges_across_a_replica() {
    let (a, b, _dir) = shared_store_pair();

    // create two links + a real file (the link targets need not exist)
    a.symlink(ROOT, "cfg", b"/etc/app.conf").unwrap();
    a.symlink(ROOT, "latest", b"releases/v1").unwrap();
    write_file(&a, ROOT, "data", b"hello");
    sync_to(&a, &[&b]);
    assert_eq!(links_under_root(&a), links_under_root(&b));
    assert_eq!(links_under_root(&b).len(), 2);

    // retarget a link by replace (unlink + recreate with a new target) — the editor /
    // `ln -sf` pattern. This is where the unlink-symlink bug bit: the unlink pushed a
    // delete but kept the row locally.
    a.unlink(ROOT, "latest").unwrap();
    a.symlink(ROOT, "latest", b"releases/v2").unwrap();
    sync_to(&a, &[&b]);
    assert_eq!(links_under_root(&a), links_under_root(&b));
    assert_eq!(
        b.readlink(b.lookup(ROOT, "latest").unwrap().ino).unwrap(),
        b"releases/v2"
    );

    // rename a link (mv) — same node_id, new name.
    a.rename(ROOT, "cfg", ROOT, "config").unwrap();
    sync_to(&a, &[&b]);
    assert_eq!(links_under_root(&a), links_under_root(&b));
    assert!(b.lookup(ROOT, "cfg").is_err());
    assert_eq!(
        b.readlink(b.lookup(ROOT, "config").unwrap().ino).unwrap(),
        b"/etc/app.conf"
    );

    // delete a link entirely — it must vanish from the replica (the divergence guard).
    a.unlink(ROOT, "config").unwrap();
    sync_to(&a, &[&b]);
    assert_eq!(links_under_root(&a), links_under_root(&b));
    assert!(
        b.lookup(ROOT, "config").is_err(),
        "deleted symlink survived on the replica"
    );
    assert_eq!(links_under_root(&b).len(), 1); // only "latest" remains
}

#[test]
fn unlinked_symlink_is_not_resurrected_by_a_re_export() {
    // The precise shape of the fixed bug: after unlinking a symlink, a fresh export +
    // re-apply (a far-behind replica catching up via export_tree, or a re-push) must NOT
    // re-introduce it.
    let (a, b, _dir) = shared_store_pair();
    a.symlink(ROOT, "link", b"/target").unwrap();
    sync_to(&a, &[&b]);
    assert!(b.lookup(ROOT, "link").is_ok());

    a.unlink(ROOT, "link").unwrap();
    // Drain the delete to b.
    sync_to(&a, &[&b]);

    // A brand-new replica catching up purely from A's CURRENT export must not see it.
    let c = {
        let dir = a.export_tree().unwrap();
        // Reuse b's chunk store implicitly via shared dir is unnecessary for a pure-link tree.
        let c = provider();
        for n in &dir {
            c.apply_remote(n).unwrap();
        }
        c
    };
    assert!(
        c.lookup(ROOT, "link").is_err(),
        "export_tree still contains the unlinked symlink — re-push would resurrect it"
    );
}
