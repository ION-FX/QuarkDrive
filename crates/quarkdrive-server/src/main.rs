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
use axum::body::Body;
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderValue, Request};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use std::io::Read as _;
use tower::{Service as _, ServiceBuilder};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

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
        /// TLS certificate (PEM). With --tls-key the server speaks HTTPS.
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        /// TLS private key (PEM).
        #[arg(long)]
        tls_key: Option<PathBuf>,
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
        Command::Serve {
            listen,
            data,
            web,
            tls_cert,
            tls_key,
        } => {
            let tls = match (tls_cert, tls_key) {
                (Some(c), Some(k)) => Some((c, k)),
                (None, None) => None,
                _ => {
                    return Err(anyhow::anyhow!(
                        "--tls-cert and --tls-key must be given together"
                    ))
                }
            };
            serve(listen, &data, web.as_deref(), tls).await
        }

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
    // tower_http at debug makes every request (method, path, status) visible,
    // so "something went wrong" moments can be diagnosed after the fact.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("quarkdrive_server=info,tower_http=debug,info")
        });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn serve(
    listen: SocketAddr,
    data: &Path,
    web: Option<&Path>,
    tls: Option<(PathBuf, PathBuf)>,
) -> Result<()> {
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
    // no-cache (revalidate before reuse, not "never store"): without it a
    // browser may heuristically serve a stale index.html against a newer
    // app.js, and mixed UI versions fail silently.
    let app = api::router(state)
        .fallback_service(ServeDir::new(&web_dir).fallback(ServeFile::new(index)))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));

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

    // ConnectInfo (client IP, used by the login rate limiter) must reach
    // handlers on both the plain and TLS paths.
    let make_svc = app.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some((cert_path, key_path)) => {
            let cert_reader = &mut std::io::BufReader::new(
                std::fs::File::open(&cert_path)
                    .with_context(|| format!("reading {}", cert_path.display()))?,
            );
            let certs = rustls_pemfile::certs(cert_reader)
                .context("parsing TLS certificate")?
                .into_iter()
                .map(tokio_rustls::rustls::Certificate)
                .collect();
            let mut key_pem = Vec::new();
            std::fs::File::open(&key_path)
                .with_context(|| format!("reading {}", key_path.display()))?
                .read_to_end(&mut key_pem)?;
            let key = load_private_key(key_pem.as_slice())?
                .context("no supported private key in --tls-key file")?;
            let mut config = tokio_rustls::rustls::ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .context("invalid TLS certificate/key pair")?;
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));

            tracing::info!("listening on https://{listen}");
            let listener = tokio::net::TcpListener::bind(listen).await?;
            loop {
                let (stream, peer) = listener.accept().await?;
                let acceptor = acceptor.clone();
                let mut make = make_svc.clone();
                tokio::spawn(async move {
                    // The per-connection service only needs the peer address
                    // (Connected<SocketAddr>), so resolve it up front; this
                    // is the same wiring axum::serve does for plain TCP.
                    let tower_service = make
                        .call(peer)
                        .await
                        .unwrap_or_else(|err| match err {});
                    let tls_stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!("TLS handshake failed: {e}");
                            return;
                        }
                    };
                    let hyper_service = TowerToHyperService::new(
                        ServiceBuilder::new()
                            .map_request(|req: Request<Incoming>| req.map(Body::new))
                            .service(tower_service),
                    );
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(
                            TokioIo::new(tls_stream),
                            hyper_service,
                        )
                        .await;
                });
            }
        }
        None => {
            tracing::info!("listening on http://{listen}");
            let listener = tokio::net::TcpListener::bind(listen).await?;
            axum::serve(listener, make_svc).await?;
        }
    }
    Ok(())
}

/// Read the first supported private key from a PEM file: PKCS#8 first,
/// then EC, then RSA — the shapes real certificates actually come in.
fn load_private_key(pem: &[u8]) -> Result<Option<tokio_rustls::rustls::PrivateKey>> {
    let keys = || rustls_pemfile::pkcs8_private_keys(&mut std::io::Cursor::new(pem));
    if let Some(key) = keys()?.first().cloned() {
        return Ok(Some(tokio_rustls::rustls::PrivateKey(key)));
    }
    let keys = rustls_pemfile::ec_private_keys(&mut std::io::Cursor::new(pem))?;
    if let Some(key) = keys.first().cloned() {
        return Ok(Some(tokio_rustls::rustls::PrivateKey(key)));
    }
    let keys = rustls_pemfile::rsa_private_keys(&mut std::io::Cursor::new(pem))?;
    if let Some(key) = keys.first().cloned() {
        return Ok(Some(tokio_rustls::rustls::PrivateKey(key)));
    }
    Ok(None)
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
