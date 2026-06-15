//! Regression smoke for the instant-mount-drop bug: `Daemon::run_until_unmounted` must
//! WAIT for an external unmount, not trigger one itself.
//!
//! The bug: `run_until_unmounted` called fuser 0.17's `BackgroundSession::umount_and_join()`,
//! which actively unmounts FIRST — so the daemon unmounted itself the instant it started
//! ("mounted at" → "sync loop stopped", `findmnt` empty). The fix uses `join()` (wait-only).
//!
//! This drives the real `Daemon` (dead server so the sync/nudge threads only error-log),
//! runs `run_until_unmounted` on a background thread, and asserts the mount is STILL present
//! ~1.5s later (the worker did NOT exit on its own). Then it externally `fusermount -u`s the
//! mount to release `run_until_unmounted` and joins, confirming the wait actually unblocks on
//! a real external unmount. `SMOKE: MOUNT HELD` (exit 0) / `SMOKE: MOUNT DROPPED` (exit 1).
//!
//!   cargo run --manifest-path client/sync-engine/Cargo.toml --example run_until_unmounted_smoke

#[cfg(target_os = "linux")]
fn main() {
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;
    use sync_engine::{Daemon, DaemonConfig};

    fn findmnt(path: &str) -> bool {
        match Command::new("findmnt").arg("--noheadings").arg("--target").arg(path).output() {
            Ok(out) => {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .any(|l| l.split_whitespace().next() == Some(path))
            }
            Err(_) => false,
        }
    }

    let base = std::env::temp_dir().join(format!("run-until-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("base");
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).ok();
    let mnt_s = mnt.to_string_lossy().into_owned();

    let cfg = DaemonConfig::new(
        &mnt,
        base.join("a.db"),
        base.join("chunks"),
        "http://127.0.0.1:9".to_string(),
        "smoke-token".to_string(),
    );
    let cfg = DaemonConfig { poll: Duration::from_millis(500), ..cfg };

    eprintln!("SMOKE: Daemon::start at {mnt_s} (dead server)…");
    let daemon = match Daemon::start(cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SMOKE: Daemon::start Err: {e}");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    };

    // Run the blocking waiter on a background thread; it must NOT return until we externally
    // unmount. It signals `done` when it finally returns.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let waiter = std::thread::spawn(move || {
        let _ = daemon.run_until_unmounted();
        let _ = done_tx.send(());
    });

    // The mount must still be live ~1.5s in (the bug would have unmounted it instantly).
    std::thread::sleep(Duration::from_millis(1500));
    let still_mounted = findmnt(&mnt_s);
    let returned_early = done_rx.try_recv().is_ok();
    eprintln!("SMOKE: after 1.5s — still_mounted={still_mounted} run_until_unmounted_returned_early={returned_early}");

    if !still_mounted || returned_early {
        eprintln!("SMOKE: the daemon unmounted itself (the instant-drop bug) — run_until_unmounted did not WAIT");
        let _ = Command::new("fusermount").arg("-u").arg(&mnt_s).status();
        let _ = waiter.join();
        println!("SMOKE: MOUNT DROPPED");
        std::process::exit(1);
    }

    // Now release it the right way: an EXTERNAL unmount. run_until_unmounted must then unblock.
    eprintln!("SMOKE: externally unmounting to release run_until_unmounted…");
    let _ = Command::new("fusermount").arg("-u").arg(&mnt_s).status();
    match done_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => {
            let _ = waiter.join();
            eprintln!("SMOKE: run_until_unmounted unblocked on the external umount");
            println!("SMOKE: MOUNT HELD");
            std::process::exit(0);
        }
        Err(_) => {
            eprintln!("SMOKE: run_until_unmounted did NOT unblock after an external umount (hung)");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SMOKE: skipped — run_until_unmounted smoke is Linux-only");
}
