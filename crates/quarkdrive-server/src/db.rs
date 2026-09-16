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
    created   INTEGER NOT NULL,
    totp_secret TEXT,
    totp_enabled INTEGER NOT NULL DEFAULT 0
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
CREATE TABLE IF NOT EXISTS shares (
    vault_id  TEXT NOT NULL,
    user_id   TEXT NOT NULL,
    role      TEXT NOT NULL,            -- 'read' or 'write'
    created   INTEGER NOT NULL,
    PRIMARY KEY (vault_id, user_id)
);
CREATE TABLE IF NOT EXISTS trash (
    id         TEXT PRIMARY KEY,
    vault_id   TEXT NOT NULL,
    path       TEXT NOT NULL,
    node_id    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tokens_user ON tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_vaults_owner ON vaults(owner_id);
CREATE TABLE IF NOT EXISTS links (
    id       TEXT PRIMARY KEY,     -- the public token that appears in the URL
    vault_id TEXT NOT NULL,
    path     TEXT NOT NULL,        -- shared file, or the root of a shared folder
    pass_hash TEXT,                -- Argon2id of the link password, if any
    pass_salt TEXT,
    expires  INTEGER,              -- unix seconds; null = never
    created  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_shares_user ON shares(user_id);
CREATE INDEX IF NOT EXISTS idx_trash_vault ON trash(vault_id);
CREATE TABLE IF NOT EXISTS uploads (
    session  TEXT PRIMARY KEY,
    vault_id TEXT NOT NULL,
    path     TEXT NOT NULL,
    size     INTEGER NOT NULL,
    received INTEGER NOT NULL DEFAULT 0,
    created  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_links_vault ON links(vault_id);
CREATE INDEX IF NOT EXISTS idx_uploads_created ON uploads(created);
"#;

/// Add `column` to `table` if an older database lacks it.
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<()> {
    let exists: bool = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|c| c.as_deref() == Ok(column));
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct VaultRow {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub encrypted: bool,
    pub created: i64,
}

/// One row of a vault's trash: the detached tree node and where it lived.
#[derive(Debug, Clone)]
pub struct TrashRow {
    pub id: String,
    pub vault_id: String,
    pub path: String,
    pub node_id: String,
    pub kind: String,
    pub size: u64,
    pub deleted_at: i64,
}

/// A vault shared with a user (`role` is "read" or "write").
#[derive(Debug, Clone)]
pub struct ShareRow {
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub created: i64,
}

/// An in-progress chunked upload.
#[derive(Debug, Clone)]
pub struct UploadRow {
    pub session: String,
    pub vault_id: String,
    pub path: String,
    pub size: u64,
    pub received: u64,
    #[allow(dead_code)]
    pub created: i64,
}

/// A public link to a path inside a vault.
#[derive(Debug, Clone)]
pub struct LinkRow {
    pub id: String,
    pub vault_id: String,
    pub path: String,
    pub pass_hash: Option<String>,
    pub pass_salt: Option<String>,
    pub expires: Option<i64>,
    pub created: i64,
}

impl LinkRow {
    pub fn has_password(&self) -> bool {
        self.pass_hash.is_some()
    }

    pub fn expired(&self, now: i64) -> bool {
        matches!(self.expires, Some(t) if t <= now)
    }
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
        // Columns added after the first release: CREATE TABLE IF NOT EXISTS
        // will not add them to an existing database.
        ensure_column(&conn, "users", "totp_secret", "TEXT")?;
        ensure_column(&conn, "users", "totp_enabled", "INTEGER NOT NULL DEFAULT 0")?;
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

    /// Identifier for a half-finished 2FA login.
    pub fn pending_id() -> String {
        Self::random_id(16)
    }

    // -------------------------------------------------------- uploads

    pub fn create_upload(&self, vault_id: &str, path: &str, size: u64) -> Result<String> {
        let session = Self::random_id(16);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO uploads (session, vault_id, path, size, received, created)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![session, vault_id, path, size as i64, Self::now()],
        )?;
        Ok(session)
    }

    pub fn upload_get(&self, session: &str) -> Result<Option<UploadRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT session, vault_id, path, size, received, created
             FROM uploads WHERE session = ?1",
            params![session],
            |r| {
                Ok(UploadRow {
                    session: r.get(0)?,
                    vault_id: r.get(1)?,
                    path: r.get(2)?,
                    size: r.get::<_, i64>(3)? as u64,
                    received: r.get::<_, i64>(4)? as u64,
                    created: r.get(5)?,
                })
            },
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn upload_record_bytes(&self, session: &str, n: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE uploads SET received = received + ?1 WHERE session = ?2",
            params![n as i64, session],
        )?;
        Ok(())
    }

    pub fn upload_delete(&self, session: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM uploads WHERE session = ?1", params![session])?;
        Ok(n > 0)
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

    // ------------------------------------------------------- two-factor

    /// Store a generated secret without enabling it yet: the user must
    /// confirm a working code first, or a lost phone could lock them out
    /// on a half-finished setup.
    pub fn set_totp_secret(&self, user_id: &str, secret_b32: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET totp_secret = ?1, totp_enabled = 0 WHERE id = ?2",
            params![secret_b32, user_id],
        )?;
        Ok(())
    }

    pub fn enable_totp(&self, user_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET totp_enabled = 1 WHERE id = ?1 AND totp_secret IS NOT NULL",
            params![user_id],
        )?;
        Ok(())
    }

    /// Turning 2FA off clears the secret entirely.
    pub fn disable_totp(&self, user_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET totp_secret = NULL, totp_enabled = 0 WHERE id = ?1",
            params![user_id],
        )?;
        Ok(())
    }

    /// (secret, enabled) — None when the user never enrolled.
    pub fn totp_for_user(&self, user_id: &str) -> Result<Option<(String, bool)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT totp_secret, totp_enabled FROM users WHERE id = ?1",
            params![user_id],
            |r| {
                let secret: Option<String> = r.get(0)?;
                let enabled: i64 = r.get(1)?;
                Ok(secret.map(|s| (s, enabled != 0)))
            },
        );
        match row {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
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

    // ---------------------------------------------------------------- shares

    /// Share a vault with a user. `role` must be "read" or "write".
    pub fn share_vault(&self, vault_id: &str, user_id: &str, role: &str) -> Result<()> {
        if role != "read" && role != "write" {
            return Err(anyhow!("role must be \"read\" or \"write\""));
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO shares (vault_id, user_id, role, created) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(vault_id, user_id) DO UPDATE SET role = excluded.role",
            params![vault_id, user_id, role, Self::now()],
        )?;
        Ok(())
    }

    pub fn unshare_vault(&self, vault_id: &str, user_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM shares WHERE vault_id = ?1 AND user_id = ?2",
            params![vault_id, user_id],
        )?;
        Ok(n > 0)
    }

    /// Everyone a vault is shared with, newest first.
    pub fn shares_for_vault(&self, vault_id: &str) -> Result<Vec<ShareRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.user_id, u.username, s.role, s.created
             FROM shares s JOIN users u ON u.id = s.user_id
             WHERE s.vault_id = ?1 ORDER BY s.created DESC",
        )?;
        let rows = stmt
            .query_map(params![vault_id], |r| {
                Ok(ShareRow {
                    user_id: r.get(0)?,
                    username: r.get(1)?,
                    role: r.get(2)?,
                    created: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The role `user_id` holds on a vault, ignoring ownership.
    pub fn share_role(&self, vault_id: &str, user_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT role FROM shares WHERE vault_id = ?1 AND user_id = ?2",
            params![vault_id, user_id],
            |r| r.get::<_, String>(0),
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Vaults shared *with* a user, i.e. not owned by them.
    pub fn list_shared_vaults(&self, user_id: &str) -> Result<Vec<(VaultRow, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.name, v.owner_id, v.encrypted, v.created, s.role
             FROM shares s JOIN vaults v ON v.id = s.vault_id
             WHERE s.user_id = ?1 ORDER BY v.name",
        )?;
        let rows = stmt
            .query_map(params![user_id], |r| {
                Ok((
                    VaultRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        owner_id: r.get(2)?,
                        encrypted: r.get::<_, i64>(3)? != 0,
                        created: r.get(4)?,
                    },
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ----------------------------------------------------------------- trash

    /// Record a detached node. Returns the trash entry id.
    pub fn trash_insert(
        &self,
        vault_id: &str,
        path: &str,
        node_id: &str,
        kind: &str,
        size: u64,
    ) -> Result<String> {
        let id = Self::random_id(12);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trash (id, vault_id, path, node_id, kind, size, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, vault_id, path, node_id, kind, size as i64, Self::now()],
        )?;
        Ok(id)
    }

    pub fn trash_list(&self, vault_id: &str) -> Result<Vec<TrashRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, vault_id, path, node_id, kind, size, deleted_at
             FROM trash WHERE vault_id = ?1 ORDER BY deleted_at DESC",
        )?;
        let rows = stmt
            .query_map(params![vault_id], |r| {
                Ok(TrashRow {
                    id: r.get(0)?,
                    vault_id: r.get(1)?,
                    path: r.get(2)?,
                    node_id: r.get(3)?,
                    kind: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    deleted_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn trash_get(&self, vault_id: &str, id: &str) -> Result<Option<TrashRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT id, vault_id, path, node_id, kind, size, deleted_at
             FROM trash WHERE vault_id = ?1 AND id = ?2",
            params![vault_id, id],
            |r| {
                Ok(TrashRow {
                    id: r.get(0)?,
                    vault_id: r.get(1)?,
                    path: r.get(2)?,
                    node_id: r.get(3)?,
                    kind: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    deleted_at: r.get(6)?,
                })
            },
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Drop a trash entry. The detached node's objects stay in the store
    /// until object-level garbage collection exists.
    pub fn trash_remove(&self, vault_id: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM trash WHERE vault_id = ?1 AND id = ?2",
            params![vault_id, id],
        )?;
        Ok(n > 0)
    }

    // ----------------------------------------------------------------- links

    /// Create a public link. `password` is hashed the same way user
    /// passwords are; an empty option means anyone with the URL can read.
    pub fn create_link(
        &self,
        vault_id: &str,
        path: &str,
        password: Option<&str>,
        expires: Option<i64>,
    ) -> Result<String> {
        let id = Self::random_id(12);
        let (hash, salt) = match password {
            Some(pw) => {
                if pw.is_empty() {
                    return Err(anyhow!("an empty password means an open link — pass none instead"));
                }
                let salt = crypto::random_salt();
                let hash =
                    crypto::derive_key_from_passphrase(pw.as_bytes(), &salt)?.as_bytes();
                (Some(hex::encode(hash)), Some(hex::encode(salt)))
            }
            None => (None, None),
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO links (id, vault_id, path, pass_hash, pass_salt, expires, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, vault_id, path, hash, salt, expires, Self::now()],
        )?;
        Ok(id)
    }

    pub fn links_for_vault(&self, vault_id: &str) -> Result<Vec<LinkRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, vault_id, path, pass_hash, pass_salt, expires, created
             FROM links WHERE vault_id = ?1 ORDER BY created DESC",
        )?;
        let rows = stmt
            .query_map(params![vault_id], link_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn link_get(&self, id: &str) -> Result<Option<LinkRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT id, vault_id, path, pass_hash, pass_salt, expires, created
             FROM links WHERE id = ?1",
            params![id],
            link_row,
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn link_delete(&self, vault_id: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM links WHERE vault_id = ?1 AND id = ?2",
            params![vault_id, id],
        )?;
        Ok(n > 0)
    }
}

fn link_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRow> {
    Ok(LinkRow {
        id: r.get(0)?,
        vault_id: r.get(1)?,
        path: r.get(2)?,
        pass_hash: r.get(3)?,
        pass_salt: r.get(4)?,
        expires: r.get(5)?,
        created: r.get(6)?,
    })
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

    #[test]
    fn upload_sessions_track_progress() {
        let (_d, db) = db("db-uploads");
        let owner = db.create_user("a", "pw").unwrap();
        let vault = db.create_vault("v", &owner, false).unwrap();

        let session = db.create_upload(&vault, "movies/big.bin", 1000).unwrap();
        let row = db.upload_get(&session).unwrap().unwrap();
        assert_eq!(row.received, 0);
        assert_eq!(row.size, 1000);
        assert_eq!(row.path, "movies/big.bin");

        db.upload_record_bytes(&session, 400).unwrap();
        db.upload_record_bytes(&session, 600).unwrap();
        assert_eq!(db.upload_get(&session).unwrap().unwrap().received, 1000);

        assert!(db.upload_delete(&session).unwrap());
        assert!(db.upload_get(&session).unwrap().is_none());
        assert!(db.upload_get("missing").unwrap().is_none());
    }

    #[test]
    fn totp_enrollment_requires_confirmation() {
        let (_d, db) = db("db-totp");
        let uid = db.create_user("ada", "pw").unwrap();
        assert!(db.totp_for_user(&uid).unwrap().is_none());

        db.set_totp_secret(&uid, "MZXW6YTB").unwrap();
        // A stored-but-unconfirmed secret is not yet second factor.
        assert_eq!(
            db.totp_for_user(&uid).unwrap(),
            Some(("MZXW6YTB".to_string(), false))
        );

        db.enable_totp(&uid).unwrap();
        assert_eq!(
            db.totp_for_user(&uid).unwrap(),
            Some(("MZXW6YTB".to_string(), true))
        );

        // Disabling clears everything, including the old secret.
        db.disable_totp(&uid).unwrap();
        assert_eq!(db.totp_for_user(&uid).unwrap(), None);
    }

    #[test]
    fn enabling_without_a_secret_changes_nothing() {
        let (_d, db) = db("db-totp-enable");
        let uid = db.create_user("a", "pw").unwrap();
        db.enable_totp(&uid).unwrap();
        assert!(db.totp_for_user(&uid).unwrap().is_none());
    }

    #[test]
    fn shares_round_trip() {
        let (_d, db) = db("db-share");
        let owner = db.create_user("ada", "pw").unwrap();
        let mate = db.create_user("bob", "pw").unwrap();
        let vault = db.create_vault("photos", &owner, false).unwrap();

        db.share_vault(&vault, &mate, "read").unwrap();
        assert_eq!(db.share_role(&vault, &mate).unwrap().as_deref(), Some("read"));
        // Re-sharing upgrades the role in place.
        db.share_vault(&vault, &mate, "write").unwrap();
        assert_eq!(db.share_role(&vault, &mate).unwrap().as_deref(), Some("write"));

        let shared = db.list_shared_vaults(&mate).unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].0.name, "photos");
        assert_eq!(shared[0].1, "write");
        // The owner does not see it in *their* shared list.
        assert!(db.list_shared_vaults(&owner).unwrap().is_empty());

        let rows = db.shares_for_vault(&vault).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].username, "bob");

        assert!(db.unshare_vault(&vault, &mate).unwrap());
        assert_eq!(db.share_role(&vault, &mate).unwrap(), None);
        assert!(!db.unshare_vault(&vault, &mate).unwrap());
    }

    #[test]
    fn invalid_share_roles_are_rejected() {
        let (_d, db) = db("db-sharerole");
        let owner = db.create_user("a", "pw").unwrap();
        let mate = db.create_user("b", "pw").unwrap();
        let vault = db.create_vault("v", &owner, false).unwrap();
        assert!(db.share_vault(&vault, &mate, "admin").is_err());
        assert!(db.share_vault(&vault, &mate, "").is_err());
    }

    #[test]
    fn trash_round_trip() {
        let (_d, db) = db("db-trash");
        let owner = db.create_user("a", "pw").unwrap();
        let vault = db.create_vault("v", &owner, false).unwrap();

        let id = db.trash_insert(&vault, "docs/notes.txt", "abc123", "file", 42).unwrap();
        let rows = db.trash_list(&vault).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "docs/notes.txt");
        assert_eq!(rows[0].size, 42);

        let got = db.trash_get(&vault, &id).unwrap().unwrap();
        assert_eq!(got.node_id, "abc123");
        assert!(db.trash_get(&vault, "missing").unwrap().is_none());

        assert!(db.trash_remove(&vault, &id).unwrap());
        assert!(db.trash_list(&vault).unwrap().is_empty());
        assert!(!db.trash_remove(&vault, &id).unwrap());
    }

    #[test]
    fn links_round_trip_with_optional_passwords() {
        let (_d, db) = db("db-links");
        let owner = db.create_user("a", "pw").unwrap();
        let vault = db.create_vault("v", &owner, false).unwrap();

        let open = db.create_link(&vault, "docs", None, None).unwrap();
        let locked = db
            .create_link(&vault, "secret.txt", Some("open sesame"), Some(9_999_999_999))
            .unwrap();

        let rows = db.links_for_vault(&vault).unwrap();
        assert_eq!(rows.len(), 2);

        let open_row = db.link_get(&open).unwrap().unwrap();
        assert!(!open_row.has_password());
        assert!(!open_row.expired(1));
        assert_eq!(open_row.path, "docs");

        let locked_row = db.link_get(&locked).unwrap().unwrap();
        assert!(locked_row.has_password());
        assert!(!locked_row.expired(9_999_999_998));
        assert!(locked_row.expired(10_000_000_000), "expired links say so");

        // The stored hash verifies against the right password only.
        let salt = hex::decode(locked_row.pass_salt.as_deref().unwrap()).unwrap();
        let mut arr = [0u8; crypto::SALT_LEN];
        arr.copy_from_slice(&salt);
        let good = crypto::derive_key_from_passphrase(b"open sesame", &arr).unwrap();
        let bad = crypto::derive_key_from_passphrase(b"wrong", &arr).unwrap();
        assert_eq!(hex::encode(good.as_bytes()), locked_row.pass_hash.unwrap());
        assert_ne!(hex::encode(bad.as_bytes()), "x");

        assert!(db.link_delete(&vault, &open).unwrap());
        assert!(db.link_get(&open).unwrap().is_none());
        assert!(!db.link_delete(&vault, &open).unwrap());
    }

    #[test]
    fn empty_link_passwords_are_rejected() {
        let (_d, db) = db("db-linkpw");
        let owner = db.create_user("a", "pw").unwrap();
        let vault = db.create_vault("v", &owner, false).unwrap();
        assert!(db.create_link(&vault, "docs", Some(""), None).is_err());
    }

    #[test]
    fn trash_is_scoped_per_vault() {
        let (_d, db) = db("db-trashscope");
        let owner = db.create_user("a", "pw").unwrap();
        let v1 = db.create_vault("one", &owner, false).unwrap();
        let v2 = db.create_vault("two", &owner, false).unwrap();
        db.trash_insert(&v1, "x.txt", "n1", "file", 1).unwrap();
        assert!(db.trash_list(&v2).unwrap().is_empty());
        assert!(db.trash_get(&v2, "nope").unwrap().is_none());
    }
}
