//! The top-level compact graph operation enum for V3 serialization.
//!
//! [`CompactGraphOp`] mirrors [`GraphOp<Option<Hash>>`](crate::change::graph_op::GraphOp)
//! but uses compact types for all position, node, and hash references.

use super::types::{CompactAtom, CompactEdgeUpdate, CompactInsertion};
use crate::change::encoding::Encoding;
use crate::change::local::Local;
use serde::{Deserialize, Serialize};
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// CompactGraphOp — GraphOp<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`GraphOp<Option<Hash>>`](crate::change::graph_op::GraphOp).
///
/// This is the top-level hunk type for V3 serialization. Each variant
/// mirrors the corresponding `GraphOp` variant but uses compact types
/// for all position, node, and hash references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactGraphOp {
    /// Add a new file.
    FileAdd {
        /// Vertex to add the filename in parent directory.
        add_name: CompactInsertion,
        /// Vertex to create the file's inode.
        add_inode: CompactInsertion,
        /// Optional initial file contents.
        #[serde(default)]
        contents: Option<CompactInsertion>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add an empty directory.
    DirAdd {
        /// Vertex to add the directory name in parent directory.
        add_name: CompactInsertion,
        /// Vertex to create the directory's inode.
        add_inode: CompactInsertion,
        /// Path for human readability.
        path: String,
    },

    /// Delete an empty directory.
    DirDel {
        /// Edges to mark as deleted.
        del: CompactEdgeUpdate,
        /// Path for human readability.
        path: String,
    },

    /// Restore a deleted directory.
    DirUndel {
        /// Edges to restore.
        undel: CompactEdgeUpdate,
        /// Path for human readability.
        path: String,
    },

    /// Delete a file.
    FileDel {
        /// Edges to mark as deleted.
        del: CompactEdgeUpdate,
        /// Content edges to delete (if file has content).
        #[serde(default)]
        contents: Option<CompactEdgeUpdate>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Restore a deleted file.
    FileUndel {
        /// Edges to restore.
        undel: CompactEdgeUpdate,
        /// Content edges to restore.
        #[serde(default)]
        contents: Option<CompactEdgeUpdate>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Move or rename a file.
    FileMove {
        /// Remove old name edge.
        del: CompactEdgeUpdate,
        /// Add new name edge.
        add: CompactInsertion,
        /// New path for human readability.
        path: String,
    },

    /// Edit file contents.
    Edit {
        /// The modification (insert or delete).
        change: CompactAtom,
        /// Local context for display (path + line number).
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Replace content (delete + insert).
    Replacement {
        /// Content to delete.
        change: CompactEdgeUpdate,
        /// Content to insert.
        replacement: CompactInsertion,
        /// Local context for display.
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Solve a name conflict.
    SolveNameConflict {
        /// The resolution operation.
        name: CompactEdgeUpdate,
        /// Path where conflict occurred.
        path: String,
    },

    /// Reopen a solved name conflict.
    UnsolveNameConflict {
        /// The operation to undo the resolution.
        name: CompactEdgeUpdate,
        /// Path where conflict is.
        path: String,
    },

    /// Solve an ordering conflict.
    SolveOrderConflict {
        /// The resolution operation.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
    },

    /// Reopen a solved ordering conflict.
    UnsolveOrderConflict {
        /// The operation to undo the resolution.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
    },

    /// Resurrect deleted content (zombies).
    ResurrectZombies {
        /// The resurrection operation.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add a repository root.
    AddRoot {
        /// Name of the root.
        name: CompactInsertion,
        /// Inode for the root.
        inode: CompactInsertion,
    },

    /// Delete a repository root.
    DelRoot {
        /// Name edges to delete.
        name: CompactEdgeUpdate,
        /// Inode edges to delete.
        inode: CompactEdgeUpdate,
    },
}

impl CompactGraphOp {
    /// Returns the path associated with this operation, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            CompactGraphOp::FileAdd { path, .. }
            | CompactGraphOp::DirAdd { path, .. }
            | CompactGraphOp::DirDel { path, .. }
            | CompactGraphOp::DirUndel { path, .. }
            | CompactGraphOp::FileDel { path, .. }
            | CompactGraphOp::FileUndel { path, .. }
            | CompactGraphOp::FileMove { path, .. }
            | CompactGraphOp::SolveNameConflict { path, .. }
            | CompactGraphOp::UnsolveNameConflict { path, .. } => Some(path),
            CompactGraphOp::Edit { local, .. }
            | CompactGraphOp::Replacement { local, .. }
            | CompactGraphOp::SolveOrderConflict { local, .. }
            | CompactGraphOp::UnsolveOrderConflict { local, .. }
            | CompactGraphOp::ResurrectZombies { local, .. } => Some(&local.path),
            CompactGraphOp::AddRoot { .. } | CompactGraphOp::DelRoot { .. } => None,
        }
    }

    /// Returns a human-readable type name for this operation.
    pub fn type_name(&self) -> &'static str {
        match self {
            CompactGraphOp::FileAdd { .. } => "FileAdd",
            CompactGraphOp::DirAdd { .. } => "DirAdd",
            CompactGraphOp::DirDel { .. } => "DirDel",
            CompactGraphOp::DirUndel { .. } => "DirUndel",
            CompactGraphOp::FileDel { .. } => "FileDel",
            CompactGraphOp::FileUndel { .. } => "FileUndel",
            CompactGraphOp::FileMove { .. } => "FileMove",
            CompactGraphOp::Edit { .. } => "Edit",
            CompactGraphOp::Replacement { .. } => "Replacement",
            CompactGraphOp::SolveNameConflict { .. } => "SolveNameConflict",
            CompactGraphOp::UnsolveNameConflict { .. } => "UnsolveNameConflict",
            CompactGraphOp::SolveOrderConflict { .. } => "SolveOrderConflict",
            CompactGraphOp::UnsolveOrderConflict { .. } => "UnsolveOrderConflict",
            CompactGraphOp::ResurrectZombies { .. } => "ResurrectZombies",
            CompactGraphOp::AddRoot { .. } => "AddRoot",
            CompactGraphOp::DelRoot { .. } => "DelRoot",
        }
    }
}

impl fmt::Display for CompactGraphOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path() {
            Some(path) => write!(f, "{}({})", self.type_name(), path),
            None => write!(f, "{}", self.type_name()),
        }
    }
}
