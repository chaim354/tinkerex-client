//! Delete + move propagation: device A deletes a file and moves another, device B
//! converges via a changelog **delta pull** (not a fresh snapshot). This is the §12
//! step-3 gap — before the fix, B never removed deleted files and a move duplicated the
//! file at both the old and new path. Run against the live Rails server:
//!
//!   STORAGE_BACKEND=memory bin/rails server -p 3001        # terminal 1
//!   SERVER_URL=http://localhost:3001 cargo run --example delete_move   # terminal 2

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn provider(tag: &str) -> SqliteProvider {
    let base = std::env::temp_dir().join(format!("maker-dm-{tag}-{}", uniq()));
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    SqliteProvider::open(&base.join("meta.db"), Box::new(LocalChunkStore::new(cache).unwrap())).unwrap()
}

fn write_file(p: &SqliteProvider, parent: u64, name: &str, body: &[u8]) {
    let f = p.create(parent, name).unwrap();
    p.write(f.ino, 0, body).unwrap();
    p.flush(f.ino).unwrap();
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let cloud = std::env::temp_dir().join(format!("maker-cloud-{}", uniq()));
    let io = SharedDirIO::new(&cloud).unwrap();

    // --- Device A: Inbox/{note.txt, trash.txt} + an empty Archive --------------
    let a = provider("A");
    let inbox = a.mkdir(ROOT_INO, "Inbox").unwrap();
    let archive = a.mkdir(ROOT_INO, "Archive").unwrap();
    write_file(&a, inbox.ino, "note.txt", b"keep me");
    write_file(&a, inbox.ino, "trash.txt", b"delete me");

    let mut client_a = ServerClient::new(&server);
    client_a.register(&format!("dm-{}@example.com", uniq()), "device-A").expect("register A");
    SyncEngine::push_outbox(&a, &client_a, &io).expect("A initial push");
    println!("✓ A pushed Inbox/{{note.txt, trash.txt}} + Archive/");

    // --- Device B: second device, initial snapshot ----------------------------
    let b = provider("B");
    let dev_b = client_a.create_device("device-B").expect("device B");
    let client_b = ServerClient::with_token(&server, dev_b.device_token);
    let cursor = SyncEngine::pull_snapshot(&b, &client_b).expect("B snapshot");

    let b_inbox = b.lookup(ROOT_INO, "Inbox").expect("B has Inbox").ino;
    let b_archive = b.lookup(ROOT_INO, "Archive").expect("B has Archive").ino;
    b.lookup(b_inbox, "note.txt").expect("B has Inbox/note.txt");
    b.lookup(b_inbox, "trash.txt").expect("B has Inbox/trash.txt");
    println!("✓ B snapshotted at seq {cursor}: Inbox/{{note.txt, trash.txt}}, Archive/ (empty)");

    // --- Device A: delete trash.txt, move note.txt Inbox -> Archive -----------
    a.unlink(inbox.ino, "trash.txt").expect("A delete trash.txt");
    a.rename(inbox.ino, "note.txt", archive.ino, "note.txt").expect("A move note.txt");
    SyncEngine::push_outbox(&a, &client_a, &io).expect("A push delete+move");
    println!("✓ A deleted trash.txt and moved note.txt -> Archive/, pushed the delta");

    // --- Device B: delta pull (the path that used to drop deletes/dupe moves) -
    let new_cursor = SyncEngine::pull_changes(&b, &client_b, cursor).expect("B delta pull");
    println!("✓ B pulled changelog delta {cursor} -> {new_cursor}");

    // Deleted file is gone on B.
    assert!(b.lookup(b_inbox, "trash.txt").is_err(), "trash.txt must be deleted on B");
    // Moved file is at the new path...
    b.lookup(b_archive, "note.txt").expect("note.txt must now be under Archive/ on B");
    // ...and NOT duplicated at the old path.
    assert!(b.lookup(b_inbox, "note.txt").is_err(), "note.txt must not remain in Inbox/ (no duplicate)");

    // The whole tree matches A exactly (same node_id/parent/name set).
    let norm = |p: &SqliteProvider| {
        let mut v: Vec<_> = p
            .export_tree()
            .unwrap()
            .into_iter()
            .map(|n| (n.node_id, n.parent_node_id, n.name, n.kind == fs_provider::Kind::Dir))
            .collect();
        v.sort();
        v
    };
    assert_eq!(norm(&a), norm(&b), "A and B trees converged exactly");

    println!("\nDELETE+MOVE OK — B converged on A's delete and move via a delta pull; no orphans, no duplicates.");
}
