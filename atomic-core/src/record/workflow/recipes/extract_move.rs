//! `ExtractMove` recipe — content-hash-driven move detection.
//!
//! When the diff between `old_content` and `new_content` includes line
//! moves (extract function, code reorder, block relocation), pure
//! line-level diff sees a big `Delete` block in one region and a big
//! `Insert` block in another.  The naïve mapping is `Delete + Insert`
//! pairs, which tombstones the original branch and creates a fresh one
//! at the new position — losing blame continuity and inflating the
//! chain unnecessarily.
//!
//! `ExtractMove` recognizes moves by hashing each new line and looking
//! it up against the old lines' hashes.  When a match is found at a
//! *different* position, the recipe emits a [`BranchOp::Reparent`]
//! instead — keeping the existing branch alive, with the same content
//! and identity, but at the new chain position.
//!
//! # Operations emitted
//!
//! For each new line:
//!
//! | Outcome | Op | Why |
//! |---|---|---|
//! | Line was at the same position in old, same content | (none — Equal) | No-op |
//! | Line was elsewhere in old (`pos_old != pos_new`), same content | `Reparent` | Move |
//! | Line is new content | `Insert` (with `after = prev_emitted`) | Net new line |
//!
//! For each old line not claimed by any new line: `Delete`.
//!
//! # Chain coherence
//!
//! Per the contract in `crdt::queries::tests::move_via_paired_reparents_…`,
//! moving a branch requires *paired* Reparents — the moved branch gets
//! its new predecessor, and any branch that pointed at the moved one
//! gets repointed at the moved one's old predecessor.  This recipe
//! emits Reparents in walk order so successor relationships stay
//! coherent: each emission becomes the `prev_emitted` for the next, so
//! the new chain is built in order regardless of where the branches
//! came from in the old chain.

use super::content_hash::LineHashIndex;
use super::RecipeContext;
use crate::change::{Encoding, FileOps};
use crate::crdt::{BranchId, BranchOp, LeafOp, TrunkId};
use crate::record::workflow::crdt::{
    CrdtBuildStats, FileOps as BuilderFileOps, LineOps as BuilderLineOps,
};
use crate::types::NodeId;

/// Build CRDT ops using move-aware matching.
pub fn build_ops(ctx: &RecipeContext<'_>) -> (FileOps, CrdtBuildStats) {
    // Fall back to the in-place recipe when we have no CRDT state to
    // match against.  Pure move detection needs the existing branches.
    let existing_branches = match ctx.existing_branches {
        Some(b) if !b.is_empty() => b,
        _ => return super::in_place_edit::build_ops(ctx),
    };

    let _placeholder_change = NodeId::new(0);
    let placeholder_trunk = TrunkId::new(NodeId::new(0), 0);

    // Tokenize.
    let old_lines: Vec<&[u8]> = ctx.old_content.split_inclusive(|&b| b == b'\n').collect();
    let new_lines: Vec<&[u8]> = ctx.new_content.split_inclusive(|&b| b == b'\n').collect();

    // Index old lines by content hash so we can find moves in O(N).
    let mut old_idx = LineHashIndex::from_lines(&old_lines);
    // Track which old positions got consumed (for the Delete sweep).
    let mut matched_old: Vec<bool> = vec![false; old_lines.len()];

    // For each new line, find an old position with matching content.
    // `new_to_old[i] = Some(j)` means new_line[i] was old_line[j].
    let mut new_to_old: Vec<Option<usize>> = Vec::with_capacity(new_lines.len());
    for new_line in &new_lines {
        match old_idx.consume(new_line) {
            Some(oi) => {
                matched_old[oi] = true;
                new_to_old.push(Some(oi));
            }
            None => new_to_old.push(None),
        }
    }

    // Build line ops.
    let mut next_branch_idx: u32 = 0;
    let mut alloc_branch = || {
        let id = BranchId::new(NodeId::new(0), next_branch_idx);
        next_branch_idx += 1;
        id
    };
    let mut next_leaf_idx: u32 = 0;
    let mut alloc_leaf = || {
        let id = crate::crdt::LeafId::new(NodeId::new(0), next_leaf_idx);
        next_leaf_idx += 1;
        id
    };

    let mut file_ops = BuilderFileOps::new(placeholder_trunk, ctx.path.to_string(), None);
    let mut stats = CrdtBuildStats::new();
    let mut prev_emitted: Option<BranchId> = None;

    for (new_idx, &maybe_old) in new_to_old.iter().enumerate() {
        match maybe_old {
            Some(old_idx) if old_idx < existing_branches.len() => {
                let branch_id = existing_branches[old_idx];

                // Decide between "Equal" (no op) and "Reparent" (move).
                //
                // Position parity is the trigger: if the matched old
                // and new line indices are the same AND the previous
                // emitted branch is the same as this branch's natural
                // predecessor in old-content order, the line hasn't
                // moved — emit nothing.
                //
                // Otherwise, the branch needs its `after` pointer
                // rewritten to the new chain predecessor.  This is
                // also the right shape for "shifted by an insertion"
                // — when a block of new lines gets emitted as Insert
                // ops above the matched branch, the matched branch's
                // `prev_emitted` becomes the last of those Inserts,
                // which is a different predecessor than it had before.
                let position_unchanged = old_idx == new_idx
                    && prev_emitted
                        == old_idx
                            .checked_sub(1)
                            .and_then(|i| existing_branches.get(i).copied());

                if position_unchanged {
                    // Equal block — no op needed.  Just advance the
                    // chain marker for downstream ops.
                } else {
                    let reparent_op = BuilderLineOps::new(
                        branch_id,
                        BranchOp::Reparent {
                            branch: branch_id,
                            new_after: prev_emitted,
                        },
                    )
                    .with_old_line_num(old_idx + 1)
                    .with_new_line_num(new_idx + 1);
                    file_ops.add_line_op(reparent_op);
                    stats.lines_modified += 1;
                }
                prev_emitted = Some(branch_id);
            }
            _ => {
                // No old-content match: net new line.
                let new_line = new_lines.get(new_idx).copied().unwrap_or(&[]);
                let leaf_ops = build_leaf_ops_for_line(new_line, ctx.encoding, &mut alloc_leaf);
                let branch_id = alloc_branch();
                let insert_op = BuilderLineOps::insert(branch_id, prev_emitted, leaf_ops)
                    .with_new_line_num(new_idx + 1);
                file_ops.add_line_op(insert_op);
                stats.lines_added += 1;
                prev_emitted = Some(branch_id);
            }
        }
    }

    // Delete sweep: any old position not matched is a deletion.
    for (old_idx, &was_matched) in matched_old.iter().enumerate() {
        if was_matched {
            continue;
        }
        let branch_id = existing_branches
            .get(old_idx)
            .copied()
            .unwrap_or_else(&mut alloc_branch);
        let old_line = old_lines.get(old_idx).copied().unwrap_or(&[]);
        let content_for_diff = build_leaf_ops_for_line(old_line, ctx.encoding, &mut alloc_leaf);
        let delete_op =
            BuilderLineOps::delete(branch_id, content_for_diff).with_old_line_num(old_idx + 1);
        file_ops.add_line_op(delete_op);
        stats.lines_deleted += 1;
    }

    (file_ops.into_change_ops(), stats)
}

/// Build a sequence of `LeafOp::Insert` ops representing the tokens of
/// `line`.  Mirrors the leaf construction used by `InPlaceEdit` for
/// `Insert`/`Modify` ops.
fn build_leaf_ops_for_line(
    line: &[u8],
    _encoding: Encoding,
    alloc_leaf: &mut dyn FnMut() -> crate::crdt::LeafId,
) -> Vec<LeafOp> {
    use crate::record::workflow::crdt::ContentTokenizer;
    let tokenizer = ContentTokenizer::new(line);
    let mut leaf_ops = Vec::new();
    let mut prev_leaf: Option<crate::crdt::LeafId> = None;
    for tokenized_line in tokenizer.lines() {
        for token in tokenized_line.tokens() {
            let leaf_id = alloc_leaf();
            leaf_ops.push(LeafOp::Insert {
                after: prev_leaf,
                kind: token.kind(),
                content: token.content().to_vec(),
            });
            prev_leaf = Some(leaf_id);
        }
    }
    leaf_ops
}

// Recipe selection lives in the rules engine
// (super::detector::RULES + super::detector::predicates).  This
// recipe is invoked only when a rule predicate (currently
// `has_large_relocated_block`) matches the context.
