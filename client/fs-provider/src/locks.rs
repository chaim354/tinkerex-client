//! Local advisory file locking — POSIX `fcntl` byte-range locks (`F_SETLK`/`F_GETLK`)
//! honored **between processes on one device**. This is the correct scope for a
//! local-first system: a lock can't span the WAN (you'd need a synchronous coordinator,
//! and you can't acquire one offline), so cross-device coordination is handled by
//! conflict detection instead. Locks are *advisory* — they only constrain callers that
//! also lock (the POSIX contract).
//!
//! Conflict rule (POSIX): two locks from *different* owners conflict when their ranges
//! overlap and at least one is exclusive (a write lock). Read locks share.

use std::collections::HashMap;
use std::sync::Mutex;

/// A held lock over `[start, end]` (inclusive; `end = u64::MAX` means "to EOF").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockRange {
    pub owner: u64, // the kernel's lock_owner (identifies the holding open/process)
    pub start: u64,
    pub end: u64,
    pub exclusive: bool,
    pub pid: u32,
}

impl LockRange {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }
}

/// Per-inode advisory lock table.
#[derive(Default)]
pub struct LockManager {
    by_ino: Mutex<HashMap<u64, Vec<LockRange>>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `F_SETLK`: try to acquire `lock`. Returns `Err(conflict)` if a different owner
    /// holds an overlapping conflicting lock (non-blocking — the caller maps this to
    /// `EAGAIN`). On success, replaces any of this owner's overlapping ranges (so
    /// re-locking / upgrading a region is idempotent).
    pub fn set(&self, ino: u64, lock: LockRange) -> Result<(), LockRange> {
        let mut map = self.by_ino.lock().unwrap();
        let ranges = map.entry(ino).or_default();
        if let Some(c) = ranges
            .iter()
            .find(|r| r.owner != lock.owner && r.overlaps(lock.start, lock.end) && (r.exclusive || lock.exclusive))
        {
            return Err(*c);
        }
        ranges.retain(|r| !(r.owner == lock.owner && r.overlaps(lock.start, lock.end)));
        ranges.push(lock);
        Ok(())
    }

    /// `F_GETLK`: the conflicting lock a `set` of `probe` would hit, or `None` if free.
    pub fn test(&self, ino: u64, probe: LockRange) -> Option<LockRange> {
        let map = self.by_ino.lock().unwrap();
        map.get(&ino).and_then(|ranges| {
            ranges
                .iter()
                .find(|r| r.owner != probe.owner && r.overlaps(probe.start, probe.end) && (r.exclusive || probe.exclusive))
                .copied()
        })
    }

    /// `F_UNLCK`: drop this owner's locks overlapping `[start, end]`.
    pub fn unlock(&self, ino: u64, owner: u64, start: u64, end: u64) {
        let mut map = self.by_ino.lock().unwrap();
        if let Some(ranges) = map.get_mut(&ino) {
            ranges.retain(|r| !(r.owner == owner && r.overlaps(start, end)));
            if ranges.is_empty() {
                map.remove(&ino);
            }
        }
    }

    /// Drop every lock an owner holds on `ino` — called when the file is closed, so a
    /// crashed/exited holder never wedges the file (POSIX releases locks on last close).
    pub fn release_owner(&self, ino: u64, owner: u64) {
        let mut map = self.by_ino.lock().unwrap();
        if let Some(ranges) = map.get_mut(&ino) {
            ranges.retain(|r| r.owner != owner);
            if ranges.is_empty() {
                map.remove(&ino);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lk(owner: u64, start: u64, end: u64, exclusive: bool) -> LockRange {
        LockRange { owner, start, end, exclusive, pid: owner as u32 }
    }

    #[test]
    fn exclusive_lock_blocks_other_owners() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, u64::MAX, true)).unwrap();
        let conflict = m.set(1, lk(20, 0, u64::MAX, true)).unwrap_err();
        assert_eq!(conflict.owner, 10, "the holder is reported as the conflict");
    }

    #[test]
    fn shared_locks_coexist_but_exclusive_does_not() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, 100, false)).unwrap();
        m.set(1, lk(20, 0, 100, false)).expect("two read locks share");
        assert!(m.set(1, lk(30, 0, 100, true)).is_err(), "a write lock conflicts with held reads");
    }

    #[test]
    fn non_overlapping_ranges_dont_conflict() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, 99, true)).unwrap();
        m.set(1, lk(20, 100, 199, true)).expect("disjoint ranges are independent");
    }

    #[test]
    fn same_owner_can_re_lock_and_upgrade() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, 100, false)).unwrap();
        m.set(1, lk(10, 0, 100, true)).expect("owner upgrades its own lock");
        // Now another owner is blocked by the (upgraded) exclusive lock.
        assert!(m.set(1, lk(20, 0, 100, false)).is_err());
    }

    #[test]
    fn unlock_frees_the_range() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, u64::MAX, true)).unwrap();
        assert!(m.set(1, lk(20, 0, u64::MAX, true)).is_err());
        m.unlock(1, 10, 0, u64::MAX);
        m.set(1, lk(20, 0, u64::MAX, true)).expect("freed after unlock");
    }

    #[test]
    fn release_owner_drops_all_its_locks_on_close() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, 50, true)).unwrap();
        m.set(1, lk(10, 100, 150, true)).unwrap();
        m.release_owner(1, 10);
        m.set(1, lk(20, 0, 200, true)).expect("all of the closed owner's locks are gone");
    }

    #[test]
    fn test_reports_conflict_without_acquiring() {
        let m = LockManager::new();
        m.set(1, lk(10, 0, u64::MAX, true)).unwrap();
        assert_eq!(m.test(1, lk(20, 0, u64::MAX, false)).map(|l| l.owner), Some(10));
        // test() must not have acquired anything for owner 20.
        m.unlock(1, 10, 0, u64::MAX);
        assert!(m.test(1, lk(20, 0, u64::MAX, true)).is_none());
    }
}
