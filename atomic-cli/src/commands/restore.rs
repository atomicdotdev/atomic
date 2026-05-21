//! The `restore` command for restoring the working copy to pristine state.
//!
//! This module implements `atomic restore` (the working-copy counterpart of
//! `git restore`), which restores tracked files to match the pristine state
//! (the last recorded state in the current view). It discards unrecorded
//! edits to the named files.
//!
//! The legacy name `atomic reset` is kept as a hidden alias.
//!
//! # Usage
//!
//! ```text
//! atomic restore [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Optional files/directories to restore (default: all)
//!
//! Options:
//!   -n, --dry-run        Preview what would be restored without changes
//!   -f, --force          Force a whole-tree restore with uncommitted changes
//!   -h, --help           Print help information
//! ```
//!
//! # Behavior
//!
//! The `restore` command:
//! 1. Compares the working copy with the pristine state
//! 2. Restores files to match the last recorded state
//! 3. Discards any unrecorded modifications
//!
//! Switching views is a separate concern handled by `atomic view switch`.
//!
//! **Warning**: Restore discards uncommitted changes permanently.
//!
//! # Examples
//!
//! Restore specific files:
//! ```text
//! $ atomic restore src/main.rs
//! Restoring working copy...
//! ✓ Restored 1 file
//! ```
//!
//! Discard all uncommitted changes:
//! ```text
//! $ atomic restore --force
//! Restoring working copy...
//! ✓ Restored 3 files
//! ```
//!
//! Dry run (preview):
//! ```text
//! $ atomic restore --dry-run src/
//! Would restore: src/main.rs
//! Would restore: src/lib.rs
//! ```

use std::path::{Path, PathBuf};

use atomic_repository::tracking::TrackingOptions;
use atomic_repository::{FileStatus, Repository, RepositoryStatus, StatusOptions};
use clap::Parser;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Restore Command

/// The result of restoring a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreOutcome {
    /// Content was restored from the pristine state.
    Restored,
    /// An added-but-unrecorded file was untracked (kept on disk).
    Untracked,
    /// Nothing was done (no pristine content available).
    Skipped,
}

/// Restore the working copy to the last recorded state.
///
/// The `restore` command restores the working copy to match the pristine
/// state (the last recorded state in the current view). This discards any
/// uncommitted changes to the named files.
///
/// # Behavior
///
/// - Without arguments: restores the entire working copy (requires `--force`)
/// - With file arguments: restores only the specified files
///
/// # Warning
///
/// Restore is destructive - uncommitted changes cannot be recovered.
/// Use `--dry-run` to preview changes first.
#[derive(Parser, Debug, Clone)]
#[command(name = "restore")]
pub struct Restore {
    /// Files or directories to restore.
    ///
    /// If not specified, restores the entire working copy.
    #[arg(value_name = "FILES")]
    pub files: Vec<String>,

    /// Dry run - show what would be restored without doing it.
    ///
    /// For a single file, outputs the pristine content to stdout.
    /// For multiple files, lists what would be restored.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force a whole-tree restore even if there are uncommitted changes.
    ///
    /// Only required when no files are named. Naming files is itself explicit
    /// consent to discard their changes.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Restore {
    /// Create a new Restore command with default settings.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set files to restore.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(|s| s.into()).collect();
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

    /// Check if this is a partial restore (specific files, not the whole copy).
    pub fn is_partial(&self) -> bool {
        !self.files.is_empty()
    }

    /// Whether this invocation must be blocked until `--force` is supplied.
    ///
    /// Only a whole-working-copy restore (no paths named) is guarded, because
    /// it would discard *all* uncommitted work. Naming specific files is
    /// explicit consent (like `git restore <file>`), and `--force` /
    /// `--dry-run` always bypass the guard.
    fn requires_force(&self, has_changes: bool) -> bool {
        !self.is_partial() && has_changes && !self.force && !self.dry_run
    }

    /// Message shown when there is nothing to restore.
    ///
    /// For a partial restore we must not claim the whole working copy is clean
    /// (other paths may still be dirty) — only that the named paths had
    /// nothing to restore.
    fn nothing_to_restore_message(&self) -> &'static str {
        if self.is_partial() {
            "Nothing to restore for the specified path(s)"
        } else {
            "Nothing to restore - working copy is clean"
        }
    }

    /// Status options used by restore.
    ///
    /// Restore only ever touches *tracked* dirty files (Modified / Deleted /
    /// Added), so we skip the (potentially expensive) untracked-file scan.
    fn status_options() -> StatusOptions {
        StatusOptions {
            include_untracked: false,
            ..StatusOptions::default()
        }
    }

    /// Get the files that need to be restored, paired with their status.
    ///
    /// Derives the list from a status that was already computed by the
    /// caller (so restore doesn't walk the tree twice). Only tracked, dirty
    /// states are considered; untracked files are never touched.
    ///
    /// - `Modified` / `Deleted`: content will be restored from pristine.
    /// - `Added`: tracking will be undone (file kept on disk as untracked),
    ///   so that `status` stops reporting a pending "new file" change.
    fn files_to_restore(&self, status: &RepositoryStatus) -> Vec<(PathBuf, FileStatus)> {
        let mut out = Vec::new();

        for entry in status.entries() {
            let file_status = entry.status();
            match file_status {
                FileStatus::Modified | FileStatus::Deleted | FileStatus::Added => {
                    let path_str = entry.path().to_string_lossy();
                    if self.matches_filter(&path_str) {
                        out.push((entry.path().to_path_buf(), file_status));
                    }
                }
                _ => {}
            }
        }

        out
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

    /// Restore a single file according to its status.
    ///
    /// - `Added` (tracked but not recorded): undo tracking. The file stays
    ///   on disk and becomes untracked, so we never destroy content the user
    ///   created. This is what makes `restore <new-file>` honest about the
    ///   "new file" change reported by `status`.
    /// - Otherwise (`Modified` / `Deleted`): restore the file's content from
    ///   the pristine state.
    ///
    /// Returns the outcome so the caller can report it accurately.
    fn restore_file(
        &self,
        repo: &Repository,
        repo_root: &Path,
        path: &Path,
        status: FileStatus,
    ) -> CliResult<RestoreOutcome> {
        if status == FileStatus::Added {
            // Undo the `add`: stop tracking, but keep the file on disk.
            repo.remove(path, TrackingOptions::default().with_recursive(false))
                .map_err(CliError::Repository)?;
            return Ok(RestoreOutcome::Untracked);
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

                Ok(RestoreOutcome::Restored)
            }
            None => {
                // No pristine content available; nothing safe to do.
                Ok(RestoreOutcome::Skipped)
            }
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
}

impl Default for Restore {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Restore {
    /// Execute the restore command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Guard a whole-tree restore behind `--force`
    /// 3. Determine files to restore
    /// 4. If `--dry-run`, preview changes
    /// 5. Otherwise, restore files to pristine state
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Compute status once. Restore only touches tracked files, so we skip
        // the untracked scan, and we reuse this single status for both the
        // safety guard and the file list (no second tree walk).
        let status = repo
            .status(Self::status_options())
            .map_err(CliError::Repository)?;

        let has_changes = !status.is_clean();

        // Safety guard: only a whole-working-copy restore (no paths named)
        // requires --force. Naming specific files is explicit consent to
        // discard them, exactly like `git restore <file>`.
        if self.requires_force(has_changes) {
            return Err(CliError::RequiresForce {
                operation: "restore".to_string(),
            });
        }

        // Determine files to restore, paired with their status.
        let files_to_restore = self.files_to_restore(&status);

        // Handle dry-run for a single file by printing its pristine content to
        // stdout (useful for piping). This only makes sense when the argument
        // is exactly one concrete file:
        // - a directory or prefix filter (e.g. `src/`) has no pristine content
        //   of its own and must fall through to the listing branch;
        // - an Added file would be untracked, not restored, so it lists too.
        let single_added = files_to_restore
            .first()
            .map(|(_, s)| *s == FileStatus::Added)
            .unwrap_or(false);
        let single_file_arg = self.files.len() == 1
            && !self.files[0].ends_with('/')
            && !repo_root.join(&self.files[0]).is_dir();
        if self.dry_run && single_file_arg && !single_added {
            return self.dry_run_single_file(&repo, &self.files[0]);
        }

        // Dry run mode - just show what would happen
        if self.dry_run {
            if files_to_restore.is_empty() {
                println!("{}", self.nothing_to_restore_message());
            } else {
                for (path, file_status) in &files_to_restore {
                    if *file_status == FileStatus::Added {
                        println!("Would untrack: {} (kept on disk)", path.display());
                    } else {
                        println!("Would restore: {}", path.display());
                    }
                }
                println!();
                print_hint(&format!(
                    "(dry run - {} would be restored)",
                    format_count(files_to_restore.len(), "file")
                ));
            }
            return Ok(());
        }

        // Check if there's anything to restore
        if files_to_restore.is_empty() {
            println!("{}", self.nothing_to_restore_message());
            return Ok(());
        }

        // Perform restore
        println!("Restoring working copy...");

        let mut restored_count = 0;
        let mut error_count = 0;

        for (path, file_status) in &files_to_restore {
            let path_display = path.display();

            match self.restore_file(&repo, &repo_root, path, *file_status) {
                Ok(RestoreOutcome::Restored) => {
                    println!("  Restored: {}", path_display);
                    restored_count += 1;
                }
                Ok(RestoreOutcome::Untracked) => {
                    println!("  Untracked: {} (kept on disk)", path_display);
                    restored_count += 1;
                }
                Ok(RestoreOutcome::Skipped) => {
                    // No pristine content to restore; nothing to do.
                }
                Err(e) => {
                    print_warning(&format!("Failed to restore '{}': {}", path_display, e));
                    error_count += 1;
                }
            }
        }

        // Summary
        println!();
        if restored_count > 0 {
            print_success(&format!(
                "Restored {}",
                format_count(restored_count, "file")
            ));
        }

        if error_count > 0 {
            print_warning(&format!(
                "{} could not be restored",
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
    fn test_restore_new() {
        let cmd = Restore::new();
        assert!(cmd.files.is_empty());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_restore_default() {
        let cmd = Restore::default();
        assert!(cmd.files.is_empty());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_restore_with_files() {
        let cmd = Restore::new().with_files(vec!["file.txt".to_string(), "src/".to_string()]);
        assert_eq!(cmd.files.len(), 2);
        assert_eq!(cmd.files[0], "file.txt");
        assert_eq!(cmd.files[1], "src/");
    }

    #[test]
    fn test_restore_with_dry_run() {
        let cmd = Restore::new().with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_restore_with_force() {
        let cmd = Restore::new().with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_restore_builder_chain() {
        let cmd = Restore::new()
            .with_files(vec!["src/main.rs".to_string()])
            .with_dry_run(true)
            .with_force(true);

        assert_eq!(cmd.files, vec!["src/main.rs"]);
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Partial Restore Tests

    #[test]
    fn test_is_partial_empty() {
        let cmd = Restore::new();
        assert!(!cmd.is_partial());
    }

    #[test]
    fn test_is_partial_with_files() {
        let cmd = Restore::new().with_files(vec!["file.txt".to_string()]);
        assert!(cmd.is_partial());
    }

    // Filter Tests

    #[test]
    fn test_matches_filter_empty() {
        let cmd = Restore::new();
        assert!(cmd.matches_filter("any/path.rs"));
    }

    #[test]
    fn test_matches_filter_exact() {
        let cmd = Restore::new().with_files(vec!["src/main.rs".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("src/lib.rs"));
    }

    #[test]
    fn test_matches_filter_directory() {
        let cmd = Restore::new().with_files(vec!["src/".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("src/utils/helpers.rs"));
        assert!(!cmd.matches_filter("tests/test.rs"));
    }

    #[test]
    fn test_matches_filter_directory_without_slash() {
        let cmd = Restore::new().with_files(vec!["src".to_string()]);
        assert!(cmd.matches_filter("src"));
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("srcfile.rs"));
    }

    #[test]
    fn test_matches_filter_multiple() {
        let cmd = Restore::new().with_files(vec!["src/".to_string(), "README.md".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("README.md"));
        assert!(!cmd.matches_filter("Cargo.toml"));
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
    fn test_restore_clone() {
        let cmd = Restore::new()
            .with_files(vec!["file.txt".to_string()])
            .with_force(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.files, cmd.files);
        assert_eq!(cloned.force, cmd.force);
    }

    // Guard Logic Tests (requires_force)

    #[test]
    fn test_requires_force_partial_not_blocked() {
        // Naming a file is explicit consent — never blocked, even when the
        // tree has changes. This is the core bug fix.
        let cmd = Restore::new().with_files(vec!["file.txt".to_string()]);
        assert!(!cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_whole_tree_blocked() {
        let cmd = Restore::new();
        assert!(cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_whole_tree_with_force_passes() {
        let cmd = Restore::new().with_force(true);
        assert!(!cmd.requires_force(true));
    }

    #[test]
    fn test_requires_force_clean_tree_passes() {
        let cmd = Restore::new();
        assert!(!cmd.requires_force(false));
    }

    #[test]
    fn test_requires_force_dry_run_passes() {
        let cmd = Restore::new().with_dry_run(true);
        assert!(!cmd.requires_force(true));
    }

    // Empty-result Message Tests

    #[test]
    fn test_nothing_to_restore_message_partial() {
        let cmd = Restore::new().with_files(vec!["a.txt".to_string()]);
        assert_eq!(
            cmd.nothing_to_restore_message(),
            "Nothing to restore for the specified path(s)"
        );
    }

    #[test]
    fn test_nothing_to_restore_message_whole_tree() {
        let cmd = Restore::new();
        assert_eq!(
            cmd.nothing_to_restore_message(),
            "Nothing to restore - working copy is clean"
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
    fn test_restore_modified_restores_pristine_content() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("file.txt");
        fs::write(root.join(path), b"recorded\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        repo.record_all("init").unwrap();

        // Local edit that we want to discard.
        fs::write(root.join(path), b"local edit\n").unwrap();

        let cmd = Restore::new();
        let outcome = cmd
            .restore_file(&repo, &root, path, FileStatus::Modified)
            .unwrap();

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(fs::read(root.join(path)).unwrap(), b"recorded\n");
    }

    #[test]
    fn test_restore_deleted_restores_file_from_pristine() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("file.txt");
        fs::write(root.join(path), b"recorded\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        repo.record_all("init").unwrap();

        // Delete it on disk; restore should bring it back.
        fs::remove_file(root.join(path)).unwrap();
        assert!(!root.join(path).exists());

        let cmd = Restore::new();
        let outcome = cmd
            .restore_file(&repo, &root, path, FileStatus::Deleted)
            .unwrap();

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert!(root.join(path).exists());
        assert_eq!(fs::read(root.join(path)).unwrap(), b"recorded\n");
    }

    #[test]
    fn test_restore_added_untracks_but_keeps_file_on_disk() {
        let (_dir, repo, root) = test_repo();
        let path = Path::new("new.txt");
        fs::write(root.join(path), b"brand new\n").unwrap();
        repo.add(path, TrackingOptions::default()).unwrap();
        // Deliberately NOT recorded → status is `Added`.

        let cmd = Restore::new().with_files(vec!["new.txt".to_string()]);

        // Before: status reports it as a pending Added change.
        let before = repo.status(Restore::status_options()).unwrap();
        let listed = cmd.files_to_restore(&before);
        assert!(listed
            .iter()
            .any(|(p, s)| p.as_path() == path && *s == FileStatus::Added));

        let outcome = cmd
            .restore_file(&repo, &root, path, FileStatus::Added)
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::Untracked);

        // File is kept on disk (we never destroy user-created content)...
        assert!(root.join(path).exists());
        assert_eq!(fs::read(root.join(path)).unwrap(), b"brand new\n");

        // ...and status no longer reports a pending change for it, so the
        // status hint is no longer lying.
        let after = repo.status(Restore::status_options()).unwrap();
        let still_listed = cmd.files_to_restore(&after);
        assert!(!still_listed.iter().any(|(p, _)| p.as_path() == path));
    }
}
