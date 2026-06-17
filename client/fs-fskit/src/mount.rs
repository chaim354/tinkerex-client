//! The mount lifecycle skeleton — the macOS analogue of `sync_engine::WebdavMount` /
//! `CfApiMount` / the FUSE `Daemon`. **STUBBED**: the live mount needs the Swift `.appex`
//! registered with the system (paid Apple Developer account), so `start` returns a clear
//! `NeedsDeveloperAccount` error today. The shape — `start(cfg) -> handle`, `is_running`,
//! `stop`/`Drop` teardown — mirrors the other mounts so wiring it into `sync-engine` behind
//! the per-OS `Mount` alias is mechanical once the extension builds (see `BUILD.md` §"Wire
//! into the engine").
//!
//! ## Why this lives here, not in `sync-engine` yet
//! Wiring `FsKitMount` into `sync-engine::daemon` now would either (a) link a Swift
//! extension that can't build without the SDK, breaking the macOS CI build, or (b) be a
//! pure no-op stub cluttering the live daemon. Instead the **shape** lives here, compiles +
//! is documented, and the engine integration is a small, mechanical follow-up (BUILD.md).
//! The handle holds the same pieces the real mount will: the shared `Arc<SqliteProvider>`
//! and the opaque [`crate::ffi::FsKitBridge`] handle the Swift side uses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fs_provider::SqliteProvider;

/// Minimal config for bringing up the FSKit mount — the subset of
/// `sync_engine::DaemonConfig` this layer needs. The engine passes its own richer config
/// when this is wired in; kept standalone here so the crate has no `sync-engine` dep
/// (avoids a dependency cycle and keeps the skeleton buildable in isolation).
#[derive(Debug, Clone)]
pub struct FsKitConfig {
    /// Where the FSKit volume should appear (a directory the extension claims). FSKit
    /// mounts a *volume*, so this is a mount path, not a drive letter.
    pub mountpoint: PathBuf,
    /// The local SQLite tree DB path.
    pub db: PathBuf,
    /// The local content-addressed chunk cache dir.
    pub chunk_dir: PathBuf,
}

/// Errors bringing up (or refusing to bring up) the FSKit mount.
#[derive(Debug)]
pub enum MountError {
    /// The live mount is account-gated — the signed `.appex` + system registration need a
    /// paid Apple Developer membership and the FSKit SDK. Until then `start` returns this so
    /// callers degrade cleanly (the macOS shipping default stays WebDAV).
    NeedsDeveloperAccount,
    /// Opening the local provider failed.
    Provider(fs_provider::FsError),
    /// Creating the local chunk-cache dir / DB path failed.
    Io(std::io::Error),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::NeedsDeveloperAccount => write!(
                f,
                "FSKit mount needs a signed .appex + system registration (paid Apple \
                 Developer account); see client/fs-fskit/BUILD.md. macOS ships WebDAV until then."
            ),
            MountError::Provider(e) => write!(f, "provider: {e}"),
            MountError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for MountError {}

impl From<fs_provider::FsError> for MountError {
    fn from(e: fs_provider::FsError) -> Self {
        MountError::Provider(e)
    }
}

impl From<std::io::Error> for MountError {
    fn from(e: std::io::Error) -> Self {
        MountError::Io(e)
    }
}

/// A live FSKit mount handle. Today it only ever holds the shared provider + a teardown
/// flag; the real one will additionally own the opaque bridge pointer and the spawned
/// extension's connection, torn down on [`stop`](Self::stop)/`Drop` — exactly like
/// `CfApiMount` holds its `CfApiMountHandle`.
// Fields are unread today because `start` always returns `NeedsDeveloperAccount` (so a
// `Self` is never constructed); they pin the shape the live mount will hold. The allow goes
// away with the success path (BUILD.md §"Wire into the engine").
#[allow(dead_code)]
pub struct FsKitMount {
    provider: Arc<SqliteProvider>,
    stop: Arc<AtomicBool>,
    mountpoint: PathBuf,
}

impl FsKitMount {
    /// Bring up the mount. **STUB**: builds the shared provider (so the wiring is exercised
    /// + the local tree binds), then returns [`MountError::NeedsDeveloperAccount`] because
    /// the Swift extension can't be registered without signing. When the account lands,
    /// replace the early return with: build the [`crate::ffi::FsKitBridge`], `into_raw` it,
    /// hand the pointer to the spawned/registered extension, and spawn the same sync + nudge
    /// threads the other mounts spawn (BUILD.md §"Wire into the engine").
    pub fn start(cfg: FsKitConfig) -> Result<Self, MountError> {
        // Build the shared provider so the foundation is real (and the engine, when it calls
        // this, gets the same Arc to share with the sync loop). Binding the local tree here
        // exercises the wiring even though the mount itself can't come up yet.
        let store = Box::new(fs_provider::LocalChunkStore::new(&cfg.chunk_dir)?);
        let _provider = Arc::new(SqliteProvider::open(&cfg.db, store)?);
        let _stop = Arc::new(AtomicBool::new(false));

        // TODO(dev-account): the account-gated remainder. When the signed `.appex` + system
        // registration are in place (BUILD.md), build the success path and return `Self`:
        //   1. let bridge = crate::ffi::FsKitBridge::new(_provider.clone(), Some(hydrator));
        //   2. let raw = crate::ffi::into_raw(bridge);
        //   3. register + start the signed `.appex`, handing it `raw` (the Swift side stores
        //      it and calls the `fskit_*` shims on every op).
        //   4. spawn sync_loop + nudge_loop threads sharing `_provider` (copy WebdavMount).
        //   5. construct `Self { provider, stop, mountpoint }` and return Ok.
        //   6. on stop/Drop: signal the extension to unmount, then `free_raw(raw)` + join.
        let _ = &cfg.mountpoint;
        Err(MountError::NeedsDeveloperAccount)
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Tear down: signal stop, unmount the volume, free the bridge, join the loops.
    /// **STUB** today (no live mount to tear down).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // TODO(dev-account): unmount the FSKit volume + free_raw(bridge) + join threads.
    }
}

impl Drop for FsKitMount {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_provider::LocalChunkStore;

    #[test]
    fn start_refuses_without_developer_account() {
        // Use a temp chunk dir + an unlikely DB path under it.
        let store_dir = LocalChunkStore::temp().unwrap();
        // We can't easily get the temp dir path back out of LocalChunkStore::temp, so build
        // our own temp paths under the OS temp dir for the config.
        let base = std::env::temp_dir().join(format!("fskit-mount-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let cfg = FsKitConfig {
            mountpoint: base.join("mnt"),
            db: base.join("tree.db"),
            chunk_dir: base.join("chunks"),
        };
        drop(store_dir);

        let err = match FsKitMount::start(cfg) {
            Ok(_) => panic!("FSKit mount should not come up without a developer account"),
            Err(e) => e,
        };
        assert!(matches!(err, MountError::NeedsDeveloperAccount));
        // The message points at BUILD.md and notes the WebDAV fallback.
        let msg = err.to_string();
        assert!(msg.contains("BUILD.md"), "msg = {msg}");
        assert!(msg.contains("WebDAV"), "msg = {msg}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
