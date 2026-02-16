//! Repository output module.
//!
//! This module provides functionality for outputting repository graph state
//! to the working copy (filesystem). It is the inverse of the `record` module -
//! where `record` reads the working copy to create changes, `output` writes
//! the graph state back to files.
//!
//! # Module Structure
//!
//! The output functionality is split into focused submodules:
//!
//! | Module | Description |
//! |--------|-------------|
//! | [`options`] | Configuration for output operations |
//! | [`outcome`] | Results and statistics tracking |
//! | [`conflict`] | Conflict types and tracking |
//! | [`error`] | Error types for output operations |
//! | [`writer`] | Conflict-aware writer implementation |
//! | [`content`] | Graph content output function |
//!
//! # Overview
//!
//! Output is needed after operations that modify the graph:
//!
//! - **Apply**: After applying changes from remotes
//! - **Pull**: After downloading and applying remote changes
//! - **Reset**: When resetting to a different state
//! - **Clone**: To populate the working copy initially
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Output Pipeline                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Pristine Graph              Output Process              Working Copy   │
//! │  ┌──────────────┐           ┌─────────────────┐        ┌────────────┐  │
//! │  │              │  collect  │                 │ write  │            │  │
//! │  │  Vertices    │ ────────► │  Alive Graph    │ ─────► │  Files     │  │
//! │  │  Edges       │           │  SCC Order      │        │  on Disk   │  │
//! │  │  Tree        │           │  Conflicts      │        │            │  │
//! │  └──────────────┘           └─────────────────┘        └────────────┘  │
//! │                                                                         │
//! │  For each file:                                                         │
//! │  1. Find inode position in graph                                        │
//! │  2. Retrieve alive subgraph (non-deleted vertices)                      │
//! │  3. Compute SCC ordering (Tarjan's algorithm)                           │
//! │  4. Detect conflicts (cyclic SCCs, order conflicts)                     │
//! │  5. Write content with conflict markers if needed                       │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::{OutputOptions, OutputOutcome, OutputError};
//! use atomic_core::output::repo::{ConflictWriter, output_graph_content};
//!
//! // Configure output options
//! let options = OutputOptions::new()
//!     .with_prefix("src/")
//!     .output_name_conflicts(true);
//!
//! // Create a conflict-aware writer
//! let mut buffer = Vec::new();
//! let mut writer = ConflictWriter::new(&mut buffer, "file.rs", position);
//!
//! // Output graph content
//! output_graph_content(&changes, hash_fn, &graph, &order, &mut writer)?;
//!
//! // Check for conflicts
//! let conflicts = writer.take_conflicts();
//! if !conflicts.is_empty() {
//!     println!("Warning: {} conflicts detected", conflicts.len());
//! }
//! ```
//!
//! # Conflict Handling
//!
//! When the graph contains conflicting changes, the output includes conflict
//! markers so users can resolve them:
//!
//! ```text
//! >>>>>>> 1 [ABCDEF12]
//! Content from first change
//! ======= 1 [GHIJKL34]
//! Content from second change
//! <<<<<<< 1
//! ```
//!
//! See the [`conflict`] module for details on conflict types.
//!
//! # Conflict Markers
//!
//! The [`markers`] module provides constants for conflict marker strings:
//!
//! - `markers::START` - Beginning of conflict region (`>>>>>>>`)
//! - `markers::SEPARATOR` - Between conflict sides (`=======`)
//! - `markers::END` - End of conflict region (`<<<<<<<`)

mod conflict;
mod content;
mod error;
mod file;
mod options;
mod outcome;
mod repository;
mod tree;
mod writer;

// Re-export all public types
pub use conflict::{FileConflict, FileConflictType};
pub use content::output_graph_content;
pub use error::{OutputError, OutputResult};
pub use file::{output_file, output_file_to_buffer, output_file_to_buffer_with_options, FileOutputError, FileOutputOptions, FileOutputResult};
pub use options::OutputOptions;
pub use outcome::{FileWritten, OutputOutcome};
pub use repository::{
    collect_children, output_repository, output_repository_prefix, OutputItem,
    RepositoryOutputError, RepositoryOutputOptions, RepositoryOutputResult,
};
pub use tree::{
    build_tree_hierarchy, collect_directories, collect_files, collect_tree,
    TreeCollectOptions, TreeCollectResult, TreeItem,
};
pub use writer::{markers, ConflictWriter};
