//! The pull-apply algorithm: fold a stream of changelog entries into a local tree.
//!
//! This is exactly what a device does to its local SQLite on `GET /v1/changes` —
//! here in a pure, in-memory form so it's testable without a database. The real
//! `fs-provider` apply mirrors this logic against its tables.

use std::collections::HashMap;

use crate::{Change, ChunkRef, TreeNode};

#[derive(Debug, Clone, Default)]
pub struct LocalNode {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub deleted: bool,
    pub version_id: Option<String>,
    pub manifest: Option<Vec<ChunkRef>>,
}

#[derive(Debug, Default)]
pub struct LocalTree {
    pub nodes: HashMap<String, LocalNode>,
    pub cursor: i64,
}

impl LocalTree {
    /// Seed from a full-tree snapshot (`GET /v1/tree`).
    pub fn from_snapshot(nodes: &[TreeNode], head_seq: i64) -> Self {
        let mut t = Self {
            cursor: head_seq,
            ..Default::default()
        };
        for n in nodes {
            t.nodes.insert(
                n.node_id.clone(),
                LocalNode {
                    node_id: n.node_id.clone(),
                    parent_node_id: n.parent_node_id.clone(),
                    name: n.name.clone(),
                    kind: n.kind.clone(),
                    deleted: false,
                    version_id: n.version_id.clone(),
                    manifest: n.manifest.clone(),
                },
            );
        }
        t
    }

    /// Apply one changelog entry (idempotent; ordered by seq).
    pub fn apply(&mut self, change: &Change) {
        let node = self.nodes.entry(change.node_id.clone()).or_default();
        node.node_id = change.node_id.clone();
        if let Some(view) = &change.node {
            node.parent_node_id = view.parent_node_id.clone();
            node.name = view.name.clone();
            node.kind = view.kind.clone();
            node.deleted = view.deleted;
        }
        if change.op == "put" {
            node.manifest = change.manifest.clone();
            node.version_id = change.version.as_ref().map(|v| v.version_id.clone());
        }
        if change.seq > self.cursor {
            self.cursor = change.seq;
        }
    }

    pub fn apply_all(&mut self, changes: &[Change]) {
        for c in changes {
            self.apply(c);
        }
    }

    pub fn live(&self) -> impl Iterator<Item = &LocalNode> {
        self.nodes.values().filter(|n| !n.deleted)
    }

    pub fn get(&self, node_id: &str) -> Option<&LocalNode> {
        self.nodes.get(node_id).filter(|n| !n.deleted)
    }

    /// Build the slash-path of a node by walking parents. A malformed tree with a parent
    /// cycle (e.g. a corrupt/adversarial changelog making A→B→A) would otherwise loop
    /// forever; a visited-set guard breaks the cycle and returns `None` instead of hanging.
    pub fn path_of(&self, node_id: &str) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = node_id.to_string();
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(cur.clone()) {
                return None; // parent cycle — bail rather than spin forever
            }
            let n = self.nodes.get(&cur)?;
            parts.push(n.name.clone());
            match &n.parent_node_id {
                Some(p) => cur = p.clone(),
                None => break,
            }
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeView, VersionView};

    fn put(seq: i64, node_id: &str, parent: Option<&str>, name: &str) -> Change {
        Change {
            seq,
            op: "put".into(),
            node_id: node_id.into(),
            node: Some(NodeView {
                node_id: node_id.into(),
                parent_node_id: parent.map(String::from),
                name: name.into(),
                kind: "file".into(),
                deleted: false,
                mode: Some(0o644),
                mtime: Some(0),
                symlink_target: None,
            }),
            manifest: Some(vec![ChunkRef {
                hash: "h".into(),
                offset: 0,
                length: 1,
            }]),
            version: Some(VersionView {
                version_id: format!("v{seq}"),
                manifest_id: "m".into(),
                size: 1,
            }),
        }
    }

    #[test]
    fn apply_put_then_delete() {
        let mut t = LocalTree::default();
        t.apply(&put(1, "n1", None, "a.txt"));
        assert_eq!(t.cursor, 1);
        assert!(t.get("n1").is_some());
        assert!(t.get("n1").unwrap().manifest.is_some());

        let mut del = put(2, "n1", None, "a.txt");
        del.op = "delete".into();
        del.node.as_mut().unwrap().deleted = true;
        t.apply(&del);
        assert_eq!(t.cursor, 2);
        assert!(
            t.get("n1").is_none(),
            "deleted node is filtered from live view"
        );
    }

    #[test]
    fn path_built_by_walking_parents() {
        let mut t = LocalTree::default();
        let mut dir = put(1, "d", None, "Projects");
        dir.node.as_mut().unwrap().kind = "dir".into();
        t.apply(&dir);
        t.apply(&put(2, "f", Some("d"), "box.lbrn2"));
        assert_eq!(t.path_of("f").as_deref(), Some("/Projects/box.lbrn2"));
    }

    #[test]
    fn path_of_bails_on_parent_cycle_instead_of_hanging() {
        // Adversarial/corrupt tree: a -> b -> a. path_of must terminate (return None), not spin.
        let mut t = LocalTree::default();
        let mut a = put(1, "a", Some("b"), "a");
        a.node.as_mut().unwrap().parent_node_id = Some("b".into());
        let mut b = put(2, "b", Some("a"), "b");
        b.node.as_mut().unwrap().parent_node_id = Some("a".into());
        t.apply(&a);
        t.apply(&b);
        assert_eq!(t.path_of("a"), None);
    }

    #[test]
    fn apply_is_order_idempotent_on_cursor() {
        let mut t = LocalTree::default();
        t.apply(&put(5, "n", None, "x"));
        t.apply(&put(3, "n", None, "x")); // older seq doesn't lower the cursor
        assert_eq!(t.cursor, 5);
    }
}
