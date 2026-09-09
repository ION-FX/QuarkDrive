//! Quarkdrive command line client.
//!
//! ```text
//! qd init --server http://host:8787 --vault photos --token <token> --dir ~/Photos
//! qd sync          # one-shot
//! qd watch         # keep syncing, driven by inotify
//! qd status        # local changes not yet pushed
//! ```
//!
//! `watch` is what makes syncing automatic: it uses inotify to notice changes
//! immediately, debounces the burst of events a single save produces, syncs,
//! and additionally re-syncs on a timer as a safety net — filesystem
//! notification can miss events, and sees nothing at all for changes made
//! while the daemon was not running.

mod config;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use quarkdrive_core::index::FileCache;
use quarkdrive_core::object::ObjectStore;
use quarkdrive_core::sync::{SyncReport, Syncer};
use quarkdrive_core::transport::HttpTransport;

use config::{Config, VaultPaths};

#[derive(Parser)]
#[command(name = "qd", about = "Quarkdrive file sync", version)]
struct Cli {
    /// Synced directory. Defaults to the enclosing vault, or the current
    /// directory.
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up a directory to sync with a server
    Init {
        #[arg(long)]
        server: String,
        #[arg(long)]
        vault: String,
        #[arg(long)]
        token: String,
        /// Name for this machine. Defaults to the hostname.
        #[arg(long)]
        device: Option<String>,
        /// Encrypt on this device, so the server cannot read the vault.
        #[arg(long)]
        encrypt: bool,
    },
    /// Log in and print an access token
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
    },
    /// Sync once and exit
    Sync,
    /// Show local changes that have not been pushed
    Status,
    /// Sync continuously, watching for changes
    Watch {
        /// Also re-sync every this many seconds, as a safety net
        #[arg(long, default_value = "30")]
        interval: u64,
    },
}

fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    let dir = resolve_dir(cli.dir.as_deref())?;

    match cli.command {
        Command::Init {
            server,
            vault,
            token,
            device,
            encrypt,
        } => init(&dir, &server, &vault, &token, device.as_deref(), encrypt),

        Command::Login {
            server,
            username,
            password,
        } => {
            let token = login(&server, &username, &password)?;
            println!("{token}");
            Ok(())
        }

        Command::Sync => {
            let mut vault = open(&dir)?;
            let report = run_sync(&mut vault)?;
            print_report(&report);
            Ok(())
        }

        Command::Status => {
            let mut vault = open(&dir)?;
            let changes = pending(&mut vault)?;
            if changes.is_empty() {
                println!("up to date: no local changes");
            } else {
                println!("{} pending change(s):", changes.len());
                for c in changes {
                    println!("  {c:?}");
                }
            }
            Ok(())
        }

        Command::Watch { interval } => {
            let mut vault = open(&dir)?;
            watch(&mut vault, interval)
        }
    }
}

/// The directory to operate on: an explicit flag, the enclosing vault, or the
/// working directory.
fn resolve_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(d.to_path_buf());
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;
    Ok(config::find_vault_root(&cwd).unwrap_or(cwd))
}

// ------------------------------------------------------------------- commands

fn init(
    dir: &Path,
    server: &str,
    vault: &str,
    token: &str,
    device: Option<&str>,
    encrypt: bool,
) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let paths = VaultPaths::new(dir);
    fs::create_dir_all(&paths.objects)?;

    let device = device
        .map(|s| s.to_string())
        .unwrap_or_else(quarkdrive_core::sync::default_host);

    if encrypt {
        config::generate_key(&paths)?;
    }

    let cfg = Config {
        server: server.trim_end_matches('/').to_string(),
        vault: vault.to_string(),
        token: token.to_string(),
        device,
        encrypted: encrypt,
    };
    cfg.save(&paths.config)?;

    println!("initialised {}", dir.display());
    println!("  server : {}", cfg.server);
    println!("  vault  : {}", cfg.vault);
    println!("  device : {}", cfg.device);
    if encrypt {
        println!("  encryption: on (the server cannot read this vault)");
        println!("  key file  : {}", paths.key.display());
        println!("  back the key up separately: losing it loses the vault");
    }
    println!("\nrun `qd sync` to sync, or `qd watch` to keep syncing");
    Ok(())
}

fn login(server: &str, username: &str, password: &str) -> Result<String> {
    let url = format!("{}/api/v1/auth/login", server.trim_end_matches('/'));
    let body = serde_json::json!({ "username": username, "password": password }).to_string();
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|e| ureq_error(e, "logging in"))?;
    let text = resp.into_string().context("reading login response")?;
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    parsed["token"]
        .as_str()
        .map(|t| t.to_string())
        .ok_or_else(|| anyhow!("server did not return a token: {text}"))
}

// ---------------------------------------------------------------- vault access

struct Vault {
    paths: VaultPaths,
    cfg: Config,
    store: ObjectStore,
    cache: FileCache,
}

fn open(dir: &Path) -> Result<Vault> {
    let paths = VaultPaths::new(dir);
    let cfg = Config::load(&paths.config).with_context(|| {
        format!(
            "{} is not a Quarkdrive vault — run `qd init` first",
            dir.display()
        )
    })?;

    let key = if cfg.encrypted {
        Some(config::load_key(&paths)?.ok_or_else(|| {
            anyhow!(
                "vault is encrypted but there is no key file at {}",
                paths.key.display()
            )
        })?)
    } else {
        None
    };

    fs::create_dir_all(&paths.objects)?;
    let store = ObjectStore::open(&paths.objects, key)?;
    let cache = FileCache::open(&paths.index)?;
    Ok(Vault {
        paths,
        cfg,
        store,
        cache,
    })
}

fn run_sync(vault: &mut Vault) -> Result<SyncReport> {
    let mut transport = HttpTransport::new(&vault.cfg.server, &vault.cfg.vault, &vault.cfg.token);
    let mut syncer = Syncer::new(
        &vault.paths.root,
        &vault.store,
        &mut vault.cache,
        &mut transport,
    )
    .with_device(&vault.cfg.device);
    syncer.sync()
}

fn pending(vault: &mut Vault) -> Result<Vec<quarkdrive_core::merge::Change>> {
    let mut transport = HttpTransport::new(&vault.cfg.server, &vault.cfg.vault, &vault.cfg.token);
    let syncer = Syncer::new(
        &vault.paths.root,
        &vault.store,
        &mut vault.cache,
        &mut transport,
    );
    syncer.pending()
}

// --------------------------------------------------------------------- daemon

fn watch(vault: &mut Vault, interval: u64) -> Result<()> {
    use notify::{RecursiveMode, Watcher};

    // Sync once on start-up, so a daemon that was down does not wait for the
    // first inotify event to catch up.
    match run_sync(vault) {
        Ok(report) => print_report(&report),
        Err(e) => eprintln!("initial sync failed: {e:#}"),
    }

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&vault.paths.root, RecursiveMode::Recursive)?;

    println!(
        "watching {} (interval {interval}s, Ctrl-C to stop)",
        vault.paths.root.display()
    );

    loop {
        match rx.recv_timeout(Duration::from_secs(interval)) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // A single save produces several events and a directory copy produces
        // thousands. Wait for the burst to finish, then sync once.
        std::thread::sleep(Duration::from_millis(400));
        drain(&rx);

        match run_sync(vault) {
            Ok(report) if report.is_idle() => {}
            Ok(report) => print_report(&report),
            Err(e) => eprintln!("sync failed: {e:#}"),
        }
    }
    Ok(())
}

fn drain<T>(rx: &std::sync::mpsc::Receiver<T>) {
    while rx.try_recv().is_ok() {}
}

// -------------------------------------------------------------------- output

fn print_report(report: &SyncReport) {
    if report.unchanged {
        println!("up to date");
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    if report.uploaded_objects > 0 {
        parts.push(format!(
            "{} objects up ({})",
            report.uploaded_objects,
            config::human_bytes(report.uploaded_bytes)
        ));
    }
    if report.downloaded_objects > 0 {
        parts.push(format!(
            "{} objects down ({})",
            report.downloaded_objects,
            config::human_bytes(report.downloaded_bytes)
        ));
    }
    if report.files_updated > 0 {
        parts.push(format!("{} file(s) written", report.files_updated));
    }
    if report.files_removed > 0 {
        parts.push(format!("{} path(s) removed", report.files_removed));
    }
    if report.conflicts > 0 {
        parts.push(format!("{} conflict(s) kept", report.conflicts));
    }
    if parts.is_empty() {
        println!("up to date");
    } else {
        println!("synced: {}", parts.join(", "));
    }
}

fn ureq_error(err: ureq::Error, what: &str) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::anyhow!("{what}: server returned {code}: {body}")
        }
        ureq::Error::Transport(t) => anyhow::anyhow!("{what}: {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarkdrive_core::scan::DEFAULT_EXCLUDES;
    use quarkdrive_core::testutil::TempDir;

    #[test]
    fn explicit_dir_is_used_as_given() {
        assert_eq!(
            resolve_dir(Some(Path::new("/tmp/x"))).unwrap(),
            PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn falls_back_to_the_current_directory() {
        let d = resolve_dir(None).unwrap();
        assert!(d.exists());
    }

    #[test]
    fn opening_a_non_vault_fails_with_guidance() {
        let dir = TempDir::new("cli-novault");
        // `unwrap_err` would need Debug on Vault, which owns an ObjectStore.
        let err = match open(dir.path()) {
            Ok(_) => panic!("opening a plain directory should fail"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("qd init"),
            "error should tell the user what to do: {err}"
        );
    }

    #[test]
    fn init_creates_a_usable_vault() {
        let dir = TempDir::new("cli-init");
        let root = dir.path().join("vault");
        init(
            &root,
            "http://localhost:8787/",
            "photos",
            "tok",
            Some("laptop"),
            false,
        )
        .unwrap();

        let vault = open(&root).unwrap();
        assert_eq!(vault.cfg.vault, "photos");
        assert_eq!(vault.cfg.server, "http://localhost:8787", "trailing slash is trimmed");
        assert!(!vault.cfg.encrypted);
        assert!(vault.paths.root.join(".quarkdrive/objects").is_dir());
    }

    #[test]
    fn init_with_encryption_stores_a_key() {
        let dir = TempDir::new("cli-init-enc");
        let root = dir.path().join("vault");
        init(&root, "http://localhost:8787", "secret", "tok", None, true).unwrap();

        let vault = open(&root).unwrap();
        assert!(vault.cfg.encrypted);
        assert!(config::load_key(&vault.paths).unwrap().is_some());
    }

    /// State lives inside the vault but must never be synced.
    #[test]
    fn state_directory_is_excluded_from_scans() {
        assert!(
            DEFAULT_EXCLUDES.contains(&".quarkdrive"),
            "the client's own state directory would otherwise sync itself"
        );
    }
}
