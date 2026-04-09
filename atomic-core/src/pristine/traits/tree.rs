//! File tree operations trait.
//!
//! `TreeTxnT` maps file paths to inodes (stable file identifiers) and
//! inodes to graph positions, enabling efficient file-level operations.

use crate::types::{GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

use crate::pristine::error::PristineError;

use super::graph::GraphTxnT;

/// File tree operations
///
/// This trait provides access to the file tree mappings that connect:
/// - File paths ↔ Inodes (stable file identifiers)
/// - Inodes ↔ Graph positions (where the file's content lives in the graph)
///
/// # Why Inodes?
///
/// Inodes provide a stable identifier for files that survives renames. When
/// you rename a file, the inode stays the same—only the path→inode mapping
/// changes. This is crucial for tracking file history across renames.
///
/// # The Inode Graph Index
///
/// The `iter_inode_vertices` method uses a secondary index (INODE_GRAPH) that
/// allows O(n) iteration over a file's content, where n is the file size.
/// Without this index, you'd need to scan the entire graph (O(N) where N is
/// total repository size).
///
/// ```text
/// Path "src/main.rs"
///        │
///        ▼
///    Inode 42
///        │
///        ├── Position (change: 5, pos: 100)  ──▶  Vertices in INODE_GRAPH[42]
///        │                                           │
///        │                                           ▼
///        │                                    ┌─────────────────┐
///        │                                    │  File content   │
///        │                                    │  as a subgraph  │
///        │                                    └─────────────────┘
/// ```
pub trait TreeTxnT: GraphTxnT {
    /// Get the inode for a path.
    ///
    /// Looks up the stable file identifier for a given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path (relative to repository root)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(inode))` - The file exists
    /// * `Ok(None)` - No file at this path
    /// * `Err(_)` - Database error
    fn get_inode(&self, path: &str) -> Result<Option<Inode>, PristineError>;

    /// Get directory flags for an inode.
    ///
    /// Checks if an inode represents a directory and returns its flags.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(flags))` - The inode is a directory with these flags
    /// * `Ok(None)` - The inode is not a directory (it's a file)
    /// * `Err(_)` - Database error
    ///
    /// # Directory Flags
    ///
    /// See `directory_flags` module for flag constants:
    /// - `DIR_EXPLICIT` (0x01): Directory was explicitly tracked
    /// - `DIR_EMPTY` (0x02): Directory has no tracked children
    fn get_directory_flags(&self, inode: Inode) -> Result<Option<u8>, PristineError>;

    /// Check if an inode represents a directory.
    ///
    /// Convenience method that returns `true` if the inode is marked as a
    /// directory in the DIRECTORIES table.
    fn is_directory(&self, inode: Inode) -> Result<bool, PristineError> {
        Ok(self.get_directory_flags(inode)?.is_some())
    }

    /// Get the path for an inode.
    ///
    /// Returns the current path for a file identified by inode.
    /// This is the inverse of `get_inode`.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(path))` - The inode has a path
    /// * `Ok(None)` - The inode doesn't exist or has no path
    /// * `Err(_)` - Database error
    fn get_path(&self, inode: Inode) -> Result<Option<String>, PristineError>;

    /// Get the graph position for an inode.
    ///
    /// Returns the position in the graph where this file's content root is.
    /// This is the entry point for traversing the file's content graph.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(pos))` - The file's root position in the graph
    /// * `Ok(None)` - The inode has no graph position
    /// * `Err(_)` - Database error
    fn inode_position(&self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError>;

    /// Get the inode for a graph position.
    ///
    /// Returns the inode that contains this position.
    /// This is the inverse of `inode_position`.
    fn position_inode(&self, pos: Position<NodeId>) -> Result<Option<Inode>, PristineError>;

    /// Iterate over all files in the tree.
    ///
    /// Returns an iterator over (path, inode) pairs for all tracked files.
    /// The order of iteration is not guaranteed.
    #[allow(clippy::type_complexity)]
    fn iter_tree(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>, PristineError>;

    /// Iterate over vertices for a specific inode.
    ///
    /// Uses the inode graph index for O(n) file traversal where n is the
    /// file size in vertices. This is much more efficient than scanning
    /// the entire graph.
    ///
    /// # Performance
    ///
    /// This uses the INODE_GRAPH secondary index, providing O(m) complexity
    /// where m is the number of vertices in the file, rather than O(N) where
    /// N is the total graph size.
    #[allow(clippy::type_complexity)]
    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> Result<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
        PristineError,
    >;

    /// Get the cached file index entry (mtime + size + content hash) for a tracked file.
    ///
    /// Returns the filesystem metadata and content hash snapshot taken at the
    /// time the file was last recorded or applied. During status, if the
    /// current `stat()` values match, we skip hashing. If they don't match,
    /// we hash the disk file and compare with the stored content hash —
    /// avoiding the expensive graph content reconstruction.
    ///
    /// # Returns
    ///
    /// * `Ok(Some((mtime_secs, mtime_nanos, file_size, content_hash)))` - Cached index entry
    /// * `Ok(None)` - No cached entry for this path
    /// * `Err(_)` - Database error
    fn get_file_index(&self, path: &str) -> Result<Option<(i64, u32, u64, Hash)>, PristineError>;
}
