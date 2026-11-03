//! The `remove` command for untracking files in the repository.
//!
//! This module implements the `atomic remove` command, which is Atomic's
//! counterpart to `git rm`: it stops tracking paths, and for content that is
//! already recorded it removes the path from the working copy so the deletion
//! can be recorded.
//!
//! # Usage
//!
//! ```text
//! atomic remove [OPTIONS] <PATHS>...
//!
//! Arguments:
//!   <PATHS>...  Paths to stop tracking
//!
//! Options:
//!       --delete        Also delete files from disk
//!       --keep          Never delete files from disk (git rm --cached)
//!   -r, --recursive     Recursively remove directory contents (default)
//!       --no-recursive  Don't recursively remove directory contents
//!   -n, --dry-run       Show what would be removed without doing it
//!   -f, --force         Ignore untracked paths, and discard uncommitted edits
//! ```
//!
//! # Behavior
//!
//! What `remove` may do depends on whether the path's content has been recorded,
//! because that decides who owns the path's name binding:
//!
//! | Path state | `atomic remove f` | with `--keep` | with `--delete` |
//! |---|---|---|---|
//! | `add`ed, never recorded | untracked; file stays on disk | untracked; file stays on disk | untracked; file deleted |
//! | recorded | file deleted; deletion pending until `atomic record` | untrack staged; bytes kept, `atomic record` completes it | same as the default |
//!
//! Untracking recorded content is a *change*: the name is durable in
//! `PATH_CLAIMS`, and the `INODES`/`REV_INODES` binding is derived from the
//! change that recorded it ([`Repository::remove`] reports such paths in
//! [`atomic_repository::TrackingStats::pending_deletions`] instead of editing
//! them). Deleting the file and recording the delete is the durable untrack
//! that mirrors `git rm`; `--keep` is the `git rm --cached` shape — it stages
//! the untrack as per-view intent that the next `atomic record` lands as the
//! `FileDel` while the bytes stay on disk (and then read as untracked).
//!
//! Mirroring `git rm`, deleting a recorded file that has uncommitted edits is
//! refused unless `--force` is given.
//!
//! # Examples
//!
//! ```text
//! # Untrack a file that was added but never recorded (keeps the file)
//! $ atomic remove draft_notes.txt
//!
//! # Stop tracking a recorded file: delete it and record the deletion
//! $ atomic remove Cargo.lock
//! $ atomic record -m "Stop tracking Cargo.lock"
//!
//! # Preview without touching anything
//! $ atomic remove --dry-run Cargo.lock
//! ```

use std::path::{Path, PathBuf};

use clap::Parser;

use atomic_repository::tracking::TrackingOptions;
use atomic_repository::{Repository, WorkspaceTxnMode};

use crate::commands::workspace_txn::enter_workspace;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Remove Command

/// Remove files from tracking, mirroring `git rm`.
///
/// A staged path (`add`ed, never recorded) is untracked and left on disk. A
/// recorded path cannot be untracked by editing tracking state — its name is
/// durable in `PATH_CLAIMS` and its binding derives from the change that
/// recorded it — so `remove` deletes it from the working copy and leaves the
/// deletion for `atomic record` to finalize.
///
/// # Behavior
///
/// - Staged paths: untracked; the file stays on disk unless `--delete`
/// - Recorded paths: deleted from the working copy; record to stop tracking
/// - Recorded paths with `--keep`: bytes kept; the untrack is staged and the
///   next record completes it (`git rm --cached`)
/// - Directories: handled recursively by default
/// - Untracked paths: error unless `--force` is used
///
/// # Options
///
/// - `--delete`: Also delete staged paths from disk
/// - `--keep`: Never delete files from disk (stages a recorded path's untrack)
/// - `--recursive` / `-r`: Recursively remove directory contents (default: true)
/// - `--no-recursive`: Don't recursively remove directory contents
/// - `--dry-run` / `-n`: Preview what would be removed
/// - `--force` / `-f`: Tolerate untracked paths, and discard uncommitted edits
#[derive(Parser, Debug, Clone)]
#[command(name = "remove")]
pub struct Remove {
    /// Files or directories to remove from tracking.
    ///
    /// Paths can be relative or absolute. Directories will be removed
    /// recursively by default.
    #[arg(value_name = "PATHS", required = true)]
    pub paths: Vec<String>,

    /// Also delete staged (never recorded) files from disk.
    ///
    /// Recorded files are deleted either way: removing them from the working
    /// copy is how their untracking becomes a recordable deletion.
    #[arg(long)]
    pub delete: bool,

    /// Never delete files from disk (`git rm --cached`).
    ///
    /// A staged path is untracked and stays on disk. A recorded path cannot be
    /// untracked by editing tracking state — its binding belongs to the change
    /// that recorded it — so `--keep` stages the untrack as pending intent on
    /// the current view: the next `atomic record` emits the deletion while the
    /// bytes stay on disk and then read as untracked.
    #[arg(long)]
    pub keep: bool,

    /// Recursively remove directory contents.
    ///
    /// When removing a directory, also remove all files within it.
    /// This is the default behavior.
    #[arg(short = 'r', long = "recursive", default_value = "true")]
    pub recursive: bool,

    /// Don't recursively remove directory contents.
    ///
    /// When set, only the specified path is removed, not its contents.
    #[arg(long = "no-recursive", conflicts_with = "recursive")]
    pub no_recursive: bool,

    /// Dry run - show what would be removed without doing it.
    ///
    /// When enabled, displays the files that would be removed but doesn't
    /// actually modify the repository or delete files.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Ignore untracked paths and discard uncommitted edits.
    ///
    /// Normally removing an untracked path is an error, and deleting a recorded
    /// path that has uncommitted edits is refused so the edits survive. This
    /// flag suppresses both, like `git rm --force`.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Remove {
    /// Create a new Remove command with default settings.
    pub fn new(paths: Vec<String>) -> Self {
        Self {
            paths,
            delete: false,
            keep: false,
            recursive: true,
            no_recursive: false,
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set the delete flag.
    pub fn with_delete(mut self, delete: bool) -> Self {
        self.delete = delete;
        self
    }

    /// Builder: set the keep flag (legacy, now the default).
    pub fn with_keep(mut self, keep: bool) -> Self {
        self.keep = keep;
        self
    }

    /// Builder: set the recursive flag.
    pub fn with_recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        if !recursive {
            self.no_recursive = true;
        }
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

    /// Convert to TrackingOptions.
    fn to_tracking_options(&self) -> TrackingOptions {
        let mut options = TrackingOptions::default()
            .with_recursive(self.recursive && !self.no_recursive)
            .with_force(self.force)
            .with_keep_recorded(self.keep);
        options.dry_run = self.dry_run;
        options
    }

    /// Delete a file from the filesystem.
    fn delete_file(&self, repo_root: &std::path::Path, path: &str) -> std::io::Result<()> {
        let full_path = repo_root.join(path);
        if full_path.is_dir() {
            std::fs::remove_dir_all(&full_path)
        } else if full_path.exists() {
            std::fs::remove_file(&full_path)
        } else {
            Ok(()) // Already doesn't exist
        }
    }

    /// Whether an untracked staged path should also leave the working copy.
    ///
    /// Recorded paths are deleted regardless of this: removing them from the
    /// working copy is what makes their untracking a recordable deletion.
    fn deletes_staged_paths(&self) -> bool {
        self.delete && !self.keep
    }
}

impl Default for Remove {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Command for Remove {
    /// Execute the remove command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository inside a workspace transaction
    /// 2. Classify every requested path read-only: a staged binding tracking may
    ///    delete, a recorded path that needs a delete change, or a refusal
    /// 3. Refuse the whole request before applying anything if any path cannot be
    ///    untracked this way, as `git rm` validates before it removes
    /// 4. Report the plan and stop for `--dry-run`
    /// 5. Untrack the staged bindings, then delete the recorded paths from the
    ///    working copy
    /// 6. Summarize, naming the `atomic record` that completes the untrack
    fn run(&self) -> CliResult<()> {
        // Route through the service when reachable: `Mode::Local` runs the same
        // handler in-process, and `Mode::Reactor` the serving daemon. The body
        // below runs only where no service can resolve the repository (a remote
        // sandbox, which executes over its local cache).
        if crate::commands::rpc::remove(self)? {
            return Ok(());
        }

        // Find repository
        let repo_root = find_repository_root()?;
        let mode = if self.dry_run {
            WorkspaceTxnMode::Observe
        } else {
            WorkspaceTxnMode::Reconcile
        };
        let mut repo = if self.dry_run {
            Repository::open_readonly(&repo_root)
        } else {
            Repository::open_for_workspace_transaction(&repo_root)
        }
        .map_err(CliError::Repository)?;
        let workspace = enter_workspace(&mut repo, mode)?;
        let working_copy = workspace.working_copy();

        let options = self.to_tracking_options();

        // Staged paths are untracked by `repo.remove`; recorded paths come back
        // in `pending_deletions` and need their working-copy bytes deleted plus a
        // recorded delete. `git rm` validates the whole request before applying
        // any of it, so the classification runs first, read-only, over every path:
        // a refusal never leaves the other paths half-untracked.
        let preview_options = TrackingOptions {
            dry_run: true,
            ..options.clone()
        };
        let mut targets: Vec<String> = Vec::new();
        let mut plan_untrack = 0usize;
        let mut plan_delete: Vec<PathBuf> = Vec::new();
        let mut plan_mark = 0usize;
        let mut dirty_guarded: Vec<String> = Vec::new();
        let mut total_errors = 0usize;

        for path in &self.paths {
            // Normalize path relative to repo root
            let normalized = if std::path::Path::new(path).is_absolute() {
                match std::path::Path::new(path).strip_prefix(&repo_root) {
                    Ok(rel) => rel.to_string_lossy().to_string(),
                    Err(_) => {
                        print_warning(&format!("Path outside repository: {}", path));
                        total_errors += 1;
                        continue;
                    }
                }
            } else {
                path.clone()
            };
            targets.push(normalized.clone());

            match repo.remove(working_copy, &normalized, preview_options.clone()) {
                Ok(stats) => {
                    plan_untrack += stats.files_removed;
                    if self.dry_run && stats.files_removed > 0 {
                        println!("Would untrack: {}", normalized);
                    }
                    for pending in stats.pending_untracks {
                        plan_mark += 1;
                        if self.dry_run {
                            println!(
                                "Would stage untrack of {}: bytes kept; 'atomic record' \
                                 completes it",
                                pending.to_string_lossy()
                            );
                        }
                    }
                    for pending in stats.pending_deletions {
                        let relative = pending.to_string_lossy().to_string();
                        let absolute = repo_root.join(&pending);
                        if absolute.exists()
                            && !self.force
                            && self.has_uncommitted_edits(&repo, working_copy, &pending)
                        {
                            if self.dry_run {
                                println!(
                                    "Would refuse {relative}: it has uncommitted edits \
                                     (--force to discard)"
                                );
                            }
                            dirty_guarded.push(relative);
                            continue;
                        }
                        plan_delete.push(pending);
                        if self.dry_run {
                            println!(
                                "Would delete recorded path: {relative} (then 'atomic record' it)"
                            );
                        }
                    }
                }
                Err(error) => {
                    print_warning(&format!("Failed to remove '{}': {}", normalized, error));
                    total_errors += 1;
                }
            }
        }

        // A refusal is reported either way; only a mutation stops before applying.
        if !self.dry_run && !dirty_guarded.is_empty() {
            return Err(CliError::InvalidArgument {
                message: refusal_message(&dirty_guarded),
            });
        }

        if self.dry_run {
            println!();
            print_hint(&format!(
                "Would remove {} file(s) from tracking",
                plan_untrack + plan_delete.len() + plan_mark
            ));
            if total_errors > 0 {
                print_warning(&format!(
                    "{} with errors",
                    format_count(total_errors, "path")
                ));
            }
            return Ok(());
        }

        // Apply the validated plan: untrack the staged bindings, then delete from
        // disk, so a mid-way failure never leaves a recorded file deleted while
        // still bound, or a binding retired with its bytes still on disk. Plan and
        // apply share one workspace transaction, so nothing can change between them.
        let mut untracked: Vec<String> = Vec::new();
        let mut marked = 0usize;
        let mut to_delete: Vec<PathBuf> = Vec::new();
        for target in &targets {
            match repo.remove(working_copy, target, options.clone()) {
                Ok(stats) => {
                    marked += stats.pending_untracks.len();
                    if stats.files_removed > 0 {
                        println!("Untracking: {}", target);
                        untracked.push(target.clone());
                        if self.deletes_staged_paths() {
                            to_delete.push(PathBuf::from(target));
                        }
                    }
                }
                Err(error) => {
                    print_warning(&format!("Failed to remove '{}': {}", target, error));
                    total_errors += 1;
                }
            }
        }
        to_delete.extend(plan_delete.iter().cloned());

        let mut failed_unlinks: Vec<PathBuf> = Vec::new();
        let mut failed_deletions: Vec<String> = Vec::new();
        for path in &to_delete {
            if let Err(e) = self.delete_file(&repo_root, &path.to_string_lossy()) {
                failed_unlinks.push(path.clone());
                failed_deletions.push(format!("{}: {}", path.display(), e));
            }
        }
        // The same wording the service handler's caller prints, so a failed
        // deletion reads identically whichever path ran the removal.
        print_failed_deletions(&failed_deletions);
        total_errors += failed_deletions.len();
        // A recorded path that was already absent from the working copy still
        // needed no unlink, so it counts: its delete is pending either way.
        let removed_recorded = plan_delete
            .iter()
            .filter(|path| !failed_unlinks.contains(path))
            .count();
        // Display summary. The wording matches the service handler exactly, so a
        // removal reports the same thing whichever path ran it.
        println!();
        if !untracked.is_empty() {
            print_success(&format!("Untracked {} file(s)", untracked.len()));
        }
        if removed_recorded > 0 {
            print_success(&format!(
                "Removed {} recorded file(s) from the working copy",
                removed_recorded
            ));
        }
        if marked > 0 {
            print_success(&format!(
                "Staged untrack of {} recorded file(s); bytes kept",
                marked
            ));
        }
        if untracked.is_empty() && removed_recorded == 0 && marked == 0 && total_errors == 0 {
            print_hint("Nothing to remove");
        }
        if total_errors > 0 {
            print_warning(&format!(
                "{} with errors",
                format_count(total_errors, "path")
            ));
        }

        // A recorded delete is the only durable untrack, so say what comes next.
        if removed_recorded > 0 || marked > 0 {
            println!();
            print_hint("Run 'atomic record' to record these deletions and stop tracking them");
        }

        // The same failure the service path reports, so a removal that could not
        // delete never reads as a success whichever path ran it.
        if !failed_deletions.is_empty() {
            return Err(failed_deletions_error(failed_deletions.len()));
        }

        Ok(())
    }
}

/// The shared refusal sentence for both removal paths, so a caller sees one
/// explanation whether the local body or the service handler rejected the
/// request. Only uncommitted edits can refuse a request now: `--keep` has a
/// real `git rm --cached` meaning and never needs guidance toward the
/// move-aside dance.
pub(crate) fn refusal_message(dirty_guarded: &[String]) -> String {
    format!(
        "recorded path(s) have uncommitted edits: {}; deleting them would discard \
         those edits (--force to discard, or --keep to untrack while keeping the \
         bytes)",
        dirty_guarded.join(", ")
    )
}

/// The paths whose bytes are still in the working copy because the deletion could
/// not be performed, shared by both removal paths.
///
/// A removal that could not delete reports that, and only that: counting such a
/// path as removed is what let an unlink failure read as a success.
pub(crate) fn print_failed_deletions(failures: &[String]) {
    for failure in failures {
        print_warning(&format!("Failed to delete {failure}"));
    }
    if !failures.is_empty() {
        print_warning(&format!(
            "{} with errors",
            format_count(failures.len(), "path")
        ));
    }
}

/// The sentence both removal paths end on when a deletion could not be
/// performed: the bytes survive, so the untrack is not complete.
pub(crate) fn failed_deletions_error(count: usize) -> CliError {
    CliError::ServiceRefusal {
        message: format!(
            "could not delete {} from the working copy; the bytes are still on \
             disk, so the untrack is not complete",
            format_count(count, "path")
        ),
    }
}

impl Remove {
    /// Whether deleting `path` would destroy uncommitted edits, mirroring the
    /// guard `git rm` applies without `-f`.
    ///
    /// Fails closed: if status cannot be computed the path counts as dirty, so a
    /// refusal never deletes bytes it could not verify.
    fn has_uncommitted_edits(
        &self,
        repo: &Repository,
        working_copy: atomic_core::WorkingCopyId,
        path: &Path,
    ) -> bool {
        repo.has_uncommitted_edits(working_copy, path)
            .unwrap_or(true)
    }
}

/// Format a count with singular/plural word.
pub(crate) fn format_count(count: usize, word: &str) -> String {
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
    fn test_remove_new() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        assert_eq!(cmd.paths, vec!["file.txt"]);
        assert!(!cmd.keep);
        assert!(cmd.recursive);
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_remove_default() {
        let cmd = Remove::default();
        assert!(cmd.paths.is_empty());
        assert!(!cmd.keep);
        assert!(cmd.recursive);
    }

    #[test]
    fn test_remove_with_keep() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_keep(true);
        assert!(cmd.keep);
    }

    #[test]
    fn test_remove_with_recursive() {
        let cmd = Remove::new(vec!["dir/".to_string()]).with_recursive(false);
        assert!(!cmd.recursive);
        assert!(cmd.no_recursive);
    }

    #[test]
    fn test_remove_with_dry_run() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_remove_with_force() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_remove_builder_chain() {
        let cmd = Remove::new(vec!["file.txt".to_string()])
            .with_keep(true)
            .with_dry_run(true)
            .with_force(true);

        assert!(cmd.keep);
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Options Conversion Tests

    #[test]
    fn test_to_tracking_options_default() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        let options = cmd.to_tracking_options();
        // TrackingOptions doesn't expose fields, so we just verify it doesn't panic
        let _ = options;
    }

    #[test]
    fn test_to_tracking_options_non_recursive() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_recursive(false);
        let options = cmd.to_tracking_options();
        let _ = options;
    }

    // Disk Deletion Policy Tests

    #[test]
    fn staged_paths_stay_on_disk_by_default() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        assert!(!cmd.deletes_staged_paths());
    }

    #[test]
    fn staged_paths_leave_disk_only_when_delete_is_explicit() {
        assert!(Remove::new(vec!["file.txt".to_string()])
            .with_delete(true)
            .deletes_staged_paths());
    }

    #[test]
    fn keep_overrides_delete_for_staged_paths() {
        assert!(!Remove::new(vec!["file.txt".to_string()])
            .with_delete(true)
            .with_keep(true)
            .deletes_staged_paths());
    }

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

    // Multiple Paths Tests

    #[test]
    fn test_remove_multiple_paths() {
        let cmd = Remove::new(vec![
            "file1.txt".to_string(),
            "file2.txt".to_string(),
            "dir/".to_string(),
        ]);
        assert_eq!(cmd.paths.len(), 3);
    }

    // Refusal Message Tests

    #[test]
    fn refusal_names_the_paths_with_uncommitted_edits_and_both_escapes() {
        let message = refusal_message(&["Cargo.lock".to_string()]);
        assert!(message.contains("uncommitted edits"), "{message}");
        assert!(message.contains("Cargo.lock"), "{message}");
        assert!(
            message.contains("--force") && message.contains("--keep"),
            "the refusal must say what to do instead: {message}"
        );
    }

    #[test]
    fn refusal_carries_every_refused_path_at_once() {
        let message = refusal_message(&["a.lock".to_string(), "b.lock".to_string()]);
        assert!(
            message.contains("a.lock") && message.contains("b.lock"),
            "{message}"
        );
        assert!(message.contains("uncommitted edits"), "{message}");
    }

    #[test]
    fn test_remove_clone() {
        let cmd = Remove::new(vec!["file.txt".to_string()])
            .with_keep(true)
            .with_force(true);
        let cloned = cmd.clone();
        assert_eq!(cloned.paths, cmd.paths);
        assert_eq!(cloned.keep, cmd.keep);
        assert_eq!(cloned.force, cmd.force);
    }
}
