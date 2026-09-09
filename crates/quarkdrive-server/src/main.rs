//! Quarkdrive server.
//!
//! Serves three kinds of client from one vault:
//!
//! * desktop and Android clients, over the content-addressed object protocol
//! * web browsers, over the file API plus this server's static UI
//! * phones uploading photos, over the same file API
//!
//! ```text
//! quarkdrive-server create-user  --data ./data --username ada --password '…'
//! quarkdrive-server create-vault --data ./data --username ada --name photos
//! quarkdrive-server serve        --data ./data --web ./web --listen 0.0.0.0:8787
//! ```

mod api;
mod db;
mod media;
mod vault;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Parser)]
#[command(name = "quarkdrive-server", about = "Quarkdrive sync and file server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server
    Serve {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        /// Directory holding vaults and the user database
        #[arg(long, default_value = "./quarkdrive-data")]
        data: PathBuf,
        /// Directory holding the static web UI
        #[arg(long)]
        web: Option<PathBuf>,
    },
    /// Create a user account
    CreateUser {
        #[arg(long, default_value = "./quarkdrive-data")]
        data: PathBuf,
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
    },
    /// Create a vault owned by a user
    CreateVault {
        #[arg(long, default_value = "./quarkdrive-data")]
        data: PathBuf,
        #[arg(long)]
        username: String,
        #[arg(long)]
        name: String,
        /// Mark the vault end-to-end encrypted.
        ///
        /// The server then cannot index, preview or serve its contents; only
        /// clients holding the key can sync it.
        #[arg(long)]
        encrypted: bool,
    },
    /// Revoke an access token
    RevokeToken {
        #[arg(long, default_value = "./quarkdrive-data")]
        data: PathBuf,
        #[arg(long)]
        token: String,
    },
    /// Issue a new access token for a user
    CreateToken {
        #[arg(long, default_value = "./quarkdrive-data")]
        data: PathBuf,
        #[arg(long)]
        username: String,
        #[arg(long)]
        device: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { listen, data, web } => serve(listen, &data, web.as_deref()).await,

        Command::CreateUser {
            data,
            username,
            password,
        } => {
            let db = db::Db::open(&data.join("server.db"))?;
            let id = db.create_user(&username, &password)?;
            println!("created user {username} ({id})");
            Ok(())
        }

        Command::CreateVault {
            data,
            username,
            name,
            encrypted,
        } => {
            let db = db::Db::open(&data.join("server.db"))?;
            let owner = db
                .user_id_for_username(&username)
                .context("looking up user")?
                .ok_or_else(|| anyhow::anyhow!("no such user: {username}"))?;
            let id = db.create_vault(&name, &owner, encrypted)?;
            // Create the storage directories now so `serve` need not.
            let row = db.vault_by_id(&id)?.context("reading back new vault")?;
            vault::Vault::open(&data, row)?;
            println!("created vault {name} ({id})");
            Ok(())
        }

        Command::RevokeToken { data, token } => {
            let db = db::Db::open(&data.join("server.db"))?;
            if db.revoke_token(&token)? {
                println!("token revoked");
                Ok(())
            } else {
                Err(anyhow::anyhow!("no such token"))
            }
        }

        Command::CreateToken {
            data,
            username,
            device,
        } => {
            let db = db::Db::open(&data.join("server.db"))?;
            // No password check: this is an administrative action on the
            // machine that owns the database, not a login.
            let user_id = db
                .user_id_for_username(&username)
                .context("looking up user")?
                .ok_or_else(|| anyhow::anyhow!("no such user: {username}"))?;
            let token = db.create_token(&user_id, device.as_deref())?;
            println!("{token}");
            Ok(())
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("quarkdrive_server=info,info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn serve(listen: SocketAddr, data: &Path, web: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(data)?;
    let state = Arc::new(api::AppState::new(data.to_path_buf())?);

    if state.db.count_users()? == 0 {
        tracing::warn!(
            "no users exist yet — create one with: quarkdrive-server create-user \
             --data {} --username <name> --password <pw>",
            data.display()
        );
    }

    let web_dir = resolve_web_dir(web);
    let index = web_dir.join("index.html");
    let app =
        api::router(state).fallback_service(ServeDir::new(&web_dir).fallback(ServeFile::new(index)));

    tracing::info!("listening on http://{listen}");
    tracing::info!("data directory: {}", data.display());
    if web_dir.join("index.html").exists() {
        tracing::info!("serving web UI from {}", web_dir.display());
    } else {
        tracing::warn!(
            "no web UI found at {} — pass --web to serve one",
            web_dir.display()
        );
    }

    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Where to serve the static UI from: an explicit flag, an environment
/// variable, or a `web` directory near the executable.
fn resolve_web_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("QUARKDRIVE_WEB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("web");
            if candidate.join("index.html").exists() {
                return candidate;
            }
            if let Some(parent) = dir.parent() {
                let candidate = parent.join("web");
                if candidate.join("index.html").exists() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from("./web")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_web_dir_wins() {
        assert_eq!(resolve_web_dir(Some(Path::new("/x/y"))), PathBuf::from("/x/y"));
    }

    #[test]
    fn falls_back_without_a_flag() {
        // Must return something rather than panicking.
        let p = resolve_web_dir(None);
        assert!(!p.as_os_str().is_empty());
    }
}
