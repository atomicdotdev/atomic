//! The `reset` command for restoring working copy to pristine state.
//!
//! This module implements the `atomic reset` command, which restores the working
//! copy to match the pristine state (last recorded state in the view).
//!
//! # Usage
//!
//! ```text
//! atomic reset [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Optional files/directories to reset (default: all)
//!
//! Options:
//!   -n, --dry-run        Preview what would be reset without changes
//!   -f, --force          Force reset even with uncommitted changes
//!   -h, --help           Print help information
//! ```
//!
//! # Behavior
//!
//! The `reset` command:
//! 1. Compares working copy with pristine state
//! 2. Restores files to match the last recorded state
//! 3. Discards any unrecorded modifications
//!
//! Switching views is a separate concern handled by `atomic view switch`.
//!
//! **Warning**: Reset discards uncommitted changes permanently.
//!
//! # Examples
//!
//! Discard all uncommitted changes:
//! ```text
//! $ atomic reset --force
//! Resetting working copy...
//! ✓ Reset 3 files
//! ```
//!
//! Reset specific files:
//! ```text
//! $ atomic reset src/main.rs
//! Resetting: src/main.rs
//! ✓ Reset 1 file
//! ```
//!
//! Dry run (preview):
//! ```text
//! $ atomic reset --dry-run src/
//! Would reset: src/main.rs
//! Would reset: src/lib.rs
//! (dry run - no changes made)
//! ```

use std::path::{Path, PathBuf};

use atomic_repository::tracking::TrackingOptions;
use atomic_repository::{FileStatus, Repository, RepositoryStatus, StatusOptions};
use clap::Parser;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Reset Command

/// The result of resetting a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetOutcome {
    /// Content was restored from the pristine state.
    Restored,
    /// An added-but-unrecorded file was untracked (kept on disk).
    Untracked,
    /// Nothing was done (no pristine content available).
    Skipped,
}

/// Reset the working copy to the last recorded state.
///
/// The `reset` command restores the working copy to match the pristine state
/// (the last recorded state in the view). This discards any uncommitted
/// changes.
///
/// # Behavior
///
/// - Without arguments: Resets entire working copy (requires `--force`)
/// - With file arguments: Resets only the specified files
///
/// # Warning
///
/// Reset is destructive - uncommitted changes cannot be recovered.
/// Use `--dry-run` to preview changes first.
#[derive(Parser, Debug, Clone)]
#[command(name = "reset")]
pub struct Reset {
    /// Files or directories to reset.
    ///
    /// If not specified, resets the entire working copy.
    #[arg(value_name = "FILES")]
    pub files: Vec<String>,

    /// Deprecated: switching views is no longer done via reset.
    ///
    /// Use `atomic view switch <view>` instead. Passing `--view` here returns
    /// an error pointing to the correct command. This flag is kept only to
    /// give a clear migration message and will be removed in a later release.
    #[arg(long, hide = true)]
    pub view: Option<String>,

    /// Dry run - show what would be reset without doing it.
    ///
    /// For a single file, outputs the pristine content to stdout.
    /// For multiple files, lists what would be reset.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force reset even if there are uncommitted changes.
    ///
    /// Without this flag, reset will warn and abort if there are
    /// uncommitted changes (safety measure).
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Reset {
    /// Create a new Reset command with default settings.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            view: None,
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set files to reset.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Builder: set the view to reset to.
    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Check if this is a partial reset (specific files, not whole working copy).
    pub fn is_partial_reset(&self) -> bool {
        !self.files.is_empty()
    }

    /// Whether this invocation must be blocked until `--force` is supplied.
    ///
    /// Only a whole-working-copy reset (no paths named) is guarded, because it
    /// would discard *all* uncommitted work. Naming specific files is explicit
    /// consent (like `git restore <file>`), and `--force` / `--dry-run` always
    /// bypass the guard.
    fn requires_force(&self, has_changes: bool) -> bool {
        !self.is_partial_reset() && has_changes && !self.force && !self.dry_run
    }

    /// Message shown when there is nothing to reset.
    ///
    /// For a partial reset we must not claim the whole working copy is clean
    /// (other paths may still be dirty) — only that the named paths had
    /// nothing to reset.
    fn nothing_to_reset_message(&self) -> &'static str {
        if self.is_partial_reset() {
            "Nothing to reset for the specified path(s)"
        } else {
            "Nothing to reset - working copy is clean"
        }
    }

    /// Normalize a path relative to the repository root.
    ///
    /// If the path is absolute and under the repo root, strips the prefix.
    /// Otherwise returns the path as-is.
    pub fn normalize_path(&self, repo_root: &std::path::Path, path: &str) -> CliResult<String> {
        let p = std::path::Path::new(path);
        // On Windows, Path::is_absolute() returns false for Unix-style "/foo"
        // paths (they're drive-relative). Treat a leading '/' as absolute on
        // all platforms so cross-platform behaviour is consistent.
        let looks_absolute = p.is_absolute() || path.starts_with('/');
        if looks_absolute {
            match p.strip_prefix(repo_root) {
                Ok(rel) => Ok(rel.to_string_lossy().to_string()),
                Err(_) => Err(CliError::PathOutsideRepository {
                    path: std::path::PathBuf::from(path),
                }),
            }
        } else {
            Ok(path.to_string())
        }
    }

    /// Check if a path matches any of the specified file filters.
    fn matches_filter(&self, path: &str) -> bool {
        if self.files.is_empty() {
            return true;
        }

        for filter in &self.files {
            let filter_normalized = filter.trim_end_matches('/');
            if path == filter_normalized
                || path.starts_with(&format!("{}/", filter_normalized))
                || filter_normalized.ends_with('/') && path.starts_with(filter_normalized)
            {
                return true;
            }
        }
        false
    }

    /// Status options used by reset.
    ///
    /// Reset only ever touches *tracked* dirty files (Modified / Deleted /
    /// Added), so we skip the (potentially expensive) untracked-file scan.
    fn status_options() -> StatusOptions {
        StatusOptions {
            include_untracked: false,
            ..StatusOptions::default()
        }
    }

    /// Get the files that need to be reset, paired with their status.
    ///
    /// Derives the list from a status that was already computed by the
    /// caller (so reset doesn't walk the tree twice). Only tracked, dirty
    /// states are considered; untracked files are never touched.
    ///
    /// - `Modified` / `Deleted`: content will be restored from pristine.
    /// - `Added`: tracking will be undone (file kept on disk as untracked),
    ///   so that `status` stops reporting a pending "new file" change.
    fn get_files_to_reset(&self, status: &RepositoryStatus) -> Vec<(PathBuf, FileStatus)> {
        let mut files_to_reset = Vec::new();

        for entry in status.entries() {
            let file_status = entry.status();
            match file_status {
                FileStatus::Modified | FileStatus::Deleted | FileStatus::Added => {
                    let path_str = entry.path().to_string_lossy();
                    if self.matches_filter(&path_str) {
                        files_to_reset.push((entry.path().to_path_buf(), file_status));
                    }
                }
                _ => {}
            }
        }

        files_to_reset
    }

    /// Execute dry-run for a single file - output pristine content to stdout.
    fn dry_run_single_file(&self, repo: &Repository, path: &str) -> CliResult<()> {
        // Get file content from pristine
        let content = repo.get_file_content(Path::new(path)).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to get file content: {}", e))
        })?;

        match content {
            Some(bytes) => {
                // Output to stdout
                use std::io::Write;
                std::io::stdout()
                    .write_all(&bytes)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to write: {}", e)))?;
                Ok(())
            }
            None => Err(CliError::FileNotFound {
                path: PathBuf::from(path),
            }),
        }
    }

    /// Reset a single file according to its status.
    ///
    /// - `Added` (tracked but not recorded): undo tracking. The file stays
    ///   on disk and becomes untracked, so we never destroy content the user
    ///   created. This is what makes `reset <new-file>` honest about the
    ///   "new file" change reported by `status`.
    /// - Otherwise (`Modified` / `Deleted`): restore the file's content from
    ///   the pristine state.
    ///
    /// Returns the outcome so the caller can report it accurately.
    fn reset_file(
        &self,
        repo: &Repository,
        repo_root: &Path,
        path: &Path,
        status: FileStatus,
    ) -> CliResult<ResetOutcome> {
        if status == FileStatus::Added {
            // Undo the `add`: stop tracking, but keep the file on disk.
            repo.remove(path, TrackingOptions::default().with_recursive(false))
                .map_err(CliError::Repository)?;
            return Ok(ResetOutcome::Untracked);
        }

        // Restore content from pristine.
        let content = repo.get_file_content(path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to get file content: {}", e))
        })?;

        let full_path = repo_root.join(path);

        match content {
            Some(bytes) => {
                // Ensure parent directory exists (a Deleted file may have had
                // its directory removed too).
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        CliError::Internal(anyhow::anyhow!("Failed to create directory: {}", e))
                    })?;
                }

                // Write content
                std::fs::write(&full_path, bytes).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to write file: {}", e))
                })?;

                Ok(ResetOutcome::Restored)
            }
            None => {
                // No pristine content available; nothing safe to do.
                Ok(ResetOutcome::Skipped)
            }
        }
    }
}

impl Default for Reset {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Reset {
    /// Execute the reset command.
    ///
    /// # Process
    ///
    /// 1. Reject `--view` (use `atomic view switch` instead)
    /// 2. Find and open the repository
    /// 3. Guard a whole-tree reset behind `--force`
    /// 4. Determine files to reset
    /// 5. If `--dry-run`, preview changes
    /// 6. Otherwise, reset files to pristine state
    fn run(&self) -> CliResult<()> {
        // `reset` is for discarding working-copy changes only. Switching views
        // is a separate concern owned by `atomic view switch`, which performs a
        // proper materialization. Reset must not reimplement that, so reject
        // `--view` with a clear migration message rather than half-doing it.
        if let Some(view_name) = &self.view {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "'atomic reset --view' is no longer supported. \
                     Use 'atomic view switch {view_name}' to switch views."
                ),
            });
        }

        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Compute status once. Reset only touches tracked files, so we skip
        // the untracked scan, and we reuse this single status for both the
        // safety guard and the file list (no second tree walk).
        let status = repo
            .status(Self::status_options())
            .map_err(CliError::Repository)?;

        let has_changes = !status.is_clean();

        // Safety guard: only a whole-working-copy reset (no paths named)
        // requires --force. Naming specific files is explicit consent to
        // discard them, exactly like `git restore <file>`.
        if self.requires_force(has_changes) {
            return Err(CliError::RequiresForce {
                operation: "reset".to_string(),
            });
        }

        // Determine files to reset, paired with their status.
        let files_to_reset = self.get_files_to_reset(&status);

        // Handle dry-run for a single file by printing its pristine content to
        // stdout (useful for piping). This only makes sense for files that
        // have pristine content; an Added file would be untracked, not
        // restored, so fall through to the listing branch below.
        let single_added = files_to_reset
            .first()
            .map(|(_, s)| *s == FileStatus::Added)
            .unwrap_or(false);
        if self.dry_run && self.files.len() == 1 && files_to_reset.len() <= 1 && !single_added {
            let path = &self.files[0];
            return self.dry_run_single_file(&repo, path);
        }

        // Dry run mode - just show what would happen
        if self.dry_run {
            if files_to_reset.is_empty() {
                println!("{}", self.nothing_to_reset_message());
            } else {
                for (path, file_status) in &files_to_reset {
                    if *file_status == FileStatus::Added {
                        println!("Would untrack: {} (kept on disk)", path.display());
                    } else {
                        println!("Would reset: {}", path.display());
                    }
                }
                println!();
                print_hint(&format!(
                    "(dry run - {} would be reset)",
                    format_count(files_to_reset.len(), "file")
                ));
            }
            return Ok(());
        }

        // Check if there's anything to reset
        if files_to_reset.is_empty() {
            println!("{}", self.nothing_to_reset_message());
            return Ok(());
        }

        // Perform reset
        println!("Resetting working copy...");

        let mut reset_count = 0;
        let mut error_count = 0;

        for (path, file_status) in &files_to_reset {
            let path_display = path.display();

            match self.reset_file(&repo, &repo_root, path, *file_status) {
                Ok(ResetOutcome::Restored) => {
                    println!("  Reset: {}", path_display);
                    reset_count += 1;
                }
                Ok(ResetOutcome::Untracked) => {
                    println!("  Untracked: {} (kept on disk)", path_display);
                    reset_count += 1;
                }
                Ok(ResetOutcome::Skipped) => {
                    // No pristine content to restore; nothing to do.
                }
                Err(e) => {
                    print_warning(&format!("Failed to reset '{}': {}", path_display, e));
                    error_count += 1;
                }
            }
        }

        // Summary
        println!();
        if reset_count > 0 {
            print_success(&format!("Reset {}", format_count(reset_count, "file")));
        }

        if error_count > 0 {
            print_warning(&format!(
                "{} could not be reset",
                format_count(error_count, "file")
            ));
        }

        Ok(())
    }
}

/// Format a count with singular/plural word.
fn format_count(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{} {}", count, word)
    } else {
        format!("{} {}s", count, word)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Builder Tests

    #[test]
    fn test_reset_new() {
        let cmd = Reset::new();
        assert!(cmd.files.is_empty());
        assert!(cmd.view.is_none());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_reset_default() {
        let cmd = Reset::default();
        assert!(cmd.files.is_empty());
        assert!(cmd.view.is_none());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_reset_with_files() {
        let cmd = Reset::new().with_files(vec!["file.txt".to_string(), "src/".to_string()]);
        assert_eq!(cmd.files.len(), 2);
        assert_eq!(cmd.files[0], "file.txt");
        assert_eq!(cmd.files[1], "src/");
    }

    #[test]
    fn test_reset_with_view() {
        let cmd = Reset::new().with_view("main");
        assert_eq!(cmd.view, Some("main".to_string()));
    }

    #[test]
    fn test_reset_with_dry_run() {
        let cmd = Reset::new().with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_reset_with_force() {
        let cmd = Reset::new().with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_reset_builder_chain() {
        let cmd = Reset::new()
            .with_files(vec!["src/main.rs".to_string()])
            .with_view("feature")
            .with_dry_run(true)
            .with_force(true);

        assert_eq!(cmd.files, vec!["src/main.rs"]);
        assert_eq!(cmd.view, Some("feature".to_string()));
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Partial Reset Tests

    #[test]
    fn test_is_partial_reset_empty() {
        let cmd = Reset::new();
        assert!(!cmd.is_partial_reset());
    }

    #[test]
    fn test_is_partial_reset_with_files() {
        let cmd = Reset::new().with_files(vec!["file.txt".to_string()]);
        assert!(cmd.is_partial_reset());
    }

    // Filter Tests

    #[test]
    fn test_matches_filter_empty() {
        let cmd = Reset::new();
        assert!(cmd.matches_filter("any/path.rs"));
    }

    #[test]
    fn test_matches_filter_exact() {
        let cmd = Reset::new().with_files(vec!["src/main.rs".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("src/lib.rs"));
    }

    #[test]
    fn test_matches_filter_directory() {
        let cmd = Reset::new().with_files(vec!["src/".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("src/utils/helpers.rs"));
        assert!(!cmd.matches_filter("tests/test.rs"));
    }

    #[test]
    fn test_matches_filter_directory_without_slash() {
        let cmd = Reset::new().with_files(vec!["src".to_string()]);
        assert!(cmd.matches_filter("src"));
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("srcfile.rs"));
    }

    #[test]
    fn test_matches_filter_multiple() {
        let cmd = Reset::new().with_files(vec!["src/".to_string(), "README.md".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("README.md"));
        assert!(!cmd.matches_filter("Cargo.toml"));
    }

    // Normalize Path Tests

    #[test]
    fn test_normalize_relative_path() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "src/file.rs").unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_inside_repo() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let abs = temp.path().join("src/file.rs");
        let result = cmd
            .normalize_path(temp.path(), abs.to_str().unwrap())
            .unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_outside_repo() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "/other/path/file.rs");
        assert!(result.is_err());
    }

    // Format Tests

    #[test]
    fn test_format_count_singular() {
        assert_eq!(format_count(1, "file"), "1 file");
    }

    #[test]
    fn test_format_count_plural() {
        assert_eq!(format_count(2, "file"), "2 files");
    }

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "file"), "0 files");
    }

    // Clone Tests

    #[test]
    fn test_reset_clone() {
        let cmd = Reset::new()
            .with_files(vec!["file.txt".to_string()])
            .with_view("main")
            .with_force(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.files, cmd.files);
        assert_eq!(cloned.view, cmd.view);
        assert_eq!(cloned.force, cmd.force);
    }

    // Guard Logic Tests (requires_force)

    #[test]
    fn test_requires_force_partial_reset_not_blocked() {
        // Naming a file is explicit consent — never blocked, even when the
        // tree has changes. This is the core bug fix.
        let cmd = Reset::new().with_files(vec!["file.txt".to_string()]);
        assert!(!cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_whole_tree_blocked() {
        let cmd = Reset::new();
        assert!(cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_whole_tree_with_force_passes() {
        let cmd = Reset::new().with_force(true);
        assert!(!cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_clean_tree_passes() {
        let cmd = Reset::new();
        assert!(!cmd.requires_force(false));
    }

    #[test]
    fn test_requires_force_dry_run_passes() {
        let cmd = Reset::new().with_dry_run(true);
        assert!(!cmd.requires_force(true));
    }

    #[test]
    fn test_reset_view_is_rejected_before_touching_repo() {
        // `--view` is rejected up front (before any filesystem access), with a
        // message pointing at `atomic view switch`.
        let cmd = Reset::new().with_view("feature");
        match cmd.run() {
            Err(CliError::InvalidArgument { message }) => {
                assert!(message.contains("view switch"));
                assert!(message.contains("feature"));
            }
            other => panic!("expected InvalidArgument for --view, got {other:?}"),
        }
    }

    // Empty-result Message Tests

    #[test]
    fn test_nothing_to_reset_message_partial() {
        let cmd = Reset::new().with_files(vec!["a.txt".to_string()]);
        assert_eq!(
            cmd.nothing_to_reset_message(),
            "Nothing to reset for the specified path(s)"
        );
    }

    #[test]
    fn test_nothing_to_reset_message_whole_tree() {
        let cmd = Reset::new();
        assert_eq!(
            cmd.nothing_to_reset_message(),
            "Nothing to reset - working copy is clean"
        );
    }

    // End-to-end Behavior Tests (real repository)

    use std::fs;
    use tempfile::TempDir;

    /// Initialize a repository in a temp dir. Returns the guard, repo, and root.
    fn test_repo() -> (TempDir, Repository, PathBuf) {
        let dir = TempDir::new().expect("create tempdir");
        let root = dir.path().to_path_buf();
        let repo = Repository::init(&root).expect("init repository");
        (dir, repo, root)
    }

    #[test]
    fn test_reset_modified_restores_pristine_content() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("file.txt");
        fs::write(root.join(path), b"recorded\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        repo.record_all("init").unwrap();

        // Local edit that we want to discard.
        fs::write(root.join(path), b"local edit\n").unwrap();

        let cmd = Reset::new();
        let outcome = cmd
            .reset_file(&repo, &root, path, FileStatus::Modified)
            .unwrap();

        assert_eq!(outcome, ResetOutcome::Restored);
        assert_eq!(fs::read(root.join(path)).unwrap(), b"recorded\n");
    }

    #[test]
    fn test_reset_deleted_restores_file_from_pristine() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("file.txt");
        fs::write(root.join(path), b"recorded\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        repo.record_all("init").unwrap();

        // Delete it on disk; reset should bring it back.
        fs::remove_file(root.join(path)).unwrap();
        assert!(!root.join(path).exists());

        let cmd = Reset::new();
        let outcome = cmd
            .reset_file(&repo, &root, path, FileStatus::Deleted)
            .unwrap();

        assert_eq!(outcome, ResetOutcome::Restored);
        assert!(root.join(path).exists());
        assert_eq!(fs::read(root.join(path)).unwrap(), b"recorded\n");
    }

    #[test]
    fn test_reset_added_untracks_but_keeps_file_on_disk() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("new.txt");
        fs::write(root.join(path), b"brand new\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        // Deliberately NOT recorded → status is `Added`.

        let cmd = Reset::new().with_files(vec!["new.txt".to_string()]);

        // Before: status reports it as a pending Added change.
        let before = repo.status(Reset::status_options()).unwrap();
        let listed = cmd.get_files_to_reset(&before);
        assert!(listed
            .iter()
            .any(|(p, s)| p.as_path() == path && *s == FileStatus::Added));

        let outcome = cmd
            .reset_file(&repo, &root, path, FileStatus::Added)
            .unwrap();
        assert_eq!(outcome, ResetOutcome::Untracked);

        // File is kept on disk (we never destroy user-created content)...
        assert!(root.join(path).exists());
        assert_eq!(fs::read(root.join(path)).unwrap(), b"brand new\n");

        // ...and status no longer reports a pending change for it, so the
        // status hint is no longer lying.
        let after = repo.status(Reset::status_options()).unwrap();
        let still_listed = cmd.get_files_to_reset(&after);
        assert!(!still_listed.iter().any(|(p, _)| p.as_path() == path));
    }
}
