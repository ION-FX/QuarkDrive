//! Photo features: thumbnails, EXIF dates, and the media index.
//!
//! Immich-style browsing needs two things a plain file listing cannot give:
//! a small preview of every photo, and the date it was *taken* rather than
//! the date it was uploaded. Both are derived here.
//!
//! Work is done lazily and cached two ways: the media index records what has
//! been computed for a given file node, and thumbnails are cached on disk
//! under the node id, so they are invalidated automatically when content
//! changes and are shared between every client that asks.

use anyhow::Result;
use quarkdrive_core::hash::ObjectId;
use rusqlite::{params, Connection};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::vault::Vault;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS media (
    path     TEXT PRIMARY KEY,
    node_id  TEXT NOT NULL,
    width    INTEGER NOT NULL,
    height   INTEGER NOT NULL,
    taken_at INTEGER,
    size     INTEGER NOT NULL,
    updated  INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone)]
pub struct MediaRow {
    pub path: String,
    pub node_id: String,
    pub width: u32,
    pub height: u32,
    /// When the photo was taken, from EXIF; falls back to the file mtime.
    pub taken_at: Option<i64>,
    pub size: u64,
    pub updated: i64,
}

pub struct MediaIndex {
    conn: Mutex<Connection>,
}

impl MediaIndex {
    pub fn open(vault_dir: &Path) -> Result<Self> {
        let conn = Connection::open(vault_dir.join("index.db"))?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        Ok(MediaIndex {
            conn: Mutex::new(conn),
        })
    }

    /// Cached metadata for a path, but only if it still describes the same
    /// file node.
    pub fn get(&self, path: &str, node_id: &ObjectId) -> Result<Option<MediaRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT path, node_id, width, height, taken_at, size, updated
             FROM media WHERE path = ?1",
            params![path],
            |r| {
                Ok(MediaRow {
                    path: r.get(0)?,
                    node_id: r.get(1)?,
                    width: r.get::<_, i64>(2)? as u32,
                    height: r.get::<_, i64>(3)? as u32,
                    taken_at: r.get::<_, Option<i64>>(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    updated: r.get(6)?,
                })
            },
        );
        match row {
            Ok(m) if m.node_id == node_id.to_hex() => Ok(Some(m)),
            Ok(_) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn upsert(&self, m: &MediaRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO media (path, node_id, width, height, taken_at, size, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                 node_id = excluded.node_id,
                 width = excluded.width,
                 height = excluded.height,
                 taken_at = excluded.taken_at,
                 size = excluded.size,
                 updated = excluded.updated",
            params![
                m.path,
                m.node_id,
                m.width as i64,
                m.height as i64,
                m.taken_at,
                m.size as i64,
                m.updated
            ],
        )?;
        Ok(())
    }
}

/// Extensions we treat as photos.
pub fn is_image(path: &str) -> bool {
    matches!(
        extension_of(path).to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "tif"
    )
}

pub fn extension_of(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((_, ext)) if !path.ends_with('.') => ext.to_string(),
        _ => String::new(),
    }
}

pub fn mime_for(path: &str) -> &'static str {
    match extension_of(path).to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Pixel dimensions without decoding the whole image.
pub fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // Read only the header rather than decoding the whole image: the timeline
    // indexes thousands of photos and only needs their size.
    image::io::Reader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// When the photo was taken, according to its EXIF.
///
/// Falls back to nothing: callers substitute the file mtime, which is the best
/// available answer for scans and screenshots that carry no EXIF at all.
pub fn exif_taken_at(data: &[u8]) -> Option<i64> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(data))
        .ok()?;
    let field = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;
    parse_exif_datetime(&field.display_value().to_string())
}

/// Parse EXIF's `YYYY:MM:DD HH:MM:SS` into a Unix timestamp.
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since the Unix epoch for a proleptic Gregorian date.
/// Howard Hinnant's civil-date algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn thumb_path(vault: &Vault, node_id: &ObjectId, size: u32) -> PathBuf {
    vault
        .dir
        .join("thumbs")
        .join(format!("{}-{}.jpg", node_id.to_hex(), size))
}

/// A cached JPEG preview, generated on first request.
///
/// `size` is the bounding box: the thumbnail fits inside it and keeps the
/// original aspect ratio.
pub fn thumbnail(vault: &Vault, node_id: &ObjectId, data: &[u8], size: u32) -> Result<Vec<u8>> {
    let path = thumb_path(vault, node_id, size);
    if let Ok(cached) = std::fs::read(&path) {
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let jpeg = make_thumbnail(data, size)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &jpeg)?;
    Ok(jpeg)
}

pub fn make_thumbnail(data: &[u8], size: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(data)?;
    let thumb = img.thumbnail(size, size);
    let mut out = Vec::new();
    thumb.write_to(
        &mut Cursor::new(&mut out),
        image::ImageOutputFormat::Jpeg(82),
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_detection() {
        assert_eq!(extension_of("a/b/photo.JPG"), "JPG");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of("trailing."), "");
    }

    #[test]
    fn image_detection_is_case_insensitive() {
        assert!(is_image("a/b/photo.JPG"));
        assert!(is_image("x.jpeg"));
        assert!(is_image("x.png"));
        assert!(is_image("x.webp"));
        assert!(!is_image("notes.txt"));
        assert!(!is_image("movie.mp4"));
    }

    #[test]
    fn mime_mapping_has_a_sane_default() {
        assert_eq!(mime_for("a.jpg"), "image/jpeg");
        assert_eq!(mime_for("a.PNG"), "image/png");
        assert_eq!(mime_for("mystery.qqq"), "application/octet-stream");
    }

    #[test]
    fn exif_datetime_parsing() {
        assert_eq!(
            parse_exif_datetime("2024:03:05 14:22:31"),
            Some(1_709_648_551)
        );
        assert_eq!(parse_exif_datetime("1970:01:01 00:00:00"), Some(0));
        assert_eq!(parse_exif_datetime("garbage"), None);
        assert_eq!(parse_exif_datetime("2024:13:05 00:00:00"), None);
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 3, 5), 19_787);
        // Leap years and pre-epoch dates.
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    /// Round-trip a real PNG through the thumbnailer.
    #[test]
    fn generates_a_thumbnail() {
        use image::{ImageBuffer, Rgb};
        let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(600, 400);
        for (x, _y, pixel) in img.enumerate_pixels_mut() {
            *pixel = Rgb([x as u8, 80, 160]);
        }
        let mut raw = Vec::new();
        img.write_to(
            &mut Cursor::new(&mut raw),
            image::ImageOutputFormat::Png,
        )
        .unwrap();

        let thumb = make_thumbnail(&raw, 200).unwrap();
        assert!(!thumb.is_empty());
        let (w, h) = dimensions(&thumb).unwrap();
        // 600x400 inside a 200x200 box, aspect ratio preserved.
        assert_eq!((w, h), (200, 133), "thumbnail must preserve aspect ratio");
        assert_eq!(dimensions(&raw).unwrap(), (600, 400));
    }

    #[test]
    fn non_image_data_is_rejected_not_panicking() {
        assert!(make_thumbnail(b"this is not an image", 100).is_err());
        assert_eq!(dimensions(b"nope"), None);
        assert_eq!(exif_taken_at(b"nope"), None);
    }
}
