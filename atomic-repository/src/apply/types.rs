//! Error types, options, statistics, and outcome types for change insertion.

use atomic_core::apply::{ConflictSummary, LocalApplyError};
use atomic_core::types::{Base32, Hash, Merkle};
use thiserror::Error;

// ── Error Types ──────────────────────────────────────────────────────

/// Result type for insert operations.
pub type InsertResult<T> = Result<T, InsertError>;

/// Errors that can occur during change insertion.
#[derive(Debug, Error)]
pub enum InsertError {
    /// The change was not found in the change store.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// The hash of the missing change
        hash: String,
    },

    /// One or more dependencies are missing.
    #[error("Missing dependencies: {}", format_hashes(.missing))]
    MissingDependencies {
        /// The hashes of missing dependencies
        missing: Vec<Hash>,
    },

    /// The change is already applied to the view.
    #[error("Change already applied: {hash}")]
    AlreadyApplied {
        /// The hash of the already-applied change
        hash: String,
    },

    /// A conflict was detected during insertion.
    #[error("Conflict during insertion: {message}")]
    Conflict {
        /// Description of the conflict
        message: String,
    },

    /// The change file is corrupted or invalid.
    #[error("Invalid change: {message}")]
    InvalidChange {
        /// Description of the problem
        message: String,
    },

    /// A cyclic dependency was detected.
    #[error("Cyclic dependency detected: {message}")]
    CyclicDependency {
        /// Description of the cycle
        message: String,
    },

    /// An internal error occurred.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LocalApplyError> for InsertError {
    fn from(e: LocalApplyError) -> Self {
        match e {
            LocalApplyError::DependencyMissing { hash } => InsertError::MissingDependencies {
                missing: vec![hash],
            },
            LocalApplyError::ChangeAlreadyApplied { hash } => InsertError::AlreadyApplied {
                hash: hash.to_base32(),
            },
            LocalApplyError::CyclicDependency { message } => {
                InsertError::CyclicDependency { message }
            }
            LocalApplyError::InvalidChange => InsertError::InvalidChange {
                message: "Change format is invalid".to_string(),
            },
            LocalApplyError::Corruption => InsertError::InvalidChange {
                message: "Change data is corrupted".to_string(),
            },
            other => InsertError::Internal(other.to_string()),
        }
    }
}

/// Format a list of hashes for display.
pub(crate) fn format_hashes(hashes: &[Hash]) -> String {
    hashes
        .iter()
        .map(|h| h.to_base32())
        .collect::<Vec<_>>()
        .join(", ")
}

// ── InsertOptions ────────────────────────────────────────────────────

/// Options for controlling change insertion.
#[derive(Debug, Clone)]
pub struct InsertOptions {
    /// View to insert to (None = current view).
    ///
    /// When `None`, the change is inserted into the repository's current view.
    /// Default: `None`
    pub view: Option<String>,

    /// Automatically insert missing dependencies if available.
    ///
    /// When `true`, if a change's dependencies are missing but available
    /// in the change store, they will be inserted first.
    /// Default: `false`
    pub apply_dependencies: bool,

    /// Allow inserting even if conflicts are detected.
    ///
    /// When `false`, insertion stops on the first conflict.
    /// When `true`, conflicts are tracked but insertion continues.
    /// Default: `true`
    pub allow_conflicts: bool,

    /// Maximum depth for recursive dependency resolution.
    ///
    /// This prevents infinite loops with cyclic dependencies.
    /// Default: `100`
    pub max_depth: usize,

    /// Record detailed conflict information.
    ///
    /// When `true`, detailed conflict tracking is performed.
    /// Default: `true`
    pub track_conflicts: bool,

    /// Skip the "already applied" validation check.
    ///
    /// When `true`, `validate_can_apply` is bypassed.  This is used by
    /// `rebuild_change_graph` which intentionally re-applies a change
    /// that is already in the view log (same NodeId, new hunks).
    /// Default: `false`
    pub skip_validation: bool,
}

impl Default for InsertOptions {
    fn default() -> Self {
        Self {
            view: None,
            apply_dependencies: false,
            allow_conflicts: true,
            max_depth: 100,
            track_conflicts: true,
            skip_validation: false,
        }
    }
}

impl InsertOptions {
    /// Set the view to insert to.
    pub fn view(mut self, name: impl Into<String>) -> Self {
        self.view = Some(name.into());
        self
    }

    /// Create options that will insert dependencies automatically.
    pub fn with_dependencies() -> Self {
        Self {
            apply_dependencies: true,
            ..Default::default()
        }
    }

    /// Create options that stop on conflicts.
    pub fn strict() -> Self {
        Self {
            allow_conflicts: false,
            ..Default::default()
        }
    }

    /// Set whether to insert dependencies automatically.
    pub fn apply_deps(mut self, apply: bool) -> Self {
        self.apply_dependencies = apply;
        self
    }

    /// Set whether to allow conflicts.
    pub fn allow_conflict(mut self, allow: bool) -> Self {
        self.allow_conflicts = allow;
        self
    }

    /// Skip the "already applied" validation check.
    ///
    /// Used by `rebuild_change_graph` which re-applies a change that is
    /// already in the view log (same NodeId, new hunks after revise).
    pub fn skip_validation(mut self, skip: bool) -> Self {
        self.skip_validation = skip;
        self
    }

    /// Set maximum recursion depth.
    pub fn max_recursion(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

// ── InsertStats ──────────────────────────────────────────────────────

/// Statistics from a change insertion operation.
#[derive(Debug, Clone, Default)]
pub struct InsertStats {
    /// Number of changes inserted.
    pub changes_applied: usize,

    /// Number of atoms (vertices + edges) processed.
    pub atoms_processed: usize,

    /// Number of conflicts detected.
    pub conflicts_detected: usize,

    /// Number of dependencies that were automatically inserted.
    pub dependencies_applied: usize,

    /// Hashes of changes that were inserted.
    pub applied_hashes: Vec<Hash>,

    /// Conflict summary if any conflicts were detected.
    pub conflict_summary: Option<ConflictSummary>,
}

impl InsertStats {
    /// Create empty stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any changes were inserted.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        self.conflicts_detected > 0
    }

    /// Merge stats from another operation.
    pub fn merge(&mut self, other: InsertStats) {
        self.changes_applied += other.changes_applied;
        self.atoms_processed += other.atoms_processed;
        self.conflicts_detected += other.conflicts_detected;
        self.dependencies_applied += other.dependencies_applied;
        self.applied_hashes.extend(other.applied_hashes);
        if other.conflict_summary.is_some() {
            self.conflict_summary = other.conflict_summary;
        }
    }
}

// ── InsertOutcome ────────────────────────────────────────────────────

/// The result of inserting a change.
#[derive(Debug, Clone)]
pub struct InsertOutcome {
    /// The new Merkle state of the view after insertion.
    pub new_state: Merkle,

    /// The sequence number of the inserted change on the view.
    pub sequence: u64,

    /// Whether any conflicts were detected during insertion.
    pub has_conflicts: bool,

    /// Statistics about the insertion.
    pub stats: InsertStats,
}

impl InsertOutcome {
    /// Create a new outcome.
    pub fn new(new_state: Merkle, sequence: u64, has_conflicts: bool, stats: InsertStats) -> Self {
        Self {
            new_state,
            sequence,
            has_conflicts,
            stats,
        }
    }
}
