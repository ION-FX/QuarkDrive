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
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use quarkdrive_core::hash::ObjectId;
use quarkdrive_core::tree::{Kind, Snapshot};

use crate::db::Db;
use crate::media::{self, MediaIndex, MediaRow};
use crate::vault::{Vault, VaultStats};

/// Uploads are limited by memory, not per request, so allow large files.
const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024 * 1024;

pub struct AppState {
    pub data_dir: PathBuf,
    pub db: Db,
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
            vaults: Mutex::new(HashMap::new()),
            indexes: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve a vault by name, enforcing that the caller owns it.
    pub fn vault(&self, name: &str, user_id: &str) -> anyhow::Result<Arc<Vault>> {
        let mut cache = self.vaults.lock().unwrap();
        if let Some(v) = cache.get(name) {
            if v.row.owner_id == user_id {
                return Ok(v.clone());
            }
            // Names are unique, so a different owner means "not yours".
            return Err(anyhow::anyhow!("no such vault"));
        }
        let row = self
            .db
            .vault_by_name(name)?
            .ok_or_else(|| anyhow::anyhow!("no such vault"))?;
        if row.owner_id != user_id {
            return Err(anyhow::anyhow!("no such vault"));
        }
        let vault = Arc::new(Vault::open(&self.data_dir, row)?);
        cache.insert(name.to_string(), vault.clone());
        Ok(vault)
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
        ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
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
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let user_id = state
        .db
        .authenticate(&req.username, &req.password)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid username or password"))?;
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
    let vaults: Vec<VaultView> = rows
        .into_iter()
        .map(|v| VaultView {
            name: v.name,
            encrypted: v.encrypted,
            created: v.created,
        })
        .collect();
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
    }))
}

// ------------------------------------------------------- object protocol

async fn get_head(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
) -> Result<Json<HeadResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;

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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
    Ok(Json(v.stats()?))
}

async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<ListResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
    v.remove(&q.path)?;
    Ok(Json(OkResp { ok: true }))
}

async fn fs_mkdir(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(vault): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<OkResp>, ApiError> {
    let user_id = require_user(&headers, &state)?;
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
    let v = state.vault(&vault, &user_id)?;
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
}
