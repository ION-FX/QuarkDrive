//! Comparing two trees.
//!
//! `diff` reports what changed between two tree roots. Because the tree is a
//! Merkle tree, subtrees whose ids match are skipped without being read, so
//! diffing two large vaults that differ in one file costs a handful of object
//! reads rather than a full traversal.
//!
//! This drives `qd status`, and its logic is the basis of the three-way merge
//! in [`crate::sync`].

use crate::hash::ObjectId;
use crate::tree::{Kind, Node, NodeRef, TreeSet};
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { path: String, kind: Kind, size: u64 },
    Removed { path: String, kind: Kind },
    Modified { path: String, kind: Kind, size: u64 },
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Added { path, .. } => path,
            Change::Removed { path, .. } => path,
            Change::Modified { path, .. } => path,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Change::Added { kind, .. } => *kind,
            Change::Removed { kind, .. } => *kind,
            Change::Modified { kind, .. } => *kind,
        }
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Child entries of a directory node; empty if `id` is absent or is not a
/// directory.
fn dir_entries(trees: &TreeSet, id: Option<ObjectId>) -> Result<BTreeMap<String, NodeRef>> {
    let Some(id) = id else {
        return Ok(BTreeMap::new());
    };
    match trees.get_node(&id)? {
        Some(Node::Dir { entries }) => Ok(entries),
        _ => Ok(BTreeMap::new()),
    }
}

/// What changed between `old` and `new`. Pass `None` for a missing side.
///
/// The comparison starts one level down, at the roots' children, so a change
/// is never reported against the empty root path.
pub fn diff(trees: &TreeSet, old: Option<ObjectId>, new: Option<ObjectId>) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    let old_entries = dir_entries(trees, old)?;
    let new_entries = dir_entries(trees, new)?;

    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(old_entries.keys());
    names.extend(new_entries.keys());

    for name in names {
        diff_ref(
            trees,
            name,
            old_entries.get(name).copied(),
            new_entries.get(name).copied(),
            &mut out,
        )?;
    }
    out.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(out)
}

fn diff_ref(
    trees: &TreeSet,
    prefix: &str,
    old: Option<NodeRef>,
    new: Option<NodeRef>,
    out: &mut Vec<Change>,
) -> Result<()> {
    match (old, new) {
        (Some(o), Some(n)) if o.id == n.id => return Ok(()),
        (Some(o), None) => return emit_removed(trees, prefix, o, out),
        (None, Some(n)) => return emit_added(trees, prefix, n, out),
        (None, None) => return Ok(()),
        _ => {}
    }

    let (o, n) = (old.unwrap(), new.unwrap());
    let o_node = trees.get_node(&o.id)?;
    let n_node = trees.get_node(&n.id)?;
    let o_entries = o_node.as_ref().and_then(|x| x.as_dir());
    let n_entries = n_node.as_ref().and_then(|x| x.as_dir());

    match (o_entries, n_entries) {
        (Some(oe), Some(ne)) => {
            let mut names: BTreeSet<&String> = BTreeSet::new();
            names.extend(oe.keys());
            names.extend(ne.keys());
            for name in names {
                diff_ref(
                    trees,
                    &join(prefix, name),
                    oe.get(name).copied(),
                    ne.get(name).copied(),
                    out,
                )?;
            }
            Ok(())
        }
        // A file whose bytes changed, or a path that changed type.
        _ => {
            out.push(Change::Modified {
                path: prefix.to_string(),
                kind: n.kind,
                size: n.size,
            });
            Ok(())
        }
    }
}

fn emit_added(trees: &TreeSet, prefix: &str, r: NodeRef, out: &mut Vec<Change>) -> Result<()> {
    out.push(Change::Added {
        path: prefix.to_string(),
        kind: r.kind,
        size: r.size,
    });
    if r.kind == Kind::Dir {
        let node = trees
            .get_node(&r.id)?
            .ok_or_else(|| anyhow!("missing node {}", r.id))?;
        if let Some(entries) = node.as_dir() {
            for (name, child) in entries {
                emit_added(trees, &join(prefix, name), *child, out)?;
            }
        }
    }
    Ok(())
}

fn emit_removed(trees: &TreeSet, prefix: &str, r: NodeRef, out: &mut Vec<Change>) -> Result<()> {
    out.push(Change::Removed {
        path: prefix.to_string(),
        kind: r.kind,
    });
    if r.kind == Kind::Dir {
        let node = trees
            .get_node(&r.id)?
            .ok_or_else(|| anyhow!("missing node {}", r.id))?;
        if let Some(entries) = node.as_dir() {
            for (name, child) in entries {
                emit_removed(trees, &join(prefix, name), *child, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectStore;
    use crate::testutil::TempDir;
    use crate::tree::{ChunkRef, Node};
    use std::collections::BTreeMap;

    struct Fixture {
        _dir: TempDir,
        store: ObjectStore,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = TempDir::new(tag);
            let store = ObjectStore::open(dir.path(), None).unwrap();
            Fixture { _dir: dir, store }
        }

        fn trees(&self) -> TreeSet {
            TreeSet::new(&self.store)
        }

        fn file(&self, data: &[u8], mtime: i64) -> ObjectId {
            let (chunk, _) = self.store.put(data).unwrap();
            self.trees()
                .put_node(&Node::File {
                    size: data.len() as u64,
                    mtime,
                    mtime_nsec: 0,
                    mode: 0o644,
                    chunks: vec![ChunkRef {
                        id: chunk,
                        len: data.len() as u32,
                    }],
                })
                .unwrap()
        }

        /// Build a directory node, deriving each child's `NodeRef` from the
        /// child node itself so kinds and sizes are faithful.
        fn dir(&self, entries: Vec<(&str, ObjectId)>) -> ObjectId {
            let mut map = BTreeMap::new();
            for (name, id) in entries {
                let node = self.trees().get_node(&id).unwrap().unwrap();
                map.insert(name.to_string(), node.to_ref(id));
            }
            self.trees().put_node(&Node::Dir { entries: map }).unwrap()
        }
    }

    #[test]
    fn identical_trees_produce_no_changes() {
        let f = Fixture::new("diff-same");
        let trees = f.trees();
        let a = f.file(b"x", 1);
        let root = f.dir(vec![("a", a)]);
        assert!(diff(&trees, Some(root), Some(root)).unwrap().is_empty());
    }

    #[test]
    fn detects_added_file() {
        let f = Fixture::new("diff-add");
        let trees = f.trees();
        let a = f.file(b"x", 1);
        let b = f.file(b"y", 2);
        let root1 = f.dir(vec![("a", a)]);
        let root2 = f.dir(vec![("a", a), ("b", b)]);
        let changes = diff(&trees, Some(root1), Some(root2)).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "b");
        assert!(matches!(changes[0], Change::Added { .. }));
    }

    #[test]
    fn detects_removed_file() {
        let f = Fixture::new("diff-remove");
        let trees = f.trees();
        let a = f.file(b"x", 1);
        let root1 = f.dir(vec![("a", a)]);
        let root2 = f.dir(vec![]);
        let changes = diff(&trees, Some(root1), Some(root2)).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], Change::Removed { .. }));
    }

    #[test]
    fn detects_modified_file() {
        let f = Fixture::new("diff-modify");
        let trees = f.trees();
        let a1 = f.file(b"before", 1);
        let a2 = f.file(b"after", 2);
        let root1 = f.dir(vec![("a", a1)]);
        let root2 = f.dir(vec![("a", a2)]);
        let changes = diff(&trees, Some(root1), Some(root2)).unwrap();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Modified { path, size, .. } => {
                assert_eq!(path, "a");
                assert_eq!(*size, 5, "reported size must come from the node");
            }
            other => panic!("expected Modified, got {other:?}"),
        }
    }

    /// Unchanged siblings must not appear, even though their parent changed.
    #[test]
    fn sibling_changes_do_not_leak_into_unchanged_subtrees() {
        let f = Fixture::new("diff-sibling");
        let trees = f.trees();
        let keep = f.file(b"untouched", 1);
        let old = f.file(b"before", 1);
        let new = f.file(b"after", 2);

        let sub = f.dir(vec![("keep", keep)]);
        let root1 = f.dir(vec![("sub", sub), ("changed", old)]);
        let root2 = f.dir(vec![("sub", sub), ("changed", new)]);

        let changes = diff(&trees, Some(root1), Some(root2)).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path()).collect();
        assert_eq!(paths, vec!["changed"]);
    }

    #[test]
    fn removing_a_directory_lists_all_descendants() {
        let f = Fixture::new("diff-rmdir");
        let trees = f.trees();
        let one = f.file(b"1", 1);
        let two = f.file(b"2", 1);
        let inner = f.dir(vec![("two", two)]);
        let outer = f.dir(vec![("one", one), ("inner", inner)]);
        let empty = f.dir(vec![]);
        let changes = diff(&trees, Some(outer), Some(empty)).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path()).collect();
        assert_eq!(paths, vec!["inner", "inner/two", "one"]);
    }

    #[test]
    fn adding_a_directory_lists_all_descendants() {
        let f = Fixture::new("diff-adddir");
        let trees = f.trees();
        let one = f.file(b"1", 1);
        let two = f.file(b"2", 1);
        let inner = f.dir(vec![("two", two)]);
        let outer = f.dir(vec![("one", one), ("inner", inner)]);
        let empty = f.dir(vec![]);
        let changes = diff(&trees, Some(empty), Some(outer)).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path()).collect();
        assert_eq!(paths, vec!["inner", "inner/two", "one"]);
    }

    #[test]
    fn type_change_is_reported_as_modified() {
        let f = Fixture::new("diff-type");
        let trees = f.trees();
        let file = f.file(b"data", 1);
        let as_dir = f.dir(vec![("nested", file)]);
        let root1 = f.dir(vec![("x", file)]);
        let root2 = f.dir(vec![("x", as_dir)]);
        let changes = diff(&trees, Some(root1), Some(root2)).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], Change::Modified { .. }));
    }

    /// The root itself is not reported as changed; only real paths are.
    #[test]
    fn diff_against_nothing_reports_every_path_once() {
        let f = Fixture::new("diff-from-empty");
        let trees = f.trees();
        let a = f.file(b"x", 1);
        let root = f.dir(vec![("a", a)]);
        let changes = diff(&trees, None, Some(root)).unwrap();
        assert_eq!(changes.len(), 1, "the root must not be reported as a path");
        assert_eq!(changes[0].path(), "a");
        assert!(matches!(changes[0], Change::Added { .. }));

        let changes = diff(&trees, Some(root), None).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], Change::Removed { .. }));
    }
}
