//! Recording changes from the working copy.
//!
//! The **record** module is responsible for detecting modifications in the
//! working copy and converting them into [`Change`] objects that can be
//! serialized and applied to other repositories.
//!
//! # Overview
//!
//! Recording is the process of:
//!
//! 1. **Scanning** the working copy for modified, added, or deleted files
//! 2. **Comparing** each file's current content with the pristine state
//! 3. **Generating** hunks that describe the differences
//! 4. **Building** a complete change with proper dependency tracking
//!
//! # Module Structure
//!
//! ```text
//! record/
//! ├── mod.rs        # This file - main exports
//! ├── builder.rs    # RecordBuilder for accumulating changes
//! ├── context.rs    # DetectContext, RecordContext for workflow
//! ├── detect.rs     # DetectOptions, FileChange, DetectResult
//! ├── error.rs      # RecordError and result types
//! ├── item.rs       # InodeUpdate, FileMetadata, RecordItem
//! └── workflow/     # Modular workflow implementation
//!     ├── mod.rs      # Workflow module exports
//!     ├── options.rs  # WorkflowOptions configuration
//!     ├── collect.rs  # File collection from pristine/working copy
//!     └── compare.rs  # Content comparison and diffing
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Recording Pipeline                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Working Copy          RecordBuilder           Change                   │
//! │  ┌──────────┐         ┌─────────────┐        ┌─────────────┐           │
//! │  │  Files   │  scan   │  Detected   │ build  │   Hunks     │           │
//! │  │  on disk │ ──────► │  Changes    │ ─────► │   Atoms     │           │
//! │  └──────────┘         └─────────────┘        │   Contents  │           │
//! │       │                     │                └─────────────┘           │
//! │       │                     │                      │                   │
//! │       ▼                     ▼                      ▼                   │
//! │  ┌──────────┐         ┌─────────────┐        ┌─────────────┐           │
//! │  │ Pristine │  diff   │   Hunks:    │ deps   │  Serialized │           │
//! │  │  State   │ ◄─────► │   Edit      │ ─────► │  Change     │           │
//! │  │ (graph)  │         │   Add/Del   │        │  File       │           │
//! │  └──────────┘         └─────────────┘        └─────────────┘           │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Components
//!
//! ## Core Types
//!
//! - [`RecordBuilder`]: Accumulates hunks and content during recording
//! - [`Recorded`]: The result of a recording session
//! - [`RecordError`]: Errors that can occur during recording
//!
//! ## Detection Types
//!
//! - [`DetectOptions`]: Configuration for change detection
//! - [`FileChange`]: Represents a detected change to a file
//! - [`FileChangeKind`]: The type of change (added, modified, deleted, etc.)
//! - [`DetectResult`]: Collection of all detected changes
//!
//! ## Context Types
//!
//! - [`DetectContext`]: Bundles pristine, working copy, and change store for detection
//! - [`RecordContext`]: Extends DetectContext with a builder for recording
//!
//! ## Workflow Types (in `workflow` submodule)
//!
//! - [`workflow::WorkflowOptions`]: Configuration for workflow operations
//! - [`workflow::TrackedFile`]: Information about a tracked file from pristine
//! - [`workflow::WorkingFile`]: Information about a file in the working copy
//! - [`workflow::CompareResult`]: Result of comparing two content blobs
//!
//! # Recording Workflow
//!
//! ## Step 1: Set Up Context
//!
//! ```rust,ignore
//! use atomic_core::record::{RecordContext, DetectOptions};
//!
//! let ctx = RecordContext::new(&txn, &stack, &working_copy, &changes);
//! ```
//!
//! ## Step 2: Configure Detection
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::WorkflowOptions;
//! use atomic_core::diff::Algorithm;
//!
//! let options = WorkflowOptions::new()
//!     .with_algorithm(Algorithm::Patience)
//!     .with_check_mtime(true)
//!     .with_prefix("src/");
//! ```
//!
//! ## Step 3: Collect and Compare
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::{collect_tracked_files, compare_content};
//!
//! let tracked = collect_tracked_files(&txn, "")?;
//! for file in &tracked.files {
//!     // Compare with working copy...
//! }
//! ```
//!
//! ## Step 4: Build Change
//!
//! ```rust,ignore
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .author(Author::new("Alice", Some("alice@example.com")))
//!     .build();
//!
//! let change = builder.finish(header)?;
//! ```
//!
//! # Diff Algorithms
//!
//! The record module uses the diff algorithms from the `crate::diff` module to compare
//! file contents:
//!
//! - **Myers**: Optimal for finding minimal edits
//! - **Patience**: Better for structural changes and code
//!
//! # Dependency Tracking
//!
//! Changes in Atomic have explicit dependencies. When recording, the builder
//! automatically tracks which existing changes the new change depends on:
//!
//! - If you edit a line, you depend on the change that created that line
//! - If you delete a file, you depend on the change that created it
//! - Dependencies form a DAG (directed acyclic graph)
//!
//! # Performance Considerations
//!
//! For large repositories, consider:
//!
//! - Using `workflow::collect_working_paths()` with file watcher output
//! - Enabling mtime checking to skip unchanged files
//! - Using prefix filtering to record only part of the repository
//! - Setting appropriate `max_file_size` limits for diffing
//!
//! # Error Handling
//!
//! Recording can fail for various reasons:
//!
//! - **IO errors**: File system access failures
//! - **Encoding errors**: Invalid file encodings
//! - **Conflict errors**: Unresolved conflicts in the working copy
//! - **Database errors**: Pristine storage issues
//!
//! See [`RecordError`] for the complete list of possible errors.
//!
//! # Example: Using the Workflow Module
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::{
//!     WorkflowOptions, collect_tracked_files, compare_content,
//! };
//! use atomic_core::diff::Algorithm;
//!
//! // Configure workflow
//! let options = WorkflowOptions::new()
//!     .with_algorithm(Algorithm::Myers)
//!     .with_check_mtime(true);
//!
//! // Collect tracked files
//! let tracked = collect_tracked_files(&txn, "")?;
//!
//! // Process each file
//! for file in &tracked.files {
//!     let old_content = get_pristine_content(&file)?;
//!     let new_content = read_working_copy(&file.path)?;
//!
//!     let result = compare_content(&old_content, &new_content, options.algorithm());
//!     if result.has_changes() {
//!         println!("Changed: {}", file.path);
//!     }
//! }
//! ```
//!
//! [`Change`]: crate::change::Change
//! [`RecordBuilder`]: crate::record::RecordBuilder
//! [`RecordError`]: crate::record::RecordError

// SUBMODULES

mod builder;
mod context;
mod detect;
mod error;
mod item;
pub mod workflow;

// RE-EXPORTS

// Builder types
pub use builder::{RecordBuilder, RecordStats, Recorded};

// Context types
pub use context::{
    DetectContext, PristineFileState, RecordContext, RecordItem as ContextRecordItem,
};

// Detection types
pub use detect::{
    compare_content, detect_encoding, is_binary_content, DetectOptions, DetectResult, FileChange,
    FileChangeKind,
};

// Error types
pub use error::{RecordError, RecordResult};

// Item types
pub use item::{FileMetadata, InodeUpdate, RecordItem};

// CONVENIENCE RE-EXPORTS FROM WORKFLOW

/// Workflow configuration options.
///
/// This is a convenience re-export of [`workflow::WorkflowOptions`].
pub use workflow::WorkflowOptions;
