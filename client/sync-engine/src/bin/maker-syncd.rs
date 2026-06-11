//! The sync daemon CLI: mount the local-first filesystem **and** run the continuous
//! sync loop + the ActionCable nudge against it. A save (FUSE `flush`/`release`)
//! enqueues the node in the local outbox; the sync thread pushes it; a `{head_seq}`
//! nudge (or the poll fallback) pulls remote changes; a read on a not-yet-hydrated file
//! fetches its chunks on demand. This is CLAUDE.md §12 step 1 wired together.
//!
//!   maker-syncd <mountpoint> <db> <chunk_dir> <server_url> <device_token>
//!
//! All the wiring lives in [`sync_engine::Daemon`] (shared with the Tauri app); this
//! binary just parses args, starts it, and blocks until unmount (Ctrl-C / `umount`).

use sync_engine::{Daemon, DaemonConfig};

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

    let cfg = DaemonConfig::new(mountpoint, db, chunk_dir, server_url, token);
    let mp = cfg.mountpoint.display().to_string();
    let daemon = match Daemon::start(cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            std::process::exit(1);
        }
    };
    println!("mounted at {mp} (Ctrl-C to unmount)");
    daemon.run_until_unmounted();
}
