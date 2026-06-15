//! STEP-2 bisection smoke: mount the REAL `FuseAdapter` over a fresh `SqliteProvider`,
//! with NO hydrator / freshness / remote-lock and NO sync threads — just the adapter the
//! daemon mounts. Same self-check + verdict as `mount_smoke`: `SMOKE: MOUNT HELD` (exit 0)
//! or `SMOKE: MOUNT DROPPED` (exit 1).
//!
//! `mount_smoke` proved stock fuser-0.17 HOLDS on this kernel with the daemon's exact
//! Config, so the instant drop is in our code. This isolates the `FuseAdapter` itself
//! (over a clean local provider) from the rest of the daemon (sync/nudge threads, hydrator,
//! notifier injection, liveness check). If THIS drops, the bug is in the adapter or the
//! provider's first FUSE callbacks; if it HOLDS, the bug is in the daemon wiring around it.
//!
//!   cargo run --manifest-path client/fs-fuse/Cargo.toml --example adapter_mount_smoke -- <mnt>

#[cfg(target_os = "linux")]
fn main() {
    use std::time::Duration;

    use fs_fuse::FuseAdapter;
    use fs_provider::{LocalChunkStore, Provider, SqliteProvider};

    // Fresh temp DB + chunk dir + mountpoint (mirrors the daemon's on-disk layout).
    let base = std::env::temp_dir().join(format!("fuse-adapter-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("create base dir");
    let db = base.join("a.db");
    let chunks = base.join("chunks");
    let mnt = std::env::args().nth(1).unwrap_or_else(|| base.join("mnt").to_string_lossy().into_owned());
    std::fs::create_dir_all(&chunks).ok();
    std::fs::create_dir_all(&mnt).ok();
    eprintln!("SMOKE: adapter mount at {mnt} (db {}, chunks {})", db.display(), chunks.display());

    let store = Box::new(LocalChunkStore::new(&chunks).expect("chunk store"));
    let provider = SqliteProvider::open(&db, store).expect("open provider");

    // Sanity: the very first FUSE op the kernel sends after INIT is usually getattr(ROOT).
    // Probe it on the main thread first, so a provider panic surfaces HERE (with a clear
    // message) instead of silently aborting the FUSE worker thread → instant mount drop.
    match provider.getattr(fs_fuse::ROOT) {
        Ok(a) => eprintln!("SMOKE: provider.getattr(ROOT) ok: ino={} kind={:?} mode={:#o}", a.ino, a.kind, a.mode),
        Err(e) => {
            eprintln!("SMOKE: provider.getattr(ROOT) FAILED: {e:?} — this would abort the FUSE worker");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    }

    let fs = FuseAdapter::new(provider); // no hydrator/freshness/remote-lock, no sync threads

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("maker".into()),
        fuser::MountOption::DefaultPermissions,
    ];

    let session = match fuser::spawn_mount2(fs, &mnt, &config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SMOKE: spawn_mount2 FAILED: {e}");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    };
    eprintln!("SMOKE: spawn_mount2 returned Ok; checking it holds…");

    std::thread::sleep(Duration::from_millis(1000));

    let worker_alive = !session.guard.is_finished();
    let findmnt_ok = findmnt_reports(&mnt);
    // A fresh tree has an empty root; readdir should succeed and list `.`/`..` only.
    let readdir_ok = std::fs::read_dir(&mnt).is_ok();
    eprintln!("SMOKE: worker_alive={worker_alive} findmnt_ok={findmnt_ok} readdir_ok={readdir_ok}");

    let held = worker_alive && findmnt_ok;
    let _ = session.umount_and_join();

    if held {
        println!("SMOKE: MOUNT HELD");
        std::process::exit(0);
    } else {
        println!("SMOKE: MOUNT DROPPED");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn findmnt_reports(path: &str) -> bool {
    use std::process::Command;
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

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SMOKE: skipped — fuser adapter smoke is Linux-only");
}
