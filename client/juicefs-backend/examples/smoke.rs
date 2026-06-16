//! Smoke test against a live `juicefs webdav` sidecar.
//!
//! Start the sidecar (after `juicefs format` + a metadata engine):
//!     juicefs webdav <META-URL> --addr 127.0.0.1:9007
//!
//! Then:
//!     JUICEFS_WEBDAV=http://127.0.0.1:9007/ cargo run --example smoke -- /
//!     JUICEFS_WEBDAV=http://127.0.0.1:9007/ cargo run --example smoke -- /Projects/part.stl
//!
//! A directory path lists its children; a file path hydrates the first 64 bytes.

use juicefs_backend::{JuiceFsBackend, WebDavBackend};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("JUICEFS_WEBDAV").unwrap_or_else(|_| "http://127.0.0.1:9007/".into());
    let path = std::env::args().nth(1).unwrap_or_else(|| "/".into());

    let backend = WebDavBackend::new(&base)?;

    let meta = backend.stat(&path).await?;
    println!(
        "stat {path}: dir={} size={} etag={:?}",
        meta.is_dir, meta.size, meta.etag
    );

    if meta.is_dir {
        println!("listing {path}:");
        for e in backend.readdir(&path).await? {
            let kind = if e.is_dir { "d" } else { "-" };
            println!("  {kind} {:>10}  {}", e.size, e.name);
        }
    } else {
        let head = backend.read_range(&path, 0, 64).await?;
        println!("first {} bytes: {:?}", head.len(), head);
    }

    Ok(())
}
