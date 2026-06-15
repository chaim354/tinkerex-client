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

use std::collections::HashMap;
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
    /// Echo guard: paths we just converted to placeholders ourselves, each stamped with the
    /// time we inserted it. Our own `CfConvertToPlaceholder` triggers another close, which would
    /// otherwise re-enter the new-file branch; a fresh entry short-circuits that re-entry exactly
    /// once. The timestamp makes it **self-healing**: if the convert-triggered close never
    /// arrives (so the entry is never consumed), a later genuine edit at the same path finds the
    /// entry stale (≥5 s old), drops it, and is captured normally — bounding any leak to a 5 s
    /// window instead of permanently suppressing the next real change.
    self_writes: Mutex<HashMap<String, std::time::Instant>>,
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

fn path_wide(p: &Path) -> U16CString {
    U16CString::from_os_str(p.as_os_str()).expect("path has no interior NUL")
}

/// Register + connect + populate. Returns a handle that tears everything down on drop.
pub fn mount(provider: Arc<SqliteProvider>, root: &Path, hydrate: Option<Hydrate>) -> std::io::Result<CfApiMountHandle> {
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
    mount_full(provider, root, hydrate, shutting_down, Arc::new(crate::PathIndex::new()))
}

/// [`mount_with_shutdown`] plus a caller-supplied [`PathIndex`] (B6.4). Pass the SAME `Arc` to the
/// engine's `CfApiInvalidate` so the index populated by `FETCH_PLACEHOLDERS` here is the one the
/// invalidator reads for O(1) path resolution. The engine wires this; the simpler entry points make
/// their own index (harmless — the invalidator they don't have wouldn't share it anyway).
pub fn mount_full(
    provider: Arc<SqliteProvider>,
    root: &Path,
    hydrate: Option<Hydrate>,
    shutting_down: Arc<AtomicBool>,
    path_index: Arc<crate::PathIndex>,
) -> std::io::Result<CfApiMountHandle> {
    std::fs::create_dir_all(root)?;
    register_sync_root(root)?;

    let state = Box::into_raw(Box::new(CfState {
        provider: provider.clone(),
        hydrate,
        connection_key: Mutex::new(None),
        root: root.to_path_buf(),
        self_writes: Mutex::new(HashMap::new()),
        shutting_down: shutting_down.clone(),
        path_index,
        cancelled_transfers: Mutex::new(std::collections::HashSet::new()),
    }));

    // The callback table must outlive the connection, so leak it (one small leak per mount).
    // Terminated by a CF_CALLBACK_TYPE_NONE entry. On-demand population: the platform raises
    // FETCH_PLACEHOLDERS when Explorer opens a directory, and FETCH_DATA when a file is read.
    let callbacks: &'static [CF_CALLBACK_REGISTRATION] = Box::leak(Box::new([
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, Callback: Some(fetch_placeholders_callback) },
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_DATA, Callback: Some(fetch_data_callback) },
        // B6.3: the kernel cancels an in-flight hydration (reading app gone / file deleted) — mark
        // the transfer key so the streaming FETCH_DATA loop stops between slices.
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, Callback: Some(cancel_fetch_data_callback) },
        // Phase 2: a file handle closed — capture a local edit (content changed) and push it.
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION, Callback: Some(notify_file_close_callback) },
        // Phase 2b: a delete/rename is about to happen — mirror it into the Provider, then ACK
        // (pre-op variants, so we observe + allow the OS op). An unanswered pre-op hangs Explorer.
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NOTIFY_DELETE, Callback: Some(notify_delete_callback) },
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NOTIFY_RENAME, Callback: Some(notify_rename_callback) },
        CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NONE, Callback: None },
    ]));

    let root_w = path_wide(root);
    unsafe {
        let key = CfConnectSyncRoot(
            PCWSTR(root_w.as_ptr()),
            callbacks.as_ptr(),
            Some(state as *const c_void),
            CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
        .map_err(win_err)?;
        (*state).connection_key.lock().unwrap().replace(key);
    }
    eprintln!("cfapi: connected sync root at {}", root.display());

    // Start the new-file watcher (CfAPI has no "file created" callback — see NewFileWatcher).
    // Best-effort: if it can't open the dir handle, log + carry on (new files just won't be
    // captured, but the rest of the write path still works).
    let watcher = match spawn_new_file_watcher(root, state) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("cfapi: new-file watcher failed to start ({e}) — new files won't be captured");
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
        unsafe { let _ = CloseHandle(dir); }
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
    Ok(NewFileWatcher { stop, dir, stop_event, thread: Some(thread) })
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
        ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_NOTIFY_INFORMATION,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

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
        if read.is_err() {
            break; // handle cancelled/closed on shutdown, or a real error — exit
        }
        // Wait for either the read to complete or a stop request.
        let waited = unsafe { WaitForMultipleObjects(&[read_event, stop_event], false, INFINITE) };
        if stop.load(Ordering::Relaxed) || waited != WAIT_OBJECT_0 {
            // Stop signalled (or wait failed): cancel the pending read and exit.
            unsafe { let _ = windows::Win32::System::IO::CancelIoEx(dir, Some(&ov)); }
            break;
        }
        let mut returned: u32 = 0;
        if unsafe { GetOverlappedResult(dir, &ov, &mut returned, false) }.is_err() || returned == 0 {
            continue; // overflow / no info — re-arm
        }
        // Walk the variable-length FILE_NOTIFY_INFORMATION chain.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, returned as usize) };
        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_NOTIFY_INFORMATION>() > bytes.len() {
                break;
            }
            let info = unsafe { &*(bytes.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };
            let name_len = info.FileNameLength as usize / 2; // bytes → u16 count
            let name_ptr = unsafe {
                (bytes.as_ptr().add(offset) as *const u8)
                    .add(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName)) as *const u16
            };
            let name: Vec<u16> = unsafe { std::slice::from_raw_parts(name_ptr, name_len) }.to_vec();
            let rel = String::from_utf16_lossy(&name);
            let action = info.Action;
            // Only ADDED/MODIFIED drive new-file capture. Renames are owned by the NOTIFY_RENAME
            // callback (which mirrors them via rename_by_node_id); treating RENAMED_NEW_NAME here
            // too could race that and create a duplicate node before the rename applied.
            if action == FILE_ACTION_ADDED || action == FILE_ACTION_MODIFIED {
                // Build the absolute path. `capture_new_file` re-maps it to a library-relative
                // path, is idempotent, and skips dirs / placeholders it already tracks.
                if !state.is_null() {
                    let st = unsafe { &*state };
                    let abs = st.root.join(rel.replace('\\', "/").trim_start_matches('/'));
                    let abs_str = abs.to_string_lossy().to_string();
                    // B6.1: a MODIFIED event can be a pin-state toggle (ATTRIBUTES) rather than a
                    // content edit. Reconcile the placeholder's pin state first (hydrate if pinned,
                    // dehydrate if unpinned); it's a no-op for a non-placeholder / unchanged pin.
                    // Then run the content-capture path (idempotent — skips when bytes are equal).
                    unsafe { reconcile_pin_state(st, &abs_str) };
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
    unsafe { let _ = CloseHandle(read_event); }
    eprintln!("cfapi: new-file watcher stopped");
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
        CfRegisterSyncRoot(PCWSTR(root_w.as_ptr()), &registration, &policies, CF_REGISTER_FLAG_UPDATE)
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
        let node_id = provider.node_id_for(c.ino).ok().flatten().unwrap_or_default();
        holders.push(PlaceholderHolder::new(&c.name, &node_id, size, mtime, c.kind == Kind::Dir));
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
        let attrs = if self.is_dir { 0x10 /* FILE_ATTRIBUTE_DIRECTORY */ } else { FILE_ATTRIBUTE_NORMAL.0 };
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
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
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
    let bytes = std::slice::from_raw_parts(info.FileIdentity as *const u8, info.FileIdentityLength as usize);
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
/// hanging to a timeout. `offset` is the kernel's required offset (echoed in the completion).
unsafe fn transfer_data_error(info: &CF_CALLBACK_INFO, offset: i64) {
    let op_info = op_info_for(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut op_params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: STATUS_UNSUCCESSFUL,
                Buffer: std::ptr::null(),
                Offset: offset,
                Length: 0,
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

    // Resolve node + ino, and make its chunks local (presigned GET into the cache). On any failure
    // here we can't produce bytes → one error completion, then bail.
    let node_id = match callback_node_id(info) {
        Some(n) => n,
        None => {
            eprintln!("cfapi: FETCH_DATA — no node identity, reporting error");
            transfer_data_error(info, req_offset);
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
            transfer_data_error(info, req_offset);
            return;
        }
    };
    let file_size = state.provider.getattr(ino).map(|a| a.size).unwrap_or(0);

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
        // Nothing to transfer (e.g. a zero-length read) — answer success with an empty range so
        // the kernel's request completes rather than hanging.
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
        return;
    }

    for chunk in plan {
        // Read just this slice from the provider (bounded memory). A read failure mid-stream → an
        // error completion at this offset so the app's read fails rather than hanging.
        let bytes = match state.provider.read(ino, chunk.offset, chunk.length as u32) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("cfapi: FETCH_DATA node={node_id} read(off={}, len={}) failed: {e}", chunk.offset, chunk.length);
                transfer_data_error(info, chunk.offset as i64);
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
            eprintln!("cfapi: CfExecute(TRANSFER_DATA off={}) failed: {e}", chunk.offset);
            return; // the kernel saw an error on this slice; stop streaming
        }
        // Report progress so Explorer shows a hydration bar for a big file. Best-effort — a
        // failure here is cosmetic and must not abort the transfer.
        completed += bytes.len() as i64;
        if let Some(key) = *state.connection_key.lock().unwrap() {
            let _ = CfReportProviderProgress(key, info.TransferKey, total, completed);
        }

        // B6.3: honor a CANCEL_FETCH_DATA that arrived for this transfer — stop streaming further
        // slices (the kernel already knows the transfer is cancelled; no extra completion needed).
        if state.cancelled_transfers.lock().unwrap().remove(&info.TransferKey) {
            eprintln!("cfapi: FETCH_DATA node={node_id} cancelled mid-stream at off={}", chunk.offset);
            return;
        }
    }
    // Done streaming cleanly — clear any (late) cancel marker for this key so it can't leak.
    state.cancelled_transfers.lock().unwrap().remove(&info.TransferKey);
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
    state.cancelled_transfers.lock().unwrap().insert(info.TransferKey);
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

    // Which directory? Root (null/empty identity) unless the FileIdentity maps to a known node.
    let dir_ino = callback_node_id(info)
        .and_then(|nid| state.provider.ino_for_node_id(&nid).ok().flatten())
        .unwrap_or(ROOT_INO);

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
    eprintln!("cfapi: FETCH_PLACEHOLDERS dir_ino={dir_ino} → {} entries", infos.len());

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
                PlaceholderArray: if infos.is_empty() { std::ptr::null_mut() } else { infos.as_mut_ptr() },
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
    let path = format!("{}{}", pcwstr_string(info.VolumeDosName), pcwstr_string(info.NormalizedPath));

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

    let on_disk = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return, // gone/unreadable (or still a dehydrated placeholder)
    };

    // Compare against the provider's current content; only a real change is a user edit.
    let size = state.provider.getattr(ino).map(|a| a.size).unwrap_or(0);
    let current = state.provider.read(ino, 0, size as u32).ok();
    if current.as_deref() == Some(on_disk.as_slice()) {
        return; // unchanged — our own hydration write or a plain read
    }
    eprintln!("cfapi: local edit captured node={node_id} ({} bytes) → ingest + push", on_disk.len());
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
        let _ = CfSetInSyncState(handle, CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, Some(&mut usn));
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
    // Echo guard (self-healing): if we just converted this path AND the entry is fresh
    // (<5 s old), this close is the convert echo — consume it and skip exactly once. If the
    // entry is stale (≥5 s, e.g. the convert-triggered close never arrived), drop it and fall
    // through so a genuine edit isn't wrongly suppressed. Bounds any leak to a 5 s window.
    {
        let mut seen = state.self_writes.lock().unwrap();
        if let Some(&inserted) = seen.get(path) {
            seen.remove(path);
            if inserted.elapsed() < std::time::Duration::from_secs(5) {
                eprintln!("cfapi: new file {path} — echo of our own convert, suppressed");
                return; // fresh — the convert echo; suppress
            }
            // stale — fall through and capture normally
        }
    }

    // Map the absolute on-disk path back into the library tree.
    let root_str = state.root.to_string_lossy();
    let rel = match library_relative(&root_str, path) {
        Some(r) if !r.is_empty() => r,
        _ => {
            eprintln!("cfapi: new file {path} — not under root {root_str} (or is the root), skipping");
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

    // Read the new file's bytes. A still-dehydrated placeholder / unreadable file → skip.
    let bytes = match std::fs::read(path) {
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
            // echo of our own write, or a plain metadata touch, leaves bytes equal → skip).
            let cur = state.provider.read(a.ino, 0, a.size as u32).ok();
            if cur.as_deref() == Some(bytes.as_slice()) {
                return; // unchanged — nothing to capture (our own write or a no-op touch)
            }
            if let Err(e) = state.provider.ingest_bytes(a.ino, &bytes) {
                eprintln!("cfapi: capture edit ingest_bytes(ino={}) failed: {e}", a.ino);
            } else {
                eprintln!("cfapi: edit captured rel={rel} ino={} ({} bytes)", a.ino, bytes.len());
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
    eprintln!("cfapi: new file captured rel={rel} node={node_id} ({} bytes)", bytes.len());

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
    state.self_writes.lock().unwrap().insert(path.to_string(), std::time::Instant::now());
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
                    state.self_writes.lock().unwrap().remove(path);
                }
            }
            let _ = CloseHandle(handle);
        }
        Err(e) => {
            eprintln!("cfapi: new file open(node={node_id}) failed: {e}");
            state.self_writes.lock().unwrap().remove(path);
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

    if let Some(node_id) = callback_node_id(info) {
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
                        // B6.4: the node's cached path (and, for a moved dir, its whole subtree)
                        // is now stale. Drop the node's entry; subtree children are re-learned on
                        // the next enumeration (a miss falls back to the tree walk meanwhile).
                        if let Ok(Some(ino)) = state.provider.ino_for_node_id(&node_id) {
                            state.path_index.invalidate(ino);
                        }
                        if let Err(e) = state.provider.rename_by_node_id(&node_id, new_parent, &new_name) {
                            eprintln!("cfapi: rename_by_node_id(node={node_id}) failed: {e}");
                        } else {
                            eprintln!("cfapi: rename captured node={node_id} -> {rel}");
                        }
                    }
                    Err(e) => eprintln!("cfapi: rename resolve_path(rel={rel}) failed: {e}"),
                },
                // Target outside the sync root (or the root itself): nothing to mirror as a
                // rename — just ACK below so the OS op isn't blocked.
                _ => eprintln!("cfapi: rename target outside root for node={node_id} — not mirrored"),
            }
        }
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
            if let Some(key) = (*self.state).connection_key.lock().unwrap().take() {
                let _ = CfDisconnectSyncRoot(key);
            }
        }
        // Unregister the sync root so nothing orphaned remains. This is the whole point of the
        // "remove" path vs the offline-keeping Drop.
        let root_w = path_wide(&self.root);
        let unregister = unsafe { CfUnregisterSyncRoot(PCWSTR(root_w.as_ptr())) };
        if let Err(e) = &unregister {
            eprintln!("cfapi: CfUnregisterSyncRoot({}) failed: {e}", self.root.display());
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
        unsafe { drop(Box::from_raw(self.state)); }
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
            if let Some(key) = (*self.state).connection_key.lock().unwrap().take() {
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
}

impl CfApiInvalidate {
    /// `shutdown` should be the mount's teardown flag ([`CfApiMountHandle::shutdown_flag`]) so a
    /// pull that lands during unmount doesn't re-enter the closing connection. A standalone caller
    /// with no mount can pass a fresh `Arc::new(AtomicBool::new(false))`. Makes its own
    /// [`PathIndex`](crate::PathIndex); use [`with_index`](Self::with_index) to share the mount's.
    pub fn new(provider: Arc<SqliteProvider>, root: PathBuf, shutdown: Arc<AtomicBool>) -> Self {
        Self::with_index(provider, root, shutdown, Arc::new(crate::PathIndex::new()))
    }

    /// Like [`new`](Self::new) but shares the SAME `path_index` the mount's `CfState` populates from
    /// `FETCH_PLACEHOLDERS` — so the invalidator gets O(1) path resolution for any node Explorer has
    /// enumerated. The engine wires this; pass the index it also handed to [`mount_full`].
    pub fn with_index(
        provider: Arc<SqliteProvider>,
        root: PathBuf,
        shutdown: Arc<AtomicBool>,
        path_index: Arc<crate::PathIndex>,
    ) -> Self {
        Self { provider, root, path_index, shutdown }
    }

    /// Resolve an ino to its absolute path under the sync root. **O(1)** on an index hit (the
    /// `FETCH_PLACEHOLDERS` enumeration recorded it); on a miss, fall back to a DFS over the
    /// provider tree and back-fill the index so the next lookup is O(1).
    fn path_for(&self, ino: u64) -> Option<PathBuf> {
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
                    eprintln!("cfapi: invalidate ino={ino} open {} failed: {e}", path.display());
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
                Ok(()) => eprintln!("cfapi: invalidated ino={ino} size={} — update+dehydrate OK", attr.size),
                Err(e) => {
                    // The combined flag can be rejected depending on the file's state. Fall back
                    // to the dedicated steps: stamp metadata, then dehydrate explicitly — the
                    // dehydrate is what actually forces the re-fetch, so it must run.
                    eprintln!("cfapi: invalidate ino={ino} update+dehydrate failed: {e} — splitting");
                    let mut usn2: i64 = 0;
                    let r_meta = CfUpdatePlaceholder(
                        handle, Some(&metadata), None, 0, None, CF_UPDATE_FLAG_MARK_IN_SYNC, Some(&mut usn2), None,
                    );
                    eprintln!("cfapi: invalidate ino={ino} metadata update: {r_meta:?}");
                    let r_dehy = CfDehydratePlaceholder(handle, 0, attr.size as i64, CF_DEHYDRATE_FLAG_NONE, None);
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
        // The parent's on-disk path. If the parent hasn't been enumerated yet there's no
        // placeholder for it, so a later FETCH_PLACEHOLDERS will surface this child anyway —
        // skip quietly.
        let parent_path = match self.path_for(parent_ino) {
            Some(p) => p,
            None => {
                eprintln!("cfapi: created ino={ino} — parent ino={parent_ino} not enumerated yet, skipping");
                return;
            }
        };
        // Build the one placeholder from the node's attrs + identity.
        let attr = match self.provider.getattr(ino) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("cfapi: created ino={ino} getattr failed: {e}");
                return;
            }
        };
        let node_id = self.provider.node_id_for(ino).ok().flatten().unwrap_or_default();
        let holder =
            PlaceholderHolder::new(&name, &node_id, attr.size, attr.mtime, attr.kind == Kind::Dir);
        let mut infos = [holder.info()];

        let base_w = path_wide(&parent_path);
        let mut processed: u32 = 0;
        unsafe {
            match CfCreatePlaceholders(
                PCWSTR(base_w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAG_NONE,
                Some(&mut processed),
            ) {
                Ok(()) => eprintln!(
                    "cfapi: created placeholder for ino={ino} name={name} in {} ({processed} processed)",
                    parent_path.display()
                ),
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
