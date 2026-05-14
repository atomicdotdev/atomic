//! Derived queries over the CRDT tables.
//!
//! These helpers compose the primitive accessors on `CrdtTxnT` into
//! higher-level reads.  They live here (not on the trait) because they're
//! pure compositions of `iter_trunk_branches`, `get_crdt_branch`, and
//! `get_crdt_branch_after`.
//!
//! Anything that needs `&impl CrdtTxnT` accepts both `ReadTxn` and
//! `WriteTxn`, so callers don't need write access just to inspect CRDT
//! state.

use crate::crdt::tables::{decode_branch_id, encode_branch_id, encode_trunk_id};
use crate::crdt::{BranchId, TrunkId};
use crate::pristine::{CrdtTxnT, PristineResult};
use std::collections::BTreeMap;

/// The all-zero `[u8; 12]` value used in `BRANCH_AFTER` to mean
/// "this branch was inserted at the start of the file".
const FILE_START_SENTINEL: [u8; 12] = [0u8; 12];

/// Returns this trunk's branches in **file order** — top-of-file first.
///
/// Iteration over `TRUNK_BRANCHES` alone returns branches in
/// `(change_id, branch_idx)` sort order, which is fine for stable identity
/// but wrong for presentation: a later commit's prepended branch would
/// sort *after* all of the original commit's branches.
///
/// This function walks the `BRANCH_AFTER` chain to recover file order:
///
/// 1. Collect every branch that belongs to this trunk.
/// 2. Build a `predecessor → [successors]` map.
/// 3. Walk from the file-start sentinel, yielding each branch in turn.
/// 4. When multiple successors share the same predecessor (concurrent
///    inserts), tie-break by natural `BranchId` order — same rule as
///    [`crate::crdt::apply::order::find_insert_position`].
///
/// Branches missing a `BRANCH_AFTER` entry (e.g., inserted by an older
/// apply path) fall back to their `BranchId` sort position relative to
/// other un-recorded siblings.
///
/// Deleted branches are included; callers that want to skip them should
/// filter on `get_crdt_branch(...)?.state`.
pub fn iter_trunk_branches_in_file_order<T: CrdtTxnT + ?Sized>(
    txn: &T,
    trunk_id: TrunkId,
) -> PristineResult<Vec<BranchId>> {
    let trunk_key = encode_trunk_id(&trunk_id);

    // 1. Collect every branch belonging to this trunk.
    let all_branches: Vec<BranchId> = txn
        .iter_trunk_branches(&trunk_key)?
        .map(|r| r.map(|bytes| decode_branch_id(&bytes)))
        .collect::<Result<Vec<_>, _>>()?;

    if all_branches.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Look up the after-reference for each branch.
    // BTreeMap orders successors by BranchId — the deterministic tie-break.
    let mut successors: BTreeMap<[u8; 12], Vec<BranchId>> = BTreeMap::new();
    let mut orphans: Vec<BranchId> = Vec::new();

    for branch_id in &all_branches {
        let branch_key = encode_branch_id(branch_id);
        match txn.get_crdt_branch_after(&branch_key)? {
            Some(after_key) => {
                successors.entry(after_key).or_default().push(*branch_id);
            }
            None => {
                // Branch predates BRANCH_AFTER population — treat as starting
                // at file start and sort by BranchId.
                orphans.push(*branch_id);
            }
        }
    }

    // 3. Sort sibling lists by BranchId.  Tie-break **descending** so that
    //    when two branches share the same `after` reference, the *later*
    //    commit's branch comes first.
    //
    //    For sequential history (the common case: one commit applied after
    //    another), this respects user intent — if commit C2 inserts a line
    //    at a position already occupied by a C1 line, C2 happened *after*
    //    seeing C1, so its insert was meant to go *before* C1's line at the
    //    same anchor.  Ascending tie-break would put C2 after C1, which is
    //    backwards for prepend-like operations.
    //
    //    For genuinely concurrent merges, either order is acceptable as long
    //    as it's deterministic — Yjs picks ascending, Automerge picks
    //    descending; we follow Automerge's choice.
    for siblings in successors.values_mut() {
        siblings.sort_by(|a, b| b.cmp(a));
    }
    orphans.sort_by(|a, b| b.cmp(a));

    // 4. Walk the after-chain starting from the file-start sentinel.
    let mut ordered: Vec<BranchId> = Vec::with_capacity(all_branches.len());
    let mut stack: Vec<BranchId> = Vec::new();

    // Seed with file-start successors (in reverse so the smallest pops first).
    if let Some(starts) = successors.remove(&FILE_START_SENTINEL) {
        for id in starts.into_iter().rev() {
            stack.push(id);
        }
    }
    // Plus any orphans (un-recorded after-refs).  Treat them as file-start
    // siblings — sorted by BranchId — appended after the explicit starts.
    for id in orphans.into_iter().rev() {
        stack.push(id);
    }

    while let Some(current) = stack.pop() {
        ordered.push(current);
        let current_key = encode_branch_id(&current);
        if let Some(children) = successors.remove(&current_key) {
            for id in children.into_iter().rev() {
                stack.push(id);
            }
        }
    }

    // Any branches we never reached (their predecessor chain is broken)
    // get appended in BranchId order so they aren't silently dropped.
    if ordered.len() < all_branches.len() {
        let seen: std::collections::BTreeSet<BranchId> = ordered.iter().copied().collect();
        let mut leftover: Vec<BranchId> = all_branches
            .iter()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        leftover.sort();
        ordered.extend(leftover);
    }

    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::tables::{encode_branch_value, SerializedBranch};
    use crate::crdt::{BranchState, TrunkId};
    use crate::pristine::{MutTxnT, Pristine};
    use crate::types::NodeId;
    use tempfile::tempdir;

    fn open_pristine() -> (tempfile::TempDir, Pristine) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();
        (dir, pristine)
    }

    fn branch(change: u64, idx: u32) -> BranchId {
        BranchId::new(NodeId::new(change), idx)
    }

    fn put_branch(
        txn: &mut impl crate::pristine::MutTxnT,
        trunk_id: TrunkId,
        b: BranchId,
        after: Option<BranchId>,
    ) {
        let trunk_key = encode_trunk_id(&trunk_id);
        let bkey = encode_branch_id(&b);
        let serialized = SerializedBranch {
            trunk_id,
            state: BranchState::Alive,
            line_hash: 0,
        };
        txn.put_crdt_branch(&bkey, &encode_branch_value(&serialized)).unwrap();
        txn.put_crdt_trunk_branch(&trunk_key, &bkey).unwrap();
        let after_key = match after {
            Some(a) => encode_branch_id(&a),
            None => [0u8; 12],
        };
        txn.put_crdt_branch_after(&bkey, &after_key).unwrap();
    }

    #[test]
    fn linear_chain_preserves_insertion_order() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);

        // Three lines inserted top-to-bottom in a single change.
        let b0 = branch(1, 0);
        let b1 = branch(1, 1);
        let b2 = branch(1, 2);
        put_branch(&mut txn, trunk, b0, None);
        put_branch(&mut txn, trunk, b1, Some(b0));
        put_branch(&mut txn, trunk, b2, Some(b1));

        let order = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
        assert_eq!(order, vec![b0, b1, b2]);
    }

    #[test]
    fn later_change_prepends_at_top_of_file() {
        // Load-bearing case: when commit 2 prepends a line ahead of all of
        // commit 1's existing lines, the prepended line must appear first.
        //
        // Both commit 2's new branch and commit 1's first branch share
        // `after = None`, so they're siblings.  Descending BranchId tie-
        // break puts the later commit first — matching the user's intent
        // for sequential history.
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);

        // Commit 1: two lines.
        let c1_b0 = branch(1, 0);
        let c1_b1 = branch(1, 1);
        put_branch(&mut txn, trunk, c1_b0, None);
        put_branch(&mut txn, trunk, c1_b1, Some(c1_b0));

        // Commit 2: one line prepended at the start.
        let c2_b0 = branch(2, 0);
        put_branch(&mut txn, trunk, c2_b0, None);

        let order = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
        // Descending tie-break: c2_b0 (later commit) first, then walk its
        // (empty) successor chain, then back to root siblings: c1_b0,
        // then c1_b1 chained off c1_b0.
        assert_eq!(order, vec![c2_b0, c1_b0, c1_b1]);
    }

    #[test]
    fn concurrent_inserts_after_same_ref_sort_by_branchid() {
        // Two siblings sharing `after = anchor`.  Tie-break is descending
        // by BranchId, so the larger one comes first.
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);

        let anchor = branch(1, 0);
        put_branch(&mut txn, trunk, anchor, None);

        let later = branch(3, 0);
        let earlier = branch(2, 0);
        put_branch(&mut txn, trunk, later, Some(anchor));
        put_branch(&mut txn, trunk, earlier, Some(anchor));

        let order = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
        assert_eq!(order, vec![anchor, later, earlier]);
    }

    #[test]
    fn branches_without_after_record_fall_back_to_branchid_order() {
        // Backfill scenario: branches without BRANCH_AFTER rows fall back
        // to BranchId order — descending, matching the new tie-break.
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);
        let trunk_key = encode_trunk_id(&trunk);

        let b0 = branch(1, 0);
        let b1 = branch(1, 1);
        for b in [b0, b1] {
            let bkey = encode_branch_id(&b);
            let serialized = SerializedBranch {
                trunk_id: trunk,
                state: BranchState::Alive,
                line_hash: 0,
            };
            txn.put_crdt_branch(&bkey, &encode_branch_value(&serialized)).unwrap();
            txn.put_crdt_trunk_branch(&trunk_key, &bkey).unwrap();
        }

        let order = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
        assert_eq!(order, vec![b1, b0]);
    }

    #[test]
    fn reparent_writes_branch_after_in_isolation() {
        // The Reparent op's storage effect is a single overwrite of
        // BRANCH_AFTER for the moved branch.  This test pins that
        // behavior at the apply level.
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);

        let a = branch(1, 0);
        let b = branch(1, 1);
        put_branch(&mut txn, trunk, a, None);
        put_branch(&mut txn, trunk, b, Some(a));

        // Reparent b to the file-start sentinel.
        txn.put_crdt_branch_after(&encode_branch_id(&b), &[0u8; 12]).unwrap();

        let after = txn.get_crdt_branch_after(&encode_branch_id(&b)).unwrap();
        assert_eq!(after, Some([0u8; 12]),
                   "Reparent must rewrite BRANCH_AFTER for the moved branch");
    }

    #[test]
    fn move_via_paired_reparents_produces_coherent_chain() {
        // Moving a branch in our after-chain CRDT requires the recipe
        // to emit *paired* Reparents: one for the moved branch (to its
        // new predecessor), one for any branch that was pointing at the
        // moved branch (to repoint at the moved branch's old predecessor).
        //
        // Without that pairing, the chain has a dangling successor
        // reference and the walker falls back to BranchId-sort leftover
        // cleanup.  This test pins the *expected* recipe behavior so
        // ExtractMove (Phase 3) has a contract to satisfy.
        //
        // Build: a → b → c → d.
        // Goal: move c to the front, after a, displacing b.
        // Required Reparents:
        //   c.after = a  (move c)
        //   b.after = c  (b now follows c)
        //   d.after = b  (d was c.successor; c is gone, so d.after = b)
        //
        // Wait — actually a cleaner reframe: move c such that the new
        // order is a → c → b → d.  Reparents:
        //   c.after = a
        //   b.after = c
        //   d.after = b
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();
        let trunk = TrunkId::new(NodeId::new(1), 0);

        let a = branch(1, 0);
        let b = branch(1, 1);
        let c = branch(1, 2);
        let d = branch(1, 3);
        put_branch(&mut txn, trunk, a, None);
        put_branch(&mut txn, trunk, b, Some(a));
        put_branch(&mut txn, trunk, c, Some(b));
        put_branch(&mut txn, trunk, d, Some(c));

        // Paired Reparents to move c from between b and d to between a and b.
        txn.put_crdt_branch_after(&encode_branch_id(&c), &encode_branch_id(&a)).unwrap();
        txn.put_crdt_branch_after(&encode_branch_id(&b), &encode_branch_id(&c)).unwrap();
        txn.put_crdt_branch_after(&encode_branch_id(&d), &encode_branch_id(&b)).unwrap();

        let order = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
        assert_eq!(order, vec![a, c, b, d],
                   "paired Reparents must produce a coherent chain");
    }
}
