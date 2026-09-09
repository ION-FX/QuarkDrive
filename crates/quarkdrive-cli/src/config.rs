//! On-disk client configuration.
//!
//! State lives inside the synced folder, in `.quarkdrive/`, the way Dropbox
//! does it. That keeps a vault self-contained: copy the folder and you copy
//! its identity, and `.quarkdrive` is excluded from every scan so the client
//! never tries to sync its own bookkeeping.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Name of the hidden state directory inside a synced folder.
pub const STATE_DIR: &str = ".quarkdrive";

pub struct VaultPaths {
    pub root: PathBuf,
    pub state: PathBuf,
    pub objects: PathBuf,
    pub index: PathBuf,
    pub config: PathBuf,
    pub key: PathBuf,
}

impl VaultPaths {
    pub fn new(root: &Path) -> Self {
        let state = root.join(STATE_DIR);
        VaultPaths {
            root: root.to_path_buf(),
            objects: state.join("objects"),
            index: state.join("index.db"),
            config: state.join("config.json"),
            key: state.join("key"),
            state,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Base URL of the server, e.g. `http://host:8787`.
    pub server: String,
    pub vault: String,
    pub token: String,
    /// A name for this machine, used in snapshots and conflict filenames.
    pub device: String,
    /// Whether the vault is end-to-end encrypted.
    #[serde(default)]
    pub encrypted: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text)?;
        Ok(())
    }
}

/// Read the vault key, if the vault is encrypted.
pub fn load_key(paths: &VaultPaths) -> Result<Option<quarkdrive_core::crypto::Key>> {
    if !paths.key.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&paths.key).context("reading vault key")?;
    let bytes = hex::decode(text.trim()).context("vault key is not valid hex")?;
    if bytes.len() != quarkdrive_core::crypto::KEY_LEN {
        return Err(anyhow!(
            "vault key is {} bytes, expected {}",
            bytes.len(),
            quarkdrive_core::crypto::KEY_LEN
        ));
    }
    let mut key = [0u8; quarkdrive_core::crypto::KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(Some(quarkdrive_core::crypto::Key::from_bytes(key)))
}

/// Generate and store a fresh vault key, readable only by this user.
pub fn generate_key(paths: &VaultPaths) -> Result<quarkdrive_core::crypto::Key> {
    let key = quarkdrive_core::crypto::Key::generate();
    if let Some(parent) = paths.key.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&paths.key, hex::encode(key.as_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paths.key, fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

/// Find the vault root by walking up from `start` looking for `.quarkdrive`.
pub fn find_vault_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    while let Some(dir) = cur {
        if dir.join(STATE_DIR).join("config.json").exists() {
            return Some(dir);
        }
        cur = dir.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Render a byte count the way a file manager would.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 1;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarkdrive_core::testutil::TempDir;

    fn paths(tag: &str) -> (TempDir, VaultPaths) {
        let dir = TempDir::new(tag);
        let p = VaultPaths::new(dir.path());
        (dir, p)
    }

    #[test]
    fn config_round_trips() {
        let (_d, p) = paths("cfg-roundtrip");
        std::fs::create_dir_all(&p.state).unwrap();
        let cfg = Config {
            server: "http://localhost:8787".into(),
            vault: "photos".into(),
            token: "abc123".into(),
            device: "laptop".into(),
            encrypted: true,
        };
        cfg.save(&p.config).unwrap();
        let loaded = Config::load(&p.config).unwrap();
        assert_eq!(loaded.server, cfg.server);
        assert_eq!(loaded.vault, "photos");
        assert!(loaded.encrypted);
    }

    #[test]
    fn missing_config_is_an_error() {
        let (_d, p) = paths("cfg-missing");
        assert!(Config::load(&p.config).is_err());
    }

    #[test]
    fn keys_round_trip_and_are_private() {
        let (_d, p) = paths("cfg-key");
        std::fs::create_dir_all(&p.state).unwrap();
        let key = generate_key(&p).unwrap();
        let loaded = load_key(&p).unwrap().unwrap();
        assert_eq!(loaded.as_bytes(), key.as_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p.key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must not be world readable");
        }
    }

    #[test]
    fn absent_key_means_no_encryption() {
        let (_d, p) = paths("cfg-nokey");
        assert!(load_key(&p).unwrap().is_none());
    }

    #[test]
    fn vault_root_is_found_from_a_subdirectory() {
        let (_d, p) = paths("cfg-find");
        std::fs::create_dir_all(&p.state).unwrap();
        std::fs::create_dir_all(p.root.join("a/b/c")).unwrap();
        let cfg = Config {
            server: "s".into(),
            vault: "v".into(),
            token: "t".into(),
            device: "d".into(),
            encrypted: false,
        };
        cfg.save(&p.config).unwrap();

        let found = find_vault_root(&p.root.join("a/b/c")).unwrap();
        assert_eq!(found, p.root);
    }

    #[test]
    fn no_vault_above_returns_none() {
        let dir = TempDir::new("cfg-nofind");
        assert!(find_vault_root(dir.path()).is_none());
    }

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
