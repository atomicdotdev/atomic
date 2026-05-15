//! Per-`DiffOp` translation rules.
//!
//! The recipe-selection layer (`super::detector::RULES`) decides
//! *which recipe* runs.  Inside a recipe, the rules in this module
//! decide *how each `DiffOp` becomes one or more CRDT `BranchOp`s*.
//!
//! Both layers share the same shape: a named predicate is paired with
//! an action.  First-match-wins.  Auditable via `log::trace!`.
//!
//! # Why a separate layer?
//!
//! Diff translation has many edge cases that look like one-liners:
//! "Equal at same position → no-op", "Equal at different position →
//! Reparent", "Replace with 1:1 sizes → Modify", "Replace with mixed
//! sizes → within-block pairing", "Insert/Delete → straightforward".
//! Inlining them as nested `if`/`match` blocks in one big function
//! makes it impossible to point at any single rule and ask "why does
//! this fire?".  Lifting them into a rule table makes each rule
//! a single named, testable function.
//!
//! # Adding a rule
//!
//! 1. Write a `match_*` predicate and an `apply_*` action in this
//!    module.
//! 2. Append a [`DiffOpRule`] entry to [`RULES`].
//! 3. Order matters — more-specific rules go first.
//! 4. Add a unit test exercising the rule's match condition and the
//!    op stream it emits.

use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId};
use crate::diff::DiffOp;
use crate::record::workflow::crdt::{
    ContentTokenizer, FileOps as BuilderFileOps, LineOps as BuilderLineOps,
};
use crate::types::NodeId;

/// Side-effect state threaded through every rule's `apply` call.
///
/// Rules consume the current `prev_branch` to know what to chain their
/// emitted ops after, and update it for the next rule.  They emit any
/// `BuilderLineOps` they produce into `emitted`.
///
/// Allocation of fresh placeholder IDs is **not** in the context —
/// the rules currently registered here (Equal-relocated, Equal-in-place)
/// only emit Reparents on existing branches, so they don't allocate.
/// Future rules that need allocation should plumb allocators through
/// without taking exclusive borrows on counters the surrounding recipe
/// also borrows.
pub struct DiffOpContext<'a> {
    pub existing_branches: Option<&'a [BranchId]>,
    pub old_lines: &'a [Line<'a>],
    pub new_lines: &'a [Line<'a>],
    pub encoding: Encoding,
    pub placeholder_change: NodeId,
    pub prev_branch: Option<BranchId>,
    pub emitted: Vec<BuilderLineOps>,
}

/// Tokenized representation of a single line of content.  Stored as a
/// thin wrapper so rules can iterate tokens without re-running the
/// tokenizer.
pub struct Line<'a> {
    pub bytes: &'a [u8],
}

impl<'a> Line<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Convert the line's tokens into `LeafOp::Insert` ops, allocating
    /// fresh leaf IDs via `alloc_leaf`.
    pub fn to_leaf_ops(&self, next_leaf_idx: &mut u32, placeholder_change: NodeId) -> Vec<LeafOp> {
        let tokenizer = ContentTokenizer::new(self.bytes);
        let mut leaf_ops = Vec::new();
        let mut prev_leaf: Option<LeafId> = None;
        for tokenized in tokenizer.lines() {
            for token in tokenized.tokens() {
                let leaf_id = LeafId::new(placeholder_change, *next_leaf_idx);
                *next_leaf_idx += 1;
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
}

/// One translation rule.
///
/// `matches` is a cheap predicate over the diff op + context.  `apply`
/// emits the corresponding `BuilderLineOps` and updates `ctx`
/// (in particular `prev_branch` and `emitted`).
pub struct DiffOpRule {
    pub name: &'static str,
    pub matches: fn(&DiffOp, &DiffOpContext<'_>) -> bool,
    pub apply: fn(&DiffOp, &mut DiffOpContext<'_>),
}

/// The ordered rule table.  First match wins.
///
/// Ordering invariant: rules that match a strict subset of another
/// rule's matching set go first, so the more-specific rule wins on
/// overlap.
pub const RULES: &[DiffOpRule] = &[
    // Equal block at different old/new positions: a Myers-detected
    // line relocation.  Each line gets a Reparent op so the existing
    // branch lands at its new chain position; otherwise the walker
    // emits stale bytes from the branch's pre-move state.
    //
    // Must come BEFORE `equal_at_same_positions` — that rule
    // matches *every* Equal block and would shadow this one.
    DiffOpRule {
        name: "equal/relocated/emit_reparent",
        matches: matchers::equal_at_different_positions,
        apply: appliers::emit_reparents_for_equal_block,
    },
    // Equal block at the same old/new positions: no CRDT op needed,
    // but advance `prev_branch` so subsequent ops chain correctly.
    DiffOpRule {
        name: "equal/in_place/advance_prev",
        matches: matchers::equal_at_same_positions,
        apply: appliers::advance_prev_branch_through_equal,
    },
    // Insert/Delete/Replace are still handled by the legacy inline
    // logic in build_crdt_ops_for_modified_file — their allocation
    // requirements don't yet fit this context shape.  Migrating them
    // here is task-tracked separately; for now `dispatch` returns
    // `false` and the caller falls back.
];

/// Predicate functions.  Pure, side-effect-free.
pub mod matchers {
    use super::*;

    pub fn equal_at_same_positions(op: &DiffOp, _ctx: &DiffOpContext<'_>) -> bool {
        matches!(op, DiffOp::Equal { old_pos, new_pos, .. } if old_pos == new_pos)
    }

    pub fn equal_at_different_positions(op: &DiffOp, _ctx: &DiffOpContext<'_>) -> bool {
        matches!(op, DiffOp::Equal { old_pos, new_pos, .. } if old_pos != new_pos)
    }

    pub fn is_insert(op: &DiffOp, _ctx: &DiffOpContext<'_>) -> bool {
        matches!(op, DiffOp::Insert { .. })
    }

    pub fn is_delete(op: &DiffOp, _ctx: &DiffOpContext<'_>) -> bool {
        matches!(op, DiffOp::Delete { .. })
    }
}

/// Action functions.  Mutate `ctx` (emit ops, advance `prev_branch`).
pub mod appliers {
    use super::*;

    /// Advance through an in-place Equal block, emitting `Reparent`
    /// only when the branch's predecessor in the *new* chain differs
    /// from its predecessor in the old chain.
    ///
    /// This is load-bearing after inserts/moves above an unchanged
    /// suffix: the lines themselves are byte-identical, but their
    /// `BRANCH_AFTER` rows must still be rewritten so the file-order
    /// walk follows the new predecessor chain instead of the stale one.
    pub fn advance_prev_branch_through_equal(op: &DiffOp, ctx: &mut DiffOpContext<'_>) {
        if let DiffOp::Equal { old_pos, len, .. } = op {
            if let Some(existing) = ctx.existing_branches {
                for i in 0..*len {
                    let idx = old_pos + i;
                    if idx >= existing.len() {
                        continue;
                    }
                    let branch_id = existing[idx];
                    let natural_prev = idx.checked_sub(1).and_then(|j| existing.get(j).copied());
                    if ctx.prev_branch != natural_prev {
                        let reparent = BuilderLineOps::new(
                            branch_id,
                            BranchOp::Reparent {
                                branch: branch_id,
                                new_after: ctx.prev_branch,
                            },
                        )
                        .with_old_line_num(idx + 1)
                        .with_new_line_num(idx + 1);
                        ctx.emitted.push(reparent);
                    }
                    ctx.prev_branch = Some(branch_id);
                }
            }
        }
    }

    /// Emit a `BranchOp::Reparent` for each line of the Equal block.
    /// Each branch gets its `after` ref rewritten to the chain's current
    /// `prev_branch`, which the previous rule(s) have set to whatever
    /// new-content branch precedes this block.
    ///
    /// This is the load-bearing rule for the extract-function pattern
    /// (RCA §11.8) — Myers identifies that a line exists in both old and
    /// new at *different* positions, and without a Reparent the existing
    /// branch stays at its old chain position with stale content.
    pub fn emit_reparents_for_equal_block(op: &DiffOp, ctx: &mut DiffOpContext<'_>) {
        if let DiffOp::Equal {
            old_pos,
            new_pos,
            len,
        } = op
        {
            let existing = match ctx.existing_branches {
                Some(b) => b,
                None => return,
            };
            for i in 0..*len {
                let old_idx = old_pos + i;
                let new_idx = new_pos + i;
                if old_idx >= existing.len() {
                    continue;
                }
                let branch_id = existing[old_idx];
                let reparent = BuilderLineOps::new(
                    branch_id,
                    BranchOp::Reparent {
                        branch: branch_id,
                        new_after: ctx.prev_branch,
                    },
                )
                .with_old_line_num(old_idx + 1)
                .with_new_line_num(new_idx + 1);
                ctx.emitted.push(reparent);
                ctx.prev_branch = Some(branch_id);
            }
        }
    }
}

/// Dispatch a single `DiffOp` through the rule table.
///
/// Walks `RULES` top-to-bottom; the first `matches` predicate that
/// returns `true` selects the rule whose `apply` is invoked.  When no
/// rule matches (e.g., for `DiffOp::Replace`, which the inline recipe
/// still handles), returns `false` so the caller knows it must fall
/// back to its own translation logic.
///
/// Returns `true` when a rule fired.
pub fn dispatch(op: &DiffOp, ctx: &mut DiffOpContext<'_>) -> bool {
    for rule in RULES {
        if (rule.matches)(op, ctx) {
            log::trace!("diff_op_rules::dispatch: matched {}", rule.name);
            (rule.apply)(op, ctx);
            return true;
        }
    }
    log::trace!("diff_op_rules::dispatch: no rule matched, caller handles");
    false
}

// Post-pass rules

/// Post-pass: collected ops contain `BranchOp::Delete` of branch B *and*
/// a `BranchOp::Insert` whose leaf content is byte-identical to B's
/// deleted content.  Promote the pair into a `BranchOp::Reparent` of B
/// to the Insert's chain position.
///
/// This is the **load-bearing** rule for Myers-style extract-function
/// refactors: Myers emits a Delete at the old location and a separate
/// Insert at the new location, leaving us no chance to recognize the
/// move from a single DiffOp.  The post-pass scans the *whole* op
/// stream after diff translation and pairs them up by exact content
/// match.
///
/// Exact-content matching (not similarity scoring) avoids the
/// false-positive trap of "shifted lines look like moved lines": pure
/// insertions/deletions don't change the chain ordering, so a content
/// match at a different chain position truly *is* a move.
///
/// # Algorithm
///
/// 1. Index all `Delete` ops by their content hash (FNV-1a).
/// 2. Scan all `Insert` ops.  For each, look up the hash bucket.
/// 3. If a matching Delete exists with byte-identical content, mark
///    both as a "promote to Reparent" pair.
/// 4. Replace the Insert with a `Reparent` of the Delete's branch
///    pointing at the Insert's `after` ref; drop the Delete.
///
/// # Returns
///
/// Number of pairs promoted, for stats / logging.
pub fn promote_delete_insert_pairs_to_reparents(line_ops: &mut Vec<BuilderLineOps>) -> usize {
    use super::content_hash::hash_line;
    use std::collections::HashMap;

    /// Minimum content length (in non-whitespace bytes) for a
    /// Delete/Insert pair to be considered a move candidate.
    ///
    /// Below this threshold, content collisions are dominated by
    /// non-distinctive lines like blank lines, `}` / `{`, single
    /// keywords, etc.  Promoting those produces false-positive moves
    /// that surface as content mismatches in unrelated commits.
    const MIN_DISTINCTIVE_BYTES: usize = 12;

    /// Decide whether `bytes` is "distinctive enough" to anchor a
    /// move-detection pair.  We require at least `MIN_DISTINCTIVE_BYTES`
    /// non-whitespace bytes — that filters blank lines, single-brace
    /// lines, and short keyword lines without disqualifying genuine
    /// statement lines.
    fn is_distinctive(bytes: &[u8]) -> bool {
        bytes.iter().filter(|b| !b.is_ascii_whitespace()).count() >= MIN_DISTINCTIVE_BYTES
    }

    // Build a hash → index map of Delete ops with content.
    // Multiple Deletes with same content go in the bucket; we consume
    // one at a time so identical-line moves pair 1:1.
    let mut delete_idx: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, op) in line_ops.iter().enumerate() {
        if let BranchOp::Delete { content, .. } = op.operation() {
            if content.is_empty() {
                continue;
            }
            let bytes = leaf_ops_to_bytes(content);
            if !is_distinctive(&bytes) {
                continue;
            }
            delete_idx.entry(hash_line(&bytes)).or_default().push(i);
        }
    }

    // Walk Inserts, look for matching Deletes.
    let mut promote: HashMap<usize, usize> = HashMap::new(); // insert_idx → delete_idx
    let mut consumed_deletes: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for (i, op) in line_ops.iter().enumerate() {
        if let BranchOp::Insert { content, .. } = op.operation() {
            if content.is_empty() {
                continue;
            }
            let bytes = leaf_ops_to_bytes(content);
            if !is_distinctive(&bytes) {
                continue;
            }
            let h = hash_line(&bytes);
            if let Some(candidates) = delete_idx.get(&h) {
                for &delete_idx_pos in candidates {
                    if consumed_deletes.contains(&delete_idx_pos) {
                        continue;
                    }
                    // Verify byte-identical (not just hash collision).
                    if let BranchOp::Delete {
                        content: del_content,
                        ..
                    } = line_ops[delete_idx_pos].operation()
                    {
                        if leaf_ops_to_bytes(del_content) == bytes {
                            promote.insert(i, delete_idx_pos);
                            consumed_deletes.insert(delete_idx_pos);
                            break;
                        }
                    }
                }
            }
        }
    }

    let promote_count = promote.len();
    if promote_count == 0 {
        return 0;
    }

    // Promoted Inserts disappear from the op stream, so any later op whose
    // `after` / `new_after` points at that placeholder branch would become
    // unreachable unless we rewrite it to the surviving existing branch.
    let placeholder_to_existing: HashMap<BranchId, BranchId> = promote
        .iter()
        .map(|(&insert_idx, &delete_idx_pos)| {
            (
                line_ops[insert_idx].branch_id(),
                line_ops[delete_idx_pos].branch_id(),
            )
        })
        .collect();

    let rewrite_ref = |r: &mut Option<BranchId>| {
        if let Some(branch_id) = *r {
            if let Some(replacement) = placeholder_to_existing.get(&branch_id) {
                *r = Some(*replacement);
            }
        }
    };

    let rewrite_after_refs = |op: &mut BuilderLineOps| match op.operation_mut() {
        BranchOp::Insert { after, .. } => rewrite_ref(after),
        BranchOp::Reparent { new_after, .. } => rewrite_ref(new_after),
        _ => {}
    };

    // Apply: walk the ops, replace Inserts with Reparents, skip Deletes.
    let mut rebuilt: Vec<BuilderLineOps> = Vec::with_capacity(line_ops.len() - promote_count);
    for (i, op) in line_ops.iter().enumerate() {
        if consumed_deletes.contains(&i) {
            // This Delete was paired — its branch is being kept via Reparent.
            continue;
        }
        if let Some(&del_pos) = promote.get(&i) {
            // Promote this Insert to a Reparent of the matched Delete's branch.
            let mut new_after = match op.operation() {
                BranchOp::Insert { after, .. } => *after,
                _ => None,
            };
            rewrite_ref(&mut new_after);
            let branch_id = line_ops[del_pos].branch_id();
            let reparent = BuilderLineOps::new(
                branch_id,
                BranchOp::Reparent {
                    branch: branch_id,
                    new_after,
                },
            );
            // Carry over the Insert's new_line_num for diagnostics.
            let reparent = if let Some(n) = op.new_line_num() {
                reparent.with_new_line_num(n)
            } else {
                reparent
            };
            rebuilt.push(reparent);
        } else {
            let mut op = op.clone();
            rewrite_after_refs(&mut op);
            rebuilt.push(op);
        }
    }

    *line_ops = rebuilt;
    log::trace!(
        "diff_op_rules::promote_delete_insert_pairs_to_reparents: promoted {} pairs",
        promote_count
    );
    promote_count
}

/// Flatten a sequence of `LeafOp` into the bytes they collectively
/// represent.  Used to compare line content for the move-detection
/// post-pass.
fn leaf_ops_to_bytes(leaves: &[LeafOp]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for leaf in leaves {
        if let LeafOp::Insert { content, .. } = leaf {
            bytes.extend_from_slice(content);
        }
    }
    bytes
}

/// Build the placeholder file_ops container the recipe assembles ops
/// into.  Kept here so callers don't need to import the builder types.
pub fn new_file_ops(path: &str, placeholder_change: NodeId) -> BuilderFileOps {
    BuilderFileOps::new(TrunkId::new(placeholder_change, 0), path.to_string(), None)
}
