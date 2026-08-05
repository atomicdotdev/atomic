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
//! 3. Walks first-parent commit history in topological order (oldest first)
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
//! - Default imports are mainline-only (first parent)
//! - Use `--all` to import all local branches as views
//! - Imports build only the graph layer by default. Git has no token-level
//!   data, so the semantic (Trunk → Branch → Leaf) layer for imported history
//!   is synthesized and derived on demand. Use `--with-crdt` to pre-materialize
//!   it for token-level blame and word-diff.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use clap::Parser;
use git2::{Repository as GitRepository, Sort};

use atomic_repository::Repository;

use super::parallel::{ParallelImportOptions, ParallelImporter};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_success, print_warning};

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

    /// Import all local branches as separate views.
    ///
    /// Creates one Atomic view for each Git branch found in the repository and
    /// imports the full reachable history for those branches. By default,
    /// `atomic git import` imports only the selected branch's mainline
    /// first-parent history.
    #[arg(long = "all", visible_alias = "all-branches")]
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

    /// Eagerly build the semantic (Trunk → Branch → Leaf) layer during import.
    ///
    /// By default, `git import` imports only the graph shape of each commit.
    /// Git stores no token-level (or even line-level) data — diffs and blame
    /// are computed, not stored — so the semantic layer for imported history
    /// is synthesized and fully derivable from the graph content plus commit
    /// metadata already written. Skipping it keeps imports smaller and faster
    /// while files and diffs still work.
    ///
    /// Use this flag to pre-materialize that synthesized layer up front (for
    /// token-level blame and word-diff on imported history) instead of
    /// deriving it on demand. Changes recorded after the import always build
    /// the semantic layer regardless of this flag.
    #[arg(long = "with-crdt")]
    pub with_crdt: bool,
}

fn import_ignore_patterns(workdir: &Path, kind: Option<&str>) -> Vec<String> {
    const COMMON_IMPORT_IGNORES: &[&str] = &[
        "node_modules/",
        "bower_components/",
        ".yarn/cache/",
        ".pnpm-store/",
    ];

    let template = if let Some(kind) = kind {
        super::super::init::get_ignore_template(kind)
    } else if workdir.join("Cargo.toml").exists() {
        super::super::init::get_ignore_template("rust")
    } else if workdir.join("package.json").exists() {
        super::super::init::get_ignore_template("node")
    } else if workdir.join("go.mod").exists() {
        super::super::init::get_ignore_template("go")
    } else if workdir.join("setup.py").exists() || workdir.join("pyproject.toml").exists() {
        super::super::init::get_ignore_template("python")
    } else {
        None
    };

    let mut patterns: Vec<String> = COMMON_IMPORT_IGNORES
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect();

    patterns.extend(
        template
            .unwrap_or(".atomic\n.git\n")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(ToOwned::to_owned),
    );
    patterns.sort();
    patterns.dedup();
    patterns
}

fn current_git_branch(git_repo: &GitRepository) -> Option<String> {
    let head = git_repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(ToOwned::to_owned)
}

const GIT_SHADOW_EXCLUDE_PATTERNS: &[&str] = &["/.atomic/", "/.vault/", "/.atomicignore"];

fn ensure_git_shadow_excludes(git_dir: &Path) -> CliResult<bool> {
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;

    let exclude_path = info_dir.join("exclude");
    let mut content = match std::fs::read_to_string(&exclude_path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let missing: Vec<&str> = GIT_SHADOW_EXCLUDE_PATTERNS
        .iter()
        .copied()
        .filter(|pattern| !content.lines().any(|line| line.trim() == *pattern))
        .collect();

    if missing.is_empty() {
        return Ok(false);
    }

    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str("# Atomic local state (managed by atomic git import)\n");
    for pattern in missing {
        content.push_str(pattern);
        content.push('\n');
    }

    std::fs::write(exclude_path, content)?;
    Ok(true)
}

impl Import {
    /// Import a single branch into an Atomic view using parallel processing.
    fn import_branch(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
        repo: &mut Repository,
        imported_shas: &HashSet<String>,
        known_states: &HashSet<atomic_core::types::Merkle>,
        mainline_only: bool,
        preserve_working_copy: bool,
    ) -> CliResult<usize> {
        // Get repository name from remote URL or working directory
        let repo_name = self.get_repo_name(git_repo);

        // Create parallel importer with options
        let options = ParallelImportOptions {
            incremental: self.incremental,
            imported_shas: imported_shas.clone(),
            repo_name,
            ignored_path_patterns: import_ignore_patterns(
                git_repo.workdir().unwrap_or_else(|| repo.root()),
                self.kind.as_deref(),
            ),
            mainline_only,
            graph_only: !self.with_crdt,
            preserve_working_copy,
            target_view: branch_name.to_string(),
            known_states: known_states.clone(),
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

    /// Get the set of already imported Git SHAs from existing changes,
    /// plus the Merkle states already present in the import target view.
    ///
    /// The states let the importer skip commits created by `atomic git
    /// push`: such a commit trailers the view state it represents, and if
    /// that state is already known the commit adds nothing.
    fn get_incremental_markers(
        &self,
        repo: &Repository,
        view_name: &str,
    ) -> CliResult<(HashSet<String>, HashSet<atomic_core::types::Merkle>)> {
        use atomic_repository::HistoryOptions;

        let mut shas = HashSet::new();
        let mut states = HashSet::new();
        let mut index_repairs = Vec::new();

        // Query the explicit target instead of the current view. Agent drafts
        // intentionally hide inherited changes from their default log, so
        // scanning the current draft makes already-imported parent commits look
        // new and duplicates them onto the Git branch view.
        let options = HistoryOptions::default()
            .view(view_name)
            .include_inherited(true);
        let entries = repo
            .log(options)
            .map_err(|error| CliError::Internal(error.into()))?;
        for entry in entries {
            states.insert(entry.state);
            let change = repo
                .load_change(&entry.hash)
                .map_err(|error| CliError::Internal(error.into()))?;
            if let Some(ref unhashed) = change.unhashed {
                if let Some(git) = unhashed.get("git") {
                    if let Some(sha) = git.get("sha").and_then(|v| v.as_str()) {
                        if shas.insert(sha.to_string()) {
                            index_repairs.push((sha.to_string(), entry.hash));
                        }
                    }
                }
            }
        }

        // Repair the acceleration index while scanning older repositories.
        // The explicit target-view history is authoritative; index repair is
        // best-effort and batched so a no-op sync never commits once per
        // historical Git change.
        if let Err(error) = repo.index_git_shas(&index_repairs) {
            log::warn!(
                "Failed to repair Git SHA index for view '{}': {}",
                view_name,
                error
            );
        }

        Ok((shas, states))
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
        if let Some(name) = current_git_branch(git_repo) {
            return Ok(name);
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
        mainline_only: bool,
    ) -> CliResult<usize> {
        let mut revwalk = git_repo.revwalk().map_err(|e| CliError::GitError {
            message: format!("Failed to create revwalk: {}", e),
        })?;

        revwalk.push(head_oid).map_err(|e| CliError::GitError {
            message: format!("Failed to push HEAD to revwalk: {}", e),
        })?;

        if mainline_only {
            revwalk
                .simplify_first_parent()
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to simplify revwalk to first-parent history: {}", e),
                })?;
        }

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
                        let count = self.count_commits(
                            &git_repo,
                            target,
                            &HashSet::new(),
                            !self.all_branches,
                        )?;
                        print_info(&format!(
                            "Would import {} commits from branch '{}'",
                            count, branch_name
                        ));
                    }
                }
            }

            return Ok(());
        }

        if ensure_git_shadow_excludes(git_repo.path())? {
            print_info("Configured Git to ignore Atomic local state.");
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

        let original_view = repo.current_view().to_string();
        let preserve_current_view = repo_exists && self.incremental;

        if self.with_crdt {
            print_info(
                "Building the semantic (Trunk → Branch → Leaf) layer during import (--with-crdt).",
            );
        } else {
            print_hint(
                "Importing graph only; the semantic layer is derived on demand. \
                 Pass --with-crdt to pre-materialize token-level blame/diff for imported history.",
            );
        }

        // Determine which branches to import
        let default_branch = self.get_default_branch(&git_repo)?;

        if self.all_branches {
            // Import all branches
            let branches = self.get_all_branches(&git_repo)?;

            let mut total_imported = 0;
            for branch_name in branches {
                let preserve_branch_working_copy =
                    preserve_current_view && original_view != branch_name;

                // Ensure the view exists
                if !repo
                    .view_exists(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?
                {
                    repo.create_shared_view(&branch_name)
                        .map_err(|e| CliError::Internal(e.into()))?;
                }

                let (imported_shas, known_states) = if self.incremental {
                    self.get_incremental_markers(&repo, &branch_name)?
                } else {
                    (HashSet::new(), HashSet::new())
                };

                // Existing incremental imports are background bookkeeping.
                // Select the target only on this handle so concurrent hooks
                // and crashes never observe a temporary global view pointer.
                if preserve_branch_working_copy {
                    repo.set_current_view_in_memory(&branch_name);
                } else {
                    repo.align_to_view(&branch_name)
                        .map_err(|e| CliError::Internal(e.into()))?;
                }

                // Import the branch. A foreign target is selected only on
                // this handle; restore the handle to the persisted working
                // copy view before processing the next branch (including on
                // error) so no later transition mistakes the scoped target
                // for the recovery source.
                let import_result = self.import_branch(
                    &git_repo,
                    &branch_name,
                    &mut repo,
                    &imported_shas,
                    &known_states,
                    false,
                    preserve_branch_working_copy,
                );
                if preserve_branch_working_copy {
                    repo.set_current_view_in_memory(&original_view);
                }
                let count = import_result?;
                total_imported += count;
            }

            if preserve_current_view {
                // Incremental Git shadow sync is background bookkeeping. Keep
                // both the user's Atomic view pointer and its working copy in
                // place while updating the requested Git branch views.
                repo.set_current_view_in_memory(&original_view);
                print_info(&format!(
                    "Preserved current Atomic view '{}'.",
                    original_view
                ));
            } else {
                // Materialize the working copy from the graph
                print_info("Materializing working copy...");
                match repo.materialize() {
                    Ok(result) => {
                        print_info(&format!("Materialized {} files", result.files_written))
                    }
                    Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
                }
                reindex_working_copy(&repo);
            }

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

            let (imported_shas, known_states) = if self.incremental {
                self.get_incremental_markers(&repo, &branch_name)?
            } else {
                (HashSet::new(), HashSet::new())
            };

            let restore_original_view = preserve_current_view && original_view != branch_name;

            if restore_original_view {
                repo.set_current_view_in_memory(&branch_name);
            } else {
                // User-facing/new imports still publish the selected branch;
                // materialization or reindexing below makes disk match it.
                repo.align_to_view(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;
            }

            // Import
            let count = self.import_branch(
                &git_repo,
                &branch_name,
                &mut repo,
                &imported_shas,
                &known_states,
                true,
                restore_original_view,
            )?;

            if restore_original_view {
                print_info(&format!(
                    "Preserving current Atomic view '{}'.",
                    original_view
                ));
            } else if current_git_branch(&git_repo).as_deref() == Some(branch_name.as_str()) {
                print_info("Using Git working copy as imported materialization.");
                reindex_working_copy(&repo);
            } else {
                // Importing a non-checked-out branch must update disk from Atomic.
                print_info("Materializing working copy...");
                match repo.materialize() {
                    Ok(result) => {
                        print_info(&format!("Materialized {} files", result.files_written))
                    }
                    Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
                }
                reindex_working_copy(&repo);
            }

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

            if restore_original_view {
                repo.set_current_view_in_memory(&original_view);
            }

            // Build the content search index (syntext)
            print_info("Building content search index...");
            match atomic_repository::build_content_index(workdir) {
                Ok(()) => print_info("Content index built."),
                Err(e) => log::warn!("Content index build failed: {}", e),
            }

            print_success(&format!(
                "Imported {} changes from branch '{}'",
                count, branch_name
            ));
        }

        Ok(())
    }
}

/// Rebuild FILE_INDEX from the current working copy.
///
/// During normal single-branch Git import the files on disk are already the
/// authoritative Git checkout for the imported branch, so there is no reason
/// to materialize the same content back out of Atomic. Indexing the tracked
/// files makes the post-import `atomic status` baseline clean.
fn reindex_working_copy(repo: &Repository) {
    use atomic_core::types::Hash;
    use std::time::SystemTime;

    let repo_root = repo.root().to_path_buf();
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

    #[test]
    fn test_all_flag_and_legacy_alias() {
        let import = Import::try_parse_from(["import", "--all"]).unwrap();
        assert!(import.all_branches);

        let import = Import::try_parse_from(["import", "--all-branches"]).unwrap();
        assert!(import.all_branches);
    }

    #[test]
    fn test_with_crdt_flag_defaults_to_graph_only() {
        // Default import is graph-only: the semantic layer is opt-in.
        let import = Import::try_parse_from(["import"]).unwrap();
        assert!(!import.with_crdt);

        let import = Import::try_parse_from(["import", "--with-crdt"]).unwrap();
        assert!(import.with_crdt);
    }

    #[test]
    fn test_import_ignore_patterns_always_exclude_dependency_dirs() {
        let patterns = import_ignore_patterns(Path::new("."), Some("go"));

        assert!(patterns.iter().any(|p| p == "node_modules/"));
        assert!(patterns.iter().any(|p| p == ".yarn/cache/"));
        assert!(patterns.iter().any(|p| p == "vendor/"));
    }

    #[test]
    fn test_git_shadow_excludes_are_added_to_git_info_exclude() {
        let temp = tempfile::tempdir().unwrap();
        let git_dir = temp.path().join(".git");

        assert!(ensure_git_shadow_excludes(&git_dir).unwrap());

        let exclude = std::fs::read_to_string(git_dir.join("info").join("exclude")).unwrap();
        assert!(exclude.contains("/.atomic/"));
        assert!(exclude.contains("/.vault/"));
        assert!(exclude.contains("/.atomicignore"));

        assert!(!ensure_git_shadow_excludes(&git_dir).unwrap());
    }
}
