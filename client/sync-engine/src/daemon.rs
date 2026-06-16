//! Supervised background daemon: one call mounts the filesystem **and** runs the sync
//! loop + the ActionCable nudge, all sharing a single `Arc<SqliteProvider>`.
//!
//! Unlike the raw `fuser::mount2` (which blocks the calling thread), [`Daemon::start`]
//! uses `spawn_mount2` so the FUSE session runs on its own thread — the daemon is
//! non-blocking and can be `start()`/`stop()`'d by a host with its own event loop (the
//! Tauri app). The `maker-syncd` CLI calls [`Daemon::run_until_unmounted`] to block.
//!
//! Three threads, one provider:
//! - the FUSE session thread (kernel callbacks → `fs-provider`),
//! - the sync thread (`sync_tick` on a nudge or the poll fallback),
//! - the nudge thread (`LibraryChannel` `{head_seq}` → wake the sync thread).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fs_provider::{LocalChunkStore, SqliteProvider};
use sync_client::ServerClient;

use crate::{ChunkIO, PresignedIO, SyncEngine, ThrottledChunkIO};

// FUSE bits — Linux only (macOS → WebDAV, Windows → WinFsp).
#[cfg(target_os = "linux")]
use crate::EngineHydrator;
#[cfg(target_os = "linux")]
use crate::{EngineInvalidate, Invalidate};
#[cfg(target_os = "linux")]
use fs_fuse::FuseAdapter;
#[cfg(target_os = "linux")]
use fuser::MountOption;

/// Default poll fallback interval (the nudge is the fast path; this bounds staleness if
/// the WebSocket is down).
pub const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// Everything the daemon needs to mount + sync one library on this device.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Where to expose the filesystem (a directory on macOS/Linux).
    pub mountpoint: PathBuf,
    /// Local SQLite tree (created if absent).
    pub db: PathBuf,
    /// Local content-addressed chunk cache dir (created if absent).
    pub chunk_dir: PathBuf,
    /// Rails sync server base URL, e.g. `http://localhost:3001`.
    pub server_url: String,
    /// This device's bearer token (from registration).
    pub token: String,
    /// Poll fallback interval.
    pub poll: Duration,
    /// Localhost port for the WebDAV mount path (macOS). Ignored by the FUSE `Daemon`.
    pub webdav_port: u16,
    /// Optional client-side **upload** bandwidth cap in bytes/sec (`None`/`0` = unlimited).
    /// Applied to chunk PUTs via [`ThrottledChunkIO`](crate::ThrottledChunkIO).
    pub up_bps: Option<u64>,
    /// Optional client-side **download** bandwidth cap in bytes/sec (`None`/`0` = unlimited).
    /// Applied to chunk GETs via [`ThrottledChunkIO`](crate::ThrottledChunkIO).
    pub down_bps: Option<u64>,
    /// G5 outbox debounce/coalesce window (default disabled). See [`Debounce`].
    pub debounce: Debounce,
    /// How many chunk fetches to run **concurrently** when hydrating a file (a bounded worker
    /// pool over the pooled HTTP client — see
    /// [`SyncEngine::hydrate_concurrent`](crate::SyncEngine::hydrate_concurrent)). The serial
    /// per-chunk round-trip to object storage (~80 ms to R2) is the data-plane bottleneck;
    /// `P` in flight cuts a cold hydrate to ≈ serial/`P`. Defaults to
    /// [`DEFAULT_HYDRATE_CONCURRENCY`]. `0`/`1` = serial (one chunk at a time).
    pub hydrate_concurrency: usize,
}

/// **G5 outbox debounce** parameters (millis). A push of an `upsert` is held while the node
/// is still being saved actively (`window_ms` quiescence) but never longer than `max_age_ms`
/// (the cap, so a continuously re-saved file still syncs). `window_ms == 0` disables it.
/// Default = disabled (eager push, the pre-G5 behavior); opt in via
/// [`DaemonConfig::with_debounce`].
#[derive(Clone, Copy, Debug)]
pub struct Debounce {
    pub window_ms: i64,
    pub max_age_ms: i64,
}

impl Default for Debounce {
    fn default() -> Self {
        // Disabled by default — push promptly. Opt-in to the 2-3s coalesce window.
        Self {
            window_ms: 0,
            max_age_ms: 0,
        }
    }
}

impl Debounce {
    /// The recommended maker-workload preset: a 2.5 s coalesce window with a 30 s cap
    /// (`docs/torn-writes-and-batching.md` §4 — D = 2-3 s, M = 30 s).
    pub const MAKER_DEFAULT: Debounce = Debounce {
        window_ms: 2_500,
        max_age_ms: 30_000,
    };
}

/// Default chunk-fetch concurrency for hydration. 8 in-flight GETs balances throughput
/// against per-host connection limits (reqwest's pool defaults) and memory (we hold at most
/// `P` chunk buffers at once). Tune via [`DaemonConfig::with_hydrate_concurrency`].
pub const DEFAULT_HYDRATE_CONCURRENCY: usize = 8;

/// A shared pair of token-bucket limiters (upload + download), built once per `Daemon` and
/// cloned (`Arc`) into the sync loop so the cap is **global per direction**. Either side
/// `None` = unlimited for that direction. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct TransferLimits {
    pub up: Option<Arc<crate::RateLimiter>>,
    pub down: Option<Arc<crate::RateLimiter>>,
}

impl TransferLimits {
    /// Build from optional byte/sec caps. `None`/`Some(0)` on a side → unlimited there.
    fn from_caps(up_bps: Option<u64>, down_bps: Option<u64>) -> Self {
        let mk = |b: Option<u64>| {
            b.filter(|&b| b > 0)
                .map(|b| Arc::new(crate::RateLimiter::new(b)))
        };
        Self {
            up: mk(up_bps),
            down: mk(down_bps),
        }
    }

    /// Wrap a transport with these limiters (a no-op pass-through when both are unlimited).
    pub(crate) fn wrap<I: ChunkIO>(&self, io: I) -> ThrottledChunkIO<I> {
        ThrottledChunkIO::new(io, self.up.clone(), self.down.clone())
    }
}

impl DaemonConfig {
    pub fn new(
        mountpoint: impl Into<PathBuf>,
        db: impl Into<PathBuf>,
        chunk_dir: impl Into<PathBuf>,
        server_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            mountpoint: mountpoint.into(),
            db: db.into(),
            chunk_dir: chunk_dir.into(),
            server_url: server_url.into(),
            token: token.into(),
            poll: DEFAULT_POLL,
            webdav_port: 9988,
            up_bps: None,
            down_bps: None,
            debounce: Debounce::default(),
            hydrate_concurrency: DEFAULT_HYDRATE_CONCURRENCY,
        }
    }

    /// Enable the G5 outbox debounce (coalesce rapid saves into one push). Returns `self`
    /// for chaining: `DaemonConfig::new(..).with_debounce(Debounce::MAKER_DEFAULT)`.
    pub fn with_debounce(mut self, debounce: Debounce) -> Self {
        self.debounce = debounce;
        self
    }

    /// Set the chunk-fetch concurrency for hydration (`0`/`1` = serial). Returns `self`.
    pub fn with_hydrate_concurrency(mut self, concurrency: usize) -> Self {
        self.hydrate_concurrency = concurrency;
        self
    }

    /// Set client-side bandwidth caps in bytes/sec (`None`/`0` = unlimited). Returns `self`
    /// for chaining: `DaemonConfig::new(..).with_bandwidth(Some(1_000_000), None)`.
    pub fn with_bandwidth(mut self, up_bps: Option<u64>, down_bps: Option<u64>) -> Self {
        self.up_bps = up_bps;
        self.down_bps = down_bps;
        self
    }

    pub(crate) fn limits(&self) -> TransferLimits {
        TransferLimits::from_caps(self.up_bps, self.down_bps)
    }
}

/// A running FUSE mount + its two background loops (Linux). Drop (or
/// [`stop`](Self::stop)) unmounts and tears the loops down. macOS uses [`WebdavMount`],
/// Windows uses [`WinfspMount`].
#[cfg(target_os = "linux")]
pub struct Daemon {
    stop: Arc<AtomicBool>,
    session: Option<fuser::BackgroundSession>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

#[cfg(target_os = "linux")]
impl Daemon {
    /// Mount and start syncing. Returns once the mount is live (non-blocking). The nudge
    /// thread is detached (a dropped socket just falls back to the poll; it's reaped on
    /// process exit / when the receiver drops at stop).
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        // Bind the local DB to this (server, token) library; if it last served a DIFFERENT
        // library, wipe its stale tree/cursor so a reused DB path can't mix two (the chunk
        // cache is kept — content-addressed, so hydrated files stay available offline).
        if provider
            .bind_library(library_id_for(&cfg.server_url, &cfg.token))
            .unwrap_or(false)
        {
            eprintln!("sync: local DB was a different library — wiped, re-syncing fresh");
        }
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        let stop = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Active cache-invalidation hook. Shared by the sync loop + the freshness adapter so
        // whichever pull applies a content change is the one that drops the kernel cache.
        // Its FUSE notifier is injected after the mount is up (set_notifier below) — see
        // EngineInvalidate's docs for the construction-cycle break.
        let invalidate = EngineInvalidate::new();
        let inval_dyn: Arc<dyn Invalidate> = Arc::new(invalidate.clone());

        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let inval = inval_dyn.clone();
            let limits = limits.clone();
            let debounce = cfg.debounce;
            thread::spawn(move || {
                sync_loop(
                    provider,
                    client,
                    stop,
                    nudge_rx,
                    poll,
                    Some(inval),
                    None,
                    limits,
                    debounce,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx));
        }

        let hydrator = Arc::new(EngineHydrator::with_concurrency(
            provider.clone(),
            client.clone(),
            cfg.hydrate_concurrency,
        ));
        let freshness = Arc::new(crate::EngineFreshness::new(
            provider.clone(),
            client.clone(),
            Some(inval_dyn.clone()),
        ));
        let remote_lock = Arc::new(crate::EngineRemoteLock::new(
            provider.clone(),
            client.clone(),
        ));
        let fs = FuseAdapter::with_hydrator(provider.clone(), hydrator)
            .with_freshness(freshness)
            .with_remote_lock(remote_lock);
        // NOTE: do NOT add MountOption::AutoUnmount. With spawn_mount2 (background session) it
        // tears the mount down IMMEDIATELY after a successful mount on many libfuse setups (the
        // auto-unmount fusermount helper can't track the spawned session's lifetime). We already
        // unmount gracefully when the BackgroundSession guard drops in stop()/Drop, so
        // AutoUnmount only bought SIGKILL crash-cleanup — not worth a mount that won't stay up.
        // (The real instant-drop culprit was `run_until_unmounted` calling `umount_and_join()`,
        // which actively unmounts; see that method. Fixed to `join()`.)
        // fuser 0.17: spawn_mount2 takes a &fuser::Config (was a &[MountOption] slice).
        // Config is #[non_exhaustive] (external crate), so a struct literal is forbidden even
        // with ..Default::default(); start from default() and set the public field.
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::FSName("maker".into()),
            MountOption::DefaultPermissions,
        ];
        // spawn_mount2 runs the FUSE session loop on its own thread and returns a handle
        // that unmounts on drop — the non-blocking counterpart to mount2.
        let session = fuser::spawn_mount2(fs, &cfg.mountpoint, &config)?;
        // Now the session exists, hand its notifier to the invalidation hook (both the sync
        // loop and the freshness adapter share it; before this they no-op harmlessly).
        invalidate.set_notifier(session.notifier());

        // POST-MOUNT LIVENESS CHECK (observability hardening — see maker-syncd).
        // `spawn_mount2` returns Ok the moment the worker thread is *spawned*, NOT once the
        // kernel mount is confirmed. A broken mount (e.g. the fuser-0.14 ENODEV-on-first-read
        // bug, or a kernel/driver mismatch) lets the worker thread exit almost immediately
        // while `start` still returns Ok — the original silent failure. Give the worker a beat
        // to either settle (stay alive) or die, then check `guard.is_finished()`. If it already
        // finished, the mount did NOT attach: surface a real error instead of a happy `Ok`.
        thread::sleep(Duration::from_millis(300));
        if session.guard.is_finished() {
            return Err(std::io::Error::other(format!(
                "FUSE mount did not attach at {} — the session worker exited immediately. \
                 Check `dmesg`, the fuser version (need >=0.17; 0.14 fails silently on modern \
                 kernels), /dev/fuse permissions, and that fusermount3 is setuid.",
                cfg.mountpoint.display()
            ))
            .into());
        }

        Ok(Self {
            stop,
            session: Some(session),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// True while the FUSE session thread is still serving.
    pub fn is_running(&self) -> bool {
        match &self.session {
            Some(s) => !s.guard.is_finished(),
            None => false,
        }
    }

    /// Unmount and stop both loops. Idempotent (also runs on `Drop`).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Dropping the session unmounts (its `_mount` guard); we don't `join()` it to
        // avoid panicking if the FUSE worker returned an error during teardown.
        self.session.take();
        if let Some(h) = self.sync_handle.take() {
            let _ = h.join();
        }
    }

    /// Block until the mount ends (Ctrl-C / external `umount`), then stop the loops. For the
    /// CLI; a GUI host uses [`start`](Self::start) + [`stop`](Self::stop). Returns the FUSE
    /// session's worker result so the caller can surface a teardown/worker error.
    ///
    /// **The bug this fixes (mount dropped instantly on Linux):** fuser 0.17 has two
    /// `BackgroundSession` finishers with OPPOSITE semantics:
    ///   - `umount_and_join()` — **actively unmounts the filesystem FIRST**, then joins the
    ///     worker. Calling it to "wait for unmount" instead *triggers* the unmount immediately:
    ///     the daemon mounted, printed "mounted at", then unmounted itself the same instant
    ///     (`findmnt` empty, "sync loop stopped"). That was the whole silent-drop symptom.
    ///   - `join()` — **just waits** for the worker thread to exit on its own (an external
    ///     `umount`/`fusermount -u`, Ctrl-C, or `stop()`/`Drop`), keeping the mount live until
    ///     then, and only unmounts when its `mount` guard drops afterwards.
    /// We want the latter: hold the mount up and block until something *else* ends it. (The
    /// 0.14 `join()` we ported from had the wait-only semantics; 0.17 split them and the
    /// migration grabbed the wrong one.)
    pub fn run_until_unmounted(mut self) -> std::io::Result<()> {
        let r = if let Some(session) = self.session.take() {
            // Wait for the worker to exit on its own — do NOT proactively unmount. The mount
            // stays live until an external umount/Ctrl-C ends the worker; `session` (and its
            // `mount` guard) then drops here, unmounting cleanly.
            session.join()
        } else {
            Ok(())
        };
        self.stop();
        r
    }
}

#[cfg(target_os = "linux")]
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The **WebDAV** mount path — serves the provider over localhost HTTP and mounts it
/// with the OS's native WebDAV client, instead of FUSE. This is the macOS path (FUSE-T
/// can't mount on macOS 26+); the sync + nudge loops are identical to [`Daemon`]. Same
/// `start`/`stop`/`is_running`/`mountpoint` shape, so a host can alias one or the other
/// per platform.
pub struct WebdavMount {
    stop: Arc<AtomicBool>,
    rt: Option<tokio::runtime::Runtime>, // owns the WebDAV server task; drop = shutdown
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

impl WebdavMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        // Bind the local DB to this (server, token) library; if it last served a DIFFERENT
        // library, wipe its stale tree/cursor so a reused DB path can't mix two (the chunk
        // cache is kept — content-addressed, so hydrated files stay available offline).
        if provider
            .bind_library(library_id_for(&cfg.server_url, &cfg.token))
            .unwrap_or(false)
        {
            eprintln!("sync: local DB was a different library — wiped, re-syncing fresh");
        }
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        let stop = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Same sync + nudge threads as the FUSE Daemon.
        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let debounce = cfg.debounce;
            thread::spawn(move || {
                sync_loop(
                    provider, client, stop, nudge_rx, poll, None, None, limits, debounce,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx));
        }

        // Hydrate-on-read: resolve the WebDAV ino → node_id and pull missing chunks.
        let hydrate: fs_webdav::Hydrate = {
            let p = provider.clone();
            let c = client.clone();
            let concurrency = cfg.hydrate_concurrency;
            Arc::new(move |ino: u64| {
                if let Ok(Some(node_id)) = p.node_id_for(ino) {
                    let io = PresignedIO::new(&c);
                    if let Err(e) =
                        SyncEngine::hydrate_concurrent(&p, &c, &io, &node_id, concurrency)
                    {
                        eprintln!("webdav hydrate ino {ino}: {e}");
                    }
                }
            })
        };

        // WebDAV server on its own multi-thread runtime (dropped on stop → shutdown).
        let addr: SocketAddr = ([127, 0, 0, 1], cfg.webdav_port).into();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.spawn(fs_webdav::serve(provider, addr, Some(hydrate)));

        // Mount via the native client once the server is listening.
        std::fs::create_dir_all(&cfg.mountpoint)?;
        thread::sleep(Duration::from_millis(400));
        mount_native(&addr, &cfg.mountpoint)?;
        println!(
            "WebDAV mounted at {} (http://{addr}/)",
            cfg.mountpoint.display()
        );

        Ok(Self {
            stop,
            rt: Some(rt),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        unmount_native(&self.mountpoint);
        self.rt.take(); // drop the runtime → WebDAV server task is cancelled
        if let Some(h) = self.sync_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WebdavMount {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
fn mount_native(addr: &SocketAddr, mp: &Path) -> std::io::Result<()> {
    let status = Command::new("/sbin/mount_webdav")
        .args(["-S", "-v", "Maker", &format!("http://{addr}/")])
        .arg(mp)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "mount_webdav exited {status}"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn mount_native(_addr: &SocketAddr, _mp: &Path) -> std::io::Result<()> {
    // Non-macOS: the server is up; mount with the OS's WebDAV client (gio/davfs/net use).
    Ok(())
}

fn unmount_native(mp: &Path) {
    let _ = Command::new("/sbin/umount").arg(mp).status();
}

/// The **WinFsp** mount path (Windows) — a drive letter via `fs-winfsp`, same
/// `start`/`stop`/`is_running`/`mountpoint` API as the FUSE/WebDAV backends. The WinFsp
/// host handle isn't `Send`, so it lives on its own thread; the sync + nudge loops are
/// identical to the others.
#[cfg(target_os = "windows")]
pub struct WinfspMount {
    stop: Arc<AtomicBool>,
    fs_thread: Option<JoinHandle<()>>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

#[cfg(target_os = "windows")]
impl WinfspMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        // Bind the local DB to this (server, token) library; if it last served a DIFFERENT
        // library, wipe its stale tree/cursor so a reused DB path can't mix two (the chunk
        // cache is kept — content-addressed, so hydrated files stay available offline).
        if provider
            .bind_library(library_id_for(&cfg.server_url, &cfg.token))
            .unwrap_or(false)
        {
            eprintln!("sync: local DB was a different library — wiped, re-syncing fresh");
        }
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        let stop = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Same sync + nudge threads as the other backends.
        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let debounce = cfg.debounce;
            thread::spawn(move || {
                sync_loop(
                    provider, client, stop, nudge_rx, poll, None, None, limits, debounce,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx));
        }

        // Hydrate-on-read: WinFsp ino → node_id → pull missing chunks.
        let hydrate: fs_winfsp::Hydrate = {
            let p = provider.clone();
            let c = client.clone();
            let concurrency = cfg.hydrate_concurrency;
            Arc::new(move |ino: u64| {
                if let Ok(Some(node_id)) = p.node_id_for(ino) {
                    let io = PresignedIO::new(&c);
                    if let Err(e) =
                        SyncEngine::hydrate_concurrent(&p, &c, &io, &node_id, concurrency)
                    {
                        eprintln!("winfsp hydrate ino {ino}: {e}");
                    }
                }
            })
        };

        // The WinFsp `Mounted` handle isn't `Send`, so create/hold/drop it on a dedicated
        // thread. It reports mount success/failure back so `start` can surface errors.
        let mountpoint = cfg.mountpoint.to_string_lossy().to_string();
        let (ready_tx, ready_rx) = mpsc::channel::<crate::Result<()>>();
        let fs_thread = {
            let provider = provider.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                match fs_winfsp::start_mount(provider, &mountpoint, Some(hydrate)) {
                    Ok(mounted) => {
                        let _ = ready_tx.send(Ok(()));
                        while !stop.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(150));
                        }
                        drop(mounted); // unmount on stop
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(std::io::Error::other(format!(
                            "winfsp mount failed: {e:?}"
                        ))
                        .into()));
                    }
                }
            })
        };

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(std::io::Error::other("winfsp mount thread exited").into());
            }
        }
        println!("WinFsp mounted at {}", cfg.mountpoint.display());

        Ok(Self {
            stop,
            fs_thread: Some(fs_thread),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.fs_thread.take() {
            let _ = h.join(); // unmounts on the way out
        }
        if let Some(h) = self.sync_handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for WinfspMount {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Adapts `fs_cfapi::CfApiInvalidate` to the engine's [`crate::Invalidate`] seam: a pulled
/// content change → `CfUpdatePlaceholder` + dehydrate, so the next read re-fetches (the CfAPI
/// analogue of FUSE `notify_inval_inode`). (The trait lives in the engine, so the adapter
/// crate can't implement it directly — this newtype bridges them.)
#[cfg(target_os = "windows")]
struct EngineCfInvalidate(fs_cfapi::CfApiInvalidate);

#[cfg(target_os = "windows")]
impl crate::Invalidate for EngineCfInvalidate {
    fn invalidate(&self, ino: u64) {
        self.0.invalidate(ino);
    }
    fn created(&self, ino: u64) {
        self.0.created(ino);
    }
    fn removed(&self, ino: u64) {
        self.0.removed(ino);
    }
    fn moved(&self, ino: u64) {
        self.0.moved(ino);
    }
}

/// The **CfAPI** mount path (Windows) — the driverless successor to [`WinfspMount`] (the
/// OneDrive model: a registered sync-root folder, on-demand hydration, native Explorer
/// integration; no GPL driver). Same `start`/`stop`/`is_running`/`mountpoint` shape so a host
/// can alias it in place of WinFsp. Phase 1: populate + hydrate + invalidate (the download
/// half); local-edit capture is Phase 2 (see `docs/plan-windows-cfapi-migration.md`).
///
/// Unlike WinFsp, the CfAPI callbacks run on the OS's own threads against a registered root,
/// so there's no FUSE-style session thread — we hold the `CfApiMountHandle` (Drop disconnects
/// + unregisters). The sync + nudge loops are identical to the other backends.
#[cfg(target_os = "windows")]
pub struct CfApiMount {
    stop: Arc<AtomicBool>,
    handle: Option<fs_cfapi::CfApiMountHandle>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
    // Kept for the post-disconnect shutdown drain (see `stop`): after the sync thread is
    // joined and the sync root disconnected (which flushes CfAPI's NOTIFY callbacks into the
    // outbox), we push whatever those callbacks captured — on the main thread, lock-contention-free.
    provider: Arc<SqliteProvider>,
    client: ServerClient,
    // Serializes the sync loop's push against the shutdown drain so they never push the same
    // outbox node concurrently (which would self-conflict on the second push's stale base version).
    push_gate: Arc<std::sync::Mutex<()>>,
    // Bandwidth caps (shared with the sync loop) so the post-disconnect drain throttles too.
    limits: TransferLimits,
}

#[cfg(target_os = "windows")]
impl CfApiMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        // Bind the local DB to this (server, token) library; if it last served a DIFFERENT
        // library, wipe its stale tree/cursor so a reused DB path can't mix two (the chunk
        // cache is kept — content-addressed, so hydrated files stay available offline).
        if provider
            .bind_library(library_id_for(&cfg.server_url, &cfg.token))
            .unwrap_or(false)
        {
            eprintln!("sync: local DB was a different library — wiped, re-syncing fresh");
        }
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        let stop = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Pull the current tree before mounting (so the first enumeration shows the library)
        // and **reset the cursor to the snapshot head**. This must happen before the sync
        // thread starts (it reads the cursor once). A reused DB can carry a cursor from a
        // *different/larger* library that's ahead of this one's head_seq — then the delta pull
        // ("changes after <stale cursor>") returns nothing and remote edits never apply.
        // Snapshotting + resetting the cursor reconciles that.
        match SyncEngine::pull_snapshot(&provider, &client) {
            Ok(head) => {
                let _ = provider.set_cursor(head);
                println!("cfapi: initial pull complete (head_seq {head}, cursor reset)");
            }
            Err(e) => {
                eprintln!("cfapi: initial pull failed (will populate as the loop catches up): {e}")
            }
        }

        // ONE teardown flag shared by the mount's callbacks AND the invalidator. The sync loop's
        // pull calls `invalidate`/`created` (which re-enter CfAPI: CfUpdatePlaceholder /
        // CfCreatePlaceholders) right up to unmount; without a shared flag a pull landing during
        // disconnect would deadlock the closing connection and wedge the shutdown join. We create
        // it here, hand it to both the invalidator and the mount, and flip it in stop().
        let shutting_down = Arc::new(AtomicBool::new(false));

        // B6.4: one ino→path index shared by the mount's FETCH_PLACEHOLDERS (populates it) and the
        // invalidator (reads it for O(1) path resolution; drops stale entries on rename/delete).
        let path_index = Arc::new(fs_cfapi::PathIndex::new());

        // Echo guard shared by the invalidator (it `mark`s the path before a daemon-applied
        // remote delete/move on disk) and the mount's NOTIFY_DELETE/RENAME callbacks (they `take`
        // it and suppress re-capturing the daemon's own FS op — no push loop / self-conflict).
        let self_removes = Arc::new(fs_cfapi::SelfOps::new());

        // Cache-invalidation hook over CfUpdatePlaceholder (no construction cycle here — CfAPI
        // resolves paths + opens handles itself, so it needs no post-mount notifier). Share the
        // path index + echo guard so it benefits from enumeration-time path knowledge and its
        // delete/move FS ops are recognized as self-echoes by the mount's NOTIFY callbacks.
        let inval: Arc<dyn crate::Invalidate> =
            Arc::new(EngineCfInvalidate(fs_cfapi::CfApiInvalidate::with_index(
                provider.clone(),
                cfg.mountpoint.clone(),
                shutting_down.clone(),
                path_index.clone(),
                self_removes.clone(),
            )));

        // Serialize the sync loop's push against the shutdown drain (see `push_gate` field).
        let push_gate = Arc::new(std::sync::Mutex::new(()));

        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let inval = inval.clone();
            let push_gate = push_gate.clone();
            let limits = limits.clone();
            let debounce = cfg.debounce;
            thread::spawn(move || {
                sync_loop(
                    provider,
                    client,
                    stop,
                    nudge_rx,
                    poll,
                    Some(inval),
                    Some(push_gate),
                    limits,
                    debounce,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx));
        }

        // Hydrate-on-FETCH_DATA: CfAPI hands us the node_id (placeholder FileIdentity) + the
        // kernel's required byte range; pull only the chunks that range overlaps via presigned
        // GET (progressive hydration — a header read of a huge file fetches a handful of chunks,
        // not the whole file). A pin→"keep on device" raises FETCH_DATA for the full range, so a
        // pinned file still hydrates completely.
        let hydrate: fs_cfapi::Hydrate = {
            let p = provider.clone();
            let c = client.clone();
            let concurrency = cfg.hydrate_concurrency;
            Arc::new(move |node_id: &str, offset: u64, length: u64| {
                let io = PresignedIO::new(&c);
                if let Err(e) = SyncEngine::hydrate_range_concurrent(
                    &p,
                    &c,
                    &io,
                    node_id,
                    offset,
                    length,
                    concurrency,
                ) {
                    eprintln!("cfapi hydrate_range {node_id} off={offset} len={length}: {e}");
                }
            })
        };

        // Register + connect (on-demand population via FETCH_PLACEHOLDERS). The handle keeps
        // the sync root alive. Share the SAME teardown flag the invalidator holds AND the same
        // ino→path index it populates from FETCH_PLACEHOLDERS (B6.4).
        let handle = fs_cfapi::win::mount_full(
            provider.clone(),
            &cfg.mountpoint,
            Some(hydrate),
            shutting_down.clone(),
            path_index,
            self_removes,
        )?;
        println!("CfAPI sync root at {}", cfg.mountpoint.display());

        Ok(Self {
            stop,
            handle: Some(handle),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
            provider,
            client,
            push_gate,
            limits,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_running())
    }

    /// **B6.2 — fully remove this library** (explicit "Stop syncing / remove this library" action,
    /// NOT the offline-keeping `stop`/`Drop`). Stops the sync threads, drains the outbox so a
    /// just-made local change still pushes, then unregisters the CfAPI sync root via
    /// [`CfApiMountHandle::remove_library`] (leaving no orphaned sync root). `delete_folder` also
    /// removes the local folder + its (now plain) files. Consumes the mount.
    pub fn remove_library(mut self, delete_folder: bool) -> crate::Result<()> {
        // Same ordered, deadlock-free teardown as `stop`, but the handle is *removed* not dropped.
        if let Some(h) = &self.handle {
            h.begin_shutdown();
        }
        self.stop.store(true, Ordering::Relaxed);
        let handle = self.handle.take();
        // Drain the outbox (push the final captured change) before we unregister, mirroring `stop`.
        self.shutdown_drain();
        if let Some(h) = self.sync_handle.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        if let Some(h) = handle {
            h.remove_library(delete_folder)?; // std::io::Error → crate::Error::Io via #[from]
        }
        eprintln!(
            "cfapi: library removed (sync root unregistered{})",
            if delete_folder {
                " + folder deleted"
            } else {
                ""
            }
        );
        Ok(())
    }

    pub fn stop(&mut self) {
        // Ordered teardown that's deadlock-free AND can't drop the captured edit/delete/rename,
        // even though CfAPI delivers its batched NOTIFY callbacks at unmount and the sync thread's
        // push can be mid-HTTP at that moment:
        //
        // 1. Enter teardown mode (begin_shutdown) so EVERY re-entrant CfAPI path bails: the NOTIFY
        //    callbacks (convert/set-in-sync/ACK) AND the sync-loop pull's invalidate/created
        //    (CfUpdatePlaceholder/CfCreatePlaceholders). Without this a callback or invalidate
        //    landing during disconnect deadlocks the closing connection.
        // 2. Signal stop. The sync thread will wind down, but it may be stuck mid-push on a slow
        //    HTTP call (observed: the join hung in the push phase). We do NOT block teardown on it.
        // 3. Disconnect the sync root NOW. Safe even with the sync thread still running: with
        //    `shutting_down` set it makes no re-entrant CfAPI calls, so the only contention is the
        //    brief SQLite lock. Disconnect flushes the pending NOTIFY callbacks into the outbox.
        // 4. Drain the outbox on the main thread — this is the authoritative final push. It pushes
        //    the full outbox (so the rename can't be lost behind a stuck sync-thread push) and
        //    settles for callbacks that land just after disconnect. A redundant double-push with
        //    the (possibly still-running) sync thread is harmless (idempotent commit).
        // 5. Finally join the sync thread with a bounded wait so its in-flight push (bounded by the
        //    30s HTTP timeout) finishes cleanly; detach if it somehow outlasts that.
        if let Some(h) = &self.handle {
            h.begin_shutdown();
        }
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take(); // Drop disconnects (flushes callbacks → outbox) + stops the watcher
        eprintln!("cfapi: shutdown — disconnected; draining outbox");
        self.shutdown_drain();
        if let Some(h) = self.sync_handle.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                eprintln!("cfapi: sync thread still busy post-drain — detaching");
            }
        }
        eprintln!("cfapi: shutdown — done");
    }

    /// Push whatever the disconnect's NOTIFY callbacks captured into the outbox. Runs on the
    /// caller's thread after the sync thread is joined + the root disconnected. Requires the
    /// outbox to stay empty for a few consecutive polls before declaring done (CfAPI can deliver
    /// a callback a moment after `CfDisconnectSyncRoot` returns), bounded by a deadline.
    fn shutdown_drain(&self) {
        let io = self.limits.wrap(PresignedIO::new(&self.client));
        let start = std::time::Instant::now();
        // CfAPI flushes its batched NOTIFY callbacks (delete/rename/edit) at disconnect, so the
        // outbox can keep growing for a moment AFTER the drain starts, and a slow multi-chunk edit
        // push can take seconds. The deadline is therefore IDLE-based (measured from the last time
        // the outbox had work), never a fixed wall-clock budget that a single slow push could eat,
        // and we never declare done while entries remain — so a late straggler (the B3 rename) is
        // always pushed. A hard cap is the only absolute backstop against a true hang.
        const STABLE: Duration = Duration::from_secs(3); // outbox empty this long → likely done
        const MAX_IDLE: Duration = Duration::from_secs(6); // give up this long after the last work
        const HARD_CAP: Duration = Duration::from_secs(120); // absolute backstop against a true hang
        let mut last_work = start;
        let mut empty_since: Option<std::time::Instant> = None;
        loop {
            let before;
            {
                // Hold the push gate so we never push concurrently with the (possibly still
                // winding-down) sync thread — that would self-conflict on a shared node. The gate
                // also makes us wait out the sync thread's in-flight push before our first attempt.
                let _gate = self.push_gate.lock().unwrap_or_else(|e| e.into_inner());
                before = self.provider.outbox_list().unwrap_or_default();
                if !before.is_empty() {
                    let ops: Vec<String> = before
                        .iter()
                        .map(|(id, op)| format!("{op}:{}", &id[..8.min(id.len())]))
                        .collect();
                    eprintln!("cfapi: drain round — outbox [{}]", ops.join(", "));
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                }
                if let Err(e) = SyncEngine::push_outbox(&self.provider, &self.client, &io) {
                    eprintln!("cfapi: shutdown drain failed: {e}");
                }
            }
            let now = std::time::Instant::now();
            let remaining = self.provider.outbox_list().map(|v| v.len()).unwrap_or(0);
            if !before.is_empty() || remaining > 0 {
                last_work = now;
                empty_since = None;
            } else if empty_since.is_none() {
                empty_since = Some(now);
            }
            // Done only when the outbox is EMPTY and has stayed empty for STABLE and nothing has
            // happened for MAX_IDLE — so a straggler that lands seconds after a slow push is caught.
            if remaining == 0 {
                if let Some(since) = empty_since {
                    if now.duration_since(since) >= STABLE
                        && now.duration_since(last_work) >= MAX_IDLE
                    {
                        break;
                    }
                }
            }
            if now.duration_since(start) >= HARD_CAP {
                eprintln!("cfapi: shutdown drain hard-cap hit with {remaining} still queued");
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        eprintln!("cfapi: shutdown drain done");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

#[cfg(target_os = "windows")]
impl Drop for CfApiMount {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A stable id for "the library this (server, token) points at", used to bind a local DB to
/// one library so a reused DB path can't mix two. (Keyed by token, so each device's cache is
/// its own — which is correct; cross-device sharing of a local cache is never wanted.)
fn library_id_for(server: &str, token: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    server.hash(&mut h);
    token.hash(&mut h);
    h.finish() as i64
}

/// Push the outbox + pull the changelog delta on each nudge or poll-timeout. `inval`, when
/// present (the FUSE `Daemon`), drops the kernel cache for any inode a pull repoints at new
/// content; the WebDAV/WinFsp mounts pass `None` (no strong push-invalidation yet).
#[allow(clippy::too_many_arguments)]
fn sync_loop(
    provider: Arc<SqliteProvider>,
    client: ServerClient,
    stop: Arc<AtomicBool>,
    nudge_rx: mpsc::Receiver<i64>,
    poll: Duration,
    inval: Option<Arc<dyn crate::Invalidate>>,
    push_gate: Option<Arc<std::sync::Mutex<()>>>,
    limits: TransferLimits,
    debounce: Debounce,
) {
    let mut cursor = provider.cursor().unwrap_or(0);
    println!("sync loop started (cursor {cursor}, poll {poll:?})");
    let mut tick: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        tick += 1;
        // Wrap the production transport in the bandwidth throttle (a no-op when unlimited).
        let io = limits.wrap(PresignedIO::new(&client));
        // Hold the push gate across the tick (CfAPI only) so the shutdown drain can't push the
        // SAME outbox node concurrently — a concurrent double-push sends the same base_version_id
        // twice and the server records a spurious self-conflict on the second.
        let guard = push_gate
            .as_ref()
            .map(|g| g.lock().unwrap_or_else(|e| e.into_inner()));
        let r = SyncEngine::sync_tick_debounced(
            &provider,
            &client,
            &io,
            cursor,
            inval.as_deref(),
            debounce.window_ms,
            debounce.max_age_ms,
        );
        drop(guard);
        match r {
            Ok(new_cursor) => {
                if new_cursor != cursor {
                    if let Err(e) = provider.set_cursor(new_cursor) {
                        eprintln!("sync: persisting cursor failed: {e}");
                    }
                    cursor = new_cursor;
                }
            }
            Err(e) => eprintln!("sync tick #{tick} error: {e}"),
        }
        // Wait for a nudge (pull now) or the poll fallback; coalesce a burst.
        match nudge_rx.recv_timeout(poll) {
            Ok(seq) => {
                while nudge_rx.try_recv().is_ok() {}
                println!("nudge: head_seq {seq} → pulling");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Graceful shutdown: drain the outbox one final time so a local change captured between the
    // last tick and `stop` (e.g. a FUSE write right before unmount) still reaches the server.
    // (The CfAPI backend does NOT rely on this — its NOTIFY callbacks flush only at disconnect,
    // *after* the sync thread is joined, so `CfApiMount::stop` runs its own post-disconnect drain.
    // For FUSE/WebDAV/WinFsp the local change is already in the outbox here, so one pass suffices.)
    let io = limits.wrap(PresignedIO::new(&client));
    if let Err(e) = SyncEngine::push_outbox(&provider, &client, &io) {
        eprintln!("sync: final outbox drain failed: {e}");
    }
    println!("sync loop stopped");
}

/// Subscribe to the LibraryChannel; forward each `{head_seq}` to the sync loop.
/// Reconnects on socket drop (the poll covers correctness meanwhile).
fn nudge_loop(client: ServerClient, stop: Arc<AtomicBool>, nudge_tx: mpsc::Sender<i64>) {
    while !stop.load(Ordering::Relaxed) {
        let tx = nudge_tx.clone();
        if let Err(e) = client.watch_library(move |seq| {
            let _ = tx.send(seq);
        }) {
            eprintln!("nudge: {e} (falling back to poll; reconnecting)");
        }
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
