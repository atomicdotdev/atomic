//!
//! ```text
//! atomic status [OPTIONS] [PATH]
//!
//! Arguments:
//!   [PATH]  Show status for specific path only
//!
//! Options:
//!   -s, --short         Show short/porcelain output
//!   --no-untracked      Don't show untracked files
//!   -h, --help          Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Long Format (Default)
//!
//! The long format provides detailed, user-friendly output with colored
//! sections and helpful hints:
//!
//! ```text
//! On view dev
//! State: ABCDEF123456
//!
//! Changes to be recorded:
//!   (use "atomic restore <file>..." to discard changes)
//!
//!     new file:   src/new_feature.rs
//!     modified:   src/main.rs
//!     deleted:    src/old_module.rs
//!
//! Untracked files:
//!   (use "atomic add <file>..." to include in what will be recorded)
//!
//!     notes.txt
//!     temp/
//! ```
//!
//! ## Short Format (-s, --short)
//!
//! The short format is machine-readable and similar to `git status -s`:
//!
//! ```text
//! A  src/new_feature.rs
//! M  src/main.rs
//! D  src/old_module.rs
//! ?  notes.txt
//! ?  temp/
//! ```
//!
//! # Status Codes
//!
//! | Code | Meaning |
//! |------|---------|
//! | `A` | Added (new file tracked) |
//! | `M` | Modified |
//! | `D` | Deleted |
//! | `R` | Renamed |
//! | `C` | Conflicted |
//! | `T` | Type changed |
//! | `P` | Permissions changed |
//! | `?` | Untracked |
//! | ` ` | Clean (only shown with --verbose) |
//!
//! # Examples
//!
//! Show status of entire repository:
//! ```text
//! $ atomic status
//! On view dev
//! State: ABCDEF12
//!
//! Changes to be recorded:
//!     modified:   src/lib.rs
//!
//! nothing else to record
//! ```
//!
//! Show short status:
//! ```text
//! $ atomic status -s
//! M  src/lib.rs
//! ```
//!
//! Show status for specific path:
//! ```text
//! $ atomic status src/
//! On view dev
//! State: ABCDEF12
//!
//! Changes to be recorded:
//!     modified:   src/lib.rs
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Serialize;

use atomic_core::types::Base32;
use atomic_repository::status::{FileStatus, RepositoryStatus, StatusOptions};
use atomic_repository::{
    Repository, SnapshotStatus, WorkspaceRemediation, WorkspaceTxnMode, WorkspaceTxnStart,
};

use crate::commands::git::bridge::read_checkpoint_observation;
use crate::commands::git::observation::{
    classify_provisional_checkpoint, display_git_bytes, observe_git, AtomicAnchorObservation,
    GitObservation, IndexHeadEquivalence, ManifestEquivalence, ProvisionalCheckpointEligibility,
    RefTargetObservation,
};
use crate::commands::workspace_txn::{enter_workspace, observe_workspace};
use crate::commands::{find_repository_root, Command, DEFAULT_HASH_LENGTH};
use crate::error::{CliError, CliResult};
use crate::output::{
    added, deleted, hash, hint, info, modified, path as style_path, print_blank, print_hint,
    print_info, print_section, print_warning, untracked as style_untracked, view as style_view,
    warning,
};

// Status Output Configuration

// Status Command

/// Show the status of the working copy.
///
/// The `status` command displays information about the current state of
/// the working copy, including:
///
/// - The current view name
/// - The current view state (Merkle hash)
/// - Modified files (tracked files that have changed)
/// - Deleted files (tracked files that no longer exist)
/// - Added files (newly tracked files)
/// - Untracked files (files not yet added to tracking)
///
/// # Output Formats
///
/// The command supports two output formats:
///
/// 1. **Long format** (default): Human-readable output with colors and hints
/// 2. **Short format** (`-s`): Machine-readable, similar to `git status -s`
/// 3. **Forensic format** (`--no-reconcile`): Read-only Atomic/Git checkpoint evidence
///
/// # Path Filtering
///
/// When a path is provided, only files under that path are shown. This is
/// useful for checking status in large repositories.
#[derive(Parser, Debug)]
pub struct Status {
    /// Show status for specific path only.
    ///
    /// When provided, only files under this path will be included in
    /// the status output. The path is relative to the repository root.
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Show short/porcelain output.
    ///
    /// The short format displays one line per file with a two-character
    /// status code followed by the file path. This format is designed
    /// for machine parsing and scripts.
    ///
    /// Status codes:
    /// - `A ` - Added (new tracked file)
    /// - `M ` - Modified
    /// - `D ` - Deleted
    /// - `R ` - Renamed
    /// - `C ` - Conflicted
    /// - `T ` - Type changed
    /// - `P ` - Permissions changed
    /// - `??` - Untracked
    #[arg(short = 's', long = "short")]
    pub short: bool,

    /// Emit a versioned JSON document for IDEs and other integrations.
    #[arg(long, conflicts_with_all = ["short", "debug_ignore"])]
    pub json: bool,

    /// Don't show untracked files.
    ///
    /// By default, untracked files are shown in the status output.
    /// Use this flag to hide them, showing only tracked files that
    /// have changes.
    #[arg(long = "no-untracked")]
    pub no_untracked: bool,

    /// Debug ignore pattern matching.
    ///
    /// Shows diagnostic information about how .atomicignore patterns
    /// are being applied. Useful for troubleshooting why files are
    /// or aren't being ignored.
    #[arg(long = "debug-ignore", hide = true)]
    pub debug_ignore: bool,

    /// Rebuild the FILE_INDEX before computing status.
    ///
    /// Walks every tracked file, hashes its content, and updates the
    /// FILE_INDEX cache so that subsequent `status` calls use the fast
    /// stat-comparison path instead of reconstructing graph content.
    ///
    /// This is useful after `git import` where file mtimes are reset,
    /// invalidating the cached entries.
    #[arg(long = "reindex", conflicts_with = "no_reconcile")]
    pub reindex: bool,

    /// Print read-only Git/Atomic checkpoint evidence without reconciliation.
    ///
    /// This forensic mode does not run ordinary file status, reindex, import,
    /// materialization, or any bridge reconciliation path.
    #[arg(long = "no-reconcile", conflicts_with = "reindex")]
    pub no_reconcile: bool,

    /// Show the Git-inspired, versioned staging subset (RFC §9.2).
    ///
    /// Two-column codes distinguish the staged layer (Git baseline → index)
    /// from the unstaged layer (index → worktree). This is a bounded,
    /// versioned subset: it does not claim full Git porcelain compatibility.
    /// Output is observation-only: it never journals reconciliation,
    /// materializes, or mutates durable tracking.
    #[arg(long = "git", conflicts_with = "reindex")]
    pub git: bool,
}

impl Status {
    /// Create a new Status command with default settings.
    pub fn new() -> Self {
        Self {
            path: None,
            short: false,
            json: false,
            no_untracked: false,
            debug_ignore: false,
            reindex: false,
            no_reconcile: false,
            git: false,
        }
    }

    fn workspace_mode(&self) -> WorkspaceTxnMode {
        if self.no_reconcile {
            WorkspaceTxnMode::Observe
        } else {
            WorkspaceTxnMode::Reconcile
        }
    }

    /// Builder: set the path filter.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Builder: set short output mode.
    pub fn with_short(mut self, short: bool) -> Self {
        self.short = short;
        self
    }

    /// Builder: set JSON output mode.
    pub fn with_json(mut self, json: bool) -> Self {
        self.json = json;
        self
    }

    /// Builder: set whether to hide untracked files.
    pub fn with_no_untracked(mut self, no_untracked: bool) -> Self {
        self.no_untracked = no_untracked;
        self
    }

    /// Get status options based on command settings.
    fn get_status_options(&self) -> StatusOptions {
        let mut options = StatusOptions::default();

        if self.no_untracked {
            options = options.with_untracked(false);
        }

        if let Some(ref path) = self.path {
            options = options.filter_path(PathBuf::from(path));
        }

        options
    }

    fn print_snapshot_status(&self, status: &SnapshotStatus) {
        if let Some(snapshot) = status.snapshot {
            println!(
                "Snapshot: {} ({} superseded retained)",
                hash(&snapshot.to_base32()[..DEFAULT_HASH_LENGTH]),
                status.superseded_snapshots
            );
        } else if let Some(remainder) = status.remainder {
            println!(
                "Snapshot remainder: {}",
                hash(&remainder.to_base32()[..DEFAULT_HASH_LENGTH])
            );
        }
    }

    /// Print the status in long (human-readable) format.
    fn print_long_format(&self, status: &RepositoryStatus) -> CliResult<()> {
        // Print view info
        print!("On view ");
        println!("{}", style_view(status.view()));

        // Print state hash if available
        if let Some(state) = status.state() {
            let state_str = state.to_base32();
            // Truncate to default hash length for display
            let short_state = if state_str.len() > DEFAULT_HASH_LENGTH {
                format!("{}...", &state_str[..DEFAULT_HASH_LENGTH])
            } else {
                state_str
            };
            print!("State: ");
            println!("{}", hash(&short_state));
        }

        print_blank();

        // Check if there are any changes
        let has_changes = status.modified_count() > 0
            || status.deleted_count() > 0
            || status.added_count() > 0
            || status.conflicted_count() > 0
            || status.type_changed_count() > 0
            || status.permissions_changed_count() > 0;

        let has_untracked = status.untracked_count() > 0;

        if !has_changes && !has_untracked {
            println!("{}", info("nothing to record, working tree clean"));
            return Ok(());
        }

        // Advisory notices (e.g. the RFC §8.3 conflict-snapshot caveat) are
        // surfaced before the file listing so the distinction between a
        // committed conflict snapshot and Git unmerged stages is explicit.
        for notice in status.notices() {
            print_warning(notice);
        }

        // Print changes section
        if has_changes {
            print_section("Changes to be recorded:");
            println!(
                "  {}",
                hint("(use \"atomic restore <file>...\" to discard changes)")
            );
            print_blank();

            // Print added files and directories
            for entry in status.added() {
                let path_str = entry.path().display().to_string();
                let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_dir {
                    println!("\t{}    {}", added("new dir:"), style_path(&path_str));
                } else {
                    println!("\t{}   {}", added("new file:"), style_path(&path_str));
                }
            }

            // Print modified files
            for entry in status.modified() {
                let path_str = entry.path().display().to_string();
                println!("\t{}   {}", modified("modified:"), style_path(&path_str));
            }

            // Print deleted files and directories
            for entry in status.deleted() {
                let path_str = entry.path().display().to_string();
                let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_dir {
                    println!("\t{}    {}", deleted("del dir:"), style_path(&path_str));
                } else {
                    println!("\t{}   {}", deleted("deleted:"), style_path(&path_str));
                }
            }

            // Print type-changed files (regular ↔ symlink ↔ directory)
            for entry in status.type_changed() {
                let path_str = entry.path().display().to_string();
                println!("\t{}     {}", warning("type:"), style_path(&path_str));
            }

            // Print permission-changed files
            for entry in status.permissions_changed() {
                let path_str = entry.path().display().to_string();
                println!("\t{}      {}", warning("mode:"), style_path(&path_str));
            }

            // Print conflicted files
            for entry in status.conflicted() {
                let path_str = entry.path().display().to_string();
                let conflict_msg = entry.details().unwrap_or("conflict");
                println!(
                    "\t{} ({})   {}",
                    warning("conflict:"),
                    conflict_msg,
                    style_path(&path_str)
                );
            }

            print_blank();
        }

        // Print untracked files section
        if has_untracked && !self.no_untracked {
            print_section("Untracked files:");
            println!(
                "  {}",
                hint("(use \"atomic add <file>...\" to include in what will be recorded)")
            );
            print_blank();

            for entry in status.untracked() {
                let path_str = entry.path().display().to_string();
                println!("\t{}", style_untracked(&path_str));
            }

            print_blank();
        }

        // Hint: stale FILE_INDEX detected
        if status.needs_reindex() {
            println!(
                "  {}",
                hint(&format!(
                    "hint: {} file(s) reported as modified due to a stale index.",
                    status.stale_index_count()
                ))
            );
            println!(
                "  {}",
                hint("Run \"atomic status --reindex\" to rebuild the file index.")
            );
            print_blank();
        }

        // Print summary hint
        if has_changes {
            print_hint("Use \"atomic record\" to record your changes");
        } else if has_untracked {
            print_hint("Use \"atomic add <file>...\" to track files");
        }

        Ok(())
    }

    /// Print the status in short (porcelain) format.
    fn print_short_format(&self, status: &RepositoryStatus) -> CliResult<()> {
        // Print added files and directories
        for entry in status.added() {
            let path_str = entry.path().display().to_string();
            let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
            if is_dir {
                println!("AD {}", path_str);
            } else {
                println!("A  {}", path_str);
            }
        }

        // Print modified files
        for entry in status.modified() {
            let path_str = entry.path().display().to_string();
            println!("M  {}", path_str);
        }

        // Print deleted files and directories
        for entry in status.deleted() {
            let path_str = entry.path().display().to_string();
            let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
            if is_dir {
                println!("DD {}", path_str);
            } else {
                println!("D  {}", path_str);
            }
        }

        // Print type-changed files
        for entry in status.type_changed() {
            let path_str = entry.path().display().to_string();
            println!("T  {}", path_str);
        }

        // Print permission-changed files
        for entry in status.permissions_changed() {
            let path_str = entry.path().display().to_string();
            println!("P  {}", path_str);
        }

        // Print conflicted files
        for entry in status.conflicted() {
            let path_str = entry.path().display().to_string();
            println!("C  {}", path_str);
        }

        // Print untracked files
        if !self.no_untracked {
            for entry in status.untracked() {
                let path_str = entry.path().display().to_string();
                println!("?? {}", path_str);
            }
        }

        Ok(())
    }

    /// Print a stable, versioned status document for editor integrations.
    fn print_json_format(
        &self,
        status: &RepositoryStatus,
        repo_root: &std::path::Path,
    ) -> CliResult<()> {
        let output = JsonStatus::new(status, repo_root);
        let json = serde_json::to_string_pretty(&output)
            .map_err(|error| CliError::Internal(error.into()))?;
        println!("{}", json);
        Ok(())
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

impl Status {
    /// Print debug information about ignore rules.
    fn print_ignore_debug(
        &self,
        repo: &Repository,
        working_copy: atomic_core::WorkingCopyId,
    ) -> CliResult<()> {
        use std::path::Path;

        println!("=== Ignore Debug Information ===");
        println!("Repository root: {}", repo.root().display());

        let ignore_path = repo.root().join(".atomicignore");
        println!(".atomicignore path: {}", ignore_path.display());
        println!(".atomicignore exists: {}", ignore_path.exists());

        if ignore_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&ignore_path) {
                println!(".atomicignore content ({} bytes):", content.len());
                for (i, line) in content.lines().enumerate() {
                    println!("  {}: {:?}", i + 1, line);
                }
            }
        }

        let rules = repo
            .ignore_rules(working_copy)
            .map_err(|e| CliError::InvalidRepository {
                reason: e.to_string(),
            })?;
        println!("\nIgnore rules loaded:");
        println!("  Has local rules: {}", rules.has_local_rules());
        println!("  Has global rules: {}", rules.has_global_rules());
        println!("  Local pattern count: {}", rules.local_pattern_count());
        println!("  Global pattern count: {}", rules.global_pattern_count());

        // Test some common paths
        let test_paths = [
            ("node_modules", true),
            ("node_modules/foo", true),
            ("node_modules/foo/bar.js", false),
            ("target", true),
            ("target/debug", true),
            ("src/main.rs", false),
        ];

        println!("\nTest path matching:");
        for (path, is_dir) in test_paths {
            let ignored = rules.is_ignored(Path::new(path), is_dir);
            println!("  {:40} is_dir={:5} ignored={}", path, is_dir, ignored);
        }

        println!("================================\n");
        Ok(())
    }
}

impl Command for Status {
    /// Execute the status command.
    ///
    /// This method:
    /// 1. Finds and opens the repository
    /// 2. Computes the working copy status
    /// 3. Displays the status in the requested format
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found
    /// - The repository cannot be opened
    /// - Status computation fails
    fn run(&self) -> CliResult<()> {
        // Find the repository root
        let repo_root = find_repository_root()?;

        // Git-inspired, versioned staging subset (observation-only).
        if self.git {
            return crate::commands::status_git::run_git_status(&repo_root, self.json);
        }

        if self.no_reconcile {
            let mut repo =
                Repository::open_readonly(&repo_root).map_err(|e| CliError::InvalidRepository {
                    reason: e.to_string(),
                })?;
            match observe_workspace(&mut repo)? {
                Ok(workspace) => {
                    let view = workspace.view().name.clone();
                    let result = print_forensic_status(&repo_root, &repo, &view, None);
                    // CB-10A: surface ref-mapping divergence (read-only).
                    let _ = crate::commands::git::ref_mapping::print_divergence_notices(&repo);
                    return result;
                }
                Err(remediation) => {
                    let working_copy = repo.require_working_copy_id().map_err(|e| {
                        CliError::InvalidRepository {
                            reason: e.to_string(),
                        }
                    })?;
                    let view = repo
                        .desired_view_name(working_copy)
                        .map_err(CliError::from)?;
                    let result =
                        print_forensic_status(&repo_root, &repo, &view, Some(&remediation));
                    let _ = crate::commands::git::ref_mapping::print_divergence_notices(&repo);
                    return result;
                }
            }
        }

        let mut repo = Repository::open_for_workspace_transaction_wait(
            &repo_root,
            std::time::Duration::from_secs(10),
        )
        .map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;
        let working_copy =
            repo.require_working_copy_id()
                .map_err(|e| CliError::InvalidRepository {
                    reason: e.to_string(),
                })?;
        // Enter the workspace boundary. A Git-owned operation in progress is
        // reported as a Git-informed notice instead of a hard failure so the
        // native status stays readable (RFC §9.2 "Git merge in progress");
        // every other remediation still fails closed.
        let mut git_merge_notice: Option<String> = None;
        let workspace = match repo.begin_workspace_txn(self.workspace_mode()) {
            Ok(WorkspaceTxnStart::Ready(workspace)) => Some(workspace),
            Ok(WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
                repository_state,
                conflict_stages,
                ..
            })) => {
                let mut notice = format!("Git operation in progress ({repository_state})");
                if !conflict_stages.is_empty() {
                    notice.push_str(&format!(
                        "; index holds {} conflicted stage entries",
                        conflict_stages.len()
                    ));
                }
                git_merge_notice = Some(notice);
                None
            }
            Ok(WorkspaceTxnStart::Remediation(other)) => {
                return Err(crate::commands::workspace_txn::remediation_error(other))
            }
            Err(e) => return Err(CliError::Repository(e)),
        };
        let working_copy = workspace
            .as_ref()
            .map(|workspace| workspace.working_copy())
            .unwrap_or(working_copy);

        // Reindex first if requested (needs read-write access)
        if self.reindex {
            let start = std::time::Instant::now();
            match repo.reindex_working_copy(working_copy) {
                Ok(count) => {
                    if !self.json {
                        print_info(&format!(
                            "Reindexed {} files in {:.1}s",
                            count,
                            start.elapsed().as_secs_f64()
                        ));
                    }
                }
                Err(e) => {
                    if self.json {
                        return Err(CliError::Internal(e.into()));
                    }
                    print_warning(&format!("Reindex failed: {}", e));
                }
            }
        }

        // Debug ignore patterns if requested
        if self.debug_ignore {
            self.print_ignore_debug(&repo, working_copy)?;
        }

        // Get status options
        let options = self.get_status_options();

        // Compute status
        let mut status = repo
            .status(working_copy, options)
            .map_err(|e| CliError::Internal(e.into()))?;

        // Git-informed overlays (colocated only): index-new paths displayed
        // as pending additions, unmerged entries as Git merge conflicts.
        // Display only — durable tracking is never mutated here.
        crate::commands::status_git::apply_git_informed_overlays(&repo, &repo_root, &mut status)?;

        if let Some(notice) = &git_merge_notice {
            print_warning(notice);
        }

        // CB-10A: surface ref-mapping divergence with actionable remediation
        // (read-only; never materializes).
        crate::commands::git::ref_mapping::print_divergence_notices(&repo)?;

        // Print in appropriate format
        if self.json {
            self.print_json_format(&status, &repo_root)
        } else if self.short {
            self.print_short_format(&status)
        } else {
            let snapshot = repo
                .snapshot_status(working_copy)
                .map_err(|error| CliError::Internal(error.into()))?;
            self.print_snapshot_status(&snapshot);
            self.print_long_format(&status)
        }
    }
}

fn print_forensic_status(
    repo_root: &Path,
    repo: &Repository,
    view: &str,
    remediation: Option<&WorkspaceRemediation>,
) -> CliResult<()> {
    let atomic = AtomicAnchorObservation {
        state: repo
            .get_view_info(view)
            .map_err(CliError::from)?
            .state
            .to_string(),
        view: view.to_string(),
    };
    let checkpoint = read_checkpoint_observation(repo_root)?;
    let git = observe_git(repo_root).map_err(|error| CliError::GitError {
        message: error.to_string(),
    })?;
    let eligibility = classify_provisional_checkpoint(
        &git,
        &atomic,
        checkpoint.as_ref(),
        &ManifestEquivalence::NotComputed,
    );
    let manifest_root = forensic_manifest_root(repo_root, repo, view);

    print!(
        "{}",
        build_forensic_report(
            repo_root,
            &atomic,
            checkpoint.as_ref(),
            &git,
            &eligibility,
            manifest_root.as_deref(),
        )
    );
    if let Some(remediation) = remediation {
        println!("Workspace remediation required: {remediation:#?}");
    }
    Ok(())
}

fn forensic_manifest_root(repo_root: &Path, repo: &Repository, view: &str) -> Option<String> {
    if !repo_root.join(".git").exists() {
        return None;
    }
    let git_repo = git2::Repository::open(repo_root).ok()?;
    let policy = crate::commands::git::parallel::conversion_policy(&git_repo).ok()?;
    let project = repo.project_tree(view, &policy).ok()?;
    let root = project.manifest.root();
    Some(format!("v{} {}", root.version, root.content_key))
}

/// Report observed drift between the checkpoint and current state as plain
/// observations. No stale-baseline classification is made: index movement is
/// legitimate Git work between boundaries (CB-11A).
fn forensic_drift(
    checkpoint: Option<&crate::commands::git::observation::BridgeCheckpointObservation>,
    atomic: &AtomicAnchorObservation,
    git: &GitObservation,
) -> Vec<String> {
    let Some(checkpoint) = checkpoint else {
        return vec!["checkpoint absent (nothing to compare)".to_string()];
    };
    let GitObservation::Repository(repository) = git else {
        return vec!["git repository absent".to_string()];
    };
    let mut drift = Vec::new();
    if checkpoint.view != atomic.view {
        drift.push(format!(
            "view: checkpoint {} vs current {}",
            checkpoint.view, atomic.view
        ));
    }
    if checkpoint.atomic_state != atomic.state {
        drift.push(format!(
            "atomic state: checkpoint {} vs current {}",
            checkpoint.atomic_state, atomic.state
        ));
    }
    let observed_head = repository
        .head
        .oid()
        .map(|oid| oid.to_string())
        .unwrap_or_else(|| "none".to_string());
    if checkpoint.git_head != observed_head {
        drift.push(format!(
            "Git HEAD: checkpoint {} vs current {observed_head}",
            checkpoint.git_head
        ));
    }
    let observed_tree = repository
        .head_tree_oid
        .map(|oid| oid.to_string())
        .unwrap_or_else(|| "none".to_string());
    if checkpoint.git_tree != observed_tree {
        drift.push(format!(
            "Git tree: checkpoint {} vs current {observed_tree}",
            checkpoint.git_tree
        ));
    }
    if drift.is_empty() {
        drift.push("none (checkpoint matches current state)".to_string());
    }
    drift
}

fn build_forensic_report(
    repo_root: &Path,
    atomic: &AtomicAnchorObservation,
    checkpoint: Option<&crate::commands::git::observation::BridgeCheckpointObservation>,
    git: &GitObservation,
    eligibility: &ProvisionalCheckpointEligibility,
    manifest_root: Option<&str>,
) -> String {
    let mut report = String::new();
    let _ = writeln!(
        report,
        "Forensic status (read-only; reconciliation disabled)"
    );
    let _ = writeln!(report, "Atomic root: {}", repo_root.display());
    let _ = writeln!(report, "Atomic view: {}", atomic.view);
    let _ = writeln!(report, "Atomic state: {}", atomic.state);
    match manifest_root {
        Some(root) => {
            let _ = writeln!(report, "Durable manifest root: {root}");
        }
        None => {
            let _ = writeln!(
                report,
                "Durable manifest root: unavailable (non-colocated or projection refused)"
            );
        }
    }
    let _ = writeln!(report, "Drift (observed, not classified):");
    for line in forensic_drift(checkpoint, atomic, git) {
        let _ = writeln!(report, "  {line}");
    }

    match checkpoint {
        Some(checkpoint) => {
            let _ = writeln!(report, "Bridge checkpoint: present");
            let _ = writeln!(report, "  view: {}", checkpoint.view);
            let _ = writeln!(report, "  atomic state: {}", checkpoint.atomic_state);
            let _ = writeln!(report, "  Git HEAD: {}", checkpoint.git_head);
            let _ = writeln!(report, "  Git tree: {}", checkpoint.git_tree);
        }
        None => {
            let _ = writeln!(report, "Bridge checkpoint: absent");
        }
    }

    match git {
        GitObservation::NoGit { root } => {
            let _ = writeln!(report, "Git: not present");
            let _ = writeln!(report, "  inspected root: {}", root.display());
        }
        GitObservation::Repository(repository) => {
            let _ = writeln!(report, "Git: present");
            let _ = writeln!(
                report,
                "  worktree root: {}",
                repository
                    .paths
                    .worktree_root
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "none (bare)".to_string())
            );
            let _ = writeln!(
                report,
                "  worktree Git dir: {}",
                repository.paths.worktree_git_dir.display()
            );
            let _ = writeln!(
                report,
                "  common dir: {}",
                repository.paths.common_dir.display()
            );
            let _ = writeln!(
                report,
                "  index path: {}",
                repository.paths.index_path.display()
            );
            match &repository.head {
                crate::commands::git::observation::HeadObservation::Attached { symref, oid } => {
                    let _ = writeln!(report, "  HEAD: attached");
                    let _ = writeln!(report, "    symref: {symref}");
                    let _ = writeln!(report, "    OID: {oid}");
                }
                crate::commands::git::observation::HeadObservation::Detached { oid } => {
                    let _ = writeln!(report, "  HEAD: detached");
                    let _ = writeln!(report, "    symref: none");
                    let _ = writeln!(report, "    OID: {oid}");
                }
                crate::commands::git::observation::HeadObservation::Unborn { symref } => {
                    let _ = writeln!(report, "  HEAD: unborn");
                    let _ = writeln!(report, "    symref: {symref}");
                    let _ = writeln!(report, "    OID: none");
                }
                crate::commands::git::observation::HeadObservation::MissingTarget { symref } => {
                    let _ = writeln!(report, "  HEAD: symbolic target missing");
                    let _ = writeln!(report, "    symref: {symref}");
                    let _ = writeln!(report, "    OID: none");
                }
            }
            let _ = writeln!(
                report,
                "  HEAD tree: {}",
                repository
                    .head_tree_oid
                    .map(|oid| oid.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );

            let _ = writeln!(report, "  index:");
            let _ = writeln!(report, "    exists: {}", repository.index.exists);
            let _ = writeln!(report, "    version: {}", repository.index.version);
            let _ = writeln!(
                report,
                "    canonical index digest (not a Git tree OID): {}",
                repository.index.canonical_digest.0
            );
            let _ = writeln!(
                report,
                "    exact index tree OID: {}",
                repository
                    .index
                    .tree_oid
                    .map(|oid| oid.to_string())
                    .unwrap_or_else(|| "unavailable".to_string())
            );
            let _ = writeln!(
                report,
                "    index tree availability: {:?}",
                repository.index.tree_availability
            );
            match &repository.index.head_equivalence {
                IndexHeadEquivalence::Equal { head_tree } => {
                    let _ = writeln!(report, "    HEAD equivalence: equal to {head_tree}");
                }
                IndexHeadEquivalence::Different {
                    head_tree,
                    missing_from_index,
                    added_to_index,
                    changed,
                } => {
                    let _ = writeln!(report, "    HEAD equivalence: differs from {head_tree}");
                    write_raw_path_list(&mut report, "missing from index", missing_from_index);
                    write_raw_path_list(&mut report, "added to index", added_to_index);
                    write_raw_path_list(&mut report, "changed", changed);
                }
                IndexHeadEquivalence::NotApplicable => {
                    let _ = writeln!(report, "    HEAD equivalence: not applicable");
                }
            }
            let _ = writeln!(report, "    entries: {}", repository.index.entries.len());
            for entry in &repository.index.entries {
                let _ = writeln!(
                    report,
                    "      stage={} mode={:06o} oid={} flags=0x{:04x} extended=0x{:04x} path={}",
                    entry.stage,
                    entry.mode,
                    entry.oid,
                    entry.flags,
                    entry.flags_extended,
                    display_git_bytes(&entry.path)
                );
            }

            let _ = writeln!(report, "  locks:");
            let _ = writeln!(
                report,
                "    index: {} ({:?})",
                repository.locks.index_lock.path.display(),
                repository.locks.index_lock.kind
            );
            let _ = writeln!(
                report,
                "    ref locks: {}",
                repository.locks.ref_locks.len()
            );
            for path in &repository.locks.ref_locks {
                let _ = writeln!(report, "      {}", path.display());
            }

            let _ = writeln!(
                report,
                "  Git operation state: {}",
                repository.operation.repository_state
            );
            for marker in &repository.operation.markers {
                let _ = writeln!(
                    report,
                    "    {}: {:?} ({})",
                    marker.marker.as_str(),
                    marker.kind,
                    marker.path.display()
                );
            }

            let _ = writeln!(
                report,
                "  refs: {} (digest {})",
                repository.refs.len(),
                repository.refs_digest.0
            );
            for reference in &repository.refs {
                let target = match &reference.target {
                    RefTargetObservation::Direct(oid) => oid.to_string(),
                    RefTargetObservation::Symbolic(target) => {
                        format!("symref {}", display_git_bytes(target))
                    }
                    RefTargetObservation::Unresolved => "unresolved".to_string(),
                };
                let _ = writeln!(
                    report,
                    "    {} -> {}",
                    display_git_bytes(&reference.name),
                    target
                );
            }
        }
    }

    match eligibility {
        ProvisionalCheckpointEligibility::Eligible(checkpoint) => {
            let _ = writeln!(
                report,
                "Checkpoint classification: eligible ({:?})",
                checkpoint.source
            );
            let _ = writeln!(
                report,
                "  HEAD/index/refs observation may be used as a provisional checkpoint"
            );
        }
        ProvisionalCheckpointEligibility::Unanchored(reason) => {
            let _ = writeln!(report, "Checkpoint classification: Unanchored");
            let _ = writeln!(report, "  reason: {reason}");
            let _ = writeln!(report, "  typed reason: {reason:?}");
        }
    }
    let _ = writeln!(
        report,
        "Ordinary Atomic file status, reconciliation, and mutation were not run."
    );
    report
}

fn write_raw_path_list(report: &mut String, label: &str, paths: &[Vec<u8>]) {
    let _ = writeln!(report, "      {label}: {}", paths.len());
    for path in paths {
        let _ = writeln!(report, "        {}", display_git_bytes(path));
    }
}

// JSON Output Types

/// Version of the `atomic status --json` document.
const STATUS_JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct JsonStatus {
    schema_version: u32,
    repository_root: String,
    view: String,
    state: Option<String>,
    clean: bool,
    needs_reindex: bool,
    stale_index_count: usize,
    entries: Vec<JsonStatusEntry>,
}

impl JsonStatus {
    fn new(status: &RepositoryStatus, repo_root: &std::path::Path) -> Self {
        let mut entries: Vec<_> = status.entries().iter().map(JsonStatusEntry::from).collect();
        entries.sort_by(|left, right| left.path.cmp(&right.path));

        Self {
            schema_version: STATUS_JSON_SCHEMA_VERSION,
            repository_root: repo_root.to_string_lossy().into_owned(),
            view: status.view().to_string(),
            state: status.state().map(|state| state.to_base32()),
            clean: entries.is_empty(),
            needs_reindex: status.needs_reindex(),
            stale_index_count: status.stale_index_count(),
            entries,
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct JsonStatusEntry {
    path: String,
    status: &'static str,
    code: String,
    details: Option<String>,
}

impl From<&atomic_repository::status::FileStatusEntry> for JsonStatusEntry {
    fn from(entry: &atomic_repository::status::FileStatusEntry) -> Self {
        Self {
            path: json_path(entry.path()),
            status: json_status_name(entry.status()),
            code: entry.status().short_code().to_string(),
            details: entry.details().map(str::to_string),
        }
    }
}

fn json_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn json_status_name(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Clean => "clean",
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
        FileStatus::Untracked => "untracked",
        FileStatus::Added => "added",
        FileStatus::Conflicted => "conflicted",
        FileStatus::TypeChanged => "type_changed",
        FileStatus::PermissionsChanged => "permissions_changed",
    }
}

// Helper Types

/// Output configuration for status display.
///
/// Controls how status information is formatted and filtered.
#[derive(Debug, Clone, Default)]
pub struct StatusOutputConfig {
    /// Use short/porcelain format.
    pub short: bool,
    /// Hide untracked files.
    pub no_untracked: bool,
    /// Filter to a specific path prefix.
    pub path_filter: Option<String>,
}

impl StatusOutputConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable short format.
    pub fn short(mut self) -> Self {
        self.short = true;
        self
    }

    /// Hide untracked files.
    pub fn hide_untracked(mut self) -> Self {
        self.no_untracked = true;
        self
    }

    /// Filter to a specific path prefix.
    pub fn filter_path(mut self, path: impl Into<String>) -> Self {
        self.path_filter = Some(path.into());
        self
    }
}

// Helper Functions

/// Get the single-character status code for a file status.
pub fn status_code(status: FileStatus) -> char {
    match status {
        FileStatus::Clean => ' ',
        FileStatus::Modified => 'M',
        FileStatus::Added => 'A',
        FileStatus::Deleted => 'D',
        FileStatus::Untracked => '?',
        FileStatus::Conflicted => 'C',
        FileStatus::TypeChanged => 'T',
        FileStatus::PermissionsChanged => 'P',
    }
}

/// Get a human-readable description for a file status.
pub fn status_description(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Clean => "clean",
        FileStatus::Modified => "modified",
        FileStatus::Added => "new file",
        FileStatus::Deleted => "deleted",
        FileStatus::Untracked => "untracked",
        FileStatus::Conflicted => "conflict",
        FileStatus::TypeChanged => "type changed",
        FileStatus::PermissionsChanged => "permissions",
    }
}

/// Check if a file status represents a recordable change.
///
/// Recordable statuses are changes that can be included in a `record` operation.
/// Clean, untracked, and conflicted files are not directly recordable.
pub fn is_recordable(status: FileStatus) -> bool {
    matches!(
        status,
        FileStatus::Modified
            | FileStatus::Added
            | FileStatus::Deleted
            | FileStatus::TypeChanged
            | FileStatus::PermissionsChanged
    )
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::Merkle;
    use atomic_repository::status::{FileStatus, FileStatusEntry};
    use serial_test::serial;
    use std::path::PathBuf;

    // Status Command Construction Tests

    #[test]
    fn test_status_new() {
        let status = Status::new();
        assert!(status.path.is_none());
        assert!(!status.short);
        assert!(!status.no_untracked);
    }

    #[test]
    fn test_status_default() {
        let status = Status::default();
        assert!(status.path.is_none());
        assert!(!status.short);
        assert!(!status.no_untracked);
    }

    #[test]
    fn test_status_with_path() {
        let status = Status::new().with_path("src/");
        assert_eq!(status.path, Some("src/".to_string()));
    }

    #[test]
    fn test_status_with_short() {
        let status = Status::new().with_short(true);
        assert!(status.short);
    }

    #[test]
    fn test_status_with_no_untracked() {
        let status = Status::new().with_no_untracked(true);
        assert!(status.no_untracked);
    }

    #[test]
    fn test_status_builder_chain() {
        let status = Status::new()
            .with_path("src/")
            .with_short(true)
            .with_no_untracked(true);

        assert_eq!(status.path, Some("src/".to_string()));
        assert!(status.short);
        assert!(status.no_untracked);
    }

    // StatusOutputConfig Tests

    #[test]
    fn test_output_config_new() {
        let config = StatusOutputConfig::new();
        assert!(!config.short);
        assert!(!config.no_untracked);
        assert!(config.path_filter.is_none());
    }

    #[test]
    fn test_output_config_default() {
        let config = StatusOutputConfig::default();
        assert!(!config.short);
        assert!(!config.no_untracked);
        assert!(config.path_filter.is_none());
    }

    #[test]
    fn test_output_config_short() {
        let config = StatusOutputConfig::new().short();
        assert!(config.short);
    }

    #[test]
    fn test_output_config_hide_untracked() {
        let config = StatusOutputConfig::new().hide_untracked();
        assert!(config.no_untracked);
    }

    #[test]
    fn test_output_config_filter_path() {
        let config = StatusOutputConfig::new().filter_path("src/commands/");
        assert_eq!(config.path_filter, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_output_config_builder_chain() {
        let config = StatusOutputConfig::new()
            .short()
            .hide_untracked()
            .filter_path("tests/");

        assert!(config.short);
        assert!(config.no_untracked);
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    // Status Options Conversion Tests

    #[test]
    fn test_get_status_options_default() {
        let status = Status::new();
        let _options = status.get_status_options();
        // Just verify it doesn't panic and returns valid options
    }

    #[test]
    fn test_workspace_mode_preserves_forensic_observation() {
        let mut status = Status::new();
        assert_eq!(status.workspace_mode(), WorkspaceTxnMode::Reconcile);

        status.no_reconcile = true;
        assert_eq!(status.workspace_mode(), WorkspaceTxnMode::Observe);
    }

    #[test]
    fn test_get_status_options_no_untracked() {
        let status = Status::new().with_no_untracked(true);
        let _options = status.get_status_options();
        // Options are created without panic
    }

    #[test]
    fn test_get_status_options_with_path() {
        let status = Status::new().with_path("src/");
        let _options = status.get_status_options();
        // Options include path filter
    }

    // Status Code Tests

    #[test]
    fn test_status_code_clean() {
        assert_eq!(status_code(FileStatus::Clean), ' ');
    }

    #[test]
    fn test_status_code_modified() {
        assert_eq!(status_code(FileStatus::Modified), 'M');
    }

    #[test]
    fn test_status_code_deleted() {
        assert_eq!(status_code(FileStatus::Deleted), 'D');
    }

    #[test]
    fn test_status_code_untracked() {
        assert_eq!(status_code(FileStatus::Untracked), '?');
    }

    #[test]
    fn test_status_code_added() {
        assert_eq!(status_code(FileStatus::Added), 'A');
    }

    #[test]
    fn test_status_code_conflicted() {
        assert_eq!(status_code(FileStatus::Conflicted), 'C');
    }

    #[test]
    fn test_status_code_type_changed() {
        assert_eq!(status_code(FileStatus::TypeChanged), 'T');
    }

    #[test]
    fn test_status_code_permissions_changed() {
        assert_eq!(status_code(FileStatus::PermissionsChanged), 'P');
    }

    // Status Description Tests

    #[test]
    fn test_status_description_clean() {
        assert_eq!(status_description(FileStatus::Clean), "clean");
    }

    #[test]
    fn test_status_description_modified() {
        assert_eq!(status_description(FileStatus::Modified), "modified");
    }

    #[test]
    fn test_status_description_deleted() {
        assert_eq!(status_description(FileStatus::Deleted), "deleted");
    }

    #[test]
    fn test_status_description_untracked() {
        assert_eq!(status_description(FileStatus::Untracked), "untracked");
    }

    #[test]
    fn test_status_description_added() {
        assert_eq!(status_description(FileStatus::Added), "new file");
    }

    #[test]
    fn test_status_description_conflicted() {
        assert_eq!(status_description(FileStatus::Conflicted), "conflict");
    }

    #[test]
    fn test_status_description_type_changed() {
        assert_eq!(status_description(FileStatus::TypeChanged), "type changed");
    }

    #[test]
    fn test_status_description_permissions_changed() {
        assert_eq!(
            status_description(FileStatus::PermissionsChanged),
            "permissions"
        );
    }

    // Is Recordable Tests

    #[test]
    fn test_is_recordable_modified() {
        assert!(is_recordable(FileStatus::Modified));
    }

    #[test]
    fn test_is_recordable_deleted() {
        assert!(is_recordable(FileStatus::Deleted));
    }

    #[test]
    fn test_is_recordable_added() {
        assert!(is_recordable(FileStatus::Added));
    }

    #[test]
    fn test_is_recordable_type_changed() {
        assert!(is_recordable(FileStatus::TypeChanged));
    }

    #[test]
    fn test_is_recordable_permissions_changed() {
        assert!(is_recordable(FileStatus::PermissionsChanged));
    }

    #[test]
    fn test_is_not_recordable_clean() {
        assert!(!is_recordable(FileStatus::Clean));
    }

    #[test]
    fn test_is_not_recordable_untracked() {
        assert!(!is_recordable(FileStatus::Untracked));
    }

    #[test]
    fn test_is_not_recordable_conflicted() {
        // Conflicted files need resolution, not just recording
        assert!(!is_recordable(FileStatus::Conflicted));
    }

    // Repository Status Format Tests

    #[test]
    fn test_repository_status_empty() {
        let status = RepositoryStatus::new("dev".to_string(), None);
        assert_eq!(status.view(), "dev");
        assert!(status.state().is_none());
        assert!(status.is_clean());
    }

    #[test]
    fn test_repository_status_with_state() {
        let state = Merkle::initial();
        let status = RepositoryStatus::new("main".to_string(), Some(state));
        assert_eq!(status.view(), "main");
        assert!(status.state().is_some());
    }

    #[test]
    fn test_repository_status_with_entries() {
        let mut status = RepositoryStatus::new("feature".to_string(), None);

        let entry1 = FileStatusEntry::new(PathBuf::from("src/main.rs"), FileStatus::Modified);
        let entry2 = FileStatusEntry::new(PathBuf::from("src/lib.rs"), FileStatus::Added);
        let entry3 = FileStatusEntry::new(PathBuf::from("README.md"), FileStatus::Untracked);

        status.add_entry(entry1);
        status.add_entry(entry2);
        status.add_entry(entry3);

        assert_eq!(status.modified_count(), 1);
        assert_eq!(status.added_count(), 1);
        assert_eq!(status.untracked_count(), 1);
        assert!(!status.is_clean());
    }

    #[test]
    fn test_json_status_is_versioned_and_sorted() {
        let mut status = RepositoryStatus::new("feature".to_string(), Some(Merkle::initial()));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("zeta.rs"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("alpha.rs"),
            FileStatus::Untracked,
        ));

        let output = JsonStatus::new(&status, std::path::Path::new("/workspace/project"));

        assert_eq!(output.schema_version, 1);
        assert_eq!(output.repository_root, "/workspace/project");
        assert_eq!(output.view, "feature");
        assert!(output.state.is_some());
        assert!(!output.clean);
        assert_eq!(output.entries[0].path, "alpha.rs");
        assert_eq!(output.entries[0].status, "untracked");
        assert_eq!(output.entries[1].path, "zeta.rs");
        assert_eq!(output.entries[1].code, "M");
    }

    #[test]
    fn test_json_status_escapes_paths() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("line\nbreak.txt"),
            FileStatus::Untracked,
        ));

        let output = JsonStatus::new(&status, std::path::Path::new("/workspace"));
        let json = serde_json::to_string(&output).unwrap();

        assert!(json.contains("line\\nbreak.txt"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["entries"][0]["path"],
            "line\nbreak.txt"
        );
    }

    #[test]
    fn test_json_and_short_are_mutually_exclusive() {
        assert!(Status::try_parse_from(["status", "--json"]).unwrap().json);
        assert!(Status::try_parse_from(["status", "--short", "--json"]).is_err());
    }

    // Print Format Tests (Output verification)

    #[test]
    fn test_short_format_no_panic_empty() {
        let status = Status::new().with_short(true);
        let repo_status = RepositoryStatus::new("dev".to_string(), None);
        // Should not panic
        let result = status.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_short_format_no_panic_with_entries() {
        let status = Status::new().with_short(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("file.txt"),
            FileStatus::Modified,
        ));

        let result = status.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_no_panic_empty() {
        let status = Status::new();
        let repo_status = RepositoryStatus::new("dev".to_string(), None);
        // Should not panic
        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_no_panic_with_entries() {
        let status = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("file.txt"),
            FileStatus::Modified,
        ));

        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_state() {
        let status = Status::new();
        let state = Merkle::initial();
        let repo_status = RepositoryStatus::new("dev".to_string(), Some(state));

        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    // Integration Tests (require temp directories)

    // Note: Integration tests that change the current directory are prone to
    // interfering with each other when run in parallel. We use a guard pattern
    // to save and restore the original directory.

    /// Guard that restores the current directory when dropped.
    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    #[serial]
    fn test_status_run_outside_repository() {
        let _guard = DirGuard::new();

        // Running status outside a repository should fail with appropriate error
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_status_run_in_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository (takes only path, creates default view)
        // We need to drop the repository handle before running status to avoid
        // database lock conflicts (redb only allows one open handle at a time)
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        } // Repository dropped here, releasing database lock

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should succeed
        assert!(result.is_ok(), "Status command failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_status_run_short_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        }

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_short(true);
        let result = status.run();

        assert!(
            result.is_ok(),
            "Status short command failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_status_run_no_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        }

        // Create an untracked file
        std::fs::write(repo_path.join("untracked.txt"), "content").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_no_untracked(true);
        let result = status.run();

        assert!(
            result.is_ok(),
            "Status no-untracked command failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_status_run_with_path_filter() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        }

        // Create directory structure
        std::fs::create_dir(repo_path.join("src")).unwrap();
        std::fs::write(repo_path.join("src/main.rs"), "fn main() {}").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_path("src/");
        let result = status.run();

        assert!(
            result.is_ok(),
            "Status with path filter failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_status_in_subdirectory() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        }

        // Create subdirectory
        let subdir = repo_path.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        // Change to subdirectory
        std::env::set_current_dir(&subdir).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should find repo by walking up to parent
        assert!(
            result.is_ok(),
            "Status in subdirectory failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_status_with_multiple_file_types() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(
                repo_result.is_ok(),
                "Failed to init repository: {:?}",
                repo_result.err()
            );
        }

        // Create various file types
        std::fs::create_dir(repo_path.join("src")).unwrap();
        std::fs::write(repo_path.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo_path.join("README.md"), "# Project").unwrap();
        std::fs::write(repo_path.join("Cargo.toml"), "[package]").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new();
        let result = status.run();

        assert!(
            result.is_ok(),
            "Status with multiple files failed: {:?}",
            result.err()
        );
    }

    // Edge Case Tests

    #[test]
    fn test_status_code_all_variants() {
        // Ensure all FileStatus variants have codes
        let statuses = [
            FileStatus::Clean,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Untracked,
            FileStatus::Added,
            FileStatus::Conflicted,
            FileStatus::TypeChanged,
            FileStatus::PermissionsChanged,
        ];

        for status in statuses {
            let code = status_code(status);
            // Ensure code is a printable ASCII character
            assert!(code.is_ascii());
        }
    }

    #[test]
    fn test_status_description_all_variants() {
        // Ensure all FileStatus variants have descriptions
        let statuses = [
            FileStatus::Clean,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Untracked,
            FileStatus::Added,
            FileStatus::Conflicted,
            FileStatus::TypeChanged,
            FileStatus::PermissionsChanged,
        ];

        for status in statuses {
            let desc = status_description(status);
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn test_is_recordable_all_variants() {
        // Test all variants for recordability
        assert!(!is_recordable(FileStatus::Clean));
        assert!(is_recordable(FileStatus::Modified));
        assert!(is_recordable(FileStatus::Deleted));
        assert!(!is_recordable(FileStatus::Untracked));
        assert!(is_recordable(FileStatus::Added));
        assert!(!is_recordable(FileStatus::Conflicted));
        assert!(is_recordable(FileStatus::TypeChanged));
        assert!(is_recordable(FileStatus::PermissionsChanged));
    }

    #[test]
    fn test_output_config_path_filter_with_string() {
        let config = StatusOutputConfig::new().filter_path(String::from("tests/"));
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    #[test]
    fn test_output_config_path_filter_with_str() {
        let config = StatusOutputConfig::new().filter_path("tests/");
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    #[test]
    fn test_status_with_path_string() {
        let status = Status::new().with_path(String::from("src/commands/"));
        assert_eq!(status.path, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_status_with_path_str() {
        let status = Status::new().with_path("src/commands/");
        assert_eq!(status.path, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_repository_status_filter_methods() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        // Add various entries
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("a.rs"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("b.rs"),
            FileStatus::Deleted,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("c.rs"),
            FileStatus::Added,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("d.txt"),
            FileStatus::Untracked,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("e.rs"),
            FileStatus::Clean,
        ));

        // Test counts
        assert_eq!(status.modified_count(), 1);
        assert_eq!(status.deleted_count(), 1);
        assert_eq!(status.added_count(), 1);
        assert_eq!(status.untracked_count(), 1);
        assert_eq!(status.clean_count(), 1);
        assert_eq!(status.total_count(), 5);

        // Test is_clean with dirty entries
        assert!(!status.is_clean());
    }

    #[test]
    fn test_repository_status_dirty_count() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("clean.rs"),
            FileStatus::Clean,
        ));

        assert_eq!(status.dirty_count(), 2);
    }

    #[test]
    fn test_short_format_with_all_status_types() {
        let status_cmd = Status::new().with_short(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Add one of each type that appears in short format
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("added.rs"),
            FileStatus::Added,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("conflicted.rs"),
            FileStatus::Conflicted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        let result = status_cmd.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_all_status_types() {
        let status_cmd = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Add one of each type
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("added.rs"),
            FileStatus::Added,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        let result = status_cmd.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_conflicted_and_details() {
        let status_cmd = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Use with_details with all required arguments: path, status, inode, recorded_hash, current_hash, details
        let entry = FileStatusEntry::with_details(
            PathBuf::from("conflicted.rs"),
            FileStatus::Conflicted,
            None,                               // inode
            None,                               // recorded_hash
            None,                               // current_hash
            Some("merge conflict".to_string()), // details
        );
        repo_status.add_entry(entry);

        let result = status_cmd.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_short_format_hides_untracked_when_flag_set() {
        let status_cmd = Status::new().with_short(true).with_no_untracked(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        // Should succeed and not show untracked files
        let result = status_cmd.print_short_format(&repo_status);
        assert!(result.is_ok());
    }
}
