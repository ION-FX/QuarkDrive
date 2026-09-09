# Quarkdrive

Self-hosted file and photo sync for Linux, the web, and Android.

One vault, three kinds of client:

- **Linux** — a daemon that watches a folder with inotify and syncs it continuously
- **Web** — a browser UI for browsing, uploading and viewing photos
- **Android** — automatic camera backup plus browsing

The backend is Rust. The web UI is hand-written HTML, CSS and JavaScript with
no npm, no bundler and no build step — the server serves the files as they are.

## Downloads

Ready-to-run artifacts are on the
[releases page](https://github.com/ION-FX/QuarkDrive/releases/latest):

- `quarkdrive-0.1.0-debug.apk` — Android app (arm64-v8a + x86_64, debug-signed)
- `quarkdrive-0.1.0-linux-x86_64.tar.gz` — server + `qd` client + web UI for Linux x86_64
- `SHA256SUMS.txt` — checksums for the above

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

For an end-to-end run against a real server, see `scripts/e2e.sh`.

## Notes on this build

The toolchain here is Rust 1.75, which predates Cargo's MSRV-aware resolver,
so dependencies are pinned in `Cargo.lock` to versions that build on it — most
notably `blake3` 1.5, `ureq` 2.9, `clap` 4.5, `image` 0.24, and `axum` 0.7.
If you are on a newer toolchain you can `cargo update` freely; `image` is
deliberately built without default features because the EXR decoder drags in a
rayon that needs Rust 1.80.

## Status

Built and verified: the sync engine, the server, the Linux client, the web
UI's data path, the Android app, and the PyQt6 desktop GUI. Two Linux clients
were exercised end-to-end against a live server — upload, pull, deletion
propagation, conflict handling, idle no-ops, and web-API interop. The Android
APK builds, installs, and was driven against a live server in an emulator:
sign-in, file listing, a real upload whose bytes arrived intact, and the JNI
bridge (the Rust core's BLAKE3 hash matches the value the desktop core
computes, byte for byte). The desktop GUI runs its whole flow offscreen
against a live server with screenshots — sign-in (right and wrong password),
listing, upload, byte-identical download, rename, delete, and the photo
timeline with a rendered thumbnail.

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

