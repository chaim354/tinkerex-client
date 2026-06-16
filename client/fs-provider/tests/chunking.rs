//! Content-defined chunking + content-addressed storage properties.
//!
//! Modeled on how restic/borg/casync test their chunkers: round-trip for every size,
//! dedup of identical content, **block-delta boundary stability** (the whole point of a
//! content-defined chunker — a local edit must re-chunk only locally, not shift every
//! boundary), bounded chunk sizes, and pathological inputs (zeros / repetitive / random).
//!
//! Everything goes through the public `Provider` API; chunk layout is inspected via
//! `export_tree`, so these also pin the on-the-wire manifest shape.

mod common;
use common::*;

use fs_provider::{ChunkConfig, Provider};
use proptest::prelude::*;
use std::collections::HashSet;

// ---- round-trip --------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Write arbitrary bytes, flush, read back — byte-for-byte, for any size including 0.
    #[test]
    fn write_flush_read_roundtrips(body in proptest::collection::vec(any::<u8>(), 0..6000)) {
        let p = provider();
        let ino = write_file(&p, ROOT, "f.bin", &body);
        prop_assert_eq!(p.getattr(ino).unwrap().size, body.len() as u64);
        prop_assert_eq!(read_all(&p, ino), body);
    }

    /// A read at an arbitrary offset/length returns exactly that slice (clamped to EOF).
    #[test]
    fn partial_reads_match_slice(
        body in proptest::collection::vec(any::<u8>(), 1..6000),
        off in 0usize..6000,
        len in 0usize..2000,
    ) {
        let p = provider();
        let ino = write_file(&p, ROOT, "f.bin", &body);
        let off = off.min(body.len());
        let got = p.read(ino, off as u64, len as u32).unwrap();
        let end = (off + len).min(body.len());
        prop_assert_eq!(got, body[off..end].to_vec());
    }

    /// Sparse + overlapping writes assembled before flush reconstruct the final buffer.
    #[test]
    fn overlapping_writes_assemble(
        base in proptest::collection::vec(any::<u8>(), 0..3000),
        patch in proptest::collection::vec(any::<u8>(), 1..1000),
        at in 0usize..3000,
    ) {
        let p = provider();
        let f = p.create(ROOT, "f.bin").unwrap();
        if !base.is_empty() { p.write(f.ino, 0, &base).unwrap(); }
        p.write(f.ino, at as u64, &patch).unwrap();
        p.flush(f.ino).unwrap();

        // Expected: base, grown to fit, with patch overlaid at `at`.
        let mut want = base.clone();
        if at + patch.len() > want.len() { want.resize(at + patch.len(), 0); }
        want[at..at + patch.len()].copy_from_slice(&patch);
        prop_assert_eq!(read_all(&p, f.ino), want);
    }
}

// ---- dedup (content addressing) ----------------------------------------------------

#[test]
fn identical_content_yields_identical_manifest_and_chunks() {
    let p = provider();
    let body: Vec<u8> = (0..4000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    write_file(&p, ROOT, "a.bin", &body);
    write_file(&p, ROOT, "b.bin", &body);

    let a = node_named(&p, "a.bin");
    let b = node_named(&p, "b.bin");
    assert_eq!(
        a.manifest_id, b.manifest_id,
        "identical content → identical manifest_id"
    );
    assert_eq!(
        a.chunks.iter().map(|c| &c.hash).collect::<Vec<_>>(),
        b.chunks.iter().map(|c| &c.hash).collect::<Vec<_>>(),
        "identical content → identical ordered chunk list",
    );
    assert!(
        a.chunks.len() > 1,
        "tiny chunk config should split 4 KB into many chunks"
    );
}

#[test]
fn different_content_differs_in_at_least_one_chunk() {
    let p = provider();
    let mut body: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
    write_file(&p, ROOT, "a.bin", &body);
    body[2000] ^= 0xff; // flip one byte in the middle
    write_file(&p, ROOT, "b.bin", &body);
    assert_ne!(
        node_named(&p, "a.bin").manifest_id,
        node_named(&p, "b.bin").manifest_id
    );
}

// ---- block-delta: boundary stability under edits -----------------------------------
// The defining property of content-defined chunking: an edit perturbs only the chunks
// near it; the rolling hash *resynchronizes*, so the rest of the file keeps the same
// boundaries and therefore the same content-addressed chunks (→ only the delta uploads).

fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_add(0x9e3779b97f4a7c15);
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

#[test]
fn insert_at_front_keeps_most_chunks_shared() {
    let p = provider();
    let body = pseudo_random(60_000, 1);
    write_file(&p, ROOT, "orig.bin", &body);
    let orig: HashSet<String> = chunk_hashes(&p, "orig.bin");

    // Prepend a handful of bytes: a fixed-block chunker would shift every boundary and
    // share ~nothing; a CDC chunker resyncs within a chunk or two.
    let mut shifted = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee];
    shifted.extend_from_slice(&body);
    write_file(&p, ROOT, "shifted.bin", &shifted);
    let shifted_hashes = chunk_hashes(&p, "shifted.bin");

    let shared = orig.intersection(&shifted_hashes).count();
    assert!(
        shared * 2 >= orig.len(),
        "front-insert should keep ≥half the chunks (CDC resync); shared {shared} of {}",
        orig.len(),
    );
}

#[test]
fn append_at_end_keeps_all_but_last_chunk() {
    let p = provider();
    let body = pseudo_random(60_000, 2);
    write_file(&p, ROOT, "orig.bin", &body);
    let orig = chunk_hashes(&p, "orig.bin");
    let orig_n = orig.len();

    let mut grown = body.clone();
    grown.extend_from_slice(&pseudo_random(3000, 99));
    write_file(&p, ROOT, "grown.bin", &grown);
    let shared = orig.intersection(&chunk_hashes(&p, "grown.bin")).count();

    // Every content-determined boundary before EOF is unchanged; only the final
    // (forced-at-EOF) chunk merges with the appended bytes.
    assert!(
        shared >= orig_n - 1,
        "append should preserve all but the last chunk; shared {shared} of {orig_n}",
    );
}

#[test]
fn single_byte_middle_edit_perturbs_few_chunks() {
    let p = provider();
    let body = pseudo_random(60_000, 3);
    write_file(&p, ROOT, "orig.bin", &body);
    let orig = chunk_hashes(&p, "orig.bin");
    let orig_n = orig.len();

    let mut edited = body.clone();
    edited[30_000] ^= 0x01;
    write_file(&p, ROOT, "edited.bin", &edited);
    let changed = orig.difference(&chunk_hashes(&p, "edited.bin")).count();

    // A one-byte change should dirty only a small neighborhood of chunks, not the file.
    assert!(
        changed <= orig_n / 4 + 2,
        "one-byte edit changed {changed} of {orig_n} chunks — CDC should localize it",
    );
    assert!(
        changed >= 1,
        "the edit must change at least the chunk it lands in"
    );
}

// ---- manifest layout invariants ----------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Chunks tile the file exactly: contiguous, gap-free, covering [0, size).
    #[test]
    fn chunks_tile_the_file(body in proptest::collection::vec(any::<u8>(), 1..8000)) {
        let p = provider();
        write_file(&p, ROOT, "f.bin", &body);
        let chunks = chunks_of(&p, "f.bin");

        let mut cursor = 0u64;
        for c in &chunks {
            prop_assert_eq!(c.offset, cursor, "chunk offsets must be contiguous");
            prop_assert!(c.length > 0, "no zero-length chunks");
            cursor += c.length;
        }
        prop_assert_eq!(cursor, body.len() as u64, "chunks must cover the whole file");
    }
}

#[test]
fn chunk_sizes_respect_configured_bounds() {
    // FastCDC guarantees min ≤ size ≤ max for every chunk except the last (which may be
    // shorter). This pins that our ChunkConfig is actually plumbed into the chunker.
    let cfg = ChunkConfig {
        min: 512,
        avg: 2048,
        max: 8192,
    };
    let p = provider_with(cfg);
    let body = pseudo_random(200_000, 7);
    write_file(&p, ROOT, "f.bin", &body);
    let chunks = chunks_of(&p, "f.bin");
    assert!(
        chunks.len() > 10,
        "200 KB should be many chunks at avg 2 KB"
    );

    for (i, c) in chunks.iter().enumerate() {
        assert!(
            c.length <= cfg.max as u64,
            "chunk {i} len {} exceeds max {}",
            c.length,
            cfg.max
        );
        if i + 1 < chunks.len() {
            assert!(
                c.length >= cfg.min as u64,
                "non-final chunk {i} len {} below min {}",
                c.length,
                cfg.min
            );
        }
    }
}

// ---- pathological inputs ------------------------------------------------------------

#[test]
fn all_zeros_roundtrips_and_dedups_hard() {
    let p = provider();
    let zeros = vec![0u8; 50_000];
    let ino = write_file(&p, ROOT, "z.bin", &zeros);
    assert_eq!(read_all(&p, ino), zeros);

    // A run of identical bytes produces runs of identical chunks → heavy dedup. The
    // distinct chunk count should be far below the total chunk count.
    let chunks = chunks_of(&p, "z.bin");
    let distinct: HashSet<_> = chunks.iter().map(|c| &c.hash).collect();
    assert!(
        distinct.len() < chunks.len(),
        "all-zeros must dedup: {} distinct of {} chunks",
        distinct.len(),
        chunks.len(),
    );
}

#[test]
fn highly_repetitive_and_random_both_roundtrip() {
    let p = provider();
    let repetitive: Vec<u8> = b"makerserver"
        .iter()
        .cycle()
        .take(40_000)
        .copied()
        .collect();
    let random = pseudo_random(40_000, 42);
    let r1 = write_file(&p, ROOT, "rep.bin", &repetitive);
    let r2 = write_file(&p, ROOT, "rnd.bin", &random);
    assert_eq!(read_all(&p, r1), repetitive);
    assert_eq!(read_all(&p, r2), random);
}

#[test]
fn empty_file_has_no_chunks_and_reads_empty() {
    let p = provider();
    let ino = write_file(&p, ROOT, "empty.bin", b"");
    assert_eq!(p.getattr(ino).unwrap().size, 0);
    assert!(read_all(&p, ino).is_empty());
    assert!(
        chunks_of(&p, "empty.bin").is_empty(),
        "an empty file carries no chunks"
    );
}
