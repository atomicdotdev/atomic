//! Leaf (token-level) structures for the hierarchical CRDT graph.
//!
//! A **Leaf** represents a token within a line. It is the bottom level
//! of the Trunk → Branch → Leaf hierarchy, containing the actual content
//! bytes and their semantic classification.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           Leaf (Token)                                   │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  id: LeafId           - Globally unique token identifier                │
//! │  branch: BranchId     - Parent line this token belongs to               │
//! │  kind: TokenKind      - Semantic classification (Word, Operator, etc.)  │
//! │  content: Range<u32>  - Byte range in the content blob                  │
//! │  state: LeafState     - Alive or Deleted                                │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Token Kinds
//!
//! Tokens are classified using [`TokenKind`] from the diff module:
//! - `Word` - Identifiers and keywords
//! - `Whitespace` - Spaces and tabs
//! - `Operator` - Symbols like `+`, `->`, `==`
//! - `Punctuation` - Structural chars like `(`, `)`, `;`
//! - `String` - String literals
//! - `Number` - Numeric literals
//! - `Comment` - Comments
//! - `Newline` - Line endings (marks branch boundaries)
//! - `Other` - Everything else
//!
//! # Operations
//!
//! Tokens can be inserted, deleted, and replaced via [`LeafOp`]:
//!
//! - [`LeafOp::Insert`] - Insert a new token after a reference point
//! - [`LeafOp::Delete`] - Mark a token as deleted
//! - [`LeafOp::Replace`] - Replace token content (preserves ID for blame)
//!
//! # CRDT Semantics
//!
//! Leaves follow CRDT principles:
//! - The [`LeafId`] is immutable and globally unique
//! - Insertions reference existing leaf IDs (or ROOT for start of line)
//! - Concurrent insertions at the same position are ordered by [`LeafId`]
//! - Deletion is a state change, not data removal
//! - Replace preserves the ID, enabling accurate blame tracking

use super::ids::{BranchId, LeafId};
use crate::diff::token::TokenKind;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

// LeafState

/// The lifecycle state of a leaf (token).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum LeafState {
    /// Token exists and is visible in the line.
    #[default]
    Alive,

    /// Token has been deleted but can be restored.
    Deleted,
}

impl LeafState {
    /// Returns `true` if the leaf is alive.
    #[inline]
    pub fn is_alive(&self) -> bool {
        matches!(self, LeafState::Alive)
    }

    /// Returns `true` if the leaf is deleted.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, LeafState::Deleted)
    }

    /// Returns the state as a single character for compact display.
    pub fn as_char(&self) -> char {
        match self {
            LeafState::Alive => 'A',
            LeafState::Deleted => 'D',
        }
    }
}

impl fmt::Display for LeafState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeafState::Alive => write!(f, "alive"),
            LeafState::Deleted => write!(f, "deleted"),
        }
    }
}

// Leaf

/// A token in the hierarchical CRDT graph.
///
/// The leaf is the token-level unit that holds actual content.
/// It tracks the token's identity, parent line, type, content location,
/// and lifecycle state.
///
/// # Content Storage
///
/// Token content is stored in a separate content blob, with each leaf
/// holding a byte range into that blob. This enables efficient storage
/// and deduplication.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{Leaf, LeafId, BranchId, LeafState};
/// use atomic_core::diff::token::TokenKind;
/// use atomic_core::types::NodeId;
///
/// let leaf = Leaf::new(
///     LeafId::new(NodeId::new(1), 0),
///     BranchId::new(NodeId::new(1), 0),
///     TokenKind::Word,
///     0..5,  // "hello"
/// );
///
/// assert!(leaf.state().is_alive());
/// assert_eq!(leaf.kind(), TokenKind::Word);
/// assert_eq!(leaf.content_range(), 0..5);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    /// Globally unique identifier for this token.
    id: LeafId,

    /// The branch (line) this token belongs to.
    branch: BranchId,

    /// Semantic classification of this token.
    kind: TokenKind,

    /// Byte range in the content blob.
    content_start: u32,
    content_end: u32,

    /// Current lifecycle state.
    state: LeafState,
}

impl Leaf {
    /// Creates a new leaf with the given properties.
    ///
    /// The leaf starts in the [`LeafState::Alive`] state.
    pub fn new(id: LeafId, branch: BranchId, kind: TokenKind, content_range: Range<u32>) -> Self {
        Leaf {
            id,
            branch,
            kind,
            content_start: content_range.start,
            content_end: content_range.end,
            state: LeafState::Alive,
        }
    }

    /// Returns the leaf's unique identifier.
    #[inline]
    pub fn id(&self) -> LeafId {
        self.id
    }

    /// Returns the parent branch's identifier.
    #[inline]
    pub fn branch(&self) -> BranchId {
        self.branch
    }

    /// Returns the token kind.
    #[inline]
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// Returns the content byte range.
    #[inline]
    pub fn content_range(&self) -> Range<u32> {
        self.content_start..self.content_end
    }

    /// Returns the content length in bytes.
    #[inline]
    pub fn content_len(&self) -> u32 {
        self.content_end - self.content_start
    }

    /// Returns `true` if this is an empty token.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content_start == self.content_end
    }

    /// Returns the current lifecycle state.
    #[inline]
    pub fn state(&self) -> LeafState {
        self.state
    }

    /// Returns `true` if this token is significant for diffing.
    ///
    /// Non-significant tokens (whitespace, newlines) can optionally
    /// be ignored during comparison.
    #[inline]
    pub fn is_significant(&self) -> bool {
        self.kind.is_significant()
    }

    /// Returns `true` if this is a whitespace token.
    #[inline]
    pub fn is_whitespace(&self) -> bool {
        self.kind.is_whitespace()
    }

    /// Sets the leaf's state.
    pub fn set_state(&mut self, state: LeafState) {
        self.state = state;
    }

    /// Sets the token kind.
    pub fn set_kind(&mut self, kind: TokenKind) {
        self.kind = kind;
    }

    /// Sets the content range.
    pub fn set_content_range(&mut self, range: Range<u32>) {
        self.content_start = range.start;
        self.content_end = range.end;
    }

    /// Marks the leaf as deleted.
    pub fn delete(&mut self) {
        self.state = LeafState::Deleted;
    }

    /// Restores a deleted leaf to alive.
    pub fn restore(&mut self) {
        self.state = LeafState::Alive;
    }
}

impl fmt::Display for Leaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Leaf({}, {:?}, {}..{}, state={})",
            self.id, self.kind, self.content_start, self.content_end, self.state
        )
    }
}

// LeafOp - Operations on Leaves

/// An operation on a leaf (token).
///
/// These operations are the CRDT primitives for token-level changes.
/// Each operation is idempotent and commutative when properly ordered.
///
/// # Insertion Semantics
///
/// Insertions specify "insert after" a reference leaf:
/// - `after: None` means insert at the start of the line
/// - `after: Some(LeafId::ROOT)` also means start of line
/// - `after: Some(id)` means insert immediately after that leaf
///
/// When two concurrent insertions have the same `after` reference,
/// they are ordered deterministically by their [`LeafId`].
///
/// # Replace Semantics
///
/// Replace changes the content of a token while preserving its ID.
/// This is crucial for accurate blame/credit tracking - a replaced
/// token maintains its attribution to the original author, with the
/// replacement recorded as a separate event.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{LeafOp, LeafId};
/// use atomic_core::diff::token::TokenKind;
/// use atomic_core::types::NodeId;
///
/// // Insert a new token at the start of a line
/// let insert_op = LeafOp::Insert {
///     after: None,
///     kind: TokenKind::Word,
///     content: b"hello".to_vec(),
/// };
///
/// // Delete an existing token
/// let delete_op = LeafOp::Delete {
///     leaf: LeafId::new(NodeId::new(1), 0),
/// };
///
/// // Replace token content (preserves ID for blame)
/// let replace_op = LeafOp::Replace {
///     leaf: LeafId::new(NodeId::new(1), 0),
///     new_content: b"world".to_vec(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeafOp {
    /// Insert a new token after a reference point.
    ///
    /// The [`LeafId`] for the new token is assigned by the containing change.
    Insert {
        /// The leaf to insert after, or `None` for start of line.
        after: Option<LeafId>,
        /// The semantic type of the token.
        kind: TokenKind,
        /// The token's content bytes.
        content: Vec<u8>,
    },

    /// Delete an existing token.
    ///
    /// The token's content remains in the graph but is marked as deleted.
    Delete {
        /// The leaf to delete.
        leaf: LeafId,
    },

    /// Replace a token's content.
    ///
    /// The token's ID is preserved, enabling accurate blame tracking.
    /// The previous content remains in history.
    Replace {
        /// The leaf to modify.
        leaf: LeafId,
        /// The new content bytes.
        new_content: Vec<u8>,
    },

    /// Restore a deleted token.
    ///
    /// Returns the token to the alive state.
    Restore {
        /// The leaf to restore.
        leaf: LeafId,
    },
}

impl LeafOp {
    /// Returns the leaf ID this operation affects, if any.
    ///
    /// Returns `None` for `Insert` since the ID is assigned later.
    pub fn leaf_id(&self) -> Option<LeafId> {
        match self {
            LeafOp::Insert { .. } => None,
            LeafOp::Delete { leaf } => Some(*leaf),
            LeafOp::Replace { leaf, .. } => Some(*leaf),
            LeafOp::Restore { leaf } => Some(*leaf),
        }
    }

    /// Returns `true` if this is an insert operation.
    #[inline]
    pub fn is_insert(&self) -> bool {
        matches!(self, LeafOp::Insert { .. })
    }

    /// Returns `true` if this is a delete operation.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self, LeafOp::Delete { .. })
    }

    /// Returns `true` if this is a replace operation.
    #[inline]
    pub fn is_replace(&self) -> bool {
        matches!(self, LeafOp::Replace { .. })
    }

    /// Returns `true` if this is a restore operation.
    #[inline]
    pub fn is_restore(&self) -> bool {
        matches!(self, LeafOp::Restore { .. })
    }

    /// Returns the operation type as a string.
    pub fn op_type(&self) -> &'static str {
        match self {
            LeafOp::Insert { .. } => "insert",
            LeafOp::Delete { .. } => "delete",
            LeafOp::Replace { .. } => "replace",
            LeafOp::Restore { .. } => "restore",
        }
    }

    /// Returns the insertion point for an Insert operation.
    pub fn insert_after(&self) -> Option<Option<LeafId>> {
        match self {
            LeafOp::Insert { after, .. } => Some(*after),
            _ => None,
        }
    }

    /// Returns the content for Insert or Replace operations.
    pub fn content(&self) -> Option<&[u8]> {
        match self {
            LeafOp::Insert { content, .. } => Some(content),
            LeafOp::Replace { new_content, .. } => Some(new_content),
            _ => None,
        }
    }

    /// Returns the token kind for an Insert operation.
    pub fn token_kind(&self) -> Option<TokenKind> {
        match self {
            LeafOp::Insert { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Returns the content length in bytes.
    pub fn content_len(&self) -> usize {
        match self {
            LeafOp::Insert { content, .. } => content.len(),
            LeafOp::Replace { new_content, .. } => new_content.len(),
            _ => 0,
        }
    }
}

impl fmt::Display for LeafOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeafOp::Insert {
                after,
                kind,
                content,
            } => {
                write!(f, "insert {:?} after ", kind)?;
                match after {
                    Some(id) => write!(f, "{}", id)?,
                    None => write!(f, "START")?,
                }
                write!(f, " ({} bytes)", content.len())
            }
            LeafOp::Delete { leaf } => write!(f, "delete {}", leaf),
            LeafOp::Replace { leaf, new_content } => {
                write!(f, "replace {} ({} bytes)", leaf, new_content.len())
            }
            LeafOp::Restore { leaf } => write!(f, "restore {}", leaf),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // -------------------------------------------------------------------------
    // LeafState Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_leaf_state_default() {
        assert_eq!(LeafState::default(), LeafState::Alive);
    }

    #[test]
    fn test_leaf_state_is_alive() {
        assert!(LeafState::Alive.is_alive());
        assert!(!LeafState::Deleted.is_alive());
    }

    #[test]
    fn test_leaf_state_is_deleted() {
        assert!(!LeafState::Alive.is_deleted());
        assert!(LeafState::Deleted.is_deleted());
    }

    #[test]
    fn test_leaf_state_as_char() {
        assert_eq!(LeafState::Alive.as_char(), 'A');
        assert_eq!(LeafState::Deleted.as_char(), 'D');
    }

    #[test]
    fn test_leaf_state_display() {
        assert_eq!(format!("{}", LeafState::Alive), "alive");
        assert_eq!(format!("{}", LeafState::Deleted), "deleted");
    }

    #[test]
    fn test_leaf_state_serde() {
        let state = LeafState::Deleted;
        let json = serde_json::to_string(&state).unwrap();
        let decoded: LeafState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }

    // -------------------------------------------------------------------------
    // Leaf Tests
    // -------------------------------------------------------------------------

    fn make_leaf() -> Leaf {
        Leaf::new(
            LeafId::new(NodeId::new(1), 0),
            BranchId::new(NodeId::new(1), 0),
            TokenKind::Word,
            0..5,
        )
    }

    #[test]
    fn test_leaf_new() {
        let leaf = make_leaf();
        assert_eq!(leaf.id(), LeafId::new(NodeId::new(1), 0));
        assert_eq!(leaf.branch(), BranchId::new(NodeId::new(1), 0));
        assert_eq!(leaf.kind(), TokenKind::Word);
        assert_eq!(leaf.content_range(), 0..5);
        assert_eq!(leaf.content_len(), 5);
        assert!(leaf.state().is_alive());
    }

    #[test]
    fn test_leaf_is_empty() {
        let empty_leaf = Leaf::new(
            LeafId::new(NodeId::new(1), 0),
            BranchId::new(NodeId::new(1), 0),
            TokenKind::Whitespace,
            10..10,
        );
        assert!(empty_leaf.is_empty());
        assert!(!make_leaf().is_empty());
    }

    #[test]
    fn test_leaf_is_significant() {
        let word_leaf = make_leaf();
        assert!(word_leaf.is_significant());

        let ws_leaf = Leaf::new(
            LeafId::new(NodeId::new(1), 1),
            BranchId::new(NodeId::new(1), 0),
            TokenKind::Whitespace,
            5..6,
        );
        assert!(!ws_leaf.is_significant());
    }

    #[test]
    fn test_leaf_is_whitespace() {
        let word_leaf = make_leaf();
        assert!(!word_leaf.is_whitespace());

        let ws_leaf = Leaf::new(
            LeafId::new(NodeId::new(1), 1),
            BranchId::new(NodeId::new(1), 0),
            TokenKind::Whitespace,
            5..6,
        );
        assert!(ws_leaf.is_whitespace());
    }

    #[test]
    fn test_leaf_set_state() {
        let mut leaf = make_leaf();
        assert!(leaf.state().is_alive());

        leaf.set_state(LeafState::Deleted);
        assert!(leaf.state().is_deleted());
    }

    #[test]
    fn test_leaf_set_kind() {
        let mut leaf = make_leaf();
        assert_eq!(leaf.kind(), TokenKind::Word);

        leaf.set_kind(TokenKind::Number);
        assert_eq!(leaf.kind(), TokenKind::Number);
    }

    #[test]
    fn test_leaf_set_content_range() {
        let mut leaf = make_leaf();
        assert_eq!(leaf.content_range(), 0..5);

        leaf.set_content_range(10..20);
        assert_eq!(leaf.content_range(), 10..20);
        assert_eq!(leaf.content_len(), 10);
    }

    #[test]
    fn test_leaf_delete_and_restore() {
        let mut leaf = make_leaf();
        assert!(leaf.state().is_alive());

        leaf.delete();
        assert!(leaf.state().is_deleted());

        leaf.restore();
        assert!(leaf.state().is_alive());
    }

    #[test]
    fn test_leaf_display() {
        let leaf = make_leaf();
        let display = format!("{}", leaf);
        assert!(display.contains("Leaf"));
        assert!(display.contains("Word"));
        assert!(display.contains("alive"));
    }

    #[test]
    fn test_leaf_serde() {
        let leaf = make_leaf();
        let json = serde_json::to_string(&leaf).unwrap();
        let decoded: Leaf = serde_json::from_str(&json).unwrap();
        assert_eq!(leaf, decoded);
    }

    // -------------------------------------------------------------------------
    // LeafOp Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_leaf_op_insert() {
        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        assert!(op.is_insert());
        assert!(!op.is_delete());
        assert!(op.leaf_id().is_none());
        assert_eq!(op.op_type(), "insert");
        assert_eq!(op.insert_after(), Some(None));
        assert_eq!(op.content(), Some(&b"hello"[..]));
        assert_eq!(op.token_kind(), Some(TokenKind::Word));
        assert_eq!(op.content_len(), 5);
    }

    #[test]
    fn test_leaf_op_insert_after() {
        let after_id = LeafId::new(NodeId::new(1), 5);
        let op = LeafOp::Insert {
            after: Some(after_id),
            kind: TokenKind::Operator,
            content: b"+".to_vec(),
        };
        assert_eq!(op.insert_after(), Some(Some(after_id)));
    }

    #[test]
    fn test_leaf_op_delete() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let op = LeafOp::Delete { leaf: leaf_id };
        assert!(op.is_delete());
        assert!(!op.is_insert());
        assert_eq!(op.leaf_id(), Some(leaf_id));
        assert_eq!(op.op_type(), "delete");
        assert!(op.insert_after().is_none());
        assert!(op.content().is_none());
        assert!(op.token_kind().is_none());
        assert_eq!(op.content_len(), 0);
    }

    #[test]
    fn test_leaf_op_replace() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"world".to_vec(),
        };
        assert!(op.is_replace());
        assert!(!op.is_delete());
        assert_eq!(op.leaf_id(), Some(leaf_id));
        assert_eq!(op.op_type(), "replace");
        assert_eq!(op.content(), Some(&b"world"[..]));
        assert_eq!(op.content_len(), 5);
    }

    #[test]
    fn test_leaf_op_restore() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let op = LeafOp::Restore { leaf: leaf_id };
        assert!(op.is_restore());
        assert!(!op.is_delete());
        assert_eq!(op.leaf_id(), Some(leaf_id));
        assert_eq!(op.op_type(), "restore");
    }

    #[test]
    fn test_leaf_op_display() {
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"test".to_vec(),
        };
        let display = format!("{}", insert_op);
        assert!(display.contains("insert"));
        assert!(display.contains("Word"));
        assert!(display.contains("START"));
        assert!(display.contains("4 bytes"));

        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let delete_op = LeafOp::Delete { leaf: leaf_id };
        assert!(format!("{}", delete_op).contains("delete"));

        let replace_op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"new".to_vec(),
        };
        assert!(format!("{}", replace_op).contains("replace"));

        let restore_op = LeafOp::Restore { leaf: leaf_id };
        assert!(format!("{}", restore_op).contains("restore"));
    }

    #[test]
    fn test_leaf_op_display_with_after() {
        let after_id = LeafId::new(NodeId::new(1), 5);
        let op = LeafOp::Insert {
            after: Some(after_id),
            kind: TokenKind::Number,
            content: b"42".to_vec(),
        };
        let display = format!("{}", op);
        assert!(display.contains("L1:5"));
    }

    #[test]
    fn test_leaf_op_serde() {
        let op = LeafOp::Insert {
            after: Some(LeafId::new(NodeId::new(1), 0)),
            kind: TokenKind::String,
            content: b"\"hello\"".to_vec(),
        };
        let json = serde_json::to_string(&op).unwrap();
        let decoded: LeafOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, decoded);
    }

    #[test]
    fn test_leaf_op_replace_serde() {
        let op = LeafOp::Replace {
            leaf: LeafId::new(NodeId::new(2), 3),
            new_content: b"replaced".to_vec(),
        };
        let json = serde_json::to_string(&op).unwrap();
        let decoded: LeafOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, decoded);
    }
}
