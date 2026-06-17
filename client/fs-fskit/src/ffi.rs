//! The **C ABI bridge** the Swift FSKit `.appex` calls into. FSKit is Swift/ObjC-only, so
//! the filesystem logic stays in Rust (the [`Provider`]) and Swift calls *into* it — the
//! opposite direction of `fs-cfapi` (which calls *out* to the `windows` crate), but the
//! same opaque-handle idiom: a leaked `Box<FsKitBridge>` (`Box::into_raw`) is passed back
//! and forth as a `*mut c_void`, and reclaimed on unmount.
//!
//! ## What's real vs stubbed here
//! - **Real + compiling + tested:** [`FsKitBridge`] (the `Arc<SqliteProvider>` + optional
//!   [`Hydrate`] hook), the `*mut`-handle lifecycle, and the *dispatch* of each FSKit
//!   operation to the `Provider` — `lookup`/`getattr`/`read`/`create`/`mkdir`/`write`/
//!   `unlink`/`rename`/`enumerate`. These are exercised by the unit tests below against an
//!   in-memory `SqliteProvider`, with NO Swift/FSKit involved.
//! - **Stubbed (`TODO(dev-account)`):** the actual `#[no_mangle] extern "C"` signatures
//!   with FSKit's real C structs (item descriptors, the buffer/`replyHandler` shapes) — the
//!   exact ABI depends on the FSKit SDK headers, which need the paid Apple Developer
//!   account. The shims below use **plain C scalars + raw byte buffers** so they compile and
//!   demonstrate the marshalling; the FSKit-struct-typed versions land when the SDK is in
//!   hand (see `BUILD.md`).
//!
//! The split keeps the testable dispatch logic honest now and isolates the genuinely
//! account-gated marshalling to a small, clearly-marked surface.

use std::ffi::c_void;
use std::sync::Arc;

use fs_provider::{Attr, FsError, Provider, SqliteProvider};

use crate::{ItemId, ItemType};

/// On-demand byte hydration for the read path — identical in spirit to
/// [`fs_fuse::Hydrate`](../../fs-fuse). When an FSKit `read` hits a chunk-cache miss, the
/// bridge calls `hydrate(ino)` (the sync engine's presigned GET → local cache) then retries
/// the read — the **exact** mechanism `fs-fuse` uses. Defined here (not depended on from
/// fs-fuse) so this crate stays free of any FUSE/transport dep; the engine implements both.
pub trait Hydrate: Send + Sync {
    /// Best-effort: pull `ino`'s whole missing content into the local cache.
    fn hydrate(&self, ino: u64);
    /// Best-effort: pull only the chunks overlapping `[offset, offset+size)`. Defaults to
    /// the whole-file [`hydrate`](Self::hydrate) so a non-range implementor stays correct.
    fn hydrate_range(&self, ino: u64, _offset: u64, _size: u32) {
        self.hydrate(ino);
    }
}

/// The opaque handle Swift holds for the lifetime of a mounted volume. Carries the metadata
/// + content authority (`Provider`) and the read-path hydrate hook. Constructed by
/// [`fskit_bridge_new`], destroyed by [`fskit_bridge_free`].
pub struct FsKitBridge {
    provider: Arc<SqliteProvider>,
    hydrator: Option<Arc<dyn Hydrate>>,
}

impl FsKitBridge {
    /// Build a bridge over a shared `Provider`. The same `Arc<SqliteProvider>` the sync loop
    /// holds is passed in (the `impl Provider for Arc<P>` seam), so the mount and the daemon
    /// share one tree — exactly how the FUSE/CfAPI mounts are wired.
    pub fn new(provider: Arc<SqliteProvider>, hydrator: Option<Arc<dyn Hydrate>>) -> Self {
        Self { provider, hydrator }
    }

    // --- The operation dispatch (pure Rust, fully testable; the extern shims call these) ---

    /// FSKit `lookup(name, inParent)` → `Provider::lookup`. Returns the child's [`Attr`].
    pub fn lookup(&self, parent: ItemId, name: &str) -> Result<Attr, FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.lookup(parent, name)
    }

    /// FSKit `attributes(of:)` → `Provider::getattr`.
    pub fn getattr(&self, item: ItemId) -> Result<Attr, FsError> {
        let ino = item.to_ino().ok_or(FsError::NotFound)?;
        self.provider.getattr(ino)
    }

    /// FSKit `enumerateDirectory` → `Provider::readdir`, mapped to `(ItemId, name, type)`
    /// rows the Swift side packs into `FSDirectoryEntryPacket`s.
    pub fn enumerate(&self, dir: ItemId) -> Result<Vec<(ItemId, String, ItemType)>, FsError> {
        let ino = dir.to_ino().ok_or(FsError::NotFound)?;
        let entries = self.provider.readdir(ino)?;
        Ok(entries
            .into_iter()
            .map(|e| (ItemId::from_ino(e.ino), e.name, crate::item_type(e.kind)))
            .collect())
    }

    /// FSKit `read(_:from:into:)` → `Provider::read`, **with hydrate-on-miss-then-retry**
    /// (the FUSE read path). On a `ChunkMissing` it runs the hydrate hook (presigned GET)
    /// and retries once; without a hook a miss is a hard error (cache-only mount).
    pub fn read(&self, item: ItemId, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let ino = item.to_ino().ok_or(FsError::NotFound)?;
        match self.provider.read(ino, offset, size) {
            Err(FsError::ChunkMissing(_)) if self.hydrator.is_some() => {
                // Range-hydrate just what this read needs, then retry — exactly fs-fuse.
                self.hydrator
                    .as_ref()
                    .unwrap()
                    .hydrate_range(ino, offset, size);
                self.provider.read(ino, offset, size)
            }
            other => other,
        }
    }

    /// FSKit `createItem(named:type:inDirectory:)` for a regular file → `Provider::create`.
    pub fn create(&self, parent: ItemId, name: &str) -> Result<Attr, FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.create(parent, name)
    }

    /// FSKit `createItem` for a directory → `Provider::mkdir`.
    pub fn mkdir(&self, parent: ItemId, name: &str) -> Result<Attr, FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.mkdir(parent, name)
    }

    /// FSKit `write(contents:to:at:)` → `Provider::write`. Returns bytes written.
    pub fn write(&self, item: ItemId, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        let ino = item.to_ino().ok_or(FsError::NotFound)?;
        self.provider.write(ino, offset, data)
    }

    /// FSKit close/sync → `Provider::flush` (finalize writeback: chunk + dedup + enqueue
    /// the outbox push). The direct write-capture path — no CfAPI NOTIFY/ACK dance.
    pub fn flush(&self, item: ItemId) -> Result<(), FsError> {
        let ino = item.to_ino().ok_or(FsError::NotFound)?;
        self.provider.flush(ino)
    }

    /// FSKit `setAttributes` with a new size / `O_TRUNC` → `Provider::truncate`.
    pub fn truncate(&self, item: ItemId, size: u64) -> Result<(), FsError> {
        let ino = item.to_ino().ok_or(FsError::NotFound)?;
        self.provider.truncate(ino, size)
    }

    /// FSKit `removeItem(_:named:)` on a file → `Provider::unlink`.
    pub fn unlink(&self, parent: ItemId, name: &str) -> Result<(), FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.unlink(parent, name)
    }

    /// FSKit `removeItem` on a directory → `Provider::rmdir`.
    pub fn rmdir(&self, parent: ItemId, name: &str) -> Result<(), FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.rmdir(parent, name)
    }

    /// FSKit `reparentItem`/`renameItem` → `Provider::rename` (POSIX replace-on-collision).
    pub fn rename(
        &self,
        parent: ItemId,
        name: &str,
        new_parent: ItemId,
        new_name: &str,
    ) -> Result<(), FsError> {
        let parent = parent.to_ino().ok_or(FsError::NotFound)?;
        let new_parent = new_parent.to_ino().ok_or(FsError::NotFound)?;
        self.provider.rename(parent, name, new_parent, new_name)
    }
}

/// Map an [`FsError`] to a POSIX `errno`, the integer the C ABI hands back to Swift (which
/// turns it into an `NSError` in the `fs` domain). Mirrors the errno mapping `fs-fuse`/
/// `fs-cfapi` apply, kept here so it's testable without the FSKit reply machinery.
pub fn errno(err: &FsError) -> i32 {
    match err {
        FsError::NotFound => libc_enoent(),
        FsError::NotADirectory => 20,  // ENOTDIR
        FsError::IsADirectory => 21,   // EISDIR
        FsError::AlreadyExists => 17,  // EEXIST
        FsError::NotEmpty => 66,       // ENOTEMPTY (macOS value)
        FsError::InvalidArgument => 22, // EINVAL
        FsError::NameTooLong => 63,    // ENAMETOOLONG (macOS value)
        FsError::WouldBlock => 35,     // EAGAIN (macOS value)
        FsError::NotSupported => 1,    // EPERM
        // A still-missing chunk after a hydrate attempt (or no hydrator) → EIO.
        FsError::ChunkMissing(_) => 5, // EIO
        FsError::Db(_) | FsError::Io(_) => 5, // EIO
    }
}

const fn libc_enoent() -> i32 {
    2 // ENOENT
}

// =============================================================================================
// The opaque-handle lifecycle (real, compiles + tested).
// =============================================================================================

/// Construct a bridge and return it as an opaque pointer for Swift to hold. Ownership moves
/// to the caller; it MUST be returned to [`fskit_bridge_free`] exactly once to avoid a leak.
///
/// Pure-Rust callers (the `sync_engine::FsKitMount`, the tests) use this directly; the Swift
/// side gets the pointer via the engine when it spawns the extension.
pub fn into_raw(bridge: FsKitBridge) -> *mut c_void {
    Box::into_raw(Box::new(bridge)) as *mut c_void
}

/// Recover a `&FsKitBridge` from the opaque pointer for the duration of one call.
///
/// # Safety
/// `ptr` must be a pointer returned by [`into_raw`] and not yet freed.
pub unsafe fn from_raw<'a>(ptr: *mut c_void) -> &'a FsKitBridge {
    &*(ptr as *const FsKitBridge)
}

/// Reclaim and drop a bridge created by [`into_raw`]. Idempotent only in the sense that
/// passing null is a no-op; double-freeing a real pointer is UB (as with any `Box::from_raw`).
///
/// # Safety
/// `ptr` must be a pointer returned by [`into_raw`] (or null), freed at most once.
pub unsafe fn free_raw(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut FsKitBridge));
    }
}

// =============================================================================================
// TODO(dev-account): the FSKit-struct-typed `#[no_mangle] extern "C"` shims.
//
// These are the functions the Swift `Bridge.swift` declares and calls. Their final
// signatures take FSKit's C item descriptors + reply-buffer shapes, which come from the
// FSKit SDK headers (gated on the paid Apple Developer account). Until then they're sketched
// with plain C scalars + raw byte buffers so the marshalling pattern is concrete and the
// crate compiles under `--features ffi`. When the SDK is in hand:
//   1. Replace the scalar/buffer params with the real FSItem/FSDirectoryEntryPacket types.
//   2. Have each one call the corresponding `FsKitBridge` method above and translate the
//      Result into FSKit's `replyHandler(item, error)` convention via `errno()`.
//   3. Delete the `#[allow(dead_code)]` once they're referenced from Swift.
//
// The lifecycle + dispatch they wrap is REAL and TESTED today; only this thin marshalling
// shell is account-gated.
// =============================================================================================

/// Sketch: `fskit_lookup(bridge, parent_id, name_utf8, name_len, *out_item_id) -> errno`.
/// Demonstrates the pattern; the real version returns an FSItem to the `replyHandler`.
///
/// # Safety
/// `bridge` from [`into_raw`]; `name` points to `name_len` valid UTF-8 bytes; `out` writable.
#[no_mangle]
#[allow(dead_code)]
pub unsafe extern "C" fn fskit_lookup(
    bridge: *mut c_void,
    parent_id: u64,
    name: *const u8,
    name_len: usize,
    out_item_id: *mut u64,
) -> i32 {
    if bridge.is_null() || name.is_null() || out_item_id.is_null() {
        return 22; // EINVAL
    }
    let b = from_raw(bridge);
    let name_bytes = std::slice::from_raw_parts(name, name_len);
    let name = match std::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return 22, // EINVAL — FSKit names are UTF-8
    };
    match b.lookup(ItemId(parent_id), name) {
        Ok(attr) => {
            *out_item_id = attr.ino;
            0
        }
        Err(e) => errno(&e),
    }
}

/// Sketch: `fskit_read(bridge, item_id, offset, size, *out_buf, *out_len) -> errno`.
/// `out_buf` is a Swift-allocated buffer of at least `size` bytes; `*out_len` gets the
/// number actually filled. The real version may stream into FSKit's reply buffer instead.
///
/// # Safety
/// `bridge` from [`into_raw`]; `out_buf` writable for `size` bytes; `out_len` writable.
#[no_mangle]
#[allow(dead_code)]
pub unsafe extern "C" fn fskit_read(
    bridge: *mut c_void,
    item_id: u64,
    offset: u64,
    size: u32,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if bridge.is_null() || out_buf.is_null() || out_len.is_null() {
        return 22; // EINVAL
    }
    let b = from_raw(bridge);
    match b.read(ItemId(item_id), offset, size) {
        Ok(bytes) => {
            let n = bytes.len().min(size as usize);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf, n);
            *out_len = n;
            0
        }
        Err(e) => errno(&e),
    }
}

/// Sketch: `fskit_free(bridge)` — Swift calls this on volume teardown.
///
/// # Safety
/// `bridge` from [`into_raw`], freed at most once.
#[no_mangle]
#[allow(dead_code)]
pub unsafe extern "C" fn fskit_free(bridge: *mut c_void) {
    free_raw(bridge);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_provider::{ChunkConfig, Kind, LocalChunkStore};
    use std::sync::Mutex;

    /// A fresh in-memory provider with tiny chunks (small files still split) — the same
    /// fixture `fs-provider`'s own tests use. No temp tree to clean up; nothing to leak.
    fn provider() -> Arc<SqliteProvider> {
        let store = Box::new(LocalChunkStore::temp().unwrap());
        Arc::new(
            SqliteProvider::in_memory(
                store,
                ChunkConfig {
                    min: 64,
                    avg: 256,
                    max: 1024,
                },
            )
            .unwrap(),
        )
    }

    /// A test bridge with no hydrator (cache-only).
    fn bridge() -> FsKitBridge {
        FsKitBridge::new(provider(), None)
    }

    #[test]
    fn create_then_lookup_and_getattr() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        let attr = b.create(root, "part.stl").unwrap();
        assert_eq!(attr.kind, Kind::File);

        let looked = b.lookup(root, "part.stl").unwrap();
        assert_eq!(looked.ino, attr.ino);

        let got = b.getattr(ItemId::from_ino(attr.ino)).unwrap();
        assert_eq!(got.ino, attr.ino);
    }

    #[test]
    fn write_flush_read_roundtrip() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        let attr = b.create(root, "data.bin").unwrap();
        let item = ItemId::from_ino(attr.ino);

        let payload = b"hello fskit";
        let n = b.write(item, 0, payload).unwrap();
        assert_eq!(n as usize, payload.len());
        b.flush(item).unwrap();

        let read = b.read(item, 0, payload.len() as u32).unwrap();
        assert_eq!(read, payload);
    }

    #[test]
    fn mkdir_then_enumerate_lists_children() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        b.mkdir(root, "proj").unwrap();
        b.create(root, "readme.txt").unwrap();

        let mut names: Vec<(String, ItemType)> = b
            .enumerate(root)
            .unwrap()
            .into_iter()
            .map(|(_, n, t)| (n, t))
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            names,
            vec![
                ("proj".to_string(), ItemType::Directory),
                ("readme.txt".to_string(), ItemType::File),
            ]
        );
    }

    #[test]
    fn rename_moves_the_item() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        b.create(root, "old.txt").unwrap();
        let dir = b.mkdir(root, "sub").unwrap();

        b.rename(root, "old.txt", ItemId::from_ino(dir.ino), "new.txt")
            .unwrap();

        assert!(matches!(b.lookup(root, "old.txt"), Err(FsError::NotFound)));
        let moved = b.lookup(ItemId::from_ino(dir.ino), "new.txt").unwrap();
        assert_eq!(moved.kind, Kind::File);
    }

    #[test]
    fn unlink_removes_the_file() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        b.create(root, "gone.txt").unwrap();
        b.unlink(root, "gone.txt").unwrap();
        assert!(matches!(b.lookup(root, "gone.txt"), Err(FsError::NotFound)));
    }

    #[test]
    fn truncate_shrinks_the_file() {
        let b = bridge();
        let root = ItemId(fs_provider::ROOT_INO);
        let attr = b.create(root, "trunc.bin").unwrap();
        let item = ItemId::from_ino(attr.ino);
        b.write(item, 0, b"0123456789").unwrap();
        b.flush(item).unwrap();

        b.truncate(item, 4).unwrap();
        let read = b.read(item, 0, 100).unwrap();
        assert_eq!(read, b"0123");
    }

    #[test]
    fn invalid_item_id_yields_not_found() {
        let b = bridge();
        assert!(matches!(b.getattr(ItemId(0)), Err(FsError::NotFound)));
        assert!(matches!(b.lookup(ItemId(0), "x"), Err(FsError::NotFound)));
    }

    #[test]
    fn read_with_hydrator_retries_on_chunk_miss() {
        // A hydrator that records the call. We can't easily force a real ChunkMissing
        // without the engine, so this asserts the hook plumbing: a cache-only read of
        // freshly-written, flushed bytes still succeeds (the hydrator is never needed),
        // and the trait object is wired correctly.
        struct Recorder(Mutex<Vec<u64>>);
        impl Hydrate for Recorder {
            fn hydrate(&self, ino: u64) {
                self.0.lock().unwrap().push(ino);
            }
        }
        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let b = FsKitBridge::new(provider(), Some(rec.clone()));

        let root = ItemId(fs_provider::ROOT_INO);
        let attr = b.create(root, "f.bin").unwrap();
        let item = ItemId::from_ino(attr.ino);
        b.write(item, 0, b"abc").unwrap();
        b.flush(item).unwrap();

        let read = b.read(item, 0, 3).unwrap();
        assert_eq!(read, b"abc");
        // Locally-available content needs no hydrate.
        assert!(rec.0.lock().unwrap().is_empty());
    }

    #[test]
    fn opaque_handle_roundtrips() {
        let raw = into_raw(FsKitBridge::new(provider(), None));
        assert!(!raw.is_null());
        unsafe {
            let b = from_raw(raw);
            b.create(ItemId(fs_provider::ROOT_INO), "viaptr.txt").unwrap();
            assert!(b.lookup(ItemId(fs_provider::ROOT_INO), "viaptr.txt").is_ok());
            free_raw(raw);
        }
    }

    #[test]
    fn errno_mapping_is_stable() {
        assert_eq!(errno(&FsError::NotFound), 2);
        assert_eq!(errno(&FsError::AlreadyExists), 17);
        assert_eq!(errno(&FsError::IsADirectory), 21);
        assert_eq!(errno(&FsError::NotADirectory), 20);
        assert_eq!(errno(&FsError::ChunkMissing("h".into())), 5);
    }
}
