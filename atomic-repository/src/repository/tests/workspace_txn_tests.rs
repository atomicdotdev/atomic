use std::fs;
use std::path::Path;
use std::process::Command;

use atomic_core::operation::OperationScope;
use tempfile::TempDir;

use super::*;

#[test]
fn observe_native_workspace_is_ready_non_mutating_and_holds_ordered_locks() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let before_record = repo.working_copy_record(working_copy).unwrap();
    let before_log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();

    let start = repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap();
    let WorkspaceTxnStart::Ready(txn) = start else {
        panic!("native workspace should be ready");
    };
    assert_eq!(txn.mode(), WorkspaceTxnMode::Observe);
    assert_eq!(txn.working_copy(), working_copy);
    assert_eq!(txn.view().id, before_record.desired_view);
    assert!(matches!(txn.git(), WorkspaceGitObservation::NoGit { .. }));
    assert!(txn.plan().is_ordered());
    let nested = repo
        .try_lock_operation(working_copy)
        .expect("nested command work should reuse the workspace lock on its owning thread");
    drop(nested);
    drop(txn);

    assert_eq!(
        repo.working_copy_record(working_copy).unwrap(),
        before_record
    );
    assert_eq!(
        repo.operation_log(OperationScope::WorkingCopy(working_copy), None, false)
            .unwrap(),
        before_log
    );
}

#[test]
fn observe_clean_git_checkpoint_is_ready_without_domain_mutation() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    let checkpoint_path = directory.path().join(".atomic/bridge/workspace.json");
    let checkpoint_before = fs::read(&checkpoint_path).unwrap();
    let tracked_before = fs::read(directory.path().join("tracked.txt")).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let record_before = repo.working_copy_record(working_copy).unwrap();

    let WorkspaceTxnStart::Ready(txn) =
        repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap()
    else {
        panic!("matching checkpoint should enter a ready transaction");
    };
    assert_eq!(txn.checkpoint().unwrap().version, 2);
    assert_eq!(txn.attempts(), 1);
    drop(txn);

    assert_eq!(fs::read(checkpoint_path).unwrap(), checkpoint_before);
    assert_eq!(
        fs::read(directory.path().join("tracked.txt")).unwrap(),
        tracked_before
    );
    assert_eq!(
        repo.working_copy_record(working_copy).unwrap(),
        record_before
    );
}

#[test]
fn sequence_state_reports_mode_specific_typed_remediation() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    fs::write(
        directory.path().join(".git/MERGE_HEAD"),
        format!("{head}\n"),
    )
    .unwrap();
    let tracked_before = fs::read(directory.path().join("tracked.txt")).unwrap();

    for (mode, expected) in [
        (
            WorkspaceTxnMode::Observe,
            GitOperationDisposition::ObserveOnly,
        ),
        (
            WorkspaceTxnMode::Reconcile,
            GitOperationDisposition::FinishOrAbortInGit,
        ),
        (
            WorkspaceTxnMode::Force,
            GitOperationDisposition::ForceForbidden,
        ),
    ] {
        let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
            markers,
            disposition,
            ..
        }) = repo.begin_workspace_txn(mode).unwrap()
        else {
            panic!("merge state should return typed remediation");
        };
        assert!(markers.contains(&GitOperationMarker::MergeHead));
        assert_eq!(disposition, expected);
    }

    assert_eq!(
        fs::read(directory.path().join("tracked.txt")).unwrap(),
        tracked_before
    );
}

#[test]
fn rebase_marker_is_detected_even_when_libgit_reports_clean() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    fs::create_dir(directory.path().join(".git/rebase-merge")).unwrap();

    let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
        markers,
        ..
    }) = repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap()
    else {
        panic!("rebase marker should return typed remediation");
    };
    assert!(markers.contains(&GitOperationMarker::RebaseMerge));
}

/// A standalone `AUTO_MERGE` root ref (merge-ort's derived tree reference)
/// with a `Clean` libgit2 state and no other marker must not fence a matching
/// anchored workspace.
#[test]
fn standalone_auto_merge_ref_permits_ready_entry() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    fs::write(
        directory.path().join(".git/AUTO_MERGE"),
        format!("{tree}\n"),
    )
    .unwrap();
    let checkpoint_path = directory.path().join(".atomic/bridge/workspace.json");
    let checkpoint_before = fs::read(&checkpoint_path).unwrap();
    let tracked_before = fs::read(directory.path().join("tracked.txt")).unwrap();

    let WorkspaceTxnStart::Ready(txn) =
        repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap()
    else {
        panic!("a standalone AUTO_MERGE must not fence a clean anchored workspace");
    };
    drop(txn);

    // The advisory ref is left byte-identical and nothing was materialized.
    assert_eq!(
        fs::read(directory.path().join(".git/AUTO_MERGE")).unwrap(),
        format!("{tree}\n").into_bytes()
    );
    assert_eq!(fs::read(&checkpoint_path).unwrap(), checkpoint_before);
    assert_eq!(
        fs::read(directory.path().join("tracked.txt")).unwrap(),
        tracked_before
    );
}

/// A genuine stopped merge writes `MERGE_HEAD`; with `AUTO_MERGE` also present
/// it must still refuse.
#[test]
fn merge_head_with_auto_merge_ref_still_refuses() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    fs::write(
        directory.path().join(".git/MERGE_HEAD"),
        format!("{head}\n"),
    )
    .unwrap();
    fs::write(
        directory.path().join(".git/AUTO_MERGE"),
        format!("{tree}\n"),
    )
    .unwrap();

    let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
        markers,
        ..
    }) = repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap()
    else {
        panic!("an active merge must return typed remediation");
    };
    assert!(markers.contains(&GitOperationMarker::MergeHead));
    assert!(markers.contains(&GitOperationMarker::AutoMerge));
}

/// An unmerged index refuses even when `AUTO_MERGE` is the only marker.
/// (The synthetic classification test in `workspace_txn.rs` covers the exact
/// `index_stages = [1,2,3]` lease; the real-repository conflict case is
/// covered by the CLI observation suite.)
#[test]
fn unmerged_index_with_standalone_auto_merge_ref_still_refuses() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    fs::write(
        directory.path().join(".git/AUTO_MERGE"),
        format!("{tree}\n"),
    )
    .unwrap();
    // Mirror the observer's unmerged-stage evidence without corrupting the
    // on-disk index: the boundary is driven purely by the observed stages.
    let mut stages = observe_git_metadata(directory.path()).unwrap();
    let WorkspaceGitObservation::Repository(git) = &mut stages else {
        panic!("expected a Git repository");
    };
    git.index_stages = vec![0, 1, 2, 3];
    let start = repo
        .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| Ok(stages.clone()))
        .unwrap();
    assert!(matches!(
        start,
        WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
    ));
}

#[test]
fn changed_head_blocks_filesystem_phase_before_reconciliation() {
    let (directory, mut repo, _head, tree) = initialized_colocated_repository();
    write_checkpoint(
        directory.path(),
        &repo,
        "0000000000000000000000000000000000000000",
        &tree,
    );

    let WorkspaceTxnStart::Remediation(WorkspaceRemediation::Unanchored {
        state: UnanchoredWorkspace::HeadChanged { .. },
        plan,
        ..
    }) = repo
        .begin_workspace_txn(WorkspaceTxnMode::Reconcile)
        .unwrap()
    else {
        panic!("changed HEAD should require reconciliation");
    };
    assert!(plan.is_ordered());
    assert!(matches!(
        plan.head(),
        WorkspaceHeadPlan::ReconcileBeforeFilesystem { .. }
    ));
    assert_eq!(
        plan.filesystem(),
        WorkspaceFilesystemPlan::BlockedUntilHeadAligned
    );
    assert_eq!(plan.refs(), WorkspaceRefPlan::Deferred);
}

#[test]
fn head_mutation_between_observations_retries_at_most_three_times_and_never_mutates() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    let working_copy = repo.require_working_copy_id().unwrap();
    let state_before = repo
        .working_copy_record(working_copy)
        .unwrap()
        .desired_state;
    let checkpoint_bytes =
        fs::read(directory.path().join(".atomic/bridge/workspace.json")).unwrap();

    // Injected TOCTOU: every observation reports a different index digest
    // while the classified facts (HEAD, index tree) stay checkpoint-stable,
    // so the boundary can never observe the same token twice. HEAD/index are
    // observed before the filesystem phase on every attempt, and after
    // MAX_WORKSPACE_TXN_ATTEMPTS (3) unstable observations the boundary
    // returns the typed ConcurrentGitMutation remediation.
    let mut flips = 0usize;
    let unstable = |root: &Path| -> Result<WorkspaceGitObservation, super::ObservationError> {
        let mut observation = observe_git_metadata(root)?;
        if let WorkspaceGitObservation::Repository(git) = &mut observation {
            flips += 1;
            let mut digest = [0u8; 32];
            digest[0] = flips as u8;
            git.index_digest = atomic_core::Hash::from_bytes(digest);
        }
        Ok(observation)
    };
    let start = repo
        .begin_workspace_txn_with(WorkspaceTxnMode::Reconcile, unstable)
        .unwrap();
    let WorkspaceTxnStart::Remediation(WorkspaceRemediation::ConcurrentGitMutation {
        attempts,
        ..
    }) = start
    else {
        panic!("an unstable baseline must end in the typed TOCTOU remediation");
    };
    assert_eq!(
        attempts, MAX_WORKSPACE_TXN_ATTEMPTS,
        "the boundary retries at most three times"
    );

    // Nothing was mutated by the refused boundary.
    assert_eq!(
        repo.working_copy_record(working_copy)
            .unwrap()
            .desired_state,
        state_before
    );
    assert_eq!(
        fs::read(directory.path().join(".atomic/bridge/workspace.json")).unwrap(),
        checkpoint_bytes
    );
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    assert!(
        !log.entries
            .iter()
            .any(|entry| entry.operation.payload().kind
                != atomic_core::operation::OperationKind::Anchor),
        "an unstable boundary journals nothing beyond the anchor"
    );
}

#[test]
fn legacy_checkpoint_is_read_without_rewrite() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    let path = directory.path().join(".atomic/bridge/workspace.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = repo
        .working_copy_record(repo.require_working_copy_id().unwrap())
        .unwrap()
        .desired_state;
    let bytes = format!(
        "{{\"version\":1,\"view\":\"dev\",\"atomic_state\":\"{state}\",\"git_head\":\"{head}\",\"git_tree\":\"{tree}\"}}"
    )
    .into_bytes();
    fs::write(&path, &bytes).unwrap();

    let WorkspaceTxnStart::Ready(txn) =
        repo.begin_workspace_txn(WorkspaceTxnMode::Observe).unwrap()
    else {
        panic!("legacy matching checkpoint should be accepted");
    };
    let checkpoint = txn.checkpoint().unwrap();
    assert_eq!(checkpoint.version, 1);
    assert_eq!(checkpoint.git_index_tree.as_deref(), Some(tree.as_str()));
    drop(txn);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

fn initialized_colocated_repository() -> (TempDir, Repository, String, String) {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    git(directory.path(), &["init", "-b", "dev"]);
    git(
        directory.path(),
        &["config", "user.email", "tests@atomic.dev"],
    );
    git(directory.path(), &["config", "user.name", "Atomic Tests"]);
    fs::write(directory.path().join("tracked.txt"), b"tracked\n").unwrap();
    git(directory.path(), &["add", "tracked.txt"]);
    git(directory.path(), &["commit", "-m", "initial"]);
    let head = git(directory.path(), &["rev-parse", "HEAD"]);
    let tree = git(directory.path(), &["rev-parse", "HEAD^{tree}"]);
    (directory, repo, head, tree)
}

fn write_checkpoint(root: &Path, repo: &Repository, head: &str, tree: &str) {
    let path = root.join(".atomic/bridge/workspace.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = repo
        .working_copy_record(repo.require_working_copy_id().unwrap())
        .unwrap()
        .desired_state;
    let symref = git(root, &["symbolic-ref", "HEAD"]);
    fs::write(
        path,
        format!(
            "{{\"version\":2,\"view\":\"dev\",\"atomic_state\":\"{state}\",\"git_head_symref\":\"{symref}\",\"git_head\":\"{head}\",\"git_tree\":\"{tree}\",\"git_index_tree\":\"{tree}\"}}"
        ),
    )
    .unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
