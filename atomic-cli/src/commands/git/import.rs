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
//! 5. Saves and inserts each change into the view
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
use crate::output::{print_info, print_success, print_warning};

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

    /// Import all branches as separate views.
    ///
    /// Creates one Atomic view for each Git branch found in the repository.
    #[arg(long)]
    pub all_branches: bool,

    /// Only import commits not already in Atomic.
    ///
    /// Useful for keeping an Atomic repository in sync with ongoing Git development.
    /// Compares Git commit SHAs with existing change metadata to skip already-imported commits.
    #[arg(long)]
    pub incremental: bool,

    /// Project kind for .atomicignore template.
    ///
    /// Auto-detected from project files (Cargo.toml → rust, package.json → node, etc.)
    /// if not specified. Supported kinds: rust, python, node, javascript, typescript,
    /// go, java, kotlin, c, cpp.
    #[arg(long, short = 'k')]
    pub kind: Option<String>,

    /// Skip vault initialization.
    ///
    /// By default, git import creates a `.vault/` with skills, prompts, and memory.
    /// Use this flag to skip vault setup.
    #[arg(long)]
    pub no_vault: bool,
}

impl Import {
    /// Import a single branch into an Atomic view using parallel processing.
    fn import_branch(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
        repo: &mut Repository,
        imported_shas: &HashSet<String>,
    ) -> CliResult<usize> {
        // Get repository name from remote URL or working directory
        let repo_name = self.get_repo_name(git_repo);

        // Create parallel importer with options
        let options = ParallelImportOptions {
            incremental: self.incremental,
            imported_shas: imported_shas.clone(),
            repo_name,
        };

        let importer = ParallelImporter::new(git_repo, options);

        // Run the three-phase parallel import
        let stats = importer.import_branch(branch_name, repo)?;

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

        // Iterate through all changes on the current view via log
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

        // Check if Atomic repository exists in THIS directory (not parent dirs).
        // Don't use find_repository_root() — it walks up and might find
        // ~/.atomic/ (global config dir) which isn't a repo.
        let repo_exists = workdir.join(".atomic").join("pristine.redb").exists();
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
                // Ensure the view exists
                if !repo
                    .view_exists(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?
                {
                    repo.create_shared_view(&branch_name)
                        .map_err(|e| CliError::Internal(e.into()))?;
                }

                // Switch to the view
                repo.set_current_view(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;

                // Import the branch
                let count =
                    self.import_branch(&git_repo, &branch_name, &mut repo, &imported_shas)?;
                total_imported += count;
            }

            // Materialize the working copy from the graph
            print_info("Materializing working copy...");
            match repo.materialize() {
                Ok(result) => print_info(&format!("Materialized {} files", result.files_written)),
                Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
            }

            // Restore files from git to fix import fidelity issues.
            // The graph reconstruction may produce slightly wrong content
            // for some files (hunk misalignment across thousands of commits).
            // Git has the authoritative content — restore from it and update
            // the FILE_INDEX so atomic status sees them as clean.
            restore_from_git_and_reindex(&repo, &git_repo);

            // Initialize .atomicignore + vault AFTER import + materialize.
            // Must be before KG enrichment so has_vault() returns true.
            if !repo_exists {
                init_atomicignore_and_vault(
                    &mut repo,
                    workdir,
                    self.kind.as_deref(),
                    self.no_vault,
                )?;
            }

            // Auto-enrich the knowledge graph from all imported VCS data.
            // Runs AFTER vault init so the KG tables exist.
            if repo.has_vault().unwrap_or(false) {
                print_info("Enriching knowledge graph...");
                match repo.kg_enrich_from_vcs() {
                    Ok(stats) => print_info(&format!("KG enriched: {}", stats)),
                    Err(e) => log::warn!("KG enrichment failed: {}", e),
                }
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

            // Ensure the view exists with the branch name
            if !repo
                .view_exists(&branch_name)
                .map_err(|e| CliError::Internal(e.into()))?
            {
                repo.create_shared_view(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;
            }

            // Switch to the view
            repo.set_current_view(&branch_name)
                .map_err(|e| CliError::Internal(e.into()))?;

            // Import
            let count = self.import_branch(&git_repo, &branch_name, &mut repo, &imported_shas)?;

            // Materialize the working copy from the graph
            print_info("Materializing working copy...");
            match repo.materialize() {
                Ok(result) => print_info(&format!("Materialized {} files", result.files_written)),
                Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
            }

            // Restore files from git to fix import fidelity issues.
            restore_from_git_and_reindex(&repo, &git_repo);

            // Initialize .atomicignore + vault AFTER import + materialize
            if !repo_exists {
                init_atomicignore_and_vault(
                    &mut repo,
                    workdir,
                    self.kind.as_deref(),
                    self.no_vault,
                )?;
            }

            // Auto-enrich the knowledge graph from imported VCS data.
            // Runs AFTER vault init so the KG tables exist.
            if repo.has_vault().unwrap_or(false) {
                print_info("Enriching knowledge graph...");
                match repo.kg_enrich_from_vcs() {
                    Ok(stats) => print_info(&format!("KG enriched: {}", stats)),
                    Err(e) => log::warn!("KG enrichment failed: {}", e),
                }
            }

            print_success(&format!(
                "Imported {} changes from branch '{}'",
                count, branch_name
            ));
        }

        Ok(())
    }
}

/// Restore working copy files from git and rebuild FILE_INDEX.
///
/// After materialize, some files may have slightly wrong content due to
/// graph reconstruction fidelity issues. Git is the source of truth for
/// file content — restore from it, then update the FILE_INDEX so that
/// `atomic status` reports the working copy as clean.
fn restore_from_git_and_reindex(repo: &Repository, git_repo: &GitRepository) {
    use atomic_core::types::Hash;
    use std::time::SystemTime;

    let repo_root = repo.root().to_path_buf();

    // `git checkout -- .` restores all tracked files to HEAD state
    let result = std::process::Command::new("git")
        .args(["checkout", "--", "."])
        .current_dir(&repo_root)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            // Rebuild FILE_INDEX for all tracked files so status is clean.
            // Only index files that exist on disk and have graph content.
            // Files without graph content (tracked but not recorded) are
            // left alone — status will correctly show them as Added.
            let tracked = repo.list_tracked_files().unwrap_or_default();
            let mut entries: Vec<(String, i64, u32, u64, Hash)> = Vec::new();

            for file in &tracked {
                let abs = repo_root.join(&file.path);
                if let Ok(metadata) = std::fs::metadata(&abs) {
                    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let duration = mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default();
                    if let Ok(bytes) = std::fs::read(&abs) {
                        entries.push((
                            file.path.to_string_lossy().replace('\\', "/"),
                            duration.as_secs() as i64,
                            duration.subsec_nanos(),
                            metadata.len(),
                            Hash::of(&bytes),
                        ));
                    }
                }
            }

            if !entries.is_empty() {
                let _ = repo.update_file_index(&entries);
            }
        }
        _ => {
            log::warn!("git checkout failed — some files may show as modified in atomic status");
        }
    }
}

/// Create .atomicignore and initialize vault AFTER git import + materialize.
///
/// This runs post-import so the import's materialize step can't stomp the
/// vault files' tracked state. The sequence is:
/// 1. Create .atomicignore (auto-detect project type) → add → record
/// 2. Create .vault/ with defaults → add → record
/// 3. Status is clean.
fn init_atomicignore_and_vault(
    repo: &mut Repository,
    workdir: &std::path::Path,
    kind: Option<&str>,
    no_vault: bool,
) -> CliResult<()> {
    // Step 1: .atomicignore
    {
        let ignore_path = workdir.join(".atomicignore");
        if !ignore_path.exists() {
            // Use explicit --kind if provided, otherwise auto-detect from project files
            let ignore_content = if let Some(k) = kind {
                super::super::init::get_ignore_template(k).unwrap_or(".atomic\n.git\n")
            } else if workdir.join("Cargo.toml").exists() {
                super::super::init::get_ignore_template("rust").unwrap_or(".atomic\n.git\n")
            } else if workdir.join("package.json").exists() {
                super::super::init::get_ignore_template("node").unwrap_or(".atomic\n.git\n")
            } else if workdir.join("go.mod").exists() {
                super::super::init::get_ignore_template("go").unwrap_or(".atomic\n.git\n")
            } else if workdir.join("setup.py").exists() || workdir.join("pyproject.toml").exists() {
                super::super::init::get_ignore_template("python").unwrap_or(".atomic\n.git\n")
            } else {
                ".atomic\n.git\n"
            };
            let _ = std::fs::write(&ignore_path, ignore_content);
        }

        let _ = repo.add(
            ".atomicignore",
            atomic_repository::TrackingOptions::default(),
        );
        let header = atomic_core::change::ChangeHeader::new("Initialize repository");
        match repo.record(header, atomic_repository::RecordOptions::default()) {
            Ok(_) => print_info("Recorded .atomicignore"),
            Err(atomic_repository::RecordError::NothingToRecord) => {}
            Err(e) => log::warn!("Failed to record .atomicignore: {}", e),
        }
    }

    // Step 2: Vault (unless --no-vault)
    if no_vault {
        return Ok(());
    }

    match repo.init_vault() {
        Ok(()) => {
            print_info("Initialized vault at .vault/");

            // Add all vault files
            fn add_dir_recursive(repo: &Repository, dir: &std::path::Path) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            add_dir_recursive(repo, &path);
                        } else if path.is_file() {
                            if let Ok(rel) = path.strip_prefix(repo.root()) {
                                let rel_str = rel.to_string_lossy().replace('\\', "/");
                                let _ = repo
                                    .add(&rel_str, atomic_repository::TrackingOptions::default());
                            }
                        }
                    }
                }
            }
            let vault_dir = repo.vault_dir();
            if vault_dir.exists() {
                add_dir_recursive(repo, &vault_dir);
            }

            let header = atomic_core::change::ChangeHeader::new("Initialize vault");
            match repo.record(header, atomic_repository::RecordOptions::default()) {
                Ok(_) => print_info("Recorded vault defaults"),
                Err(atomic_repository::RecordError::NothingToRecord) => {}
                Err(e) => log::warn!("Failed to record vault files: {}", e),
            }
        }
        Err(e) => log::warn!("Vault initialization failed: {}", e),
    }

    Ok(())
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
    }
}
