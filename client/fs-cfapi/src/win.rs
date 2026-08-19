//! Windows Cloud Filter API FFI — the live mount. `#[cfg(windows)]` only.
//!
//! Flow (Phase 1 — the download/read half):
//! 1. [`mount`] registers the folder as a **sync root** (`CfRegisterSyncRoot`) with full
//!    hydration + population policy, connects the callback table (`CfConnectSyncRoot`), and
//!    **populates every node as a dehydrated placeholder** from the local `Provider` tree
//!    (`CfCreatePlaceholders`).
//! 2. A read of a dehydrated file raises **FETCH_DATA**; [`fetch_data_callback`] resolves the
//!    placeholder's `FileIdentity` → `node_id`, runs the hydrate hook (presigned GET into the
//!    chunk cache), reads the requested range from the `Provider`, and hands the bytes back
//!    with `CfExecute(TRANSFER_DATA)`.
//! 3. A pulled remote content change calls [`CfApiInvalidate::invalidate`] →
//!    `CfUpdatePlaceholder` (+ dehydrate), so the next read re-fetches (the CfAPI analogue of
//!    the FUSE `notify_inval_inode`).
//!
//! Phase 2 (NOT here): capturing **local** edits (the OS owns the on-disk bytes) via a USN /
//! `ReadDirectoryChangesW` watcher → `Provider::ingest_bytes` → push → `CfSetInSyncState`.
//!
//! **Verified live on Windows 11** (2026-06-12): a real CfAPI sync root registers, Explorer
//! enumerates it (FETCH_PLACEHOLDERS), and files hydrate on open (FETCH_DATA → presigned GET →
//! TRANSFER_DATA) — the download half works end-to-end. Still unexercised live: remote-change
//! invalidation + teardown; local-edit capture is PHASE 2. Also typechecks for the Windows
//! target on macOS (`cargo check --target x86_64-pc-windows-gnu`) + the `cfapi-windows` CI job.
//! Gaps flagged `LIMITATION:`/`PHASE 2:`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fs_provider::{Kind, Provider, SqliteProvider, ROOT_INO};
use widestring::U16CString;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, STATUS_SUCCESS, STATUS_UNSUCCESSFUL};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::{library_relative, unix_nanos_to_filetime, FileIdentity};

/// On-demand **range** hydration hook: "make the chunks of `node_id` overlapping
/// `[offset, offset + length)` local" (presigned GET into the cache). FETCH_DATA carries the
/// kernel's required byte range, so we fetch only the chunks that range needs — a read of a huge
/// file's header pulls a handful of chunks, not gigabytes (progressive hydration). The sync
/// engine supplies it (see `sync_engine::CfApiMount`); a bare local mount passes `None` and
/// serves only already-cached chunks. A caller that wants the whole file passes `0..u64::MAX`
/// (e.g. the pin→hydrate path, which hydrates everything).
pub type Hydrate = Arc<dyn Fn(&str, u64, u64) + Send + Sync>;

/// A fixed provider id for the sync-root registration (one logical cloud provider). A real
/// multi-account build would derive this per user; one constant is fine for v1.
const PROVIDER_ID: GUID = GUID::from_u128(0x6d61_6b65_7200_0000_0000_0000_0000_0001);
const PROVIDER_NAME: &str = "Maker";
const PROVIDER_VERSION: &str = "1.0";

/// Shared state the FETCH_DATA callback reaches via the connection's callback context. Leaked
/// (`Box::into_raw`) for the connection's lifetime and reclaimed on disconnect.
struct CfState {
    provider: Arc<SqliteProvider>,
    hydrate: Option<Hydrate>,
    connection_key: Mutex<Option<CF_CONNECTION_KEY>>,
    /// The absolute sync-root path — the prefix we strip to derive a library-relative path in
    /// the new-file capture branch (Phase 2b). The close callback only gets the full on-disk
    /// path, so it needs the root to map back into the `Provider` tree.
    root: PathBuf,
    /// Echo guard: paths we just converted to placeholders ourselves. Our own
    /// `CfConvertToPlaceholder` triggers another close, which would otherwise re-enter the
    /// new-file branch; a fresh mark short-circuits that re-entry exactly once.
    ///
    /// A [`crate::SelfOps`] — the same tested type as `self_removes` below, keyed by the same
    /// [`crate::normalize_path_key`], with the same self-healing TTL
    /// ([`crate::SelfOps::ECHO_TTL`]): if the convert-triggered close never arrives (so the mark
    /// is never consumed), a later genuine edit at the same path finds it stale, drops it, and is
    /// captured normally — bounding any leak to that window instead of permanently suppressing
    /// the next real change. This was an open-coded duplicate of `SelfOps` (same key function,
    /// same TTL as a bare literal, same remove-on-hit) — two copies free to drift, and the last
    /// untested copy of the logic.
    ///
    /// **A SEPARATE `SelfOps` FROM `self_removes`, DELIBERATELY — do not share one.** They guard
    /// different callbacks that can name the SAME path: sharing would let a daemon-initiated
    /// *remove* mark be consumed by this *convert-echo* check, silently disabling the push-loop
    /// guard `self_removes` exists to be.
    self_writes: crate::SelfOps,
    /// Set the instant teardown begins (before `CfDisconnectSyncRoot`). While true, callbacks
    /// still **mirror** the change into the provider/outbox (so a local edit/delete/rename made
    /// right before unmount is not lost), but **skip the re-entrant CfAPI calls**
    /// (`CfConvertToPlaceholder` / `CfSetInSyncState` / the ACK `CfExecute`s). Issuing those while
    /// the connection is tearing down can deadlock the Cloud Filter runtime — the cause of a
    /// hang on graceful shutdown when CfAPI flushed its batched NOTIFY callbacks at disconnect.
    /// Shared (`Arc`) with the mount handle + the `CfApiInvalidate` so one flip covers EVERY
    /// re-entrant CfAPI path (callbacks AND the sync-loop pull's invalidate/create).
    shutting_down: Arc<AtomicBool>,
    /// B6.4: ino→path index, populated as `FETCH_PLACEHOLDERS` enumerates each directory (we hold
    /// the dir path + the children's names there), so `CfApiInvalidate::path_for` is O(1) on a hit
    /// instead of an O(tree) DFS. Shared (`Arc`) with the invalidator, which reads it + drops stale
    /// entries on rename/delete. A miss falls back to the tree walk, so it's always correct.
    path_index: Arc<crate::PathIndex>,
    /// B6.3: transfer keys for FETCH_DATA hydrations the kernel asked us to **cancel** (the reading
    /// app went away / the file was deleted mid-download). The streamed `fetch_data_callback`
    /// checks this between slices and stops early; `cancel_fetch_data_callback` inserts the key.
    /// A `HashSet<i64>` of `CF_CALLBACK_INFO::TransferKey`s.
    cancelled_transfers: Mutex<std::collections::HashSet<i64>>,
    /// Echo guard for placeholder deletes/renames the **daemon itself** performs while applying a
    /// remote delete/move (`CfApiInvalidate::removed`/`moved`). Those FS ops fire `NOTIFY_DELETE` /
    /// `NOTIFY_RENAME`, which would otherwise mirror the change back into the `Provider` (re-enqueue
    /// the outbox) → a push loop / self-conflict. The invalidator `mark`s the path before the op;
    /// the NOTIFY callback `take`s it and suppresses its own re-capture exactly once. Shared (`Arc`)
    /// with the `CfApiInvalidate` so one guard covers both sides.
    self_removes: Arc<crate::SelfOps>,
}

/// A live CfAPI sync root: registered, connected, populated. Drop disconnects + unregisters.
pub struct CfApiMountHandle {
    root: PathBuf,
    state: *mut CfState,
    stop: Arc<AtomicBool>,
    /// Teardown flag shared with `CfState` + the `CfApiInvalidate`. `begin_shutdown` sets it so
    /// every re-entrant CfAPI path (the NOTIFY callbacks AND a pull-triggered invalidate/create on
    /// the sync thread) bails out before issuing a call that would deadlock the closing connection.
    shutting_down: Arc<AtomicBool>,
    /// New-file watcher (see [`spawn_new_file_watcher`]). CfAPI delivers NO callback for a
    /// brand-new file created directly in the sync root (its NOTIFY_* callbacks only cover
    /// files already in the placeholder namespace — verified live: a new file fires no
    /// `NOTIFY_FILE_CLOSE`), so a `ReadDirectoryChangesW` watcher surfaces them — the same
    /// pattern OneDrive uses. The handle holds the watch dir HANDLE (closed on Drop to unblock
    /// the synchronous read) + the thread to join.
    watcher: Option<NewFileWatcher>,
    /// B6.2: set true by [`remove_library`](CfApiMountHandle::remove_library) once it has
    /// disconnected + unregistered the sync root. Drop then skips its own disconnect (the
    /// connection is already gone) so we never double-disconnect or touch a stale key.
    torn_down: bool,
}

// The raw `*mut CfState` is only ever touched behind the connection (single owner); the
// handle is moved to one thread. Mark Send so it can live in the daemon's mount slot.
unsafe impl Send for CfApiMountHandle {}

/// Owns the `ReadDirectoryChangesW` watcher thread + the handles it waits on. The read is issued
/// **overlapped**, and the loop waits on {change-event, stop-event}; [`shutdown`](Self::shutdown)
/// signals the stop event (and `CancelIoEx`'s the pending read) so the wait returns promptly and
/// the thread exits — a clean teardown that doesn't rely on closing a handle out from under a
/// blocking synchronous call (which can hang). Dropped via [`CfApiMountHandle`]'s Drop *before*
/// the `CfState` box is reclaimed, so the watcher never touches freed state.
struct NewFileWatcher {
    stop: Arc<AtomicBool>,
    dir: windows::Win32::Foundation::HANDLE,
    stop_event: windows::Win32::Foundation::HANDLE,
    thread: Option<std::thread::JoinHandle<()>>,
}

unsafe impl Send for NewFileWatcher {}

impl NewFileWatcher {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        unsafe {
            // Wake the wait (stop event) and cancel any pending overlapped read.
            let _ = windows::Win32::System::Threading::SetEvent(self.stop_event);
            let _ = windows::Win32::System::IO::CancelIoEx(self.dir, None);
        }
        if let Some(t) = self.thread.take() {
            // Bounded join: the thread should exit promptly (stop flag + cancelled read +
            // `shutting_down` makes any in-flight capture return without re-entrant CfAPI calls).
            // But never let a wedged watcher hang teardown — poll briefly, then detach. (Detach is
            // safe: with `shutting_down` set the thread does no CfAPI work and the closed handles
            // make its next wait return; it just exits on its own.)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                eprintln!("cfapi: new-file watcher didn't exit in time — detaching");
            }
        }
        unsafe {
            let _ = CloseHandle(self.dir);
            let _ = CloseHandle(self.stop_event);
        }
    }
}

fn to_wide(s: &str) -> U16CString {
    U16CString::from_str(s).expect("string has no interior NUL")
}

/// Read a node's ENTIRE content from the provider, regardless of size.
///
/// FIX (#9): `Provider::read` takes a `u32` length, so `read(ino, 0, size as u32)` **wraps** for a
/// file > 4 GiB → a short read → the content-compare in the capture paths always mismatches → a
/// full re-ingest + re-push on every close, forever. Loop over the real `u64` size in bounded
/// `u32` slices so any size is read faithfully. (Whole-file buffering itself is a known,
/// deliberately-deferred perf item — this only fixes the correctness wrap.)
fn read_full(provider: &SqliteProvider, ino: u64, size: u64) -> fs_provider::Result<Vec<u8>> {
    const SLICE: u64 = 64 * 1024 * 1024; // 64 MiB per read call, well under u32::MAX
    let mut out = Vec::with_capacity(size.min(SLICE) as usize);
    let mut off = 0u64;
    while off < size {
        let want = (size - off).min(SLICE) as u32;
        let chunk = provider.read(ino, off, want)?;
        if chunk.is_empty() {
            break; // short read / EOF — stop rather than spin
        }
        off += chunk.len() as u64;
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// FIX (#4): is `path` a **dehydrated** cloud placeholder (bytes not resident)? Checks the file's
/// attributes via `GetFileAttributesW` (a metadata-only query — it does NOT recall the data), then
/// the pure [`crate::is_placeholder_dehydrated`] predicate. The capture paths call this to SKIP
/// reading such files (a plain read would recall/re-download them under CF_HYDRATION_POLICY_FULL).
/// A gone/unreadable path returns `false` so the caller's own read handles the error.
fn is_dehydrated_placeholder(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let path_w = path_wide(Path::new(path));
    let attrs = unsafe { GetFileAttributesW(PCWSTR(path_w.as_ptr())) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    crate::is_placeholder_dehydrated(attrs)
}

/// FIX (#8): is `path` a reparse point (a CfAPI placeholder carries the cloud reparse tag; a plain
/// file does not)? Metadata-only via `GetFileAttributesW` (no recall). The watcher uses this to
/// tell a genuine plain new file (atomic-save rename target → capture) from a placeholder rename
/// (owned by NOTIFY_RENAME → do NOT double-capture). Gone/unreadable → `false`.
fn is_reparse_point_path(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let path_w = path_wide(Path::new(path));
    let attrs = unsafe { GetFileAttributesW(PCWSTR(path_w.as_ptr())) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    crate::is_reparse_point(attrs)
}

/// Read a file's bytes **without recalling** a dehydrated placeholder (FIX #4). Opens with
/// `FILE_FLAG_OPEN_NO_RECALL` so the read never triggers CfAPI hydration — belt-and-suspenders on
/// top of the [`is_dehydrated_placeholder`] skip (guards the TOCTOU window where a file dehydrates
/// between the attribute check and the read). If the bytes aren't resident the `ReadFile` errors,
/// which the callers treat as "nothing to capture".
unsafe fn read_no_recall(path: &str) -> std::io::Result<Vec<u8>> {
    use windows::Win32::Storage::FileSystem::{ReadFile, FILE_FLAG_OPEN_NO_RECALL};
    let path_w = path_wide(Path::new(path));
    let handle = CreateFileW(
        PCWSTR(path_w.as_ptr()),
        FILE_GENERIC_READ.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_NO_RECALL,
        None,
    )
    .map_err(win_err)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB read buffer
    loop {
        let mut read: u32 = 0;
        if let Err(e) = ReadFile(handle, Some(&mut buf), Some(&mut read), None) {
            let _ = CloseHandle(handle);
            return Err(std::io::Error::other(format!("cfapi: read_no_recall {e}")));
        }
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
    }
    let _ = CloseHandle(handle);
    Ok(out)
}

fn path_wide(p: &Path) -> U16CString {
    U16CString::from_os_str(p.as_os_str()).expect("path has no interior NUL")
}

/// Register + connect + populate. Returns a handle that tears everything down on drop.
pub fn mount(
    provider: Arc<SqliteProvider>,
    root: &Path,
    hydrate: Option<Hydrate>,
) -> std::io::Result<CfApiMountHandle> {
    mount_with_shutdown(provider, root, hydrate, Arc::new(AtomicBool::new(false)))
}

/// [`mount`] but with a caller-supplied teardown flag — pass the SAME `Arc` to the engine's
/// `CfApiInvalidate` so a pull-triggered invalidate/create during unmount also bails out of its
/// re-entrant CfAPI calls. The plain `mount` makes its own (fine for a mount with no invalidator).
pub fn mount_with_shutdown(
    provider: Arc<SqliteProvider>,
    root: &Path,
    hydrate: Option<Hydrate>,
    shutting_down: Arc<AtomicBool>,
) -> std::io::Result<CfApiMountHandle> {
    mount_full(
        provider,
        root,
        hydrate,
        shutting_down,
        Arc::new(crate::PathIndex::new()),
        Arc::new(crate::SelfOps::new()),
    )
}

/// [`mount_with_shutdown`] plus a caller-supplied [`PathIndex`] (B6.4) + [`SelfOps`](crate::SelfOps)
/// echo guard. Pass the SAME `Arc`s to the engine's `CfApiInvalidate` so the index populated by
/// `FETCH_PLACEHOLDERS` here is the one the invalidator reads for O(1) path resolution, and the
/// echo guard the invalidator `mark`s on a daemon-initiated delete/move is the one this mount's
/// NOTIFY callbacks check. The engine wires this; the simpler entry points make their own
/// (harmless — the invalidator they don't have wouldn't share it anyway).
pub fn mount_full(
    provider: Arc<SqliteProvider>,
    root: &Path,
    hydrate: Option<Hydrate>,
    shutting_down: Arc<AtomicBool>,
    path_index: Arc<crate::PathIndex>,
    self_removes: Arc<crate::SelfOps>,
) -> std::io::Result<CfApiMountHandle> {
    std::fs::create_dir_all(root)?;
    register_sync_root(root)?;

    // FIX (#6): after a remount the placeholders persist on disk but the in-memory index is empty,
    // so `removed`/`moved` (which resolve the OLD on-disk path from the index) would be inert and a
    // remote delete/move observed after remount would leave a ghost. Seed the index from the
    // provider tree ∩ what's materialized on disk before wiring callbacks.
    seed_path_index(&provider, root, &path_index);

    let state = Box::into_raw(Box::new(CfState {
        provider: provider.clone(),
        hydrate,
        connection_key: Mutex::new(None),
        root: root.to_path_buf(),
        self_writes: crate::SelfOps::new(),
        shutting_down: shutting_down.clone(),
        path_index,
        cancelled_transfers: Mutex::new(std::collections::HashSet::new()),
        self_removes,
    }));

    // The callback table must outlive the connection, so leak it (one small leak per mount).
    // Terminated by a CF_CALLBACK_TYPE_NONE entry. On-demand population: the platform raises
    // FETCH_PLACEHOLDERS when Explorer opens a directory, and FETCH_DATA when a file is read.
    let callbacks: &'static [CF_CALLBACK_REGISTRATION] = Box::leak(Box::new([
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(fetch_placeholders_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(fetch_data_callback),
        },
        // B6.3: the kernel cancels an in-flight hydration (reading app gone / file deleted) — mark
        // the transfer key so the streaming FETCH_DATA loop stops between slices.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(cancel_fetch_data_callback),
        },
        // Phase 2: a file handle closed — capture a local edit (content changed) and push it.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            Callback: Some(notify_file_close_callback),
        },
        // Phase 2b: a delete/rename is about to happen — mirror it into the Provider, then ACK
        // (pre-op variants, so we observe + allow the OS op). An unanswered pre-op hangs Explorer.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(notify_delete_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(notify_rename_callback),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ]));

    let root_w = path_wide(root);
    unsafe {
        let key = match CfConnectSyncRoot(
            PCWSTR(root_w.as_ptr()),
            callbacks.as_ptr(),
            Some(state as *const c_void),
            CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        ) {
            Ok(k) => k,
            // MINOR-fix: reclaim the leaked `CfState` box on a connect failure (no connection ever
            // owns it, so nothing else would). The `callbacks` slice is deliberately 'static-leaked.
            Err(e) => {
                drop(Box::from_raw(state));
                return Err(win_err(e));
            }
        };
        (*state).connection_key.lock().unwrap().replace(key);
    }
    eprintln!("cfapi: connected sync root at {}", root.display());

    // Start the new-file watcher (CfAPI has no "file created" callback — see NewFileWatcher).
    // Best-effort: if it can't open the dir handle, log + carry on (new files just won't be
    // captured, but the rest of the write path still works).
    let watcher = match spawn_new_file_watcher(root, state) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!(
                "cfapi: new-file watcher failed to start ({e}) — new files won't be captured"
            );
            None
        }
    };

    Ok(CfApiMountHandle {
        root: root.to_path_buf(),
        state,
        stop: Arc::new(AtomicBool::new(false)),
        shutting_down,
        watcher,
        torn_down: false,
    })
}

/// Spawn a `ReadDirectoryChangesW` watcher over the sync root that feeds newly-created /
/// externally-modified files into [`capture_new_file`]. This fills CfAPI's gap: a brand-new
/// file dropped into the sync root raises **no** CfAPI callback (its NOTIFY_* set only covers
/// placeholders already in the namespace), so without this the file never reaches the
/// Provider/outbox. `capture_new_file` is idempotent (it routes an existing node to an edit and
/// skips the convert), so a duplicate ADDED+MODIFIED for the same file — or an overlap with the
/// close callback — is harmless.
fn spawn_new_file_watcher(root: &Path, state: *mut CfState) -> std::io::Result<NewFileWatcher> {
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    };
    use windows::Win32::System::Threading::CreateEventW;

    let root_w = path_wide(root);
    // Open the directory for **overlapped** change notifications (BACKUP_SEMANTICS gets a dir
    // handle; OVERLAPPED lets the read complete asynchronously so the loop can also wait on a
    // stop event; share everything so we never block other openers).
    let dir = unsafe {
        CreateFileW(
            PCWSTR(root_w.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(win_err)?;

    // Manual-reset stop event: `shutdown` signals it to break the wait promptly.
    let stop_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| {
        unsafe {
            let _ = CloseHandle(dir);
        }
        win_err(e)
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    // B6.1: also watch ATTRIBUTES — Explorer's "Always keep on this device" / "Free up space"
    // toggles the file's pinned/unpinned attribute, surfacing here as a FILE_ACTION_MODIFIED so we
    // can drive hydrate/dehydrate (CfAPI has no dedicated pin-change callback).
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_ATTRIBUTES;
    let state_addr = state as usize; // make the raw ptr Send for the thread
    let dir_addr = dir.0 as usize;
    let stop_event_addr = stop_event.0 as usize;
    let thread = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            new_file_watch_loop(dir_addr, stop_event_addr, state_addr, filter, stop);
        })
    };
    eprintln!("cfapi: new-file watcher started on {}", root.display());
    Ok(NewFileWatcher {
        stop,
        dir,
        stop_event,
        thread: Some(thread),
    })
}

/// FIX (#7): a full reconcile of the sync root against the provider, run when the watcher's change
/// buffer OVERFLOWS (the kernel drops events when more changes arrive than the buffer holds — e.g.
/// unzipping a large archive). Walks the on-disk tree and routes every file through
/// `capture_new_file` (idempotent, dehydration-guarded, content-compared), so a new file or edit
/// whose notification was lost still reaches the provider/outbox. Best-effort.
unsafe fn rescan_sync_root(state: *const CfState) {
    if state.is_null() {
        return;
    }
    let st = &*state;
    eprintln!("cfapi: watcher buffer overflow — rescanning sync root to recover dropped events");
    let mut stack = vec![st.root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else {
                capture_new_file(st, &p.to_string_lossy());
            }
        }
    }
}

/// The watcher thread body: issue an **overlapped** `ReadDirectoryChangesW`, then wait on
/// {change-event, stop-event}. On a change, parse the `FILE_NOTIFY_INFORMATION` chain and route
/// ADDED / MODIFIED / RENAMED_NEW_NAME paths through `capture_new_file`; on stop, exit. Using an
/// overlapped read + stop event (rather than closing a handle under a blocking synchronous read)
/// gives a deterministic, prompt teardown.
fn new_file_watch_loop(
    dir_addr: usize,
    stop_event_addr: usize,
    state_addr: usize,
    filter: windows::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE,
    stop: Arc<AtomicBool>,
) {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::{
        ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_NOTIFY_INFORMATION,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    // FIX (#7): a read error used to kill the watcher thread permanently (silently ending all
    // new-file / pin capture for the session). Retry a bounded number of times (re-arming the watch
    // on the same handle) before giving up, so a transient failure doesn't end capture. Reset on
    // any successful read.
    const MAX_READ_RETRIES: u32 = 10;
    let mut read_retries: u32 = 0;

    let dir = HANDLE(dir_addr as *mut c_void);
    let stop_event = HANDLE(stop_event_addr as *mut c_void);
    let state = state_addr as *const CfState;

    // The overlapped read needs its own completion event (distinct from the stop event).
    let read_event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cfapi: watcher CreateEventW failed: {e}");
            return;
        }
    };

    // 64 KiB buffer, DWORD-aligned (FILE_NOTIFY_INFORMATION starts with a u32).
    let mut buf = vec![0u32; 16 * 1024];
    while !stop.load(Ordering::Relaxed) {
        let mut ov = OVERLAPPED::default();
        ov.hEvent = read_event;
        let read = unsafe {
            ReadDirectoryChangesW(
                dir,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 4) as u32,
                true, // watch subtree
                filter,
                None,
                Some(&mut ov),
                None,
            )
        };
        if let Err(e) = read {
            // Shutdown cancels/closes the handle — that error is expected, exit cleanly.
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // FIX (#7): a transient read error — log + bounded retry (re-arm) instead of dying.
            read_retries += 1;
            if read_retries > MAX_READ_RETRIES {
                eprintln!(
                    "cfapi: watcher ReadDirectoryChangesW failed {read_retries}× ({e}) — giving up"
                );
                break;
            }
            eprintln!(
                "cfapi: watcher ReadDirectoryChangesW error ({e}) — retry {read_retries}/{MAX_READ_RETRIES}"
            );
            std::thread::sleep(std::time::Duration::from_millis(100 * read_retries as u64));
            continue;
        }
        read_retries = 0; // a successful arm resets the failure budget
        // Wait for either the read to complete or a stop request.
        let waited = unsafe { WaitForMultipleObjects(&[read_event, stop_event], false, INFINITE) };
        if stop.load(Ordering::Relaxed) || waited != WAIT_OBJECT_0 {
            // Stop signalled (or wait failed): cancel the pending read and exit.
            unsafe {
                let _ = windows::Win32::System::IO::CancelIoEx(dir, Some(&ov));
            }
            break;
        }
        let mut returned: u32 = 0;
        if unsafe { GetOverlappedResult(dir, &ov, &mut returned, false) }.is_err() || returned == 0
        {
            // FIX (#7): `returned == 0` means the change buffer OVERFLOWED — the kernel dropped
            // events. Re-arming alone would silently lose them (e.g. a batch of files from an
            // unzip). Reconcile the whole sync root so nothing is missed, then re-arm.
            unsafe { rescan_sync_root(state) };
            continue;
        }
        // Walk the variable-length FILE_NOTIFY_INFORMATION chain.
        let bytes =
            unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, returned as usize) };
        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_NOTIFY_INFORMATION>() > bytes.len() {
                break;
            }
            let info = unsafe { &*(bytes.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };
            let name_len = info.FileNameLength as usize / 2; // bytes → u16 count
            let name_ptr = unsafe {
                (bytes.as_ptr().add(offset) as *const u8)
                    .add(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName))
                    as *const u16
            };
            let name: Vec<u16> = unsafe { std::slice::from_raw_parts(name_ptr, name_len) }.to_vec();
            let rel = String::from_utf16_lossy(&name);
            let action = info.Action;
            let is_add_mod = action == FILE_ACTION_ADDED || action == FILE_ACTION_MODIFIED;
            // FIX (#8): also handle RENAMED_NEW_NAME. An atomic save of a NEW file (write `temp` →
            // rename `temp`→`final`) surfaces the destination ONLY as a rename — NOTIFY_RENAME
            // covers only the placeholder namespace, and ADDED/MODIFIED never fires for the dest —
            // so without this the new file never syncs until its next modify. We restrict it to a
            // PLAIN file (no cloud reparse tag): a placeholder rename is owned by NOTIFY_RENAME, and
            // double-capturing it could create a duplicate node before that rename applies.
            let is_rename_new = action == FILE_ACTION_RENAMED_NEW_NAME;
            if (is_add_mod || is_rename_new) && !state.is_null() {
                // Build the absolute path. `capture_new_file` re-maps it to a library-relative
                // path, is idempotent, and skips dirs / placeholders it already tracks.
                let st = unsafe { &*state };
                let abs = st.root.join(rel.replace('\\', "/").trim_start_matches('/'));
                let abs_str = abs.to_string_lossy().to_string();
                if is_add_mod {
                    // B6.1: a MODIFIED event can be a pin-state toggle (ATTRIBUTES) rather than a
                    // content edit. Reconcile the placeholder's pin state first (hydrate if pinned,
                    // dehydrate if unpinned); it's a no-op for a non-placeholder / unchanged pin.
                    // Then run the content-capture path (idempotent — skips when bytes are equal).
                    unsafe { reconcile_pin_state(st, &abs_str) };
                    unsafe { capture_new_file(st, &abs_str) };
                } else if !is_reparse_point_path(&abs_str) {
                    // RENAMED_NEW_NAME on a plain (non-placeholder) file — a genuine new-file
                    // atomic-save target. Capture it (idempotent if the NOTIFY_RENAME path or a
                    // prior event already created the node).
                    unsafe { capture_new_file(st, &abs_str) };
                }
            }
            let next = info.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
    unsafe {
        let _ = CloseHandle(read_event);
    }
    eprintln!("cfapi: new-file watcher stopped");
}

/// FIX (#6): rebuild the `ino → path` index from the provider tree ∩ the on-disk placeholders, so a
/// remount starts with the same path knowledge a full enumeration would have produced. DFS the
/// provider tree; for each child that is actually materialized on disk, record `ino → path` (and
/// descend into on-disk directories). `symlink_metadata` is a metadata-only probe — it does NOT
/// recall a dehydrated placeholder. Best-effort; a readdir error just stops that branch.
fn seed_path_index(provider: &SqliteProvider, root: &Path, index: &crate::PathIndex) {
    let mut stack = vec![(ROOT_INO, root.to_path_buf())];
    let mut count = 0usize;
    while let Some((dir_ino, dir_path)) = stack.pop() {
        let children = match provider.readdir(dir_ino) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for c in children {
            let child = dir_path.join(&c.name);
            if std::fs::symlink_metadata(&child).is_ok() {
                index.insert(c.ino, child.clone());
                count += 1;
                if c.kind == Kind::Dir {
                    stack.push((c.ino, child));
                }
            }
        }
    }
    if count > 0 {
        eprintln!("cfapi: seeded path index with {count} on-disk placeholder(s) at mount");
    }
}

fn register_sync_root(root: &Path) -> std::io::Result<()> {
    let display = to_wide(PROVIDER_NAME);
    let version = to_wide(PROVIDER_VERSION);
    // SyncRootIdentity is an opaque per-root blob; the root path is a fine identity.
    let identity = path_wide(root);
    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(display.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: identity.as_ptr() as *const c_void,
        SyncRootIdentityLength: (identity.len() * 2) as u32,
        ProviderId: PROVIDER_ID,
        ..Default::default()
    };
    // Full hydration (a read fetches the whole file) + **partial/on-demand** population: the
    // platform raises FETCH_PLACEHOLDERS as Explorer opens each directory, and we answer from
    // the provider's *current* tree — so we don't depend on the local tree being populated at
    // mount time (the sync loop fills it asynchronously).
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_PARTIAL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_NONE,
        HardLink: CF_HARDLINK_POLICY_NONE,
        ..Default::default()
    };
    let root_w = path_wide(root);
    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(root_w.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        )
        .map_err(win_err)?;
    }
    Ok(())
}

/// Build the placeholder holders for the direct children of `dir_ino` from the provider's
/// current tree. Each carries the child's `node_id` as `FileIdentity` and its real mtime.
/// The returned holders own the name/identity backing storage the `CF_PLACEHOLDER_CREATE_INFO`
/// pointers reference, so they must outlive the CfAPI call that consumes them.
fn placeholders_for_children(
    provider: &SqliteProvider,
    children: &[fs_provider::DirEntry],
) -> Vec<PlaceholderHolder> {
    let mut holders = Vec::new();
    for c in children {
        let attr = provider.getattr(c.ino).ok();
        let size = attr.map(|a| a.size).unwrap_or(0);
        let mtime = attr.map(|a| a.mtime).unwrap_or(0);
        let node_id = provider
            .node_id_for(c.ino)
            .ok()
            .flatten()
            .unwrap_or_default();
        holders.push(PlaceholderHolder::new(
            &c.name,
            &node_id,
            size,
            mtime,
            c.kind == Kind::Dir,
        ));
    }
    holders
}

/// Owns the wide-string + identity backing storage for one `CF_PLACEHOLDER_CREATE_INFO`
/// (the struct holds raw pointers into these, so they must outlive the create call).
struct PlaceholderHolder {
    name: U16CString,
    identity: Vec<u8>,
    size: i64,
    mtime_ticks: i64,
    is_dir: bool,
}

impl PlaceholderHolder {
    fn new(name: &str, node_id: &str, size: u64, mtime_nanos: i64, is_dir: bool) -> Self {
        Self {
            name: to_wide(name),
            identity: FileIdentity::from_node_id(node_id).0,
            size: if is_dir { 0 } else { size as i64 },
            // CfAPI metadata times are i64 (100 ns ticks since 1601) — exactly our FILETIME.
            mtime_ticks: unix_nanos_to_filetime(mtime_nanos) as i64,
            is_dir,
        }
    }
    fn info(&self) -> CF_PLACEHOLDER_CREATE_INFO {
        let attrs = if self.is_dir {
            0x10 /* FILE_ATTRIBUTE_DIRECTORY */
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };
        CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(self.name.as_ptr()),
            FsMetadata: CF_FS_METADATA {
                FileSize: self.size,
                BasicInfo: FILE_BASIC_INFO {
                    CreationTime: self.mtime_ticks,
                    LastAccessTime: self.mtime_ticks,
                    LastWriteTime: self.mtime_ticks,
                    ChangeTime: self.mtime_ticks,
                    FileAttributes: attrs,
                },
            },
            FileIdentity: self.identity.as_ptr() as *const c_void,
            FileIdentityLength: self.identity.len() as u32,
            // Mark in-sync (just populated from the source of truth); a later content change
            // dehydrates it via CfUpdatePlaceholder. Disable on-demand population — Phase 1
            // fully populates up front.
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
                | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
            ..Default::default()
        }
    }
}

/// Recover the `node_id` from a callback's `FileIdentity`, **safely**: the sync root itself
/// has a null/empty identity (`from_raw_parts` on a null pointer is UB — it must not be
/// called), so this returns `None` there rather than dereferencing null.
unsafe fn callback_node_id(info: &CF_CALLBACK_INFO) -> Option<String> {
    if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        return None;
    }
    let bytes = std::slice::from_raw_parts(
        info.FileIdentity as *const u8,
        info.FileIdentityLength as usize,
    );
    FileIdentity(bytes.to_vec()).to_node_id()
}

/// Build a `CF_OPERATION_INFO` for answering a callback (shared by the transfer ops).
unsafe fn op_info_for(info: &CF_CALLBACK_INFO, op_type: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: op_type,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: info.CorrelationVector as *const _,
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    }
}

/// Max bytes per `CfExecute(TRANSFER_DATA)` slice (B6.3). A FETCH_DATA for a multi-GB file is
/// streamed in page-aligned chunks of at most this size, so we never buffer (or read from the
/// provider) the whole file at once. 16 MiB is a whole number of 4 KiB pages.
const FETCH_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

/// Hand one error completion back for a failed FETCH_DATA so the reading app fails fast instead of
/// hanging to a timeout. `offset`/`length` are the range the error covers.
///
/// FIX (#10): the error MUST span the kernel's required range (the CloudMirror sample completes a
/// failed transfer over `RequiredLength`, not `Length: 0`). A zero-length error completion leaves
/// the required bytes unanswered, so the reader blocks until the CfAPI timeout instead of failing
/// promptly. Callers pass the still-unsatisfied portion of the required range.
unsafe fn transfer_data_error(info: &CF_CALLBACK_INFO, offset: i64, length: i64) {
    let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut op_params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: STATUS_UNSUCCESSFUL,
                Buffer: std::ptr::null(),
                Offset: offset,
                Length: length.max(0),
            },
        },
    };
    if let Err(e) = CfExecute(&op_info, &mut op_params) {
        eprintln!("cfapi: CfExecute(TRANSFER_DATA error) failed: {e}");
    }
}

/// FETCH_DATA: the kernel needs bytes for a dehydrated placeholder. Resolve node → hydrate →
/// **stream** the requested range back in page-aligned slices via `CfExecute(TRANSFER_DATA)`
/// (B6.3 — bounded memory for multi-GB files + per-slice progress). **Always** answers (a
/// per-slice success, or an error completion) — an unanswered FETCH_DATA hangs the reading app.
unsafe extern "system" fn fetch_data_callback(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() || params.is_null() {
        return;
    }
    let info = &*info;
    let params = &*params;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;

    let fetch = params.Anonymous.FetchData;
    let req_offset = fetch.RequiredFileOffset;
    let req_length = fetch.RequiredLength;

    // FIX (#3, teardown deadlock): the ONLY callback that never checked `shutting_down`. During
    // teardown, `CfDisconnectSyncRoot` is draining in-flight callbacks; issuing more re-entrant
    // `CfExecute`s here (per streamed slice) can wedge the closing connection. Fail this transfer
    // fast over the full required range (the reader errors instead of hanging) and stop — no
    // streaming `CfExecute` loop while the connection is going away.
    if state.shutting_down.load(Ordering::Relaxed) {
        eprintln!("cfapi: FETCH_DATA during teardown — reporting error, not streaming");
        transfer_data_error(info, req_offset, req_length);
        return;
    }

    // Resolve node + ino, and make its chunks local (presigned GET into the cache). On any failure
    // here we can't produce bytes → one error completion over the full required range, then bail.
    let node_id = match callback_node_id(info) {
        Some(n) => n,
        None => {
            eprintln!("cfapi: FETCH_DATA — no node identity, reporting error");
            transfer_data_error(info, req_offset, req_length);
            return;
        }
    };
    eprintln!("cfapi: FETCH_DATA node={node_id} off={req_offset} len={req_length}");
    // Range hydration: fetch only the chunks overlapping the kernel's required range, not the
    // whole file. (`CfHydratePlaceholder(0, -1)` from the pin path raises FETCH_DATA for the
    // full range, so a pinned file still ends up fully local.)
    if let Some(h) = &state.hydrate {
        h(&node_id, req_offset as u64, req_length as u64);
    }
    let ino = match state.provider.ino_for_node_id(&node_id).ok().flatten() {
        Some(i) => i,
        None => {
            eprintln!("cfapi: FETCH_DATA node={node_id} — not found locally, reporting error");
            transfer_data_error(info, req_offset, req_length);
            return;
        }
    };
    let file_size = state.provider.getattr(ino).map(|a| a.size).unwrap_or(0);
    // FIX (#3): snapshot the connection key ONCE before the streaming loop instead of locking the
    // `connection_key` mutex per slice. Re-locking per slice both wastes work and widens the window
    // for the teardown deadlock (Drop/remove_library used to hold this mutex across disconnect).
    let conn_key = *state.connection_key.lock().unwrap();

    // Plan the page-aligned slices for the required range (the unit-tested core). An empty plan
    // (zero length / past EOF) still needs a single success completion so the read can finish.
    let plan = crate::plan_transfer_chunks(
        req_offset as u64,
        req_length as u64,
        file_size,
        FETCH_CHUNK_BYTES,
    );
    let total: i64 = plan.iter().map(|c| c.length as i64).sum::<i64>().max(1);
    let mut completed: i64 = 0;

    if plan.is_empty() {
        if req_length == 0 {
            // A genuine zero-length read — answer success with an empty range so the request
            // completes.
            let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_DATA);
            let mut op_params = CF_OPERATION_PARAMETERS {
                ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
                Anonymous: CF_OPERATION_PARAMETERS_0 {
                    TransferData: CF_OPERATION_PARAMETERS_0_0 {
                        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                        CompletionStatus: STATUS_SUCCESS,
                        Buffer: std::ptr::null(),
                        Offset: req_offset,
                        Length: 0,
                    },
                },
            };
            let _ = CfExecute(&op_info, &mut op_params);
        } else {
            // FIX (#10): the required range starts at/after EOF while the kernel still wants
            // `req_length` bytes — the placeholder advertises more than the provider holds. We
            // cannot produce those bytes; complete an ERROR over the required range so the reader
            // fails fast instead of hanging on a range that will never be satisfied.
            eprintln!(
                "cfapi: FETCH_DATA node={node_id} required range off={req_offset} len={req_length} \
                 past provider EOF ({file_size}) — reporting error over the range"
            );
            transfer_data_error(info, req_offset, req_length);
        }
        return;
    }

    for chunk in plan {
        // Read just this slice from the provider (bounded memory). A read failure mid-stream → an
        // error completion over the REMAINDER of the required range (FIX #10) so the app's read
        // fails rather than hanging waiting for the bytes we couldn't produce.
        let bytes = match state.provider.read(ino, chunk.offset, chunk.length as u32) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "cfapi: FETCH_DATA node={node_id} read(off={}, len={}) failed: {e}",
                    chunk.offset, chunk.length
                );
                let remaining = (req_offset + req_length) - chunk.offset as i64;
                transfer_data_error(info, chunk.offset as i64, remaining);
                // Clear any cancel marker for this key so it can't leak (minor-fix hygiene).
                state
                    .cancelled_transfers
                    .lock()
                    .unwrap()
                    .remove(&info.TransferKey);
                return;
            }
        };
        let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_DATA);
        let mut op_params = CF_OPERATION_PARAMETERS {
            ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_0 {
                    Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                    CompletionStatus: STATUS_SUCCESS,
                    Buffer: bytes.as_ptr() as *const c_void,
                    Offset: chunk.offset as i64,
                    Length: bytes.len() as i64,
                },
            },
        };
        if let Err(e) = CfExecute(&op_info, &mut op_params) {
            eprintln!(
                "cfapi: CfExecute(TRANSFER_DATA off={}) failed: {e}",
                chunk.offset
            );
            return; // the kernel saw an error on this slice; stop streaming
        }
        // Report progress so Explorer shows a hydration bar for a big file. Best-effort — a
        // failure here is cosmetic and must not abort the transfer. Uses the key snapshotted once
        // before the loop (FIX #3 — no per-slice mutex lock).
        completed += bytes.len() as i64;
        if let Some(key) = conn_key {
            let _ = CfReportProviderProgress(key, info.TransferKey, total, completed);
        }

        // B6.3: honor a CANCEL_FETCH_DATA that arrived for this transfer — stop streaming further
        // slices (the kernel already knows the transfer is cancelled; no extra completion needed).
        if state
            .cancelled_transfers
            .lock()
            .unwrap()
            .remove(&info.TransferKey)
        {
            eprintln!(
                "cfapi: FETCH_DATA node={node_id} cancelled mid-stream at off={}",
                chunk.offset
            );
            return;
        }
    }
    // FIX (#10): if the placeholder claims a larger size than the provider actually holds, the
    // success slices (clamped to `file_size`) leave the tail `[file_size, req_offset+req_length)`
    // uncompleted — the reader would hang waiting for bytes that don't exist. Complete that tail as
    // an error so the read fails fast. (`invalidate` can leave a stale/oversized placeholder if its
    // open failed with no retry; this is the belt-and-suspenders on the read side.)
    let req_end = req_offset + req_length;
    if (file_size as i64) < req_end {
        let tail_off = file_size as i64;
        eprintln!(
            "cfapi: FETCH_DATA node={node_id} provider EOF {file_size} < required end {req_end} \
             — completing tail as error"
        );
        transfer_data_error(info, tail_off, req_end - tail_off);
    }

    // Done streaming cleanly — clear any (late) cancel marker for this key so it can't leak.
    state
        .cancelled_transfers
        .lock()
        .unwrap()
        .remove(&info.TransferKey);
}

/// CANCEL_FETCH_DATA: the platform aborts an in-flight hydration (the reading app closed the
/// handle, or the file was deleted while downloading). Record the transfer key so the streaming
/// [`fetch_data_callback`] loop stops before its next slice. No completion is issued — a cancel is
/// not answered with `CfExecute` (the kernel owns the cancellation).
unsafe extern "system" fn cancel_fetch_data_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;
    eprintln!("cfapi: CANCEL_FETCH_DATA transfer_key={}", info.TransferKey);
    state
        .cancelled_transfers
        .lock()
        .unwrap()
        .insert(info.TransferKey);
}

/// B6.1 — reconcile a placeholder's **pin state** to its local bytes ("Always keep on this device"
/// → hydrate; "Free up space" → dehydrate). Driven by the watcher's ATTRIBUTES events (CfAPI has
/// no dedicated pin-change callback). Reads the placeholder's `CF_PLACEHOLDER_STANDARD_INFO.PinState`,
/// maps it through the pure [`crate::pin_hydration_action`], and calls `CfHydratePlaceholder` /
/// `CfDehydratePlaceholder` accordingly. Best-effort + idempotent:
/// - A non-placeholder / non-cloud file (`CfGetPlaceholderInfo` fails) is silently skipped.
/// - Hydrate raises FETCH_DATA → our streaming callback (which runs the hydrate hook), so a pinned
///   file ends up fully local. Dehydrate drops the on-disk bytes; the next read re-fetches.
/// - During teardown we skip (re-entrant CfAPI on a closing connection can deadlock).
unsafe fn reconcile_pin_state(state: &CfState, path: &str) {
    if state.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    // Only placeholders we track have a node_id; resolve the kind so we no-op on directories.
    let rel = match library_relative(&state.root.to_string_lossy(), path) {
        Some(r) if !r.is_empty() => r,
        _ => return,
    };
    let kind = match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(_) => Kind::File,
        Err(_) => return, // gone/unreadable
    };
    let _ = rel;

    // Open with BACKUP_SEMANTICS so we can open either a file or a dehydrated placeholder without
    // triggering hydration just by opening (no data read here).
    let path_w = path_wide(Path::new(path));
    let handle = match CreateFileW(
        PCWSTR(path_w.as_ptr()),
        (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_ATTRIBUTE_NORMAL.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };

    // Read the placeholder's standard info to get its current pin state. If this fails the file
    // isn't a cloud placeholder (a plain file) — nothing to reconcile.
    let mut info = CF_PLACEHOLDER_STANDARD_INFO::default();
    let mut returned: u32 = 0;
    let got = CfGetPlaceholderInfo(
        handle,
        CF_PLACEHOLDER_INFO_STANDARD,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() as u32,
        Some(&mut returned),
    );
    if got.is_err() {
        let _ = CloseHandle(handle);
        return;
    }

    let pin = crate::PinState::from_raw(info.PinState.0);
    match crate::pin_hydration_action(pin, kind) {
        crate::HydrationAction::Hydrate => {
            eprintln!("cfapi: pin → hydrate {path}");
            // Hydrate the whole file (offset 0, length -1 = to EOF). FETCH_DATA does the actual
            // chunk fetch via the hydrate hook.
            if let Err(e) = CfHydratePlaceholder(handle, 0, -1, CF_HYDRATE_FLAG_NONE, None) {
                eprintln!("cfapi: CfHydratePlaceholder({path}) failed: {e}");
            }
        }
        crate::HydrationAction::Dehydrate => {
            eprintln!("cfapi: unpin → dehydrate {path}");
            if let Err(e) = CfDehydratePlaceholder(handle, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) {
                eprintln!("cfapi: CfDehydratePlaceholder({path}) failed: {e}");
            }
        }
        crate::HydrationAction::None => {}
    }
    let _ = CloseHandle(handle);
}

/// FETCH_PLACEHOLDERS: Explorer opened a directory — enumerate its children from the
/// provider's *current* tree and hand them back as placeholders via
/// `CfExecute(TRANSFER_PLACEHOLDERS)`. Resolving the directory: a child dir carries its
/// `node_id` as `FileIdentity`; the sync root itself has no node identity, so an
/// unrecognized identity means the root (`ROOT_INO`). Always answers (an unanswered
/// enumeration hangs Explorer to a timeout).
unsafe extern "system" fn fetch_placeholders_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;

    // Which directory? A NULL/empty identity is the sync root itself → ROOT_INO. A PRESENT identity
    // that doesn't resolve is an unknown/foreign placeholder — do NOT fall back to ROOT_INO (that
    // would enumerate the ROOT's children into the wrong directory). Answer an empty set instead.
    let dir_ino = match callback_node_id(info) {
        None => ROOT_INO, // null identity = the sync root
        Some(nid) => match state.provider.ino_for_node_id(&nid).ok().flatten() {
            Some(i) => i,
            None => {
                eprintln!(
                    "cfapi: FETCH_PLACEHOLDERS for unknown node={nid} — answering empty (not root)"
                );
                let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
                let mut op_params = CF_OPERATION_PARAMETERS {
                    ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
                    Anonymous: CF_OPERATION_PARAMETERS_0 {
                        TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                            Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
                            CompletionStatus: STATUS_SUCCESS,
                            PlaceholderTotalCount: 0,
                            PlaceholderArray: std::ptr::null_mut(),
                            PlaceholderCount: 0,
                            EntriesProcessed: 0,
                        },
                    },
                };
                let _ = CfExecute(&op_info, &mut op_params);
                return;
            }
        },
    };

    // B6.4: record this dir's children in the ino→path index (O(1) path resolution later). The
    // dir's own path is the sync root for ROOT_INO, else its cached index entry; if we don't know
    // the dir path yet we just skip indexing (the invalidator falls back to a tree walk).
    let dir_path = if dir_ino == ROOT_INO {
        Some(state.root.clone())
    } else {
        state.path_index.get(dir_ino)
    };

    let children = state.provider.readdir(dir_ino).unwrap_or_default();
    if let Some(dp) = &dir_path {
        state
            .path_index
            .insert_children(dp, children.iter().map(|c| (c.ino, c.name.as_str())));
    }
    let holders = placeholders_for_children(&state.provider, &children);
    let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = holders.iter().map(|h| h.info()).collect();
    eprintln!(
        "cfapi: FETCH_PLACEHOLDERS dir_ino={dir_ino} → {} entries",
        infos.len()
    );

    let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let mut op_params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                // Mark the dir fully populated so the platform won't re-ask.
                Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
                CompletionStatus: STATUS_SUCCESS,
                PlaceholderTotalCount: infos.len() as i64,
                // An EMPTY directory must transfer a NULL array, not the dangling pointer
                // `Vec::as_mut_ptr()` returns for a zero-length Vec — CfExecute rejects the
                // latter with 0x8007017C ("the cloud operation is invalid"), which broke
                // enumeration of any empty folder / a fresh (empty) library.
                PlaceholderArray: if infos.is_empty() {
                    std::ptr::null_mut()
                } else {
                    infos.as_mut_ptr()
                },
                PlaceholderCount: infos.len() as u32,
                EntriesProcessed: 0,
            },
        },
    };
    if let Err(e) = CfExecute(&op_info, &mut op_params) {
        eprintln!("cfapi: CfExecute(TRANSFER_PLACEHOLDERS) failed: {e}");
    }
}

/// Read a `PCWSTR` (NUL-terminated wide string) into a Rust `String` (lossy). Empty on null.
unsafe fn pcwstr_string(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    p.to_string().unwrap_or_default()
}

/// Local-change capture: a file handle closed.
///
/// **Existing synced file** (placeholder carries a `node_id`): if the on-disk content now
/// differs from what the provider holds, the **user edited it** — re-ingest the bytes (which
/// enqueues the outbox; the sync loop pushes) and mark the placeholder back in-sync. The
/// content comparison is the self-write filter: our own hydration writes and plain reads leave
/// the on-disk bytes equal to the provider's, so they're skipped (no push loop).
///
/// **New file** (no `node_id` — Phase 2b): a real file the user dropped into the sync root.
/// Derive the library-relative path, resolve/create the parent dirs + the node in the
/// `Provider`, ingest its bytes (enqueues the outbox), then `CfConvertToPlaceholder` to give the
/// on-disk file our `node_id` identity + mark it in-sync. The convert itself fires another close;
/// the `self_writes` echo guard short-circuits that re-entry. Deletes / renames are captured in
/// the dedicated [`notify_delete_callback`] / [`notify_rename_callback`] (NOTIFY_DELETE/RENAME).
unsafe extern "system" fn notify_file_close_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;

    // Full on-disk path = VolumeDosName ("C:") + NormalizedPath ("\Users\…\file").
    let path = format!(
        "{}{}",
        pcwstr_string(info.VolumeDosName),
        pcwstr_string(info.NormalizedPath)
    );

    // A placeholder carries its node_id; a brand-new file the user dropped in does not.
    let node_id = match callback_node_id(info) {
        Some(n) => n,
        // PHASE 2b: new file. Capture it (resolve_path + create + ingest + convert-to-placeholder).
        None => {
            capture_new_file(state, &path);
            return;
        }
    };
    let ino = match state.provider.ino_for_node_id(&node_id) {
        Ok(Some(i)) => i,
        _ => return,
    };

    // FIX (#4): SKIP a dehydrated placeholder. Reading it (`std::fs::read`) under
    // CF_HYDRATION_POLICY_FULL would RECALL it (re-download the bytes) just to content-compare —
    // e.g. an unpin (dehydrate) immediately followed by this close would re-hydrate the file the
    // user just freed. A dehydrated placeholder can't have been locally edited, so there's nothing
    // to capture. A hydrated/plain file (no recall bits) reads freely.
    if is_dehydrated_placeholder(&path) {
        return;
    }

    let on_disk = match read_no_recall(&path) {
        Ok(b) => b,
        Err(_) => return, // gone/unreadable (or still a dehydrated placeholder)
    };

    // Compare against the provider's current content; only a real change is a user edit. Read the
    // FULL size (FIX #9 — no u32 wrap on a >4 GiB file).
    let size = state.provider.getattr(ino).map(|a| a.size).unwrap_or(0);
    let current = read_full(&state.provider, ino, size).ok();
    if current.as_deref() == Some(on_disk.as_slice()) {
        return; // unchanged — our own hydration write or a plain read
    }
    eprintln!(
        "cfapi: local edit captured node={node_id} ({} bytes) → ingest + push",
        on_disk.len()
    );
    if let Err(e) = state.provider.ingest_bytes(ino, &on_disk) {
        eprintln!("cfapi: ingest_bytes(node={node_id}) failed: {e}");
        return;
    }
    // During teardown, skip the re-entrant CfSetInSyncState (can deadlock the closing
    // connection). The edit is already in the provider/outbox → the shutdown drain pushes it.
    if state.shutting_down.load(Ordering::Relaxed) {
        return;
    }
    // Mark the placeholder back in-sync (captured locally; the sync loop pushes it).
    let path_w = path_wide(Path::new(&path));
    if let Ok(handle) = CreateFileW(
        PCWSTR(path_w.as_ptr()),
        (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
        None,
    ) {
        let mut usn: i64 = 0;
        let _ = CfSetInSyncState(
            handle,
            CF_IN_SYNC_STATE_IN_SYNC,
            CF_SET_IN_SYNC_FLAG_NONE,
            Some(&mut usn),
        );
        let _ = CloseHandle(handle);
    }
}

/// PHASE 2b — capture a **new** file the user dropped into the sync root (a close with no
/// `node_id`). Maps the on-disk path back to a library-relative path, materializes the node in
/// the `Provider` (`resolve_path` → `create` → `ingest_bytes`, which enqueues the outbox so the
/// sync loop pushes), then `CfConvertToPlaceholder`s the on-disk file so it carries our
/// `node_id` identity (subsequent closes take the edit path) and is marked in-sync.
///
/// The convert itself fires another close on the same path; the `self_writes` echo guard
/// short-circuits that one re-entry so we don't create a duplicate node.
unsafe fn capture_new_file(state: &CfState, path: &str) {
    // Echo guard (self-healing): if we just converted this path AND the mark is fresh, this close
    // is the convert echo — `take` consumes it and we skip exactly once. A stale mark is dropped
    // and we fall through, so a genuine edit isn't wrongly suppressed. `SelfOps` keys by the
    // NORMALIZED path (case/separator-insensitive), so a `mark` from one call site and a `take`
    // from another (the watcher's `root.join(rel)` form vs the close callback's
    // `VolumeDosName + NormalizedPath` form) collide — otherwise the guard is effectively dead for
    // any path whose two spellings differ (e.g. subdir files).
    if state.self_writes.take(path) {
        eprintln!("cfapi: new file {path} — echo of our own convert, suppressed");
        return; // fresh — the convert echo; suppress
    }

    // Map the absolute on-disk path back into the library tree.
    let root_str = state.root.to_string_lossy();
    let rel = match library_relative(&root_str, path) {
        Some(r) if !r.is_empty() => r,
        _ => {
            eprintln!(
                "cfapi: new file {path} — not under root {root_str} (or is the root), skipping"
            );
            return; // outside the root (or the root itself) — nothing to capture
        }
    };

    // Skip directories: a dir close with no identity isn't a new-file capture (the parent dirs
    // are materialized lazily by `resolve_path` when their children land).
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            eprintln!("cfapi: new file {path} — is a directory, skipping");
            return;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("cfapi: new file {path} — metadata failed ({e}), skipping");
            return; // gone/unreadable (e.g. still a placeholder, or deleted)
        }
    }

    // FIX (#4): SKIP a dehydrated placeholder — reading it would recall (re-download) it. This
    // matters for the MODIFIED path (`reconcile_pin_state` dehydrates on unpin, then this runs) and
    // for a NOTIFY_FILE_CLOSE on a freed-up file. A dehydrated placeholder has no local edit to
    // capture. (`capture_new_file` also runs for genuine new plain files, which are never
    // dehydrated, so this never blocks a real capture.)
    if is_dehydrated_placeholder(path) {
        return;
    }

    // Read the new file's bytes without recalling (FIX #4). A still-dehydrated / unreadable file
    // → skip.
    let bytes = match read_no_recall(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cfapi: new file {path} — read failed ({e}), skipping");
            return;
        }
    };

    // Resolve/create the parent dirs, then the leaf node.
    let (parent_ino, leaf) = match state.provider.resolve_path(&rel) {
        Ok(pl) => pl,
        Err(e) => {
            eprintln!("cfapi: new file resolve_path(rel={rel}) failed: {e}");
            return;
        }
    };
    // The directory watcher (which also drives this path) can fire ADDED *and* a later
    // MODIFIED for the same file, and a placeholder-convert can race a watcher event. So this
    // must be idempotent: if a node already exists at the leaf, treat the close/event as an
    // EDIT (re-ingest into the existing ino) and skip the convert (it's already a placeholder).
    let existing = state.provider.lookup(parent_ino, &leaf).ok();
    let attr = match existing {
        Some(a) => {
            // Existing node: ingest only if the content actually changed (the watcher's MODIFIED
            // echo of our own write, or a plain metadata touch, leaves bytes equal → skip). Read
            // the FULL size (FIX #9 — no u32 wrap on a >4 GiB file).
            let cur = read_full(&state.provider, a.ino, a.size).ok();
            if cur.as_deref() == Some(bytes.as_slice()) {
                return; // unchanged — nothing to capture (our own write or a no-op touch)
            }
            if let Err(e) = state.provider.ingest_bytes(a.ino, &bytes) {
                eprintln!(
                    "cfapi: capture edit ingest_bytes(ino={}) failed: {e}",
                    a.ino
                );
            } else {
                eprintln!(
                    "cfapi: edit captured rel={rel} ino={} ({} bytes)",
                    a.ino,
                    bytes.len()
                );
            }
            return; // already a placeholder; no convert needed
        }
        None => match state.provider.create(parent_ino, &leaf) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("cfapi: new file create(parent={parent_ino}, {leaf}) failed: {e}");
                return;
            }
        },
    };
    let node_id = match state.provider.node_id_for(attr.ino) {
        Ok(Some(n)) => n,
        _ => {
            eprintln!("cfapi: new file node_id_for(ino={}) missing", attr.ino);
            return;
        }
    };
    if let Err(e) = state.provider.ingest_bytes(attr.ino, &bytes) {
        eprintln!("cfapi: new file ingest_bytes(node={node_id}) failed: {e}");
        return;
    }
    eprintln!(
        "cfapi: new file captured rel={rel} node={node_id} ({} bytes)",
        bytes.len()
    );

    // During teardown, skip the re-entrant CfAPI convert (it can deadlock the closing
    // connection). The node + bytes are already in the provider/outbox, so the shutdown drain
    // still pushes the file; we just don't bother giving the on-disk file a placeholder identity
    // (the process is exiting anyway).
    if state.shutting_down.load(Ordering::Relaxed) {
        return;
    }

    // Convert the real on-disk file into a placeholder carrying our identity so future closes
    // take the edit path (content-compare). Mark in-sync — the bytes are already correct locally
    // and the sync loop will push them. Guard the echo (our convert fires another close).
    state.self_writes.mark(path);
    let identity = FileIdentity::from_node_id(&node_id);
    let path_w = path_wide(Path::new(path));
    match CreateFileW(
        PCWSTR(path_w.as_ptr()),
        (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
        None,
    ) {
        Ok(handle) => {
            let mut usn: i64 = 0;
            let blob = identity.as_bytes();
            match CfConvertToPlaceholder(
                handle,
                Some(blob.as_ptr() as *const c_void),
                blob.len() as u32,
                CF_CONVERT_FLAG_MARK_IN_SYNC,
                Some(&mut usn),
                None,
            ) {
                Ok(()) => {
                    // Both plan docs specify convert **+ an explicit `CfSetInSyncState`** (the
                    // sibling edit-capture block does the same). MARK_IN_SYNC on the convert is
                    // belt-and-suspenders; this makes code and spec agree.
                    let mut sync_usn: i64 = 0;
                    let _ = CfSetInSyncState(
                        handle,
                        CF_IN_SYNC_STATE_IN_SYNC,
                        CF_SET_IN_SYNC_FLAG_NONE,
                        Some(&mut sync_usn),
                    );
                }
                Err(e) => {
                    eprintln!("cfapi: new file CfConvertToPlaceholder(node={node_id}) failed: {e}");
                    // The convert didn't fire its own close, so drop the echo guard we inserted.
                    let _ = state.self_writes.take(path);
                }
            }
            let _ = CloseHandle(handle);
        }
        Err(e) => {
            eprintln!("cfapi: new file open(node={node_id}) failed: {e}");
            let _ = state.self_writes.take(path);
        }
    }
}

/// NOTIFY_DELETE (pre-op): a placeholder is about to be deleted from the sync root. Mirror the
/// delete into the `Provider` (recursive subtree sweep + outbox push via `delete_by_node_id`),
/// then **ACK** so the OS proceeds with the on-disk delete.
///
/// **Always ACKs** — even on a provider error (logged): a failed local mirror must not wedge
/// Explorer by vetoing the user's delete. A null/unknown `FileIdentity` (e.g. a never-tracked
/// file) just ACKs with nothing to remove.
unsafe extern "system" fn notify_delete_callback(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;

    // Echo guard: if the daemon itself is deleting this placeholder (applying a remote delete/move
    // via `CfApiInvalidate::removed`/`moved`), this NOTIFY_DELETE is our own echo. Mirroring it would
    // re-enqueue a spurious `delete` to the outbox (or, mid-move, delete the relocated node). Consume
    // the mark and skip the mirror — but still ACK below so the OS delete proceeds.
    let path = format!(
        "{}{}",
        pcwstr_string(info.VolumeDosName),
        pcwstr_string(info.NormalizedPath)
    );
    let is_echo = state.self_removes.take(&path);
    if is_echo {
        eprintln!("cfapi: delete at {path} — echo of our own remove, not mirrored");
    } else if let Some(node_id) = callback_node_id(info) {
        eprintln!("cfapi: delete captured node={node_id}");
        // B6.4: drop the node's stale ino→path index entry (resolve the ino BEFORE the delete
        // removes the row). Best-effort — a miss just leaves the entry to be overwritten later.
        if let Ok(Some(ino)) = state.provider.ino_for_node_id(&node_id) {
            state.path_index.invalidate(ino);
        }
        if let Err(e) = state.provider.delete_by_node_id(&node_id) {
            // Log + still ACK — a failed mirror must not block the OS delete.
            eprintln!("cfapi: delete_by_node_id(node={node_id}) failed: {e}");
        }
    }

    // During teardown, skip the ACK `CfExecute` (re-entering the closing connection can
    // deadlock). The delete is already mirrored to the provider/outbox; the OS op is moot since
    // the connection is going away.
    if state.shutting_down.load(Ordering::Relaxed) {
        return;
    }

    // ACK the delete so the platform proceeds (an unanswered pre-op hangs Explorer).
    let op_info = op_info_for(info, CF_OPERATION_TYPE_ACK_DELETE);
    let mut op_params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: STATUS_SUCCESS,
            },
        },
    };
    if let Err(e) = CfExecute(&op_info, &mut op_params) {
        eprintln!("cfapi: CfExecute(ACK_DELETE) failed: {e}");
    }
}

/// NOTIFY_RENAME (pre-op): a placeholder is about to be renamed/moved. Resolve the moved node's
/// `node_id` from its `FileIdentity`, map the callback's `TargetPath` (volume-relative) to a
/// library-relative path, resolve the new `(parent_ino, leaf)` in the `Provider`, and apply the
/// rename (`rename_by_node_id`, which enqueues the outbox). Then **ACK** so the OS proceeds.
///
/// **Always ACKs** — a failed local mirror must not veto the user's rename. A null/unknown
/// identity, an out-of-root target, or a resolution error just ACKs (nothing mirrored).
unsafe extern "system" fn notify_rename_callback(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if info.is_null() {
        return;
    }
    let info = &*info;
    let state = info.CallbackContext as *const CfState;
    if state.is_null() {
        return;
    }
    let state = &*state;

    // Echo guard: a daemon-applied move is done as remove-old + create-new (not a `MoveFileEx`), so
    // it should NOT surface here — but if a future path or the OS coalesces it into a rename, the
    // SOURCE path carries our mark. Consume it and skip the mirror (still ACK below).
    let src_path = format!(
        "{}{}",
        pcwstr_string(info.VolumeDosName),
        pcwstr_string(info.NormalizedPath)
    );
    let is_echo = state.self_removes.take(&src_path);
    if !is_echo {
        if let Some(node_id) = callback_node_id(info) {
            // The target path is volume-relative (e.g. `\Users\…\new`); prefix the volume DOS name
            // (e.g. `C:`) to form the full on-disk path, mirroring the close callback.
            if !params.is_null() {
                let params = &*params;
                let target = format!(
                    "{}{}",
                    pcwstr_string(info.VolumeDosName),
                    pcwstr_string(params.Anonymous.Rename.TargetPath),
                );
                let root_str = state.root.to_string_lossy();
                match library_relative(&root_str, &target) {
                    Some(rel) if !rel.is_empty() => match state.provider.resolve_path(&rel) {
                        Ok((new_parent, new_name)) => {
                            // FIX (#5): the node's cached path AND — for a moved directory — every
                            // descendant's cached path embed the OLD ancestor path and are now stale.
                            // The old code dropped only the moved node's own entry, leaving
                            // descendants as poisoned *hits* (`path_for`/`removed`/`invalidate` trust
                            // a hit → a remote update to a child served old bytes forever; a remote
                            // delete left a ghost). Drop the whole moved subtree by its OLD path so
                            // those consumers fall back to the authoritative tree walk (a miss).
                            if let Ok(Some(ino)) = state.provider.ino_for_node_id(&node_id) {
                                state.path_index.invalidate(ino);
                            }
                            state.path_index.invalidate_subtree(Path::new(&src_path));
                            if let Err(e) = state
                                .provider
                                .rename_by_node_id(&node_id, new_parent, &new_name)
                            {
                                eprintln!("cfapi: rename_by_node_id(node={node_id}) failed: {e}");
                            } else {
                                eprintln!("cfapi: rename captured node={node_id} -> {rel}");
                            }
                        }
                        Err(e) => eprintln!("cfapi: rename resolve_path(rel={rel}) failed: {e}"),
                    },
                    // Target outside the sync root (or the root itself): nothing to mirror as a
                    // rename — just ACK below so the OS op isn't blocked.
                    _ => eprintln!(
                        "cfapi: rename target outside root for node={node_id} — not mirrored"
                    ),
                }
            }
        }
    } else {
        eprintln!("cfapi: rename of {src_path} — echo of our own move, not mirrored");
    }

    // During teardown, skip the ACK `CfExecute` (re-entering the closing connection can
    // deadlock). The rename is already mirrored to the provider/outbox.
    if state.shutting_down.load(Ordering::Relaxed) {
        return;
    }

    // ACK the rename so the platform proceeds.
    let op_info = op_info_for(info, CF_OPERATION_TYPE_ACK_RENAME);
    let mut op_params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_6 {
                Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: STATUS_SUCCESS,
            },
        },
    };
    if let Err(e) = CfExecute(&op_info, &mut op_params) {
        eprintln!("cfapi: CfExecute(ACK_RENAME) failed: {e}");
    }
}

impl CfApiMountHandle {
    pub fn mountpoint(&self) -> &Path {
        &self.root
    }
    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
    /// Block until stopped (for the `maker-cfapi` CLI; the caller sets `stop` on Ctrl-C/exit).
    pub fn run_until_ctrl_c(self) {
        while !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Enter teardown mode **early** — before the owner joins its sync thread and disconnects.
    /// CfAPI may deliver its batched NOTIFY callbacks at that point (when the user unmounts), and
    /// those callbacks must already see `shutting_down` so they skip the re-entrant CfAPI calls
    /// (convert/set-in-sync/ACK) that deadlock a closing connection. Idempotent; callbacks still
    /// mirror their change into the provider/outbox (nothing is lost — the shutdown drain pushes
    /// it). Safe to call before `Drop`.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// The shared teardown flag. Pass it to `CfApiInvalidate::new` so a pull that lands during
    /// unmount skips its re-entrant `CfUpdatePlaceholder`/`CfCreatePlaceholders` too. Returned as
    /// a clone so the caller can construct the invalidator BEFORE mounting (the engine does).
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutting_down.clone()
    }

    /// **B6.2 — fully remove this library** (the opposite of the offline-by-default `Drop`). Use
    /// this for an explicit "Stop syncing / remove this library" action, NOT on every unmount:
    /// it tears the connection down AND **unregisters the sync root** (`CfUnregisterSyncRoot`), so
    /// no orphaned sync root is left behind (a re-register at the same path would otherwise hit a
    /// stale registration). When `delete_folder` is true it also removes the now-plain folder and
    /// its contents — choose that when the user wants the local copies gone too; pass false to keep
    /// the (now ordinary, fully-hydrated) files on disk.
    ///
    /// Consumes the handle so it can't be used afterward and Drop won't double-disconnect.
    /// Ordering matches the safe teardown: enter shutdown → stop+join the watcher → disconnect →
    /// unregister → (optional) delete. Each step is best-effort + logged; a failure in one doesn't
    /// abort the rest (we still want to unregister even if the folder delete fails, etc.).
    pub fn remove_library(mut self, delete_folder: bool) -> std::io::Result<()> {
        self.shutting_down.store(true, Ordering::Relaxed);
        if let Some(mut w) = self.watcher.take() {
            w.shutdown();
        }
        unsafe {
            // Disconnect first (drops the callback connection) so the unregister can't race a
            // callback. Reclaim the leaked CfState box.
            //
            // FIX (teardown deadlock): take the key out of the mutex and DROP THE GUARD before
            // `CfDisconnectSyncRoot`. `CfDisconnectSyncRoot` blocks until in-flight callbacks drain;
            // `fetch_data_callback` locks this same mutex, so holding the guard across the
            // disconnect deadlocks an unmount that races an in-flight hydration. Binding to a local
            // ends the guard's borrow at the statement's semicolon, before we disconnect.
            let key = (*self.state).connection_key.lock().unwrap().take();
            if let Some(key) = key {
                let _ = CfDisconnectSyncRoot(key);
            }
        }
        // Unregister the sync root so nothing orphaned remains. This is the whole point of the
        // "remove" path vs the offline-keeping Drop.
        let root_w = path_wide(&self.root);
        let unregister = unsafe { CfUnregisterSyncRoot(PCWSTR(root_w.as_ptr())) };
        if let Err(e) = &unregister {
            eprintln!(
                "cfapi: CfUnregisterSyncRoot({}) failed: {e}",
                self.root.display()
            );
        } else {
            eprintln!("cfapi: unregistered sync root {}", self.root.display());
        }
        // Optionally remove the now-plain folder + contents.
        let mut folder_err = None;
        if delete_folder {
            if let Err(e) = std::fs::remove_dir_all(&self.root) {
                eprintln!("cfapi: remove_dir_all({}) failed: {e}", self.root.display());
                folder_err = Some(e);
            } else {
                eprintln!("cfapi: removed library folder {}", self.root.display());
            }
        }
        // Mark torn down + reclaim the state box, so Drop is a no-op.
        self.torn_down = true;
        unsafe {
            drop(Box::from_raw(self.state));
        }
        // Surface a failure (unregister first, then folder delete) so a caller/UI can report it.
        unregister.map_err(win_err)?;
        if let Some(e) = folder_err {
            return Err(e);
        }
        Ok(())
    }
}

impl Drop for CfApiMountHandle {
    fn drop(&mut self) {
        // B6.2: if `remove_library` already tore everything down (disconnected, unregistered, and
        // reclaimed the CfState box), Drop must do nothing — the connection is gone and `state` is
        // a dangling pointer we must not touch or free again.
        if self.torn_down {
            return;
        }
        // Enter teardown (idempotent — the owner usually called begin_shutdown already): from here
        // callbacks still mirror changes into the provider/outbox but skip re-entrant CfAPI calls
        // (convert/set-in-sync/ACK/invalidate), which can deadlock the runtime while the connection
        // is closing — the cause of the graceful-shutdown hang.
        self.shutting_down.store(true, Ordering::Relaxed);
        // Stop + join the new-file watcher FIRST, while `state` is still valid — the watcher
        // thread dereferences it. (shutdown signals the stop event + cancels the overlapped read,
        // then joins with a bounded wait so a wedged callback can't hang teardown.)
        if let Some(mut w) = self.watcher.take() {
            w.shutdown();
        }
        unsafe {
            // Disconnect the provider, but deliberately do NOT unregister the sync root or
            // remove the folder. Hydrated files keep their bytes on disk so the user can
            // "download what they need, then work offline"; the registration persists so a
            // restart reconnects and the placeholders stay valid. (Online-only files still
            // need the daemon running to hydrate; removing a library is a separate action.)
            //
            // FIX (teardown deadlock): release the mutex guard BEFORE `CfDisconnectSyncRoot` — it
            // waits for in-flight callbacks to drain, and `fetch_data_callback` locks this same
            // mutex, so holding it across the disconnect would deadlock unmount-during-hydration.
            let key = (*self.state).connection_key.lock().unwrap().take();
            if let Some(key) = key {
                let _ = CfDisconnectSyncRoot(key);
            }
            drop(Box::from_raw(self.state));
        }
        eprintln!("cfapi: disconnected — sync root + hydrated files kept for offline use");
    }
}

/// Active cache invalidation for CfAPI — the [`sync_engine::Invalidate`]-shaped hook. On a
/// pulled content change, update the placeholder's metadata and **dehydrate** it so the next
/// read re-raises FETCH_DATA and pulls the new manifest. Resolves the on-disk path from the
/// `node_id` via the `Provider` tree.
pub struct CfApiInvalidate {
    provider: Arc<SqliteProvider>,
    root: PathBuf,
    /// B6.4: the ino→path index shared with the mount's `CfState`, populated during
    /// `FETCH_PLACEHOLDERS`. O(1) on a hit; a miss falls back to the [`path_for`](Self::path_for)
    /// tree walk (which then back-fills the index). Replaces the old per-invalidator `paths`
    /// cache so enumeration-time knowledge is reused.
    path_index: Arc<crate::PathIndex>,
    /// Shared with the mount's teardown flag (see [`CfApiMountHandle::shutdown_flag`]). Once set,
    /// `invalidate`/`created` skip their re-entrant CfAPI calls (`CfUpdatePlaceholder` /
    /// `CfDehydratePlaceholder` / `CfCreatePlaceholders`) — issuing those while the connection is
    /// closing deadlocks the runtime, and the sync thread runs the pull (hence these) right up to
    /// shutdown, so without this guard a pull-triggered invalidate could wedge the join.
    shutdown: Arc<AtomicBool>,
    /// Echo guard shared with the mount's `CfState`. `removed`/`moved` `mark` the path(s) they're
    /// about to mutate so the resulting `NOTIFY_DELETE`/`NOTIFY_RENAME` callback suppresses its own
    /// re-capture (no push loop / self-conflict).
    self_removes: Arc<crate::SelfOps>,
}

impl CfApiInvalidate {
    /// `shutdown` should be the mount's teardown flag ([`CfApiMountHandle::shutdown_flag`]) so a
    /// pull that lands during unmount doesn't re-enter the closing connection. A standalone caller
    /// with no mount can pass a fresh `Arc::new(AtomicBool::new(false))`. Makes its own
    /// [`PathIndex`](crate::PathIndex); use [`with_index`](Self::with_index) to share the mount's.
    pub fn new(provider: Arc<SqliteProvider>, root: PathBuf, shutdown: Arc<AtomicBool>) -> Self {
        Self::with_index(
            provider,
            root,
            shutdown,
            Arc::new(crate::PathIndex::new()),
            Arc::new(crate::SelfOps::new()),
        )
    }

    /// Like [`new`](Self::new) but shares the SAME `path_index` + `self_removes` echo guard the
    /// mount's `CfState` uses — so the invalidator gets O(1) path resolution for any node Explorer
    /// has enumerated, and a daemon-initiated delete/move it performs is suppressed by the mount's
    /// NOTIFY callbacks (no echo push loop). The engine wires this; pass the same `Arc`s it handed
    /// to [`mount_full`].
    pub fn with_index(
        provider: Arc<SqliteProvider>,
        root: PathBuf,
        shutdown: Arc<AtomicBool>,
        path_index: Arc<crate::PathIndex>,
        self_removes: Arc<crate::SelfOps>,
    ) -> Self {
        Self {
            provider,
            root,
            path_index,
            shutdown,
            self_removes,
        }
    }

    /// Resolve an ino to its absolute path under the sync root. **O(1)** on an index hit (the
    /// `FETCH_PLACEHOLDERS` enumeration recorded it); on a miss, fall back to a DFS over the
    /// provider tree and back-fill the index so the next lookup is O(1).
    fn path_for(&self, ino: u64) -> Option<PathBuf> {
        // The root maps to the sync-root folder itself — it has no parent and is never recorded
        // by `FETCH_PLACEHOLDERS` (which only inserts *children*), so without this special case
        // `path_for(ROOT_INO)` would miss the index AND never be returned by the DFS below,
        // dropping every root-level node with "parent not enumerated yet".
        if ino == ROOT_INO {
            return Some(self.root.clone());
        }
        if let Some(p) = self.path_index.get(ino) {
            return Some(p);
        }
        // Index miss: DFS from root building paths until we hit `target`. O(tree), but rare now
        // (only for a node never enumerated). Back-fill the index with what we learn.
        let mut stack = vec![(ROOT_INO, self.root.clone())];
        while let Some((dir_ino, path)) = stack.pop() {
            for e in self.provider.readdir(dir_ino).ok()? {
                let child = path.join(&e.name);
                self.path_index.insert(e.ino, child.clone());
                if e.ino == ino {
                    return Some(child);
                }
                if e.kind == Kind::Dir {
                    stack.push((e.ino, child));
                }
            }
        }
        None
    }

    pub fn invalidate(&self, ino: u64) {
        // During teardown, skip the re-entrant CfUpdatePlaceholder/dehydrate — they deadlock a
        // closing connection. (The sync thread runs the pull, hence this, right up to unmount.)
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // path cache can hold a stale entry if a node was renamed; for a content change the
        // name is unchanged, so the cache is fine. Re-resolve on a miss.
        let path = match self.path_for(ino) {
            Some(p) => p,
            None => {
                eprintln!("cfapi: invalidate ino={ino} — no path (not yet enumerated?)");
                return;
            }
        };
        // The new size + mtime to stamp on the placeholder (so it reports the new version).
        let attr = match self.provider.getattr(ino) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("cfapi: invalidate ino={ino} getattr failed: {e}");
                return;
            }
        };
        let t = unix_nanos_to_filetime(attr.mtime) as i64;
        let metadata = CF_FS_METADATA {
            FileSize: attr.size as i64,
            BasicInfo: FILE_BASIC_INFO {
                CreationTime: t,
                LastAccessTime: t,
                LastWriteTime: t,
                ChangeTime: t,
                FileAttributes: FILE_ATTRIBUTE_NORMAL.0,
            },
        };
        let path_w = path_wide(&path);
        unsafe {
            let handle = match CreateFileW(
                PCWSTR(path_w.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_ATTRIBUTE_NORMAL.0),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "cfapi: invalidate ino={ino} open {} failed: {e}",
                        path.display()
                    );
                    return;
                }
            };
            // Update the placeholder to the new version's metadata AND dehydrate the stale
            // data in one call, so the next read re-raises FETCH_DATA and pulls the new content.
            let mut usn: i64 = 0;
            let r = CfUpdatePlaceholder(
                handle,
                Some(&metadata),
                None,
                0,
                None,
                CF_UPDATE_FLAG_DEHYDRATE | CF_UPDATE_FLAG_MARK_IN_SYNC,
                Some(&mut usn),
                None,
            );
            match &r {
                Ok(()) => eprintln!(
                    "cfapi: invalidated ino={ino} size={} — update+dehydrate OK",
                    attr.size
                ),
                Err(e) => {
                    // The combined flag can be rejected depending on the file's state. Fall back
                    // to the dedicated steps: stamp metadata, then dehydrate explicitly — the
                    // dehydrate is what actually forces the re-fetch, so it must run.
                    eprintln!(
                        "cfapi: invalidate ino={ino} update+dehydrate failed: {e} — splitting"
                    );
                    let mut usn2: i64 = 0;
                    let r_meta = CfUpdatePlaceholder(
                        handle,
                        Some(&metadata),
                        None,
                        0,
                        None,
                        CF_UPDATE_FLAG_MARK_IN_SYNC,
                        Some(&mut usn2),
                        None,
                    );
                    eprintln!("cfapi: invalidate ino={ino} metadata update: {r_meta:?}");
                    let r_dehy = CfDehydratePlaceholder(
                        handle,
                        0,
                        attr.size as i64,
                        CF_DEHYDRATE_FLAG_NONE,
                        None,
                    );
                    eprintln!("cfapi: invalidate ino={ino} CfDehydratePlaceholder: {r_dehy:?}");
                }
            }
            let _ = CloseHandle(handle);
        }
    }

    /// A **pull inserted a brand-new node** (an `apply_remote` INSERT). Create an Explorer
    /// placeholder for it in its parent directory via `CfCreatePlaceholders`, so it appears
    /// without the user re-navigating (the parent dir was marked fully populated after its
    /// first FETCH_PLACEHOLDERS, so the platform won't re-enumerate on its own). Best-effort:
    /// any failure is logged, never panics (this runs on the sync-loop thread).
    pub fn created(&self, ino: u64) {
        // During teardown, skip the re-entrant CfCreatePlaceholders (deadlocks a closing
        // connection — the pull that triggers this runs right up to unmount).
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // Resolve the node's parent ino + leaf name (the placeholder is created *in* the
        // parent directory, named by the leaf). Root / unknown → nothing to do.
        let (parent_ino, name) = match self.provider.parent_and_name(ino) {
            Ok(Some(pn)) => pn,
            Ok(None) => {
                eprintln!("cfapi: created ino={ino} — no parent (root/unknown), skipping");
                return;
            }
            Err(e) => {
                eprintln!("cfapi: created ino={ino} parent_and_name failed: {e}");
                return;
            }
        };

        // CfAPI disables on-demand population per directory after its first enumeration, so the OS
        // will NOT re-enumerate a dir that has since gained children — we must materialize them
        // ourselves. And `CfCreatePlaceholders` requires the *parent directory's placeholder to
        // exist on disk*, else `0x80070002`/`0x80070003`. So before creating this node we walk its
        // ancestor chain root-down and create any missing parent-dir placeholder. The root maps to
        // the sync-root folder (always present), so a root-level node has an empty chain and is
        // created directly. Each create is idempotent (an existing placeholder is treated as OK).
        let provider = &self.provider;
        let chain = crate::ancestor_chain(ino, |i| provider.parent_and_name(i).ok().flatten());
        let chain = match chain {
            Some(c) => c,
            None => {
                eprintln!(
                    "cfapi: created ino={ino} — ancestor chain unresolvable (cycle?), skipping"
                );
                return;
            }
        };
        // Create each ancestor dir placeholder top-down, in the root-most dir's parent path.
        // Parent path of the root-most ancestor is always the sync root.
        for (dir_ino, dir_name) in &chain {
            let dir_parent = match self.provider.parent_and_name(*dir_ino) {
                Ok(Some((p, _))) => p,
                _ => ROOT_INO, // a root-level ancestor's parent is the root
            };
            let parent_path = match self.path_for(dir_parent) {
                Some(p) => p,
                None => {
                    eprintln!("cfapi: created ino={ino} — ancestor dir parent ino={dir_parent} unresolved, skipping");
                    return;
                }
            };
            self.create_one_placeholder(*dir_ino, dir_name, &parent_path);
            // Record the now-on-disk dir in the index so the next ancestor / the leaf resolves it.
            self.path_index.insert(*dir_ino, parent_path.join(dir_name));
        }

        // Now the leaf, in its (now-existing) parent directory.
        let parent_path = match self.path_for(parent_ino) {
            Some(p) => p,
            None => {
                eprintln!("cfapi: created ino={ino} — parent ino={parent_ino} unresolved after ancestor pass, skipping");
                return;
            }
        };
        self.create_one_placeholder(ino, &name, &parent_path);
        // Record the leaf too, so a subsequent child (e.g. of a dir we just made) resolves O(1).
        self.path_index.insert(ino, parent_path.join(&name));
    }

    /// A **pull removed a node** (a remote `delete`, or a recursive subtree sweep). Remove the
    /// now-orphaned on-disk placeholder so Explorer stops showing it. The provider row is already
    /// gone (the engine deletes it before calling this), so we resolve the path from the
    /// `path_index` — which still holds the pre-delete path (it's only refreshed by enumeration /
    /// `created`). A subtree delete removes the whole on-disk subtree via `remove_dir_all`.
    ///
    /// **Echo guard:** removing the placeholder fires `NOTIFY_DELETE` (its identity = the node_id),
    /// which would otherwise re-`delete_by_node_id` (a no-op now — the row is gone — but it
    /// re-enqueues the outbox → a spurious `delete` push). We `mark` the path in `self_removes`
    /// first; the callback `take`s it and suppresses itself.
    pub fn removed(&self, ino: u64) {
        // During teardown skip the FS op — the connection is going away and the on-disk file is
        // kept for offline use anyway; a remove racing disconnect can spuriously fire NOTIFY.
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        let path = match self.path_index.get(ino) {
            Some(p) => p,
            None => {
                // Never enumerated → no on-disk placeholder to remove (Explorer never created one).
                eprintln!(
                    "cfapi: removed ino={ino} — no cached path (not enumerated), nothing on disk"
                );
                return;
            }
        };
        // Guard the echo BEFORE touching the filesystem (the NOTIFY can fire synchronously).
        self.self_removes.mark(&path.to_string_lossy());
        let removed = match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            // Already gone (a parent dir's `remove_dir_all` swept it, or the user deleted it) — fine.
            Err(_) => Ok(()),
        };
        match removed {
            Ok(()) => eprintln!("cfapi: removed placeholder ino={ino} at {}", path.display()),
            Err(e) => eprintln!(
                "cfapi: removed ino={ino} remove {} failed: {e}",
                path.display()
            ),
        }
        // Drop the stale index entry regardless (the path no longer maps to this ino).
        self.path_index.invalidate(ino);
    }

    /// A **pull moved/renamed an existing node** (same node_id, new `(parent, name)`). The OLD-path
    /// placeholder is stale and the NEW one doesn't exist yet. Resolve the OLD path from the
    /// `path_index` (still holding the pre-move path — the tree row already has the new location),
    /// create the NEW-path placeholder ([`created`](Self::created), which resolves the new path from
    /// the tree and refreshes the index), then remove the OLD-path placeholder.
    ///
    /// **Echo guard:** both the NEW-path create (`NOTIFY` is not fired for our `CfCreatePlaceholders`)
    /// and the OLD-path remove are daemon-initiated. The remove fires `NOTIFY_DELETE` whose identity
    /// is THIS node_id — which, if mirrored, would `delete_by_node_id` the node we just relocated
    /// (deleting it from the tree at its NEW path!). So we `mark` the OLD path in `self_removes`
    /// before removing; the `NOTIFY_DELETE` callback `take`s it and suppresses itself.
    pub fn moved(&self, ino: u64) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // Capture the OLD path BEFORE `created` refreshes the index to the new path.
        let old_path = self.path_index.get(ino);
        // Is the moved node a directory? (Determines whether we must re-materialize a subtree and
        // whether the old-path removal is recursive.)
        let is_dir = self
            .provider
            .getattr(ino)
            .map(|a| a.kind == Kind::Dir)
            .unwrap_or(false);

        // Create the placeholder at the new location (resolves the new path from the tree, and
        // updates the index entry for `ino` to the new path).
        self.created(ino);

        // FIX (#2): a moved DIRECTORY arrives via `created` as an EMPTY placeholder — its children's
        // provider rows didn't change (so no `created` fires for them) and the new dir placeholder
        // has on-demand population DISABLED, so the platform never re-enumerates it. Materialize the
        // whole subtree at the new location from the provider tree.
        if is_dir {
            if let Some(new_path) = self.path_index.get(ino) {
                self.create_children_recursive(ino, &new_path);
            }
        }

        // Remove the stale OLD-path placeholder, if it was ever materialized on disk.
        if let Some(old) = old_path {
            // If the node didn't actually move on disk (index already matched the new path — e.g. a
            // metadata-only "move" to the same place), `created` reinserted the same path; skip a
            // self-destructive remove of the path we just (re)created.
            if self.path_index.get(ino).as_deref() == Some(old.as_path()) {
                return;
            }
            let removed = match std::fs::metadata(&old) {
                Ok(m) if m.is_dir() => {
                    // FIX (#1): mark EVERY descendant path (and the dir itself) in the echo guard
                    // BEFORE `remove_dir_all`, so the per-child `NOTIFY_DELETE` each on-disk delete
                    // fires is recognized as our own echo and NOT mirrored as a real delete of the
                    // (still-live, just-relocated) child — which would propagate library-wide.
                    self.mark_subtree_for_removal(&old);
                    self.self_removes.mark(&old.to_string_lossy());
                    // FIX (#5): the descendants' index entries embed the OLD ancestor path and are
                    // now stale. `create_children_recursive` already re-inserted the moved subtree
                    // at its new paths, but drop anything still keyed under the old path so no
                    // consumer trusts a stale hit.
                    self.path_index.invalidate_subtree(&old);
                    std::fs::remove_dir_all(&old)
                }
                Ok(_) => {
                    self.self_removes.mark(&old.to_string_lossy());
                    std::fs::remove_file(&old)
                }
                Err(_) => Ok(()), // already gone — fine
            };
            match removed {
                Ok(()) => eprintln!(
                    "cfapi: moved ino={ino} — removed old placeholder at {}",
                    old.display()
                ),
                Err(e) => eprintln!(
                    "cfapi: moved ino={ino} remove old {} failed: {e}",
                    old.display()
                ),
            }
        } else {
            eprintln!("cfapi: moved ino={ino} — no cached old path (not enumerated); new placeholder created");
        }
    }

    /// Recursively materialize placeholders for **every descendant** of the directory `dir_ino`
    /// (whose own placeholder already exists on disk at `dir_path`), reading the subtree from the
    /// provider. Each `create_one_placeholder` sets `DISABLE_ON_DEMAND_POPULATION`, and a
    /// remotely-moved directory arrives via `created` as an EMPTY placeholder the platform will
    /// never re-enumerate — so its contents are missing (FIX #2) unless we create them ourselves.
    /// Records each child in the index at its NEW path so later resolution is O(1) + correct.
    fn create_children_recursive(&self, dir_ino: u64, dir_path: &Path) {
        let children = match self.provider.readdir(dir_ino) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cfapi: moved — readdir(ino={dir_ino}) for subtree failed: {e}");
                return;
            }
        };
        for c in children {
            let child_path = dir_path.join(&c.name);
            self.create_one_placeholder(c.ino, &c.name, dir_path);
            self.path_index.insert(c.ino, child_path.clone());
            if c.kind == Kind::Dir {
                self.create_children_recursive(c.ino, &child_path);
            }
        }
    }

    /// Echo-guard **every path under `dir`** (recursively, on disk) before a `remove_dir_all` that
    /// applies a remote directory move (FIX #1). `remove_dir_all` deletes each child placeholder,
    /// and each delete fires `NOTIFY_DELETE` carrying that CHILD's node_id. Without a mark, the
    /// callback would `delete_by_node_id` the child — but a move left the children's provider rows
    /// intact (only the parent dir moved), so that would DELETE each child's subtree AND push a
    /// spurious `delete` to the server, propagating the loss library-wide. Marking every descendant
    /// path (keyed by the echo guard's normalized path form, which `notify_delete_callback`
    /// consumes) suppresses those self-echoes. Best-effort: an unreadable branch just stops there.
    fn mark_subtree_for_removal(&self, dir: &Path) {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let entries = match std::fs::read_dir(&d) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                self.self_removes.mark(&p.to_string_lossy());
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(p);
                }
            }
        }
    }

    /// Create a single placeholder for `ino` (named `name`, dir or file per its attrs) inside the
    /// already-existing directory `parent_path`. **Idempotent**: if a placeholder already exists at
    /// that path, `CfCreatePlaceholders` returns `0x800700B7` (ERROR_ALREADY_EXISTS) — treated as
    /// success (the proactive path can be re-driven by a re-pull / retry without duplicating work).
    fn create_one_placeholder(&self, ino: u64, name: &str, parent_path: &Path) {
        let attr = match self.provider.getattr(ino) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("cfapi: created ino={ino} getattr failed: {e}");
                return;
            }
        };
        let node_id = self
            .provider
            .node_id_for(ino)
            .ok()
            .flatten()
            .unwrap_or_default();
        let holder = PlaceholderHolder::new(
            name,
            &node_id,
            attr.size,
            attr.mtime,
            attr.kind == Kind::Dir,
        );
        let mut infos = [holder.info()];

        let base_w = path_wide(parent_path);
        let mut processed: u32 = 0;
        // ERROR_ALREADY_EXISTS as an HRESULT (FACILITY_WIN32) — an idempotent re-create. CfAPI can
        // surface it either as the call's HRESULT OR in the per-entry `infos[0].Result`, so check
        // both. A pre-existing placeholder (e.g. a re-pull, or a dir we already made this pass) is
        // success, not a failure.
        const HRESULT_ALREADY_EXISTS: windows::core::HRESULT =
            windows::core::HRESULT(0x8007_00B7u32 as i32);
        unsafe {
            let call = CfCreatePlaceholders(
                PCWSTR(base_w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAG_NONE,
                Some(&mut processed),
            );
            let entry_result = infos[0].Result;
            match call {
                Ok(()) if entry_result.is_ok() || entry_result == HRESULT_ALREADY_EXISTS => eprintln!(
                    "cfapi: created placeholder for ino={ino} name={name} in {} ({processed} processed)",
                    parent_path.display()
                ),
                Ok(()) => eprintln!(
                    "cfapi: created ino={ino} name={name} in {} — per-entry result {entry_result:?}",
                    parent_path.display()
                ),
                Err(e) if e.code() == HRESULT_ALREADY_EXISTS || entry_result == HRESULT_ALREADY_EXISTS => {
                    eprintln!(
                        "cfapi: created ino={ino} name={name} in {} — already a placeholder (ok)",
                        parent_path.display()
                    )
                }
                Err(e) => eprintln!(
                    "cfapi: created ino={ino} CfCreatePlaceholders in {} failed: {e}",
                    parent_path.display()
                ),
            }
        }
    }
}

fn win_err(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("cfapi: {e}"))
}
