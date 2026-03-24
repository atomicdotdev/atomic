//! Working copy output module
//!
//! This module handles outputting the repository graph state to the working copy
//! and provides CRDT-based content retrieval for human-readable output.
//! (file system). It traverses the file tree stored in the graph and reconstructs
//! files on disk, handling conflicts, encoding, and file metadata.
//!
//! # Overview
//!
//! The output process works as follows:
//!
//! 1. **Tree Traversal**: Walk the file tree from the root, identifying files
//!    and directories to output
//! 2. **Graph Retrieval**: For each file, retrieve its content graph (the DAG
//!    of vertices representing the file's content)
//! 3. **Conflict Detection**: Identify any ordering conflicts, zombie content,
//!    or cyclic dependencies in the graph
//! 4. **Content Output**: Output the file content, inserting conflict markers
//!    where necessary
//! 5. **Cleanup**: Remove files that have been deleted from the repository
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Output Pipeline                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Repository Graph           Tree Walker            File System          │
//! │  ┌──────────────┐         ┌────────────┐         ┌──────────────┐      │
//! │  │  Vertices    │ traverse│ Enumerate  │  write  │ Create Dirs  │      │
//! │  │  Edges       │ ──────► │ Files      │ ──────► │ Write Files  │      │
//! │  │  File Tree   │         │ Detect Del │         │ Set Perms    │      │
//! │  └──────────────┘         └────────────┘         └──────────────┘      │
//! │        │                        │                       │              │
//! │        │                        │                       │              │
//! │        ▼                        ▼                       ▼              │
//! │  ┌──────────────┐         ┌────────────┐         ┌──────────────┐      │
//! │  │ Change Store │         │ Conflicts  │         │ Working Copy │      │
//! │  │ (contents)   │         │ Tracked    │         │ Updated      │      │
//! │  └──────────────┘         └────────────┘         └──────────────┘      │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Module Structure
//!
//! - [`error`]: Error types for output operations
//! - [`traits`]: Working copy abstraction traits (`WorkingCopy`, `VertexBuffer`)
//! - [`memory`]: In-memory working copy for testing
//! - [`crdt`]: CRDT-based content retrieval (line-by-line with token granularity)
//!
//! # Conflict Handling
//!
//! When the graph contains ambiguous orderings, the output includes conflict
//! markers similar to other VCS tools:
//!
//! ```text
//! >>>>>>> 1 [ABCD1234]
//! Content from first change
//! ======= 1
//! Content from second change
//! <<<<<<< 1
//! ```
//!
//! Three types of conflicts are tracked:
//!
//! - **Order Conflicts**: Multiple valid orderings for content at the same position
//! - **Zombie Conflicts**: Deleted content that has been modified by another change
//! - **Cyclic Conflicts**: Circular dependencies in the content graph
//!
//! # Example: Basic Output
//!
//! ```rust,ignore
//! use atomic_core::output::{Memory, WorkingCopy, OutputResult};
//!
//! // Create an in-memory working copy
//! let wc = Memory::new();
//!
//! // Output the repository state
//! let conflicts = output_repository(
//!     &wc,
//!     &changes,
//!     &txn,
//!     &channel,
//!     "",           // prefix (empty = all files)
//!     true,         // output_name_conflicts
//!     None,         // if_modified_since
//! )?;
//!
//! // Check for conflicts
//! if !conflicts.is_empty() {
//!     for conflict in &conflicts {
//!         eprintln!("Conflict in {}: {}", conflict.path(), conflict.conflict_type());
//!     }
//! }
//! ```
//!
//! # Example: Using VertexBuffer
//!
//! ```rust
//! use atomic_core::output::{Writer, VertexBuffer, markers};
//! use atomic_core::types::{NodeId, GraphNode, ChangePosition};
//!
//! let mut output = Vec::new();
//! let mut writer = Writer::new(&mut output);
//!
//! // Begin a conflict
//! writer.begin_conflict(1, None).unwrap();
//!
//! // Output first side
//! let v1 = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(6));
//! let _: Result<(), std::io::Error> = writer.output_line(v1, |buf| {
//!     buf.copy_from_slice(b"side A");
//!     Ok(())
//! });
//!
//! // Separator
//! writer.conflict_next(1, None).unwrap();
//!
//! // Output second side
//! let v2 = GraphNode::new(NodeId::new(2), ChangePosition::new(0), ChangePosition::new(6));
//! let _: Result<(), std::io::Error> = writer.output_line(v2, |buf| {
//!     buf.copy_from_slice(b"side B");
//!     Ok(())
//! });
//!
//! // End conflict
//! writer.end_conflict(1).unwrap();
//!
//! let result = String::from_utf8(output).unwrap();
//! assert!(result.contains(markers::START));
//! assert!(result.contains(markers::SEPARATOR));
//! assert!(result.contains(markers::END));
//! ```
//!
//! # Performance Considerations
//!
//! - **Incremental Output**: Use `if_modified_since` to only output changed files
//! - **Parallel Processing**: Future support for parallel file output
//! - **Memory Efficiency**: Stream content rather than loading entire files
//!
//! # Thread Safety
//!
//! Output operations require exclusive access to the working copy. The traits
//! do not require `Sync`, so concurrent output to different files is possible
//! with appropriate coordination.

pub mod alive;
pub mod crdt;
mod error;
pub mod filesystem;
pub mod memory;
pub mod repo;
mod traits;

// Re-export error types
pub use error::{
    ConflictType, ContentError, ContentResult, OutputError, OutputResult, TreeError, TreeResult,
};

// Re-export traits
pub use traits::{
    markers, FileMetadata, Sink, SinkError, VertexBuffer, WorkingCopy, WorkingCopyRead, Writer,
};

// Re-export memory implementation
pub use memory::{Memory, MemoryError};

// Re-export filesystem implementation
pub use filesystem::{FileSystem, FileWriter, DOT_DIR};

// Re-export repository output types
pub use repo::{
    FileConflict, FileConflictType, FileWritten, OutputError as OutputRepoError, OutputOptions,
    OutputOutcome, OutputResult as OutputRepoResult,
};

// Re-export alive graph types
pub use alive::{
    compute_order, retrieve_graph, AliveGraph, AliveVertex, ConflictPath, ConflictTree, GraphStats,
    OrderResult, PathElement, RedundantEdge, RetrieveOptions, RetrieveResult, SccId, VertexFlags,
    VertexId,
};

// Re-export CRDT content retrieval types
pub use crdt::{
    file_exists, get_file, get_file_lines, get_file_lines_with_options, get_file_with_options,
    get_trunk_id, File, Line, RetrievalOptions, Token,
};

use crate::types::{Hash, Inode, NodeId, Position};

// CONFLICT TRACKING

/// A conflict detected during output.
///
/// Conflicts occur when the graph state cannot be unambiguously serialized
/// to file content. The conflict is marked in the output with special markers,
/// and tracked here for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The type of conflict.
    pub conflict_type: ConflictType,
    /// Path to the file containing the conflict.
    pub path: String,
    /// Position in the graph where the conflict occurs.
    pub inode_vertex: Position<NodeId>,
    /// Line number in the output file (1-based).
    pub line: usize,
    /// Changes involved in this conflict.
    pub changes: Vec<Hash>,
    /// Unique ID for matching begin/end markers.
    pub id: usize,
}

impl Conflict {
    /// Create a new order conflict.
    ///
    /// Order conflicts occur when multiple changes insert content at the
    /// same position with no clear ordering.
    pub fn order(
        path: String,
        inode_vertex: Position<NodeId>,
        line: usize,
        changes: Vec<Hash>,
        id: usize,
    ) -> Self {
        Self {
            conflict_type: ConflictType::Order,
            path,
            inode_vertex,
            line,
            changes,
            id,
        }
    }

    /// Create a new zombie conflict.
    ///
    /// Zombie conflicts occur when content has been deleted by one change
    /// but modified by another.
    pub fn zombie(
        path: String,
        inode_vertex: Position<NodeId>,
        line: usize,
        changes: Vec<Hash>,
        id: usize,
    ) -> Self {
        Self {
            conflict_type: ConflictType::Zombie,
            path,
            inode_vertex,
            line,
            changes,
            id,
        }
    }

    /// Create a new cyclic conflict.
    ///
    /// Cyclic conflicts occur when there are circular dependencies in the
    /// content graph that prevent linear output.
    pub fn cyclic(
        path: String,
        inode_vertex: Position<NodeId>,
        line: usize,
        changes: Vec<Hash>,
        id: usize,
    ) -> Self {
        Self {
            conflict_type: ConflictType::Cyclic,
            path,
            inode_vertex,
            line,
            changes,
            id,
        }
    }

    /// Create a new name conflict.
    ///
    /// Name conflicts occur when a file has been renamed by concurrent changes.
    pub fn name(path: String, changes: Vec<Hash>, id: usize) -> Self {
        Self {
            conflict_type: ConflictType::Name,
            path,
            inode_vertex: Position::ROOT,
            line: 0,
            changes,
            id,
        }
    }

    /// Get the file path where this conflict occurs.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the conflict type.
    pub fn conflict_type(&self) -> ConflictType {
        self.conflict_type
    }

    /// Get the line number where the conflict starts (1-based).
    pub fn line(&self) -> usize {
        self.line
    }

    /// Get the changes involved in this conflict.
    pub fn changes(&self) -> &[Hash] {
        &self.changes
    }

    /// Get the conflict ID.
    pub fn id(&self) -> usize {
        self.id
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} in {} at line {} (id: {})",
            self.conflict_type, self.path, self.line, self.id
        )
    }
}

// OUTPUT ITEM

/// An item to be output to the working copy.
///
/// This represents a file or directory that needs to be written or updated.
#[derive(Debug, Clone)]
pub struct OutputItem {
    /// Parent inode.
    pub parent: Inode,
    /// Path to the item (relative to repository root).
    pub path: String,
    /// File metadata (permissions, type).
    pub metadata: FileMetadata,
    /// Graph position for file content.
    pub pos: Position<NodeId>,
    /// If this is a zombie file, the changes that deleted it.
    pub is_zombie: Option<Vec<Hash>>,
}

impl OutputItem {
    /// Create a new output item for a regular file.
    pub fn file(parent: Inode, path: String, pos: Position<NodeId>) -> Self {
        Self {
            parent,
            path,
            metadata: FileMetadata::file(),
            pos,
            is_zombie: None,
        }
    }

    /// Create a new output item for a directory.
    pub fn directory(parent: Inode, path: String, pos: Position<NodeId>) -> Self {
        Self {
            parent,
            path,
            metadata: FileMetadata::directory(),
            pos,
            is_zombie: None,
        }
    }

    /// Set this item as a zombie (deleted but with live content).
    pub fn with_zombie(mut self, changes: Vec<Hash>) -> Self {
        self.is_zombie = Some(changes);
        self
    }

    /// Set custom metadata.
    pub fn with_metadata(mut self, metadata: FileMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Check if this is a zombie item.
    pub fn is_zombie(&self) -> bool {
        self.is_zombie.is_some()
    }

    /// Check if this is a directory.
    pub fn is_directory(&self) -> bool {
        self.metadata.is_dir
    }
}

// OUTPUT STATISTICS

/// Statistics from an output operation.
///
/// Tracks counts of various operations performed during output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputStats {
    /// Number of files written.
    pub files_written: usize,
    /// Number of directories created.
    pub directories_created: usize,
    /// Number of files deleted.
    pub files_deleted: usize,
    /// Number of files skipped (not modified).
    pub files_skipped: usize,
    /// Number of conflicts detected.
    pub conflicts: usize,
    /// Total bytes written.
    pub bytes_written: u64,
}

impl OutputStats {
    /// Create empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge statistics from another operation.
    pub fn merge(&mut self, other: &Self) {
        self.files_written += other.files_written;
        self.directories_created += other.directories_created;
        self.files_deleted += other.files_deleted;
        self.files_skipped += other.files_skipped;
        self.conflicts += other.conflicts;
        self.bytes_written += other.bytes_written;
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        self.conflicts > 0
    }
}

impl std::fmt::Display for OutputStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} files written, {} dirs created, {} deleted, {} skipped, {} conflicts",
            self.files_written,
            self.directories_created,
            self.files_deleted,
            self.files_skipped,
            self.conflicts
        )
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangePosition, GraphNode};

    // -------------------------------------------------------------------------
    // Conflict Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_conflict_order() {
        let hash = Hash::of(b"test");
        let pos = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(0),
        };
        let conflict = Conflict::order("src/main.rs".to_string(), pos, 42, vec![hash], 1);

        assert_eq!(conflict.conflict_type(), ConflictType::Order);
        assert_eq!(conflict.path(), "src/main.rs");
        assert_eq!(conflict.line(), 42);
        assert_eq!(conflict.id(), 1);
        assert_eq!(conflict.changes().len(), 1);
    }

    #[test]
    fn test_conflict_zombie() {
        let conflict = Conflict::zombie("test.txt".to_string(), Position::ROOT, 10, vec![], 5);
        assert_eq!(conflict.conflict_type(), ConflictType::Zombie);
    }

    #[test]
    fn test_conflict_cyclic() {
        let conflict = Conflict::cyclic("circular.rs".to_string(), Position::ROOT, 1, vec![], 3);
        assert_eq!(conflict.conflict_type(), ConflictType::Cyclic);
    }

    #[test]
    fn test_conflict_name() {
        let conflict = Conflict::name("renamed.txt".to_string(), vec![], 7);
        assert_eq!(conflict.conflict_type(), ConflictType::Name);
        assert_eq!(conflict.line(), 0); // Name conflicts don't have a specific line
    }

    #[test]
    fn test_conflict_display() {
        let conflict = Conflict::order("file.rs".to_string(), Position::ROOT, 100, vec![], 1);
        let display = conflict.to_string();
        assert!(display.contains("order conflict"));
        assert!(display.contains("file.rs"));
        assert!(display.contains("100"));
    }

    #[test]
    fn test_conflict_equality() {
        let c1 = Conflict::order("a.txt".to_string(), Position::ROOT, 1, vec![], 1);
        let c2 = Conflict::order("a.txt".to_string(), Position::ROOT, 1, vec![], 1);
        let c3 = Conflict::order("b.txt".to_string(), Position::ROOT, 1, vec![], 1);

        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    // -------------------------------------------------------------------------
    // OutputItem Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_output_item_file() {
        let item = OutputItem::file(Inode::ROOT, "test.txt".to_string(), Position::ROOT);
        assert!(!item.is_directory());
        assert!(!item.is_zombie());
    }

    #[test]
    fn test_output_item_directory() {
        let item = OutputItem::directory(Inode::ROOT, "src".to_string(), Position::ROOT);
        assert!(item.is_directory());
    }

    #[test]
    fn test_output_item_with_zombie() {
        let hash = Hash::of(b"deleter");
        let item = OutputItem::file(Inode::ROOT, "deleted.txt".to_string(), Position::ROOT)
            .with_zombie(vec![hash]);
        assert!(item.is_zombie());
        assert_eq!(item.is_zombie.unwrap().len(), 1);
    }

    #[test]
    fn test_output_item_with_metadata() {
        let meta = FileMetadata::executable();
        let item = OutputItem::file(Inode::ROOT, "script.sh".to_string(), Position::ROOT)
            .with_metadata(meta);
        assert!(item.metadata.is_executable());
    }

    // -------------------------------------------------------------------------
    // OutputStats Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_output_stats_new() {
        let stats = OutputStats::new();
        assert_eq!(stats.files_written, 0);
        assert_eq!(stats.conflicts, 0);
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_output_stats_merge() {
        let mut s1 = OutputStats {
            files_written: 5,
            directories_created: 2,
            files_deleted: 1,
            files_skipped: 3,
            conflicts: 1,
            bytes_written: 1000,
        };

        let s2 = OutputStats {
            files_written: 3,
            directories_created: 1,
            files_deleted: 2,
            files_skipped: 0,
            conflicts: 2,
            bytes_written: 500,
        };

        s1.merge(&s2);

        assert_eq!(s1.files_written, 8);
        assert_eq!(s1.directories_created, 3);
        assert_eq!(s1.files_deleted, 3);
        assert_eq!(s1.files_skipped, 3);
        assert_eq!(s1.conflicts, 3);
        assert_eq!(s1.bytes_written, 1500);
    }

    #[test]
    fn test_output_stats_has_conflicts() {
        let mut stats = OutputStats::new();
        assert!(!stats.has_conflicts());

        stats.conflicts = 1;
        assert!(stats.has_conflicts());
    }

    #[test]
    fn test_output_stats_display() {
        let stats = OutputStats {
            files_written: 10,
            directories_created: 3,
            files_deleted: 2,
            files_skipped: 5,
            conflicts: 1,
            bytes_written: 2048,
        };
        let display = stats.to_string();
        assert!(display.contains("10"));
        assert!(display.contains("3"));
        assert!(display.contains("2"));
        assert!(display.contains("5"));
        assert!(display.contains("1"));
    }

    #[test]
    fn test_output_stats_default() {
        let stats = OutputStats::default();
        assert_eq!(stats, OutputStats::new());
    }

    // -------------------------------------------------------------------------
    // Integration-style Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_memory_working_copy_basic_workflow() {
        let wc = Memory::new();

        // Create directory structure
        wc.create_dir_all("src/utils").unwrap();

        // Write a file
        let inode = wc.allocate_inode();
        {
            use std::io::Write;
            let mut writer = wc.write_file("src/main.rs", inode).unwrap();
            writer.write_all(b"fn main() {}").unwrap();
        }

        // Verify
        let mut buffer = Vec::new();
        wc.read_file("src/main.rs", &mut buffer).unwrap();
        assert_eq!(buffer, b"fn main() {}");
    }

    #[test]
    fn test_writer_vertex_buffer_workflow() {
        let mut output = Vec::new();
        let mut writer = Writer::new(&mut output);

        // Write a simple span
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );
        let result: Result<(), std::io::Error> = writer.output_line(v, |buf| {
            buf.copy_from_slice(b"hello");
            Ok(())
        });
        result.unwrap();

        assert_eq!(output, b"hello");
    }

    #[test]
    fn test_sink_discards_output() {
        use std::io::Write;

        let sink = Sink::new();
        assert!(!sink.is_writable("/any/path").unwrap());

        let mut writer = sink.write_file("/test", Inode::ROOT).unwrap();
        writer.write_all(b"discarded").unwrap();
        // No error, but nothing stored
    }
}
