//! Content-addressed integrity: a corrupted chunk on disk must be DETECTED, not silently
//! assembled into a file. The store is keyed by hex SHA-256, so the bytes a `get(hash)`
//! returns can be re-hashed and checked against the key. Without that check, bitrot, a
//! torn write, or a wrong presigned download flows straight into a reconstructed file with
//! no error — exactly the "never corrupt data" failure the layer must not have.

mod common;
use common::*;

use fs_provider::{ChunkStore, FsError, LocalChunkStore, Provider};
use sha2::{Digest, Sha256};

#[test]
fn get_verified_detects_a_corrupted_chunk() {
    let dir = tempdir::Dir::new();
    let store = LocalChunkStore::new(dir.path()).unwrap();
    let data = b"the quick brown fox jumps over the lazy dog".repeat(4);
    let hash = hex::encode(Sha256::digest(&data));
    store.put(&hash, &data).unwrap();

    // Honest read verifies fine.
    assert_eq!(store.get_verified(&hash).unwrap(), data);

    // Corrupt the on-disk bytes (flip one byte) WITHOUT changing the key.
    let path = corrupt_chunk_file(dir.path(), &hash);
    assert!(path.exists());

    // Plain get still returns the corrupted bytes (the silent-corruption path).
    let corrupted = store.get(&hash).unwrap();
    assert_ne!(corrupted, data, "test setup: corruption didn't take");

    // The verifying read MUST reject it instead of handing back wrong bytes.
    let err = store.get_verified(&hash).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn read_surfaces_a_corrupted_chunk_as_an_error_not_wrong_bytes() {
    // End-to-end through the Provider: write a file, corrupt a backing chunk on disk,
    // then read — the provider must surface an error, never silently return wrong content.
    let dir = tempdir::Dir::new();
    let store = LocalChunkStore::new(dir.path()).unwrap();
    let p = fs_provider::SqliteProvider::in_memory(Box::new(store), TINY).unwrap();

    let body = b"CORRECT CONTENT ".repeat(64); // many chunks at TINY cdc params
    let ino = write_file(&p, ROOT, "f", &body);
    assert_eq!(read_all(&p, ino), body);

    // Corrupt the first chunk backing the file.
    let h = node_named(&p, "f").chunks[0].hash.clone();
    corrupt_chunk_file(dir.path(), &h);

    // A read must NOT return the wrong bytes silently. Either it errors (integrity) — the
    // correct, data-safe outcome.
    match p.read(ino, 0, body.len() as u32) {
        Err(FsError::ChunkCorrupt(_)) | Err(FsError::ChunkMissing(_)) => { /* detected */ }
        Err(other) => panic!("unexpected error variant: {other:?}"),
        Ok(got) => assert_eq!(
            got, body,
            "read silently returned CORRUPTED content — data integrity violated"
        ),
    }
}

/// Flip a byte in the on-disk chunk file for `hash` (sharded `aa/aabb..`). Returns the path.
fn corrupt_chunk_file(root: &std::path::Path, hash: &str) -> std::path::PathBuf {
    let path = root.join(&hash[0..2]).join(hash);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();
    path
}
