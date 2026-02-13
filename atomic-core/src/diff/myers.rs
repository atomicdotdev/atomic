//! Myers diff algorithm implementation.
//!
//! This module implements a diff algorithm based on the Longest Common
//! Subsequence (LCS) approach. While the original Myers algorithm uses
//! an O(ND) approach, this implementation uses dynamic programming for
//! correctness and simplicity.
//!
//! # Algorithm Overview
//!
//! The algorithm works by:
//! 1. Computing the Longest Common Subsequence (LCS) of both sequences
//! 2. Using the LCS to determine which elements are kept vs changed
//! 3. Converting this into a sequence of diff operations
//!
//! # Complexity
//!
//! - **Time**: O(NM) where N and M are the sequence lengths
//! - **Space**: O(NM) for the DP table
//!
//! For very large files, consider using a more memory-efficient algorithm.

use super::line::Line;
use super::ops::{DiffOp, DiffResult};

/// Compute the diff between two sequences using the LCS-based algorithm.
///
/// This finds the operations needed to transform `old` into `new`.
///
/// # Arguments
///
/// * `old` - The original sequence
/// * `new` - The modified sequence
///
/// # Returns
///
/// A [`DiffResult`] containing the operations to transform old → new.
pub fn diff<'a>(old: &[Line<'a>], new: &[Line<'a>]) -> DiffResult {
    let n = old.len();
    let m = new.len();

    // Special cases for empty sequences
    if n == 0 && m == 0 {
        return DiffResult::new();
    }
    if n == 0 {
        let mut result = DiffResult::new();
        result.push(DiffOp::insert(0, 0, m));
        return result;
    }
    if m == 0 {
        let mut result = DiffResult::new();
        result.push(DiffOp::delete(0, 0, n));
        return result;
    }

    // Compute LCS using dynamic programming
    let lcs = compute_lcs(old, new);

    // Convert LCS to diff operations
    lcs_to_diff(&lcs, n, m)
}

/// Represents an element in the LCS.
#[derive(Debug, Clone, Copy)]
struct LcsElement {
    old_idx: usize,
    new_idx: usize,
}

/// Compute the Longest Common Subsequence using dynamic programming.
///
/// Returns a list of matching positions in both sequences.
fn compute_lcs<'a>(old: &[Line<'a>], new: &[Line<'a>]) -> Vec<LcsElement> {
    let n = old.len();
    let m = new.len();

    // Build DP table
    // dp[i][j] = length of LCS of old[0..i] and new[0..j]
    let mut dp = vec![vec![0usize; m + 1]; n + 1];

    for i in 1..=n {
        for j in 1..=m {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find the actual LCS
    let mut lcs = Vec::with_capacity(dp[n][m]);
    let mut i = n;
    let mut j = m;

    while i > 0 && j > 0 {
        if old[i - 1] == new[j - 1] {
            lcs.push(LcsElement {
                old_idx: i - 1,
                new_idx: j - 1,
            });
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    lcs.reverse();
    lcs
}

/// Convert an LCS to diff operations.
fn lcs_to_diff(lcs: &[LcsElement], old_len: usize, new_len: usize) -> DiffResult {
    let mut result = DiffResult::new();
    let mut old_pos = 0;
    let mut new_pos = 0;

    for elem in lcs {
        // Handle deletions and insertions before this match
        let del_count = elem.old_idx - old_pos;
        let ins_count = elem.new_idx - new_pos;

        if del_count > 0 && ins_count > 0 {
            // Replace operation
            result.push(DiffOp::replace(old_pos, del_count, new_pos, ins_count));
        } else if del_count > 0 {
            result.push(DiffOp::delete(old_pos, new_pos, del_count));
        } else if ins_count > 0 {
            result.push(DiffOp::insert(old_pos, new_pos, ins_count));
        }

    }

    // Recalculate to properly merge consecutive equals
    result = DiffResult::new();
    old_pos = 0;
    new_pos = 0;
    let mut lcs_idx = 0;

    while old_pos < old_len || new_pos < new_len {
        // Check if current position is part of LCS
        if lcs_idx < lcs.len() && lcs[lcs_idx].old_idx == old_pos && lcs[lcs_idx].new_idx == new_pos
        {
            // Count consecutive matches
            let start_old = old_pos;
            let start_new = new_pos;

            while lcs_idx < lcs.len()
                && lcs[lcs_idx].old_idx == old_pos
                && lcs[lcs_idx].new_idx == new_pos
            {
                old_pos += 1;
                new_pos += 1;
                lcs_idx += 1;
            }

            let match_len = old_pos - start_old;
            result.push(DiffOp::equal(start_old, start_new, match_len));
        } else {
            // Find the next LCS element or end
            let next_old = if lcs_idx < lcs.len() {
                lcs[lcs_idx].old_idx
            } else {
                old_len
            };
            let next_new = if lcs_idx < lcs.len() {
                lcs[lcs_idx].new_idx
            } else {
                new_len
            };

            let del_count = next_old - old_pos;
            let ins_count = next_new - new_pos;

            if del_count > 0 && ins_count > 0 {
                result.push(DiffOp::replace(old_pos, del_count, new_pos, ins_count));
            } else if del_count > 0 {
                result.push(DiffOp::delete(old_pos, new_pos, del_count));
            } else if ins_count > 0 {
                result.push(DiffOp::insert(old_pos, new_pos, ins_count));
            }

            old_pos = next_old;
            new_pos = next_new;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create lines from strings
    fn lines<'a>(strs: &[&'a str]) -> Vec<Line<'a>> {
        strs.iter().map(|s| Line::new(s.as_bytes())).collect()
    }

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

        // Should be detected as replace (delete b, insert x)
        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 1);
    }

    #[test]
    fn test_multiple_changes() {
        let old = lines(&["a\n", "b\n", "c\n", "d\n", "e\n"]);
        let new = lines(&["a\n", "x\n", "c\n", "y\n", "e\n"]);

        let result = diff(&old, &new);

        // b→x and d→y
        assert_eq!(result.deletions(), 2);
        assert_eq!(result.insertions(), 2);
    }

    #[test]
    fn test_insert_at_start() {
        let old = lines(&["b\n", "c\n"]);
        let new = lines(&["a\n", "b\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.insertions(), 1);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_insert_at_end() {
        let old = lines(&["a\n", "b\n"]);
        let new = lines(&["a\n", "b\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.insertions(), 1);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_delete_at_start() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["b\n", "c\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_delete_at_end() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "b\n"]);

        let result = diff(&old, &new);

        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_completely_different() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["x\n", "y\n", "z\n"]);

        let result = diff(&old, &new);

        // All replaced
        assert_eq!(result.deletions(), 3);
        assert_eq!(result.insertions(), 3);
    }

    #[test]
    fn test_operations_have_correct_positions() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "x\n", "c\n"]);

        let result = diff(&old, &new);

        // Find the change operation
        let changes: Vec<_> = result.changes().collect();
        assert_eq!(changes.len(), 1);

        // The change should be at position 1 (second line)
        let change = changes[0];
        assert_eq!(change.old_range().start, 1);
    }

    #[test]
    fn test_edit_distance() {
        let old = lines(&["a\n", "b\n", "c\n"]);
        let new = lines(&["a\n", "x\n", "y\n", "c\n"]);

        let result = diff(&old, &new);

        // Delete b, insert x, insert y = 3 operations
        assert_eq!(result.edit_distance(), 3);
    }

    #[test]
    fn test_large_common_prefix() {
        let mut old_strs: Vec<&str> = (0..100).map(|_| "same\n").collect();
        old_strs.push("old\n");

        let mut new_strs: Vec<&str> = (0..100).map(|_| "same\n").collect();
        new_strs.push("new\n");

        let old = lines(&old_strs);
        let new = lines(&new_strs);

        let result = diff(&old, &new);

        // Only the last line differs
        assert_eq!(result.edit_distance(), 2); // delete old, insert new
    }

    #[test]
    fn test_large_common_suffix() {
        let mut old_strs = vec!["old\n"];
        old_strs.extend((0..100).map(|_| "same\n"));

        let mut new_strs = vec!["new\n"];
        new_strs.extend((0..100).map(|_| "same\n"));

        let old = lines(&old_strs);
        let new = lines(&new_strs);

        let result = diff(&old, &new);

        // Only the first line differs
        assert_eq!(result.edit_distance(), 2);
    }

    #[test]
    fn test_lcs_computation() {
        let old = lines(&["a\n", "b\n", "c\n", "d\n"]);
        let new = lines(&["a\n", "c\n", "d\n"]);

        let lcs = compute_lcs(&old, &new);

        // LCS should be a, c, d (3 elements)
        assert_eq!(lcs.len(), 3);
        assert_eq!(lcs[0].old_idx, 0);
        assert_eq!(lcs[0].new_idx, 0);
        assert_eq!(lcs[1].old_idx, 2);
        assert_eq!(lcs[1].new_idx, 1);
        assert_eq!(lcs[2].old_idx, 3);
        assert_eq!(lcs[2].new_idx, 2);
    }
}
