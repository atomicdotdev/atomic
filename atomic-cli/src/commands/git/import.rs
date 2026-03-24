//! The `git import` command for importing Git repositories into Atomic.
//!
//! This module implements the conversion of Git commit history into Atomic changes,
//! preserving authorship, timestamps, commit messages, and file operations.
//!
//! # Design
//!
//! The import process:
//! 1. Opens the Git repository in the current directory
//! 2. Resolves the target branch (default or specified)
//! 3. Walks commit history in topological order (oldest first)
//! 4. For each commit, creates an Atomic change with:
//!    - Author from Git commit
//!    - Message from commit subject/body
//!    - Timestamp from commit time
//!    - File operations from tree diff
//!    - Git SHA stored in unhashed metadata
//! 5. Saves and applies each change to the stack
//!
//! # Limitations
//!
//! - Submodules are skipped with a warning
//! - Binary files are imported as-is
//! - Merge commits are linearized (first parent only)

use std::collections::HashSet;

use clap::Parser;
use git2::{Repository as GitRepository, Sort};

use atomic_repository::Repository;

use super::parallel::{ParallelImportOptions, ParallelImporter};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success};

/// Import a Git repository into Atomic.
///
/// Converts Git commit history into Atomic changes, preserving metadata
/// like author, timestamp, and commit message.
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "import")]
pub struct Import {
    /// Preview what would be imported without creating an Atomic repository.
    ///
    /// Shows the commits that would be imported but doesn't create any files
    /// or modify the repository.
    #[arg(long)]
    pub dry_run: bool,

    /// Import a specific branch instead of the default branch.
    ///
    /// By default, imports the currently checked-out branch.
    #[arg(long, short = 'b', value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Import all branches as separate stacks.
    ///
    /// Creates one Atomic stack for each Git branch found in the repository.
    #[arg(long)]
    pub all_branches: bool,

    /// Only import commits not already in Atomic.
    ///
    /// Useful for keeping an Atomic repository in sync with ongoing Git development.
    /// Compares Git commit SHAs with existing change metadata to skip already-imported commits.
    #[arg(long)]
    pub incremental: bool,

    /// Number of commits to process per batch.
    ///
    /// Larger batches use slightly more memory but reduce checkpoint overhead.
    /// The default works well for most repositories including very large ones.
    #[arg(long, default_value = "5000")]
    pub batch_size: usize,
}

impl Import {
    /// Import a single branch into an Atomic stack using parallel processing.
    fn import_branch(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
        repo: &mut Repository,
        imported_shas: &HashSet<String>,
        repo_root: &std::path::Path,
    ) -> CliResult<usize> {
        // Get repository name from remote URL or working directory
        let repo_name = self.get_repo_name(git_repo);

        // Create parallel importer with options
        let options = ParallelImportOptions {
            incremental: self.incremental,
            imported_shas: imported_shas.clone(),
            repo_name,
            batch_size: self.batch_size,
        };

        let importer = ParallelImporter::new(git_repo, options);

        // Run the batched parallel import
        let stats = importer.import_branch(branch_name, repo, repo_root)?;

        // Return total changes created (written + empty + merge)
        Ok(stats.changes_written + stats.empty_commits + stats.merge_commits)
    }

    /// Get repository name from remote URL or working directory.
    fn get_repo_name(&self, git_repo: &GitRepository) -> String {
        git_repo
            .find_remote("origin")
            .ok()
            .and_then(|remote| remote.url().map(|s| s.to_string()))
            .and_then(|url| {
                // Extract repo name from URL like "https://github.com/holman/spark.git"
                // or "git@github.com:holman/spark.git"
                url.trim_end_matches(".git")
                    .rsplit('/')
                    .next()
                    .or_else(|| url.rsplit(':').next().and_then(|s| s.rsplit('/').next()))
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                // Fall back to working directory name
                git_repo
                    .workdir()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Get the set of already imported Git SHAs from existing changes.
    fn get_imported_shas(&self, repo: &Repository) -> HashSet<String> {
        use atomic_repository::HistoryOptions;

        let mut shas = HashSet::new();

        // Iterate through all changes on the current stack via log
        let options = HistoryOptions::default();
        if let Ok(entries) = repo.log(options) {
            for entry in entries {
                if let Ok(change) = repo.load_change(&entry.hash) {
                    if let Some(ref unhashed) = change.unhashed {
                        if let Some(git) = unhashed.get("git") {
                            if let Some(sha) = git.get("sha").and_then(|v| v.as_str()) {
                                shas.insert(sha.to_string());
                            }
                        }
                    }
                }
            }
        }

        shas
    }

    /// Get all local branch names.
    fn get_all_branches(&self, git_repo: &GitRepository) -> CliResult<Vec<String>> {
        let branches = git_repo
            .branches(Some(git2::BranchType::Local))
            .map_err(|e| CliError::GitError {
                message: format!("Failed to list branches: {}", e),
            })?;

        let mut names = Vec::new();
        for branch_result in branches {
            let (branch, _) = branch_result.map_err(|e| CliError::GitError {
                message: format!("Failed to get branch: {}", e),
            })?;
            if let Some(name) = branch.name().ok().flatten() {
                names.push(name.to_string());
            }
        }

        Ok(names)
    }

    /// Get the default branch name.
    fn get_default_branch(&self, git_repo: &GitRepository) -> CliResult<String> {
        // Try HEAD first (current branch)
        if let Ok(head) = git_repo.head() {
            if head.is_branch() {
                if let Some(name) = head.shorthand() {
                    return Ok(name.to_string());
                }
            }
        }

        // Fall back to common default names
        for name in &["main", "master"] {
            if git_repo.find_branch(name, git2::BranchType::Local).is_ok() {
                return Ok(name.to_string());
            }
        }

        Err(CliError::GitError {
            message: "Could not determine default branch".to_string(),
        })
    }

    /// Count commits for dry-run mode.
    fn count_commits(
        &self,
        git_repo: &GitRepository,
        head_oid: git2::Oid,
        imported_shas: &HashSet<String>,
    ) -> CliResult<usize> {
        let mut revwalk = git_repo.revwalk().map_err(|e| CliError::GitError {
            message: format!("Failed to create revwalk: {}", e),
        })?;

        revwalk.push(head_oid).map_err(|e| CliError::GitError {
            message: format!("Failed to push HEAD to revwalk: {}", e),
        })?;

        revwalk
            .set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to set sorting: {}", e),
            })?;

        let mut count = 0;
        for oid_result in revwalk {
            let oid = oid_result.map_err(|e| CliError::GitError {
                message: format!("Revwalk error: {}", e),
            })?;

            if self.incremental && imported_shas.contains(&oid.to_string()) {
                continue;
            }

            count += 1;
        }

        Ok(count)
    }
}

impl Command for Import {
    fn run(&self) -> CliResult<()> {
        // Open Git repository
        let git_repo = GitRepository::discover(".").map_err(|_| CliError::GitError {
            message: "Not a git repository (or any parent up to mount point)".to_string(),
        })?;

        let workdir = git_repo.workdir().ok_or_else(|| CliError::GitError {
            message: "Git repository has no working directory (bare repository?)".to_string(),
        })?;

        // Dry run mode
        if self.dry_run {
            print_info("Dry run mode - no changes will be made");

            let default_branch = self.get_default_branch(&git_repo)?;
            let branches = if self.all_branches {
                self.get_all_branches(&git_repo)?
            } else {
                vec![self.branch.clone().unwrap_or(default_branch)]
            };

            for branch_name in &branches {
                if let Ok(reference) = git_repo.find_branch(branch_name, git2::BranchType::Local) {
                    if let Some(target) = reference.get().target() {
                        let count = self.count_commits(&git_repo, target, &HashSet::new())?;
                        print_info(&format!(
                            "Would import {} commits from branch '{}'",
                            count, branch_name
                        ));
                    }
                }
            }

            return Ok(());
        }

        // Check if Atomic repository exists, if not initialize it
        let repo_exists = find_repository_root().is_ok();
        let mut repo = if repo_exists {
            Repository::open(workdir).map_err(|e| CliError::Internal(e.into()))?
        } else {
            print_info("Initializing Atomic repository...");
            Repository::init(workdir).map_err(|e| CliError::Internal(e.into()))?
        };

        // Get already imported SHAs for incremental mode
        let imported_shas = if self.incremental {
            self.get_imported_shas(&repo)
        } else {
            HashSet::new()
        };

        // Determine which branches to import
        let default_branch = self.get_default_branch(&git_repo)?;

        if self.all_branches {
            // Import all branches
            let branches = self.get_all_branches(&git_repo)?;

            let mut total_imported = 0;
            for branch_name in branches {
                // Ensure the stack exists
                if !repo
                    .stack_exists(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?
                {
                    repo.create_stack(&branch_name)
                        .map_err(|e| CliError::Internal(e.into()))?;
                }

                // Switch to the stack
                repo.set_current_stack(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;

                // Import the branch
                let count =
                    self.import_branch(&git_repo, &branch_name, &mut repo, &imported_shas, workdir)?;
                total_imported += count;
            }

            print_success(&format!(
                "Imported {} total changes across all branches",
                total_imported
            ));
        } else {
            // Import single branch
            let branch_name = self.branch.clone().unwrap_or(default_branch);

            // Validate branch exists
            git_repo
                .find_branch(&branch_name, git2::BranchType::Local)
                .map_err(|_| CliError::GitError {
                    message: format!("Branch '{}' not found", branch_name),
                })?;

            // Ensure the stack exists with the branch name
            if !repo
                .stack_exists(&branch_name)
                .map_err(|e| CliError::Internal(e.into()))?
            {
                repo.create_stack(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;
            }

            // Switch to the stack
            repo.set_current_stack(&branch_name)
                .map_err(|e| CliError::Internal(e.into()))?;

            // Import
            let count = self.import_branch(&git_repo, &branch_name, &mut repo, &imported_shas, workdir)?;

            print_success(&format!(
                "Imported {} changes from branch '{}'",
                count, branch_name
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_import() {
        let import = Import::default();
        assert!(!import.dry_run);
        assert!(!import.all_branches);
        assert!(!import.incremental);
        assert!(import.branch.is_none());
        assert_eq!(import.batch_size, 0);
    }
}
