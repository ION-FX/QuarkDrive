//! A vault, as the server sees it.
//!
//! The server stores objects exactly as clients send them, so the desktop
//! sync protocol needs almost nothing from here beyond object storage and head
//! management.
//!
//! What it adds is the *file API*: web browsers and phones do not run the
//! Merkle sync engine, so the server must be able to list, read, write and
//! delete paths itself. Every write re-chunks the file with the same chunker
//! the clients use, rewrites the directory nodes above it, and commits a new
//! snapshot — so content uploaded through the web is indistinguishable from
//! content pushed by a desktop client, and stays deduplicated against it.
//!
//! For an end-to-end encrypted vault none of this is possible: the server has
//! no key, so every file operation is refused rather than silently corrupted.

use anyhow::{anyhow, Result};
use std::collections::HashSet;
use quarkdrive_core::chunker::chunk_reader;
use quarkdrive_core::hash::ObjectId;
use quarkdrive_core::object::ObjectStore;
use quarkdrive_core::tree::{ChunkRef, Kind, Node, NodeRef, Snapshot, TreeSet};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::db::VaultRow;

/// One entry in a directory listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
}

pub struct Vault {
    pub row: VaultRow,
    pub dir: PathBuf,
    pub objects: ObjectStore,
}

impl Vault {
    pub fn open(data_dir: &Path, row: VaultRow) -> Result<Self> {
        let dir = data_dir.join("vaults").join(&row.id);
        fs::create_dir_all(dir.join("objects"))?;
        fs::create_dir_all(dir.join("thumbs"))?;
        // The server never holds a vault key: plain vaults are unencrypted,
        // and for encrypted ones holding no key is exactly what makes the
        // encryption end-to-end.
        let objects = ObjectStore::open(dir.join("objects"), None)?;
        Ok(Vault { dir, objects, row })
    }

    pub fn trees(&self) -> TreeSet<'_> {
        TreeSet::new(&self.objects)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Reading or writing files requires a vault the server can decrypt.
    pub fn require_readable(&self) -> Result<()> {
        if self.row.encrypted {
            return Err(anyhow!(
                "vault '{}' is end-to-end encrypted; the file API is unavailable",
                self.row.name
            ));
        }
        Ok(())
    }

    // -------------------------------------------------------------- snapshots

    pub fn head(&self) -> Result<Option<ObjectId>> {
        match fs::read_to_string(self.dir.join("HEAD")) {
            Ok(s) => {
                let s = s.trim();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ObjectId::from_hex(s)?))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_head(&self, id: ObjectId) -> Result<()> {
        fs::write(self.dir.join("HEAD"), id.to_hex())?;
        Ok(())
    }

    pub fn snapshot(&self, id: &ObjectId) -> Result<Option<Snapshot>> {
        self.trees().get_snapshot(id)
    }

    pub fn put_snapshot(&self, snap: &Snapshot) -> Result<ObjectId> {
        self.trees().put_snapshot(snap)
    }

    /// Root of the current head, or an empty directory for a new vault.
    pub fn root(&self) -> Result<ObjectId> {
        match self.head()? {
            Some(id) => match self.snapshot(&id)? {
                Some(s) => Ok(s.root),
                None => Err(anyhow!("vault head {id} has no snapshot")),
            },
            None => Snapshot::empty_root(&self.trees()),
        }
    }

    /// Commit a new tree as the vault head.
    pub fn commit(&self, root: ObjectId, device: &str) -> Result<ObjectId> {
        let trees = self.trees();
        let mut files = 0u64;
        let mut dirs = 1u64;
        let mut bytes = 0u64;
        for (_path, r) in trees.walk(&root)? {
            match r.kind {
                Kind::File => {
                    files += 1;
                    bytes += r.size;
                }
                Kind::Dir => dirs += 1,
                Kind::Symlink => files += 1,
            }
        }
        let parent = self.head()?;
        let snap = Snapshot {
            root,
            device: device.to_string(),
            host: "server".to_string(),
            time: Self::now(),
            parent,
            files,
            dirs,
            bytes,
        };
        let id = trees.put_snapshot(&snap)?;
        self.set_head(id)?;
        Ok(id)
    }

    // ---------------------------------------------------------------- objects

    pub fn has_object(&self, id: &ObjectId) -> bool {
        self.objects.has(id)
    }

    pub fn get_object(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        Ok(self.objects.get_encoded(id)?)
    }

    /// Accept an object uploaded by a client.
    ///
    /// For a plain vault the bytes are verified against their content address,
    /// so a buggy or hostile client cannot poison the store. For an encrypted
    /// vault the server cannot verify, and stores the bytes as given.
    pub fn put_object(&self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
        if self.row.encrypted {
            self.objects.put_opaque(id, encoded)
        } else {
            self.objects.put_encoded(id, encoded)
        }
        .map_err(Into::into)
    }

    // -------------------------------------------------------------- file API

    /// Resolve a path to its node id and node.
    ///
    /// The id is what caches (thumbnails, the media index) are keyed on, so
    /// they invalidate automatically the moment the content changes.
    pub fn lookup_id(&self, path: &str) -> Result<Option<(ObjectId, Node)>> {
        let trees = self.trees();
        let mut cur = self.root()?;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let node = match trees.get_node(&cur)? {
                Some(n) => n,
                None => return Ok(None),
            };
            let entries = match node.as_dir() {
                Some(e) => e,
                None => return Ok(None),
            };
            cur = match entries.get(segment) {
                Some(r) => r.id,
                None => return Ok(None),
            };
        }
        Ok(trees.get_node(&cur)?.map(|n| (cur, n)))
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<Entry>> {
        self.require_readable()?;
        let trees = self.trees();
        let root = self.root()?;
        let node = match path.trim_matches('/') {
            "" => trees.get_node(&root)?,
            p => trees.lookup(&root, p)?,
        };
        let entries = match node.as_ref().and_then(|n| n.as_dir()) {
            Some(e) => e,
            None => return Err(anyhow!("{path} is not a directory")),
        };

        let mut out = Vec::with_capacity(entries.len());
        for (name, r) in entries {
            let child = if path.trim_matches('/').is_empty() {
                name.clone()
            } else {
                format!("{}/{}", path.trim_matches('/'), name)
            };
            out.push(Entry {
                name: name.clone(),
                path: child,
                kind: r.kind,
                size: r.size,
                mtime: r.mtime,
            });
        }
        // Directories first, then files, each alphabetically.
        out.sort_by(|a, b| {
            let a_dir = a.kind == Kind::Dir;
            let b_dir = b.kind == Kind::Dir;
            b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    pub fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.require_readable()?;
        let trees = self.trees();
        match trees.lookup(&self.root()?, path)? {
            Some(node) if node.kind() == Kind::File => Ok(Some(trees.read_file(&node)?)),
            _ => Ok(None),
        }
    }

    /// Write a file at `path`, creating intermediate directories as needed.
    pub fn put_file(&self, path: &str, data: &[u8], mtime: Option<i64>) -> Result<()> {
        self.require_readable()?;
        let parts = split_path(path)?;

        let mut chunks = Vec::new();
        let store = &self.objects;
        let mut cursor = std::io::Cursor::new(data);
        chunk_reader(&mut cursor, |chunk| {
            let (id, _new) = store
                .put(chunk)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            chunks.push(ChunkRef {
                id,
                len: chunk.len() as u32,
            });
            Ok(())
        })?;

        let node = Node::File {
            size: data.len() as u64,
            mtime: mtime.unwrap_or_else(Self::now),
            mtime_nsec: 0,
            mode: 0o644,
            chunks,
        };
        let trees = self.trees();
        let node_id = trees.put_node(&node)?;

        let root = self.root()?;
        let new_root = set_in_tree(&trees, root, &parts, Some(node.to_ref(node_id)))?;
        self.commit(new_root, "web")?;
        Ok(())
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        self.require_readable()?;
        let parts = split_path(path)?;
        let trees = self.trees();
        let empty = trees.put_node(&Node::Dir {
            entries: BTreeMap::new(),
        })?;
        let root = self.root()?;
        let new_root = set_in_tree(
            &trees,
            root,
            &parts,
            Some(NodeRef {
                id: empty,
                kind: Kind::Dir,
                size: 0,
                mtime: 0,
            }),
        )?;
        self.commit(new_root, "web")?;
        Ok(())
    }

    /// Remove a file or directory (recursively, as it is just a tree entry).
    pub fn remove(&self, path: &str) -> Result<bool> {
        self.require_readable()?;
        let parts = split_path(path)?;
        let trees = self.trees();
        let root = self.root()?;

        // Removing something that is not there is not an error.
        if trees.lookup(&root, path)?.is_none() {
            return Ok(false);
        }
        let new_root = set_in_tree(&trees, root, &parts, None)?;
        self.commit(new_root, "web")?;
        Ok(true)
    }

    /// Remove `path` from the tree, handing back the detached node so the
    /// caller can put it back later. This is what makes the trash cheap:
    /// only directory objects change, the file's chunks stay exactly where
    /// they are, so restoring is instant and stores nothing new.
    pub fn detach(&self, path: &str) -> Result<Option<NodeRef>> {
        self.require_readable()?;
        let parts = split_path(path)?;
        let trees = self.trees();
        if trees.lookup(&self.root()?, path)?.is_none() {
            return Ok(None);
        }
        let (node_id, node) = self
            .lookup_id(path)?
            .ok_or_else(|| anyhow!("no such path: {}", path))?;
        let node_ref = node.to_ref(node_id);
        let new_root = set_in_tree(&trees, self.root()?, &parts, None)?;
        self.commit(new_root, "web")?;
        Ok(Some(node_ref))
    }

    /// Put a previously detached node back into the tree. Refuses to
    /// silently overwrite; the caller picks an unoccupied path.
    pub fn attach(&self, path: &str, node_ref: NodeRef) -> Result<()> {
        self.require_readable()?;
        let parts = split_path(path)?;
        if self.lookup_id(path)?.is_some() {
            return Err(anyhow!("{} already exists", path));
        }
        let trees = self.trees();
        let root = set_in_tree(&trees, self.root()?, &parts, Some(node_ref))?;
        self.commit(root, "web")?;
        Ok(())
    }

    /// Rename `from`, or move it into a different folder.
    ///
    /// In the tree this is just "detach the entry here, attach it there", so
    /// nothing is copied: a 4 GB video moves instantly and keeps its chunks.
    /// The content is untouched, which is also why moving does not disturb
    /// deduplication -- the chunks are the same addresses before and after.
    pub fn move_path(&self, from: &str, to: &str) -> Result<()> {
        self.require_readable()?;
        let from_parts = split_path(from)?;
        let to_parts = split_path(to)?;

        if from_parts == to_parts {
            return Ok(());
        }
        // A directory cannot be filed inside itself.
        if to_parts.len() > from_parts.len() && to_parts[..from_parts.len()] == from_parts[..] {
            return Err(anyhow!("cannot move {} inside itself", from));
        }

        let trees = self.trees();
        let (node_id, node) = self
            .lookup_id(from)?
            .ok_or_else(|| anyhow!("no such path: {}", from))?;

        // Refuse to silently replace something; rename it away first if that
        // is what was meant.
        if self.lookup_id(to)?.is_some() {
            return Err(anyhow!("{} already exists", to));
        }

        let root = set_in_tree(&trees, self.root()?, &from_parts, None)?;
        let root = set_in_tree(&trees, root, &to_parts, Some(node.to_ref(node_id)))?;
        self.commit(root, "web")?;
        Ok(())
    }

    /// Resolve a path against an arbitrary root (a snapshot's tree, not
    /// necessarily the current one).
    fn lookup_id_at(&self, root: &ObjectId, path: &str) -> Result<Option<(ObjectId, Node)>> {
        let trees = self.trees();
        let mut cur = *root;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let node = match trees.get_node(&cur)? {
                Some(n) => n,
                None => return Ok(None),
            };
            let entries = match node.as_dir() {
                Some(e) => e,
                None => return Ok(None),
            };
            cur = match entries.get(segment) {
                Some(r) => r.id,
                None => return Ok(None),
            };
        }
        Ok(trees.get_node(&cur)?.map(|n| (cur, n)))
    }

    /// Every distinct version of the file at `path`, newest first.
    ///
    /// Walks the snapshot chain and notes each content address the path has
    /// ever pointed at. Overwrites create new snapshots; renames away and
    /// back are picked up too. Bounded on both distinct versions and chain
    /// depth, so ancient histories cost a bounded number of object reads.
    pub fn versions(&self, path: &str, limit: usize) -> Result<Vec<FileVersion>> {
        self.require_readable()?;
        let mut out: Vec<FileVersion> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor = self.head()?;
        let mut steps = 0usize;
        while let Some(snap_id) = cursor {
            if out.len() >= limit || steps >= 500 {
                break;
            }
            steps += 1;
            // History is bounded: a pruned ancestor ends the walk.
            let Some(snap) = self.snapshot(&snap_id)? else {
                break;
            };
            cursor = snap.parent;
            if let Some((node_id, node)) = self.lookup_id_at(&snap.root, path)? {
                if node.kind() == Kind::File && seen.insert(node_id) {
                    out.push(FileVersion {
                        node_id,
                        size: node.size(),
                        mtime: node.mtime(),
                        snapshot_time: snap.time,
                        device: snap.device.clone(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Reassemble the bytes of a specific tree node. Only file nodes carry
    /// chunks; anything else (or a missing object) is None.
    pub fn read_node_bytes(&self, node_id: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.require_readable()?;
        let trees = self.trees();
        match trees.get_node(node_id)? {
            Some(n) if n.kind() == Kind::File => Ok(Some(trees.read_file(&n)?)),
            _ => Ok(None),
        }
    }

    /// Put an old version's content back at `path`. The chunk stores are
    /// content-addressed, so this re-uses the existing bytes.
    pub fn restore_version(&self, path: &str, node_id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let data = self.read_node_bytes(node_id)?;
        if let Some(bytes) = &data {
            self.put_file(path, bytes, None)?;
        }
        Ok(data)
    }

    /// Mark a node and everything it references (child nodes, file chunks).
    fn mark_node_refs(&self, id: &ObjectId, live: &mut HashSet<ObjectId>) -> Result<()> {
        if !live.insert(*id) {
            return Ok(());
        }
        if let Some(node) = self.trees().get_node(id)? {
            match &node {
                Node::File { chunks, .. } => {
                    for c in chunks {
                        live.insert(c.id);
                    }
                }
                Node::Dir { entries } => {
                    for r in entries.values() {
                        self.mark_node_refs(&r.id, live)?;
                    }
                }
                Node::Symlink { .. } => {}
            }
        }
        Ok(())
    }

    /// Delete every object that nothing alive points at any more.
    ///
    /// What counts as alive:
    ///  * the head snapshot and its whole tree — the files that exist now;
    ///  * the newest `history` snapshots behind it — version history has
    ///    bounded retention, so old states age out instead of pinning
    ///    purged content forever;
    ///  * every id in `keep_roots` — a trash entry's detached node, which
    ///    is why deleted-but-not-purged files always restore.
    ///
    /// Returns how many objects were deleted. `qd` clients are expected to
    /// push objects before committing, so this is safe to run at any time;
    /// a client mid-push simply retries its commit.
    pub fn gc(&self, keep_roots: &[ObjectId], history: usize) -> Result<usize> {
        let mut live: HashSet<ObjectId> = HashSet::new();

        let mut cursor = self.head()?;
        let mut kept = 0usize;
        while let Some(snap_id) = cursor {
            if !live.insert(snap_id) {
                break; // cycle guard; the chain is a chain, but be defensive
            }
            // A previous gc may have pruned this ancestor: that is the
            // beginning of history as far as anyone can now see.
            let Some(snap) = self.snapshot(&snap_id)? else {
                break;
            };
            cursor = snap.parent;
            self.mark_node_refs(&snap.root, &mut live)?;
            kept += 1;
            if kept >= history.max(1) {
                break;
            }
        }
        for root in keep_roots {
            self.mark_node_refs(root, &mut live)?;
        }

        let mut freed = 0;
        for id in self.objects.ids()? {
            if !live.contains(&id) && self.objects.delete(&id)? {
                freed += 1;
            }
        }
        Ok(freed)
    }

    /// What is in this vault, and when it was last touched.
    ///
    /// Walks the tree rather than trusting a counter, so it stays right even
    /// if a commit was interrupted.
    pub fn stats(&self) -> Result<VaultStats> {
        self.require_readable()?;
        let trees = self.trees();
        let root = self.root()?;
        // walk() lists children only, so the root has to be counted here --
        // the same convention commit() uses.
        let mut st = VaultStats { dirs: 1, ..VaultStats::default() };
        for (_path, r) in trees.walk(&root)? {
            match r.kind {
                Kind::File => {
                    st.files += 1;
                    st.bytes += r.size;
                }
                Kind::Dir => st.dirs += 1,
                Kind::Symlink => st.symlinks += 1,
            }
        }
        if let Some(id) = self.head()? {
            if let Some(snap) = self.snapshot(&id)? {
                st.device = Some(snap.device);
                st.host = Some(snap.host);
                st.updated = Some(snap.time);
            }
        }
        Ok(st)
    }

    /// Paths whose *file name* contains `needle`, case-insensitively.
    ///
    /// Searches the whole vault, not just one folder -- that is the point.
    /// Matching the name rather than the whole path keeps the results relevant
    /// when a common directory name would otherwise match everything.
    pub fn search(&self, needle: &str, limit: usize) -> Result<Vec<Entry>> {
        self.require_readable()?;
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let trees = self.trees();
        let root = self.root()?;
        let mut out = Vec::new();
        for (path, r) in trees.walk(&root)? {
            let name = path.rsplit('/').next().unwrap_or("");
            if name.to_lowercase().contains(&needle) {
                out.push(Entry {
                    name: name.to_string(),
                    path,
                    kind: r.kind,
                    size: r.size,
                    mtime: r.mtime,
                });
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// One stored version of a file: the tree node is its own content address,
/// so "a version" is just an id that still reassembles.
#[derive(Debug, Clone)]
pub struct FileVersion {
    pub node_id: ObjectId,
    pub size: u64,
    pub mtime: i64,
    /// When the snapshot that captured this version was committed.
    pub snapshot_time: i64,
    pub device: String,
}

/// A summary of a vault's contents.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct VaultStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
    /// Device that made the current head snapshot.
    pub device: Option<String>,
    pub host: Option<String>,
    /// When that snapshot was committed.
    pub updated: Option<i64>,
}

pub(crate) fn split_path(path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(|s| s.to_string())
        .collect();
    if parts.is_empty() {
        return Err(anyhow!("path must not be empty"));
    }
    if parts.iter().any(|p| p == "..") {
        return Err(anyhow!("path must not contain '..'"));
    }
    // Control characters poison everything downstream: NUL truncates in
    // C-string consumers and cannot appear in an HTTP header, newlines and
    // tabs wreck logs and terminal listings. Names are for humans.
    if parts.iter().any(|p| p.chars().any(|c| c.is_control())) {
        return Err(anyhow!("path must not contain control characters"));
    }
    Ok(parts)
}

/// Insert, replace or delete an entry at `parts` within the tree at `root`.
///
/// Returns the id of the new root. Only the nodes along `parts` are rewritten,
/// so every other subtree keeps its identity and stays deduplicated.
fn set_in_tree(
    trees: &TreeSet,
    root: ObjectId,
    parts: &[String],
    child: Option<NodeRef>,
) -> Result<ObjectId> {
    let node = trees
        .get_node(&root)?
        .ok_or_else(|| anyhow!("missing node {root}"))?;
    let mut entries = node
        .as_dir()
        .cloned()
        .ok_or_else(|| anyhow!("node {root} is not a directory"))?;

    if parts.len() == 1 {
        match child {
            Some(c) => entries.insert(parts[0].clone(), c),
            None => entries.remove(&parts[0]),
        };
        return Ok(trees.put_node(&Node::Dir { entries })?);
    }

    let head = &parts[0];
    let child_root = match entries.get(head) {
        Some(r) if r.kind == Kind::Dir => r.id,
        Some(_) => return Err(anyhow!("{head} is not a directory")),
        None => trees.put_node(&Node::Dir {
            entries: BTreeMap::new(),
        })?,
    };

    let new_child_root = set_in_tree(trees, child_root, &parts[1..], child)?;
    let child_node = trees
        .get_node(&new_child_root)?
        .ok_or_else(|| anyhow!("missing node {new_child_root}"))?;
    entries.insert(head.clone(), child_node.to_ref(new_child_root));
    Ok(trees.put_node(&Node::Dir { entries })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::VaultRow;
    use quarkdrive_core::testutil::{pseudo_random, TempDir};

    fn vault(tag: &str, encrypted: bool) -> (TempDir, Vault) {
        let dir = TempDir::new(tag);
        let row = VaultRow {
            id: format!("vault-{tag}"),
            name: tag.to_string(),
            owner_id: "owner".to_string(),
            encrypted,
            created: 0,
        };
        let v = Vault::open(dir.path(), row).unwrap();
        (dir, v)
    }

    fn names(v: &Vault, path: &str) -> Vec<String> {
        v.list_dir(path).unwrap().iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn split_path_normalises_and_rejects_hostile_segments() {
        assert_eq!(split_path("/a//b/./c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(split_path("unicode/名前").unwrap(), vec!["unicode", "名前"]);
        assert!(split_path("").is_err());
        assert!(split_path("..").is_err());
        assert!(split_path("a/../b").is_err());
        assert!(split_path("bad\u{0}name").is_err());
        assert!(split_path("bad\nname").is_err());
        assert!(split_path("bad\tname").is_err());
        assert!(split_path("bad\u{7f}name").is_err());
    }

    #[test]
    fn new_vault_starts_empty() {
        let (_d, v) = vault("v-empty", false);
        assert!(v.head().unwrap().is_none());
        assert!(names(&v, "").is_empty());
    }

    #[test]
    fn put_then_list_then_read() {
        let (_d, v) = vault("v-basic", false);
        v.put_file("hello.txt", b"world", None).unwrap();
        assert_eq!(names(&v, ""), vec!["hello.txt"]);
        assert_eq!(v.read_file("hello.txt").unwrap().unwrap(), b"world");
    }

    #[test]
    fn nested_paths_create_directories() {
        let (_d, v) = vault("v-nested", false);
        v.put_file("a/b/c/deep.txt", b"buried", None).unwrap();
        assert_eq!(names(&v, ""), vec!["a"]);
        assert_eq!(names(&v, "a"), vec!["b"]);
        assert_eq!(names(&v, "a/b"), vec!["c"]);
        assert_eq!(v.read_file("a/b/c/deep.txt").unwrap().unwrap(), b"buried");
    }

    #[test]
    fn overwriting_a_file_replaces_its_content() {
        let (_d, v) = vault("v-overwrite", false);
        v.put_file("f.txt", b"first", None).unwrap();
        v.put_file("f.txt", b"second", None).unwrap();
        assert_eq!(v.read_file("f.txt").unwrap().unwrap(), b"second");
        assert_eq!(names(&v, "").len(), 1, "no duplicate entry");
    }

    #[test]
    fn remove_deletes_files_and_directories() {
        let (_d, v) = vault("v-remove", false);
        v.put_file("keep.txt", b"k", None).unwrap();
        v.put_file("doomed.txt", b"d", None).unwrap();
        assert!(v.remove("doomed.txt").unwrap());

        let left: Vec<String> = names(&v, "");
        assert_eq!(left, vec!["keep.txt"]);

        v.put_file("dir/inner.txt", b"i", None).unwrap();
        assert!(v.remove("dir").unwrap());
        assert_eq!(names(&v, ""), vec!["keep.txt"]);
        assert!(!v.remove("never-existed").unwrap(), "removing nothing is not an error");
    }

    #[test]
    fn gc_frees_purged_content_and_honors_trash() {
        let (_d, v) = vault("v-gc", false);
        let unique: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        v.put_file("secret.bin", &unique, None).unwrap();

        // Deleted-but-not-purged: the trash root pins every byte.
        let node_ref = v.detach("secret.bin").unwrap().unwrap();
        assert_eq!(v.gc(&[node_ref.id], 10).unwrap(), 0);
        assert!(v.read_node_bytes(&node_ref.id).unwrap().is_some());

        // Purged: the trash reference is gone. The content survives only
        // while it is inside the retained history window, so push the
        // chain past it.
        let mut pad = 1u8;
        for _ in 0..14 {
            pad = pad.wrapping_add(11);
            v.put_file("noise.bin", &[pad; 4096], None).unwrap();
        }
        let freed = v.gc(&[], 10).unwrap();
        assert!(freed >= 2, "at least the node and its chunk should free, got {freed}");
        assert!(
            v.read_node_bytes(&node_ref.id).unwrap().is_none(),
            "purged content must be unrecoverable"
        );
        // What is alive stays alive.
        assert!(v.read_file("noise.bin").unwrap().is_some());

        // Deduplication is respected: a chunk another file still uses is
        // never collected.
        v.put_file("twin-a", b"shared bytes", None).unwrap();
        v.put_file("twin-b", b"shared bytes", None).unwrap();
        let (twin_id, _) = v.lookup_id("twin-a").unwrap().unwrap();
        v.detach("twin-a").unwrap();
        for i in 0..3u8 {
            v.put_file("noise.bin", &[i; 4096], None).unwrap();
        }
        v.gc(&[], 10).unwrap();
        assert_eq!(
            v.read_file("twin-b").unwrap().as_deref(),
            Some(b"shared bytes".as_slice()),
            "a shared chunk must survive a twin's deletion"
        );
        let _ = twin_id;
    }

    #[test]
    fn gc_never_breaks_the_head() {
        let (_d, v) = vault("v-gc-head", false);
        for i in 0..20u8 {
            v.put_file("f.bin", &[i; 3000], None).unwrap();
        }
        // With the whole history retained the only orphan is the empty
        // root materialised for the very first commit — a pre-existing
        // leftover GC now cleans up.
        assert_eq!(v.gc(&[], 100).unwrap(), 1);
        assert!(v.read_file("f.bin").unwrap().is_some());
        // Tightening retention frees old versions but the head survives.
        let pruned = v.gc(&[], 5).unwrap();
        assert!(pruned > 0, "old versions age out of a tightened window");
        assert!(v.read_file("f.bin").unwrap().is_some());
    }

    #[test]
    fn overwrites_build_a_version_history() {
        let (_d, v) = vault("v-versions", false);
        v.put_file("report.txt", b"first draft", None).unwrap();
        v.put_file("report.txt", b"second draft with more words", None).unwrap();
        v.remove("report.txt").unwrap();
        v.put_file("report.txt", b"third draft", None).unwrap();

        let versions = v.versions("report.txt", 10).unwrap();
        assert_eq!(versions.len(), 3, "three distinct contents, oldest delete is skipped");
        let sizes: Vec<u64> = versions.iter().map(|x| x.size).collect();
        assert_eq!(sizes, vec![11, 28, 11], "newest first");

        // Any version can be read back by its node id, and restoring an old
        // one replaces the current content with those exact bytes.
        let oldest = versions.last().unwrap();
        assert_eq!(
            v.read_node_bytes(&oldest.node_id).unwrap().as_deref(),
            Some(b"first draft".as_slice())
        );
        v.restore_version("report.txt", &oldest.node_id).unwrap();
        assert_eq!(v.read_file("report.txt").unwrap().as_deref(), Some(b"first draft".as_slice()));

        // Directories and unknown paths have no versions.
        assert!(v.versions("docs", 10).unwrap().is_empty());
        assert!(v.versions("never-existed.txt", 10).unwrap().is_empty());
    }

    #[test]
    fn detached_nodes_can_be_attached_again() {
        let (_d, v) = vault("v-trash", false);
        v.put_file("docs/report.txt", b"quarterly numbers", None).unwrap();

        let node_ref = v.detach("docs/report.txt").unwrap().expect("present");
        assert!(v.read_file("docs/report.txt").unwrap().is_none());
        assert!(v.detach("docs/report.txt").unwrap().is_none(), "double detach is None");

        // Restore somewhere else entirely.
        v.attach("archive/report.txt", node_ref).unwrap();
        assert_eq!(
            v.read_file("archive/report.txt").unwrap().as_deref(),
            Some(b"quarterly numbers".as_slice())
        );

        // Attaching onto an occupied path is refused.
        let (id, node) = v.lookup_id("archive/report.txt").unwrap().unwrap();
        assert!(v.attach("archive/report.txt", node.to_ref(id)).is_err());
        v.attach("docs/report.txt", node_ref).unwrap();

        assert_eq!(
            v.read_file("archive/report.txt").unwrap().as_deref(),
            Some(b"quarterly numbers".as_slice())
        );
        assert_eq!(v.read_file("docs/report.txt").unwrap().as_deref(), Some(b"quarterly numbers".as_slice()));
    }

    #[test]
    fn mkdir_creates_an_empty_directory() {
        let (_d, v) = vault("v-mkdir", false);
        v.mkdir("albums/trips").unwrap();
        assert_eq!(names(&v, ""), vec!["albums"]);
        assert!(names(&v, "albums/trips").is_empty());
    }

    /// Uploading through the file API must produce the same chunks a desktop
    /// client would, so the two stay deduplicated against each other.
    #[test]
    fn uploaded_content_is_deduplicated() {
        let (_d, v) = vault("v-dedup", false);
        let body = pseudo_random(7, 400 * 1024);
        // Explicit mtimes: a node's hash covers its mtime, so wall-clock
        // seconds crossing between the two puts would defeat the dedup and
        // make this test flaky rather than the dedup wrong.
        v.put_file("one.bin", &body, Some(1000)).unwrap();
        let after_one = v.objects.ids().unwrap().len();
        v.put_file("two.bin", &body, Some(1000)).unwrap();
        let after_two = v.objects.ids().unwrap().len();

        // Only the second file's node should be new; its chunks are shared.
        assert!(
            after_two - after_one <= 2,
            "duplicate upload added {} objects, expected at most 2",
            after_two - after_one
        );
    }

    #[test]
    fn large_upload_chunks_and_reassembles() {
        let (_d, v) = vault("v-large", false);
        let body = pseudo_random(9, 2 * 1024 * 1024);
        v.put_file("big.bin", &body, None).unwrap();
        assert_eq!(v.read_file("big.bin").unwrap().unwrap(), body);
    }

    #[test]
    fn empty_file_can_be_stored() {
        let (_d, v) = vault("v-emptyfile", false);
        v.put_file("empty", b"", None).unwrap();
        assert_eq!(v.read_file("empty").unwrap().unwrap(), b"");
    }

    /// The server must refuse rather than return garbage.
    #[test]
    fn encrypted_vault_refuses_file_operations() {
        let (_d, v) = vault("v-encrypted", true);
        assert!(v.put_file("f.txt", b"x", None).is_err());
        assert!(v.read_file("f.txt").is_err());
        assert!(v.list_dir("").is_err());
        assert!(v.remove("f.txt").is_err());
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (_d, v) = vault("v-traversal", false);
        assert!(v.put_file("../escape", b"x", None).is_err());
        assert!(v.put_file("a/../../escape", b"x", None).is_err());
        assert!(v.put_file("", b"x", None).is_err());
    }

    #[test]
    fn rename_keeps_the_content() {
        let (_d, v) = vault("v-rename", false);
        v.put_file("old.txt", b"same bytes", None).unwrap();
        v.move_path("old.txt", "new.txt").unwrap();
        assert!(v.read_file("new.txt").unwrap().is_some());
        assert!(v.lookup_id("old.txt").unwrap().is_none(), "the old name must go");
    }

    #[test]
    fn move_into_an_existing_folder() {
        let (_d, v) = vault("v-move", false);
        v.put_file("a.txt", b"data", None).unwrap();
        v.mkdir("archive").unwrap();
        v.move_path("a.txt", "archive/a.txt").unwrap();
        assert_eq!(v.read_file("archive/a.txt").unwrap().unwrap(), b"data");
        assert!(v.lookup_id("a.txt").unwrap().is_none());
    }

    #[test]
    fn moving_a_folder_moves_its_contents() {
        let (_d, v) = vault("v-movedir", false);
        v.put_file("2024/one.jpg", b"1", None).unwrap();
        v.put_file("2024/two.jpg", b"2", None).unwrap();
        v.move_path("2024", "photos/2024").unwrap();
        assert_eq!(v.read_file("photos/2024/one.jpg").unwrap().unwrap(), b"1");
        assert_eq!(v.read_file("photos/2024/two.jpg").unwrap().unwrap(), b"2");
        assert!(v.lookup_id("2024").unwrap().is_none());
    }

    #[test]
    fn refuses_to_move_a_directory_inside_itself() {
        let (_d, v) = vault("v-selfmove", false);
        v.put_file("album/a.jpg", b"x", None).unwrap();
        assert!(v.move_path("album", "album/album").is_err());
        assert!(v.lookup_id("album/a.jpg").unwrap().is_some(), "nothing should be lost");
    }

    #[test]
    fn refuses_to_overwrite_an_existing_path() {
        let (_d, v) = vault("v-overwrite-move", false);
        v.put_file("a.txt", b"aaa", None).unwrap();
        v.put_file("b.txt", b"bbb", None).unwrap();
        assert!(v.move_path("a.txt", "b.txt").is_err());
        assert_eq!(v.read_file("a.txt").unwrap().unwrap(), b"aaa");
        assert_eq!(v.read_file("b.txt").unwrap().unwrap(), b"bbb");
    }

    #[test]
    fn moving_a_missing_path_is_an_error() {
        let (_d, v) = vault("v-movemissing", false);
        assert!(v.move_path("ghost.txt", "elsewhere.txt").is_err());
    }

    #[test]
    fn moving_to_the_same_place_is_a_no_op() {
        let (_d, v) = vault("v-samemove", false);
        v.put_file("f.txt", b"x", None).unwrap();
        let before = v.head().unwrap();
        v.move_path("f.txt", "f.txt").unwrap();
        assert_eq!(v.head().unwrap(), before, "no work means no new commit");
    }

    #[test]
    fn encrypted_vault_refuses_to_move() {
        let (_d, v) = vault("v-encmove", true);
        assert!(v.move_path("a", "b").is_err());
    }

    #[test]
    fn search_finds_by_name_anywhere_in_the_vault() {
        let (_d, v) = vault("v-search", false);
        v.put_file("a/notes.txt", b"1", None).unwrap();
        v.put_file("deep/b/Q3-report.md", b"2", None).unwrap();
        v.put_file("unrelated.bin", b"3", None).unwrap();

        let hits = v.search("report", 100).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "deep/b/Q3-report.md");

        // Case-insensitive, and matches the name rather than the whole path --
        // a folder called "a" must not make every file under it a hit.
        assert_eq!(v.search("NOTES", 100).unwrap().len(), 1);
        assert_eq!(v.search("a", 100).unwrap().iter().all(|e| e.name.to_lowercase().contains("a")), true);
    }

    #[test]
    fn search_respects_the_limit_and_blank_query() {
        let (_d, v) = vault("v-searchlimit", false);
        for i in 0..30u32 {
            v.put_file(&format!("img{i}.jpg"), b"x", None).unwrap();
        }
        assert_eq!(v.search("img", 5).unwrap().len(), 5);
        assert!(v.search("   ", 100).unwrap().is_empty(), "a blank query matches nothing");
        assert!(v.search("zzz-nothing", 100).unwrap().is_empty());
    }

    #[test]
    fn stats_count_everything() {
        let (_d, v) = vault("v-stats", false);
        v.put_file("a/f1.txt", b"hello", None).unwrap();
        v.put_file("a/f2.txt", b"hello there", None).unwrap();
        v.mkdir("empty").unwrap();

        let st = v.stats().unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.bytes, 16);
        assert!(st.dirs >= 3, "root, a, and empty: {}", st.dirs);
        assert_eq!(st.device.as_deref(), Some("web"));
        assert!(st.updated.is_some());
    }

    #[test]
    fn encrypted_vault_refuses_stats_and_search() {
        let (_d, v) = vault("v-encsearch", true);
        assert!(v.stats().is_err());
        assert!(v.search("x", 10).is_err());
    }

    #[test]
    fn commits_form_a_chain() {
        let (_d, v) = vault("v-chain", false);
        v.put_file("a.txt", b"1", None).unwrap();
        let first = v.head().unwrap().unwrap();
        v.put_file("b.txt", b"2", None).unwrap();
        let second = v.head().unwrap().unwrap();

        assert_ne!(first, second);
        let snap = v.snapshot(&second).unwrap().unwrap();
        assert_eq!(snap.parent, Some(first));
        assert_eq!(snap.files, 2);
    }

    #[test]
    fn listing_reports_directories_before_files() {
        let (_d, v) = vault("v-order", false);
        v.put_file("aaa.txt", b"1", None).unwrap();
        v.mkdir("zzz").unwrap();
        v.put_file("bbb.txt", b"2", None).unwrap();
        let entries = v.list_dir("").unwrap();
        assert_eq!(entries[0].name, "zzz", "directories come first");
        assert_eq!(entries[1].name, "aaa.txt");
        assert_eq!(entries[2].name, "bbb.txt");
    }
}
