//! Mount controller for the Maker client.
//!
//! Supervises a foreground `juicefs mount` child process. On Windows that child
//! loads WinFsp and exposes a drive letter (e.g. `Z:`); killing/unmounting tears
//! it down. This is the v1 data-path bridge — the same supervision shape later
//! drives the CfAPI provider once that lands (which is why mount logic lives
//! behind this controller, not inline in the Tauri commands).

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::Serialize;

/// How to launch the mount. For a Windows test, `mount_point` is a drive spec
/// like `"Z:"`; on Unix it's a directory path.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Path to the `juicefs` binary (or just `"juicefs"` if it's on PATH).
    pub juicefs_bin: PathBuf,
    /// Metadata engine URL, e.g. `redis://127.0.0.1:6379/1` or a postgres DSN.
    pub meta_url: String,
    /// Mount target: `"Z:"` on Windows, a path on Unix.
    pub mount_point: String,
    /// Extra `juicefs mount` flags, e.g. `--cache-size 10240 --writeback`.
    pub extra_args: Vec<String>,
}

impl MountConfig {
    /// Build from env vars, for quick test wiring:
    /// `JUICEFS_BIN`, `JUICEFS_META`, `JUICEFS_MOUNTPOINT`, `JUICEFS_ARGS`.
    pub fn from_env() -> Self {
        let juicefs_bin = std::env::var("JUICEFS_BIN").unwrap_or_else(|_| "juicefs".into());
        let meta_url = std::env::var("JUICEFS_META").unwrap_or_default();
        let mount_point = std::env::var("JUICEFS_MOUNTPOINT").unwrap_or_else(|_| "Z:".into());
        let extra_args = std::env::var("JUICEFS_ARGS")
            .ok()
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        Self {
            juicefs_bin: juicefs_bin.into(),
            meta_url,
            mount_point,
            extra_args,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("already mounted")]
    AlreadyMounted,
    #[error("not mounted")]
    NotMounted,
    #[error("meta url is empty (set JUICEFS_META)")]
    NoMetaUrl,
    #[error("failed to spawn juicefs: {0}")]
    Spawn(String),
    #[error("juicefs exited immediately: {0}")]
    ExitedEarly(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountState {
    Unmounted,
    Mounted,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountStatus {
    pub state: MountState,
    pub mount_point: String,
    pub detail: Option<String>,
}

pub struct MountController {
    config: MountConfig,
    child: Option<Child>,
    last_error: Option<String>,
}

impl MountController {
    pub fn new(config: MountConfig) -> Self {
        Self {
            config,
            child: None,
            last_error: None,
        }
    }

    /// The exact argv passed to `juicefs`. Pure/testable — no spawning.
    fn build_args(&self) -> Vec<String> {
        let mut args = vec![
            "mount".to_string(),
            self.config.meta_url.clone(),
            self.config.mount_point.clone(),
        ];
        args.extend(self.config.extra_args.iter().cloned());
        args
    }

    pub fn mount(&mut self) -> Result<(), MountError> {
        if self.is_running() {
            return Err(MountError::AlreadyMounted);
        }
        if self.config.meta_url.trim().is_empty() {
            return Err(MountError::NoMetaUrl);
        }

        let mut child = Command::new(&self.config.juicefs_bin)
            .args(self.build_args())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MountError::Spawn(e.to_string()))?;

        // If juicefs fails fast (bad meta, no WinFsp, bad mountpoint) it exits
        // within a moment — catch that and surface its stderr to the user.
        std::thread::sleep(Duration::from_millis(800));
        if let Some(status) = child
            .try_wait()
            .map_err(|e| MountError::Spawn(e.to_string()))?
        {
            let mut detail = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut detail);
            }
            let msg = format!("exit {status}: {}", detail.trim());
            self.last_error = Some(msg.clone());
            return Err(MountError::ExitedEarly(msg));
        }

        // Healthy: drain stderr in the background so its pipe never fills (which
        // would deadlock the child) and so logs reach the parent console.
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("[juicefs] {line}");
                }
            });
        }

        self.last_error = None;
        self.child = Some(child);
        Ok(())
    }

    pub fn unmount(&mut self) -> Result<(), MountError> {
        if self.child.is_none() {
            return Err(MountError::NotMounted);
        }
        // Clean unmount: ask juicefs to release the mount; the foreground
        // process then exits on its own. Fall back to killing the child.
        let _ = Command::new(&self.config.juicefs_bin)
            .arg("umount")
            .arg(&self.config.mount_point)
            .status();

        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        Ok(())
    }

    /// True if the supervised mount process is still alive.
    pub fn is_running(&mut self) -> bool {
        match &mut self.child {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn status(&mut self) -> MountStatus {
        let running = self.is_running();
        let state = if running {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MountConfig {
        MountConfig {
            juicefs_bin: "juicefs".into(),
            meta_url: "redis://127.0.0.1:6379/1".into(),
            mount_point: "Z:".into(),
            extra_args: vec!["--cache-size".into(), "10240".into()],
        }
    }

    #[test]
    fn builds_expected_argv() {
        let c = MountController::new(cfg());
        let args = c.build_args();
        assert_eq!(args[0], "mount");
        assert_eq!(args[1], "redis://127.0.0.1:6379/1");
        assert_eq!(args[2], "Z:");
        assert_eq!(&args[3..], &["--cache-size", "10240"]);
    }

    #[test]
    fn empty_meta_is_rejected() {
        let mut bad = cfg();
        bad.meta_url = "  ".into();
        let mut c = MountController::new(bad);
        assert!(matches!(c.mount(), Err(MountError::NoMetaUrl)));
    }

    #[test]
    fn fresh_controller_is_unmounted() {
        let mut c = MountController::new(cfg());
        assert_eq!(c.status().state, MountState::Unmounted);
        assert!(matches!(c.unmount(), Err(MountError::NotMounted)));
    }
}
