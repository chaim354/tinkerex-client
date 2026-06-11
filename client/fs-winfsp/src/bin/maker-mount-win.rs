//! Standalone WinFsp mount — the Windows counterpart to `fs-fuse`'s `maker-mount`.
//! Proves the adapter against a real `Provider` before wiring it into the sync daemon
//! (the same staged approach used to bring up FUSE first).
//!
//!   maker-mount-win <mountpoint> [db_path] [chunk_dir]
//!   e.g.  maker-mount-win Z: C:\maker\tree.db C:\maker\chunks
//!
//! Requires WinFsp installed (https://winfsp.dev). Ctrl-C to unmount.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    use fs_provider::{LocalChunkStore, SqliteProvider};

    let mut args = std::env::args().skip(1);
    let mountpoint = match args.next() {
        Some(m) => m,
        None => {
            eprintln!("usage: maker-mount-win <mountpoint> [db_path] [chunk_dir]");
            std::process::exit(2);
        }
    };
    let db = args.next().unwrap_or_else(|| "maker-fs.db".into());
    let chunk_dir = args.next().unwrap_or_else(|| "maker-fs-chunks".into());

    let store = Box::new(LocalChunkStore::new(chunk_dir).expect("chunk dir"));
    let provider = SqliteProvider::open(&PathBuf::from(db), store).expect("open provider");

    if let Err(e) = fs_winfsp::mount_blocking(provider, &mountpoint) {
        eprintln!("mount failed: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("maker-mount-win is Windows-only (it links WinFsp). Build and run it on Windows.");
    std::process::exit(1);
}
