//! In-progress writes, buffered in memory until `flush`. (Production streams to a staging
//! file; in-memory keeps the foundation simple and fully testable.)
//!
//! # Why this is a type and not a documented lock order
//!
//! The provider holds three mutexes — `conn`, this one, and `locks` — and the mount thread
//! and the sync thread share one `Arc<SqliteProvider>` by design, so all three are
//! contended concurrently. Before this type existed, "is this inode open for write?" was
//! re-derived by **fourteen inline probes** across `lib.rs`, each picking its own guard
//! scope, and they landed in **four different scoping disciplines**:
//!
//! 1. a statement temporary (`self.staging.lock().unwrap().insert(..)`) — safe;
//! 2. an explicit block — safe, deliberately;
//! 3. an `if let` whose second lock is taken *after* the block — safe only by accident of
//!    temporary-scope rules (`current_content`, `read`);
//! 4. an `if let` whose second lock is taken *inside* the block — a **deadlock**
//!    (`getattr`, which took this lock and then, still holding it, `conn`, while every
//!    caller of `purge_inode_state` holds `conn` and then takes this one).
//!
//! Disciplines 3 and 4 were syntactically near-identical and 1230 lines apart. One was
//! safe and one wedged the mount, and nothing in the source said which. A documented lock
//! order plus four scoped blocks would not have fixed that: it would have *elected* an
//! in-memory `HashMap` mutex as the OUTER lock, sanctioning holding it across SQLite work.
//!
//! So instead: **every method here acquires and releases the lock internally and returns
//! OWNED values — never a guard, never a borrow.** A caller therefore *cannot* hold this
//! lock across a tree query, which makes it a **leaf lock by construction** and the
//! inversion unrepresentable rather than merely avoided. That costs nothing, because no
//! call site actually needed a borrow: `getattr` wants a length, `current_content` and
//! `read` already copied, `flush` already removed by value, and the only two in-place
//! mutators (`write`, `truncate`) become command methods here.
//!
//! **Do not add a method that returns a `MutexGuard`, a `&Vec<u8>`, or a closure run under
//! the lock** — any of those re-opens the door this type closes.

use std::collections::HashMap;
use std::sync::Mutex;

/// Per-inode staged write buffers. See the module docs: this is a **leaf lock**.
pub(crate) struct StagingArea {
    buffers: Mutex<HashMap<u64, Vec<u8>>>,
}

/// Overwrite `[offset, offset + data.len())`, zero-filling any gap past the current end.
fn splice(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let off = offset as usize;
    if off + data.len() > buf.len() {
        buf.resize(off + data.len(), 0);
    }
    buf[off..off + data.len()].copy_from_slice(data);
}

impl StagingArea {
    pub(crate) fn new() -> Self {
        Self {
            buffers: Mutex::new(HashMap::new()),
        }
    }

    /// Staged byte length of `ino`, or `None` if it is not open for write.
    pub(crate) fn len(&self, ino: u64) -> Option<u64> {
        self.buffers
            .lock()
            .unwrap()
            .get(&ino)
            .map(|b| b.len() as u64)
    }

    /// `[offset, offset + size)` of the staged buffer, clamped to its end — `None` if the
    /// inode is not open for write (so the caller falls through to committed chunks).
    pub(crate) fn slice(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        let map = self.buffers.lock().unwrap();
        let buf = map.get(&ino)?;
        let start = (offset as usize).min(buf.len());
        let end = (start + size as usize).min(buf.len());
        Some(buf[start..end].to_vec())
    }

    /// A copy of the whole staged buffer, or `None` if not open for write.
    pub(crate) fn snapshot(&self, ino: u64) -> Option<Vec<u8>> {
        self.buffers.lock().unwrap().get(&ino).cloned()
    }

    /// Stage `bytes` for `ino`, replacing whatever was there.
    pub(crate) fn put(&self, ino: u64, bytes: Vec<u8>) {
        self.buffers.lock().unwrap().insert(ino, bytes);
    }

    /// Remove and return the staged buffer — the `flush` path. `None` means "nothing
    /// staged", which its caller treats as success, not as an error.
    pub(crate) fn take(&self, ino: u64) -> Option<Vec<u8>> {
        self.buffers.lock().unwrap().remove(&ino)
    }

    /// Splice `data` in at `offset`. Returns `false` — writing nothing — when `ino` is not
    /// open for write, which is the signal to seed and retry via [`Self::seed_and_write_at`].
    pub(crate) fn write_at(&self, ino: u64, offset: u64, data: &[u8]) -> bool {
        let mut map = self.buffers.lock().unwrap();
        match map.get_mut(&ino) {
            Some(buf) => {
                splice(buf, offset, data);
                true
            }
            None => false,
        }
    }

    /// Seed `ino` with `seed` **only if it is still absent**, then splice `data` in —
    /// both under one lock hold, so no concurrent writer can slip between them.
    ///
    /// The `or_insert` is load-bearing: the seed is necessarily computed by the caller with
    /// this lock released (it reads committed content, which takes `conn`), so another
    /// writer may have seeded in the meantime. A plain insert would clobber that writer's
    /// bytes.
    pub(crate) fn seed_and_write_at(&self, ino: u64, seed: Vec<u8>, offset: u64, data: &[u8]) {
        let mut map = self.buffers.lock().unwrap();
        let buf = map.entry(ino).or_insert(seed);
        splice(buf, offset, data);
    }

    /// Resize a live staged buffer in place, zero-filling growth. Returns `false` when the
    /// inode is not open for write — the caller's signal to fall through to the
    /// seed-and-commit paths rather than treating the truncate as done.
    pub(crate) fn resize(&self, ino: u64, size: u64) -> bool {
        let mut map = self.buffers.lock().unwrap();
        match map.get_mut(&ino) {
            Some(buf) => {
                buf.resize(size as usize, 0);
                true
            }
            None => false,
        }
    }

    /// Drop the in-progress staged write for `ino`, if any.
    pub(crate) fn discard(&self, ino: u64) {
        self.buffers.lock().unwrap().remove(&ino);
    }

    /// Drop staged bytes for a set of inodes whose tree rows have just been removed (the
    /// rowids are free for reuse, so stale bytes must not survive to shadow a future node).
    /// Takes `i64` because every caller collects rowids straight out of SQLite.
    pub(crate) fn purge_many(&self, inos: &[i64]) {
        let mut map = self.buffers.lock().unwrap();
        for &ino in inos {
            map.remove(&(ino as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn len_and_snapshot_report_absent_vs_staged() {
        let s = StagingArea::new();
        assert_eq!(s.len(1), None);
        assert_eq!(s.snapshot(1), None);
        s.put(1, b"hello".to_vec());
        assert_eq!(s.len(1), Some(5));
        assert_eq!(s.snapshot(1), Some(b"hello".to_vec()));
    }

    #[test]
    fn slice_clamps_to_the_buffer_end() {
        let s = StagingArea::new();
        s.put(1, b"0123456789".to_vec());
        assert_eq!(s.slice(1, 2, 3), Some(b"234".to_vec()));
        // Past EOF in both the offset and the length: clamped, never panicking.
        assert_eq!(s.slice(1, 8, 99), Some(b"89".to_vec()));
        assert_eq!(s.slice(1, 50, 4), Some(Vec::new()));
        // Not staged is None, not an empty read — the caller must fall through to chunks.
        assert_eq!(s.slice(2, 0, 4), None);
    }

    #[test]
    fn take_removes_and_is_none_the_second_time() {
        let s = StagingArea::new();
        s.put(1, b"x".to_vec());
        assert_eq!(s.take(1), Some(b"x".to_vec()));
        assert_eq!(s.take(1), None);
    }

    #[test]
    fn write_at_reports_absence_rather_than_creating() {
        let s = StagingArea::new();
        // The probe that tells `write` it must seed. It must NOT create the entry.
        assert!(!s.write_at(1, 0, b"abc"));
        assert_eq!(s.snapshot(1), None);

        s.put(1, b"abc".to_vec());
        assert!(s.write_at(1, 1, b"Z"));
        assert_eq!(s.snapshot(1), Some(b"aZc".to_vec()));
    }

    #[test]
    fn write_at_grows_with_zero_fill() {
        let s = StagingArea::new();
        s.put(1, b"ab".to_vec());
        assert!(s.write_at(1, 4, b"Z"));
        assert_eq!(s.snapshot(1), Some(vec![b'a', b'b', 0, 0, b'Z']));
    }

    #[test]
    fn seed_and_write_at_seeds_only_when_absent() {
        let s = StagingArea::new();
        s.seed_and_write_at(1, b"seed".to_vec(), 0, b"S");
        assert_eq!(s.snapshot(1), Some(b"Seed".to_vec()));

        // A concurrent writer already seeded: the seed must be DISCARDED, not applied over
        // that writer's bytes. (`insert` here would silently lose their write.)
        s.put(2, b"theirs".to_vec());
        s.seed_and_write_at(2, b"mine!!".to_vec(), 0, b"M");
        assert_eq!(s.snapshot(2), Some(b"Mheirs".to_vec()));
    }

    #[test]
    fn resize_distinguishes_absent_from_resized() {
        let s = StagingArea::new();
        // False is the fall-through signal truncate's later paths depend on.
        assert!(!s.resize(1, 4));
        s.put(1, b"abcdef".to_vec());
        assert!(s.resize(1, 3));
        assert_eq!(s.snapshot(1), Some(b"abc".to_vec()));
        assert!(s.resize(1, 5));
        assert_eq!(s.snapshot(1), Some(vec![b'a', b'b', b'c', 0, 0]));
    }

    #[test]
    fn discard_and_purge_many_drop_buffers() {
        let s = StagingArea::new();
        s.put(1, b"a".to_vec());
        s.discard(1);
        assert_eq!(s.snapshot(1), None);

        for i in 1..=3 {
            s.put(i, b"x".to_vec());
        }
        s.purge_many(&[1, 3]);
        assert_eq!(s.snapshot(1), None);
        assert_eq!(s.snapshot(2), Some(b"x".to_vec()));
        assert_eq!(s.snapshot(3), None);
    }
}
