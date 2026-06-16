//! Mount the local-first provider as a real filesystem.
//!
//!   maker-mount <mountpoint> [db_path] [chunk_dir]
//!
//! Requires a FUSE driver installed: libfuse (Linux), FUSE-T (macOS), WinFsp
//! (Windows). Ctrl-C to unmount.

use std::path::PathBuf;

use fs_fuse::FuseAdapter;
use fs_provider::{LocalChunkStore, SqliteProvider};
use fuser::MountOption;

fn main() {
    let mut args = std::env::args().skip(1);
    let mountpoint = match args.next() {
        Some(m) => m,
        None => {
            eprintln!("usage: maker-mount <mountpoint> [db_path] [chunk_dir]");
            std::process::exit(2);
        }
    };
    let db_path = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/maker-fs.db".into()));
    let chunk_dir = args.next().unwrap_or_else(|| "/tmp/maker-fs-chunks".into());

    let store = Box::new(LocalChunkStore::new(chunk_dir).expect("chunk dir"));
    let provider = SqliteProvider::open(&db_path, store).expect("open provider");
    let fs = FuseAdapter::new(provider);

    println!("mounting at {mountpoint} (Ctrl-C to unmount)");
    // fuser 0.17: mount2 takes a &fuser::Config (was a &[MountOption] slice). Config is
    // #[non_exhaustive], so start from default() and set the public field (no struct literal).
    let mut config = fuser::Config::default();
    let mut opts = vec![MountOption::FSName("maker".into())];
    // `AutoUnmount` (clean up the mount if we die) + `DefaultPermissions` (kernel perm
    // checks) are the right defaults for the desktop app, but in a headless container the
    // fuser-0.17 `AutoUnmount` path requires a non-Owner allow ACL (`allow_other`/
    // `allow_root`) and errors otherwise (`auto_unmount requires acl != Owner`). Let a
    // test/CI harness drop them so a bare-root container mount works.
    if std::env::var_os("MAKER_MOUNT_NO_AUTOUNMOUNT").is_none() {
        opts.push(MountOption::AutoUnmount);
        opts.push(MountOption::DefaultPermissions);
    }
    config.mount_options = opts;
    if let Err(e) = fuser::mount2(fs, &mountpoint, &config) {
        eprintln!("mount failed: {e}");
        std::process::exit(1);
    }
}
