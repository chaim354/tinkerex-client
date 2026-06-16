//! Seed a library + mint a Windows token, for testing the Windows CfAPI client.
//!
//! No mount involved — it pushes files straight to the server via the sync engine (so it
//! avoids the WebDAV-mount lifecycle entirely). It (1) gets a token (registers a fresh
//! account from `EMAIL`, or reuses `TOKEN`), (2) mints a *second* device token for the
//! Windows box (same library), (3) pushes files into the library — the contents of
//! `FILES_DIR` if set, otherwise a few generated samples — and (4) prints the device token
//! to paste into `maker-cfsyncd` on Windows.
//!
//!   # generated sample files:
//!   SERVER_URL=http://192.168.1.95 cargo run --example mac_host
//!   # push your own files (top-level files of a folder):
//!   SERVER_URL=http://192.168.1.95 FILES_DIR=~/Desktop/to-sync cargo run --example mac_host
//!   # reuse an existing library:
//!   SERVER_URL=http://192.168.1.95 TOKEN=<existing-device-token> cargo run --example mac_host
//!
//! NOTE: the server must use the S3/SeaweedFS chunk backend (the deployed Unraid server
//! does) so the Windows box can hydrate the bytes.

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{PresignedIO, SyncEngine};

fn main() {
    let server = std::env::var("SERVER_URL").expect("set SERVER_URL (e.g. http://192.168.1.95)");

    // 1. Get a token for the library we'll seed.
    let mut client;
    match std::env::var("TOKEN") {
        Ok(tok) => {
            client = ServerClient::with_token(&server, tok);
            println!("✓ using existing library via TOKEN");
        }
        Err(_) => {
            let tag = format!(
                "{:x}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            );
            let email =
                std::env::var("EMAIL").unwrap_or_else(|_| format!("maker-{tag}@example.com"));
            client = ServerClient::new(&server);
            client
                .enroll(&email, "correct horse battery staple", "mac-host")
                .expect("register failed (email may already exist — omit EMAIL or pass TOKEN=)");
            println!("✓ registered new library: {email}");
        }
    }

    // 2. Mint a device token for the Windows machine (same library).
    let win = client
        .create_device("windows-cfapi")
        .expect("mint windows device token");

    // 3. Build a scratch local tree + seed it, then push to the server.
    let tag = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let db = std::env::temp_dir().join(format!("maker-host-{tag}.db"));
    let chunks = std::env::temp_dir().join(format!("maker-host-{tag}-chunks"));
    let provider = SqliteProvider::open(
        &db,
        Box::new(LocalChunkStore::new(&chunks).expect("chunk store")),
    )
    .expect("open db");

    let mut names: Vec<String> = Vec::new();
    match std::env::var("FILES_DIR") {
        Ok(dir) => {
            let dir = shellexpand(&dir);
            for entry in std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("read FILES_DIR {dir}: {e}"))
                .flatten()
            {
                let path = entry.path();
                if path.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        continue;
                    } // skip .DS_Store etc.
                    let bytes = std::fs::read(&path).expect("read file");
                    let attr = provider.create(ROOT_INO, &name).expect("create");
                    provider.ingest_bytes(attr.ino, &bytes).expect("ingest");
                    println!("  + {name} ({} bytes)", bytes.len());
                    names.push(name);
                }
            }
            if names.is_empty() {
                panic!("FILES_DIR {dir} had no top-level files to push");
            }
        }
        Err(_) => {
            for (name, body) in [
                ("hello.txt", b"hello from the mac host\n".to_vec()),
                (
                    "notes.md",
                    b"# Maker\n\nThis file was pushed to the server by mac_host.\n".to_vec(),
                ),
                (
                    "blob.bin",
                    (0..4096u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
                ),
            ] {
                let attr = provider.create(ROOT_INO, name).expect("create");
                provider.ingest_bytes(attr.ino, &body).expect("ingest");
                println!("  + {name} ({} bytes)", body.len());
                names.push(name.to_string());
            }
        }
    }

    let io = PresignedIO::new(&client);
    SyncEngine::push(&provider, &client, &io).expect("push to server (S3 backend + reachable?)");

    // Machine-parseable line for the CfAPI smoke (cfapi-smoke.ps1 greps this); keep it stable.
    println!("WINDOWS_DEVICE_TOKEN={}", win.device_token);

    println!("\n────────────────────────────────────────────────────────");
    println!(
        "  Pushed {} file(s) to the library: {}",
        names.len(),
        names.join(", ")
    );
    println!();
    println!("  PASTE THIS ON WINDOWS as the device token:");
    println!("    {}", win.device_token);
    println!("  …and use SERVER_URL: {server}");
    println!("────────────────────────────────────────────────────────");
}

/// Minimal `~` expansion for the FILES_DIR convenience.
fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}
