# fs-provider reliability hardening — 2026-06-18

> Relocated from the maker app's `docs/STATUS.md` when this branch moved to
> `tinkerex-client`. Original commit: `0e34b71` in `chaim354/tinkerex`.

A test-driven hardening pass on the local file layer. Every failing test = a real bug,
fixed RED→GREEN. **4 real data-safety bugs found + fixed** (all in `client/fs-provider/src/lib.rs`
/ `chunkstore.rs`), plus broad coverage of the failure space. `fs-provider` suite **208/0**
on macOS; `sync-engine` + `sync-client` suites still green against the changes; real-mount
pjdfstest (Docker FUSE) unchanged baseline.

- **BUG: `unlink` didn't remove symlinks** — it enqueued a `delete` to the outbox but the row
  DELETE was scoped `kind='file'`, so a symlink stayed in the local tree while a delete was
  pushed → silent divergence + resurrection on re-export. Fix: delete the non-dir row.
  (`tests/symlink_unlink_bug.rs`, `tests/symlink_convergence.rs`)
- **BUG: reused inode served a dead file's staged bytes** — `nodes.ino` is `INTEGER PRIMARY KEY`
  (no AUTOINCREMENT) so SQLite reuses rowids; a file opened-for-write then deleted left its
  in-memory staging buffer behind, and a remote create reusing that ino read back the OLD
  file's uncommitted bytes (silent cross-file corruption). Fix: evict staging on the reused
  ino at `apply_remote` INSERT + on local delete paths. (`tests/ino_reuse_stale_staging.rs`)
- **BUG: a wild write/truncate offset SIGABRT'd the daemon** — `write(offset,…)`/`truncate(size)`
  resized the staging buffer with no bound; a 1 PiB offset (or near-`u64::MAX`, which also
  overflows `offset+len`) attempted a petabyte allocation → process abort (mount down). Fix:
  `MAX_FILE_SIZE` cap (16 TiB) + `checked_add`; new `FsError::FileTooLarge` (→ EFBIG).
  (`tests/write_offset_bounds.rs`)
- **BUG: a failed `flush` silently dropped the staged edit** — `flush` removed the staging
  buffer up front, so if a chunk `put` failed (full disk / network blip mid-finalize) the
  edit was lost while the node kept its old content. Fix: clone-and-keep; only remove staging
  after `tx.commit()` succeeds (and only if still byte-identical, preserving a racing write).
  Driven by a fault-injecting ChunkStore. (`tests/flush_fault_recovery.rs`)
- **Hardening (no bug):** content-addressed **chunk-integrity verification on read** — new
  `ChunkStore::get_verified` (re-hash, `InvalidData` on mismatch) routed through
  `assemble`/`assemble_range`; new `FsError::ChunkCorrupt` (kept distinct from `ChunkMissing`
  so a real miss still hydrates while corruption fails loudly as EIO).
  (`tests/chunk_integrity.rs`)
- **Coverage added:** high-volume mixed-op concurrency stress on one shared `Arc<Provider>`
  (`tests/concurrency_stress.rs`), export→apply sync round-trip proptest over arbitrary trees +
  symlink targets (`tests/sync_roundtrip_prop.rs`), Unicode-name byte-preservation / NFC≠NFD /
  exotic names (`tests/unicode_names.rs`), and the outbox topo-drain parents-first invariant,
  mutation-verified (`tests/outbox_topo_invariant.rs`).

