//! Full integrated macOS path: `WebdavMount` (WebDAV + native mount_webdav) + the sync
//! loop, against a live server. Mounts, writes a file *through the mount*, waits for the
//! sync loop to push it, then verifies it landed on the server.
//!
//!   SERVER_URL=http://192.168.1.95 cargo run --example webdav_live

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sync_client::ServerClient;
use sync_engine::{DaemonConfig, WebdavMount};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://192.168.1.95".into());
    let home = std::env::var("HOME").unwrap();
    let mp = format!("{home}/MakerDavTest");
    let tag = uniq();

    let mut client = ServerClient::new(&server);
    let email = format!("webdav-{tag}@example.com");
    let reg = client.register(&email, "webdav-test").expect("register");
    println!("✓ registered {email}");

    let _ = std::fs::remove_file("/tmp/maker-webdav-live.db");
    let _ = std::fs::remove_dir_all("/tmp/maker-webdav-live-chunks");
    let mut cfg = DaemonConfig::new(
        &mp,
        "/tmp/maker-webdav-live.db",
        "/tmp/maker-webdav-live-chunks",
        &server,
        reg.device_token,
    );
    cfg.webdav_port = 9989;

    let mount = WebdavMount::start(cfg).expect("WebdavMount::start");
    println!("✓ mounted at {}", mount.mountpoint().display());

    // Write a file THROUGH the macOS WebDAV mount.
    std::thread::sleep(Duration::from_millis(800));
    let fname = format!("hello-{tag}.txt");
    std::fs::write(format!("{mp}/{fname}"), b"synced from a mac via webdav!\n")
        .expect("write through mount");
    println!("✓ wrote {fname} into the mounted volume");

    // Let the sync loop (2s poll) push it.
    println!("  waiting for the sync loop to push…");
    std::thread::sleep(Duration::from_secs(5));

    // Verify server-side via a second device on the same account.
    let dev2 = client.create_device("verify").expect("device 2");
    let c2 = ServerClient::with_token(&server, dev2.device_token);
    let tree = c2.tree(0).expect("tree");
    let found = tree.nodes.iter().find(|n| n.name == fname);
    match found {
        Some(n) => {
            let size: u64 = n.manifest.as_ref().map(|m| m.iter().map(|c| c.length).sum()).unwrap_or(0);
            println!("✓ SERVER has {fname} ({size} B) — WebDAV write → sync push → server confirmed");
        }
        None => println!("✗ {fname} NOT found on server (tree has {} nodes)", tree.nodes.len()),
    }

    drop(mount); // unmount + stop loops
    println!("✓ unmounted; done");
}
