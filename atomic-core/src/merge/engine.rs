//! The semantic merge engine.
//!
//! Given a [`ConflictGroup`] (two or more competing vertices), the engine:
//! 1. Gets content bytes for each competing vertex from the [`ChangeStore`]
//! 2. Finds the common ancestor (dead vertex both sides replaced)
//! 3. Gets content bytes for the ancestor
//! 4. Tokenizes all three versions
//! 5. Performs a three-way diff at the token level
//! 6. Returns `AutoMerged` if edits don't overlap, `Conflict` if they do
//!
//! # Type Parameters
//!
//! * `T` — a read-only graph transaction implementing [`GraphTxnT`]
//! * `C` — a change store implementing [`ChangeStore`]

use super::three_way::{three_way_merge, tokenize, ThreeWayResult};
use super::{ConflictGroup, MergeOutcome, MergeSource};
use crate::change::ChangeStore;
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{GraphNode, Hash, NodeId};

/// The semantic merge engine.
///
/// Wraps a transaction reference and a change store, providing methods for
/// attempting token-level merges of conflicting graph vertices.
///
/// # Type Parameters
///
/// * `T` — a read-only graph transaction implementing [`GraphTxnT`].
/// * `C` — a change store implementing [`ChangeStore`].
///
/// # Example
///
/// ```ignore
/// use atomic_core::merge::{SemanticMergeEngine, ConflictGroup, MergeOutcome};
///
/// let engine = SemanticMergeEngine::new(&txn, &changes);
/// let outcome = engine.try_merge(&group)?;
/// match outcome {
///     MergeOutcome::AutoMerged { content, .. } => { /* write merged bytes */ }
///     MergeOutcome::Conflict { .. }            => { /* write conflict markers */ }
///     MergeOutcome::NoCrdtData                 => { /* fall back to graph-level */ }
///     MergeOutcome::Clean(bytes)               => { /* no conflict at all */ }
/// }
/// ```
pub struct SemanticMergeEngine<'a, T, C> {
    txn: &'a T,
    changes: &'a C,
}

impl<'a, T: GraphTxnT, C: ChangeStore> SemanticMergeEngine<'a, T, C> {
    /// Create a new engine backed by the given transaction and change store.
    pub fn new(txn: &'a T, changes: &'a C) -> Self {
        Self { txn, changes }
    }

    /// Return a reference to the underlying transaction.
    pub fn txn(&self) -> &'a T {
        self.txn
    }

    /// Return a reference to the underlying change store.
    pub fn change_store(&self) -> &'a C {
        self.changes
    }

    /// Attempt to merge a conflict group using token-level semantics.
    ///
    /// # Algorithm
    ///
    /// 1. Extract the two competing vertices (only 2-way supported).
    /// 2. Retrieve content bytes for both from the [`ChangeStore`].
    /// 3. Find the common ancestor vertex — the dead vertex that both
    ///    competing changes deleted.
    /// 4. Retrieve content bytes for the ancestor.
    /// 5. Tokenize all three byte sequences.
    /// 6. Run a three-way diff at the token level.
    /// 7. If no edits overlap → [`MergeOutcome::AutoMerged`].
    /// 8. If edits overlap   → [`MergeOutcome::Conflict`].
    ///
    /// Falls back to [`MergeOutcome::NoCrdtData`] when:
    /// - The conflict is not 2-way
    /// - Content bytes cannot be retrieved
    /// - No common ancestor can be determined
    ///
    /// # Errors
    ///
    /// Returns [`PristineError`] on database access failure.
    pub fn try_merge(&self, group: &ConflictGroup) -> Result<MergeOutcome, PristineError> {
        // Only two-way conflicts are candidates (the most common case).
        if group.vertex_count() != 2 {
            log::debug!(
                "SemanticMergeEngine::try_merge: skipping {}-way conflict (only 2-way supported)",
                group.vertex_count(),
            );
            return Ok(MergeOutcome::NoCrdtData);
        }

        let v_left = &group.vertices[0];
        let v_right = &group.vertices[1];

        // Step 1-2: Get content bytes for both competing vertices.
        let left_content = match self.get_vertex_content(v_left) {
            Ok(c) => c,
            Err(e) => {
                log::debug!(
                    "SemanticMergeEngine::try_merge: cannot read left vertex {}: {}",
                    v_left,
                    e,
                );
                return Ok(MergeOutcome::NoCrdtData);
            }
        };

        let right_content = match self.get_vertex_content(v_right) {
            Ok(c) => c,
            Err(e) => {
                log::debug!(
                    "SemanticMergeEngine::try_merge: cannot read right vertex {}: {}",
                    v_right,
                    e,
                );
                return Ok(MergeOutcome::NoCrdtData);
            }
        };

        // Step 3: Find the common ancestor vertex (the dead vertex both sides
        // replaced). Try the explicit `ancestor` field first, then fall back
        // to graph-based discovery using the `parent` field.
        let base_content = if let Some(ref ancestor) = group.ancestor {
            match self.get_vertex_content(ancestor) {
                Ok(c) => c,
                Err(e) => {
                    log::debug!(
                        "SemanticMergeEngine::try_merge: cannot read ancestor {}: {}",
                        ancestor,
                        e,
                    );
                    return Ok(MergeOutcome::NoCrdtData);
                }
            }
        } else if let Some(ref parent) = group.parent {
            match self.find_ancestor_content(parent, v_left.change, v_right.change) {
                Ok(Some(content)) => content,
                Ok(None) => {
                    log::debug!(
                        "SemanticMergeEngine::try_merge: no ancestor found from parent {}",
                        parent,
                    );
                    return Ok(MergeOutcome::NoCrdtData);
                }
                Err(e) => {
                    log::debug!(
                        "SemanticMergeEngine::try_merge: ancestor search failed: {}",
                        e,
                    );
                    return Ok(MergeOutcome::NoCrdtData);
                }
            }
        } else {
            log::debug!(
                "SemanticMergeEngine::try_merge: no ancestor or parent available, \
                 falling back to NoCrdtData",
            );
            return Ok(MergeOutcome::NoCrdtData);
        };

        // Step 5: Tokenize all three versions.
        let base_tokens = tokenize(&base_content);
        let left_tokens = tokenize(&left_content);
        let right_tokens = tokenize(&right_content);

        // Step 6-7-8: Three-way merge at the token level.
        match three_way_merge(&base_tokens, &left_tokens, &right_tokens) {
            ThreeWayResult::Merged(content) => {
                log::debug!(
                    "SemanticMergeEngine::try_merge: auto-merged {} + {} → {} bytes",
                    left_content.len(),
                    right_content.len(),
                    content.len(),
                );

                let left_token_count = left_tokens
                    .iter()
                    .zip(base_tokens.iter())
                    .filter(|(l, b)| l != b)
                    .count();
                let right_token_count = right_tokens
                    .iter()
                    .zip(base_tokens.iter())
                    .filter(|(r, b)| r != b)
                    .count();

                Ok(MergeOutcome::AutoMerged {
                    content,
                    sources: vec![
                        MergeSource::new(v_left.change, left_token_count),
                        MergeSource::new(v_right.change, right_token_count),
                    ],
                })
            }
            ThreeWayResult::Conflict => {
                log::debug!(
                    "SemanticMergeEngine::try_merge: conflict between {} and {}",
                    v_left,
                    v_right,
                );
                Ok(MergeOutcome::Conflict {
                    base: base_content,
                    left: left_content,
                    right: right_content,
                    left_change: v_left.change,
                    right_change: v_right.change,
                })
            }
        }
    }

    /// Read the content bytes for a graph vertex from the change store.
    ///
    /// Returns an empty `Vec` for root or empty (structural) vertices.
    fn get_vertex_content(&self, vertex: &GraphNode<NodeId>) -> Result<Vec<u8>, PristineError> {
        if vertex.is_root() || vertex.start == vertex.end {
            return Ok(Vec::new());
        }

        let len = vertex.end.get().saturating_sub(vertex.start.get()) as usize;
        if len == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; len];

        let txn = self.txn;
        let hash_fn = |id: NodeId| -> Option<Hash> { txn.get_external(id).ok().flatten() };

        self.changes
            .get_contents(hash_fn, *vertex, &mut buf)
            .map_err(|e| PristineError::Inconsistent {
                message: format!("ChangeStore::get_contents failed for {}: {}", vertex, e),
            })?;

        Ok(buf)
    }

    /// Walk deleted forward edges from `parent` to find a dead vertex that
    /// was deleted by both `left_change` and `right_change`.
    ///
    /// Returns the content bytes of the ancestor, or `None` if no shared
    /// dead vertex could be found.
    fn find_ancestor_content(
        &self,
        parent: &GraphNode<NodeId>,
        left_change: NodeId,
        right_change: NodeId,
    ) -> Result<Option<Vec<u8>>, PristineError> {
        // Look at all forward edges from the parent, including deleted ones.
        let forward_edges = self.txn.iter_forward(*parent, true)?;

        for edge in &forward_edges {
            if !edge.kind.is_deleted() {
                continue;
            }

            let dead_vertex = self.txn.find_block(edge.dest)?;

            // Check whether this vertex was deleted by BOTH competing changes
            // by examining its parent edges.
            let parent_edges = self.txn.iter_parents(dead_vertex, true)?;

            let mut deleted_by_left = false;
            let mut deleted_by_right = false;

            for parent_edge in &parent_edges {
                if parent_edge.kind.is_deleted() {
                    if parent_edge.introduced_by == left_change {
                        deleted_by_left = true;
                    }
                    if parent_edge.introduced_by == right_change {
                        deleted_by_right = true;
                    }
                }
            }

            if deleted_by_left && deleted_by_right {
                let content = self.get_vertex_content(&dead_vertex)?;
                return Ok(Some(content));
            }
        }

        Ok(None)
    }
}

// ===========================================================================
// Backward-compatible single-type-parameter wrapper
// ===========================================================================

/// A transaction-only merge engine that always returns [`MergeOutcome::NoCrdtData`].
///
/// This preserves backward compatibility with code that creates the engine
/// without a change store. Use [`SemanticMergeEngine`] directly when you have
/// both a transaction and a change store.
pub struct TxnOnlyMergeEngine<'a, T> {
    txn: &'a T,
}

impl<'a, T: GraphTxnT> TxnOnlyMergeEngine<'a, T> {
    /// Create a new engine backed by only a transaction (no change store).
    pub fn new(txn: &'a T) -> Self {
        Self { txn }
    }

    /// Return a reference to the underlying transaction.
    pub fn txn(&self) -> &'a T {
        self.txn
    }

    /// Attempt to merge — always returns [`MergeOutcome::NoCrdtData`] because
    /// no change store is available to read vertex content.
    pub fn try_merge(&self, group: &ConflictGroup) -> Result<MergeOutcome, PristineError> {
        log::debug!(
            "TxnOnlyMergeEngine::try_merge: {} vertices, no change store available",
            group.vertex_count(),
        );
        Ok(MergeOutcome::NoCrdtData)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, ChangeHeader, MemoryChangeStore};
    use crate::types::{ChangePosition, EdgeFlags, GraphNode, NodeId};

    /// Minimal mock that satisfies [`GraphTxnT`] for unit testing.
    ///
    /// All methods return `Ok(None)` / empty results — just enough to
    /// compile and exercise the engine paths.
    struct MockTxn {
        /// Map from NodeId → Hash for `get_external`.
        externals: std::collections::HashMap<NodeId, Hash>,
    }

    impl MockTxn {
        fn new() -> Self {
            Self {
                externals: std::collections::HashMap::new(),
            }
        }

        fn register(&mut self, id: NodeId, hash: Hash) {
            self.externals.insert(id, hash);
        }
    }

    impl GraphTxnT for MockTxn {
        type Adj = std::vec::IntoIter<Result<crate::types::SerializedGraphEdge, PristineError>>;

        fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
            Ok(self.externals.get(&id).copied())
        }

        fn get_internal(&self, _hash: &Hash) -> Result<Option<NodeId>, PristineError> {
            Ok(None)
        }

        fn get_node_type(&self, _id: NodeId) -> Result<Option<u8>, PristineError> {
            Ok(None)
        }

        fn iter_adjacent(
            &self,
            _node: GraphNode<NodeId>,
            _min_flag: EdgeFlags,
            _max_flag: EdgeFlags,
        ) -> Result<Self::Adj, PristineError> {
            Ok(Vec::new().into_iter())
        }

        fn find_block(
            &self,
            _pos: crate::types::Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            Ok(GraphNode::ROOT)
        }

        fn find_block_end(
            &self,
            _pos: crate::types::Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            Ok(GraphNode::ROOT)
        }

        fn has_vertex(&self, _node: GraphNode<NodeId>) -> Result<bool, PristineError> {
            Ok(false)
        }

        fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
            Ok(Vec::new())
        }

        fn has_change_in_graph(&self, _change_id: NodeId) -> Result<bool, PristineError> {
            Ok(false)
        }
    }

    // ---- Helpers ----------------------------------------------------------

    /// Create a change with the given content bytes and return its hash.
    fn make_change(store: &MemoryChangeStore, content: &[u8]) -> Hash {
        let mut change = Change::empty(ChangeHeader::new("test change"));
        change.contents = content.to_vec();
        store.insert_change(change).expect("insert_change")
    }

    // ---- TxnOnlyMergeEngine tests -----------------------------------------

    #[test]
    fn txn_only_returns_no_crdt_data() {
        let txn = MockTxn::new();
        let engine = TxnOnlyMergeEngine::new(&txn);

        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );

        let group = ConflictGroup::new(vec![v1, v2]);
        let outcome = engine.try_merge(&group).expect("try_merge should not fail");
        assert!(
            matches!(outcome, MergeOutcome::NoCrdtData),
            "TxnOnlyMergeEngine should always return NoCrdtData"
        );
    }

    // ---- SemanticMergeEngine: skip >2-way ---------------------------------

    #[test]
    fn skips_three_way_conflicts() {
        let txn = MockTxn::new();
        let store = MemoryChangeStore::new();
        let engine = SemanticMergeEngine::new(&txn, &store);

        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let v3 = GraphNode::new(
            NodeId::new(3),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );

        let group = ConflictGroup::new(vec![v1, v2, v3]);
        let outcome = engine.try_merge(&group).unwrap();
        assert!(matches!(outcome, MergeOutcome::NoCrdtData));
    }

    // ---- SemanticMergeEngine: no ancestor → NoCrdtData --------------------

    #[test]
    fn no_ancestor_returns_no_crdt_data() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        let hash = make_change(&store, b"hello world");
        let id = NodeId::new(1);
        txn.register(id, hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        let v1 = GraphNode::new(id, ChangePosition::new(0), ChangePosition::new(5));
        let v2 = GraphNode::new(id, ChangePosition::new(5), ChangePosition::new(11));

        // No ancestor, no parent → NoCrdtData.
        let group = ConflictGroup::new(vec![v1, v2]);
        let outcome = engine.try_merge(&group).unwrap();
        assert!(matches!(outcome, MergeOutcome::NoCrdtData));
    }

    // ---- SemanticMergeEngine: auto-merge with explicit ancestor ------------

    #[test]
    fn auto_merge_non_overlapping_edits() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        // Base content: "host: localhost, port: 3000"
        let base_hash = make_change(&store, b"host: localhost, port: 3000");
        let base_id = NodeId::new(1);
        txn.register(base_id, base_hash);

        // Left changes host: "host: production, port: 3000"
        let left_hash = make_change(&store, b"host: production, port: 3000");
        let left_id = NodeId::new(2);
        txn.register(left_id, left_hash);

        // Right changes port: "host: localhost, port: 8080"
        let right_hash = make_change(&store, b"host: localhost, port: 8080");
        let right_id = NodeId::new(3);
        txn.register(right_id, right_hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        let v_base = GraphNode::new(base_id, ChangePosition::new(0), ChangePosition::new(27));
        let v_left = GraphNode::new(left_id, ChangePosition::new(0), ChangePosition::new(28));
        let v_right = GraphNode::new(right_id, ChangePosition::new(0), ChangePosition::new(27));

        let group = ConflictGroup::new(vec![v_left, v_right]).with_ancestor(v_base);
        let outcome = engine.try_merge(&group).unwrap();

        assert!(
            outcome.is_auto_merged(),
            "expected AutoMerged, got {:?}",
            outcome,
        );
        assert_eq!(
            std::str::from_utf8(outcome.content()).unwrap(),
            "host: production, port: 8080",
        );
    }

    // ---- SemanticMergeEngine: conflict on same token -----------------------

    #[test]
    fn conflict_on_overlapping_edits() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        // Base: "x = 1"
        let base_hash = make_change(&store, b"x = 1");
        let base_id = NodeId::new(1);
        txn.register(base_id, base_hash);

        // Left: "x = 2"
        let left_hash = make_change(&store, b"x = 2");
        let left_id = NodeId::new(2);
        txn.register(left_id, left_hash);

        // Right: "x = 3"
        let right_hash = make_change(&store, b"x = 3");
        let right_id = NodeId::new(3);
        txn.register(right_id, right_hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        let v_base = GraphNode::new(base_id, ChangePosition::new(0), ChangePosition::new(5));
        let v_left = GraphNode::new(left_id, ChangePosition::new(0), ChangePosition::new(5));
        let v_right = GraphNode::new(right_id, ChangePosition::new(0), ChangePosition::new(5));

        let group = ConflictGroup::new(vec![v_left, v_right]).with_ancestor(v_base);
        let outcome = engine.try_merge(&group).unwrap();

        assert!(
            outcome.is_conflict(),
            "expected Conflict, got {:?}",
            outcome,
        );
        if let MergeOutcome::Conflict {
            base,
            left,
            right,
            left_change,
            right_change,
        } = &outcome
        {
            assert_eq!(base, b"x = 1");
            assert_eq!(left, b"x = 2");
            assert_eq!(right, b"x = 3");
            assert_eq!(*left_change, left_id);
            assert_eq!(*right_change, right_id);
        }
    }

    // ---- SemanticMergeEngine: both sides same edit → AutoMerged -----------

    #[test]
    fn both_sides_same_edit_auto_merges() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        let base_hash = make_change(&store, b"x = 1");
        let base_id = NodeId::new(1);
        txn.register(base_id, base_hash);

        let left_hash = make_change(&store, b"x = 2");
        let left_id = NodeId::new(2);
        txn.register(left_id, left_hash);

        let right_hash = make_change(&store, b"x = 2");
        let right_id = NodeId::new(3);
        txn.register(right_id, right_hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        let v_base = GraphNode::new(base_id, ChangePosition::new(0), ChangePosition::new(5));
        let v_left = GraphNode::new(left_id, ChangePosition::new(0), ChangePosition::new(5));
        let v_right = GraphNode::new(right_id, ChangePosition::new(0), ChangePosition::new(5));

        let group = ConflictGroup::new(vec![v_left, v_right]).with_ancestor(v_base);
        let outcome = engine.try_merge(&group).unwrap();

        assert!(
            outcome.is_auto_merged(),
            "expected AutoMerged, got {:?}",
            outcome
        );
        assert_eq!(std::str::from_utf8(outcome.content()).unwrap(), "x = 2");
    }

    // ---- SemanticMergeEngine: empty vertex content → NoCrdtData? ----------

    #[test]
    fn empty_vertices_produce_auto_merge() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        // All three are empty content (structural inodes).
        let hash = make_change(&store, b"");
        let id = NodeId::new(1);
        txn.register(id, hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        // Empty vertices: start == end.
        let v1 = GraphNode::new(id, ChangePosition::new(0), ChangePosition::new(0));
        let v2 = GraphNode::new(id, ChangePosition::new(0), ChangePosition::new(0));
        let ancestor = GraphNode::new(id, ChangePosition::new(0), ChangePosition::new(0));

        let group = ConflictGroup::new(vec![v1, v2]).with_ancestor(ancestor);
        let outcome = engine.try_merge(&group).unwrap();

        // Empty tokens on all sides → trivially merged.
        assert!(
            outcome.is_auto_merged(),
            "expected AutoMerged for empty, got {:?}",
            outcome
        );
        assert!(outcome.content().is_empty());
    }

    // ---- SemanticMergeEngine: exposes references ---------------------------

    #[test]
    fn engine_exposes_references() {
        let txn = MockTxn::new();
        let store = MemoryChangeStore::new();
        let engine = SemanticMergeEngine::new(&txn, &store);
        // Smoke-test that we can access the underlying references.
        let _t: &MockTxn = engine.txn();
        let _c: &MemoryChangeStore = engine.change_store();
    }

    // ---- SemanticMergeEngine: content read failure → NoCrdtData -----------

    #[test]
    fn unreadable_left_returns_no_crdt_data() {
        let txn = MockTxn::new(); // No externals registered.
        let store = MemoryChangeStore::new();
        let engine = SemanticMergeEngine::new(&txn, &store);

        // Vertices reference NodeId(99) which has no external hash.
        let v1 = GraphNode::new(
            NodeId::new(99),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let v2 = GraphNode::new(
            NodeId::new(99),
            ChangePosition::new(5),
            ChangePosition::new(10),
        );

        let ancestor = GraphNode::new(
            NodeId::new(99),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let group = ConflictGroup::new(vec![v1, v2]).with_ancestor(ancestor);
        let outcome = engine.try_merge(&group).unwrap();
        assert!(matches!(outcome, MergeOutcome::NoCrdtData));
    }

    // ---- SemanticMergeEngine: code-like auto-merge -------------------------

    #[test]
    fn code_auto_merge() {
        let mut txn = MockTxn::new();
        let store = MemoryChangeStore::new();

        let base_hash = make_change(&store, b"fn main() { return 0; }");
        let base_id = NodeId::new(1);
        txn.register(base_id, base_hash);

        let left_hash = make_change(&store, b"fn main() { return 1; }");
        let left_id = NodeId::new(2);
        txn.register(left_id, left_hash);

        let right_hash = make_change(&store, b"fn run() { return 0; }");
        let right_id = NodeId::new(3);
        txn.register(right_id, right_hash);

        let engine = SemanticMergeEngine::new(&txn, &store);

        let v_base = GraphNode::new(base_id, ChangePosition::new(0), ChangePosition::new(23));
        let v_left = GraphNode::new(left_id, ChangePosition::new(0), ChangePosition::new(23));
        let v_right = GraphNode::new(right_id, ChangePosition::new(0), ChangePosition::new(22));

        let group = ConflictGroup::new(vec![v_left, v_right]).with_ancestor(v_base);
        let outcome = engine.try_merge(&group).unwrap();

        assert!(
            outcome.is_auto_merged(),
            "expected AutoMerged, got {:?}",
            outcome
        );
        assert_eq!(
            std::str::from_utf8(outcome.content()).unwrap(),
            "fn run() { return 1; }",
        );
    }
}
