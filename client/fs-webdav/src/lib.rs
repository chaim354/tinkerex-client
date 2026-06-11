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

use std::convert::Infallible;
use std::fmt;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::Component;
use std::sync::Arc;
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

/// A [`Provider`] exposed as a WebDAV filesystem.
pub struct WebdavFs<P: Provider + 'static> {
    provider: Arc<P>,
    hydrate: Option<Hydrate>,
}

impl<P: Provider + 'static> Clone for WebdavFs<P> {
    fn clone(&self) -> Self {
        Self { provider: self.provider.clone(), hydrate: self.hydrate.clone() }
    }
}

impl<P: Provider + 'static> WebdavFs<P> {
    pub fn new(provider: Arc<P>, hydrate: Option<Hydrate>) -> Self {
        Self { provider, hydrate }
    }

    /// Resolve a WebDAV path to `(ino, attr)` by walking `lookup` from the root.
    fn resolve(&self, path: &DavPath) -> FsResult<(u64, Attr)> {
        let pb = path.as_pathbuf();
        let mut ino = ROOT_INO;
        let mut attr = self.provider.getattr(ROOT_INO).map_err(map_err)?;
        for comp in pb.components() {
            if let Component::Normal(os) = comp {
                attr = self.provider.lookup(ino, &os.to_string_lossy()).map_err(map_err)?;
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

    fn make_file(&self, ino: u64) -> Box<dyn DavFile> {
        Box::new(WebdavFile {
            provider: self.provider.clone(),
            hydrate: self.hydrate.clone(),
            ino,
            pos: 0,
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
                out.push(Box::new(WebdavEntry { name: c.name, meta: WebdavMeta::from(a) }));
            }
            Ok(Box::pin(stream::iter(out).map(Ok)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn open<'a>(&'a self, path: &'a DavPath, options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
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
                // A truncating open (the usual PUT-overwrite) drops the old body first,
                // so replacing a file with a shorter one leaves no stale tail.
                if options.truncate {
                    self.provider.truncate(ino, 0).map_err(map_err)?;
                }
                Ok(self.make_file(ino))
            } else {
                let (ino, attr) = self.resolve(path)?;
                if attr.kind == Kind::Dir {
                    return Err(FsError::Forbidden);
                }
                Ok(self.make_file(ino))
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
            self.provider.rename(fp, &fname, tp, &tname).map_err(map_err)
        })
    }
}

/// One open WebDAV file handle.
struct WebdavFile<P: Provider + 'static> {
    provider: Arc<P>,
    hydrate: Option<Hydrate>,
    ino: u64,
    pos: u64,
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
                Err(ProvError::ChunkMissing(_)) if self.hydrate.is_some() => {
                    (self.hydrate.as_ref().unwrap())(self.ino);
                    self.provider.read(self.ino, self.pos, count as u32).map_err(map_err)?
                }
                Err(e) => return Err(map_err(e)),
            };
            self.pos += data.len() as u64;
            Ok(Bytes::from(data))
        })
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let n = self.provider.write(self.ino, self.pos, &buf).map_err(map_err)?;
            self.pos += n as u64;
            Ok(())
        })
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            while buf.has_remaining() {
                let chunk = buf.chunk().to_vec();
                let n = self.provider.write(self.ino, self.pos, &chunk).map_err(map_err)?;
                self.pos += n as u64;
                buf.advance(chunk.len());
            }
            Ok(())
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move { self.provider.flush(self.ino).map_err(map_err) })
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
        Self { size: a.size, is_dir: a.kind == Kind::Dir, mtime: a.mtime }
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
        Ok(if self.mtime > 0 {
            UNIX_EPOCH + Duration::from_secs(self.mtime as u64)
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
    DavHandler::builder()
        .filesystem(Box::new(WebdavFs::new(provider, hydrate)))
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
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
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
