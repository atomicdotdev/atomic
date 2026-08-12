//! The `push` command for syncing views to a remote repository.
//!
//! This module implements the `atomic push` command, which syncs local views
//! to a remote repository **identity-preservingly** via view manifests. It
//! communicates with `atomic-api` servers using the HTTP protocol defined in
//! the `atomic-remote` crate.
//!
//! # Module Structure
//!
//! The push command is split into several submodules for maintainability:
//!
//! - [`command`]: The main `Push` struct and `Command` implementation
//! - [`types`]: Data structures (`PushChange`, `PushStats`, `PushOutcome`)
//! - [`helpers`]: Chain construction, sync planning, error conversion, etc.
//!
//! # Overview
//!
//! Push no longer flattens a draft view into a shared remote view. Instead,
//! each view in the target's parent chain is transferred as a
//! [`ViewManifest`](atomic_repository::ViewManifest) — name, scope, parent,
//! ordered change log, merkle state — so the remote ends up with the same
//! view hierarchy as the local repository:
//!
//! 1. **Open local repository** and determine the leaf view (current or
//!    `--from-view`)
//! 2. **Build the ancestor chain** root → leaf by walking view parents
//!    (cycle-guarded)
//! 3. **Connect to remote** using HTTP client
//! 4. For each view in the chain, root → leaf:
//!    - **Export the local manifest** (`Repository::view_manifest`)
//!    - **Fetch the remote manifest** (`?view-manifest=<name>`); a server
//!      without manifest support is a hard error — no flatten fallback
//!    - **Require the fast-forward rule**: the remote log must be a prefix
//!      of the local log, otherwise the push reports divergence
//!    - **Store the suffix** — change files the remote lacks — via
//!      `?store=<hash>` (content-only, idempotent, no view application)
//!    - **Declare the manifest** (`POST ?view-manifest=<name>`); the server
//!      creates/fast-forwards the view and verifies the merkle state
//! 5. **Upload attestations, provenance graphs, and tags** that travel with
//!    the pushed changes
//! 6. **Report results** to the user
//!
//! Because the chain is synced root → leaf, a draft's parent always exists
//! on the remote before the draft's manifest references it, and a draft's
//! inherited prefix is never re-stored (it was synced with the parent).
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
//!       --to-view <VIEW>      Remote name for the leaf view (default: same as local)
//!       --from-view <VIEW>    Local view to push from (default: current)
//!   -n, --dry-run             Show what would be synced without pushing
//!   -f, --force               Attempt the push even with diverged history
//!                             (server remains authoritative)
//!   -a, --all                 Store all changes in the log, not just the suffix
//!   -k, --insecure            Skip TLS certificate verification
//!       --timeout <SECONDS>   Request timeout in seconds (default: 30)
//!   -h, --help                Print help information
//! ```
//!
//! # Output
//!
//! On success, the command displays the per-view sync progress:
//!
//! ```text
//! Pushing to origin (https://api.example.com/tenant/t/portfolio/p/project/pr/code)
//! ✓ Remote dev has 2 changes
//! ✓ Remote orange does not exist yet
//! Syncing view dev (1 new):
//!   ✓ GHI789... (1/1) Fix login bug
//! ✓ Declared dev [shared] (3 changes in log)
//! Syncing view orange (2 new):
//!   ✓ JKL012... (1/2) Add authentication module
//!   ✓ DEF456... (2/2) Update tests
//! ✓ Declared orange [draft] (5 changes in log)
//!
//! Push complete: 2 views synced (3 changes stored) to origin
//! ```
//!
//! # Dry Run
//!
//! Use `--dry-run` to preview what would be synced:
//!
//! ```text
//! $ atomic push --dry-run
//! Would sync 2 views to origin:
//!
//!   dev [shared]: 1 change to store, declare manifest (3 changes in log)
//!     GHI789... Fix login bug
//!   orange [draft, parent dev]: 2 changes to store, declare manifest (5 changes in log)
//!     JKL012... Add authentication module
//!     DEF456... Update tests
//! ```
//!
//! # Error Handling
//!
//! The command handles several error conditions:
//!
//! - **No remote configured**: Suggests adding a remote
//! - **Authentication failed**: Suggests checking credentials
//! - **Network errors**: Provides retry suggestions
//! - **Diverged history**: The remote log is not a prefix of the local log;
//!   suggests `atomic pull`, or `--force` to let the server decide
//! - **Identity mismatch**: The view exists remotely with a different
//!   scope/parent; never forceable
//! - **Old server**: A server without `?view-manifest` support is a hard
//!   error — identity-preserving push has no flatten fallback
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
//! │  │  (local)    │    │ (local)     │    │ (atomic-remote)             │ │
//! │  └──────┬──────┘    └──────┬──────┘    └────────────┬────────────────┘ │
//! │         │                  │                        │                  │
//! │         │ 1. Build chain + │                        │                  │
//! │         │    export manifests                       │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │           2. GET ?view-manifest per view  │                  │
//! │         │───────────────────────────────────────────►                  │
//! │         │                  │                        │                  │
//! │         │           3. Plan fast-forward suffix     │                  │
//! │         │◄──────────────────────────────────────────┤                  │
//! │         │                  │                        │                  │
//! │         │  4. Load changes │                        │                  │
//! │         ├─────────────────►│                        │                  │
//! │         │                  │                        │                  │
//! │         │           5. POST ?store per new change   │                  │
//! │         │───────────────────────────────────────────►                  │
//! │         │                  │                        │                  │
//! │         │           6. POST ?view-manifest per view │                  │
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
