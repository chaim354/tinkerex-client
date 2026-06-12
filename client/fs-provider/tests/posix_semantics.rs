//! Filesystem-semantics edge cases — the JuiceFS/pjdfstest angle: the small POSIX-ish
//! rules a mount must honor or real apps misbehave. Notably **rename-over-existing**,
//! the atomic-save pattern (`write file.tmp` → `rename file.tmp → file`) that almost
//! every editor uses; if it errors, saves through the mount fail.

mod common;
use common::*;

use fs_provider::{FsError, Provider};

#[test]
fn create_and_mkdir_duplicates_are_already_exists() {
    let p = provider();
    p.create(ROOT, "f").unwrap();
    assert!(matches!(p.create(ROOT, "f"), Err(FsError::AlreadyExists)));
    p.mkdir(ROOT, "d").unwrap();
    assert!(matches!(p.mkdir(ROOT, "d"), Err(FsError::AlreadyExists)));
}

#[test]
fn create_in_missing_parent_is_not_found() {
    let p = provider();
    assert!(matches!(p.create(99_999, "f"), Err(FsError::NotFound)));
}

#[test]
fn create_under_a_file_is_not_a_directory() {
    let p = provider();
    let f = p.create(ROOT, "f").unwrap();
    assert!(matches!(p.create(f.ino, "x"), Err(FsError::NotADirectory)));
}

#[test]
fn lookup_and_unlink_missing_are_not_found() {
    let p = provider();
    assert!(matches!(p.lookup(ROOT, "nope"), Err(FsError::NotFound)));
    assert!(matches!(p.unlink(ROOT, "nope"), Err(FsError::NotFound)));
}

#[test]
fn rmdir_nonempty_is_not_empty_then_succeeds_when_emptied() {
    let p = provider();
    let d = p.mkdir(ROOT, "d").unwrap();
    write_file(&p, d.ino, "child", b"x");
    assert!(matches!(p.rmdir(ROOT, "d"), Err(FsError::NotEmpty)));
    p.unlink(d.ino, "child").unwrap();
    p.rmdir(ROOT, "d").unwrap();
    assert!(matches!(p.lookup(ROOT, "d"), Err(FsError::NotFound)));
}

#[test]
fn rename_missing_source_is_not_found() {
    let p = provider();
    assert!(matches!(p.rename(ROOT, "nope", ROOT, "x"), Err(FsError::NotFound)));
}

#[test]
fn rename_preserves_node_id_across_move() {
    let p = provider();
    let a = p.mkdir(ROOT, "a").unwrap();
    let b = p.mkdir(ROOT, "b").unwrap();
    let ino = write_file(&p, a.ino, "f", b"hi");
    let id_before = p.node_id_for(ino).unwrap().unwrap();
    p.rename(a.ino, "f", b.ino, "f").unwrap();
    let moved = p.lookup(b.ino, "f").unwrap();
    assert_eq!(p.node_id_for(moved.ino).unwrap().unwrap(), id_before, "id stable across move");
    assert!(matches!(p.lookup(a.ino, "f"), Err(FsError::NotFound)));
}

#[test]
fn rename_over_existing_file_replaces_it() {
    // The atomic-save pattern: write `note.txt.tmp`, then rename it over `note.txt`.
    // POSIX rename() atomically replaces the destination. The displaced node's bytes go
    // away; the surviving entry is the source's content.
    let p = provider();
    write_file(&p, ROOT, "note.txt", b"old version, longer");
    let tmp = write_file(&p, ROOT, "note.txt.tmp", b"new");
    let tmp_id = p.node_id_for(tmp).unwrap().unwrap();

    p.rename(ROOT, "note.txt.tmp", ROOT, "note.txt").expect("atomic-save rename must succeed");

    // Source name is gone; destination now holds the source's content + id.
    assert!(matches!(p.lookup(ROOT, "note.txt.tmp"), Err(FsError::NotFound)));
    let dst = p.lookup(ROOT, "note.txt").unwrap();
    assert_eq!(read_all(&p, dst.ino), b"new", "destination has the source's content");
    assert_eq!(p.node_id_for(dst.ino).unwrap().unwrap(), tmp_id, "surviving id is the source's");

    // Exactly one entry named note.txt remains (no duplicate row).
    let n = p.readdir(ROOT).unwrap().iter().filter(|e| e.name == "note.txt").count();
    assert_eq!(n, 1, "no duplicate note.txt after replace");
}

#[test]
fn recreate_after_delete_gets_a_fresh_node_id() {
    let p = provider();
    let f1 = write_file(&p, ROOT, "f", b"one");
    let id1 = p.node_id_for(f1).unwrap().unwrap();
    p.unlink(ROOT, "f").unwrap();
    let f2 = write_file(&p, ROOT, "f", b"two");
    let id2 = p.node_id_for(f2).unwrap().unwrap();
    assert_ne!(id1, id2, "a re-created path is a new node, not a resurrection");
}

#[test]
fn unicode_and_long_names_roundtrip() {
    let p = provider();
    let names = ["café ☕.lbrn2", "日本語のファイル.stl", &"x".repeat(240)];
    for (i, name) in names.iter().enumerate() {
        let body = format!("body {i}").into_bytes();
        write_file(&p, ROOT, name, &body);
        let ino = p.lookup(ROOT, name).unwrap().ino;
        assert_eq!(read_all(&p, ino), body, "name {name:?} round-trips");
    }
    assert_eq!(p.readdir(ROOT).unwrap().len(), names.len());
}

#[test]
fn deep_nesting_roundtrips() {
    let p = provider();
    let mut parent = ROOT;
    for i in 0..40 {
        parent = p.mkdir(parent, &format!("d{i}")).unwrap().ino;
    }
    let ino = write_file(&p, parent, "deep.bin", b"bottom");
    assert_eq!(read_all(&p, ino), b"bottom");
    // Walk back down from root and confirm the path resolves.
    let mut cur = ROOT;
    for i in 0..40 {
        cur = p.lookup(cur, &format!("d{i}")).unwrap().ino;
    }
    assert_eq!(p.lookup(cur, "deep.bin").unwrap().ino, ino);
}

#[test]
fn readdir_lists_all_children_with_kinds() {
    let p = provider();
    p.mkdir(ROOT, "sub").unwrap();
    write_file(&p, ROOT, "a.bin", b"a");
    write_file(&p, ROOT, "b.bin", b"b");
    let entries = p.readdir(ROOT).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries.iter().filter(|e| e.kind == Kind::Dir).count(), 1);
    assert_eq!(entries.iter().filter(|e| e.kind == Kind::File).count(), 2);
}
