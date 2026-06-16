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

// maker-syncd is the FUSE daemon (Linux). macOS mounts via WebDAV and Windows via WinFsp
// — both through the Maker app — so there's no FUSE `Daemon` to drive on those.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "maker-syncd is the Linux FUSE daemon. On macOS (WebDAV) and Windows (WinFsp), \
         mount via the Maker app (client/maker-app)."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use sync_engine::{Daemon, DaemonConfig};

#[cfg(target_os = "linux")]
fn main() {
    let mut args = std::env::args().skip(1);
    let (mountpoint, db, chunk_dir, server_url, token) = match (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) {
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
            // `Daemon::start` now fails loudly if the FUSE worker died on spawn (the
            // post-mount liveness check) instead of returning a happy handle.
            eprintln!("failed to start daemon: {e}");
            std::process::exit(1);
        }
    };

    // OBSERVABILITY HARDENING: do NOT print "mounted at" until we've actually confirmed the
    // mount attached. The original fuser-0.14 failure mode was that `spawn_mount2` returned
    // Ok, "mounted at" printed, and then nothing was mounted — the worker had already exited
    // (0.14 silently treated the first-read ENODEV as Ok). We now double-check two ways:
    //   1. `is_running()` — the session worker thread hasn't already finished.
    //   2. `findmnt` — the kernel actually shows a fuse mount at this path.
    // If either says the mount isn't live, print an explicit, actionable error and exit non-zero
    // rather than pretending success.
    if !daemon.is_running() {
        eprintln!(
            "FUSE mount did not attach at {mp} — the session worker exited immediately. \
             Check `dmesg`, the fuser version (need >=0.17; 0.14 fails silently on modern \
             kernels), /dev/fuse permissions, and that fusermount3 is setuid."
        );
        std::process::exit(1);
    }
    if !mount_is_present(&mp) {
        eprintln!(
            "FUSE worker is alive but no mount is visible at {mp} (findmnt found nothing). \
             The kernel mount did not attach — check `dmesg` and /dev/fuse."
        );
        std::process::exit(1);
    }

    println!("mounted at {mp} (Ctrl-C to unmount)");
    if let Err(e) = daemon.run_until_unmounted() {
        // `run_until_unmounted` waits (join) for an external umount/Ctrl-C, then reports any
        // worker error instead of panicking.
        eprintln!("FUSE session ended with an error: {e}");
        std::process::exit(1);
    }
}

/// Best-effort kernel-side confirmation that a filesystem is mounted at `path`, using
/// `findmnt` (util-linux, present on every modern Linux box). Returns `true` only if
/// `findmnt` positively reports a mount; if `findmnt` itself can't be run we return `true`
/// (don't fail the daemon just because the probe tool is missing — the `is_running()` check
/// above already caught the common silent-failure case).
#[cfg(target_os = "linux")]
fn mount_is_present(path: &str) -> bool {
    use std::process::Command;
    match Command::new("findmnt")
        .arg("--noheadings")
        .arg("--target")
        .arg(path)
        .output()
    {
        // `findmnt --target` prints the mount covering `path`; a 0 exit with non-empty output
        // means something is mounted at/above it. We additionally require the mountpoint column
        // to equal `path` so a mount on a *parent* dir doesn't read as a false positive.
        Ok(out) => {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|l| l.split_whitespace().next() == Some(path))
        }
        Err(_) => true, // findmnt unavailable — don't block on the probe.
    }
}
