//! Content hashing utility for recipe-side line matching.
//!
//! Recipes that need to detect "which new line is the same as which old
//! line" (e.g., [`ExtractMove`](super::extract_move), future
//! `WhitespaceCleanup`, `GitImportWithMoves`) all need fast O(1) lookup
//! from line content to candidate positions.  This module provides:
//!
//! - [`hash_line`] — FNV-1a 64-bit hash of a byte slice
//! - [`LineHashIndex`] — `HashMap<u64, Vec<usize>>` indexing lines by
//!   content hash, with helpers for building from an iterator of lines
//!   and consuming candidates
//!
//! FNV-1a is chosen for speed; the index is in-memory and only valid for
//! the duration of one recipe invocation, so cryptographic strength
//! isn't needed.  Collisions are rare for line-length inputs and the
//! consumer treats the hash as a *candidate* signal that must be
//! verified by content equality.

use std::collections::HashMap;

/// FNV-1a 64-bit hash of a byte slice.
///
/// Used internally by [`LineHashIndex`] and exposed for recipes that
/// need to compute and compare hashes outside the index abstraction.
#[inline]
pub fn hash_line(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Index of line content → positions where that content appears.
///
/// Lookup is O(1) average.  Each insert appends to the position list
/// for its hash, so multiple identical lines produce a `Vec` of all
/// their positions (relevant for files with duplicate lines like blank
/// lines or repeated boilerplate).
#[derive(Debug, Default)]
pub struct LineHashIndex {
    buckets: HashMap<u64, Vec<usize>>,
}

impl LineHashIndex {
    /// Build an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an index from a slice of line byte-slices.
    ///
    /// `lines[i].as_ref()` is the byte content of position `i`.
    pub fn from_lines<T: AsRef<[u8]>>(lines: &[T]) -> Self {
        let mut idx = Self::new();
        for (i, line) in lines.iter().enumerate() {
            idx.insert(line.as_ref(), i);
        }
        idx
    }

    /// Insert a single line at `position`.
    pub fn insert(&mut self, line: &[u8], position: usize) {
        let h = hash_line(line);
        self.buckets.entry(h).or_default().push(position);
    }

    /// Return all positions that hash to the same value as `line`.
    ///
    /// Returns an empty slice if no match.  Callers must verify content
    /// equality before treating a hash hit as a definite match (FNV-1a
    /// has rare collisions).
    pub fn candidates(&self, line: &[u8]) -> &[usize] {
        let h = hash_line(line);
        self.buckets
            .get(&h)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Consume one candidate position for `line` — pops the first
    /// remaining position from the bucket.  Used when matching is
    /// 1:1 (each old line claims at most one new line, vice versa).
    ///
    /// Returns `None` if no candidates left.
    pub fn consume(&mut self, line: &[u8]) -> Option<usize> {
        let h = hash_line(line);
        if let Some(positions) = self.buckets.get_mut(&h) {
            if !positions.is_empty() {
                return Some(positions.remove(0));
            }
        }
        None
    }

    /// Number of distinct hash buckets (not total entries).
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Total number of indexed positions across all buckets.
    pub fn position_count(&self) -> usize {
        self.buckets.values().map(Vec::len).sum()
    }

    /// `true` if no positions have been indexed.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty() || self.position_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash_line(b"foo"), hash_line(b"foo"));
        assert_ne!(hash_line(b"foo"), hash_line(b"bar"));
        assert_eq!(hash_line(b""), hash_line(b""));
    }

    #[test]
    fn index_candidates_returns_positions_for_matching_content() {
        let lines: Vec<&[u8]> = vec![b"a", b"b", b"a", b"c"];
        let idx = LineHashIndex::from_lines(&lines);
        assert_eq!(idx.candidates(b"a"), &[0, 2]);
        assert_eq!(idx.candidates(b"b"), &[1]);
        assert_eq!(idx.candidates(b"c"), &[3]);
        assert!(idx.candidates(b"missing").is_empty());
    }

    #[test]
    fn consume_pops_one_at_a_time() {
        let lines: Vec<&[u8]> = vec![b"x", b"x", b"x"];
        let mut idx = LineHashIndex::from_lines(&lines);
        assert_eq!(idx.consume(b"x"), Some(0));
        assert_eq!(idx.consume(b"x"), Some(1));
        assert_eq!(idx.consume(b"x"), Some(2));
        assert_eq!(idx.consume(b"x"), None);
    }

    #[test]
    fn empty_index_reports_empty() {
        let idx: LineHashIndex = LineHashIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.position_count(), 0);
    }
}
