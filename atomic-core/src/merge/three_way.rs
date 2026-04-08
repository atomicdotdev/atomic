//! Three-way token-level merge algorithm.
//!
//! Given three token sequences — **base**, **left**, and **right** — this
//! module determines whether the left and right edits can be combined
//! without conflict.
//!
//! # Algorithm
//!
//! 1. Compute an LCS-based edit script from `base → left` and `base → right`.
//! 2. Map each edit to the base token index it affects.
//! 3. If any base index is modified by **both** sides (and the modifications
//!    differ), report a conflict.
//! 4. Otherwise, combine both edit sets into a single merged token sequence.
//!
//! # Handling Insertions and Deletions
//!
//! When the token counts differ between base/left/right, we fall back to
//! an LCS-based alignment so that insertions and deletions at non-overlapping
//! positions can still be auto-merged.

use crate::diff::token::Tokenizer;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a three-way token merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreeWayResult {
    /// All edits are non-overlapping — merged content is available.
    Merged(Vec<u8>),
    /// At least one token position was edited by both sides differently.
    Conflict,
}

/// A single token extracted from content bytes.
///
/// This is an owned representation (unlike [`Token`] which borrows), so
/// that token sequences can outlive the input buffer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MergeToken {
    /// The raw content bytes of this token.
    pub content: Vec<u8>,
}

impl MergeToken {
    /// Create a new merge token from a byte slice.
    pub fn new(content: &[u8]) -> Self {
        Self {
            content: content.to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// Edit operations (internal)
// ---------------------------------------------------------------------------

/// An edit operation on a base token sequence.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum EditOp {
    /// Keep the base token at this index.
    Keep(usize),
    /// Replace the base token at this index with new content.
    Replace(usize, Vec<u8>),
    /// Delete the base token at this index.
    Delete(usize),
    /// Insert new content *after* the given base index.
    /// `usize::MAX` means insert before the very first token.
    Insert(usize, Vec<u8>),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Tokenize a byte slice into owned [`MergeToken`]s.
///
/// Uses the project's code-aware [`Tokenizer`] so that operators,
/// strings, numbers, and comments are recognised as single tokens.
pub fn tokenize(content: &[u8]) -> Vec<MergeToken> {
    Tokenizer::tokenize_all(content)
        .into_iter()
        .map(|t| MergeToken::new(t.content()))
        .collect()
}

/// Perform a three-way merge of token sequences.
///
/// * `base`  — the common ancestor's tokens
/// * `left`  — one side's tokens
/// * `right` — the other side's tokens
///
/// Returns [`ThreeWayResult::Merged`] when all edits are to different
/// positions, or [`ThreeWayResult::Conflict`] when at least one token
/// position was changed by both sides to different values.
pub fn three_way_merge(
    base: &[MergeToken],
    left: &[MergeToken],
    right: &[MergeToken],
) -> ThreeWayResult {
    // Fast path: if left and right are identical, no conflict possible.
    if left == right {
        let content = reassemble_tokens(left);
        return ThreeWayResult::Merged(content);
    }

    // Fast path: if left is unchanged, take right.
    if left == base {
        let content = reassemble_tokens(right);
        return ThreeWayResult::Merged(content);
    }

    // Fast path: if right is unchanged, take left.
    if right == base {
        let content = reassemble_tokens(left);
        return ThreeWayResult::Merged(content);
    }

    // --- General case: LCS-based alignment ---

    let left_ops = diff_tokens(base, left);
    let right_ops = diff_tokens(base, right);

    // Collect base indices modified by each side.
    let left_modified = modified_indices(&left_ops);
    let right_modified = modified_indices(&right_ops);

    // Check for overlapping modifications.
    for idx in left_modified.intersection(&right_modified) {
        // Both sides touched the same base index. This is only OK
        // if they produced the *same* replacement.
        let left_replacement = replacement_for(&left_ops, *idx);
        let right_replacement = replacement_for(&right_ops, *idx);
        if left_replacement != right_replacement {
            return ThreeWayResult::Conflict;
        }
    }

    // Check for conflicting insertions at the same position.
    let left_inserts = insert_positions(&left_ops);
    let right_inserts = insert_positions(&right_ops);
    for pos in left_inserts.keys() {
        if let Some(right_content) = right_inserts.get(pos) {
            let left_content = &left_inserts[pos];
            if left_content != right_content {
                return ThreeWayResult::Conflict;
            }
        }
    }

    // No conflicts — merge.
    let merged = merge_ops(base, &left_ops, &right_ops);
    ThreeWayResult::Merged(merged)
}

/// Convenience wrapper: tokenize byte slices and merge.
pub fn three_way_merge_bytes(base: &[u8], left: &[u8], right: &[u8]) -> ThreeWayResult {
    let base_tokens = tokenize(base);
    let left_tokens = tokenize(left);
    let right_tokens = tokenize(right);
    three_way_merge(&base_tokens, &left_tokens, &right_tokens)
}

// ---------------------------------------------------------------------------
// LCS-based token diff
// ---------------------------------------------------------------------------

/// Compute an edit script transforming `old` into `new` at the token level.
///
/// Returns a sequence of [`EditOp`]s. The algorithm uses dynamic-programming
/// LCS to align the two sequences and then walks the DP table to emit
/// keep / delete / insert / replace operations.
fn diff_tokens(old: &[MergeToken], new: &[MergeToken]) -> Vec<EditOp> {
    let n = old.len();
    let m = new.len();

    if n == 0 && m == 0 {
        return Vec::new();
    }
    if n == 0 {
        // Everything in `new` is an insertion before the start.
        return new
            .iter()
            .map(|t| EditOp::Insert(usize::MAX, t.content.clone()))
            .collect();
    }
    if m == 0 {
        return (0..n).map(EditOp::Delete).collect();
    }

    // DP table for LCS lengths: dp[i][j] = LCS(old[0..i], new[0..j])
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Back-track to produce edit operations.
    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;

    // We collect in reverse order, then reverse at the end.
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            ops.push(EditOp::Keep(i - 1));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            // Insertion from `new` — record the base index *after which* it sits.
            let after = if i > 0 { i - 1 } else { usize::MAX };
            ops.push(EditOp::Insert(after, new[j - 1].content.clone()));
            j -= 1;
        } else {
            // Deletion from `old`.
            ops.push(EditOp::Delete(i - 1));
            i -= 1;
        }
    }

    ops.reverse();

    // Coalesce adjacent Delete(i) + Insert(i, _) into Replace(i, _).
    coalesce_replace(&mut ops);

    ops
}

/// Turn adjacent Delete / Insert pairs at the same position into Replace.
fn coalesce_replace(ops: &mut Vec<EditOp>) {
    let mut i = 0;
    while i + 1 < ops.len() {
        let is_pair = {
            if let (EditOp::Delete(del_idx), EditOp::Insert(ins_after, _)) = (&ops[i], &ops[i + 1])
            {
                // The insert sits right after the deleted index.
                *ins_after == *del_idx || (*del_idx == 0 && *ins_after == usize::MAX)
            } else {
                false
            }
        };

        if is_pair {
            let content = match &ops[i + 1] {
                EditOp::Insert(_, c) => c.clone(),
                _ => unreachable!(),
            };
            let idx = match &ops[i] {
                EditOp::Delete(idx) => *idx,
                _ => unreachable!(),
            };
            ops[i] = EditOp::Replace(idx, content);
            ops.remove(i + 1);
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// Helpers: analyse edit scripts
// ---------------------------------------------------------------------------

/// Collect the set of base indices that are modified (replaced or deleted).
fn modified_indices(ops: &[EditOp]) -> HashSet<usize> {
    let mut set = HashSet::new();
    for op in ops {
        match op {
            EditOp::Replace(idx, _) | EditOp::Delete(idx) => {
                set.insert(*idx);
            }
            _ => {}
        }
    }
    set
}

/// For a given base index, return the replacement content (if any).
/// `None` means the token was deleted rather than replaced.
fn replacement_for(ops: &[EditOp], base_idx: usize) -> Option<Vec<u8>> {
    for op in ops {
        match op {
            EditOp::Replace(idx, content) if *idx == base_idx => {
                return Some(content.clone());
            }
            EditOp::Delete(idx) if *idx == base_idx => {
                return None;
            }
            _ => {}
        }
    }
    // Not modified — return the sentinel "keep".
    Some(Vec::new())
}

/// Collect all insertions keyed by the base index they follow.
fn insert_positions(ops: &[EditOp]) -> HashMap<usize, Vec<Vec<u8>>> {
    let mut map: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
    for op in ops {
        if let EditOp::Insert(after, content) = op {
            map.entry(*after).or_default().push(content.clone());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Merge application
// ---------------------------------------------------------------------------

/// Combine two non-conflicting edit scripts and produce merged content bytes.
fn merge_ops(base: &[MergeToken], left_ops: &[EditOp], right_ops: &[EditOp]) -> Vec<u8> {
    // Build per-index action maps.
    let left_action = build_action_map(left_ops);
    let right_action = build_action_map(right_ops);

    let left_inserts = insert_positions(left_ops);
    let right_inserts = insert_positions(right_ops);

    let mut result: Vec<u8> = Vec::new();

    // Handle insertions before the first token.
    emit_inserts(&left_inserts, usize::MAX, &mut result);
    emit_inserts(&right_inserts, usize::MAX, &mut result);

    for (i, base_token) in base.iter().enumerate() {
        // Determine what to emit for base token i.
        let left_act = left_action.get(&i);
        let right_act = right_action.get(&i);

        match (left_act, right_act) {
            // Both sides kept (or neither touched) — emit base.
            (None, None) => {
                result.extend_from_slice(&base_token.content);
            }
            // Only left modified.
            (Some(action), None) => {
                emit_action(action, base_token, &mut result);
            }
            // Only right modified.
            (None, Some(action)) => {
                emit_action(action, base_token, &mut result);
            }
            // Both modified identically (already verified no conflict).
            (Some(action), Some(_)) => {
                emit_action(action, base_token, &mut result);
            }
        }

        // Emit any insertions that follow base token i.
        emit_inserts(&left_inserts, i, &mut result);
        emit_inserts(&right_inserts, i, &mut result);
    }

    result
}

/// Action on a single base token: either replace or delete.
#[derive(Debug, Clone)]
enum Action {
    Replace(Vec<u8>),
    Delete,
}

/// Build a map from base index → action for modifications only (not keeps).
fn build_action_map(ops: &[EditOp]) -> HashMap<usize, Action> {
    let mut map = HashMap::new();
    for op in ops {
        match op {
            EditOp::Replace(idx, content) => {
                map.insert(*idx, Action::Replace(content.clone()));
            }
            EditOp::Delete(idx) => {
                map.insert(*idx, Action::Delete);
            }
            _ => {}
        }
    }
    map
}

/// Emit the result of an action (replace or delete) on a base token.
fn emit_action(action: &Action, _base_token: &MergeToken, result: &mut Vec<u8>) {
    match action {
        Action::Replace(content) => {
            result.extend_from_slice(content);
        }
        Action::Delete => {
            // Token deleted — emit nothing.
        }
    }
}

/// Emit all insertions after a given base index.
fn emit_inserts(inserts: &HashMap<usize, Vec<Vec<u8>>>, after_idx: usize, result: &mut Vec<u8>) {
    if let Some(contents) = inserts.get(&after_idx) {
        for content in contents {
            result.extend_from_slice(content);
        }
    }
}

/// Reassemble a token sequence into raw bytes.
fn reassemble_tokens(tokens: &[MergeToken]) -> Vec<u8> {
    tokens
        .iter()
        .flat_map(|t| t.content.iter().copied())
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Helpers ----------------------------------------------------------

    fn merged_string(result: &ThreeWayResult) -> Option<String> {
        match result {
            ThreeWayResult::Merged(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            ThreeWayResult::Conflict => None,
        }
    }

    // ---- Basic merge cases ------------------------------------------------

    #[test]
    fn test_no_changes() {
        let base = tokenize(b"hello world");
        let result = three_way_merge(&base, &base, &base);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "hello world");
    }

    #[test]
    fn test_only_left_changes() {
        let base = tokenize(b"x = 1");
        let left = tokenize(b"x = 2");
        let right = tokenize(b"x = 1");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "x = 2");
    }

    #[test]
    fn test_only_right_changes() {
        let base = tokenize(b"x = 1");
        let left = tokenize(b"x = 1");
        let right = tokenize(b"x = 2");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "x = 2");
    }

    #[test]
    fn test_non_overlapping_token_edits() {
        let base = tokenize(b"host: localhost, port: 3000");
        let left = tokenize(b"host: production, port: 3000");
        let right = tokenize(b"host: localhost, port: 8080");

        let result = three_way_merge(&base, &left, &right);
        assert!(
            matches!(result, ThreeWayResult::Merged(_)),
            "expected Merged, got {:?}",
            result,
        );
        assert_eq!(
            merged_string(&result).unwrap(),
            "host: production, port: 8080",
        );
    }

    #[test]
    fn test_overlapping_token_edits_conflict() {
        let base = tokenize(b"x = 1");
        let left = tokenize(b"x = 2");
        let right = tokenize(b"x = 3");

        let result = three_way_merge(&base, &left, &right);
        assert!(
            matches!(result, ThreeWayResult::Conflict),
            "expected Conflict, got {:?}",
            result,
        );
    }

    #[test]
    fn test_both_same_change_no_conflict() {
        let base = tokenize(b"x = 1");
        let left = tokenize(b"x = 2");
        let right = tokenize(b"x = 2");

        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "x = 2");
    }

    // ---- Empty inputs -----------------------------------------------------

    #[test]
    fn test_empty_base_empty_sides() {
        let base = tokenize(b"");
        let result = three_way_merge(&base, &base, &base);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "");
    }

    #[test]
    fn test_left_adds_to_empty_base() {
        let base = tokenize(b"");
        let left = tokenize(b"new content");
        let right = tokenize(b"");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "new content");
    }

    #[test]
    fn test_both_add_same_to_empty_base() {
        let base = tokenize(b"");
        let left = tokenize(b"same");
        let right = tokenize(b"same");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "same");
    }

    // ---- Deletion cases ---------------------------------------------------

    #[test]
    fn test_left_deletes_token() {
        // "a b c" → left removes "b ", right keeps
        let base = tokenize(b"a b c");
        let left = tokenize(b"a c");
        let right = tokenize(b"a b c");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "a c");
    }

    #[test]
    fn test_right_deletes_token() {
        let base = tokenize(b"a b c");
        let left = tokenize(b"a b c");
        let right = tokenize(b"a c");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "a c");
    }

    // ---- Insertion cases --------------------------------------------------

    #[test]
    fn test_left_inserts_token() {
        let base = tokenize(b"a c");
        let left = tokenize(b"a b c");
        let right = tokenize(b"a c");
        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "a b c");
    }

    // ---- Code-like content ------------------------------------------------

    #[test]
    fn test_code_merge_non_overlapping() {
        let base = tokenize(b"fn main() { return 0; }");
        let left = tokenize(b"fn main() { return 1; }");
        let right = tokenize(b"fn run() { return 0; }");

        let result = three_way_merge(&base, &left, &right);
        assert!(
            matches!(result, ThreeWayResult::Merged(_)),
            "expected Merged, got {:?}",
            result,
        );
        assert_eq!(merged_string(&result).unwrap(), "fn run() { return 1; }",);
    }

    #[test]
    fn test_code_merge_same_token_conflict() {
        let base = tokenize(b"fn main() { return 0; }");
        let left = tokenize(b"fn main() { return 1; }");
        let right = tokenize(b"fn main() { return 2; }");

        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Conflict));
    }

    // ---- Byte-level convenience -------------------------------------------

    #[test]
    fn test_three_way_merge_bytes() {
        let result = three_way_merge_bytes(
            b"host: localhost, port: 3000",
            b"host: production, port: 3000",
            b"host: localhost, port: 8080",
        );
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        if let ThreeWayResult::Merged(content) = &result {
            assert_eq!(
                String::from_utf8_lossy(content).as_ref(),
                "host: production, port: 8080",
            );
        }
    }

    // ---- Tokenizer smoke --------------------------------------------------

    #[test]
    fn test_tokenize_roundtrip() {
        let input = b"let x = 42;";
        let tokens = tokenize(input);
        let reassembled = reassemble_tokens(&tokens);
        assert_eq!(reassembled, input);
    }

    #[test]
    fn test_tokenize_empty() {
        let tokens = tokenize(b"");
        assert!(tokens.is_empty());
    }

    // ---- diff_tokens unit tests -------------------------------------------

    #[test]
    fn test_diff_tokens_identical() {
        let tokens = tokenize(b"hello world");
        let ops = diff_tokens(&tokens, &tokens);
        // All operations should be Keep.
        for op in &ops {
            assert!(matches!(op, EditOp::Keep(_)), "expected Keep, got {:?}", op,);
        }
    }

    #[test]
    fn test_diff_tokens_all_new() {
        let old: Vec<MergeToken> = Vec::new();
        let new = tokenize(b"hello");
        let ops = diff_tokens(&old, &new);
        assert!(!ops.is_empty());
        // Should be all inserts.
        for op in &ops {
            assert!(
                matches!(op, EditOp::Insert(_, _)),
                "expected Insert, got {:?}",
                op,
            );
        }
    }

    #[test]
    fn test_diff_tokens_all_deleted() {
        let old = tokenize(b"hello");
        let new: Vec<MergeToken> = Vec::new();
        let ops = diff_tokens(&old, &new);
        assert!(!ops.is_empty());
        for op in &ops {
            assert!(
                matches!(op, EditOp::Delete(_)),
                "expected Delete, got {:?}",
                op,
            );
        }
    }

    // ---- Property: applying only one side reproduces that side -------------

    #[test]
    fn test_left_only_reproduces_left() {
        let base = tokenize(b"a = 1, b = 2");
        let left = tokenize(b"a = 10, b = 2");
        let right = tokenize(b"a = 1, b = 2");

        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "a = 10, b = 2");
    }

    #[test]
    fn test_right_only_reproduces_right() {
        let base = tokenize(b"a = 1, b = 2");
        let left = tokenize(b"a = 1, b = 2");
        let right = tokenize(b"a = 1, b = 20");

        let result = three_way_merge(&base, &left, &right);
        assert!(matches!(result, ThreeWayResult::Merged(_)));
        assert_eq!(merged_string(&result).unwrap(), "a = 1, b = 20");
    }
}
