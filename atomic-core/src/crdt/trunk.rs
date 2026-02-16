//! Trunk (file-level) structures for the hierarchical CRDT graph.
//!
//! A **Trunk** represents a file in the repository. It is the top level
//! of the Trunk → Branch → Leaf hierarchy, containing metadata about
//! the file and serving as the anchor for all lines (branches) within it.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           Trunk (File)                                   │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  id: TrunkId          - Globally unique file identifier                 │
//! │  inode: Inode         - Stable filesystem reference (survives renames)  │
//! │  state: TrunkState    - Alive, Deleted, or Zombie                       │
//! │  encoding: Encoding   - Text encoding (UTF-8, binary, etc.)             │
//! └─────────────────────────────────────────────────────────────────────────┘
//!        │
//!        │ contains
//!        ▼
//!   ┌─────────┐  ┌─────────┐  ┌─────────┐
//!   │ Branch  │──│ Branch  │──│ Branch  │  (lines)
//!   └─────────┘  └─────────┘  └─────────┘
//! ```
//!
//! # Operations
//!
//! Files can be created, deleted, moved, and undeleted via [`TrunkOp`]:
//!
//! - [`TrunkOp::Create`] - Create a new file at a path
//! - [`TrunkOp::Delete`] - Mark a file as deleted
//! - [`TrunkOp::Move`] - Change a file's path (rename/move)
//! - [`TrunkOp::Undelete`] - Restore a deleted file
//!
//! # CRDT Semantics
//!
//! Trunks follow CRDT principles:
//! - The [`TrunkId`] is immutable and globally unique
//! - Deletion is a state change, not data removal
//! - Concurrent operations are resolved deterministically

use super::ids::TrunkId;
use crate::change::Encoding;
use crate::types::Inode;
use serde::{Deserialize, Serialize};
use std::fmt;

// TrunkState

/// The lifecycle state of a trunk (file).
///
/// Files can transition between states based on operations:
///
/// ```text
///                    ┌──────────────────┐
///                    │                  │
///     Create ───────►│      Alive       │◄─────── Undelete
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
pub enum TrunkState {
    /// File exists and is visible in the working copy.
    #[default]
    Alive,

    /// File has been deleted but can be restored.
    Deleted,

    /// File was deleted but has live content referencing it.
    /// This occurs during conflict resolution.
    Zombie,
}

impl TrunkState {
    /// Returns `true` if the trunk is alive.
    #[inline]
    pub fn is_alive(&self) -> bool {
        matches!(self, TrunkState::Alive)
    }

    /// Returns `true` if the trunk is deleted.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, TrunkState::Deleted)
    }

    /// Returns `true` if the trunk is a zombie.
    #[inline]
    pub fn is_zombie(&self) -> bool {
        matches!(self, TrunkState::Zombie)
    }

    /// Returns `true` if content should be output for this state.
    #[inline]
    pub fn should_output(&self) -> bool {
        matches!(self, TrunkState::Alive | TrunkState::Zombie)
    }

    /// Returns the state as a single character for compact display.
    pub fn as_char(&self) -> char {
        match self {
            TrunkState::Alive => 'A',
            TrunkState::Deleted => 'D',
            TrunkState::Zombie => 'Z',
        }
    }
}

impl fmt::Display for TrunkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrunkState::Alive => write!(f, "alive"),
            TrunkState::Deleted => write!(f, "deleted"),
            TrunkState::Zombie => write!(f, "zombie"),
        }
    }
}

// Trunk

/// A file in the hierarchical CRDT graph.
///
/// The trunk is the file-level container that holds all branches (lines).
/// It tracks the file's identity, path, encoding, and lifecycle state.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{Trunk, TrunkId, TrunkState};
/// use atomic_core::change::Encoding;
/// use atomic_core::types::{NodeId, Inode};
///
/// let trunk = Trunk::new(
///     TrunkId::new(NodeId::new(1), 0),
///     Inode::new(42),
///     "src/main.rs".to_string(),
///     Some(Encoding::Utf8),
/// );
///
/// assert!(trunk.state().is_alive());
/// assert_eq!(trunk.path(), "src/main.rs");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trunk {
    /// Globally unique identifier for this file.
    id: TrunkId,

    /// Stable filesystem reference that survives renames.
    inode: Inode,

    /// Current file path (relative to repository root).
    path: String,

    /// Text encoding, if known.
    encoding: Option<Encoding>,

    /// Current lifecycle state.
    state: TrunkState,
}

impl Trunk {
    /// Creates a new trunk with the given properties.
    ///
    /// The trunk starts in the [`TrunkState::Alive`] state.
    pub fn new(
        id: TrunkId,
        inode: Inode,
        path: String,
        encoding: Option<Encoding>,
    ) -> Self {
        Trunk {
            id,
            inode,
            path,
            encoding,
            state: TrunkState::Alive,
        }
    }

    /// Returns the trunk's unique identifier.
    #[inline]
    pub fn id(&self) -> TrunkId {
        self.id
    }

    /// Returns the trunk's inode.
    #[inline]
    pub fn inode(&self) -> Inode {
        self.inode
    }

    /// Returns the current file path.
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the text encoding, if known.
    #[inline]
    pub fn encoding(&self) -> Option<Encoding> {
        self.encoding
    }

    /// Returns the current lifecycle state.
    #[inline]
    pub fn state(&self) -> TrunkState {
        self.state
    }

    /// Returns `true` if this is a text file.
    #[inline]
    pub fn is_text(&self) -> bool {
        self.encoding.map_or(false, |e| e.is_text())
    }

    /// Returns `true` if this is a binary file.
    #[inline]
    pub fn is_binary(&self) -> bool {
        self.encoding == Some(Encoding::Binary)
    }

    /// Sets the trunk's state.
    pub fn set_state(&mut self, state: TrunkState) {
        self.state = state;
    }

    /// Sets the trunk's path (for move operations).
    pub fn set_path(&mut self, path: String) {
        self.path = path;
    }

    /// Sets the trunk's encoding.
    pub fn set_encoding(&mut self, encoding: Option<Encoding>) {
        self.encoding = encoding;
    }

    /// Marks the trunk as deleted.
    pub fn delete(&mut self) {
        self.state = TrunkState::Deleted;
    }

    /// Restores a deleted trunk to alive.
    pub fn undelete(&mut self) {
        self.state = TrunkState::Alive;
    }
}

impl fmt::Display for Trunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Trunk({}, {}, state={})",
            self.id,
            self.path,
            self.state
        )
    }
}

// TrunkOp - Operations on Trunks

/// An operation on a trunk (file).
///
/// These operations are the CRDT primitives for file-level changes.
/// Each operation is idempotent and commutative when properly ordered.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::{TrunkOp, TrunkId};
/// use atomic_core::change::Encoding;
/// use atomic_core::types::NodeId;
///
/// // Create a new file
/// let create_op = TrunkOp::Create {
///     path: "README.md".to_string(),
///     encoding: Some(Encoding::Utf8),
/// };
///
/// // Move/rename a file
/// let move_op = TrunkOp::Move {
///     trunk: TrunkId::new(NodeId::new(1), 0),
///     new_path: "docs/README.md".to_string(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrunkOp {
    /// Create a new file.
    ///
    /// The [`TrunkId`] is assigned by the change that contains this op.
    Create {
        /// Initial file path.
        path: String,
        /// Text encoding (None for binary or unknown).
        encoding: Option<Encoding>,
    },

    /// Delete an existing file.
    ///
    /// The file's content remains in the graph but is marked as deleted.
    Delete {
        /// The trunk to delete.
        trunk: TrunkId,
    },

    /// Move or rename a file.
    ///
    /// This changes the file's path while preserving its identity.
    Move {
        /// The trunk to move.
        trunk: TrunkId,
        /// The new path.
        new_path: String,
    },

    /// Restore a deleted file.
    ///
    /// Returns the file to the alive state.
    Undelete {
        /// The trunk to restore.
        trunk: TrunkId,
    },
}

impl TrunkOp {
    /// Returns the trunk ID this operation affects, if any.
    ///
    /// Returns `None` for `Create` since the ID is assigned later.
    pub fn trunk_id(&self) -> Option<TrunkId> {
        match self {
            TrunkOp::Create { .. } => None,
            TrunkOp::Delete { trunk } => Some(*trunk),
            TrunkOp::Move { trunk, .. } => Some(*trunk),
            TrunkOp::Undelete { trunk } => Some(*trunk),
        }
    }

    /// Returns `true` if this is a create operation.
    #[inline]
    pub fn is_create(&self) -> bool {
        matches!(self, TrunkOp::Create { .. })
    }

    /// Returns `true` if this is a delete operation.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self, TrunkOp::Delete { .. })
    }

    /// Returns `true` if this is a move operation.
    #[inline]
    pub fn is_move(&self) -> bool {
        matches!(self, TrunkOp::Move { .. })
    }

    /// Returns `true` if this is an undelete operation.
    #[inline]
    pub fn is_undelete(&self) -> bool {
        matches!(self, TrunkOp::Undelete { .. })
    }

    /// Returns the operation type as a string.
    pub fn op_type(&self) -> &'static str {
        match self {
            TrunkOp::Create { .. } => "create",
            TrunkOp::Delete { .. } => "delete",
            TrunkOp::Move { .. } => "move",
            TrunkOp::Undelete { .. } => "undelete",
        }
    }
}

impl fmt::Display for TrunkOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrunkOp::Create { path, encoding } => {
                write!(f, "create {:?}", path)?;
                if let Some(enc) = encoding {
                    write!(f, " ({})", enc)?;
                }
                Ok(())
            }
            TrunkOp::Delete { trunk } => write!(f, "delete {}", trunk),
            TrunkOp::Move { trunk, new_path } => {
                write!(f, "move {} -> {:?}", trunk, new_path)
            }
            TrunkOp::Undelete { trunk } => write!(f, "undelete {}", trunk),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // -------------------------------------------------------------------------
    // TrunkState Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_state_default() {
        assert_eq!(TrunkState::default(), TrunkState::Alive);
    }

    #[test]
    fn test_trunk_state_is_alive() {
        assert!(TrunkState::Alive.is_alive());
        assert!(!TrunkState::Deleted.is_alive());
        assert!(!TrunkState::Zombie.is_alive());
    }

    #[test]
    fn test_trunk_state_is_deleted() {
        assert!(!TrunkState::Alive.is_deleted());
        assert!(TrunkState::Deleted.is_deleted());
        assert!(!TrunkState::Zombie.is_deleted());
    }

    #[test]
    fn test_trunk_state_is_zombie() {
        assert!(!TrunkState::Alive.is_zombie());
        assert!(!TrunkState::Deleted.is_zombie());
        assert!(TrunkState::Zombie.is_zombie());
    }

    #[test]
    fn test_trunk_state_should_output() {
        assert!(TrunkState::Alive.should_output());
        assert!(!TrunkState::Deleted.should_output());
        assert!(TrunkState::Zombie.should_output());
    }

    #[test]
    fn test_trunk_state_as_char() {
        assert_eq!(TrunkState::Alive.as_char(), 'A');
        assert_eq!(TrunkState::Deleted.as_char(), 'D');
        assert_eq!(TrunkState::Zombie.as_char(), 'Z');
    }

    #[test]
    fn test_trunk_state_display() {
        assert_eq!(format!("{}", TrunkState::Alive), "alive");
        assert_eq!(format!("{}", TrunkState::Deleted), "deleted");
        assert_eq!(format!("{}", TrunkState::Zombie), "zombie");
    }

    #[test]
    fn test_trunk_state_serde() {
        let state = TrunkState::Deleted;
        let json = serde_json::to_string(&state).unwrap();
        let decoded: TrunkState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, decoded);
    }

    // -------------------------------------------------------------------------
    // Trunk Tests
    // -------------------------------------------------------------------------

    fn make_trunk() -> Trunk {
        Trunk::new(
            TrunkId::new(NodeId::new(1), 0),
            Inode::new(42),
            "src/main.rs".to_string(),
            Some(Encoding::Utf8),
        )
    }

    #[test]
    fn test_trunk_new() {
        let trunk = make_trunk();
        assert_eq!(trunk.id(), TrunkId::new(NodeId::new(1), 0));
        assert_eq!(trunk.inode(), Inode::new(42));
        assert_eq!(trunk.path(), "src/main.rs");
        assert_eq!(trunk.encoding(), Some(Encoding::Utf8));
        assert!(trunk.state().is_alive());
    }

    #[test]
    fn test_trunk_is_text() {
        let trunk = make_trunk();
        assert!(trunk.is_text());

        let binary_trunk = Trunk::new(
            TrunkId::new(NodeId::new(1), 1),
            Inode::new(43),
            "image.png".to_string(),
            Some(Encoding::Binary),
        );
        assert!(!binary_trunk.is_text());
    }

    #[test]
    fn test_trunk_is_binary() {
        let trunk = make_trunk();
        assert!(!trunk.is_binary());

        let binary_trunk = Trunk::new(
            TrunkId::new(NodeId::new(1), 1),
            Inode::new(43),
            "image.png".to_string(),
            Some(Encoding::Binary),
        );
        assert!(binary_trunk.is_binary());
    }

    #[test]
    fn test_trunk_set_state() {
        let mut trunk = make_trunk();
        assert!(trunk.state().is_alive());

        trunk.set_state(TrunkState::Deleted);
        assert!(trunk.state().is_deleted());
    }

    #[test]
    fn test_trunk_set_path() {
        let mut trunk = make_trunk();
        assert_eq!(trunk.path(), "src/main.rs");

        trunk.set_path("src/lib.rs".to_string());
        assert_eq!(trunk.path(), "src/lib.rs");
    }

    #[test]
    fn test_trunk_delete_and_undelete() {
        let mut trunk = make_trunk();
        assert!(trunk.state().is_alive());

        trunk.delete();
        assert!(trunk.state().is_deleted());

        trunk.undelete();
        assert!(trunk.state().is_alive());
    }

    #[test]
    fn test_trunk_display() {
        let trunk = make_trunk();
        let display = format!("{}", trunk);
        assert!(display.contains("Trunk"));
        assert!(display.contains("src/main.rs"));
        assert!(display.contains("alive"));
    }

    #[test]
    fn test_trunk_serde() {
        let trunk = make_trunk();
        let json = serde_json::to_string(&trunk).unwrap();
        let decoded: Trunk = serde_json::from_str(&json).unwrap();
        assert_eq!(trunk, decoded);
    }

    // -------------------------------------------------------------------------
    // TrunkOp Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_op_create() {
        let op = TrunkOp::Create {
            path: "README.md".to_string(),
            encoding: Some(Encoding::Utf8),
        };
        assert!(op.is_create());
        assert!(!op.is_delete());
        assert!(op.trunk_id().is_none());
        assert_eq!(op.op_type(), "create");
    }

    #[test]
    fn test_trunk_op_delete() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let op = TrunkOp::Delete { trunk: trunk_id };
        assert!(op.is_delete());
        assert!(!op.is_create());
        assert_eq!(op.trunk_id(), Some(trunk_id));
        assert_eq!(op.op_type(), "delete");
    }

    #[test]
    fn test_trunk_op_move() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let op = TrunkOp::Move {
            trunk: trunk_id,
            new_path: "docs/README.md".to_string(),
        };
        assert!(op.is_move());
        assert_eq!(op.trunk_id(), Some(trunk_id));
        assert_eq!(op.op_type(), "move");
    }

    #[test]
    fn test_trunk_op_undelete() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let op = TrunkOp::Undelete { trunk: trunk_id };
        assert!(op.is_undelete());
        assert_eq!(op.trunk_id(), Some(trunk_id));
        assert_eq!(op.op_type(), "undelete");
    }

    #[test]
    fn test_trunk_op_display() {
        let create_op = TrunkOp::Create {
            path: "test.txt".to_string(),
            encoding: None,
        };
        assert!(format!("{}", create_op).contains("create"));

        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let delete_op = TrunkOp::Delete { trunk: trunk_id };
        assert!(format!("{}", delete_op).contains("delete"));

        let move_op = TrunkOp::Move {
            trunk: trunk_id,
            new_path: "new.txt".to_string(),
        };
        assert!(format!("{}", move_op).contains("move"));
    }

    #[test]
    fn test_trunk_op_serde() {
        let op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };
        let json = serde_json::to_string(&op).unwrap();
        let decoded: TrunkOp = serde_json::from_str(&json).unwrap();
        assert_eq!(op, decoded);
    }
}
