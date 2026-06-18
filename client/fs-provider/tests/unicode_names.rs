//! Names are opaque UTF-8 byte strings — the provider does NOT normalize Unicode.
//!
//! This is the correct, byte-preserving contract (it matches Linux; macOS's NFD
//! normalization is a mount-adapter concern, applied above the provider, not here). The
//! provider must treat the NFC and NFD encodings of the same glyph as DISTINCT names — a
//! silent merge would conflate two real files and lose data. These tests pin that contract
//! so no future "helpful" normalization sneaks in, and exercise emoji / combining marks /
//! RTL / names that are pure whitespace or the `" 2"` macOS-dup suffix.

mod common;
use common::*;

use fs_provider::Provider;

#[test]
fn nfc_and_nfd_of_the_same_glyph_are_distinct_names() {
    let p = provider();
    // "é": NFC = U+00E9 (2 bytes), NFD = "e" + U+0301 combining acute (3 bytes).
    let nfc = "caf\u{00e9}.txt";
    let nfd = "cafe\u{0301}.txt";
    assert_ne!(
        nfc.as_bytes(),
        nfd.as_bytes(),
        "test premise: distinct byte encodings"
    );

    write_file(&p, ROOT, nfc, b"NFC content");
    // The NFD form must be a SEPARATE file, not a collision with the NFC one.
    write_file(&p, ROOT, nfd, b"NFD content");

    assert_eq!(
        read_all(&p, p.lookup(ROOT, nfc).unwrap().ino),
        b"NFC content"
    );
    assert_eq!(
        read_all(&p, p.lookup(ROOT, nfd).unwrap().ino),
        b"NFD content"
    );
    assert_eq!(
        p.readdir(ROOT).unwrap().len(),
        2,
        "NFC/NFD must not be merged"
    );
}

#[test]
fn exotic_names_roundtrip_and_sync() {
    let (a, b, _dir) = shared_store_pair();
    let names = [
        "🦀-rustacean.stl",            // emoji
        "a\u{0301}\u{0302}\u{0303}.x", // stacked combining marks
        "مرحبا.txt",                   // RTL Arabic
        "   ",                         // pure spaces
        "model 2.3mf",                 // embedded space
        "design 2.stl",                // the macOS " 2" duplicate-suffix shape
        "tab\tname",                   // embedded control char (opaque bytes)
    ];
    let mut made = 0;
    for (i, n) in names.iter().enumerate() {
        // Skip if it would exceed NAME_MAX (none here do) — all should create cleanly.
        write_file(&a, ROOT, n, format!("body{i}").as_bytes());
        made += 1;
    }
    replicate_tree(&a, &b);
    assert_eq!(tree_snapshot(&a), tree_snapshot(&b));
    assert_eq!(b.readdir(ROOT).unwrap().len(), made);
    for (i, n) in names.iter().enumerate() {
        let ino = b.lookup(ROOT, n).unwrap().ino;
        assert_eq!(
            read_all(&b, ino),
            format!("body{i}").as_bytes(),
            "{n:?} diverged"
        );
    }
}

#[test]
fn rename_preserves_exact_name_bytes() {
    let p = provider();
    let from = "café\u{0301}.a"; // NFC é then a stray combining mark
    let to = "🦀 final 2.stl";
    write_file(&p, ROOT, from, b"x");
    p.rename(ROOT, from, ROOT, to).unwrap();
    assert!(p.lookup(ROOT, from).is_err());
    assert_eq!(read_all(&p, p.lookup(ROOT, to).unwrap().ino), b"x");
    // The exported name must be byte-identical to what we asked for.
    assert_eq!(node_named(&p, to).name, to);
}
