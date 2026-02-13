//! The `clone` command for creating a local copy of a remote repository.
//!
//! This module implements the `atomic clone` command, which creates a new local
//! repository by downloading all changes from a remote repository. It communicates
//! with `atomic-api` servers using the HTTP protocol defined in the `atomic-remote-client`
//! crate.
//!
//! # Module Structure
//!
//! The clone command is split into several submodules for maintainability:
//!
//! - [`command`]: The main `Clone` struct and `Command` implementation
//! - [`types`]: Data structures (`CloneProgress`, `CloneStats`, `CloneOutcome`)
//! - [`helpers`]: Utility functions for URL parsing, directory creation, etc.
//!
//! # Overview
//!
//! The clone command performs the following workflow:
//!
//! 1. **Parse remote URL** and infer repository name if path not provided
//! 2. **Validate target** directory doesn't already exist
//! 3. **Create target directory** with cleanup guard for error recovery
//! 4. **Initialize empty repository** using `Repository::init()`
//! 5. **Connect to remote** using HTTP client
//! 6. **Query remote state** to get the stack's changelist
//! 7. **Download all changes** in dependency order
//! 8. **Apply changes** to the local stack
//! 9. **Download tags** for any tagged states
//! 10. **Configure remote** as "origin" in repository config
//! 11. **Report results** to the user
//!
//! # Usage
//!
//! ```text
//! atomic clone [OPTIONS] <URL> [PATH]
//!
//! Arguments:
//!   <URL>   URL of the repository to clone
//!   [PATH]  Directory to clone into (defaults to repository name)
//!
//! Options:
//!       --stack <STACK>        Clone specific stack (default: dev)
//!   -k, --insecure            Skip TLS certificate verification
//!       --timeout <SECONDS>   Request timeout in seconds (default: 30)
//!       --download-only       Download changes without applying them
//!   -h, --help                Print help information
//! ```
//!
//! # Output
//!
//! On success, the command displays information about the cloned repository:
//!
//! ```text
//! Cloning from https://api.example.com/tenant/t/portfolio/p/project/pr/code
//! into my-project...
//!
//! Connecting to remote...  ✓ Connected
//! Querying remote state... ✓ main at DEF456... (15 changes)
//!
//! Downloading 15 changes:
//!   ✓ ABC123... (1/15) Initial commit
//!   ✓ DEF456... (2/15) Add authentication
//!   ...
//!   ✓ XYZ789... (15/15) Update tests
//!
//! Applying changes...      ✓ Applied 15 changes
//! Configuring remote...    ✓ origin configured
//!
//! Clone complete: 15 changes downloaded into my-project/
//! ```
//!
//! # Error Handling
//!
//! The command handles several error conditions:
//!
//! - **Target exists**: Directory already exists at target path
//! - **Invalid URL**: Cannot parse or connect to the remote URL
//! - **Authentication failed**: Suggests checking credentials
//! - **Network errors**: Provides retry suggestions
//! - **Empty stack**: Remote stack has no changes
//!
//! # Cleanup on Error
//!
//! If the clone fails partway through, the `CleanupGuard` ensures the partially
//! created directory is removed, leaving no trace of the failed operation.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           Clone Command                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │                      CleanupGuard                                │   │
//! │  │  (ensures partial clone is removed on error)                    │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │         │                                                               │
//! │         ▼                                                               │
//! │  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────────────┐ │
//! │  │  Repository │    │ ChangeStore │    │      HttpRemote             │ │
//! │  │  (local)    │    │ (local)     │    │ (atomic-remote-client)      │ │
//! │  └──────┬──────┘    └──────┬──────┘    └────────────┬────────────────┘ │
//! │         │                  │                        │                  │
//! │         │  1. Init repo    │                        │                  │
//! │         ├───────────────────                        │                  │
//! │         │                  │                        │                  │
//! │         │           2. Query remote state           │                  │
//! │         │───────────────────────────────────────────►                  │
//! │         │                  │                        │                  │
//! │         │           3. Get changelist               │                  │
//! │         │◄──────────────────────────────────────────┤                  │
//! │         │                  │                        │                  │
//! │         │           4. Download changes             │                  │
//! │         │◄──────────────────────────────────────────┤                  │
//! │         │                  │                        │                  │
//! │         │  5. Save changes │                        │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │  6. Apply to stack                        │                  │
//! │         ├───────────────────                        │                  │
//! │         │                  │                        │                  │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Partial Clone (Future)
//!
//! The `--path` option (when implemented) will support partial clones where only
//! changes affecting specific paths are downloaded. This is useful for large
//! repositories where only a subset of files are needed.

// =============================================================================
// Submodules
// =============================================================================

mod command;
mod helpers;
pub mod types;

// =============================================================================
// Re-exports
// =============================================================================

// Main command struct
pub use command::Clone;

// Types for external use

// Helper functions that might be useful externally

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the Clone struct is properly re-exported and constructible.
    #[test]
    fn test_clone_reexported() {
        let clone = Clone::new("https://example.com/repo".to_string());
        assert_eq!(clone.url, "https://example.com/repo");
    }

    /// Verify that CloneStats is properly re-exported.
    #[test]
    fn test_clone_stats_reexported() {
        let stats = CloneStats::new();
        assert_eq!(stats.total_downloaded(), 0);
    }

    /// Verify that CloneOutcome is properly re-exported.
    #[test]
    fn test_clone_outcome_reexported() {
        let outcome = CloneOutcome::default();
        assert!(outcome.is_success());
    }

    /// Verify that format_count helper is properly re-exported.
    #[test]
    fn test_format_count_reexported() {
        assert_eq!(format_count(1, "change"), "1 change");
        assert_eq!(format_count(5, "change"), "5 changes");
    }

    /// Verify that default constants are properly exported.
    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_STACK, "dev");
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }

    /// Verify that infer_repo_name helper is properly re-exported.
    #[test]
    fn test_infer_repo_name_reexported() {
        let name = infer_repo_name("https://example.com/org/project/code");
        assert!(name.is_some());
    }

    /// Verify that CleanupGuard is properly re-exported.
    #[test]
    fn test_cleanup_guard_reexported() {
        use std::path::PathBuf;
        let guard = CleanupGuard::new(PathBuf::from("/tmp/test"));
        // Guard should be created without panic
        assert!(!guard.is_disabled());
    }
}
