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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fs_fuse::FuseAdapter;
use fs_provider::{LocalChunkStore, SqliteProvider};
use fuser::MountOption;
use sync_client::ServerClient;

use crate::{EngineHydrator, PresignedIO, SyncEngine};

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
        }
    }
}

/// A running mount + its two background loops. Drop (or [`stop`](Self::stop)) unmounts
/// and tears the loops down.
pub struct Daemon {
    stop: Arc<AtomicBool>,
    session: Option<fuser::BackgroundSession>,
    sync_handle: Option<JoinHandle<()>>,
    mountpoint: PathBuf,
}

impl Daemon {
    /// Mount and start syncing. Returns once the mount is live (non-blocking). The nudge
    /// thread is detached (a dropped socket just falls back to the poll; it's reaped on
    /// process exit / when the receiver drops at stop).
    pub fn start(cfg: DaemonConfig) -> crate::Result<Self> {
        let store = Box::new(LocalChunkStore::new(&cfg.chunk_dir)?);
        let provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        let client = ServerClient::with_token(&cfg.server_url, &cfg.token);
        let stop = Arc::new(AtomicBool::new(false));
        let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

        let sync_handle = {
            let provider = provider.clone();
            let client = client.clone();
            let stop = stop.clone();
            let poll = cfg.poll;
            thread::spawn(move || sync_loop(provider, client, stop, nudge_rx, poll))
        };
        {
            let client = client.clone();
            let stop = stop.clone();
            thread::spawn(move || nudge_loop(client, stop, nudge_tx));
        }

        let hydrator = Arc::new(EngineHydrator::new(provider.clone(), client.clone()));
        let fs = FuseAdapter::with_hydrator(provider.clone(), hydrator);
        let opts = [
            MountOption::FSName("maker".into()),
            MountOption::AutoUnmount,
            MountOption::DefaultPermissions,
        ];
        // spawn_mount2 runs the FUSE session loop on its own thread and returns a handle
        // that unmounts on drop — the non-blocking counterpart to mount2.
        let session = fuser::spawn_mount2(fs, &cfg.mountpoint, &opts)?;

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

    /// Block until the mount ends (Ctrl-C / `umount` / `AutoUnmount`), then stop the
    /// loops. For the CLI; a GUI host uses [`start`](Self::start) + [`stop`](Self::stop).
    pub fn run_until_unmounted(mut self) {
        if let Some(session) = self.session.take() {
            session.join(); // blocks; unmounts + joins the FUSE worker
        }
        self.stop();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push the outbox + pull the changelog delta on each nudge or poll-timeout.
fn sync_loop(
    provider: Arc<SqliteProvider>,
    client: ServerClient,
    stop: Arc<AtomicBool>,
    nudge_rx: mpsc::Receiver<i64>,
    poll: Duration,
) {
    let mut cursor = provider.cursor().unwrap_or(0);
    println!("sync loop started (cursor {cursor}, poll {poll:?})");
    while !stop.load(Ordering::Relaxed) {
        let io = PresignedIO::new(&client);
        match SyncEngine::sync_tick(&provider, &client, &io, cursor) {
            Ok(new_cursor) => {
                if new_cursor != cursor {
                    if let Err(e) = provider.set_cursor(new_cursor) {
                        eprintln!("sync: persisting cursor failed: {e}");
                    }
                    cursor = new_cursor;
                }
            }
            Err(e) => eprintln!("sync tick error: {e}"),
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
