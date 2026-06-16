//! pjdfstest-style **errno conformance** for the operations our sync mount implements.
//!
//! pjdfstest checks the *exact* errno of every failure path (ENOENT / EEXIST / ENOTDIR /
//! EISDIR / ENOTEMPTY / ENAMETOOLONG / EINVAL), not just "it errored". Several of these
//! were wrong or silently succeeded before (e.g. `unlink` of a directory returned ENOENT
//! instead of EISDIR; `rename` of a file *over* an empty directory silently succeeded;
//! `rename` into a missing parent orphaned the node). Each test below pins one rule the
//! way the suite's `chmod`/`rename`/`unlink`/`rmdir`/`mkdir`/`open`/`truncate` `.t` files do.
//!
//! These are Provider-level (run on macOS, no kernel mount). The real-FUSE pjdfstest run
//! (`client/test-conformance/pjdfstest-fuse.sh`, Linux-only) exercises the same rules
//! through the kernel; the errno map lives in `fs-fuse::errno`.

mod common;
use common::*;

use fs_provider::{FsError, Provider};

// ---- unlink / rmdir target-kind errnos -------------------------------------------------

#[test]
fn unlink_of_a_directory_is_eisdir() {
    // POSIX: unlink() on a directory fails. Linux returns EISDIR (FreeBSD EPERM); we map
    // to EISDIR. It must NOT masquerade as ENOENT (which is what filtering kind='file' did).
    let p = provider();
    p.mkdir(ROOT, "d").unwrap();
    assert!(matches!(p.unlink(ROOT, "d"), Err(FsError::IsADirectory)));
    // The directory is untouched.
    assert!(p.lookup(ROOT, "d").is_ok());
}

#[test]
fn rmdir_of_a_file_is_enotdir() {
    // POSIX: rmdir() on a non-directory fails with ENOTDIR. Must not look like ENOENT.
    let p = provider();
    write_file(&p, ROOT, "f", b"x");
    assert!(matches!(p.rmdir(ROOT, "f"), Err(FsError::NotADirectory)));
    assert!(p.lookup(ROOT, "f").is_ok());
}

// ---- rename cross-type + destination-parent errnos -------------------------------------

#[test]
fn rename_file_onto_existing_directory_is_eisdir() {
    // POSIX rename(): the source is not a directory but the destination IS → EISDIR.
    // Before this fix it *silently succeeded* over an empty dir (and was ENOTEMPTY over a
    // non-empty one, masking the real rule). pjdfstest rename/09.
    let p = provider();
    write_file(&p, ROOT, "src", b"s");
    p.mkdir(ROOT, "dstdir").unwrap();
    assert!(matches!(
        p.rename(ROOT, "src", ROOT, "dstdir"),
        Err(FsError::IsADirectory)
    ));
    // Both survive: nothing was replaced.
    assert_eq!(read_all(&p, p.lookup(ROOT, "src").unwrap().ino), b"s");
    assert_eq!(p.lookup(ROOT, "dstdir").unwrap().kind, Kind::Dir);
}

#[test]
fn rename_directory_onto_existing_file_is_enotdir() {
    // POSIX rename(): the source IS a directory but the destination is NOT → ENOTDIR.
    // Before this fix it silently clobbered the file with the directory. pjdfstest rename/10.
    let p = provider();
    p.mkdir(ROOT, "srcdir").unwrap();
    write_file(&p, ROOT, "dstfile", b"d");
    assert!(matches!(
        p.rename(ROOT, "srcdir", ROOT, "dstfile"),
        Err(FsError::NotADirectory)
    ));
    assert_eq!(p.lookup(ROOT, "srcdir").unwrap().kind, Kind::Dir);
    assert_eq!(read_all(&p, p.lookup(ROOT, "dstfile").unwrap().ino), b"d");
}

#[test]
fn rename_into_missing_parent_is_enoent() {
    // POSIX rename(): a component of the destination path prefix doesn't exist → ENOENT.
    // Before this fix the UPDATE happily reparented the node under a non-existent ino,
    // orphaning it (it vanished from every readdir but still occupied a row). pjdfstest
    // rename/03.
    let p = provider();
    write_file(&p, ROOT, "src", b"s");
    assert!(matches!(
        p.rename(ROOT, "src", 999_999, "x"),
        Err(FsError::NotFound)
    ));
    // The source is still where it was — the failed rename was a no-op.
    assert_eq!(read_all(&p, p.lookup(ROOT, "src").unwrap().ino), b"s");
}

#[test]
fn rename_into_a_file_as_parent_is_enotdir() {
    // POSIX rename(): a component used as a directory in the destination path is a file →
    // ENOTDIR. Before this fix it reparented under the file's ino. pjdfstest rename/04.
    let p = provider();
    let f = write_file(&p, ROOT, "afile", b"x");
    write_file(&p, ROOT, "src", b"s");
    assert!(matches!(
        p.rename(ROOT, "src", f, "x"),
        Err(FsError::NotADirectory)
    ));
    assert_eq!(read_all(&p, p.lookup(ROOT, "src").unwrap().ino), b"s");
}

#[test]
fn rename_directory_into_its_own_subtree_is_einval() {
    // POSIX rename(): the destination is contained within the source directory → EINVAL.
    // Before this fix it created a cycle / orphaned the subtree (a directory became its own
    // ancestor). pjdfstest rename/17.
    let p = provider();
    let a = p.mkdir(ROOT, "a").unwrap();
    let b = p.mkdir(a.ino, "b").unwrap();
    // Try to move `a` to be `a/b/a` — `a` would contain itself.
    assert!(matches!(
        p.rename(ROOT, "a", b.ino, "a"),
        Err(FsError::InvalidArgument)
    ));
    // The tree is intact: a/b still resolves from root.
    assert_eq!(
        p.lookup(p.lookup(ROOT, "a").unwrap().ino, "b").unwrap().ino,
        b.ino
    );
}

#[test]
fn rename_directory_onto_nonempty_directory_is_enotempty() {
    // POSIX rename(): destination is a non-empty directory → ENOTEMPTY (dir-over-dir is
    // allowed only when the destination is empty). pjdfstest rename/12.
    let p = provider();
    let src = p.mkdir(ROOT, "src").unwrap();
    write_file(&p, src.ino, "x", b"x");
    let dst = p.mkdir(ROOT, "dst").unwrap();
    write_file(&p, dst.ino, "y", b"y");
    assert!(matches!(
        p.rename(ROOT, "src", ROOT, "dst"),
        Err(FsError::NotEmpty)
    ));
}

#[test]
fn rename_directory_onto_empty_directory_succeeds() {
    // The allowed dir-over-dir case: destination is an empty directory → replace it.
    let p = provider();
    let src = p.mkdir(ROOT, "src").unwrap();
    write_file(&p, src.ino, "kid", b"k");
    p.mkdir(ROOT, "dst").unwrap(); // empty
    p.rename(ROOT, "src", ROOT, "dst")
        .expect("dir over empty dir replaces");
    assert!(matches!(p.lookup(ROOT, "src"), Err(FsError::NotFound)));
    // The moved directory keeps its child.
    let dst = p.lookup(ROOT, "dst").unwrap();
    assert_eq!(dst.kind, Kind::Dir);
    assert_eq!(read_all(&p, p.lookup(dst.ino, "kid").unwrap().ino), b"k");
}

#[test]
fn rename_directory_into_a_sibling_subtree_is_allowed() {
    // The cycle guard must reject ONLY a move into the source's *own* subtree — a move into
    // an unrelated sibling's subtree is a perfectly legal directory move. Guards against the
    // ancestor-walk being over-eager (which would break ordinary `mv dir other/dir`).
    let p = provider();
    let src = p.mkdir(ROOT, "src").unwrap();
    write_file(&p, src.ino, "kid", b"k");
    let other = p.mkdir(ROOT, "other").unwrap();
    let deep = p.mkdir(other.ino, "deep").unwrap();
    p.rename(ROOT, "src", deep.ino, "moved")
        .expect("move into an unrelated subtree is legal");
    assert!(matches!(p.lookup(ROOT, "src"), Err(FsError::NotFound)));
    let moved = p.lookup(deep.ino, "moved").unwrap();
    assert_eq!(moved.kind, Kind::Dir);
    assert_eq!(read_all(&p, p.lookup(moved.ino, "kid").unwrap().ino), b"k");
}

#[test]
fn rename_onto_self_is_a_noop() {
    // POSIX: rename(x, x) when both resolve to the same node succeeds and changes nothing.
    let p = provider();
    let ino = write_file(&p, ROOT, "f", b"data");
    p.rename(ROOT, "f", ROOT, "f")
        .expect("rename onto self is a no-op");
    assert_eq!(read_all(&p, p.lookup(ROOT, "f").unwrap().ino), b"data");
    assert_eq!(p.lookup(ROOT, "f").unwrap().ino, ino);
}

// ---- ENAMETOOLONG (a single component longer than 255 bytes) ---------------------------

// POSIX `NAME_MAX` is 255 bytes per component; a 256-byte name fails with ENAMETOOLONG.
// pjdfstest's `<op>/02.t` files check this for create/mkdir/rename. A 255-byte name must
// still succeed (the boundary).

#[test]
fn create_with_256_byte_name_is_enametoolong() {
    let p = provider();
    let long = "z".repeat(256);
    assert!(matches!(p.create(ROOT, &long), Err(FsError::NameTooLong)));
    // The boundary (255) is allowed.
    let ok = "z".repeat(255);
    p.create(ROOT, &ok).expect("255-byte name is allowed");
}

#[test]
fn mkdir_with_256_byte_name_is_enametoolong() {
    let p = provider();
    let long = "z".repeat(256);
    assert!(matches!(p.mkdir(ROOT, &long), Err(FsError::NameTooLong)));
    let ok = "z".repeat(255);
    p.mkdir(ROOT, &ok).expect("255-byte name is allowed");
}

#[test]
fn rename_to_256_byte_name_is_enametoolong() {
    let p = provider();
    write_file(&p, ROOT, "src", b"s");
    let long = "z".repeat(256);
    assert!(matches!(
        p.rename(ROOT, "src", ROOT, &long),
        Err(FsError::NameTooLong)
    ));
    // The source is untouched (the failed rename was a no-op).
    assert_eq!(read_all(&p, p.lookup(ROOT, "src").unwrap().ino), b"s");
}

#[test]
fn name_length_limit_counts_bytes_not_chars() {
    // NAME_MAX is a *byte* limit. A multibyte name whose char count is < 255 but whose
    // byte length exceeds 255 must still be rejected (matches the kernel, which works in
    // bytes). 'é' is 2 bytes in UTF-8, so 128 of them = 256 bytes > 255.
    let p = provider();
    let multibyte = "é".repeat(128); // 256 bytes, 128 chars
    assert_eq!(multibyte.len(), 256);
    assert!(matches!(
        p.create(ROOT, &multibyte),
        Err(FsError::NameTooLong)
    ));
}

// ---- chmod: full 12-bit permission round-trip ------------------------------------------

#[test]
fn chmod_preserves_all_permission_bits_including_special() {
    // pjdfstest chmod/00 sweeps the whole 07777 space: the rwx triples *and* the special
    // bits (setuid 04000, setgid 02000, sticky 01000). set_mode masks to 07777 — confirm a
    // value using every group survives a round-trip (it must not drop setuid/setgid/sticky).
    let p = provider();
    let ino = write_file(&p, ROOT, "f", b"x");
    for &mode in &[
        0o644u32, 0o600, 0o755, 0o4755, /*setuid*/
        0o2755, /*setgid*/
        0o1777, /*sticky*/
        0o7777,
    ] {
        p.set_mode(ino, mode).unwrap();
        assert_eq!(
            p.getattr(ino).unwrap().mode,
            mode,
            "mode {mode:o} round-trips"
        );
    }
}

#[test]
fn chmod_ignores_non_permission_high_bits() {
    // The kernel passes the file-type bits (S_IFMT) in the same word as the mode on some
    // paths; set_mode must keep only the permission bits (07777), never store the type bits.
    let p = provider();
    let ino = write_file(&p, ROOT, "f", b"x");
    p.set_mode(ino, 0o100644 /* S_IFREG | 0644 */).unwrap();
    assert_eq!(
        p.getattr(ino).unwrap().mode,
        0o644,
        "type bits stripped, perms kept"
    );
}

#[test]
fn chmod_on_missing_inode_is_not_found() {
    let p = provider();
    assert!(matches!(p.set_mode(999_999, 0o644), Err(FsError::NotFound)));
}

// ---- truncate target/errno -------------------------------------------------------------

#[test]
fn truncate_of_a_directory_is_eisdir() {
    // POSIX truncate()/ftruncate() on a directory → EISDIR. pjdfstest truncate/09.
    let p = provider();
    let d = p.mkdir(ROOT, "d").unwrap();
    assert!(matches!(p.truncate(d.ino, 0), Err(FsError::IsADirectory)));
    // The directory is intact.
    assert_eq!(p.getattr(d.ino).unwrap().kind, Kind::Dir);
}
