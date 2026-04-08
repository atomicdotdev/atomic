//! Types for the semantic merge engine.
//!
//! This module defines the core data structures used by the merge engine
//! to represent conflict groups, individual token edits, and merge
//! provenance information.

use crate::crdt::ids::{BranchId, LeafId};
use crate::types::{GraphNode, NodeId};

/// A group of competing vertices at the same graph position.
///
/// When two or more vertices are alive children of the same parent
/// with no ordering edge between them, they form a conflict group.
/// The merge engine attempts to resolve these by comparing token-level
/// edits from the CRDT semantic layer.
///
/// # Example
///
/// ```rust
/// use atomic_core::merge::ConflictGroup;
/// use atomic_core::types::{GraphNode, NodeId, ChangePosition};
///
/// let v1 = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(10));
/// let v2 = GraphNode::new(NodeId::new(2), ChangePosition::new(0), ChangePosition::new(10));
///
/// let group = ConflictGroup::new(vec![v1, v2]);
/// assert_eq!(group.vertex_count(), 2);
/// assert!(group.ancestor.is_none());
/// ```
#[derive(Debug, Clone)]
pub struct ConflictGroup {
    /// The vertices competing for this position (2 or more).
    pub vertices: Vec<GraphNode<NodeId>>,
    /// The common ancestor vertex (the one they all replaced).
    /// `None` if the ancestor can't be determined.
    pub ancestor: Option<GraphNode<NodeId>>,
    /// The parent vertex we were traversing from when the conflict was found.
    ///
    /// In the `retrieve_graph` DFS, we follow forward edges from a parent to
    /// its children. When two children have no ordering between them, that's a
    /// conflict. The parent is the vertex we were traversing *from*.
    ///
    /// This is needed by `SemanticMergeEngine::find_ancestor` to locate the
    /// dead vertex that both competing changes deleted.
    pub parent: Option<GraphNode<NodeId>>,
    /// The branch (line) these vertices belong to, if known.
    /// Used to scope the token-level diff.
    pub branch_id: Option<BranchId>,
}

impl ConflictGroup {
    /// Creates a new conflict group from competing vertices.
    pub fn new(vertices: Vec<GraphNode<NodeId>>) -> Self {
        Self {
            vertices,
            ancestor: None,
            parent: None,
            branch_id: None,
        }
    }

    /// Sets the common ancestor vertex via builder pattern.
    pub fn with_ancestor(mut self, ancestor: GraphNode<NodeId>) -> Self {
        self.ancestor = Some(ancestor);
        self
    }

    /// Sets the parent vertex via builder pattern.
    pub fn with_parent(mut self, parent: GraphNode<NodeId>) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Sets the branch context via builder pattern.
    pub fn with_branch(mut self, branch_id: BranchId) -> Self {
        self.branch_id = Some(branch_id);
        self
    }

    /// Returns how many vertices are competing at this position.
    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    /// Returns `true` if this is a two-way conflict (the most common case).
    pub fn is_two_way(&self) -> bool {
        self.vertices.len() == 2
    }

    /// Returns `true` if a common ancestor is available for three-way merge.
    pub fn has_ancestor(&self) -> bool {
        self.ancestor.is_some()
    }

    /// Returns `true` if the parent vertex is known.
    pub fn has_parent(&self) -> bool {
        self.parent.is_some()
    }
}

/// Identifies which change contributed content to a merge result.
///
/// After an auto-merge, each `MergeSource` records which change provided
/// tokens and how many were included. This enables accurate blame and
/// provenance tracking through merges.
#[derive(Debug, Clone)]
pub struct MergeSource {
    /// The change that introduced this content.
    pub change_id: NodeId,
    /// How many tokens from this change were included in the merge.
    pub token_count: usize,
}

impl MergeSource {
    /// Creates a new merge source.
    pub fn new(change_id: NodeId, token_count: usize) -> Self {
        Self {
            change_id,
            token_count,
        }
    }
}

/// An edit to a single leaf (token) within a branch (line).
///
/// `LeafEdit` represents the atomic unit of change in the semantic merge
/// engine. During three-way merge, each side's changes are decomposed into
/// a sequence of `LeafEdit` operations. If the two sides' edits don't
/// overlap (i.e., they touch different `LeafId`s), the merge succeeds
/// automatically.
///
/// # Variants
///
/// | Variant | Meaning |
/// |---------|---------|
/// | `Keep` | Token unchanged from base |
/// | `Replace` | Token content was modified |
/// | `Delete` | Token was removed |
/// | `Insert` | New token was added |
#[derive(Debug, Clone, PartialEq)]
pub enum LeafEdit {
    /// Token was kept unchanged from the base version.
    Keep {
        /// The leaf being kept.
        leaf_id: LeafId,
    },

    /// Token was replaced with new content.
    Replace {
        /// The leaf being replaced.
        leaf_id: LeafId,
        /// The original content from the base.
        old_content: Vec<u8>,
        /// The new content from this side.
        new_content: Vec<u8>,
    },

    /// Token was deleted.
    Delete {
        /// The leaf being deleted.
        leaf_id: LeafId,
    },

    /// New token was inserted after an existing token (or at the start).
    Insert {
        /// The leaf after which this token is inserted.
        /// `None` means insert at the beginning of the line.
        after: Option<LeafId>,
        /// The content of the new token.
        content: Vec<u8>,
    },
}

impl LeafEdit {
    /// Returns the `LeafId` this edit targets, if applicable.
    ///
    /// `Insert` edits create new tokens and don't target an existing leaf
    /// directly, so they return `None`.
    pub fn target_leaf(&self) -> Option<LeafId> {
        match self {
            LeafEdit::Keep { leaf_id } => Some(*leaf_id),
            LeafEdit::Replace { leaf_id, .. } => Some(*leaf_id),
            LeafEdit::Delete { leaf_id } => Some(*leaf_id),
            LeafEdit::Insert { .. } => None,
        }
    }

    /// Returns `true` if this edit modifies content (not a keep).
    pub fn is_modification(&self) -> bool {
        !matches!(self, LeafEdit::Keep { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    #[test]
    fn test_conflict_group_new() {
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
        assert!(group.is_two_way());
        assert!(group.ancestor.is_none());
        assert!(group.branch_id.is_none());
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
        assert!(group.has_ancestor());
        assert_eq!(group.ancestor.unwrap().change, NodeId::new(0));
    }

    #[test]
    fn test_conflict_group_with_branch() {
        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let branch = BranchId::new(NodeId::new(1), 0);

        let group = ConflictGroup::new(vec![v1]).with_branch(branch);
        assert_eq!(group.branch_id, Some(branch));
    }

    #[test]
    fn test_conflict_group_not_two_way() {
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
        let v3 = GraphNode::new(
            NodeId::new(3),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );

        let group = ConflictGroup::new(vec![v1, v2, v3]);
        assert!(!group.is_two_way());
        assert_eq!(group.vertex_count(), 3);
    }

    #[test]
    fn test_merge_source() {
        let src = MergeSource::new(NodeId::new(42), 5);
        assert_eq!(src.change_id, NodeId::new(42));
        assert_eq!(src.token_count, 5);
    }

    #[test]
    fn test_leaf_edit_keep() {
        let leaf = LeafId::new(NodeId::new(1), 0);
        let edit = LeafEdit::Keep { leaf_id: leaf };

        assert_eq!(edit.target_leaf(), Some(leaf));
        assert!(!edit.is_modification());
    }

    #[test]
    fn test_leaf_edit_replace() {
        let leaf = LeafId::new(NodeId::new(1), 0);
        let edit = LeafEdit::Replace {
            leaf_id: leaf,
            old_content: b"foo".to_vec(),
            new_content: b"bar".to_vec(),
        };

        assert_eq!(edit.target_leaf(), Some(leaf));
        assert!(edit.is_modification());
    }

    #[test]
    fn test_leaf_edit_delete() {
        let leaf = LeafId::new(NodeId::new(1), 0);
        let edit = LeafEdit::Delete { leaf_id: leaf };

        assert_eq!(edit.target_leaf(), Some(leaf));
        assert!(edit.is_modification());
    }

    #[test]
    fn test_leaf_edit_insert() {
        let after = LeafId::new(NodeId::new(1), 0);
        let edit = LeafEdit::Insert {
            after: Some(after),
            content: b"new".to_vec(),
        };

        assert_eq!(edit.target_leaf(), None);
        assert!(edit.is_modification());
    }

    #[test]
    fn test_leaf_edit_insert_at_start() {
        let edit = LeafEdit::Insert {
            after: None,
            content: b"start".to_vec(),
        };

        assert_eq!(edit.target_leaf(), None);
        assert!(edit.is_modification());
    }

    #[test]
    fn test_leaf_edit_equality() {
        let leaf = LeafId::new(NodeId::new(1), 0);
        let a = LeafEdit::Keep { leaf_id: leaf };
        let b = LeafEdit::Keep { leaf_id: leaf };
        let c = LeafEdit::Delete { leaf_id: leaf };

        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
