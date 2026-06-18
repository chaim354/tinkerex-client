//! Pathological write/read offsets must not panic or overflow.
//!
//! `write(ino, offset, data)` resizes the staging buffer to `offset + data.len()`. A huge
//! or near-`u64::MAX` offset (a buggy or hostile client, or a legitimate sparse-file write)
//! must fail gracefully with an error, never panic on an integer overflow or try to
//! allocate petabytes and abort the process. A mount that panics here takes the whole
//! daemon down — a denial-of-service / availability bug.

mod common;
use common::*;

use fs_provider::Provider;

#[test]
fn write_at_near_u64_max_offset_does_not_panic() {
    let p = provider();
    let f = p.create(ROOT, "sparse").unwrap();
    // offset + len overflows usize → must be an Err, not a panic / abort.
    let r = p.write(f.ino, u64::MAX - 4, b"abcdefgh");
    assert!(
        r.is_err(),
        "expected an error for an overflowing offset, got {r:?}"
    );
    // The provider must still be usable afterward (no poisoned lock).
    let g = p.create(ROOT, "ok").unwrap();
    p.write(g.ino, 0, b"fine").unwrap();
    p.flush(g.ino).unwrap();
    assert_eq!(read_all(&p, g.ino), b"fine");
}

#[test]
fn write_at_absurd_but_in_range_offset_is_rejected_not_ooming() {
    // A 1 PiB offset is in-range for u64 arithmetic but would demand a petabyte buffer.
    // The provider must refuse rather than attempt the allocation.
    let p = provider();
    let f = p.create(ROOT, "huge").unwrap();
    let r = p.write(f.ino, 1 << 50, b"x");
    assert!(
        r.is_err(),
        "a petabyte-offset write should be rejected, not attempted (got {r:?})"
    );
}

#[test]
fn truncate_to_absurd_size_is_rejected_not_ooming() {
    let p = provider();
    let f = p.create(ROOT, "g").unwrap();
    p.write(f.ino, 0, b"seed").unwrap();
    p.flush(f.ino).unwrap();
    // Grow-truncate to 1 PiB would zero-fill a petabyte staging buffer.
    let r = p.truncate(f.ino, 1 << 50);
    assert!(
        r.is_err(),
        "petabyte truncate should be rejected, got {r:?}"
    );
    // A truncate of an OPEN file to an absurd size must also be rejected (not resize live buf).
    let g = p.create(ROOT, "open").unwrap();
    p.write(g.ino, 0, b"x").unwrap(); // staging present
    assert!(p.truncate(g.ino, 1 << 50).is_err());
    // Normal modest truncate still works.
    p.truncate(f.ino, 2).unwrap();
    assert_eq!(read_all(&p, f.ino), b"se");
}

#[test]
fn read_at_huge_offset_returns_empty_not_panic() {
    let p = provider();
    let ino = write_file(&p, ROOT, "f", b"hello");
    // Reading past EOF (even absurdly far) yields no bytes, never a panic.
    let got = p.read(ino, u64::MAX - 1, 16).unwrap();
    assert!(got.is_empty());
}
