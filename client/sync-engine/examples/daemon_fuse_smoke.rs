//! STEP-2 bisection: the FULL `Daemon::start` FUSE path (hydrator + freshness + remote-lock
//! + notifier injection + the two background sync/nudge threads + the liveness check) — but
//! pointed at a DEAD server URL so the network threads just error-log. This isolates the
//! daemon's FUSE WIRING from any server interaction.
//!
//! The adapter-only smoke (`fs-fuse/examples/adapter_mount_smoke`) HELD, so a drop HERE pins
//! the bug on the daemon's extra wiring (the trait objects, the notifier injection, the
//! background threads sharing the provider, or the liveness check). A HELD here means the
//! mount is fine and the drop is triggered by something the live server does.
//!
//! Watches `is_running()` for ~3s after `start` and reports `SMOKE: MOUNT HELD` (exit 0) or
//! `SMOKE: MOUNT DROPPED` (exit 1). Linux-only (the FUSE `Daemon`).
//!
//!   cargo run --manifest-path client/sync-engine/Cargo.toml --example daemon_fuse_smoke

#[cfg(target_os = "linux")]
fn main() {
    use std::time::{Duration, Instant};
    use sync_engine::{Daemon, DaemonConfig};

    let base = std::env::temp_dir().join(format!("daemon-fuse-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("base");
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).ok();
    let db = base.join("a.db");
    let chunks = base.join("chunks");
    std::fs::create_dir_all(&chunks).ok();

    // Dead server URL: the sync + nudge threads will fail to connect and just error-log; the
    // FUSE mount + all the daemon wiring is exactly the production path. A short poll so the
    // dead-server threads don't spin too long.
    let cfg = DaemonConfig::new(
        &mnt,
        &db,
        &chunks,
        "http://127.0.0.1:9".to_string(), // 9 = discard, nothing listening
        "smoke-token".to_string(),
    );
    let cfg = DaemonConfig {
        poll: Duration::from_millis(500),
        ..cfg
    };

    eprintln!("SMOKE: Daemon::start at {} (dead server)…", mnt.display());
    let daemon = match Daemon::start(cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SMOKE: Daemon::start returned Err (liveness check tripped?): {e}");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    };
    eprintln!(
        "SMOKE: Daemon::start Ok, is_running={}",
        daemon.is_running()
    );

    // Watch for ~3s: if the mount drops, is_running() flips false.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut dropped_at: Option<Duration> = None;
    let start = Instant::now();
    while Instant::now() < deadline {
        if !daemon.is_running() {
            dropped_at = Some(start.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let held = daemon.is_running();
    eprintln!("SMOKE: after watch, is_running={held} dropped_at={dropped_at:?}");
    // Drop the daemon explicitly (unmounts + stops threads) before exiting.
    drop(daemon);

    if held {
        println!("SMOKE: MOUNT HELD");
        std::process::exit(0);
    } else {
        println!("SMOKE: MOUNT DROPPED");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SMOKE: skipped — daemon FUSE smoke is Linux-only");
}
