//! Server-backed ADVERSARIAL sync red-tests — the unhappy concurrency races the existing
//! `integration.rs` doesn't cover: delete-vs-edit, 3-way concurrent edits, and conflict
//! keep-resolution convergence. These hit the REAL Rails control plane + changelog (chunk
//! bytes through a `SharedDirIO` cloud stand-in), so they exercise the server's conflict
//! adjudication, not a mock.
//!
//! Discipline: each test tries to break a no-data-loss / convergence claim. Skips cleanly
//! (printed notice) when `SYNC_IT_SERVER_URL`/`SERVER_URL` is unset, like integration.rs.
//! A real defect is fixed (flipped green) or left `#[ignore]`'d with a precise repro.

use std::time::{SystemTime, UNIX_EPOCH};

use fs_provider::{ChunkConfig, LocalChunkStore, Provider, SqliteProvider, ROOT_INO};
use sync_client::ServerClient;
use sync_engine::{SharedDirIO, SyncEngine};

macro_rules! server_url {
    () => {
        match std::env::var("SYNC_IT_SERVER_URL").or_else(|_| std::env::var("SERVER_URL")) {
            Ok(u) => u,
            Err(_) => {
                eprintln!("SKIPPED (set SYNC_IT_SERVER_URL or SERVER_URL to run adversarial sync)");
                return;
            }
        }
    };
}

const TINY: ChunkConfig = ChunkConfig {
    min: 64,
    avg: 256,
    max: 1024,
};

fn uniq() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        n
    )
}

fn dev() -> SqliteProvider {
    let dir = std::env::temp_dir().join(format!("adv-dev-{}", uniq()));
    SqliteProvider::in_memory(Box::new(LocalChunkStore::new(dir).unwrap()), TINY).unwrap()
}

fn read_all(p: &SqliteProvider, ino: u64) -> Vec<u8> {
    let size = p.getattr(ino).unwrap().size;
    if size == 0 {
        Vec::new()
    } else {
        p.read(ino, 0, size as u32).unwrap()
    }
}

/// One library, three client tokens (A is the owner; B and C join via create_device).
fn three_devices(url: &str) -> (ServerClient, ServerClient, ServerClient, SharedDirIO) {
    let mut a = ServerClient::new(url);
    a.enroll(
        &format!("adv-{}@example.com", uniq()),
        "correct horse battery staple",
        "device-A",
    )
    .expect("register A");
    let b_tok = a.create_device("device-B").expect("device B").device_token;
    let c_tok = a.create_device("device-C").expect("device C").device_token;
    let b = ServerClient::with_token(url, b_tok);
    let c = ServerClient::with_token(url, c_tok);
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("adv-cloud-{}", uniq()))).unwrap();
    (a, b, c, cloud)
}

// =======================================================================================
// RACE 1: delete-vs-edit. A deletes a file; B (which never saw the delete) edits the SAME
// file based on the pre-delete version and pushes. The classic sync data-loss trap: B's
// edit must NOT silently vanish. Acceptable safe outcomes: (a) the server records a
// conflict (B's edit preserved for the user to adjudicate), or (b) B's edit resurrects the
// file (its bytes survive). The UNACCEPTABLE outcome: B's push is accepted as a clean
// fast-forward, the changelog shows the file deleted, and B's bytes are gone with no signal.
// =======================================================================================

#[test]
fn delete_vs_offline_edit_does_not_silently_lose_the_edit() {
    let url = server_url!();
    let (ca, cb, _cc, cloud) = three_devices(&url);

    // A creates + pushes v1; B snapshots (tracks v1).
    let a = dev();
    let f = a.create(ROOT_INO, "part.scad").unwrap();
    a.write(f.ino, 0, b"original part geometry").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let _b_cur = SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "part.scad").unwrap();

    // A deletes the file and pushes (server: node soft-deleted).
    a.unlink(ROOT_INO, "part.scad").unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B — never saw the delete — edits its copy (still based on v1) and pushes.
    let b_edit = b"B's important offline edit to a now-deleted file".to_vec();
    b.truncate(bf.ino, 0).unwrap();
    b.write(bf.ino, 0, &b_edit).unwrap();
    b.flush(bf.ino).unwrap();
    SyncEngine::push_outbox(&b, &cb, &cloud).unwrap();

    // Inspect the authoritative outcome on the server.
    let conflicts = cb.conflicts().unwrap().conflicts;
    let resurrected = {
        // Does a fresh device pulling the full snapshot still see the file with B's bytes?
        let fresh = dev();
        let fresh_tok = ca.create_device("fresh").unwrap().device_token;
        let cf = ServerClient::with_token(&url, fresh_tok);
        SyncEngine::pull_snapshot(&fresh, &cf).unwrap();
        match fresh.lookup(ROOT_INO, "part.scad") {
            Ok(attr) => {
                let nid = fresh.node_id_for(attr.ino).unwrap().unwrap();
                // Hydrate + compare to B's edit.
                let _ = SyncEngine::hydrate(&fresh, &cf, &cloud, &nid);
                read_all(&fresh, attr.ino) == b_edit
            }
            Err(_) => false,
        }
    };

    assert!(
        !conflicts.is_empty() || resurrected,
        "BUG: delete-vs-edit silently lost B's edit — no conflict recorded AND the file is \
         not present with B's bytes on a fresh snapshot. B's offline work vanished without a signal."
    );
}

// =======================================================================================
// RACE 2: three-way concurrent edit off the SAME base — FIXED (was a documented RED).
//
// A, B, C all snapshot v1, then each edits + pushes. A holds the current base → its push is
// the clean v2. B and C are both based on the stale v1, so EACH raises a conflict and BOTH
// divergent edits are presented to the user for adjudication (no edit silently lost).
//
// THE FIX (server conflict-model change): `Sync::Commit#finalize` used to record the conflict
// with `find_or_initialize_by(node_id:)` and overwrite `incoming_version_id`, so C's stale push
// reused B's open row and REPLACED B's incoming version — B's edit survived only in version
// history, referenced by no conflict, invisible to the resolution UI. The row is now keyed by
// (node_id, incoming_version_id) (unique index), so every divergent incoming version gets its
// own conflict row and surfaces in `GET /v1/conflicts`. keep/split resolve ALL the node's rows.
//
// EXPECTED: >= 2 unresolved conflicts here (one per divergent incoming edit).
// =======================================================================================

#[test]
fn three_way_concurrent_edit_records_conflicts_no_silent_lww() {
    let url = server_url!();
    let (ca, cb, cc, cloud) = three_devices(&url);

    // A creates v1; B and C snapshot (all three track v1).
    let a = dev();
    let f = a.create(ROOT_INO, "model.step").unwrap();
    a.write(f.ino, 0, b"shared base v1").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "model.step").unwrap();
    let c = dev();
    SyncEngine::pull_snapshot(&c, &cc).unwrap();
    let cf = c.lookup(ROOT_INO, "model.step").unwrap();

    // All three edit their local copy off v1.
    let af = a.lookup(ROOT_INO, "model.step").unwrap();
    for (p, ino, body) in [
        (&a, af.ino, b"A diverged".to_vec()),
        (&b, bf.ino, b"B diverged differently".to_vec()),
        (&c, cf.ino, b"C diverged a third way".to_vec()),
    ] {
        p.truncate(ino, 0).unwrap();
        p.write(ino, 0, &body).unwrap();
        p.flush(ino).unwrap();
    }

    // A already holds the current base (it never re-pulled), so A's push is the clean v2.
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();
    // B and C push stale-based edits → each should conflict.
    SyncEngine::push_outbox(&b, &cb, &cloud).unwrap();
    SyncEngine::push_outbox(&c, &cc, &cloud).unwrap();

    // The server must have recorded conflicts for the two stale edits (B and C), not LWW them.
    let conflicts = cb.conflicts().unwrap().conflicts;
    let incoming: Vec<String> = conflicts
        .iter()
        .filter_map(|c| c.incoming.as_ref().map(|s| s.version_id.clone()))
        .collect();
    assert!(
        conflicts.len() >= 2,
        "BUG: expected >=2 conflicts (B and C edited a stale base), got {} — \
         a silent last-writer-wins dropped at least one device's edit",
        conflicts.len()
    );
    // Two DISTINCT incoming versions (B's and C's) must both be reachable — the N-way-collapse
    // bug surfaced only one, dropping the other from the resolution UI.
    let mut distinct = incoming.clone();
    distinct.sort();
    distinct.dedup();
    assert!(
        distinct.len() >= 2,
        "BUG (N-way collapse): expected 2 DISTINCT incoming versions reachable, got {:?}",
        incoming
    );
}

// =======================================================================================
// RACE 2b: FOUR-way concurrent edit. A holds current (clean v2); B, C, D all push off the
// stale v1 → THREE divergent incoming versions, all independently reachable. Then KEEPING
// one side resolves every open conflict on the node and every device converges on it.
// =======================================================================================

#[test]
fn four_way_concurrent_edit_surfaces_all_then_keep_resolves_all() {
    let url = server_url!();
    let mut ca = ServerClient::new(&url);
    ca.enroll(
        &format!("adv4-{}@example.com", uniq()),
        "correct horse battery staple",
        "device-A",
    )
    .expect("register A");
    let clients: Vec<ServerClient> = ["B", "C", "D"]
        .iter()
        .map(|n| {
            let tok = ca
                .create_device(&format!("device-{n}"))
                .expect("device")
                .device_token;
            ServerClient::with_token(&url, tok)
        })
        .collect();
    let cloud =
        SharedDirIO::new(std::env::temp_dir().join(format!("adv4-cloud-{}", uniq()))).unwrap();

    // A creates v1; B, C, D snapshot.
    let a = dev();
    let f = a.create(ROOT_INO, "model.step").unwrap();
    a.write(f.ino, 0, b"shared base v1").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let mut others = Vec::new();
    for cl in &clients {
        let p = dev();
        SyncEngine::pull_snapshot(&p, cl).unwrap();
        let fino = p.lookup(ROOT_INO, "model.step").unwrap().ino;
        others.push((p, fino));
    }

    // A advances current to a clean v2 (it never re-pulled).
    let af = a.lookup(ROOT_INO, "model.step").unwrap();
    a.truncate(af.ino, 0).unwrap();
    a.write(af.ino, 0, b"A clean v2 current").unwrap();
    a.flush(af.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    // B, C, D each push a distinct stale-based edit → three conflicts.
    for (i, ((p, fino), cl)) in others.iter().zip(clients.iter()).enumerate() {
        p.truncate(*fino, 0).unwrap();
        p.write(*fino, 0, format!("diverged side {i}").as_bytes())
            .unwrap();
        p.flush(*fino).unwrap();
        SyncEngine::push_outbox(p, cl, &cloud).unwrap();
    }

    let conflicts = clients[0].conflicts().unwrap().conflicts;
    let mut distinct: Vec<String> = conflicts
        .iter()
        .filter_map(|c| c.incoming.as_ref().map(|s| s.version_id.clone()))
        .collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        3,
        "BUG: expected 3 distinct divergent incoming versions (B,C,D), got {:?}",
        distinct
    );

    // KEEP A's current v2 on the node — must clear ALL three conflict rows.
    let node_id = conflicts[0].node_id.clone();
    let keep_v = conflicts[0]
        .current
        .as_ref()
        .expect("current side")
        .version_id
        .clone();
    clients[0]
        .resolve_conflict_keep(&node_id, &keep_v)
        .expect("keep current");
    assert!(
        clients[0].conflicts().unwrap().conflicts.is_empty(),
        "keeping one version clears every open conflict on the node (N-way)"
    );

    // A fresh device converges on the kept version, byte-for-byte.
    let fresh = dev();
    let fresh_tok = ca.create_device("fresh4").unwrap().device_token;
    let cf = ServerClient::with_token(&url, fresh_tok);
    SyncEngine::pull_snapshot(&fresh, &cf).unwrap();
    let ff = fresh
        .lookup(ROOT_INO, "model.step")
        .expect("present after keep");
    let nid = fresh.node_id_for(ff.ino).unwrap().unwrap();
    SyncEngine::hydrate(&fresh, &cf, &cloud, &nid).expect("hydrate kept");
    assert_eq!(
        read_all(&fresh, ff.ino),
        b"A clean v2 current",
        "every device converges on the kept version"
    );
}

// =======================================================================================
// RACE 3: conflict keep-resolution convergence. After a conflict, resolving by KEEPING the
// incoming side must (a) clear the conflict and (b) make that version the one every device
// converges on — not leave a device pinned to the losing version.
// =======================================================================================

#[test]
fn conflict_keep_resolution_converges_all_devices_on_the_kept_version() {
    let url = server_url!();
    let (ca, cb, _cc, cloud) = three_devices(&url);

    // A v1 → B snapshots. A edits → v2 (current). B edits stale → conflict.
    let a = dev();
    let f = a.create(ROOT_INO, "sketch.scad").unwrap();
    a.write(f.ino, 0, b"v1").unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    let b = dev();
    let b_cur = SyncEngine::pull_snapshot(&b, &cb).unwrap();
    let bf = b.lookup(ROOT_INO, "sketch.scad").unwrap();

    let a_winner = b"A's v2 current".to_vec();
    a.truncate(f.ino, 0).unwrap();
    a.write(f.ino, 0, &a_winner).unwrap();
    a.flush(f.ino).unwrap();
    SyncEngine::push_outbox(&a, &ca, &cloud).unwrap();

    b.truncate(bf.ino, 0).unwrap();
    b.write(bf.ino, 0, b"B's stale-based edit").unwrap();
    b.flush(bf.ino).unwrap();
    SyncEngine::push_outbox(&b, &cb, &cloud).unwrap();

    let conflicts = cb.conflicts().unwrap().conflicts;
    assert_eq!(
        conflicts.len(),
        1,
        "B's stale edit raised exactly one conflict"
    );
    let c = &conflicts[0];
    // KEEP the current side (A's v2) — the realistic "discard my stale edit, take theirs".
    let keep_v = c
        .current
        .as_ref()
        .expect("current side reported")
        .version_id
        .clone();
    cb.resolve_conflict_keep(&c.node_id, &keep_v)
        .expect("resolve by keeping current");
    assert!(
        cb.conflicts().unwrap().conflicts.is_empty(),
        "conflict cleared after keep"
    );

    // A fresh device must converge on the KEPT version (A's v2), byte-for-byte.
    let fresh = dev();
    let fresh_tok = ca.create_device("fresh-keep").unwrap().device_token;
    let cf = ServerClient::with_token(&url, fresh_tok);
    SyncEngine::pull_snapshot(&fresh, &cf).unwrap();
    let ff = fresh
        .lookup(ROOT_INO, "sketch.scad")
        .expect("file present after keep-resolve");
    let nid = fresh.node_id_for(ff.ino).unwrap().unwrap();
    SyncEngine::hydrate(&fresh, &cf, &cloud, &nid).expect("hydrate kept version");
    assert_eq!(
        read_all(&fresh, ff.ino),
        a_winner,
        "every device converges on the kept version"
    );

    // And B itself, pulling the resolution delta, must end up on the kept version too.
    let _ = SyncEngine::pull_changes(&b, &cb, b_cur);
    let b_nid = b.node_id_for(bf.ino).unwrap().unwrap();
    let _ = SyncEngine::hydrate(&b, &cb, &cloud, &b_nid);
    assert_eq!(
        read_all(&b, bf.ino),
        a_winner,
        "B converges on the kept version after pulling the resolution"
    );
}
