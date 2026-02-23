//! Branch (line-level) structures for the hierarchical CRDT graph.
//!
//! A **Branch** represents a line within a file. It is the middle level
//! of the Trunk → Branch → Leaf hierarchy, containing the tokens (leaves)
//! that make up the line's content.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          Branch (Line)                                   │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  id: BranchId         - Globally unique line identifier                 │
//! │  trunk: TrunkId       - Parent file this line belongs to                │
//! │  state: BranchState   - Alive or Deleted                                │
//! │  line_hash: u64       - Fast equality check (FNV-1a of content)         │
//! └─────────────────────────────────────────────────────────────────────────┘
//!        │
//!        │ contains
//!        ▼
//!   ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐
//!   │  Leaf  │──│  Leaf  │──│  Leaf  │──│  Leaf  │  (tokens)
//!   │  "fn"  │  │  " "   │  │ "main" │  │  "()"  │
//!   └────────┘  └────────┘  └────────┘  └────────┘
//! ```
//!
//! # Operations
//!
//! Lines can be inserted, deleted, and restored via [`BranchOp`]:
//!
//! - [`BranchOp::Insert`] - Insert a new line after a reference point
//! - [`BranchOp::Delete`] - Mark a line as deleted
//! - [`BranchOp::Restore`] - Restore a deleted line
//!
//! # CRDT Semantics
//!
//! Branches follow CRDT principles:
//! - The [`BranchId`] is immutable and globally unique
//! - Insertions reference existing branch IDs (or ROOT for start of file)
//! - Concurrent insertions at the same position are ordered by [`BranchId`]
//! - Deletion is a state change, not data removal

use super::ids::{BranchId, TrunkId};
use super::leaf::LeafOp;
use serde::{Deserialize, Serialize};
use std::fmt;

// BranchState

/// The lifecycle state of a branch (line).
///
/// Lines can transition between states based on operations:
///
/// ```text
///                    ┌──────────────────┐
///                    │                  │
///     Insert ───────►│      Alive       │◄─────── Restore
///                    │                  │
///                    └────────┬─────────┘
///                             │
///                           Delete
///                             │
///                             ▼
///                    ┌──────────────────┐
///                    │                  │
///                    │     Deleted      │
///                    │                  │
///                    └──────────────────┘
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum BranchState {
    /// Line exists and is visible in the file.
    #[default]
    Alive,

    /// Line has been deleted but can be restored.
    Deleted,
}

impl BranchState {
    /// Returns `true` if the branch is alive.
    #[inline]
    pub fn is_alive(&self) -> bool {
        matches!(self, BranchState::Alive)
    }

    /// Returns `true` if the branch is deleted.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, BranchState::Deleted)
    }

    /// Returns the state as a single character for compact display.
    pub fn as_char(&self) -> char {
        match self {
            BranchState::Alive => 'A',
            BranchState::Deleted => 'D',
        }
    }
}

impl fmt::Display for BranchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BranchState::Alive => write!(f, "alive"),
            BranchState::Deleted => write!(f, "deleted"),
        }
    }
}

// Branch

/// A line in the hierarchical CRDT graph.
///
/// The branch is the line-level container that holds all leaves (tokens).
/// It tracks the line's identity, parent file, and lifecycle state.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{Branch, BranchId, TrunkId, BranchState};
/// use atomic_core::types::NodeId;
///
/// let branch = Branch::new(
///     BranchId::new(NodeId::new(1), 0),
///     TrunkId::new(NodeId::new(1), 0),
/// );
///
/// assert!(branch.state().is_alive());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Globally unique identifier for this line.
    id: BranchId,

    /// The trunk (file) this line belongs to.
    trunk: TrunkId,

    /// Current lifecycle state.
    state: BranchState,

    /// Fast content hash for equality checks (FNV-1a).
    /// This is computed from the line's leaf content.
    line_hash: u64,
}

impl Branch {
    /// FNV-1a offset basis for 64-bit hashes.
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;

    /// Creates a new branch with the given properties.
    ///
    /// The branch starts in the [`BranchState::Alive`] state with
    /// an empty content hash.
    pub fn new(id: BranchId, trunk: TrunkId) -> Self {
        Branch {
            id,
            trunk,
            state: BranchState::Alive,
            line_hash: Self::FNV_OFFSET_BASIS,
        }
    }

    /// Creates a new branch with a pre-computed hash.
    pub fn with_hash(id: BranchId, trunk: TrunkId, line_hash: u64) -> Self {
        Branch {
            id,
            trunk,
            state: BranchState::Alive,
            line_hash,
        }
    }

    /// Returns the branch's unique identifier.
    #[inline]
    pub fn id(&self) -> BranchId {
        self.id
    }

    /// Returns the parent trunk's identifier.
    #[inline]
    pub fn trunk(&self) -> TrunkId {
        self.trunk
    }

    /// Returns the current lifecycle state.
    #[inline]
    pub fn state(&self) -> BranchState {
        self.state
    }

    /// Returns the content hash.
    #[inline]
    pub fn line_hash(&self) -> u64 {
        self.line_hash
    }

    /// Sets the branch's state.
    pub fn set_state(&mut self, state: BranchState) {
        self.state = state;
    }

    /// Sets the content hash.
    pub fn set_line_hash(&mut self, hash: u64) {
        self.line_hash = hash;
    }

    /// Marks the branch as deleted.
    pub fn delete(&mut self) {
        self.state = BranchState::Deleted;
    }

    /// Restores a deleted branch to alive.
    pub fn restore(&mut self) {
        self.state = BranchState::Alive;
    }

    /// Returns `true` if this branch has the same content hash as another.
    #[inline]
    pub fn content_eq(&self, other: &Branch) -> bool {
        self.line_hash == other.line_hash
    }
}

impl fmt::Display for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Branch({}, trunk={}, state={})",
            self.id, self.trunk, self.state
        )
    }
}

// BranchOp - Operations on Branches

/// An operation on a branch (line).
///
/// These operations are the CRDT primitives for line-level changes.
/// Each operation is idempotent and commutative when properly ordered.
///
/// # Insertion Semantics
///
/// Insertions specify "insert after" a reference branch:
/// - `after: None` means insert at the start of the file
/// - `after: Some(BranchId::ROOT)` also means start of file
/// - `after: Some(id)` means insert immediately after that branch
///
/// When two concurrent insertions have the same `after` reference,
/// they are ordered deterministically by their [`BranchId`].
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{BranchOp, BranchId};
/// use atomic_core::types::NodeId;
///
/// // Insert a new line at the start of a file
/// let insert_op = BranchOp::Insert {
///     after: None,
///     content: vec![],  // LeafOps for the line's tokens
/// };
///
/// // Delete an existing line
/// let delete_op = BranchOp::Delete {
///     branch: BranchId::new(NodeId::new(1), 0),
///     content: vec![],  // Original line content for diff display
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchOp {
    /// Insert a new line after a reference point.
    ///
    /// The [`BranchId`] for the new line is assigned by the containing change.
    Insert {
        /// The branch to insert after, or `None` for start of file.
        after: Option<BranchId>,
        /// The initial tokens for this line.
        content: Vec<LeafOp>,
    },

    /// Delete an existing line.
    ///
    /// The line's content remains in the graph but is marked as deleted.
    /// The original content is stored for diff display purposes.
    Delete {
        /// The branch to delete.
        branch: BranchId,
        /// The original content of the line (for diff display).
        /// This is a snapshot of the line's tokens at deletion time.
        #[serde(default)]
        content: Vec<LeafOp>,
    },

    /// Modify an existing line (replace its content).
    ///
    /// This is a first-class semantic operation: the line's identity is
    /// preserved but its content changed.  Unlike a Delete+Insert pair,
    /// a Modify explicitly carries both the old and new content so that
    /// every consumer (CLI, WebUI, API) can render word-level diffs
    /// without heuristic re-pairing.
    ///
    /// At the graph layer a Modify is equivalent to deleting the old
    /// branch and inserting a new one, but at the semantic layer it
    /// preserves the relationship between old and new — enabling:
    ///
    /// - Side-by-side diff alignment (the two lines occupy the same row)
    /// - Token-level highlighting (word-diff within the line)
    /// - Accurate blame (the line was *changed*, not removed + added)
    ///
    /// # Backward Compatibility
    ///
    /// Older changes that lack Modify will still have Delete+Insert
    /// pairs.  Display code should handle both representations.
    Modify {
        /// The branch being modified.
        branch: BranchId,
        /// The old content of the line (tokens before the change).
        old_content: Vec<LeafOp>,
        /// The new content of the line (tokens after the change).
        new_content: Vec<LeafOp>,
    },

    /// Restore a deleted line.
    ///
    /// Returns the line to the alive state.
    Restore {
        /// The branch to restore.
        branch: BranchId,
    },
}

impl BranchOp {
    /// Returns the branch ID this operation affects, if any.
    ///
    /// Returns `None` for `Insert` since the ID is assigned later.
    pub fn branch_id(&self) -> Option<BranchId> {
        match self {
            BranchOp::Insert { .. } => None,
            BranchOp::Delete { branch, .. } => Some(*branch),
            BranchOp::Modify { branch, .. } => Some(*branch),
            BranchOp::Restore { branch } => Some(*branch),
        }
    }

    /// Returns `true` if this is an insert operation.
    #[inline]
    pub fn is_insert(&self) -> bool {
        matches!(self, BranchOp::Insert { .. })
    }

    /// Returns `true` if this is a modify operation.
    #[inline]
    pub fn is_modify(&self) -> bool {
        matches!(self, BranchOp::Modify { .. })
    }

    /// Returns the content of the operation (for Insert or Delete).
    ///
    /// For Modify, returns the **new** content.
    /// Returns `None` for `Restore` operations.
    pub fn content(&self) -> Option<&[LeafOp]> {
        match self {
            BranchOp::Insert { content, .. } => Some(content),
            BranchOp::Delete { content, .. } => Some(content),
            BranchOp::Modify { new_content, .. } => Some(new_content),
            BranchOp::Restore { .. } => None,
        }
    }

    /// Returns the old content for a Delete or Modify operation.
    ///
    /// For Delete, this is the content at deletion time.
    /// For Modify, this is the content before the change.
    /// Returns `None` for Insert and Restore.
    pub fn old_content(&self) -> Option<&[LeafOp]> {
        match self {
            BranchOp::Delete { content, .. } => Some(content),
            BranchOp::Modify { old_content, .. } => Some(old_content),
            _ => None,
        }
    }

    /// Returns the new content for an Insert or Modify operation.
    ///
    /// For Insert, this is the initial content.
    /// For Modify, this is the content after the change.
    /// Returns `None` for Delete and Restore.
    pub fn new_content(&self) -> Option<&[LeafOp]> {
        match self {
            BranchOp::Insert { content, .. } => Some(content),
            BranchOp::Modify { new_content, .. } => Some(new_content),
            _ => None,
        }
    }

    /// Returns `true` if this is a delete operation.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self, BranchOp::Delete { .. })
    }

    /// Returns `true` if this is a restore operation.
    #[inline]
    pub fn is_restore(&self) -> bool {
        matches!(self, BranchOp::Restore { .. })
    }

    /// Returns the operation type as a string.
    pub fn op_type(&self) -> &'static str {
        match self {
            BranchOp::Insert { .. } => "insert",
            BranchOp::Delete { .. } => "delete",
            BranchOp::Modify { .. } => "modify",
            BranchOp::Restore { .. } => "restore",
        }
    }

    /// Returns the insertion point for an Insert operation.
    pub fn insert_after(&self) -> Option<Option<BranchId>> {
        match self {
            BranchOp::Insert { after, .. } => Some(*after),
            _ => None,
        }
    }
}

impl fmt::Display for BranchOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BranchOp::Insert { after, content } => {
                write!(f, "insert after ")?;
                match after {
                    Some(id) => write!(f, "{}", id)?,
                    None => write!(f, "START")?,
                }
                write!(f, " ({} tokens)", content.len())
            }
            BranchOp::Delete { branch, .. } => write!(f, "delete {}", branch),
            BranchOp::Modify {
                branch,
                old_content,
                new_content,
            } => {
                write!(
                    f,
                    "modify {} ({} → {} tokens)",
                    branch,
                    old_content.len(),
                    new_content.len()
                )
            }
            BranchOp::Restore { branch } => write!(f, "restore {}", branch),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // -------------------------------------------------------------------------
    // BranchState Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_branch_state_default() {
        assert_eq!(BranchState::default(), BranchState::Alive);
    }

    #[test]
    fn test_branch_state_is_alive() {
        assert!(BranchState::Alive.is_alive());
        assert!(!BranchState::Deleted.is_alive());
    }

    #[test]
    fn test_branch_state_is_deleted() {
        assert!(!BranchState::Alive.is_deleted());
        assert!(BranchState::Deleted.is_deleted());
    }

    #[test]
    fn test_branch_state_as_char() {
        assert_eq!(BranchState::Alive.as_char(), 'A');
        assert_eq!(BranchState::Deleted.as_char(), 'D');
    }

    #[test]
    fn test_branch_state_display() {
        assert_eq!(format!("{}", BranchState::Alive), "alive");
        assert_eq!(format!("{}", BranchState::Deleted), "deleted");
    }

    #[test]
    fn test_branch_state_serde() {
        let state = BranchState::Deleted;
        let json = serde_json::to_string(&state).unwrap();
        let decoded: BranchState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }

    // -------------------------------------------------------------------------
    // Branch Tests
    // -------------------------------------------------------------------------

    fn make_branch() -> Branch {
        Branch::new(
            BranchId::new(NodeId::new(1), 0),
            TrunkId::new(NodeId::new(1), 0),
        )
    }

    #[test]
    fn test_branch_new() {
        let branch = make_branch();
        assert_eq!(branch.id(), BranchId::new(NodeId::new(1), 0));
        assert_eq!(branch.trunk(), TrunkId::new(NodeId::new(1), 0));
        assert!(branch.state().is_alive());
        assert_eq!(branch.line_hash(), Branch::FNV_OFFSET_BASIS);
    }

    #[test]
    fn test_branch_with_hash() {
        let branch = Branch::with_hash(
            BranchId::new(NodeId::new(1), 0),
            TrunkId::new(NodeId::new(1), 0),
            12345,
        );
        assert_eq!(branch.line_hash(), 12345);
    }

    #[test]
    fn test_branch_set_state() {
        let mut branch = make_branch();
        assert!(branch.state().is_alive());

        branch.set_state(BranchState::Deleted);
        assert!(branch.state().is_deleted());
    }

    #[test]
    fn test_branch_set_line_hash() {
        let mut branch = make_branch();
        branch.set_line_hash(99999);
        assert_eq!(branch.line_hash(), 99999);
    }

    #[test]
    fn test_branch_delete_and_restore() {
        let mut branch = make_branch();
        assert!(branch.state().is_alive());

        branch.delete();
        assert!(branch.state().is_deleted());

        branch.restore();
        assert!(branch.state().is_alive());
    }

    #[test]
    fn test_branch_content_eq() {
        let mut branch1 = make_branch();
        let mut branch2 = Branch::new(
            BranchId::new(NodeId::new(2), 0),
            TrunkId::new(NodeId::new(1), 0),
        );

        // Same default hash
        assert!(branch1.content_eq(&branch2));

        // Different hash
        branch1.set_line_hash(111);
        branch2.set_line_hash(222);
        assert!(!branch1.content_eq(&branch2));

        // Same hash again
        branch2.set_line_hash(111);
        assert!(branch1.content_eq(&branch2));
    }

    #[test]
    fn test_branch_display() {
        let branch = make_branch();
        let display = format!("{}", branch);
        assert!(display.contains("Branch"));
        assert!(display.contains("alive"));
    }

    #[test]
    fn test_branch_serde() {
        let branch = make_branch();
        let json = serde_json::to_string(&branch).unwrap();
        let decoded: Branch = serde_json::from_str(&json).unwrap();
        assert_eq!(branch, decoded);
    }

    // -------------------------------------------------------------------------
    // BranchOp Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_branch_op_insert() {
        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        assert!(op.is_insert());
        assert!(!op.is_delete());
        assert!(op.branch_id().is_none());
        assert_eq!(op.op_type(), "insert");
        assert_eq!(op.insert_after(), Some(None));
        assert_eq!(op.content(), Some(&[][..]));
    }

    #[test]
    fn test_branch_op_insert_after() {
        let after_id = BranchId::new(NodeId::new(1), 5);
        let op = BranchOp::Insert {
            after: Some(after_id),
            content: vec![],
        };
        assert_eq!(op.insert_after(), Some(Some(after_id)));
    }

    #[test]
    fn test_branch_op_delete() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let op = BranchOp::Delete {
            branch: branch_id,
            content: Vec::new(),
        };
        assert!(op.is_delete());
        assert!(!op.is_insert());
        assert_eq!(op.branch_id(), Some(branch_id));
        assert_eq!(op.op_type(), "delete");
        assert!(op.insert_after().is_none());
        assert!(op.content().unwrap().is_empty());
    }

    #[test]
    fn test_branch_op_restore() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let op = BranchOp::Restore { branch: branch_id };
        assert!(op.is_restore());
        assert!(!op.is_delete());
        assert_eq!(op.branch_id(), Some(branch_id));
        assert_eq!(op.op_type(), "restore");
    }

    #[test]
    fn test_branch_op_display() {
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        assert!(format!("{}", insert_op).contains("insert"));
        assert!(format!("{}", insert_op).contains("START"));

        let branch_id = BranchId::new(NodeId::new(1), 0);
        let delete_op = BranchOp::Delete {
            branch: branch_id,
            content: Vec::new(),
        };
        assert!(format!("{}", delete_op).contains("delete"));

        let restore_op = BranchOp::Restore { branch: branch_id };
        assert!(format!("{}", restore_op).contains("restore"));
    }

    #[test]
    fn test_branch_op_display_with_after() {
        let after_id = BranchId::new(NodeId::new(1), 5);
        let op = BranchOp::Insert {
            after: Some(after_id),
            content: vec![],
        };
        let display = format!("{}", op);
        assert!(display.contains("B1:5"));
    }

    #[test]
    fn test_branch_op_serde() {
        let op = BranchOp::Insert {
            after: Some(BranchId::new(NodeId::new(1), 0)),
            content: vec![],
        };
        let json = serde_json::to_string(&op).unwrap();
        let decoded: BranchOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, decoded);
    }
}
