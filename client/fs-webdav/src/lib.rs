//! Serve an [`fs_provider::Provider`] over **WebDAV** — the macOS mount path.
//!
//! macOS, Windows, and Linux all ship a native WebDAV client (`/sbin/mount_webdav`,
//! Explorer "Map network drive", `gio mount dav://…`), so exposing the local-first
//! tree over WebDAV gives a real, Finder-visible drive **without FUSE, a kext, or
//! FSKit** — which is exactly what's needed on macOS 26 where FUSE-T can't mount.
//!
//! This is another front-end over the same `Provider` the FUSE/WinFsp adapters use:
//! WebDAV paths resolve to inodes by walking `lookup` from the root, and reads hydrate
//! missing chunks on demand (same as the FUSE read path) via an optional hook.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::Component;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fakels::FakeLs;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dav_server::DavHandler;
use fs_provider::{Attr, FsError as ProvError, Kind, Provider, ROOT_INO};
use futures_util::{future, stream, StreamExt};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// On a chunk-cache miss, fetch `ino`'s chunks from the cloud (same role as the FUSE
/// `Hydrate` hook). Closure-based so this crate stays free of any server dependency.
pub type Hydrate = Arc<dyn Fn(u64) + Send + Sync>;

fn map_err(e: ProvError) -> FsError {
    match e {
        ProvError::NotFound => FsError::NotFound,
        ProvError::AlreadyExists => FsError::Exists,
        ProvError::NotEmpty | ProvError::NotADirectory | ProvError::IsADirectory => {
            FsError::Forbidden
        }
        _ => FsError::GeneralFailure,
    }
}

/// One `tokio::sync::Mutex` per inode that currently has (or is awaiting) a write handle.
///
/// The `Arc` around the map is NOT decoration. `DavHandler::handle` builds a fresh `DavInner`
/// per request and clones the filesystem into it (`GuardedFileSystem: DynClone`), so a bare
/// `Mutex<HashMap<..>>` field would give every REQUEST its own map and serialize nothing at all
/// — while passing any test that builds one `WebdavFs` and calls `open` twice. The two
/// HTTP-level tests at the bottom of this file exist to make that mutation go red.
type WriterLocks = Arc<Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>>;

/// How long a second writer waits for the incumbent write handle on the same path before the
/// open is refused.
///
/// **Bounded on purpose.** A WebDAV write handle is held for the whole PUT body and dav-server
/// has no per-request timeout, so an unbounded wait converts corruption into a permanent
/// per-path HANG — plus an unbounded pile of parked tasks as the client retries — which on the
/// shipped macOS mount is not obviously better than the bug. 30s is chosen because the value
/// only ever elapses when two write handles OVERLAP on ONE path for that long, which for a
/// single-user mount means a stalled client rather than a busy one: a slow PUT is never itself
/// refused, only a second concurrent writer behind it, and an ordinary large Finder copy
/// releases the handle long before a queued writer expires. It is also short enough to land
/// inside the OS WebDAV client's own request timeouts, so the refusal surfaces as our answer
/// rather than as the client giving up first.
const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Exclusive right to hold a write handle on one inode, released when the handle is dropped.
///
/// Two overlapping write handles on ONE path corrupt each other in both directions, and neither
/// direction reports anything — both clients are answered `204 No Content`:
///
///   * a truncating `open` calls `begin_overwrite`, which is an UNCONDITIONAL
///     `staged.put(ino, Vec::new())`, so it wipes the incumbent writer's staged bytes; the
///     incumbent then writes at its own `pos`, misses staging, and `Provider::write` re-seeds
///     from COMMITTED content and zero-fills the gap; and
///   * the abort-`Drop` below discards staging BY INODE, so one client hanging up mid-PUT
///     destroys a DIFFERENT handle's in-flight body.
///
/// Nothing else serializes them — `FakeLs::check` is `future::ready(Ok(()))`, so WebDAV LOCK
/// tokens are accepted and never enforced.
struct WriteGuard<P: Provider + 'static> {
    ino: u64,
    provider: Arc<P>,
    locks: WriterLocks,
    _held: tokio::sync::OwnedMutexGuard<()>,
}

impl<P: Provider + 'static> Drop for WriteGuard<P> {
    fn drop(&mut self) {
        // A write handle that never successfully flushed (a client disconnect mid-body errors
        // out of dav-server's PUT loop WITHOUT calling `flush`) must not leave partial staged
        // bytes shadowing the committed content. Discard them; the last committed version — the
        // PRIOR content for an overwrite (`begin_overwrite` staged empty without committing) —
        // stays durable.
        //
        // THIS LIVES IN THE GUARD SO THE ORDER CANNOT BE WRITTEN WRONG. `discard` is BY INODE,
        // so it must run while this handle still EXCLUDES other writers — release the lock first
        // and it wipes the staging of the writer that just acquired it, which is the very
        // cross-handle corruption this guard exists to stop. A `Drop` body runs before the
        // struct's own fields are dropped, so `_held` is necessarily still held here and is
        // released immediately after. Putting the discard in the CALLER instead would leave that
        // ordering as a statement-order convention, and the window it opens is a few
        // instructions wide — real, but far too narrow for any non-flaky test to pin.
        //
        // Unconditional is correct AND is the point: a clean `flush` has already TAKEN the
        // staged buffer, so this is a no-op there. A "did we commit?" condition here would be a
        // second, driftable copy of a fact `Provider::flush` already records by emptying staging.
        let _ = self.provider.discard(self.ino);

        // Drop the map entry once nobody holds or awaits it, so the map does not grow one entry
        // per inode ever written for the life of the mount. Counting under the map lock answers
        // the only question asked here exactly: every acquirer clones the `Arc` out of the map
        // while holding this same lock, so a count of 2 — the map's own reference plus this
        // guard's `_held` (a field, so still alive during this body) — means no waiter exists
        // and none can appear before the removal lands. Any larger count leaves the entry in
        // place, which is wasteful and never wrong.
        let mut map = match self.locks.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        if map.get(&self.ino).is_some_and(|e| Arc::strong_count(e) <= 2) {
            map.remove(&self.ino);
        }
    }
}

/// A [`Provider`] exposed as a WebDAV filesystem.
pub struct WebdavFs<P: Provider + 'static> {
    provider: Arc<P>,
    hydrate: Option<Hydrate>,
    writers: WriterLocks,
    write_timeout: Duration,
}

impl<P: Provider + 'static> Clone for WebdavFs<P> {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            hydrate: self.hydrate.clone(),
            // Shared, NOT rebuilt: this clone happens per REQUEST (see `WriterLocks`).
            writers: self.writers.clone(),
            write_timeout: self.write_timeout,
        }
    }
}

impl<P: Provider + 'static> WebdavFs<P> {
    pub fn new(provider: Arc<P>, hydrate: Option<Hydrate>) -> Self {
        Self {
            provider,
            hydrate,
            writers: Arc::new(Mutex::new(HashMap::new())),
            write_timeout: WRITE_LOCK_TIMEOUT,
        }
    }

    /// Override [`WRITE_LOCK_TIMEOUT`] — keeps the refusal path sub-second in tests.
    #[cfg(test)]
    fn with_write_timeout(mut self, d: Duration) -> Self {
        self.write_timeout = d;
        self
    }

    /// Take the sole write handle on `ino`, waiting at most [`WRITE_LOCK_TIMEOUT`].
    ///
    /// On expiry the open is REFUSED with a real status rather than waited on forever. 423
    /// Locked is the semantically right answer and is not available: dav-server 0.11's
    /// `FsError` has no `Locked` variant (`NotImplemented`/`GeneralFailure`/`Exists`/`NotFound`/
    /// `Forbidden`/`InsufficientStorage`/`LoopDetected`/`PathTooLong`/`TooLarge`/`IsRemote`), and
    /// `open` can only return one of those. `Forbidden` (403) is the closest honest one: terminal,
    /// and distinguishable in a log from the `GeneralFailure` (500) dav-server already emits for
    /// a genuine provider fault.
    async fn acquire_write(&self, ino: u64) -> FsResult<WriteGuard<P>> {
        let lock = {
            let mut map = match self.writers.lock() {
                Ok(m) => m,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.entry(ino).or_default().clone()
        };
        match tokio::time::timeout(self.write_timeout, lock.lock_owned()).await {
            Ok(held) => Ok(WriteGuard {
                ino,
                provider: self.provider.clone(),
                locks: self.writers.clone(),
                _held: held,
            }),
            Err(_elapsed) => Err(FsError::Forbidden),
        }
    }

    /// Resolve a WebDAV path to `(ino, attr)` by walking `lookup` from the root.
    fn resolve(&self, path: &DavPath) -> FsResult<(u64, Attr)> {
        let pb = path.as_pathbuf();
        let mut ino = ROOT_INO;
        let mut attr = self.provider.getattr(ROOT_INO).map_err(map_err)?;
        for comp in pb.components() {
            if let Component::Normal(os) = comp {
                attr = self
                    .provider
                    .lookup(ino, &os.to_string_lossy())
                    .map_err(map_err)?;
                ino = attr.ino;
            }
        }
        Ok((ino, attr))
    }

    /// `(parent_ino, leaf_name)` — for create/remove/rename.
    fn split(&self, path: &DavPath) -> FsResult<(u64, String)> {
        let name = path.file_name().ok_or(FsError::Forbidden)?.to_string();
        let (parent_ino, _) = self.resolve(&path.parent())?;
        Ok((parent_ino, name))
    }

    fn make_file(&self, ino: u64, write: Option<WriteGuard<P>>) -> Box<dyn DavFile> {
        Box::new(WebdavFile {
            provider: self.provider.clone(),
            hydrate: self.hydrate.clone(),
            ino,
            pos: 0,
            write,
        })
    }
}

impl<P: Provider + 'static> DavFileSystem for WebdavFs<P> {
    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let (_, attr) = self.resolve(path)?;
            Ok(Box::new(WebdavMeta::from(attr)) as Box<dyn DavMetaData>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let (ino, attr) = self.resolve(path)?;
            if attr.kind != Kind::Dir {
                return Err(FsError::Forbidden);
            }
            let mut out: Vec<Box<dyn DavDirEntry>> = Vec::new();
            for c in self.provider.readdir(ino).map_err(map_err)? {
                let a = self.provider.getattr(c.ino).map_err(map_err)?;
                out.push(Box::new(WebdavEntry {
                    name: c.name,
                    meta: WebdavMeta::from(a),
                }));
            }
            Ok(Box::pin(stream::iter(out).map(Ok)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            if options.write {
                let (parent, name) = self.split(path)?;
                let ino = match self.provider.lookup(parent, &name) {
                    Ok(a) if a.kind == Kind::Dir => return Err(FsError::Forbidden),
                    Ok(a) => a.ino,
                    Err(ProvError::NotFound) if options.create || options.create_new => {
                        self.provider.create(parent, &name).map_err(map_err)?.ino
                    }
                    Err(e) => return Err(map_err(e)),
                };
                // A truncating open (the usual PUT-overwrite) stages an EMPTY buffer WITHOUT
                // committing, so replacing a file with a shorter one leaves no stale tail once
                // flushed — while the LAST COMMITTED version stays the durable fallback until the
                // full body arrives. (Using `truncate(ino, 0)` here would commit an empty version
                // immediately, so an aborted/short PUT would recover to 0 bytes and an empty
                // intermediate could sync mid-PUT.)
                //
                // ONE WRITE HANDLE PER INODE AT A TIME, AND IT IS TAKEN **BEFORE**
                // `begin_overwrite`. Taken after, the truncating open would still wipe the
                // incumbent writer's staged bytes on its way to the queue — the corruption
                // happens in the very statement the wait exists to order.
                let write = self.acquire_write(ino).await?;
                if options.truncate {
                    self.provider.begin_overwrite(ino).map_err(map_err)?;
                }
                Ok(self.make_file(ino, Some(write)))
            } else {
                let (ino, attr) = self.resolve(path)?;
                if attr.kind == Kind::Dir {
                    return Err(FsError::Forbidden);
                }
                // A read-only handle: never a write handle, so its `flush`/`Drop` must never
                // commit or discard (only write handles touch the committed content). It also
                // takes NO write guard — a GET must not queue behind a PUT.
                Ok(self.make_file(ino, None))
            }
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (parent, name) = self.split(path)?;
            self.provider.mkdir(parent, &name).map_err(map_err)?;
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (parent, name) = self.split(path)?;
            self.provider.unlink(parent, &name).map_err(map_err)
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (parent, name) = self.split(path)?;
            self.provider.rmdir(parent, &name).map_err(map_err)
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (fp, fname) = self.split(from)?;
            let (tp, tname) = self.split(to)?;
            self.provider
                .rename(fp, &fname, tp, &tname)
                .map_err(map_err)
        })
    }
}

/// One open WebDAV file handle.
struct WebdavFile<P: Provider + 'static> {
    provider: Arc<P>,
    hydrate: Option<Hydrate>,
    ino: u64,
    pos: u64,
    /// `Some` iff this handle currently owns the write on `ino` — which is at once the
    /// "opened for write" flag, the "not yet committed" flag, and the exclusion itself. Only a
    /// write handle may commit (`flush`) or, on an aborted write, `discard`; a read-only handle
    /// must never touch the committed content, and never queues behind a writer.
    ///
    /// A successful `flush` sets it back to `None`, which both discards nothing (the staging is
    /// already committed) and frees the path for the next writer as soon as the bytes are
    /// durable. Dropping the handle without flushing drops this instead, and the guard's own
    /// `Drop` does the abort cleanup — so this struct needs no `Drop` impl at all.
    /// (These were two independent `bool`s — `write_handle` and `flushed`. Neither excluded
    /// anything, so two handles could hold "the" write at once.)
    write: Option<WriteGuard<P>>,
}

impl<P: Provider + 'static> fmt::Debug for WebdavFile<P> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "WebdavFile {{ ino: {}, pos: {} }}", self.ino, self.pos)
    }
}

impl<P: Provider + 'static> DavFile for WebdavFile<P> {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let a = self.provider.getattr(self.ino).map_err(map_err)?;
            Ok(Box::new(WebdavMeta::from(a)) as Box<dyn DavMetaData>)
        })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let data = match self.provider.read(self.ino, self.pos, count as u32) {
                Ok(d) => d,
                // Chunk-cache miss on a known file: hydrate from the cloud, then retry.
                // The hydrate hook does **blocking** HTTP (reqwest::blocking spins up — and
                // drops — its own tokio runtime). Calling it directly here, inside this
                // async WebDAV worker, panics: "Cannot drop a runtime in a context where
                // blocking is not allowed." Run it on a blocking thread instead. Regression:
                // `hydrate_does_not_panic_when_it_drops_a_runtime`.
                Err(ProvError::ChunkMissing(_)) if self.hydrate.is_some() => {
                    let hydrate = self.hydrate.as_ref().unwrap().clone();
                    let ino = self.ino;
                    let _ = tokio::task::spawn_blocking(move || hydrate(ino)).await;
                    self.provider
                        .read(self.ino, self.pos, count as u32)
                        .map_err(map_err)?
                }
                Err(e) => return Err(map_err(e)),
            };
            self.pos += data.len() as u64;
            Ok(Bytes::from(data))
        })
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let n = self
                .provider
                .write(self.ino, self.pos, &buf)
                .map_err(map_err)?;
            self.pos += n as u64;
            Ok(())
        })
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            while buf.has_remaining() {
                let chunk = buf.chunk().to_vec();
                let n = self
                    .provider
                    .write(self.ino, self.pos, &chunk)
                    .map_err(map_err)?;
                self.pos += n as u64;
                buf.advance(chunk.len());
            }
            Ok(())
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            // Only a write handle commits. A read-only handle's close must NOT commit (that
            // would re-chunk + re-enqueue an unchanged file, and could commit stale staging).
            if self.write.is_none() {
                return Ok(());
            }
            self.provider.flush(self.ino).map_err(map_err)?;
            // The bytes are durable: `Drop` has nothing left to discard, and the write guard is
            // released here rather than at close, freeing the path for the next writer.
            self.write = None;
            Ok(())
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let size = self.provider.getattr(self.ino).map(|a| a.size).unwrap_or(0);
            let newpos = match pos {
                SeekFrom::Start(n) => n,
                SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
                SeekFrom::End(d) => (size as i64 + d).max(0) as u64,
            };
            self.pos = newpos;
            Ok(newpos)
        })
    }
}

#[derive(Debug, Clone)]
struct WebdavMeta {
    size: u64,
    is_dir: bool,
    mtime: i64,
}

impl From<Attr> for WebdavMeta {
    fn from(a: Attr) -> Self {
        Self {
            size: a.size,
            is_dir: a.kind == Kind::Dir,
            mtime: a.mtime,
        }
    }
}

impl DavMetaData for WebdavMeta {
    fn len(&self) -> u64 {
        self.size
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn modified(&self) -> FsResult<SystemTime> {
        // `Attr.mtime` is the real modification time in unix **nanoseconds** (preserved +
        // synced). Using `from_secs` here treated ~1.78e18 nanos as seconds, which
        // `dav-server` then overflowed building an HTTP date (panic → dropped connection).
        Ok(if self.mtime > 0 {
            UNIX_EPOCH + Duration::from_nanos(self.mtime as u64)
        } else {
            UNIX_EPOCH
        })
    }
}

struct WebdavEntry {
    name: String,
    meta: WebdavMeta,
}

impl DavDirEntry for WebdavEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let m = self.meta.clone();
        Box::pin(future::ok(Box::new(m) as Box<dyn DavMetaData>))
    }
}

/// Build a [`DavHandler`] serving `provider` (with `FakeLs` — enough LOCK/UNLOCK for
/// the macOS/Windows WebDAV clients).
pub fn handler<P: Provider + 'static>(provider: Arc<P>, hydrate: Option<Hydrate>) -> DavHandler {
    handler_for(WebdavFs::new(provider, hydrate))
}

fn handler_for<P: Provider + 'static>(fs: WebdavFs<P>) -> DavHandler {
    DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new())
        .build_handler()
}

/// Run a WebDAV server on `addr` until the process ends (blocking; owns its own tokio
/// runtime). Mount it with the OS's native client, e.g. on macOS:
/// `mount_webdav -S -v Maker http://127.0.0.1:9988/ ~/Maker`.
pub fn serve_blocking<P: Provider + 'static>(
    provider: Arc<P>,
    addr: SocketAddr,
    hydrate: Option<Hydrate>,
) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(provider, addr, hydrate))
}

/// Async server loop (for callers that already have a runtime).
pub async fn serve<P: Provider + 'static>(
    provider: Arc<P>,
    addr: SocketAddr,
    hydrate: Option<Hydrate>,
) -> std::io::Result<()> {
    let dav = handler(provider, hydrate);
    let listener = TcpListener::bind(addr).await?;
    accept_loop(dav, listener).await
}

/// The accept loop, split out of [`serve`] so a test can bind its own ephemeral listener
/// (and learn the port) while still driving **this** code rather than a copy of it — a
/// second copy would drift, and the concurrency guard's whole proof depends on requests
/// going through `DavHandler::handle`, which clones the filesystem PER REQUEST.
async fn accept_loop(dav: DavHandler, listener: TcpListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let dav = dav.clone();
        tokio::task::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| {
                        let dav = dav.clone();
                        async move { Ok::<_, Infallible>(dav.handle(req).await) }
                    }),
                )
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_provider::{ChunkConfig, LocalChunkStore, SqliteProvider};

    /// Regression for the macOS "Cannot drop a runtime in a context where blocking is not
    /// allowed" panic: a WebDAV read on a not-yet-hydrated file invokes the hydrate hook,
    /// which — like `reqwest::blocking` — creates and drops its own tokio runtime. If that
    /// runs inside the async read worker it panics. `read_bytes` must run it on a blocking
    /// thread (`spawn_blocking`). Here the hook drops a (multi-thread) runtime to reproduce
    /// the exact trigger, then provisions the missing chunks so the retry succeeds; with the
    /// bug the read panics/fails, with the fix it returns the bytes.
    #[test]
    fn hydrate_does_not_panic_when_it_drops_a_runtime() {
        let base = std::env::temp_dir().join(format!("fswebdav-hydrate-{}", std::process::id()));
        let store_a = base.join("a");
        let store_b = base.join("b");
        std::fs::create_dir_all(&store_a).unwrap();
        std::fs::create_dir_all(&store_b).unwrap();
        let cfg = ChunkConfig {
            min: 64,
            avg: 256,
            max: 1024,
        };

        // A writes a file (its chunks live in store A).
        let a = Arc::new(
            SqliteProvider::in_memory(Box::new(LocalChunkStore::new(&store_a).unwrap()), cfg)
                .unwrap(),
        );
        let body = vec![7u8; 2000];
        let fa = a.create(ROOT_INO, "f.bin").unwrap();
        a.write(fa.ino, 0, &body).unwrap();
        a.flush(fa.ino).unwrap();
        let nodes = a.export_tree().unwrap();
        let node_id = nodes
            .iter()
            .find(|n| n.name == "f.bin")
            .unwrap()
            .node_id
            .clone();

        // B has the tree (manifest) but an EMPTY store → a read misses → hydrate fires.
        let b = Arc::new(
            SqliteProvider::in_memory(Box::new(LocalChunkStore::new(&store_b).unwrap()), cfg)
                .unwrap(),
        );
        for n in &nodes {
            b.apply_remote(n).unwrap();
        }
        let ino = b.ino_for_node_id(&node_id).unwrap().unwrap();

        // The hook: mimic reqwest::blocking (build + drop a runtime — the panic trigger),
        // then copy the missing chunks from A into B's cache so the retry reads cleanly.
        let hashes = b.node_manifest_hashes(&node_id).unwrap();
        let (ac, bc) = (a.clone(), b.clone());
        let hydrate: Hydrate = Arc::new(move |_ino| {
            let inner = tokio::runtime::Runtime::new().unwrap();
            drop(inner); // panics here if we're on an async worker (the bug)
            for h in &hashes {
                let bytes = ac.chunk_get(h).expect("source chunk");
                bc.chunk_put(h, &bytes).expect("cache chunk");
            }
        });

        // Drive the real async read path on a multi-thread runtime, exactly like `serve`.
        let n = body.len();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let got = rt
            .block_on(async move {
                let mut file = WebdavFile {
                    provider: b.clone(),
                    hydrate: Some(hydrate),
                    ino,
                    pos: 0,
                    write: None,
                };
                file.read_bytes(n).await
            })
            .expect("read should succeed after hydrate (no runtime-drop panic)");

        assert_eq!(&got[..], &body[..], "hydrated content matches the source");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn mem_provider() -> Arc<SqliteProvider> {
        let cfg = ChunkConfig {
            min: 64,
            avg: 256,
            max: 1024,
        };
        Arc::new(SqliteProvider::in_memory(Box::new(LocalChunkStore::temp().unwrap()), cfg).unwrap())
    }

    /// A held [`WriteGuard`] for the tests below that construct a `WebdavFile` directly
    /// instead of going through `open`. Its map is a throwaway — these tests drive ONE handle,
    /// so what is being stood in for is the flag, not the exclusion.
    fn test_guard(p: Arc<SqliteProvider>, ino: u64) -> WriteGuard<SqliteProvider> {
        let locks: WriterLocks = Arc::new(Mutex::new(HashMap::new()));
        let lock = locks.lock().unwrap().entry(ino).or_default().clone();
        let held = lock.try_lock_owned().expect("uncontended in a single-handle test");
        WriteGuard {
            ino,
            provider: p,
            locks,
            _held: held,
        }
    }

    fn read_committed(p: &SqliteProvider, ino: u64) -> Vec<u8> {
        let size = p.getattr(ino).unwrap().size;
        if size == 0 {
            return Vec::new();
        }
        p.read(ino, 0, size as u32).unwrap()
    }

    /// BUG #5: an aborted/short PUT over an EXISTING file must leave the PRIOR committed content
    /// intact (not 0 bytes), and the truncating open must NOT commit an empty intermediate
    /// version. Mirrors dav-server's overwrite sequence — `begin_overwrite` (the truncating open)
    /// → partial `write_bytes` → the handle is dropped WITHOUT `flush` (a client disconnect).
    #[test]
    fn aborted_overwrite_keeps_prior_content() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"ORIGINAL-LONG-CONTENT").unwrap();
        p.flush(ino).unwrap();
        let committed_manifest = p.sync_node(&p.node_id_for(ino).unwrap().unwrap()).unwrap().unwrap().manifest_id;

        let rt = tokio::runtime::Runtime::new().unwrap();
        {
            // The truncating write-open: stage empty WITHOUT committing.
            p.begin_overwrite(ino).unwrap();
            // At open time the committed version is untouched — no empty version was flushed.
            assert_eq!(
                p.sync_node(&p.node_id_for(ino).unwrap().unwrap()).unwrap().unwrap().manifest_id,
                committed_manifest,
                "the truncating open must NOT commit an empty intermediate version"
            );

            let mut wf = WebdavFile {
                provider: p.clone(),
                hydrate: None,
                ino,
                pos: 0,
                write: Some(test_guard(p.clone(), ino)),
            };
            rt.block_on(wf.write_bytes(Bytes::from_static(b"SHORT"))).unwrap();
            // Client disconnects: the handle is dropped WITHOUT a successful flush.
            drop(wf);
        }

        // The PRIOR committed content survives the aborted PUT (not 0 bytes, not "SHORT").
        assert_eq!(
            read_committed(&p, ino),
            b"ORIGINAL-LONG-CONTENT".to_vec(),
            "an aborted overwrite must recover the prior committed content"
        );
    }

    /// A clean overwrite (write + flush) still replaces the content and leaves no stale tail —
    /// the `begin_overwrite`-then-flush path must preserve the existing shorter-overwrite guarantee.
    #[test]
    fn clean_overwrite_replaces_content_no_stale_tail() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"ORIGINAL-LONG-CONTENT").unwrap();
        p.flush(ino).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        p.begin_overwrite(ino).unwrap();
        let mut wf = WebdavFile {
            provider: p.clone(),
            hydrate: None,
            ino,
            pos: 0,
            write: Some(test_guard(p.clone(), ino)),
        };
        rt.block_on(async {
            wf.write_bytes(Bytes::from_static(b"NEW")).await.unwrap();
            wf.flush().await.unwrap();
        });
        drop(wf);
        assert_eq!(
            read_committed(&p, ino),
            b"NEW".to_vec(),
            "a clean overwrite replaces the content with no stale tail"
        );
    }

    /// BUG #9: a read-only handle's close must NOT commit. Its `flush` must be a no-op (a commit
    /// would re-chunk + re-enqueue an unchanged file). We detect a spurious commit via the outbox.
    #[test]
    fn read_only_close_does_not_commit() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "g.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"content").unwrap();
        p.flush(ino).unwrap();
        let nid = p.node_id_for(ino).unwrap().unwrap();
        // Drain the create/flush enqueue so a spurious re-commit is observable.
        p.outbox_clear(&nid).unwrap();
        assert!(p.outbox_list().unwrap().is_empty());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut wf = WebdavFile {
            provider: p.clone(),
            hydrate: None,
            ino,
            pos: 0,
            write: None,
        };
        rt.block_on(async {
            let _ = wf.read_bytes(7).await.unwrap();
            wf.flush().await.unwrap(); // read-only close → must be a no-op
        });
        drop(wf);
        assert!(
            p.outbox_list().unwrap().is_empty(),
            "a read-only close must NOT commit (no re-enqueue)"
        );
    }

    // ---- HTTP-level concurrency harness ------------------------------------------------
    //
    // The two tests below drive REAL HTTP over TWO TCP CONNECTIONS, and that is load-bearing
    // rather than merely thorough. `DavHandler::handle` builds a `DavInner` per request and
    // `GuardedFileSystem: DynClone` CLONES the `WebdavFs` for each one — so a test that built
    // ONE `WebdavFs` and called `open` twice would pass over a guard whose state is per-request,
    // i.e. over a guard that serializes nothing in production. Mutation check (b) is exactly
    // that: store the writer-lock map as a bare `Mutex<HashMap<..>>` instead of an
    // `Arc<Mutex<HashMap<..>>>` and these must STILL go RED.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Bind an ephemeral port and serve `p` on it through the crate's own [`accept_loop`] —
    /// the shipped code, not a copy of it.
    async fn spawn_server(p: Arc<SqliteProvider>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = accept_loop(handler(p, None), listener).await;
        });
        addr
    }

    fn put_headers(len: usize) -> Vec<u8> {
        format!("PUT /f.txt HTTP/1.1\r\nHost: t\r\nContent-Length: {len}\r\n\r\n").into_bytes()
    }

    /// Read one HTTP response head and return its status line.
    ///
    /// Stops at the header terminator rather than at EOF, and is bounded by a timeout: a test
    /// here is ALSO a claim about a client that never gets an answer, so a missing response has
    /// to come back as a value to assert on ("<no response ...>") instead of hanging the suite.
    /// (`read_to_end` was the first spelling and it hung both concurrency tests indefinitely —
    /// the server does not always drop the socket the instant it answers.)
    async fn read_head(s: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match s.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .await;
        String::from_utf8_lossy(&buf).to_string()
    }

    /// The status line of one response, or a marker the assertions can report.
    async fn status_line(s: &mut TcpStream) -> String {
        let head = read_head(s).await;
        head.lines()
            .next()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or("<no response: connection dropped or timed out>")
            .trim()
            .to_string()
    }

    /// Long enough for the server to consume what was just sent and park where the test needs
    /// it (in `open`, or mid-body). Generous because a false pass here is a missed regression,
    /// and the tests are still sub-second.
    const SETTLE: Duration = Duration::from_millis(400);

    /// TWO CONCURRENT PUTs ON ONE PATH MUST COMMIT EXACTLY ONE WRITER'S BYTES.
    ///
    /// Probed before the fix over real HTTP on two TCP connections — BOTH clients got
    /// `204 No Content` and NEITHER one's bytes were committed:
    ///
    ///   A: PUT, Content-Length 40 — headers + the first 20 'A', then stalls
    ///   B: a complete 12-byte PUT on its OWN connection -> 204
    ///   A: sends the remaining 20 'A' -> 204
    ///   committed: "BBBBBBBBBBBB\0\0\0\0\0\0\0\0AAAAAAAAAAAAAAAAAAAA"  (40 bytes)
    ///
    /// Mechanism: B's truncating `open` calls `begin_overwrite`, which is an UNCONDITIONAL
    /// `staged.put(ino, Vec::new())`, so it wiped A's staged bytes; A then wrote at its own
    /// `pos` (20), missed staging, and `Provider::write` re-seeded from COMMITTED content and
    /// zero-filled the gap.
    #[test]
    fn two_concurrent_puts_commit_exactly_one_writers_bytes() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"PRIOR-GOOD").unwrap();
        p.flush(ino).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let (sa, sb) = rt.block_on(async {
            let addr = spawn_server(p.clone()).await;

            // A: headers + the first half of a 40-byte body, then stall holding the handle.
            let mut a = TcpStream::connect(addr).await.unwrap();
            a.write_all(&put_headers(40)).await.unwrap();
            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            tokio::time::sleep(SETTLE).await;

            // B: a COMPLETE PUT on a SECOND TCP connection, while A's handle is still open.
            let hb = tokio::spawn(async move {
                let mut b = TcpStream::connect(addr).await.unwrap();
                b.write_all(&put_headers(12)).await.unwrap();
                b.write_all(&[b'B'; 12]).await.unwrap();
                b.flush().await.unwrap();
                status_line(&mut b).await
            });
            tokio::time::sleep(SETTLE).await;

            // A finishes its body.
            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            let sa = status_line(&mut a).await;
            let sb = hb.await.unwrap();
            (sa, sb)
        });

        let got = read_committed(&p, ino);
        assert!(
            got == vec![b'A'; 40] || got == vec![b'B'; 12],
            "two overlapping PUTs must commit exactly ONE writer's bytes — never a splice of \
             both, never a zero-filled gap. A={sa:?} B={sb:?} committed={:?} (len {})",
            String::from_utf8_lossy(&got),
            got.len()
        );
        // Nobody may be answered over bytes that were never committed as a whole: each response
        // is a REAL status line, not a dropped connection.
        for (who, st) in [("A", &sa), ("B", &sb)] {
            assert!(
                st.starts_with("HTTP/1.1 "),
                "{who} must get a real HTTP status, got {st:?}"
            );
        }
    }

    /// AN ABORTED PUT MUST NOT DESTROY A CONCURRENT WRITER'S IN-FLIGHT BODY.
    ///
    /// Probed before the fix over real HTTP on two TCP connections:
    ///
    ///   B: PUT, Content-Length 30 — headers + the first 10 'B', then stalls
    ///   A: PUT on another connection, client hangs up mid-body
    ///   B: sends the rest -> 204
    ///   committed: "PRIOR-GOODBBBBBBBBBBBBBBBBBBBB"  (30 bytes)
    ///
    /// Mechanism: the `Drop` guard that exists for the aborted-overwrite case
    /// (`aborted_overwrite_keeps_prior_content`) discards staging BY INODE, so A's abort
    /// destroyed B's buffer; B then wrote at its own `pos` (10), missed staging, and re-seeded
    /// from COMMITTED content — splicing the PRIOR file's bytes into the new one. This needs no
    /// second writer to SUCCEED, which makes it the more mundane trigger of the two.
    #[test]
    fn an_aborted_put_does_not_discard_a_concurrent_writers_staging() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"PRIOR-GOOD").unwrap();
        p.flush(ino).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let sb = rt.block_on(async {
            let addr = spawn_server(p.clone()).await;

            // B: the incumbent writer — headers + the first third of a 30-byte body, then stall.
            let mut b = TcpStream::connect(addr).await.unwrap();
            b.write_all(&put_headers(30)).await.unwrap();
            b.write_all(&[b'B'; 10]).await.unwrap();
            b.flush().await.unwrap();
            tokio::time::sleep(SETTLE).await;

            // A: opens a PUT on a SECOND connection and hangs up mid-body.
            {
                let mut a = TcpStream::connect(addr).await.unwrap();
                a.write_all(&put_headers(40)).await.unwrap();
                a.write_all(&[b'A'; 5]).await.unwrap();
                a.flush().await.unwrap();
                tokio::time::sleep(SETTLE).await;
                // Dropped without the rest of the body: dav-server errors out of its PUT loop.
            }
            tokio::time::sleep(SETTLE).await;

            // B finishes.
            b.write_all(&[b'B'; 20]).await.unwrap();
            b.flush().await.unwrap();
            status_line(&mut b).await
        });

        let got = read_committed(&p, ino);
        assert_eq!(
            got,
            vec![b'B'; 30],
            "a concurrent client's aborted PUT must not splice the PRIOR committed content into \
             the live writer's file. B={sb:?} committed={:?}",
            String::from_utf8_lossy(&got)
        );
    }

    /// Same harness, but with the writer wait shortened so the REFUSAL path is testable in
    /// under a second. Serving `WebdavFs` directly (rather than via `handler`) is the only
    /// reason `with_write_timeout` exists — it is `#[cfg(test)]` and adds no public knob.
    async fn spawn_server_with_timeout(p: Arc<SqliteProvider>, d: Duration) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fs = WebdavFs::new(p, None).with_write_timeout(d);
        tokio::spawn(async move {
            let _ = accept_loop(handler_for(fs), listener).await;
        });
        addr
    }

    /// A WRITER THAT CANNOT GET THE HANDLE IS TOLD SO, WITH A REAL STATUS — never `204 No
    /// Content` over bytes that were dropped, and never an unbounded hang.
    ///
    /// The wait is deliberately bounded ([`WRITE_LOCK_TIMEOUT`]): a WebDAV write handle is held
    /// for the whole PUT body and dav-server has no per-request timeout, so waiting forever
    /// would turn this corruption into a permanent per-path wedge. 423 Locked is the right code
    /// and dav-server 0.11's `FsError` cannot express it (no `Locked` variant), so the refusal
    /// is 403 — asserted as "an error status", not as the literal 403, so the reason this test
    /// exists survives a future dav-server that CAN say 423.
    #[test]
    fn a_writer_that_cannot_take_the_handle_gets_a_real_error_not_a_204() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"PRIOR-GOOD").unwrap();
        p.flush(ino).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let sb = rt.block_on(async {
            let addr = spawn_server_with_timeout(p.clone(), Duration::from_millis(250)).await;

            // A takes the handle and stalls mid-body for longer than the wait.
            let mut a = TcpStream::connect(addr).await.unwrap();
            a.write_all(&put_headers(40)).await.unwrap();
            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            tokio::time::sleep(SETTLE).await;

            // B waits, expires, and must be TOLD.
            let mut b = TcpStream::connect(addr).await.unwrap();
            b.write_all(&put_headers(12)).await.unwrap();
            b.write_all(&[b'B'; 12]).await.unwrap();
            b.flush().await.unwrap();
            let sb = status_line(&mut b).await;

            // Let A finish so the file ends in a defined state.
            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            let _ = status_line(&mut a).await;
            sb
        });

        assert!(
            sb.starts_with("HTTP/1.1 4") || sb.starts_with("HTTP/1.1 5"),
            "a refused writer must get an ERROR status — a 2xx here is the original bug's whole \
             signature, a client told its bytes landed when they did not. got {sb:?}"
        );
        // ...and the refusal cost the incumbent nothing.
        let got = read_committed(&p, ino);
        assert_eq!(
            got,
            vec![b'A'; 40],
            "refusing the second writer must leave the incumbent's PUT intact. committed={:?}",
            String::from_utf8_lossy(&got)
        );
    }

    /// READS ARE NOT GUARDED. Only `options.write` takes the per-inode lock, so a GET must not
    /// queue behind an in-flight PUT — guard every open instead and one slow upload stalls every
    /// reader of that path (Finder previews, Spotlight, the sync daemon) for the whole body.
    #[test]
    fn a_read_does_not_queue_behind_an_in_flight_write() {
        let p = mem_provider();
        let f = p.create(ROOT_INO, "f.txt").unwrap();
        let ino = f.ino;
        p.write(ino, 0, b"PRIOR-GOOD").unwrap();
        p.flush(ino).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let (elapsed, body) = rt.block_on(async {
            // A long wait: if a GET took the write lock it would block on it, not time out.
            let addr = spawn_server_with_timeout(p.clone(), Duration::from_secs(30)).await;

            let mut a = TcpStream::connect(addr).await.unwrap();
            a.write_all(&put_headers(40)).await.unwrap();
            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            tokio::time::sleep(SETTLE).await;

            // A GET while that PUT holds the write handle.
            let t0 = std::time::Instant::now();
            let mut g = TcpStream::connect(addr).await.unwrap();
            g.write_all(b"GET /f.txt HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            g.flush().await.unwrap();
            let mut buf = Vec::new();
            let _ = g.read_to_end(&mut buf).await;
            let elapsed = t0.elapsed();

            a.write_all(&[b'A'; 20]).await.unwrap();
            a.flush().await.unwrap();
            let _ = status_line(&mut a).await;
            (elapsed, String::from_utf8_lossy(&buf).to_string())
        });

        assert!(
            elapsed < Duration::from_secs(5),
            "a GET must not wait on a writer's lock; it took {elapsed:?}"
        );
        assert!(
            body.starts_with("HTTP/1.1 200 "),
            "the read must be SERVED while a PUT holds the write handle, not queued behind it \
             (a 30s writer wait is in force, so a guarded read could only fail slowly). got {body:?}"
        );
        // Deliberately NOT asserted here: WHICH bytes come back. `Provider::read` answers from
        // staging when a write is open, so this GET sees the in-flight PUT's partial body rather
        // than the last committed version. That is pre-existing fs-provider behaviour, unchanged
        // and unblessed by this commit — pinning it here would quietly adopt it as the contract.
    }
}
