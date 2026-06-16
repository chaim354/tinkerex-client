//! Minimal, dependency-free fuser-0.17 mount smoke — the STEP-1 isolation experiment.
//!
//! Mounts a trivial read-only "hello" filesystem (root dir ino 1 + one file `hello.txt`)
//! with the SAME `fuser::Config` the daemon uses (`FSName` + `DefaultPermissions`, via
//! `spawn_mount2`), then SELF-CHECKS whether the mount actually HOLDS:
//!   - sleep ~1s so the kernel either settles the mount or tears it down,
//!   - the background session worker thread is still running (`!guard.is_finished()`),
//!   - `findmnt` positively reports a fuse mount at the mountpoint,
//!   - the file reads back its expected bytes through the mount.
//! Prints `SMOKE: MOUNT HELD` + exit 0 on success, or `SMOKE: MOUNT DROPPED` + exit 1.
//!
//! This is stock fuser code (no `fs-provider`, no sync threads, no `FuseAdapter`) so a
//! drop here pins the bug on the fuser crate vs this kernel; a HELD here pins it on our
//! adapter/daemon. Run on the Linux runner:
//!   cargo run --manifest-path client/fs-fuse/Cargo.toml --example mount_smoke -- <mnt>
//! (mountpoint optional; defaults to a fresh tempdir under $TMPDIR).
//!
//! fuser is Linux-only in this workspace (`cfg(target_os = "linux")` in the daemon), but
//! this example depends on `fuser` directly, so it compiles wherever fuser does. The body
//! is guarded so non-Linux just prints a skip.

#[cfg(target_os = "linux")]
fn main() {
    use std::ffi::OsStr;
    use std::time::{Duration, UNIX_EPOCH};

    use fuser::{
        FileAttr, FileType, Filesystem, Generation, INodeNo, MountOption, ReplyAttr, ReplyData,
        ReplyDirectory, ReplyEntry, Request,
    };

    const TTL: Duration = Duration::from_secs(1);
    const HELLO: &str = "Hello World!\n";

    fn dir_attr() -> FileAttr {
        FileAttr {
            ino: INodeNo::ROOT,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }
    fn file_attr() -> FileAttr {
        FileAttr {
            ino: INodeNo(2),
            size: HELLO.len() as u64,
            blocks: 1,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    struct HelloFS;
    impl Filesystem for HelloFS {
        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            if u64::from(parent) == 1 && name.to_str() == Some("hello.txt") {
                reply.entry(&TTL, &file_attr(), Generation(0));
            } else {
                reply.error(fuser::Errno::ENOENT);
            }
        }
        fn getattr(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: Option<fuser::FileHandle>,
            reply: ReplyAttr,
        ) {
            match u64::from(ino) {
                1 => reply.attr(&TTL, &dir_attr()),
                2 => reply.attr(&TTL, &file_attr()),
                _ => reply.error(fuser::Errno::ENOENT),
            }
        }
        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: fuser::FileHandle,
            offset: u64,
            _size: u32,
            _flags: fuser::OpenFlags,
            _lock: Option<fuser::LockOwner>,
            reply: ReplyData,
        ) {
            if u64::from(ino) == 2 {
                reply.data(&HELLO.as_bytes()[offset as usize..]);
            } else {
                reply.error(fuser::Errno::ENOENT);
            }
        }
        fn readdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: fuser::FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            if u64::from(ino) != 1 {
                reply.error(fuser::Errno::ENOENT);
                return;
            }
            let entries = [
                (1u64, FileType::Directory, "."),
                (1, FileType::Directory, ".."),
                (2, FileType::RegularFile, "hello.txt"),
            ];
            for (i, (e_ino, ft, name)) in entries.into_iter().enumerate().skip(offset as usize) {
                if reply.add(INodeNo(e_ino), (i + 1) as u64, ft, name) {
                    break;
                }
            }
            reply.ok();
        }
    }

    // --- mountpoint: arg or a fresh tempdir ---
    let mnt = std::env::args().nth(1).unwrap_or_else(|| {
        let p = std::env::temp_dir().join(format!("fuser-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create tempdir mountpoint");
        p.to_string_lossy().into_owned()
    });
    std::fs::create_dir_all(&mnt).ok();
    eprintln!(
        "SMOKE: mounting at {mnt} (fuser {})",
        env!("CARGO_PKG_VERSION")
    );

    // EXACTLY the daemon's Config: FSName + DefaultPermissions, via spawn_mount2.
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("maker".into()),
        MountOption::DefaultPermissions,
    ];

    let session = match fuser::spawn_mount2(HelloFS, &mnt, &config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SMOKE: spawn_mount2 FAILED at mount/handshake: {e}");
            println!("SMOKE: MOUNT DROPPED");
            std::process::exit(1);
        }
    };
    eprintln!("SMOKE: spawn_mount2 returned Ok (mount + INIT handshake done); checking it holds…");

    // Give the kernel a beat to either settle the mount or tear it down.
    std::thread::sleep(Duration::from_millis(1000));

    let worker_alive = !session.guard.is_finished();
    let findmnt_ok = findmnt_reports(&mnt);
    let read_ok = std::fs::read_to_string(format!("{mnt}/hello.txt"))
        .map(|s| s == HELLO)
        .unwrap_or(false);
    eprintln!("SMOKE: worker_alive={worker_alive} findmnt_ok={findmnt_ok} read_ok={read_ok}");

    // HELD = worker still serving AND the kernel shows the mount. read_ok is a bonus signal
    // (DefaultPermissions or a missing read could fail the read yet the mount still HELD).
    let held = worker_alive && findmnt_ok;

    // Clean up the mount before exiting (best-effort).
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
    match Command::new("findmnt")
        .arg("--noheadings")
        .arg("--target")
        .arg(path)
        .output()
    {
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
    eprintln!("SMOKE: skipped — fuser mount smoke is Linux-only");
}
