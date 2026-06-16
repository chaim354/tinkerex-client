//! `maker-offline` — the **make-available-offline CLI**, the workshop user's path to the
//! engine's offline primitives without the GUI. It operates on an existing local library
//! (the same SQLite tree + chunk dir the daemon uses) and the server, resolving a
//! filesystem-style `<path>` to a stable `node_id` and driving the engine:
//!
//!   maker-offline <db> <chunks> <server_url> <token> pin           <path>
//!   maker-offline <db> <chunks> <server_url> <token> unpin         <path> [--reclaim]
//!   maker-offline <db> <chunks> <server_url> <token> offline-status <path>
//!
//! - **pin** — record the pin AND download the subtree now; the running daemon then keeps it
//!   resident across pulls (`SyncEngine::pin`).
//! - **unpin** — remove the pin; `--reclaim` also drops the cached bytes (`SyncEngine::unpin`).
//! - **offline-status** — the pre-disconnect readiness report (NO network):
//!   `SyncEngine::offline_readiness` — "X/Y files, Z% chunks local; still online-only: …".
//!
//! Cross-platform (pure provider + engine calls; no mount), so it works on Linux, macOS, and
//! Windows alike. Run it while the daemon is stopped (or pointed at a copy of the DB) to avoid
//! two writers on one SQLite file.

use std::process::ExitCode;

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{OfflineReadiness, PresignedIO, SyncEngine};

/// Default per-file chunk-fetch concurrency for the CLI's downloads (matches the daemon's
/// hydrate default).
const CONCURRENCY: usize = 8;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!(
            "usage: maker-offline <db> <chunks> <server_url> <token> <command> <path> [--reclaim]\n\
             commands: pin <path> | unpin <path> [--reclaim] | offline-status <path>"
        );
        return ExitCode::from(2);
    }
    let (db, chunks, server_url, token, command, path) = (
        &args[0],
        &args[1],
        &args[2],
        &args[3],
        &args[4],
        args.get(5),
    );
    let path = match path {
        Some(p) => p.as_str(),
        None => {
            eprintln!("error: <path> is required for `{command}`");
            return ExitCode::from(2);
        }
    };
    let reclaim = args.iter().any(|a| a == "--reclaim");

    match run(db, chunks, server_url, token, command, path, reclaim) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("maker-offline: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    db: &str,
    chunks: &str,
    server_url: &str,
    token: &str,
    command: &str,
    path: &str,
    reclaim: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = Box::new(LocalChunkStore::new(chunks)?);
    let provider = SqliteProvider::open(std::path::Path::new(db), store)?;
    let client = ServerClient::with_token(server_url, token);
    let io = PresignedIO::new(&client);

    let node_id = resolve_path(&provider, path)?;

    match command {
        "pin" => {
            let summary = SyncEngine::pin(&provider, &client, &io, &node_id, CONCURRENCY)?;
            println!(
                "pinned {path}: downloaded {} file(s), {} bytes{}",
                summary.files_hydrated,
                summary.bytes_hydrated,
                if summary.failures.is_empty() {
                    String::new()
                } else {
                    format!(", {} FAILED", summary.failures.len())
                }
            );
            for (p, e) in &summary.failures {
                eprintln!("  failed: {p}: {e}");
            }
            // Show the post-pin readiness so the user has the confirmation the workshop flow needs.
            let readiness = SyncEngine::offline_readiness(&provider, &node_id)?;
            print_status(path, &readiness);
            Ok(if summary.failures.is_empty() && readiness.is_ready() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        "unpin" => {
            let reclaimed = SyncEngine::unpin(&provider, &node_id, reclaim)?;
            println!(
                "unpinned {path}{}",
                if reclaim {
                    format!(" (reclaimed {reclaimed} chunk(s))")
                } else {
                    String::new()
                }
            );
            Ok(ExitCode::SUCCESS)
        }
        "offline-status" => {
            let readiness = SyncEngine::offline_readiness(&provider, &node_id)?;
            print_status(path, &readiness);
            // Exit 0 iff fully ready — so a script can gate "safe to disconnect" on the exit code.
            Ok(if readiness.is_ready() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        other => {
            eprintln!("unknown command `{other}` (expected: pin | unpin | offline-status)");
            Ok(ExitCode::from(2))
        }
    }
}

/// Render a readiness report as the human-facing pre-disconnect summary.
fn print_status(path: &str, r: &OfflineReadiness) {
    let pct = if r.total_chunks == 0 {
        100
    } else {
        r.present_chunks * 100 / r.total_chunks
    };
    println!(
        "offline-status {path}: {}/{} files local, {}% of chunks ({}/{}) — {}",
        r.ready_files,
        r.total_files,
        pct,
        r.present_chunks,
        r.total_chunks,
        if r.is_ready() {
            "READY for offline ✓"
        } else {
            "NOT ready — still online-only:"
        }
    );
    for f in &r.missing_files {
        println!("  online-only: {}", f.path);
    }
}

/// Resolve a filesystem-style `<path>` (leading slash optional, e.g. `/Jobs/ClientX` or
/// `Jobs/ClientX`) to the stable `node_id` of the node it names, by walking `lookup` from the
/// library root over the components. The empty path (or `/`) resolves to the root — which has
/// no `node_id` (it's the implicit library root), so that's rejected with a clear message
/// (you pin a *folder/file*, not the whole library — use the daemon's default-include for that).
fn resolve_path(
    provider: &SqliteProvider,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut ino = ROOT_INO;
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Err(
            format!("path `{path}` is the library root — pin a folder or file under it").into(),
        );
    }
    for comp in components {
        let attr = provider
            .lookup(ino, comp)
            .map_err(|_| format!("no such path: `{path}` (component `{comp}` not found)"))?;
        ino = attr.ino;
    }
    provider.node_id_for(ino)?.ok_or_else(|| {
        format!("`{path}` has no stable node_id (unexpected for a non-root node)").into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_provider::ChunkConfig;

    fn provider() -> SqliteProvider {
        SqliteProvider::in_memory(
            Box::new(LocalChunkStore::temp().unwrap()),
            ChunkConfig {
                min: 64,
                avg: 256,
                max: 1024,
            },
        )
        .unwrap()
    }

    /// A nested path resolves to the right node's stable node_id; a leading slash is optional.
    #[test]
    fn resolve_path_walks_components() {
        let p = provider();
        let jobs = p.mkdir(ROOT_INO, "Jobs").unwrap();
        let clientx = p.mkdir(jobs.ino, "ClientX").unwrap();
        let want = p.node_id_for(clientx.ino).unwrap().unwrap();

        assert_eq!(
            resolve_path(&p, "/Jobs/ClientX").unwrap(),
            want,
            "leading slash"
        );
        assert_eq!(
            resolve_path(&p, "Jobs/ClientX").unwrap(),
            want,
            "no leading slash"
        );
        assert_eq!(
            resolve_path(&p, "Jobs/ClientX/").unwrap(),
            want,
            "trailing slash tolerated"
        );
    }

    /// A file path resolves too (you can make a single file available offline).
    #[test]
    fn resolve_path_resolves_a_file() {
        let p = provider();
        let jobs = p.mkdir(ROOT_INO, "Jobs").unwrap();
        let f = p.create(jobs.ino, "part.step").unwrap();
        let want = p.node_id_for(f.ino).unwrap().unwrap();
        assert_eq!(resolve_path(&p, "/Jobs/part.step").unwrap(), want);
    }

    /// The library root (empty / "/") is rejected with a clear message — you target a
    /// folder/file, not the whole library.
    #[test]
    fn resolve_path_rejects_the_root() {
        let p = provider();
        assert!(resolve_path(&p, "/").is_err());
        assert!(resolve_path(&p, "").is_err());
    }

    /// A non-existent path errors (rather than silently resolving to root or panicking).
    #[test]
    fn resolve_path_unknown_errors() {
        let p = provider();
        p.mkdir(ROOT_INO, "Jobs").unwrap();
        let err = resolve_path(&p, "/Jobs/Nope").unwrap_err().to_string();
        assert!(err.contains("no such path"), "got: {err}");
    }

    /// `print_status` is the human summary; smoke-test it doesn't panic on the edge cases
    /// (empty subtree → 100%, partial, full). (Output goes to stdout; we just exercise it.)
    #[test]
    fn print_status_handles_empty_partial_and_full() {
        // empty (no chunks) is trivially ready
        let empty = OfflineReadiness::default();
        assert!(empty.is_ready());
        print_status("/Empty", &empty);

        let partial = OfflineReadiness {
            total_files: 2,
            ready_files: 1,
            total_chunks: 10,
            present_chunks: 4,
            missing_files: vec![fs_provider::SubtreeFile {
                node_id: "n".into(),
                path: "Jobs/big.step".into(),
            }],
        };
        assert!(!partial.is_ready());
        print_status("/Jobs", &partial);
    }
}
