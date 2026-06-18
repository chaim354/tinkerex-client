//! Property: an `export_tree` → `apply_remote` round-trip onto a fresh replica reproduces
//! the source tree byte-for-byte, for arbitrary file bodies and arbitrary symlink targets.
//!
//! This is the core sync invariant stated as a property (the convergence suite covers op
//! *streams*; this nails the snapshot-replication primitive over a wide input space,
//! including the tricky symlink-target bytes — empty, embedded NULs, high bytes, very long).

mod common;
use common::*;

use fs_provider::Provider;
use proptest::prelude::*;

/// A node spec the generator can emit.
#[derive(Debug, Clone)]
enum Spec {
    File(String, Vec<u8>),
    Dir(String),
    Link(String, Vec<u8>),
}

fn spec_strategy() -> impl Strategy<Value = Spec> {
    let name = "[a-zA-Z0-9_.][a-zA-Z0-9_. -]{0,30}";
    prop_oneof![
        (name, proptest::collection::vec(any::<u8>(), 0..2000)).prop_map(|(n, b)| Spec::File(n, b)),
        name.prop_map(Spec::Dir),
        // Symlink targets: arbitrary bytes incl. empty, NULs, high bytes — the link content.
        (name, proptest::collection::vec(any::<u8>(), 0..300)).prop_map(|(n, t)| Spec::Link(n, t)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn export_apply_reproduces_arbitrary_flat_tree(specs in proptest::collection::vec(spec_strategy(), 0..20)) {
        let (a, b, _dir) = shared_store_pair();

        // Build the source tree, skipping name collisions (each create must be unique).
        let mut used = std::collections::HashSet::new();
        let mut links: Vec<(String, Vec<u8>)> = Vec::new();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for s in &specs {
            let name = match s { Spec::File(n,_)|Spec::Dir(n)|Spec::Link(n,_) => n.clone() };
            if !used.insert(name.clone()) { continue; }
            match s {
                Spec::File(n, body) => {
                    let f = a.create(ROOT, n).unwrap();
                    if !body.is_empty() { a.write(f.ino, 0, body).unwrap(); }
                    a.flush(f.ino).unwrap();
                    files.push((n.clone(), body.clone()));
                }
                Spec::Dir(n) => { a.mkdir(ROOT, n).unwrap(); }
                Spec::Link(n, target) => {
                    a.symlink(ROOT, n, target).unwrap();
                    links.push((n.clone(), target.clone()));
                }
            }
        }

        // Replicate via the export→apply primitive.
        replicate_tree(&a, &b);

        // Structural + content convergence (manifest_id == byte equality for files/dirs).
        prop_assert_eq!(tree_snapshot(&a), tree_snapshot(&b));

        // File bytes: hydrate-read each on B (shared store) and compare.
        for (n, body) in &files {
            let ino = b.lookup(ROOT, n).unwrap().ino;
            prop_assert_eq!(&read_all(&b, ino), body, "file {} bytes diverged", n);
        }

        // Symlink targets: snapshot doesn't carry them (no manifest), so check readlink.
        for (n, target) in &links {
            let ino = b.lookup(ROOT, n).unwrap().ino;
            prop_assert_eq!(&b.readlink(ino).unwrap(), target, "link {} target diverged", n);
        }
    }
}

#[test]
fn empty_file_and_empty_symlink_target_roundtrip() {
    // Explicit edge anchors the property: a 0-byte file and a 0-byte link target.
    let (a, b, _dir) = shared_store_pair();
    let f = a.create(ROOT, "empty").unwrap();
    a.flush(f.ino).unwrap();
    a.symlink(ROOT, "emptylink", b"").unwrap();
    replicate_tree(&a, &b);

    let fi = b.lookup(ROOT, "empty").unwrap();
    assert_eq!(fi.size, 0);
    assert_eq!(read_all(&b, fi.ino), b"");
    let li = b.lookup(ROOT, "emptylink").unwrap();
    assert_eq!(b.readlink(li.ino).unwrap(), b"");
}
