//! The `push` command for uploading changes to a remote repository.
//!
//! This module implements the `atomic push` command, which uploads local changes
//! to a remote repository. It communicates with `atomic-api` servers using the
//! HTTP protocol defined in the `atomic-remote-client` crate.
//!
//! # Module Structure
//!
//! The push command is split into several submodules for maintainability:
//!
//! - [`command`]: The main `Push` struct and `Command` implementation
//! - [`types`]: Data structures (`PushChange`, `PushStats`, `PushOutcome`)
//! - [`helpers`]: Utility functions for delta calculation, error conversion, etc.
//!
//! # Overview
//!
//! The push command performs the following workflow:
//!
//! 1. **Open local repository** and determine current view
//! 2. **Load remote configuration** from repository settings
//! 3. **Connect to remote** using HTTP client
//! 4. **Query remote state** to determine what changes exist remotely
//! 5. **Calculate delta** between local and remote changelists
//! 6. **Upload changes** in dependency order (dependencies first)
//! 7. **Upload tags** for any tagged states
//! 8. **Report results** to the user
//!
//! # Usage
//!
//! ```text
//! atomic push [OPTIONS] [REMOTE]
//!
//! Arguments:
//!   [REMOTE]  Remote name or URL (default: the configured default remote, or "origin")
//!
//! Options:
//!       --to-channel <CHANNEL>    Remote channel to push to (default: same as local)
//!       --from-channel <CHANNEL>  Local channel to push from (default: current)
//!   -n, --dry-run                 Show what would be pushed without pushing
//!   -f, --force                   Force push even with diverged history
//!   -a, --all                     Push all changes (not just new ones)
//!   -k, --insecure                Skip TLS certificate verification
//!       --timeout <SECONDS>       Request timeout in seconds (default: 30)
//!   -h, --help                    Print help information
//! ```
//!
//! # Output
//!
//! On success, the command displays information about pushed changes:
//!
//! ```text
//! Pushing to origin (https://api.example.com/tenant/t/portfolio/p/project/pr/code)
//!   Remote: main at ABC123...
//!   Local:  main at DEF456...
//!
//! Uploading 3 changes:
//!   ✓ GHI789... (1/3) Add authentication module
//!   ✓ JKL012... (2/3) Fix login bug
//!   ✓ DEF456... (3/3) Update tests
//!
//! Push complete: 3 changes uploaded
//! ```
//!
//! # Dry Run
//!
//! Use `--dry-run` to preview what would be pushed:
//!
//! ```text
//! $ atomic push --dry-run
//! Would push 3 changes to origin:
//!   GHI789... Add authentication module
//!   JKL012... Fix login bug
//!   DEF456... Update tests
//! ```
//!
//! # Error Handling
//!
//! The command handles several error conditions:
//!
//! - **No remote configured**: Suggests adding a remote
//! - **Authentication failed**: Suggests checking credentials
//! - **Network errors**: Provides retry suggestions
//! - **Diverged history**: Explains the situation and suggests `--force`
//! - **Missing dependencies**: Suggests pushing with `--all`
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           Push Command                                   │
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
//! │         │  4. Load changes │                        │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │           5. Upload changes               │                  │
//! │         │───────────────────────────────────────────►                  │
//! │         │                  │                        │                  │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

// Submodules

mod command;
mod helpers;
pub mod types;

// Re-exports

// Main command struct
pub use command::{Push, DEFAULT_REMOTE, DEFAULT_TIMEOUT_SECS};
pub use types::{PushChange, PushOutcome, PushStats};

// Types for external use

// Helper functions that might be useful externally

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: format a count with auto-pluralization (singular + "s").
    fn format_count(count: usize, singular: &str) -> String {
        if count == 1 {
            format!("1 {}", singular)
        } else {
            format!("{} {}s", count, singular)
        }
    }

    #[test]
    fn test_push_reexported() {
        let push = Push::new();
        assert!(push.remote.is_none());
    }

    #[test]
    fn test_push_change_reexported() {
        use atomic_core::types::{Hash, Merkle};

        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO);
        assert_eq!(change.sequence, 0);
    }

    #[test]
    fn test_push_stats_reexported() {
        let stats = PushStats::new();
        assert_eq!(stats.total_uploaded(), 0);
    }

    #[test]
    fn test_push_outcome_reexported() {
        let outcome = PushOutcome::default();
        assert!(outcome.is_success());
    }

    #[test]
    fn test_format_count_reexported() {
        assert_eq!(format_count(1, "change"), "1 change");
        assert_eq!(format_count(5, "change"), "5 changes");
    }

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_REMOTE, "origin");
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }
}
