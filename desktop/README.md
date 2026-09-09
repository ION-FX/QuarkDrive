# Quarkdrive desktop apps (Linux)

Two desktop clients, same feature set: browse files, upload and download,
rename or move, make folders and vaults, and browse the photo timeline with
server-generated thumbnails.

Both talk to the same file API as the web UI and the Android app, so neither
needs the Rust core locally; the server does the chunking, hashing and
storage. Background sync stays with `qd watch` — these windows are the
human-facing half.

## quarkdrive-gui — native (recommended)

A single self-contained Rust binary built on egui, shipped in the release
tarball. No Python, no runtime dependencies beyond OpenGL.

```sh
./target/release/quarkdrive-gui
```

- **Sign-in** with the same server / username / password as the web UI; on an
  empty server the card turns into first-run sign-up, exactly like the web.
- **Files** — breadcrumb navigation, sizes and modification times, per-row
  get/rename/delete, a two-step delete confirmation, search across the vault,
  and an upload path field (or `zenity`/`kdialog` picker if installed).
- **Photos** — the timeline as a thumbnail grid; thumbnails stream in as
  cells scroll into view; click for a full-size preview. Stats live in the
  footer.
- Downloads land in `~/Downloads` (override with `QD_DOWNLOAD_DIR`); the
  server, username and last vault are remembered in
  `~/.config/quarkdrive/gui.json` — never the password.

### Testing without a display

`scripts/test-gui.sh` starts a throwaway server, registers an account, and
drives the real app through egui frames headlessly (`Context::run`, no
window server): sign-up detection, wrong-password rejection, upload,
folders, byte-identical download, rename, search, the photo grid with live
decoded thumbnails, preview and delete. A small software rasteriser turns
the actual egui frames into PNGs so the screenshots show the real UI:

```sh
./scripts/test-gui.sh          # screenshots in /tmp/qd-gui-shots/
```

## qd-gui.py — PyQt6 alternative

Same feature set with drag-and-drop and Qt's native dialogs, for machines
where Python is already home.

```sh
pip install --user --break-system-packages PyQt6   # Ubuntu 24.04
python3 desktop/qd-gui.py
```

The session is kept in `~/.config/quarkdrive/gui.json` (mode 600), so next
launch goes straight to your files. `test-gui.py` drives it under Qt's
offscreen platform against a live server and captures screenshots the same
way (`QT_QPA_PLATFORM=offscreen`).
