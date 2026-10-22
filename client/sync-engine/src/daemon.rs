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

use fs_provider::{LocalChunkStore, Reset, SqliteProvider};
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

/// Whether the sync server still accepts this device's token, as the sync loop last learned it.
/// A host reads it to show "signed out" instead of "mounted" (tinkerex#472).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncHealth {
    /// Syncing, or retrying a transient failure (offline, a 5xx) — nothing for the user to do.
    Syncing,
    /// The sync server refused this device's token (revoked, expired, unknown). Sync is parked:
    /// the drive still serves local files, edits keep queueing in the outbox, and only a cheap
    /// identity probe goes out, on a growing interval. Signing in again is the fix.
    SignedOut,
}

impl SyncHealth {
    fn from_flag(signed_out: &AtomicBool) -> Self {
        if signed_out.load(Ordering::SeqCst) {
            SyncHealth::SignedOut
        } else {
            SyncHealth::Syncing
        }
    }
}

/// How a signed-out sync loop spaces its identity probes: `first`, doubling, capped at `cap`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeBackoff {
    pub first: Duration,
    pub cap: Duration,
}

/// 30 s, then doubling to one probe every 15 minutes — a revoked device costs its server a
/// handful of requests an hour, not one every poll.
pub(crate) const SIGNED_OUT_PROBE: ProbeBackoff = ProbeBackoff {
    first: Duration::from_secs(30),
    cap: Duration::from_secs(15 * 60),
};

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
/// [`stop`](Self::stop)) unmounts and tears the loops down. macOS uses [`WebdavMount`];
/// Windows uses [`CfApiMount`] by default, with [`WinfspMount`] behind the Tauri app's
/// `winfsp-backend` feature. (This line named `WinfspMount` alone until 2026-08-25, which
/// predated the CfAPI flip.)
#[cfg(target_os = "linux")]
pub struct Daemon {
    stop: Arc<AtomicBool>,
    signed_out: Arc<AtomicBool>,
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
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        bind_to_library(&provider, &client, &cfg.server_url, &cfg.token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(false));
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
            let signed_out = signed_out.clone();
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
                    signed_out,
                    SIGNED_OUT_PROBE,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            let signed_out = signed_out.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx, signed_out));
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
            signed_out,
            session: Some(session),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Whether the sync server still accepts this device's token — see [`SyncHealth`].
    pub fn sync_health(&self) -> SyncHealth {
        SyncHealth::from_flag(&self.signed_out)
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
    signed_out: Arc<AtomicBool>,
    rt: Option<tokio::runtime::Runtime>, // owns the WebDAV server task; drop = shutdown
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

impl WebdavMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        bind_to_library(&provider, &client, &cfg.server_url, &cfg.token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Same sync + nudge threads as the FUSE Daemon.
        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let debounce = cfg.debounce;
            let signed_out = signed_out.clone();
            thread::spawn(move || {
                sync_loop(
                    provider,
                    client,
                    stop,
                    nudge_rx,
                    poll,
                    None,
                    None,
                    limits,
                    debounce,
                    signed_out,
                    SIGNED_OUT_PROBE,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            let signed_out = signed_out.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx, signed_out));
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
            signed_out,
            rt: Some(rt),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Whether the sync server still accepts this device's token — see [`SyncHealth`].
    pub fn sync_health(&self) -> SyncHealth {
        SyncHealth::from_flag(&self.signed_out)
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
    // `-v` is the volume name Finder shows under Locations.
    let status = Command::new("/sbin/mount_webdav")
        .args(["-S", "-v", "Tinkerex", &format!("http://{addr}/")])
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
    signed_out: Arc<AtomicBool>,
    fs_thread: Option<JoinHandle<()>>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

#[cfg(target_os = "windows")]
impl WinfspMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        bind_to_library(&provider, &client, &cfg.server_url, &cfg.token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        // Same sync + nudge threads as the other backends.
        let limits = cfg.limits();
        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            let debounce = cfg.debounce;
            let signed_out = signed_out.clone();
            thread::spawn(move || {
                sync_loop(
                    provider,
                    client,
                    stop,
                    nudge_rx,
                    poll,
                    None,
                    None,
                    limits,
                    debounce,
                    signed_out,
                    SIGNED_OUT_PROBE,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            let signed_out = signed_out.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx, signed_out));
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
            signed_out,
            fs_thread: Some(fs_thread),
            sync_handle: Some(sync_handle),
            mountpoint: cfg.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Whether the sync server still accepts this device's token — see [`SyncHealth`].
    pub fn sync_health(&self) -> SyncHealth {
        SyncHealth::from_flag(&self.signed_out)
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
    signed_out: Arc<AtomicBool>,
    handle: Option<fs_cfapi::CfApiMountHandle>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
    // Kept for the post-disconnect shutdown drain (see `stop`): once the sync root is
    // disconnected (which flushes CfAPI's NOTIFY callbacks into the outbox) we push whatever those
    // callbacks captured — on the caller's thread, and BEFORE the sync thread is joined, never
    // after. `stop`'s step 4 says why that order is deliberate.
    provider: Arc<SqliteProvider>,
    client: ServerClient,
    // Serializes the sync loop's push against the shutdown drain so they never push the same
    // outbox node concurrently. NOT to prevent a conflict — that rationale is false. Both push
    // through the same `ServerClient` (built once in `start`, cloned into the sync thread), so both
    // carry the same device token and resolve to the same server-side `device_id`, and
    // `Sync::Commit` has a dedicated same-device exemption that fast-forwards a stale base version
    // instead of recording one ("a divergence from the SAME device is not a real conflict",
    // app/services/sync/commit.rb; test/services/sync/conflicts_test.rb pins it). What the gate
    // actually saves is waste: a duplicate `versions` row, a duplicate `changelog_entries` row
    // broadcast to every device, and redundant content-addressed chunk re-PUTs.
    push_gate: Arc<std::sync::Mutex<()>>,
    // Bandwidth caps (shared with the sync loop) so the post-disconnect drain throttles too.
    limits: TransferLimits,
}

#[cfg(target_os = "windows")]
impl CfApiMount {
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        bind_to_library(&provider, &client, &cfg.server_url, &cfg.token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(false));
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
            let signed_out = signed_out.clone();
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
                    signed_out,
                    SIGNED_OUT_PROBE,
                )
            })
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            let signed_out = signed_out.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx, signed_out));
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
            signed_out,
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

    /// Whether the sync server still accepts this device's token — see [`SyncHealth`].
    pub fn sync_health(&self) -> SyncHealth {
        SyncHealth::from_flag(&self.signed_out)
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
    /// caller's thread after the root is disconnected but **BEFORE the sync thread is joined** —
    /// deliberately, per step 4 of `stop`: the sync thread may be stuck mid-push on a slow HTTP
    /// call (observed: the join hung in the push phase), so blocking on it first would risk losing
    /// the very rename this drain exists to deliver. That the sync thread is therefore still
    /// running is exactly why the loop below takes `push_gate`. Requires the outbox to stay empty
    /// for a few consecutive polls before declaring done (CfAPI can deliver a callback a moment
    /// after `CfDisconnectSyncRoot` returns), bounded by a deadline.
    fn shutdown_drain(&self) {
        // Signed out, every push below would be refused and retried until HARD_CAP — two
        // minutes of requests on each unmount. The outbox stays queued for the next sign-in.
        if self.signed_out.load(Ordering::SeqCst) {
            eprintln!("cfapi: signed out — no shutdown drain; the outbox stays queued");
            return;
        }
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
                // winding-down) sync thread — a shared node would otherwise be committed twice,
                // costing a duplicate version + changelog entry and redundant chunk PUTs. NOT a
                // conflict; see the `push_gate` field doc for why the server exempts this. The gate
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

// ---- binding the local DB to a library -------------------------------------------------
//
// A local DB holds ONE library's tree, so a reused DB path must not mix two (the Windows
// "cursor 14 > head_seq 5" + duplicate-placeholder bug). Until tinkerex#472 the binding was a hash
// of (server, TOKEN). A token is a device credential, not a library: a device signed out by its
// server and signed in again gets a fresh token for the SAME library, the hash changed, and the
// wipe that followed deleted the tree, the cursor and every outbox entry the old token never
// pushed. The binding is now the library the SERVER says the credential reaches (`whoami`), and
// the wipe itself refuses while anything is unpushed (`SqliteProvider::reset_synced_state`).

/// `sync_state` key: [`library_tag`] of the library this DB holds.
const LIBRARY_KEY: &str = "library_key";
/// `sync_state` key: [`credential_tag`] of the credential that last proved it, so a start with
/// the same credential needs no network.
const CREDENTIAL_KEY: &str = "credential_key";
/// `sync_state` key the pre-#472 binding wrote: [`legacy_library_id`]. Read, never written.
const LEGACY_LIBRARY_KEY: &str = "library";

/// What [`bind_to_library`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binding {
    /// The credential that last proved this DB's library: nothing asked, nothing changed.
    Kept,
    /// A new credential for the library this DB already holds — a fresh grant. Everything kept.
    Rebound,
    /// The DB's library was never recorded (a DB from before #472, or a first start) and it
    /// holds unpushed changes: adopted as it stands rather than wiped.
    Adopted,
    /// A different (or unrecorded) library and nothing unpushed: the stale tree was wiped.
    Wiped,
    /// The server could not say which library the credential reaches (offline, 5xx, 401):
    /// left exactly as it was, and asked again next start.
    Unresolved,
}

/// Bind the local DB to the library `token` reaches on `server_url`, before anything syncs.
///
/// Only a credential this DB has not seen costs a request (`whoami`), so an offline start with
/// the usual credential mounts as it always did. `Err` only for a DIFFERENT library while
/// changes are unpushed — [`crate::Error::UnpushedChangesElsewhere`]: the mount is refused rather
/// than either losing them or pushing them into a library they were not made in.
pub(crate) fn bind_to_library(
    provider: &SqliteProvider,
    client: &ServerClient,
    server_url: &str,
    token: &str,
) -> crate::Result<Binding> {
    let server = server_url.trim_end_matches('/');
    let credential = credential_tag(server, token);
    let stored_credential = provider.sync_state(CREDENTIAL_KEY)?;
    if stored_credential == Some(credential) {
        return Ok(Binding::Kept);
    }
    if stored_credential.is_none()
        && provider.sync_state(LEGACY_LIBRARY_KEY)? == Some(legacy_library_id(server_url, token))
    {
        return Ok(Binding::Kept); // the pre-#472 binding of this very token
    }

    let who = match client.whoami() {
        Ok(who) => who,
        Err(e) => {
            eprintln!("sync: could not ask {server} which library this sign-in reaches ({e}); local DB left as it was");
            return Ok(Binding::Unresolved);
        }
    };
    let library = library_tag(server, who.user_id);
    let stored_library = provider.sync_state(LIBRARY_KEY)?;
    let binding = if stored_library == Some(library) {
        Binding::Rebound
    } else {
        match provider.reset_synced_state()? {
            Reset::Wiped { had_nodes } => {
                if had_nodes {
                    eprintln!("sync: local DB was a different library — wiped, re-syncing fresh");
                }
                Binding::Wiped
            }
            Reset::Refused { pending } if stored_library.is_none() => {
                eprintln!("sync: adopting this DB's tree for the library this sign-in reaches ({pending} unpushed change(s) kept)");
                Binding::Adopted
            }
            Reset::Refused { pending } => {
                return Err(crate::Error::UnpushedChangesElsewhere { pending });
            }
        }
    };
    provider.set_sync_state(LIBRARY_KEY, library)?;
    provider.set_sync_state(CREDENTIAL_KEY, credential)?;
    Ok(binding)
}

/// A tag of `parts` stable across Rust releases (SHA-256, not `DefaultHasher`), as the i64 that
/// `sync_state` stores.
fn stable_tag(parts: &[&str]) -> i64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in parts {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let digest = h.finalize();
    i64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

/// The library: the server, and the account `whoami` says the credential belongs to (one
/// library per sync-server user). Unchanged by a new device token.
fn library_tag(server: &str, user_id: i64) -> i64 {
    stable_tag(&["library", server, &user_id.to_string()])
}

/// The device credential — changes with every grant.
fn credential_tag(server: &str, token: &str) -> i64 {
    stable_tag(&["credential", server, token])
}

/// The pre-#472 binding, hash of (server, token): kept ONLY to recognise a DB bound by an older
/// build to the very token it is being started with. `DefaultHasher` may change between Rust
/// releases; if it does, such a DB just takes the `whoami` path, which loses nothing.
fn legacy_library_id(server: &str, token: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    server.hash(&mut h);
    token.hash(&mut h);
    h.finish() as i64
}

/// Push the outbox + pull the changelog delta on each nudge or poll-timeout. `inval`, when
/// present (the FUSE `Daemon`), drops the kernel cache for any inode a pull repoints at new
/// content; the WebDAV/WinFsp mounts pass `None` (no strong push-invalidation yet).
///
/// **A refused token parks the loop** (tinkerex#472). A tick that fails with the server's own
/// 401 ([`sync_client::Error::is_auth_rejection`]), confirmed on the identity route, sets
/// `signed_out`; from then on the loop sends nothing but one `whoami` per `probe` step (doubling
/// to its cap), resumes by itself if one succeeds, and skips the final drain on stop — every
/// push would be refused, and the outbox must outlive this token for the next sign-in to push.
/// Any other failure (offline, a 5xx, a proxy's 401) is retried on the normal poll as before.
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
    signed_out: Arc<AtomicBool>,
    probe: ProbeBackoff,
) {
    let mut cursor = provider.cursor().unwrap_or(0);
    println!("sync loop started (cursor {cursor}, poll {poll:?})");
    let mut tick: u64 = 0;
    let mut wait = probe.first;
    while !stop.load(Ordering::Relaxed) {
        if signed_out.load(Ordering::SeqCst) {
            if !sleep_unless_stopped(&stop, wait) {
                break;
            }
            wait = (wait * 2).min(probe.cap);
            match client.whoami() {
                Ok(_) => {
                    eprintln!("sync: the server accepts this device's token again — resuming");
                    signed_out.store(false, Ordering::SeqCst);
                }
                Err(e) if e.is_auth_rejection() => {}
                Err(e) => eprintln!("sync: signed-out probe failed ({e}); still parked"),
            }
            continue;
        }
        tick += 1;
        // Wrap the production transport in the bandwidth throttle (a no-op when unlimited).
        let io = limits.wrap(PresignedIO::new(&client));
        // Hold the push gate across the tick (CfAPI only) so the shutdown drain can't push the
        // SAME outbox node concurrently. A double-push does send the same base_version_id twice,
        // but the server does NOT conflict on it: both pushes carry this device's token, and
        // `Sync::Commit`'s same-device exemption fast-forwards instead. What the gate avoids is a
        // duplicate version row, a duplicate changelog entry broadcast to every device, and
        // redundant chunk re-PUTs. See the `push_gate` field doc on `CfApiMount`.
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
            Err(e) if refused_by_server(&e, &client) => {
                eprintln!(
                    "sync: the server refused this device's token ({e}) — signed out; sync parked, \
                     local changes stay queued until this device signs in again"
                );
                signed_out.store(true, Ordering::SeqCst);
                wait = probe.first;
                continue;
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
    // For FUSE/WebDAV/WinFsp the local change is already in the outbox here, so one pass suffices —
    // and all three pass `push_gate: None`, so this is their ONLY drain and it races nothing.
    // The CfAPI backend does NOT rely on this: its NOTIFY callbacks flush at disconnect, and
    // `CfApiMount::stop` runs its own post-disconnect drain — BEFORE joining this thread, not
    // after it (step 4 there says why). So on Windows alone the two can overlap, and this final
    // push is not gated. The cost of that overlap is a duplicate version row, a duplicate
    // changelog entry and redundant chunk re-PUTs — never a conflict, because both drains push
    // through the same `ServerClient` and `Sync::Commit` exempts a same-device divergence.
    if signed_out.load(Ordering::SeqCst) {
        let queued = provider.outbox_len().unwrap_or(0);
        println!("sync loop stopped signed out; {queued} local change(s) stay queued");
        return;
    }
    let io = limits.wrap(PresignedIO::new(&client));
    if let Err(e) = SyncEngine::push_outbox(&provider, &client, &io) {
        eprintln!("sync: final outbox drain failed: {e}");
    }
    println!("sync loop stopped");
}

/// Whether a failed tick means the server refused this device's token: its own 401 on the
/// tick, CONFIRMED by a 401 on the identity route. The confirmation costs one request and keeps
/// any other 401 a tick might carry from signing the device out.
fn refused_by_server(e: &crate::Error, client: &ServerClient) -> bool {
    matches!(e, crate::Error::Api(api) if api.is_auth_rejection())
        && client.whoami().is_err_and(|w| w.is_auth_rejection())
}

/// Sleep `total` in short slices so a `stop` is honoured promptly even across a long backoff.
/// `false` if stopped.
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) -> bool {
    let deadline = std::time::Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
    false
}

/// Subscribe to the LibraryChannel; forward each `{head_seq}` to the sync loop.
/// Reconnects on socket drop (the poll covers correctness meanwhile).
fn nudge_loop(
    client: ServerClient,
    stop: Arc<AtomicBool>,
    nudge_tx: mpsc::Sender<i64>,
    signed_out: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        // Signed out, the socket would only be refused: stay off it until the sync loop's probe
        // finds the token accepted again.
        if signed_out.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
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

#[cfg(test)]
pub(crate) mod stub {
    //! A tiny scripted HTTP server for the daemon's tests: every request is answered by `route`
    //! (request line in, `(status, body)` out) and its request line recorded, so a test can count
    //! exactly what the daemon sent. One request per connection (`connection: close`).
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub struct Stub {
        pub url: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        pub fn start(route: impl Fn(&str) -> (u16, String) + Send + 'static) -> Stub {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let mut content_length = 0usize;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    let _ = std::io::Read::read_exact(&mut reader, &mut body);
                    let line = line.trim_end().to_string();
                    let (status, reply) = route(&line);
                    log.lock().unwrap().push(line);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                }
            });
            Stub { url, seen }
        }

        /// Every request line received so far, in order.
        pub fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// A `whoami` answer for sync-server user (library) `user_id`.
    pub fn whoami(user_id: i64) -> (u16, String) {
        (
            200,
            format!(r#"{{"user_id":{user_id},"device_id":7,"head_seq":0}}"#),
        )
    }
}

#[cfg(test)]
mod binding_tests {
    //! THE DATA-LOSS PATH (tinkerex#472). A device signed out by its server signs in again and is
    //! handed a FRESH grant: same library, new device token. Before this fix the local DB was
    //! bound to a hash of (server, TOKEN), so the new token read as "a different library" and
    //! `bind_library` deleted the tree, the cursor and — the loss — every outbox entry the old
    //! token never got to push. Each test opens a real file-backed DB, closes it, and reopens it
    //! the way the next start does.
    use super::stub::{self, Stub};
    use super::*;
    use fs_provider::{Provider, ROOT_INO};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sync-bind-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(dir: &Path) -> SqliteProvider {
        let store = Box::new(LocalChunkStore::new(dir.join("chunks")).unwrap());
        SqliteProvider::open(&dir.join("tree.db"), store).unwrap()
    }

    /// A local edit the server has not seen: a file created and flushed, so it sits in the outbox.
    fn unpushed_edit(p: &SqliteProvider, name: &str, body: &[u8]) {
        let f = p.create(ROOT_INO, name).unwrap();
        p.write(f.ino, 0, body).unwrap();
        p.flush(f.ino).unwrap();
    }

    fn names(p: &SqliteProvider) -> Vec<String> {
        let mut v: Vec<String> = p
            .export_tree()
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        v.sort();
        v
    }

    /// First start under token A (the server says: library 42), an edit that never got pushed,
    /// and a cursor — the state a device is in when its token is revoked.
    fn device_with_unpushed_edit(dir: &Path, server: &Stub) {
        let p = open(dir);
        let client = ServerClient::with_token(&server.url, "token-A");
        bind_to_library(&p, &client, &server.url, "token-A").unwrap();
        unpushed_edit(&p, "draft.scad", b"cube(10);");
        p.set_cursor(17).unwrap();
        assert_eq!(
            p.outbox_list().unwrap().len(),
            1,
            "the edit is queued, unpushed"
        );
    }

    #[test]
    fn a_fresh_grant_for_the_same_library_keeps_the_unpushed_outbox() {
        let dir = scratch("fresh-grant");
        let server = Stub::start(|_| stub::whoami(42));
        device_with_unpushed_edit(&dir, &server);

        // Signed out, signed in again: the next start runs with token B for the SAME library.
        let p = open(&dir);
        let client = ServerClient::with_token(&server.url, "token-B");
        bind_to_library(&p, &client, &server.url, "token-B").unwrap();

        assert_eq!(
            p.outbox_list().unwrap().len(),
            1,
            "the unpushed edit must survive a new grant"
        );
        assert_eq!(
            names(&p),
            vec!["draft.scad".to_string()],
            "the local tree is kept"
        );
        assert_eq!(p.cursor().unwrap(), 17, "the cursor is kept: same library");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_db_bound_by_the_old_token_hash_keeps_its_unpushed_edit_through_a_new_grant() {
        // Every install that predates this fix: the DB carries only the pre-#472 key, a hash of
        // (server, old token). Its first start with a fresh grant cannot tell which library
        // that was, and it holds an unpushed edit — adopted, never wiped.
        let dir = scratch("legacy");
        let server = Stub::start(|_| stub::whoami(42));
        {
            let p = open(&dir);
            p.set_sync_state("library", legacy_library_id(&server.url, "token-A"))
                .unwrap();
            unpushed_edit(&p, "draft.scad", b"cube(10);");
            p.set_cursor(17).unwrap();
        }
        let p = open(&dir);
        // The old token itself still binds with no question asked, exactly as before.
        let old = ServerClient::with_token(&server.url, "token-A");
        assert_eq!(
            bind_to_library(&p, &old, &server.url, "token-A").unwrap(),
            Binding::Kept
        );
        assert!(
            server.seen().is_empty(),
            "the pre-fix binding of the same token needs no request"
        );

        let client = ServerClient::with_token(&server.url, "token-B");
        assert_eq!(
            bind_to_library(&p, &client, &server.url, "token-B").unwrap(),
            Binding::Adopted
        );
        assert_eq!(
            p.outbox_len().unwrap(),
            1,
            "the unpushed edit survives the upgrade + new grant"
        );
        assert_eq!(names(&p), vec!["draft.scad".to_string()]);
        assert_eq!(p.cursor().unwrap(), 17);
        // From here on it is an ordinary bound DB: the next start with token B asks nothing.
        let asked = server.seen().len();
        assert_eq!(
            bind_to_library(&p, &client, &server.url, "token-B").unwrap(),
            Binding::Kept
        );
        assert_eq!(server.seen().len(), asked);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server whose `whoami` names whichever library `library` holds at the time.
    fn switchable_server(library: &Arc<std::sync::atomic::AtomicI64>) -> Stub {
        let library = library.clone();
        Stub::start(move |_| stub::whoami(library.load(Ordering::SeqCst)))
    }

    #[test]
    fn a_different_library_with_unpushed_changes_refuses_to_mount_and_deletes_nothing() {
        let dir = scratch("other-library");
        let library = Arc::new(std::sync::atomic::AtomicI64::new(42));
        let server = switchable_server(&library);
        device_with_unpushed_edit(&dir, &server);

        // The server now says this sign-in reaches library 99, not 42.
        library.store(99, Ordering::SeqCst);
        let p = open(&dir);
        let client = ServerClient::with_token(&server.url, "token-B");
        let err = bind_to_library(&p, &client, &server.url, "token-B")
            .expect_err("a different library with pending changes must not bind");

        assert!(
            matches!(err, crate::Error::UnpushedChangesElsewhere { pending: 1 }),
            "{err}"
        );
        assert!(err.to_string().contains("Nothing was deleted"), "{err}");
        assert_eq!(p.outbox_len().unwrap(), 1, "the unpushed edit is untouched");
        assert_eq!(names(&p), vec!["draft.scad".to_string()]);
        assert_eq!(p.cursor().unwrap(), 17);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_different_library_with_nothing_unpushed_is_still_wiped() {
        // The protection the binding exists for is kept: two libraries never share one tree.
        let dir = scratch("wipe");
        let library = Arc::new(std::sync::atomic::AtomicI64::new(42));
        let server = switchable_server(&library);
        device_with_unpushed_edit(&dir, &server);
        {
            let p = open(&dir);
            let (node_id, _) = p.outbox_list().unwrap().remove(0);
            p.outbox_clear(&node_id).unwrap(); // pushed
        }
        library.store(99, Ordering::SeqCst);
        let p = open(&dir);
        let client = ServerClient::with_token(&server.url, "token-B");
        assert_eq!(
            bind_to_library(&p, &client, &server.url, "token-B").unwrap(),
            Binding::Wiped
        );
        assert!(names(&p).is_empty(), "the other library's tree is gone");
        assert_eq!(p.cursor().unwrap(), 0, "and its cursor");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_credential_binds_offline_without_a_request() {
        let dir = scratch("offline-same");
        let server = Stub::start(|_| stub::whoami(42));
        device_with_unpushed_edit(&dir, &server);
        assert_eq!(server.seen().len(), 1, "the first start asked once");

        // Next start, network down: the credential is the one the DB last proved.
        let p = open(&dir);
        let offline = ServerClient::with_token(closed_port(), "token-A");
        assert_eq!(
            bind_to_library(&p, &offline, &server.url, "token-A").unwrap(),
            Binding::Kept
        );
        assert_eq!(p.outbox_len().unwrap(), 1);
        assert_eq!(server.seen().len(), 1, "no request at all");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_credential_the_server_cannot_place_leaves_the_db_exactly_as_it_was() {
        // Offline, a 5xx, or a 401 on whoami: no authoritative answer, so nothing is decided.
        let dir = scratch("unresolved");
        let server = Stub::start(|_| stub::whoami(42));
        device_with_unpushed_edit(&dir, &server);
        let p = open(&dir);
        let answers: [(&str, Option<(u16, &str)>); 3] = [
            ("network down", None),
            ("503", Some((503, r#"{"error":"unavailable"}"#))),
            ("401", Some((401, r#"{"error":"unauthorized"}"#))),
        ];
        for (what, answer) in answers {
            let url = match answer {
                None => closed_port(),
                Some((status, body)) => {
                    let body = body.to_string();
                    Stub::start(move |_| (status, body.clone())).url
                }
            };
            let client = ServerClient::with_token(&url, "token-B");
            assert_eq!(
                bind_to_library(&p, &client, &server.url, "token-B").unwrap(),
                Binding::Unresolved,
                "{what}"
            );
            assert_eq!(
                p.outbox_len().unwrap(),
                1,
                "{what}: the edit is still queued"
            );
            assert_eq!(names(&p), vec!["draft.scad".to_string()], "{what}");
            assert_eq!(p.cursor().unwrap(), 17, "{what}");
        }
        // Once the server answers, the same library is recognised and nothing is lost.
        let client = ServerClient::with_token(&server.url, "token-B");
        assert_eq!(
            bind_to_library(&p, &client, &server.url, "token-B").unwrap(),
            Binding::Rebound
        );
        assert_eq!(p.outbox_len().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A URL nothing listens on: connection refused, the cheapest "network down".
    fn closed_port() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        url
    }
}

#[cfg(test)]
mod signed_out_tests {
    //! A device whose token the SYNC server refuses (tinkerex#472; real once tinkerex#473 lands).
    //! Before this fix the sync loop logged the 401 and retried every poll, forever, while the
    //! host went on reporting "mounted"; the nudge loop re-dialled `/cable` every second. Each
    //! test runs the REAL `sync_loop` on its own thread against a scripted server, with the
    //! poll shrunk to 20 ms so "forever" is visible inside a second.
    use super::stub::{self, Stub};
    use super::*;
    use fs_provider::{ChunkConfig, Provider, ROOT_INO};
    use std::sync::atomic::AtomicI64;
    use std::time::Instant;

    const REFUSED: &str = r#"{"error":"unauthorized"}"#;
    const POLL: Duration = Duration::from_millis(20);
    const PROBE: ProbeBackoff = ProbeBackoff {
        first: Duration::from_millis(150),
        cap: Duration::from_millis(300),
    };

    fn provider() -> Arc<SqliteProvider> {
        let store = Box::new(LocalChunkStore::temp().unwrap());
        let tiny = ChunkConfig {
            min: 64,
            avg: 256,
            max: 1024,
        };
        Arc::new(SqliteProvider::in_memory(store, tiny).unwrap())
    }

    struct Running {
        stop: Arc<AtomicBool>,
        signed_out: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
        _nudge: mpsc::Sender<i64>,
    }

    impl Running {
        fn health(&self) -> SyncHealth {
            SyncHealth::from_flag(&self.signed_out)
        }
        fn stop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                h.join().unwrap();
            }
        }
    }

    fn run(provider: Arc<SqliteProvider>, url: &str) -> Running {
        let client = ServerClient::with_token(url, "dead-token");
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let handle = {
            let (stop, signed_out) = (stop.clone(), signed_out.clone());
            thread::spawn(move || {
                sync_loop(
                    provider,
                    client,
                    stop,
                    rx,
                    POLL,
                    None,
                    None,
                    TransferLimits::default(),
                    Debounce::default(),
                    signed_out,
                    PROBE,
                )
            })
        };
        Running {
            stop,
            signed_out,
            handle: Some(handle),
            _nudge: tx,
        }
    }

    fn count(server: &Stub, prefix: &str) -> usize {
        server
            .seen()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_refused_token_parks_the_loop_instead_of_retrying_every_poll() {
        let server = Stub::start(|_| (401, REFUSED.into()));
        let mut running = run(provider(), &server.url);
        thread::sleep(Duration::from_secs(1));

        assert_eq!(running.health(), SyncHealth::SignedOut);
        // One tick saw the 401 and stopped. The old loop asked ~50 times in this second.
        assert_eq!(count(&server, "GET /v1/changes"), 1, "{:?}", server.seen());
        // Parked, it only probes identity, on a growing interval: ~4 in a second at 150/300 ms.
        let probes = count(&server, "GET /v1/devices/whoami");
        assert!(
            (1..=6).contains(&probes),
            "{probes} probes: {:?}",
            server.seen()
        );
        assert_eq!(server.seen().len(), 1 + probes, "nothing but the probes");
        running.stop();
    }

    #[test]
    fn a_5xx_never_signs_the_device_out() {
        let server = Stub::start(|_| (503, r#"{"error":"unavailable"}"#.into()));
        let mut running = run(provider(), &server.url);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(running.health(), SyncHealth::Syncing);
        assert!(
            count(&server, "GET /v1/changes") > 5,
            "a transient error keeps the normal poll going"
        );
        running.stop();
    }

    #[test]
    fn a_network_failure_never_signs_the_device_out() {
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            drop(l);
            url
        };
        let mut running = run(provider(), &closed);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(running.health(), SyncHealth::Syncing);
        running.stop();
    }

    #[test]
    fn a_401_without_the_apis_body_is_not_a_sign_out() {
        // A proxy in front of the server, not the server refusing the token.
        let server = Stub::start(|_| (401, "<html>401 Authorization Required</html>".into()));
        let mut running = run(provider(), &server.url);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(running.health(), SyncHealth::Syncing);
        running.stop();
    }

    #[test]
    fn a_parked_loop_resumes_once_the_server_accepts_the_token_again() {
        let refusing = Arc::new(AtomicI64::new(1));
        let server = {
            let refusing = refusing.clone();
            Stub::start(move |line| {
                if refusing.load(Ordering::SeqCst) == 1 {
                    (401, REFUSED.into())
                } else if line.starts_with("GET /v1/devices/whoami") {
                    stub::whoami(42)
                } else {
                    (200, r#"{"changes":[],"head_seq":0,"oldest_seq":0}"#.into())
                }
            })
        };
        let mut running = run(provider(), &server.url);
        wait_until("signed out", || running.health() == SyncHealth::SignedOut);

        refusing.store(0, Ordering::SeqCst);
        wait_until("syncing again", || running.health() == SyncHealth::Syncing);
        let before = count(&server, "GET /v1/changes");
        wait_until("polling again", || {
            count(&server, "GET /v1/changes") > before + 3
        });
        running.stop();
    }

    #[test]
    fn stopping_a_signed_out_loop_keeps_the_outbox_and_does_not_push_it_again() {
        let p = provider();
        let f = p.create(ROOT_INO, "draft.scad").unwrap();
        p.write(f.ino, 0, b"cube(10);").unwrap();
        p.flush(f.ino).unwrap();
        let server = Stub::start(|_| (401, REFUSED.into()));
        let mut running = run(p.clone(), &server.url);
        wait_until("signed out", || running.health() == SyncHealth::SignedOut);
        let pushes = count(&server, "POST ");
        assert_eq!(
            pushes,
            1,
            "the first tick tried the edit once: {:?}",
            server.seen()
        );

        running.stop();
        assert_eq!(
            count(&server, "POST "),
            pushes,
            "no final drain with a refused token: {:?}",
            server.seen()
        );
        assert_eq!(
            p.outbox_len().unwrap(),
            1,
            "the edit stays queued for the next sign-in"
        );
    }

    #[test]
    fn the_nudge_loop_stays_off_the_socket_while_signed_out() {
        let server = Stub::start(|_| (401, REFUSED.into()));
        let client = ServerClient::with_token(&server.url, "dead-token");
        let stop = Arc::new(AtomicBool::new(false));
        let signed_out = Arc::new(AtomicBool::new(true));
        let (tx, _rx) = mpsc::channel();
        let h = {
            let (stop, signed_out) = (stop.clone(), signed_out.clone());
            thread::spawn(move || nudge_loop(client, stop, tx, signed_out))
        };
        thread::sleep(Duration::from_millis(400));
        stop.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert!(
            server.seen().is_empty(),
            "dialled /cable: {:?}",
            server.seen()
        );
    }
}

#[cfg(test)]
mod live_server_tests {
    //! The two facts the fix for tinkerex#472 rests on, against a REAL sync server
    //! (chaim354/tinkerex-sync) instead of the scripted stub. Skipped, with a notice, unless
    //! `SYNC_IT_SERVER_URL` and `SYNC_IT_ADMIN_TOKEN` are set — the suites' admin-door mode
    //! (CLAUDE.md §5).
    use super::*;
    use fs_provider::{Provider, ROOT_INO};

    fn server() -> Option<String> {
        let url = std::env::var("SYNC_IT_SERVER_URL").ok()?;
        sync_client::admin_token_configured().then_some(url)
    }

    macro_rules! server_or_skip {
        () => {
            match server() {
                Some(url) => url,
                None => {
                    eprintln!("SKIPPED (set SYNC_IT_SERVER_URL and SYNC_IT_ADMIN_TOKEN to run)");
                    return;
                }
            }
        };
    }

    #[test]
    fn live_a_sibling_device_token_reaches_the_same_library_and_keeps_the_outbox() {
        // A fresh grant is a SIBLING device on the same sync-server user: the maker app's
        // SyncGrant mints one with `create_device`. Its token differs; `whoami` must not.
        let url = server_or_skip!();
        let email = format!("bind-{}@example.com", uuid::Uuid::new_v4());
        let mut a = ServerClient::new(&url);
        a.enroll(&email, "unused", "device A")
            .expect("provision a library");
        let token_a = a.token().unwrap().to_string();
        let token_b = a.create_device("device B").expect("a sibling").device_token;
        assert_ne!(token_a, token_b);
        let b = ServerClient::with_token(&url, &token_b);
        assert_eq!(a.whoami().unwrap().user_id, b.whoami().unwrap().user_id);

        let dir = std::env::temp_dir().join(format!("sync-bind-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let open = || {
            let store = Box::new(LocalChunkStore::new(dir.join("chunks")).unwrap());
            SqliteProvider::open(&dir.join("tree.db"), store).unwrap()
        };
        {
            let p = open();
            bind_to_library(&p, &a, &url, &token_a).unwrap();
            let f = p.create(ROOT_INO, "draft.scad").unwrap();
            p.write(f.ino, 0, b"cube(10);").unwrap();
            p.flush(f.ino).unwrap();
        }
        let p = open();
        assert_eq!(
            bind_to_library(&p, &b, &url, &token_b).unwrap(),
            Binding::Rebound
        );
        assert_eq!(
            p.outbox_len().unwrap(),
            1,
            "the unpushed edit survives the new token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_the_servers_refusal_is_what_parks_the_loop() {
        let url = server_or_skip!();
        let dead = ServerClient::with_token(&url, "not-a-device-token");
        let e = dead.whoami().unwrap_err();
        assert!(e.is_auth_rejection(), "{e}");
        let tick: crate::Error = dead.changes_paged(0, 10).unwrap_err().into();
        assert!(refused_by_server(&tick, &dead), "{tick}");
    }
}
