//! WebDAV (class 1 with soft locks) over the file API.
//!
//! Endpoint: `/dav/{vault}/path/to/file`. Authentication is HTTP Basic —
//! username and password, the only scheme file managers and `davfs2`
//! speak. Write methods honour the caller's vault role, and deletions go
//! to the trash like everywhere else, so a mistaken drag in a file
//! manager is still recoverable.
//!
//! Deliberately minimal where the protocol allows: PROPFIND request bodies
//! are ignored (an allprop-style answer is always returned), and LOCK
//! grants soft locks kept in memory only — enough for gvfs and davfs2 to
//! mount happily, without pretending to be a full locking authority.

use crate::api::{AppState, Role};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quarkdrive_core::tree::Kind;

/// LOCKs expire after an hour of not being refreshed.
const LOCK_TTL_SECS: u64 = 3600;

/// Soft in-memory locks: (vault, path) → (token, issued at).
pub fn lock_registry(
) -> &'static Mutex<HashMap<(String, String), (String, std::time::Instant)>> {
    static REGISTRY: std::sync::OnceLock<
        Mutex<HashMap<(String, String), (String, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Entry point for `/dav/:vault` (the vault root itself).
pub async fn handle_root(
    State(state): State<Arc<AppState>>,
    Path(vault): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, vault, String::new(), method, headers, body).await
}

/// Entry point for `/dav/:vault/*path`.
pub async fn handle(
    State(state): State<Arc<AppState>>,
    Path((vault, rest)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, vault, rest, method, headers, body).await
}

async fn dispatch(
    state: Arc<AppState>,
    vault: String,
    rest: String,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authed = match authenticate(&state, &headers, &vault) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let ctx = Dav {
        state: &state,
        vault: authed.vault,
        role: authed.role,
        base: format!("/dav/{vault}"),
    };

    let rel = rest.trim_end_matches('/');
    match method.as_str() {
        "OPTIONS" => options(),
        "PROPFIND" => propfind(&ctx, rel, &headers),
        "GET" => get(&ctx, rel, false),
        "HEAD" => get(&ctx, rel, true),
        "PUT" => put(&ctx, rel, body),
        "MKCOL" => mkcol(&ctx, rel),
        "DELETE" => delete(&ctx, rel),
        "MOVE" => move_resource(&ctx, rel, &headers),
        "COPY" => copy_resource(&ctx, rel, &headers),
        "LOCK" => lock(&ctx, rel),
        "UNLOCK" => unlock(&ctx, rel),
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

struct Dav<'a> {
    state: &'a AppState,
    vault: Arc<crate::vault::Vault>,
    role: Role,
    base: String,
}

/// HTTP Basic: the username and password a file manager has on file.
fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    vault_name: &str,
) -> Result<Authed, Response> {
    let header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return Err(unauthorized());
    };
    let decoded = base64::decode(encoded.trim()).map_err(|_| unauthorized())?;
    let creds = String::from_utf8(decoded).map_err(|_| unauthorized())?;
    let Some((username, password)) = creds.split_once(':') else {
        return Err(unauthorized());
    };

    let user_id = state
        .db
        .authenticate(username, password)
        .map_err(|_| unauthorized())?
        .ok_or_else(|| unauthorized())?;

    let (vault, role) = state.vault(vault_name, &user_id).map_err(|_| {
        // Names are scoped per user: an unknown vault and someone else's
        // vault are the same "not yours".
        forbidden()
    })?;
    Ok(Authed { vault, role })
}

struct Authed {
    vault: Arc<crate::vault::Vault>,
    role: Role,
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("WWW-Authenticate", r#"Basic realm="Quarkdrive""#)],
        "authentication required",
    )
        .into_response()
}

fn forbidden() -> Response {
    // Same answer as a vault that does not exist: names are scoped per
    // user, and existence is not ours to reveal.
    (StatusCode::NOT_FOUND, "no such vault").into_response()
}

fn forbidden_write() -> Response {
    (StatusCode::FORBIDDEN, "read-only access").into_response()
}

fn options() -> Response {
    (
        StatusCode::OK,
        [
            ("DAV", "1, 2"),
            (
                "Allow",
                "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, MOVE, COPY, LOCK, UNLOCK",
            ),
            ("Content-Length", "0"),
        ],
        "",
    )
        .into_response()
}

// -------------------------------------------------------------- handlers

fn propfind(ctx: &Dav, rel: &str, headers: &HeaderMap) -> Response {
    let depth = headers
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");
    if depth.eq_ignore_ascii_case("infinity") {
        // RFC 4918 lets a server refuse infinite depth.
        return (StatusCode::FORBIDDEN, "infinite depth is not supported").into_response();
    }

    // An empty path is the vault root itself — same convention as list_dir.
    let full = if rel.is_empty() {
        String::new()
    } else {
        match crate::vault::split_path(rel) {
            Ok(parts) => parts.join("/"),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };
    let Some((_, node)) = ctx.vault.lookup_id(&full).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let is_dir = node.kind() == Kind::Dir;

    let mut body = String::from(xml_open());
    body.push_str(&response_xml(
        &href_for(&ctx.base, &full, is_dir),
        (node.kind(), node.size(), node.mtime()),
    ));
    if is_dir && depth == "1" {
        if let Ok(entries) = ctx.vault.list_dir(&full) {
            for e in entries {
                body.push_str(&response_xml(
                    &href_for(&ctx.base, &e.path, e.kind == Kind::Dir),
                    (e.kind, e.size, e.mtime),
                ));
            }
        }
    }
    body.push_str("</D:multistatus>\n");
    multistatus(body)
}

fn get(ctx: &Dav, rel: &str, head_only: bool) -> Response {
    match ctx.vault.read_file(rel) {
        Ok(Some(data)) => {
            let name = rel.rsplit('/').next().unwrap_or("file");
            let headers = [
                ("Content-Type".to_string(), crate::media::mime_for(name).to_string()),
                ("Content-Length".to_string(), data.len().to_string()),
            ];
            if head_only {
                (StatusCode::OK, headers, "").into_response()
            } else {
                (StatusCode::OK, headers, data).into_response()
            }
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

fn put(ctx: &Dav, rel: &str, body: Bytes) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    let existed = ctx.vault.read_file(rel).map(|f| f.is_some()).unwrap_or(false);
    match ctx.vault.put_file(rel, &body, None) {
        Ok(()) => {
            if existed {
                StatusCode::NO_CONTENT.into_response()
            } else {
                StatusCode::CREATED.into_response()
            }
        }
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

fn mkcol(ctx: &Dav, rel: &str) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    match ctx.vault.mkdir(rel) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already exists") {
                StatusCode::METHOD_NOT_ALLOWED.into_response()
            } else if msg.contains("parent") || msg.contains("not a directory") {
                StatusCode::CONFLICT.into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        }
    }
}

fn delete(ctx: &Dav, rel: &str) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    // Trash, like the web UI: a mistaken drag in a file manager is still
    // recoverable, and purge still erases for real.
    match ctx.vault.detach(rel) {
        Ok(Some(node_ref)) => {
            let kind = match node_ref.kind {
                Kind::File => "file",
                Kind::Dir => "dir",
                Kind::Symlink => "symlink",
            };
            let _ = ctx
                .state
                .db
                .trash_insert(&ctx.vault.row.id, rel, &node_ref.id.to_string(), kind, node_ref.size);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

fn move_resource(ctx: &Dav, rel: &str, headers: &HeaderMap) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    let Some(dest) = destination_path(headers, &ctx.base) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match ctx.vault.move_path(rel, &dest) {
        Ok(()) => {
            let mut registry = lock_registry().lock().unwrap();
            let from = (ctx.base.clone(), format!("/{rel}"));
            if let Some((token, at)) = registry.remove(&from) {
                registry.insert((ctx.base.clone(), format!("/{dest}")), (token, at));
            }
            StatusCode::CREATED.into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already exists") || msg.contains("inside itself") {
                StatusCode::CONFLICT.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    }
}

fn copy_resource(ctx: &Dav, rel: &str, headers: &HeaderMap) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    let Some(dest) = destination_path(headers, &ctx.base) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match ctx.vault.read_file(rel) {
        Ok(Some(data)) => match ctx.vault.put_file(&dest, &data, None) {
            Ok(()) => StatusCode::CREATED.into_response(),
            Err(_) => StatusCode::CONFLICT.into_response(),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

// ------------------------------------------------------------------ locks

fn lock(ctx: &Dav, rel: &str) -> Response {
    if !ctx.role.can_write() {
        return forbidden_write();
    }
    let token = format!("opaquelocktoken:quarkdrive-{}", crate::db::Db::pending_id());
    lock_registry().lock().unwrap().insert(
        (ctx.base.clone(), format!("/{rel}")),
        (token.clone(), std::time::Instant::now()),
    );
    let body = [
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock>"#,
        r#"<D:locktoken><D:href>"#,
        &token,
        r#"</D:href></D:locktoken>"#,
        r#"<D:timeout>Second-"#,
        &LOCK_TTL_SECS.to_string(),
        r#"</D:timeout></D:activelock></D:lockdiscovery></D:prop>"#,
    ]
    .concat();
    (
        StatusCode::OK,
        [("Content-Type", "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

fn unlock(ctx: &Dav, rel: &str) -> Response {
    lock_registry()
        .lock()
        .unwrap()
        .remove(&(ctx.base.clone(), format!("/{rel}")));
    StatusCode::NO_CONTENT.into_response()
}

// ------------------------------------------------------------------- xml

fn xml_open() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>"#.to_string()
        + r#"<D:multistatus xmlns:D="DAV:">"# 
        + "\n"
}

fn multistatus(body: String) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [("Content-Type", "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

/// One `<D:response>` element for a resource that exists.
fn response_xml(href: &str, stat: (Kind, u64, i64)) -> String {
    let (kind, size, mtime) = stat;
    let (resourcetype, ctype) = if kind == Kind::Dir {
        ("<D:collection/>".to_string(), "httpd/unix-directory".to_string())
    } else {
        (String::new(), "application/octet-stream".to_string())
    };
    let modified = rfc1123_time(mtime);
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:resourcetype>{resourcetype}</D:resourcetype>\
         <D:getlastmodified>{modified}</D:getlastmodified>\
         <D:getcontentlength>{size}</D:getcontentlength>\
         <D:getcontenttype>{ctype}</D:getcontenttype>\
         </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n",
        href = escape_xml(href),
        resourcetype = resourcetype,
        modified = modified,
        size = size,
        ctype = escape_xml(&ctype),
    )
}

fn href_for(base: &str, rel: &str, is_dir: bool) -> String {
    let rel = rel.trim_matches('/');
    let mut href = if rel.is_empty() {
        format!("{base}/")
    } else {
        format!(
            "{base}/{}",
            rel.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
        )
    };
    if is_dir && !href.ends_with('/') {
        href.push('/');
    }
    href
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Parse a `Destination` header into a path inside this same share.
fn destination_path(headers: &HeaderMap, href_base: &str) -> Option<String> {
    let dest = headers.get("destination")?.to_str().ok()?;
    let idx = dest.find(href_base)?;
    let path = &dest[idx + href_base.len()..];
    let trimmed = percent_decode(path).trim_end_matches('/').to_string();
    let parts = crate::vault::split_path(&trimmed).ok()?;
    Some(parts.join("/"))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                16,
            ) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 1123 date, the form WebDAV's getlastmodified expects
/// ("Sun, 02 Jan 2005 00:00:00 GMT"). All times are UTC.
fn rfc1123_time(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // 1970-01-01 was a Thursday; DAYS is arranged so the modulo lands there.
    let weekday = DAYS[days.rem_euclid(7) as usize];

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!(
        "{weekday}, {d:02} {month} {y:04} {h:02}:{mi:02}:{s:02} GMT",
        month = MONTHS[(m - 1) as usize]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_specials_are_escaped() {
        assert_eq!(
            escape_xml(r#"notes & <ideas> "quoted""#),
            r#"notes &amp; &lt;ideas&gt; &quot;quoted&quot;"#
        );
    }

    #[test]
    fn hrefs_are_percent_encoded_and_directories_get_slashes() {
        assert_eq!(href_for("/dav/v", "", true), "/dav/v/");
        assert_eq!(href_for("/dav/v", "docs", true), "/dav/v/docs/");
        assert_eq!(href_for("/dav/v", "docs/report.txt", false), "/dav/v/docs/report.txt");
        assert_eq!(
            href_for("/dav/v", "my docs/holiday photo.jpg", false),
            "/dav/v/my%20docs/holiday%20photo.jpg"
        );
    }

    #[test]
    fn destinations_across_url_forms_resolve() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "destination",
            "http://host:8787/dav/vault/other%20name.txt".parse().unwrap(),
        );
        assert_eq!(
            destination_path(&headers, "/dav/vault").as_deref(),
            Some("other name.txt")
        );

        headers.insert("destination", "/dav/vault/a/b.txt".parse().unwrap());
        assert_eq!(
            destination_path(&headers, "/dav/vault").as_deref(),
            Some("a/b.txt")
        );

        // Escapes and foreign destinations are refused.
        headers.insert("destination", "/dav/vault/../other".parse().unwrap());
        assert_eq!(destination_path(&headers, "/dav/vault"), None);
        headers.insert("destination", "http://evil.example/other".parse().unwrap());
        assert_eq!(destination_path(&headers, "/dav/vault"), None);
    }

    #[test]
    fn rfc1123_dates_render() {
        assert_eq!(rfc1123_time(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // A known instant: 2026-09-16 12:00:00 UTC is Wednesday.
        assert_eq!(
            rfc1123_time(1_789_560_000),
            "Wed, 16 Sep 2026 12:00:00 GMT"
        );
    }

    #[test]
    fn percent_decode_handles_escapes_and_garbage() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2Fx"), "/x");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
