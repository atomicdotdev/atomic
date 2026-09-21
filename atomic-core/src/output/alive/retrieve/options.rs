//! Retrieval options and result types.
//!
//! This module contains [`RetrieveOptions`] for configuring graph retrieval
//! and [`RetrieveResult`] for returning the retrieved graph with statistics.

use std::collections::HashMap;

use super::super::graph::AliveGraph;
use crate::pristine::{GraphTxnT, GraphVisibilityClosure, PristineError};
use crate::types::{ForwardEdge, GraphNode, NodeId, ParentEdge, ParentEdgeKind};

// RETRIEVE OPTIONS

/// Options for graph retrieval.
///
/// These options control what content is included in the retrieved graph.
/// The `graph_visibility` option enables state-based content retrieval with a
/// validated dependency closure, which is essential for showing what a
/// specific change modified.
///
/// # State-Based Retrieval
///
/// When reviewing a specific change, you want to see:
/// 1. The file content BEFORE the change (parent state)
/// 2. The file content AFTER the change (current state)
///
/// This is achieved by setting `graph_visibility` to the validated closure
/// for changes applied up to a certain point:
///
/// ```text
/// Change Sequence:  [0]  [1]  [2]  [3]  [4]  [5]  ...
///                    │    │    │    │    │    │
///                    ▼    ▼    ▼    ▼    ▼    ▼
/// Parent State: ────────────────────┘    │
/// (filter includes 0-3)                  │
///                                        │
/// Current State: ────────────────────────┘
/// (filter includes 0-4)
/// ```
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
/// use atomic_core::pristine::GraphVisibilityClosure;
///
/// // Retrieve content at a specific state (e.g., before change at seq 5).
/// let membership_before = get_membership_up_to_sequence(&txn, &view, 5)?;
/// let visibility_before =
///     GraphVisibilityClosure::try_from_membership(&txn, &membership_before)?;
/// let options = RetrieveOptions::new().with_graph_visibility(visibility_before);
/// let parent_graph = retrieve_graph(&txn, file_pos, options)?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct RetrieveOptions {
    /// Include deleted vertices in the graph.
    ///
    /// When true, vertices that have been deleted will be included and
    /// marked appropriately. This is useful for showing conflict content.
    pub include_deleted: bool,

    /// Maximum number of vertices to retrieve.
    ///
    /// If set, retrieval will stop after this many vertices. This can
    /// prevent runaway retrieval on corrupted or very large files.
    pub max_vertices: Option<usize>,

    /// Optional validated dependency closure for graph traversal.
    ///
    /// When set, only vertices whose `change_id` is in this closure (or is
    /// ROOT) are included. `None` explicitly means the ambient graph is read
    /// without visibility filtering; `Some(GraphVisibilityClosure::empty())`
    /// is an active filter that permits only ROOT.
    pub graph_visibility: Option<GraphVisibilityClosure>,

    /// Treat every visible deletion as authoritative, without a closure.
    ///
    /// This applies the same aliveness semantics as graph visibility that
    /// contains every visible change: any `BLOCK|DELETED` parent edge the
    /// transaction can see marks the vertex dead, and retrieval walks
    /// through it to its live successors instead of surfacing it as a
    /// zombie.  Use this when the transaction itself already scopes edge
    /// visibility (e.g. a `ViewGraph`) or when all changes are visible by
    /// construction (e.g. git import on a bare transaction), and the
    /// caller needs the graph to match what a filtered materialization
    /// renders.
    ///
    /// Without this (and without `graph_visibility`), the additive edge
    /// model keeps a deleted vertex "alive": its original parent edge
    /// remains next to the deletion marker, so it is retrieved as a
    /// zombie and occupies a position in the output order.
    pub deletions_final: bool,

    /// Stop the traversal as soon as the graph holds at least one alive
    /// non-empty content vertex.
    ///
    /// This is an optional liveness fast path (review E4): a caller that only
    /// needs "does this position render any alive bytes?" (for example the
    /// path-projection's absent-entry resurrection check) can request the
    /// early exit instead of walking every vertex of a long-history file. The
    /// returned graph is PARTIAL and must not be used as content — only its
    /// `total_bytes() > 0` verdict is meaningful.
    pub stop_at_first_content: bool,
}

impl RetrieveOptions {
    /// Create new options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include deleted vertices.
    ///
    /// When true, vertices marked as deleted will be included in the graph.
    /// This is useful for showing conflict content or full history.
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum number of vertices to retrieve.
    ///
    /// This provides a safety limit for retrieval. If the graph exceeds
    /// this size, retrieval will stop and the result will be truncated.
    pub fn max_vertices(mut self, max: usize) -> Self {
        self.max_vertices = Some(max);
        self
    }

    /// Request the liveness fast path: stop as soon as the traversal has
    /// recorded one alive non-empty content vertex (review E4).
    pub fn stop_at_first_content(mut self, stop: bool) -> Self {
        self.stop_at_first_content = stop;
        self
    }

    /// Set validated graph visibility for state-based content retrieval.
    ///
    /// The closure must be constructed through
    /// [`GraphVisibilityClosure::try_from_membership`], which proves indexed
    /// dependency completeness before traversal starts.
    pub fn with_graph_visibility(mut self, visibility: GraphVisibilityClosure) -> Self {
        self.graph_visibility = Some(visibility);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_change_filter(self, filter: std::collections::HashSet<NodeId>) -> Self {
        self.with_graph_visibility(GraphVisibilityClosure::from_ordered_unchecked(filter))
    }

    #[cfg(test)]
    pub(crate) fn with_change_filter_arc(
        self,
        filter: std::sync::Arc<std::collections::HashSet<NodeId>>,
    ) -> Self {
        self.with_graph_visibility(GraphVisibilityClosure::from_ordered_unchecked(
            filter.iter().copied(),
        ))
    }

    /// Treat every visible deletion as authoritative (see
    /// [`RetrieveOptions::deletions_final`]).
    pub fn deletions_final(mut self, value: bool) -> Self {
        self.deletions_final = value;
        self
    }

    /// Whether vertex aliveness uses the deletion-aware (filtered) logic.
    pub(crate) fn deletion_aware(&self) -> bool {
        self.graph_visibility.is_some() || self.deletions_final
    }

    /// Check if a change ID passes graph visibility.
    ///
    /// Returns true if:
    /// - No closure is set (the ambient graph is unfiltered)
    /// - The change_id is ROOT (NodeId(0), always passes)
    /// - The change_id is in the validated closure
    pub fn passes_filter(&self, change_id: NodeId) -> bool {
        match &self.graph_visibility {
            None => true, // Explicitly unfiltered ambient graph.
            Some(visibility) => {
                // ROOT always passes (it's the origin of the graph)
                if change_id == NodeId::ROOT {
                    return true;
                }
                visibility.contains(change_id)
            }
        }
    }

    /// Check if graph visibility filtering is active.
    pub fn has_filter(&self) -> bool {
        self.graph_visibility.is_some()
    }

    /// Whether `iter_forward` should include deleted edges.
    ///
    /// When graph visibility is active, we always include deleted edges
    /// because we need to traverse them to find content that was deleted
    /// by changes OUTSIDE our filter (which means the content was still
    /// alive at the target state). Same when `include_deleted` is
    /// explicitly set.
    pub(crate) fn include_deleted_edges(&self) -> bool {
        self.include_deleted || self.graph_visibility.is_some() || self.deletions_final
    }

    // ------------------------------------------------------------------
    // Typed edge model — Phase A3 replacements
    // ------------------------------------------------------------------

    /// Check if a forward edge should be followed during traversal.
    ///
    /// A `BlockDeleted` / `FolderDeleted` edge is **never** an alive
    /// forward edge.  In the additive model a deletion is recorded by
    /// adding a new edge with the `DELETED` flag *alongside* the
    /// original `Block`/`Folder` edge — the original stays in the
    /// B-tree forever.  Reachability of the destination always comes
    /// from the original edge, which is filtered separately by
    /// `passes_filter` on its `introduced_by`.
    ///
    /// Treating a `BlockDeleted` edge as alive (because its introducer
    /// is outside the filter) would push a *second* child entry to the
    /// same destination into the alive graph, which fork-detection
    /// misreads as a concurrent CRDT conflict.
    ///
    /// Vertex-level aliveness still consults parent-side `BlockDeleted`
    /// edges via [`Self::is_vertex_alive`] — that path correctly distinguishes
    /// "deletion happened from our view" from "deletion is invisible".
    pub fn is_edge_alive(&self, edge: &ForwardEdge) -> bool {
        !edge.kind.is_deleted()
    }

    /// Check if a vertex is alive by examining all its parent edges.
    ///
    /// A vertex is alive if it has at least one live parent AND was not
    /// deleted by a change in the active visibility closure.
    ///
    /// Uses typed [`ParentEdgeKind`] matching instead of raw `EdgeFlags`
    /// bitflag checks, so every case is visible and the compiler rejects
    /// missing arms.
    ///
    /// # Logic
    ///
    /// - **Non-deleted parent edge** → live parent
    /// - **DELETED parent, introduced OUTSIDE visibility** → live parent
    ///   (the deletion is "in the future" from our perspective)
    /// - **DELETED parent, introduced IN visibility** → marks vertex dead
    ///
    /// The vertex is alive when it has at least one live parent AND
    /// was not deleted by an in-filter change.
    pub fn is_vertex_alive<T: GraphTxnT>(
        &self,
        txn: &T,
        vertex: GraphNode<NodeId>,
    ) -> Result<bool, PristineError> {
        let mut memo = HashMap::new();
        self.is_vertex_alive_with_memo(txn, vertex, &mut memo)
    }

    /// Vertex aliveness with a caller-owned dependency memo.
    ///
    /// `change_depends_on` is a pure function of the change pair; sharing the
    /// memo across a whole retrieval avoids re-resolving the same causal
    /// dominance questions for every vertex of a heavily edited file.
    pub fn is_vertex_alive_with_memo<T: GraphTxnT>(
        &self,
        txn: &T,
        vertex: GraphNode<NodeId>,
        memo: &mut HashMap<(NodeId, NodeId), bool>,
    ) -> Result<bool, PristineError> {
        // Without a filter (and without deletions_final), delegate to the
        // unfiltered classifier. With deletions_final set, the logic below
        // applies with every visible change "in the filter" — passes_filter
        // returns true for everything when no set is present.
        if !self.deletion_aware() {
            return super::classify::is_vertex_alive(txn, &vertex);
        }

        // Root is always alive
        if vertex.is_root() {
            return Ok(true);
        }

        // Edge updates are additive. Keep only causally maximal visible parent
        // states so A -> delete(A) -> undelete(delete) resolves to the final
        // alive state without using view order. Sibling-only updates are ignored.
        let parents: Vec<ParentEdge> = txn
            .iter_parents(vertex, true)?
            .into_iter()
            .filter(|parent| self.passes_filter(parent.introduced_by))
            .collect();
        let mut visiting = Vec::new();
        let mut maximal = Vec::new();
        for (index, candidate) in parents.iter().enumerate() {
            let mut superseded = false;
            for (other_index, other) in parents.iter().enumerate() {
                if index == other_index || candidate.introduced_by == other.introduced_by {
                    continue;
                }
                if change_depends_on(
                    txn,
                    other.introduced_by,
                    candidate.introduced_by,
                    memo,
                    &mut visiting,
                )? {
                    superseded = true;
                    break;
                }
            }
            if !superseded {
                maximal.push(*candidate);
            }
        }

        let mut has_live_parent = false;
        let mut has_deleted_parent = false;
        for parent in maximal {
            match parent.kind {
                ParentEdgeKind::Block | ParentEdgeKind::Folder => has_live_parent = true,
                ParentEdgeKind::PseudoBlock | ParentEdgeKind::PseudoFolder => {
                    if vertex.is_empty() {
                        has_live_parent = true;
                    }
                }
                ParentEdgeKind::BlockDeleted | ParentEdgeKind::FolderDeleted => {
                    has_deleted_parent = true;
                }
            }
        }

        // Concurrent maximal delete and alive states remain conservatively dead;
        // conflict handling may surface surviving descendants separately.
        Ok(has_live_parent && !has_deleted_parent)
    }
}

fn change_depends_on<T: GraphTxnT>(
    txn: &T,
    descendant: NodeId,
    ancestor: NodeId,
    memo: &mut HashMap<(NodeId, NodeId), bool>,
    visiting: &mut Vec<NodeId>,
) -> Result<bool, PristineError> {
    if descendant == ancestor {
        return Ok(true);
    }
    if descendant.is_root() || ancestor.is_root() {
        return Ok(false);
    }
    if let Some(result) = memo.get(&(descendant, ancestor)) {
        return Ok(*result);
    }
    if let Some(start) = visiting.iter().position(|change| *change == descendant) {
        let mut cycle: Vec<u64> = visiting[start..]
            .iter()
            .map(|change| change.get())
            .collect();
        cycle.push(descendant.get());
        return Err(PristineError::DependencyCycle { cycle });
    }
    visiting.push(descendant);

    let dependencies = txn.get_indexed_change_deps(descendant)?;
    for dependency in dependencies {
        let dependency_id = txn.get_internal(&dependency)?.ok_or_else(|| {
            PristineError::MissingRegisteredDependency {
                change_id: descendant.get(),
                dependency: dependency.to_string(),
            }
        })?;
        if dependency_id == ancestor
            || change_depends_on(txn, dependency_id, ancestor, memo, visiting)?
        {
            visiting.pop();
            memo.insert((descendant, ancestor), true);
            return Ok(true);
        }
    }
    visiting.pop();
    memo.insert((descendant, ancestor), false);
    Ok(false)
}

impl PartialEq for RetrieveOptions {
    fn eq(&self, other: &Self) -> bool {
        self.include_deleted == other.include_deleted
            && self.max_vertices == other.max_vertices
            && self.deletions_final == other.deletions_final
            && self.graph_visibility == other.graph_visibility
    }
}

impl Eq for RetrieveOptions {}

// RETRIEVE RESULT

/// Result of a graph retrieval operation.
#[derive(Debug)]
pub struct RetrieveResult {
    /// The retrieved graph.
    pub graph: AliveGraph,

    /// Whether retrieval was truncated due to max_vertices.
    pub truncated: bool,

    /// Number of positions visited (may be more than vertices if some skipped).
    pub positions_visited: usize,

    /// Number of edges traversed.
    pub edges_traversed: usize,

    /// Whether graph visibility filtering was active during retrieval.
    ///
    /// When `true` and `graph.is_empty()`, it means the file has no
    /// content on the target view (all vertices were filtered out).
    /// Callers can use this to distinguish "genuinely empty file" from
    /// "file belongs to a different view".
    pub was_filtered: bool,
}

impl RetrieveResult {
    /// Create a new retrieve result.
    pub(super) fn new(graph: AliveGraph) -> Self {
        Self {
            graph,
            truncated: false,
            positions_visited: 0,
            edges_traversed: 0,
            was_filtered: false,
        }
    }
}
