//! CLI-specific error types for the Atomic command-line interface.
//!
//! This module provides error types that wrap underlying library errors
//! with user-friendly messages and helpful suggestions. These errors are
//! designed to guide users towards resolving issues rather than just
//! reporting what went wrong.
//!
//! # Design Philosophy
//!
//! CLI errors should:
//! 1. Be clear and actionable
//! 2. Provide context about what operation failed
//! 3. Suggest how to resolve the issue when possible
//! 4. Chain underlying errors for debugging
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic::error::{CliError, CliResult};
//!
//! fn my_command() -> CliResult<()> {
//!     // Operations that might fail...
//!     Err(CliError::RepositoryNotFound {
//!         searched_path: "/home/user/project".into(),
//!     })
//! }
//! ```

use std::path::PathBuf;

use thiserror::Error;

/// Result type for CLI operations.
///
/// This is the standard result type used throughout the CLI codebase.
/// It uses [`CliError`] as the error type, which provides user-friendly
/// error messages with suggestions.
pub type CliResult<T> = Result<T, CliError>;

/// CLI-specific errors with user-friendly messages.
///
/// Each variant includes contextual information and, where applicable,
/// suggestions for how to resolve the issue. The error messages are
/// designed to guide users rather than just report failures.
///
/// # Categories
///
/// Errors are organized into several categories:
///
/// - **Repository errors**: Issues finding or accessing repositories
/// - **Stack errors**: Problems with stack operations
/// - **File errors**: Issues with file tracking or access
/// - **Change errors**: Problems recording or applying changes
/// - **Remote errors**: Issues with remote operations
/// - **Internal errors**: Unexpected failures (bugs)
#[derive(Debug, Error)]
pub enum CliError {
    // Repository Errors
    /// No Atomic repository found in the current directory or any parent.
    ///
    /// This error occurs when a command that requires a repository is run
    /// outside of any Atomic repository.
    #[error("Not in an Atomic repository (or any parent up to mount point)")]
    RepositoryNotFound {
        /// The path where the search started
        searched_path: PathBuf,
    },

    /// A repository already exists at the target location.
    ///
    /// This error occurs when trying to initialize a new repository
    /// in a directory that already contains a `.atomic` folder.
    #[error("Repository already exists at '{path}'")]
    RepositoryExists {
        /// The path where the repository already exists
        path: PathBuf,
    },

    /// The specified path doesn't exist or is inaccessible.
    ///
    /// This can occur when trying to initialize a repository in a
    /// non-existent directory, or when accessing files.
    #[error("Path does not exist or is not accessible: '{path}'")]
    InvalidPath {
        /// The path that couldn't be accessed
        path: PathBuf,
        /// The underlying IO error, if any
        #[source]
        source: Option<std::io::Error>,
    },

    /// The repository structure is corrupted or invalid.
    ///
    /// This indicates that the `.atomic` directory exists but is
    /// missing required components or has been corrupted.
    #[error("Invalid repository structure: {reason}")]
    InvalidRepository {
        /// Description of what's wrong with the repository
        reason: String,
    },

    // Stack Errors
    /// The specified stack doesn't exist.
    ///
    /// This occurs when trying to switch to, delete, or otherwise
    /// operate on a stack that hasn't been created.
    #[error("Stack '{name}' not found")]
    StackNotFound {
        /// The name of the stack that wasn't found
        name: String,
    },

    /// A stack with the given name already exists.
    ///
    /// This occurs when trying to create a new stack with a name
    /// that's already in use.
    #[error("Stack '{name}' already exists")]
    StackAlreadyExists {
        /// The name of the existing stack
        name: String,
    },

    /// Cannot delete the currently active stack.
    ///
    /// Users must switch to a different stack before deleting
    /// the current one.
    #[error("Cannot delete the current stack '{name}'")]
    CannotDeleteCurrentStack {
        /// The name of the current stack
        name: String,
    },

    // File Errors
    /// The specified file doesn't exist.
    ///
    /// This occurs when trying to add, diff, or otherwise operate
    /// on a file that doesn't exist in the working copy.
    #[error("File not found: '{path}'")]
    FileNotFound {
        /// The path to the missing file
        path: PathBuf,
    },

    /// The specified file is not tracked by the repository.
    ///
    /// This occurs when trying to operate on a file that exists
    /// but hasn't been added to the repository.
    #[error("File not tracked: '{path}'")]
    FileNotTracked {
        /// The path to the untracked file
        path: PathBuf,
    },

    /// The file is already being tracked.
    ///
    /// This occurs when trying to add a file that's already tracked.
    #[error("File already tracked: '{path}'")]
    FileAlreadyTracked {
        /// The path to the already-tracked file
        path: PathBuf,
    },

    /// The path is outside the repository root.
    ///
    /// For security and consistency, Atomic only operates on files
    /// within the repository boundaries.
    #[error("Path is outside repository: '{path}'")]
    PathOutsideRepository {
        /// The path that's outside the repository
        path: PathBuf,
    },

    /// The path is ignored by .atomicignore rules.
    ///
    /// This occurs when explicitly trying to add a path that matches
    /// an ignore pattern.
    #[error("Path is ignored: '{path}'")]
    PathIgnored {
        /// The ignored path
        path: PathBuf,
    },

    // Change Errors
    /// There are no changes to record.
    ///
    /// This occurs when running `atomic record` with no modified files.
    #[error("Nothing to record - working tree is clean")]
    NothingToRecord,

    /// A change with the given hash wasn't found.
    ///
    /// This can occur when trying to view, apply, or unrecord a
    /// change that doesn't exist in the repository.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// The hash (or partial hash) of the missing change
        hash: String,
    },

    /// The change hash is ambiguous (matches multiple changes).
    ///
    /// This occurs when a partial hash matches more than one change.
    /// Users should provide more characters to disambiguate.
    #[error("Ambiguous change hash '{hash}' - matches multiple changes")]
    AmbiguousHash {
        /// The ambiguous hash prefix
        hash: String,
    },

    /// The change cannot be applied due to missing dependencies.
    ///
    /// Changes in Atomic can depend on other changes. This error
    /// occurs when trying to apply a change whose dependencies
    /// haven't been applied yet.
    #[error("Missing dependency for change {change}: requires {dependency}")]
    MissingDependency {
        /// The change that has missing dependencies
        change: String,
        /// The missing dependency
        dependency: String,
    },

    /// A conflict occurred during the operation.
    ///
    /// This can occur when applying changes that conflict with
    /// existing content.
    #[error("Conflict: {description}")]
    Conflict {
        /// Description of the conflict
        description: String,
    },

    // Identity Errors
    /// The specified identity doesn't exist.
    ///
    /// This occurs when trying to use, show, or delete an identity
    /// that hasn't been created.
    #[error("Identity not found: '{0}'")]
    IdentityNotFound(String),

    /// An identity with the given name already exists.
    ///
    /// This occurs when trying to create a new identity with a name
    /// that's already in use.
    #[error("Identity already exists: '{0}'")]
    IdentityAlreadyExists(String),

    // Remote Errors
    /// Failed to connect to or communicate with the remote.
    ///
    /// This can occur due to network issues, authentication failures,
    /// or the remote being unavailable.
    #[error("Remote operation failed: {message}")]
    RemoteError {
        /// Description of what went wrong
        message: String,
        /// The remote URL, if known
        url: Option<String>,
    },

    /// The specified remote doesn't exist in the configuration.
    #[error("Remote '{name}' not found")]
    RemoteNotFound {
        /// The name of the missing remote
        name: String,
    },

    /// Authentication failed for the remote operation.
    #[error("Authentication failed for remote '{remote}'")]
    AuthenticationFailed {
        /// The remote that rejected authentication
        remote: String,
    },

    // Git Interop Errors
    /// A Git operation failed.
    ///
    /// This occurs during git import or other git interoperability operations.
    #[error("Git error: {message}")]
    GitError {
        /// Description of what went wrong
        message: String,
    },

    // User Input Errors
    /// The user cancelled the operation.
    #[error("Operation cancelled by user")]
    Cancelled,

    /// Invalid argument provided to a command.
    #[error("Invalid argument: {message}")]
    InvalidArgument {
        /// Description of what's wrong with the argument
        message: String,
    },

    // Internal Errors
    /// An unexpected internal error occurred.
    ///
    /// This indicates a bug in Atomic. If you encounter this error,
    /// please report it at: https://github.com/atomic-vcs/atomic/issues
    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// An IO error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A configuration error occurred.
    #[error("Configuration error: {0}")]
    Config(#[from] atomic_config::ConfigError),

    /// A repository library error occurred.
    #[error("{0}")]
    Repository(#[from] atomic_repository::RepositoryError),
}

impl CliError {
    // Constructor Helpers

    /// Create a `RepositoryExists` error for the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where a repository already exists
    pub fn repository_exists<P: Into<PathBuf>>(path: P) -> Self {
        Self::RepositoryExists { path: path.into() }
    }

    /// Create an `InvalidPath` error for the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that couldn't be accessed
    /// * `source` - Optional underlying IO error
    pub fn invalid_path<P: Into<PathBuf>>(path: P, source: Option<std::io::Error>) -> Self {
        Self::InvalidPath {
            path: path.into(),
            source,
        }
    }

    /// Create a `RepositoryNotFound` error for the given path.
    pub fn repository_not_found<P: Into<PathBuf>>(path: P) -> Self {
        Self::RepositoryNotFound {
            searched_path: path.into(),
        }
    }

    /// Create a `StackNotFound` error for the given name.
    pub fn stack_not_found(name: impl Into<String>) -> Self {
        Self::StackNotFound { name: name.into() }
    }

    /// Create a `FileNotFound` error for the given path.
    pub fn file_not_found<P: Into<PathBuf>>(path: P) -> Self {
        Self::FileNotFound { path: path.into() }
    }

    /// Create a `ChangeNotFound` error for the given hash.
    pub fn change_not_found(hash: impl Into<String>) -> Self {
        Self::ChangeNotFound { hash: hash.into() }
    }

    /// Create a `RemoteError` with an optional URL.
    pub fn remote_error(message: impl Into<String>, url: Option<String>) -> Self {
        Self::RemoteError {
            message: message.into(),
            url,
        }
    }

    // Error Classification

    /// Check if this error indicates something was not found.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::RepositoryNotFound { .. }
                | Self::StackNotFound { .. }
                | Self::FileNotFound { .. }
                | Self::ChangeNotFound { .. }
                | Self::RemoteNotFound { .. }
        )
    }

    /// Check if this error is related to stack operations.
    pub fn is_stack_error(&self) -> bool {
        matches!(
            self,
            Self::StackNotFound { .. }
                | Self::StackAlreadyExists { .. }
                | Self::CannotDeleteCurrentStack { .. }
        )
    }

    /// Check if this error is related to file operations.
    pub fn is_file_error(&self) -> bool {
        matches!(
            self,
            Self::FileNotFound { .. }
                | Self::FileNotTracked { .. }
                | Self::FileAlreadyTracked { .. }
                | Self::PathOutsideRepository { .. }
                | Self::PathIgnored { .. }
        )
    }

    /// Check if this error is related to remote operations.
    pub fn is_remote_error(&self) -> bool {
        matches!(
            self,
            Self::RemoteError { .. }
                | Self::RemoteNotFound { .. }
                | Self::AuthenticationFailed { .. }
        )
    }

    /// Check if this error is fixable by the user (not a bug).
    pub fn is_user_fixable(&self) -> bool {
        matches!(
            self,
            Self::RepositoryNotFound { .. }
                | Self::RepositoryExists { .. }
                | Self::StackNotFound { .. }
                | Self::StackAlreadyExists { .. }
                | Self::CannotDeleteCurrentStack { .. }
                | Self::FileNotFound { .. }
                | Self::FileNotTracked { .. }
                | Self::FileAlreadyTracked { .. }
                | Self::PathOutsideRepository { .. }
                | Self::PathIgnored { .. }
                | Self::NothingToRecord
                | Self::ChangeNotFound { .. }
                | Self::AmbiguousHash { .. }
                | Self::MissingDependency { .. }
                | Self::Conflict { .. }
                | Self::IdentityNotFound(_)
                | Self::IdentityAlreadyExists(_)
                | Self::RemoteNotFound { .. }
                | Self::AuthenticationFailed { .. }
                | Self::InvalidArgument { .. }
                | Self::InvalidRepository { .. }
                | Self::InvalidPath { .. }
        )
    }

    /// Check if this is an internal error (likely a bug).
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Internal(_))
    }

    // User Guidance

    /// Get a suggestion for how to resolve this error.
    ///
    /// Returns a human-readable suggestion that can help users
    /// understand what action to take to resolve the error.
    ///
    /// # Returns
    ///
    /// An optional string with a suggestion, or `None` if no
    /// specific suggestion is available.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let err = CliError::repository_not_found("/home/user/project");
    /// if let Some(suggestion) = err.suggestion() {
    ///     eprintln!("Hint: {}", suggestion);
    /// }
    /// ```
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::RepositoryNotFound { .. } => {
                Some("Run 'atomic init' to create a new repository, or change to a directory inside an existing repository.")
            }
            Self::RepositoryExists { .. } => {
                Some("The directory is already an Atomic repository. Use 'atomic status' to see its state.")
            }
            Self::StackNotFound { .. } => {
                Some("Run 'atomic stack list' to see available stacks, or 'atomic stack new <name>' to create a new one.")
            }
            Self::StackAlreadyExists { .. } => {
                Some("Choose a different name, or use 'atomic stack switch <name>' to switch to the existing stack.")
            }
            Self::CannotDeleteCurrentStack { .. } => {
                Some("Switch to a different stack first with 'atomic stack switch <name>', then delete this one.")
            }
            Self::FileNotFound { .. } => {
                Some("Check that the file path is correct. Use 'ls' or your file manager to verify the file exists.")
            }
            Self::FileNotTracked { .. } => {
                Some("Run 'atomic add <file>' to start tracking this file.")
            }
            Self::FileAlreadyTracked { .. } => {
                Some("This file is already tracked. Use 'atomic status' to see its current state.")
            }
            Self::PathOutsideRepository { .. } => {
                Some("Atomic can only track files within the repository. Move the file inside the repository or initialize a new repository in its parent directory.")
            }
            Self::NothingToRecord => {
                Some("Make some changes to tracked files, or use 'atomic add' to track new files.")
            }
            Self::ChangeNotFound { .. } => {
                Some("Run 'atomic log' to see available changes and their hashes.")
            }
            Self::AmbiguousHash { .. } => {
                Some("Provide more characters of the hash to uniquely identify the change.")
            }
            Self::MissingDependency { .. } => {
                Some("Apply the required dependency first, or use '--force' to skip dependency checking (not recommended).")
            }
            Self::RemoteNotFound { .. } => {
                Some("Check your repository configuration, or add a remote with 'atomic remote add <name> <url>'.")
            }
            Self::AuthenticationFailed { .. } => {
                Some("Check your credentials. You may need to set up SSH keys or update your access token.")
            }
            Self::GitError { .. } => {
                Some("A Git operation failed. Ensure you are in a valid Git repository, the referenced branch or commit exists, and you have the necessary permissions. Run 'git status' and 'git branch' to investigate.")
            }
            Self::IdentityNotFound(_) => {
                Some("Run 'atomic identity list' to see available identities, or 'atomic identity new <name>' to create one.")
            }
            Self::IdentityAlreadyExists(_) => {
                Some("Choose a different name, or use 'atomic identity show <name>' to view the existing identity.")
            }
            Self::Cancelled => None,
            Self::InvalidArgument { .. } => {
                Some("Run 'atomic <command> --help' for usage information.")
            }
            Self::Internal(_) => {
                Some("This appears to be a bug. Please report it at: https://github.com/atomic-vcs/atomic/issues")
            }
            _ => None,
        }
    }

    /// Get the exit code that should be used for this error.
    ///
    /// Different errors have different severity levels, reflected
    /// in their exit codes:
    ///
    /// - 1: General errors (user-fixable issues)
    /// - 2: Command-line usage errors
    /// - 3: Repository/data errors
    /// - 4: Remote/network errors
    /// - 128: Internal errors (bugs)
    ///
    /// # Returns
    ///
    /// The exit code to use when terminating due to this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            // User-fixable, general errors
            Self::NothingToRecord
            | Self::Cancelled
            | Self::FileAlreadyTracked { .. }
            | Self::StackAlreadyExists { .. } => 1,

            // Command-line usage errors
            Self::InvalidArgument { .. } => 2,

            // Repository/data errors
            Self::RepositoryNotFound { .. }
            | Self::RepositoryExists { .. }
            | Self::InvalidPath { .. }
            | Self::InvalidRepository { .. }
            | Self::StackNotFound { .. }
            | Self::CannotDeleteCurrentStack { .. }
            | Self::FileNotFound { .. }
            | Self::FileNotTracked { .. }
            | Self::PathOutsideRepository { .. }
            | Self::PathIgnored { .. }
            | Self::ChangeNotFound { .. }
            | Self::AmbiguousHash { .. }
            | Self::MissingDependency { .. }
            | Self::Conflict { .. }
            | Self::IdentityNotFound(_)
            | Self::IdentityAlreadyExists(_) => 3,

            // Remote/network errors
            Self::RemoteError { .. }
            | Self::RemoteNotFound { .. }
            | Self::AuthenticationFailed { .. }
            | Self::GitError { .. } => 4,

            // IO and config errors
            Self::Io(_) | Self::Config(_) => 3,

            // Repository library errors
            Self::Repository(_) => 3,

            // Internal errors (bugs)
            Self::Internal(_) => 128,
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Error Display Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_repository_not_found_display() {
        let err = CliError::repository_not_found("/home/user/project");
        let msg = err.to_string();
        assert!(msg.contains("Not in an Atomic repository"));
    }

    #[test]
    fn test_repository_exists_display() {
        let err = CliError::repository_exists("/home/user/project");
        let msg = err.to_string();
        assert!(msg.contains("Repository already exists"));
        assert!(msg.contains("/home/user/project"));
    }

    #[test]
    fn test_stack_not_found_display() {
        let err = CliError::stack_not_found("feature");
        let msg = err.to_string();
        assert!(msg.contains("Stack 'feature' not found"));
    }

    #[test]
    fn test_file_not_found_display() {
        let err = CliError::file_not_found("src/main.rs");
        let msg = err.to_string();
        assert!(msg.contains("File not found"));
        assert!(msg.contains("src/main.rs"));
    }

    #[test]
    fn test_change_not_found_display() {
        let err = CliError::change_not_found("ABC123");
        let msg = err.to_string();
        assert!(msg.contains("Change not found"));
        assert!(msg.contains("ABC123"));
    }

    #[test]
    fn test_remote_error_display() {
        let err = CliError::remote_error("connection refused", Some("https://example.com".into()));
        let msg = err.to_string();
        assert!(msg.contains("Remote operation failed"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn test_nothing_to_record_display() {
        let err = CliError::NothingToRecord;
        let msg = err.to_string();
        assert!(msg.contains("Nothing to record"));
    }

    #[test]
    fn test_cancelled_display() {
        let err = CliError::Cancelled;
        let msg = err.to_string();
        assert!(msg.contains("cancelled"));
    }

    #[test]
    fn test_missing_dependency_display() {
        let err = CliError::MissingDependency {
            change: "ABC".into(),
            dependency: "XYZ".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Missing dependency"));
        assert!(msg.contains("ABC"));
        assert!(msg.contains("XYZ"));
    }

    // -------------------------------------------------------------------------
    // Error Classification Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_not_found() {
        let err = CliError::repository_not_found("/path");
        assert!(err.is_not_found());

        let err = CliError::NothingToRecord;
        assert!(!err.is_not_found());
    }

    #[test]
    fn test_is_stack_error() {
        assert!(CliError::stack_not_found("main").is_stack_error());
        assert!(CliError::StackAlreadyExists { name: "dev".into() }.is_stack_error());
        assert!(CliError::CannotDeleteCurrentStack { name: "dev".into() }.is_stack_error());

        assert!(!CliError::NothingToRecord.is_stack_error());
    }

    #[test]
    fn test_is_file_error() {
        assert!(CliError::file_not_found("test.txt").is_file_error());
        assert!(CliError::FileNotTracked {
            path: "test.txt".into()
        }
        .is_file_error());
        assert!(CliError::FileAlreadyTracked {
            path: "test.txt".into()
        }
        .is_file_error());
        assert!(CliError::PathOutsideRepository {
            path: "../outside".into()
        }
        .is_file_error());

        assert!(!CliError::NothingToRecord.is_file_error());
    }

    #[test]
    fn test_is_remote_error() {
        assert!(CliError::remote_error("failed", None).is_remote_error());
        assert!(CliError::RemoteNotFound {
            name: "origin".into()
        }
        .is_remote_error());
        assert!(CliError::AuthenticationFailed {
            remote: "origin".into()
        }
        .is_remote_error());

        assert!(!CliError::NothingToRecord.is_remote_error());
    }

    #[test]
    fn test_is_user_fixable() {
        assert!(CliError::repository_not_found("/path").is_user_fixable());
        assert!(CliError::NothingToRecord.is_user_fixable());
        assert!(CliError::FileNotTracked {
            path: "test.txt".into()
        }
        .is_user_fixable());

        assert!(!CliError::Cancelled.is_user_fixable());
        assert!(
            !CliError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test")).is_user_fixable()
        );
    }

    #[test]
    fn test_is_internal() {
        let err = CliError::Internal(anyhow::anyhow!("something broke"));
        assert!(err.is_internal());

        assert!(!CliError::NothingToRecord.is_internal());
    }

    // -------------------------------------------------------------------------
    // Suggestion Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_repository_not_found_suggestion() {
        let err = CliError::repository_not_found("/path");
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("atomic init"));
    }

    #[test]
    fn test_stack_not_found_suggestion() {
        let err = CliError::stack_not_found("feature");
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("atomic stack list"));
    }

    #[test]
    fn test_file_not_tracked_suggestion() {
        let err = CliError::FileNotTracked {
            path: "test.txt".into(),
        };
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("atomic add"));
    }

    #[test]
    fn test_nothing_to_record_suggestion() {
        let err = CliError::NothingToRecord;
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("atomic add"));
    }

    #[test]
    fn test_internal_error_suggestion() {
        let err = CliError::Internal(anyhow::anyhow!("bug"));
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("bug"));
    }

    #[test]
    fn test_cancelled_no_suggestion() {
        let err = CliError::Cancelled;
        assert!(err.suggestion().is_none());
    }

    // -------------------------------------------------------------------------
    // Exit Code Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_exit_code_user_fixable() {
        assert_eq!(CliError::NothingToRecord.exit_code(), 1);
        assert_eq!(CliError::Cancelled.exit_code(), 1);
    }

    #[test]
    fn test_exit_code_usage_error() {
        assert_eq!(
            CliError::InvalidArgument {
                message: "bad arg".into()
            }
            .exit_code(),
            2
        );
    }

    #[test]
    fn test_exit_code_repository_error() {
        assert_eq!(CliError::repository_not_found("/path").exit_code(), 3);
        assert_eq!(CliError::stack_not_found("main").exit_code(), 3);
        assert_eq!(CliError::file_not_found("test.txt").exit_code(), 3);
    }

    #[test]
    fn test_exit_code_remote_error() {
        assert_eq!(CliError::remote_error("failed", None).exit_code(), 4);
        assert_eq!(
            CliError::RemoteNotFound {
                name: "origin".into()
            }
            .exit_code(),
            4
        );
    }

    #[test]
    fn test_exit_code_internal() {
        let err = CliError::Internal(anyhow::anyhow!("bug"));
        assert_eq!(err.exit_code(), 128);
    }

    // -------------------------------------------------------------------------
    // Constructor Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_constructor_repository_not_found() {
        let err = CliError::repository_not_found("/home/user");
        match err {
            CliError::RepositoryNotFound { searched_path } => {
                assert_eq!(searched_path, PathBuf::from("/home/user"));
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_repository_exists() {
        let err = CliError::repository_exists("/home/user");
        match err {
            CliError::RepositoryExists { path } => {
                assert_eq!(path, PathBuf::from("/home/user"));
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_invalid_path_with_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err = CliError::invalid_path("/nonexistent", Some(io_err));
        match err {
            CliError::InvalidPath { path, source } => {
                assert_eq!(path, PathBuf::from("/nonexistent"));
                assert!(source.is_some());
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_invalid_path_without_source() {
        let err = CliError::invalid_path("/nonexistent", None);
        match err {
            CliError::InvalidPath { path, source } => {
                assert_eq!(path, PathBuf::from("/nonexistent"));
                assert!(source.is_none());
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_stack_not_found() {
        let err = CliError::stack_not_found("feature-branch");
        match err {
            CliError::StackNotFound { name } => {
                assert_eq!(name, "feature-branch");
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_file_not_found() {
        let err = CliError::file_not_found("src/lib.rs");
        match err {
            CliError::FileNotFound { path } => {
                assert_eq!(path, PathBuf::from("src/lib.rs"));
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_change_not_found() {
        let err = CliError::change_not_found("DEADBEEF");
        match err {
            CliError::ChangeNotFound { hash } => {
                assert_eq!(hash, "DEADBEEF");
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_remote_error_with_url() {
        let err = CliError::remote_error("timeout", Some("https://git.example.com".into()));
        match err {
            CliError::RemoteError { message, url } => {
                assert_eq!(message, "timeout");
                assert_eq!(url, Some("https://git.example.com".to_string()));
            }
            _ => panic!("Wrong error variant"),
        }
    }

    #[test]
    fn test_constructor_remote_error_without_url() {
        let err = CliError::remote_error("network error", None);
        match err {
            CliError::RemoteError { message, url } => {
                assert_eq!(message, "network error");
                assert!(url.is_none());
            }
            _ => panic!("Wrong error variant"),
        }
    }

    // -------------------------------------------------------------------------
    // From Trait Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let cli_err: CliError = io_err.into();
        match cli_err {
            CliError::Io(_) => {}
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let cli_err: CliError = anyhow_err.into();
        match cli_err {
            CliError::Internal(_) => {}
            _ => panic!("Expected Internal variant"),
        }
    }

    // -------------------------------------------------------------------------
    // Edge Case Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_empty_stack_name() {
        let err = CliError::stack_not_found("");
        assert!(err.to_string().contains("Stack '' not found"));
    }

    #[test]
    fn test_path_with_special_characters() {
        let err = CliError::file_not_found("path/with spaces/and'quotes.txt");
        assert!(err.to_string().contains("path/with spaces/and'quotes.txt"));
    }

    #[test]
    fn test_unicode_in_error_messages() {
        let err = CliError::stack_not_found("功能分支");
        assert!(err.to_string().contains("功能分支"));
    }

    #[test]
    fn test_all_error_variants_have_display() {
        // Ensure all error variants can be displayed without panicking
        let errors: Vec<CliError> = vec![
            CliError::repository_not_found("/path"),
            CliError::repository_exists("/path"),
            CliError::invalid_path("/path", None),
            CliError::InvalidRepository {
                reason: "corrupted".into(),
            },
            CliError::stack_not_found("main"),
            CliError::StackAlreadyExists { name: "dev".into() },
            CliError::CannotDeleteCurrentStack { name: "dev".into() },
            CliError::file_not_found("test.txt"),
            CliError::FileNotTracked {
                path: "test.txt".into(),
            },
            CliError::FileAlreadyTracked {
                path: "test.txt".into(),
            },
            CliError::PathOutsideRepository {
                path: "../outside".into(),
            },
            CliError::NothingToRecord,
            CliError::change_not_found("ABC"),
            CliError::AmbiguousHash { hash: "AB".into() },
            CliError::MissingDependency {
                change: "A".into(),
                dependency: "B".into(),
            },
            CliError::Conflict {
                description: "conflict".into(),
            },
            CliError::remote_error("failed", None),
            CliError::RemoteNotFound {
                name: "origin".into(),
            },
            CliError::AuthenticationFailed {
                remote: "origin".into(),
            },
            CliError::Cancelled,
            CliError::InvalidArgument {
                message: "bad".into(),
            },
        ];

        for err in errors {
            // Just ensure to_string() doesn't panic
            let _ = err.to_string();
            // Also ensure suggestion() doesn't panic
            let _ = err.suggestion();
            // And exit_code() doesn't panic
            let _ = err.exit_code();
        }
    }
}
