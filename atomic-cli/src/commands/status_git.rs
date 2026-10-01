//! CB-11A: versioned Git-inspired `status --git` rendering.
//!
//! This is a deliberately bounded, versioned subset (RFC §9.1/§9.2): it
//! distinguishes the staged layer (Git baseline → index) from the unstaged
//! layer (index → worktree) with two-column codes, decorates Git-informed
//! notices, reports shelves under a separate heading, and never claims full
//! Git porcelain compatibility. Machine-readable output carries the format
//! version, bridge state, and origin.
//!
//! The command is observation-only: it enters the workspace boundary in
//! Observe mode, so it never journals reconcile operations, never
//! materializes, and never mutates durable tracking. When the boundary
//! returns a remediation (for example a Git merge in progress), the
//! Git-informed picture is still reported; the durable layer is simply not
//! claimed.

use std::path::{Path, PathBuf};

use atomic_core::types::Base32;
use atomic_repository::{
    check_ignore_policy, git_object_id_hex, observe_git_staging_state, observe_staging_state,
    quote_path, ConversionPolicy, StageCode, StagingEntry, StagingNotice, StagingState,
};
use atomic_repository::{Repository, WorkspaceRemediation, WorkspaceTxnStart};

use crate::commands::git::parallel::conversion_policy;
use crate::error::{CliError, CliResult};
use crate::output::print_blank;

/// Version of the machine-readable `status --git --json` document.
pub const GIT_STATUS_JSON_VERSION: u32 = 1;

/// Render `status --git` for the repository at `repo_root`.
pub fn run_git_status(repo_root: &Path, json: bool) -> CliResult<()> {
    let mut repo =
        Repository::open_readonly(repo_root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;

    // Observe-only boundary: --git never journals or mutates.
    let boundary = repo
        .begin_workspace_txn(atomic_repository::WorkspaceTxnMode::Observe)
        .map_err(CliError::Repository)?;

    let git_repo = open_git(repo_root)?;
    let policy = conversion_policy(&git_repo)?;

    let (staging, bridge, remediation_notice) = match boundary {
        WorkspaceTxnStart::Ready(workspace) => {
            let working_copy = workspace.working_copy();
            let staging = observe_staging_state(&repo, working_copy, &policy)
                .map_err(|error| CliError::Internal(error.into()))?;
            let bridge = BridgeSummary {
                state: "aligned".to_string(),
                origin: if workspace.checkpoint().is_some() {
                    "checkpoint".to_string()
                } else {
                    "observed".to_string()
                },
                view: workspace.view().name.clone(),
                atomic_state: workspace.view().state.to_string(),
                git_head: workspace
                    .checkpoint()
                    .map(|checkpoint| checkpoint.git_head.clone()),
                git_tree: workspace
                    .checkpoint()
                    .map(|checkpoint| checkpoint.git_tree.clone()),
            };
            (staging, bridge, None)
        }
        WorkspaceTxnStart::Remediation(remediation) => {
            let mut staging = observe_git_staging_state(repo_root, &policy)
                .map_err(|error| CliError::Internal(error.into()))?;
            // The remediation notice is rendered once; drop the duplicate
            // operation-in-progress notice the raw observation produced.
            staging
                .notices
                .retain(|notice| !matches!(notice, StagingNotice::GitOperationInProgress { .. }));
            let head = git_repo.head().ok().and_then(|head| head.target());
            let view = repo
                .require_working_copy_id()
                .ok()
                .and_then(|working_copy| repo.desired_view_name(working_copy).ok());
            let bridge = BridgeSummary {
                state: "remediation".to_string(),
                origin: "observed".to_string(),
                view: view.unwrap_or_default(),
                atomic_state: String::new(),
                git_head: head.map(|oid| oid.to_string()),
                git_tree: None,
            };
            let notice = match &remediation {
                WorkspaceRemediation::GitOperationInProgress {
                    repository_state,
                    conflict_stages,
                    ..
                } => {
                    let mut text = format!("Git operation in progress ({repository_state})");
                    if !conflict_stages.is_empty() {
                        text.push_str(&format!(
                            "; index holds {} conflicted stage entries",
                            conflict_stages.len()
                        ));
                    }
                    Some(text)
                }
                other => Some(format!(
                    "workspace requires attention: {}",
                    describe_remediation(other)
                )),
            };
            (staging, bridge, notice)
        }
    };

    let ignore = check_ignore_policy(repo_root).map_err(|error| CliError::GitError {
        message: error.to_string(),
    })?;
    let shelved = collect_shelved_artifacts(repo_root, &bridge.view);

    if json {
        let document = git_status_json(
            &staging,
            &bridge,
            &ignore,
            &shelved,
            remediation_notice.as_deref(),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&document)
                .map_err(|error| CliError::Internal(error.into()))?
        );
        return Ok(());
    }

    // `git status --short` prints a line only when a layer relation
    // differs; fully-unmodified entries (including flagged
    // skip-worktree/assume-unchanged paths) are omitted.
    for entry in &staging.entries {
        if entry.x == StageCode::Unmodified && entry.y == StageCode::Unmodified {
            continue;
        }
        println!("{}", atomic_repository::format_two_column(entry));
    }

    let mut printed_note = false;
    if let Some(notice) = &remediation_notice {
        print_blank();
        println!("Note: {notice}");
        printed_note = true;
    }
    for notice in describe_notices(&staging.notices) {
        if !printed_note {
            print_blank();
            printed_note = true;
        }
        println!("Note: {notice}");
    }
    if ignore.diverges() {
        if !printed_note {
            print_blank();
            printed_note = true;
        }
        println!(
            "Note: ignore-policy divergence: .atomicignore pattern(s) not represented in Git ignore sources (consent required to mirror into .git/info/exclude): {}",
            ignore.unmirrored.join(", ")
        );
    }
    if !shelved.is_empty() {
        if !printed_note {
            print_blank();
        }
        println!("Shelved (excluded from view status; swapped on view change):");
        for path in &shelved {
            println!("  {path}");
        }
    }
    Ok(())
}

struct BridgeSummary {
    state: String,
    origin: String,
    view: String,
    atomic_state: String,
    git_head: Option<String>,
    git_tree: Option<String>,
}

fn describe_remediation(remediation: &WorkspaceRemediation) -> String {
    match remediation {
        WorkspaceRemediation::GitOperationInProgress { .. } => {
            "Git operation in progress".to_string()
        }
        WorkspaceRemediation::Unanchored { state, .. } => {
            format!("workspace is not anchored to a verified Git baseline: {state:?}")
        }
        WorkspaceRemediation::OperationHeadsDiverged { heads } => format!(
            "operation heads diverged: {}",
            heads
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        WorkspaceRemediation::ConcurrentGitMutation { attempts, .. } => {
            format!("Git state changed between observations across {attempts} attempt(s)")
        }
        WorkspaceRemediation::GitLocksBusy { reason, .. } => {
            format!("Git-owned transaction in flight ({reason})")
        }
    }
}

fn describe_notices(notices: &[StagingNotice]) -> Vec<String> {
    let mut output = Vec::new();
    for notice in notices {
        match notice {
            StagingNotice::GitOperationInProgress { state } => {
                output.push(format!("Git operation in progress ({state})"));
            }
            StagingNotice::UnmergedIndexEntries { paths } => {
                output.push(format!(
                    "Git merge conflict: index holds unmerged entries for {}",
                    paths.join(", ")
                ));
            }
            StagingNotice::SparseIndexEntries { paths } => {
                output.push(format!(
                    "sparse index: {} path(s) covered by unexpanded directory entries, observed via in-memory expansion (sparse absence is not deletion): {}",
                    paths.len(),
                    paths.join(", ")
                ));
            }
            StagingNotice::FlaggedEntries {
                skip_worktree,
                assume_unchanged,
            } => {
                if !skip_worktree.is_empty() {
                    output.push(format!(
                        "skip-worktree entries exempt from worktree comparison: {}",
                        skip_worktree.join(", ")
                    ));
                }
                if !assume_unchanged.is_empty() {
                    output.push(format!(
                        "assume-unchanged entries exempt from worktree comparison: {}",
                        assume_unchanged.join(", ")
                    ));
                }
            }
            StagingNotice::IntentToAdd { paths } => {
                output.push(format!(
                    "intent-to-add: content not staged for {}",
                    paths.join(", ")
                ));
            }
            StagingNotice::AlternateIndex { path } => {
                output.push(format!("alternate index observed (GIT_INDEX_FILE): {path}"));
            }
            StagingNotice::CommittedConflictSnapshot { head } => {
                output.push(format!(
                    "committed conflict snapshot at HEAD {head}; Git status is clean by design (markers are ordinary content, not unmerged index state)"
                ));
            }
        }
    }
    output
}

fn collect_shelved_artifacts(repo_root: &Path, view: &str) -> Vec<String> {
    let dot_dir = repo_root.join(".atomic");
    let mut roots = vec![
        // Per-working-copy shelf artifacts take precedence when present.
        dot_dir.join("working-copies"),
        dot_dir.join("workspaces").join(view),
    ];
    let mut output = Vec::new();
    let working_copy_root = roots.remove(0);
    // Enumerate every working copy's shelf for this view plus the legacy
    // per-view directory, deterministically.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&working_copy_root) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("workspaces").join(view);
            if candidate.is_dir() {
                candidates.push(candidate);
            }
        }
    }
    let legacy = roots.remove(0);
    if legacy.is_dir() {
        candidates.push(legacy);
    }
    candidates.sort();
    for candidate in candidates {
        collect_files(&candidate, &candidate, &mut output, 0);
    }
    output.sort();
    output.dedup();
    output
}

fn collect_files(base: &Path, directory: &Path, output: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, output, depth + 1);
        } else {
            let relative = path
                .strip_prefix(base)
                .map(Path::to_string_lossy)
                .unwrap_or_default();
            output.push(relative.replace('\\', "/"));
        }
    }
}

fn stage_code_string(code: StageCode) -> String {
    code.glyph().to_string()
}

fn entry_json(entry: &StagingEntry) -> serde_json::Value {
    serde_json::json!({
        "path": quote_path(entry.path.as_bytes()),
        "x": stage_code_string(entry.x),
        "y": stage_code_string(entry.y),
        "durable_tracked": entry.durable_tracked,
        "intent_to_add": entry.intent_to_add,
        "skip_worktree": entry.skip_worktree,
        "assume_unchanged": entry.assume_unchanged,
        "unmerged": entry.unmerged,
    })
}

fn git_status_json(
    staging: &StagingState,
    bridge: &BridgeSummary,
    ignore: &atomic_repository::IgnorePolicyReport,
    shelved: &[String],
    remediation_notice: Option<&str>,
) -> serde_json::Value {
    let staged: Vec<serde_json::Value> = staging
        .entries
        .iter()
        .filter(|entry| entry.x != StageCode::Unmodified)
        .map(entry_json)
        .collect();
    let unstaged: Vec<serde_json::Value> = staging
        .entries
        .iter()
        .filter(|entry| entry.y != StageCode::Unmodified)
        .map(entry_json)
        .collect();
    let untracked: Vec<String> = staging
        .untracked()
        .map(|entry| quote_path(entry.path.as_bytes()))
        .collect();
    let mut notices = describe_notices(&staging.notices);
    if let Some(remediation_notice) = remediation_notice {
        notices.push(remediation_notice.to_string());
    }
    if ignore.diverges() {
        notices.push(format!(
            "ignore-policy divergence: {} unmirrored .atomicignore pattern(s): {}",
            ignore.unmirrored.len(),
            ignore.unmirrored.join(", ")
        ));
    }

    serde_json::json!({
        "format": "atomic-status-git",
        "version": GIT_STATUS_JSON_VERSION,
        "clean": staging.is_clean(),
        "bridge": {
            "state": bridge.state,
            "origin": bridge.origin,
            "view": bridge.view,
            "atomic_state": bridge.atomic_state,
            "git_head": bridge.git_head,
            "git_tree": bridge.git_tree,
        },
        "layers": {
            "baseline_tree": staging.baseline_tree.as_ref().map(git_object_id_hex),
            "index_tree": staging.index_tree.as_ref().map(git_object_id_hex),
            "durable_manifest_root": staging.durable_manifest_root.as_ref().map(|root| root.content_key.clone()),
            "snapshot": staging.snapshot.as_ref().map(|hash| hash.to_base32()),
            "remainder": staging.remainder.as_ref().map(|hash| hash.to_base32()),
        },
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
        "notices": notices,
        "shelved": shelved,
        "ignore_policy": {
            "divergence": ignore.diverges(),
            "unmirrored": ignore.unmirrored,
            "managed_block_present": ignore.managed_block_present,
        },
    })
}

fn open_git(repo_root: &Path) -> CliResult<git2::Repository> {
    git2::Repository::open(repo_root).map_err(|error| CliError::GitError {
        message: format!("cannot open Git repository: {error}"),
    })
}

/// Overlay Git-informed classifications onto native status output.
///
/// This never mutates durable tracking. It only adjusts what native status
/// *displays* for paths whose Git index relation differs from durable
/// tracking, so the golden §9.2 rows hold without conflating layers:
///
/// - index stage-0 paths that are not durably tracked are displayed as
///   `Added` with an explicit "staged via Git index" detail;
/// - index stages 1-3 (unmerged) are displayed as `Conflicted` with a
///   Git merge detail.
pub(crate) fn apply_git_informed_overlays(
    repo: &Repository,
    repo_root: &Path,
    status: &mut atomic_repository::RepositoryStatus,
) -> CliResult<()> {
    // Git-informed overlays are colocated bridge behavior (RFC §9); native
    // status of a working copy that never enrolled stays Git-independent.
    if !repo_root.join(".git").exists()
        || !repo
            .bridge_workspace_active()
            .map_err(CliError::Repository)?
    {
        return Ok(());
    }
    let git_repo = open_git(repo_root)?;
    let policy = conversion_policy(&git_repo)?;
    let index = atomic_repository::observe_git_index(repo_root, &policy).map_err(|error| {
        CliError::GitError {
            message: error.to_string(),
        }
    })?;

    let durably_tracked: std::collections::BTreeSet<String> = repo
        .list_tracked_files()
        .map_err(|e| CliError::Internal(e.into()))?
        .iter()
        .filter(|file| !file.is_directory)
        .map(|file| file.path.display().to_string())
        .collect();

    let mut unmerged: Vec<String> = Vec::new();
    let mut stage_zero_indexed: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for entry in &index.entries {
        let path = String::from_utf8_lossy(entry.path.as_bytes()).into_owned();
        if entry.stage == 0 {
            stage_zero_indexed.insert(path);
        } else {
            unmerged.push(path);
        }
    }
    unmerged.sort();
    unmerged.dedup();

    // Index-new paths displayed as pending additions (Git-index tracking).
    for path in &stage_zero_indexed {
        if durably_tracked.contains(path) {
            continue;
        }
        let mut entry = atomic_repository::FileStatusEntry::new(
            PathBuf::from(path),
            atomic_repository::FileStatus::Added,
        );
        entry.set_details(
            "staged in Git index; not durably tracked (run `atomic add` for durable tracking)"
                .to_string(),
        );
        status.add_or_replace_entry(entry);
    }

    // Unmerged index entries displayed as Git merge conflicts.
    for path in &unmerged {
        let mut entry = atomic_repository::FileStatusEntry::new(
            PathBuf::from(path),
            atomic_repository::FileStatus::Conflicted,
        );
        entry.set_details("Git merge conflict (index stages 1-3)".to_string());
        status.add_or_replace_entry(entry);
    }
    Ok(())
}
