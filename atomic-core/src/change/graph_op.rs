//! High-level modification units (hunks)
//!
//! A **graph_op** represents a semantic modification to the repository:
//! - Adding, deleting, moving, or renaming files
//! - Editing file contents
//! - Replacing content (delete + insert)
//!
//! Hunks provide human-readable context for changes while containing
//! the underlying graph operations (atoms) that actually modify the
//! repository state.
//!
//! # GraphOp Types
//!
//! | GraphOp | Description |
//! |------|-------------|
//! | `FileAdd` | Create a new file or directory |
//! | `FileDel` | Delete a file or directory |
//! | `FileMove` | Rename or move a file |
//! | `FileUndel` | Restore a deleted file |
//! | `Edit` | Modify file contents |
//! | `Replacement` | Replace content (delete + insert) |
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::{GraphOp, Atom, Insertion, Encoding};
//!
//! // Create a file addition graph_op
//! let graph_op = GraphOp::FileAdd {
//!     add_name: name_vertex,    // Add filename to parent directory
//!     add_inode: inode_vertex,  // Create the file's inode
//!     contents: Some(content_vertex), // Optional initial content
//!     path: "README.md".to_string(),
//!     encoding: Some(Encoding::Utf8),
//! };
//!
//! // Edit existing content
//! let edit = GraphOp::Edit {
//!     change: Atom::Insertion(insert_vertex),
//!     local: Local::new("src/main.rs", 42),
//!     encoding: Some(Encoding::Utf8),
//! };
//! ```

use super::atom::{Atom, EdgeUpdate, Insertion};
use super::encoding::Encoding;
use super::local::Local;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A high-level modification unit.
///
/// Hunks are the building blocks of changes, representing semantic
/// operations like "add file" or "edit line 42". Each graph_op contains
/// one or more atoms (primitive graph operations) plus metadata for
/// human-readable display.
///
/// # Type Parameter
///
/// - `H`: The change identifier type. Use `Hash` for serialized changes,
///   or `Option<Hash>` when building a change that references itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphOp<H> {
    /// Add a new file or directory.
    ///
    /// This creates:
    /// - A name entry in the parent directory
    /// - An inode span for the file
    /// - Optionally, initial content
    FileAdd {
        /// Span to add the filename in parent directory
        add_name: Insertion<H>,
        /// Span to create the file's inode
        add_inode: Insertion<H>,
        /// Optional initial file contents
        #[serde(default)]
        contents: Option<Insertion<H>>,
        /// Path for human readability
        path: String,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add an empty directory.
    ///
    /// Unlike `FileAdd`, this explicitly tracks a directory without content.
    /// This enables tracking empty directories without the `.keep` file workaround.
    ///
    /// The graph structure uses the `FOLDER` edge flag to represent the
    /// directory relationship:
    ///
    /// ```text
    /// ┌────────────────────────────────────────────────────────────────┐
    /// │                  Directory Graph Structure                      │
    /// ├────────────────────────────────────────────────────────────────┤
    /// │                                                                │
    /// │  Parent Directory                                              │
    /// │  ┌─────────────┐                                               │
    /// │  │ Inode Span│                                               │
    /// │  │  (parent)   │                                               │
    /// │  └──────┬──────┘                                               │
    /// │         │ FOLDER edge                                          │
    /// │         ▼                                                      │
    /// │  ┌─────────────┐      ┌─────────────┐                         │
    /// │  │ Name Span │─────▶│ Inode Span│  ← New directory        │
    /// │  │ "subdir"    │      │  (empty)    │                         │
    /// │  └─────────────┘      └─────────────┘                         │
    /// │                                                                │
    /// └────────────────────────────────────────────────────────────────┘
    /// ```
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let dir_add = GraphOp::DirAdd {
    ///     add_name: name_vertex,
    ///     add_inode: inode_vertex,
    ///     path: "src/empty_module".to_string(),
    /// };
    /// ```
    DirAdd {
        /// Span to add the directory name in parent directory.
        ///
        /// Uses `FOLDER` edge flag to indicate this is a directory entry.
        add_name: Insertion<H>,
        /// Span to create the directory's inode.
        ///
        /// This is an empty span (start == end) that serves as the
        /// anchor point for the directory's children.
        add_inode: Insertion<H>,
        /// Path for human readability.
        path: String,
    },

    /// Delete an empty directory.
    ///
    /// This marks the directory's edges as deleted. The directory structure
    /// remains in the graph but is no longer "alive".
    ///
    /// # Note
    ///
    /// A directory can only be deleted if it has no remaining children.
    /// Attempting to delete a non-empty directory should fail at the
    /// application layer, not in the graph_op itself.
    DirDel {
        /// Edges to mark as deleted (name and inode edges).
        del: EdgeUpdate<H>,
        /// Path for human readability.
        path: String,
    },

    /// Restore a deleted directory.
    ///
    /// This removes the DELETED flag from the directory's edges,
    /// making it "alive" again. Any children that were not explicitly
    /// deleted will also become visible again.
    DirUndel {
        /// Edges to restore (remove DELETED flag).
        undel: EdgeUpdate<H>,
        /// Path for human readability.
        path: String,
    },

    /// Delete a file or directory.
    ///
    /// This marks the file's edges as deleted. The content remains
    /// in the graph but is no longer "alive".
    FileDel {
        /// Edges to mark as deleted
        del: EdgeUpdate<H>,
        /// Content edges to delete (if file has content)
        #[serde(default)]
        contents: Option<EdgeUpdate<H>>,
        /// Path for human readability
        path: String,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Restore a deleted file.
    ///
    /// This removes the DELETED flag from edges, making the content
    /// "alive" again.
    FileUndel {
        /// Edges to restore (remove DELETED flag)
        undel: EdgeUpdate<H>,
        /// Content edges to restore (if any)
        #[serde(default)]
        contents: Option<EdgeUpdate<H>>,
        /// Path for human readability
        path: String,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Move or rename a file.
    ///
    /// This is implemented as:
    /// 1. Delete the old name edge
    /// 2. Add a new name edge to the target location
    FileMove {
        /// Remove old name edge
        del: EdgeUpdate<H>,
        /// Add new name edge
        add: Insertion<H>,
        /// New path for human readability
        path: String,
    },

    /// Edit file contents.
    ///
    /// This can be either:
    /// - `Insertion`: Insert new content
    /// - `EdgeUpdate`: Delete existing content
    Edit {
        /// The modification (insert or delete)
        change: Atom<H>,
        /// Local context for display
        local: Local,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Replace content (delete + insert).
    ///
    /// This is an atomic replacement operation, equivalent to
    /// deleting some content and inserting new content in its place.
    Replacement {
        /// Content to delete
        change: EdgeUpdate<H>,
        /// Content to insert
        replacement: Insertion<H>,
        /// Local context for display
        local: Local,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Solve a name conflict.
    ///
    /// When multiple changes add files with the same name, this
    /// resolves the conflict by choosing one version.
    SolveNameConflict {
        /// The resolution operation
        name: EdgeUpdate<H>,
        /// Path where conflict occurred
        path: String,
    },

    /// Reopen a solved name conflict.
    ///
    /// This undoes a previous `SolveNameConflict`, allowing the
    /// conflict to be resolved differently.
    UnsolveNameConflict {
        /// The operation to undo the resolution
        name: EdgeUpdate<H>,
        /// Path where conflict is
        path: String,
    },

    /// Solve an ordering conflict.
    ///
    /// When multiple changes modify the same location, this
    /// establishes a definite ordering between them.
    SolveOrderConflict {
        /// The resolution operation
        change: EdgeUpdate<H>,
        /// Local context for display
        local: Local,
    },

    /// Reopen a solved ordering conflict.
    ///
    /// This undoes a previous `SolveOrderConflict`.
    UnsolveOrderConflict {
        /// The operation to undo the resolution
        change: EdgeUpdate<H>,
        /// Local context for display
        local: Local,
    },

    /// Resurrect deleted content (zombies).
    ///
    /// When content is deleted in one change but modified in another,
    /// the modified content becomes a "zombie". This operation brings
    /// zombies back to life.
    ResurrectZombies {
        /// The resurrection operation
        change: EdgeUpdate<H>,
        /// Local context for display
        local: Local,
        /// Text encoding (if text file)
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add a repository root.
    ///
    /// This is used during repository initialization to create
    /// the root of the file tree.
    AddRoot {
        /// Name of the root
        name: Insertion<H>,
        /// Inode for the root
        inode: Insertion<H>,
    },

    /// Delete a repository root.
    ///
    /// This removes a root from the repository.
    DelRoot {
        /// Name edges to delete
        name: EdgeUpdate<H>,
        /// Inode edges to delete
        inode: EdgeUpdate<H>,
    },
}

impl<H> GraphOp<H> {
    /// Get the path associated with this graph_op, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            GraphOp::FileAdd { path, .. }
            | GraphOp::FileDel { path, .. }
            | GraphOp::FileUndel { path, .. }
            | GraphOp::FileMove { path, .. }
            | GraphOp::DirAdd { path, .. }
            | GraphOp::DirDel { path, .. }
            | GraphOp::DirUndel { path, .. }
            | GraphOp::SolveNameConflict { path, .. }
            | GraphOp::UnsolveNameConflict { path, .. } => Some(path),

            GraphOp::Edit { local, .. }
            | GraphOp::Replacement { local, .. }
            | GraphOp::SolveOrderConflict { local, .. }
            | GraphOp::UnsolveOrderConflict { local, .. }
            | GraphOp::ResurrectZombies { local, .. } => Some(&local.path),

            GraphOp::AddRoot { .. } | GraphOp::DelRoot { .. } => None,
        }
    }

    /// Get the local context for this graph_op, if any.
    pub fn local(&self) -> Option<&Local> {
        match self {
            GraphOp::Edit { local, .. }
            | GraphOp::Replacement { local, .. }
            | GraphOp::SolveOrderConflict { local, .. }
            | GraphOp::UnsolveOrderConflict { local, .. }
            | GraphOp::ResurrectZombies { local, .. } => Some(local),

            _ => None,
        }
    }

    /// Get the line number for this graph_op, if applicable.
    pub fn line(&self) -> Option<u64> {
        self.local().map(|l| l.line)
    }

    /// Get the encoding for this graph_op, if any.
    pub fn encoding(&self) -> Option<Encoding> {
        match self {
            GraphOp::FileAdd { encoding, .. }
            | GraphOp::FileDel { encoding, .. }
            | GraphOp::FileUndel { encoding, .. }
            | GraphOp::Edit { encoding, .. }
            | GraphOp::Replacement { encoding, .. }
            | GraphOp::ResurrectZombies { encoding, .. } => *encoding,

            _ => None,
        }
    }

    /// Check if this graph_op represents a file-level operation.
    pub fn is_file_operation(&self) -> bool {
        matches!(
            self,
            GraphOp::FileAdd { .. }
                | GraphOp::FileDel { .. }
                | GraphOp::FileUndel { .. }
                | GraphOp::FileMove { .. }
        )
    }

    /// Check if this graph_op represents a directory-level operation.
    ///
    /// Directory operations create or modify directory structure without
    /// file content. They use the `FOLDER` edge flag in the graph.
    pub fn is_directory_operation(&self) -> bool {
        matches!(
            self,
            GraphOp::DirAdd { .. } | GraphOp::DirDel { .. } | GraphOp::DirUndel { .. }
        )
    }

    /// Check if this graph_op represents any structural operation (file or directory).
    ///
    /// Structural operations modify the repository's tree structure rather than
    /// file contents.
    pub fn is_structural_operation(&self) -> bool {
        self.is_file_operation() || self.is_directory_operation()
    }

    /// Check if this graph_op represents a content edit.
    pub fn is_content_edit(&self) -> bool {
        matches!(self, GraphOp::Edit { .. } | GraphOp::Replacement { .. })
    }

    /// Check if this graph_op represents a conflict resolution.
    pub fn is_conflict_resolution(&self) -> bool {
        matches!(
            self,
            GraphOp::SolveNameConflict { .. }
                | GraphOp::UnsolveNameConflict { .. }
                | GraphOp::SolveOrderConflict { .. }
                | GraphOp::UnsolveOrderConflict { .. }
                | GraphOp::ResurrectZombies { .. }
        )
    }

    /// Check if this graph_op represents a root operation.
    pub fn is_root_operation(&self) -> bool {
        matches!(self, GraphOp::AddRoot { .. } | GraphOp::DelRoot { .. })
    }

    /// Get a human-readable description of this graph_op type.
    pub fn type_name(&self) -> &'static str {
        match self {
            GraphOp::FileAdd { .. } => "FileAdd",
            GraphOp::FileDel { .. } => "FileDel",
            GraphOp::FileUndel { .. } => "FileUndel",
            GraphOp::FileMove { .. } => "FileMove",
            GraphOp::DirAdd { .. } => "DirAdd",
            GraphOp::DirDel { .. } => "DirDel",
            GraphOp::DirUndel { .. } => "DirUndel",
            GraphOp::Edit { .. } => "Edit",
            GraphOp::Replacement { .. } => "Replacement",
            GraphOp::SolveNameConflict { .. } => "SolveNameConflict",
            GraphOp::UnsolveNameConflict { .. } => "UnsolveNameConflict",
            GraphOp::SolveOrderConflict { .. } => "SolveOrderConflict",
            GraphOp::UnsolveOrderConflict { .. } => "UnsolveOrderConflict",
            GraphOp::ResurrectZombies { .. } => "ResurrectZombies",
            GraphOp::AddRoot { .. } => "AddRoot",
            GraphOp::DelRoot { .. } => "DelRoot",
        }
    }
}

impl<H: fmt::Debug> fmt::Display for GraphOp<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphOp::FileAdd { path, .. } => write!(f, "FileAdd: {}", path),
            GraphOp::FileDel { path, .. } => write!(f, "FileDel: {}", path),
            GraphOp::FileUndel { path, .. } => write!(f, "FileUndel: {}", path),
            GraphOp::FileMove { path, .. } => write!(f, "FileMove: {}", path),
            GraphOp::DirAdd { path, .. } => write!(f, "DirAdd: {}", path),
            GraphOp::DirDel { path, .. } => write!(f, "DirDel: {}", path),
            GraphOp::DirUndel { path, .. } => write!(f, "DirUndel: {}", path),
            GraphOp::Edit { local, .. } => write!(f, "Edit: {}:{}", local.path, local.line),
            GraphOp::Replacement { local, .. } => {
                write!(f, "Replacement: {}:{}", local.path, local.line)
            }
            GraphOp::SolveNameConflict { path, .. } => write!(f, "SolveNameConflict: {}", path),
            GraphOp::UnsolveNameConflict { path, .. } => write!(f, "UnsolveNameConflict: {}", path),
            GraphOp::SolveOrderConflict { local, .. } => {
                write!(f, "SolveOrderConflict: {}:{}", local.path, local.line)
            }
            GraphOp::UnsolveOrderConflict { local, .. } => {
                write!(f, "UnsolveOrderConflict: {}:{}", local.path, local.line)
            }
            GraphOp::ResurrectZombies { local, .. } => {
                write!(f, "ResurrectZombies: {}:{}", local.path, local.line)
            }
            GraphOp::AddRoot { .. } => write!(f, "AddRoot"),
            GraphOp::DelRoot { .. } => write!(f, "DelRoot"),
        }
    }
}

/// Iterator over atoms contained in a graph_op.
///
/// This allows iterating over all the primitive graph operations
/// that a graph_op contains.
pub struct HunkAtomIter<'a, H> {
    graph_op: &'a GraphOp<H>,
    index: usize,
}

impl<'a, H> Iterator for HunkAtomIter<'a, H> {
    type Item = AtomRef<'a, H>;

    fn next(&mut self) -> Option<Self::Item> {
        let result = match (self.graph_op, self.index) {
            // FileAdd: add_name, add_inode, contents
            (GraphOp::FileAdd { add_name, .. }, 0) => Some(AtomRef::Insertion(add_name)),
            (GraphOp::FileAdd { add_inode, .. }, 1) => Some(AtomRef::Insertion(add_inode)),
            (
                GraphOp::FileAdd {
                    contents: Some(c), ..
                },
                2,
            ) => Some(AtomRef::Insertion(c)),
            (GraphOp::FileAdd { .. }, _) => None,

            // FileDel: del, contents
            (GraphOp::FileDel { del, .. }, 0) => Some(AtomRef::EdgeUpdate(del)),
            (
                GraphOp::FileDel {
                    contents: Some(c), ..
                },
                1,
            ) => Some(AtomRef::EdgeUpdate(c)),
            (GraphOp::FileDel { .. }, _) => None,

            // FileUndel: undel, contents
            (GraphOp::FileUndel { undel, .. }, 0) => Some(AtomRef::EdgeUpdate(undel)),
            (
                GraphOp::FileUndel {
                    contents: Some(c), ..
                },
                1,
            ) => Some(AtomRef::EdgeUpdate(c)),
            (GraphOp::FileUndel { .. }, _) => None,

            // FileMove: del, add
            (GraphOp::FileMove { del, .. }, 0) => Some(AtomRef::EdgeUpdate(del)),
            (GraphOp::FileMove { add, .. }, 1) => Some(AtomRef::Insertion(add)),
            (GraphOp::FileMove { .. }, _) => None,

            // Edit: change
            (GraphOp::Edit { change, .. }, 0) => Some(AtomRef::Atom(change)),
            (GraphOp::Edit { .. }, _) => None,

            // Replacement: change, replacement
            (GraphOp::Replacement { change, .. }, 0) => Some(AtomRef::EdgeUpdate(change)),
            (GraphOp::Replacement { replacement, .. }, 1) => Some(AtomRef::Insertion(replacement)),
            (GraphOp::Replacement { .. }, _) => None,

            // SolveNameConflict: name
            (GraphOp::SolveNameConflict { name, .. }, 0) => Some(AtomRef::EdgeUpdate(name)),
            (GraphOp::SolveNameConflict { .. }, _) => None,

            // UnsolveNameConflict: name
            (GraphOp::UnsolveNameConflict { name, .. }, 0) => Some(AtomRef::EdgeUpdate(name)),
            (GraphOp::UnsolveNameConflict { .. }, _) => None,

            // SolveOrderConflict: change
            (GraphOp::SolveOrderConflict { change, .. }, 0) => Some(AtomRef::EdgeUpdate(change)),
            (GraphOp::SolveOrderConflict { .. }, _) => None,

            // UnsolveOrderConflict: change
            (GraphOp::UnsolveOrderConflict { change, .. }, 0) => Some(AtomRef::EdgeUpdate(change)),
            (GraphOp::UnsolveOrderConflict { .. }, _) => None,

            // ResurrectZombies: change
            (GraphOp::ResurrectZombies { change, .. }, 0) => Some(AtomRef::EdgeUpdate(change)),
            (GraphOp::ResurrectZombies { .. }, _) => None,

            // AddRoot: name, inode
            (GraphOp::AddRoot { name, .. }, 0) => Some(AtomRef::Insertion(name)),
            (GraphOp::AddRoot { inode, .. }, 1) => Some(AtomRef::Insertion(inode)),
            (GraphOp::AddRoot { .. }, _) => None,

            // DelRoot: name, inode
            (GraphOp::DelRoot { name, .. }, 0) => Some(AtomRef::EdgeUpdate(name)),
            (GraphOp::DelRoot { inode, .. }, 1) => Some(AtomRef::EdgeUpdate(inode)),
            (GraphOp::DelRoot { .. }, _) => None,

            // DirAdd: add_name, add_inode
            (GraphOp::DirAdd { add_name, .. }, 0) => Some(AtomRef::Insertion(add_name)),
            (GraphOp::DirAdd { add_inode, .. }, 1) => Some(AtomRef::Insertion(add_inode)),
            (GraphOp::DirAdd { .. }, _) => None,

            // DirDel: del
            (GraphOp::DirDel { del, .. }, 0) => Some(AtomRef::EdgeUpdate(del)),
            (GraphOp::DirDel { .. }, _) => None,

            // DirUndel: undel
            (GraphOp::DirUndel { undel, .. }, 0) => Some(AtomRef::EdgeUpdate(undel)),
            (GraphOp::DirUndel { .. }, _) => None,
        };

        if result.is_some() {
            self.index += 1;
        }
        result
    }
}

/// Reference to an atom within a graph_op.
///
/// This enum allows iterating over atoms without copying them.
pub enum AtomRef<'a, H> {
    /// Reference to a Insertion
    Insertion(&'a Insertion<H>),
    /// Reference to an EdgeUpdate
    EdgeUpdate(&'a EdgeUpdate<H>),
    /// Reference to a full Atom
    Atom(&'a Atom<H>),
}

impl<H> GraphOp<H> {
    /// Iterate over all atoms in this graph_op.
    pub fn atoms(&self) -> HunkAtomIter<'_, H> {
        HunkAtomIter {
            graph_op: self,
            index: 0,
        }
    }

    /// Count the number of atoms in this graph_op.
    pub fn atom_count(&self) -> usize {
        match self {
            GraphOp::FileAdd {
                contents: Some(_), ..
            } => 3,
            GraphOp::FileAdd { contents: None, .. } => 2,
            GraphOp::FileDel {
                contents: Some(_), ..
            } => 2,
            GraphOp::FileDel { contents: None, .. } => 1,
            GraphOp::FileUndel {
                contents: Some(_), ..
            } => 2,
            GraphOp::FileUndel { contents: None, .. } => 1,
            GraphOp::FileMove { .. } => 2,
            GraphOp::Edit { .. } => 1,
            GraphOp::Replacement { .. } => 2,
            GraphOp::SolveNameConflict { .. } => 1,
            GraphOp::UnsolveNameConflict { .. } => 1,
            GraphOp::SolveOrderConflict { .. } => 1,
            GraphOp::UnsolveOrderConflict { .. } => 1,
            GraphOp::ResurrectZombies { .. } => 1,
            GraphOp::AddRoot { .. } => 2,
            GraphOp::DelRoot { .. } => 2,
            GraphOp::DirAdd { .. } => 2,
            GraphOp::DirDel { .. } => 1,
            GraphOp::DirUndel { .. } => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChangePosition, EdgeFlags, Hash, Position};

    // Helper to create test positions
    fn test_hash_position(pos: u64) -> Position<Hash> {
        Position::new(Hash::of(b"test"), ChangePosition::new(pos))
    }

    fn test_new_vertex() -> Insertion<Hash> {
        Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        }
    }

    fn test_edge_map() -> EdgeUpdate<Hash> {
        EdgeUpdate {
            edges: vec![],
            inode: test_hash_position(0),
        }
    }

    // GraphOp Type Tests

    #[test]
    fn test_file_add() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: Some(test_new_vertex()),
            path: "README.md".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        assert!(graph_op.is_file_operation());
        assert!(!graph_op.is_content_edit());
        assert_eq!(graph_op.path(), Some("README.md"));
        assert_eq!(graph_op.encoding(), Some(Encoding::Utf8));
        assert_eq!(graph_op.type_name(), "FileAdd");
    }

    #[test]
    fn test_file_del() {
        let graph_op: GraphOp<Hash> = GraphOp::FileDel {
            del: test_edge_map(),
            contents: None,
            path: "old_file.txt".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        assert!(graph_op.is_file_operation());
        assert_eq!(graph_op.path(), Some("old_file.txt"));
        assert_eq!(graph_op.type_name(), "FileDel");
    }

    #[test]
    fn test_file_move() {
        let graph_op: GraphOp<Hash> = GraphOp::FileMove {
            del: test_edge_map(),
            add: test_new_vertex(),
            path: "new/path/file.rs".to_string(),
        };

        assert!(graph_op.is_file_operation());
        assert_eq!(graph_op.path(), Some("new/path/file.rs"));
        assert_eq!(graph_op.encoding(), None);
    }

    #[test]
    fn test_edit() {
        let graph_op: GraphOp<Hash> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("src/lib.rs", 42),
            encoding: Some(Encoding::Utf8),
        };

        assert!(graph_op.is_content_edit());
        assert!(!graph_op.is_file_operation());
        assert_eq!(graph_op.path(), Some("src/lib.rs"));
        assert_eq!(graph_op.line(), Some(42));
        assert_eq!(graph_op.local().unwrap().line, 42);
    }

    #[test]
    fn test_replacement() {
        let graph_op: GraphOp<Hash> = GraphOp::Replacement {
            change: test_edge_map(),
            replacement: test_new_vertex(),
            local: Local::new("src/main.rs", 100),
            encoding: Some(Encoding::Utf8),
        };

        assert!(graph_op.is_content_edit());
        assert_eq!(graph_op.path(), Some("src/main.rs"));
        assert_eq!(graph_op.line(), Some(100));
    }

    #[test]
    fn test_solve_name_conflict() {
        let graph_op: GraphOp<Hash> = GraphOp::SolveNameConflict {
            name: test_edge_map(),
            path: "conflicted.txt".to_string(),
        };

        assert!(graph_op.is_conflict_resolution());
        assert_eq!(graph_op.path(), Some("conflicted.txt"));
    }

    #[test]
    fn test_add_root() {
        let graph_op: GraphOp<Hash> = GraphOp::AddRoot {
            name: test_new_vertex(),
            inode: test_new_vertex(),
        };

        assert!(graph_op.is_root_operation());
        assert!(!graph_op.is_file_operation());
        assert_eq!(graph_op.path(), None);
    }

    // Display Tests

    #[test]
    fn test_hunk_display() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: None,
            path: "test.txt".to_string(),
            encoding: None,
        };

        let display = format!("{}", graph_op);
        assert!(display.contains("FileAdd"));
        assert!(display.contains("test.txt"));
    }

    #[test]
    fn test_edit_display() {
        let graph_op: GraphOp<Hash> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("file.rs", 50),
            encoding: None,
        };

        let display = format!("{}", graph_op);
        assert!(display.contains("Edit"));
        assert!(display.contains("file.rs"));
        assert!(display.contains("50"));
    }

    // Atom Iterator Tests

    #[test]
    fn test_atom_count_file_add_with_contents() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: Some(test_new_vertex()),
            path: "test.txt".to_string(),
            encoding: None,
        };

        assert_eq!(graph_op.atom_count(), 3);
        assert_eq!(graph_op.atoms().count(), 3);
    }

    #[test]
    fn test_atom_count_file_add_without_contents() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: None,
            path: "test.txt".to_string(),
            encoding: None,
        };

        assert_eq!(graph_op.atom_count(), 2);
        assert_eq!(graph_op.atoms().count(), 2);
    }

    #[test]
    fn test_atom_count_edit() {
        let graph_op: GraphOp<Hash> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("file.rs", 1),
            encoding: None,
        };

        assert_eq!(graph_op.atom_count(), 1);
        assert_eq!(graph_op.atoms().count(), 1);
    }

    #[test]
    fn test_atom_count_replacement() {
        let graph_op: GraphOp<Hash> = GraphOp::Replacement {
            change: test_edge_map(),
            replacement: test_new_vertex(),
            local: Local::new("file.rs", 1),
            encoding: None,
        };

        assert_eq!(graph_op.atom_count(), 2);
        assert_eq!(graph_op.atoms().count(), 2);
    }

    // Serialization Tests

    #[test]
    fn test_file_add_json_roundtrip() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: Some(test_new_vertex()),
            path: "test.txt".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        let json = serde_json::to_string(&graph_op).unwrap();
        let parsed: GraphOp<Hash> = serde_json::from_str(&json).unwrap();
        assert_eq!(graph_op.path(), parsed.path());
        assert_eq!(graph_op.encoding(), parsed.encoding());
    }

    #[test]
    fn test_edit_json_roundtrip() {
        let graph_op: GraphOp<Hash> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("src/lib.rs", 42),
            encoding: Some(Encoding::Utf8),
        };

        let json = serde_json::to_string(&graph_op).unwrap();
        let parsed: GraphOp<Hash> = serde_json::from_str(&json).unwrap();
        assert_eq!(graph_op.path(), parsed.path());
        assert_eq!(graph_op.line(), parsed.line());
    }

    #[test]
    fn test_json_roundtrip_all_types() {
        let hunks: Vec<GraphOp<Hash>> = vec![
            GraphOp::FileAdd {
                add_name: test_new_vertex(),
                add_inode: test_new_vertex(),
                contents: None,
                path: "file.txt".to_string(),
                encoding: Some(Encoding::Utf8),
            },
            GraphOp::FileDel {
                del: test_edge_map(),
                contents: None,
                path: "old.txt".to_string(),
                encoding: None,
            },
            GraphOp::Edit {
                change: Atom::EdgeUpdate(test_edge_map()),
                local: Local::new("edit.rs", 10),
                encoding: None,
            },
        ];

        for graph_op in hunks {
            let json = serde_json::to_string(&graph_op).unwrap();
            let parsed: GraphOp<Hash> = serde_json::from_str(&json).unwrap();
            assert_eq!(graph_op.path(), parsed.path());
        }
    }

    // Edge Cases

    #[test]
    fn test_empty_path() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: None,
            path: String::new(),
            encoding: None,
        };

        assert_eq!(graph_op.path(), Some(""));
    }

    #[test]
    fn test_deep_path() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: None,
            path: "a/b/c/d/e/f/g/h/file.txt".to_string(),
            encoding: None,
        };

        assert_eq!(graph_op.path(), Some("a/b/c/d/e/f/g/h/file.txt"));
    }

    #[test]
    fn test_binary_file() {
        let graph_op: GraphOp<Hash> = GraphOp::FileAdd {
            add_name: test_new_vertex(),
            add_inode: test_new_vertex(),
            contents: Some(test_new_vertex()),
            path: "image.png".to_string(),
            encoding: Some(Encoding::Binary),
        };

        assert_eq!(graph_op.encoding(), Some(Encoding::Binary));
    }

    #[test]
    fn test_all_hunk_types_have_type_name() {
        let hunks: Vec<GraphOp<Hash>> = vec![
            GraphOp::FileAdd {
                add_name: test_new_vertex(),
                add_inode: test_new_vertex(),
                contents: None,
                path: "a".to_string(),
                encoding: None,
            },
            GraphOp::FileDel {
                del: test_edge_map(),
                contents: None,
                path: "b".to_string(),
                encoding: None,
            },
            GraphOp::FileUndel {
                undel: test_edge_map(),
                contents: None,
                path: "c".to_string(),
                encoding: None,
            },
            GraphOp::FileMove {
                del: test_edge_map(),
                add: test_new_vertex(),
                path: "d".to_string(),
            },
            GraphOp::Edit {
                change: Atom::Insertion(test_new_vertex()),
                local: Local::new("e", 1),
                encoding: None,
            },
            GraphOp::Replacement {
                change: test_edge_map(),
                replacement: test_new_vertex(),
                local: Local::new("f", 1),
                encoding: None,
            },
            GraphOp::SolveNameConflict {
                name: test_edge_map(),
                path: "g".to_string(),
            },
            GraphOp::UnsolveNameConflict {
                name: test_edge_map(),
                path: "h".to_string(),
            },
            GraphOp::SolveOrderConflict {
                change: test_edge_map(),
                local: Local::new("i", 1),
            },
            GraphOp::UnsolveOrderConflict {
                change: test_edge_map(),
                local: Local::new("j", 1),
            },
            GraphOp::ResurrectZombies {
                change: test_edge_map(),
                local: Local::new("k", 1),
                encoding: None,
            },
            GraphOp::AddRoot {
                name: test_new_vertex(),
                inode: test_new_vertex(),
            },
            GraphOp::DelRoot {
                name: test_edge_map(),
                inode: test_edge_map(),
            },
        ];

        for graph_op in &hunks {
            assert!(!graph_op.type_name().is_empty());
        }
    }
}
