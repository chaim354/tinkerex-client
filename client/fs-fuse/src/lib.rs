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
// fuser 0.17 wraps many former bare-integer params in strongly-typed newtypes
// (`INodeNo(u64)`, `FileHandle(u64)`, `LockOwner(u64)`, `OpenFlags(i32)`, `Generation(u64)`)
// and bitflags (`FopenFlags`, `RenameFlags`, `WriteFlags`, `BsdFileFlags`), and the
// `Filesystem` trait methods now take `&self` (not `&mut self`). See the migration notes
// at the bottom of this file's git history / the 0.17 CHANGELOG.
use fuser::{
    FileAttr, FileType, Filesystem, FopenFlags, Generation, INodeNo, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen, ReplyWrite, Request,
    TimeOrNow,
};

const TTL: Duration = Duration::from_secs(1);

/// G8 guardrail: decide whether a `mknod(mode)` request targets a node kind we model.
/// We support only regular files (and dirs, which never come through `mknod`). A `mode`
/// with no type bits (`S_IFMT == 0`) is treated as a regular file per POSIX. Everything
/// else — FIFO (`S_IFIFO`), socket (`S_IFSOCK`), block (`S_IFBLK`) / char (`S_IFCHR`)
/// device, or symlink (`S_IFLNK`, though that arrives via `symlink`) — is a deliberate
/// non-goal and is rejected. Pure + platform-independent so it is unit-testable on any OS.
fn mknod_is_regular_file(mode: u32) -> bool {
    // `S_IFMT`/`S_IFREG` are `mode_t`, which is `u32` on Linux but `u16` on macOS/BSD —
    // cast both to `u32` so this compiles + unit-tests on any host (the FUSE `mode` param
    // is always `u32`). The bit values are identical across platforms.
    let fmt = mode & (libc::S_IFMT as u32);
    fmt == 0 || fmt == (libc::S_IFREG as u32)
}

fn errno(e: &FsError) -> fuser::Errno {
    // fuser 0.17: ReplyXxx::error() takes fuser::Errno (named consts), not a bare i32.
    match e {
        FsError::NotFound => fuser::Errno::ENOENT,
        FsError::NotADirectory => fuser::Errno::ENOTDIR,
        FsError::IsADirectory => fuser::Errno::EISDIR,
        FsError::AlreadyExists => fuser::Errno::EEXIST,
        FsError::NotEmpty => fuser::Errno::ENOTEMPTY,
        // A directory moved into its own subtree (rename cycle) and other structurally
        // invalid arguments → EINVAL, the POSIX errno pjdfstest's rename/17 expects.
        FsError::InvalidArgument => fuser::Errno::EINVAL,
        // A path component longer than NAME_MAX (255 bytes) → ENAMETOOLONG (pjdfstest <op>/02).
        FsError::NameTooLong => fuser::Errno::ENAMETOOLONG,
        // Special files (symlink/FIFO/socket/device) are a deliberate non-goal — reject
        // the attempt cleanly with EPERM rather than ENOSYS/EIO so callers (cp -a, tar,
        // git) get a deterministic "operation not permitted" instead of a confusing crash.
        FsError::NotSupported => fuser::Errno::EPERM,
        _ => fuser::Errno::EIO,
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
        Kind::Symlink => FileType::Symlink,
    };
    // Convert a unix-nanos timestamp to a SystemTime, falling back to now for an unset (0) one.
    let to_time = |nanos: i64| {
        if nanos > 0 {
            UNIX_EPOCH + Duration::from_nanos(nanos as u64)
        } else {
            SystemTime::now()
        }
    };
    // `a.mtime` is the real modification time in unix **nanoseconds** (preserved + synced);
    // `a.atime` is the access time (device-local, set via `utimensat`). ctime/crtime have no
    // separate store, so they track mtime.
    let mtime = to_time(a.mtime);
    let atime = to_time(a.atime);
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        // fuser 0.17: FileAttr.ino is now the `INodeNo` newtype (was a bare u64).
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: (a.size + 511) / 512,
        atime,
        mtime,
        ctime: mtime,
        crtime: mtime,
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
    /// Best-effort: pull `ino`'s **whole** missing content into the local cache. Used by
    /// open-for-write (where a random write must not miss any region) and as the fallback
    /// for callers that don't know a range.
    fn hydrate(&self, ino: u64);

    /// **Progressive / range hydration**: best-effort pull only the chunks overlapping
    /// `[offset, offset + size)` — what a read at that range needs — instead of the whole
    /// file. The read path calls this so opening a huge file and reading just its header
    /// fetches a handful of chunks, not gigabytes. Default delegates to the whole-file
    /// [`hydrate`](Self::hydrate), so an implementor that doesn't (yet) range-fetch stays
    /// correct (it just fetches more than strictly needed).
    fn hydrate_range(&self, ino: u64, _offset: u64, _size: u32) {
        self.hydrate(ino);
    }
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

// fuser 0.17: `Filesystem` methods take `&self` (was `&mut self` in 0.14) and the mounted
// FS must be `Send + Sync + 'static`. Our `FuseAdapter` only ever calls `&self` methods on
// the `Provider` (itself `Send + Sync`, all `&self`), so the switch is mechanical — no
// interior mutability needed.
impl<P: Provider + 'static> Filesystem for FuseAdapter<P> {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.provider.lookup(parent.0, &name.to_string_lossy()) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.15+ added the `fh: Option<FileHandle>` arg to getattr (a stat on an open fd);
    // we don't use file handles, so it's ignored.
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<fuser::FileHandle>, reply: ReplyAttr) {
        match self.provider.getattr(ino.0) {
            Ok(a) => reply.attr(&TTL, &file_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = ino.0;
        // We persist size (truncate — the kernel issues `setattr(size=0)` on an `O_TRUNC`
        // open), mode (chmod — so the executable bit sticks + syncs), mtime (the real
        // modification time from `utimensat`/`cp -p` — preserved + synced instead of being
        // overwritten with the sync time), and atime (device-local, not synced). The kernel
        // resolves `utimensat`'s special values before they reach us: UTIME_OMIT arrives as
        // `None` (so we leave that timestamp alone) and UTIME_NOW as `TimeOrNow::Now`. Owner
        // + ctime are no-ops; we echo current state.
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
        if let Some(at) = atime {
            if let Err(e) = self.provider.set_atime(ino, time_or_now_nanos(at)) {
                reply.error(errno(&e));
                return;
            }
        }
        match self.provider.getattr(ino) {
            Ok(a) => reply.attr(&TTL, &file_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `fh` is a `FileHandle` and `offset` is now `u64` (was `i64`); also
    // `ReplyDirectory::add` takes an `INodeNo` and a `u64` offset.
    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: fuser::FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let ino = ino.0;
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
                        Kind::Symlink => FileType::Symlink,
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
            if reply.add(INodeNo(e_ino), (i + 1) as u64, ft, name) {
                break;
            }
        }
        reply.ok();
    }

    // fuser 0.17: `fh: FileHandle`, `offset: u64`, `flags: OpenFlags`, `lock_owner: Option<LockOwner>`.
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let ino = ino.0;
        // fuser 0.17: `offset` is already `u64` (was `i64` in 0.14), matching `Provider::read`.
        match self.provider.read(ino, offset, size) {
            Ok(data) => reply.data(&data),
            // A chunk-cache miss on a file we know about: ask the hydrator to fetch the
            // bytes from the cloud, then retry the read once. This is the on-demand
            // hydration path (placeholder → materialized) for both initial pulls and
            // files repointed at new manifests by a remote change.
            //
            // **Range hydration:** fetch only the chunks overlapping the touched
            // `[offset, size)` — a header read of a huge file pulls a handful of chunks,
            // not the whole file. Write-open already full-hydrated up front (`open`), so
            // random writes never reach this miss path with a partial cache.
            Err(FsError::ChunkMissing(_)) if self.hydrator.is_some() => {
                self.hydrator.as_ref().unwrap().hydrate_range(ino, offset, size);
                match self.provider.read(ino, offset, size) {
                    Ok(data) => reply.data(&data),
                    Err(e) => reply.error(errno(&e)),
                }
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `fh: FileHandle`, `offset: u64`, `write_flags: WriteFlags`,
    // `flags: OpenFlags`, `lock_owner: Option<LockOwner>`.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino = ino.0;
        match self.provider.write(ino, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `parent: INodeNo`; `mode`/`umask` stay `u32`, `flags` stays `i32` on
    // create (not newtyped). `ReplyCreate::created` now takes `Generation`, `FileHandle`,
    // `FopenFlags` for the trailing three args.
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.provider.create(parent.0, &name.to_string_lossy()) {
            Ok(a) => {
                // Capture the create-time mode (e.g. a tool creating a 0755 file) — the
                // default is 0644, so only override when it differs.
                if (mode & 0o7777) != a.mode {
                    let _ = self.provider.set_mode(a.ino, mode);
                }
                let a = self.provider.getattr(a.ino).unwrap_or(a);
                reply.created(&TTL, &file_attr(&a), Generation(0), fuser::FileHandle(0), FopenFlags::empty())
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.provider.mkdir(parent.0, &name.to_string_lossy()) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// `mknod` — the kernel routes both *regular-file* creation (some tools, e.g. older
    /// `cp`/`tar`, create the inode via `mknod(S_IFREG)` then `open` it) **and** special
    /// files (FIFOs, sockets, block/char device nodes) here. We model only regular files +
    /// directories, so:
    ///   - `S_IFREG` (regular file) → create it (same as `create`, minus the open handle).
    ///   - FIFO / socket / block / char device → reject with **EPERM** (special files are a
    ///     deliberate non-goal — CLAUDE.md / parity-roadmap POSIX caveats). This is the
    ///     guardrail (G8): without an explicit `mknod` the default returns `ENOSYS`, which
    ///     surfaces as a confusing "Function not implemented" and lets some callers retry in
    ///     odd ways; `EPERM` is the deterministic "operation not permitted" macFUSE/Linux
    ///     callers expect for an unsupported node type. The tree is never touched for the
    ///     rejected types, so it cannot be corrupted.
    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if !mknod_is_regular_file(mode) {
            reply.error(errno(&FsError::NotSupported));
            return;
        }
        match self.provider.create(parent.0, &name.to_string_lossy()) {
            Ok(a) => {
                if (mode & 0o7777) != a.mode {
                    let _ = self.provider.set_mode(a.ino, mode & 0o7777);
                }
                let a = self.provider.getattr(a.ino).unwrap_or(a);
                reply.entry(&TTL, &file_attr(&a), Generation(0))
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// `symlink` — create a symbolic link. Git stores symlinks (mode 120000) and
    /// `git checkout` materializes them via this call; `ln -s`, `cp -a` use it too. The
    /// target is stored verbatim (its raw bytes, not interpreted) and synced, so the link
    /// resolves identically on every device.
    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let target_bytes = target.as_os_str().as_bytes();
        match self.provider.symlink(parent.0, &link_name.to_string_lossy(), target_bytes) {
            Ok(a) => reply.entry(&TTL, &file_attr(&a), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// `readlink` — return a symlink's target bytes (what `git status`/`ls -l`/path
    /// resolution read). Errors (`EPERM`) if the inode isn't a symlink.
    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.provider.readlink(ino.0) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.provider.unlink(parent.0, &name.to_string_lossy()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.provider.rmdir(parent.0, &name.to_string_lossy()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `parent`/`newparent` are `INodeNo`, `flags` is now `RenameFlags` (ignored).
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self.provider.rename(
            parent.0,
            &name.to_string_lossy(),
            newparent.0,
            &newname.to_string_lossy(),
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `ino: INodeNo`, `flags: OpenFlags` (wraps i32), reply `opened` takes
    // `FileHandle` + `FopenFlags`.
    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        let ino = ino.0;
        let flags = flags.0;
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
        reply.opened(fuser::FileHandle(0), FopenFlags::empty());
    }

    // fuser 0.17: `ino: INodeNo`, `fh: FileHandle`, `lock_owner: LockOwner` (no longer Option).
    fn flush(&self, _req: &Request, ino: INodeNo, _fh: fuser::FileHandle, _lock_owner: fuser::LockOwner, reply: ReplyEmpty) {
        match self.provider.flush(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    // fuser 0.17: `ino: INodeNo`, `fh: FileHandle`, `flags: OpenFlags`,
    // `lock_owner: Option<LockOwner>`.
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0;
        // Ensure writeback is finalized on close even if the app didn't flush.
        let _ = self.provider.flush(ino);
        // POSIX: a process's locks are released when it closes the file.
        if let Some(owner) = lock_owner {
            let _ = self.provider.release_locks(ino, owner.0);
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
    // fuser 0.17: `ino: INodeNo`, `fh: FileHandle`, `lock_owner: LockOwner`; start/end/typ/pid
    // unchanged (u64/u64/i32/u32).
    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        lock_owner: fuser::LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0;
        let lock_owner = lock_owner.0;
        if typ == libc::F_UNLCK {
            let _ = self.provider.unlock(ino, lock_owner, start, end);
            return reply.ok();
        }
        let lock = LockRange { owner: lock_owner, start, end, exclusive: typ == libc::F_WRLCK, pid };
        match self.provider.set_lock(ino, lock) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(fuser::Errno::EAGAIN),
        }
    }

    // fuser 0.17: `ino: INodeNo`, `fh: FileHandle`, `lock_owner: LockOwner`; `ReplyLock::locked`
    // args (start: u64, end: u64, typ: i32, pid: u32) are unchanged.
    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        lock_owner: fuser::LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let ino = ino.0;
        let lock_owner = lock_owner.0;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- G8: special-file guardrail (pure routing + errno mapping) ----

    #[test]
    fn mknod_accepts_regular_and_untyped() {
        // A bare permission `mode` with no type bits → regular file.
        assert!(mknod_is_regular_file(0o644));
        // Explicit S_IFREG | perms → regular file.
        assert!(mknod_is_regular_file(libc::S_IFREG as u32 | 0o644));
        assert!(mknod_is_regular_file(libc::S_IFREG as u32));
    }

    #[test]
    fn mknod_rejects_every_special_kind() {
        // FIFO, socket, char device, block device — special, all rejected via mknod.
        // (Symlinks do NOT arrive via mknod — the kernel routes them to the dedicated
        // `symlink` op, which IS supported; an S_IFLNK *through mknod* is still not a
        // regular file, so the guardrail correctly declines it here.)
        for special in [
            libc::S_IFIFO as u32,
            libc::S_IFSOCK as u32,
            libc::S_IFCHR as u32,
            libc::S_IFBLK as u32,
            libc::S_IFLNK as u32,
        ] {
            assert!(
                !mknod_is_regular_file(special | 0o644),
                "special kind {special:#o} must NOT be treated as a regular file",
            );
        }
    }

    #[test]
    fn special_file_error_maps_to_eperm() {
        // The guardrail surfaces as a deterministic EPERM, never ENOSYS/EIO.
        assert_eq!(errno(&FsError::NotSupported), fuser::Errno::EPERM);
    }
}
