//! macOS **FSKit** adapter for `fs-provider` — the native, kext-free macOS mount, the
//! principled peer of `fs-fuse` (Linux) and `fs-cfapi` (Windows) over the **same**
//! [`Provider`]. See `docs/plan-macos-fskit-migration.md`.
//!
//! ## The architectural call: FSKit ≈ FUSE, not ≈ CfAPI
//! macOS offers two native models — **FSKit** (`FSUnaryFileSystem`/`FSVolume`: a full
//! userspace filesystem the kernel mounts, answering `lookup`/`enumerate`/`read`/`write`/
//! `create`/`remove`/`rename`) and **File Provider** (`NSFileProviderExtension`: a
//! placeholder/on-demand-sync surface). This crate targets **FSKit** — so it **mirrors
//! `fs-fuse`** (a full FS over the `Provider` with **hydrate-on-read**), NOT the CfAPI
//! placeholder state machine. The write path is *direct* callbacks
//! (`createItem`/`removeItem`/`renameItem` → `Provider`), with none of CfAPI's NOTIFY /
//! pre-op-ACK / `delete_by_node_id` bookkeeping (FSKit hands us parent+name like FUSE does).
//!
//! ## What's here vs what's stubbed
//! - **Cross-platform pure logic** (this file): [`ItemId`] (the FSKit 64-bit item
//!   identifier ↔ our `ino`), [`item_kind`] mapping, and [`FsKitChange`] (what a pulled
//!   remote change means for the kernel cache — the active-invalidation decision). Pure,
//!   `#[no_std]`-shaped, unit-tested on every platform.
//! - **The FFI bridge** ([`mod@ffi`], behind the `ffi` feature): `#[no_mangle] extern "C"`
//!   shims the Swift `.appex` calls into — `fskit_lookup`/`_read`/`_write`/`_create`/
//!   `_remove`/`_rename`/`_getattr`/`_enumerate`, each translating the FSKit item-identifier
//!   model to our `ino`/`node_id` and routing to the [`Provider`]. A leaked
//!   `Box<FsKitBridge>` is the opaque handle (the `Box::into_raw` pattern from `fs-cfapi`).
//! - **The Swift extension + entitlements + signing** ([`swift/`](../swift), `Info.plist`,
//!   the `.appex` target): **STUBBED** with `TODO(dev-account)` markers — these need the
//!   FSKit SDK + a paid Apple Developer account and will NOT build without them. See
//!   `BUILD.md` for the exact when-account-arrives checklist.
//!
//! ## Status — SKELETON (no live mount yet; Apple Developer account pending)
//! The Rust side that **can** compile **does** compile and is unit-tested (the pure logic
//! below + the `ffi` bridge typechecks under `--features ffi`). The Swift `.appex`, its
//! entitlements/`Info.plist`, signing, notarization, and system registration are the
//! account-gated remainder (`BUILD.md`). The moment the account lands, the bridge wires to
//! the Swift conformances and the `macos-fskit-smoke` CI job (the macOS analogue of
//! `windows-smoke`) exercises it against the deployed server.

use fs_provider::Kind;

#[cfg(feature = "ffi")]
pub mod ffi;

/// An FSKit **item identifier** — the 64-bit handle the kernel/Swift side uses to refer to
/// a filesystem item across callbacks. We map it **directly to our `ino`** (stable
/// per-device), exactly as `fs-fuse` uses the FUSE inode and `fs-cfapi` packs the `node_id`
/// into a placeholder identity. Cross-device identity is carried by the `node_id` in the
/// `Provider` (the `ino` is the local handle), same as the other two mounts.
///
/// FSKit reserves a couple of identifier values; we model them so the translation is total
/// and testable without any Apple types:
/// - **0** is FSKit's "invalid/none" sentinel — never a real item.
/// - **1** is the volume **root**, which we pin to [`fs_provider::ROOT_INO`] (also 1), so
///   the root maps through with no special-casing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemId(pub u64);

/// FSKit's invalid/none item identifier.
pub const ITEM_ID_INVALID: u64 = 0;
/// FSKit's root-directory item identifier (matches [`fs_provider::ROOT_INO`]).
pub const ITEM_ID_ROOT: u64 = fs_provider::ROOT_INO;

impl ItemId {
    /// Map one of our local inodes to an FSKit item identifier. `ino` and the identifier
    /// share a value space (both 64-bit, root = 1), so this is the identity — but going
    /// through this constructor keeps the mapping in one auditable place and lets us assert
    /// the contract (a real inode is never the invalid sentinel `0`).
    pub fn from_ino(ino: u64) -> Self {
        ItemId(ino)
    }

    /// Recover the local inode from an FSKit item identifier handed back by Swift. `None`
    /// for the invalid sentinel (`0`) — a malformed/foreign identifier we should reject
    /// rather than dereference into the `Provider`.
    pub fn to_ino(self) -> Option<u64> {
        if self.0 == ITEM_ID_INVALID {
            None
        } else {
            Some(self.0)
        }
    }

    /// Is this the volume root?
    pub fn is_root(self) -> bool {
        self.0 == ITEM_ID_ROOT
    }
}

/// FSKit's item-type tag (`FSItem.ItemType` in Swift). We only ever surface the kinds the
/// sync engine models; special files (FIFO/socket/device) are never created through this
/// mount (the `Provider` rejects them with `EPERM` — see `FsError::NotSupported`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemType {
    File,
    Directory,
    Symlink,
}

/// Map a `Provider` [`Kind`] to the FSKit item type the Swift side reports for an item.
pub fn item_type(kind: Kind) -> ItemType {
    match kind {
        Kind::File => ItemType::File,
        Kind::Dir => ItemType::Directory,
        Kind::Symlink => ItemType::Symlink,
    }
}

/// A thin, platform-free summary of what a pull did to one node (the FSKit-side projection
/// of the engine's `ApplyOutcome`), so the **active-invalidation** decision is pure and
/// unit-testable without a live volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteNodeEvent {
    /// The item already existed locally before this pull (an UPDATE, not an INSERT).
    pub existed: bool,
    /// The node's content (manifest) changed — only meaningful for files.
    pub content_changed: bool,
    /// The pull moved/renamed the node (its `(parent, name)` changed).
    pub moved: bool,
    /// The node was deleted/tombstoned remotely.
    pub deleted: bool,
    pub kind: Kind,
}

/// What the FSKit layer must do to the **kernel cache** after a pulled remote change. FSKit
/// — unlike WebDAV, which leans on metadata timeouts — can actively purge the kernel's
/// cache for an item (the FSKit analogue of FUSE's `notify_inval_inode`, abi-7-12). So the
/// macOS `invalidate(ino)` seam becomes a **real** call here, the same primitive slot the
/// Option-B plan reserved (`docs/plan-real-mtime-and-invalidation.md`).
///
/// This mirrors `fs-cfapi`'s `PlaceholderAction` but for the FUSE-shaped model: there are
/// no placeholders to create/dehydrate — a fresh node is simply *enumerated* lazily (the
/// kernel will `lookup` it), and a changed/removed item just needs its cache dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKitChange {
    /// File content changed under an existing item → purge the kernel **data** cache for
    /// the item, so the next read re-`lookup`s and re-hydrates the new manifest.
    InvalidateData,
    /// The item's metadata/path changed (rename/move/mtime) but not its bytes → purge the
    /// **attribute** cache so the new name/parent/mtime is observed.
    InvalidateMetadata,
    /// The item was removed → drop it from the kernel cache (the next `lookup` returns
    /// ENOENT live from the tree).
    Invalidate(InvalidateRemoved),
    /// Nothing cached to invalidate — a brand-new item (the kernel hasn't seen it; it'll be
    /// served live on the next `enumerate`/`lookup`), or a no-op metadata change on a dir.
    None,
}

/// Marker for the removed-item invalidation (keeps [`FsKitChange`] `Copy` + exhaustive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidateRemoved;

/// Decide the kernel-cache action for a pulled change — pure, so it's unit-tested without a
/// live volume. Deletes win; then a brand-new node is `None` (nothing cached yet, served
/// live on the next enumerate); then an existing file whose content changed invalidates its
/// data cache; a move/rename/metadata-only change invalidates the attribute cache.
pub fn decide_change(ev: RemoteNodeEvent) -> FsKitChange {
    if ev.deleted {
        // A delete only needs a cache drop if the kernel had actually seen the item.
        return if ev.existed {
            FsKitChange::Invalidate(InvalidateRemoved)
        } else {
            FsKitChange::None
        };
    }
    if !ev.existed {
        // Brand-new: nothing in the kernel cache to invalidate; it's served live on the
        // next `enumerate`/`lookup`.
        return FsKitChange::None;
    }
    // Existing item. Content change on a file → drop the data cache (re-hydrate next read).
    if ev.content_changed && ev.kind == Kind::File {
        return FsKitChange::InvalidateData;
    }
    // A move/rename, or a metadata-only change → drop the attribute cache so the new
    // name/parent/mtime is observed. (A dir with no change at all is a no-op.)
    if ev.moved {
        return FsKitChange::InvalidateMetadata;
    }
    // No content change, no move: e.g. an mtime/mode-only update — refresh attributes.
    FsKitChange::InvalidateMetadata
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ItemId <-> ino mapping ---

    #[test]
    fn ino_roundtrips_through_item_id() {
        for ino in [1u64, 2, 42, 1_000_000, u64::MAX] {
            let id = ItemId::from_ino(ino);
            assert_eq!(id.to_ino(), Some(ino), "ino {ino} must round-trip");
        }
    }

    #[test]
    fn invalid_item_id_maps_to_no_ino() {
        assert_eq!(ItemId(ITEM_ID_INVALID).to_ino(), None);
    }

    #[test]
    fn root_item_id_is_provider_root_ino() {
        assert_eq!(ITEM_ID_ROOT, fs_provider::ROOT_INO);
        assert!(ItemId(ITEM_ID_ROOT).is_root());
        assert!(!ItemId(2).is_root());
        // The root is a real, dereferenceable inode (not the invalid sentinel).
        assert_eq!(ItemId(ITEM_ID_ROOT).to_ino(), Some(fs_provider::ROOT_INO));
    }

    // --- Kind -> FSKit item type ---

    #[test]
    fn kind_maps_to_item_type() {
        assert_eq!(item_type(Kind::File), ItemType::File);
        assert_eq!(item_type(Kind::Dir), ItemType::Directory);
        assert_eq!(item_type(Kind::Symlink), ItemType::Symlink);
    }

    // --- decide_change: the active-invalidation decision ---

    fn ev(existed: bool, content_changed: bool, moved: bool, deleted: bool, kind: Kind) -> RemoteNodeEvent {
        RemoteNodeEvent { existed, content_changed, moved, deleted, kind }
    }

    #[test]
    fn brand_new_node_invalidates_nothing() {
        // Not yet in the kernel cache → served live on the next enumerate/lookup.
        let c = decide_change(ev(false, true, false, false, Kind::File));
        assert_eq!(c, FsKitChange::None);
    }

    #[test]
    fn deleted_existing_node_is_invalidated() {
        let c = decide_change(ev(true, false, false, true, Kind::File));
        assert_eq!(c, FsKitChange::Invalidate(InvalidateRemoved));
    }

    #[test]
    fn deleted_never_seen_node_is_noop() {
        // A delete of something the kernel never cached: nothing to drop.
        let c = decide_change(ev(false, false, false, true, Kind::File));
        assert_eq!(c, FsKitChange::None);
    }

    #[test]
    fn content_change_on_file_invalidates_data() {
        let c = decide_change(ev(true, true, false, false, Kind::File));
        assert_eq!(c, FsKitChange::InvalidateData);
    }

    #[test]
    fn content_change_flag_on_dir_does_not_invalidate_data() {
        // Dirs have no data cache; a "content changed" flag on a dir falls through to a
        // metadata refresh, never a data invalidation.
        let c = decide_change(ev(true, true, false, false, Kind::Dir));
        assert_eq!(c, FsKitChange::InvalidateMetadata);
    }

    #[test]
    fn move_invalidates_metadata() {
        let c = decide_change(ev(true, false, true, false, Kind::File));
        assert_eq!(c, FsKitChange::InvalidateMetadata);
    }

    #[test]
    fn metadata_only_change_invalidates_metadata() {
        // e.g. an mtime/mode update with no content change and no move.
        let c = decide_change(ev(true, false, false, false, Kind::File));
        assert_eq!(c, FsKitChange::InvalidateMetadata);
    }

    #[test]
    fn delete_takes_priority_over_content_and_move() {
        let c = decide_change(ev(true, true, true, true, Kind::File));
        assert_eq!(c, FsKitChange::Invalidate(InvalidateRemoved));
    }
}
