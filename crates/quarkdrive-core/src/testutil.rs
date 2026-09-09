//! Shared test helpers. Compiled only for `cargo test`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory that is deleted when it goes out of scope.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "qd-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            nanos,
            n
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Deterministic pseudo-random bytes, so tests that need bulk data do not
/// have to agree on a shared RNG.
pub fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.push((s >> 24) as u8);
    }
    out
}

/// Build a small directory tree on disk for scan/sync tests.
///
/// Keys are relative slash-separated paths; `None` means "make a directory".
pub fn make_tree(base: &Path, entries: &[(&str, Option<&[u8]>)]) {
    for (path, content) in entries {
        let full = base.join(path);
        match content {
            Some(bytes) => {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&full, bytes).unwrap();
            }
            None => {
                fs::create_dir_all(&full).unwrap();
            }
        }
    }
}
