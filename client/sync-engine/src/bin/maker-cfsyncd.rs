//! The **CfAPI** sync daemon CLI (Windows) — the driverless counterpart to `maker-syncd`.
//! Registers a Cloud Filter sync root, populates it from the local tree, runs the continuous
//! sync loop + ActionCable nudge, and hydrates files on demand (presigned GET) when read in
//! Explorer. Phase 1: download/read + remote-change invalidation. See
//! `client/fs-cfapi/BUILD.md` for the smoke test.
//!
//!   maker-cfsyncd <sync-root-dir> <db> <chunk_dir> <server_url> <device_token>

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("maker-cfsyncd is the Windows CfAPI daemon. On Linux use maker-syncd (FUSE); on macOS use the Maker app (WebDAV).");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    use sync_engine::{CfApiMount, DaemonConfig};

    let mut args = std::env::args().skip(1);
    let (root, db, chunk_dir, server_url, token) = match (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e)) => (a, b, c, d, e),
        _ => {
            eprintln!(
                "usage: maker-cfsyncd <sync-root-dir> <db> <chunk_dir> <server_url> <device_token>"
            );
            std::process::exit(2);
        }
    };

    // A sibling sentinel file the caller can `touch` to request a GRACEFUL shutdown. The smoke
    // creates `<db>.stop` instead of `Stop-Process -Force` so an in-flight push completes: on
    // seeing it we `drop(mount)`, which sets the stop flag and **joins the sync thread** (it
    // wakes within one poll interval and drains the outbox one last time before exiting).
    let stop_file = std::path::PathBuf::from(format!("{db}.stop"));
    let _ = std::fs::remove_file(&stop_file); // clear a stale sentinel from a prior run

    let cfg = DaemonConfig::new(root, db, chunk_dir, server_url, token);
    let mp = cfg.mountpoint.display().to_string();
    let mount = match CfApiMount::start(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to start CfAPI mount: {e}");
            std::process::exit(1);
        }
    };
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        // Interactive use: wait for Enter, then unmount cleanly.
        println!("CfAPI sync root at {mp} — browse it in Explorer. Press Enter to unmount.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        drop(mount); // Drop disconnects + unregisters the sync root
    } else {
        // Headless (service / CI / Start-Process): stdin is EOF immediately, so DON'T read it —
        // that would unmount the moment we mounted. Stay mounted until the process is terminated
        // (Ctrl-C / kill) OR the `<db>.stop` sentinel appears (graceful shutdown for the smoke).
        // `mount` is kept alive for the lifetime of the loop.
        println!(
            "CfAPI sync root at {mp} — running headless; touch {} or terminate to unmount.",
            stop_file.display()
        );
        while !stop_file.exists() {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        println!("stop sentinel seen — unmounting gracefully (final push first)");
        drop(mount); // Drop sets stop + joins the sync thread (one last outbox drain)
        let _ = std::fs::remove_file(&stop_file);
    }
}
