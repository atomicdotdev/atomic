//! The top-level compact graph operation enum for V3 serialization.
//!
//! [`CompactGraphOp`] mirrors [`GraphOp<Option<Hash>>`](crate::change::graph_op::GraphOp)
//! but uses compact types for all position, node, and hash references.

use super::types::{CompactAtom, CompactEdgeUpdate, CompactInsertion};
use crate::change::attribute::InodeAttr;
use crate::change::encoding::Encoding;
use crate::change::format_v3::types::CompactPosition;
use crate::change::local::Local;
use crate::EdgeFlags;
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

    /// Retain `name.inode` at `path` and tombstone all losing claim edges.
    SolveNameConflict {
        /// Non-empty exact `FOLDER | BLOCK` to deleted claim transitions.
        name: CompactEdgeUpdate,
        /// Path retained by the surviving claimant.
        path: String,
    },

    /// Restore all losing claim edges tombstoned by a resolution.
    UnsolveNameConflict {
        /// Non-empty exact inverse transitions for the losing claims.
        name: CompactEdgeUpdate,
        /// Path of the previously selected claimant.
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

    /// Add a causal value to an inode attribute register.
    SetAttr {
        /// Stable graph position of the inode.
        inode: CompactPosition,
        /// Path for human-readable output.
        path: String,
        /// Canonical attribute value.
        value: InodeAttr,
    },
}

impl CompactGraphOp {
    pub(super) fn validate_serialized_name_conflict(&self) -> Result<(), String> {
        if let CompactGraphOp::SetAttr { inode, path, value } = self {
            if path.is_empty() {
                return Err("SetAttr requires a non-empty path".to_string());
            }
            // Compact HASH_INDEX_NONE is the legacy self-reference sentinel;
            // ROOT versus self can only be validated after hash-table expansion.
            let _ = inode;
            value.validate().map_err(|error| error.to_string())?;
        }
        let alive = (EdgeFlags::FOLDER | EdgeFlags::BLOCK).bits();
        let deleted = (EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED).bits();
        let (operation, name, path, expected_previous, expected_flag) = match self {
            CompactGraphOp::SolveNameConflict { name, path } => {
                ("SolveNameConflict", name, path, alive, deleted)
            }
            CompactGraphOp::UnsolveNameConflict { name, path } => {
                ("UnsolveNameConflict", name, path, deleted, alive)
            }
            _ => return Ok(()),
        };

        if path.is_empty() {
            return Err(format!("{operation} requires a non-empty surviving path"));
        }
        if name.edges.is_empty() {
            return Err(format!(
                "{operation} requires at least one losing name claim"
            ));
        }
        // Hash indices cannot be classified as ROOT versus an explicit change
        // without the accompanying table. Production V3 writers place the
        // Hash::NONE placeholder at index 0, while Option::None uses the legacy
        // HASH_INDEX_NONE sentinel. Validate references after expansion, where
        // their actual Option<Hash> meaning is available.
        for (index, edge) in name.edges.iter().enumerate() {
            if edge.previous != expected_previous || edge.flag != expected_flag {
                return Err(format!(
                    "{operation} edge {index} must transition exactly from 0x{expected_previous:02X} to 0x{expected_flag:02X}, got 0x{:02X} to 0x{:02X}",
                    edge.previous, edge.flag
                ));
            }
            if edge.to.start >= edge.to.end {
                return Err(format!(
                    "{operation} edge {index} must target a non-empty name vertex"
                ));
            }
            if name.edges[..index].contains(edge) {
                return Err(format!(
                    "{operation} edge {index} duplicates an earlier losing claim"
                ));
            }
        }
        Ok(())
    }

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
            CompactGraphOp::SetAttr { path, .. } => Some(path),
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
            CompactGraphOp::SetAttr { .. } => "SetAttr",
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
