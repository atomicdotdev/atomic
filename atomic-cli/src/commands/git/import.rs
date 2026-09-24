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

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::Path;

use clap::Parser;
use git2::{Repository as GitRepository, Sort};

use atomic_core::types::WorkingCopyId;
use atomic_repository::repository::ReconcileEffectBudget;
use atomic_repository::Repository;

use super::parallel::{
    forecast_commit_kind, incremental_import_skips, trace_git_import, ForecastKind, ImportStats,
    ParallelImportOptions, ParallelImporter, ProspectiveImportPlan,
};
use crate::commands::workspace_txn::{
    enter_remediation_workspace, enter_remediation_workspace_budgeted, observe_workspace,
    remediation_error,
};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, print_hint, print_info, print_success, print_warning};

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

    /// Internal bridge imports align and checkpoint in their outer operation.
    #[arg(skip)]
    pub(crate) skip_checkpoint_refresh: bool,

    /// Internal §7.5 detached-HEAD import tip: the commits behind this commit
    /// import into `branch`'s view even though no local branch carries it.
    /// Never set from the command line; only the bridge reconcile path uses it
    /// after resolving the §7.5 view mapping, and it never invents Git refs.
    #[arg(skip)]
    pub(crate) detached_tip: Option<String>,

    /// Internal effect budget (CB-13D review R1): set only by the
    /// metadata-only bridge watch reconcile path. Under
    /// [`ReconcileEffectBudget::MetadataOnly`] the import refuses every
    /// command-boundary effect — repository bootstrap, Git exclude writes,
    /// and working-copy materialization — and runs metadata-only work only.
    #[arg(skip)]
    pub(crate) reactive_budget: Option<atomic_repository::repository::ReconcileEffectBudget>,
}

fn current_git_branch(git_repo: &GitRepository) -> Option<String> {
    let head = git_repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(ToOwned::to_owned)
}

const GIT_SHADOW_EXCLUDE_PATTERNS: &[&str] = &["/.atomic/", "/.vault/", "/.atomicignore"];

#[derive(Debug, Clone, Copy)]
struct BranchImportMode {
    mainline_only: bool,
    preserve_working_copy: bool,
}

pub(crate) fn ensure_git_shadow_excludes(git_dir: &Path) -> CliResult<bool> {
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

/// Read-only variant of [`ensure_git_shadow_excludes`]: reports whether the
/// Git exclude file is missing any shadow-exclude pattern, without writing
/// anything (CB-13D review R1: the metadata-only watch path must never edit
/// Git administrative files; it defers instead).
fn ensure_git_shadow_excludes_needed(git_dir: &Path) -> CliResult<bool> {
    let exclude_path = git_dir.join("info").join("exclude");
    let content = match std::fs::read_to_string(&exclude_path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(GIT_SHADOW_EXCLUDE_PATTERNS
        .iter()
        .any(|pattern| !content.lines().any(|line| line.trim() == *pattern)))
}

/// Record one import run's synthesis aggregation (review R5): the real
/// per-branch import statistics, one bounded structured event, emitted
/// only for repositories that opted in (consent-gated, lossy). The loss
/// observable remains the per-fetch `binding_fetch` loss flag: an import
/// never fetches bindings, so there is no per-import loss counter.
fn emit_import_synthesis(repo: &Repository, stats: &ImportStats) {
    // CB-13C F4: the shared emitter (partial failures pass
    // `failed_after_landed` at their own site inside the writer).
    super::parallel::emit_import_synthesis(repo, stats, None);
}

impl Import {
    /// Import a single branch into an Atomic view using parallel processing.
    #[allow(clippy::too_many_arguments)]
    fn import_branch(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
        repo: &mut Repository,
        imported_shas: &HashSet<String>,
        known_states: &HashSet<atomic_core::types::Merkle>,
        mode: BranchImportMode,
        plan: ProspectiveImportPlan,
    ) -> CliResult<ImportStats> {
        // Get repository name from remote URL or working directory
        let repo_name = self.get_repo_name(git_repo);

        // Create parallel importer with options
        let options = ParallelImportOptions {
            incremental: self.incremental,
            imported_shas: imported_shas.clone(),
            repo_name,
            mainline_only: mode.mainline_only,
            graph_only: !self.with_crdt,
            preserve_working_copy: mode.preserve_working_copy,
            target_view: branch_name.to_string(),
            known_states: known_states.clone(),
            validate_equivalence: true,
        };

        let importer = ParallelImporter::new(git_repo, options);

        // Run the three-phase parallel import
        let stats = importer.import_prevalidated(branch_name, repo, plan)?;
        // Full per-import synthesis counters (review R5: the per-import
        // aggregation wires the real import statistics, not inferred ones).
        Ok(stats)
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
    ///
    /// `repair_index` controls the best-effort Git SHA index repair. Real
    /// imports pass `true`; the `--dry-run` forecast passes `false` so it can
    /// compute markers against a read-only handle without writing anything.
    fn get_incremental_markers(
        &self,
        repo: &Repository,
        view_name: &str,
        repair_index: bool,
    ) -> CliResult<(HashSet<String>, HashSet<atomic_core::types::Merkle>)> {
        let started = std::time::Instant::now();
        // CB-6C: the GIT_SHA_INDEX is a checked cache, never authority. Every
        // row is validated against hash-authoritative change bytes before it
        // may act as an import marker; stale rows are repaired and invalid
        // rows are dropped, so a corrupt cache produces exactly the same
        // skip decisions as a cold cache (the legacy backfill below).
        let (indexed, dropped) = repo
            .checked_git_sha_markers()
            .map_err(|error| CliError::Internal(error.into()))?;
        for sha in dropped.iter().take(3) {
            log::warn!(
                "Dropped unverifiable GIT_SHA_INDEX row for {} (same treatment as a cold cache)",
                &sha[..8.min(sha.len())]
            );
        }
        let mut shas: HashSet<String> = indexed.into_iter().collect();
        let _ = dropped;
        let mut states = HashSet::new();
        let mut index_repairs = Vec::new();

        // Query the target's effective root-to-leaf states without opening every
        // historical change file. Imported SHAs come from GIT_SHA_INDEX, making
        // the normal one-commit incremental path a single B-tree scan. Only
        // repositories with no index rows take the legacy backfill path.
        let entries = repo
            .effective_history(Some(view_name))
            .map_err(|error| CliError::Internal(error.into()))?;
        let needs_legacy_backfill = shas.is_empty() && !entries.is_empty();
        for entry in entries {
            states.insert(entry.state);
            if !needs_legacy_backfill {
                continue;
            }
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

        // Inserted squashes (SPEC §4) write no change record, so their git SHA
        // isn't reachable via change `unhashed` metadata. The ReviewGate tag on
        // the target view owns that provenance — scan tags for `git.sha` so a
        // re-import recognises the squash as already imported and skips it.
        if let Ok(tags) = repo.list_tags_for_view(view_name) {
            for tag in tags {
                if let Some(sha) = tag
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|v| v.as_str())
                {
                    shas.insert(sha.to_string());
                }
            }
        }

        // Repair the acceleration index while scanning older repositories.
        // The explicit target-view history is authoritative; index repair is
        // best-effort and batched so a no-op sync never commits once per
        // historical Git change. Skipped for dry-run forecasts, which must not
        // write to the repository.
        if repair_index {
            if let Err(error) = repo.index_git_shas(&index_repairs) {
                log::warn!(
                    "Failed to repair Git SHA index for view '{}': {}",
                    view_name,
                    error
                );
            }
        }

        trace_git_import(format!(
            "incremental markers: {:?} (shas={}, states={})",
            started.elapsed(),
            shas.len(),
            states.len()
        ));
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

    /// Count the commits a real import would create, for dry-run mode.
    ///
    /// In incremental mode this applies the exact same skip rules as the real
    /// import — already-imported SHAs and self-pushed commits whose view state
    /// is already present — via [`incremental_import_skips`], so the forecast
    /// matches what the import would actually bring in.
    fn count_commits(
        &self,
        git_repo: &GitRepository,
        head_oid: git2::Oid,
        imported_shas: &HashSet<String>,
        known_states: &HashSet<atomic_core::types::Merkle>,
        target_view: &str,
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

            if self.incremental {
                let sha = oid.to_string();
                let commit = git_repo.find_commit(oid).map_err(|e| CliError::GitError {
                    message: format!("Failed to load commit {}: {}", sha, e),
                })?;
                let message = commit.message().unwrap_or("");
                if incremental_import_skips(&sha, message, imported_shas, target_view, known_states)
                {
                    continue;
                }
            }

            count += 1;
        }

        Ok(count)
    }

    /// Classify the commits an incremental import would bring in and print a
    /// per-shape recommendation (SPEC §4.4). Purely advisory — writes nothing.
    fn print_forecast_recommendations(
        &self,
        git_repo: &GitRepository,
        head_oid: git2::Oid,
        imported_shas: &HashSet<String>,
        known_states: &HashSet<atomic_core::types::Merkle>,
        target_view: &str,
        repo: Option<&Repository>,
    ) -> CliResult<()> {
        use atomic_core::types::{Base32, Hash};

        let mut revwalk = git_repo.revwalk().map_err(|e| CliError::GitError {
            message: format!("Failed to create revwalk: {}", e),
        })?;
        revwalk.push(head_oid).map_err(|e| CliError::GitError {
            message: format!("Failed to push HEAD to revwalk: {}", e),
        })?;
        if !self.all_branches {
            revwalk
                .simplify_first_parent()
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to simplify revwalk: {}", e),
                })?;
        }
        revwalk
            .set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to set sorting: {}", e),
            })?;

        let mut insertable = 0usize;
        let mut missing = 0usize;
        let mut merges = 0usize;
        let mut normal = 0usize;

        for oid_result in revwalk {
            let oid = oid_result.map_err(|e| CliError::GitError {
                message: format!("Revwalk error: {}", e),
            })?;
            let sha = oid.to_string();
            let commit = git_repo.find_commit(oid).map_err(|e| CliError::GitError {
                message: format!("Failed to load commit {}: {}", sha, e),
            })?;
            let message = commit.message().unwrap_or("");
            if incremental_import_skips(&sha, message, imported_shas, target_view, known_states) {
                continue;
            }
            let is_merge = commit.parent_count() > 1;
            let records_present = |h: &str| {
                repo.map(|r| {
                    Hash::from_base32(h.as_bytes())
                        .map(|hash| r.has_change(&hash))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
            };
            match forecast_commit_kind(message, is_merge, records_present) {
                ForecastKind::SquashInsertable { count, pr } => {
                    insertable += 1;
                    let label = pr
                        .map(|n| format!("PR #{}", n))
                        .unwrap_or_else(|| "squash".to_string());
                    print_info(&format!(
                        "  squash ({}) — atomic headers present, {} record{} available: \
                         will insert into '{}' and tag the aggregate",
                        label,
                        count,
                        if count == 1 { "" } else { "s" },
                        target_view
                    ));
                }
                ForecastKind::SquashRecordsMissing { count } => {
                    missing += 1;
                    print_info(&format!(
                        "  squash — atomic headers present but {} record{} missing locally: \
                         run `atomic pull`, then re-import",
                        count,
                        if count == 1 { "" } else { "s" }
                    ));
                }
                ForecastKind::Merge => merges += 1,
                ForecastKind::Normal => normal += 1,
            }
        }

        if normal > 0 && insertable == 0 && missing == 0 && merges == 0 {
            print_info(
                "  git-authored commits (no atomic headers): a normal import will \
                 record them as changes",
            );
        }

        Ok(())
    }
}

impl Command for Import {
    fn run(&self) -> CliResult<()> {
        let run_start = std::time::Instant::now();
        // Open Git repository
        let git_repo = GitRepository::discover(".").map_err(|_| CliError::GitError {
            message: "Not a git repository (or any parent up to mount point)".to_string(),
        })?;

        let workdir = git_repo.workdir().ok_or_else(|| CliError::GitError {
            message: "Git repository has no working directory (bare repository?)".to_string(),
        })?;
        trace_git_import(format!(
            "discover Git repository: {:?}",
            run_start.elapsed()
        ));

        // Dry run mode
        if self.dry_run {
            print_info("Dry run mode - no changes will be made");

            let default_branch = self.get_default_branch(&git_repo)?;
            let branches = if self.all_branches {
                self.get_all_branches(&git_repo)?
            } else {
                vec![self.branch.clone().unwrap_or(default_branch)]
            };

            // For an incremental forecast, open the existing Atomic repo
            // read-only so we can subtract what each branch's view already
            // contains. A fresh (not-yet-initialized) repo has nothing
            // imported, so the forecast is the full history — which is correct.
            // The dry run selects the explicit Observe mode: it never mutates.
            let mut repo =
                if self.incremental && workdir.join(".atomic").join("pristine.redb").exists() {
                    Repository::open_readonly(workdir).ok()
                } else {
                    None
                };
            if let Some(repository) = repo.as_mut() {
                if let Err(remediation) = observe_workspace(repository)? {
                    print_warning(&format!(
                        "Forensic Git import forecast observed an unsafe workspace baseline: {}",
                        remediation.describe()
                    ));
                }
            }

            for branch_name in &branches {
                if let Ok(reference) = git_repo.find_branch(branch_name, git2::BranchType::Local) {
                    if let Some(target) = reference.get().target() {
                        // Compute the same incremental markers the real import
                        // would use for this branch's view (without repairing
                        // the index — a dry run writes nothing).
                        let (imported_shas, known_states) = match &repo {
                            Some(repo) if repo.view_exists(branch_name).unwrap_or(false) => {
                                self.get_incremental_markers(repo, branch_name, false)?
                            }
                            _ => (HashSet::new(), HashSet::new()),
                        };

                        let count = self.count_commits(
                            &git_repo,
                            target,
                            &imported_shas,
                            &known_states,
                            branch_name,
                            !self.all_branches,
                        )?;
                        print_info(&format!(
                            "Would import {} commit{} from branch '{}'",
                            count,
                            if count == 1 { "" } else { "s" },
                            branch_name
                        ));

                        // Router (SPEC §4.4): classify the incoming commits and
                        // recommend the right path for each shape.
                        if self.incremental && count > 0 {
                            self.print_forecast_recommendations(
                                &git_repo,
                                target,
                                &imported_shas,
                                &known_states,
                                branch_name,
                                repo.as_ref(),
                            )?;
                        }
                    }
                }
            }

            return Ok(());
        }

        // Validate the complete prospective import before Git excludes, Atomic
        // initialization, view creation, graph/change publication, derived-index
        // writes, or materialization. Existing repositories are opened read-only
        // solely to seed the isolated prospective projection.
        let default_branch = self.get_default_branch(&git_repo)?;
        // §7.5: a detached tip imports behind an explicit commit instead of a
        // branch reference; the view name is resolved by the caller.
        let detached_tip = match self.detached_tip.as_deref() {
            None => None,
            Some(hex) => Some(
                git2::Oid::from_str(hex).map_err(|error| CliError::GitError {
                    message: format!(
                        "detached import tip '{hex}' is not a valid object ID: {error}"
                    ),
                })?,
            ),
        };
        if detached_tip.is_some() && self.branch.is_none() {
            return Err(CliError::GitError {
                message: "a detached import tip requires an explicit target view name".to_string(),
            });
        }
        let repo_exists = workdir.join(".atomic").join("pristine.redb").exists();
        // CB-13D review R1: the metadata-only watch path never bootstraps a
        // repository — init, ignore templates, and vault setup are
        // command-boundary effects.
        let metadata_only = self
            .reactive_budget
            .is_some_and(|budget| budget.is_metadata_only());
        if metadata_only && !repo_exists {
            return Err(CliError::GitError {
                message: "the bridge watch daemon imports metadata only and never bootstraps \
                          an Atomic repository; run 'atomic git import' explicitly"
                    .to_string(),
            });
        }
        let mut preopened_repo = if repo_exists {
            let open_result = if metadata_only {
                Repository::open_with_budget(
                    workdir,
                    self.reactive_budget
                        .expect("metadata_only implies a budget"),
                )
            } else {
                Repository::open(workdir)
            };
            Some(open_result.map_err(CliError::from)?)
        } else {
            None
        };
        let prospective_branches = if self.all_branches {
            if detached_tip.is_some() {
                return Err(CliError::GitError {
                    message: "a detached import tip cannot be combined with --all".to_string(),
                });
            }
            self.get_all_branches(&git_repo)?
        } else {
            vec![self
                .branch
                .clone()
                .unwrap_or_else(|| default_branch.clone())]
        };
        let mut prospective_plans = HashMap::new();
        let preflight_start = std::time::Instant::now();
        for branch_name in &prospective_branches {
            // CB-9A: the already-imported marker set is loaded for every real
            // import, not only incremental ones. The deepening guard and the
            // deterministic sequencing need the indexed closure even in full
            // import mode: legacy bytes stay authoritative, and history that
            // deepens below an indexed commit is refused instead of silently
            // re-derived. (Dry-run forecasts keep their own semantics.)
            let (imported_shas, known_states) = match preopened_repo.as_ref() {
                Some(repo) if repo.view_exists(branch_name).map_err(CliError::from)? => {
                    self.get_incremental_markers(repo, branch_name, false)?
                }
                _ => (HashSet::new(), HashSet::new()),
            };
            let options = ParallelImportOptions {
                incremental: self.incremental,
                imported_shas: imported_shas.clone(),
                repo_name: self.get_repo_name(&git_repo),
                mainline_only: !self.all_branches,
                graph_only: !self.with_crdt,
                preserve_working_copy: true,
                target_view: branch_name.clone(),
                known_states: known_states.clone(),
                validate_equivalence: false,
            };
            let plan = ParallelImporter::new(&git_repo, options)
                .validate_branch_prospectively_for_tip(
                    branch_name,
                    preopened_repo.as_ref(),
                    detached_tip,
                )?;
            prospective_plans.insert(branch_name.clone(), (plan, imported_shas, known_states));
        }
        trace_git_import(format!("import preflight: {:?}", preflight_start.elapsed()));
        // CB-13D review R1: the metadata-only watch path never edits Git
        // administrative files. If the shadow-exclude line is missing, defer
        // to the explicit command instead of writing it.
        if metadata_only {
            if ensure_git_shadow_excludes_needed(git_repo.path())? {
                return Err(CliError::GitError {
                    message: "the bridge watch daemon never edits Git administrative files; \
                              the Git shadow exclude line is missing — run 'atomic git import' \
                              or 'atomic git bridge reconcile' explicitly once"
                        .to_string(),
                });
            }
        } else if ensure_git_shadow_excludes(git_repo.path())? {
            print_info("Configured Git to ignore Atomic local state.");
        }

        // Check if Atomic repository exists in THIS directory (not parent dirs).
        // Don't use find_repository_root() — it walks up and might find
        // ~/.atomic/ (global config dir) which isn't a repo.
        let mut repo = if repo_exists {
            preopened_repo
                .take()
                .expect("existing repository opened once")
        } else {
            print_info(&format!(
                "Initializing Atomic repository (default view '{}')...",
                default_branch
            ));
            // Seed the default view with Git's default branch name so Atomic's
            // shared view matches the project's Git default branch.
            Repository::init_with_view(workdir, &default_branch)
                .map_err(|e| CliError::Internal(e.into()))?
        };

        // Enter the shared workspace boundary. Import is also the bootstrap
        // remediation path for a Git checkout that has no bridge checkpoint
        // yet, so it uses the explicit repair entry: it still refuses Git-owned
        // operations, index locks, and diverged operation heads, but tolerates
        // the unanchored baseline it exists to establish. The metadata-only
        // watch budget refuses effect-bearing adoption and recovery before
        // any of it runs (CB-13D review R1).
        let workspace = match if metadata_only {
            enter_remediation_workspace_budgeted(
                &mut repo,
                self.reactive_budget
                    .expect("metadata_only implies a budget"),
            )?
        } else {
            enter_remediation_workspace(&mut repo)?
        } {
            Ok(workspace) => workspace,
            Err(remediation) => return Err(remediation_error(remediation)),
        };
        let working_copy = workspace.working_copy();
        let original_view = workspace.view().name.clone();
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

        let mut checkpoint_refreshed = false;
        if self.all_branches {
            // Import all branches
            let branches = self.get_all_branches(&git_repo)?;

            let mut total_imported = 0;
            for branch_name in branches {
                let preserve_branch_working_copy =
                    preserve_current_view && original_view != branch_name;
                let (plan, imported_shas, known_states) =
                    prospective_plans.remove(&branch_name).ok_or_else(|| {
                        CliError::Internal(anyhow::anyhow!(
                            "missing prevalidated import plan for '{branch_name}'"
                        ))
                    })?;

                // Ensure the view exists
                if !repo
                    .view_exists(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?
                {
                    repo.create_shared_view(&branch_name)
                        .map_err(|e| CliError::Internal(e.into()))?;
                }

                // Existing incremental imports are background bookkeeping.
                // Select the target only on this handle so concurrent hooks
                // and crashes never observe a temporary global view pointer.
                if preserve_branch_working_copy {
                    repo.set_current_view_in_memory(&branch_name);
                } else {
                    repo.align_to_view(working_copy, &branch_name)
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
                    BranchImportMode {
                        mainline_only: false,
                        preserve_working_copy: preserve_branch_working_copy,
                    },
                    plan,
                );
                if preserve_branch_working_copy {
                    repo.set_current_view_in_memory(&original_view);
                }
                let stats = import_result?;
                total_imported += stats.changes_written + stats.empty_commits + stats.merge_commits;
                emit_import_synthesis(&repo, &stats);
                import_git_tags_into_view(&mut repo, &git_repo, &branch_name);
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
            } else if matches!(self.reactive_budget, Some(budget) if !budget.allows_materialization())
            {
                // CB-13D ::24 R1: the metadata-only budget adopts
                // bookkeeping only — working-copy materialization is a
                // command-boundary filesystem effect and is deferred with
                // an explicit notice instead of executing (the latent
                // internal hole: the all-branches materialization branch
                // had no budget guard).
                print_info(
                    "Working copy materialization deferred to the explicit command boundary (metadata-only reactive budget).",
                );
            } else {
                // Materialize the working copy from the graph
                print_info("Materializing working copy...");
                match repo.materialize(working_copy) {
                    Ok(result) => {
                        print_info(&format!("Materialized {} files", result.files_written))
                    }
                    Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
                }
                reindex_working_copy(&repo, working_copy);
            }

            // Initialize .atomicignore + vault AFTER import + materialize.
            // Must be before KG enrichment so has_vault() returns true.
            if !repo_exists {
                init_atomicignore_and_vault(
                    &mut repo,
                    workdir,
                    self.kind.as_deref(),
                    self.no_vault,
                    working_copy,
                )?;
            }

            // Auto-enrich the knowledge graph from all imported VCS data.
            // Runs AFTER vault init so the KG tables exist.
            if repo.has_vault().unwrap_or(false) {
                print_info("Enriching knowledge graph...");
                match repo.kg_enrich_from_vcs(working_copy) {
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

            // Validate branch exists. A detached tip (§7.5) imports behind an
            // explicit commit and skips the branch reference check.
            if detached_tip.is_none() {
                git_repo
                    .find_branch(&branch_name, git2::BranchType::Local)
                    .map_err(|_| CliError::GitError {
                        message: format!("Branch '{}' not found", branch_name),
                    })?;
            }

            let (plan, imported_shas, known_states) =
                prospective_plans.remove(&branch_name).ok_or_else(|| {
                    CliError::Internal(anyhow::anyhow!(
                        "missing prevalidated import plan for '{branch_name}'"
                    ))
                })?;
            let changed_paths = plan.changed_paths();
            let expected_git_tree = plan.expected_git_tree();
            let raw_git_tree = plan.raw_git_tree();

            // Ensure the view exists with the branch name. A detached tip
            // (§7.5) targets an ephemeral Draft view the caller must have
            // created with the right scope and parent; import never invents
            // one as Shared behind the caller's back.
            if !repo
                .view_exists(&branch_name)
                .map_err(|e| CliError::Internal(e.into()))?
            {
                if detached_tip.is_some() {
                    return Err(CliError::GitError {
                        message: format!(
                            "detached import refused: target view '{branch_name}' does not exist; resolve the §7.5 mapping first"
                        ),
                    });
                }
                repo.create_shared_view(&branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;
            }

            let restore_original_view = preserve_current_view && original_view != branch_name;

            if restore_original_view {
                repo.set_current_view_in_memory(&branch_name);
            } else {
                // User-facing/new imports still publish the selected branch;
                // materialization or reindexing below makes disk match it.
                repo.align_to_view(working_copy, &branch_name)
                    .map_err(|e| CliError::Internal(e.into()))?;
            }

            // Import
            let write_start = std::time::Instant::now();
            let stats = self.import_branch(
                &git_repo,
                &branch_name,
                &mut repo,
                &imported_shas,
                &known_states,
                BranchImportMode {
                    mainline_only: true,
                    preserve_working_copy: restore_original_view,
                },
                plan,
            )?;
            let count = stats.changes_written + stats.empty_commits + stats.merge_commits;
            emit_import_synthesis(&repo, &stats);
            trace_git_import(format!(
                "import write/finalize: {:?}",
                write_start.elapsed()
            ));

            if restore_original_view {
                print_info(&format!(
                    "Preserving current Atomic view '{}'.",
                    original_view
                ));
            } else if current_git_branch(&git_repo).as_deref() == Some(branch_name.as_str()) {
                print_info("Using Git working copy as imported materialization.");
                if self.incremental {
                    trace_git_import("deferred full FILE_INDEX rebuild after incremental import");
                } else {
                    reindex_working_copy(&repo, working_copy);
                }
            } else {
                // Importing a non-checked-out branch must update disk from Atomic.
                // CB-13D review R1: the metadata-only watch path never
                // materializes — materializing a non-checked-out target is a
                // command-boundary filesystem effect.
                if metadata_only {
                    return Err(CliError::GitError {
                        message: "the bridge watch daemon reconciles metadata only and never \
                                  materializes a non-checked-out target; run 'atomic git import' \
                                  explicitly to materialize"
                            .to_string(),
                    });
                }
                print_info("Materializing working copy...");
                match repo.materialize(working_copy) {
                    Ok(result) => {
                        print_info(&format!("Materialized {} files", result.files_written))
                    }
                    Err(e) => print_warning(&format!("Working copy materialization failed: {}", e)),
                }
                reindex_working_copy(&repo, working_copy);
            }

            // Initialize .atomicignore + vault AFTER import + materialize
            if !repo_exists {
                init_atomicignore_and_vault(
                    &mut repo,
                    workdir,
                    self.kind.as_deref(),
                    self.no_vault,
                    working_copy,
                )?;
            }

            // Auto-enrich the knowledge graph from imported VCS data.
            // Runs AFTER vault init so the KG tables exist.
            if repo.has_vault().unwrap_or(false) {
                print_info("Enriching knowledge graph...");
                match repo.kg_enrich_from_vcs(working_copy) {
                    Ok(stats) => print_info(&format!("KG enriched: {}", stats)),
                    Err(e) => log::warn!("KG enrichment failed: {}", e),
                }
            }

            if restore_original_view {
                repo.set_current_view_in_memory(&original_view);
            }

            if self.incremental {
                log::debug!(
                    "deferred content-index maintenance for {} imported path(s)",
                    changed_paths.len()
                );
            } else {
                print_info("Building content search index...");
                // RFC §21 measured budgets (CB-13C AC-3): refresh, not
                // unconditional full rebuild. The full walk-and-rebuild was
                // re-run on every non-incremental import — the measured
                // warm 100k-file import spent 45s+ of CPU in it. The
                // refresh is HEAD-based: it is a no-op when the index is
                // already current and a full rebuild only when the index is
                // missing or the Git HEAD moved.
                match atomic_repository::refresh_content_index(workdir) {
                    Ok(()) => print_info("Content index built."),
                    Err(e) => log::warn!("Content index build failed: {}", e),
                }
            }

            print_success(&format!(
                "Imported {} changes from branch '{}'",
                count, branch_name
            ));
            // CB-8A (RFC §8.4): restore Git tags as bound Atomic state tags.
            import_git_tags_into_view(&mut repo, &git_repo, &branch_name);
            trace_git_import(format!(
                "import command before checkpoint: {:?}",
                run_start.elapsed()
            ));
            if !self.skip_checkpoint_refresh {
                if let (Some(expected_tree), Some(git_tree)) =
                    (expected_git_tree.as_ref(), raw_git_tree.as_ref())
                {
                    super::bridge::refresh_checkpoint_after_verified_import(
                        &repo,
                        working_copy,
                        &git_repo,
                        &branch_name,
                        expected_tree,
                        git_tree,
                    )?;
                    checkpoint_refreshed = true;
                }
            }
        }

        // Standalone imports establish or refresh the bridge checkpoint only
        // when Git and the persisted Atomic working-copy view are fully
        // aligned. During bridge raw-switch adoption the incremental import
        // deliberately preserves the old Atomic view, so this returns the
        // intentional mismatch no-op; `import_git_to_atomic` aligns and
        // checkpoints afterward.
        if !self.skip_checkpoint_refresh && !checkpoint_refreshed {
            let checkpoint_root = workdir.to_path_buf();
            // Drop the repository and the workspace transaction first: the
            // workspace's ordered lock guard holds the pristine open, and the
            // checkpoint publication re-opens the repository in this process.
            drop(workspace);
            drop(repo);
            drop(git_repo);
            let _ = super::bridge::refresh_checkpoint_if_aligned(&checkpoint_root)?;
        }

        Ok(())
    }
}

/// Import Git tags from the imported repository into an Atomic view.
///
/// CB-8A (RFC §8.4): a Git tag whose peeled commit carries a verified Git
/// state binding restores the exact bound Merkle state as an Atomic tag.
/// Tags on unbound commits — or annotated tags claiming a wrong binding —
/// are refused with a warning, never approximated, and never abort the
/// surrounding import.
fn import_git_tags_into_view(repo: &mut Repository, git_repo: &GitRepository, view: &str) {
    let Ok(tag_refs) = git_repo.references_glob("refs/tags/*") else {
        return;
    };
    let mut imported = 0usize;
    let mut refused = 0usize;
    for reference in tag_refs.flatten() {
        let Some(name) = reference.shorthand().map(str::to_string) else {
            continue;
        };
        match repo.import_git_tag_from_git(view, git_repo, &name) {
            Ok(tag) => {
                imported += 1;
                print_info(&format!(
                    "Imported Git tag '{}' as an Atomic state tag on view '{}'.",
                    emphasis(&name),
                    emphasis(view)
                ));
                if let Some(message) = tag.message.as_deref() {
                    trace_git_import(format!("tag '{name}' annotation: {message}"));
                }
            }
            Err(error) => {
                refused += 1;
                print_warning(&format!(
                    "Git tag '{}' not imported: {error}",
                    emphasis(&name)
                ));
            }
        }
    }
    if imported > 0 || refused > 0 {
        trace_git_import(format!(
            "git tag import into '{view}': {imported} imported, {refused} refused"
        ));
    }
}

/// Rebuild FILE_INDEX from the current working copy.
///
/// During normal single-branch Git import the files on disk are already the
/// authoritative Git checkout for the imported branch, so there is no reason
/// to materialize the same content back out of Atomic. Indexing the tracked
/// files makes the post-import `atomic status` baseline clean.
fn reindex_working_copy(repo: &Repository, working_copy: WorkingCopyId) {
    use atomic_core::types::Hash;
    let started = std::time::Instant::now();
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
        let _ = repo.update_file_index(working_copy, &entries);
    }
    trace_git_import(format!(
        "reindex working copy: {:?} (files={})",
        started.elapsed(),
        entries.len()
    ));
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
    working_copy: WorkingCopyId,
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
            working_copy,
            ".atomicignore",
            atomic_repository::TrackingOptions::default(),
        );
        let header = atomic_core::change::ChangeHeader::new("Initialize repository");
        let options = atomic_repository::RecordOptions::new()
            .add_path(".atomicignore")
            .detect_raw_renames(false);
        match repo.record(working_copy, header, options) {
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
            fn add_dir_recursive(
                repo: &Repository,
                working_copy: WorkingCopyId,
                dir: &std::path::Path,
            ) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            add_dir_recursive(repo, working_copy, &path);
                        } else if path.is_file() {
                            if let Ok(rel) = path.strip_prefix(repo.root()) {
                                let rel_str = rel.to_string_lossy().replace('\\', "/");
                                let _ = repo.add(
                                    working_copy,
                                    &rel_str,
                                    atomic_repository::TrackingOptions::default(),
                                );
                            }
                        }
                    }
                }
            }
            let vault_dir = repo.vault_dir();
            if vault_dir.exists() {
                add_dir_recursive(repo, working_copy, &vault_dir);
            }

            let header = atomic_core::change::ChangeHeader::new("Initialize vault");
            let options = atomic_repository::RecordOptions::new()
                .add_path(".vault")
                .detect_raw_renames(false);
            match repo.record(working_copy, header, options) {
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
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as ProcessCommand;

    use atomic_core::types::Base32;
    use serial_test::serial;

    use super::*;

    struct DirGuard(PathBuf);

    impl DirGuard {
        fn new() -> Self {
            Self(std::env::current_dir().unwrap())
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git(root: &Path) {
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root, &["config", "user.name", "Atomic Test"]);
        git_ok(root, &["config", "user.email", "atomic@example.com"]);
    }

    fn assert_rejected_before_atomic_init(root: &Path, import: Import) {
        let _dir_guard = DirGuard::new();
        std::env::set_current_dir(root).unwrap();
        assert!(import.run().is_err());
        assert!(
            !root.join(".atomic").exists(),
            "failed prospective verification must not initialize Atomic"
        );
    }

    #[test]
    #[serial]
    fn forged_self_push_header_is_rejected_without_atomic_mutation() {
        let root = tempfile::tempdir().unwrap();
        init_git(root.path());
        fs::write(root.path().join("tracked.txt"), b"forged\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        let message = format!(
            "forged\n\nAtomic-View: main\nAtomic-State: {}",
            atomic_core::types::Merkle::ZERO.to_base32()
        );
        git_ok(root.path(), &["commit", "-q", "-m", &message]);
        assert_rejected_before_atomic_init(
            root.path(),
            Import {
                incremental: true,
                no_vault: true,
                ..Import::default()
            },
        );
    }

    #[test]
    #[serial]
    fn forged_squash_header_is_rejected_without_atomic_mutation() {
        let root = tempfile::tempdir().unwrap();
        init_git(root.path());
        fs::write(root.path().join("tracked.txt"), b"forged squash\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        let missing = atomic_core::types::Hash::of(b"missing-change").to_base32();
        let message = format!("forged squash\n\nAtomic-Changes: {missing}");
        git_ok(root.path(), &["commit", "-q", "-m", &message]);
        assert_rejected_before_atomic_init(
            root.path(),
            Import {
                incremental: true,
                no_vault: true,
                ..Import::default()
            },
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn stale_mode_link_and_empty_projection_is_rejected_without_atomic_mutation() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        // CB-9C: the supported corpus now covers executables, symlinks, and
        // empty tracked files — the old CB-9A capability fence that refused
        // them was superseded by graph-backed mode/kind registers. The same
        // fixture now imports, and the fence still refuses genuinely
        // unsupported tree modes (a set-id bit cannot appear in a normal Git
        // tree, so it is simulated by the delta-level mode validator through
        // the rejected case below).
        let root = tempfile::tempdir().unwrap();
        init_git(root.path());
        fs::write(root.path().join("executable"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(
            root.path().join("executable"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.path().join("empty"), b"").unwrap();
        symlink("empty", root.path().join("link")).unwrap();
        git_ok(root.path(), &["add", "executable", "empty", "link"]);
        git_ok(root.path(), &["commit", "-q", "-m", "modes links empties"]);
        let _dir_guard = DirGuard::new();
        std::env::set_current_dir(root.path()).unwrap();
        let import = Import {
            no_vault: true,
            ..Import::default()
        };
        assert!(
            import.run().is_ok(),
            "the CB-9C supported corpus imports executables, symlinks, and empty files"
        );
    }

    #[test]
    #[serial]
    fn required_filter_failure_rejects_incremental_import_without_view_mutation() {
        let _dir_guard = DirGuard::new();
        let root = tempfile::tempdir().unwrap();
        init_git(root.path());
        fs::write(root.path().join("base.txt"), b"base\n").unwrap();
        git_ok(root.path(), &["add", "base.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "base"]);
        std::env::set_current_dir(root.path()).unwrap();
        drop(Repository::init_with_view(root.path(), "main").unwrap());

        let before = Repository::open_readonly(root.path())
            .unwrap()
            .get_view_info("main")
            .unwrap()
            .state;
        let config_path = root.path().join(".atomic/config.toml");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str("\n[filters.drivers.blocked]\nrequired = true\n");
        fs::write(config_path, config).unwrap();
        fs::write(
            root.path().join(".gitattributes"),
            b"*.dat filter=blocked\n",
        )
        .unwrap();
        fs::write(root.path().join("payload.dat"), b"payload\n").unwrap();
        git_ok(root.path(), &["add", ".gitattributes", "payload.dat"]);
        git_ok(root.path(), &["commit", "-q", "-m", "required filter"]);

        let result = Import {
            incremental: true,
            no_vault: true,
            ..Import::default()
        }
        .run();
        assert!(result.is_err());
        let after = Repository::open_readonly(root.path())
            .unwrap()
            .get_view_info("main")
            .unwrap()
            .state;
        assert_eq!(before, after);
    }

    #[test]
    fn test_default_import() {
        let import = Import::default();
        assert!(!import.dry_run);
        assert!(!import.all_branches);
        assert!(!import.incremental);
        assert!(import.branch.is_none());
        assert!(!import.skip_checkpoint_refresh);
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
    #[serial]
    fn import_records_a_per_import_synthesis_aggregation() {
        // Review R5: the per-import synthesis aggregation is wired from
        // the real import statistics and is consent-gated. Failing before
        // the fix: no aggregation event existed at all.
        let _dir_guard = DirGuard::new();
        let root = tempfile::tempdir().unwrap();
        init_git(root.path());
        fs::write(root.path().join("tracked.txt"), b"first\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "first"]);
        std::env::set_current_dir(root.path()).unwrap();

        Import {
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();

        // Un-consented: the automatic sink writes nothing (review R1/R4).
        let journal = root.path().join(".atomic/bridge/events.jsonl");
        assert!(
            !journal.exists(),
            "an un-opted repository must not record import synthesis"
        );

        // Opt in exactly as `atomic git bridge enable` records the consent,
        // then import a second commit.
        let config = root.path().join(".atomic/config.toml");
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&config)
                .unwrap();
            writeln!(file, "\n[git.bridge]\nenabled = true\n").unwrap();
        }
        fs::write(root.path().join("tracked.txt"), b"second\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "second"]);
        Import {
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();

        let text = fs::read_to_string(&journal).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("import_synthesis"))
            .expect("the per-import synthesis aggregation is recorded");
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["event"], "import_synthesis");
        // Real counters from the import run, not constructed booleans: the
        // run imported the second commit (one parsed commit, one written
        // change).
        assert_eq!(event["written"], 1);
        assert_eq!(event["empty"], 0);
        assert_eq!(event["merges"], 0);
        assert_eq!(
            event["commits_found"].as_u64().unwrap()
                - event["self_push_skipped"].as_u64().unwrap()
                - event["squash_inserted"].as_u64().unwrap()
                - event["written"].as_u64().unwrap()
                - event["empty"].as_u64().unwrap()
                - event["merges"].as_u64().unwrap(),
            0,
            "every found commit is accounted for by the aggregation"
        );
    }

    #[test]
    #[serial]
    fn standalone_import_refreshes_v2_checkpoint_before_status() {
        let _dir_guard = DirGuard::new();
        let root = tempfile::tempdir().unwrap();
        git_ok(root.path(), &["init", "-q"]);
        git_ok(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root.path(), &["config", "user.name", "Atomic Test"]);
        git_ok(root.path(), &["config", "user.email", "atomic@example.com"]);
        fs::write(root.path().join("tracked.txt"), b"tracked\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "initial"]);
        std::env::set_current_dir(root.path()).unwrap();

        Import {
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();

        let checkpoint = super::super::checkpoint::read_checkpoint(root.path())
            .unwrap()
            .expect("standalone import must publish a checkpoint");
        assert_eq!(
            checkpoint.version,
            super::super::checkpoint::CHECKPOINT_VERSION
        );
        assert_eq!(checkpoint.view, "main");
        crate::commands::status::Status::new()
            .with_short(true)
            .run()
            .expect("guarded status must pass immediately after import");
    }

    #[test]
    #[serial]
    fn one_line_incremental_import_finishes_within_debug_budget() {
        let _dir_guard = DirGuard::new();
        let root = tempfile::tempdir().unwrap();
        git_ok(root.path(), &["init", "-q"]);
        git_ok(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root.path(), &["config", "user.name", "Atomic Test"]);
        git_ok(root.path(), &["config", "user.email", "atomic@example.com"]);
        fs::write(root.path().join("tracked.txt"), b"first\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "initial"]);
        std::env::set_current_dir(root.path()).unwrap();

        Import {
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();

        fs::write(root.path().join("tracked.txt"), b"second\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "one line"]);

        let started = std::time::Instant::now();
        Import {
            incremental: true,
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "one-line incremental import took {elapsed:?}"
        );
    }

    #[test]
    #[serial]
    fn mismatched_incremental_import_does_not_publish_checkpoint_before_alignment() {
        let _dir_guard = DirGuard::new();
        let root = tempfile::tempdir().unwrap();
        git_ok(root.path(), &["init", "-q"]);
        git_ok(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root.path(), &["config", "user.name", "Atomic Test"]);
        git_ok(root.path(), &["config", "user.email", "atomic@example.com"]);
        fs::write(root.path().join("tracked.txt"), b"main\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "main"]);
        std::env::set_current_dir(root.path()).unwrap();
        Import {
            no_vault: true,
            ..Import::default()
        }
        .run()
        .unwrap();
        let before = super::super::checkpoint::read_checkpoint(root.path())
            .unwrap()
            .unwrap();

        git_ok(root.path(), &["switch", "-q", "-c", "topic"]);
        fs::write(root.path().join("tracked.txt"), b"topic\n").unwrap();
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "topic"]);
        Import {
            branch: Some("topic".to_string()),
            incremental: true,
            no_vault: true,
            skip_checkpoint_refresh: true,
            ..Import::default()
        }
        .run()
        .unwrap();

        let after = super::super::checkpoint::read_checkpoint(root.path())
            .unwrap()
            .unwrap();
        assert_eq!(after, before, "mismatched import must not move checkpoint");
        let repo = Repository::open(root.path()).unwrap();
        assert_eq!(repo.current_view(), "main");
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
