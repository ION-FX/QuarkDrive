//! # Quarkdrive core
//!
//! The sync engine shared by every Quarkdrive client: the Linux daemon, the
//! Android app (via the C ABI in `quarkdrive-ffi`), and the server, which uses
//! it to store and reassemble objects.
//!
//! ## How sync works
//!
//! 1. **Scan** the local directory tree, splitting each file into
//!    content-defined chunks ([`chunker`]). Chunks are stored in a
//!    content-addressed store ([`object`]), so identical data is kept once no
//!    matter how many files or devices contain it.
//! 2. **Model** the result as a Merkle tree ([`tree`]) — each directory is an
//!    object listing the ids of its children.
//! 3. **Merge** the local tree against the server's tree using the last agreed
//!    tree as the common ancestor ([`merge`]), a three-way merge that keeps
//!    genuinely independent edits on different devices.
//! 4. **Transfer** only the objects the peer is missing ([`sync`]).
//!
//! A single byte inserted in the middle of a large file invalidates one chunk
//! and the directory nodes above it — not the whole file, and not the whole
//! vault.

pub mod chunker;
pub mod crypto;
pub mod hash;
pub mod index;
pub mod merge;
pub mod object;
pub mod scan;
pub mod sync;
pub mod transport;
pub mod tree;

mod gear;

#[cfg(any(test, feature = "testing"))]
pub mod testutil;

pub use hash::ObjectId;
pub use object::ObjectStore;
pub use tree::{Node, Snapshot, TreeSet};
