//! Error types for repository operations

use std::path::PathBuf;
use thiserror::Error;

use crate::remote::RemoteError;

/// Result type for repository operations
pub type Result<T> = std::result::Result<T, RepositoryError>;

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

    /// Stack not found
    #[error("Stack not found: {name}")]
    StackNotFound { name: String },

    /// Stack already exists
    #[error("Stack already exists: {name}")]
    StackAlreadyExists { name: String },

    /// Cannot delete the current stack
    #[error("Cannot delete the current stack '{name}'")]
    CannotDeleteCurrentStack { name: String },

    /// Working copy has uncommitted changes
    #[error("Working copy has uncommitted changes")]
    UncommittedChanges,

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

    /// Change not found
    #[error("Change not found: {hash}")]
    ChangeNotFound { hash: String },

    /// Ambiguous hash prefix (multiple matches)
    #[error("Ambiguous hash prefix '{prefix}': matches {}", matches.join(", "))]
    AmbiguousHash {
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

    /// Lock error (another process holds the lock)
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
}

impl RepositoryError {
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
                | RepositoryError::MergeConflict { .. }
                | RepositoryError::FileNotTracked { .. }
                | RepositoryError::MissingDependency { .. }
                | RepositoryError::TagAlreadyExists { .. }
                | RepositoryError::InvalidTagName { .. }
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

        let err = RepositoryError::StackNotFound {
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
        let err = RepositoryError::StackNotFound {
            name: "feature".to_string(),
        };
        assert_eq!(err.to_string(), "Stack not found: feature");

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
