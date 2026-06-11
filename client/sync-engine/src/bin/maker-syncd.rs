//! The sync daemon: mount the local-first filesystem **and** run the continuous sync
//! loop against it. A save (FUSE `flush`/`release`) enqueues the node in the local
//! outbox; the background thread pushes it; a read on a not-yet-hydrated file fetches
//! its chunks on demand. This is CLAUDE.md §12 step 1 wired together.
//!
//!   maker-syncd <mountpoint> <db> <chunk_dir> <server_url> <device_token>
//!
//! The FUSE thread and the sync thread share one `Arc<SqliteProvider>` (the SQLite tree
//! serializes them behind its internal mutex); chunk bytes move client ⇄ storage via
//! presigned URLs (`PresignedIO`), never through the metadata path.
//!
//! Pull is driven two ways: a cursor-delta **poll** every `POLL_INTERVAL` (the
//! correctness backbone every sync client relies on — Dropbox/OneDrive/Nextcloud) and
//! an ActionCable `LibraryChannel` **nudge** that wakes the loop the instant the server
//! broadcasts a `{head_seq}`, cutting latency to near-realtime. The nudge is best-effort
//! (a dropped socket falls back to the poll), so it never affects correctness.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use fs_fuse::FuseAdapter;
use fs_provider::{LocalChunkStore, SqliteProvider};
use fuser::MountOption;
use sync_client::ServerClient;
use sync_engine::{EngineHydrator, PresignedIO, SyncEngine};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn main() {
    let mut args = std::env::args().skip(1);
    let (mountpoint, db, chunk_dir, server_url, token) =
        match (args.next(), args.next(), args.next(), args.next(), args.next()) {
            (Some(a), Some(b), Some(c), Some(d), Some(e)) => (a, b, c, d, e),
            _ => {
                eprintln!(
                    "usage: maker-syncd <mountpoint> <db> <chunk_dir> <server_url> <device_token>"
                );
                std::process::exit(2);
            }
        };

    let store = Box::new(LocalChunkStore::new(&chunk_dir).expect("chunk dir"));
    let provider = Arc::new(SqliteProvider::open(&PathBuf::from(&db), store).expect("open provider"));
    let client = ServerClient::with_token(&server_url, &token);

    let stop = Arc::new(AtomicBool::new(false));
    // The ActionCable nudge thread signals the sync loop to pull *now*; the loop also
    // wakes on a `POLL_INTERVAL` timeout as the fallback (the nudge is latency-only).
    let (nudge_tx, nudge_rx) = mpsc::channel::<i64>();

    // --- background sync thread: push the outbox, pull the changelog delta -----
    let sync_handle = {
        let provider = provider.clone();
        let client = client.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut cursor = provider.cursor().unwrap_or(0);
            println!("sync loop started (cursor {cursor}, poll {POLL_INTERVAL:?})");
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
                // Wait for a nudge (pull now) or the poll fallback, whichever comes
                // first; coalesce a burst of nudges into the single tick above.
                match nudge_rx.recv_timeout(POLL_INTERVAL) {
                    Ok(seq) => {
                        while nudge_rx.try_recv().is_ok() {}
                        println!("nudge: head_seq {seq} → pulling");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            println!("sync loop stopped");
        })
    };

    // --- ActionCable nudge thread: LibraryChannel {head_seq} → wake the sync loop --
    // Not joined: a dropped socket just means we fall back to the poll, and on shutdown
    // the process exit reaps this thread (it may be blocked in a socket read).
    {
        let client = client.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let tx = nudge_tx.clone();
                if let Err(e) = client.watch_library(|seq| {
                    let _ = tx.send(seq);
                }) {
                    eprintln!("nudge: {e} (falling back to poll; reconnecting)");
                }
                // Backoff before reconnect, but bail promptly if we're shutting down.
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }

    // --- FUSE mount on this thread (blocks until unmount) ----------------------
    let hydrator = Arc::new(EngineHydrator::new(provider.clone(), client.clone()));
    let fs = FuseAdapter::with_hydrator(provider.clone(), hydrator);
    let opts = vec![
        MountOption::FSName("maker".into()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
    ];
    println!("mounting at {mountpoint} (Ctrl-C to unmount)");
    let mount_result = fuser::mount2(fs, &mountpoint, &opts);

    // Mount returned (unmount or error): tear down the sync thread cleanly.
    stop.store(true, Ordering::Relaxed);
    let _ = sync_handle.join();

    if let Err(e) = mount_result {
        eprintln!("mount failed: {e}");
        std::process::exit(1);
    }
}
