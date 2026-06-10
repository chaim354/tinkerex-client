//! macOS mount strategy: run a `juicefs webdav` sidecar on localhost and mount
//! it with the **native** macOS WebDAV client (`mount_webdav`). No FUSE, no
//! kext, no macFUSE/WinFsp licensing — the OS does the mounting.
//!
//! Flow: spawn `juicefs webdav <meta> <addr>` → wait for the port → run
//! `mount_webdav -S -v <name> http://<addr>/ <mount_point>`. Unmount reverses it.

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{MountError, MountState, MountStatus, Mounter};

#[derive(Debug, Clone)]
pub struct WebDavConfig {
    /// Path to the `juicefs` binary (or `"juicefs"` if on PATH).
    pub juicefs_bin: PathBuf,
    /// Metadata engine URL, e.g. `redis://192.168.1.8:6379/1`.
    pub meta_url: String,
    /// Address the WebDAV sidecar listens on, e.g. `localhost:9007`.
    pub addr: String,
    /// Where to mount. macOS `/Volumes` isn't user-writable, so this defaults
    /// to a path under `$HOME`.
    pub mount_point: String,
    /// Display name for the mounted volume.
    pub volume_name: String,
}

impl WebDavConfig {
    /// `JUICEFS_BIN`, `JUICEFS_META`, `JUICEFS_WEBDAV_ADDR`, `JUICEFS_VOLUME`,
    /// `JUICEFS_MOUNTPOINT`.
    pub fn from_env() -> Self {
        let juicefs_bin = std::env::var("JUICEFS_BIN").unwrap_or_else(|_| "juicefs".into());
        let meta_url = std::env::var("JUICEFS_META").unwrap_or_default();
        let addr = std::env::var("JUICEFS_WEBDAV_ADDR").unwrap_or_else(|_| "localhost:9007".into());
        let volume_name = std::env::var("JUICEFS_VOLUME").unwrap_or_else(|_| "Maker".into());
        let mount_point = std::env::var("JUICEFS_MOUNTPOINT").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/{volume_name}")
        });
        Self {
            juicefs_bin: juicefs_bin.into(),
            meta_url,
            addr,
            mount_point,
            volume_name,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
}

pub struct WebDavMountController {
    config: WebDavConfig,
    sidecar: Option<Child>,
    mounted: bool,
    last_error: Option<String>,
}

impl WebDavMountController {
    pub fn new(config: WebDavConfig) -> Self {
        Self {
            config,
            sidecar: None,
            mounted: false,
            last_error: None,
        }
    }

    fn sidecar_alive(&mut self) -> bool {
        match &mut self.sidecar {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn kill_sidecar(&mut self) {
        if let Some(mut child) = self.sidecar.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

impl Mounter for WebDavMountController {
    fn mount(&mut self) -> Result<(), MountError> {
        if self.mounted || self.sidecar_alive() {
            return Err(MountError::AlreadyMounted);
        }
        if self.config.meta_url.trim().is_empty() {
            return Err(MountError::NoMetaUrl);
        }

        // 1. Start the WebDAV sidecar.
        let mut child = Command::new(&self.config.juicefs_bin)
            .args(["webdav", &self.config.meta_url, &self.config.addr])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MountError::Spawn(e.to_string()))?;

        // Catch a fast failure (bad meta, port in use) and surface its stderr.
        std::thread::sleep(Duration::from_millis(500));
        if let Some(status) = child
            .try_wait()
            .map_err(|e| MountError::Spawn(e.to_string()))?
        {
            let mut detail = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut detail);
            }
            let msg = format!("juicefs webdav exit {status}: {}", detail.trim());
            self.last_error = Some(msg.clone());
            return Err(MountError::ExitedEarly(msg));
        }

        // 2. Wait for the server to actually accept connections.
        if !wait_for_port(&self.config.addr, Duration::from_secs(10)) {
            self.kill_sidecar_with(child);
            let msg = format!("webdav server never listened on {}", self.config.addr);
            self.last_error = Some(msg.clone());
            return Err(MountError::ExitedEarly(msg));
        }

        // 3. Native macOS WebDAV mount. /Volumes isn't user-writable, so the
        //    mount point lives under $HOME; create it (must be empty).
        let _ = std::fs::create_dir_all(&self.config.mount_point);
        let out = Command::new("mount_webdav")
            .arg("-S") // suppress the auth dialog (localhost, no auth)
            .arg("-v")
            .arg(&self.config.volume_name)
            .arg(self.config.url())
            .arg(&self.config.mount_point)
            .output();

        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                self.kill_sidecar_with(child);
                let msg = format!(
                    "mount_webdav failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                self.last_error = Some(msg.clone());
                return Err(MountError::ExitedEarly(msg));
            }
            Err(e) => {
                self.kill_sidecar_with(child);
                let msg = format!("could not run mount_webdav: {e}");
                self.last_error = Some(msg.clone());
                return Err(MountError::Spawn(msg));
            }
        }

        // Healthy: drain the sidecar's stderr so its pipe never fills.
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("[juicefs-webdav] {line}");
                }
            });
        }

        self.sidecar = Some(child);
        self.mounted = true;
        self.last_error = None;
        Ok(())
    }

    fn unmount(&mut self) -> Result<(), MountError> {
        if !self.mounted && self.sidecar.is_none() {
            return Err(MountError::NotMounted);
        }
        // Unmount the volume first; fall back to a forced diskutil unmount.
        let umount = Command::new("umount").arg(&self.config.mount_point).status();
        if !matches!(umount, Ok(s) if s.success()) {
            let _ = Command::new("diskutil")
                .args(["unmount", "force", &self.config.mount_point])
                .status();
        }
        self.kill_sidecar();
        self.mounted = false;
        Ok(())
    }

    fn status(&mut self) -> MountStatus {
        let state = if self.sidecar_alive() && self.mounted {
            MountState::Mounted
        } else if self.last_error.is_some() {
            MountState::Failed
        } else {
            MountState::Unmounted
        };
        MountStatus {
            state,
            mount_point: self.config.mount_point.clone(),
            detail: self.last_error.clone(),
        }
    }
}

impl WebDavMountController {
    /// Kill a specific child (used on the error paths before `self.sidecar` is set).
    fn kill_sidecar_with(&mut self, mut child: Child) {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// Poll until `addr` accepts a TCP connection, or `timeout` elapses.
fn wait_for_port(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(mut addrs) = addr.to_socket_addrs() {
            if let Some(sa) = addrs.next() {
                if TcpStream::connect_timeout(&sa, Duration::from_millis(300)).is_ok() {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_defaults() {
        let cfg = WebDavConfig {
            juicefs_bin: "juicefs".into(),
            meta_url: "redis://192.168.1.8:6379/1".into(),
            addr: "localhost:9007".into(),
            mount_point: "/Users/me/Maker".into(),
            volume_name: "Maker".into(),
        };
        assert_eq!(cfg.url(), "http://localhost:9007/");
    }

    #[test]
    fn empty_meta_rejected() {
        let mut cfg = WebDavConfig {
            juicefs_bin: "juicefs".into(),
            meta_url: "  ".into(),
            addr: "localhost:9007".into(),
            mount_point: "/tmp/Maker".into(),
            volume_name: "Maker".into(),
        };
        cfg.meta_url = "".into();
        let mut c = WebDavMountController::new(cfg);
        assert!(matches!(c.mount(), Err(MountError::NoMetaUrl)));
        assert_eq!(c.status().state, MountState::Unmounted);
    }
}
