//! Windows **Cloud Filter API** (CfAPI) adapter for `fs-provider` — the driverless,
//! native-Explorer Windows mount (the model OneDrive/Dropbox use), replacing the
//! GPL-WinFsp path. See `docs/plan-windows-cfapi-migration.md`.
//!
//! ## The model inversion (why this isn't a drop-in like `fs-winfsp`)
//! WinFsp/FUSE route **every** `read`/`write`/`getattr` through our [`Provider`]. CfAPI
//! does not: the files are **real NTFS files** in a registered *sync root* folder, and the
//! OS serves hydrated reads/writes directly. We only get callbacks to (a) **fetch data**
//! for a dehydrated placeholder (hydrate), (b) **enumerate** placeholders on demand, and
//! we *signal* remote changes back with `CfUpdatePlaceholder`. So this adapter bridges the
//! [`Provider`] (the metadata + content-addressed-chunk authority) to the OS, rather than
//! serving the filesystem itself.
//!
//! ## What's here
//! - **Cross-platform pure logic** (this file): [`FileIdentity`] (the `node_id` ↔ opaque
//!   placeholder-identity blob), unix-nanos ↔ Windows `FILETIME`, and [`PlaceholderAction`]
//!   (what a pulled change means for a placeholder). Unit-tested on every platform.
//! - **Windows FFI** ([`mod@win`], `#[cfg(windows)]`): registration, the callback table,
//!   placeholder populate, FETCH_DATA hydration, and the [`win::CfApiInvalidate`] hook.
//!
//! ## Status — UNVERIFIED on Windows (authored on macOS, like `fs-winfsp` was)
//! The FFI is written against the real `windows` 0.61 CloudFilters API but has **not been
//! compiled or run** on the author's macOS host. The `cfapi-windows` CI job compiles it;
//! runtime behavior needs the manual smoke in `client/fs-cfapi/BUILD.md`. Pure logic below
//! IS tested here. Gaps are flagged inline (`LIMITATION:` / `PHASE 2:`).

use fs_provider::Kind;

#[cfg(windows)]
pub mod win;
#[cfg(windows)]
pub use win::{CfApiInvalidate, CfApiMountHandle, Hydrate};

/// A CfAPI placeholder's opaque **file identity** — the blob the OS stores per placeholder
/// and hands back on every callback. We pack our stable `node_id` (a UUID string) into it,
/// so a callback's identity round-trips to the `Provider` node. Kept as raw bytes here so
/// the encoding is testable without any Windows types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity(pub Vec<u8>);

impl FileIdentity {
    /// Pack a `node_id` into an identity blob (UTF-8 bytes — `node_id`s are ASCII UUIDs).
    pub fn from_node_id(node_id: &str) -> Self {
        FileIdentity(node_id.as_bytes().to_vec())
    }
    /// Recover the `node_id` from an identity blob the OS handed back. `None` if it isn't
    /// valid UTF-8 (a foreign/corrupt placeholder we didn't create).
    pub fn to_node_id(&self) -> Option<String> {
        std::str::from_utf8(&self.0).ok().map(str::to_owned)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Difference between the Windows FILETIME epoch (1601-01-01) and the Unix epoch
/// (1970-01-01), in **100-nanosecond ticks** (FILETIME's unit).
const FILETIME_UNIX_EPOCH_TICKS: i64 = 11_644_473_600 * 10_000_000;

/// Unix **nanoseconds** (what `Attr.mtime` carries since the real-mtime work) → a Windows
/// `FILETIME` as a `u64` of 100 ns ticks since 1601. Non-positive (unknown) → 0.
pub fn unix_nanos_to_filetime(nanos: i64) -> u64 {
    if nanos <= 0 {
        return 0;
    }
    // nanos / 100 = 100 ns ticks since the Unix epoch; add the epoch offset.
    ((nanos / 100) + FILETIME_UNIX_EPOCH_TICKS) as u64
}

/// Inverse of [`unix_nanos_to_filetime`]: a Windows `FILETIME` (100 ns ticks since 1601)
/// back to Unix **nanoseconds**. 0 (or pre-Unix-epoch) → 0.
pub fn filetime_to_unix_nanos(ticks: u64) -> i64 {
    let ticks = ticks as i64;
    if ticks <= FILETIME_UNIX_EPOCH_TICKS {
        return 0;
    }
    (ticks - FILETIME_UNIX_EPOCH_TICKS) * 100
}

/// What a pulled remote change means for the on-disk placeholder. Computed by pure logic
/// ([`decide_action`]) from what the `Provider` apply reported, then executed by the
/// Windows layer (`CfCreatePlaceholders` / `CfUpdatePlaceholder` / delete). Keeping the
/// decision here makes it unit-testable without a live sync root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderAction {
    /// Brand-new node → create a (dehydrated) placeholder.
    Create,
    /// Existing node, metadata-only change (e.g. mtime/rename) → update metadata in place.
    UpdateMetadata,
    /// Existing node whose **content** changed → update metadata **and dehydrate**, so the
    /// next read raises FETCH_DATA and pulls the new manifest. The CfAPI analogue of the
    /// FUSE `notify_inval_inode` cache-drop.
    UpdateAndDehydrate,
    /// Node removed remotely → delete the placeholder.
    Delete,
    /// Nothing to do (e.g. a dir whose content is meaningless, no metadata delta).
    None,
}

/// Inputs describing what a pull did to one node (a thin, platform-free summary of the
/// engine's `ApplyOutcome` + the changelog op), so the placeholder decision is pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteNodeEvent {
    /// The placeholder already exists on disk (we'd created it earlier).
    pub existed: bool,
    /// The node's content (manifest) changed (only meaningful for files).
    pub content_changed: bool,
    /// The node was deleted/tombstoned remotely.
    pub deleted: bool,
    pub kind: Kind,
}

/// Decide the placeholder op for a pulled change. Deletes win; then new nodes are created;
/// then existing files with new content dehydrate (cache-drop); metadata-only changes
/// update in place; a dir that already exists with no delta is a no-op.
pub fn decide_action(ev: RemoteNodeEvent) -> PlaceholderAction {
    if ev.deleted {
        // A delete only needs an op if the placeholder is actually on disk.
        return if ev.existed { PlaceholderAction::Delete } else { PlaceholderAction::None };
    }
    if !ev.existed {
        return PlaceholderAction::Create;
    }
    match ev.kind {
        Kind::File if ev.content_changed => PlaceholderAction::UpdateAndDehydrate,
        Kind::File => PlaceholderAction::UpdateMetadata,
        // Dirs carry no content; an existing dir reappearing is a no-op (metadata like
        // mtime can be refreshed, but it's not worth a placeholder write).
        Kind::Dir => PlaceholderAction::None,
    }
}

/// Strip the sync-root prefix from an absolute on-disk path, yielding a library-relative
/// forward-slash path. `None` if `abs` is not under `root`.
///
/// Tolerant matching (Windows reports the path as `VolumeDosName + NormalizedPath`, which may
/// differ in casing/form/separators from the raw user-supplied `root`):
/// - A leading `\\?\` extended-length prefix is stripped from BOTH sides before comparing
///   (so we never compare a `\\?\C:\…` against a `C:\…` and miss). We deliberately do NOT
///   `canonicalize` — on Windows that yields the `\\?\C:\…` form, which would mismatch the
///   `C:\…` callback path and make this WORSE.
/// - The root prefix is matched **case-insensitively** (ASCII) and `\`/`/` are treated as
///   equivalent separators.
/// - The remainder is returned from the ORIGINAL `abs`, separator-normalized to `/` and with
///   leading separators trimmed — the unchanged output format.
pub fn library_relative(root: &str, abs: &str) -> Option<String> {
    // Strip the `\\?\` extended-length prefix (BEFORE separator replacement — afterwards it
    // would read as `//?/`), then normalize separators to `/` for the comparison form.
    fn norm(s: &str) -> String {
        s.strip_prefix(r"\\?\").unwrap_or(s).replace('\\', "/")
    }
    let root_norm = norm(root);
    let abs_norm = norm(abs);
    // Trim a trailing separator off the root so `C:\X` and `C:\X\` behave identically.
    let root_norm = root_norm.trim_end_matches('/');

    if abs_norm.len() < root_norm.len() {
        return None;
    }
    let (prefix, rest) = abs_norm.split_at(root_norm.len());
    if !prefix.eq_ignore_ascii_case(root_norm) {
        return None;
    }
    // The char right after the prefix must be a separator (or end) — guards against a root of
    // `C:/X` spuriously matching `C:/Xtra/...`.
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(rest.trim_start_matches('/').to_string())
}

/// CfAPI requires every `CfExecute(TRANSFER_DATA)` chunk to start and end on a **4096-byte
/// page boundary**, with one exception: the final chunk of a file may end short (at EOF). A
/// transfer whose offset or non-final length isn't page-aligned is rejected by the runtime
/// (`STATUS_INVALID_PARAMETER`). [`TRANSFER_BLOCK`] is that alignment unit.
pub const TRANSFER_BLOCK: u64 = 4096;

/// One page-aligned slice of a FETCH_DATA hydration to transfer back to the kernel: read
/// `[offset, offset+length)` from the `Provider` and hand it to `CfExecute(TRANSFER_DATA)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferChunk {
    pub offset: u64,
    pub length: u64,
}

/// Plan a **streamed** FETCH_DATA response: split the kernel's required range
/// `[required_offset, required_offset + required_length)` into page-aligned [`TransferChunk`]s
/// of at most `max_chunk` bytes each, so a multi-GB file hydrates incrementally (bounded
/// memory + progress reporting) instead of buffering the whole range.
///
/// CfAPI alignment rules, enforced here so the math is unit-tested without a live sync root:
/// - Every chunk's `offset` is a multiple of [`TRANSFER_BLOCK`]. The kernel always asks on a
///   page boundary, but we defensively floor `required_offset` to the page so a stray
///   unaligned request can't produce an unaligned transfer.
/// - Every chunk's `length` is a multiple of [`TRANSFER_BLOCK`] **except** a chunk that reaches
///   `file_size` (EOF), which may be short — that's the only legal unaligned length.
/// - `max_chunk` is floored to a whole number of pages (and at least one page) so a chunk
///   boundary never lands mid-page.
///
/// Returns an empty plan when there's nothing to transfer (zero length, or the required range
/// starts at/after EOF).
pub fn plan_transfer_chunks(
    required_offset: u64,
    required_length: u64,
    file_size: u64,
    max_chunk: u64,
) -> Vec<TransferChunk> {
    if required_length == 0 || required_offset >= file_size {
        return Vec::new();
    }
    // Floor the start to a page boundary (the kernel asks page-aligned; be defensive).
    let start = (required_offset / TRANSFER_BLOCK) * TRANSFER_BLOCK;
    // The end of the required range, clamped to EOF — we never transfer past the file.
    let req_end = required_offset.saturating_add(required_length).min(file_size);
    // Per-chunk step: floor max_chunk to whole pages, at least one page.
    let step = (max_chunk / TRANSFER_BLOCK).max(1) * TRANSFER_BLOCK;
    // Round req_end UP to a page so a non-final chunk stays page-aligned; the chunk that
    // actually reaches EOF is then clamped to file_size below (its short length is the one
    // CfAPI permits).
    let req_end_ceil = req_end.div_ceil(TRANSFER_BLOCK) * TRANSFER_BLOCK;

    let mut chunks = Vec::new();
    let mut off = start;
    while off < req_end {
        let aligned_end = (off + step).min(req_end_ceil);
        // Clamp to file_size: the chunk that reaches EOF is the only one allowed a short length.
        let end = aligned_end.min(file_size);
        chunks.push(TransferChunk { offset: off, length: end - off });
        off = aligned_end;
    }
    chunks
}

/// Windows Explorer pin state for a file ("Always keep on this device" / "Free up space"),
/// mapped from `CF_PIN_STATE`. We act on the two the user drives; the rest are no-ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// "Always keep on this device" → force-hydrate now and keep bytes (don't auto-dehydrate).
    Pinned,
    /// "Free up space" → dehydrate, dropping local bytes (next read re-fetches).
    Unpinned,
    /// Unspecified / inherit / excluded — leave hydration as-is.
    Other,
}

impl PinState {
    /// Map a raw `CF_PIN_STATE` integer (1 = PINNED, 2 = UNPINNED, others = Other) to [`PinState`].
    /// Kept as `i32` so it's testable without the Windows `CF_PIN_STATE` newtype.
    pub fn from_raw(raw: i32) -> PinState {
        match raw {
            1 => PinState::Pinned,   // CF_PIN_STATE_PINNED
            2 => PinState::Unpinned, // CF_PIN_STATE_UNPINNED
            _ => PinState::Other,    // UNSPECIFIED / INHERIT / EXCLUDED
        }
    }
}

/// What a pin-state change should do to a placeholder's local bytes. Pure decision so the
/// hydrate/dehydrate side-effect is driven by unit-tested logic, not ad-hoc FFI branching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydrationAction {
    /// Force the file's bytes local now (the user pinned it).
    Hydrate,
    /// Drop the file's local bytes (the user freed up space).
    Dehydrate,
    /// Nothing to do.
    None,
}

/// Decide the hydration side-effect for a pin-state transition on a node of `kind`. Directories
/// carry no bytes, so a pinned/unpinned dir is a no-op here (CfAPI applies the pin recursively to
/// the children, which each get their own callback). Files hydrate when pinned, dehydrate when
/// unpinned; any other pin state leaves them alone.
pub fn pin_hydration_action(pin: PinState, kind: Kind) -> HydrationAction {
    if kind == Kind::Dir {
        return HydrationAction::None;
    }
    match pin {
        PinState::Pinned => HydrationAction::Hydrate,
        PinState::Unpinned => HydrationAction::Dehydrate,
        PinState::Other => HydrationAction::None,
    }
}

/// The chain of ancestor directories of `ino` that must hold a placeholder before `ino`'s own
/// placeholder can be created, listed **top-down (root-most first)**, root-exclusive. Each entry
/// is an `(ino, name)` pair (the dir's stable ino + its leaf name within its parent).
///
/// `lookup(ino)` returns `Some((parent_ino, name))` for any non-root node and `None` for the root
/// (`ROOT_INO`) or an unknown ino — exactly [`fs_provider::SqliteProvider::parent_and_name`]'s
/// contract, so the Windows path passes that through. Kept pure (no SQLite / no Windows types) so
/// the ordering + parent-resolution logic is unit-testable on any host.
///
/// Why this exists: CfAPI population is disabled per directory after its first (possibly empty)
/// enumeration, so when a remote pull inserts nodes into an already-enumerated tree we must
/// **proactively** `CfCreatePlaceholders` each one — and `CfCreatePlaceholders` requires the
/// parent directory's placeholder to already exist on disk (else `0x80070002`/`0x80070003`). This
/// yields the full root→leaf list so the caller can create each missing ancestor dir top-down
/// before the leaf, regardless of the order the changelog delivered the nodes.
///
/// Returns the ancestors **only** (not `ino` itself); the caller appends the leaf. A cycle or a
/// chain that doesn't terminate at the root within `MAX_DEPTH` hops yields `None` (corrupt tree —
/// don't loop forever).
pub fn ancestor_chain<F>(ino: u64, lookup: F) -> Option<Vec<(u64, String)>>
where
    F: Fn(u64) -> Option<(u64, String)>,
{
    const MAX_DEPTH: usize = 4096;
    // Walk up parent links, collecting (dir_ino, name) for each ancestor, until we reach the root
    // (lookup returns None). Bottom-up here; reverse to top-down before returning.
    let mut chain = Vec::new();
    let mut cur = ino;
    for _ in 0..MAX_DEPTH {
        match lookup(cur) {
            Some((parent, _name)) => {
                // `parent` is the directory that must exist for `cur`. Record the *parent's*
                // identity by looking it up in turn — but the parent's name comes from ITS own
                // lookup. So we record (cur's parent, cur's parent's name) when we step to it.
                // Simpler: record the parent dir itself on the next iteration. Here we just climb.
                if parent == fs_provider::ROOT_INO {
                    // `cur`'s parent is the root → root is the base, no more ancestors to create.
                    chain.reverse();
                    return Some(chain);
                }
                // The parent is a non-root directory that must hold a placeholder. Record it with
                // its own leaf name (from the parent's lookup).
                let parent_name = lookup(parent).map(|(_, n)| n)?;
                chain.push((parent, parent_name));
                cur = parent;
            }
            // No parent: `cur` is the root (or unknown). If `cur == ino == ROOT_INO` the caller
            // asked about the root itself (no ancestors); otherwise the chain terminated cleanly.
            None => {
                chain.reverse();
                return Some(chain);
            }
        }
    }
    None // ran past MAX_DEPTH — a cycle / corrupt tree; bail rather than loop forever
}

/// An **ino → absolute-path** index for O(1) placeholder-path resolution.
///
/// `CfApiInvalidate` (and the close/rename callbacks) need a node's on-disk path to open it for
/// `CfUpdatePlaceholder` / dehydrate / create. Walking the provider tree per lookup is O(tree).
/// Instead we populate this index incrementally **during `FETCH_PLACEHOLDERS`**: every time we
/// enumerate a directory's children we already know the dir's path and each child's name, so we
/// can record `child_ino → dir_path/child_name` for free. Later resolution is then a hash lookup.
///
/// It's a *cache*, not the source of truth, so it's correct to be lossy:
/// - A **rename** moves a node; its cached path goes stale. We expose [`invalidate`](Self::invalidate)
///   so the rename path drops the entry (and the subtree under a moved dir is re-learned on the
///   next enumeration). A caller that can't prove the cache is fresh should fall back to a tree
///   walk on a miss (the Windows layer does).
/// - A **delete** removes the node; [`invalidate`](Self::invalidate) drops it.
///
/// Pure + cross-platform (just `u64 → PathBuf`), so the indexing logic is unit-tested on any host.
#[derive(Debug, Default)]
pub struct PathIndex {
    map: std::sync::Mutex<std::collections::HashMap<u64, std::path::PathBuf>>,
}

impl PathIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a node's absolute path (learned during enumeration). Overwrites any stale entry —
    /// the most recent enumeration is authoritative for a given ino.
    pub fn insert(&self, ino: u64, path: std::path::PathBuf) {
        self.map.lock().unwrap().insert(ino, path);
    }

    /// Record every child of a just-enumerated directory: `dir_path.join(name)` per `(ino, name)`.
    /// Called from `FETCH_PLACEHOLDERS`, where we hold the dir's path and its children's names.
    pub fn insert_children<'a, I>(&self, dir_path: &std::path::Path, children: I)
    where
        I: IntoIterator<Item = (u64, &'a str)>,
    {
        let mut map = self.map.lock().unwrap();
        for (ino, name) in children {
            map.insert(ino, dir_path.join(name));
        }
    }

    /// O(1) lookup. `None` on a miss (caller falls back to a tree walk).
    pub fn get(&self, ino: u64) -> Option<std::path::PathBuf> {
        self.map.lock().unwrap().get(&ino).cloned()
    }

    /// Drop a node's cached path (after a rename/delete made it stale). Idempotent.
    pub fn invalidate(&self, ino: u64) {
        self.map.lock().unwrap().remove(&ino);
    }

    /// Drop every cached entry (e.g. a library rebind wiped the tree).
    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_relative_strips_root_and_normalizes_backslashes() {
        let root = r"C:\Users\me\MakerCF";
        let abs = r"C:\Users\me\MakerCF\proj\part.stl";
        assert_eq!(library_relative(root, abs).as_deref(), Some("proj/part.stl"));
    }

    #[test]
    fn library_relative_returns_none_when_not_under_root() {
        let root = r"C:\Users\me\MakerCF";
        let abs = r"C:\Users\me\Other\part.stl";
        assert_eq!(library_relative(root, abs), None);
    }

    #[test]
    fn library_relative_handles_trailing_slash_on_root() {
        // Root with a trailing separator and without should both yield the same relative path.
        let abs = r"C:\Users\me\MakerCF\a\b.txt";
        assert_eq!(library_relative(r"C:\Users\me\MakerCF", abs).as_deref(), Some("a/b.txt"));
        assert_eq!(library_relative(r"C:\Users\me\MakerCF\", abs).as_deref(), Some("a/b.txt"));
    }

    #[test]
    fn library_relative_file_at_root_yields_bare_leaf() {
        let root = r"C:\Users\me\MakerCF";
        let abs = r"C:\Users\me\MakerCF\hello.txt";
        assert_eq!(library_relative(root, abs).as_deref(), Some("hello.txt"));
    }

    #[test]
    fn library_relative_matches_case_insensitively() {
        // CfAPI may report a differently-cased drive/path than the user-supplied root.
        let abs = r"c:\users\ME\makercf\a.txt";
        assert_eq!(library_relative(r"C:\Users\me\MakerCF", abs).as_deref(), Some("a.txt"));
    }

    #[test]
    fn library_relative_strips_extended_length_prefix_on_abs() {
        // `\\?\C:\X\...` abs vs a plain `C:\X` root must still match.
        let abs = r"\\?\C:\Users\me\MakerCF\proj\part.stl";
        assert_eq!(
            library_relative(r"C:\Users\me\MakerCF", abs).as_deref(),
            Some("proj/part.stl")
        );
    }

    #[test]
    fn library_relative_strips_extended_length_prefix_on_root() {
        // …and the symmetric case: a `\\?\`-prefixed root vs a plain abs.
        let abs = r"C:\Users\me\MakerCF\proj\part.stl";
        assert_eq!(
            library_relative(r"\\?\C:\Users\me\MakerCF", abs).as_deref(),
            Some("proj/part.stl")
        );
    }

    #[test]
    fn library_relative_treats_mixed_separators_as_equivalent() {
        // Forward slashes on one side, backslashes on the other.
        let abs = "C:/Users/me/MakerCF/proj/part.stl";
        assert_eq!(
            library_relative(r"C:\Users\me\MakerCF", abs).as_deref(),
            Some("proj/part.stl")
        );
    }

    #[test]
    fn library_relative_none_for_sibling_with_shared_prefix() {
        // A genuinely-outside path that shares a textual prefix must NOT match.
        let abs = r"C:\Users\me\MakerCFextra\a.txt";
        assert_eq!(library_relative(r"C:\Users\me\MakerCF", abs), None);
    }

    #[test]
    fn file_identity_round_trips_a_node_id() {
        let nid = "550e8400-e29b-41d4-a716-446655440000";
        let id = FileIdentity::from_node_id(nid);
        assert_eq!(id.to_node_id().as_deref(), Some(nid));
        assert_eq!(id.as_bytes(), nid.as_bytes());
    }

    #[test]
    fn file_identity_rejects_non_utf8() {
        // A foreign placeholder we didn't create — must not panic, just decline.
        let id = FileIdentity(vec![0xff, 0xfe, 0x00]);
        assert_eq!(id.to_node_id(), None);
    }

    #[test]
    fn filetime_round_trips_real_nanos() {
        // A 2026 mtime in unix nanoseconds; FILETIME has 100 ns resolution, so a value
        // that's a whole number of 100 ns ticks round-trips exactly.
        let nanos = 1_780_000_000_000_000_000_i64; // multiple of 100
        let ft = unix_nanos_to_filetime(nanos);
        assert!(ft > FILETIME_UNIX_EPOCH_TICKS as u64, "after the 1601 epoch by >1 unix-epoch");
        assert_eq!(filetime_to_unix_nanos(ft), nanos, "round-trips at 100 ns resolution");
    }

    #[test]
    fn filetime_handles_unknown_and_epoch() {
        assert_eq!(unix_nanos_to_filetime(0), 0, "unknown mtime → 0");
        assert_eq!(unix_nanos_to_filetime(-5), 0, "negative → 0");
        assert_eq!(filetime_to_unix_nanos(0), 0, "FILETIME 0 → unknown");
    }

    #[test]
    fn filetime_matches_a_known_anchor() {
        // 2001-09-09T01:46:40Z = unix 1_000_000_000 s. In FILETIME ticks that's
        // (1_000_000_000 + 11_644_473_600) * 10_000_000.
        let nanos = 1_000_000_000_i64 * 1_000_000_000;
        let expected = (1_000_000_000_u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(unix_nanos_to_filetime(nanos), expected);
    }

    #[test]
    fn decide_action_covers_the_matrix() {
        let f = Kind::File;
        let d = Kind::Dir;
        // Deletes
        assert_eq!(decide_action(RemoteNodeEvent { existed: true, content_changed: false, deleted: true, kind: f }), PlaceholderAction::Delete);
        assert_eq!(decide_action(RemoteNodeEvent { existed: false, content_changed: false, deleted: true, kind: f }), PlaceholderAction::None);
        // New nodes
        assert_eq!(decide_action(RemoteNodeEvent { existed: false, content_changed: false, deleted: false, kind: f }), PlaceholderAction::Create);
        assert_eq!(decide_action(RemoteNodeEvent { existed: false, content_changed: false, deleted: false, kind: d }), PlaceholderAction::Create);
        // Existing file, content change → dehydrate (the cache-drop)
        assert_eq!(decide_action(RemoteNodeEvent { existed: true, content_changed: true, deleted: false, kind: f }), PlaceholderAction::UpdateAndDehydrate);
        // Existing file, metadata-only
        assert_eq!(decide_action(RemoteNodeEvent { existed: true, content_changed: false, deleted: false, kind: f }), PlaceholderAction::UpdateMetadata);
        // Existing dir → no-op
        assert_eq!(decide_action(RemoteNodeEvent { existed: true, content_changed: false, deleted: false, kind: d }), PlaceholderAction::None);
    }

    // --- B6.3: streamed FETCH_DATA chunk planning ---------------------------------------

    #[test]
    fn transfer_small_file_is_one_short_final_chunk() {
        // A 1000-byte file, whole range requested: one chunk, short (EOF) length — legal.
        let plan = plan_transfer_chunks(0, 1000, 1000, 1 << 20);
        assert_eq!(plan, vec![TransferChunk { offset: 0, length: 1000 }]);
    }

    #[test]
    fn transfer_splits_into_page_aligned_chunks_then_short_final() {
        // 10 MiB file, whole range, 4 MiB max → 4 MiB + 4 MiB + 2 MiB. All page-aligned; the
        // last reaches EOF exactly (also page-aligned here).
        let mb = 1024 * 1024;
        let plan = plan_transfer_chunks(0, 10 * mb, 10 * mb, 4 * mb);
        assert_eq!(
            plan,
            vec![
                TransferChunk { offset: 0, length: 4 * mb },
                TransferChunk { offset: 4 * mb, length: 4 * mb },
                TransferChunk { offset: 8 * mb, length: 2 * mb },
            ]
        );
    }

    #[test]
    fn transfer_every_nonfinal_chunk_is_page_aligned_with_odd_size() {
        // A file whose size is NOT a page multiple, with a max_chunk that's also not page-aligned:
        // every non-final chunk must still be page-aligned in offset AND length; only the final
        // chunk (reaching EOF) may be short.
        let file = 100_000u64; // not a multiple of 4096
        let max = 30_000u64; // floors to 7 pages = 28672
        let plan = plan_transfer_chunks(0, file, file, max);
        let step = (max / TRANSFER_BLOCK) * TRANSFER_BLOCK;
        assert!(step % TRANSFER_BLOCK == 0 && step > 0);
        let mut covered = 0u64;
        for (i, c) in plan.iter().enumerate() {
            assert_eq!(c.offset % TRANSFER_BLOCK, 0, "chunk {i} offset must be page-aligned");
            assert_eq!(c.offset, covered, "chunks must be contiguous");
            let is_final = i == plan.len() - 1;
            if !is_final {
                assert_eq!(c.length % TRANSFER_BLOCK, 0, "non-final chunk {i} length must be page-aligned");
            }
            covered += c.length;
        }
        assert_eq!(covered, file, "the plan must cover exactly the whole file");
        // The final chunk reaches EOF and is short (file isn't page-aligned).
        assert_ne!(plan.last().unwrap().length % TRANSFER_BLOCK, 0);
    }

    #[test]
    fn transfer_floors_unaligned_required_offset_to_page() {
        // A (defensive) unaligned required offset is floored to its page so the transfer offset
        // is always page-aligned.
        let plan = plan_transfer_chunks(5000, 1000, 100_000, 1 << 20);
        assert_eq!(plan[0].offset, 4096, "offset floored to the containing page");
    }

    #[test]
    fn transfer_partial_range_does_not_overrun_eof() {
        // A range that runs past EOF is clamped: total covered never exceeds file_size.
        let plan = plan_transfer_chunks(8000, 1_000_000, 10_000, 1 << 20);
        let covered_end = plan.last().map(|c| c.offset + c.length).unwrap_or(0);
        assert!(covered_end <= 10_000, "must not transfer past EOF");
        assert_eq!(covered_end, 10_000, "covers up to EOF");
    }

    #[test]
    fn transfer_empty_for_zero_length_or_past_eof() {
        assert!(plan_transfer_chunks(0, 0, 1000, 1 << 20).is_empty(), "zero length → nothing");
        assert!(plan_transfer_chunks(2000, 100, 1000, 1 << 20).is_empty(), "offset past EOF → nothing");
        assert!(plan_transfer_chunks(0, 100, 0, 1 << 20).is_empty(), "empty file → nothing");
    }

    #[test]
    fn transfer_zero_max_chunk_falls_back_to_one_page() {
        // A degenerate max_chunk of 0 must not divide-by-zero or loop forever — it floors to one
        // page step.
        let plan = plan_transfer_chunks(0, 10_000, 10_000, 0);
        assert!(!plan.is_empty());
        // 10000 / 4096 → pages 0..4096, 4096..8192, 8192..10000 (short final).
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0], TransferChunk { offset: 0, length: 4096 });
        assert_eq!(plan[2], TransferChunk { offset: 8192, length: 10_000 - 8192 });
    }

    // --- B6.1: pin/unpin → hydrate/dehydrate decision -----------------------------------

    #[test]
    fn pin_state_maps_raw_cf_pin_state() {
        assert_eq!(PinState::from_raw(1), PinState::Pinned);
        assert_eq!(PinState::from_raw(2), PinState::Unpinned);
        for other in [0, 3, 4, -1, 99] {
            assert_eq!(PinState::from_raw(other), PinState::Other, "raw {other} → Other");
        }
    }

    #[test]
    fn pin_hydration_action_files_hydrate_unpin_dehydrate() {
        assert_eq!(pin_hydration_action(PinState::Pinned, Kind::File), HydrationAction::Hydrate);
        assert_eq!(pin_hydration_action(PinState::Unpinned, Kind::File), HydrationAction::Dehydrate);
        assert_eq!(pin_hydration_action(PinState::Other, Kind::File), HydrationAction::None);
    }

    #[test]
    fn pin_hydration_action_dirs_are_always_noop() {
        // Dirs carry no bytes; CfAPI recurses the pin to children, which get their own callbacks.
        for pin in [PinState::Pinned, PinState::Unpinned, PinState::Other] {
            assert_eq!(pin_hydration_action(pin, Kind::Dir), HydrationAction::None);
        }
    }

    // --- B6.4: ino → path index ---------------------------------------------------------

    #[test]
    fn path_index_inserts_children_under_their_dir() {
        use std::path::{Path, PathBuf};
        // Use a platform-agnostic dir so `join` (which uses the host separator) is deterministic
        // on the macOS test host; on Windows the same code joins with `\`. The logic is identical.
        let idx = PathIndex::new();
        let dir = Path::new("root").join("proj");
        idx.insert_children(&dir, [(10u64, "a.stl"), (11u64, "sub")]);
        assert_eq!(idx.get(10), Some(PathBuf::from("root").join("proj").join("a.stl")));
        assert_eq!(idx.get(11), Some(PathBuf::from("root").join("proj").join("sub")));
        assert_eq!(idx.get(99), None, "unknown ino → miss");
    }

    #[test]
    fn path_index_invalidate_drops_a_stale_entry() {
        use std::path::PathBuf;
        let idx = PathIndex::new();
        idx.insert(5, PathBuf::from(r"C:\x\old.txt"));
        assert!(idx.get(5).is_some());
        idx.invalidate(5);
        assert_eq!(idx.get(5), None, "invalidated entry is gone");
        idx.invalidate(5); // idempotent
    }

    #[test]
    fn path_index_latest_enumeration_wins() {
        use std::path::PathBuf;
        // A re-enumeration after a move records the new path, overwriting the stale one.
        let idx = PathIndex::new();
        idx.insert(7, PathBuf::from(r"C:\x\here.txt"));
        idx.insert(7, PathBuf::from(r"C:\x\moved\here.txt"));
        assert_eq!(idx.get(7), Some(PathBuf::from(r"C:\x\moved\here.txt")));
    }

    #[test]
    fn path_index_clear_empties_it() {
        use std::path::PathBuf;
        let idx = PathIndex::new();
        idx.insert(1, PathBuf::from("/a"));
        idx.insert(2, PathBuf::from("/b"));
        idx.clear();
        assert_eq!(idx.get(1), None);
        assert_eq!(idx.get(2), None);
    }

    // --- ancestor_chain: proactive-placeholder ordering / parent-resolution -------------

    /// Build a `lookup` closure over a fixed tree. `tree[ino] = Some((parent, name))` for a
    /// non-root node; the root (ino 1) is absent → `None` (matches `parent_and_name`).
    fn tree_lookup<'a>(
        tree: &'a std::collections::HashMap<u64, (u64, &'static str)>,
    ) -> impl Fn(u64) -> Option<(u64, String)> + 'a {
        move |ino| tree.get(&ino).map(|(p, n)| (*p, (*n).to_string()))
    }

    #[test]
    fn ancestor_chain_root_level_file_has_no_ancestors() {
        // ino 2 is a file directly under the root (ino 1). Its parent IS the root, so there are
        // no non-root ancestor dirs to create — the chain is empty (the leaf goes in the root).
        // This is the regression: the old `created` path treated ROOT as "not enumerated" and
        // dropped the file; here root-level resolves to an empty chain cleanly.
        let mut tree = std::collections::HashMap::new();
        tree.insert(2u64, (fs_provider::ROOT_INO, "lamp.scad"));
        let chain = ancestor_chain(2, tree_lookup(&tree)).expect("resolvable");
        assert!(chain.is_empty(), "root-level file → no ancestor dirs, got {chain:?}");
    }

    #[test]
    fn ancestor_chain_nested_lists_dirs_top_down() {
        // Tree: root/Docs(3)/2026(4)/plan.txt(5). Creating the leaf (5) needs Docs then 2026
        // created first, in that (top-down) order.
        let mut tree = std::collections::HashMap::new();
        tree.insert(3u64, (fs_provider::ROOT_INO, "Docs"));
        tree.insert(4u64, (3u64, "2026"));
        tree.insert(5u64, (4u64, "plan.txt"));
        let chain = ancestor_chain(5, tree_lookup(&tree)).expect("resolvable");
        assert_eq!(
            chain,
            vec![(3u64, "Docs".to_string()), (4u64, "2026".to_string())],
            "ancestors must be root-most first (Docs before 2026)"
        );
    }

    #[test]
    fn ancestor_chain_for_a_dir_excludes_itself() {
        // Asking about dir 4 (root/Docs/2026) yields only its ancestor Docs — the leaf (4) is the
        // caller's responsibility to append.
        let mut tree = std::collections::HashMap::new();
        tree.insert(3u64, (fs_provider::ROOT_INO, "Docs"));
        tree.insert(4u64, (3u64, "2026"));
        let chain = ancestor_chain(4, tree_lookup(&tree)).expect("resolvable");
        assert_eq!(chain, vec![(3u64, "Docs".to_string())]);
    }

    #[test]
    fn ancestor_chain_root_itself_is_empty() {
        // The root (ino 1) has no parent → no ancestors.
        let tree = std::collections::HashMap::new();
        let chain = ancestor_chain(fs_provider::ROOT_INO, tree_lookup(&tree)).expect("resolvable");
        assert!(chain.is_empty());
    }

    #[test]
    fn ancestor_chain_breaks_on_a_cycle() {
        // A corrupt tree where 2's parent is 3 and 3's parent is 2 (never reaches root). Must
        // bail with None rather than loop forever.
        let mut tree = std::collections::HashMap::new();
        tree.insert(2u64, (3u64, "a"));
        tree.insert(3u64, (2u64, "b"));
        assert_eq!(ancestor_chain(2, tree_lookup(&tree)), None);
    }
}
