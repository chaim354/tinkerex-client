//! Data-path backend for the on-demand file providers (CfAPI on Windows,
//! File Provider on macOS).
//!
//! The native providers only ever call the [`JuiceFsBackend`] trait. Today the
//! only implementation is [`WebDavBackend`], which talks to `juicefs webdav`
//! over localhost — chosen so the CfAPI/File Provider risk can be proven against
//! a dead-simple HTTP backend before anything exotic.
//!
//! ## Swapping in libjfs later
//!
//! When hydration/writeback performance demands it, add a second implementation
//! of [`JuiceFsBackend`] backed by a thin Go sidecar over `libjfs` (native
//! `pread`/`pwrite` against JuiceFS's block cache, partial writeback, server-side
//! dedup). Because the provider depends on the trait, that swap is a one-line
//! construction change — no provider rewrite. Sync FFI maps onto these async
//! methods via `tokio::task::spawn_blocking`.

mod backend;
mod webdav;

pub use backend::{BackendError, JuiceFsBackend, Metadata, Result};
pub use webdav::WebDavBackend;
