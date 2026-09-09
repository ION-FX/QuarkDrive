//! End-to-end exercise of the GUI against a live Quarkdrive server.
//!
//! Requires a running server and account, provided by scripts/test-gui.sh:
//!
//!     QD_TEST_SERVER=http://127.0.0.1:8931
//!     QD_TEST_USER=gui-test
//!     QD_TEST_PASS=gui-test-pass
//!
//! Everything a user can do — first-run detection, failed and successful
//! sign-in, upload, folders, download round-trip, rename, search, the photo
//! grid with live thumbnails, full-size preview, stats, delete — is driven
//! through the same methods the buttons call, with real egui frames drawn
//! between steps. Screenshots land in $QD_SHOTS (default /tmp/qd-gui-shots).

mod common;

use common::{pump, Renderer};
use quarkdrive_gui::app::{App, Listing, Tab};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static STEP: AtomicUsize = AtomicUsize::new(1);

fn step(msg: &str) {
    let n = STEP.fetch_add(1, Ordering::SeqCst);
    println!("  {:2}. {msg}", n - 1);
}

fn shots_dir() -> PathBuf {
    PathBuf::from(std::env::var("QD_SHOTS").unwrap_or_else(|_| "/tmp/qd-gui-shots".into()))
}

/// A small deterministic PNG so the photo pipeline has something to chew on.
fn make_test_png(rgb: [u8; 3]) -> Vec<u8> {
    let mut img = image::RgbImage::new(96, 64);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([
            rgb[0].saturating_add((x % 16) as u8),
            rgb[1].saturating_add((y % 16) as u8),
            rgb[2],
        ]);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png");
    buf.into_inner()
}

#[test]
fn full_gui_flow_against_live_server() {
    let server = match std::env::var("QD_TEST_SERVER") {
        Ok(s) => s,
        Err(_) => {
            eprintln!("QD_TEST_SERVER not set — skipping (see scripts/test-gui.sh)");
            return;
        }
    };
    let user = std::env::var("QD_TEST_USER").unwrap_or_else(|_| "gui-test".into());
    let pass = std::env::var("QD_TEST_PASS").unwrap_or_else(|_| "gui-test-pass".into());
    let shots = shots_dir();
    let work = std::env::temp_dir().join("qd-gui-test");
    let _ = std::fs::create_dir_all(&work);

    // A fresh egui context per test; frames are drawn without any display.
    let ctx = egui::Context::default();
    let mut app = App::new();
    let mut shot = Renderer::new();
    app.server = server.clone();
    app.username = user.clone();

    // ------------------------------------------------ first-run detection
    pump(&mut app, &ctx, &mut shot, 30.0, "initial status");
    assert_eq!(
        app.signup_hint,
        Some(false),
        "server should already have an account (registered by the test script)"
    );
    step("server status checked: sign-up not offered");

    // --------------------------------------------------- failed sign-in
    app.password = "definitely-wrong".into();
    app.sign_in();
    pump(&mut app, &ctx, &mut shot, 30.0, "failed sign-in");
    assert!(app.api.is_none(), "must not be signed in with a bad password");
    let err = app
        .login_err
        .clone()
        .unwrap_or_else(|| "no error surfaced".into());
    assert!(err.contains("invalid"), "error should say why: {err}");
    shot
        .screenshot(&mut app, &ctx, &shots.join("01-login-error.png"))
        .unwrap();
    step("wrong password rejected with a visible message");

    // ------------------------------------------------- successful sign-in
    app.password = pass.clone();
    app.sign_in();
    pump(&mut app, &ctx, &mut shot, 30.0, "sign-in");
    assert!(app.api.is_some(), "expected to be signed in");
    assert_eq!(app.vault.as_deref(), Some(user.as_str()), "vault auto-picked");
    assert!(app.stats.is_some(), "stats should arrive with the first listing");
    shot
        .screenshot(&mut app, &ctx, &shots.join("02-files-root.png"))
        .unwrap();
    step("signed in, vault auto-picked, stats loaded");

    // ------------------------------------------------------------- upload
    let hello = work.join("hello.txt");
    std::fs::write(&hello, b"hello from the quarkdrive gui test\n").unwrap();
    app.upload_buf = hello.display().to_string();
    app.upload_manual();
    pump(&mut app, &ctx, &mut shot, 60.0, "upload hello.txt");
    assert!(
        app.entries.iter().any(|e| e.name == "hello.txt"),
        "hello.txt should be listed after upload"
    );
    step("uploaded hello.txt via the path field");

    // ------------------------------------------------------------ folders
    app.mkdir_open = true;
    app.mkdir_buf = "Docs".into();
    app.do_mkdir();
    pump(&mut app, &ctx, &mut shot, 30.0, "mkdir Docs");
    assert!(app.entries.iter().any(|e| e.name == "Docs" && e.is_dir()));

    app.open_dir("Docs");
    pump(&mut app, &ctx, &mut shot, 30.0, "open Docs");
    assert_eq!(app.cwd, "Docs");

    let notes = work.join("notes.txt");
    std::fs::write(&notes, b"meeting notes\n- egui works headless\n").unwrap();
    app.upload_buf = notes.display().to_string();
    app.upload_manual();
    pump(&mut app, &ctx, &mut shot, 60.0, "upload Docs/notes.txt");
    assert!(app.entries.iter().any(|e| e.name == "notes.txt"));
    step("created Docs/, uploaded notes.txt into it, navigated back");

    app.up();
    pump(&mut app, &ctx, &mut shot, 30.0, "navigate up");
    assert_eq!(app.cwd, "", "Up returns to the vault root");

    // ---------------------------------------------------------- download
    let download_dir = std::env::var("QD_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("qd-gui-downloads"));
    let _ = std::fs::create_dir_all(&download_dir);
    let saved = download_dir.join("hello.txt");
    let _ = std::fs::remove_file(&saved);
    app.select("hello.txt");
    app.download_selected();
    pump(&mut app, &ctx, &mut shot, 60.0, "download hello.txt");
    let bytes = std::fs::read(&saved).unwrap_or_else(|e| {
        panic!(
            "downloaded file should exist at {}: {e} (app note: {:?})",
            saved.display(),
            app.note
        )
    });
    assert_eq!(
        bytes,
        b"hello from the quarkdrive gui test\n".as_slice(),
        "downloaded bytes must round-trip exactly"
    );
    step("downloaded hello.txt, bytes identical");

    // ------------------------------------------------------------- rename
    app.start_rename("hello.txt");
    app.rename = Some(("hello.txt".into(), "hello-renamed.txt".into()));
    app.commit_rename();
    pump(&mut app, &ctx, &mut shot, 30.0, "rename hello.txt");
    assert!(app.entries.iter().any(|e| e.name == "hello-renamed.txt"));
    assert!(!app.entries.iter().any(|e| e.name == "hello.txt"));
    step("renamed hello.txt → hello-renamed.txt");

    // ------------------------------------------------------------- search
    app.search_buf = "notes".into();
    app.do_search();
    pump(&mut app, &ctx, &mut shot, 30.0, "search");
    assert_eq!(app.listing, Listing::Search("notes".into()));
    assert!(
        app.entries.iter().any(|e| e.name == "notes.txt"),
        "search should find Docs/notes.txt"
    );
    shot
        .screenshot(&mut app, &ctx, &shots.join("03-search.png"))
        .unwrap();
    app.close_search();
    pump(&mut app, &ctx, &mut shot, 30.0, "close search");
    assert_eq!(app.listing, Listing::Dir);
    step("search found Docs/notes.txt, returned to the file list");

    // ------------------------------------------------------------- photos
    let photo = make_test_png([180, 40, 60]);
    app.upload_bytes("photo.png", photo);
    pump(&mut app, &ctx, &mut shot, 60.0, "upload photo.png");

    app.tab = Tab::Photos;
    app.open_photos();
    // Frames keep drawing while thumbnails stream in; wait for the first.
    let mut seen_thumb = false;
    for _ in 0..300 {
        pump(&mut app, &ctx, &mut shot, 10.0, "photo thumbnail");
        if !app.thumbs.is_empty() {
            seen_thumb = true;
            break;
        }
    }
    assert!(seen_thumb, "at least one thumbnail should decode");
    assert!(matches!(app.photos, Some(Ok(ref items)) if items.len() >= 1));
    shot
        .screenshot(&mut app, &ctx, &shots.join("04-photos.png"))
        .unwrap();
    step("photo grid live with a decoded thumbnail");

    let photo_path = match app.photos.as_ref() {
        Some(Ok(items)) => items[0].path.clone(),
        other => panic!("timeline should list the photo, got {other:?}"),
    };
    app.open_preview(&photo_path);
    pump(&mut app, &ctx, &mut shot, 60.0, "photo preview");
    assert!(app.preview.is_some(), "full-size preview should be loaded");
    shot
        .screenshot(&mut app, &ctx, &shots.join("05-preview.png"))
        .unwrap();
    app.close_preview();
    step("full-size preview opened and closed");

    // -------------------------------------------------------------- stats
    let stats = app.stats.clone().expect("stats loaded earlier");
    assert!(stats.files >= 3, "expect ≥3 files by now, got {}", stats.files);
    assert!(stats.bytes > 0);
    step(format!("stats: {} files, {} bytes", stats.files, stats.bytes).as_str());

    // ------------------------------------------------------------- delete
    app.open_dir("Docs");
    pump(&mut app, &ctx, &mut shot, 30.0, "open Docs for delete");
    app.select("Docs/notes.txt");
    app.delete_arm = Some("Docs/notes.txt".into());
    shot
        .screenshot(&mut app, &ctx, &shots.join("06-confirm-delete.png"))
        .unwrap();
    app.confirm_delete();
    pump(&mut app, &ctx, &mut shot, 30.0, "delete notes.txt");
    assert!(!app.entries.iter().any(|e| e.name == "notes.txt"));
    step("two-step delete removed Docs/notes.txt");

    // ------------------------------------------------------------- logout
    app.logout();
    pump(&mut app, &ctx, &mut shot, 5.0, "logout");
    assert!(app.api.is_none());
    assert!(app.entries.is_empty());
    step("signed out cleanly");

    println!("all GUI flow checks passed; screenshots in {}", shots.display());
}
