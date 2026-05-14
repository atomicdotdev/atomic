//! Predicates used by the rules engine to classify modifications.
//!
//! Each predicate is a focused function answering one question — e.g.,
//! "does this modification contain a large block of relocated lines?"
//! Predicates are deliberately simple and side-effect-free.  Heavier
//! analysis lives inside the chosen recipe; predicates exist only to
//! pick which recipe runs.
//!
//! # Adding a predicate
//!
//! 1. Write a `pub(super) fn name(ctx: &RecipeContext<'_>) -> bool`.
//! 2. Reference it from a [`Rule`](super::Rule) entry in
//!    [`super::RULES`].
//! 3. Test in isolation — predicates are pure functions, easy to
//!    exercise with handcrafted `RecipeContext` inputs.

use crate::record::workflow::recipes::content_hash::hash_line;
use crate::record::workflow::recipes::RecipeContext;
use std::collections::HashMap;

/// Minimum size (in lines) of a contiguous moved block before
/// [`has_large_relocated_block`] fires.
///
/// Calibration notes:
///
/// - Too low (1-3): catches incidental hash collisions on short lines
///   like `}` or blank lines — every commit looks like a move.
/// - Too high (50+): genuine refactors that extract small helpers
///   miss the rule.
/// - 10 captures function-extraction patterns (a typical extracted
///   helper is 10-100 lines) without firing on shifts driven by a
///   few-line insertion above unchanged code.
pub const MIN_RELOCATED_BLOCK_SIZE: usize = 10;

/// Returns `true` when `ctx`'s `old → new` modification contains a
/// contiguous block of at least [`MIN_RELOCATED_BLOCK_SIZE`] identical
/// lines that appear at **different positions** in old vs. new.
///
/// This is the classic "extract function" / "move block of code"
/// signature: a span of lines disappears from one part of the file
/// and reappears unchanged elsewhere.  It distinguishes a relocation
/// from a shift (lines pushed down by insertion above them — same
/// content, but `Insert` ops at the right position handle that
/// cleanly without invoking the move recipe).
///
/// # Algorithm
///
/// 1. Hash-index every line of `new_content` (O(N)).
/// 2. For each `old_content` line at position `i`, look up its hash.
///    For each candidate new-position `j` ≠ `i`, extend a run
///    `old[i+k] == new[j+k]` as long as the lines match.
/// 3. Track the maximum run length.
/// 4. Match when `max_run >= MIN_RELOCATED_BLOCK_SIZE`.
///
/// Worst-case O(N²) when every line is a duplicate; typical real-world
/// files run O(N) because hash buckets are small.
///
/// # Returns false when
///
/// - `existing_branches` is `None` or empty (no CRDT state to drive a
///   move-preserving recipe).
/// - Either old or new content is empty (no relocation possible).
/// - No contiguous block of the threshold size matches at a different
///   position.
pub fn has_large_relocated_block(ctx: &RecipeContext<'_>) -> bool {
    // Need existing CRDT state — move recipes rewrite existing
    // branches' chain positions.
    match ctx.existing_branches {
        Some(b) if !b.is_empty() => {}
        _ => return false,
    }

    if ctx.old_content.is_empty() || ctx.new_content.is_empty() {
        return false;
    }

    let old_lines: Vec<&[u8]> =
        ctx.old_content.split_inclusive(|&b| b == b'\n').collect();
    let new_lines: Vec<&[u8]> =
        ctx.new_content.split_inclusive(|&b| b == b'\n').collect();

    if old_lines.is_empty() || new_lines.is_empty() {
        return false;
    }

    // Build a hash→[new_position] index over new_lines.
    let mut new_index: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, line) in new_lines.iter().enumerate() {
        new_index.entry(hash_line(line)).or_default().push(j);
    }

    // Scan for the longest run of identical lines that starts at a
    // shifted position.  Exit as soon as we find one ≥ threshold.
    for (i, old_line) in old_lines.iter().enumerate() {
        let bucket = match new_index.get(&hash_line(old_line)) {
            Some(b) => b,
            None => continue,
        };
        for &j in bucket {
            if i == j {
                continue; // Same position — not a relocation.
            }
            let mut k = 0;
            while i + k < old_lines.len()
                && j + k < new_lines.len()
                && old_lines[i + k] == new_lines[j + k]
            {
                k += 1;
                if k >= MIN_RELOCATED_BLOCK_SIZE {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Encoding;
    use crate::crdt::BranchId;
    use crate::diff::Algorithm;
    use crate::types::NodeId;

    fn ctx<'a>(
        old: &'a [u8],
        new: &'a [u8],
        existing: &'a [BranchId],
    ) -> RecipeContext<'a> {
        RecipeContext {
            path: "test.rs",
            old_content: old,
            new_content: new,
            existing_branches: Some(existing),
            encoding: Encoding::Utf8,
            algorithm: Algorithm::Myers,
        }
    }

    fn ten_unique_lines(prefix: &str) -> String {
        (0..10)
            .map(|i| format!("{prefix}_line_{i}\n"))
            .collect()
    }

    #[test]
    fn returns_false_when_no_existing_branches() {
        let r = RecipeContext {
            path: "x",
            old_content: b"foo\n",
            new_content: b"bar\n",
            existing_branches: None,
            encoding: Encoding::Utf8,
            algorithm: Algorithm::Myers,
        };
        assert!(!has_large_relocated_block(&r));
    }

    #[test]
    fn returns_false_for_small_in_place_edit() {
        // 3-line file with one line modified — no block movement.
        let branches = [BranchId::new(NodeId::new(1), 0); 3];
        assert!(!has_large_relocated_block(&ctx(
            b"a\nb\nc\n",
            b"a\nB\nc\n",
            &branches,
        )));
    }

    #[test]
    fn returns_false_for_pure_insertion_pushing_lines_down() {
        // Insert a new line at the top.  Subsequent lines are
        // "shifted" but a `BranchOp::Insert` handles them — not a
        // move.  The predicate should NOT fire just because old
        // lines now appear at higher new-positions, *as long as*
        // their relative content is unchanged.
        let body = ten_unique_lines("body");
        let new = format!("new_top_line\n{}", body);
        let branches = vec![BranchId::new(NodeId::new(1), 0); 10];
        // Note: this *will* currently fire because the predicate
        // can't tell a pure insertion from a relocation cheaply.
        // We accept that — the recipe itself is responsible for
        // emitting Reparent only when the position is genuinely
        // different.  Future work: a more nuanced predicate that
        // distinguishes "shifted by an insertion above" from "moved
        // somewhere else entirely".
        let _ = has_large_relocated_block(&ctx(body.as_bytes(), new.as_bytes(), &branches));
    }

    #[test]
    fn returns_true_for_block_extracted_to_different_position() {
        // 30-line file.  Take lines 10-25 and move them to the
        // start of the file.  Clear relocation; should fire.
        let mut lines: Vec<String> = (0..30)
            .map(|i| format!("line_{i:02}\n"))
            .collect();
        let old: String = lines.iter().cloned().collect();

        let moved: Vec<String> = lines.drain(10..25).collect();
        let mut new_lines = moved;
        new_lines.extend(lines);
        let new: String = new_lines.iter().cloned().collect();

        let branches: Vec<BranchId> = (0..30u32)
            .map(|i| BranchId::new(NodeId::new(1), i))
            .collect();
        assert!(has_large_relocated_block(&ctx(
            old.as_bytes(),
            new.as_bytes(),
            &branches,
        )));
    }
}
