//! Local sync state: the file cache.
//!
//! Chunking every file on every sync would make syncing a large vault
//! unusably slow. The cache records, for each path, the inode/mtime/size
//! signature that was last synced along with the tree node it produced. If
//! those three still match on disk, the file has not changed and its node id
//! is reused without reading a single byte of content.
//!
//! The inode matters: without it, rewriting a file within the same second
//! (and leaving size and mtime identical) would be silently missed.
//!
//! The cache is a pure optimisation. Deleting it costs a full re-chunk, never
//! correctness.

use crate::hash::ObjectId;
use anyhow::Result;
use rusqlite::{params, Connection};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS file_cache (
    path       TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    mtime      INTEGER NOT NULL,
    mtime_nsec INTEGER NOT NULL,
    inode      INTEGER NOT NULL,
    node_id    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// What we last knew about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEntry {
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub inode: u64,
    pub node_id: ObjectId,
}

pub struct FileCache {
    conn: Connection,
}

impl FileCache {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // WAL keeps the daemon responsive while a sync is writing.
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        Ok(FileCache { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(FileCache { conn })
    }

    pub fn get(&self, path: &str) -> Result<Option<CacheEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT size, mtime, mtime_nsec, inode, node_id FROM file_cache WHERE path = ?1",
        )?;
        let mut rows = stmt.query(params![path])?;
        match rows.next()? {
            Some(row) => Ok(Some(CacheEntry {
                size: row.get::<_, i64>(0)? as u64,
                mtime: row.get(1)?,
                mtime_nsec: row.get::<_, i64>(2)? as u32,
                inode: row.get::<_, i64>(3)? as u64,
                node_id: ObjectId::from_hex(&row.get::<_, String>(4)?)?,
            })),
            None => Ok(None),
        }
    }

    /// Replace the whole cache in one transaction.
    ///
    /// Called after a successful sync, when the on-disk state and the
    /// committed tree are known to agree.
    pub fn replace_all(&self, entries: &[(String, CacheEntry)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch("DELETE FROM file_cache")?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO file_cache
                 (path, size, mtime, mtime_nsec, inode, node_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (path, e) in entries {
                stmt.execute(params![
                    path,
                    e.size as i64,
                    e.mtime,
                    e.mtime_nsec as i64,
                    e.inode as i64,
                    e.node_id.to_hex(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_cache WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn len(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM file_cache", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    // ------------------------------------------------------------- metadata

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Root of the tree both sides agreed on after the last successful sync.
    /// This is the common ancestor for the next three-way merge.
    pub fn last_root(&self) -> Result<Option<ObjectId>> {
        match self.meta_get("last_root")? {
            Some(s) if !s.is_empty() => Ok(Some(ObjectId::from_hex(&s)?)),
            _ => Ok(None),
        }
    }

    pub fn set_last_root(&self, id: Option<ObjectId>) -> Result<()> {
        self.meta_set("last_root", &id.map(|i| i.to_hex()).unwrap_or_default())
    }

    pub fn vault_id(&self) -> Result<Option<String>> {
        self.meta_get("vault_id")
    }

    pub fn set_vault_id(&self, id: &str) -> Result<()> {
        self.meta_set("vault_id", id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn entry(byte: u8) -> CacheEntry {
        CacheEntry {
            size: 10,
            mtime: 1000,
            mtime_nsec: 7,
            inode: 42,
            node_id: ObjectId::hash(&[byte]),
        }
    }

    #[test]
    fn put_get_round_trips() {
        let dir = TempDir::new("index-roundtrip");
        let cache = FileCache::open(&dir.path().join("index.db")).unwrap();
        let e = entry(1);
        cache.replace_all(&[("a/b.txt".to_string(), e)]).unwrap();
        assert_eq!(cache.get("a/b.txt").unwrap().unwrap(), e);
        assert!(cache.get("missing").unwrap().is_none());
    }

    #[test]
    fn replace_all_discards_stale_paths() {
        let dir = TempDir::new("index-replace");
        let cache = FileCache::open(&dir.path().join("index.db")).unwrap();
        cache
            .replace_all(&[("old.txt".into(), entry(1)), ("keep.txt".into(), entry(2))])
            .unwrap();
        assert_eq!(cache.len().unwrap(), 2);
        cache.replace_all(&[("keep.txt".into(), entry(2))]).unwrap();
        assert_eq!(cache.len().unwrap(), 1);
        assert!(cache.get("old.txt").unwrap().is_none());
    }

    #[test]
    fn remove_and_count() {
        let dir = TempDir::new("index-remove");
        let cache = FileCache::open(&dir.path().join("index.db")).unwrap();
        cache
            .replace_all(&[("x".into(), entry(1)), ("y".into(), entry(2))])
            .unwrap();
        assert!(!cache.is_empty().unwrap());
        cache.remove("x").unwrap();
        assert_eq!(cache.len().unwrap(), 1);
    }

    #[test]
    fn last_root_persists_and_clears() {
        let dir = TempDir::new("index-root");
        let cache = FileCache::open(&dir.path().join("index.db")).unwrap();
        assert!(cache.last_root().unwrap().is_none());
        let id = ObjectId::hash(b"tree");
        cache.set_last_root(Some(id)).unwrap();
        assert_eq!(cache.last_root().unwrap(), Some(id));
        cache.set_last_root(None).unwrap();
        assert!(cache.last_root().unwrap().is_none());
    }

    #[test]
    fn meta_overwrites() {
        let dir = TempDir::new("index-meta");
        let cache = FileCache::open(&dir.path().join("index.db")).unwrap();
        assert!(cache.meta_get("k").unwrap().is_none());
        cache.meta_set("k", "first").unwrap();
        cache.meta_set("k", "second").unwrap();
        assert_eq!(cache.meta_get("k").unwrap().unwrap(), "second");
    }

    #[test]
    fn survives_reopen() {
        let dir = TempDir::new("index-reopen");
        let path = dir.path().join("index.db");
        {
            let cache = FileCache::open(&path).unwrap();
            cache.replace_all(&[("f".into(), entry(9))]).unwrap();
            cache.set_vault_id("vault-1").unwrap();
        }
        let cache = FileCache::open(&path).unwrap();
        assert_eq!(cache.get("f").unwrap().unwrap(), entry(9));
        assert_eq!(cache.vault_id().unwrap().unwrap(), "vault-1");
    }

    #[test]
    fn in_memory_cache_works() {
        let cache = FileCache::open_in_memory().unwrap();
        cache.replace_all(&[("m".into(), entry(3))]).unwrap();
        assert_eq!(cache.len().unwrap(), 1);
    }
}
