//! Fallback file watcher using filesystem snapshots.
//!
//! This module provides a [`FallbackWatcher`] that detects file changes by
//! taking snapshots of the filesystem at turn boundaries and diffing them.
//! It is used when Watchman is not available (not installed or not running).
//!
//! # Performance
//!
//! The fallback watcher is O(all files) per turn boundary, compared to
//! Watchman's O(changed files). For small-to-medium repositories this is
//! fine. For large repositories (100K+ files), Watchman is recommended.
//!
//! # How It Works
//!
//! ```text
//! begin_turn()                              end_turn()
//!     │                                         │
//!     ▼                                         ▼
//!  Snapshot A                                Snapshot B
//!  { path → mtime }                         { path → mtime }
//!     │                                         │
//!     └─────────────── diff ────────────────────┘
//!                        │
//!                        ▼
//!                   TurnChanges {
//!                     modified: paths where mtime changed,
//!                     added: paths in B but not A,
//!                     deleted: paths in A but not B,
//!                   }
//! ```
//!
//! # Ignore Patterns
//!
//! The `.atomic/` directory is always excluded. Additional patterns from
//! `WatcherConfig::ignore_patterns` are matched against path components.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::error::{AgentError, AgentResult};
use crate::event::TurnChanges;
use crate::watcher::{FileWatcher, WatcherConfig};

// FileSnapshot

/// A point-in-time snapshot of the filesystem: path → (mtime, size).
///
/// We track both mtime and size to reduce false positives (some operations
/// touch mtime without changing content) and false negatives (some
/// operations change content without updating mtime on the same second).
#[derive(Clone, Debug)]
struct FileEntry {
    /// Last modification time.
    mtime: SystemTime,
    /// File size in bytes.
    size: u64,
}

type FileSnapshot = HashMap<PathBuf, FileEntry>;

/// Walk the directory tree and build a snapshot of all files.
///
/// Skips directories matching any of the ignore patterns.
/// Only includes regular files (not directories, symlinks, etc.).
fn take_snapshot(repo_root: &Path, ignore_patterns: &[String]) -> AgentResult<FileSnapshot> {
    let mut snapshot = HashMap::new();

    let walker = WalkDir::new(repo_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            // Skip ignored directories
            if entry.file_type().is_dir() {
                let name = entry.file_name().to_string_lossy();
                for pattern in ignore_patterns {
                    if name == pattern.as_str() {
                        return false;
                    }
                }
                // Also skip common VCS/build directories for performance
                if matches!(
                    name.as_ref(),
                    ".git" | "node_modules" | "target" | "__pycache__" | ".DS_Store"
                ) {
                    return false;
                }
            }
            true
        });

    for entry in walker {
        let entry = entry.map_err(|e| AgentError::Internal(format!("walkdir error: {}", e)))?;

        // Only track regular files
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        // Get metadata for mtime and size
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue, // Skip files we can't stat (permissions, etc.)
        };

        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size = metadata.len();

        // Store path relative to repo root
        if let Ok(relative) = path.strip_prefix(repo_root) {
            snapshot.insert(relative.to_path_buf(), FileEntry { mtime, size });
        }
    }

    Ok(snapshot)
}

/// Diff two snapshots to produce a `TurnChanges`.
fn diff_snapshots(before: &FileSnapshot, after: &FileSnapshot) -> TurnChanges {
    let mut modified = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();

    // Find modified and deleted files
    for (path, before_entry) in before {
        match after.get(path) {
            Some(after_entry) => {
                // File exists in both — check if changed
                if before_entry.mtime != after_entry.mtime || before_entry.size != after_entry.size
                {
                    modified.push(path.clone());
                }
            }
            None => {
                // File was in before but not after — deleted
                deleted.push(path.clone());
            }
        }
    }

    // Find added files
    for path in after.keys() {
        if !before.contains_key(path) {
            added.push(path.clone());
        }
    }

    // Sort for deterministic output
    modified.sort();
    added.sort();
    deleted.sort();

    TurnChanges::new()
        .with_modified(modified)
        .with_added(added)
        .with_deleted(deleted)
}

// FallbackWatcher

/// File watcher using filesystem snapshots (no Watchman required).
///
/// Takes a snapshot of file paths and metadata at `begin_turn()`, takes
/// another at `end_turn()`, and diffs them to produce `TurnChanges`.
///
/// # Usage
///
/// ```rust,ignore
/// use atomic_agent::watcher::fallback::FallbackWatcher;
/// use atomic_agent::watcher::WatcherConfig;
///
/// let config = WatcherConfig::new("/path/to/repo");
/// let mut watcher = FallbackWatcher::new(config);
///
/// watcher.begin_turn("session-1").await?;
/// // ... agent modifies files ...
/// let changes = watcher.end_turn().await?;
/// println!("{}", changes.summary());
/// ```
#[derive(Debug)]
pub struct FallbackWatcher {
    /// Watcher configuration (repo root, ignore patterns).
    config: WatcherConfig,

    /// Snapshot taken at `begin_turn()`.
    ///
    /// `None` when no turn is active.
    pre_snapshot: Option<FileSnapshot>,

    /// The session ID of the active turn (for error messages).
    active_session: Option<String>,
}

impl FallbackWatcher {
    /// Create a new fallback watcher with the given configuration.
    pub fn new(config: WatcherConfig) -> Self {
        Self {
            config,
            pre_snapshot: None,
            active_session: None,
        }
    }

    /// Returns the repository root path.
    pub fn repo_root(&self) -> &Path {
        self.config.repo_root()
    }
}

impl FileWatcher for FallbackWatcher {
    fn begin_turn(
        &mut self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let snapshot = take_snapshot(self.config.repo_root(), self.config.ignore_patterns())?;

            self.pre_snapshot = Some(snapshot);
            self.active_session = Some(session_id);
            Ok(())
        })
    }

    fn end_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<TurnChanges>> + Send + '_>> {
        Box::pin(async move {
            let pre = self
                .pre_snapshot
                .take()
                .ok_or_else(|| AgentError::TurnNotActive {
                    session_id: self
                        .active_session
                        .as_deref()
                        .unwrap_or("unknown")
                        .to_string(),
                })?;

            let post = take_snapshot(self.config.repo_root(), self.config.ignore_patterns())?;

            let changes = diff_snapshots(&pre, &post);

            self.active_session = None;
            Ok(changes)
        })
    }

    fn cancel_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>> {
        Box::pin(async move {
            self.pre_snapshot = None;
            self.active_session = None;
            Ok(())
        })
    }

    fn is_active(&self) -> bool {
        self.pre_snapshot.is_some()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_config(dir: &TempDir) -> WatcherConfig {
        WatcherConfig::new(dir.path())
    }

    // Snapshot tests

    #[test]
    fn test_take_snapshot_empty_dir() {
        let dir = TempDir::new().unwrap();
        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn test_take_snapshot_with_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("b.rs"), "fn test() {}").unwrap();

        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains_key(Path::new("a.rs")));
        assert!(snapshot.contains_key(Path::new("b.rs")));
    }

    #[test]
    fn test_take_snapshot_nested_dirs() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src/auth")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("src/auth/login.rs"), "fn login() {}").unwrap();

        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains_key(Path::new("src/main.rs")));
        assert!(snapshot.contains_key(Path::new("src/auth/login.rs")));
    }

    #[test]
    fn test_take_snapshot_ignores_atomic_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".atomic/changes")).unwrap();
        fs::write(dir.path().join(".atomic/config.toml"), "data").unwrap();
        fs::write(dir.path().join("real_file.rs"), "code").unwrap();

        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.contains_key(Path::new("real_file.rs")));
    }

    #[test]
    fn test_take_snapshot_ignores_git_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "ref").unwrap();
        fs::write(dir.path().join("file.rs"), "code").unwrap();

        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.contains_key(Path::new("file.rs")));
    }

    #[test]
    fn test_take_snapshot_custom_ignore() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("build")).unwrap();
        fs::write(dir.path().join("build/output.o"), "binary").unwrap();
        fs::write(dir.path().join("src.rs"), "code").unwrap();

        let snapshot =
            take_snapshot(dir.path(), &[".atomic".to_string(), "build".to_string()]).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.contains_key(Path::new("src.rs")));
    }

    #[test]
    fn test_take_snapshot_tracks_mtime_and_size() {
        let dir = TempDir::new().unwrap();
        let content = "hello world";
        fs::write(dir.path().join("file.txt"), content).unwrap();

        let snapshot = take_snapshot(dir.path(), &[".atomic".to_string()]).unwrap();
        let entry = snapshot.get(Path::new("file.txt")).unwrap();
        assert_eq!(entry.size, content.len() as u64);
        assert!(entry.mtime > SystemTime::UNIX_EPOCH);
    }

    // Diff tests

    #[test]
    fn test_diff_no_changes() {
        let mut snapshot = HashMap::new();
        snapshot.insert(
            PathBuf::from("a.rs"),
            FileEntry {
                mtime: SystemTime::UNIX_EPOCH,
                size: 100,
            },
        );

        let changes = diff_snapshots(&snapshot, &snapshot);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_diff_added_file() {
        let before = HashMap::new();
        let mut after = HashMap::new();
        after.insert(
            PathBuf::from("new.rs"),
            FileEntry {
                mtime: SystemTime::now(),
                size: 50,
            },
        );

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.added, vec![PathBuf::from("new.rs")]);
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn test_diff_deleted_file() {
        let mut before = HashMap::new();
        before.insert(
            PathBuf::from("old.rs"),
            FileEntry {
                mtime: SystemTime::now(),
                size: 50,
            },
        );
        let after = HashMap::new();

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.deleted, vec![PathBuf::from("old.rs")]);
        assert!(changes.modified.is_empty());
        assert!(changes.added.is_empty());
    }

    #[test]
    fn test_diff_modified_file_mtime() {
        let t1 = SystemTime::UNIX_EPOCH;
        let t2 = SystemTime::now();

        let mut before = HashMap::new();
        before.insert(
            PathBuf::from("mod.rs"),
            FileEntry {
                mtime: t1,
                size: 100,
            },
        );

        let mut after = HashMap::new();
        after.insert(
            PathBuf::from("mod.rs"),
            FileEntry {
                mtime: t2,
                size: 100,
            },
        );

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.modified, vec![PathBuf::from("mod.rs")]);
    }

    #[test]
    fn test_diff_modified_file_size() {
        let t = SystemTime::now();

        let mut before = HashMap::new();
        before.insert(
            PathBuf::from("mod.rs"),
            FileEntry {
                mtime: t,
                size: 100,
            },
        );

        let mut after = HashMap::new();
        after.insert(
            PathBuf::from("mod.rs"),
            FileEntry {
                mtime: t,
                size: 200,
            },
        );

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.modified, vec![PathBuf::from("mod.rs")]);
    }

    #[test]
    fn test_diff_mixed_changes() {
        let t = SystemTime::UNIX_EPOCH;

        let mut before = HashMap::new();
        before.insert(
            PathBuf::from("unchanged.rs"),
            FileEntry {
                mtime: t,
                size: 100,
            },
        );
        before.insert(
            PathBuf::from("modified.rs"),
            FileEntry {
                mtime: t,
                size: 100,
            },
        );
        before.insert(
            PathBuf::from("deleted.rs"),
            FileEntry { mtime: t, size: 50 },
        );

        let mut after = HashMap::new();
        after.insert(
            PathBuf::from("unchanged.rs"),
            FileEntry {
                mtime: t,
                size: 100,
            },
        );
        after.insert(
            PathBuf::from("modified.rs"),
            FileEntry {
                mtime: t,
                size: 200,
            }, // size changed
        );
        after.insert(PathBuf::from("added.rs"), FileEntry { mtime: t, size: 75 });

        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.modified, vec![PathBuf::from("modified.rs")]);
        assert_eq!(changes.added, vec![PathBuf::from("added.rs")]);
        assert_eq!(changes.deleted, vec![PathBuf::from("deleted.rs")]);
        assert_eq!(changes.file_count(), 3);
    }

    #[test]
    fn test_diff_results_sorted() {
        let t = SystemTime::UNIX_EPOCH;
        let t2 = SystemTime::now();

        let mut before = HashMap::new();
        before.insert(PathBuf::from("z.rs"), FileEntry { mtime: t, size: 10 });
        before.insert(PathBuf::from("a.rs"), FileEntry { mtime: t, size: 10 });
        before.insert(PathBuf::from("m.rs"), FileEntry { mtime: t, size: 10 });

        let mut after = HashMap::new();
        after.insert(
            PathBuf::from("z.rs"),
            FileEntry {
                mtime: t2,
                size: 10,
            },
        );
        after.insert(
            PathBuf::from("a.rs"),
            FileEntry {
                mtime: t2,
                size: 10,
            },
        );
        after.insert(
            PathBuf::from("m.rs"),
            FileEntry {
                mtime: t2,
                size: 10,
            },
        );

        let changes = diff_snapshots(&before, &after);
        // Results should be sorted alphabetically
        assert_eq!(
            changes.modified,
            vec![
                PathBuf::from("a.rs"),
                PathBuf::from("m.rs"),
                PathBuf::from("z.rs"),
            ]
        );
    }

    #[test]
    fn test_diff_both_empty() {
        let changes = diff_snapshots(&HashMap::new(), &HashMap::new());
        assert!(changes.is_empty());
    }

    // FallbackWatcher lifecycle tests

    #[tokio::test]
    async fn test_watcher_not_active_initially() {
        let dir = TempDir::new().unwrap();
        let watcher = FallbackWatcher::new(make_config(&dir));
        assert!(!watcher.is_active());
    }

    #[tokio::test]
    async fn test_watcher_active_after_begin() {
        let dir = TempDir::new().unwrap();
        let mut watcher = FallbackWatcher::new(make_config(&dir));

        watcher.begin_turn("sess-1").await.unwrap();
        assert!(watcher.is_active());
    }

    #[tokio::test]
    async fn test_watcher_inactive_after_end() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.rs"), "code").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();
        let _changes = watcher.end_turn().await.unwrap();
        assert!(!watcher.is_active());
    }

    #[tokio::test]
    async fn test_watcher_inactive_after_cancel() {
        let dir = TempDir::new().unwrap();
        let mut watcher = FallbackWatcher::new(make_config(&dir));

        watcher.begin_turn("sess-1").await.unwrap();
        assert!(watcher.is_active());

        watcher.cancel_turn().await.unwrap();
        assert!(!watcher.is_active());
    }

    #[tokio::test]
    async fn test_watcher_end_without_begin_errors() {
        let dir = TempDir::new().unwrap();
        let mut watcher = FallbackWatcher::new(make_config(&dir));

        let result = watcher.end_turn().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::TurnNotActive { session_id } => {
                assert_eq!(session_id, "unknown");
            }
            other => panic!("Expected TurnNotActive, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_watcher_cancel_without_begin_is_ok() {
        let dir = TempDir::new().unwrap();
        let mut watcher = FallbackWatcher::new(make_config(&dir));

        // Cancel without begin should be a no-op, not an error
        let result = watcher.cancel_turn().await;
        assert!(result.is_ok());
    }

    // FallbackWatcher change detection tests

    #[tokio::test]
    async fn test_watcher_no_changes() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.rs"), "code").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // No modifications
        let changes = watcher.end_turn().await.unwrap();
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_detects_added_file() {
        let dir = TempDir::new().unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // Add a file during the turn
        fs::write(dir.path().join("new_file.rs"), "fn new() {}").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.added, vec![PathBuf::from("new_file.rs")]);
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_detects_deleted_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("to_delete.rs");
        fs::write(&file_path, "old code").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // Delete the file during the turn
        fs::remove_file(&file_path).unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.deleted, vec![PathBuf::from("to_delete.rs")]);
        assert!(changes.modified.is_empty());
        assert!(changes.added.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_detects_modified_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("modify_me.rs");
        fs::write(&file_path, "original content").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // Small sleep to ensure mtime changes (some filesystems have 1s granularity)
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Modify the file (change size to guarantee detection even with
        // coarse mtime granularity)
        fs::write(&file_path, "modified content that is longer").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.modified, vec![PathBuf::from("modify_me.rs")]);
    }

    #[tokio::test]
    async fn test_watcher_detects_mixed_changes() {
        let dir = TempDir::new().unwrap();

        // Pre-existing files
        fs::write(dir.path().join("keep.rs"), "keep this").unwrap();
        fs::write(dir.path().join("modify.rs"), "original").unwrap();
        fs::write(dir.path().join("delete.rs"), "will delete").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        // Mixed changes during the turn
        fs::write(
            dir.path().join("modify.rs"),
            "modified content that is different length",
        )
        .unwrap();
        fs::remove_file(dir.path().join("delete.rs")).unwrap();
        fs::write(dir.path().join("added.rs"), "new file").unwrap();

        let changes = watcher.end_turn().await.unwrap();

        assert_eq!(changes.modified, vec![PathBuf::from("modify.rs")]);
        assert_eq!(changes.added, vec![PathBuf::from("added.rs")]);
        assert_eq!(changes.deleted, vec![PathBuf::from("delete.rs")]);
        assert_eq!(changes.file_count(), 3);
    }

    #[tokio::test]
    async fn test_watcher_ignores_atomic_directory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // Write to .atomic/ — should be ignored
        fs::write(dir.path().join(".atomic/data.db"), "database stuff").unwrap();
        // Write to repo — should be detected
        fs::write(dir.path().join("real.rs"), "code").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.added, vec![PathBuf::from("real.rs")]);
        // .atomic/data.db should NOT appear
        assert_eq!(changes.file_count(), 1);
    }

    #[tokio::test]
    async fn test_watcher_nested_directory_changes() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src/auth")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));
        watcher.begin_turn("sess-1").await.unwrap();

        // Add a nested file
        fs::write(dir.path().join("src/auth/oauth.rs"), "fn oauth() {}").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.added, vec![PathBuf::from("src/auth/oauth.rs")]);
    }

    #[tokio::test]
    async fn test_watcher_multiple_turns() {
        let dir = TempDir::new().unwrap();

        let mut watcher = FallbackWatcher::new(make_config(&dir));

        // Turn 1: add a file
        watcher.begin_turn("sess-1").await.unwrap();
        fs::write(dir.path().join("turn1.rs"), "turn 1").unwrap();
        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.added, vec![PathBuf::from("turn1.rs")]);

        // Turn 2: add another file
        watcher.begin_turn("sess-1").await.unwrap();
        fs::write(dir.path().join("turn2.rs"), "turn 2").unwrap();
        let changes = watcher.end_turn().await.unwrap();
        // Should only see turn2.rs, not turn1.rs (which existed before this turn)
        assert_eq!(changes.added, vec![PathBuf::from("turn2.rs")]);
        assert!(changes.modified.is_empty());

        // Turn 3: modify turn1.rs
        watcher.begin_turn("sess-1").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(
            dir.path().join("turn1.rs"),
            "turn 1 modified with more content",
        )
        .unwrap();
        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.modified, vec![PathBuf::from("turn1.rs")]);
        assert!(changes.added.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_begin_turn_overwrites_previous() {
        let dir = TempDir::new().unwrap();
        let mut watcher = FallbackWatcher::new(make_config(&dir));

        // Begin a turn
        watcher.begin_turn("sess-1").await.unwrap();
        assert!(watcher.is_active());

        // Begin another turn without ending — should overwrite
        fs::write(dir.path().join("between.rs"), "between turns").unwrap();
        watcher.begin_turn("sess-1").await.unwrap();
        assert!(watcher.is_active());

        // End turn — should only see changes after the SECOND begin
        let changes = watcher.end_turn().await.unwrap();
        // between.rs was created before the second begin_turn snapshot,
        // so it should show as already existing (not added)
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn test_watcher_repo_root() {
        let dir = TempDir::new().unwrap();
        let watcher = FallbackWatcher::new(make_config(&dir));
        assert_eq!(watcher.repo_root(), dir.path());
    }

    #[tokio::test]
    async fn test_watcher_custom_ignore_patterns() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();

        let config = WatcherConfig::new(dir.path()).with_ignore_pattern("dist");
        let mut watcher = FallbackWatcher::new(config);

        watcher.begin_turn("sess-1").await.unwrap();

        fs::write(dir.path().join("dist/bundle.js"), "bundled code").unwrap();
        fs::write(dir.path().join("src.rs"), "source code").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        // dist/bundle.js should be ignored
        assert_eq!(changes.added, vec![PathBuf::from("src.rs")]);
        assert_eq!(changes.file_count(), 1);
    }

    // FileWatcher trait object test

    #[tokio::test]
    async fn test_watcher_as_trait_object() {
        let dir = TempDir::new().unwrap();
        let config = make_config(&dir);
        let mut watcher: Box<dyn FileWatcher> = Box::new(FallbackWatcher::new(config));

        assert!(!watcher.is_active());
        watcher.begin_turn("sess-1").await.unwrap();
        assert!(watcher.is_active());

        fs::write(dir.path().join("test.rs"), "test code").unwrap();

        let changes = watcher.end_turn().await.unwrap();
        assert_eq!(changes.file_count(), 1);
        assert!(!watcher.is_active());
    }
}
