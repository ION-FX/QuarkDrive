//! Turning a directory tree on disk into a Merkle tree.
//!
//! Walks the vault directory, chunks every file, and stores the resulting
//! nodes, returning the id of the root directory node.
//!
//! The file cache is consulted first: if a path's size, mtime and inode are
//! unchanged since the last sync, the previously computed node is reused and
//! the file is never opened. That turns a steady-state sync of a large vault
//! into a metadata-only operation.

use crate::chunker::chunk_reader;
use crate::hash::ObjectId;
use crate::index::FileCache;
use crate::object::ObjectStore;
use crate::tree::{ChunkRef, Node, NodeRef, TreeSet};
use anyhow::Result;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Directories and noise files that are never synced.
///
/// `.quarkdrive` holds the client's own state and must not be uploaded, or
/// every sync would invalidate the previous one.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".quarkdrive",
    ".git",
    ".svn",
    ".hg",
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    "*.swp",
    "*~",
];

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub excludes: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            excludes: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl ScanOptions {
    /// Exact name match, or a `*` wildcard at either end: `build*` matches a
    /// prefix, `*.tmp` and `*~` match a suffix.
    pub fn is_excluded(&self, name: &str) -> bool {
        self.excludes.iter().any(|pat| {
            if let Some(prefix) = pat.strip_suffix('*') {
                if !prefix.is_empty() {
                    return name.starts_with(prefix);
                }
            }
            if let Some(suffix) = pat.strip_prefix('*') {
                if !suffix.is_empty() {
                    return name.ends_with(suffix);
                }
            }
            pat == name
        })
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Files whose content did not need to be read.
    pub reused: u64,
    /// Files that were chunked.
    pub chunked: u64,
}

/// Split a `SystemTime` into whole seconds and nanoseconds since the epoch.
pub fn system_time_to_parts(t: std::time::SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        // Clamp pre-epoch timestamps rather than failing the scan.
        Err(_) => (0, 0),
    }
}

struct Scanner<'a> {
    trees: TreeSet<'a>,
    cache: Option<&'a FileCache>,
    opts: &'a ScanOptions,
    stats: ScanStats,
}

impl<'a> Scanner<'a> {
    fn scan_dir(&mut self, dir: &Path, rel: &Path) -> Result<ObjectId> {
        let mut entries = std::collections::BTreeMap::new();

        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if self.opts.is_excluded(&name) {
                continue;
            }
            let ftype = entry.file_type()?;
            names.push((name, ftype));
        }
        // Deterministic order keeps progress output and error reporting stable.
        names.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, ftype) in names {
            let path = dir.join(&name);
            let child_rel = if rel.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                rel.join(&name)
            };

            if ftype.is_symlink() {
                let target = std::fs::read_link(&path)?.to_string_lossy().to_string();
                let meta = std::fs::symlink_metadata(&path)?;
                let (mtime, mtime_nsec) = system_time_to_parts(meta.modified()?);
                let id = self.trees.put_node(&Node::Symlink {
                    target,
                    mtime,
                    mtime_nsec,
                })?;
                entries.insert(
                    name,
                    NodeRef {
                        id,
                        kind: crate::tree::Kind::Symlink,
                        size: 0,
                        mtime,
                    },
                );
                self.stats.files += 1;
                continue;
            }

            if ftype.is_dir() {
                let id = self.scan_dir(&path, &child_rel)?;
                entries.insert(
                    name,
                    NodeRef {
                        id,
                        kind: crate::tree::Kind::Dir,
                        size: 0,
                        mtime: 0,
                    },
                );
                self.stats.dirs += 1;
                continue;
            }

            if !ftype.is_file() {
                // Sockets, fifos and device nodes are not synced.
                continue;
            }

            let meta = std::fs::metadata(&path)?;
            let size = meta.len();
            let (mtime, mtime_nsec) = system_time_to_parts(meta.modified()?);
            let inode = meta.ino();
            let mode = meta.mode() & 0o7777;

            let (id, node) =
                self.scan_file(&path, &child_rel, size, mtime, mtime_nsec, mode, inode)?;
            entries.insert(name, node.to_ref(id));
            self.stats.files += 1;
            self.stats.bytes += size;
        }

        self.trees.put_node(&Node::Dir { entries }).map_err(Into::into)
    }

    fn scan_file(
        &mut self,
        path: &Path,
        rel: &Path,
        size: u64,
        mtime: i64,
        mtime_nsec: u32,
        mode: u32,
        inode: u64,
    ) -> Result<(ObjectId, Node)> {
        let rel_str = rel.to_string_lossy();

        // Fast path: reuse the node computed last time, but only if the
        // object it points at is still present in the store.
        if let Some(cache) = self.cache {
            if let Some(cached) = cache.get(&rel_str)? {
                if cached.size == size
                    && cached.mtime == mtime
                    && cached.mtime_nsec == mtime_nsec
                    && cached.inode == inode
                    && self.trees.store().has(&cached.node_id)
                {
                    if let Some(node) = self.trees.get_node(&cached.node_id)? {
                        self.stats.reused += 1;
                        return Ok((cached.node_id, node));
                    }
                }
            }
        }

        let mut chunks = Vec::new();
        let store = self.trees.store();
        let mut file = File::open(path)
            .map_err(|e| anyhow::anyhow!("cannot open {}: {}", path.display(), e))?;
        chunk_reader(&mut file, |chunk| {
            let (id, _new) = store
                .put(chunk)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            chunks.push(ChunkRef {
                id,
                len: chunk.len() as u32,
            });
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("chunking {}: {}", path.display(), e))?;

        let node = Node::File {
            size,
            mtime,
            mtime_nsec,
            mode,
            chunks,
        };
        let id = self.trees.put_node(&node)?;
        self.stats.chunked += 1;
        Ok((id, node))
    }
}

/// Scan `root` and return the id of its root directory node.
pub fn scan_tree(
    root: &Path,
    store: &ObjectStore,
    cache: Option<&FileCache>,
    opts: &ScanOptions,
) -> Result<(ObjectId, ScanStats)> {
    let trees = TreeSet::new(store);
    let mut scanner = Scanner {
        trees,
        cache,
        opts,
        stats: ScanStats::default(),
    };
    let id = scanner.scan_dir(root, Path::new(""))?;
    // scan_dir counts the directories it descends into, which excludes the
    // root itself.
    scanner.stats.dirs += 1;
    Ok((id, scanner.stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{make_tree, pseudo_random, TempDir};

    fn scan(dir: &Path, store: &ObjectStore, cache: Option<&FileCache>) -> (ObjectId, ScanStats) {
        scan_tree(dir, store, cache, &ScanOptions::default()).unwrap()
    }

    #[test]
    fn scans_files_and_directories() {
        let dir = TempDir::new("scan-basic");
        make_tree(
            dir.path(),
            &[
                ("a.txt", Some(b"hello")),
                ("sub", None),
                ("sub/b.txt", Some(b"world")),
            ],
        );
        let store_dir = TempDir::new("scan-basic-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, stats) = scan(dir.path(), &store, None);
        let trees = TreeSet::new(&store);

        assert_eq!(stats.files, 2);
        assert_eq!(stats.dirs, 2, "sub and root");
        assert_eq!(stats.bytes, 10);
        assert_eq!(stats.chunked, 2);

        let listing = trees.walk(&root).unwrap();
        let paths: Vec<&str> = listing.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);

        let node = trees.lookup(&root, "sub/b.txt").unwrap().unwrap();
        assert_eq!(trees.read_file(&node).unwrap(), b"world");
    }

    #[test]
    fn empty_directory_scans_to_empty_dir_node() {
        let dir = TempDir::new("scan-empty");
        let store_dir = TempDir::new("scan-empty-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, stats) = scan(dir.path(), &store, None);
        assert_eq!(stats.files, 0);
        let trees = TreeSet::new(&store);
        assert_eq!(trees.get_node(&root).unwrap().unwrap().as_dir().unwrap().len(), 0);
    }

    /// The whole point of the cache: a second scan must not re-read content.
    #[test]
    fn unchanged_files_are_not_rechunked() {
        let dir = TempDir::new("scan-cache");
        make_tree(
            dir.path(),
            &[
                ("one.txt", Some(&pseudo_random(1, 200_000))),
                ("two.txt", Some(&pseudo_random(2, 200_000))),
            ],
        );
        let store_dir = TempDir::new("scan-cache-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();

        let (root1, first) = scan(dir.path(), &store, None);
        assert_eq!(first.chunked, 2);
        assert_eq!(first.reused, 0);

        // Populate a cache the way sync does after a successful commit.
        let cache = FileCache::open_in_memory().unwrap();
        let trees = TreeSet::new(&store);
        let mut entries = Vec::new();
        for (path, r) in trees.walk(&root1).unwrap() {
            let meta = std::fs::metadata(dir.path().join(&path)).unwrap();
            let (mtime, mtime_nsec) = system_time_to_parts(meta.modified().unwrap());
            entries.push((
                path,
                crate::index::CacheEntry {
                    size: meta.len(),
                    mtime,
                    mtime_nsec,
                    inode: meta.ino(),
                    node_id: r.id,
                },
            ));
        }
        cache.replace_all(&entries).unwrap();

        let (root2, second) = scan(dir.path(), &store, Some(&cache));
        assert_eq!(second.reused, 2, "both files should come from cache");
        assert_eq!(second.chunked, 0);
        assert_eq!(root1, root2, "cached scan must produce the same tree");
    }

    #[test]
    fn modified_file_is_rechunked() {
        let dir = TempDir::new("scan-modify");
        make_tree(dir.path(), &[("f.txt", Some(b"original"))]);
        let store_dir = TempDir::new("scan-modify-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();

        let (root1, _) = scan(dir.path(), &store, None);

        let cache = FileCache::open_in_memory().unwrap();
        let trees = TreeSet::new(&store);
        let mut entries = Vec::new();
        for (path, r) in trees.walk(&root1).unwrap() {
            let meta = std::fs::metadata(dir.path().join(&path)).unwrap();
            let (mtime, mtime_nsec) = system_time_to_parts(meta.modified().unwrap());
            entries.push((
                path,
                crate::index::CacheEntry {
                    size: meta.len(),
                    mtime,
                    mtime_nsec,
                    inode: meta.ino(),
                    node_id: r.id,
                },
            ));
        }
        cache.replace_all(&entries).unwrap();

        std::fs::write(dir.path().join("f.txt"), b"changed content").unwrap();
        let (root2, stats) = scan(dir.path(), &store, Some(&cache));
        assert_eq!(stats.chunked, 1);
        assert_eq!(stats.reused, 0);
        assert_ne!(root1, root2);
    }

    #[test]
    fn excludes_are_respected() {
        let dir = TempDir::new("scan-exclude");
        make_tree(
            dir.path(),
            &[
                ("keep.txt", Some(b"k")),
                (".git/config", Some(b"g")),
                (".DS_Store", Some(b"d")),
                ("notes.txt~", Some(b"backup")),
            ],
        );
        let store_dir = TempDir::new("scan-exclude-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, _) = scan(dir.path(), &store, None);
        let trees = TreeSet::new(&store);
        let listing = trees.walk(&root).unwrap();
        let paths: Vec<&str> = listing.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt"]);
    }

    #[test]
    fn symlinks_are_recorded_not_followed() {
        let dir = TempDir::new("scan-symlink");
        make_tree(dir.path(), &[("target.txt", Some(b"data"))]);
        std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
        let store_dir = TempDir::new("scan-symlink-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, _) = scan(dir.path(), &store, None);
        let trees = TreeSet::new(&store);
        let node = trees.lookup(&root, "link.txt").unwrap().unwrap();
        match node {
            Node::Symlink { target, .. } => assert_eq!(target, "target.txt"),
            other => panic!("expected symlink node, got {:?}", other.kind()),
        }
    }

    #[test]
    fn empty_file_produces_file_node_with_no_chunks() {
        let dir = TempDir::new("scan-emptyfile");
        make_tree(dir.path(), &[("empty", Some(b""))]);
        let store_dir = TempDir::new("scan-emptyfile-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, stats) = scan(dir.path(), &store, None);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.bytes, 0);
        let trees = TreeSet::new(&store);
        let node = trees.lookup(&root, "empty").unwrap().unwrap();
        assert!(node.as_file().unwrap().is_empty());
        assert!(trees.read_file(&node).unwrap().is_empty());
    }

    /// Content-defined chunking means appending to a file must not invalidate
    /// the chunks that came before it.
    #[test]
    fn appended_data_reuses_earlier_chunks() {
        let dir = TempDir::new("scan-append");
        let body = pseudo_random(11, 400_000);
        make_tree(dir.path(), &[("log", Some(&body))]);
        let store_dir = TempDir::new("scan-append-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root1, _) = scan(dir.path(), &store, None);
        let trees = TreeSet::new(&store);
        let before = trees
            .lookup(&root1, "log")
            .unwrap()
            .unwrap()
            .as_file()
            .unwrap()
            .clone();

        let mut grown = body.clone();
        grown.extend_from_slice(&pseudo_random(12, 200_000));
        std::fs::write(dir.path().join("log"), &grown).unwrap();
        let (root2, _) = scan(dir.path(), &store, None);
        let after = trees
            .lookup(&root2, "log")
            .unwrap()
            .unwrap()
            .as_file()
            .unwrap()
            .clone();

        assert!(after.len() > before.len());
        let shared = before
            .iter()
            .filter(|c| after.iter().any(|c2| c2.id == c.id))
            .count();
        assert!(
            shared >= before.len() - 2,
            "expected nearly all original chunks to survive, shared {} of {}",
            shared,
            before.len()
        );
    }

    #[test]
    fn identical_content_in_two_paths_is_stored_once() {
        let dir = TempDir::new("scan-dedup");
        let body = pseudo_random(21, 100_000);
        make_tree(
            dir.path(),
            &[("a.bin", Some(&body)), ("copy.bin", Some(&body))],
        );
        let store_dir = TempDir::new("scan-dedup-store");
        let store = ObjectStore::open(store_dir.path(), None).unwrap();
        let (root, _) = scan(dir.path(), &store, None);
        let trees = TreeSet::new(&store);

        let a = trees.lookup(&root, "a.bin").unwrap().unwrap();
        let b = trees.lookup(&root, "copy.bin").unwrap().unwrap();
        let ids_a: Vec<_> = a.as_file().unwrap().iter().map(|c| c.id).collect();
        let ids_b: Vec<_> = b.as_file().unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids_a, ids_b, "identical files must share chunk objects");
        // Beyond the shared chunks, only the file node(s) and the root
        // directory node should exist. (Both files may even collapse to a
        // single node if their metadata matches too, so this is an upper bound.)
        let objects = store.ids().unwrap().len();
        let upper_bound = ids_a.len() + 3;
        assert!(
            objects <= upper_bound,
            "expected at most {upper_bound} objects ({} chunks + 2 file nodes + 1 dir), got {objects}",
            ids_a.len()
        );
    }
}
