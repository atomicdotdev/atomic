//! Workflow functions for change detection and recording.
//!
//! This module provides the main entry points for detecting changes in the
//! working copy and recording them as change objects. It is organized into
//! focused submodules for maintainability.
//!
//! # Module Structure
//!
//! ```text
//! workflow/
//! ├── mod.rs        # This file - exports and documentation
//! ├── options.rs    # WorkflowOptions configuration
//! ├── collect.rs    # File collection from pristine/working copy
//! ├── compare.rs    # Content comparison and diffing
//! ├── retrieve.rs   # Content retrieval from pristine graph
//! ├── graph_op.rs       # GraphOp building from diff operations
//! ├── detect.rs     # High-level change detection
//! ├── record.rs     # Recording functions to build changes
//! └── crdt/         # CRDT operation generation (Trunk → Branch → Leaf)
//! ```
//!
//! # Overview
//!
//! The recording workflow consists of several stages:
//!
//! 1. **Collection**: Gather files from pristine and working copy
//! 2. **Detection**: Identify added, deleted, and modified files
//! 3. **Comparison**: Diff content to generate operations
//! 4. **Building**: Convert operations into change hunks
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Recording Workflow                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌──────────────────┐                                                   │
//! │  │  WorkflowOptions │ ◄─── Configuration for all operations             │
//! │  └────────┬─────────┘                                                   │
//! │           │                                                             │
//! │           ▼                                                             │
//! │  ┌──────────────────┐     ┌──────────────────┐                         │
//! │  │ collect_tracked  │     │ collect_working  │                         │
//! │  │ (pristine files) │     │ (disk files)     │                         │
//! │  └────────┬─────────┘     └────────┬─────────┘                         │
//! │           │                        │                                    │
//! │           └────────────┬───────────┘                                    │
//! │                        │                                                │
//! │                        ▼                                                │
//! │              ┌──────────────────┐                                       │
//! │              │  compare_content │ ◄─── Diff and encoding detection      │
//! │              └────────┬─────────┘                                       │
//! │                       │                                                 │
//! │                       ▼                                                 │
//! │              ┌──────────────────┐                                       │
//! │              │  DetectResult    │ ◄─── Categorized file changes         │
//! │              └──────────────────┘                                       │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::{
//!     WorkflowOptions,
//!     collect::{collect_tracked_files, get_working_file},
//!     compare::compare_content,
//! };
//! use atomic_core::diff::Algorithm;
//!
//! // Configure the workflow
//! let options = WorkflowOptions::new()
//!     .algorithm(Algorithm::Patience)
//!     .prefix("src/");
//!
//! // Collect tracked files
//! let tracked = collect_tracked_files(&txn, options.prefix())?;
//!
//! // Check each tracked file
//! for file in &tracked.files {
//!     if let Some(working) = get_working_file(&working_copy, &file.path)? {
//!         // File exists - compare content
//!         let old_content = get_pristine_content(&txn, &changes, &file)?;
//!         let new_content = read_working_content(&working_copy, &file.path)?;
//!
//!         let result = compare_content(&old_content, &new_content, options.algorithm());
//!         if result.has_changes() {
//!             println!("Modified: {}", file.path);
//!         }
//!     } else {
//!         // File deleted
//!         println!("Deleted: {}", file.path);
//!     }
//! }
//! ```
//!
//! # Submodule Documentation
//!
//! - [`options`]: Configuration for workflow behavior
//! - [`collect`]: File collection from pristine and working copy
//! - [`compare`]: Content comparison and diff generation
//! - [`retrieve`]: Content retrieval from the pristine graph
//! - [`graph_op`]: GraphOp building from diff operations
//! - [`detect`]: High-level change detection integrating all modules
//! - [`record`]: Recording functions to build changes from detected files
//! - [`crdt`]: CRDT operation generation for hierarchical graph model

pub mod assembly;
pub mod collect;
pub mod compare;
pub mod crdt;
pub mod detect;
pub mod globalize;
pub mod graph_op;
pub mod options;
pub mod record;
pub mod retrieve;

// Re-export main types for convenience
pub use assembly::{
    assemble_change, compute_content_offsets, collect_dependencies, create_empty_change,
    finalize_hunks, AssemblyContext, AssemblyError, AssemblyOptions, AssemblyResult_,
    AssemblyStats,
};
pub use collect::{
    collect_tracked_files, collect_working_files, collect_working_paths, get_tracked_file,
    get_working_file, CollectionResult, HasPath, TrackedFile, WorkingFile,
};
pub use compare::{
    compare_content, compare_content_with_limit, content_identical, detect_encoding, generate_diff,
    is_binary, CompareResult,
};
pub use detect::{
    detect_changes_simple, DetectedFile, DetectionKind, DetectionOptions, DetectionResult,
};
pub use globalize::{
    create_content_vertex, create_deletion_edges, create_inode_vertex, create_name_vertex,
    extract_filename, extract_parent, globalize_hunk, globalize_recorded_file,
    resolve_file_position, resolve_inode_to_position, resolve_parent_inode, resolve_path_to_inode,
    CacheStats, GlobalizeContext, GlobalizeError, GlobalizeOptions, GlobalizeResult,
    GlobalizedFile,
};
pub use graph_op::{
    BuiltHunk, BuiltHunkKind, HunkBuildOptions, HunkBuildResult, HunkBuilder, PendingChange,
    PendingChangeKind,
};
pub use options::WorkflowOptions;
pub use record::{
    record_added_file, record_deleted_file, record_modified_file, RecordedFile, RecordingOptions,
    RecordingResult, RecordingStats,
};

// Re-export commonly used CRDT types for token-level diff support
pub use crdt::{
    CrdtBuildStats, CrdtChangeBuilder, CrdtChangeResult, ContentTokenizer, FileOps, LineOps,
    TokenOps, TokenizeOptions,
};
pub use retrieve::{
    has_content, retrieve_content, retrieve_content_with_options, RetrieveContentOptions,
    RetrieveResult,
};

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports_options() {
        let opts = WorkflowOptions::new();
        assert!(opts.get_check_mtime());
    }

    #[test]
    fn test_module_exports_collect_types() {
        use crate::types::{ChangePosition, Inode, NodeId, Position};

        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let tracked = TrackedFile::new("test.rs", Inode::new(1), pos);
        assert_eq!(tracked.path(), "test.rs");

        let working = WorkingFile::new("test.rs");
        assert_eq!(working.path(), "test.rs");
    }

    #[test]
    fn test_module_exports_compare() {
        use crate::change::Encoding;
        use crate::diff::Algorithm;

        let result = compare_content(b"old", b"new", Algorithm::Myers);
        assert!(result.has_changes());

        let enc = detect_encoding(b"hello");
        assert_eq!(enc, Encoding::Utf8);
    }

    #[test]
    fn test_workflow_integration() {
        use crate::diff::Algorithm;

        // This test demonstrates the intended workflow
        let options = WorkflowOptions::new()
            .algorithm(Algorithm::Patience)
            .check_mtime(false);

        // Verify options are configured
        assert_eq!(options.get_algorithm(), Algorithm::Patience);
        assert!(!options.get_check_mtime());

        // Compare some content
        let old = b"line1\nline2\n";
        let new = b"line1\nmodified\n";

        let result = compare_content(old, new, options.get_algorithm());
        assert!(result.has_changes());
        assert!(!result.is_binary);
    }
}
