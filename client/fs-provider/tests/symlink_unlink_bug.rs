//! Regression: `unlink` must remove a **symlink** node, not just files.
//!
//! POSIX `unlink(2)` removes any non-directory directory entry — regular files AND
//! symbolic links. The provider's `unlink` correctly accepts a symlink (it's not a dir,
//! so no EISDIR) and enqueues a `delete` to the outbox, but the actual row removal was
//! scoped `... AND kind = 'file'`, so the symlink row survived. Net effect: a delete was
//! pushed to the server while the link stayed in the local tree — silent divergence (the
//! local replica keeps a node every other device has dropped, and a re-export/re-push
//! would resurrect it).

mod common;
use common::*;

use fs_provider::{FsError, Provider};

#[test]
fn unlink_removes_a_symlink_node() {
    let p = provider();
    p.symlink(ROOT, "link", b"/some/target").unwrap();

    // Sanity: it's there and resolvable.
    let attr = p.lookup(ROOT, "link").unwrap();
    assert_eq!(attr.kind, Kind::Symlink);

    // unlink must succeed AND actually remove the directory entry.
    p.unlink(ROOT, "link").unwrap();

    // The link must be gone from the tree.
    assert!(
        matches!(p.lookup(ROOT, "link"), Err(FsError::NotFound)),
        "symlink survived unlink — local tree diverged from the pushed delete"
    );
    // readdir must not list it either.
    let names: Vec<_> = p
        .readdir(ROOT)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        !names.contains(&"link".to_string()),
        "unlinked symlink still appears in readdir: {names:?}"
    );
    // And it must be gone from the sync export (or the next push resurrects it).
    let exported: Vec<_> = p
        .export_tree()
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert!(
        !exported.contains(&"link".to_string()),
        "unlinked symlink still in export_tree — would re-push and resurrect: {exported:?}"
    );
}

#[test]
fn unlink_still_removes_a_regular_file() {
    // Guard the original behavior didn't regress.
    let p = provider();
    write_file(&p, ROOT, "f", b"hello");
    p.unlink(ROOT, "f").unwrap();
    assert!(matches!(p.lookup(ROOT, "f"), Err(FsError::NotFound)));
}
