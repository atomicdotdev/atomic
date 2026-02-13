//! The `pull` command for downloading changes from a remote repository.
//!
//! This module implements the `atomic pull` command, which downloads remote changes
//! and applies them to the local repository. It communicates with `atomic-api` servers
//! using the HTTP protocol defined in the `atomic-remote-client` crate.
//!
//! # Module Structure
//!
//! The pull command is split into several submodules for maintainability:
//!
//! - [`command`]: The main `Pull` struct and `Command` implementation
//! - [`types`]: Data structures (`PullChange`, `PullStats`, `PullOutcome`)
//! - [`helpers`]: Utility functions for delta calculation, error conversion, etc.
//!
//! # Overview
//!
//! The pull command performs the following workflow:
//!
//! 1. **Open local repository** and determine current stack
//! 2. **Load remote configuration** from repository settings
//! 3. **Connect to remote** using HTTP client
//! 4. **Query remote state** to determine what changes exist remotely
//! 5. **Calculate delta** between remote and local changelists
//! 6. **Download changes** in dependency order (dependencies first)
//! 7. **Apply changes** to the local stack (unless download-only)
//! 8. **Download tags** for any tagged states
//! 9. **Report results** to the user
//!
//! # Usage
//!
//! ```text
//! atomic pull [OPTIONS] [REMOTE]
//!
//! Arguments:
//!   [REMOTE]  Remote name or URL (default: "origin")
//!
//! Options:
//!       --to-channel <CHANNEL>    Local channel to pull into (default: current)
//!       --from-channel <CHANNEL>  Remote channel to pull from (default: same as local)
//!   -n, --dry-run                 Show what would be pulled without pulling
//!   -a, --all                     Pull all changes (not just new ones)
//!   -k, --insecure                Skip TLS certificate verification
//!       --timeout <SECONDS>       Request timeout in seconds (default: 30)
//!       --download-only           Download changes without applying them
//!   -h, --help                    Print help information
//! ```
//!
//! # Output
//!
//! On success, the command displays information about pulled changes:
//!
//! ```text
//! Pulling from origin (https://api.example.com/tenant/t/portfolio/p/project/pr/code)
//!   Remote: main at DEF456...
//!   Local:  main at ABC123...
//!
//! Downloading 3 changes:
//!   ✓ GHI789... (1/3) Add authentication module
//!   ✓ JKL012... (2/3) Fix login bug
//!   ✓ DEF456... (3/3) Update tests
//!
//! Applying changes...
//!   ✓ Applied 3 changes
//!
//! Pull complete: 3 changes downloaded and applied
//! ```
//!
//! # Dry Run
//!
//! Use `--dry-run` to preview what would be pulled:
//!
//! ```text
//! $ atomic pull --dry-run
//! Would pull 3 changes from origin:
//!   GHI789... Add authentication module
//!   JKL012... Fix login bug
//!   DEF456... Update tests
//! ```
//!
//! # Download Only
//!
//! Use `--download-only` to fetch changes without applying them:
//!
//! ```text
//! $ atomic pull --download-only
//! Downloaded 3 changes (not applied)
//! ```
//!
//! # Error Handling
//!
//! The command handles several error conditions:
//!
//! - **No remote configured**: Suggests adding a remote
//! - **Authentication failed**: Suggests checking credentials
//! - **Network errors**: Provides retry suggestions
//! - **Diverged history**: Warns about local-only changes
//! - **Missing dependencies**: Suggests using `--all`
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           Pull Command                                   │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────────────┐ │
//! │  │  Repository │    │ ChangeStore │    │      HttpRemote             │ │
//! │  │  (local)    │    │ (local)     │    │ (atomic-remote-client)      │ │
//! │  └──────┬──────┘    └──────┬──────┘    └────────────┬────────────────┘ │
//! │         │                  │                        │                  │
//! │         │  1. Get history  │                        │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │           2. Query remote state           │                  │
//! │         │───────────────────────────────────────────►                  │
//! │         │                  │                        │                  │
//! │         │           3. Calculate delta              │                  │
//! │         │◄──────────────────────────────────────────┤                  │
//! │         │                  │                        │                  │
//! │         │           4. Download changes             │                  │
//! │         │◄──────────────────────────────────────────┤                  │
//! │         │                  │                        │                  │
//! │         │  5. Store changes│                        │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │  6. Apply to stack                        │                  │
//! │         ├───────────────────                        │                  │
//! │         │                  │                        │                  │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Diverged History Detection
//!
//! The pull command detects when local and remote histories have diverged:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Diverged History Example                          │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Local:   A → B → C → D (local-only)                                    │
//! │                    ↓                                                    │
//! │  Remote:  A → B → C → E → F                                             │
//! │                                                                         │
//! │  In this case, D is local-only and E,F are remote-only.                │
//! │  The pull will warn about D but still download E and F.                │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

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
pub use command::Pull;

// Types for external use

// Helper functions that might be useful externally

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the Pull struct is properly re-exported and constructible.
    #[test]
    fn test_pull_reexported() {
        let pull = Pull::new();
        assert_eq!(pull.remote, DEFAULT_REMOTE);
    }

    /// Verify that PullChange is properly re-exported.
    #[test]
    fn test_pull_change_reexported() {
        use atomic_core::types::{Hash, Merkle};

        let hash = Hash::of(b"test");
        let change = PullChange::new(hash, 0, Merkle::ZERO);
        assert_eq!(change.sequence, 0);
    }

    /// Verify that PullStats is properly re-exported.
    #[test]
    fn test_pull_stats_reexported() {
        let stats = PullStats::new();
        assert_eq!(stats.total_downloaded(), 0);
    }

    /// Verify that PullOutcome is properly re-exported.
    #[test]
    fn test_pull_outcome_reexported() {
        let outcome = PullOutcome::default();
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
        assert_eq!(DEFAULT_REMOTE, "origin");
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }

    /// Verify dry_run mode is available on PullOutcome.
    #[test]
    fn test_pull_outcome_dry_run() {
        let outcome = PullOutcome::dry_run(PullStats::new());
        assert!(outcome.dry_run);
        assert!(outcome.is_success());
    }

    /// Verify PullStats tracking methods work correctly.
    #[test]
    fn test_pull_stats_tracking() {
        let mut stats = PullStats::new();
        assert!(!stats.has_downloads());

        stats.record_change_downloaded(1024);
        assert!(stats.has_downloads());
        assert_eq!(stats.changes_downloaded, 1);
        assert_eq!(stats.bytes_transferred, 1024);
    }
}
