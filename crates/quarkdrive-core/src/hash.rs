//! Content addressing.
//!
//! Every object in Quarkdrive — a file chunk, a directory listing, a snapshot —
//! is identified by the BLAKE3 hash of its own canonical plaintext bytes.
//!
//! Two consequences drive the whole design:
//!
//! * **Deduplication is automatic.** Identical bytes anywhere in the vault,
//!   from any device, hash to the same id, so they are stored and transferred
//!   once.
//! * **The store is self-verifying.** Because the id *is* the hash, any object
//!   can be re-hashed on read to detect corruption or tampering by the server.
//!   An untrusted server cannot silently alter data it cannot decrypt.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Length of a BLAKE3 digest in bytes.
pub const OBJECT_ID_LEN: usize = 32;

/// A content address: 32 raw bytes of BLAKE3 output.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; OBJECT_ID_LEN]);

impl ObjectId {
    /// The all-zero id, used as a sentinel for "no object".
    pub const ZERO: ObjectId = ObjectId([0u8; OBJECT_ID_LEN]);

    pub fn from_bytes(bytes: [u8; OBJECT_ID_LEN]) -> Self {
        ObjectId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; OBJECT_ID_LEN] {
        &self.0
    }

    /// Hash a buffer and return its content address.
    pub fn hash(data: &[u8]) -> Self {
        ObjectId(*blake3::hash(data).as_bytes())
    }

    /// BLAKE3 in keyed mode. Used to derive per-object encryption material.
    pub fn keyed_hash(key: &[u8; OBJECT_ID_LEN], data: &[u8]) -> Self {
        ObjectId(*blake3::keyed_hash(key, data).as_bytes())
    }

    pub fn from_hex(s: &str) -> Result<Self, HexError> {
        if s.len() != OBJECT_ID_LEN * 2 {
            return Err(HexError::Length(s.len()));
        }
        let mut out = [0u8; OBJECT_ID_LEN];
        hex::decode_to_slice(s, &mut out).map_err(HexError::Invalid)?;
        Ok(ObjectId(out))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 8 hex characters, for logs and progress output.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; OBJECT_ID_LEN]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HexError {
    #[error("object id must be 64 hex characters, got {0}")]
    Length(usize),
    #[error("object id is not valid hex: {0}")]
    Invalid(#[from] hex::FromHexError),
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "oid:{}", self.short())
    }
}

// Object ids cross the wire inside JSON bodies, where a hex string is far more
// debuggable than an array of numbers.
impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        ObjectId::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// Streaming hasher for large buffers.
pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Hasher(blake3::Hasher::new())
    }

    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.0.update(data);
        self
    }

    pub fn finalize(&self) -> ObjectId {
        ObjectId(*self.0.finalize().as_bytes())
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex_round_trips() {
        let id = ObjectId::hash(b"hello quarkdrive");
        assert_eq!(id.to_hex().len(), 64);
        assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
        assert_eq!(ObjectId::hash(b"hello quarkdrive"), id);
        assert_ne!(ObjectId::hash(b"hello quarkdrivf"), id);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(ObjectId::from_hex("").is_err());
        assert!(ObjectId::from_hex("zz").is_err());
        // 64 chars but not hex.
        assert!(ObjectId::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn streaming_matches_one_shot() {
        let mut h = Hasher::new();
        h.update(b"hello ");
        h.update(b"quarkdrive");
        assert_eq!(h.finalize(), ObjectId::hash(b"hello quarkdrive"));
    }

    #[test]
    fn serde_uses_hex() {
        let id = ObjectId::hash(b"x");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.to_hex()));
        assert_eq!(serde_json::from_str::<ObjectId>(&json).unwrap(), id);
    }
}
