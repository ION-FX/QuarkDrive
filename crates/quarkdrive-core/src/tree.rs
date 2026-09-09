//! The Merkle tree model: files, directories, and snapshots.
//!
//! A vault's contents are a tree of *nodes*, and every node is itself an
//! object stored under its own content hash. A directory node holds the ids of
//! its children rather than their contents, so the tree is a Merkle tree.
//!
//! That single property is what makes syncing cheap. Two devices comparing
//! trees can walk them in lockstep and stop descending the moment two
//! subtrees hash equal — so discovering "one file changed in a directory of
//! ten thousand" costs a handful of small fetches instead of listing the
//! world.

use crate::hash::ObjectId;
use crate::object::ObjectStore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// One content-defined slice of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub id: ObjectId,
    pub len: u32,
}

/// A pointer to a child node, carrying enough metadata to render a directory
/// listing without loading the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    pub id: ObjectId,
    pub kind: Kind,
    /// File size in bytes; 0 for directories and symlinks.
    pub size: u64,
    /// Modification time, whole seconds since the Unix epoch.
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Node {
    File {
        size: u64,
        mtime: i64,
        mtime_nsec: u32,
        /// Unix permission bits.
        mode: u32,
        chunks: Vec<ChunkRef>,
    },
    Dir {
        entries: BTreeMap<String, NodeRef>,
    },
    Symlink {
        target: String,
        mtime: i64,
        mtime_nsec: u32,
    },
}

impl Node {
    pub fn kind(&self) -> Kind {
        match self {
            Node::File { .. } => Kind::File,
            Node::Dir { .. } => Kind::Dir,
            Node::Symlink { .. } => Kind::Symlink,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Node::File { size, .. } => *size,
            _ => 0,
        }
    }

    pub fn mtime(&self) -> i64 {
        match self {
            Node::File { mtime, .. } => *mtime,
            Node::Symlink { mtime, .. } => *mtime,
            Node::Dir { .. } => 0,
        }
    }

    pub fn as_dir(&self) -> Option<&BTreeMap<String, NodeRef>> {
        match self {
            Node::Dir { entries } => Some(entries),
            _ => None,
        }
    }

    pub fn as_file(&self) -> Option<&Vec<ChunkRef>> {
        match self {
            Node::File { chunks, .. } => Some(chunks),
            _ => None,
        }
    }

    /// A compact reference suitable for a parent directory's entry list.
    pub fn to_ref(&self, id: ObjectId) -> NodeRef {
        NodeRef {
            id,
            kind: self.kind(),
            size: self.size(),
            mtime: self.mtime(),
        }
    }
}

/// Canonical encoding of a node. Must be byte-stable across platforms and
/// versions, because the hash of these bytes *is* the node's identity.
pub fn encode_node(node: &Node) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(node)?)
}

pub fn decode_node(bytes: &[u8]) -> anyhow::Result<Node> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Read/write access to the node tree inside an object store.
pub struct TreeSet<'a> {
    store: &'a ObjectStore,
}

impl<'a> TreeSet<'a> {
    pub fn new(store: &'a ObjectStore) -> Self {
        TreeSet { store }
    }

    pub fn store(&self) -> &'a ObjectStore {
        self.store
    }

    /// Store a node and return its id.
    pub fn put_node(&self, node: &Node) -> anyhow::Result<ObjectId> {
        let bytes = encode_node(node)?;
        let (id, _is_new) = self.store.put(&bytes)?;
        Ok(id)
    }

    pub fn get_node(&self, id: &ObjectId) -> anyhow::Result<Option<Node>> {
        match self.store.get(id)? {
            Some(bytes) => Ok(Some(decode_node(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Resolve a slash-separated path below `root`.
    pub fn lookup(&self, root: &ObjectId, path: &str) -> anyhow::Result<Option<Node>> {
        let mut cur = *root;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let node = match self.get_node(&cur)? {
                Some(n) => n,
                None => return Ok(None),
            };
            let entries = match node.as_dir() {
                Some(e) => e,
                None => return Ok(None), // path descends through a non-directory
            };
            cur = match entries.get(segment) {
                Some(r) => r.id,
                None => return Ok(None),
            };
        }
        self.get_node(&cur)
    }

    /// Every node and chunk object reachable from `root`.
    ///
    /// Used to work out what a peer is missing: send this set, the peer says
    /// which ids it already has, and only the remainder is transferred.
    pub fn collect_objects(&self, root: &ObjectId) -> anyhow::Result<BTreeSet<ObjectId>> {
        let mut out = BTreeSet::new();
        let mut stack = vec![*root];
        while let Some(id) = stack.pop() {
            if !out.insert(id) {
                continue;
            }
            let node = match self.get_node(&id)? {
                Some(n) => n,
                None => anyhow::bail!("missing node object {}", id),
            };
            match node {
                Node::File { chunks, .. } => {
                    for c in chunks {
                        out.insert(c.id);
                    }
                }
                Node::Dir { entries } => {
                    for r in entries.values() {
                        stack.push(r.id);
                    }
                }
                Node::Symlink { .. } => {}
            }
        }
        Ok(out)
    }

    /// Flatten the tree into a sorted list of paths.
    pub fn walk(&self, root: &ObjectId) -> anyhow::Result<Vec<(String, NodeRef)>> {
        let mut out = Vec::new();
        let mut stack: Vec<(String, ObjectId)> = vec![(String::new(), *root)];
        while let Some((prefix, id)) = stack.pop() {
            let node = match self.get_node(&id)? {
                Some(n) => n,
                None => anyhow::bail!("missing node object {}", id),
            };
            if let Node::Dir { entries } = node {
                for (name, r) in entries.iter() {
                    let path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    out.push((path.clone(), *r));
                    if r.kind == Kind::Dir {
                        stack.push((path, r.id));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Reassemble a file's bytes from its chunks.
    pub fn read_file(&self, node: &Node) -> anyhow::Result<Vec<u8>> {
        let chunks = match node.as_file() {
            Some(c) => c,
            None => anyhow::bail!("not a file node"),
        };
        let size = node.size();
        let mut out = Vec::with_capacity(size as usize);
        for c in chunks {
            let bytes = self
                .store
                .get(&c.id)?
                .ok_or_else(|| anyhow::anyhow!("missing chunk {}", c.id))?;
            if bytes.len() != c.len as usize {
                anyhow::bail!(
                    "chunk {} is {} bytes, expected {}",
                    c.id,
                    bytes.len(),
                    c.len
                );
            }
            out.extend_from_slice(&bytes);
        }
        if out.len() as u64 != size {
            anyhow::bail!("file is {} bytes, expected {}", out.len(), size);
        }
        Ok(out)
    }

    /// Sum of file sizes below `root`.
    pub fn total_size(&self, root: &ObjectId) -> anyhow::Result<u64> {
        let mut total = 0u64;
        for (_path, r) in self.walk(root)? {
            if r.kind == Kind::File {
                total += r.size;
            }
        }
        Ok(total)
    }
}

/// A committed view of a vault at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Root directory node.
    pub root: ObjectId,
    /// Device that produced this snapshot.
    pub device: String,
    pub host: String,
    /// Whole seconds since the Unix epoch.
    pub time: i64,
    /// Snapshot this one was built from, if any. Drives optimistic
    /// concurrency: a commit is rejected if the server's head has moved on.
    pub parent: Option<ObjectId>,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

impl Snapshot {
    /// An empty snapshot, used as the ancestor of a vault's first commit.
    pub fn empty_root(trees: &TreeSet) -> anyhow::Result<ObjectId> {
        trees.put_node(&Node::Dir {
            entries: BTreeMap::new(),
        })
    }
}

impl<'a> TreeSet<'a> {
    pub fn put_snapshot(&self, snap: &Snapshot) -> anyhow::Result<ObjectId> {
        let bytes = serde_json::to_vec(snap)?;
        let (id, _) = self.store.put(&bytes)?;
        Ok(id)
    }

    pub fn get_snapshot(&self, id: &ObjectId) -> anyhow::Result<Option<Snapshot>> {
        match self.store.get(id)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Key;
    use crate::testutil::TempDir;

    fn file_node(store: &ObjectStore, data: &[u8], mtime: i64) -> ObjectId {
        let (chunk_id, _) = store.put(data).unwrap();
        let node = Node::File {
            size: data.len() as u64,
            mtime,
            mtime_nsec: 0,
            mode: 0o644,
            chunks: vec![ChunkRef {
                id: chunk_id,
                len: data.len() as u32,
            }],
        };
        let trees = TreeSet::new(store);
        trees.put_node(&node).unwrap()
    }

    #[test]
    fn node_round_trips() {
        let dir = TempDir::new("tree-roundtrip");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let id = file_node(&store, b"file contents", 100);
        let node = trees.get_node(&id).unwrap().unwrap();
        assert_eq!(node.kind(), Kind::File);
        assert_eq!(node.size(), 13);
        assert_eq!(trees.read_file(&node).unwrap(), b"file contents");
    }

    /// Canonicalisation: a directory's identity must not depend on the order
    /// its entries were inserted, or two devices would disagree about whether
    /// anything changed.
    #[test]
    fn directory_id_is_insertion_order_independent() {
        let dir = TempDir::new("tree-canonical");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let a = file_node(&store, b"a", 1);
        let b = file_node(&store, b"b", 2);
        let c = file_node(&store, b"c", 3);

        let id1 = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([
                    ("a".into(), NodeRef { id: a, kind: Kind::File, size: 1, mtime: 1 }),
                    ("b".into(), NodeRef { id: b, kind: Kind::File, size: 1, mtime: 2 }),
                ]),
            })
            .unwrap();
        let id2 = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([
                    ("b".into(), NodeRef { id: b, kind: Kind::File, size: 1, mtime: 2 }),
                    ("a".into(), NodeRef { id: a, kind: Kind::File, size: 1, mtime: 1 }),
                ]),
            })
            .unwrap();
        assert_eq!(id1, id2);
        let _ = c;
    }

    /// The property the whole design rests on.
    #[test]
    fn deep_change_perturbs_only_ancestors() {
        let dir = TempDir::new("tree-merkle");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);

        let unchanged = file_node(&store, b"untouched", 1);
        let before = file_node(&store, b"old content", 1);
        let after = file_node(&store, b"new content", 2);

        let left_before = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("f".into(), NodeRef { id: before, kind: Kind::File, size: 11, mtime: 1 })]),
            })
            .unwrap();
        let left_after = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("f".into(), NodeRef { id: after, kind: Kind::File, size: 11, mtime: 2 })]),
            })
            .unwrap();
        let right = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("g".into(), NodeRef { id: unchanged, kind: Kind::File, size: 9, mtime: 1 })]),
            })
            .unwrap();

        let root_before = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([
                    ("left".into(), NodeRef { id: left_before, kind: Kind::Dir, size: 0, mtime: 0 }),
                    ("right".into(), NodeRef { id: right, kind: Kind::Dir, size: 0, mtime: 0 }),
                ]),
            })
            .unwrap();
        let root_after = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([
                    ("left".into(), NodeRef { id: left_after, kind: Kind::Dir, size: 0, mtime: 0 }),
                    ("right".into(), NodeRef { id: right, kind: Kind::Dir, size: 0, mtime: 0 }),
                ]),
            })
            .unwrap();

        assert_ne!(root_before, root_after, "root must change");
        assert_ne!(left_before, left_after, "parent must change");

        // The untouched sibling subtree is bit-identical, so a syncing peer
        // that already has it transfers nothing for it.
        let root_node = trees.get_node(&root_after).unwrap().unwrap();
        let entries = root_node.as_dir().unwrap();
        assert_eq!(entries["right"].id, right);
    }

    #[test]
    fn lookup_resolves_nested_paths() {
        let dir = TempDir::new("tree-lookup");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let f = file_node(&store, b"deep", 5);
        let inner = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("x".into(), NodeRef { id: f, kind: Kind::File, size: 4, mtime: 5 })]),
            })
            .unwrap();
        let root = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("a".into(), NodeRef { id: inner, kind: Kind::Dir, size: 0, mtime: 0 })]),
            })
            .unwrap();

        assert_eq!(trees.lookup(&root, "a/x").unwrap().unwrap().size(), 4);
        assert_eq!(trees.lookup(&root, "/a/x/").unwrap().unwrap().size(), 4);
        assert!(trees.lookup(&root, "a/nope").unwrap().is_none());
        assert!(trees.lookup(&root, "nope/x").unwrap().is_none());
        // Descending through a file must not panic.
        assert!(trees.lookup(&root, "a/x/deeper").unwrap().is_none());
        assert_eq!(trees.lookup(&root, "").unwrap().unwrap().kind(), Kind::Dir);
    }

    #[test]
    fn walk_lists_everything_in_order() {
        let dir = TempDir::new("tree-walk");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let f1 = file_node(&store, b"one", 1);
        let f2 = file_node(&store, b"two", 2);
        let sub = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("b.txt".into(), NodeRef { id: f2, kind: Kind::File, size: 3, mtime: 2 })]),
            })
            .unwrap();
        let root = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([
                    ("a.txt".into(), NodeRef { id: f1, kind: Kind::File, size: 3, mtime: 1 }),
                    ("sub".into(), NodeRef { id: sub, kind: Kind::Dir, size: 0, mtime: 0 }),
                ]),
            })
            .unwrap();

        let listing = trees.walk(&root).unwrap();
        let paths: Vec<&str> = listing.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);
    }

    #[test]
    fn collect_objects_includes_chunks_and_nodes() {
        let dir = TempDir::new("tree-collect");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let (chunk, _) = store.put(b"payload").unwrap();
        let node = Node::File {
            size: 7,
            mtime: 1,
            mtime_nsec: 0,
            mode: 0o644,
            chunks: vec![ChunkRef { id: chunk, len: 7 }],
        };
        let root = trees.put_node(&node).unwrap();
        let objects = trees.collect_objects(&root).unwrap();
        assert!(objects.contains(&chunk));
        assert!(objects.contains(&root));
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn read_file_detects_missing_chunks() {
        let dir = TempDir::new("tree-readfile");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let node = Node::File {
            size: 5,
            mtime: 1,
            mtime_nsec: 0,
            mode: 0o644,
            chunks: vec![ChunkRef { id: ObjectId::hash(b"gone"), len: 5 }],
        };
        assert!(trees.read_file(&node).is_err());
        assert!(trees.read_file(&Node::Dir { entries: BTreeMap::new() }).is_err());
    }

    #[test]
    fn snapshot_round_trips() {
        let dir = TempDir::new("tree-snapshot");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let trees = TreeSet::new(&store);
        let root = Snapshot::empty_root(&trees).unwrap();
        let snap = Snapshot {
            root,
            device: "laptop".into(),
            host: "hostname".into(),
            time: 1_700_000_000,
            parent: None,
            files: 3,
            dirs: 1,
            bytes: 42,
        };
        let id = trees.put_snapshot(&snap).unwrap();
        assert_eq!(trees.get_snapshot(&id).unwrap().unwrap(), snap);
        assert!(trees.get_snapshot(&ObjectId::hash(b"nope")).unwrap().is_none());
    }

    #[test]
    fn works_under_encryption() {
        let dir = TempDir::new("tree-encrypted");
        let store = ObjectStore::open(dir.path(), Some(Key::generate())).unwrap();
        let trees = TreeSet::new(&store);
        let f = file_node(&store, b"secret file", 9);
        let root = trees
            .put_node(&Node::Dir {
                entries: BTreeMap::from([("s".into(), NodeRef { id: f, kind: Kind::File, size: 11, mtime: 9 })]),
            })
            .unwrap();
        let node = trees.get_node(&root).unwrap().unwrap();
        assert_eq!(node.as_dir().unwrap().len(), 1);
        assert_eq!(trees.read_file(&trees.lookup(&root, "s").unwrap().unwrap()).unwrap(), b"secret file");
    }
}
