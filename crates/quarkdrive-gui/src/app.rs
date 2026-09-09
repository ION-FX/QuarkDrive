//! Application state for the Quarkdrive desktop GUI.
//!
//! Everything the UI can do lives here as a plain method (`sign_in`,
//! `open_dir`, `upload_manual`, …) that validates, spawns one worker thread
//! and returns immediately; results come back through an mpsc channel and
//! are applied in [`App::handle`]. That keeps the UI thread free of blocking
//! IO and makes the whole app testable without a display: a test drives the
//! same methods a click would and runs egui frames with `Context::run`.

use crate::api::{self, Api, Entry, Photo, Stats, VaultInfo};
use egui::TextureHandle;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Files,
    Photos,
}

/// What the file list is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Dir,
    Search(String),
}

/// One completed piece of work, sent from a worker thread.
pub enum Event {
    FirstRun(Result<bool, String>),
    LoggedIn(Result<(String, Vec<VaultInfo>), String>), // (token, vaults)
    Listed(Result<Vec<Entry>, String>),
    Stats(Result<Stats, String>),
    Uploaded(Result<String, String>),
    MkdirDone(Result<String, String>),
    MoveDone(Result<String, String>),
    DeleteDone(Result<String, String>),
    Downloaded(Result<String, String>),
    Vaults(Result<Vec<VaultInfo>, String>),
    Searched(Result<Vec<Entry>, String>),
    Timeline(Result<Vec<Photo>, String>),
    Thumb {
        path: String,
        img: Result<egui::ColorImage, String>,
    },
    Preview {
        path: String,
        img: Result<egui::ColorImage, String>,
    },
}

/// Small remember-me file: server + username + last vault, never the password.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub server: String,
    pub username: String,
    pub vault: String,
}

fn config_path() -> Option<std::path::PathBuf> {
    // Tests must not read the developer's remembered session.
    if std::env::var("QD_TEST_MODE").is_ok() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let dir = std::path::PathBuf::from(home).join(".config").join("quarkdrive");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("gui.json"))
}

impl Config {
    fn load() -> Config {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Some(path) = config_path() {
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

pub struct App {
    pub tx: Sender<Event>,
    rx: Receiver<Event>,
    /// Worker threads in flight; actions stay disabled until this is zero.
    pub busy: usize,
    /// egui hands us a context on the first frame; textures need it.
    ctx: Option<egui::Context>,

    // ------------------------------------------------------- login form
    pub server: String,
    pub username: String,
    pub password: String,
    pub vault_buf: String,
    /// None until we've asked the server; Some(true) means first-run
    /// sign-up should be offered, mirroring the web UI.
    pub signup_hint: Option<bool>,
    pub login_err: Option<String>,
    /// Guards against re-asking /auth/status every frame.
    pub status_checked: bool,

    // --------------------------------------------------------- session
    pub api: Option<Api>,
    pub vaults: Vec<VaultInfo>,
    pub vault: Option<String>,
    pub tab: Tab,

    // ----------------------------------------------------------- files
    pub listing: Listing,
    pub cwd: String,
    pub entries: Vec<Entry>,
    pub selected: Option<String>,
    pub rename: Option<(String, String)>,
    pub mkdir_open: bool,
    pub mkdir_buf: String,
    pub upload_buf: String,
    /// Two-step delete: armed path awaiting confirmation.
    pub delete_arm: Option<String>,
    pub stats: Option<Stats>,
    pub search_buf: String,
    /// Input for the "no vaults yet" creation form.
    pub vault_new_buf: String,

    // ---------------------------------------------------------- photos
    pub photos: Option<Result<Vec<Photo>, String>>,
    pub thumbs: HashMap<String, TextureHandle>,
    pub thumb_pending: HashSet<String>,
    pub preview: Option<(String, TextureHandle)>,

    /// (is_error, message) shown in the status bar.
    pub note: Option<(bool, String)>,
    config: Config,
}

impl App {
    pub fn new() -> App {
        let (tx, rx) = std::sync::mpsc::channel();
        let config = Config::load();
        App {
            tx,
            rx,
            busy: 0,
            ctx: None,
            server: if config.server.is_empty() {
                "http://localhost:8787".to_string()
            } else {
                config.server.clone()
            },
            username: config.username.clone(),
            password: String::new(),
            vault_buf: String::new(),
            signup_hint: None,
            login_err: None,
            status_checked: false,
            api: None,
            vaults: Vec::new(),
            vault: None,
            tab: Tab::Files,
            listing: Listing::Dir,
            cwd: String::new(),
            entries: Vec::new(),
            selected: None,
            rename: None,
            mkdir_open: false,
            mkdir_buf: String::new(),
            upload_buf: String::new(),
            delete_arm: None,
            stats: None,
            search_buf: String::new(),
            vault_new_buf: String::new(),
            photos: None,
            thumbs: HashMap::new(),
            thumb_pending: HashSet::new(),
            preview: None,
            note: None,
            config,
        }
    }

    /// Called by the UI on every frame before drawing.
    pub fn set_ctx(&mut self, ctx: &egui::Context) {
        self.ctx = Some(ctx.clone());
    }

    /// Drain finished worker results; called once per frame.
    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.busy = self.busy.saturating_sub(1);
            self.handle(ev);
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::FirstRun(res) => match res {
                Ok(first) => {
                    self.signup_hint = Some(first);
                    if first && self.vault_buf.is_empty() {
                        self.vault_buf = self.username.clone();
                    }
                }
                Err(e) => self.note = Some((true, e)),
            },
            Event::LoggedIn(res) => match res {
                Ok((token, vaults)) => {
                    self.login_err = None;
                    self.password.clear();
                    self.api = Api::new(&self.server, &token).ok();
                    self.vaults = vaults;
                    let remembered = self.config.vault.clone();
                    let pick = self
                        .vaults
                        .iter()
                        .find(|v| v.name == remembered)
                        .or_else(|| self.vaults.first())
                        .map(|v| v.name.clone());
                    self.config.server = self.server.clone();
                    self.config.username = self.username.clone();
                    self.config.save();
                    match pick {
                        Some(v) => self.pick_vault(&v),
                        None => {
                            self.vault = None;
                            self.note =
                                Some((false, "signed in — this account has no vaults yet".into()));
                        }
                    }
                }
                Err(e) => self.login_err = Some(e),
            },
            Event::Listed(res) => match res {
                Ok(entries) => self.entries = entries,
                Err(e) => self.note = Some((true, e)),
            },
            Event::Stats(res) => match res {
                Ok(s) => self.stats = Some(s),
                Err(e) => self.note = Some((true, e)),
            },
            Event::Uploaded(res) => match res {
                Ok(msg) => {
                    self.upload_buf.clear();
                    self.note = Some((false, msg));
                    self.refresh();
                }
                Err(e) => self.note = Some((true, e)),
            },
            Event::MkdirDone(res) => match res {
                Ok(msg) => {
                    self.mkdir_open = false;
                    self.mkdir_buf.clear();
                    self.note = Some((false, msg));
                    self.refresh();
                }
                Err(e) => self.note = Some((true, e)),
            },
            Event::MoveDone(res) => match res {
                Ok(msg) => {
                    self.rename = None;
                    self.note = Some((false, msg));
                    self.refresh();
                }
                Err(e) => {
                    self.rename = None;
                    self.note = Some((true, e));
                }
            },
            Event::DeleteDone(res) => match res {
                Ok(msg) => {
                    self.delete_arm = None;
                    self.selected = None;
                    self.note = Some((false, msg));
                    self.refresh();
                }
                Err(e) => {
                    self.delete_arm = None;
                    self.note = Some((true, e));
                }
            },
            Event::Downloaded(res) => match res {
                Ok(msg) => self.note = Some((false, msg)),
                Err(e) => self.note = Some((true, e)),
            },
            Event::Vaults(res) => match res {
                Ok(vaults) => {
                    self.vaults = vaults;
                    if self.vault.is_none() {
                        if let Some(v) = self.vaults.first().map(|v| v.name.clone()) {
                            self.pick_vault(&v);
                        }
                    }
                }
                Err(e) => self.note = Some((true, e)),
            },
            Event::Searched(res) => match res {
                Ok(entries) => self.entries = entries,
                Err(e) => self.note = Some((true, e)),
            },
            Event::Timeline(res) => self.photos = Some(res),
            Event::Thumb { path, img } => {
                self.thumb_pending.remove(&path);
                if let Ok(img) = img {
                    if let Some(tex) = self.make_texture(&format!("thumb:{path}"), img) {
                        self.thumbs.insert(path, tex);
                    }
                }
            }
            Event::Preview { path, img } => match img {
                Ok(img) => {
                    if let Some(tex) = self.make_texture(&format!("preview:{path}"), img) {
                        self.preview = Some((path, tex));
                    }
                }
                Err(e) => self.note = Some((true, e)),
            },
        }
    }

    fn make_texture(&self, name: &str, img: egui::ColorImage) -> Option<TextureHandle> {
        let ctx = self.ctx.as_ref()?;
        Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR))
    }

    // -------------------------------------------------------- plumbing

    fn spawn<F>(&mut self, job: F)
    where
        F: FnOnce() -> Event + Send + 'static,
    {
        let tx = self.tx.clone();
        self.busy += 1;
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    fn api_or_note(&mut self) -> Option<Api> {
        match &self.api {
            Some(a) => Some(a.clone()),
            None => {
                self.note = Some((true, "not signed in".into()));
                None
            }
        }
    }

    // ----------------------------------------------------------- auth

    /// Ask the server whether it still needs a first account.
    pub fn check_status(&mut self) {
        self.status_checked = true;
        let server = self.server.clone();
        self.spawn(move || Event::FirstRun(api::Api::first_run(&server)));
    }

    /// The login card's primary button: sign in, or sign up on first run.
    pub fn primary(&mut self) {
        if self.busy > 0 {
            return;
        }
        match self.signup_hint {
            None => self.check_status(),
            Some(true) => self.sign_up(),
            Some(false) => self.sign_in(),
        }
    }

    pub fn sign_in(&mut self) {
        let (server, user, pass) =
            (self.server.clone(), self.username.clone(), self.password.clone());
        self.spawn(move || match Api::login(&server, &user, &pass) {
            Ok((token, _)) => Self::fetch_session_static(&server, &token),
            Err(e) => Event::LoggedIn(Err(e)),
        });
    }

    pub fn sign_up(&mut self) {
        let (server, user, pass, vault) = (
            self.server.clone(),
            self.username.clone(),
            self.password.clone(),
            self.vault_buf.clone(),
        );
        if let Err(e) = validate_signup(&user, &pass, &vault) {
            self.login_err = Some(e);
            return;
        }
        self.spawn(move || match Api::register(&server, &user, &pass, &vault) {
            Ok((token, _vault)) => Self::fetch_session_static(&server, &token),
            Err(e) => Event::LoggedIn(Err(e)),
        });
    }

    fn fetch_session_static(server: &str, token: &str) -> Event {
        match Api::new(server, token) {
            Ok(a) => match a.vaults() {
                Ok(vaults) => Event::LoggedIn(Ok((token.to_string(), vaults))),
                Err(e) => Event::LoggedIn(Err(e)),
            },
            Err(e) => Event::LoggedIn(Err(e)),
        }
    }

    pub fn logout(&mut self) {
        self.api = None;
        self.vault = None;
        self.vaults.clear();
        self.entries.clear();
        self.stats = None;
        self.photos = None;
        self.thumbs.clear();
        self.thumb_pending.clear();
        self.preview = None;
        self.cwd.clear();
        self.listing = Listing::Dir;
        self.selected = None;
        self.signup_hint = None;
        self.tab = Tab::Files;
        self.note = Some((false, "signed out".into()));
    }

    // ---------------------------------------------------------- vault

    pub fn pick_vault(&mut self, name: &str) {
        self.vault = Some(name.to_string());
        self.config.vault = name.to_string();
        self.config.save();
        self.cwd.clear();
        self.listing = Listing::Dir;
        self.selected = None;
        self.photos = None;
        self.thumbs.clear();
        self.thumb_pending.clear();
        self.preview = None;
        self.refresh();
    }

    pub fn create_vault(&mut self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            self.note = Some((true, "vault name cannot be empty or contain '/'".into()));
            return;
        }
        let Some(a) = self.api_or_note() else { return };
        self.spawn(move || match a.create_vault(&name) {
            Ok(()) => match a.vaults() {
                Ok(vaults) => Event::Vaults(Ok(vaults)),
                Err(e) => Event::Vaults(Err(e)),
            },
            Err(e) => Event::Vaults(Err(format!("{e}"))),
        });
    }

    // ----------------------------------------------------------- files

    /// Re-list the current directory and refresh stats.
    pub fn refresh(&mut self) {
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        let path = self.cwd.clone();
        self.spawn(move || Event::Listed(a.list(&vault, &path)));
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.spawn(move || Event::Stats(a.stats(&vault)));
    }

    pub fn open_dir(&mut self, path: &str) {
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.cwd = path.to_string();
        self.listing = Listing::Dir;
        self.selected = None;
        self.delete_arm = None;
        let path = path.to_string();
        self.spawn(move || Event::Listed(a.list(&vault, &path)));
    }

    pub fn up(&mut self) {
        let parent = match self.cwd.rfind('/') {
            Some(i) => self.cwd[..i].to_string(),
            None => String::new(),
        };
        self.open_dir(&parent);
    }

    pub fn select(&mut self, path: &str) {
        self.selected = Some(path.to_string());
        self.delete_arm = None;
    }

    pub fn do_search(&mut self) {
        let q = self.search_buf.trim().to_string();
        if q.is_empty() {
            return;
        }
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.listing = Listing::Search(q.clone());
        self.selected = None;
        self.spawn(move || Event::Searched(a.search(&vault, &q)));
    }

    pub fn close_search(&mut self) {
        self.search_buf.clear();
        self.open_dir("");
    }

    /// Pick a local file with zenity/kdialog when available; returns None
    /// (with a note) when no dialog helper exists — the manual path field
    /// always works as a fallback.
    pub fn upload_via_picker(&mut self) {
        match pick_file() {
            Some(local) => {
                self.upload_buf = local;
                self.upload_manual();
            }
            None => {
                self.note = Some((true, "no file picker found (install zenity) — use the path field below".into()));
            }
        }
    }

    pub fn upload_manual(&mut self) {
        let local = self.upload_buf.trim().to_string();
        if local.is_empty() {
            self.note = Some((true, "give a local file path to upload".into()));
            return;
        }
        let name = std::path::Path::new(&local)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let Some(name) = name else {
            self.note = Some((true, format!("not a file path: {local}")));
            return;
        };
        let bytes = match std::fs::read(&local) {
            Ok(b) => b,
            Err(e) => {
                self.note = Some((true, format!("cannot read {local}: {e}")));
                return;
            }
        };
        self.upload_bytes(&name, bytes);
    }

    /// Upload raw bytes under `name` into the current directory.
    pub fn upload_bytes(&mut self, name: &str, bytes: Vec<u8>) {
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        let path = join(&self.cwd, name);
        let shown = path.clone();
        let n = bytes.len();
        self.spawn(move || match a.upload(&vault, &path, &bytes) {
            Ok(()) => {
                Event::Uploaded(Ok(format!("uploaded {shown} ({})", api::human_size(n as u64))))
            }
            Err(e) => Event::Uploaded(Err(e)),
        });
    }

    pub fn download(&mut self, path: &str) {
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        let dir = download_dir();
        let name = path.rsplit('/').next().unwrap_or("file").to_string();
        let target = dir.join(&name);
        let shown = target.display().to_string();
        let path = path.to_string();
        self.spawn(move || match a.download(&vault, &path) {
            Ok(bytes) => match std::fs::write(&target, &bytes) {
                Ok(()) => Event::Downloaded(Ok(format!(
                    "saved {shown} ({})",
                    api::human_size(bytes.len() as u64)
                ))),
                Err(e) => Event::Downloaded(Err(format!("cannot write {shown}: {e}"))),
            },
            Err(e) => Event::Downloaded(Err(e)),
        });
    }

    pub fn download_selected(&mut self) {
        if let Some(p) = self.selected.clone() {
            self.download(&p);
        }
    }

    pub fn start_rename(&mut self, path: &str) {
        let name = path.rsplit('/').next().unwrap_or("").to_string();
        self.rename = Some((path.to_string(), name));
    }

    pub fn commit_rename(&mut self) {
        let Some((from, buf)) = self.rename.clone() else { return };
        let new_name = buf.trim().to_string();
        if new_name.is_empty() || new_name.contains('/') {
            self.note = Some((true, "name cannot be empty or contain '/'".into()));
            return;
        }
        let to = match from.rfind('/') {
            Some(i) => format!("{}/{}", &from[..i], new_name),
            None => new_name,
        };
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.spawn(move || match a.move_path(&vault, &from, &to) {
            Ok(()) => Event::MoveDone(Ok(format!("renamed to {to}"))),
            Err(e) => Event::MoveDone(Err(e)),
        });
    }

    /// Second half of the two-step delete.
    pub fn confirm_delete(&mut self) {
        let Some(path) = self.delete_arm.clone() else { return };
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        if self.selected.as_deref() == Some(path.as_str()) {
            self.selected = None;
        }
        self.spawn(move || match a.delete(&vault, &path) {
            Ok(()) => Event::DeleteDone(Ok(format!("deleted {path}"))),
            Err(e) => Event::DeleteDone(Err(e)),
        });
    }

    pub fn do_mkdir(&mut self) {
        let name = self.mkdir_buf.trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        let path = join(&self.cwd, &name);
        self.spawn(move || match a.mkdir(&vault, &path) {
            Ok(()) => Event::MkdirDone(Ok(format!("created folder {path}"))),
            Err(e) => Event::MkdirDone(Err(e)),
        });
    }

    // ---------------------------------------------------------- photos

    pub fn open_photos(&mut self) {
        if self.photos.is_some() {
            return;
        }
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.spawn(move || Event::Timeline(a.timeline(&vault, 500)));
    }

    /// The photo grid calls this for cells that scrolled into view.
    pub fn request_thumb(&mut self, path: &str) {
        if self.thumbs.contains_key(path) || self.thumb_pending.contains(path) {
            return;
        }
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        self.thumb_pending.insert(path.to_string());
        let path = path.to_string();
        self.spawn(move || {
            let img = a.thumb(&vault, &path, 320).and_then(|d| decode_color(&d));
            Event::Thumb { path, img }
        });
    }

    pub fn open_preview(&mut self, path: &str) {
        let Some(a) = self.api_or_note() else { return };
        let Some(vault) = self.vault.clone() else { return };
        let path = path.to_string();
        self.spawn(move || {
            let img = a.thumb(&vault, &path, 1024).and_then(|d| decode_color(&d));
            Event::Preview { path, img }
        });
    }

    pub fn close_preview(&mut self) {
        self.preview = None;
    }
}

// ------------------------------------------------------------- helpers

/// "a/b" + "name" → "a/b/name"; root + "name" → "name".
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn validate_signup(user: &str, pass: &str, vault: &str) -> Result<(), String> {
    let user = user.trim();
    if user.is_empty() || user.len() > 40 {
        return Err("username must be 1–40 characters".into());
    }
    if pass.len() < 6 {
        return Err("password must be at least 6 characters".into());
    }
    if vault.contains('/') {
        return Err("vault name cannot contain '/'".into());
    }
    Ok(())
}

/// Where downloads land: QD_DOWNLOAD_DIR (tests), else ~/Downloads, else ~.
fn download_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("QD_DOWNLOAD_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let downloads = std::path::PathBuf::from(&home).join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return std::path::PathBuf::from(home);
    }
    std::path::PathBuf::from(".")
}

/// Native open-file dialog via whatever helper the desktop provides.
fn pick_file() -> Option<String> {
    let zenity = std::process::Command::new("zenity")
        .args(["--file-selection", "--title=Upload to Quarkdrive"])
        .output()
        .ok()?;
    if zenity.status.success() {
        let s = String::from_utf8_lossy(&zenity.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    let kdialog = std::process::Command::new("kdialog")
        .args(["--getopenfilename", ".", "--title", "Upload to Quarkdrive"])
        .output()
        .ok()?;
    if kdialog.status.success() {
        let s = String::from_utf8_lossy(&kdialog.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Any image the server hands us becomes an egui texture payload.
fn decode_color(img: &image::DynamicImage) -> Result<egui::ColorImage, String> {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}
