//! FUSE adapter: translates kernel FUSE callbacks into [`fs_provider::Provider`]
//! calls. The same provider also backs the future Windows CfAPI and macOS File
//! Provider adapters — this is just one front-end.
//!
//! One implementation runs on every FUSE driver: libfuse (Linux), FUSE-T (macOS),
//! WinFsp (Windows, via the winfsp-rs binding). The driver is installed by the user.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs_provider::{Attr, FsError, Kind, LockRange, Provider};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyLock, ReplyOpen, ReplyWrite, Request, TimeOrNow,
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

/// A FUSE `setattr` time (`utimensat`/`cp -p`) as unix nanoseconds — what the provider
/// stores for `mtime`. `Now` resolves to the current wall clock.
fn time_or_now_nanos(t: TimeOrNow) -> i64 {
    let st = match t {
        TimeOrNow::SpecificTime(st) => st,
        TimeOrNow::Now => SystemTime::now(),
    };
    st.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

fn file_attr(a: &Attr) -> FileAttr {
    let kind = match a.kind {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
    };
    // `a.mtime` is the real modification time in unix **nanoseconds** (preserved + synced).
    let t = if a.mtime > 0 {
        UNIX_EPOCH + Duration::from_nanos(a.mtime as u64)
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
        perm: a.mode as u16,
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

/// Freshness-on-access: a quick catch-up pull when a file is opened, so an open never
/// serves a copy another device has already changed. The sync engine implements this as
/// a (throttled) changelog delta pull — cheap when already in sync. Belt-and-suspenders
/// over the background poll + ActionCable nudge, closing the sub-tick staleness window.
pub trait Freshness: Send + Sync {
    /// Best-effort: bring the local tree up to date before serving the open.
    fn refresh(&self);
}

/// Best-effort cross-device advisory lock. On open-for-write the adapter asks the engine
/// to acquire the server lock (so other *connected* clients see the file is being edited);
/// on close it releases. Non-blocking — if offline the implementor just no-ops and the
/// open proceeds on the local cached copy (conflict detection reconciles on reconnect).
pub trait RemoteLock: Send + Sync {
    fn acquire(&self, ino: u64);
    fn release(&self, ino: u64);
}

pub struct FuseAdapter<P: Provider> {
    provider: P,
    hydrator: Option<Arc<dyn Hydrate>>,
    freshness: Option<Arc<dyn Freshness>>,
    remote_lock: Option<Arc<dyn RemoteLock>>,
}

impl<P: Provider> FuseAdapter<P> {
    /// A mount with no hydrator — reads serve only what's already in the local cache
    /// (a cache miss is `EIO`). Used by the plain `maker-mount` binary and tests.
    pub fn new(provider: P) -> Self {
        Self { provider, hydrator: None, freshness: None, remote_lock: None }
    }

    /// A mount whose read path hydrates missing chunks on demand (the sync daemon).
    pub fn with_hydrator(provider: P, hydrator: Arc<dyn Hydrate>) -> Self {
        Self { provider, hydrator: Some(hydrator), freshness: None, remote_lock: None }
    }

    /// Add a freshness checker: every open first does a quick catch-up pull.
    pub fn with_freshness(mut self, freshness: Arc<dyn Freshness>) -> Self {
        self.freshness = Some(freshness);
        self
    }

    /// Add a best-effort cross-device lock: open-for-write acquires, close releases.
    pub fn with_remote_lock(mut self, remote_lock: Arc<dyn RemoteLock>) -> Self {
        self.remote_lock = Some(remote_lock);
        self
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
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // We persist size (truncate — the kernel issues `setattr(size=0)` on an `O_TRUNC`
        // open), mode (chmod — so the executable bit sticks + syncs), and mtime (the real
        // modification time from `utimensat`/`cp -p` — preserved + synced instead of being
        // overwritten with the sync time). Owner + atime/ctime are no-ops; we echo current state.
        if let Some(sz) = size {
            if let Err(e) = self.provider.truncate(ino, sz) {
                reply.error(errno(&e));
                return;
            }
        }
        if let Some(m) = mode {
            if let Err(e) = self.provider.set_mode(ino, m) {
                reply.error(errno(&e));
                return;
            }
        }
        if let Some(mt) = mtime {
            if let Err(e) = self.provider.set_mtime(ino, time_or_now_nanos(mt)) {
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
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.provider.create(parent, &name.to_string_lossy()) {
            Ok(a) => {
                // Capture the create-time mode (e.g. a tool creating a 0755 file) — the
                // default is 0644, so only override when it differs.
                if (mode & 0o7777) != a.mode {
                    let _ = self.provider.set_mode(a.ino, mode);
                }
                let a = self.provider.getattr(a.ino).unwrap_or(a);
                reply.created(&TTL, &file_attr(&a), 0, 0, 0)
            }
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

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        // Freshness-on-access: a quick catch-up pull so an open never serves a copy
        // another device already changed (closes the window between background sync
        // ticks). Throttled + cheap-when-in-sync inside the implementor.
        if let Some(f) = &self.freshness {
            f.refresh();
        }
        // Opening for write: pull the whole file local first, so random writes — which
        // seed the staging buffer from the committed content — never hit a chunk-cache
        // miss (a `write` has no hydrate-retry, unlike `read`). A read-only open needs
        // nothing here; reads hydrate lazily on miss (and freshness above already pulled
        // any new manifest, so that lazy hydrate fetches the latest chunks).
        const O_ACCMODE: i32 = 0o3;
        if (flags & O_ACCMODE) != 0 {
            // Best-effort cross-device lock: tell other connected clients we're editing
            // this. Non-blocking; if offline it no-ops and we open the local copy anyway.
            if let Some(rl) = &self.remote_lock {
                rl.acquire(ino);
            }
            if let Some(h) = &self.hydrator {
                h.hydrate(ino);
            }
        }
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
        lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Ensure writeback is finalized on close even if the app didn't flush.
        let _ = self.provider.flush(ino);
        // POSIX: a process's locks are released when it closes the file.
        if let Some(owner) = lock_owner {
            let _ = self.provider.release_locks(ino, owner);
        }
        // Release the best-effort cross-device lock on close (best-effort; TTL covers a
        // crash where this never runs).
        if let Some(rl) = &self.remote_lock {
            rl.release(ino);
        }
        reply.ok();
    }

    // Advisory POSIX byte-range locks routed through the provider's lock manager, so
    // lock state is explicit + uniform across our adapters (the Linux kernel would also
    // do local locking for free, but routing it here makes it visible to the app and
    // consistent with the WebDAV/WinFsp paths). Non-blocking: a conflicting `F_SETLK`
    // returns EAGAIN; a blocking `F_SETLKW` is not yet waited on (callers retry).
    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        if typ == libc::F_UNLCK {
            let _ = self.provider.unlock(ino, lock_owner, start, end);
            return reply.ok();
        }
        let lock = LockRange { owner: lock_owner, start, end, exclusive: typ == libc::F_WRLCK, pid };
        match self.provider.set_lock(ino, lock) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(libc::EAGAIN),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let probe = LockRange { owner: lock_owner, start, end, exclusive: typ == libc::F_WRLCK, pid };
        match self.provider.test_lock(ino, probe) {
            Ok(Some(c)) => {
                let t = if c.exclusive { libc::F_WRLCK } else { libc::F_RDLCK };
                reply.locked(c.start, c.end, t, c.pid)
            }
            // F_UNLCK in the reply means "no conflict — the lock could be placed".
            _ => reply.locked(start, end, libc::F_UNLCK, pid),
        }
    }
}

// Re-export so callers don't need ROOT_INO from two crates.
pub use fs_provider::ROOT_INO as ROOT;
