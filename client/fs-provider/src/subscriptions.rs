//! Selective sync — per-device subtree subscription policy.
//!
//! **Selectivity is a CLIENT hydration policy, never a partial server tree.** The server
//! tree stays complete and *all* node metadata still syncs into the local SQLite tree (so
//! navigation + convergence are unaffected — see `CLAUDE.md` §2/§6). What selective sync
//! changes is purely whether the sync engine *proactively* hydrates a node's chunk bytes
//! after a pull:
//!
//! - **In scope** → proactively hydrated as today (a changed file dehydrates→rehydrates).
//! - **Out of scope** → left as a dehydrated placeholder; bytes are fetched only **on
//!   demand** (an actual `open`/`read`), exactly like OneDrive online-only files. It
//!   degrades gracefully: the file is still navigable and openable, it just isn't kept
//!   resident.
//!
//! ## Model
//!
//! The policy is a set of **rules keyed by stable `node_id`** (not inode, not path — a move
//! keeps a node's `node_id`, so a subscription survives renames/moves, which is what a user
//! expects). Each rule is [`Include`](SubRule::Include) or [`Exclude`](SubRule::Exclude) and
//! applies to a node *and its whole subtree*. Resolution for any node walks **up** its
//! ancestry to the nearest ancestor (or the node itself) that carries a rule; that rule
//! wins. With no rule anywhere up to the root, the **default policy** applies.
//!
//! The default policy is [`SubRule::Include`] — i.e. with an empty subscription table every
//! node is in scope, so selective sync is opt-in and the existing "mirror everything"
//! behavior is preserved byte-for-byte until the user excludes something. A device that
//! wants the Dropbox "subscribe to a few folders" shape instead sets the default to
//! [`Exclude`](SubRule::Exclude) and includes the subtrees it wants.
//!
//! Storage lives in a `subscriptions` table in the same local SQLite DB as the tree. It is
//! purely local device state — it is **never** pushed to the server (the server tree is
//! complete and shared; subscriptions are this device's view of it).

use rusqlite::{params, Connection, OptionalExtension};

use crate::{FsError, Result, SqliteProvider, ROOT_INO};

/// A subscription rule for a node and its subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubRule {
    /// This subtree IS in scope for proactive hydration.
    Include,
    /// This subtree is NOT in scope — kept as placeholders, hydrated only on demand.
    Exclude,
}

impl SubRule {
    fn as_str(self) -> &'static str {
        match self {
            SubRule::Include => "include",
            SubRule::Exclude => "exclude",
        }
    }
    fn from_str(s: &str) -> SubRule {
        if s == "exclude" {
            SubRule::Exclude
        } else {
            SubRule::Include
        }
    }
}

/// The `sync_state`-style default-policy key.
const DEFAULT_POLICY_KEY: &str = "subscription_default";

impl SqliteProvider {
    /// Ensure the subscriptions table exists. Called from `init`.
    pub(crate) fn init_subscriptions(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS subscriptions (
                 node_id TEXT PRIMARY KEY,
                 rule    TEXT NOT NULL    -- 'include' | 'exclude'
             );",
        )?;
        Ok(())
    }

    /// The default scope policy when no ancestor rule applies. [`SubRule::Include`] (mirror
    /// everything) unless the device has flipped it to [`Exclude`](SubRule::Exclude) (the
    /// "subscribe to a few folders" shape). Stored in `sync_state`.
    pub fn subscription_default(&self) -> Result<SubRule> {
        let conn = self.conn_lock();
        self.default_rule(&conn)
    }

    /// Set the default scope policy (see [`subscription_default`](Self::subscription_default)).
    pub fn set_subscription_default(&self, rule: SubRule) -> Result<()> {
        let conn = self.conn_lock();
        let v: i64 = match rule {
            SubRule::Include => 0,
            SubRule::Exclude => 1,
        };
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![DEFAULT_POLICY_KEY, v],
        )?;
        Ok(())
    }

    /// **Subscribe** a subtree (by `node_id`) for proactive hydration: record an
    /// [`Include`](SubRule::Include) rule. Returns the inode of the subscribed node so the
    /// caller (engine) can trigger an immediate proactive hydrate of the now-in-scope files.
    /// Errors `NotFound` if the `node_id` isn't in the local tree.
    pub fn subscribe(&self, node_id: &str) -> Result<u64> {
        self.set_subscription(node_id, SubRule::Include)
    }

    /// **Unsubscribe** a subtree (by `node_id`): record an [`Exclude`](SubRule::Exclude)
    /// rule so its files stop being proactively hydrated. Does NOT itself drop already-cached
    /// bytes — call [`dehydrate_subtree`](Self::dehydrate_subtree) to reclaim that space.
    /// Returns the node's inode. Errors `NotFound` if the `node_id` isn't in the local tree.
    pub fn unsubscribe(&self, node_id: &str) -> Result<u64> {
        self.set_subscription(node_id, SubRule::Exclude)
    }

    /// Record (or replace) an explicit rule for a node's subtree.
    pub fn set_subscription(&self, node_id: &str, rule: SubRule) -> Result<u64> {
        let conn = self.conn_lock();
        let ino: Option<i64> = conn
            .query_row("SELECT ino FROM nodes WHERE node_id = ?1", params![node_id], |r| r.get(0))
            .optional()?;
        let ino = ino.ok_or(FsError::NotFound)?;
        conn.execute(
            "INSERT INTO subscriptions(node_id, rule) VALUES (?1, ?2)
             ON CONFLICT(node_id) DO UPDATE SET rule = excluded.rule",
            params![node_id, rule.as_str()],
        )?;
        Ok(ino as u64)
    }

    /// Remove an explicit rule for a node, so it falls back to whatever its ancestry /
    /// the default policy says. Idempotent (a no-op if no rule was set).
    pub fn clear_subscription(&self, node_id: &str) -> Result<()> {
        let conn = self.conn_lock();
        conn.execute("DELETE FROM subscriptions WHERE node_id = ?1", params![node_id])?;
        Ok(())
    }

    /// Every explicit rule set on this device, as `(node_id, rule)`. (For the app to render
    /// the current selective-sync configuration.)
    pub fn subscription_rules(&self) -> Result<Vec<(String, SubRule)>> {
        let conn = self.conn_lock();
        let mut stmt = conn.prepare("SELECT node_id, rule FROM subscriptions")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, SubRule::from_str(&r.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// **The proactive-hydration gate.** Is this node (by `node_id`) in scope for proactive
    /// hydration on this device? Walks the node up its ancestry to the nearest node carrying
    /// an explicit rule; that rule decides. With no rule up to the root, the
    /// [`subscription_default`](Self::subscription_default) decides.
    ///
    /// This is the single query the sync engine consults before *proactively* hydrating a
    /// pulled file. It does **not** gate on-demand hydration (an actual open/read) — that
    /// path ignores subscriptions so an out-of-scope file still opens, degrading gracefully.
    ///
    /// An unknown `node_id` (not in the local tree) returns the default policy rather than
    /// erroring — the caller is asking a yes/no policy question, not asserting existence.
    pub fn proactively_hydrate(&self, node_id: &str) -> Result<bool> {
        Ok(self.scope_rule(node_id)? == SubRule::Include)
    }

    /// Like [`proactively_hydrate`](Self::proactively_hydrate) but keyed by inode (the seam
    /// the FUSE/CfAPI adapters hold — they think in inodes). Same ancestry walk.
    pub fn proactively_hydrate_ino(&self, ino: u64) -> Result<bool> {
        Ok(self.scope_rule_by_ino(ino)? == SubRule::Include)
    }

    /// Resolve the effective rule for a node by `node_id` (the ancestry walk).
    pub fn scope_rule(&self, node_id: &str) -> Result<SubRule> {
        let conn = self.conn_lock();
        let ino: Option<i64> = conn
            .query_row("SELECT ino FROM nodes WHERE node_id = ?1", params![node_id], |r| r.get(0))
            .optional()?;
        let dflt = self.default_rule(&conn)?;
        match ino {
            Some(i) => Self::scope_rule_walk(&conn, i, dflt),
            None => Ok(dflt),
        }
    }

    fn scope_rule_by_ino(&self, ino: u64) -> Result<SubRule> {
        let conn = self.conn_lock();
        let dflt = self.default_rule(&conn)?;
        Self::scope_rule_walk(&conn, ino as i64, dflt)
    }

    fn default_rule(&self, conn: &Connection) -> Result<SubRule> {
        let v: Option<i64> = conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![DEFAULT_POLICY_KEY],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match v {
            Some(1) => SubRule::Exclude,
            _ => SubRule::Include,
        })
    }

    /// Walk `ino` up the tree, returning the rule of the nearest ancestor (or self) that
    /// carries one, else `default`. Implemented as a single recursive CTE that climbs
    /// `parent_ino` and joins `subscriptions`, taking the deepest (closest) match.
    fn scope_rule_walk(conn: &Connection, ino: i64, default: SubRule) -> Result<SubRule> {
        // depth 0 = the node itself; increasing depth = further toward root. We want the
        // MINIMUM depth that has a rule (the closest ancestor wins).
        let rule: Option<String> = conn
            .query_row(
                "WITH RECURSIVE anc(ino, depth) AS (
                     SELECT ino, 0 FROM nodes WHERE ino = ?1
                     UNION ALL
                     SELECT n.parent_ino, anc.depth + 1
                       FROM nodes n JOIN anc ON n.ino = anc.ino
                      WHERE n.parent_ino IS NOT NULL
                 )
                 SELECT s.rule
                   FROM anc
                   JOIN nodes n ON n.ino = anc.ino
                   JOIN subscriptions s ON s.node_id = n.node_id
                  ORDER BY anc.depth ASC
                  LIMIT 1",
                params![ino],
                |r| r.get(0),
            )
            .optional()?;
        Ok(rule.map(|r| SubRule::from_str(&r)).unwrap_or(default))
    }

    /// **Dehydrate an out-of-scope subtree to reclaim disk** — drop the locally cached chunk
    /// bytes for every file under `node_id`, EXCEPT any chunk still referenced by a file that
    /// remains in scope (chunks are content-addressed + deduped, so a shared chunk must not be
    /// dropped out from under an in-scope file). The tree, manifests, and placeholders are
    /// left intact — only the cached bytes go, so the files become placeholders that
    /// re-hydrate on demand. Returns the number of chunks actually dropped.
    ///
    /// Typically called by the engine right after [`unsubscribe`](Self::unsubscribe) to
    /// reclaim the space a now-unsubscribed (formerly hydrated) subtree was holding.
    pub fn dehydrate_subtree(&self, node_id: &str) -> Result<usize> {
        // 1. Collect the chunk hashes belonging to the subtree's files.
        let subtree_hashes = self.subtree_chunk_hashes(node_id)?;
        if subtree_hashes.is_empty() {
            return Ok(0);
        }
        // 2. Collect the chunk hashes still referenced by files OUTSIDE this subtree — those
        //    are off-limits (a shared/deduped chunk an in-scope file depends on).
        let keep = self.chunk_hashes_outside(node_id)?;
        // 3. Drop the bytes for subtree-only chunks.
        let mut dropped = 0;
        for h in &subtree_hashes {
            if keep.contains(h) {
                continue;
            }
            if self.store.has(h) {
                self.store.delete(h).map_err(FsError::Io)?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// The set of chunk hashes referenced by all file manifests within `node_id`'s subtree
    /// (the subtree root included).
    fn subtree_chunk_hashes(&self, node_id: &str) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn_lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT mc.chunk_hash
               FROM nodes n
               JOIN manifest_chunks mc ON mc.manifest_id = n.manifest_id
              WHERE n.ino IN (
                  WITH RECURSIVE sub(ino) AS (
                      SELECT ino FROM nodes WHERE node_id = ?1
                      UNION ALL
                      SELECT c.ino FROM nodes c JOIN sub ON c.parent_ino = sub.ino
                  )
                  SELECT ino FROM sub
              )",
        )?;
        let rows = stmt.query_map(params![node_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The set of chunk hashes referenced by every file manifest OUTSIDE `node_id`'s subtree
    /// — the chunks that must be kept even when dehydrating that subtree (dedup safety).
    fn chunk_hashes_outside(&self, node_id: &str) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn_lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT mc.chunk_hash
               FROM nodes n
               JOIN manifest_chunks mc ON mc.manifest_id = n.manifest_id
              WHERE n.ino NOT IN (
                  WITH RECURSIVE sub(ino) AS (
                      SELECT ino FROM nodes WHERE node_id = ?1
                      UNION ALL
                      SELECT c.ino FROM nodes c JOIN sub ON c.parent_ino = sub.ino
                  )
                  SELECT ino FROM sub
              )",
        )?;
        let rows = stmt.query_map(params![node_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The in-scope file `node_id`s whose chunks are NOT all cached locally — i.e. the
    /// proactive-hydration work list after a pull. The engine iterates this and hydrates each.
    /// Skips directories and files already fully hydrated. Honors the subscription gate so
    /// out-of-scope files never appear here.
    pub fn proactive_hydration_targets(&self) -> Result<Vec<String>> {
        // Pull the node_id of every file with a manifest, then filter in Rust by the gate +
        // cache state (the ancestry walk + chunk-store lookups don't compose cleanly into one
        // SQL statement).
        let files: Vec<String> = {
            let conn = self.conn_lock();
            let mut stmt = conn.prepare(
                "SELECT node_id FROM nodes
                  WHERE kind = 'file' AND manifest_id IS NOT NULL AND node_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut out = Vec::new();
        for node_id in files {
            if !self.proactively_hydrate(&node_id)? {
                continue; // out of scope — leave as a placeholder, hydrate on demand only
            }
            let hashes = self.node_manifest_hashes(&node_id)?;
            if hashes.iter().any(|h| !self.store.has(h)) {
                out.push(node_id);
            }
        }
        Ok(out)
    }

    /// Small accessor so the subscription queries can share the connection lock without
    /// duplicating the `.lock().unwrap()` boilerplate.
    fn conn_lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

/// `ROOT_INO` re-export guard so the module compiles standalone if the constant moves.
#[allow(unused)]
const _: u64 = ROOT_INO;
