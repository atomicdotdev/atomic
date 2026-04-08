//! Semantic merge engine for token-level conflict resolution.
//!
//! When two graph vertices compete for the same position (both are alive
//! children of the same parent), the merge engine checks if the edits
//! are to different tokens.  If so, it auto-merges.  If the same token
//! was edited by both sides, it reports a true conflict.
//!
//! # Architecture
//!
//! The merge engine sits between graph traversal and file output:
//!
//! ```text
//! retrieve_graph() → detect conflicts → SemanticMergeEngine::try_merge()
//!                                            │
//!                                    ┌───────┴───────┐
//!                                    │               │
//!                              AutoMerged       Conflict
//!                              (write merged)   (write markers)
//! ```

mod engine;
mod resolved;
pub mod three_way;
mod types;

pub use engine::{SemanticMergeEngine, TxnOnlyMergeEngine};
pub use resolved::ResolvedConflicts;
pub use three_way::{three_way_merge, three_way_merge_bytes, tokenize, MergeToken, ThreeWayResult};
pub use types::{ConflictGroup, LeafEdit, MergeSource};

use crate::types::NodeId;

/// The result of attempting to merge competing graph vertices.
#[derive(Debug, Clone)]
pub enum MergeOutcome {
    /// No conflict — only one vertex at this position.
    Clean(Vec<u8>),

    /// Semantic layer auto-resolved the conflict.
    /// Both sides edited different tokens on the same line.
    AutoMerged {
        /// The merged content bytes.
        content: Vec<u8>,
        /// Changes that contributed to the merge.
        sources: Vec<MergeSource>,
    },

    /// True conflict — both sides edited the same token(s).
    /// Cannot be auto-resolved.
    Conflict {
        /// The original content (common ancestor).
        base: Vec<u8>,
        /// One side's version.
        left: Vec<u8>,
        /// Other side's version.
        right: Vec<u8>,
        /// Change that produced left.
        left_change: NodeId,
        /// Change that produced right.
        right_change: NodeId,
    },

    /// CRDT data not available for these vertices.
    /// Fall back to graph-level conflict handling.
    NoCrdtData,
}

impl MergeOutcome {
    /// Returns `true` if the outcome is [`Clean`](Self::Clean).
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean(_))
    }

    /// Returns `true` if the outcome is [`AutoMerged`](Self::AutoMerged).
    pub fn is_auto_merged(&self) -> bool {
        matches!(self, Self::AutoMerged { .. })
    }

    /// Returns `true` if the outcome is [`Conflict`](Self::Conflict).
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    /// Get the content bytes regardless of outcome type.
    ///
    /// For conflicts, returns the left side (caller should check
    /// [`is_conflict`](Self::is_conflict) first). For [`NoCrdtData`](Self::NoCrdtData),
    /// returns an empty slice.
    pub fn content(&self) -> &[u8] {
        match self {
            Self::Clean(c) => c,
            Self::AutoMerged { content, .. } => content,
            Self::Conflict { left, .. } => left,
            Self::NoCrdtData => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangePosition, GraphNode};

    #[test]
    fn test_merge_outcome_clean() {
        let outcome = MergeOutcome::Clean(b"hello".to_vec());
        assert!(outcome.is_clean());
        assert!(!outcome.is_auto_merged());
        assert!(!outcome.is_conflict());
        assert_eq!(outcome.content(), b"hello");
    }

    #[test]
    fn test_merge_outcome_auto_merged() {
        let outcome = MergeOutcome::AutoMerged {
            content: b"merged".to_vec(),
            sources: vec![],
        };
        assert!(outcome.is_auto_merged());
        assert!(!outcome.is_clean());
        assert!(!outcome.is_conflict());
        assert_eq!(outcome.content(), b"merged");
    }

    #[test]
    fn test_merge_outcome_conflict() {
        let outcome = MergeOutcome::Conflict {
            base: b"original".to_vec(),
            left: b"left".to_vec(),
            right: b"right".to_vec(),
            left_change: NodeId::new(1),
            right_change: NodeId::new(2),
        };
        assert!(outcome.is_conflict());
        assert!(!outcome.is_clean());
        assert!(!outcome.is_auto_merged());
        // Conflict content returns the left side
        assert_eq!(outcome.content(), b"left");
    }

    #[test]
    fn test_merge_outcome_no_crdt_data() {
        let outcome = MergeOutcome::NoCrdtData;
        assert!(!outcome.is_clean());
        assert!(!outcome.is_auto_merged());
        assert!(!outcome.is_conflict());
        assert_eq!(outcome.content(), b"");
    }

    #[test]
    fn test_merge_outcome_auto_merged_with_sources() {
        let sources = vec![
            MergeSource {
                change_id: NodeId::new(10),
                token_count: 3,
            },
            MergeSource {
                change_id: NodeId::new(20),
                token_count: 5,
            },
        ];
        let outcome = MergeOutcome::AutoMerged {
            content: b"merged content".to_vec(),
            sources,
        };
        assert!(outcome.is_auto_merged());
        if let MergeOutcome::AutoMerged { sources, .. } = &outcome {
            assert_eq!(sources.len(), 2);
            assert_eq!(sources[0].change_id, NodeId::new(10));
            assert_eq!(sources[1].token_count, 5);
        }
    }

    #[test]
    fn test_conflict_group() {
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
        assert_eq!(group.vertex_count(), 2);
        assert!(group.ancestor.is_none());
    }

    #[test]
    fn test_conflict_group_with_ancestor() {
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
        let ancestor = GraphNode::new(
            NodeId::new(0),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let group = ConflictGroup::new(vec![v1, v2]).with_ancestor(ancestor);
        assert_eq!(group.vertex_count(), 2);
        assert!(group.ancestor.is_some());
        assert_eq!(group.ancestor.unwrap().change, NodeId::new(0));
    }
}
