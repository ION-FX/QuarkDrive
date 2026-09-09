//! Convergent end-to-end encryption.
//!
//! Quarkdrive can run a vault in one of two modes:
//!
//! * **Plain** — the server stores objects as-is. It can therefore do
//!   server-side work: thumbnails, EXIF timelines, and the simple file API
//!   that the web UI and the Android client use.
//! * **End-to-end encrypted** — the server only ever sees ciphertext and
//!   cannot read filenames or contents.
//!
//! # Why convergent
//!
//! The obvious way to encrypt an object is with a fresh random nonce. That
//! breaks deduplication between devices: two clients holding the same file
//! would produce different ciphertext for the same content address, and the
//! server would have to keep both copies.
//!
//! So instead the key *and* the nonce are derived deterministically from the
//! object's own content address. Identical plaintext therefore yields byte-
//! identical ciphertext on every device, and cross-device deduplication keeps
//! working even though the server is blind.
//!
//! **The tradeoff matters and is deliberate:** deterministic encryption means
//! anyone who can guess a file's contents can confirm whether that file is
//! present in the vault (a "confirmation of file" attack). It does not let
//! them decrypt anything they do not already have. Deduplication and
//! server-blindness cannot both be had without accepting this, so it is
//! offered as a choice rather than imposed.

use crate::hash::{ObjectId, OBJECT_ID_LEN};
use chacha20poly1305::aead::Payload;
use chacha20poly1305::{aead::Aead, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroize;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const SALT_LEN: usize = 16;

/// Argon2id parameters for deriving a vault key from a passphrase.
///
/// 64 MiB, 3 passes. Key derivation happens once when a client unlocks a
/// vault, so paying ~200 ms there is worth making offline guessing expensive.
const ARGON2_M_COST: u32 = 65_536; // KiB
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 1;

/// Weak parameters for tests, which would otherwise spend most of their time
/// in Argon2. Never used for real vaults.
pub const TEST_ARGON2_M_COST: u32 = 4096;
pub const TEST_ARGON2_T_COST: u32 = 1;

/// A 32-byte secret that is zeroed on drop.
#[derive(Clone)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Key(bytes)
    }

    /// Generate a fresh random key.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut b = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut b);
        Key(b)
    }

    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    pub fn as_bytes(&self) -> [u8; KEY_LEN] {
        self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// Derive a vault key from a passphrase with Argon2id.
pub fn derive_key_from_passphrase(passphrase: &[u8], salt: &[u8; SALT_LEN]) -> anyhow::Result<Key> {
    derive_key_with_params(passphrase, salt, ARGON2_M_COST, ARGON2_T_COST)
}

/// Same as [`derive_key_from_passphrase`] with explicit cost parameters.
pub fn derive_key_with_params(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
    m_cost: u32,
    t_cost: u32,
) -> anyhow::Result<Key> {
    let params = argon2::Params::new(m_cost, t_cost, ARGON2_P_COST, Some(KEY_LEN))
        .map_err(|e| anyhow::anyhow!("bad argon2 params: {e}"))?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon2
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| anyhow::anyhow!("argon2 failed: {e}"))?;
    Ok(Key::from_bytes(out))
}

/// Derive a domain-separated subkey from the vault key.
fn subkey(key: &Key, domain: &[u8]) -> [u8; OBJECT_ID_LEN] {
    *ObjectId::keyed_hash(key.expose(), domain).as_bytes()
}

const DOMAIN_OBJ_KEY: &[u8] = b"quarkdrive/v1/object-key";
const DOMAIN_OBJ_NONCE: &[u8] = b"quarkdrive/v1/object-nonce";

/// Deterministic per-object key.
fn object_key(key: &Key, id: &ObjectId) -> [u8; KEY_LEN] {
    *ObjectId::keyed_hash(&subkey(key, DOMAIN_OBJ_KEY), id.as_bytes()).as_bytes()
}

/// Deterministic 24-byte XChaCha20-Poly1305 nonce.
fn object_nonce(key: &Key, id: &ObjectId) -> [u8; NONCE_LEN] {
    let h = ObjectId::keyed_hash(&subkey(key, DOMAIN_OBJ_NONCE), id.as_bytes());
    let mut n = [0u8; NONCE_LEN];
    n.copy_from_slice(&h.as_bytes()[..NONCE_LEN]);
    n
}

/// Encrypt `plain`, binding the ciphertext to its content address.
///
/// Returns `nonce || ciphertext`, with `id` used as additional authenticated
/// data so a ciphertext cannot be relocated to a different object id.
pub fn seal(plain: &[u8], id: &ObjectId, key: &Key) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(&object_key(key, id))
        .map_err(|_| anyhow::anyhow!("invalid key length"))?;
    let nonce_bytes = object_nonce(key, id);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plain,
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Reverse of [`seal`]. Fails if the key is wrong or the data was altered.
pub fn open_sealed(sealed: &[u8], id: &ObjectId, key: &Key) -> anyhow::Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN {
        anyhow::bail!("sealed object too short: {} bytes", sealed.len());
    }
    let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new_from_slice(&object_key(key, id))
        .map_err(|_| anyhow::anyhow!("invalid key length"))?;
    let nonce = XNonce::from_slice(nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ct,
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong key or corrupted data)"))
}

/// Generate a random salt for passphrase derivation.
pub fn random_salt() -> [u8; SALT_LEN] {
    use rand::RngCore;
    let mut s = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let key = Key::generate();
        let plain = b"a chunk of file data";
        let id = ObjectId::hash(plain);
        let sealed = seal(plain, &id, &key).unwrap();
        assert_ne!(sealed, plain.as_slice());
        assert_eq!(open_sealed(&sealed, &id, &key).unwrap(), plain);
    }

    /// The property that keeps cross-device dedup working under E2EE.
    #[test]
    fn encryption_is_deterministic() {
        let key = Key::generate();
        let plain = b"identical bytes on two devices";
        let id = ObjectId::hash(plain);
        let a = seal(plain, &id, &key).unwrap();
        let b = seal(plain, &id, &key).unwrap();
        assert_eq!(a, b, "same plaintext must produce identical ciphertext");
    }

    #[test]
    fn different_objects_use_different_keys() {
        let key = Key::generate();
        let id1 = ObjectId::hash(b"one");
        let id2 = ObjectId::hash(b"two");
        let sealed = seal(b"one", &id1, &key).unwrap();
        // Right key material is only derivable for the object's own id.
        assert!(open_sealed(&sealed, &id2, &key).is_err());
        assert!(open_sealed(&sealed, &id1, &key).is_ok());
    }

    #[test]
    fn tampering_is_detected() {
        let key = Key::generate();
        let plain = b"do not modify me";
        let id = ObjectId::hash(plain);
        let mut sealed = seal(plain, &id, &key).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open_sealed(&sealed, &id, &key).is_err());
    }

    #[test]
    fn wrong_key_is_detected() {
        let plain = b"secret";
        let id = ObjectId::hash(plain);
        let sealed = seal(plain, &id, &Key::generate()).unwrap();
        assert!(open_sealed(&sealed, &id, &Key::generate()).is_err());
    }

    #[test]
    fn short_input_is_rejected_not_panicking() {
        let key = Key::generate();
        let id = ObjectId::hash(b"x");
        assert!(open_sealed(&[0u8; 5], &id, &key).is_err());
        assert!(open_sealed(&[], &id, &key).is_err());
    }

    #[test]
    fn passphrase_derivation_is_salt_dependent() {
        let salt = [7u8; SALT_LEN];
        let a = derive_key_with_params(b"correct horse", &salt, TEST_ARGON2_M_COST, TEST_ARGON2_T_COST).unwrap();
        let b = derive_key_with_params(b"correct horse", &salt, TEST_ARGON2_M_COST, TEST_ARGON2_T_COST).unwrap();
        let c = derive_key_with_params(b"correct horse", &[8u8; SALT_LEN], TEST_ARGON2_M_COST, TEST_ARGON2_T_COST).unwrap();
        let d = derive_key_with_params(b"wrong horse", &salt, TEST_ARGON2_M_COST, TEST_ARGON2_T_COST).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes(), "derivation must be deterministic");
        assert_ne!(a.as_bytes(), c.as_bytes(), "salt must change the key");
        assert_ne!(a.as_bytes(), d.as_bytes(), "passphrase must change the key");
    }

    #[test]
    fn key_debug_does_not_leak() {
        let k = Key::from_bytes([0xABu8; KEY_LEN]);
        assert_eq!(format!("{:?}", k), "Key(..)");
    }
}
