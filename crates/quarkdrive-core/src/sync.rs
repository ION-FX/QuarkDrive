//! Three-way merge sync.
//!
//! Each sync is a merge of three trees:
//!
//! * **base** — the tree both sides agreed on at the end of the last sync
//! * **local** — the tree as it stands on disk right now
//! * **remote** — the tree at the server's current head
//!
//! Comparing against the base, rather than only against each other, is what
//! lets the client tell "you changed this" from "I deleted that":
//!
//! | local vs base | remote vs base | result                                  |
//! |---------------|----------------|-----------------------------------------|
//! | unchanged     | changed        | take remote                             |
//! | changed       | unchanged      | take local                              |
//! | changed       | changed        | last write wins, loser kept as conflict |
//! | deleted       | changed        | remote wins (edits beat deletions)      |
//! | changed       | deleted        | local wins (resurrect the file)         |
//!
//! The merged tree is committed with optimistic concurrency: the commit names
//! the head it was computed from, and the server rejects it if the head has
//! moved. The client then re-merges and retries, so two devices syncing at
//! once converge instead of clobbering each other.

use crate::hash::ObjectId;
use crate::index::{CacheEntry, FileCache};
use crate::merge::{diff, Change};
use crate::object::ObjectStore;
use crate::scan::{self, ScanOptions};
use crate::tree::{Kind, Node, NodeRef, Snapshot, TreeSet};
use crate::transport::{CommitOutcome, Transport};
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many times to re-merge after losing a commit race.
const MAX_ATTEMPTS: usize = 4;

/// Objects per `has_objects` round trip.
const HAVE_BATCH: usize = 2000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub uploaded_objects: u64,
    pub uploaded_bytes: u64,
    pub downloaded_objects: u64,
    pub downloaded_bytes: u64,
    pub files_updated: u64,
    pub files_removed: u64,
    pub dirs_created: u64,
    pub conflicts: u64,
    /// True once local and remote agree. False only if we lost every commit race.
    pub committed: bool,
    /// True if there was nothing at all to do.
    pub unchanged: bool,
    pub attempts: usize,
}

impl SyncReport {
    /// Nothing moved in either direction.
    pub fn is_idle(&self) -> bool {
        self.uploaded_objects == 0
            && self.downloaded_objects == 0
            && self.files_updated == 0
            && self.files_removed == 0
            && self.conflicts == 0
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A reasonable default for the `host` field of a snapshot.
pub fn default_host() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

pub struct Syncer<'a> {
    root: &'a Path,
    store: &'a ObjectStore,
    cache: &'a mut FileCache,
    transport: &'a mut dyn Transport,
    device: String,
    host: String,
    opts: ScanOptions,
}

impl<'a> Syncer<'a> {
    pub fn new(
        root: &'a Path,
        store: &'a ObjectStore,
        cache: &'a mut FileCache,
        transport: &'a mut dyn Transport,
    ) -> Self {
        Syncer {
            root,
            store,
            cache,
            transport,
            device: "device".to_string(),
            host: default_host(),
            opts: ScanOptions::default(),
        }
    }

    pub fn with_device(mut self, device: &str) -> Self {
        self.device = device.to_string();
        self
    }

    pub fn with_host(mut self, host: &str) -> Self {
        self.host = host.to_string();
        self
    }

    pub fn with_scan_options(mut self, opts: ScanOptions) -> Self {
        self.opts = opts;
        self
    }

    fn trees(&self) -> TreeSet<'_> {
        TreeSet::new(self.store)
    }

    /// Sync once, re-merging if another device commits first.
    pub fn sync(&mut self) -> Result<SyncReport> {
        for attempt in 1..=MAX_ATTEMPTS {
            let mut report = self.attempt()?;
            report.attempts = attempt;
            if report.committed || attempt == MAX_ATTEMPTS {
                return Ok(report);
            }
            tracing::debug!("head moved during sync; re-merging (attempt {attempt})");
        }
        unreachable!("the loop returns on the final attempt")
    }

    /// Local changes not yet pushed. Backs `qd status`.
    pub fn pending(&self) -> Result<Vec<Change>> {
        let (local_root, _) = scan::scan_tree(self.root, self.store, Some(self.cache), &self.opts)?;
        let base = self.cache.last_root()?;
        diff(&self.trees(), base, Some(local_root))
    }

    fn attempt(&mut self) -> Result<SyncReport> {
        let mut rep = SyncReport::default();

        let (local_root, _) = scan::scan_tree(self.root, self.store, Some(self.cache), &self.opts)
            .context("scanning local vault")?;
        let base = self.cache.last_root()?;

        let remote = self.transport.head()?;
        let remote_root = remote.as_ref().map(|(_, s)| s.root);

        // Pull the remote tree's nodes. Object ids are content addresses, so
        // we only fetch directories we do not already have.
        if let Some(root) = remote_root {
            self.fetch_tree(root, &mut rep)?;
        }

        let merged = self
            .merge_dir(Path::new(""), base, Some(local_root), remote_root, &mut rep)?
            .ok_or_else(|| anyhow!("merge produced no root"))?;

        // Nothing of ours to push: we only pulled, or both sides already agreed.
        let empty_root = Snapshot::empty_root(&self.trees())?;
        if Some(merged) == remote_root || (remote_root.is_none() && merged == empty_root) {
            rep.unchanged = base == Some(merged);
            self.rebuild_cache(merged)?;
            rep.committed = true;
            return Ok(rep);
        }

        self.upload_tree(merged, &mut rep)?;

        let (files, dirs, bytes) = self.tree_stats(merged)?;
        let expected_parent = remote.as_ref().map(|(id, _)| *id);
        let snapshot = Snapshot {
            root: merged,
            device: self.device.clone(),
            host: self.host.clone(),
            time: now_secs(),
            parent: expected_parent,
            files,
            dirs,
            bytes,
        };

        match self.transport.commit(expected_parent, &snapshot)? {
            CommitOutcome::Accepted(_) => {
                self.rebuild_cache(merged)?;
                rep.committed = true;
            }
            // Someone else committed first. Leave the cache alone; the next
            // attempt re-reads the head and merges against the newer tree.
            CommitOutcome::Conflict { .. } => {
                rep.committed = false;
            }
        }
        Ok(rep)
    }

    // ------------------------------------------------------------- transfer

    /// Download every node object in the remote tree, but not file contents.
    ///
    /// Metadata is small relative to data; chunks are fetched lazily by
    /// [`Syncer::ensure_chunks`], only for files we decide to materialise.
    fn fetch_tree(&mut self, root: ObjectId, rep: &mut SyncReport) -> Result<()> {
        let mut stack = vec![root];
        let mut seen: HashSet<ObjectId> = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) || self.store.has(&id) {
                continue;
            }
            let encoded = self
                .transport
                .get_object(&id)?
                .ok_or_else(|| anyhow!("server is missing object {id}"))?;
            self.store.put_encoded(&id, &encoded)?;
            rep.downloaded_objects += 1;
            rep.downloaded_bytes += encoded.len() as u64;

            let node = self
                .trees()
                .get_node(&id)?
                .ok_or_else(|| anyhow!("object {id} is not a valid node"))?;
            if let Node::Dir { entries } = node {
                for r in entries.values() {
                    stack.push(r.id);
                }
            }
        }
        Ok(())
    }

    fn upload_tree(&mut self, root: ObjectId, rep: &mut SyncReport) -> Result<()> {
        let objects = self.trees().collect_objects(&root)?;
        let ids: Vec<ObjectId> = objects.into_iter().collect();
        for batch in ids.chunks(HAVE_BATCH) {
            let have = self.transport.has_objects(batch)?;
            if have.len() != batch.len() {
                return Err(anyhow!(
                    "server answered {} of {} ids",
                    have.len(),
                    batch.len()
                ));
            }
            for (id, present) in batch.iter().zip(have) {
                if present {
                    continue;
                }
                let encoded = self
                    .store
                    .get_encoded(id)?
                    .ok_or_else(|| anyhow!("local store is missing object {id}"))?;
                self.transport.put_object(id, &encoded)?;
                rep.uploaded_objects += 1;
                rep.uploaded_bytes += encoded.len() as u64;
            }
        }
        Ok(())
    }

    /// Fetch a file's chunks on demand.
    fn ensure_chunks(&mut self, node: &Node, rep: &mut SyncReport) -> Result<()> {
        let Some(chunks) = node.as_file() else {
            return Ok(());
        };
        let ids: Vec<ObjectId> = chunks.iter().map(|c| c.id).collect();
        for id in ids {
            if self.store.has(&id) {
                continue;
            }
            let encoded = self
                .transport
                .get_object(&id)?
                .ok_or_else(|| anyhow!("server is missing chunk {id}"))?;
            self.store.put_encoded(&id, &encoded)?;
            rep.downloaded_objects += 1;
            rep.downloaded_bytes += encoded.len() as u64;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- merge

    fn entries_of(&self, id: Option<ObjectId>) -> Result<BTreeMap<String, NodeRef>> {
        let Some(id) = id else {
            return Ok(BTreeMap::new());
        };
        match self.trees().get_node(&id)? {
            Some(Node::Dir { entries }) => Ok(entries),
            Some(_) => Ok(BTreeMap::new()),
            None => Err(anyhow!("missing tree node {id}")),
        }
    }

    fn merge_dir(
        &mut self,
        rel: &Path,
        base: Option<ObjectId>,
        local: Option<ObjectId>,
        remote: Option<ObjectId>,
        rep: &mut SyncReport,
    ) -> Result<Option<ObjectId>> {
        let b = self.entries_of(base)?;
        let l = self.entries_of(local)?;
        let r = self.entries_of(remote)?;

        let mut names: BTreeSet<String> = BTreeSet::new();
        names.extend(b.keys().cloned());
        names.extend(l.keys().cloned());
        names.extend(r.keys().cloned());

        let mut merged = BTreeMap::new();
        for name in names {
            let child_rel = rel.join(&name);
            if let Some(node_ref) = self.merge_entry(
                &child_rel,
                b.get(&name).copied(),
                l.get(&name).copied(),
                r.get(&name).copied(),
                rep,
            )? {
                merged.insert(name, node_ref);
            }
        }
        Ok(Some(self.trees().put_node(&Node::Dir { entries: merged })?))
    }

    fn merge_entry(
        &mut self,
        rel: &Path,
        base: Option<NodeRef>,
        local: Option<NodeRef>,
        remote: Option<NodeRef>,
        rep: &mut SyncReport,
    ) -> Result<Option<NodeRef>> {
        let b = base.map(|x| x.id);
        let l = local.map(|x| x.id);
        let r = remote.map(|x| x.id);

        // Identical on both sides, or absent on both: nothing to decide.
        if l == r {
            return Ok(local);
        }
        // Only the remote moved, so take it wholesale — including the case
        // where the remote deleted the path and we never touched it.
        if b == l {
            return self.take_remote(rel, remote, rep);
        }
        // Only the local side moved: keep it, whether that is an edit or a
        // deletion.
        if b == r {
            return Ok(local);
        }

        // Both sides moved relative to the base.
        match (local, remote) {
            (None, None) => Ok(None),
            // Deleted remotely but edited here: the edit wins and is restored.
            (Some(_), None) => Ok(local),
            // Deleted here but edited there: the edit wins.
            (None, Some(_)) => self.take_remote(rel, remote, rep),
            (Some(lr), Some(rr)) => {
                if lr.kind == Kind::Dir && rr.kind == Kind::Dir {
                    let merged = self.merge_dir(rel, b, Some(lr.id), Some(rr.id), rep)?;
                    return Ok(merged.map(|id| NodeRef {
                        id,
                        kind: Kind::Dir,
                        size: 0,
                        mtime: 0,
                    }));
                }
                self.resolve_conflict(rel, lr, rr, rep)
            }
        }
    }

    fn take_remote(
        &mut self,
        rel: &Path,
        remote: Option<NodeRef>,
        rep: &mut SyncReport,
    ) -> Result<Option<NodeRef>> {
        match remote {
            None => {
                self.remove_local(rel, rep)?;
                Ok(None)
            }
            Some(r) => {
                self.materialize(rel, r, rep)?;
                Ok(Some(r))
            }
        }
    }

    /// Last write wins. The loser is preserved next to the winner so a sync
    /// never silently destroys work.
    fn resolve_conflict(
        &mut self,
        rel: &Path,
        local: NodeRef,
        remote: NodeRef,
        rep: &mut SyncReport,
    ) -> Result<Option<NodeRef>> {
        rep.conflicts += 1;
        // Ties go to the local side, so the outcome does not depend on which
        // device happened to sync first.
        if local.mtime >= remote.mtime {
            self.save_conflict_copy(rel, remote, false, rep)?;
            Ok(Some(local))
        } else {
            self.save_conflict_copy(rel, local, true, rep)?;
            self.take_remote(rel, Some(remote), rep)
        }
    }

    /// Write the losing version to `<name>.conflict-<device>-<time>`.
    fn save_conflict_copy(
        &mut self,
        rel: &Path,
        loser: NodeRef,
        loser_is_local: bool,
        rep: &mut SyncReport,
    ) -> Result<()> {
        if loser.kind != Kind::File {
            return Ok(());
        }
        let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        let conflict_name = format!("{name}.conflict-{}-{}", self.device, now_secs());
        let conflict_rel = match rel.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join(&conflict_name),
            _ => PathBuf::from(&conflict_name),
        };
        let dest = self.root.join(&conflict_rel);

        if loser_is_local {
            let src = self.root.join(rel);
            if std::fs::symlink_metadata(&src).is_ok() {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dest)?;
            }
        } else {
            let node = self
                .trees()
                .get_node(&loser.id)?
                .ok_or_else(|| anyhow!("missing node {}", loser.id))?;
            self.ensure_chunks(&node, rep)?;
            let data = self.trees().read_file(&node)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &data)?;
        }
        Ok(())
    }

    /// Write a tree node out to the filesystem.
    fn materialize(&mut self, rel: &Path, r: NodeRef, rep: &mut SyncReport) -> Result<()> {
        match r.kind {
            Kind::Dir => self.materialize_dir(rel, r.id, rep),
            Kind::File => self.materialize_file(rel, r.id, rep),
            Kind::Symlink => self.materialize_symlink(rel, r.id, rep),
        }
    }

    /// Bring a directory into line with the remote tree.
    ///
    /// The merge can adopt a remote directory wholesale — whenever the local
    /// copy is unchanged relative to the base, there is nothing to compare
    /// entry by entry. That means deletions *inside* such a directory are not
    /// visible to the entry-level merge, so they have to be applied here:
    /// anything present locally that the remote directory does not list is
    /// removed. Without this, deleting a file one level down would appear to
    /// succeed and then resurrect itself on the next sync of that directory.
    fn materialize_dir(&mut self, rel: &Path, id: ObjectId, rep: &mut SyncReport) -> Result<()> {
        let path = self.root.join(rel);
        if !path.is_dir() {
            if path.exists() || path.is_symlink() {
                std::fs::remove_file(&path)?;
            }
            std::fs::create_dir_all(&path)?;
            rep.dirs_created += 1;
        }

        let node = self
            .trees()
            .get_node(&id)?
            .ok_or_else(|| anyhow!("missing dir node {id}"))?;
        let entries = node.as_dir().cloned().unwrap_or_default();

        // Collect before removing: read_dir while deleting is not safe.
        let mut stale = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&path) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if entries.contains_key(&name) {
                    continue;
                }
                // Never touch the client's own state, and never delete a
                // conflict copy we may have just written.
                if self.opts.is_excluded(&name) || name.contains(".conflict-") {
                    continue;
                }
                stale.push(name);
            }
        }
        for name in stale {
            self.remove_local(&rel.join(&name), rep)?;
        }

        for (name, child) in entries {
            self.materialize(&rel.join(&name), child, rep)?;
        }
        Ok(())
    }

    fn materialize_file(&mut self, rel: &Path, id: ObjectId, rep: &mut SyncReport) -> Result<()> {
        let path = self.root.join(rel);
        let node = self
            .trees()
            .get_node(&id)?
            .ok_or_else(|| anyhow!("missing file node {id}"))?;
        self.ensure_chunks(&node, rep)?;
        let data = self.trees().read_file(&node)?;
        let (mtime, mtime_nsec, mode) = match &node {
            Node::File {
                mtime,
                mtime_nsec,
                mode,
                ..
            } => (*mtime, *mtime_nsec, *mode),
            _ => (0, 0, 0),
        };

        // A file can replace a directory of the same name.
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        }
        // Adopting a directory wholesale walks every file inside it. Skip the
        // ones that already match, or a single remote change would rewrite
        // the whole directory.
        if self.local_matches(rel, id, mtime, mtime_nsec, data.len() as u64) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &data)?;
        // Restoring mtime is what stops the next scan from seeing this file
        // as locally modified.
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(mtime, mtime_nsec))?;
        if mode != 0 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        rep.files_updated += 1;
        Ok(())
    }

    /// Does the file on disk already hold exactly this node's content?
    ///
    /// True when the cache says this path produced this node and the stat
    /// signature still agrees, which is the same test the scanner uses to
    /// avoid re-chunking.
    fn local_matches(
        &self,
        rel: &Path,
        node_id: ObjectId,
        mtime: i64,
        mtime_nsec: u32,
        size: u64,
    ) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(self.root.join(rel)) else {
            return false;
        };
        if !meta.is_file() || meta.len() != size {
            return false;
        }
        let (actual_mtime, actual_nsec) = match meta.modified() {
            Ok(t) => scan::system_time_to_parts(t),
            Err(_) => return false,
        };
        if actual_mtime != mtime || actual_nsec != mtime_nsec {
            return false;
        }
        match self.cache.get(&rel.to_string_lossy()) {
            Ok(Some(entry)) => entry.node_id == node_id && entry.inode == meta.ino(),
            _ => false,
        }
    }

    fn materialize_symlink(&mut self, rel: &Path, id: ObjectId, rep: &mut SyncReport) -> Result<()> {
        let path = self.root.join(rel);
        let node = self
            .trees()
            .get_node(&id)?
            .ok_or_else(|| anyhow!("missing symlink node {id}"))?;
        if let Node::Symlink { target, .. } = &node {
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&path);
            std::os::unix::fs::symlink(target, &path)?;
            rep.files_updated += 1;
        }
        Ok(())
    }

    fn remove_local(&mut self, rel: &Path, rep: &mut SyncReport) -> Result<()> {
        let path = self.root.join(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                std::fs::remove_dir_all(&path)?;
                rep.files_removed += 1;
            }
            Ok(_) => {
                std::fs::remove_file(&path)?;
                rep.files_removed += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    // ------------------------------------------------------------- finishing

    fn tree_stats(&self, root: ObjectId) -> Result<(u64, u64, u64)> {
        let mut files = 0u64;
        let mut dirs = 1u64;
        let mut bytes = 0u64;
        for (_path, r) in self.trees().walk(&root)? {
            match r.kind {
                Kind::File => {
                    files += 1;
                    bytes += r.size;
                }
                Kind::Dir => dirs += 1,
                Kind::Symlink => files += 1,
            }
        }
        Ok((files, dirs, bytes))
    }

    /// Record the tree we just agreed on, along with each file's stat
    /// signature, so the next scan can skip unchanged files.
    fn rebuild_cache(&mut self, root: ObjectId) -> Result<()> {
        let listing = self.trees().walk(&root)?;
        let mut entries = Vec::with_capacity(listing.len());
        for (path, r) in listing {
            if r.kind != Kind::File {
                continue;
            }
            let abs = self.root.join(&path);
            let Ok(meta) = std::fs::symlink_metadata(&abs) else {
                continue;
            };
            let (mtime, mtime_nsec) = match meta.modified() {
                Ok(t) => scan::system_time_to_parts(t),
                Err(_) => (0, 0),
            };
            entries.push((
                path,
                CacheEntry {
                    size: meta.len(),
                    mtime,
                    mtime_nsec,
                    inode: meta.ino(),
                    node_id: r.id,
                },
            ));
        }
        self.cache.replace_all(&entries)?;
        self.cache.set_last_root(Some(root))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Key;
    use crate::testutil::{make_tree, pseudo_random, TempDir};
    use crate::transport::MemoryVault;

    /// A device: a synced directory, its own object store, and its cache.
    struct Device {
        _dir: TempDir,
        _store_dir: TempDir,
        root: PathBuf,
        store: ObjectStore,
        cache: FileCache,
    }

    impl Device {
        fn new(tag: &str, key: Option<Key>) -> Self {
            let dir = TempDir::new(tag);
            let store_dir = TempDir::new(&format!("{tag}-store"));
            let store = ObjectStore::open(store_dir.path(), key).unwrap();
            let cache = FileCache::open_in_memory().unwrap();
            Device {
                root: dir.path().to_path_buf(),
                _dir: dir,
                _store_dir: store_dir,
                store,
                cache,
            }
        }

        fn sync(&mut self, vault: &mut MemoryVault, device: &str) -> Result<SyncReport> {
            let mut syncer = Syncer::new(&self.root, &self.store, &mut self.cache, vault)
                .with_device(device)
                .with_host("test-host");
            syncer.sync()
        }

        fn read(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.root.join(rel)).expect("read synced file")
        }

        fn exists(&self, rel: &str) -> bool {
            self.root.join(rel).exists()
        }
    }

    /// Build a tree inside the vault directly, to stand in for content
    /// committed by a device we have never talked to.
    fn seed_remote(vault: &mut MemoryVault, files: &[(&str, &[u8])]) -> Result<ObjectId> {
        let scratch = TempDir::new("seed-remote");
        let entries: Vec<(&str, Option<&[u8]>)> =
            files.iter().map(|(p, d)| (*p, Some(*d))).collect();
        make_tree(scratch.path(), &entries);
        let store_dir = TempDir::new("seed-remote-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, _) = scan::scan_tree(scratch.path(), &store, None, &ScanOptions::default())?;
        let trees = TreeSet::new(&store);
        for id in trees.collect_objects(&root)? {
            if let Some(encoded) = store.get_encoded(&id)? {
                vault.put_object(&id, &encoded)?;
            }
        }
        Ok(root)
    }

    #[test]
    fn first_sync_uploads_everything() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-first", None);
        make_tree(
            &dev.root,
            &[("a.txt", Some(b"alpha")), ("sub/b.txt", Some(b"beta"))],
        );

        let rep = dev.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed, "first sync must commit");
        assert!(rep.uploaded_objects > 0);
        assert_eq!(vault.commits, 1);
        assert!(vault.object_count() > 0);
    }

    /// The steady state: a second sync with no changes must move nothing.
    #[test]
    fn second_sync_is_a_no_op() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-idle", None);
        make_tree(&dev.root, &[("a.txt", Some(b"alpha"))]);

        dev.sync(&mut vault, "laptop").unwrap();
        let baseline = vault.uploaded_objects;

        let rep = dev.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed);
        assert_eq!(
            vault.uploaded_objects, baseline,
            "nothing changed, so nothing should be uploaded"
        );
        assert_eq!(rep.files_updated, 0);
        assert_eq!(rep.downloaded_objects, 0, "we already have every object");
        assert!(rep.unchanged, "should report no work to do");
    }

    #[test]
    fn local_edit_is_pushed() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-edit", None);
        make_tree(&dev.root, &[("a.txt", Some(b"before"))]);
        dev.sync(&mut vault, "laptop").unwrap();

        std::fs::write(dev.root.join("a.txt"), b"after").unwrap();
        let rep = dev.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed);
        assert!(rep.uploaded_objects > 0, "the new chunk must be uploaded");
        assert_eq!(vault.commits, 2);

        let mut other = Device::new("sync-edit-other", None);
        other.sync(&mut vault, "desktop").unwrap();
        assert_eq!(other.read("a.txt"), b"after");
    }

    /// Appending to a large file must only transfer the new chunks.
    #[test]
    fn appending_transfers_only_new_chunks() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-append", None);
        let body = pseudo_random(31, 512 * 1024);
        make_tree(&dev.root, &[("data.bin", Some(&body))]);
        dev.sync(&mut vault, "laptop").unwrap();
        let after_first = vault.uploaded_objects;

        let mut grown = body.clone();
        grown.extend_from_slice(&pseudo_random(32, 64 * 1024));
        std::fs::write(dev.root.join("data.bin"), &grown).unwrap();

        dev.sync(&mut vault, "laptop").unwrap();
        let delta = vault.uploaded_objects - after_first;
        // A fixed-block sync would re-send every block after the append;
        // content-defined chunking should touch only the tail and the nodes.
        assert!(
            delta <= 5,
            "appending 64 KiB uploaded {delta} objects; expected only the tail"
        );
    }

    #[test]
    fn two_devices_exchange_files() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-pull-a", None);
        let mut desktop = Device::new("sync-pull-b", None);

        make_tree(&laptop.root, &[("mine.txt", Some(b"laptop"))]);
        make_tree(&desktop.root, &[("theirs.txt", Some(b"desktop"))]);

        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        assert!(desktop.exists("mine.txt"), "desktop should pull the laptop's file");

        laptop.sync(&mut vault, "laptop").unwrap();
        assert_eq!(laptop.read("theirs.txt"), b"desktop");
        assert!(laptop.exists("mine.txt"), "our own file must survive the merge");
        assert!(desktop.exists("theirs.txt"));
    }

    #[test]
    fn pulls_changes_made_by_another_device() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-offline-a", None);
        let mut desktop = Device::new("sync-offline-b", None);

        make_tree(&laptop.root, &[("shared.txt", Some(b"v1"))]);
        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        std::fs::write(desktop.root.join("shared.txt"), b"v2").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        laptop.sync(&mut vault, "laptop").unwrap();
        assert_eq!(laptop.read("shared.txt"), b"v2");
    }

    /// Two devices editing the same file: neither version may be lost.
    #[test]
    fn conflicting_edits_are_both_preserved() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-conflict-a", None);
        let mut desktop = Device::new("sync-conflict-b", None);

        make_tree(&laptop.root, &[("doc.txt", Some(b"original"))]);
        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        // The desktop edits and pushes first, with a distinctly newer mtime.
        std::fs::write(desktop.root.join("doc.txt"), b"desktop version").unwrap();
        filetime::set_file_mtime(
            &desktop.root.join("doc.txt"),
            filetime::FileTime::from_unix_time(now_secs() + 100, 0),
        )
        .unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        // The laptop edits its older copy and syncs.
        std::fs::write(laptop.root.join("doc.txt"), b"laptop version").unwrap();
        filetime::set_file_mtime(
            &laptop.root.join("doc.txt"),
            filetime::FileTime::from_unix_time(now_secs(), 0),
        )
        .unwrap();

        let rep = laptop.sync(&mut vault, "laptop").unwrap();
        assert_eq!(rep.conflicts, 1, "exactly one file conflicted");
        assert_eq!(
            laptop.read("doc.txt"),
            b"desktop version",
            "the newer edit wins"
        );

        // The laptop's own edit must still be on disk under a conflict name.
        let kept = std::fs::read_dir(&laptop.root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.starts_with("doc.txt.conflict-"))
            .expect("conflict copy must exist");
        assert_eq!(
            std::fs::read(laptop.root.join(kept)).unwrap(),
            b"laptop version",
            "the losing edit must be preserved"
        );
    }

    #[test]
    fn local_deletion_propagates() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-del-local", None);
        make_tree(
            &dev.root,
            &[("gone.txt", Some(b"x")), ("stay.txt", Some(b"y"))],
        );
        dev.sync(&mut vault, "laptop").unwrap();

        std::fs::remove_file(dev.root.join("gone.txt")).unwrap();
        dev.sync(&mut vault, "laptop").unwrap();

        let mut fresh = Device::new("sync-del-fresh", None);
        fresh.sync(&mut vault, "phone").unwrap();
        assert!(!fresh.exists("gone.txt"), "deletion must reach other devices");
        assert!(fresh.exists("stay.txt"));
    }

    #[test]
    fn remote_deletion_removes_local_file() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-rmdel-a", None);
        let mut desktop = Device::new("sync-rmdel-b", None);

        make_tree(&laptop.root, &[("doomed.txt", Some(b"x"))]);
        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        std::fs::remove_file(desktop.root.join("doomed.txt")).unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        laptop.sync(&mut vault, "laptop").unwrap();
        assert!(
            !laptop.exists("doomed.txt"),
            "remote delete must apply locally"
        );
    }

    /// An edit must beat a deletion: losing work is worse than resurrecting it.
    #[test]
    fn local_edit_beats_remote_deletion() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-resurrect-a", None);
        let mut desktop = Device::new("sync-resurrect-b", None);

        make_tree(&laptop.root, &[("f.txt", Some(b"v1"))]);
        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        std::fs::remove_file(desktop.root.join("f.txt")).unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();
        assert!(!desktop.exists("f.txt"));

        std::fs::write(laptop.root.join("f.txt"), b"v2-laptop").unwrap();
        laptop.sync(&mut vault, "laptop").unwrap();
        assert_eq!(laptop.read("f.txt"), b"v2-laptop");

        let mut fresh = Device::new("sync-resurrect-c", None);
        fresh.sync(&mut vault, "phone").unwrap();
        assert_eq!(fresh.read("f.txt"), b"v2-laptop", "the edit must survive");
    }

    /// Content we have never seen must be fetched, not merely merged.
    #[test]
    fn pulls_unseen_remote_content() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-unseen", None);
        make_tree(&dev.root, &[("local.txt", Some(b"here"))]);
        dev.sync(&mut vault, "laptop").unwrap();

        // A device that merged properly would have kept our file, so the
        // seeded tree contains both the new file and our existing one.
        let root = seed_remote(
            &mut vault,
            &[("local.txt", b"here"), ("from/elsewhere.txt", b"remote bytes")],
        )
        .unwrap();
        let snap = Snapshot {
            root,
            device: "server-seed".into(),
            host: "seed".into(),
            time: now_secs(),
            parent: vault.head,
            files: 2,
            dirs: 2,
            bytes: 16,
        };
        vault.simulate_other_device_commit(&snap);

        dev.sync(&mut vault, "laptop").unwrap();
        assert_eq!(dev.read("from/elsewhere.txt"), b"remote bytes");
        assert!(dev.exists("local.txt"), "our own file must survive");
    }

    #[test]
    fn empty_vault_and_empty_folder_needs_no_commit() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-empty-both", None);
        let rep = dev.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed);
        assert_eq!(vault.commits, 0, "nothing to commit for an empty vault");
    }

    #[test]
    fn empty_file_syncs() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-emptyfile", None);
        make_tree(&dev.root, &[("empty", Some(b""))]);
        dev.sync(&mut vault, "laptop").unwrap();

        let mut other = Device::new("sync-emptyfile-2", None);
        other.sync(&mut vault, "phone").unwrap();
        assert!(other.exists("empty"));
        assert_eq!(other.read("empty"), b"");
    }

    #[test]
    fn nested_directories_round_trip() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-nested", None);
        make_tree(
            &dev.root,
            &[
                ("a/b/c/deep.txt", Some(b"deep")),
                ("a/b/other.txt", Some(b"other")),
                ("top.txt", Some(b"top")),
            ],
        );
        dev.sync(&mut vault, "laptop").unwrap();

        let mut other = Device::new("sync-nested-2", None);
        other.sync(&mut vault, "phone").unwrap();
        assert_eq!(other.read("a/b/c/deep.txt"), b"deep");
        assert_eq!(other.read("a/b/other.txt"), b"other");
        assert_eq!(other.read("top.txt"), b"top");
    }

    #[test]
    fn large_file_round_trips() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-large", None);
        let body = pseudo_random(41, 3 * 1024 * 1024);
        make_tree(&dev.root, &[("big.bin", Some(&body))]);
        dev.sync(&mut vault, "laptop").unwrap();

        let mut other = Device::new("sync-large-2", None);
        other.sync(&mut vault, "phone").unwrap();
        assert_eq!(other.read("big.bin"), body);
    }

    #[test]
    fn symlinks_round_trip() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-symlink-a", None);
        make_tree(&dev.root, &[("target.txt", Some(b"payload"))]);
        std::os::unix::fs::symlink("target.txt", dev.root.join("link.txt")).unwrap();
        dev.sync(&mut vault, "laptop").unwrap();

        let mut other = Device::new("sync-symlink-b", None);
        other.sync(&mut vault, "phone").unwrap();
        let link = other.root.join("link.txt");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap().to_string_lossy(), "target.txt");
    }

    /// The server must not be able to read an end-to-end encrypted vault, and
    /// a device without the key must not be able to sync at all.
    #[test]
    fn encrypted_vault_is_opaque_to_the_server() {
        let key = Key::generate();
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-e2ee-a", Some(key.clone()));
        make_tree(&laptop.root, &[("secret.txt", Some(b"classified"))]);
        laptop.sync(&mut vault, "laptop").unwrap();

        let leaked = vault
            .objects
            .values()
            .any(|bytes| bytes.windows(11).any(|w| w == b"classified"));
        assert!(!leaked, "plaintext reached the server");

        // Wrong key: syncing must fail rather than silently produce garbage.
        let mut intruder = Device::new("sync-e2ee-b", Some(Key::generate()));
        assert!(
            intruder.sync(&mut vault, "intruder").is_err(),
            "a different key must not be able to sync"
        );

        // Right key: content comes back intact.
        let mut phone = Device::new("sync-e2ee-c", Some(key));
        phone.sync(&mut vault, "phone").unwrap();
        assert_eq!(phone.read("secret.txt"), b"classified");
    }

    /// The whole point of optimistic concurrency: losing the race must not
    /// lose the data.
    #[test]
    fn losing_a_commit_race_retries_and_converges() {
        /// Commits on the client's behalf, but moves the head out from under
        /// it the first time.
        struct RacingVault {
            inner: MemoryVault,
            raced: bool,
            intruder_root: ObjectId,
        }

        impl Transport for RacingVault {
            fn head(&mut self) -> Result<Option<(ObjectId, Snapshot)>> {
                self.inner.head()
            }
            fn get_object(&mut self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
                self.inner.get_object(id)
            }
            fn has_objects(&mut self, ids: &[ObjectId]) -> Result<Vec<bool>> {
                self.inner.has_objects(ids)
            }
            fn put_object(&mut self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
                self.inner.put_object(id, encoded)
            }
            fn commit(
                &mut self,
                expected_parent: Option<ObjectId>,
                snapshot: &Snapshot,
            ) -> Result<CommitOutcome> {
                if !self.raced {
                    self.raced = true;
                    let intruder = Snapshot {
                        root: self.intruder_root,
                        device: "intruder".into(),
                        host: "intruder".into(),
                        time: now_secs(),
                        parent: self.inner.head,
                        files: 1,
                        dirs: 1,
                        bytes: 5,
                    };
                    self.inner.simulate_other_device_commit(&intruder);
                }
                self.inner.commit(expected_parent, snapshot)
            }
        }

        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-race-a", None);
        make_tree(&laptop.root, &[("mine.txt", Some(b"mine!"))]);
        laptop.sync(&mut vault, "laptop").unwrap();

        let intruder_root = seed_remote(&mut vault, &[("intruder.txt", b"hello")]).unwrap();
        std::fs::write(laptop.root.join("mine.txt"), b"edited").unwrap();

        let mut racing = RacingVault {
            inner: vault,
            raced: false,
            intruder_root,
        };
        let mut syncer = Syncer::new(
            &laptop.root,
            &laptop.store,
            &mut laptop.cache,
            &mut racing,
        )
        .with_device("laptop");
        let rep = syncer.sync().unwrap();

        assert!(rep.committed, "sync must still converge");
        assert_eq!(rep.attempts, 2, "the first commit should lose the race");
        assert_eq!(racing.inner.rejected_commits, 1);
        assert!(
            laptop.exists("intruder.txt"),
            "must pick up the other device's file"
        );
        assert_eq!(laptop.read("mine.txt"), b"edited");
    }

    /// The exact sequence from the end-to-end run: a device adds a file, the
    /// other pulls it, then deletes a different file. The deletion must survive
    /// the intervening commit.
    #[test]
    fn deletion_propagates_after_an_intervening_commit() {
        let mut vault = MemoryVault::new();
        let mut laptop = Device::new("sync-del-inter-a", None);
        let mut desktop = Device::new("sync-del-inter-b", None);

        make_tree(
            &laptop.root,
            &[("docs/readme.md", Some(b"line\n")), ("notes.txt", Some(b"n"))],
        );
        laptop.sync(&mut vault, "laptop").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        std::fs::write(desktop.root.join("from-desktop.txt"), b"typed").unwrap();
        desktop.sync(&mut vault, "desktop").unwrap();

        laptop.sync(&mut vault, "laptop").unwrap();
        assert_eq!(laptop.read("from-desktop.txt"), b"typed");

        std::fs::remove_file(laptop.root.join("docs/readme.md")).unwrap();
        let rep = laptop.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed);

        let rep = desktop.sync(&mut vault, "desktop").unwrap();
        assert_eq!(rep.files_removed, 1, "desktop did not remove the deleted file");
        assert!(
            !desktop.exists("docs/readme.md"),
            "deletion must propagate across an intervening commit"
        );
        assert!(desktop.exists("notes.txt"));
        assert!(desktop.exists("from-desktop.txt"));
    }

    #[test]
    fn pending_reports_only_local_changes() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-pending", None);
        make_tree(&dev.root, &[("a.txt", Some(b"one"))]);
        dev.sync(&mut vault, "laptop").unwrap();

        {
            let syncer = Syncer::new(&dev.root, &dev.store, &mut dev.cache, &mut vault);
            assert!(
                syncer.pending().unwrap().is_empty(),
                "nothing pending right after a sync"
            );
        }

        std::fs::write(dev.root.join("a.txt"), b"two").unwrap();
        std::fs::write(dev.root.join("new.txt"), b"brand new").unwrap();

        let syncer = Syncer::new(&dev.root, &dev.store, &mut dev.cache, &mut vault);
        let pending = syncer.pending().unwrap();
        let paths: Vec<&str> = pending.iter().map(|c| c.path()).collect();
        assert_eq!(paths, vec!["a.txt", "new.txt"]);
    }

    /// The cache is an optimisation only: dropping it must not change the
    /// outcome, just the amount of work done.
    #[test]
    fn sync_is_correct_without_a_cache() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-nocache", None);
        make_tree(&dev.root, &[("a.txt", Some(b"content"))]);
        dev.sync(&mut vault, "laptop").unwrap();

        dev.cache.replace_all(&[]).unwrap();
        dev.cache.set_last_root(None).unwrap();

        let rep = dev.sync(&mut vault, "laptop").unwrap();
        assert!(rep.committed);
        assert!(dev.exists("a.txt"));
        assert_eq!(dev.read("a.txt"), b"content");
    }

    /// Identical content in two places must be stored once, not twice.
    #[test]
    fn duplicate_content_is_stored_once() {
        let mut vault = MemoryVault::new();
        let mut dev = Device::new("sync-dedup", None);
        let body = pseudo_random(51, 300 * 1024);
        make_tree(
            &dev.root,
            &[("one.bin", Some(&body)), ("two.bin", Some(&body))],
        );
        dev.sync(&mut vault, "laptop").unwrap();

        let with_dedup = vault.stored_bytes();
        assert!(
            with_dedup < body.len() * 2,
            "two copies of {} bytes should not cost {} bytes",
            body.len(),
            with_dedup
        );
    }
}
