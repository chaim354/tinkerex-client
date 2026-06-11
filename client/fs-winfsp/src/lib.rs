//! WinFsp adapter for `fs-provider` — the Windows counterpart to `fs-fuse`.
//!
//! It maps WinFsp's [`FileSystemContext`] callbacks onto the same platform-agnostic
//! [`fs_provider::Provider`] trait the FUSE adapter uses, so the local-first tree +
//! content-addressed chunk store back a real Windows drive (via the user-installed
//! WinFsp driver). WinFsp is **path-oriented** where the `Provider` is **inode-oriented**,
//! so this layer resolves `\a\b\c` paths to inodes by walking `lookup` from the root.
//!
//! ## Status — UNVERIFIED on Windows
//! This crate was written against the real `winfsp` 0.13 API but has **not been
//! compiled or run** (the author's build host is macOS; `winfsp` only builds on
//! Windows with WinFsp installed). Treat it as a faithful first cut that needs a
//! Windows build pass — see `BUILD.md` and the `windows-mount` CI job. Known gaps are
//! flagged inline (`LIMITATION:`), the main one being truncate/overwrite (the
//! `Provider` has no truncate yet).
//!
//! Everything real is behind `#[cfg(windows)]`; on other platforms this is an empty
//! crate, so it never affects the macOS/Linux builds.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use fs_provider::{Attr, FsError, Kind, Provider, ROOT_INO};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::Result;
use winfsp::U16CStr;

// Windows file-attribute + create-option bits we need (avoids pulling extra features).
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

/// Map a provider error onto the nearest NTSTATUS (via WinFsp's `FspError`).
fn to_fsp(e: FsError) -> winfsp::FspError {
    use windows::Win32::Foundation::{
        STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY, STATUS_NOT_A_DIRECTORY,
        STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL,
    };
    let status = match e {
        FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        FsError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        FsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        FsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        _ => STATUS_UNSUCCESSFUL,
    };
    status.into()
}

/// Convert a Unix mtime (seconds) to a Windows FILETIME (100 ns ticks since 1601).
fn unix_to_filetime(secs: i64) -> u64 {
    if secs <= 0 {
        return 0;
    }
    ((secs as u64) + 11_644_473_600) * 10_000_000
}

/// Populate a WinFsp `FileInfo` from our provider `Attr`.
fn fill_file_info(fi: &mut FileInfo, attr: &Attr) {
    let is_dir = attr.kind == Kind::Dir;
    fi.file_attributes = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    fi.file_size = attr.size;
    fi.allocation_size = attr.size.div_ceil(4096) * 4096;
    let ft = unix_to_filetime(attr.mtime);
    fi.creation_time = ft;
    fi.last_access_time = ft;
    fi.last_write_time = ft;
    fi.change_time = ft;
    fi.index_number = attr.ino;
}

/// Per-open-handle context. WinFsp hands this back (by shared ref) on every call.
pub struct Context {
    ino: u64,
    parent_ino: u64,
    is_dir: bool,
    dir_buffer: DirBuffer,
    delete_pending: AtomicBool,
}

impl Context {
    fn new(ino: u64, parent_ino: u64, is_dir: bool) -> Self {
        Self {
            ino,
            parent_ino,
            is_dir,
            dir_buffer: DirBuffer::new(),
            delete_pending: AtomicBool::new(false),
        }
    }
}

/// The adapter: a `Provider` exposed as a WinFsp filesystem.
pub struct WinFspAdapter<P: Provider> {
    provider: P,
}

impl<P: Provider> WinFspAdapter<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Resolve a WinFsp path (`\`, `\a`, `\a\b`) to `(ino, parent_ino, is_dir)` by
    /// walking `lookup` from the root.
    fn resolve(&self, path: &str) -> Result<(u64, u64, bool)> {
        let mut ino = ROOT_INO;
        let mut parent = ROOT_INO;
        let mut is_dir = true;
        for comp in path.split('\\').filter(|c| !c.is_empty()) {
            let attr = self.provider.lookup(ino, comp).map_err(to_fsp)?;
            parent = ino;
            ino = attr.ino;
            is_dir = attr.kind == Kind::Dir;
        }
        Ok((ino, parent, is_dir))
    }

    /// Split a path into `(parent_ino, leaf_name)` — for create/rename/delete.
    fn split_parent<'a>(&self, path: &'a str) -> Result<(u64, &'a str)> {
        let trimmed = path.trim_end_matches('\\');
        let (parent_path, name) = match trimmed.rfind('\\') {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        let (parent_ino, _, _) = self.resolve(parent_path)?;
        Ok((parent_ino, name))
    }
}

impl<P: Provider + 'static> FileSystemContext for WinFspAdapter<P> {
    type FileContext = Context;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        // No-ACL filesystem (tenant = user; isolation is server-side). We report
        // attributes and a zero-length descriptor; WinFsp synthesizes a default DACL.
        let (_, _, is_dir) = self.resolve(&file_name.to_string_lossy())?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL },
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let (ino, parent, is_dir) = self.resolve(&file_name.to_string_lossy())?;
        let attr = self.provider.getattr(ino).map_err(to_fsp)?;
        fill_file_info(file_info.as_mut(), &attr);
        Ok(Context::new(ino, parent, is_dir))
    }

    fn close(&self, _context: Self::FileContext) {}

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = file_name.to_string_lossy();
        let (parent, name) = self.split_parent(&path)?;
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let attr = if is_dir {
            self.provider.mkdir(parent, name)
        } else {
            self.provider.create(parent, name)
        }
        .map_err(to_fsp)?;
        fill_file_info(file_info.as_mut(), &attr);
        Ok(Context::new(attr.ino, parent, is_dir))
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, _flags: u32) {
        // Commit any staged writes on close.
        if !context.is_dir {
            let _ = self.provider.flush(context.ino);
        }
        // Honor a deferred delete (set via set_delete) now that the handle is closing.
        if context.delete_pending.load(Ordering::Relaxed) {
            if let Some(fname) = file_name {
                let path = fname.to_string_lossy();
                if let Ok((parent, name)) = self.split_parent(&path) {
                    let _ = if context.is_dir {
                        self.provider.rmdir(parent, name)
                    } else {
                        self.provider.unlink(parent, name)
                    };
                }
            }
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        if let Some(ctx) = context {
            let _ = self.provider.flush(ctx.ino);
            let attr = self.provider.getattr(ctx.ino).map_err(to_fsp)?;
            fill_file_info(file_info, &attr);
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        let attr = self.provider.getattr(context.ino).map_err(to_fsp)?;
        fill_file_info(file_info, &attr);
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // LIMITATION: the Provider has no truncate, so we can't reset the file to empty
        // here. CREATE_ALWAYS/OVERWRITE on an *existing* file keeps the old tail until a
        // `truncate` lands on the Provider. New-file writes are unaffected.
        let attr = self.provider.getattr(context.ino).map_err(to_fsp)?;
        fill_file_info(file_info, &attr);
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        let data = self
            .provider
            .read(context.ino, offset, buffer.len() as u32)
            .map_err(to_fsp)?;
        let n = data.len().min(buffer.len());
        buffer[..n].copy_from_slice(&data[..n]);
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        let off = if write_to_eof {
            self.provider.getattr(context.ino).map_err(to_fsp)?.size
        } else {
            offset
        };
        let n = self.provider.write(context.ino, off, buffer).map_err(to_fsp)?;
        let attr = self.provider.getattr(context.ino).map_err(to_fsp)?;
        fill_file_info(file_info, &attr);
        Ok(n)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        // acquire() returns Ok only when the buffer needs (re)filling; on a continuation
        // read it returns Err and we just read out the already-filled buffer.
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            // "." and ".." first.
            for (nm, ino) in [(".", context.ino), ("..", context.parent_ino)] {
                let attr = self.provider.getattr(ino).map_err(to_fsp)?;
                let mut info = DirInfo::<255>::new();
                info.set_name(nm)?;
                fill_file_info(info.file_info_mut(), &attr);
                lock.write(&mut info)?;
            }
            for child in self.provider.readdir(context.ino).map_err(to_fsp)? {
                let attr = self.provider.getattr(child.ino).map_err(to_fsp)?;
                let mut info = DirInfo::<255>::new();
                info.set_name(&child.name)?;
                fill_file_info(info.file_info_mut(), &attr);
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> Result<()> {
        let from = file_name.to_string_lossy();
        let to = new_file_name.to_string_lossy();
        let (fp, fname) = self.split_parent(&from)?;
        let (tp, tname) = self.split_parent(&to)?;
        self.provider.rename(fp, fname, tp, tname).map_err(to_fsp)
    }

    #[allow(clippy::too_many_arguments)]
    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        // We don't persist Windows attributes/times; just echo back current info.
        let attr = self.provider.getattr(context.ino).map_err(to_fsp)?;
        fill_file_info(file_info, &attr);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        // Never delete here — just flag it; `cleanup` does the actual removal.
        context.delete_pending.store(delete_file, Ordering::Relaxed);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        let cur = self.provider.getattr(context.ino).map_err(to_fsp)?.size;
        if new_size > cur && new_size > 0 {
            // Grow: write a single zero byte at the new EOF (the gap reads back as zeros).
            let _ = self.provider.write(context.ino, new_size - 1, &[0]);
        }
        // LIMITATION: shrink needs a Provider truncate (not yet implemented).
        let attr = self.provider.getattr(context.ino).map_err(to_fsp)?;
        fill_file_info(file_info, &attr);
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        out_volume_info.total_size = 1 << 40; // 1 TiB nominal
        out_volume_info.free_size = 1 << 39; // 512 GiB nominal
        out_volume_info.set_volume_label("Maker");
        Ok(())
    }
}

/// Mount `provider` at `mountpoint` (e.g. `"Z:"` or a directory) and serve until the
/// process exits (WinFsp auto-unmounts on exit). Blocking.
pub fn mount_blocking<P: Provider + 'static>(provider: P, mountpoint: &str) -> Result<()> {
    let _init = winfsp::winfsp_init_or_die(); // must stay alive for the mount's lifetime

    let mut params = VolumeParams::new();
    params
        .filesystem_name("maker")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .file_info_timeout(1000)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .post_cleanup_when_modified_only(true);

    // Annotate the host type so `S` (the operation-guard strategy) resolves to its
    // default (`FineGuard`) — otherwise `host.start()` is ambiguous between the
    // FineGuard and CoarseGuard impls.
    let mut host: FileSystemHost<WinFspAdapter<P>> =
        FileSystemHost::new(params, WinFspAdapter::new(provider))?;
    host.mount(mountpoint.to_string())?;
    host.start()?;
    println!("mounted at {mountpoint} (WinFsp); Ctrl-C to unmount");

    // The WinFsp dispatcher runs on its own threads; keep `host` + `_init` alive.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
