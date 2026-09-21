//! Error types for repository operations

use std::fmt;
use std::path::PathBuf;

use atomic_core::pristine::PristineError;
use atomic_core::WorkingCopyId;
use thiserror::Error;

use crate::remote::RemoteError;

/// Result type for repository operations
pub type Result<T> = std::result::Result<T, RepositoryError>;

/// A cross-process lock participating in the repository operation hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryLockKind {
    /// Repository-common refs, bindings, and operation state.
    Common,
    /// Index and materialization state for one persistent working copy.
    WorkingCopy { id: WorkingCopyId },
    /// Shelved filesystem state for one persistent working copy.
    Shelf { id: WorkingCopyId },
    /// Repository-common deferred TREE journal and alignment state.
    DeferredTree,
}

impl fmt::Display for RepositoryLockKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Common => formatter.write_str("common repository operation lock"),
            Self::WorkingCopy { id } => write!(formatter, "working-copy operation lock for {id}"),
            Self::Shelf { id } => write!(formatter, "working-copy shelf lock for {id}"),
            Self::DeferredTree => formatter.write_str("deferred-tree operation lock"),
        }
    }
}

/// Errors that can occur during repository operations
#[derive(Debug, Error)]
pub enum RepositoryError {
    /// Repository not found at the specified path
    #[error("Repository not found: {path}")]
    NotFound { path: String },

    /// Repository already exists at the specified path
    #[error("Repository already exists at: {path}")]
    AlreadyExists { path: String },

    /// Not inside a repository
    #[error("Not in a Atomic repository (or any parent up to root)")]
    NotInRepository,

    /// Invalid repository structure
    #[error("Invalid repository structure: {reason}")]
    InvalidRepository { reason: String },

    /// The repository requires capabilities unsupported by this Atomic build.
    #[error("{source}")]
    UnsupportedRequiredCapabilities {
        #[source]
        source: PristineError,
    },

    /// View not found
    #[error("View not found: {name}")]
    ViewNotFound { name: String },

    /// View already exists
    #[error("View already exists: {name}")]
    ViewAlreadyExists { name: String },

    /// Cannot delete the current view
    #[error("Cannot delete the current view '{name}'")]
    CannotDeleteCurrentView { name: String },

    /// A change requested for a split is not present in the source view's
    /// own change log (it may be inherited from a parent view).
    #[error("Change {hash} is not in view '{view}'")]
    ChangeNotInView { hash: String, view: String },

    /// Splitting the requested changes would leave changes behind in the
    /// source view that still depend on them. Re-run with cascade to move
    /// the dependents too.
    #[error(
        "cannot split: {} change(s) remaining in '{view}' still depend on the changes being split \
         (use --cascade to move them too): {}",
        blocking.len(),
        blocking.join(", ")
    )]
    ViewSplitHasDependents { view: String, blocking: Vec<String> },

    /// Working copy has uncommitted changes
    #[error("Working copy has uncommitted changes")]
    UncommittedChanges,

    /// Persistent working-copy identity must be initialized by a writable open.
    #[error(
        "working-copy identity at '{}' requires writable migration: {reason}; rerun with a command that opens the repository for writing",
        path.display()
    )]
    WorkingCopyMigrationRequired { path: PathBuf, reason: String },

    /// A nonempty working-copy identity is corrupt and must not be replaced implicitly.
    #[error("malformed working-copy identity at '{}': {reason}", path.display())]
    MalformedWorkingCopyIdentity { path: PathBuf, reason: String },

    /// The requested operation requires a registered physical working copy.
    #[error("this repository handle has no registered physical working copy")]
    WorkingCopyRequired,

    /// No persistent record exists for the requested working-copy identity.
    #[error("working-copy record not found: {id}")]
    WorkingCopyRecordNotFound { id: WorkingCopyId },

    /// The supplied identity does not identify this repository handle's working directory.
    #[error("working-copy identity mismatch: requested {requested}, this directory uses {actual}")]
    WorkingCopyIdentityMismatch {
        requested: WorkingCopyId,
        actual: WorkingCopyId,
    },

    /// The persistent identity is bound to a different canonical location.
    #[error("working-copy identity {id} is bound to a different canonical location")]
    WorkingCopyLocationMismatch { id: WorkingCopyId },

    /// File not found
    #[error("File not found: {path}")]
    FileNotFound { path: PathBuf },

    /// File not tracked
    #[error("File not tracked: {path}")]
    FileNotTracked { path: PathBuf },

    /// File already tracked
    #[error("File already tracked: {path}")]
    FileAlreadyTracked { path: PathBuf },

    /// Path is outside the repository
    #[error("Path is outside the repository: {path}")]
    PathOutsideRepository { path: PathBuf },

    /// Path is ignored by .atomicignore rules
    #[error("Path is ignored: {path}")]
    PathIgnored { path: PathBuf },

    /// Invalid operation (e.g., wrong type of path)
    #[error("Invalid operation: {message}")]
    InvalidOperation { message: String },

    /// The legacy Git shadow pipeline refused to write (CB-13B).
    ///
    /// The repository durably requires the `git-bridge-cutover` capability,
    /// so the colocated bridge owns writes. Nothing was written: the legacy
    /// writer refuses before taking its lock or mutating any state.
    #[error(
        "repository has cut over to the colocated Git bridge: legacy shadow writes are \
         fenced by required capability '{capability}'"
    )]
    LegacyShadowWriterFenced { capability: String },

    /// The trusted provenance publication gate refused (CB-12B, RFC §10.4).
    ///
    /// No metadata, ref, or pack was moved: the refusal happens before any
    /// mutation or publication. Content correctness is NOT implied to be
    /// wrong — the report names the exact evidence blockers and limitations.
    #[error(
        "publication refused at {boundary}: trusted provenance gate found {managed_changes} \
         managed-session change(s) with incomplete evidence (of {checked} checked):\n{report}"
    )]
    PublicationGateRefused {
        boundary: String,
        report: String,
        managed_changes: usize,
        checked: usize,
    },

    /// Change not found
    #[error("Change not found: {hash}")]
    ChangeNotFound { hash: String },

    /// Operation not found by full identity or prefix.
    #[error("Operation not found: {selector}")]
    OperationNotFound { selector: String },

    /// Ambiguous operation identity prefix.
    #[error("Ambiguous operation prefix '{prefix}': matches {}", matches.join(", "))]
    AmbiguousOperation {
        prefix: String,
        matches: Vec<String>,
    },

    /// An operation selector is malformed or too short to resolve safely.
    #[error("Invalid operation selector '{selector}': {reason}")]
    InvalidOperationSelector { selector: String, reason: String },

    /// Concurrent operation heads cannot be consolidated without inventing state.
    #[error("Operation scope '{scope}' is Diverged: {}", heads.join(", "))]
    OperationHeadsDiverged { scope: String, heads: Vec<String> },

    /// A leased Git effect observed a third value: newer external work owns
    /// the resource and was preserved; the effect mutated nothing (RFC §12.6).
    #[error("SyncConflict: leased effect on {target} diverged — expected {expected}, observed {observed}")]
    LeaseDiverged {
        target: String,
        expected: String,
        observed: String,
    },

    /// The selected operation has not completed verification.
    #[error("Operation {operation} is not verified")]
    OperationNotVerified { operation: String },

    /// A metadata-only (reactive watch) boundary deferred instead of acting:
    /// the repository/entry holds unsafe recovery or Git-owned work that only
    /// an explicit command boundary may execute. Nothing was mutated (CB-13D,
    /// RFC §11.2 rule 4).
    #[error("reactive metadata-only boundary deferred: {detail}; the pending work requires an explicit command boundary")]
    ReactiveDeferred { detail: String },

    /// The selected operation is outside the current scope's reachable history.
    #[error("Operation {operation} is not reachable from scope '{scope}'")]
    OperationNotReachable { operation: String, scope: String },

    /// The selected operation cannot be inverted by the current implementation.
    #[error("Operation {operation} ({kind}) is not reversible: {reason}")]
    OperationNotReversible {
        operation: String,
        kind: String,
        reason: String,
    },

    /// Ambiguous hash prefix (multiple matches)
    #[error("Ambiguous hash prefix '{prefix}': matches {}", matches.join(", "))]
    AmbiguousHash {
        prefix: String,
        matches: Vec<String>,
    },

    /// Ambiguous intent UID prefix (multiple matches)
    #[error("Ambiguous intent reference '{prefix}': matches {}", matches.join(", "))]
    AmbiguousIntent {
        prefix: String,
        matches: Vec<String>,
    },

    /// Change already applied
    #[error("Change already applied: {hash}")]
    ChangeAlreadyApplied { hash: String },

    /// Missing dependency
    #[error("Missing dependency: change {change} requires {dependency}")]
    MissingDependency { change: String, dependency: String },

    /// Merge conflict
    #[error("Merge conflict: {description}")]
    MergeConflict { description: String },

    /// Apply error
    #[error("Apply error: {0}")]
    Apply(String),

    /// Tag not found
    #[error("Tag not found: {name}")]
    TagNotFound { name: String },

    /// Tag already exists
    #[error("Tag already exists: {name}")]
    TagAlreadyExists { name: String },

    /// Invalid tag name
    #[error("Invalid tag name '{name}': {reason}")]
    InvalidTagName { name: String, reason: String },

    /// Archive error
    #[error("Archive error: {0}")]
    Archive(String),

    /// Output error (working copy sync)
    #[error("Output error: {0}")]
    Output(String),

    /// Unrecord error
    #[error("Unrecord error: {0}")]
    Unrecord(String),

    /// An ordered operation lock is held by another process.
    #[error("{lock} at '{}' is held by another process; retry the operation", path.display())]
    LockContended {
        lock: RepositoryLockKind,
        path: PathBuf,
    },

    /// A mapping replacement was built from an observation that is no longer
    /// the stored row (CB-10A review R6): the caller must re-observe and
    /// rebuild instead of clobbering a newer row under an invented lease.
    #[error(
        "ref mapping for view '{view}' moved since the caller observed it; \
         re-observe and rebuild the replacement (nothing was written)"
    )]
    RefMappingObservationMoved {
        view: String,
    },

    /// Legacy unscoped lock error.
    #[error("Repository is locked by another process")]
    Locked,

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Remote not found
    #[error("Remote '{name}' not found")]
    RemoteNotFound { name: String },

    /// No remotes configured
    #[error("No remotes configured")]
    NoRemotesConfigured,

    /// Remote error
    #[error("Remote error: {0}")]
    Remote(#[from] RemoteError),

    /// Core library error
    #[error("Core error: {0}")]
    Core(#[from] atomic_core::CoreError),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Database error
    #[error("Database error: {0}")]
    Database(String),

    /// Walkdir error (during file traversal)
    #[error("Directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),

    /// View manifest structural error (parse or fold verification)
    #[error("Manifest error: {0}")]
    Manifest(#[from] crate::manifest::ManifestError),

    /// Manifest references change files not present in the local store
    #[error("Manifest for view '{view}' references {count} change(s) not present locally (first: {first})")]
    ManifestMissingChanges {
        view: String,
        count: usize,
        first: String,
    },

    /// A change in the manifest log has a dependency that is neither earlier
    /// in the log nor already applied locally
    #[error("Manifest for view '{view}': change {change} depends on {dependency}, which is neither earlier in the log nor present locally")]
    ManifestDependencyMissing {
        view: String,
        change: String,
        dependency: String,
    },

    /// The local view log is not a prefix of the manifest log
    #[error("View '{view}' has diverged from the manifest (first mismatch at sequence {at})")]
    ManifestDiverged { view: String, at: u64 },

    /// The local view exists with a different identity (scope or parent)
    #[error("View '{view}' identity mismatch: {reason}")]
    ManifestIdentityMismatch { view: String, reason: String },

    /// The manifest declares a parent view that does not exist locally
    #[error("Manifest for view '{view}' requires parent view '{parent}', which does not exist")]
    ManifestParentMissing { view: String, parent: String },

    /// After replaying the manifest, the view state does not match the
    /// declared merkle
    #[error(
        "View '{view}' state mismatch after manifest apply: declared {declared}, got {actual}"
    )]
    ManifestStateMismatch {
        view: String,
        declared: String,
        actual: String,
    },

    /// A stored Git-state binding names this commit but failed verification.
    /// Resurrection refuses closed instead of re-synthesizing over a known
    /// binding claim (RFC §12.4); the rejection is recoverable by retry.
    #[error(
        "binding {id} for commit {commit} failed verification and cannot be resurrected: {reason}"
    )]
    BindingRejected {
        id: String,
        commit: String,
        reason: String,
    },

    /// The binding's change closure could not be completed from any source.
    #[error(
        "binding {id} closure is incomplete ({count} missing); refusing resurrection: {reasons}"
    )]
    BindingClosureIncomplete {
        id: String,
        count: usize,
        reasons: String,
    },

    /// The isolated projection recomputed from the restored closure does not
    /// match the binding's bound Git tree or SetId (RFC §5.2 projection
    /// compare). Nothing was published; the rejection is recoverable.
    #[error("resurrection of binding {id} rejected: {reason}")]
    ResurrectionRejected { id: String, reason: String },

    /// The observed Git lease changed during resurrection (TOCTOU guard);
    /// the attempt was aborted and the retry budget exhausted.
    #[error(
        "resurrection of binding {id} contended with concurrent Git mutation for {attempts} attempts; refusing"
    )]
    ResurrectionContended { id: String, attempts: u8 },

    /// Internal marker: the Git lease observed before the projection proof
    /// changed before publication. Never surfaced directly; the guard loop
    /// converts it into a fresh attempt or a [`RepositoryError::ResurrectionContended`].
    #[error("Git lease changed during resurrection; retrying")]
    ResurrectionLeaseChanged,

    /// CB-7A: no verified binding covers the observed Git HEAD, so the bound
    /// HEAD adoption refuses. Unbound Git adoption ships with Phase 9 foreign
    /// synthesis (RFC §7.3, §12.4); the prototype importer is never invoked.
    #[error("cannot adopt unbound Git HEAD {head}: {reason}")]
    HeadAdoptionUnbound { head: String, reason: String },

    /// CB-7A: the bound HEAD adoption refused for a typed reason — unexplained
    /// post-checkout differences, unborn/missing HEAD, verification failure,
    /// or contention. Nothing was adopted and no tracked file was rewritten.
    #[error("cannot adopt Git HEAD {head}: {reason}")]
    HeadAdoptionRefused { head: String, reason: String },
}

impl RepositoryError {
    /// Whether retrying after the competing operation completes can succeed.
    pub fn is_lock_contended(&self) -> bool {
        matches!(self, RepositoryError::LockContended { .. })
    }

    /// Check if this error indicates the repository doesn't exist
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            RepositoryError::NotFound { .. } | RepositoryError::NotInRepository
        )
    }

    /// Check if this error is recoverable by user action
    pub fn is_user_fixable(&self) -> bool {
        matches!(
            self,
            RepositoryError::UncommittedChanges
                | RepositoryError::UnsupportedRequiredCapabilities { .. }
                | RepositoryError::WorkingCopyMigrationRequired { .. }
                | RepositoryError::MalformedWorkingCopyIdentity { .. }
                | RepositoryError::MergeConflict { .. }
                | RepositoryError::FileNotTracked { .. }
                | RepositoryError::MissingDependency { .. }
                | RepositoryError::TagAlreadyExists { .. }
                | RepositoryError::InvalidTagName { .. }
                | RepositoryError::ViewAlreadyExists { .. }
                | RepositoryError::ChangeNotInView { .. }
                | RepositoryError::ViewSplitHasDependents { .. }
        )
    }

    /// Check if this error is because a path is ignored
    pub fn is_ignored(&self) -> bool {
        matches!(self, RepositoryError::PathIgnored { .. })
    }

    /// Check if this error is related to tags
    pub fn is_tag_error(&self) -> bool {
        matches!(
            self,
            RepositoryError::TagNotFound { .. }
                | RepositoryError::TagAlreadyExists { .. }
                | RepositoryError::InvalidTagName { .. }
        )
    }

    /// Check if this error is related to remote operations
    pub fn is_remote_error(&self) -> bool {
        matches!(
            self,
            RepositoryError::RemoteNotFound { .. }
                | RepositoryError::NoRemotesConfigured
                | RepositoryError::Remote(_)
        )
    }

    /// Check if this error is related to apply operations
    pub fn is_apply_error(&self) -> bool {
        matches!(
            self,
            RepositoryError::Apply(_)
                | RepositoryError::ChangeNotFound { .. }
                | RepositoryError::ChangeAlreadyApplied { .. }
                | RepositoryError::MissingDependency { .. }
        )
    }
}

impl From<PristineError> for RepositoryError {
    fn from(error: PristineError) -> Self {
        match error {
            source @ PristineError::UnsupportedRequiredCapabilities { .. } => {
                Self::UnsupportedRequiredCapabilities { source }
            }
            other => Self::Database(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for RepositoryError {
    fn from(e: serde_json::Error) -> Self {
        RepositoryError::Serialization(e.to_string())
    }
}

impl From<toml::de::Error> for RepositoryError {
    fn from(e: toml::de::Error) -> Self {
        RepositoryError::Serialization(e.to_string())
    }
}

impl From<toml::ser::Error> for RepositoryError {
    fn from(e: toml::ser::Error) -> Self {
        RepositoryError::Serialization(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_detection() {
        let err = RepositoryError::NotFound {
            path: "/some/path".to_string(),
        };
        assert!(err.is_not_found());

        let err = RepositoryError::NotInRepository;
        assert!(err.is_not_found());

        let err = RepositoryError::ViewNotFound {
            name: "main".to_string(),
        };
        assert!(!err.is_not_found());
    }

    #[test]
    fn test_user_fixable_detection() {
        let err = RepositoryError::UncommittedChanges;
        assert!(err.is_user_fixable());

        let err = RepositoryError::MergeConflict {
            description: "conflict in file.txt".to_string(),
        };
        assert!(err.is_user_fixable());

        let err = RepositoryError::Locked;
        assert!(!err.is_user_fixable());
    }

    #[test]
    fn test_error_display() {
        let err = RepositoryError::ViewNotFound {
            name: "feature".to_string(),
        };
        assert_eq!(err.to_string(), "View not found: feature");

        let err = RepositoryError::MissingDependency {
            change: "ABC123".to_string(),
            dependency: "DEF456".to_string(),
        };
        assert!(err.to_string().contains("ABC123"));
        assert!(err.to_string().contains("DEF456"));
    }

    #[test]
    fn test_tag_error_detection() {
        let err = RepositoryError::TagNotFound {
            name: "v1.0.0".to_string(),
        };
        assert!(err.is_tag_error());

        let err = RepositoryError::TagAlreadyExists {
            name: "v1.0.0".to_string(),
        };
        assert!(err.is_tag_error());
        assert!(err.is_user_fixable());

        let err = RepositoryError::InvalidTagName {
            name: "bad/name".to_string(),
            reason: "contains slash".to_string(),
        };
        assert!(err.is_tag_error());
        assert!(err.is_user_fixable());
    }

    #[test]
    fn test_apply_error_detection() {
        let err = RepositoryError::Apply("conflict".to_string());
        assert!(err.is_apply_error());

        let err = RepositoryError::ChangeNotFound {
            hash: "ABC123".to_string(),
        };
        assert!(err.is_apply_error());

        let err = RepositoryError::ChangeAlreadyApplied {
            hash: "ABC123".to_string(),
        };
        assert!(err.is_apply_error());
    }

    #[test]
    fn test_archive_error_display() {
        let err = RepositoryError::Archive("too large".to_string());
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn test_unrecord_error_display() {
        let err = RepositoryError::Unrecord("has dependents".to_string());
        assert!(err.to_string().contains("has dependents"));
    }
}
