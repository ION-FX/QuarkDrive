//! Content-defined chunking.
//!
//! Splitting files at fixed offsets is what makes naive sync wasteful: insert
//! one byte at the front of a 2 GB video and every subsequent fixed-size block
//! shifts, so the whole file looks changed and gets re-uploaded.
//!
//! Quarkdrive instead picks cut points from the *content itself*, using a gear
//! hash (a rolling hash over a byte-at-a-time substitution table). The chunker
//! walks the bytes and cuts whenever the rolling hash hits a target pattern.
//! Because boundaries follow the data rather than the offset, an edit only
//! perturbs the chunks it actually touches.

use crate::gear::GEAR;

/// Minimum chunk size. Prevents pathologically small chunks in highly
/// self-similar data (long runs of one byte, for example).
pub const MIN_CHUNK: usize = 8 * 1024;

/// Nominal mean distance between cut points, achieved by cutting whenever the
/// low `AVG_CHUNK_BITS` bits of the rolling hash are zero.
///
/// This is the mean of the underlying geometric distribution, not the mean
/// chunk size you will actually observe, which is higher by roughly
/// `MIN_CHUNK`: a cut offered before the floor is skipped, and because the
/// process is memoryless the wait for the next one effectively starts over
/// from there. Chunks therefore average about `MIN_CHUNK + AVG_CHUNK`.
pub const AVG_CHUNK: usize = 64 * 1024;
const AVG_CHUNK_BITS: u32 = 16; // 2^16 == 64 KiB
const MASK: u64 = (1u64 << AVG_CHUNK_BITS) - 1;

/// Hard ceiling, so a region with no cut point cannot grow unbounded.
pub const MAX_CHUNK: usize = 256 * 1024;

/// Size of the read buffer used when chunking a stream.
const STREAM_BUF: usize = 1024 * 1024;

/// Incremental gear-hash chunker.
///
/// Keeps its rolling state across buffers so a file can be chunked as a stream
/// without ever holding it all in memory.
#[derive(Debug, Clone)]
pub struct Chunker {
    hash: u64,
    /// Bytes consumed since the last cut.
    count: usize,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        Chunker { hash: 0, count: 0 }
    }

    /// Discard rolling state. Call after every cut.
    pub fn reset(&mut self) {
        self.hash = 0;
        self.count = 0;
    }

    /// Feed a slice and return the offset within it at which to cut.
    ///
    /// `None` means no boundary in this slice — feed more data. The returned
    /// offset is relative to the start of `data`, not to the stream.
    pub fn find_boundary(&mut self, data: &[u8]) -> Option<usize> {
        for (i, &b) in data.iter().enumerate() {
            // Gear hash: shift left one bit, then mix in the byte's constant.
            self.hash = (self.hash << 1).wrapping_add(GEAR[b as usize]);
            self.count += 1;
            if self.count >= MIN_CHUNK && (self.hash & MASK) == 0 {
                return Some(i + 1);
            }
            if self.count >= MAX_CHUNK {
                return Some(i + 1);
            }
        }
        None
    }
}

/// Split an in-memory buffer into content-defined chunks.
pub fn chunk_slices(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut chunker = Chunker::new();
    let mut start = 0;
    while start < data.len() {
        match chunker.find_boundary(&data[start..]) {
            Some(cut) => {
                out.push(&data[start..start + cut]);
                start += cut;
                chunker.reset();
            }
            None => {
                out.push(&data[start..]);
                break;
            }
        }
    }
    out
}

/// Stream `reader` through the chunker, calling `emit` once per chunk.
///
/// Returns the total number of bytes read. Memory use is bounded by the chunk
/// ceiling, not by file size, so multi-gigabyte files are fine.
pub fn chunk_reader<R, F>(reader: &mut R, mut emit: F) -> std::io::Result<u64>
where
    R: std::io::Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut buf = vec![0u8; STREAM_BUF];
    let mut start = 0usize; // start of unconsumed data
    let mut end = 0usize; // end of valid data
    let mut scanned = 0usize; // bytes of buf[start..end] already fed to the chunker
    let mut total = 0u64;
    let mut eof = false;
    let mut chunker = Chunker::new();

    loop {
        let avail = end - start;
        if scanned < avail {
            match chunker.find_boundary(&buf[start + scanned..end]) {
                Some(cut_in_slice) => {
                    let cut = scanned + cut_in_slice;
                    emit(&buf[start..start + cut])?;
                    total += cut as u64;
                    start += cut;
                    scanned = 0;
                    chunker.reset();
                }
                None => scanned = avail,
            }
            continue;
        }

        // Everything buffered has been scanned and no boundary was found.
        if eof {
            if avail > 0 {
                emit(&buf[start..end])?;
                total += avail as u64;
            }
            break;
        }

        if start > 0 {
            buf.copy_within(start..end, 0);
            end -= start;
            start = 0;
        }
        if end == buf.len() {
            // Only reachable if MAX_CHUNK >= STREAM_BUF; grow rather than fail.
            buf.resize(buf.len() + STREAM_BUF, 0);
        }
        let n = reader.read(&mut buf[end..])?;
        if n == 0 {
            eof = true;
        } else {
            end += n;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Deterministic pseudo-random filler so tests exercise real content
    /// without pulling in a dev-dependency RNG.
    fn gen(seed: u64, n: usize) -> Vec<u8> {
        let mut s = seed;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            v.push((s >> 24) as u8);
        }
        v
    }

    #[test]
    fn chunks_reassemble_to_original() {
        let data = gen(42, 1_500_000);
        let chunks = chunk_slices(&data);
        assert!(chunks.len() > 10, "expected many chunks, got {}", chunks.len());
        let joined: Vec<u8> = chunks.concat();
        assert_eq!(joined, data);
    }

    #[test]
    fn chunk_sizes_stay_within_bounds() {
        let data = gen(7, 3_000_000);
        for c in chunk_slices(&data) {
            assert!(c.len() >= MIN_CHUNK || c.len() == data.len(), "chunk too small: {}", c.len());
            assert!(c.len() <= MAX_CHUNK, "chunk too large: {}", c.len());
        }
    }

    #[test]
    fn average_chunk_size_is_near_target() {
        let data = gen(1234, 32 * 1024 * 1024);
        let chunks = chunk_slices(&data);
        let avg = data.len() / chunks.len();
        // Cut points are geometric with mean AVG_CHUNK, but cuts offered
        // before MIN_CHUNK are skipped, which lifts the observed mean by
        // about MIN_CHUNK.
        let expected = MIN_CHUNK + AVG_CHUNK;
        let lo = expected * 7 / 10;
        let hi = expected * 15 / 10;
        assert!(
            avg > lo && avg < hi,
            "average {avg} not within {lo}..{hi} (nominal {AVG_CHUNK} plus {MIN_CHUNK} floor)"
        );
    }

    /// The property that justifies all of this: an insertion in the middle
    /// must not shift the boundaries of the surrounding data.
    #[test]
    fn insertion_only_changes_local_chunks() {
        let original = gen(99, 4 * 1024 * 1024);
        let before: Vec<Vec<u8>> = chunk_slices(&original).iter().map(|s| s.to_vec()).collect();
        let total_before = before.len();

        // Splice 1000 bytes in near the front.
        let splice_at = 100_000;
        let mut after_data = Vec::with_capacity(original.len() + 1000);
        after_data.extend_from_slice(&original[..splice_at]);
        after_data.extend_from_slice(&[0xA5u8; 1000]);
        after_data.extend_from_slice(&original[splice_at..]);

        let after_chunks = chunk_slices(&after_data);

        // Only a handful of chunks should differ; the tail must be identical.
        let mut differing = 0;
        let n = after_chunks.len().min(before.len());
        let mut first_diff = n;
        for i in 0..n {
            if before[i] != after_chunks[i] {
                differing += 1;
                if i < first_diff {
                    first_diff = i;
                }
            }
        }
        assert!(first_diff < 4, "first differing chunk index {first_diff} should be near the splice");
        assert!(
            differing <= 4,
            "expected at most 4 chunks to change, {differing} did (of {total_before})"
        );
    }

    #[test]
    fn identical_prefix_yields_identical_chunks() {
        let a = gen(5, 500_000);
        let mut b = a.clone();
        b.extend_from_slice(&gen(6, 500_000));
        let ca = chunk_slices(&a);
        let cb = chunk_slices(&b);
        // All chunks of `a` except possibly the last must appear unchanged in `b`.
        for (i, c) in ca[..ca.len() - 1].iter().enumerate() {
            assert_eq!(*c, cb[i], "chunk {i} drifted after appending data");
        }
    }

    #[test]
    fn small_inputs_become_one_chunk() {
        assert_eq!(chunk_slices(b"").len(), 0);
        assert_eq!(chunk_slices(b"hello").len(), 1);
        assert_eq!(chunk_slices(&vec![0u8; MIN_CHUNK - 1]).len(), 1);
    }

    #[test]
    fn streaming_matches_in_memory() {
        let data = gen(2024, 2_500_000);
        let streamed: Vec<Vec<u8>> = {
            let mut out = Vec::new();
            let mut cur = Cursor::new(&data);
            let n = chunk_reader(&mut cur, |c| {
                out.push(c.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(n, data.len() as u64);
            out
        };
        let direct = chunk_slices(&data);
        let a: Vec<&[u8]> = streamed.iter().map(|v| v.as_slice()).collect();
        assert_eq!(a, direct);
    }

    #[test]
    fn streaming_handles_empty_and_tiny() {
        for size in [0usize, 1, 100, MIN_CHUNK, MIN_CHUNK + 1] {
            let data = gen(size as u64 + 1, size);
            let mut out: Vec<Vec<u8>> = Vec::new();
            let mut cur = Cursor::new(&data);
            let n = chunk_reader(&mut cur, |c| {
                out.push(c.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(n, size as u64, "size {size}");
            assert_eq!(out.concat(), data, "size {size}");
        }
    }

    /// A file of one repeated byte has a degenerate rolling hash; the min/max
    /// clamps must still produce sane chunking.
    #[test]
    fn pathological_repeated_bytes() {
        let data = vec![0u8; 1024 * 1024];
        let chunks = chunk_slices(&data);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), data);
        for c in chunks {
            assert!(c.len() <= MAX_CHUNK);
        }
    }
}
