//! HTTP API.
//!
//! Two APIs share one storage engine:
//!
//! * the **object protocol** under `/objects`, `/head` and `/commit`, used by
//!   the desktop and Android sync engine. It moves content-addressed objects
//!   and knows nothing about filenames.
//! * the **file API** under `/fs`, used by the web UI and phone clients that
//!   have no sync engine. It does the chunking server-side.
//!
//! Both mutate the same Merkle tree, so a photo uploaded from a phone and a
//! document pushed from a laptop end up in one consistent history.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use quarkdrive_core::crypto;
use quarkdrive_core::hash::ObjectId;
use quarkdrive_core::tree::{Kind, NodeRef, Snapshot};

use crate::db::{Db, LinkRow};
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;
use axum::extract::ConnectInfo;
use crate::media::{self, MediaIndex, MediaRow};
use crate::vault::{split_path, Vault, VaultStats, FileVersion};

/// Uploads are limited by memory, not per request, so allow large files.
const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// What a user may do with a vault. Owners can do everything; a share can
/// be read-only or read-write. Computed per request so revoked shares take
/// effect immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Write,
    Read,
}

impl Role {
    pub fn can_write(self) -> bool {
        self != Role::Read
    }

    fn parse(s: &str) -> Option<Role> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Write => "write",
            Role::Read => "read",
        }
    }
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub db: Db,
    pub logins: LoginLimiter,
    vaults: Mutex<HashMap<String, Arc<Vault>>>,
    indexes: Mutex<HashMap<String, Arc<MediaIndex>>>,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir.join("vaults"))?;
        let db = Db::open(&data_dir.join("server.db"))?;
        Ok(AppState {
            data_dir,
            db,
            logins: LoginLimiter::default(),
            vaults: Mutex::new(HashMap::new()),
            indexes: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve a vault by name for a user, along with their role in it:
    /// the owner, a sharee, or nobody (an error that does not reveal
    /// whether the vault exists).
    pub fn vault(&self, name: &str, user_id: &str) -> anyhow::Result<(Arc<Vault>, Role)> {
        let row = self
            .db
            .vault_by_name(name)?
            .ok_or_else(|| anyhow::anyhow!("no such vault"))?;
        let role = if row.owner_id == user_id {
            Role::Owner
        } else {
            match self.db.share_role(&row.id, user_id)? {
                Some(r) => Role::parse(&r).ok_or_else(|| anyhow::anyhow!("no such vault"))?,
                None => return Err(anyhow::anyhow!("no such vault")),
            }
        };
        let vault = {
            let mut cache = self.vaults.lock().unwrap();
            match cache.get(name) {
                Some(v) => v.clone(),
                None => {
                    let v = Arc::new(Vault::open(&self.data_dir, row.clone())?);
                    cache.insert(name.to_string(), v.clone());
                    v
                }
            }
        };
        Ok((vault, role))
    }

    pub fn index(&self, vault: &Vault) -> anyhow::Result<Arc<MediaIndex>> {
        let mut cache = self.indexes.lock().unwrap();
        if let Some(i) = cache.get(&vault.row.id) {
            return Ok(i.clone());
        }
        let index = Arc::new(MediaIndex::open(&vault.dir)?);
        cache.insert(vault.row.id.clone(), index.clone());
        Ok(index)
    }
}

/// Brute-force guard for `POST /auth/login`.
///
/// Counters live in memory: a restart forgives everyone, which is the
/// right bias for a self-hosted server where the alternative is locking
/// yourself out of your own box. Keyed by (source IP, username) so one
/// attacker guessing "ada" does not lock the real ada out from a
/// different address.
#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<(IpAddr, String), (u32, Instant)>>,
}

/// Five bad passwords inside ten minutes stops further attempts for the
/// rest of that window.
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW_SECS: u64 = 600;

impl LoginLimiter {
    fn key(ip: IpAddr, username: &str) -> (IpAddr, String) {
        (ip, username.trim().to_lowercase())
    }

    /// May a sign-in attempt proceed right now?
    fn allowed(&self, key: &(IpAddr, String), now: Instant) -> bool {
        let map = self.failures.lock().unwrap();
        match map.get(key) {
            Some((count, since)) => {
                *count < LOGIN_MAX_FAILURES || now.duration_since(*since).as_secs() >= LOGIN_WINDOW_SECS
            }
            None => true,
        }
    }

    fn record_failure(&self, key: (IpAddr, String), now: Instant) {
        let mut map = self.failures.lock().unwrap();
        // Forgetting stale entries keeps the map bounded under scanning.
        map.retain(|_, (_, since)| now.duration_since(*since).as_secs() < LOGIN_WINDOW_SECS * 2);
        let entry = map.entry(key).or_insert((0, now));
        // A failure after the previous window expired starts a new window,
        // so hammering the door keeps it shut rather than counting into a
        // counter nobody reads.
        if entry.0 >= LOGIN_MAX_FAILURES
            || now.duration_since(entry.1).as_secs() >= LOGIN_WINDOW_SECS
        {
            *entry = (0, now);
        }
        entry.0 += 1;
    }

    fn clear(&self, key: &(IpAddr, String)) {
        self.failures.lock().unwrap().remove(key);
    }
}

// ------------------------------------------------------------------- errors

pub struct ApiError {
    status: StatusCode,
    body: serde_json::Value,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            body: serde_json::json!({ "error": message.into() }),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, message)
    }

    /// A commit that lost the race to another device.
    pub fn conflict(current: Option<ObjectId>) -> Self {
        ApiError {
            status: StatusCode::CONFLICT,
            body: serde_json::json!({
                "error": "vault head has moved",
                "current": current,
            }),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // A vault (or a share on one) that does not resolve must read as
        // "not there" to callers who cannot see it, exactly like the
        // not-found paths that answer directly.
        if e.to_string() == "no such vault" {
            ApiError::not_found("no such vault")
        } else {
            ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
        }
    }
}

// --------------------------------------------------------------------- auth

fn require_user(headers: &HeaderMap, state: &AppState) -> Result<String, ApiError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "missing Authorization header"))?;
    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "expected a bearer token"))?;
    state
        .db
        .user_for_token(token)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid token"))
}

// ------------------------------------------------------------------ request

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResp {
    token: String,
    user_id: String,
}

#[derive(Deserialize)]
struct CreateVaultReq {
    name: String,
    #[serde(default)]
    encrypted: bool,
}

#[derive(Serialize)]
struct VaultView {
    name: String,
    encrypted: bool,
    created: i64,
    /// "owner", "write" or "read" — vaults can now be shared.
    role: &'static str,
}

#[derive(Deserialize)]
struct HaveReq {
    ids: Vec<ObjectId>,
}

#[derive(Serialize)]
struct HaveResp {
    have: Vec<bool>,
}

#[derive(Serialize)]
struct HeadResp {
    snapshot: Option<ObjectId>,
}

#[derive(Deserialize)]
struct CommitReq {
    parent: Option<ObjectId>,
    snapshot: Snapshot,
}

#[derive(Serialize)]
struct CommitResp {
    snapshot: ObjectId,
}

#[derive(Serialize)]
struct PutObjectResp {
    created: bool,
}

#[derive(Deserialize)]
struct MoveQuery {
    from: String,
    to: String,
}

#[derive(Deserialize, Default)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ThumbQuery {
    path: String,
    #[serde(default = "default_thumb_size")]
    size: u32,
}

fn default_thumb_size() -> u32 {
    320
}

#[derive(Deserialize)]
struct TimelineQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    /// Return only photos taken before this timestamp (for paging).
    before: Option<i64>,
}

fn default_limit() -> usize {
    500
}

#[derive(Serialize)]
struct EntryView {
    name: String,
    path: String,
    kind: Kind,
    size: u64,
    mtime: i64,
    mime: &'static str,
    thumb: Option<String>,
}

#[derive(Serialize)]
struct ListResp {
    path: String,
    entries: Vec<EntryView>,
}

#[derive(Serialize)]
struct UploadResp {
    ok: bool,
    size: usize,
}

#[derive(Serialize)]
struct OkResp {
    ok: bool,
}

#[derive(Serialize)]
struct TimelineItem {
    path: String,
    taken_at: Option<i64>,
    width: u32,
    height: u32,
    size: u64,
    thumb: String,
}

#[derive(Serialize)]
struct TimelineResp {
    items: Vec<TimelineItem>,
}

// ----------------------------------------------------------------- handlers

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "service": "quarkdrive",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let key = LoginLimiter::key(addr.ip(), &req.username);
    if !state.logins.allowed(&key, Instant::now()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed sign-ins — wait ten minutes and try again",
        ));
    }
    let user_id = state
        .db
        .authenticate(&req.username, &req.password)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            state.logins.record_failure(key.clone(), Instant::now());
            ApiError::new(StatusCode::UNAUTHORIZED, "invalid username or password")
        })?;
    state.logins.clear(&key);
    let token = state
        .db
        .create_token(&user_id, None)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(LoginResp { token, user_id }))
}

#[derive(Deserialize)]
struct RegisterReq {
    username: String,
    password: String,
    /// The first vault to create for the account. Defaults to the username.
    #[serde(default)]
    vault: String,
}

#[derive(Serialize)]
struct RegisterResp {
    token: String,
    user_id: String,
    vault: String,
}

/// First-run sign-up, the way most self-hosted projects do it: while the
/// server has no accounts, the web UI offers to create the first one; after
/// that this endpoint closes permanently (accounts come from `create-user`).
async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<RegisterResp>, ApiError> {
    let vault_name = if req.vault.trim().is_empty() {
        req.username.trim().to_string()
    } else {
        req.vault.trim().to_string()
    };
    let (user_id, token, _vault_id) = state
        .db
        .register_first_user(&req.username, &req.password, &vault_name)
        .map_err(|e| {
            let msg = e.to_string();
            let status = if msg.contains("registration is closed") {
                StatusCode::FORBIDDEN
            } else if msg.contains("already taken") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::new(status, msg)
        })?;
    // Create the storage directories up front, as the authenticated
    // create-vault path does.
    if let Some(row) = state.db.vault_by_name(&vault_name)? {
        Vault::open(&state.data_dir, row)?;
    }
    Ok(Json(RegisterResp {
        token,
        user_id,
        vault: vault_name,
    }))
}

#[derive(Serialize)]
struct AuthStatus {
    /// True while the server has no accounts: the web UI offers sign-up.
    first_run: bool,
}

async fn auth_status(State(state): State<Arc<AppState>>) -> Result<Json<AuthStatus>, ApiError> {
    Ok(Json(AuthStatus {
        first_run: state.db.count_users()? == 0,
    }))
}

async fn whoami(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let username = state
        .db
        .username_for_id(&user_id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_default();
    Ok(Json(serde_json::json!({ "user_id": user_id, "username": username })))
}

async fn list_vaults(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let rows = state
        .db
        .list_vaults(&user_id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut vaults: Vec<VaultView> = rows
        .into_iter()
        .map(|v| VaultView {
            name: v.name,
            encrypted: v.encrypted,
            created: v.created,
            role: "owner",
        })
        .collect();
    let shared = state
        .db
        .list_shared_vaults(&user_id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for (v, role) in shared {
        let role = if role == "write" { "write" } else { "read" };
        vaults.push(VaultView {
            name: v.name,
            encrypted: v.encrypted,
            created: v.created,
            role,
        });
    }
    vaults.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(serde_json::json!({ "vaults": vaults })))
}

async fn create_vault(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateVaultReq>,
) -> Result<Json<VaultView>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let id = state.db.create_vault(&req.name, &user_id, req.encrypted)?;
    let row = state
        .db
        .vault_by_id(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "vault vanished"))?;
    // Create the storage directories up front.
    Vault::open(&state.data_dir, row.clone())?;
    Ok(Json(VaultView {
        name: row.name,
        encrypted: row.encrypted,
        created: row.created,
        role: "owner",
    }))
}

// ---------------------------------------------------------------- shares

#[derive(Deserialize)]
struct ShareReq {
    username: String,
    role: String,
}

#[derive(Serialize)]
struct ShareView {
    username: String,
    role: String,
    created: i64,
}

/// Vault owners manage who else can reach their vault. Shares are
/// read-only or read-write; there is deliberately no admin role.
async fn list_shares(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if role != Role::Owner {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the vault's owner can manage shares",
        ));
    }
    let shares: Vec<ShareView> = state
        .db
        .shares_for_vault(&v.row.id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(|s| ShareView {
            username: s.username,
            role: s.role,
            created: s.created,
        })
        .collect();
    Ok(Json(serde_json::json!({ "shares": shares })))
}

async fn create_share(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Json(req): Json<ShareReq>,
) -> Result<Json<ShareView>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if role != Role::Owner {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the vault's owner can manage shares",
        ));
    }
    if Role::parse(&req.role).is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "role must be \"read\" or \"write\"",
        ));
    }
    if v.row.encrypted {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "end-to-end encrypted vaults cannot be shared — the recipient has no key",
        ));
    }
    let target = state
        .db
        .user_id_for_username(req.username.trim())
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such user"))?;
    if target == v.row.owner_id {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "that user already owns this vault",
        ));
    }
    state
        .db
        .share_vault(&v.row.id, &target, &req.role)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    let created = state
        .db
        .shares_for_vault(&v.row.id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .find(|s| s.user_id == target)
        .map(|s| s.created)
        .unwrap_or_default();
    Ok(Json(ShareView {
        username: req.username.trim().to_string(),
        role: req.role,
        created,
    }))
}

async fn delete_share(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((vault, username)): Path<(String, String)>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if role != Role::Owner {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the vault's owner can manage shares",
        ));
    }
    let target = state
        .db
        .user_id_for_username(&username)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such user"))?;
    if state.db.unshare_vault(&v.row.id, &target)? {
        Ok(Json(OkResp { ok: true }))
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "that vault is not shared with that user",
        ))
    }
}

// ------------------------------------------------------- object protocol

async fn get_head(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<HeadResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    Ok(Json(HeadResp {
        snapshot: v.head()?,
    }))
}

async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((vault, id)): Path<(String, String)>,
) -> Result<Json<Snapshot>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let oid = ObjectId::from_hex(&id).map_err(|_| ApiError::not_found("bad object id"))?;
    v.snapshot(&oid)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no such snapshot"))
}

async fn have_objects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Json(req): Json<HaveReq>,
) -> Result<Json<HaveResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    // One query per batch rather than one per id.
    let mut out = Vec::with_capacity(req.ids.len());
    {
        for id in &req.ids {
            out.push(v.has_object(id));
        }
    }
    Ok(Json(HaveResp { have: out }))
}

async fn get_object(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((vault, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let oid = ObjectId::from_hex(&id).map_err(|_| ApiError::not_found("bad object id"))?;
    match v.get_object(&oid)? {
        Some(bytes) => Ok((
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response()),
        None => Err(ApiError::not_found("no such object")),
    }
}

async fn put_object(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((vault, id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<PutObjectResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    let oid = ObjectId::from_hex(&id).map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad object id"))?;
    // put_object verifies the content hash, so a wrong or corrupt upload is
    // rejected here rather than discovered later.
    let created = v.put_object(&oid, &body).map_err(ApiError::new_bad_request)?;
    Ok(Json(PutObjectResp { created }))
}

async fn commit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Json(req): Json<CommitReq>,
) -> Result<Json<CommitResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }

    // Optimistic concurrency: if the head is not where the client saw it,
    // its merge was computed against stale data.
    let current = v.head()?;
    if current != req.parent {
        return Err(ApiError::conflict(current));
    }
    if !v.has_object(&req.snapshot.root) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "snapshot root object has not been uploaded",
        ));
    }
    let id = v.put_snapshot(&req.snapshot)?;
    v.set_head(id)?;
    Ok(Json(CommitResp { snapshot: id }))
}

// -------------------------------------------------------------- file API

/// Present vault entries the way the file API's clients expect, including
/// server-relative thumbnail URLs for images.
fn entry_views(vault: &str, entries: Vec<crate::vault::Entry>) -> Vec<EntryView> {
    entries
        .into_iter()
        .map(|e| {
            let is_image = e.kind == Kind::File && media::is_image(&e.name);
            EntryView {
                thumb: if is_image {
                    Some(format!(
                        "/api/v1/vaults/{}/thumb?path={}&size=320",
                        vault,
                        urlencode(&e.path)
                    ))
                } else {
                    None
                },
                mime: if e.kind == Kind::Dir {
                    "inode/directory"
                } else {
                    media::mime_for(&e.name)
                },
                name: e.name,
                path: e.path,
                kind: e.kind,
                size: e.size,
                mtime: e.mtime,
            }
        })
        .collect()
}

async fn fs_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<ListResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let entries = v.list_dir(&q.path)?;

    let views = entry_views(&vault, entries);
    Ok(Json(ListResp {
        path: q.path,
        entries: views,
    }))
}

async fn fs_stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<VaultStats>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    Ok(Json(v.stats()?))
}

async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<ListResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let entries = v.search(&q.q, q.limit.unwrap_or(50).min(500))?;
    Ok(Json(ListResp {
        path: String::new(),
        entries: entry_views(&vault, entries),
    }))
}

async fn fs_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Response, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let data = v
        .read_file(&q.path)?
        .ok_or_else(|| ApiError::not_found("no such file"))?;
    let filename = q.path.rsplit('/').next().unwrap_or("download").to_string();
    Ok((
        [
            (header::CONTENT_TYPE, media::mime_for(&q.path).to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", sanitize_filename(&filename)),
            ),
        ],
        data,
    )
        .into_response())
}

async fn fs_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
    body: Bytes,
) -> Result<Json<UploadResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if q.path.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "path is required"));
    }
    let size = body.len();
    v.put_file(&q.path, &body, None)?;
    Ok(Json(UploadResp { ok: true, size }))
}

async fn fs_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if let Some(node_ref) = v.detach(&q.path)? {
        let kind = match node_ref.kind {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Symlink => "symlink",
        };
        state
            .db
            .trash_insert(
                &v.row.id,
                &q.path,
                &node_ref.id.to_string(),
                kind,
                node_ref.size,
            )
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(OkResp { ok: true }))
}

// ------------------------------------------------------- file versions

#[derive(Serialize)]
struct VersionView {
    /// The tree node's content address — the version's identity.
    id: String,
    size: u64,
    mtime: i64,
    snapshot_time: i64,
    device: String,
}

/// Every distinct content the path has ever held, newest first.
async fn fs_versions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let items: Vec<VersionView> = v
        .versions(&q.path, 100)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
        .into_iter()
        .map(|fv: FileVersion| VersionView {
            id: fv.node_id.to_string(),
            size: fv.size,
            mtime: fv.mtime,
            snapshot_time: fv.snapshot_time,
            device: fv.device,
        })
        .collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct VersionQuery {
    path: String,
    id: String,
}

async fn fs_version_download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<VersionQuery>,
) -> Result<Response, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let node_id = ObjectId::from_hex(&q.id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad version id"))?;
    let data = v
        .read_node_bytes(&node_id)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
        .ok_or_else(|| ApiError::not_found("no such version"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"version.bin\"".to_string(),
            ),
        ],
        data,
    )
        .into_response())
}

async fn fs_version_restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<VersionQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    let node_id = ObjectId::from_hex(&q.id)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad version id"))?;
    v.restore_version(&q.path, &node_id)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
        .ok_or_else(|| ApiError::not_found("no such version"))?;
    Ok(Json(OkResp { ok: true }))
}

// ---------------------------------------------------------- public links

#[derive(Deserialize)]
struct CreateLinkReq {
    path: String,
    #[serde(default)]
    password: Option<String>,
    /// Unix seconds; absent or null means the link never expires.
    #[serde(default)]
    expires_secs: Option<i64>,
}

#[derive(Serialize)]
struct LinkView {
    id: String,
    path: String,
    /// The visitor URL, relative to this server.
    url: String,
    has_password: bool,
    expires: Option<i64>,
    created: i64,
}

async fn create_link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Json(req): Json<CreateLinkReq>,
) -> Result<Json<LinkView>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if v.lookup_id(&req.path)?.is_none() {
        return Err(ApiError::not_found("no such path"));
    }
    let id = state
        .db
        .create_link(&v.row.id, &req.path, req.password.as_deref(), req.expires_secs)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    let row = state
        .db
        .link_get(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "link vanished"))?;
    Ok(Json(link_view(&row)))
}

fn link_view(row: &LinkRow) -> LinkView {
    LinkView {
        id: row.id.clone(),
        path: row.path.clone(),
        url: format!("/s/{}", row.id),
        has_password: row.has_password(),
        expires: row.expires,
        created: row.created,
    }
}

async fn list_links(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    let items: Vec<LinkView> = state
        .db
        .links_for_vault(&v.row.id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .iter()
        .map(link_view)
        .collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn delete_link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((vault, id)): Path<(String, String)>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if state.db.link_delete(&v.row.id, &id)? {
        Ok(Json(OkResp { ok: true }))
    } else {
        Err(ApiError::not_found("no such link"))
    }
}

/// What someone with the URL may read.
///
/// Passwords travel in an `X-Link-Password` header on every request: the
/// server keeps no link sessions, and nothing sensitive lands in a URL.
fn public_link(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
) -> Result<(LinkRow, Vault), ApiError> {
    let link = state
        .db
        .link_get(id)?
        .ok_or_else(|| ApiError::not_found("no such link"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if link.expired(now) {
        return Err(ApiError::new(StatusCode::GONE, "this link has expired"));
    }
    if link.has_password() {
        let given = headers
            .get("x-link-password")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify_link_password(&link, given) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "this link needs a password",
            ));
        }
    }
    let row = state
        .db
        .vault_by_id(&link.vault_id)?
        .ok_or_else(|| ApiError::not_found("no such link"))?;
    let vault = Vault::open(&state.data_dir, row)?;
    Ok((link, vault))
}

fn verify_link_password(link: &LinkRow, given: &str) -> bool {
    let (Some(hash_hex), Some(salt_hex)) = (&link.pass_hash, &link.pass_salt) else {
        return false;
    };
    let (Ok(hash), Ok(salt_bytes)) = (hex::decode(hash_hex), hex::decode(salt_hex)) else {
        return false;
    };
    let mut salt = [0u8; crypto::SALT_LEN];
    if salt_bytes.len() != crypto::SALT_LEN {
        return false;
    }
    salt.copy_from_slice(&salt_bytes);
    let actual = match crypto::derive_key_from_passphrase(given.as_bytes(), &salt) {
        Ok(k) => k.as_bytes().to_vec(),
        Err(_) => return false,
    };
    // Constant-time comparison, as with user passwords.
    let mut diff = 0u8;
    for (a, b) in hash.iter().zip(actual.iter()) {
        diff |= a ^ b;
    }
    diff == 0 && hash.len() == actual.len()
}

/// Join the link's root with a visitor-supplied relative path, rejecting
/// anything that escapes (split_path already refuses ".." and controls).
fn resolve_under_link(link: &LinkRow, rel: &str) -> Result<String, ApiError> {
    let base =
        split_path(&link.path).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    // An empty relative path means the link's root itself — same convention
    // as list_dir, and not an error.
    let rel_trim = rel.trim_matches('/');
    let rel_parts = if rel_trim.is_empty() {
        Vec::new()
    } else {
        split_path(rel_trim).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
    };
    Ok(base
        .iter()
        .chain(rel_parts.iter())
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("/"))
}

async fn public_meta(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (link, vault) = public_link(&state, &id, &headers)?;
    let (_node_id, node) = vault
        .lookup_id(&link.path)?
        .ok_or_else(|| ApiError::not_found("the shared item no longer exists"))?;
    Ok(Json(serde_json::json!({
        "id": link.id,
        "name": link.path.rsplit('/').next().unwrap_or(&link.path),
        "kind": if node.kind() == Kind::Dir { "dir" } else { "file" },
        "size": node.size(),
        "has_password": link.has_password(),
    })))
}

#[derive(Deserialize)]
struct PublicListQuery {
    #[serde(default)]
    path: String,
}

/// List a folder inside a shared link. `path` is relative to the link root.
async fn public_list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PublicListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (link, vault) = public_link(&state, &id, &headers)?;
    let full = resolve_under_link(&link, &q.path)?;
    let entries = vault
        .list_dir(&full)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    // Paths stay relative to the link, so the visitor never learns where in
    // the vault the share lives.
    let mut items: Vec<serde_json::Value> = Vec::new();
    for e in entry_views(&link.vault_id, entries) {
        let rel = if link.path.is_empty() {
            e.path.clone()
        } else {
            e.path[link.path.len() + 1..].to_string()
        };
        items.push(serde_json::json!({
            "name": e.name,
            "path": rel,
            "kind": e.kind,
            "size": e.size,
        }));
    }
    Ok(Json(serde_json::json!({ "path": q.path, "items": items })))
}

/// Download a file from a shared link.
async fn public_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PublicListQuery>,
) -> Result<Response, ApiError> {
    let (link, vault) = public_link(&state, &id, &headers)?;
    let full = resolve_under_link(&link, &q.path)?;
    let data = vault
        .read_file(&full)?
        .ok_or_else(|| ApiError::not_found("no such file"))?;
    let filename = full.rsplit('/').next().unwrap_or("download").to_string();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", sanitize_filename(&filename)),
            ),
        ],
        data,
    )
        .into_response())
}

#[derive(Serialize)]
struct TrashView {
    id: String,
    path: String,
    name: String,
    kind: String,
    size: u64,
    deleted_at: i64,
}

/// Deletions land here first. Files can be restored to their old path (or
/// a `name.restored-<time>` sibling if that is now taken) or purged.
/// Purging only drops the pointer — the content-addressed objects stay
/// until object-level garbage collection exists, which also means a purge
/// is not a secure erase.
async fn trash_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    let items: Vec<TrashView> = state
        .db
        .trash_list(&v.row.id)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(|t| TrashView {
            name: t.path.rsplit('/').next().unwrap_or(&t.path).to_string(),
            id: t.id,
            path: t.path,
            kind: t.kind,
            size: t.size,
            deleted_at: t.deleted_at,
        })
        .collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
struct RestoreQuery {
    id: String,
}

async fn trash_restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<RestoreQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    let row = state
        .db
        .trash_get(&v.row.id, &q.id)?
        .ok_or_else(|| ApiError::not_found("no such trash entry"))?;

    // The node id is its own content address, so the stored bytes must
    // still be in the vault; a missing node object means real corruption,
    // not a normal case.
    let node_id = ObjectId::from_hex(&row.node_id)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "corrupt trash entry"))?;
    if !v.has_object(&node_id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the deleted content is no longer in the vault",
        ));
    }

    let node_ref = NodeRef {
        id: node_id,
        kind: match row.kind.as_str() {
            "dir" => Kind::Dir,
            "symlink" => Kind::Symlink,
            _ => Kind::File,
        },
        size: row.size,
        mtime: row.deleted_at,
    };

    let mut target = row.path.clone();
    for attempt in 0.. {
        if v.lookup_id(&target)?.is_none() {
            break;
        }
        if attempt > 5 {
            return Err(ApiError::conflict(None));
        }
        let stem = row.path.rsplit('/').next().unwrap_or("item");
        let dir = &row.path[..row.path.len() - stem.len()];
                let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        target = format!("{dir}{stem}.restored-{secs}");
    }

    v.attach(&target, node_ref)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, e.to_string()))?;
    state.db.trash_remove(&v.row.id, &q.id)?;
    Ok(Json(serde_json::json!({ "ok": true, "path": target })))
}

async fn trash_purge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PurgeQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if q.all.unwrap_or(false) {
        for t in state.db.trash_list(&v.row.id)? {
            state.db.trash_remove(&v.row.id, &t.id)?;
        }
        return Ok(Json(OkResp { ok: true }));
    }
    let id = q
        .id
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "pass ?id= or ?all=true"))?;
    if state.db.trash_remove(&v.row.id, &id)? {
        Ok(Json(OkResp { ok: true }))
    } else {
        Err(ApiError::not_found("no such trash entry"))
    }
}

#[derive(Deserialize)]
struct PurgeQuery {
    id: Option<String>,
    all: Option<bool>,
}

async fn fs_mkdir(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    v.mkdir(&q.path)?;
    Ok(Json(OkResp { ok: true }))
}

/// Rename a file, or move it into another folder.
async fn fs_move(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<MoveQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, role) = state.vault(&vault, &user_id)?;
    if !role.can_write() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this vault is shared with you read-only",
        ));
    }
    if q.from.trim().is_empty() || q.to.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "from and to are required"));
    }
    v.move_path(&q.from, &q.to)?;
    Ok(Json(OkResp { ok: true }))
}

async fn fs_thumb(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    v.require_readable()?;

    let (node_id, node) = v
        .lookup_id(&q.path)?
        .ok_or_else(|| ApiError::not_found("no such file"))?;
    if node.kind() != Kind::File {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "not a file"));
    }
    let data = v.trees().read_file(&node)?;
    let size = q.size.clamp(16, 2048);
    let jpeg = media::thumbnail(&v, &node_id, &data, size)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("cannot thumbnail: {e}")))?;
    Ok((
        [(header::CONTENT_TYPE, "image/jpeg")],
        jpeg,
    )
        .into_response())
}

async fn timeline(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<TimelineQuery>,
) -> Result<Json<TimelineResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let (v, _role) = state.vault(&vault, &user_id)?;
    v.require_readable()?;
    let index = state.index(&v)?;

    let root = v.root()?;
    let listing = v.trees().walk(&root)?;
    let mut items = Vec::new();

    for (path, r) in listing {
        if r.kind != Kind::File || !media::is_image(&path) {
            continue;
        }
        let (node_id, _node) = match v.lookup_id(&path)? {
            Some(x) => x,
            None => continue,
        };

        // Re-index only when the file's node has changed.
        let row: MediaRow = match index.get(&path, &node_id)? {
            Some(cached) => cached,
            None => {
                let data = match v.read_file(&path)? {
                    Some(d) => d,
                    None => continue,
                };
                let (width, height) = media::dimensions(&data).unwrap_or((0, 0));
                // No EXIF (screenshots, scans): fall back to the file mtime.
                let taken_at = media::exif_taken_at(&data).or(Some(r.mtime));
                let row = MediaRow {
                    path: path.clone(),
                    node_id: node_id.to_hex(),
                    width,
                    height,
                    taken_at,
                    size: r.size,
                    updated: now_secs(),
                };
                index.upsert(&row)?;
                row
            }
        };

        if let Some(before) = q.before {
            if row.taken_at.unwrap_or(0) >= before {
                continue;
            }
        }

        items.push(TimelineItem {
            thumb: format!(
                "/api/v1/vaults/{}/thumb?path={}&size=400",
                vault,
                urlencode(&path)
            ),
            path,
            taken_at: row.taken_at,
            width: row.width,
            height: row.height,
            size: row.size,
        });
    }

    // Newest first.
    items.sort_by(|a, b| {
        b.taken_at
            .unwrap_or(0)
            .cmp(&a.taken_at.unwrap_or(0))
            .then_with(|| b.path.cmp(&a.path))
    });
    items.truncate(q.limit);

    Ok(Json(TimelineResp { items }))
}

// ------------------------------------------------------------------ helpers

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Percent-encode a path for use in a query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Strip characters that could break out of the Content-Disposition header.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/status", get(auth_status))
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/vaults", get(list_vaults).post(create_vault))
        .route(
            "/api/v1/vaults/:vault/shares",
            get(list_shares).post(create_share),
        )
        .route("/api/v1/vaults/:vault/shares/:username", delete(delete_share))
        .route(
            "/api/v1/vaults/:vault/trash",
            get(trash_list).delete(trash_purge),
        )
        .route("/api/v1/vaults/:vault/trash/restore", post(trash_restore))
        .route("/api/v1/vaults/:vault/fs/versions", get(fs_versions))
        .route(
            "/api/v1/vaults/:vault/fs/versions/download",
            get(fs_version_download),
        )
        .route(
            "/api/v1/vaults/:vault/fs/versions/restore",
            post(fs_version_restore),
        )
        .route(
            "/api/v1/vaults/:vault/links",
            get(list_links).post(create_link),
        )
        .route("/api/v1/vaults/:vault/links/:id", delete(delete_link))
        .route("/api/v1/public/:id", get(public_meta))
        .route("/api/v1/public/:id/list", get(public_list))
        .route("/api/v1/public/:id/download", get(public_download))
        // Object protocol used by the desktop and Android sync engine.
        .route("/api/v1/vaults/:vault/head", get(get_head))
        .route("/api/v1/vaults/:vault/snapshots/:id", get(get_snapshot))
        .route("/api/v1/vaults/:vault/objects/have", post(have_objects))
        .route(
            "/api/v1/vaults/:vault/objects/:id",
            get(get_object).put(put_object),
        )
        .route("/api/v1/vaults/:vault/commit", post(commit))
        // File API used by the web UI and phone clients.
        .route(
            "/api/v1/vaults/:vault/fs",
            get(fs_list).put(fs_upload).delete(fs_delete),
        )
        .route("/api/v1/vaults/:vault/fs/mkdir", post(fs_mkdir))
        .route("/api/v1/vaults/:vault/fs/move", post(fs_move))
        .route("/api/v1/vaults/:vault/fs/download", get(fs_download))
        .route("/api/v1/vaults/:vault/thumb", get(fs_thumb))
        .route("/api/v1/vaults/:vault/timeline", get(timeline))
        .route("/api/v1/vaults/:vault/stats", get(fs_stats))
        .route("/api/v1/vaults/:vault/search", get(search))
        // Any other /api path is a client bug — answer 404 so callers see it
        // — rather than falling through to the static UI's index.html, which
        // once hid a missing endpoint behind a 200.
        .route("/api", any(api_not_found))
        .route("/api/*rest", any(api_not_found))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Unmatched API paths return JSON 404; the static UI keeps the non-API
/// fallback (see main.rs).
async fn api_not_found(uri: Uri) -> Response {
    ApiError::not_found(&format!("no such API endpoint: {}", uri.path())).into_response()
}

impl ApiError {
    /// A rejected object upload: the client sent bytes that do not match the
    /// id it claimed, or that this server cannot accept.
    fn new_bad_request(e: anyhow::Error) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn lockout_key(user: &str) -> (IpAddr, String) {
        (IpAddr::V4(Ipv4Addr::LOCALHOST), user.to_string())
    }

    #[test]
    fn urlencode_leaves_safe_characters_alone() {
        assert_eq!(urlencode("photos/2024/img 1.jpg"), "photos%2F2024%2Fimg%201.jpg");
        assert_eq!(urlencode("simple-name_1.txt"), "simple-name_1.txt");
    }

    #[test]
    fn filenames_are_sanitized_for_headers() {
        assert_eq!(sanitize_filename("a\"b\\c\nd"), "abcd");
        assert_eq!(sanitize_filename("normal.jpg"), "normal.jpg");
    }

    #[test]
    fn five_failures_lock_the_door_for_the_window() {
        let limiter = LoginLimiter::default();
        let k = lockout_key("ada");
        let t0 = Instant::now();
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(limiter.allowed(&k, t0));
            limiter.record_failure(k.clone(), t0);
        }
        assert!(!limiter.allowed(&k, t0), "locked after {LOGIN_MAX_FAILURES} failures");
        // A different user from the same address is unaffected.
        assert!(limiter.allowed(&lockout_key("bob"), t0));
        // So is the same user from a different address.
        let elsewhere = (IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), "ada".into());
        assert!(limiter.allowed(&elsewhere, t0));
        // The lock expires with the window.
        let later = t0 + std::time::Duration::from_secs(LOGIN_WINDOW_SECS + 1);
        assert!(limiter.allowed(&k, later));
    }

    #[test]
    fn a_successful_sign_in_forgets_the_failures() {
        let limiter = LoginLimiter::default();
        let k = lockout_key("ada");
        let t0 = Instant::now();
        for _ in 0..LOGIN_MAX_FAILURES - 1 {
            limiter.record_failure(k.clone(), t0);
        }
        limiter.clear(&k);
        for _ in 0..LOGIN_MAX_FAILURES {
            assert!(limiter.allowed(&k, t0));
            limiter.record_failure(k.clone(), t0);
        }
        assert!(!limiter.allowed(&k, t0));
    }
}
