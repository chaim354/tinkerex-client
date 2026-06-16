//! Git-on-mount: does a git repository work inside the Maker Server sync filesystem?
//!
//! Git is the canonical "small-files + atomic-rename + lockfile + symlink" workload. These
//! tests simulate, at the `Provider` level (the layer that runs on every OS), the exact
//! syscall sequences `git` issues — so we get a real signal on macOS where `fs-fuse` can't
//! link. The gold-standard real-FUSE signal lives behind a Linux/Docker harness (see the PR
//! notes); this suite is the mechanical backstop for the git-critical operations.
//!
//! What git needs (mapped to the model):
//! - many small files + `mkdir` (objects, refs)            → File/Dir, already supported
//! - atomic rename over an existing name (`index.lock`→`index`) → `rename` POSIX-replace, supported
//! - O_EXCL lockfile create (`*.lock`)                      → create must fail EEXIST if present
//! - chmod exec bit (hooks, mode 100755 blobs)              → `set_mode`, already synced
//! - **symlinks** (a repo can contain/check out a symlink)  → NEW: `Kind::Symlink` + target
//! - hardlinks (`clone --local` object dedup)               → documented gap (git degrades)

mod common;
use common::*;

use fs_provider::{FsError, Kind, Provider};

// ---------------------------------------------------------------------------
// Symlink support — the git-critical gap this MVP closes.
// A git repo can store a symlink (mode 120000); `git checkout` then calls
// symlink(2) to materialize it, and `git status`/`git add` call lstat+readlink.
// ---------------------------------------------------------------------------

#[test]
fn symlink_then_readlink_roundtrips_the_target() {
    let p = provider();
    let attr = p.symlink(ROOT, "link", b"target/path.txt").unwrap();
    assert_eq!(attr.kind, Kind::Symlink);
    // lstat reports the symlink itself (size = target length).
    let st = p.getattr(attr.ino).unwrap();
    assert_eq!(st.kind, Kind::Symlink);
    // readlink returns the stored target bytes verbatim.
    assert_eq!(p.readlink(attr.ino).unwrap(), b"target/path.txt");
}

#[test]
fn symlink_shows_in_readdir_as_a_symlink() {
    let p = provider();
    p.symlink(ROOT, "link", b"elsewhere").unwrap();
    let entry = p
        .readdir(ROOT)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "link")
        .unwrap();
    assert_eq!(entry.kind, Kind::Symlink);
}

#[test]
fn readlink_on_a_regular_file_is_an_error() {
    let p = provider();
    let f = write_file(&p, ROOT, "f", b"not a link");
    assert!(matches!(
        p.readlink(f),
        Err(FsError::NotSupported) | Err(FsError::NotFound)
    ));
}

#[test]
fn symlink_duplicate_name_is_already_exists() {
    let p = provider();
    p.symlink(ROOT, "link", b"a").unwrap();
    assert!(matches!(
        p.symlink(ROOT, "link", b"b"),
        Err(FsError::AlreadyExists)
    ));
}

// ---------------------------------------------------------------------------
// Symlinks must round-trip through sync (export_tree / sync_node / apply_remote)
// or a checked-out symlink would vanish (or arrive empty) on the other device.
// ---------------------------------------------------------------------------

#[test]
fn symlink_round_trips_through_export_and_apply() {
    let (a, b, _store) = shared_store_pair();
    a.symlink(ROOT, "link", b"../objects/pack").unwrap();

    // export_tree carries the kind + target; apply on B reconstructs it.
    replicate_tree(&a, &b);

    let st = b.lookup(ROOT, "link").unwrap();
    assert_eq!(st.kind, Kind::Symlink);
    assert_eq!(b.readlink(st.ino).unwrap(), b"../objects/pack");
}

#[test]
fn symlink_round_trips_through_sync_node_outbox() {
    // The daemon path: a local symlink enqueues an outbox upsert; sync_node yields a
    // SyncNode the replica applies. (sync_to drives outbox→sync_node→apply_remote.)
    let (a, b, _store) = shared_store_pair();
    a.symlink(ROOT, "HEAD", b"refs/heads/main").unwrap();

    sync_to(&a, &[&b]);

    let st = b.lookup(ROOT, "HEAD").unwrap();
    assert_eq!(st.kind, Kind::Symlink);
    assert_eq!(b.readlink(st.ino).unwrap(), b"refs/heads/main");
}

#[test]
fn changing_a_symlink_target_propagates() {
    let (a, b, _store) = shared_store_pair();
    a.symlink(ROOT, "cur", b"v1").unwrap();
    sync_to(&a, &[&b]);
    // Atomic-swap a symlink the way tooling does: new temp link, rename over.
    a.symlink(ROOT, "cur.tmp", b"v2").unwrap();
    a.rename(ROOT, "cur.tmp", ROOT, "cur").unwrap();
    sync_to(&a, &[&b]);

    let st = b.lookup(ROOT, "cur").unwrap();
    assert_eq!(b.readlink(st.ino).unwrap(), b"v2");
}

/// When a pull changes an EXISTING symlink's target (same node_id, in place), `apply_remote`
/// must report `content_changed` so the FUSE adapter invalidates the kernel's cached readlink.
/// Symlinks have no manifest, so the manifest-comparison heuristic would miss this — the target
/// itself must be compared.
#[test]
fn apply_remote_flags_content_changed_when_symlink_target_changes() {
    let (a, b, _store) = shared_store_pair();
    a.symlink(ROOT, "lnk", b"old-target").unwrap();
    replicate_tree(&a, &b);

    // A repoints the same link; B pulls the in-place update.
    let node = a
        .export_tree()
        .unwrap()
        .into_iter()
        .find(|n| n.name == "lnk")
        .unwrap();
    let mut changed = node.clone();
    changed.symlink_target = Some(b"new-target".to_vec());
    let outcome = b.apply_remote(&changed).unwrap();

    assert!(
        outcome.content_changed,
        "a changed symlink target must flag content_changed"
    );
    let st = b.lookup(ROOT, "lnk").unwrap();
    assert_eq!(b.readlink(st.ino).unwrap(), b"new-target");
}

// ---------------------------------------------------------------------------
// Git-critical POSIX operations that AREN'T about symlinks — verified to hold
// against the model so we have an explicit per-op signal, not an assumption.
// ---------------------------------------------------------------------------

/// `.git/index.lock` → `.git/index`: git writes its new index to a lock file then
/// atomically renames it over the live index. This is THE operation that breaks atomic
/// saves if rename-over-existing isn't POSIX-replace. (Already supported — this is the
/// regression guard for the git case specifically.)
#[test]
fn index_lock_atomic_rename_replaces_the_index() {
    let p = provider();
    let git = p.mkdir(ROOT, ".git").unwrap().ino;
    write_file(&p, git, "index", b"old index v1");

    // git writes the next index to index.lock, then renames it over index.
    write_file(&p, git, "index.lock", b"new index v2");
    p.rename(git, "index.lock", git, "index").unwrap();

    // index now holds the new content; the lock name is gone (consumed by the rename).
    let idx = p.lookup(git, "index").unwrap();
    assert_eq!(read_all(&p, idx.ino), b"new index v2");
    assert!(matches!(
        p.lookup(git, "index.lock"),
        Err(FsError::NotFound)
    ));
}

/// Git takes a lock by creating `*.lock` with O_EXCL — a second `create` of the same name
/// must fail (EEXIST), which is how git detects a concurrent operation. If create silently
/// succeeded/overwrote, two gits could corrupt the repo.
#[test]
fn lockfile_create_is_exclusive() {
    let p = provider();
    let git = p.mkdir(ROOT, ".git").unwrap().ino;
    p.create(git, "config.lock").unwrap();
    // O_EXCL semantics: the second create must report EEXIST.
    assert!(matches!(
        p.create(git, "config.lock"),
        Err(FsError::AlreadyExists)
    ));
}

/// Git hooks and 100755-mode blobs must keep their executable bit on checkout, and it must
/// propagate across devices (the mode field already syncs — this pins the git case).
#[test]
fn executable_bit_on_a_hook_survives_sync() {
    let (a, b, _store) = shared_store_pair();
    let hooks = a.mkdir(ROOT, "hooks").unwrap().ino;
    let f = a.create(hooks, "pre-commit").unwrap();
    a.write(f.ino, 0, b"#!/bin/sh\nexit 0\n").unwrap();
    a.flush(f.ino).unwrap();
    a.set_mode(f.ino, 0o755).unwrap();

    sync_to(&a, &[&b]);

    let dir = b.lookup(ROOT, "hooks").unwrap();
    let hook = b.lookup(dir.ino, "pre-commit").unwrap();
    assert_eq!(
        hook.mode & 0o111,
        0o111,
        "executable bits must survive sync"
    );
}

/// A git "object" directory layout: many small files in 2-char fanout dirs. Verifies the
/// nested mkdir + small-file create/read path (the bulk of what `git add`/`commit` does).
#[test]
fn loose_object_fanout_dirs_and_small_files() {
    let p = provider();
    let git = p.mkdir(ROOT, ".git").unwrap().ino;
    let objects = p.mkdir(git, "objects").unwrap().ino;
    let fan = p.mkdir(objects, "ab").unwrap().ino;
    let oid = write_file(&p, fan, "cdef0123456789", b"\x78\x01blob payload");

    // readdir of the fanout dir, lookup + read the object back byte-for-byte.
    let entries = p.readdir(fan).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(read_all(&p, oid), b"\x78\x01blob payload");
}
