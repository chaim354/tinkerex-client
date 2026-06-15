//! Range → chunk mapping: the `manifest_range_hashes` primitive behind progressive /
//! partial hydration. A read of just a slice of a (potentially huge) file must map to the
//! SUBSET of manifest chunks overlapping that byte range — so the hydrator fetches only
//! those, not the whole file.
//!
//! These exercise the pure mapping (no cloud / hydration), the half-open overlap math at
//! chunk boundaries, dedup, and the degenerate cases (empty range, empty manifest, past
//! EOF, whole-file). The end-to-end "fetch only-needed chunks" assertion lives in the
//! sync-engine suite (with a counting ChunkIO); here we pin the mapping it relies on.

mod common;
use common::*;

use fs_provider::{ChunkLoc, Provider};

/// The full ordered chunk layout for a top-level file (offset/length per chunk).
fn layout(p: &fs_provider::SqliteProvider, name: &str) -> Vec<ChunkLoc> {
    chunks_of(p, name)
}

/// The node_id for a top-level file by name.
fn node_id(p: &fs_provider::SqliteProvider, name: &str) -> String {
    node_named(p, name).node_id
}

/// Which manifest chunks (by *index* into the ordered layout) overlap a byte range —
/// computed independently of the provider, so the test's expectation isn't tautological.
fn expected_overlapping_indices(layout: &[ChunkLoc], offset: u64, size: u64) -> Vec<usize> {
    if size == 0 {
        return Vec::new();
    }
    let end = offset.saturating_add(size);
    layout
        .iter()
        .enumerate()
        .filter(|(_, c)| c.offset < end && offset < c.offset + c.length)
        .map(|(i, _)| i)
        .collect()
}

/// A unique (non-deduping) multi-chunk file: a pseudo-random body so FastCDC produces
/// several distinct chunks and no two are byte-identical (so dedup doesn't collapse them).
fn unique_body(len: usize) -> Vec<u8> {
    // A simple LCG — deterministic, high-entropy enough that the chunks differ.
    let mut x: u64 = 0x9E3779B97F4A7C15;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

#[test]
fn multi_chunk_file_splits_into_several_chunks() {
    // Sanity: the TINY config must actually split our test file, or the range tests below
    // are vacuous.
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    assert!(l.len() >= 3, "expected several chunks, got {}", l.len());
    // Chunks must be contiguous and cover [0, size).
    let mut at = 0u64;
    for c in &l {
        assert_eq!(c.offset, at, "chunks contiguous");
        at += c.length;
    }
    assert_eq!(at, 8192, "chunks cover the whole file");
}

#[test]
fn first_bytes_map_to_only_the_first_chunk() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    let id = node_id(&p, "f.bin");

    // Read the first 10 bytes (a header read). It must overlap only chunk 0.
    let hashes = p.manifest_range_hashes(&id, 0, 10).unwrap();
    let expected: Vec<String> = expected_overlapping_indices(&l, 0, 10)
        .into_iter()
        .map(|i| l[i].hash.clone())
        .collect();
    assert_eq!(expected.len(), 1, "first 10 bytes are inside chunk 0");
    assert_eq!(hashes, expected);
    // And it's a strict SUBSET — not the whole manifest.
    assert!(hashes.len() < l.len(), "range fetch is fewer chunks than the whole file");
}

#[test]
fn later_range_maps_to_its_overlapping_chunks_only() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    let id = node_id(&p, "f.bin");

    // Pick a range squarely inside the last chunk.
    let last = l.last().unwrap();
    let offset = last.offset + last.length / 2;
    let size = (last.length / 4).max(1);

    let hashes = p.manifest_range_hashes(&id, offset, size).unwrap();
    let expected: Vec<String> = expected_overlapping_indices(&l, offset, size)
        .into_iter()
        .map(|i| l[i].hash.clone())
        .collect();
    assert_eq!(hashes, expected);
    assert_eq!(hashes, vec![last.hash.clone()], "a slice of the last chunk → just that chunk");
}

#[test]
fn range_spanning_a_chunk_boundary_returns_both_chunks() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    let id = node_id(&p, "f.bin");

    // A range straddling the boundary between chunk 0 and chunk 1: end of 0 + start of 1.
    let boundary = l[0].length; // == chunk 1's offset
    let offset = boundary - 1;
    let size = 2; // [boundary-1, boundary+1) touches both chunk 0 and chunk 1
    let hashes = p.manifest_range_hashes(&id, offset, size).unwrap();
    assert_eq!(hashes, vec![l[0].hash.clone(), l[1].hash.clone()], "straddle → both chunks, in order");
}

#[test]
fn range_exactly_at_a_boundary_is_half_open() {
    // A read of [boundary, boundary+1): byte `boundary` belongs to chunk 1 (its first
    // byte), NOT chunk 0 (whose last byte is boundary-1). Half-open intervals.
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    let id = node_id(&p, "f.bin");

    let boundary = l[0].length;
    let hashes = p.manifest_range_hashes(&id, boundary, 1).unwrap();
    assert_eq!(hashes, vec![l[1].hash.clone()], "the boundary byte is in chunk 1 only");
}

#[test]
fn whole_file_range_returns_every_distinct_chunk() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(8192));
    let l = layout(&p, "f.bin");
    let id = node_id(&p, "f.bin");

    // offset 0, u64::MAX size — the "read whole / mmap" policy — must include all chunks.
    let hashes = p.manifest_range_hashes(&id, 0, u64::MAX).unwrap();
    // Distinct hashes in manifest order (this body is unique, so no dedup collapse).
    let all: Vec<String> = l.iter().map(|c| c.hash.clone()).collect();
    assert_eq!(hashes, all, "whole-file range == every chunk in order");

    // And it agrees with node_manifest_hashes (the pre-existing full-hydrate source).
    let full = p.node_manifest_hashes(&id).unwrap();
    assert_eq!(hashes, full);
}

#[test]
fn zero_length_range_returns_nothing() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(4096));
    let id = node_id(&p, "f.bin");
    assert!(p.manifest_range_hashes(&id, 100, 0).unwrap().is_empty());
}

#[test]
fn range_past_eof_returns_nothing() {
    let p = provider();
    write_file(&p, ROOT, "f.bin", &unique_body(4096));
    let id = node_id(&p, "f.bin");
    // Start well past EOF — no chunk overlaps.
    assert!(p.manifest_range_hashes(&id, 1_000_000, 1024).unwrap().is_empty());
}

#[test]
fn empty_and_missing_manifests_return_nothing() {
    let p = provider();
    // A freshly created (never written) file has no manifest.
    let f = p.create(ROOT, "empty.bin").unwrap();
    p.flush(f.ino).unwrap(); // flush of empty → no chunks
    let id = node_id(&p, "empty.bin");
    assert!(p.manifest_range_hashes(&id, 0, 1024).unwrap().is_empty());

    // An unknown node_id is also empty (not an error).
    assert!(p.manifest_range_hashes("no-such-node", 0, 1024).unwrap().is_empty());
}

#[test]
fn deduped_chunks_appear_once() {
    // A file whose content repeats a chunk-sized pattern dedups to one chunk hash. A range
    // spanning multiple copies must return that hash only ONCE (fetch each distinct chunk
    // at most once).
    let p = provider();
    // 4 KiB of a single repeated byte → FastCDC may still emit several chunks, but the
    // all-zero-ish repetition makes identical chunks dedup to one hash.
    let body = vec![0xABu8; 8192];
    write_file(&p, ROOT, "rep.bin", &body);
    let l = layout(&p, "rep.bin");
    let id = node_id(&p, "rep.bin");

    let distinct: std::collections::HashSet<&String> = l.iter().map(|c| &c.hash).collect();
    // Whole-file range returns each distinct hash once, regardless of how many manifest
    // entries share it.
    let hashes = p.manifest_range_hashes(&id, 0, u64::MAX).unwrap();
    assert_eq!(hashes.len(), distinct.len(), "deduped: one entry per distinct hash");
    let mut seen = std::collections::HashSet::new();
    for h in &hashes {
        assert!(seen.insert(h.clone()), "no duplicate hash in the returned list");
    }
}
