//! Command module for the Atomic CLI.
//!
//! This module provides the infrastructure for implementing CLI commands,
//! including the [`Command`] trait that all commands implement, and shared
//! utilities for common operations like finding repositories and formatting
//! output.
//!
//! # Architecture
//!
//! Each command is implemented in its own submodule and implements the
//! [`Command`] trait. The main entry point dispatches to the appropriate
//! command based on CLI arguments.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                           commands/                                  │
//! │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐   │
//! │  │  init   │  │ status  │  │   add   │  │ record  │  │   log   │   │
//! │  └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘   │
//! │       │            │            │            │            │        │
//! │       └────────────┴────────────┼────────────┴────────────┘        │
//! │                                 │                                   │
//! │                          ┌──────┴──────┐                           │
//! │                          │   Command   │                           │
//! │                          │    trait    │                           │
//! │                          └─────────────┘                           │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic::commands::{Command, find_repository_root};
//! use atomic::error::CliResult;
//!
//! struct MyCommand {
//!     // command-specific fields
//! }
//!
//! impl Command for MyCommand {
//!     fn run(&self) -> CliResult<()> {
//!         let repo_root = find_repository_root()?;
//!         // ... perform command logic ...
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # Shared Utilities
//!
//! The module provides several utility functions that commands commonly need:
//!
//! - [`find_repository_root`]: Find the `.atomic` directory by searching upward
//! - [`open_repository`]: Open a repository at a given path
//! - [`format_hash`]: Format a hash for display (with optional truncation)
//! - [`format_timestamp`]: Format a timestamp for human-readable display
//! - [`require_repository`]: Open repo or return user-friendly error

use std::path::{Path, PathBuf};

use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;
use chrono::{DateTime, Local, Utc};

use crate::error::{CliError, CliResult};

// Command Submodules

// Phase 2: Core Local Commands
pub mod add;
pub mod change;
pub mod diff;
pub mod doctor;
pub mod init;
pub mod insert;
pub mod log;
pub mod mv;
pub mod record;
pub mod remove;
pub mod restore;
pub mod revise;
pub mod split;
pub mod stash;
pub mod status;
pub mod tag;
pub mod unrecord;
pub mod update;
pub mod vault;
pub mod view;

// Phase 6: Agent Integration
pub mod agent;

// Phase 3: Remote Commands
pub mod auth;
pub mod clone;
pub mod pull;
pub mod push;
pub mod remote;

// Phase 4: Identity Management
pub mod identity;

// Git Interoperability
pub mod git;

// Knowledge graph query (top-level command)
pub mod query;

// Storage management commands (always available)
pub mod client;
pub mod project;
pub mod token;
pub mod workspace;

// Team collaboration commands (feature-gated)
#[cfg(feature = "teams")]
pub mod org;
#[cfg(feature = "teams")]
pub mod team;

// Re-export command structs for convenience
pub use add::Add;
pub use agent::Agent;
pub use change::ChangeCmd;
pub use clone::Clone;
pub use diff::Diff;
pub use doctor::Doctor;
pub use git::Git;
pub use identity::Identity;
pub use init::Init;
pub use insert::Insert;
pub use log::Log;
pub use mv::Move;
pub use project::ProjectCmd;
pub use pull::Pull;
pub use push::Push;
pub use query::Query;
pub use record::Record;
pub use remote::Remote;
pub use remove::Remove;
pub use restore::Restore;
pub use revise::Revise;
pub use split::Split;
pub use stash::Stash;
pub use status::Status;
pub use tag::Tag;
pub use unrecord::Unrecord;
pub use update::Update;
pub use vault::Vault;
pub use view::View;
pub use workspace::WorkspaceCmd;

#[cfg(feature = "teams")]
pub use org::OrgCmd;
#[cfg(feature = "teams")]
pub use team::TeamCmd;

// Command Trait

/// Trait that all CLI commands must implement.
///
/// This trait provides a uniform interface for executing commands,
/// enabling consistent error handling and testing across all commands.
///
/// # Implementing Commands
///
/// Each command should:
/// 1. Parse and validate its arguments in the struct fields
/// 2. Implement `run()` to perform the actual operation
/// 3. Return appropriate errors using [`CliError`]
///
/// # Example
///
/// ```rust,ignore
/// use atomic::commands::Command;
/// use atomic::error::CliResult;
///
/// #[derive(clap::Parser)]
/// pub struct MyCommand {
///     #[arg(short, long)]
///     verbose: bool,
/// }
///
/// impl Command for MyCommand {
///     fn run(&self) -> CliResult<()> {
///         if self.verbose {
///             println!("Running in verbose mode");
///         }
///         // ... perform command ...
///         Ok(())
///     }
/// }
/// ```
pub trait Command {
    /// Execute the command.
    ///
    /// This method performs the main logic of the command. It should:
    /// - Validate any preconditions
    /// - Perform the requested operation
    /// - Print appropriate output to the user
    /// - Return errors using [`CliError`] for consistent error handling
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] if the command fails for any reason.
    /// The error should be as specific as possible to help users
    /// understand what went wrong and how to fix it.
    fn run(&self) -> CliResult<()>;
}

// Repository Discovery

/// The name of the Atomic directory.
pub const DOT_DIR: &str = ".atomic";

/// Find the repository root by searching upward from the current directory.
///
/// This function starts at the current working directory and walks up
/// the directory tree looking for a `.atomic` directory. This allows
/// commands to work from any subdirectory within a repository.
///
/// # Returns
///
/// The path to the repository root (the directory containing `.atomic`).
///
/// # Errors
///
/// Returns [`CliError::RepositoryNotFound`] if no `.atomic` directory
/// is found before reaching the filesystem root.
///
/// # Example
///
/// ```rust,ignore
/// let repo_root = find_repository_root()?;
/// println!("Repository found at: {}", repo_root.display());
/// ```
pub fn find_repository_root() -> CliResult<PathBuf> {
    let cwd = std::env::current_dir().map_err(CliError::Io)?;
    find_repository_root_from(&cwd)
}

/// Find the repository root starting from a specific path.
///
/// Like [`find_repository_root`], but starts from the given path instead
/// of the current working directory.
///
/// # Arguments
///
/// * `start_path` - The path to start searching from
///
/// # Returns
///
/// The path to the repository root.
///
/// # Errors
///
/// Returns [`CliError::RepositoryNotFound`] if no `.atomic` directory is found.
///
/// # Example
///
/// ```rust,ignore
/// let repo_root = find_repository_root_from("/home/user/project/src")?;
/// ```
pub fn find_repository_root_from(start_path: &Path) -> CliResult<PathBuf> {
    let mut current = start_path.to_path_buf();

    // Canonicalize if possible to handle symlinks correctly
    if let Ok(canonical) = current.canonicalize() {
        current = canonical;
    }

    loop {
        let dot_dir = current.join(DOT_DIR);
        if dot_dir.is_dir() {
            return Ok(current);
        }

        // Move to parent directory
        if !current.pop() {
            // Reached the root without finding a repository
            return Err(CliError::RepositoryNotFound {
                searched_path: start_path.to_path_buf(),
            });
        }
    }
}

/// Open a repository, optionally at a specific path.
///
/// If `path` is provided, opens the repository at that path.
/// If `path` is `None`, searches upward from the current directory.
///
/// # Arguments
///
/// * `path` - Optional path to the repository
///
/// # Returns
///
/// An open [`Repository`] handle.
///
/// # Errors
///
/// Returns an error if no repository is found or if the repository
/// cannot be opened.
///
/// # Example
///
/// ```rust,ignore
/// // Open from current directory
/// let repo = open_repository(None)?;
///
/// // Open from specific path
/// let repo = open_repository(Some(Path::new("/home/user/project")))?;
/// ```
pub fn open_repository(path: Option<&Path>) -> CliResult<Repository> {
    let repo_path = match path {
        Some(p) => {
            // If a path is provided, check if it's a repository or search from there
            let dot_dir = p.join(DOT_DIR);
            if dot_dir.is_dir() {
                p.to_path_buf()
            } else {
                find_repository_root_from(p)?
            }
        }
        None => find_repository_root()?,
    };

    Repository::open(&repo_path).map_err(CliError::from)
}

/// Open a repository or return a user-friendly error.
///
/// This is a convenience wrapper around [`open_repository`] that provides
/// more context in error messages, making it clearer what the user should do.
///
/// # Arguments
///
/// * `path` - Optional path to the repository
///
/// # Returns
///
/// An open [`Repository`] handle.
///
/// # Errors
///
/// Returns a [`CliError`] with helpful suggestions if the repository
/// cannot be found or opened.
pub fn require_repository(path: Option<&Path>) -> CliResult<Repository> {
    open_repository(path)
}

// Formatting Utilities

/// Default number of characters to show for truncated hashes.
pub const DEFAULT_HASH_LENGTH: usize = 12;

/// Format a hash for display.
///
/// By default, hashes are truncated to [`DEFAULT_HASH_LENGTH`] characters
/// for readability. Use `full = true` to show the complete hash.
///
/// # Arguments
///
/// * `hash` - The hash to format
/// * `full` - Whether to show the full hash or truncate it
///
/// # Returns
///
/// A formatted string representation of the hash.
///
/// # Example
///
/// ```rust,ignore
/// let hash = Hash::from_base32(b"ABCDEFGHIJKLMNOP").unwrap();
///
/// // Truncated (default)
/// println!("{}", format_hash(&hash, false)); // "ABCDEFGHIJKL"
///
/// // Full
/// println!("{}", format_hash(&hash, true));  // "ABCDEFGHIJKLMNOP"
/// ```
pub fn format_hash(hash: &Hash, full: bool) -> String {
    let base32 = hash.to_base32();
    if full || base32.len() <= DEFAULT_HASH_LENGTH {
        base32
    } else {
        base32[..DEFAULT_HASH_LENGTH].to_string()
    }
}

/// Format a hash for display with custom length.
///
/// Like [`format_hash`], but allows specifying the truncation length.
///
/// # Arguments
///
/// * `hash` - The hash to format
/// * `length` - Maximum characters to show (0 means no truncation)
///
/// # Returns
///
/// A formatted string representation of the hash.
pub fn format_hash_with_length(hash: &Hash, length: usize) -> String {
    let base32 = hash.to_base32();
    if length == 0 || base32.len() <= length {
        base32
    } else {
        base32[..length].to_string()
    }
}

/// Format a timestamp for display.
///
/// Converts a UTC timestamp to local time and formats it in a
/// human-readable format.
///
/// # Arguments
///
/// * `timestamp` - The UTC timestamp to format
///
/// # Returns
///
/// A human-readable string representation of the timestamp.
///
/// # Example
///
/// ```rust,ignore
/// let ts = Utc::now();
/// println!("Created: {}", format_timestamp(&ts));
/// // Output: "Created: 2024-01-15 10:30:45"
/// ```
pub fn format_timestamp(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Format a timestamp in a relative format (e.g., "2 hours ago").
///
/// This provides a more conversational representation of time that's
/// easier to understand at a glance.
///
/// # Arguments
///
/// * `timestamp` - The UTC timestamp to format
///
/// # Returns
///
/// A relative time string.
///
/// # Example
///
/// ```rust,ignore
/// let ts = Utc::now() - chrono::Duration::hours(2);
/// println!("{}", format_timestamp_relative(&ts));
/// // Output: "2 hours ago"
/// ```
pub fn format_timestamp_relative(timestamp: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*timestamp);

    if duration.num_seconds() < 60 {
        "just now".to_string()
    } else if duration.num_minutes() < 60 {
        let mins = duration.num_minutes();
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{} minutes ago", mins)
        }
    } else if duration.num_hours() < 24 {
        let hours = duration.num_hours();
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{} hours ago", hours)
        }
    } else if duration.num_days() < 7 {
        let days = duration.num_days();
        if days == 1 {
            "yesterday".to_string()
        } else {
            format!("{} days ago", days)
        }
    } else if duration.num_weeks() < 4 {
        let weeks = duration.num_weeks();
        if weeks == 1 {
            "1 week ago".to_string()
        } else {
            format!("{} weeks ago", weeks)
        }
    } else {
        // For older dates, use absolute format
        format_timestamp(timestamp)
    }
}

/// Format a byte size in human-readable form.
///
/// Automatically selects the appropriate unit (bytes, KiB, MiB, GiB).
///
/// # Arguments
///
/// * `bytes` - The size in bytes
///
/// # Returns
///
/// A formatted string like "1.5 MiB" or "42 bytes".
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_size(1024), "1.0 KiB");
/// assert_eq!(format_size(42), "42 bytes");
/// ```
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a count with singular/plural label.
///
/// # Arguments
///
/// * `count` - The count
/// * `singular` - The singular form of the noun
/// * `plural` - The plural form of the noun
///
/// # Returns
///
/// A formatted string like "1 file" or "3 files".
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_count(1, "file", "files"), "1 file");
/// assert_eq!(format_count(3, "file", "files"), "3 files");
/// ```
pub fn format_count(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

/// Format a count with auto-pluralization (appends 's').
///
/// Convenience wrapper around [`format_count`] for regular English nouns
/// where the plural is just the singular + 's'.
///
/// # Arguments
///
/// * `count` - The count
/// * `singular` - The singular form of the noun (plural = singular + "s")
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_count_auto(1, "change"), "1 change");
/// assert_eq!(format_count_auto(5, "change"), "5 changes");
/// ```
pub fn format_count_auto(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    // -------------------------------------------------------------------------
    // Repository Discovery Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_find_repository_root_not_found() {
        let temp = TempDir::new().unwrap();

        // On some CI environments (especially Windows), a .atomic directory
        // may exist somewhere above the temp dir in the filesystem hierarchy.
        // Verify the precondition before asserting, and skip gracefully if not met.
        let has_atomic_ancestor = {
            let mut check = temp.path().to_path_buf();
            let mut found = false;
            loop {
                if check.join(DOT_DIR).is_dir() {
                    found = true;
                    break;
                }
                if !check.pop() {
                    break;
                }
            }
            found
        };
        if has_atomic_ancestor {
            // A real .atomic dir exists above the temp dir; this test cannot
            // reliably assert "not found" in this environment — skip it.
            return;
        }

        let result = find_repository_root_from(temp.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CliError::RepositoryNotFound { .. }
        ));
    }

    #[test]
    fn test_find_repository_root_in_current() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join(DOT_DIR)).unwrap();

        let result = find_repository_root_from(temp.path());
        assert!(result.is_ok());
        // Compare canonical paths to handle macOS /private/var vs /var symlinks
        let expected = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let actual = result.unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_find_repository_root_in_parent() {
        let temp = TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join(DOT_DIR)).unwrap();

        let subdir = temp.path().join("subdir").join("deep");
        std::fs::create_dir_all(&subdir).unwrap();

        let result = find_repository_root_from(&subdir);
        assert!(result.is_ok());
        // Compare canonical paths to handle macOS /private/var vs /var symlinks
        let expected = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf());
        let actual = result.unwrap();
        assert_eq!(actual, expected);
    }

    // -------------------------------------------------------------------------
    // Hash Formatting Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_format_hash_truncated() {
        // Create a test hash (32 bytes = 64 base32 chars when encoded)
        let bytes = [0u8; 32];
        let hash = Hash::from_bytes(bytes);

        let formatted = format_hash(&hash, false);
        assert!(formatted.len() <= DEFAULT_HASH_LENGTH);
        assert!(!formatted.ends_with("..."));
    }

    #[test]
    fn test_format_hash_full() {
        let bytes = [0u8; 32];
        let hash = Hash::from_bytes(bytes);

        let formatted = format_hash(&hash, true);
        assert!(!formatted.ends_with("..."));
    }

    #[test]
    fn test_format_hash_with_custom_length() {
        let bytes = [0u8; 32];
        let hash = Hash::from_bytes(bytes);

        let formatted = format_hash_with_length(&hash, 8);
        assert!(formatted.len() <= 11); // 8 + 3 for "..."

        let full = format_hash_with_length(&hash, 0);
        assert!(!full.ends_with("..."));
    }

    // -------------------------------------------------------------------------
    // Timestamp Formatting Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_format_timestamp() {
        let ts = Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 45).unwrap();
        let formatted = format_timestamp(&ts);
        assert!(formatted.contains("2024"));
        assert!(formatted.contains("01"));
        assert!(formatted.contains("15"));
    }

    #[test]
    fn test_format_timestamp_relative_just_now() {
        let ts = Utc::now();
        let formatted = format_timestamp_relative(&ts);
        assert_eq!(formatted, "just now");
    }

    #[test]
    fn test_format_timestamp_relative_minutes() {
        let ts = Utc::now() - chrono::Duration::minutes(5);
        let formatted = format_timestamp_relative(&ts);
        assert!(formatted.contains("minutes ago"));
    }

    #[test]
    fn test_format_timestamp_relative_hours() {
        let ts = Utc::now() - chrono::Duration::hours(3);
        let formatted = format_timestamp_relative(&ts);
        assert!(formatted.contains("hours ago"));
    }

    #[test]
    fn test_format_timestamp_relative_yesterday() {
        let ts = Utc::now() - chrono::Duration::days(1);
        let formatted = format_timestamp_relative(&ts);
        assert_eq!(formatted, "yesterday");
    }

    #[test]
    fn test_format_timestamp_relative_days() {
        let ts = Utc::now() - chrono::Duration::days(3);
        let formatted = format_timestamp_relative(&ts);
        assert!(formatted.contains("days ago"));
    }

    #[test]
    fn test_format_timestamp_relative_weeks() {
        let ts = Utc::now() - chrono::Duration::weeks(2);
        let formatted = format_timestamp_relative(&ts);
        assert!(formatted.contains("weeks ago"));
    }

    // -------------------------------------------------------------------------
    // Size Formatting Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(100), "100 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kib() {
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
    }

    #[test]
    fn test_format_size_mib() {
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(1024 * 1024 * 5), "5.0 MiB");
    }

    #[test]
    fn test_format_size_gib() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    // -------------------------------------------------------------------------
    // Count Formatting Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_format_count_singular() {
        assert_eq!(format_count(1, "file", "files"), "1 file");
        assert_eq!(format_count(1, "change", "changes"), "1 change");
    }

    #[test]
    fn test_format_count_plural() {
        assert_eq!(format_count(0, "file", "files"), "0 files");
        assert_eq!(format_count(2, "file", "files"), "2 files");
        assert_eq!(format_count(100, "change", "changes"), "100 changes");
    }

    // -------------------------------------------------------------------------
    // DOT_DIR Constant Test
    // -------------------------------------------------------------------------

    #[test]
    fn test_dot_dir_constant() {
        assert_eq!(DOT_DIR, ".atomic");
    }
}
