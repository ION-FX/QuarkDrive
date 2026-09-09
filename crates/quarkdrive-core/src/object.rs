//! The content-addressed object store.
//!
//! Everything Quarkdrive stores — file chunks, directory nodes, snapshots —
//! is an *object*, filed under its own BLAKE3 hash. Writes go to
//! `<root>/<first 2 hex chars>/<remaining 62>` so that a vault with millions
//! of objects does not put millions of entries in one directory.
//!
//! The store is the trust boundary. Because an object's name *is* the hash of
//! its plaintext, `decode` re-hashes whatever it reads and refuses to return
//! anything that does not match. A malicious or merely buggy server cannot
//! hand a client data it did not originally write, whether or not the vault is
//! encrypted.

use crate::crypto::{self, Key};
use crate::hash::ObjectId;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAGIC: u8 = b'Q';
pub const FORMAT_VERSION: u8 = 1;
pub const FLAG_ENCRYPTED: u8 = 1 << 0;
pub const FLAG_COMPRESSED: u8 = 1 << 1;
const HEADER_LEN: usize = 3;

/// Below this size, compression is not worth attempting.
const COMPRESS_MIN_LEN: usize = 512;

/// zstd level. Low enough that compression is not the bottleneck on sync.
const COMPRESS_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("object {0} is truncated ({1} bytes)")]
    Truncated(ObjectId, usize),
    #[error("object {0} has a bad header (not a Quarkdrive object, or wrong format version)")]
    BadHeader(ObjectId),
    #[error("object {0} failed verification: content does not hash to its id")]
    HashMismatch(ObjectId),
    #[error("object {0} is encrypted but no vault key was supplied")]
    MissingKey(ObjectId),
    #[error("object {0} is not encrypted but a vault key was supplied")]
    UnexpectedKey(ObjectId),
    #[error("decompression failed for {0}: {1}")]
    Decompress(ObjectId, String),
    #[error("crypto failure for {0}: {1}")]
    Crypto(ObjectId, String),
}

pub type Result<T> = std::result::Result<T, ObjectError>;

/// On-disk (and on-wire) object representation.
pub struct ObjectStore {
    root: PathBuf,
    key: Option<Key>,
}

impl ObjectStore {
    pub fn open(root: impl AsRef<Path>, key: Option<Key>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("tmp"))?;
        Ok(ObjectStore { root, key })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
    }

    /// Two-level fanout path for an object.
    pub fn path_for(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Encode plaintext into the stored representation, returning its id.
    ///
    /// Compression is applied first (it can only help on real plaintext) and
    /// is skipped when it does not actually save space, which is the common
    /// case for already-compressed media.
    pub fn encode(&self, plain: &[u8]) -> Result<(ObjectId, Vec<u8>)> {
        let id = ObjectId::hash(plain);

        let mut flags = 0u8;
        let mut body: Vec<u8> = plain.to_vec();

        if plain.len() >= COMPRESS_MIN_LEN {
            match zstd::encode_all(plain, COMPRESS_LEVEL) {
                Ok(compressed) if compressed.len() < body.len() => {
                    body = compressed;
                    flags |= FLAG_COMPRESSED;
                }
                Ok(_) => {}
                Err(e) => return Err(ObjectError::Decompress(id, e.to_string())),
            }
        }

        if let Some(key) = &self.key {
            body = crypto::seal(&body, &id, key).map_err(|e| ObjectError::Crypto(id, e.to_string()))?;
            flags |= FLAG_ENCRYPTED;
        }

        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.push(MAGIC);
        out.push(FORMAT_VERSION);
        out.push(flags);
        out.extend_from_slice(&body);
        Ok((id, out))
    }

    /// Reverse of [`ObjectStore::encode`]. Verifies the content hash.
    pub fn decode(&self, id: &ObjectId, encoded: &[u8]) -> Result<Vec<u8>> {
        if encoded.len() < HEADER_LEN {
            return Err(ObjectError::Truncated(*id, encoded.len()));
        }
        if encoded[0] != MAGIC || encoded[1] != FORMAT_VERSION {
            return Err(ObjectError::BadHeader(*id));
        }
        let flags = encoded[2];
        let mut body: &[u8] = &encoded[HEADER_LEN..];

        let decrypted: Vec<u8>;
        if flags & FLAG_ENCRYPTED != 0 {
            match &self.key {
                Some(key) => {
                    decrypted = crypto::open_sealed(body, id, key)
                        .map_err(|e| ObjectError::Crypto(*id, e.to_string()))?;
                    body = &decrypted;
                }
                None => return Err(ObjectError::MissingKey(*id)),
            }
        } else if self.key.is_some() {
            return Err(ObjectError::UnexpectedKey(*id));
        }

        let plain: Vec<u8>;
        if flags & FLAG_COMPRESSED != 0 {
            plain = zstd::decode_all(body).map_err(|e| ObjectError::Decompress(*id, e.to_string()))?;
        } else {
            plain = body.to_vec();
        }

        // The whole point of content addressing: nothing leaves the store
        // unless it actually hashes to the name it was filed under.
        let actual = ObjectId::hash(&plain);
        if actual != *id {
            return Err(ObjectError::HashMismatch(*id));
        }
        Ok(plain)
    }

    pub fn has(&self, id: &ObjectId) -> bool {
        self.path_for(id).exists()
    }

    /// Read and verify an object. `None` if absent.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        match self.get_encoded(id)? {
            Some(encoded) => Ok(Some(self.decode(id, &encoded)?)),
            None => Ok(None),
        }
    }

    /// Raw stored bytes, without decoding. Used to serve uploads/downloads.
    pub fn get_encoded(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ObjectError::Io(e)),
        }
    }

    /// Store plaintext, returning its id and whether it was newly written.
    pub fn put(&self, plain: &[u8]) -> Result<(ObjectId, bool)> {
        let (id, encoded) = self.encode(plain)?;
        let is_new = self.put_encoded(&id, &encoded)?;
        Ok((id, is_new))
    }

    /// Store already-encoded bytes under `id`.
    ///
    /// The bytes are decoded and verified before being written, so a peer
    /// cannot poison the store by uploading garbage under some id. Returns
    /// whether the object was newly written.
    pub fn put_encoded(&self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
        // Verify first: refuse to persist anything that does not hash to `id`.
        self.decode(id, encoded)?;
        if self.has(id) {
            return Ok(false);
        }
        self.write_atomic(&self.path_for(id), encoded)?;
        Ok(true)
    }

    /// Store already-encoded bytes without verifying them against `id`.
    ///
    /// A server hosting an end-to-end encrypted vault cannot decode what it
    /// holds, so it cannot verify uploads the way [`ObjectStore::put_encoded`]
    /// does. It still checks the header, so it cannot be used as a
    /// general-purpose blob store. Integrity is enforced where it matters —
    /// every client holding the key verifies on read.
    pub fn put_opaque(&self, id: &ObjectId, encoded: &[u8]) -> Result<bool> {
        if encoded.len() < HEADER_LEN || encoded[0] != MAGIC || encoded[1] != FORMAT_VERSION {
            return Err(ObjectError::BadHeader(*id));
        }
        if self.has(id) {
            return Ok(false);
        }
        self.write_atomic(&self.path_for(id), encoded)?;
        Ok(true)
    }

    fn write_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self
            .root
            .join("tmp")
            .join(format!("{}.tmp", ObjectId::hash(data).short()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            // Durability matters more than throughput here: a crash between
            // write and rename must not leave a half-written object behind.
            f.sync_all()?;
        }
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // Another writer may have won the race; that is fine.
                if path.exists() {
                    Ok(())
                } else {
                    Err(ObjectError::Io(e))
                }
            }
        }
    }

    pub fn delete(&self, id: &ObjectId) -> Result<bool> {
        match fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ObjectError::Io(e)),
        }
    }

    /// Every object id in the store.
    pub fn ids(&self) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() || shard.file_name() == "tmp" {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let hex = shard.file_name().to_string_lossy().to_string() + &name;
                if let Ok(id) = ObjectId::from_hex(&hex) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Total bytes occupied by stored objects.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0u64;
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() || shard.file_name() == "tmp" {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    total += entry.metadata()?.len();
                }
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn put_then_get_round_trips() {
        let dir = TempDir::new("store-roundtrip");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (id, is_new) = store.put(b"hello object store").unwrap();
        assert!(is_new);
        assert!(store.has(&id));
        assert_eq!(store.get(&id).unwrap().unwrap(), b"hello object store");
    }

    /// Deduplication falls out of content addressing for free.
    #[test]
    fn identical_content_is_stored_once() {
        let dir = TempDir::new("store-dedup");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (a, new_a) = store.put(b"duplicate me").unwrap();
        let (b, new_b) = store.put(b"duplicate me").unwrap();
        assert_eq!(a, b);
        assert!(new_a);
        assert!(!new_b, "second put should be a no-op");
        assert_eq!(store.ids().unwrap().len(), 1);
    }

    #[test]
    fn missing_object_is_none_not_error() {
        let dir = TempDir::new("store-missing");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        assert!(store.get(&ObjectId::hash(b"absent")).unwrap().is_none());
    }

    #[test]
    fn corrupted_bytes_are_rejected() {
        let dir = TempDir::new("store-corrupt");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (id, _) = store.put(b"integrity matters").unwrap();
        let mut encoded = store.get_encoded(&id).unwrap().unwrap();
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF;
        assert!(matches!(
            store.decode(&id, &encoded),
            Err(ObjectError::HashMismatch(_))
        ));
    }

    #[test]
    fn rejects_bad_header_and_truncation() {
        let dir = TempDir::new("store-header");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let id = ObjectId::hash(b"x");
        assert!(matches!(
            store.decode(&id, &[1, 2]),
            Err(ObjectError::Truncated(_, 2))
        ));
        let mut bad = store.encode(b"x").unwrap().1;
        bad[0] = b'Z';
        assert!(matches!(
            store.decode(&id, &bad),
            Err(ObjectError::BadHeader(_))
        ));
    }

    #[test]
    fn upload_under_wrong_id_is_refused() {
        let dir = TempDir::new("store-poison");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (_id, encoded) = store.encode(b"real content").unwrap();
        let claimed = ObjectId::hash(b"something else");
        assert!(store.put_encoded(&claimed, &encoded).is_err());
        assert!(!store.has(&claimed), "poisoned object must not be stored");
    }

    #[test]
    fn encrypted_store_hides_plaintext() {
        let dir = TempDir::new("store-e2ee");
        let key = Key::generate();
        let store = ObjectStore::open(dir.path(), Some(key.clone())).unwrap();
        let (id, _) = store.put(b"confidential").unwrap();
        let raw = fs::read(store.path_for(&id)).unwrap();
        assert!(
            !raw.windows(12).any(|w| w == b"confidential"),
            "plaintext leaked into stored bytes"
        );
        assert_eq!(store.get(&id).unwrap().unwrap(), b"confidential");
    }

    /// Two devices with the same key must produce byte-identical stored
    /// objects, which is what lets the server deduplicate across them.
    #[test]
    fn encrypted_objects_are_convergent() {
        let key = Key::generate();
        let dir_a = TempDir::new("conv-a");
        let dir_b = TempDir::new("conv-b");
        let a = ObjectStore::open(dir_a.path(), Some(key.clone())).unwrap();
        let b = ObjectStore::open(dir_b.path(), Some(key)).unwrap();
        let (ida, ea) = a.encode(b"same bytes both devices").unwrap();
        let (idb, eb) = b.encode(b"same bytes both devices").unwrap();
        assert_eq!(ida, idb);
        assert_eq!(ea, eb, "ciphertext must match for dedup to work");
    }

    #[test]
    fn wrong_key_cannot_read() {
        let dir = TempDir::new("store-wrongkey");
        let store = ObjectStore::open(dir.path(), Some(Key::generate())).unwrap();
        let (id, _) = store.put(b"locked").unwrap();
        let other = ObjectStore::open(dir.path(), Some(Key::generate())).unwrap();
        assert!(other.get(&id).is_err());
    }

    #[test]
    fn plain_store_rejects_keyed_access_and_vice_versa() {
        let dir = TempDir::new("store-keymix");
        let plain = ObjectStore::open(dir.path(), None).unwrap();
        let (id, _) = plain.put(b"unencrypted").unwrap();
        let keyed = ObjectStore::open(dir.path(), Some(Key::generate())).unwrap();
        assert!(keyed.get(&id).is_err(), "keyed store must not read plaintext objects");
    }

    #[test]
    fn large_compressible_and_incompressible_payloads() {
        let dir = TempDir::new("store-sizes");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let compressible = vec![7u8; 512 * 1024];
        let (id, _) = store.put(&compressible).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap(), compressible);

        let mut noise = Vec::with_capacity(64 * 1024);
        let mut s = 0x12345678u64;
        for _ in 0..64 * 1024 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            noise.push((s >> 24) as u8);
        }
        let (id2, _) = store.put(&noise).unwrap();
        assert_eq!(store.get(&id2).unwrap().unwrap(), noise);
    }

    #[test]
    fn empty_and_tiny_objects() {
        let dir = TempDir::new("store-empty");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (id, _) = store.put(b"").unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap(), b"");
        let (id2, _) = store.put(b"a").unwrap();
        assert_eq!(store.get(&id2).unwrap().unwrap(), b"a");
    }

    #[test]
    fn delete_removes_object() {
        let dir = TempDir::new("store-delete");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        let (id, _) = store.put(b"goodbye").unwrap();
        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap());
        assert!(!store.has(&id));
    }

    #[test]
    fn ids_and_total_size_cover_every_object() {
        let dir = TempDir::new("store-listing");
        let store = ObjectStore::open(dir.path(), None).unwrap();
        for i in 0..25u32 {
            store.put(format!("object {i}").as_bytes()).unwrap();
        }
        assert_eq!(store.ids().unwrap().len(), 25);
        assert!(store.total_size().unwrap() > 0);
    }
}
