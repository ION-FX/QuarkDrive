# Quarkdrive design

How the pieces fit, and why they are shaped that way.

## The one idea

Everything is stored under its own BLAKE3 hash.

That single choice produces most of what the system does for free:

- **Deduplication.** Identical bytes hash identically, so they are stored once
  no matter how many files or devices contain them.
- **Integrity.** An object's name is the hash of its contents, so reading it
  and re-hashing detects corruption or tampering. The server cannot return
  data a client did not write.
- **Cheap comparison.** Two devices deciding what differs can stop descending
  a tree the moment two subtrees hash equal.

## Chunking

Fixed-size blocks would make sync wasteful: insert one byte at the front of a
large file and every subsequent block shifts, so the whole file looks new.

Quarkdrive picks cut points from the data. A gear hash — a rolling hash built
from a 256-entry table of 64-bit constants, one per byte value — is updated
once per byte, and a cut is taken when its low 16 bits are zero:

```rust
self.hash = (self.hash << 1).wrapping_add(GEAR[b]);
if self.count >= MIN_CHUNK && (self.hash & 0xFFFF) == 0 { /* cut */ }
```

Cut points therefore follow the content rather than the offset, so an edit
perturbs only the chunks it touches.

Two details are worth knowing. The table is generated deterministically once
(from SplitMix64) and embedded, so every build chunks identically. And the
observed mean chunk size is *not* 64 KiB: cut points are geometric with mean
`AVG_CHUNK`, but cuts offered before the `MIN_CHUNK` floor are skipped, and
being memoryless the wait restarts from there — so chunks average roughly
`MIN_CHUNK + AVG_CHUNK`. That is a property of the algorithm, not a bug, and
the test asserts the model rather than a round number.

## The tree

A file is a list of chunk addresses plus its metadata. A directory is a list of
its children's node ids. Both are objects, so the tree hashes bottom-up:

```
root ──► Dir{ "photos": D, "notes.txt": F }
           │                │
           ▼                ▼
        Dir{ "IMG_1.jpg": F }   File{ size, mtime, mode, [chunk…] }
```

Changing one photo rewrites that file's node and the directory nodes above it
— nothing else. Clients walk both trees in lockstep during a merge and skip
any subtree whose id matches.

Directory ids must not depend on insertion order, so entries live in a
`BTreeMap` and serialise in sorted order. If they did not, two devices with
identical files would disagree about whether anything changed.

## Sync

Each sync is a three-way merge of three trees: the tree both sides agreed on
at the last sync (**base**), the tree on disk now (**local**), and the server's
head (**remote**).

Comparing against the base, rather than only against each other, is what tells
"you changed this" apart from "I deleted that". The rules are in the README's
conflict table; the two asymmetric ones are deliberate:

- **An edit beats a deletion.** Losing work is worse than resurrecting a file
  that someone deleted on another device.
- **A loser is never destroyed.** Conflicting edits are resolved by mtime and
  the losing version is written alongside as `<name>.conflict-<device>-<time>`,
  which then syncs normally.

### A bug worth remembering

Adopting a remote directory wholesale (the fast path when your copy is
unchanged) originally only ever *added* children. A deletion one level down was
therefore undone on the next sync: the file came back. The entry-level merge
never saw it, because it had no reason to descend.

The fix is in `materialize_dir`, which prunes local entries the remote
directory does not list — skipping the client's own `.quarkdrive` state and any
conflict copies. The unit tests missed this because they deleted at the root,
where the merge always works entry by entry; the end-to-end suite caught it.
`scripts/e2e.sh` exists because of this bug.

### Concurrency

A commit names the head it was computed from, and the server rejects it if the
head has moved (HTTP 409). The client re-merges against the new head and
retries, up to four times. Two devices syncing simultaneously converge instead
of clobbering each other.

### The cache

Re-chunking every file on every sync would make large vaults unusable, so a
SQLite index records, for each path, the size/mtime/inode signature that was
last synced along with the node it produced. If the signature still matches,
the node is reused without reading the file.

The inode matters: without it, rewriting a file within the same second, at the
same size, would be invisible.

The cache is a pure optimisation. Deleting it costs a full re-chunk, never
correctness, and there is a test that proves sync still converges without it.

## Encryption

Vaults are either plain or end-to-end encrypted. Plain vaults let the server
do useful work — thumbnails, EXIF dates, the file API the web UI and phones
use. Encrypted vaults make the server blind and restrict it to the object
protocol.

ChaCha20-Poly1305 (XChaCha20 variant) with keys from Argon2id. Each object's
key and nonce are derived from the object's own content address, using BLAKE3
in keyed mode with domain separation. That determinism is what makes
encryption *convergent*: identical plaintext produces byte-identical
ciphertext on every device, so the server can deduplicate across them while
being unable to read anything.

The cost is the standard one for convergent encryption: someone who can guess
a file's contents can confirm whether it is present. This is unavoidable if
you want both deduplication and server-blindness, so it is a choice rather than
a default.

When a server hosts an encrypted vault it cannot verify uploads (it cannot
decode them), so it stores them un verified after a header check. Verification
still happens everywhere it matters: on every read by a client holding the key.

## Two APIs, one store

The server exposes two APIs over the same Merkle tree:

- **The object protocol** (`/head`, `/objects`, `/commit`) moves
  content-addressed objects and knows nothing about filenames. Desktop and
  Android clients use it; it transfers only what changed.
- **The file API** (`/fs`, `/thumb`, `/timeline`) works in paths and files.
  The web UI and phones use it because they do not run the sync engine.

A write through the file API re-chunks the file with the same chunker clients
use, rewrites the directory nodes above it, and commits a snapshot. Content
uploaded from a phone is therefore indistinguishable from content pushed by a
laptop, and deduplicates against it — there is a test for exactly that.

## Photos

Thumbnails and EXIF dates are derived lazily and cached: the media index
records what was computed for a given node id, and thumbnails are cached on
disk under that id, so both invalidate automatically when content changes and
are shared between every client that asks. Photos without EXIF (screenshots,
scans) fall back to the file mtime.

## Platform notes

- **Linux** watches with inotify, debounces the burst of events a single save
  produces, and re-syncs on a timer as well — filesystem notification can miss
  events, and sees nothing that happened while the daemon was down.
- **Web** is plain ES2020 served straight from disk. Every endpoint needs an
  Authorization header, which an `<img src>` cannot send, so thumbnails are
  fetched as authenticated blobs and loaded lazily as they scroll into view.
- **Android** reuses the Rust core over JNI for hashing and chunking, and keeps
  networking, scheduling and the backup ledger in Kotlin, close to the platform
  APIs they need. Backup is idempotent because it keys on content addresses
  rather than MediaStore ids.
