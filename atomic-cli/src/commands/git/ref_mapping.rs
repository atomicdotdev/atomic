//! Mapping-driven ref reconciliation (RFC §8.1/§8.5, CB-10A).
//!
//! The CLI owns Git observation through its own handle; the repository layer
//! owns the persisted mapping and the closure evidence. This module glues the
//! two: build/refresh the mapping under the journaled `RefMapping` operation,
//! classify the three-way matrix, and expose the status/publish lifecycle
//! surfaces.

use atomic_core::pristine::{RefMapping, RefSyncStatus, ViewTxnT, ViewScope, REF_MAPPING_VERSION};
use atomic_core::types::Base32;
use atomic_core::WorkingCopyId;
use atomic_repository::repository::ref_mapping::{
    classify_three_way, commits_added_since, git_contains_atomic_export, is_ephemeral_view_name,
    mapped_local_ref, AddedCommits, RefMappingDecision, ThreeWayObservation,
};
pub(crate) use atomic_repository::repository::ref_mapping::{Containment, ThreeWayAction};
use crate::commands::git::bridge::{journal_and_create_branch_cas_with_mapping, journal_and_move_branch};
use crate::commands::workspace_txn::enter_workspace;
use atomic_repository::{Repository, WorkspaceTxnMode};
use git2::Repository as GitRepository;

use crate::error::{CliError, CliResult};

/// The mapped local ref for `view` under the §8.1 scope policy.
///
/// Shared → `refs/heads/<name>` (symbolic HEAD), Draft →
/// `refs/atomic/views/<name>` (detached HEAD), ephemeral `git/<oid>` →
/// `None` (no extra ref beyond the original commit).
pub(crate) fn mapped_ref_for_view(repo: &Repository, view: &str) -> CliResult<Option<String>> {
    if is_ephemeral_view_name(view) {
        return Ok(None);
    }
    let scope = repo.get_view_info(view).map_err(CliError::from)?.scope;
    Ok(mapped_local_ref(scope, view))
}

/// The durable internal view id for `view` (mapping key identity).
fn durable_view_id(repo: &Repository, view: &str) -> CliResult<u64> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|error| CliError::Repository(atomic_repository::RepositoryError::Database(error.to_string())))?;
    txn.get_view(view)
        .map_err(|error| {
            CliError::Repository(atomic_repository::RepositoryError::Database(error.to_string()))
        })?
        .map(|view_state| view_state.id)
        .ok_or(CliError::ViewNotFound {
            name: view.to_string(),
        })
}

fn observed_tip(git: &GitRepository, local_ref: Option<&str>) -> Option<String> {
    local_ref.and_then(|reference| {
        git.find_reference(reference)
            .ok()
            .and_then(|reference| reference.target())
            .map(|oid| oid.to_string())
    })
}

/// The live tip of `view`'s mapped local ref (hex), or `None`.
///
/// CB-10A review R6: the ref NAME comes from the PERSISTED published
/// mapping row when one exists (an explicit publish/rename reconciled it),
/// not from a recomputed scope policy that can disagree with the durable
/// row.
pub(crate) fn mapped_ref_tip(
    git: &GitRepository,
    repo: &Repository,
    view: &str,
) -> Option<String> {
    let persisted = repo
        .get_ref_mapping(view)
        .ok()
        .flatten()
        .and_then(|mapping| mapping.local_ref.clone());
    let local_ref = match persisted {
        Some(local_ref) => local_ref,
        None => mapped_ref_for_view(repo, view).ok()??,
    };
    observed_tip(git, Some(&local_ref))
}

/// Load the persisted mapping for `view`, creating the Synchronized baseline
/// from the live observation when absent (journaled, CB-10A task 1).
pub(crate) fn ensure_baseline_mapping(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    view: &str,
) -> CliResult<(RefMapping, bool)> {
    if let Some(existing) = repo.get_ref_mapping(view).map_err(CliError::from)? {
        return Ok((existing, false));
    }
    let info = repo.get_view_info(view).map_err(CliError::from)?;
    let local_ref = if is_ephemeral_view_name(view) {
        None
    } else {
        mapped_local_ref(info.scope, view)
    };
    let tip = observed_tip(git, local_ref.as_deref());
    let mapping = RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: durable_view_id(repo, view)?,
        view_name: view.to_string(),
        scope: info.scope as u8,
        local_ref,
        remote: None,
        // A fresh baseline observes both sides as they stand: the first
        // reconcile after enabling mapping bookkeeping is a no-op, never a
        // spurious import/export of already-aligned history.
        last_observed_local: tip.clone(),
        last_observed_remote: None,
        last_exported: tip,
        // A baseline observation is not a verified export: the export stays
        // unbound until a real projection writes it (CB-10A review R1).
        last_exported_state: None,
        last_observed_atomic: Some(info.state.to_base32()),
        status: RefSyncStatus::Synchronized,
    };
    // CB-10A review R6: the baseline creation is an atomic expected-absence
    // lease — a row another working copy wrote between the read above and
    // this write refuses closed instead of storing a stale baseline over it.
    repo.create_ref_mapping_expected_absent(working_copy, view, mapping.clone())
        .map_err(CliError::from)?;
    // CB-13C F3: the caller must know the run JOURNALED a mapping (a
    // mutation) so no later `Refused` terminal outcome is emitted after a
    // mutation — `Refused` promises before-any-mutation.
    Ok((mapping, true))
}

/// Record a fresh Synchronized observation for `view` after a successful
/// reconciliation side (import/export/no-op), preserving the remote pair.
///
/// `exported_tip` binds the export state (CB-10A review R1/R4): the caller
/// passes the exact verified tip of a real export/verified-alignment
/// transition, and the binding is written only when it still equals the live
/// mapped ref tip — otherwise the previous binding is preserved untouched.
///
/// CB-10A review R6: the lease covers the observation. When `common` is
/// `None` the repository-common operation lock is acquired here before the
/// mapping row is read (contention is a typed retry); a caller that already
/// holds it (the shadow projection) passes its guard and the replacement is
/// built and written under that same lease, pinned to the observed row.
pub(crate) fn refresh_mapping_observation(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    view: &str,
    exported_tip: Option<&str>,
    common: Option<&atomic_repository::RepositoryCommonLockGuard>,
) -> CliResult<()> {
    let held;
    let common = match common {
        Some(common) => common,
        None => {
            let Some(acquired) = repo
                .try_lock_ref_mapping_observation()
                .map_err(CliError::from)?
            else {
                return Err(git_error(
                    "another atomic git operation holds the repository lock; retry the reconcile",
                ));
            };
            held = acquired;
            &held
        }
    };
    let previous = repo.get_ref_mapping(view).map_err(CliError::from)?;
    let info = repo.get_view_info(view).map_err(CliError::from)?;
    // CB-10A: an existing mapping's local ref is authoritative (explicit
    // publish/rename reconciled it); only a fresh row falls back to the pure
    // scope policy.
    let local_ref = if is_ephemeral_view_name(view) {
        None
    } else {
        previous
            .as_ref()
            .and_then(|mapping| mapping.local_ref.clone())
            .or_else(|| mapped_local_ref(info.scope, view))
    };
    let tip = observed_tip(git, local_ref.as_deref());
    let state = info.state.to_base32();
    // The export binding is refreshed only from the verified transition's
    // exact observation (CB-10A review R4): the verified tip must still be
    // the live mapped ref tip, or the previous binding stays untouched.
    let (bound_export, bound_state) = match exported_tip {
        Some(verified) if Some(verified) == tip.as_ref().map(String::as_str) => {
            (Some(verified.to_string()), Some(state.clone()))
        }
        _ => (
            previous.as_ref().and_then(|mapping| mapping.last_exported.clone()),
            previous
                .as_ref()
                .and_then(|mapping| mapping.last_exported_state.clone()),
        ),
    };
    let mapping = RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: durable_view_id(repo, view)?,
        view_name: view.to_string(),
        scope: info.scope as u8,
        local_ref,
        remote: previous.as_ref().and_then(|mapping| mapping.remote.clone()),
        last_observed_local: tip.clone(),
        last_observed_remote: previous
            .as_ref()
            .and_then(|mapping| mapping.last_observed_remote.clone()),
        last_exported: bound_export,
        last_exported_state: bound_state,
        last_observed_atomic: Some(state),
        status: RefSyncStatus::Synchronized,
    };
    repo.set_ref_mapping_from_observation_with_common(
        working_copy,
        view,
        previous.as_ref(),
        Some(mapping),
        Some(&common),
    )
    .map_err(CliError::from)?;
    Ok(())
}

/// Record the remote observation of a completed mapped-ref push (CB-10B,
/// RFC §8.5): the remote tip was verified to equal the pushed commit, so the
/// mapping's `last_observed_remote` advances to it and the remote tracking
/// pair is created when absent. Written under the repository-common lease,
/// pinned to the observed row like every other mapping write. A caller that
/// already holds the common lock (the shadow pipeline) passes its guard.
pub(crate) fn record_remote_push_observation(
    repo: &Repository,
    working_copy: WorkingCopyId,
    view: &str,
    remote_name: &str,
    remote_ref: &str,
    new_remote_tip: &str,
    common: Option<&atomic_repository::RepositoryCommonLockGuard>,
) -> CliResult<()> {
    let held;
    let common = match common {
        Some(common) => common,
        None => {
            let Some(acquired) = repo
                .try_lock_ref_mapping_observation()
                .map_err(CliError::from)?
            else {
                return Err(git_error(
                    "another atomic git operation holds the repository lock; retry the push to record the remote observation",
                ));
            };
            held = acquired;
            &held
        }
    };
    let previous = repo.get_ref_mapping(view).map_err(CliError::from)?;
    let info = repo.get_view_info(view).map_err(CliError::from)?;
    let mapping = RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: durable_view_id(repo, view)?,
        view_name: view.to_string(),
        scope: info.scope as u8,
        local_ref: previous
            .as_ref()
            .and_then(|mapping| mapping.local_ref.clone())
            .or_else(|| mapped_local_ref(info.scope, view)),
        remote: Some((remote_name.to_string(), remote_ref.to_string())),
        last_observed_local: previous.as_ref().and_then(|mapping| mapping.last_observed_local.clone()),
        last_observed_remote: Some(new_remote_tip.to_string()),
        last_exported: previous.as_ref().and_then(|mapping| mapping.last_exported.clone()),
        last_exported_state: previous
            .as_ref()
            .and_then(|mapping| mapping.last_exported_state.clone()),
        last_observed_atomic: Some(info.state.to_base32()),
        status: RefSyncStatus::Synchronized,
    };
    repo.set_ref_mapping_from_observation_with_common(
        working_copy,
        view,
        previous.as_ref(),
        Some(mapping),
        Some(common),
    )
    .map_err(CliError::from)?;
    Ok(())
}

/// Classify the mapped view against live Git with closure-containment proofs.
pub(crate) fn classify_mapping_direction(
    repo: &Repository,
    git: &GitRepository,
    view: &str,
    mapping: &RefMapping,
) -> CliResult<RefMappingDecision> {
    let tip = mapping.local_ref.as_deref().and_then(|reference| {
        git.find_reference(reference)
            .ok()
            .and_then(|reference| reference.target())
            .map(|oid| oid.to_string())
    });
    let current_atomic = repo.get_view_info(view).map_err(CliError::from)?.state.to_base32();
    let observation = ThreeWayObservation {
        current_git: tip.clone(),
        current_atomic: current_atomic.clone(),
    };
    let added = commits_added_since(
        git,
        mapping.last_observed_local.as_deref(),
        tip.as_deref(),
    );
    // CB-10A review R2: only a complete walk can carry a containment proof;
    // an unprovable movement (missing tip, unreachable baseline, missing
    // object, budget exhaustion) is never a positive result.
    let no_added: Vec<String> = Vec::new();
    let (added_set, walk_complete) = match &added {
        AddedCommits::NoMovement => (no_added.as_slice(), true),
        AddedCommits::Added(commits) => (commits.as_slice(), true),
        AddedCommits::Unprovable(_) => (no_added.as_slice(), false),
    };
    let atomic_contains_git = if walk_complete {
        repo.atomic_contains_git_closure(view, added_set)
            .map_err(CliError::from)?
    } else {
        None
    };
    let git_contains_atomic = if walk_complete {
        git_contains_atomic_export(
            added_set,
            mapping.last_exported.as_deref(),
            mapping.last_exported_state.as_deref(),
            &current_atomic,
        )
    } else {
        None
    };
    Ok(classify_three_way(
        mapping,
        &observation,
        Containment {
            atomic_contains_git,
            git_contains_atomic,
        },
    ))
}

/// Persist the `Diverged` status without moving either side (RFC §8.5).
pub(crate) fn persist_diverged_status(
    repo: &Repository,
    working_copy: WorkingCopyId,
    mapping: &RefMapping,
) -> CliResult<()> {
    let mut diverged = mapping.clone();
    diverged.status = RefSyncStatus::Diverged;
    // Pinned to the classified row: a concurrent mapping move must not be
    // clobbered by this status write (CB-10A review R6).
    repo.set_ref_mapping_from_observation(
        working_copy,
        &mapping.view_name,
        Some(mapping),
        Some(diverged),
    )
    .map_err(CliError::from)?;
    Ok(())
}

/// The actionable remediation text for a mapping status (AC-2 diagnostics).
pub(crate) fn remediation_for(status: RefSyncStatus) -> &'static str {
    match status {
        RefSyncStatus::Diverged => {
            "resolve with 'atomic view insert' from the other side or 'git merge', then re-run 'atomic git bridge reconcile'"
        }
        RefSyncStatus::GitAhead => {
            "run 'atomic git bridge reconcile' to import the Git movement"
        }
        RefSyncStatus::AtomicAhead => {
            "run 'atomic git bridge reconcile' to export the Atomic projection"
        }
        RefSyncStatus::Synchronized => "no action needed",
        RefSyncStatus::Unrepresentable => {
            "the view no longer maps to a Git ref; republish via 'atomic git bridge publish' or 'git switch -c'"
        }
    }
}

/// Surface persisted ref-mapping divergence in `atomic status` (CB-10A AC-2):
/// an actionable Diverged/Unrepresentable warning with explicit insert/merge
/// remediation. Read-only; it never materializes and never moves a ref.
pub(crate) fn print_divergence_notices(repo: &Repository) -> CliResult<()> {
    for mapping in repo.list_ref_mappings().map_err(CliError::from)? {
        let diverged = mapping.status == RefSyncStatus::Diverged;
        // A deleted-view tombstone is reported by view existence alone; the
        // persisted local ref (CB-10A review R5: tombstones keep the ref
        // name) is preserved in the diagnostic instead of being erased.
        let orphaned = !repo
            .view_exists(&mapping.view_name)
            .map_err(CliError::from)?;
        if diverged {
            crate::output::print_warning(&format!(
                "ref mapping for view '{}' is Diverged: Git and Atomic both moved beyond the last \
                 mutually observed mapping; neither side was moved. {}",
                mapping.view_name,
                remediation_for(RefSyncStatus::Diverged)
            ));
        } else if orphaned {
            let mapped_ref = mapping
                .local_ref
                .as_deref()
                .unwrap_or("<no ref persisted>");
            crate::output::print_warning(&format!(
                "ref mapping for deleted view '{}' is Unrepresentable: its Git ref '{mapped_ref}' \
                 was left untouched. {}",
                mapping.view_name,
                remediation_for(RefSyncStatus::Unrepresentable)
            ));
        }
    }
    Ok(())
}

/// `atomic git bridge status`: read-only per-view ref-mapping reconciliation
/// report (CB-10A AC-2). Never materializes; never moves a ref.
pub(crate) fn run_status() -> CliResult<()> {
    let root = crate::commands::find_repository_root()?;
    let repo = Repository::open(&root).map_err(CliError::from)?;
    let mappings = repo.list_ref_mappings().map_err(CliError::from)?;

    // Also report the current workspace view when it has no mapping yet.
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let current_view = repo.desired_view_name(working_copy).map_err(CliError::from)?;
    let mut rows: Vec<(RefMapping, bool)> = mappings
        .into_iter()
        .map(|mapping| (mapping, true))
        .collect();
    if !rows.iter().any(|(mapping, _)| mapping.view_name == current_view) {
        if repo.view_exists(&current_view).map_err(CliError::from)? {
            rows.push((
                unpersisted_mapping(&repo, &current_view)?,
                false,
            ));
        }
    }

    if rows.is_empty() {
        println!("no ref mappings (run 'atomic git bridge reconcile' to establish one)");
        return Ok(());
    }
    let git = GitRepository::open(&root)
        .map_err(|error| CliError::GitError { message: format!("cannot open Git repository: {error}") })?;
    for (mapping, persisted) in rows {
        let view_exists = repo.view_exists(&mapping.view_name).map_err(CliError::from)?;
        let (display_status, remediation) = if !view_exists {
            (
                RefSyncStatus::Unrepresentable,
                "view no longer exists; its Git ref was left untouched. republish via 'git switch -c' or 'atomic git bridge publish'",
            )
        } else {
            let decision = classify_mapping_direction(&repo, &git, &mapping.view_name, &mapping)?;
            match decision.action {
                ThreeWayAction::Diverged => (
                    RefSyncStatus::Diverged,
                    remediation_for(RefSyncStatus::Diverged),
                ),
                ThreeWayAction::Import => (
                    RefSyncStatus::GitAhead,
                    remediation_for(RefSyncStatus::GitAhead),
                ),
                ThreeWayAction::Export => (
                    RefSyncStatus::AtomicAhead,
                    remediation_for(RefSyncStatus::AtomicAhead),
                ),
                ThreeWayAction::Noop => (
                    mapping.status,
                    remediation_for(mapping.status),
                ),
                ThreeWayAction::Unrepresentable => (
                    RefSyncStatus::Unrepresentable,
                    remediation_for(RefSyncStatus::Unrepresentable),
                ),
            }
        };
        println!(
            "{} {}",
            if persisted { "mapping" } else { "unpersisted" },
            mapping.view_name
        );
        println!(
            "  scope:       {}",
            ViewScope::from_u8(mapping.scope).map(|s| s.to_string()).unwrap_or_else(|| format!("unknown({})", mapping.scope))
        );
        println!("  local ref:   {}", mapping.local_ref.as_deref().unwrap_or("<none>"));
        println!(
            "  last observed local:   {}",
            mapping.last_observed_local.as_deref().unwrap_or("<none>")
        );
        println!(
            "  last observed atomic:  {}",
            mapping.last_observed_atomic.as_deref().unwrap_or("<none>")
        );
        println!(
            "  last exported:         {}",
            mapping.last_exported.as_deref().unwrap_or("<none>")
        );
        println!("  status:      {display_status}");
        println!("  remediation: {remediation}");
    }
    Ok(())
}

/// Build the display-only mapping for a workspace view without a persisted
/// row (read-only; `view_id` 0 marks it unpersisted).
fn unpersisted_mapping(repo: &Repository, view: &str) -> CliResult<RefMapping> {
    let info = repo.get_view_info(view).map_err(CliError::from)?;
    Ok(RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: 0,
        view_name: view.to_string(),
        scope: info.scope as u8,
        local_ref: if is_ephemeral_view_name(view) {
            None
        } else {
            mapped_local_ref(info.scope, view)
        },
        remote: None,
        last_observed_local: None,
        last_observed_remote: None,
        last_exported: None,
        last_exported_state: None,
        last_observed_atomic: None,
        status: RefSyncStatus::Synchronized,
    })
}

/// `atomic git bridge publish <view> --branch <name>`: explicit Draft
/// publication (RFC §8.1, CB-10A AC-2).
///
/// Publication is create-only and guarded (CB-10A review R3): the shared
/// workspace boundary must accept the workspace before any baseline or ref
/// mutation (Git sequence markers, conflicted/staged index, dirty or unaligned
/// state all refuse), an occupied target branch is a typed refusal, and the
/// ref write goes through a real Git reference transaction — the ref is
/// locked and re-compared under Git's own ref lock — behind a journaled
/// expected-absent operation with leased receipts.
pub(crate) fn run_publish(view: &str, branch: &str) -> CliResult<()> {
    let root = crate::commands::find_repository_root()?;
    let mut repo = Repository::open(&root).map_err(CliError::from)?;
    let info = repo.get_view_info(view).map_err(CliError::from)?;
    if !matches!(info.scope, ViewScope::Draft) {
        return Err(CliError::GitError {
            message: format!(
                "view '{view}' is {} and already maps to a Git ref; publish applies to Draft views",
                if info.scope == ViewScope::Shared {
                    "shared"
                } else {
                    "unknown-scope"
                }
            ),
        });
    }
    if is_ephemeral_view_name(view) {
        // Publishing an ephemeral view gives it a branch; the mapping policy
        // moves from "no extra ref" to the branch-backed mapping.
    }

    // Review R3: the workspace preflight runs BEFORE any baseline or ref
    // mutation; a refused publication mutates nothing.
    let workspace = enter_workspace(&mut repo, WorkspaceTxnMode::Reconcile)?;
    let working_copy = workspace.working_copy();
    let git = GitRepository::open(&root)
        .map_err(|error| CliError::GitError { message: format!("cannot open Git repository: {error}") })?;
    crate::commands::git::bridge::require_clean_git_worktree(&git)?;

    let target_ref = format!("refs/heads/{branch}");
    // Review R3: publication is create-only. An occupied target is never a
    // permissible update — moving existing history requires explicit
    // authorization that this command does not have.
    if let Ok(reference) = git.find_reference(&target_ref) {
        let tip = reference
            .target()
            .map(|oid| oid.to_string())
            .unwrap_or_else(|| "<symbolic>".to_string());
        return Err(git_error(format!(
            "refusing to publish view '{view}': target branch 'refs/heads/{branch}' already \
             exists at {tip}; publication is create-only — delete or rename the branch first"
        )));
    }

    let (baseline, _) = ensure_baseline_mapping(&repo, working_copy, &git, view)?;
    let draft_ref = baseline.local_ref.clone().unwrap_or_else(|| format!("refs/atomic/views/{view}"));
    let commit = git
        .find_reference(&draft_ref)
        .ok()
        .and_then(|reference| reference.target())
        .or_else(|| {
            // An ephemeral draft keeps its adopted commit as the publication
            // point.
            super::shadow::ephemeral_original_commit(&git, view)
                .and_then(|hex| git2::Oid::from_str(&hex).ok())
        })
        .ok_or_else(|| CliError::GitError {
            message: format!(
                "view '{view}' has no projection commit to publish; run 'atomic git bridge reconcile' or record first"
            ),
        })?;

    // CB-10A review R1: the publication source is the VERIFIED projection
    // commit — the exact Git HEAD the verified workspace checkpoint names —
    // never the live tip of the draft's private ref. A foreign writer can
    // move `refs/atomic/views/<view>` without touching the aligned workspace
    // (the workspace verifies HEAD, cleanliness and alignment; the private
    // ref is not part of that invariant), so a publication that trusted the
    // live ref tip would export unverified history and then claim
    // `Synchronized` for it. The stale-source probe — external
    // `git update-ref refs/atomic/views/topic refs/heads/main`, then
    // `git bridge publish topic --branch …` — refuses here instead.
    let checkpoint = crate::commands::git::bridge::read_workspace_metadata(&root)?;
    let checkpoint = checkpoint.ok_or_else(|| git_error(format!(
        "refusing to publish view '{view}': there is no verified bridge checkpoint; \
         run 'atomic git bridge reconcile' to verify the workspace first"
    )))?;
    if checkpoint.view != view {
        return Err(git_error(format!(
            "refusing to publish view '{view}': the verified bridge checkpoint names view '{}' \
             (the workspace must be reconciled and checked out on '{view}' first)",
            checkpoint.view
        )));
    }
    if checkpoint.git_head != commit.to_string() {
        return Err(git_error(format!(
            "refusing to publish view '{view}': the candidate publication commit {} is not the \
             verified projection commit {} of the current workspace state. The draft's mapped ref \
             moved outside a verified transition; run 'atomic git bridge reconcile' and re-verify \
             before publishing",
            commit,
            checkpoint.git_head
        )));
    }

    // Review R3: the durable MAPPING INTENT travels IN the same journaled
    // operation as the ref effect. The target mapping is built here — from
    // the verified state the R1 checkpoint check just proved — and leased
    // against the baseline row; the intent is durable before the ref write
    // and applied exactly once, after the ref receipt. A crash window can
    // therefore never leave a published branch without recoverable mapping
    // intent.
    let tip = commit.to_string();
    let state = info.state.to_base32();
    let mapping = RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: durable_view_id(&repo, view)?,
        view_name: view.to_string(),
        scope: info.scope as u8,
        local_ref: Some(target_ref.clone()),
        remote: baseline.remote.clone(),
        last_observed_local: Some(tip.clone()),
        last_observed_remote: baseline.last_observed_remote.clone(),
        last_exported: Some(tip.clone()),
        last_exported_state: Some(state.clone()),
        last_observed_atomic: Some(state),
        status: RefSyncStatus::Synchronized,
    };
    // The baseline row is always present here (ensure_baseline_mapping
    // created it); its canonical bytes are the expected-old lease.
    let expected_old = atomic_core::operation::MetadataValue::Bytes(
        baseline.encode().map_err(|error| git_error(error.to_string()))?,
    );
    let mapping_intent = atomic_core::operation::MetadataTransition {
        target: atomic_core::operation::MetadataTarget::RefMapping {
            view: view.to_string(),
        },
        expected_old,
        expected_new: atomic_core::operation::MetadataValue::Bytes(
            mapping.encode().map_err(|error| git_error(error.to_string()))?,
        ),
    };
    journal_and_create_branch_cas_with_mapping(
        &repo,
        working_copy,
        &git,
        &target_ref,
        commit,
        "atomic view publish",
        mapping_intent,
    )?;
    crate::output::print_success(&format!(
        "Published draft view '{view}' on branch '{branch}' at {tip}; the view remains a Draft in Atomic"
    ));
    Ok(())
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}