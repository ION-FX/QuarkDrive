# Quarkdrive

Self-hosted file and photo sync for Linux, the web, and Android.

One vault, three kinds of client:

- **Linux** — a daemon that watches a folder with inotify and syncs it continuously, a native desktop app for browsing files and photos, and the browser UI
- **Web** — a browser UI for browsing, uploading and viewing photos
- **Android** — automatic camera backup plus browsing

And the things that make it a place rather than a folder: **share a vault**
with another account on the server (view or edit), a **trash** that keeps
deleted files until they are purged, optional native **HTTPS**, and a login
**rate limiter** against password guessing.

The backend is Rust. The desktop app is native Rust too (egui) — one static
binary, no Python, no Electron, no npm. The web UI is hand-written HTML, CSS
and JavaScript with no npm, no bundler and no build step — the server serves
the files as they are.

## Downloads

Ready-to-run artifacts are on the
[releases page](https://github.com/ION-FX/QuarkDrive/releases/latest):

- `quarkdrive-0.1.1-linux-x86_64.tar.gz` — server + `qd` sync client + `quarkdrive-gui` desktop app + web UI, for Linux x86_64
- `quarkdrive-0.1.0-debug.apk` — Android app (arm64-v8a + x86_64, debug-signed; unchanged in 0.1.1)
- `SHA256SUMS.txt` — checksums for the above

Unpack the tarball and run `./quarkdrive-gui` for the desktop app: it asks
for a server, username and password (and offers first-run sign-up on an
empty server, like the web UI).

## Quick start

```sh
./scripts/setup.sh --start
```

That builds everything, asks for a username, password and vault name, creates
them, and starts the server. Open the address it prints and sign in.

Re-running it never overwrites data. It prints the commands to run again
later, which are the same four by hand if you would rather:

```sh
cargo build --release
./target/release/quarkdrive-server create-user  --data ./quarkdrive-data --username ada --password '…'
./target/release/quarkdrive-server create-vault --data ./quarkdrive-data --username ada --name photos
./target/release/quarkdrive-server create-token --data ./quarkdrive-data --username ada
./target/release/quarkdrive-server serve        --data ./quarkdrive-data --web ./web --listen 0.0.0.0:8787
```

Then open <http://localhost:8787>. `scripts/setup.sh --help` lists every
option, including `--data`, `--port`, `--host` and `--no-prompt` style flags.

**No account yet? The web UI offers a sign-up.** Start the server against an
empty data directory and the login card becomes a first-run form: pick a
username, a password and a name for your first vault, and the account is
created — Gitea/Nextcloud-style. Registration closes permanently the moment
an account exists; from then on new accounts come from `create-user`, so a
LAN server is never left open to anyone who finds the page.

**Sync a folder from this machine** (the token is written to
`quarkdrive-data/<user>.token` by setup):

```sh
./target/release/qd init --server http://localhost:8787 --vault photos \
    --token "$(cat quarkdrive-data/ada.token)" --device "$(hostname)" --dir ~/Quarkdrive
./target/release/qd watch --dir ~/Quarkdrive      # keep syncing
./target/release/qd status --dir ~/Quarkdrive     # what is pending
```

## Setup in detail

`scripts/setup.sh` does five things, and only the first is optional:

1. **Builds** the server and client into `target/release`, if they are not
   already there.
2. **Creates an administrator** — Argon2id-hashed password, asked for twice
   and not echoed.
3. **Creates the first vault.** A vault is one synced space; create more later
   with `create-vault`.
4. **Issues an access token** and writes it to
   `quarkdrive-data/<user>.token` with mode 600. Tokens are what clients
   authenticate with; revoke one with `quarkdrive-server revoke-token`.
5. **Starts the server** if you passed `--start`, detached, logging to
   `quarkdrive-data/server.log`.

Data — objects, the index, users — lives entirely in `quarkdrive-data`.
Delete that directory and you have a fresh install.

Useful flags:

| Flag | Meaning |
|------|---------|
| `--data DIR` | Where vaults and the user database live |
| `--host ADDR` | Bind address; `127.0.0.1` keeps it local |
| `--port N` | Listen port (default 8787) |
| `--user` / `--password` / `--vault` | Skip the matching prompt |
| `--yes` | Never prompt; requires `--password` |
| `--start` | Start the server when setup finishes |
| `--profile debug\|release` | Which build to run |

## Sharing and the trash

A vault owner can invite other accounts on the same server from the web UI
(the 👝 button in the top bar): *can view* or *can edit*. A shared vault
appears in the invitee's own vault list, works in every client that speaks
the file API, and revoking takes effect immediately. Shares are refused for
end-to-end encrypted vaults — the recipient would have no key.

Deleted files are not destroyed. The web UI (and the API) moves them to a
per-vault trash: restore puts a file back at its old path — or next to it as
`name.restored-<time>` if that path is taken again — and purging drops the
pointer. Because storage is content-addressed, a restore never copies
anything. One honest limit: purging is not a secure erase; the chunks stay
on disk until object-level garbage collection exists.

## Public links and version history

The share drawer's *Public links* section publishes the folder you are
browsing at `/s/<id>`: visitors get a read-only page — list, browse,
download — with no account. A link can require a password (sent in a
header on every request, never in a URL) and can expire. Removing the
link cuts off everyone instantly, and a visitor cannot read a single byte
outside the shared folder.

Every overwrite a file has ever had is a version. The *Versions* button on
a file walks the snapshot history and lists each distinct content with its
date and size: download any of them, or restore one to bring those exact
bytes back. Restores re-use the stored chunks, so they cost nothing.

## Running it as a service

```sh
docker compose up -d --build        # or:
cargo build --release
sudo cp target/release/quarkdrive-server /usr/local/bin/
sudo cp -r web /usr/local/share/quarkdrive/web
sudo cp deploy/quarkdrive.service /etc/systemd/system/
sudo systemctl enable --now quarkdrive
```

For HTTPS, either point a reverse proxy at port 8787 or give the server
certificates directly:

```sh
quarkdrive-server serve --data /var/lib/quarkdrive --listen 0.0.0.0:443 \
    --tls-cert /etc/letsencrypt/live/example.com/fullchain.pem \
    --tls-key  /etc/letsencrypt/live/example.com/privkey.pem
```

Repeated failed sign-ins (five inside ten minutes, per source address and
username) are refused for the rest of the window, so an internet-facing
server is not wide open to password guessing.

## Why it is fast

Three ideas, in the order they matter.

**Content-defined chunking.** Files are split at boundaries chosen from the
data itself, using a gear hash. Insert one byte at the front of a 2 GB video
and only the chunks near the edit change; a fixed-block sync would see every
block after the edit as new and re-upload the whole file. Chunks average about
64 KiB.

**A Merkle tree, not a file list.** Every directory is an object listing the
content addresses of its children, so the tree hashes bottom-up. Two devices
comparing trees stop descending as soon as two subtrees match — finding the one
changed file in a folder of ten thousand costs a handful of small fetches.

**Content addressing.** Every chunk, directory and snapshot is stored under
its own BLAKE3 hash. Identical bytes are stored once no matter how many files
or devices contain them, and because an object's name *is* its hash, every
read is verified: a broken or hostile server cannot hand back data you did not
write.

## How conflicts are resolved

Each sync is a three-way merge between the tree you last agreed on (the base),
your tree, and the server's tree:

| You, vs base | Server, vs base | Result                                   |
|--------------|-----------------|------------------------------------------|
| unchanged    | changed         | take the server's version                |
| changed      | unchanged       | keep yours                               |
| changed      | changed         | newest mtime wins; the loser is kept     |
| deleted      | changed         | the edit wins and the file is restored   |
| changed      | deleted         | yours wins                               |

A losing edit is never discarded — it is written next to the winner as
`<name>.conflict-<device>-<time>` and syncs to your other devices like any
other file.

Commits use optimistic concurrency: a commit names the head it was computed
from and the server rejects it if the head has moved. The client re-merges and
retries, so two devices syncing at the same time converge.

## End-to-end encryption

A vault can be marked end-to-end encrypted. The server then only ever holds
ciphertext and cannot list, preview or serve the contents; only the object
protocol works, so the web UI and phone client lose the file API.

Encryption is XChaCha20-Poly1305 with keys derived by Argon2id. Keys and
nonces are derived deterministically from each object's own content address
(*convergent encryption*), which is what keeps deduplication working across
devices while the server is blind.

**The tradeoff is real and deliberate:** deterministic encryption means someone
who can guess a file's contents can confirm whether that file is in the vault.
It does not let them decrypt anything they do not already have. Deduplication
and server-blindness cannot both be had without accepting this.

```sh
qd init --encrypt --server … --vault secret --token <token> --dir ~/Secret
```

Back up `.quarkdrive/key` separately. Losing it loses the vault.

## Themes

**Galaxy** — deep violet with a slow starfield — is the default. Six others
ship with it, dark and light: Nebula, Midnight, Aurora, Ember, Paper and Linen.

Pick one from 🎨 in the top bar. Make your own from the same place: start from
any theme, change the ten colours that everything is drawn from, and point it
at a background image by URL or upload. Changes preview as you go, themes are
saved in your browser, and **Export…**/**Import…** moves them between machines
as a small JSON file.

When a background image is set, panels turn translucent and blur whatever is
behind them, so the picture reads as a backdrop rather than a wallpaper.

Because a theme is nothing but CSS variables, one can also be written by hand
— see `web/THEMES.md` for the format and the rest of the detail.

## Layout

```
crates/
  quarkdrive-core    chunking, Merkle trees, crypto, merge, sync client
  quarkdrive-server  Axum server: object API, file API, thumbnails, static UI
  quarkdrive-cli     Linux client: qd init/sync/watch/status
  quarkdrive-gui     native desktop app (egui): files, photos, uploads
  quarkdrive-ffi     JNI bindings so Android reuses the Rust core
web/                 the browser UI (plain HTML/CSS/JS, no build step)
  themes.js          the built-in palettes and the code that applies them
  test-themes.js     tests for the above, run with `node`
  test-app.js        executes app.js in a stub DOM: sign-in, sign-up flows
desktop/             PyQt6 desktop GUI: files, photos, drag-and-drop upload
  qd-gui.py          the app
  test-gui.py        offscreen integration test against a live server
android/             Kotlin client: browsing, photo backup over WorkManager
scripts/             setup.sh, e2e.sh
```

## Tests

```sh
cargo test
```

143 unit tests cover the chunker (including the boundary-shift property that
justifies content-defined chunking), the crypto, the object store's tamper
detection, the merge rules, and full two-device sync scenarios.

The web UI has dependency-free tests that run directly with node:
`test-themes.js` covers the theming logic and the login/sign-up wiring,
and `test-app.js` executes the real `app.js` inside a stub DOM — boot,
sign-in, first-run sign-up, validation, and the stale-page case where a
cached older markup meets a newer script:

```sh
node web/test-themes.js
node web/test-app.js
```

For an end-to-end run against a real server, see `scripts/e2e.sh`. The
native GUI has the same kind of harness: `scripts/test-gui.sh` starts a
throwaway server, drives the real app through egui frames with no display
involved (sign-in, wrong password, upload, folders, byte-identical download,
rename, search, photo grid with live thumbnails, preview, delete), and
software-rasterises the frames into screenshots you can eyeball afterwards.

## Notes on this build

Everything builds on current stable Rust (1.98 as of this writing). The
toolchain was 1.75 until the native GUI arrived; 2026-era dependencies want
edition-2024 crates, which 1.75's cargo cannot even parse, so the toolchain
moved rather than pinning dozens of transitive versions. Android
cross-compilation still works — the NDK targets are installed for the new
toolchain and `android/build-rust.sh` is unchanged.

## Status

Built and verified: the sync engine, the server, the Linux client, the web
UI's data path, the Android app, both desktop GUIs, vault sharing, the
trash, login rate limiting and native HTTPS. Two Linux clients
were exercised end-to-end against a live server — upload, pull, deletion
propagation, conflict handling, idle no-ops, and web-API interop. The Android
APK builds, installs, and was driven against a live server in an emulator:
sign-in, file listing, a real upload whose bytes arrived intact, and the JNI
bridge (the Rust core's BLAKE3 hash matches the value the desktop core
computes, byte for byte). The PyQt6 GUI runs its whole flow offscreen
against a live server with screenshots. The native GUI does the same without
even a window server: the test drives real egui frames headlessly — sign-in
(right and wrong password), first-run detection, upload, folders,
byte-identical download, rename, search, the photo grid with live decoded
thumbnails, full-size preview, stats and delete — and a small software
rasteriser turns the actual frames into screenshots (`/tmp/qd-gui-shots/`).

Two server bugs that only real clients catch were found and fixed along the
way: the `/stats` and `/search` routes had never been registered (the SPA
fallback answered with `index.html`), and the `/timeline` response's `items`
key was misparsed by the newer clients. See `desktop/README.md` for the GUI.

The one thing this VM still cannot do is open the web UI in an actual browser
(no browser backend is available here). Its JavaScript parses, every element
it references exists in the markup, the theming logic has passing tests, and
every endpoint it calls was exercised against a live server. Bugs of exactly
the untestable kind have shipped and been caught: a `hidden` attribute
silently ignored because of a competing `display` rule (login screen
impossible to leave), and a login-card flash on reload — both are now
prevented before first paint and checked by tests.

