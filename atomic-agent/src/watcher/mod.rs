//! Watchman file watching integration for turn-level change detection.
//!
// NOTE: The `FileWatcher` trait uses `Pin<Box<dyn Future>>` return types
// instead of `async fn` to maintain dyn-compatibility (object safety).
// This allows `Box<dyn FileWatcher>` in the orchestrator.
//!
//! This module provides the [`FileWatcher`] trait and its implementations for
//! detecting file changes during agent turns. The primary implementation uses
//! Facebook's [Watchman](https://facebook.github.io/watchman/) for real-time,
//! OS-level file monitoring. A fallback implementation using filesystem
//! snapshots is provided for environments where Watchman is not available.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      FileWatcher Trait                                   │
//! │                                                                         │
//! │   begin_turn(session_id)          end_turn() -> TurnChanges             │
//! │       │                               │                                 │
//! │       │  Captures clock/snapshot      │  Queries changes since start    │
//! │       │  at turn start                │  Returns { modified, added,     │
//! │       │                               │            deleted }            │
//! │       │                               │                                 │
//! ├───────┼───────────────────────────────┼─────────────────────────────────┤
//! │       │                               │                                 │
//! │  ┌────▼──────────────────┐  ┌─────────▼────────────────────┐           │
//! │  │  WatchmanTurnWatcher  │  │    FallbackWatcher           │           │
//! │  │  (Phase 16.2-16.3)    │  │    (Phase 16.5)              │           │
//! │  │                       │  │                               │           │
//! │  │  • Watchman clock()   │  │  • walkdir snapshot          │           │
//! │  │  • state_enter/leave  │  │  • mtime-based diffing       │           │
//! │  │  • query(since: ...)  │  │  • .atomicignore rules       │           │
//! │  │  • O(changed files)   │  │  • O(all files) per boundary │           │
//! │  └───────────────────────┘  └───────────────────────────────┘           │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Watchman Integration Details
//!
//! The Watchman-based watcher uses three key Watchman features:
//!
//! 1. **`clock`** — Captures a logical timestamp at turn start. This is
//!    Watchman's opaque clock value that represents a point-in-time view
//!    of the filesystem.
//!
//! 2. **`state-enter` / `state-leave`** — Brackets the turn with a named
//!    state (`"atomic-turn"`) so other Watchman subscribers (IDE plugins,
//!    CI tools) can defer or drop notifications during active agent turns.
//!    See `watchman_client::Client::state_enter()`.
//!
//! 3. **`query(since: clock)`** — At turn end, queries for all files that
//!    changed since the captured clock value. This returns exactly the files
//!    modified during the turn with zero filesystem scanning. Files in
//!    `.atomic/` are excluded via an `Expr::Not(DirName(".atomic"))` filter.
//!
//! # Fallback Behavior
//!
//! When Watchman is not running, `create_watcher()` automatically returns a
//! `FallbackWatcher` that uses `walkdir` to snapshot file metadata (paths +
//! mtimes) at turn boundaries and diffs them. This is O(all files) instead
//! of O(changed files) but requires no external daemon.
//!
//! # Subscription (Optional)
//!
//! The `subscription` submodule (Phase 16.4) provides a background
//! `FileSubscription` that yields real-time file change events. This is
//! intended for IDE integration — showing "agent is modifying these files"
//! live — not for turn recording. Subscriptions use `defer: ["atomic-turn"]`
//! so notifications are buffered during active turns and delivered in bulk
//! after `state_leave`.
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_agent::watcher::create_watcher;
//! use atomic_agent::watcher::WatcherConfig;
//!
//! // Auto-detect: Watchman if available, fallback otherwise
//! let config = WatcherConfig::new("/path/to/repo");
//! let mut watcher = create_watcher(config).await?;
//!
//! // On turn start: capture filesystem state
//! watcher.begin_turn("session-abc").await?;
//!
//! // ... agent works, modifies files ...
//!
//! // On turn end: query what changed
//! let changes = watcher.end_turn().await?;
//! println!("{}", changes.summary()); // "3 files changed (2 modified, 1 added)"
//! ```
//!
//! # Implementation Status
//!
//! This module is planned for Phase 16 of the task list:
//!
//! - **16.1** — `FileWatcher` trait and `WatcherConfig` (this file)
//! - **16.2** — `WatchmanConnection` manager
//! - **16.3** — `WatchmanTurnWatcher` implementing `FileWatcher`
//! - **16.4** — `FileSubscription` for background real-time events (optional)
//! - **16.5** — `FallbackWatcher` for environments without Watchman

pub mod fallback;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::error::AgentResult;
use crate::event::TurnChanges;

// WatcherConfig

/// Configuration for creating a [`FileWatcher`].
///
/// Controls the repository root path and patterns for excluding files
/// from change detection (e.g., the `.atomic/` directory itself).
///
/// # Example
///
/// ```rust
/// use atomic_agent::watcher::WatcherConfig;
///
/// let config = WatcherConfig::new("/path/to/repo");
/// assert_eq!(config.repo_root().to_str().unwrap(), "/path/to/repo");
///
/// // Customize ignore patterns
/// let config = WatcherConfig::new("/path/to/repo")
///     .with_ignore_pattern("build/")
///     .with_ignore_pattern("*.tmp");
/// ```
#[derive(Clone, Debug)]
pub struct WatcherConfig {
    /// Root directory of the Atomic repository.
    repo_root: PathBuf,

    /// Glob patterns for paths to exclude from change detection.
    ///
    /// The `.atomic/` directory is always excluded by default.
    /// Additional patterns can be added for build artifacts, etc.
    ignore_patterns: Vec<String>,
}

impl WatcherConfig {
    /// Default ignore patterns applied to all watchers.
    const DEFAULT_IGNORE: &'static [&'static str] = &[".atomic"];

    /// Create a new `WatcherConfig` for the given repository root.
    ///
    /// The `.atomic/` directory is automatically added to the ignore list.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            ignore_patterns: Self::DEFAULT_IGNORE
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }

    /// Add a glob pattern to the ignore list.
    ///
    /// Files matching these patterns will be excluded from `TurnChanges`
    /// produced by the watcher.
    #[must_use]
    pub fn with_ignore_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.ignore_patterns.push(pattern.into());
        self
    }

    /// Returns the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the list of ignore patterns.
    pub fn ignore_patterns(&self) -> &[String] {
        &self.ignore_patterns
    }
}

// FileWatcher Trait

/// Trait for detecting file changes during agent turns.
///
/// Implementations bracket a turn with `begin_turn()` / `end_turn()` and
/// return the precise set of files that changed during that window.
///
/// Two implementations are planned:
///
/// - `WatchmanTurnWatcher` — Uses Watchman's `clock` + `since` queries
///   for O(changed-files) detection with zero scanning.
///
/// - [`crate::watcher::fallback::FallbackWatcher`] — Uses filesystem snapshots via `walkdir` for
///   O(all-files) detection when Watchman is unavailable.
///
/// The trait is async because the Watchman client is async (`tokio`-based).
/// The fallback watcher's async methods are trivially synchronous internally.
///
/// # Lifecycle
///
/// ```text
/// begin_turn("session-id")   →   agent works   →   end_turn() → TurnChanges
///         │                                              │
///         │  Captures baseline                           │  Returns delta
///         │  (Watchman clock or fs snapshot)              │  since baseline
/// ```
///
/// # Cancel
///
/// If a turn needs to be abandoned (e.g., agent crashes), call `cancel_turn()`
/// to release any held state without querying for changes.
///
/// # Dyn Compatibility
///
/// Methods return `Pin<Box<dyn Future>>` instead of using `async fn` so that
/// the trait can be used as `Box<dyn FileWatcher>` in the orchestrator.
pub trait FileWatcher: Send + Sync {
    /// Mark the beginning of an agent turn.
    ///
    /// Captures the current filesystem state so that `end_turn()` can
    /// compute the delta. For Watchman, this calls `clock()` and
    /// `state_enter("atomic-turn")`. For the fallback, this snapshots
    /// file paths and mtimes.
    ///
    /// # Arguments
    ///
    /// * `session_id` — The agent session identifier. Passed as metadata
    ///   to Watchman's `state_enter` so other subscribers can see which
    ///   session is active.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::AgentError::WatchmanQueryFailed`] if the Watchman clock
    /// or state_enter call fails. Returns [`crate::error::AgentError::Io`] if the
    /// fallback watcher cannot read the filesystem.
    fn begin_turn(
        &mut self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>>;

    /// Mark the end of an agent turn and return the files that changed.
    ///
    /// Queries for all file changes since `begin_turn()` was called.
    /// For Watchman, this calls `state_leave` + `query(since: clock)`.
    /// For the fallback, this re-snapshots and diffs.
    ///
    /// # Returns
    ///
    /// A [`TurnChanges`] struct with `modified`, `added`, and `deleted`
    /// file lists. Returns an empty `TurnChanges` if nothing changed.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::AgentError::TurnNotActive`] if `begin_turn()` was not
    /// called first. Returns [`crate::error::AgentError::WatchmanQueryFailed`] if the
    /// Watchman query fails.
    fn end_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<TurnChanges>> + Send + '_>>;

    /// Cancel the current turn without querying for changes.
    ///
    /// Releases any held state (Watchman `state_leave`, snapshot memory).
    /// This is a no-op if no turn is active.
    fn cancel_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>>;

    /// Returns `true` if a turn is currently active (between `begin_turn`
    /// and `end_turn` / `cancel_turn`).
    fn is_active(&self) -> bool;
}

// Factory Function

/// Create a [`FileWatcher`] using the best available backend.
///
/// Attempts to connect to Watchman first. If Watchman is not running or
/// the connection fails, falls back to the snapshot-based watcher.
///
/// # Arguments
///
/// * `config` — Watcher configuration (repo root, ignore patterns)
///
/// # Returns
///
/// A boxed `FileWatcher` implementation. The caller does not need to know
/// which backend was selected.
///
/// # Implementation Status
///
/// Currently returns a `FallbackWatcher` unconditionally. The Watchman
/// backend will be implemented in Phase 16.2–16.3.
pub async fn create_watcher(config: WatcherConfig) -> AgentResult<Box<dyn FileWatcher>> {
    // Watchman backend (Phase 16.2-16.3 in ATOMIC-AGENT-TASKS.md):
    //
    // When implemented, this function will attempt to connect to the Watchman
    // daemon first. If successful, it returns a WatchmanTurnWatcher that uses
    // clock + since queries for O(changed-files) detection. If Watchman is not
    // running, it falls through to the snapshot-based fallback below.
    //
    // The Watchman backend will use:
    //   - watchman_client::Connector::new().connect() for connection
    //   - client.clock() + client.query(since: ...) for turn boundary diffing
    //   - client.state_enter/state_leave("atomic-turn") for subscriber coordination
    //   - Expr::Not(DirName(".atomic")) to exclude the repo metadata directory

    // Fall back to snapshot-based watcher (always available, O(all files) per boundary)
    log::info!("Using fallback file watcher (install Watchman for faster change detection)");
    Ok(Box::new(fallback::FallbackWatcher::new(config)))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // WatcherConfig tests

    #[test]
    fn test_config_new() {
        let config = WatcherConfig::new("/repo");
        assert_eq!(config.repo_root(), Path::new("/repo"));
        // Should have default ignore pattern
        assert!(config.ignore_patterns().contains(&".atomic".to_string()));
    }

    #[test]
    fn test_config_new_from_pathbuf() {
        let path = PathBuf::from("/some/repo");
        let config = WatcherConfig::new(path.clone());
        assert_eq!(config.repo_root(), path.as_path());
    }

    #[test]
    fn test_config_default_ignore_patterns() {
        let config = WatcherConfig::new("/repo");
        let patterns = config.ignore_patterns();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0], ".atomic");
    }

    #[test]
    fn test_config_with_ignore_pattern() {
        let config = WatcherConfig::new("/repo")
            .with_ignore_pattern("build/")
            .with_ignore_pattern("*.tmp");

        let patterns = config.ignore_patterns();
        assert_eq!(patterns.len(), 3);
        assert!(patterns.contains(&".atomic".to_string()));
        assert!(patterns.contains(&"build/".to_string()));
        assert!(patterns.contains(&"*.tmp".to_string()));
    }

    #[test]
    fn test_config_with_ignore_pattern_chaining() {
        let config = WatcherConfig::new("/repo")
            .with_ignore_pattern("a")
            .with_ignore_pattern("b")
            .with_ignore_pattern("c");

        // Original default + 3 custom
        assert_eq!(config.ignore_patterns().len(), 4);
    }

    #[test]
    fn test_config_clone() {
        let config = WatcherConfig::new("/repo").with_ignore_pattern("extra");
        let cloned = config.clone();

        assert_eq!(cloned.repo_root(), config.repo_root());
        assert_eq!(cloned.ignore_patterns(), config.ignore_patterns());
    }

    #[test]
    fn test_config_debug() {
        let config = WatcherConfig::new("/repo");
        let debug = format!("{:?}", config);
        assert!(debug.contains("WatcherConfig"));
        assert!(debug.contains("/repo"));
        assert!(debug.contains(".atomic"));
    }

    // FileWatcher trait tests (object safety)

    #[test]
    fn test_file_watcher_is_object_safe() {
        // This test verifies that FileWatcher can be used as a trait object.
        // If this compiles, the trait is object-safe.
        fn _accept_boxed(_watcher: Box<dyn FileWatcher>) {}
    }

    // create_watcher tests

    #[tokio::test]
    async fn test_create_watcher_returns_fallback() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = WatcherConfig::new(dir.path());
        let result = create_watcher(config).await;
        assert!(result.is_ok());

        let watcher = result.unwrap();
        assert!(!watcher.is_active());
    }

    #[tokio::test]
    async fn test_create_watcher_fallback_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = WatcherConfig::new(dir.path());
        let mut watcher = create_watcher(config).await.unwrap();

        watcher.begin_turn("test-session").await.unwrap();
        assert!(watcher.is_active());

        std::fs::write(dir.path().join("new.rs"), "fn new() {}").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.file_count(), 1);
        assert!(!watcher.is_active());
    }
}
