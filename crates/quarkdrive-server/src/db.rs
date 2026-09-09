//! Users, tokens and vault metadata.
//!
//! Passwords are hashed with Argon2id, reusing the key derivation in
//! `quarkdrive-core` so there is only one password-hashing implementation in
//! the project. Tokens are random 32-byte values; because they are looked up
//! in the database rather than verified cryptographically, they are revocable.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

use quarkdrive_core::crypto;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id        TEXT PRIMARY KEY,
    username  TEXT NOT NULL UNIQUE,
    pass_hash TEXT NOT NULL,
    pass_salt TEXT NOT NULL,
    created   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    token   TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    device  TEXT,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS vaults (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE,
    owner_id  TEXT NOT NULL,
    encrypted INTEGER NOT NULL DEFAULT 0,
    created   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_vaults_owner ON vaults(owner_id);
"#;

#[derive(Debug, Clone)]
pub struct VaultRow {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub encrypted: bool,
    pub created: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn random_id(bytes: usize) -> String {
        use rand::RngCore;
        let mut buf = vec![0u8; bytes];
        rand::thread_rng().fill_bytes(&mut buf);
        hex::encode(&buf)
    }

    pub fn count_users(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    pub fn create_user(&self, username: &str, password: &str) -> Result<String> {
        if username.trim().is_empty() {
            return Err(anyhow!("username must not be empty"));
        }
        if password.is_empty() {
            return Err(anyhow!("password must not be empty"));
        }
        let id = Self::random_id(16);
        let salt = crypto::random_salt();
        let hash = crypto::derive_key_from_passphrase(password.as_bytes(), &salt)?.as_bytes();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, username, pass_hash, pass_salt, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                username,
                hex::encode(hash),
                hex::encode(salt),
                Self::now()
            ],
        )
        .map_err(|e| anyhow!("username is already taken: {e}"))?;
        Ok(id)
    }

    /// First-run registration: create the first account together with a
    /// sign-in token and its first vault, atomically, so two simultaneous
    /// submissions cannot both claim "first". Returns (user_id, token,
    /// vault_id). Fails once any account exists — from then on accounts are
    /// made with `create-user`, keeping the server closed by default.
    pub fn register_first_user(
        &self,
        username: &str,
        password: &str,
        vault_name: &str,
    ) -> Result<(String, String, String)> {
        let username = username.trim();
        if username.is_empty() || username.len() > 40 {
            return Err(anyhow!("username must be 1-40 characters"));
        }
        if password.chars().count() < 6 {
            return Err(anyhow!("password must be at least 6 characters"));
        }
        let vault_name = vault_name.trim();
        if vault_name.is_empty() || vault_name.len() > 60 || vault_name.contains('/') {
            return Err(anyhow!("vault name must be 1-60 characters without '/'"));
        }

        let mut conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        if count > 0 {
            return Err(anyhow!(
                "an account already exists on this server; registration is closed"
            ));
        }

        let user_id = Self::random_id(16);
        let salt = crypto::random_salt();
        let hash = crypto::derive_key_from_passphrase(password.as_bytes(), &salt)?.as_bytes();
        let token = Self::random_id(32);
        let vault_id = Self::random_id(16);
        let now = Self::now();

        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO users (id, username, pass_hash, pass_salt, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user_id, username, hex::encode(hash), hex::encode(salt), now],
        )
        .map_err(|e| anyhow!("username is already taken: {e}"))?;
        tx.execute(
            "INSERT INTO tokens (token, user_id, device, created)
             VALUES (?1, ?2, ?3, ?4)",
            params![token, user_id, "first-run", now],
        )?;
        tx.execute(
            "INSERT INTO vaults (id, name, owner_id, encrypted, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![vault_id, vault_name, user_id, 0, now],
        )
        .map_err(|e| anyhow!("vault name is already taken: {e}"))?;
        tx.commit()?;
        Ok((user_id, token, vault_id))
    }

    /// Returns the user id if the credentials are valid.
    pub fn authenticate(&self, username: &str, password: &str) -> Result<Option<String>> {
        let (id, expected_hex, salt_hex) = {
            let conn = self.conn.lock().unwrap();
            let row = conn.query_row(
                "SELECT id, pass_hash, pass_salt FROM users WHERE username = ?1",
                params![username],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            );
            match row {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        };

        let salt_bytes = hex::decode(&salt_hex)?;
        if salt_bytes.len() != crypto::SALT_LEN {
            return Err(anyhow!("stored salt has the wrong length"));
        }
        let mut salt = [0u8; crypto::SALT_LEN];
        salt.copy_from_slice(&salt_bytes);

        let expected = hex::decode(&expected_hex)?;
        let actual = crypto::derive_key_from_passphrase(password.as_bytes(), &salt)?.as_bytes();

        // Constant-time comparison: leaking how much of the hash matched would
        // let an attacker guess a password byte at a time.
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(actual.iter()) {
            diff |= a ^ b;
        }
        if diff == 0 && expected.len() == actual.len() {
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    pub fn create_token(&self, user_id: &str, device: Option<&str>) -> Result<String> {
        let token = Self::random_id(32);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tokens (token, user_id, device, created) VALUES (?1, ?2, ?3, ?4)",
            params![token, user_id, device, Self::now()],
        )?;
        Ok(token)
    }

    pub fn user_for_token(&self, token: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT user_id FROM tokens WHERE token = ?1",
            params![token],
            |r| r.get::<_, String>(0),
        );
        match row {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve a username to a user id without checking a password.
    ///
    /// Used by administrative CLI commands that already have full access to
    /// the database, where demanding a password would be pointless ceremony.
    pub fn user_id_for_username(&self, username: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row("SELECT id FROM users WHERE username = ?1", params![username], |r| {
            r.get::<_, String>(0)
        });
        match row {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn username_for_id(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row("SELECT username FROM users WHERE id = ?1", params![id], |r| {
            r.get::<_, String>(0)
        });
        match row {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn revoke_token(&self, token: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM tokens WHERE token = ?1", params![token])?;
        Ok(n > 0)
    }

    // ---------------------------------------------------------------- vaults

    pub fn create_vault(&self, name: &str, owner_id: &str, encrypted: bool) -> Result<String> {
        if name.trim().is_empty() || name.contains('/') || name.contains("..") {
            return Err(anyhow!("invalid vault name"));
        }
        let id = Self::random_id(16);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO vaults (id, name, owner_id, encrypted, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, owner_id, encrypted as i64, Self::now()],
        )
        .map_err(|e| anyhow!("vault name is already taken: {e}"))?;
        Ok(id)
    }

    pub fn vault_by_name(&self, name: &str) -> Result<Option<VaultRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT id, name, owner_id, encrypted, created FROM vaults WHERE name = ?1",
            params![name],
            |r| {
                Ok(VaultRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner_id: r.get(2)?,
                    encrypted: r.get::<_, i64>(3)? != 0,
                    created: r.get(4)?,
                })
            },
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn vault_by_id(&self, id: &str) -> Result<Option<VaultRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT id, name, owner_id, encrypted, created FROM vaults WHERE id = ?1",
            params![id],
            |r| {
                Ok(VaultRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner_id: r.get(2)?,
                    encrypted: r.get::<_, i64>(3)? != 0,
                    created: r.get(4)?,
                })
            },
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_vaults(&self, owner_id: &str) -> Result<Vec<VaultRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, owner_id, encrypted, created FROM vaults
             WHERE owner_id = ?1 ORDER BY name",
        )?;
        let rows = stmt
            .query_map(params![owner_id], |r| {
                Ok(VaultRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    owner_id: r.get(2)?,
                    encrypted: r.get::<_, i64>(3)? != 0,
                    created: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarkdrive_core::testutil::TempDir;

    fn db(tag: &str) -> (TempDir, Db) {
        let dir = TempDir::new(tag);
        let d = Db::open(&dir.path().join("server.db")).unwrap();
        (dir, d)
    }

    #[test]
    fn create_and_authenticate_user() {
        let (_d, db) = db("db-auth");
        assert_eq!(db.count_users().unwrap(), 0);
        let id = db.create_user("ada", "correct horse").unwrap();
        assert_eq!(db.count_users().unwrap(), 1);
        assert_eq!(db.authenticate("ada", "correct horse").unwrap(), Some(id));
        assert_eq!(db.authenticate("ada", "wrong horse").unwrap(), None);
        assert_eq!(db.authenticate("nobody", "correct horse").unwrap(), None);
    }

    #[test]
    fn duplicate_usernames_are_rejected() {
        let (_d, db) = db("db-dup");
        db.create_user("ada", "pw").unwrap();
        assert!(db.create_user("ada", "pw2").is_err());
        assert!(db.user_id_for_username("ada").unwrap().is_some());
    }

    #[test]
    fn blank_credentials_are_rejected() {
        let (_d, db) = db("db-blank");
        assert!(db.create_user("", "pw").is_err());
        assert!(db.create_user("ada", "").is_err());
    }

    #[test]
    fn tokens_round_trip_and_revoke() {
        let (_d, db) = db("db-token");
        let uid = db.create_user("ada", "pw").unwrap();
        let token = db.create_token(&uid, Some("laptop")).unwrap();
        assert_eq!(db.user_for_token(&token).unwrap(), Some(uid));
        assert_eq!(db.user_for_token("bogus").unwrap(), None);
        assert!(db.revoke_token(&token).unwrap());
        assert_eq!(db.user_for_token(&token).unwrap(), None);
    }

    #[test]
    fn vaults_are_scoped_to_their_owner() {
        let (_d, db) = db("db-vault");
        let a = db.create_user("a", "pw").unwrap();
        let b = db.create_user("b", "pw").unwrap();

        db.create_vault("photos", &a, false).unwrap();
        db.create_vault("docs", &a, true).unwrap();
        db.create_vault("secret", &b, false).unwrap();

        let mine = db.list_vaults(&a).unwrap();
        let names: Vec<&str> = mine.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "photos"]);
        assert!(mine.iter().any(|v| v.name == "docs" && v.encrypted));

        assert_eq!(db.list_vaults(&b).unwrap().len(), 1);
        let v = db.vault_by_name("photos").unwrap().unwrap();
        assert_eq!(v.owner_id, a);
    }

    #[test]
    fn bad_vault_names_are_rejected() {
        let (_d, db) = db("db-badname");
        let a = db.create_user("a", "pw").unwrap();
        assert!(db.create_vault("../escape", &a, false).is_err());
        assert!(db.create_vault("a/b", &a, false).is_err());
        assert!(db.create_vault("", &a, false).is_err());
    }
}
