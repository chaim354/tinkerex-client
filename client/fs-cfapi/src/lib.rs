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
}
