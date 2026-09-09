//! Thin blocking client for the Quarkdrive file API.
//!
//! The GUI never calls this on the UI thread — [`crate::app::App`] wraps
//! every call in a worker thread and receives the result as an event. This
//! module stays deliberately boring: plain requests, JSON in, JSON out, and
//! error messages good enough to show straight to a human.

use serde::Deserialize;
use std::io::Read;
use std::time::Duration;

/// Cap on how much of an error body we read before giving up.
const MAX_ERROR_BODY: u64 = 4096;

/// Thumbnails are server-side JPEGs at most ~2048px; this is generous.
const THUMB_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Api {
    agent: ureq::Agent,
    pub server: String,
    pub token: String,
}

impl Api {
    pub fn new(server: &str, token: &str) -> Result<Api, String> {
        Ok(Api {
            agent: agent()?,
            server: normalise_server(server)?,
            token: token.trim().to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &self.url(path))
            .set("Authorization", &format!("Bearer {}", self.token))
    }

    // ------------------------------------------------------------ auth

    /// GET /auth/status — true while the server has no accounts at all.
    pub fn first_run(server: &str) -> Result<bool, String> {
        let url = format!("{}/api/v1/auth/status", normalise_server(server)?);
        let resp = agent()?.get(&url).call().map_err(explain)?;
        #[derive(Deserialize)]
        struct Status {
            first_run: bool,
        }
        Ok(resp.into_json::<Status>().map_err(|e| e.to_string())?.first_run)
    }

    /// POST /auth/login → bearer token for later calls.
    pub fn login(server: &str, username: &str, password: &str) -> Result<(String, String), String> {
        let url = format!("{}/api/v1/auth/login", normalise_server(server)?);
        let resp = agent()?
            .post(&url)
            .send_json(serde_json::json!({ "username": username, "password": password }))
            .map_err(explain)?;
        #[derive(Deserialize)]
        struct Resp {
            token: String,
        }
        let r = resp.into_json::<Resp>().map_err(|e| e.to_string())?;
        Ok((r.token, normalise_server(server)?))
    }

    /// POST /auth/register — first-run sign-up; the server closes this for
    /// good once one account exists.
    pub fn register(
        server: &str,
        username: &str,
        password: &str,
        vault: &str,
    ) -> Result<(String, String), String> {
        let url = format!("{}/api/v1/auth/register", normalise_server(server)?);
        let resp = agent()?
            .post(&url)
            .send_json(serde_json::json!({
                "username": username,
                "password": password,
                "vault": vault,
            }))
            .map_err(explain)?;
        #[derive(Deserialize)]
        struct Resp {
            token: String,
            vault: String,
        }
        let r = resp.into_json::<Resp>().map_err(|e| e.to_string())?;
        Ok((r.token, r.vault))
    }

    // ---------------------------------------------------------- vaults

    pub fn vaults(&self) -> Result<Vec<VaultInfo>, String> {
        #[derive(Deserialize)]
        struct Resp {
            vaults: Vec<VaultInfo>,
        }
        Ok(self
            .req("GET", "/api/v1/vaults")
            .call()
            .map_err(explain)?
            .into_json::<Resp>()
            .map_err(|e| e.to_string())?
            .vaults)
    }

    pub fn create_vault(&self, name: &str) -> Result<(), String> {
        self.req("POST", "/api/v1/vaults")
            .send_json(serde_json::json!({ "name": name }))
            .map_err(explain)?;
        Ok(())
    }

    // ------------------------------------------------------------- fs

    pub fn list(&self, vault: &str, path: &str) -> Result<Vec<Entry>, String> {
        #[derive(Deserialize)]
        struct Resp {
            entries: Vec<Entry>,
        }
        Ok(self
            .req("GET", &format!("/api/v1/vaults/{vault}/fs"))
            .query("path", path)
            .call()
            .map_err(explain)?
            .into_json::<Resp>()
            .map_err(|e| e.to_string())?
            .entries)
    }

    pub fn upload(&self, vault: &str, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.req("PUT", &format!("/api/v1/vaults/{vault}/fs"))
            .set("Content-Type", "application/octet-stream")
            .query("path", path)
            .send(bytes)
            .map_err(explain)?;
        Ok(())
    }

    pub fn download(&self, vault: &str, path: &str) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        self.req(
            "GET",
            &format!("/api/v1/vaults/{vault}/fs/download"),
        )
        .query("path", path)
        .call()
        .map_err(explain)?
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
        Ok(buf)
    }

    pub fn delete(&self, vault: &str, path: &str) -> Result<(), String> {
        self.req("DELETE", &format!("/api/v1/vaults/{vault}/fs"))
            .query("path", path)
            .call()
            .map_err(explain)?;
        Ok(())
    }

    pub fn mkdir(&self, vault: &str, path: &str) -> Result<(), String> {
        self.req(
            "POST",
            &format!("/api/v1/vaults/{vault}/fs/mkdir"),
        )
        .query("path", path)
        .call()
        .map_err(explain)?;
        Ok(())
    }

    pub fn move_path(&self, vault: &str, from: &str, to: &str) -> Result<(), String> {
        self.req(
            "POST",
            &format!("/api/v1/vaults/{vault}/fs/move"),
        )
        .query("from", from)
        .query("to", to)
        .call()
        .map_err(explain)?;
        Ok(())
    }

    pub fn stats(&self, vault: &str) -> Result<Stats, String> {
        self.req(
            "GET",
            &format!("/api/v1/vaults/{vault}/stats"),
        )
        .call()
        .map_err(explain)?
        .into_json::<Stats>()
        .map_err(|e| e.to_string())
    }

    pub fn search(&self, vault: &str, query: &str) -> Result<Vec<Entry>, String> {
        #[derive(Deserialize)]
        struct Resp {
            entries: Vec<Entry>,
        }
        Ok(self
            .req("GET", &format!("/api/v1/vaults/{vault}/search"))
            .query("q", query)
            .call()
            .map_err(explain)?
            .into_json::<Resp>()
            .map_err(|e| e.to_string())?
            .entries)
    }

    // ---------------------------------------------------------- photos

    pub fn timeline(&self, vault: &str, limit: usize) -> Result<Vec<Photo>, String> {
        #[derive(Deserialize)]
        struct Resp {
            items: Vec<Photo>,
        }
        Ok(self
            .req(
                "GET",
                &format!("/api/v1/vaults/{vault}/timeline"),
            )
            .query("limit", &limit.to_string())
            .call()
            .map_err(explain)?
            .into_json::<Resp>()
            .map_err(|e| e.to_string())?
            .items)
    }

    /// Fetch and decode a server-generated thumbnail (always JPEG).
    pub fn thumb(&self, vault: &str, path: &str, size: u32) -> Result<image::DynamicImage, String> {
        let resp = self
            .req("GET", &format!("/api/v1/vaults/{vault}/thumb"))
            .query("path", path)
            .query("size", &size.to_string())
            .call()
            .map_err(explain)?;
        let mut data = Vec::new();
        resp.into_reader()
            .take(THUMB_LIMIT)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        image::load_from_memory(&data).map_err(|e| format!("bad thumbnail: {e}"))
    }
}

// -------------------------------------------------------------- types

#[derive(Debug, Clone, Deserialize)]
pub struct VaultInfo {
    pub name: String,
    pub encrypted: bool,
    #[allow(dead_code)]
    pub created: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: String, // "dir" | "file" | "symlink"
    pub size: u64,
    pub mtime: i64,
    #[allow(dead_code)]
    pub mime: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    #[allow(dead_code)]
    pub symlinks: u64,
    pub bytes: u64,
    #[allow(dead_code)]
    pub device: Option<String>,
    #[allow(dead_code)]
    pub host: Option<String>,
    pub updated: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Photo {
    pub path: String,
    pub taken_at: Option<i64>,
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    #[allow(dead_code)]
    pub size: u64,
}

// ------------------------------------------------------------ helpers

fn agent() -> Result<ureq::Agent, String> {
    Ok(ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build())
}

/// Accept "localhost:8787" as well as full URLs.
pub fn normalise_server(server: &str) -> Result<String, String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("server address is empty".into());
    }
    if s.contains("://") {
        Ok(s.to_string())
    } else {
        Ok(format!("http://{s}"))
    }
}

/// Turn any ureq failure into a one-line message worth showing a user.
/// Server errors carry {"error": "..."} — prefer that text over a generic
/// status code so problems like "registration is closed" stay readable.
fn explain(e: ureq::Error) -> String {
    if let ureq::Error::Status(code, resp) = e {
        let mut body = String::new();
        let _ = resp
            .into_reader()
            .take(MAX_ERROR_BODY)
            .read_to_string(&mut body);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(msg) = v.get("error").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
        }
        if !body.trim().is_empty() {
            return body.trim().to_string();
        }
        return format!("server returned HTTP {code}");
    }
    match e {
        ureq::Error::Transport(t) => format!("cannot reach server: {t}"),
        other => other.to_string(),
    }
}

/// "1.4 MB" style sizes everywhere in the UI.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Local-time-ish rendering of a unix timestamp, no timezone crate needed:
/// civil-from-days (Howard Hinnant's algorithm) gives us the date, and we
/// leave the time in UTC so it at least sorts and compares sanely.
pub fn human_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
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
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_server_addresses() {
        assert_eq!(normalise_server("localhost:8787").unwrap(), "http://localhost:8787");
        assert_eq!(normalise_server(" http://x.io/ ").unwrap(), "http://x.io");
        assert!(normalise_server("").is_err());
    }

    #[test]
    fn sizes_and_times_are_human() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_time(0), "1970-01-01 00:00:00");
    }
}
