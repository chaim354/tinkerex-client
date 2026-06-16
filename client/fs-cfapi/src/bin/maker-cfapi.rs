//! Standalone CfAPI mount runner (Windows). Registers a sync root, connects the Cloud
//! Filter callbacks, populates placeholders from the local SQLite tree, and serves
//! on-demand hydration. The full sync loop lives in `sync-engine::CfApiMount`; this binary
//! is the minimal "mount + hydrate locally" harness for the BUILD.md smoke test.
//!
//! On non-Windows it's an inert stub so the crate stays cross-platform-buildable.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    let mut args = std::env::args().skip(1);
    let usage = "usage: maker-cfapi <sync-root-dir> <db> <chunks>";
    let root = PathBuf::from(args.next().expect(usage));
    let db = PathBuf::from(args.next().expect(usage));
    let chunks = PathBuf::from(args.next().expect(usage));

    let store = Box::new(fs_provider::LocalChunkStore::new(&chunks).expect("chunk store"));
    let provider =
        std::sync::Arc::new(fs_provider::SqliteProvider::open(&db, store).expect("open db"));

    // No hydrator here: a bare local mount serves whatever chunks are already cached (a
    // miss errors). The daemon wires a presigned-GET hydrator — see sync-engine::CfApiMount.
    let handle = fs_cfapi::win::mount(provider, &root, None).expect("cfapi mount");
    println!(
        "CfAPI sync root mounted at {} — Ctrl-C to stop",
        root.display()
    );
    handle.run_until_ctrl_c();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "maker-cfapi is Windows-only (Cloud Filter API). This is an inert stub on this platform."
    );
    std::process::exit(1);
}
