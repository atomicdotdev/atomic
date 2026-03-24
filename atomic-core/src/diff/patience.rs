//! Patience diff algorithm implementation.
//!
//! This module implements the Patience diff algorithm, created by Bram Cohen.
//! It often produces more human-readable diffs than Myers, especially for
//! code with repeated patterns or large structural changes.
//!
//! # Algorithm Overview
//!
//! The Patience diff algorithm works in three phases:
//!
//! 1. **Find unique matching lines**: Identify lines that appear exactly once
//!    in both sequences. These are reliable anchor points.
//!
//! 2. **Find the Longest Increasing Subsequence (LIS)**: Among the unique
//!    matches, find the longest sequence where positions increase in both
//!    sequences. This gives us the "skeleton" of unchanged structure.
//!
//! 3. **Recursively diff between anchors**: For regions between LIS anchors,
//!    recursively apply the algorithm (or fall back to Myers for regions
//!    with no unique matches).
//!
//! # Why "Patience"?
//!
//! The name comes from the card game "Patience" (Solitaire), specifically
//! the algorithm used to play it optimally, which is related to finding
//! the Longest Increasing Subsequence.
//!
//! # Example
//!
//! Consider diffing:
//! ```text
//! Old:        New:
//! void foo()  void foo()
//! {           {
//! }               int x;
//! void bar()  }
//! {           void bar()
//! }           {
//!             }
//! ```
//!
//! Myers might match the `}` on line 3 of old with the `}` on line 4 of new,
//! producing a confusing diff. Patience recognizes that `void foo()` and
//! `void bar()` are unique anchors and produces a cleaner diff showing
//! `int x;` was inserted inside `foo()`.
//!
//! # Complexity
//!
//! - **Time**: O(N log N) for the LIS phase, plus recursive diffing
//! - **Space**: O(N) for storing unique line mappings
//!
//! In the worst case (no unique lines), falls back to Myers which is O(ND).

use std::collections::HashMap;

use super::line::Line;
use super::myers;
use super::ops::{DiffOp, DiffResult};

/// Compute the diff between two sequences using the Patience algorithm.
///
/// This algorithm often produces more intuitive diffs for source code by
/// using unique lines as anchor points.
///
/// # Arguments
///
/// * `old` - The original sequence
/// * `new` - The modified sequence
///
/// # Returns
///
/// A [`DiffResult`] containing the operations to transform old → new.
///
/// # Algorithm
///
/// 1. Find lines that appear exactly once in both sequences
/// 2. Find the Longest Increasing Subsequence (LIS) of matching unique lines
/// 3. Use LIS as anchors and recursively diff regions between them
/// 4. Fall back to Myers diff for regions with no unique matches
pub fn diff<'a>(old: &[Line<'a>], new: &[Line<'a>]) -> DiffResult {
    // Special cases
    if old.is_empty() && new.is_empty() {
        return DiffResult::new();
    }
    if old.is_empty() {
        let mut result = DiffResult::new();
        result.push(DiffOp::insert(0, 0, new.len()));
        return result;
    }
    if new.is_empty() {
        let mut result = DiffResult::new();
        result.push(DiffOp::delete(0, 0, old.len()));
        return result;
    }

    // Run the patience algorithm
    patience_diff_recursive(old, new, 0, 0)
}

/// Recursively apply patience diff to a region.
///
/// # Arguments
///
/// * `old` - Slice of the old sequence for this region
/// * `new` - Slice of the new sequence for this region
/// * `old_offset` - Offset of this region in the original old sequence
/// * `new_offset` - Offset of this region in the original new sequence
fn patience_diff_recursive<'a>(
    old: &[Line<'a>],
    new: &[Line<'a>],
    old_offset: usize,
    new_offset: usize,
) -> DiffResult {
    // Base cases
    if old.is_empty() && new.is_empty() {
        return DiffResult::new();
    }
    if old.is_empty() {
        let mut result = DiffResult::new();
        result.push(DiffOp::insert(old_offset, new_offset, new.len()));
        return result;
    }
    if new.is_empty() {
        let mut result = DiffResult::new();
        result.push(DiffOp::delete(old_offset, new_offset, old.len()));
        return result;
    }

    // Find unique lines and their matches
    let matches = find_unique_matches(old, new);

    if matches.is_empty() {
        // No unique matches - fall back to Myers
        let mut result = myers::diff(old, new);
        result.adjust_offsets(old_offset);
        return result;
    }

    // Find the Longest Increasing Subsequence of matches
    let lis = longest_increasing_subsequence(&matches);

    if lis.is_empty() {
        // No LIS (shouldn't happen if matches is non-empty, but be safe)
        let mut result = myers::diff(old, new);
        result.adjust_offsets(old_offset);
        return result;
    }

    // Build the diff by processing regions between LIS anchors
    diff_with_anchors(old, new, old_offset, new_offset, &lis)
}

/// A match between unique lines in old and new sequences.
#[derive(Debug, Clone, Copy)]
struct UniqueMatch {
    /// Index in the old sequence
    old_idx: usize,
    /// Index in the new sequence
    new_idx: usize,
}

/// Find lines that appear exactly once in both sequences and match.
///
/// Returns a list of matches sorted by position in the old sequence.
fn find_unique_matches<'a>(old: &[Line<'a>], new: &[Line<'a>]) -> Vec<UniqueMatch> {
    // Count occurrences in old sequence
    let mut old_counts: HashMap<u64, (usize, usize)> = HashMap::new(); // hash -> (count, index)
    for (idx, line) in old.iter().enumerate() {
        let hash = line.hash_value();
        old_counts
            .entry(hash)
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, idx));
    }

    // Count occurrences in new sequence and find matches
    let mut new_counts: HashMap<u64, (usize, usize)> = HashMap::new();
    for (idx, line) in new.iter().enumerate() {
        let hash = line.hash_value();
        new_counts
            .entry(hash)
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, idx));
    }

    // Find lines that are unique in both and actually match
    let mut matches = Vec::new();
    for (idx, line) in old.iter().enumerate() {
        let hash = line.hash_value();

        // Check if unique in old
        if let Some(&(old_count, _)) = old_counts.get(&hash) {
            if old_count != 1 {
                continue;
            }
        }

        // Check if unique in new and find the index
        if let Some(&(new_count, new_idx)) = new_counts.get(&hash) {
            if new_count != 1 {
                continue;
            }

            // Verify actual equality (not just hash match)
            if old[idx] == new[new_idx] {
                matches.push(UniqueMatch {
                    old_idx: idx,
                    new_idx,
                });
            }
        }
    }

    // Sort by old_idx (should already be sorted, but ensure it)
    matches.sort_by_key(|m| m.old_idx);

    matches
}

/// Find the Longest Increasing Subsequence of matches by new_idx.
///
/// Given matches sorted by old_idx, find the longest subsequence where
/// new_idx values are strictly increasing. This ensures the matches
/// maintain relative order in both sequences.
///
/// Uses the O(N log N) algorithm with binary search.
fn longest_increasing_subsequence(matches: &[UniqueMatch]) -> Vec<UniqueMatch> {
    if matches.is_empty() {
        return Vec::new();
    }

    let n = matches.len();

    // tails[i] = index in matches of the smallest ending element of all
    // increasing subsequences of length i+1
    let mut tails: Vec<usize> = Vec::with_capacity(n);

    // predecessors[i] = index of the predecessor of matches[i] in the LIS
    let mut predecessors: Vec<Option<usize>> = vec![None; n];

    for i in 0..n {
        let new_idx = matches[i].new_idx;

        // Binary search for the position to insert/replace
        let pos = tails
            .binary_search_by(|&j| matches[j].new_idx.cmp(&new_idx))
            .unwrap_or_else(|x| x);

        // Update predecessor
        if pos > 0 {
            predecessors[i] = Some(tails[pos - 1]);
        }

        // Update tails
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
    }

    // Reconstruct the LIS by following predecessors
    let mut lis = Vec::with_capacity(tails.len());
    let mut current = tails.last().copied();

    while let Some(idx) = current {
        lis.push(matches[idx]);
        current = predecessors[idx];
    }

    lis.reverse();
    lis
}

/// Build the diff result using LIS anchors.
///
/// Process regions between consecutive anchors recursively.
fn diff_with_anchors<'a>(
    old: &[Line<'a>],
    new: &[Line<'a>],
    old_offset: usize,
    new_offset: usize,
    anchors: &[UniqueMatch],
) -> DiffResult {
    let mut result = DiffResult::new();

    let mut old_pos = 0;
    let mut new_pos = 0;

    for anchor in anchors {
        // Diff the region before this anchor
        if old_pos < anchor.old_idx || new_pos < anchor.new_idx {
            let old_region = &old[old_pos..anchor.old_idx];
            let new_region = &new[new_pos..anchor.new_idx];

            let region_result = patience_diff_recursive(
                old_region,
                new_region,
                old_offset + old_pos,
                new_offset + new_pos,
            );

            for op in region_result {
                result.push(op);
            }
        }

        // The anchor itself is an equal line
        result.push(DiffOp::equal(
            old_offset + anchor.old_idx,
            new_offset + anchor.new_idx,
            1,
        ));

        old_pos = anchor.old_idx + 1;
        new_pos = anchor.new_idx + 1;
    }

    // Diff the region after the last anchor
    if old_pos < old.len() || new_pos < new.len() {
        let old_region = &old[old_pos..];
        let new_region = &new[new_pos..];

        let region_result = patience_diff_recursive(
            old_region,
            new_region,
            old_offset + old_pos,
            new_offset + new_pos,
        );

        for op in region_result {
            result.push(op);
        }
    }

    // Merge consecutive equal operations
    merge_equal_ops(&mut result);

    result
}

/// Merge consecutive Equal operations in the result.
fn merge_equal_ops(result: &mut DiffResult) {
    // Take ownership of the ops by replacing with empty DiffResult
    let old_result = std::mem::take(result);
    let ops = old_result.into_ops();
    let mut merged = Vec::with_capacity(ops.len());

    for op in ops {
        if let DiffOp::Equal {
            old_pos,
            new_pos,
            len,
        } = op
        {
            // Check if we can merge with the previous operation
            if let Some(DiffOp::Equal {
                old_pos: prev_old,
                new_pos: prev_new,
                len: prev_len,
            }) = merged.last_mut()
            {
                if *prev_old + *prev_len == old_pos && *prev_new + *prev_len == new_pos {
                    *prev_len += len;
                    continue;
                }
            }
        }
        merged.push(op);
    }

    *result = DiffResult::with_ops(merged);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create lines from strings
    fn lines<'a>(strs: &[&'a str]) -> Vec<Line<'a>> {
        strs.iter().map(|s| Line::new(s.as_bytes())).collect()
    }

    // Basic Tests
    #[test]
    fn test_empty_sequences() {
        let result = diff(&[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_insert_all() {
        let old: Vec<Line> = vec![];
        let new = lines(&["a\n", "b\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.insertions(), 3);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_delete_all() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new: Vec<Line> = vec![];

        let result = diff(&old, &new);

        assert_eq!(result.deletions(), 3);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_identical() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "b\n", "c\n"]);

        let result = diff(&old, &new);

        assert!(result.is_unchanged());
    }

    #[test]
    fn test_single_insert() {
        let old = lines(&["a\n", "c\n"]);
        let new = lines(&["a\n", "b\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.insertions(), 1);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_single_delete() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_replace() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "x\n", "c\n"]);

        let result = diff(&old, &new);

        // a and c are unique anchors, so b→x is a change between them
        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 1);
    }

    // Unique Match Tests
    #[test]
    fn test_find_unique_matches_simple() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "x\n", "c\n"]);

        let matches = find_unique_matches(&old, &new);

        // a and c are unique in both
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].old_idx, 0);
        assert_eq!(matches[0].new_idx, 0);
        assert_eq!(matches[1].old_idx, 2);
        assert_eq!(matches[1].new_idx, 2);
    }

    #[test]
    fn test_find_unique_matches_duplicates() {
        let old = lines(&["a\n", "a\n", "b\n"]);
        let new = lines(&["a\n", "b\n", "a\n"]);

        let matches = find_unique_matches(&old, &new);

        // Only b is unique in both
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].old_idx, 2);
        assert_eq!(matches[0].new_idx, 1);
    }

    #[test]
    fn test_find_unique_matches_none() {
        let old = lines(&["a\n", "a\n"]);
        let new = lines(&["a\n", "a\n"]);

        let matches = find_unique_matches(&old, &new);

        // No unique lines
        assert!(matches.is_empty());
    }

    #[test]
    fn test_find_unique_matches_no_common() {
        let old = lines(&["a\n", "b\n"]);
        let new = lines(&["x\n", "y\n"]);

        let matches = find_unique_matches(&old, &new);

        // No common lines
        assert!(matches.is_empty());
    }

    // LIS Tests
    #[test]
    fn test_lis_simple() {
        let matches = vec![
            UniqueMatch {
                old_idx: 0,
                new_idx: 0,
            },
            UniqueMatch {
                old_idx: 1,
                new_idx: 2,
            },
            UniqueMatch {
                old_idx: 2,
                new_idx: 1,
            },
            UniqueMatch {
                old_idx: 3,
                new_idx: 3,
            },
        ];

        let lis = longest_increasing_subsequence(&matches);

        // LIS by new_idx: 0 -> 2 -> 3 (indices 0, 1, 3) or 0 -> 1 -> 3 (indices 0, 2, 3)
        // Both have length 3
        assert_eq!(lis.len(), 3);
    }

    #[test]
    fn test_lis_empty() {
        let matches: Vec<UniqueMatch> = vec![];
        let lis = longest_increasing_subsequence(&matches);
        assert!(lis.is_empty());
    }

    #[test]
    fn test_lis_single() {
        let matches = vec![UniqueMatch {
            old_idx: 5,
            new_idx: 3,
        }];
        let lis = longest_increasing_subsequence(&matches);
        assert_eq!(lis.len(), 1);
    }

    #[test]
    fn test_lis_all_increasing() {
        let matches = vec![
            UniqueMatch {
                old_idx: 0,
                new_idx: 0,
            },
            UniqueMatch {
                old_idx: 1,
                new_idx: 1,
            },
            UniqueMatch {
                old_idx: 2,
                new_idx: 2,
            },
        ];

        let lis = longest_increasing_subsequence(&matches);

        assert_eq!(lis.len(), 3);
    }

    #[test]
    fn test_lis_all_decreasing() {
        let matches = vec![
            UniqueMatch {
                old_idx: 0,
                new_idx: 2,
            },
            UniqueMatch {
                old_idx: 1,
                new_idx: 1,
            },
            UniqueMatch {
                old_idx: 2,
                new_idx: 0,
            },
        ];

        let lis = longest_increasing_subsequence(&matches);

        // Only one element can be in LIS
        assert_eq!(lis.len(), 1);
    }

    // Integration Tests
    #[test]
    fn test_patience_vs_myers_structural() {
        // A case where patience should produce better results
        let old = lines(&["void foo() {\n", "}\n", "void bar() {\n", "}\n"]);
        let new = lines(&[
            "void foo() {\n",
            "    int x;\n",
            "}\n",
            "void bar() {\n",
            "}\n",
        ]);

        let result = diff(&old, &new);

        // Should detect insertion of "int x;" inside foo
        assert_eq!(result.insertions(), 1);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_fallback_to_myers() {
        // All lines are duplicates - no unique matches
        let old = lines(&["{\n", "{\n", "}\n", "}\n"]);
        let new = lines(&["{\n", "x\n", "{\n", "}\n", "}\n"]);

        let result = diff(&old, &new);

        // Should still produce valid diff (via Myers fallback)
        assert!(!result.is_unchanged());
    }

    #[test]
    fn test_complex_reordering() {
        let old = lines(&["first\n", "second\n", "third\n", "fourth\n", "fifth\n"]);
        let new = lines(&["first\n", "third\n", "inserted\n", "fourth\n", "fifth\n"]);

        let result = diff(&old, &new);

        // second deleted, inserted added
        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 1);
    }

    #[test]
    fn test_preserves_order() {
        let old = lines(&["a\n", "b\n", "c\n", "d\n", "e\n"]);
        let new = lines(&["a\n", "c\n", "e\n"]);

        let result = diff(&old, &new);

        // b and d deleted
        assert_eq!(result.deletions(), 2);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_merge_equal_ops() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 1));
        result.push(DiffOp::equal(1, 1, 1));
        result.push(DiffOp::equal(2, 2, 1));

        merge_equal_ops(&mut result);

        assert_eq!(result.len(), 1);
        if let DiffOp::Equal { len, .. } = result[0] {
            assert_eq!(len, 3);
        } else {
            panic!("Expected Equal operation");
        }
    }

    #[test]
    fn test_edit_distance() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "x\n", "y\n", "c\n"]);

        let result = diff(&old, &new);

        // Delete b, insert x, insert y = 3 operations
        assert_eq!(result.edit_distance(), 3);
    }
}
