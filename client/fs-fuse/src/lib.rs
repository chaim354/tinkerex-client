//! FUSE adapter: translates kernel FUSE callbacks into [`fs_provider::Provider`]
//! calls. The same provider also backs the future Windows CfAPI and macOS File
//! Provider adapters — this is just one front-end.
//!
//! One implementation runs on every FUSE driver: libfuse (Linux), FUSE-T (macOS),
//! WinFsp (Windows, via the winfsp-rs binding). The driver is installed by the user.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs_provider::{Attr, FsError, Kind, Provider};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};

const TTL: Duration = Duration::from_secs(1);

fn errno(e: &FsError) -> i32 {
    match e {
        FsError::NotFound => libc::ENOENT,
        FsError::NotADirectory => libc::ENOTDIR,
        FsError::IsADirectory => libc::EISDIR,
        FsError::AlreadyExists => libc::EEXIST,
        FsError::NotEmpty => libc::ENOTEMPTY,
        _ => libc::EIO,
    }
}

fn file_attr(a: &Attr) -> FileAttr {
    let kind = match a.kind {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
    };
    let t = if a.mtime > 0 {
        UNIX_EPOCH + Duration::from_secs(a.mtime as u64)
    } else {
        SystemTime::now()
    };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: a.ino,
        size: a.size,
        blocks: (a.size + 511) / 512,
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind,
        perm: if a.kind == Kind::Dir { 0o755 } else { 0o644 },
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// On-demand byte hydration for the read path. The sync engine implements this; when a
/// read hits a chunk-cache miss, the adapter calls `hydrate(ino)` to fetch the file's
/// chunks from the cloud, then retries the read. Keeping it a trait here means `fs-fuse`
/// stays free of any server/transport dependency.
pub trait Hydrate: Send + Sync {
    /// Best-effort: pull `ino`'s missing chunks into the local cache. Errors are the
    /// hydrator's to log; the adapter just retries the read once afterward.
    fn hydrate(&self, ino: u64);
}

pub struct FuseAdapter<P: Provider> {
    provider: P,
    hydrator: Option<Arc<dyn Hydrate>>,
}

impl<P: Provider> FuseAdapter<P> {
    /// A mount with no hydrator — reads serve only what's already in the local cache
    /// (a cache miss is `EIO`). Used by the plain `maker-mount` binary and tests.
    pub fn new(provider: P) -> Self {
        Self { provider, hydrator: None }
    }

    /// A mount whose read path hydrates missing chunks on demand (the sync daemon).
    pub fn with_hydrator(provider: P, hydrator: Arc<dyn Hydrate>) -> Self {
        Self { provider, hydrator: Some(hydrator) }
    }
}

impl<P: Provider + 'static> Filesystem for FuseAdapter<P> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.provider.lookup(parent, &name.to_string_lossy()) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a), 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.provider.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &file_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // The only attribute we persist is size: a `size` change is a truncate (the
        // kernel issues `setattr(size=0)` on an `O_TRUNC` open, then writes the new,
        // possibly shorter, body). Drop the old tail past the new EOF here. Other
        // attrs (mode/owner/times) are accepted as no-ops and we echo current state.
        if let Some(sz) = size {
            if let Err(e) = self.provider.truncate(ino, sz) {
                reply.error(errno(&e));
                return;
            }
        }
        match self.provider.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &file_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (ino, FileType::Directory, "..".into()),
        ];
        match self.provider.readdir(ino) {
            Ok(children) => {
                for c in children {
                    let ft = match c.kind {
                        Kind::Dir => FileType::Directory,
                        Kind::File => FileType::RegularFile,
                    };
                    entries.push((c.ino, ft, c.name));
                }
            }
            Err(e) => {
                reply.error(errno(&e));
                return;
            }
        }
        for (i, (e_ino, ft, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // offset is the *next* entry index
            if reply.add(e_ino, (i + 1) as i64, ft, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        match self.provider.read(ino, offset as u64, size) {
            Ok(data) => reply.data(&data),
            // A chunk-cache miss on a file we know about: ask the hydrator to fetch the
            // bytes from the cloud, then retry the read once. This is the on-demand
            // hydration path (placeholder → materialized) for both initial pulls and
            // files repointed at new manifests by a remote change.
            Err(FsError::ChunkMissing(_)) if self.hydrator.is_some() => {
                self.hydrator.as_ref().unwrap().hydrate(ino);
                match self.provider.read(ino, offset as u64, size) {
                    Ok(data) => reply.data(&data),
                    Err(e) => reply.error(errno(&e)),
                }
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.provider.write(ino, offset as u64, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.provider.create(parent, &name.to_string_lossy()) {
            Ok(a) => reply.created(&TTL, &file_attr(&a), 0, 0, 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.provider.mkdir(parent, &name.to_string_lossy()) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a), 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.provider.unlink(parent, &name.to_string_lossy()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.provider.rmdir(parent, &name.to_string_lossy()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        match self.provider.rename(
            parent,
            &name.to_string_lossy(),
            newparent,
            &newname.to_string_lossy(),
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        match self.provider.flush(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Ensure writeback is finalized on close even if the app didn't flush.
        let _ = self.provider.flush(ino);
        reply.ok();
    }
}

// Re-export so callers don't need ROOT_INO from two crates.
pub use fs_provider::ROOT_INO as ROOT;
