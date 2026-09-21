//! Journaled projection-publication leases (RFC §7.2, CB-8B ac-3/ac-4).
//!
//! Covers the reviewer blockers from ATOM::aaron::2: real compare-and-set
//! ref/HEAD movement under Git ref-transaction locking with inside-window
//! regressions, operational-error propagation, third-value lease rejection
//! that preserves newer external work, effect combinations (checkpoint-only,
//! index-only, object-only), negative missing-receipt finalization, and the
//! ac-4 deterministic matrix (linked worktrees, concurrent writers).

use super::super::projection_effects::{
    aligned_git_index_lease, observe_checkpoint_facts, observe_git_index_lease, read_head_target,
    ProjectionCheckpointPlan,
};
use super::*;

use atomic_core::operation::{DigestKind, EffectReceiptKind, EffectTarget, EffectValue};
use atomic_core::pristine::OperationTxnT;
use std::fs;
use std::process::Command;
use std::thread;

/// Run one `git` command in `root` and assert success.
fn run_git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Atomic Tests")
        .env("GIT_AUTHOR_EMAIL", "tests@atomic.dev")
        .env("GIT_COMMITTER_NAME", "Atomic Tests")
        .env("GIT_COMMITTER_EMAIL", "tests@atomic.dev")
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

/// Colocated fixture: an Atomic repository over a Git repository with one
/// commit on `master` and a verified bridge checkpoint naming that state.
fn colocated() -> (TempDir, Repository, git2::Repository) {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init_with_view(directory.path(), "master").unwrap();
    run_git_init(&directory);
    let info = repo.get_view_info("master").unwrap();
    let head = run_git(directory.path(), &["rev-parse", "HEAD"]);
    let tree = run_git(directory.path(), &["rev-parse", "HEAD^{tree}"]);
    super::super::workspace_txn::write_workspace_checkpoint(
        repo.root(),
        &super::super::workspace_txn::WorkspaceCheckpoint {
            version: 2,
            view: "master".to_string(),
            atomic_state: info.state.to_base32(),
            git_head_symref: Some("refs/heads/master".to_string()),
            git_head: head,
            git_tree: tree,
            git_index_tree: None,
            git_index_digest: None,
        },
    )
    .unwrap();
    let git_repo = git2::Repository::open(directory.path()).unwrap();
    (directory, repo, git_repo)
}

fn run_git_init(directory: &TempDir) {
    run_git(directory.path(), &["init", "-b", "master"]);
    run_git(directory.path(), &["config", "user.email", "tests@atomic.dev"]);
    run_git(directory.path(), &["config", "user.name", "Atomic Tests"]);
    fs::write(directory.path().join("tracked.txt"), b"tracked\n").unwrap();
    run_git(directory.path(), &["add", "tracked.txt"]);
    run_git(directory.path(), &["commit", "-m", "initial"]);
}

fn core_oid(oid: git2::Oid) -> atomic_core::operation::GitObjectId {
    atomic_core::operation::GitObjectId::new(
        atomic_core::operation::GitHashAlgorithm::Sha1,
        oid.as_bytes().to_vec(),
    )
    .unwrap()
}

fn direct(oid: git2::Oid) -> atomic_core::operation::GitRefTarget {
    atomic_core::operation::GitRefTarget::Direct(core_oid(oid))
}

fn write_object(git: &git2::Repository, bytes: &[u8]) -> git2::Oid {
    git.odb().unwrap().write(git2::ObjectType::Blob, bytes).unwrap()
}

fn build_tree(git: &git2::Repository, entries: &[(&str, &[u8])]) -> git2::Oid {
    let mut builder = git.treebuilder(None).unwrap();
    for (name, bytes) in entries {
        builder.insert(*name, write_object(git, bytes), 0o100644).unwrap();
    }
    builder.write().unwrap()
}

#[test]
fn checkpoint_only_plan_executes_and_finalizes() {
    let (_directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // A fresh commit the checkpoint will name, not referenced by any ref.
    let tree = build_tree(&git, &[("extra.txt", b"checkpoint only\n")]);
    let commit_oid = git
        .commit(
            None,
            &signature(),
            &signature(),
            "checkpoint only",
            &git.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    // A checkpoint-only publication baselines the live Git state: the
    // checkpoint's HEAD facts must resolve in Git, so HEAD is detached to
    // the fresh commit before the plan is journaled (no ref/HEAD effect is
    // planned — the publication is checkpoint-only).
    git.set_head_detached(commit_oid).unwrap();
    let evidence = atomic_core::Hash::of(b"checkpoint-only");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            None,
            Vec::new(),
            None,
            Some(ProjectionCheckpointPlan {
                git_head_symref: None,
                git_head: core_oid(commit_oid),
                git_tree: core_oid(tree),
            }),
            evidence,
        )
        .unwrap();
    assert!(prepared.checkpoint_plan, "the checkpoint effect must be journaled");
    repo.execute_projection_publish(&prepared, &git).unwrap();
    repo.finalize_projection_publish(prepared, &git).unwrap();
    let checkpoint = super::super::workspace_txn::read_workspace_checkpoint(repo.root())
        .unwrap()
        .expect("checkpoint published");
    assert_eq!(checkpoint.view, "master");
    assert_eq!(checkpoint.git_head, commit_oid.to_string());
    assert_eq!(checkpoint.git_head_symref, None, "detached Draft policy");
}

#[test]
fn index_only_plan_executes_and_finalizes() {
    let (_directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // The index holds an unrelated entry; the target tree does not.
    fs::write(_directory.path().join("staged.txt"), b"staged\n").unwrap();
    run_git(_directory.path(), &["add", "staged.txt"]);
    let tree = build_tree(&git, &[("tracked.txt", b"tracked\n")]);
    let evidence = atomic_core::Hash::of(b"index-only");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            None,
            Vec::new(),
            Some(tree),
            None,
            evidence,
        )
        .unwrap();
    assert!(prepared.index_plan, "the index effect must be journaled");
    repo.execute_projection_publish(&prepared, &git).unwrap();
    repo.finalize_projection_publish(prepared, &git).unwrap();
    let observed = observe_git_index_lease(&git).unwrap();
    let aligned = aligned_git_index_lease(&git, tree).unwrap();
    assert_eq!(observed, aligned, "the replaced index holds the aligned tree");
}

#[test]
fn object_only_plan_is_idempotent_when_objects_already_landed() {
    let (_directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    let tree = build_tree(&git, &[("tracked.txt", b"tracked\n")]);
    let commit_oid = git
        .commit(
            None,
            &signature(),
            &signature(),
            "idempotent",
            &git.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    let evidence = atomic_core::Hash::of(b"object-only");
    // The mapped ref already tips at the projection and no other lease
    // moves: the publication is a zero-op.
    git.reference("refs/atomic/views/agent", commit_oid, true, "seed").unwrap();
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(commit_oid)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    assert_eq!(
        prepared.operation_id,
        atomic_core::OperationId::from_bytes([0u8; 32]),
        "an already-current publication is an idempotent no-op"
    );
}

#[test]
fn ref_third_value_is_rejected_and_the_external_value_is_preserved() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // Prepare a plan that moves a ref the external writer then claims.
    fs::write(directory.path().join("extra.txt"), b"second\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "second"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let evidence = atomic_core::Hash::of(b"ref-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    // An external writer lands a competing value after prepare.
    let external = write_object(&git, b"external\n");
    git.reference(
        "refs/atomic/views/agent",
        external,
        false,
        "external writer",
    )
    .unwrap();
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(error.is_err(), "the third value must fail closed: {error:?}");
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        external,
        "the newer external value is never overwritten"
    );
}

#[test]
fn head_third_value_is_rejected_and_the_external_head_is_preserved() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"head\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "head target"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let evidence = atomic_core::Hash::of(b"head-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    // An external writer detaches HEAD elsewhere after prepare: the plan
    // recorded an attached chain, the live HEAD is a third value. HEAD can
    // only carry a commit, so the external value is an orphan commit.
    let external_tree = build_tree(&git, &[("external.txt", b"external commit bytes\n")]);
    let external = git
        .commit(
            None,
            &signature(),
            &signature(),
            "external writer",
            &git.find_tree(external_tree).unwrap(),
            &[],
        )
        .unwrap();
    git.set_head_detached(external).unwrap();
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(error.is_err(), "the third value must fail closed");
    assert_eq!(
        git.head().unwrap().target().unwrap(),
        external,
        "the external HEAD value is never overwritten"
    );
}

#[test]
fn ref_cas_holds_the_ref_lock_through_the_comparison() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"contended\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "contended"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // A competing transaction holds the ref's lockfile across the window.
    let mut competitor = git.transaction().unwrap();
    competitor.lock_ref("refs/atomic/views/agent").unwrap();
    let evidence = atomic_core::Hash::of(b"ref-lock");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(error.is_err(), "the bounded budget must refuse a contended ref");
    assert!(
        git.find_reference("refs/atomic/views/agent").is_err(),
        "the contended ref was never created behind the competitor's back"
    );
    drop(competitor);
    // Release the failed attempt's operation boundary before the retry: the
    // prepared guard holds the working-copy operation lock until it drops.
    drop(prepared);
    // The contended attempt left an incomplete journal head: recover it
    // through the inverse-recovery gate before preparing another operation.
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    // Retry without contention completes the publication under the lease.
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    repo.execute_projection_publish(&prepared, &git).unwrap();
    repo.finalize_projection_publish(prepared, &git).unwrap();
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        target
    );
}

#[test]
fn head_cas_holds_the_head_lock_through_the_comparison() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"head\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "head"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // A competing transaction locks HEAD across the window.
    let mut competitor = git.transaction().unwrap();
    competitor.lock_ref("HEAD").unwrap();
    let evidence = atomic_core::Hash::of(b"head-lock");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(error.is_err(), "the bounded budget must refuse a contended HEAD");
    drop(competitor);
    // Release the failed attempt's operation boundary before the retry: the
    // prepared guard holds the working-copy operation lock until it drops.
    drop(prepared);
    // The contended attempt left an incomplete journal head: recover it
    // through the inverse-recovery gate before preparing another operation.
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    // Retry without contention completes the detached move under the lease.
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    repo.execute_projection_publish(&prepared, &git).unwrap();
    repo.finalize_projection_publish(prepared, &git).unwrap();
    assert_eq!(git.head().unwrap().target().unwrap(), target);
}

#[test]
fn read_head_distinguishes_unborn_detached_and_attached_states() {
    let directory = TempDir::new().unwrap();
    let git = git2::Repository::init(directory.path()).unwrap();
    // An unborn HEAD is a missing target, not an operational failure.
    assert!(read_head_target(&git).unwrap().is_none());
    // Detached HEAD reads as a direct target once a commit exists.
    let tree = git.treebuilder(None).unwrap().write().unwrap();
    let oid = git
        .commit(
            None,
            &git2::Signature::now("t", "t@example.com").unwrap(),
            &git2::Signature::now("t", "t@example.com").unwrap(),
            "seed",
            &git.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    git.set_head_detached(oid).unwrap();
    assert_eq!(read_head_target(&git).unwrap(), Some(direct(oid)));
    // An attached HEAD reads as its symbolic target once the branch exists.
    git.reference("refs/heads/master", oid, true, "test seed").unwrap();
    git.set_head("refs/heads/master").unwrap();
    assert_eq!(
        read_head_target(&git).unwrap(),
        Some(atomic_core::operation::GitRefTarget::Symbolic(
            "refs/heads/master".to_string()
        ))
    );
}

#[cfg(feature = "adoption-test-injection")]
#[test]
fn finalize_requires_every_effect_receipt() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"finalize\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "finalize"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let evidence = atomic_core::Hash::of(b"finalize-negative");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    // Crash after the first effect's receipt (the ref) — the HEAD effect
    // never landed, so its receipt is missing.
    std::env::set_var("ATOMIC_FAIL_PROJECTION_AFTER_REF", "1");
    let crashed = repo.execute_projection_publish(&prepared, &git);
    std::env::remove_var("ATOMIC_FAIL_PROJECTION_AFTER_REF");
    assert!(crashed.is_err(), "the failpoint must fail the publication");
    // Direct finalization without the remaining receipts is refused.
    let error = repo
        .finalize_operation_verified(&prepared.lock, prepared.operation_id)
        .unwrap_err();
    assert!(
        error.to_string().contains("cannot be verified before effect"),
        "missing-receipt finalization must refuse: {error}"
    );
}

#[test]
fn index_third_value_preserves_the_external_staged_content() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // Prepare an index replacement (a tree the live index does not hold, so an
    // index effect is journaled), then stage unrelated user content.
    let tree = build_tree(
        &git,
        &[("tracked.txt", b"tracked\n"), ("aligned.txt", b"aligned\n")],
    );
    let evidence = atomic_core::Hash::of(b"index-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            None,
            Vec::new(),
            Some(tree),
            None,
            evidence,
        )
        .unwrap();
    fs::write(directory.path().join("user.txt"), b"user bytes\n").unwrap();
    run_git(directory.path(), &["add", "user.txt"]);
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(error.is_err(), "the third value must fail closed");
    // The user's staged bytes survive byte-for-byte.
    let staged = run_git(directory.path(), &["status", "--porcelain"]);
    assert!(
        staged.contains("A  user.txt"),
        "the external staged entry is preserved: {staged:?}"
    );
}

// ── Shared helpers ──────────────────────────────────────────────────────

fn signature() -> git2::Signature<'static> {
    git2::Signature::now("Atomic Tests", "tests@atomic.dev")
        .unwrap()
        .to_owned()
}

// ── AC-4 deterministic matrix: linked worktrees ─────────────────────────

/// A linked Git worktree shares the object database and every ref with the
/// main worktree, but keeps its own HEAD file and index. A journaled
/// projection publication from the main worktree must never move the linked
/// worktree's HEAD or index, and a shared ref the linked worktree claims
/// between prepare and execute is the third value — preserved, never
/// overwritten (RFC §8.5, §12).
#[test]
fn linked_worktree_publication_preserves_the_other_worktree_head_and_third_values() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // A second commit on master and a linked worktree on its own branch.
    fs::write(directory.path().join("extra.txt"), b"second\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "second"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let linked_path = directory.path().join("linked-wt");
    git.worktree("linked", &linked_path, None).unwrap();
    let linked = git2::Repository::open(directory.path().join("linked-wt")).unwrap();
    let linked_head_before = linked.head().unwrap().target().unwrap();
    let linked_index_before = observe_git_index_lease(&linked).unwrap();

    // The linked worktree claims the shared projection ref after prepare.
    let external_tree = build_tree(&git, &[("linked.txt", b"linked writer\n")]);
    let external = git
        .commit(
            None,
            &signature(),
            &signature(),
            "linked writer",
            &git.find_tree(external_tree).unwrap(),
            &[],
        )
        .unwrap();
    let evidence = atomic_core::Hash::of(b"linked-worktree-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    // Inside the prepare→execute window the linked worktree moves the SHARED
    // ref (refs are common across worktrees) — a third value.
    git.reference(
        "refs/atomic/views/agent",
        external,
        true,
        "linked writer",
    )
    .unwrap();
    let error = repo.execute_projection_publish(&prepared, &git);
    assert!(
        error.is_err(),
        "the linked worktree's ref claim must fail the publication closed: {error:?}"
    );
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        external,
        "the linked worktree's shared-ref claim is preserved"
    );
    // The linked worktree's HEAD and index were never part of the plan: the
    // publication (even a failed one) must not mix materialization across
    // worktrees.
    assert_eq!(
        linked.head().unwrap().target().unwrap(),
        linked_head_before,
        "the linked worktree's HEAD is untouched by the main publication"
    );
    assert_eq!(
        observe_git_index_lease(&linked).unwrap(),
        linked_index_before,
        "the linked worktree's index is untouched by the main publication"
    );
    // Release the failed attempt's boundary and recover its journal head
    // through the inverse-recovery gate before the retry.
    drop(prepared);
    let mut repo = repo;
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    // A clean retry publishes the ref and main-worktree HEAD while the
    // linked worktree stays byte-identical.
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    repo.execute_projection_publish(&prepared, &git).unwrap();
    repo.finalize_projection_publish(prepared, &git).unwrap();
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        target
    );
    assert_eq!(
        linked.head().unwrap().target().unwrap(),
        linked_head_before,
        "the linked worktree's HEAD is still untouched after a successful publication"
    );
    assert_eq!(
        observe_git_index_lease(&linked).unwrap(),
        linked_index_before,
        "the linked worktree's index is untouched by the main publication"
    );
}

// ── AC-4 deterministic matrix: concurrent writers ───────────────────────

/// A concurrent external Git writer races a journaled projection publication
/// on the same shared ref (the linked-worktree and concurrent-CLI schedule in
/// one process: redb admits one pristine handle, so the second writer acts
/// through Git directly, exactly like an external `git update-ref`). The
/// deterministic invariants (RFC §7/§11/§12): the ref settles at either the
/// journal's target or the external writer's last landed value — never a torn
/// or mixed value; the journal's outcome is always typed (verified or a
/// durable rejection that preserves the newer external value); the Git ODB
/// stays intact; and after the storm the journal gates converge so a
/// follow-up publication is an idempotent no-op.
#[test]
fn concurrent_external_ref_writes_never_lose_updates_or_tear_the_projection() {
    let (directory, repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    // The journaled publication's target.
    let tree_target = build_tree(&git, &[("target.txt", b"target\n")]);
    let target = git
        .commit(
            None,
            &signature(),
            &signature(),
            "journaled target",
            &git.find_tree(tree_target).unwrap(),
            &[],
        )
        .unwrap();
    // Seed the shared ref at the fixture's HEAD so both writers race a live ref.
    let seed = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    git.reference("refs/atomic/views/agent", seed, true, "seed").unwrap();
    // Four alternating external candidate commits.
    let externals: Vec<git2::Oid> = (0..4)
        .map(|index| {
            let name = format!("ext-{index}.txt");
            let content = format!("external {index}\n");
            let tree = build_tree(&git, &[(name.as_str(), content.as_bytes())]);
            git.commit(
                None,
                &signature(),
                &signature(),
                &format!("external writer {index}"),
                &git.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap()
        })
        .collect();
    let root = directory.path().to_path_buf();

    // The external writer hammers the shared ref, contending with the
    // journal's ref lock instead of bypassing it (Git's own lock protocol).
    let storm = {
        let root = root.clone();
        let externals = externals.clone();
        thread::spawn(move || -> git2::Oid {
            let storm_git = git2::Repository::open(&root).unwrap();
            let mut last = seed;
            for oid in &externals {
                let mut landed = false;
                for _ in 0..20 {
                    match storm_git.reference(
                        "refs/atomic/views/agent",
                        *oid,
                        true,
                        "external writer",
                    ) {
                        Ok(_) => {
                            landed = true;
                            break;
                        }
                        Err(error) if error.code() == git2::ErrorCode::Locked => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(error) => panic!("the storm write failed unexpectedly: {error}"),
                    }
                }
                if landed {
                    last = *oid;
                }
            }
            last
        })
    };

    // The journaled publication races the storm from the main thread.
    let evidence = atomic_core::Hash::of(b"storm");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    let executed = repo.execute_projection_publish(&prepared, &git);
    let external_final = storm.join().unwrap();
    if let Err(error) = &executed {
        let text = error.to_string();
        assert!(
            text.contains("diverged") || text.contains("rejected") || text.contains("locked"),
            "a raced publication must fail only with a typed refusal: {error}"
        );
    }
    if executed.is_ok() {
        repo.finalize_projection_publish(prepared, &git).unwrap();
    } else {
        drop(prepared);
    }
    // The ref settled at exactly one of the two writers' values.
    let final_ref = git
        .find_reference("refs/atomic/views/agent")
        .unwrap()
        .target()
        .unwrap();
    assert!(
        final_ref == target || final_ref == external_final,
        "the ref must settle at the journal's target or the external writer's last value, found {final_ref}"
    );
    // The Git object database carries the journaled target object.
    assert!(
        git.odb().unwrap().exists(target),
        "the journaled target object exists"
    );

    // After the storm the journal gates converge: recovery clears any
    // interrupted head (AlreadyComplete when verified), and a follow-up
    // publication of the observed value is an idempotent no-op.
    let mut repo = repo;
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    let follow_git = git2::Repository::open(directory.path()).unwrap();
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &follow_git,
            "refs/atomic/views/agent",
            Some(direct(final_ref)),
            None,
            Vec::new(),
            None,
            None,
            atomic_core::Hash::of(b"after-storm"),
        )
        .unwrap();
    assert_eq!(
        prepared.operation_id,
        atomic_core::OperationId::from_bytes([0u8; 32]),
        "after the storm the journal gates are clean and the current publication is an idempotent no-op"
    );
}

// ── Review ATOM::aaron::2: inside-window external-writer regressions ────

/// The effect receipt of one journaled operation, looked up by effect target.
fn receipt_for(
    repo: &Repository,
    operation_id: atomic_core::OperationId,
    wanted: &EffectTarget,
) -> (u32, atomic_core::operation::EffectReceipt) {
    let txn = repo.pristine().read_txn().unwrap();
    let operation = txn.get_operation(operation_id).unwrap().unwrap();
    let ordinal = operation
        .payload()
        .delta
        .effects
        .iter()
        .find(|effect| &effect.target == wanted)
        .map(|effect| effect.ordinal)
        .unwrap_or_else(|| panic!("effect {wanted:?} is journaled"));
    let receipt = txn
        .get_effect_receipts(operation_id)
        .unwrap()
        .into_iter()
        .find(|receipt| receipt.payload().effect_ordinal == Some(ordinal))
        .unwrap_or_else(|| panic!("effect {wanted:?} carries a receipt"));
    drop(txn);
    (ordinal, receipt)
}

/// The ref's lease observes expected-old at entry, then an external writer's
/// value lands inside the window between that observation and the
/// ref-transaction lock (deterministic, opt-in injection). The durable
/// receipt must record the ACTUAL under-lock value as both observed sides —
/// never the stale pre-lock absence, which would send recovery after the
/// wrong value — and recovery must preserve the newer external value instead
/// of restoring the leased old one (review ATOM::aaron::2).
#[cfg(feature = "adoption-test-injection")]
#[test]
fn ref_third_value_inside_the_lock_window_is_receipted_and_preserved() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"target\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "target"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    // The external writer's orphan commit that lands inside the window.
    let external_tree = build_tree(&git, &[("external.txt", b"external\n")]);
    let external = git
        .commit(
            None,
            &signature(),
            &signature(),
            "external writer",
            &git.find_tree(external_tree).unwrap(),
            &[],
        )
        .unwrap();
    let evidence = atomic_core::Hash::of(b"ref-window-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    std::env::set_var(
        "ATOMIC_TEST_INJECT_REF_BEFORE_LOCK",
        format!("refs/atomic/views/agent={external}"),
    );
    let executed = repo.execute_projection_publish(&prepared, &git);
    std::env::remove_var("ATOMIC_TEST_INJECT_REF_BEFORE_LOCK");
    assert!(
        executed.is_err(),
        "the inside-window third value must fail closed: {executed:?}"
    );
    // The immutable receipt carries the actual under-lock value on both
    // observed sides — not the stale pre-lock Absent observation.
    let (_, receipt) = receipt_for(
        &repo,
        prepared.operation_id,
        &EffectTarget::GitRef {
            name: "refs/atomic/views/agent".to_string(),
        },
    );
    assert_eq!(receipt.payload().kind, EffectReceiptKind::LeaseRejected);
    let external_value = EffectValue::GitRef(direct(external));
    assert_eq!(
        receipt.payload().observed_old.as_ref(),
        Some(&external_value),
        "the receipt records the actual third value, not the stale pre-lock observation"
    );
    assert_eq!(receipt.payload().observed_new.as_ref(), Some(&external_value));
    // Recovery preserves the newer external value — it never rolls the ref
    // back to the leased expected-old absence.
    drop(prepared);
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        external,
        "recovery preserves the external value the receipt recorded"
    );
}

/// The HEAD lease's inside-window analogue: an external writer detaches HEAD
/// elsewhere between the initial observation and the HEAD ref-transaction
/// lock. The receipt records the actual detached value and recovery preserves
/// it (review ATOM::aaron::2).
#[cfg(feature = "adoption-test-injection")]
#[test]
fn head_third_value_inside_the_lock_window_is_receipted_and_preserved() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"head\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "head target"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let external_tree = build_tree(&git, &[("external.txt", b"external head\n")]);
    let external = git
        .commit(
            None,
            &signature(),
            &signature(),
            "external writer",
            &git.find_tree(external_tree).unwrap(),
            &[],
        )
        .unwrap();
    let evidence = atomic_core::Hash::of(b"head-window-third-value");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            Some(direct(target)),
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    std::env::set_var("ATOMIC_TEST_INJECT_HEAD_BEFORE_LOCK", format!("{external}"));
    let executed = repo.execute_projection_publish(&prepared, &git);
    std::env::remove_var("ATOMIC_TEST_INJECT_HEAD_BEFORE_LOCK");
    assert!(
        executed.is_err(),
        "the inside-window third value must fail closed: {executed:?}"
    );
    let (_, receipt) = receipt_for(
        &repo,
        prepared.operation_id,
        &EffectTarget::GitHead {
            working_copy,
        },
    );
    assert_eq!(receipt.payload().kind, EffectReceiptKind::LeaseRejected);
    let external_value = EffectValue::GitRef(direct(external));
    assert_eq!(
        receipt.payload().observed_old.as_ref(),
        Some(&external_value),
        "the receipt records the actual detached HEAD, not the stale attached observation"
    );
    assert_eq!(receipt.payload().observed_new.as_ref(), Some(&external_value));
    drop(prepared);
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    assert_eq!(
        git.head().unwrap().target().unwrap(),
        external,
        "recovery preserves the external HEAD value the receipt recorded"
    );
}

/// A concurrent writer that lands the INTENDED value inside the window turns
/// the under-lock comparison into AlreadyApplied: a Recovered receipt from
/// the actual under-lock value, and the publication completes and finalizes —
/// never a false rejection (review ATOM::aaron::2).
#[cfg(feature = "adoption-test-injection")]
#[test]
fn ref_landing_the_intended_value_inside_the_window_is_recovered_not_rejected() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("extra.txt"), b"target\n").unwrap();
    run_git(directory.path(), &["add", "extra.txt"]);
    run_git(directory.path(), &["commit", "-m", "target"]);
    let target = git
        .revparse_single("HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    let evidence = atomic_core::Hash::of(b"ref-window-already-applied");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            Some(direct(target)),
            None,
            Vec::new(),
            None,
            None,
            evidence,
        )
        .unwrap();
    std::env::set_var(
        "ATOMIC_TEST_INJECT_REF_BEFORE_LOCK",
        format!("refs/atomic/views/agent={target}"),
    );
    let executed = repo.execute_projection_publish(&prepared, &git);
    std::env::remove_var("ATOMIC_TEST_INJECT_REF_BEFORE_LOCK");
    executed.expect(
        "an inside-window already-applied lease must not be a false rejection: {executed:?}",
    );
    let (_, receipt) = receipt_for(
        &repo,
        prepared.operation_id,
        &EffectTarget::GitRef {
            name: "refs/atomic/views/agent".to_string(),
        },
    );
    assert_eq!(receipt.payload().kind, EffectReceiptKind::Recovered);
    let target_value = EffectValue::GitRef(direct(target));
    assert_eq!(receipt.payload().observed_old.as_ref(), Some(&target_value));
    assert_eq!(receipt.payload().observed_new.as_ref(), Some(&target_value));
    repo.finalize_projection_publish(prepared, &git).unwrap();
    assert_eq!(
        git.find_reference("refs/atomic/views/agent")
            .unwrap()
            .target()
            .unwrap(),
        target
    );
}

/// An external writer that replaces the checkpoint after the executor's first
/// read but before the guarded write is a third FACTS value: the write is
/// refused, the external bytes survive byte-for-byte, the rejection receipt
/// records the actual external digest read back from disk, and recovery
/// converges without publishing the prepared checkpoint (review ATOM::aaron::2,
/// the CB-7B stable-observation pattern).
#[cfg(feature = "adoption-test-injection")]
#[test]
fn checkpoint_replaced_inside_the_write_window_is_preserved_and_receipted() {
    let (directory, mut repo, git) = colocated();
    let working_copy = repo.require_working_copy_id().unwrap();
    let tree = build_tree(&git, &[("extra.txt", b"checkpoint window\n")]);
    let commit_oid = git
        .commit(
            None,
            &signature(),
            &signature(),
            "checkpoint window",
            &git.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    git.set_head_detached(commit_oid).unwrap();
    let evidence = atomic_core::Hash::of(b"checkpoint-window");
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            None,
            &git,
            "refs/atomic/views/agent",
            None,
            None,
            Vec::new(),
            None,
            Some(ProjectionCheckpointPlan {
                git_head_symref: None,
                git_head: core_oid(commit_oid),
                git_tree: core_oid(tree),
            }),
            evidence,
        )
        .unwrap();
    assert!(prepared.checkpoint_plan);
    // The external writer's replacement: a valid v2 checkpoint naming a
    // different commit — a third FACTS value written by the seam between the
    // executor's first read and the guarded write.
    let external_tree = build_tree(&git, &[("external.txt", b"external checkpoint\n")]);
    let external_commit = git
        .commit(
            None,
            &signature(),
            &signature(),
            "external checkpoint",
            &git.find_tree(external_tree).unwrap(),
            &[],
        )
        .unwrap();
    let state = repo.get_view_info("master").unwrap().state.to_base32();
    let external_checkpoint = super::super::workspace_txn::WorkspaceCheckpoint {
        version: 2,
        view: "master".to_string(),
        atomic_state: state,
        git_head_symref: None,
        git_head: external_commit.to_string(),
        git_tree: external_tree.to_string(),
        git_index_tree: None,
        git_index_digest: None,
    };
    let external_bytes =
        super::super::workspace_txn::workspace_checkpoint_bytes(&external_checkpoint).unwrap();
    let injection = directory.path().join("external-checkpoint.json");
    fs::write(&injection, &external_bytes).unwrap();
    let prepared_bytes = fs::read(checkpoint_path_of(&repo)).unwrap();
    assert_ne!(
        prepared_bytes, external_bytes,
        "the external writer's bytes are genuinely different"
    );
    std::env::set_var("ATOMIC_TEST_INJECT_CHECKPOINT_BEFORE_WRITE", &injection);
    let executed = repo.execute_projection_publish(&prepared, &git);
    std::env::remove_var("ATOMIC_TEST_INJECT_CHECKPOINT_BEFORE_WRITE");
    assert!(
        executed.is_err(),
        "the inside-window replacement must fail closed: {executed:?}"
    );
    // The external bytes survive byte-for-byte: the prepared checkpoint was
    // never written over them.
    assert_eq!(
        fs::read(checkpoint_path_of(&repo)).unwrap(),
        external_bytes,
        "the external checkpoint bytes are preserved"
    );
    // The rejection receipt records the actual external FACTS digest, read
    // back from the real on-disk bytes.
    let external_digest =
        observe_checkpoint_facts(repo.root()).unwrap().expect("external checkpoint on disk");
    let (_, receipt) = receipt_for(
        &repo,
        prepared.operation_id,
        &EffectTarget::Checkpoint {
            working_copy,
            kind: atomic_core::operation::CheckpointKind::Bridge,
        },
    );
    assert_eq!(receipt.payload().kind, EffectReceiptKind::LeaseRejected);
    let external_value = EffectValue::Digest {
        kind: DigestKind::Checkpoint,
        hash: external_digest,
    };
    assert_eq!(
        receipt.payload().observed_old.as_ref(),
        Some(&external_value),
        "the receipt records the actual external FACTS value"
    );
    assert_eq!(receipt.payload().observed_new.as_ref(), Some(&external_value));
    // Recovery converges without publishing the prepared checkpoint.
    drop(prepared);
    let lock = repo.try_lock_operation(working_copy).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    drop(lock);
    assert_eq!(
        fs::read(checkpoint_path_of(&repo)).unwrap(),
        external_bytes,
        "recovery preserves the external checkpoint bytes"
    );
}

fn checkpoint_path_of(repo: &Repository) -> std::path::PathBuf {
    repo.root().join(".atomic/bridge/workspace.json")
}
