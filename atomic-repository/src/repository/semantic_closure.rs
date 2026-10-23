//! Memoized per-trunk CRDT branch state over a view's assembly closure.
//!
//! Resolving the semantic state of one path needs every branch the closure
//! has observed on that path's trunk and the latest predecessor of each.
//! Folding that from the closure means deserializing every visible change.
//! Doing it once per deleted path turned a commit that deletes or moves N
//! files into N full passes over the history, and once the view outgrew the
//! change store's LRU cache each pass re-read every change from disk: a
//! 113-file move in a 1,190-change view stalled `atomic git import` for more
//! than ten minutes on one commit.
//!
//! The memo folds each visible change exactly once per closure and answers
//! every trunk from the result. A closure is keyed by the view and the
//! exclusion set; the most recently used closures are kept side by side so
//! an import stream that alternates plain commits (no exclusions) with
//! merge commits (one exclusion set each) does not restart the fold on
//! every switch. A closure starts over when a previously folded change
//! leaves it, so an unrecord or a view switch never serves stale ops. New
//! changes are folded after the ones already folded, which is a
//! dependency-first order because a visible change never depends on one
//! that became visible later.

use std::collections::{BTreeMap, HashMap, HashSet};

use atomic_core::change::Change;
use atomic_core::crdt::{BranchId, BranchOp, TrunkId};
use atomic_core::pristine::GraphVisibilityClosure;
use atomic_core::types::{Hash, NodeId};

/// How many closures the memo keeps side by side. Plain commits share one
/// key; each merge commit brings its own exclusion set.
const CLOSURE_CAPACITY: usize = 4;

/// Every branch one closure has observed on a trunk: whether its last
/// operation left it alive, and its latest predecessor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TrunkBranchState {
    pub(super) alive: HashMap<BranchId, bool>,
    pub(super) after: BTreeMap<BranchId, Option<BranchId>>,
}

/// Folded branch state for the most recently aligned closures; the closure
/// aligned last answers every query.
#[derive(Debug, Default)]
pub(super) struct SemanticClosureMemo {
    closures: Vec<FoldedClosure>,
}

/// The fold of one `(view, exclusions)` closure.
#[derive(Debug)]
struct FoldedClosure {
    key: MemoKey,
    folded: HashSet<NodeId>,
    trunks: HashMap<TrunkId, TrunkBranchState>,
}

impl FoldedClosure {
    fn new(key: MemoKey) -> Self {
        Self {
            key,
            folded: HashSet::new(),
            trunks: HashMap::new(),
        }
    }

    fn describes(&self, visibility: &GraphVisibilityClosure) -> bool {
        self.folded
            .iter()
            .all(|change_id| visibility.contains(change_id))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MemoKey {
    view: String,
    excluded: Vec<Hash>,
}

impl MemoKey {
    fn new(view: &str, excluded: &[Hash]) -> Self {
        let mut excluded = excluded.to_vec();
        excluded.sort();
        excluded.dedup();
        Self {
            view: view.to_string(),
            excluded,
        }
    }
}

impl SemanticClosureMemo {
    /// Make the closure of `view` under `excluded` the current one: its
    /// folded prefix is kept when every folded change is still visible,
    /// otherwise it starts over. The closures aligned least recently are
    /// dropped beyond `CLOSURE_CAPACITY`.
    pub(super) fn align(
        &mut self,
        view: &str,
        excluded: &[Hash],
        visibility: &GraphVisibilityClosure,
    ) {
        let key = MemoKey::new(view, excluded);
        let retained = self
            .closures
            .iter()
            .position(|closure| closure.key == key)
            .map(|index| self.closures.remove(index));
        let current = match retained {
            Some(closure) if closure.describes(visibility) => closure,
            _ => FoldedClosure::new(key),
        };
        self.closures.insert(0, current);
        self.closures.truncate(CLOSURE_CAPACITY);
    }

    pub(super) fn is_folded(&self, change_id: NodeId) -> bool {
        self.closures
            .first()
            .is_some_and(|closure| closure.folded.contains(&change_id))
    }

    /// Fold one visible change's semantic ops into every trunk of the
    /// current closure. Folding the same change twice, or folding before
    /// any closure was aligned, is a no-op.
    pub(super) fn fold(&mut self, change_id: NodeId, change: &Change) {
        let Some(closure) = self.closures.first_mut() else {
            return;
        };
        if !closure.folded.insert(change_id) {
            return;
        }
        for ops in change.file_ops() {
            let trunk = resolve_trunk(change_id, ops.trunk_id());
            let state = closure.trunks.entry(trunk).or_default();
            for line in ops.line_ops() {
                let branch = resolve_branch(change_id, line.branch_id());
                match line.operation() {
                    BranchOp::Insert { after, .. } => {
                        state.alive.insert(branch, true);
                        state
                            .after
                            .insert(branch, after.map(|id| resolve_branch(change_id, id)));
                    }
                    BranchOp::Modify { .. } | BranchOp::Restore { .. } => {
                        state.alive.insert(branch, true);
                    }
                    BranchOp::Delete { .. } => {
                        state.alive.insert(branch, false);
                    }
                    BranchOp::Reparent { new_after, .. } => {
                        state
                            .after
                            .insert(branch, new_after.map(|id| resolve_branch(change_id, id)));
                    }
                }
            }
        }
    }

    /// The folded state of `trunk` in the current closure; empty when the
    /// closure never touched it.
    pub(super) fn trunk(&self, trunk: &TrunkId) -> TrunkBranchState {
        self.closures
            .first()
            .and_then(|closure| closure.trunks.get(trunk))
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn folded_len(&self) -> usize {
        self.closures
            .first()
            .map_or(0, |closure| closure.folded.len())
    }

    #[cfg(test)]
    pub(super) fn closure_count(&self) -> usize {
        self.closures.len()
    }
}

/// A trunk id recorded relative to its own change resolves to that change.
fn resolve_trunk(change_id: NodeId, raw: TrunkId) -> TrunkId {
    if raw.change_id().is_root() {
        TrunkId::new(change_id, raw.file_idx())
    } else {
        raw
    }
}

/// A branch id recorded relative to its own change resolves to that change.
fn resolve_branch(change_id: NodeId, raw: BranchId) -> BranchId {
    if raw.change_id().is_root() {
        BranchId::new(change_id, raw.branch_idx())
    } else {
        raw
    }
}
