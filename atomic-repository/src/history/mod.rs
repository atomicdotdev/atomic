//! History operations for Atomic VCS
//!
//! This module provides functionality for querying and traversing the history
//! of changes applied to a stack. History in Atomic is fundamentally different
//! from Git: it's not a linked list of commits but an ordered log of changes
//! applied to a view (stack) of the graph.
//!
//! # Overview
//!
//! Each stack maintains an ordered log of changes that have been applied to it.
//! This log is indexed by sequence number and includes Merkle state hashes at
//! each point, enabling efficient synchronization and state verification.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          Stack History Log                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │   Seq   │   Change Hash        │   Merkle State                        │
//! │  ──────┼─────────────────────┼────────────────────────────────────    │
//! │    0   │ ABC123...            │ state_0 = Hash(empty)                  │
//! │    1   │ DEF456...            │ state_1 = Hash(state_0 || DEF456)      │
//! │    2   │ GHI789...            │ state_2 = Hash(state_1 || GHI789)      │
//! │   ...  │ ...                  │ ...                                    │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Concepts
//!
//! - **Sequence Number**: A 0-indexed position in the change log
//! - **Merkle State**: Cumulative hash representing all changes up to a point
//! - **Change Hash**: Content-addressed identifier for a specific change
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, HistoryOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Get forward history
//! let history = repo.log(HistoryOptions::default())?;
//! for entry in history {
//!     println!("#{}: {} (state: {})",
//!         entry.sequence,
//!         entry.hash.to_base32(),
//!         entry.state.to_base32()
//!     );
//! }
//!
//! // Get reverse history (most recent first)
//! let history = repo.reverse_log(HistoryOptions::default())?;
//!
//! // Get changes affecting a specific path
//! let path_history = repo.log_for_path("src/main.rs", HistoryOptions::default())?;
//! ```
//!
//! # Performance
//!
//! History queries are efficient O(k) operations where k is the number of
//! entries requested. The underlying B-tree structure allows cursor-based
//! iteration without loading the entire history into memory.

mod iter;
mod operations;
mod types;

pub use iter::{
    find_change_sequence, get_change_at_sequence, history_summary, is_change_in_history, log,
    reverse_log, HistoryIter,
};
pub use operations::{
    get_changes_up_to_change, get_changes_up_to_sequence, get_files_in_change,
    get_state_before_change, StateBeforeChange,
};
pub use types::{
    HistoryEntry, HistoryError, HistoryOptions, HistoryResult, HistorySummary, PathHistoryEntry,
    PathModificationType,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
