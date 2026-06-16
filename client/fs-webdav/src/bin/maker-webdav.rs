//! Standalone WebDAV server over the local-first provider — proves the adapter and is
//! the macOS mount path (no FUSE).
//!
//!   maker-webdav [addr] [db] [chunk_dir]
//!   e.g.  maker-webdav 127.0.0.1:9988 /tmp/maker-webdav.db /tmp/maker-webdav-chunks
//!
//! Then mount with the OS's native WebDAV client:
//!   macOS:  mkdir -p ~/Maker && mount_webdav -S -v Maker http://127.0.0.1:9988/ ~/Maker
//!   (or Finder → Go → Connect to Server → http://127.0.0.1:9988/)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use fs_provider::{LocalChunkStore, SqliteProvider};

fn main() {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:9988".into())
        .parse()
        .expect("addr like 127.0.0.1:9988");
    let db = args.next().unwrap_or_else(|| "/tmp/maker-webdav.db".into());
    let chunk_dir = args
        .next()
        .unwrap_or_else(|| "/tmp/maker-webdav-chunks".into());

    let store = Box::new(LocalChunkStore::new(chunk_dir).expect("chunk dir"));
    let provider =
        Arc::new(SqliteProvider::open(&PathBuf::from(db), store).expect("open provider"));

    println!("WebDAV serving on http://{addr}/");
    println!("mount (macOS): mkdir -p ~/Maker && mount_webdav -S -v Maker http://{addr}/ ~/Maker");
    if let Err(e) = fs_webdav::serve_blocking(provider, addr, None) {
        eprintln!("serve failed: {e}");
        std::process::exit(1);
    }
}
