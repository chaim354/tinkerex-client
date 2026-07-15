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
        Self {
            provider: self.provider.clone(),
            hydrate: self.hydrate.clone(),
        }
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

    fn make_file(&self, ino: u64, write_handle: bool) -> Box<dyn DavFile> {
        Box::new(WebdavFile {
            provider: self.provider.clone(),
            hydrate: self.hydrate.clone(),
            ino,
            pos: 0,
            write_handle,
            flushed: false,
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
                if options.truncate {
                    self.provider.begin_overwrite(ino).map_err(map_err)?;
                }
                Ok(self.make_file(ino, true))
            } else {
                let (ino, attr) = self.resolve(path)?;
                if attr.kind == Kind::Dir {
                    return Err(FsError::Forbidden);
                }
                // A read-only handle: never a write handle, so its `flush`/`Drop` must never
                // commit or discard (only write handles touch the committed content).
                Ok(self.make_file(ino, false))
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
    /// True iff opened for write. Only a write handle may commit (`flush`) or, on an aborted
    /// write, `discard` — a read-only handle must never touch the committed content.
    write_handle: bool,
    /// Set true after a successful `flush`. If a write handle is dropped with this still false
    /// (a client disconnect mid-PUT errored out of dav-server's write loop before `flush`), the
    /// `Drop` guard discards the partial staging so it can't shadow the committed content.
    flushed: bool,
}

impl<P: Provider + 'static> fmt::Debug for WebdavFile<P> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "WebdavFile {{ ino: {}, pos: {} }}", self.ino, self.pos)
    }
}

impl<P: Provider + 'static> Drop for WebdavFile<P> {
    fn drop(&mut self) {
        // A write handle that never successfully flushed (a client disconnect mid-body errors
        // out of dav-server's PUT loop WITHOUT calling `flush`) must not leave partial staged
        // bytes shadowing the committed content. Discard them; the last committed version — the
        // PRIOR content for an overwrite (`begin_overwrite` staged empty without committing) —
        // stays durable. Read-only handles and cleanly-flushed writes do nothing.
        if self.write_handle && !self.flushed {
            let _ = self.provider.discard(self.ino);
        }
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
            if !self.write_handle {
                return Ok(());
            }
            self.provider.flush(self.ino).map_err(map_err)?;
            self.flushed = true; // a clean flush → Drop won't discard
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
                    write_handle: false,
                    flushed: false,
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
                write_handle: true,
                flushed: false,
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
            write_handle: true,
            flushed: false,
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
            write_handle: false,
            flushed: false,
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
}
