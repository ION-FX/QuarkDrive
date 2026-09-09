# Quarkdrive desktop GUI (Linux, PyQt6)

A native desktop window over a Quarkdrive vault: browse files, upload and
download, rename or move, make folders and vaults, and browse the photo
timeline with server-generated thumbnails — the same feature set as the web
UI, with drag-and-drop and your desktop's file dialogs.

It talks to the same file API as the web UI and the Android app, so nothing
here needs the Rust core locally; the server does the chunking, hashing and
storage. Background sync stays with `qd watch` — this window is the
human-facing half.

## Running

```sh
pip install --user --break-system-packages PyQt6   # Ubuntu 24.04
python3 desktop/qd-gui.py
```

Sign in with the same server / username / password as the web UI. The session
is kept in `~/.config/quarkdrive/gui.json` (mode 600), so next launch goes
straight to your files.

## What it does

- **Files** — a sortable list with sizes and modification times. Double-click
  a folder to enter, a file to save it locally. Right-click for download,
  rename/move (type a path to move into a folder), and delete. Upload with
  the button or by dropping files onto the window. The filter box narrows
  the current view as you type.
- **Photos** — the timeline as a thumbnail grid, newest first, with the
  server-rendered thumbnails fetched with your token attached. Double-click
  for a full-size preview.
- **Vaults** — switch vaults from the combo box, create new ones with the
  button.

## Testing without a display

The app runs fully under Qt's offscreen platform, and `test-gui.py` drives it
against a live server — sign in (right and wrong password), list, upload,
byte-identical download, rename, delete, photo timeline with a rendered
thumbnail — capturing screenshots along the way:

```sh
    QT_QPA_PLATFORM=offscreen python3 desktop/test-gui.py \
        --server http://localhost:8787 --user demo --password 'change-me-now'
```

Screenshots land in `/tmp/qd-gui-shots/`. Modal error dialogs are recorded
and printed rather than blocking, and the test fails if one appears when
nothing should have gone wrong.
