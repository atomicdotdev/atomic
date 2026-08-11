//! Recording changes from the working copy.
//!
//! This module provides the high-level interface for recording changes from
//! the working copy into serializable `Change` objects that can be applied
//! to other repositories.
//!
//! # Overview
//!
//! Recording is the process of:
//!
//! 1. **Detecting** modifications in the working copy (added, modified, deleted files)
//! 2. **Comparing** file contents with the pristine state
//! 3. **Building** hunks that describe the differences
//! 4. **Globalizing** local positions to graph vertices
//! 5. **Assembling** a complete change with dependencies
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Recording Pipeline                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Working Copy          Detection           Globalization    Change      │
//! │  ┌──────────┐         ┌──────────┐        ┌──────────┐    ┌──────────┐ │
//! │  │  Files   │  scan   │ Modified │ build  │ Graph    │asm │  Hash    │ │
//! │  │  on disk │ ──────▶ │ Tracked  │ ─────▶ │ Vertices │───▶│  Hunks   │ │
//! │  └──────────┘         └──────────┘        │ Edges    │    │  Content │ │
//! │       │                    │              └──────────┘    └──────────┘ │
//! │       │                    │                   │               │       │
//! │       ▼                    ▼                   ▼               ▼       │
//! │  ┌──────────┐         ┌──────────┐        ┌──────────┐    ┌──────────┐ │
//! │  │ Pristine │  diff   │ Built    │  deps  │ Depend-  │save│ Stored   │ │
//! │  │  State   │ ◄─────▶ │ Hunks    │ ─────▶ │ encies   │───▶│ on Disk  │ │
//! │  └──────────┘         └──────────┘        └──────────┘    └──────────┘ │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, RecordOptions};
//! use atomic_core::change::{Author, ChangeHeader};
//!
//! let repo = Repository::open(".")?;
//!
//! // Create change header
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .author(Author::new("Alice", Some("alice@example.com")))
//!     .build();
//!
//! // Record all changes
//! let result = repo.record(header, RecordOptions::default())?;
//! println!("Created change: {}", result.hash.to_base32());
//! ```
//!
//! # Partial Recording
//!
//! You can record a subset of changes by specifying paths:
//!
//! ```rust,ignore
//! let options = RecordOptions::default()
//!     .paths(vec!["src/main.rs", "src/lib.rs"]);
//!
//! let result = repo.record(header, options)?;
//! ```

mod assemble;
mod options;

pub use assemble::{build_header, filter_files, RecordOutcome, RecordStats};
pub use options::RecordOptions;

use atomic_core::record::workflow::{AssemblyError, GlobalizeError};
use thiserror::Error;

use crate::RepositoryError;

// ERROR TYPES

/// Result type for record operations.
pub type RecordResult<T> = Result<T, RecordError>;

/// Errors that can occur during recording.
#[derive(Debug, Error)]
pub enum RecordError {
    /// No changes to record.
    ///
    /// The working copy matches the pristine state.
    #[error("Nothing to record: working copy is clean")]
    NothingToRecord,

    /// No files matched the specified paths.
    #[error("No files matched the specified paths")]
    NoFilesMatched,

    /// File not found in working copy.
    #[error("File not found: {path}")]
    FileNotFound {
        /// The path that was not found
        path: String,
    },

    /// File not tracked.
    #[error("File not tracked: {path}")]
    FileNotTracked {
        /// The untracked file path
        path: String,
    },

    /// Globalization failed.
    #[error("Failed to globalize changes: {0}")]
    Globalize(#[from] GlobalizeError),

    /// Assembly failed.
    #[error("Failed to assemble change: {0}")]
    Assembly(#[from] AssemblyError),

    /// IO error reading files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Repository error.
    #[error("Repository error: {0}")]
    Repository(#[from] RepositoryError),

    /// Change store error.
    #[error("Change store error: {0}")]
    ChangeStore(String),

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// Invalid header.
    #[error("Invalid change header: {reason}")]
    InvalidHeader {
        /// Reason the header is invalid
        reason: String,
    },

    /// Conflicts detected.
    ///
    /// The working copy has unresolved conflicts.
    #[error("Unresolved conflicts in working copy")]
    UnresolvedConflicts,

    /// A file still contains conflict markers.
    ///
    /// Recording would bake the markers into history. Resolve the conflict
    /// (remove the markers) or pass `--allow-conflict-markers` to override.
    #[error(
        "{path} still contains conflict markers at line {line} \
             (resolve the conflict, or pass --allow-conflict-markers to override)"
    )]
    ConflictMarkersPresent {
        /// The file path.
        path: String,
        /// 1-based line of the first marker.
        line: u32,
    },

    /// Binary file exceeds size limit.
    #[error("File too large: {path} ({size} bytes, limit: {limit})")]
    FileTooLarge {
        /// The file path
        path: String,
        /// Actual size in bytes
        size: u64,
        /// Maximum allowed size
        limit: u64,
    },
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
