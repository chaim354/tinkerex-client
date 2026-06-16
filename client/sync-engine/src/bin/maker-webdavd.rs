//! The **WebDAV** sync daemon CLI (macOS) — the WebDAV counterpart to `maker-syncd` (FUSE) and
//! `maker-cfsyncd` (CfAPI). Serves the local tree over localhost WebDAV, mounts it with the native
//! `/sbin/mount_webdav`, and runs the continuous sync loop + ActionCable nudge — exactly the
//! `sync_engine::WebdavMount` the Tauri app's `sync_start` launches on macOS. Exists so the macOS
//! real-mount leg of the two-device end-to-end test is drivable from a script (two instances, two
//! ports, two tokens, one library).
//!
//!   maker-webdavd <mountpoint> <db> <chunk_dir> <server_url> <device_token> [webdav_port]
//!
//! Like `maker-cfsyncd`, a sibling `<db>.stop` sentinel requests a graceful shutdown (final push
//! before unmount) — so a test harness can stop it without a hard kill that drops an in-flight push.
//! Needs an INTERACTIVE macOS login session: `mount_webdav` won't hold a mount headless.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "maker-webdavd is the macOS WebDAV daemon. On Linux use maker-syncd (FUSE); on Windows use \
         maker-cfsyncd (CfAPI)."
    );
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    use sync_engine::{DaemonConfig, WebdavMount};

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
                    "usage: maker-webdavd <mountpoint> <db> <chunk_dir> <server_url> <device_token> [webdav_port]"
                );
            std::process::exit(2);
        }
    };
    // Distinct WebDAV ports let two instances run at once on one box (the two-device test).
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(9988);

    let stop_file = std::path::PathBuf::from(format!("{db}.stop"));
    let _ = std::fs::remove_file(&stop_file); // clear a stale sentinel from a prior run

    let mut cfg = DaemonConfig::new(mountpoint, db, chunk_dir, server_url, token);
    cfg.webdav_port = port;
    let mp = cfg.mountpoint.display().to_string();
    let mount = match WebdavMount::start(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to start WebDAV mount: {e}");
            std::process::exit(1);
        }
    };

    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        println!(
            "WebDAV mounted at {mp} (port {port}) — browse it in Finder. Press Enter to unmount."
        );
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        drop(mount); // unmount + stop loops
    } else {
        // Headless (Start-Process / & background): stdin is EOF immediately, so DON'T read it. Stay
        // mounted until terminated OR the `<db>.stop` sentinel appears (graceful: final push first).
        println!(
            "WebDAV mounted at {mp} (port {port}) — running until terminated; touch {} to unmount gracefully.",
            stop_file.display()
        );
        while !stop_file.exists() {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        println!("stop sentinel seen — unmounting gracefully (final push first)");
        drop(mount); // Drop unmounts + joins the sync thread (one last outbox drain)
        let _ = std::fs::remove_file(&stop_file);
    }
}
