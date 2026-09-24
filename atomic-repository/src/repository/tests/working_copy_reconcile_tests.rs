//! Tests for journaled working-copy registration reconciliation.
//!
//! Every test runs on a disposable `tempfile` repository. The scenario under
//! test is the live refusal `working-copy X expects view V at state S, but the
//! view is at T`: a persistent working-copy record whose desired view advanced
//! past the state the record captured.

use super::super::operation::{
    current_operation_timestamp_ms, working_copy_state_ref, PreparedSwitchOperation,
};
use super::*;
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, OperationScope, RepoStateRef, ViewStateRef,
    WorkingCopyStateRef,
};
use atomic_core::pristine::{
    MutTxnT, OperationTxnT, ViewState, ViewTxnT, WorkingCopyMutTxnT, WorkingCopyTxnT,
};
use atomic_core::{Hash, Merkle, OperationId};

fn view_state(repo: &TestRepository, name: &str) -> ViewState {
    let txn = repo.pristine.read_txn().unwrap();
    txn.get_view(name)
        .unwrap()
        .unwrap_or_else(|| panic!("view '{name}' must exist"))
}

/// Force the persistent record's `desired_state` to a stale value without
/// touching the view itself, reproducing the live preflight failure.
fn force_stale_desired_state(repo: &TestRepository, stale: Merkle) {
    let mut txn = repo.pristine.write_txn().unwrap();
    let mut record = txn
        .get_working_copy(repo.working_copy())
        .unwrap()
        .expect("working-copy record");
    record.desired_state = stale;
    txn.put_working_copy(&record).unwrap();
    txn.commit().unwrap();
}

fn advance_view_state(repo: &TestRepository, name: &str) -> Merkle {
    let mut txn = repo.pristine.write_txn().unwrap();
    let mut view = txn.get_view(name).unwrap().expect("view");
    let next = Hash::of(format!("advanced:{name}").as_bytes());
    view.state = next;
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();
    next
}

fn working_copy_scope(wc: WorkingCopyId) -> OperationScope {
    OperationScope::WorkingCopy(wc)
}

fn repository_head(repo: &TestRepository) -> Result<OperationId, RepositoryError> {
    repo.sole_operation_head(OperationScope::Repository)
}

fn operation_count(repo: &TestRepository) -> usize {
    let txn = repo.pristine.read_txn().unwrap();
    txn.list_operations().unwrap().len()
}

/// Build the exact reconciliation operation without finalizing it, so a test
/// can inject a race between prepare/apply and finalize.
#[allow(clippy::too_many_arguments)]
fn prepare_reconcile(
    repo: &TestRepository,
    previous: &ViewState,
    target: &ViewState,
    before: &WorkingCopyStateRef,
    after: &WorkingCopyStateRef,
) -> (
    PreparedSwitchOperation,
    super::super::locks::WorkingCopyOperationLockGuard,
) {
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let before_state = RepoStateRef {
        view: Some(ViewStateRef {
            name: previous.name.clone(),
            state: previous.state,
            set_id: None,
        }),
        working_copy: Some(before.clone()),
        git: None,
    };
    let after_state = RepoStateRef {
        view: Some(ViewStateRef {
            name: target.name.clone(),
            state: target.state,
            set_id: None,
        }),
        working_copy: Some(after.clone()),
        git: None,
    };
    let effect = EffectPlan {
        ordinal: 0,
        target: EffectTarget::WorkingCopy {
            working_copy: repo.working_copy(),
        },
        expected_old: EffectValue::WorkingCopy(before.clone()),
        expected_new: EffectValue::WorkingCopy(after.clone()),
    };
    let prepared = repo
        .prepare_working_copy_transition(
            &lock,
            atomic_core::operation::OperationKind::ReconcileWorkingCopy,
            None,
            before_state,
            after_state,
            vec![effect],
            Vec::new(),
            ActorRef::System {
                name: "reconcile-test".to_string(),
            },
            current_operation_timestamp_ms(),
        )
        .unwrap();
    (prepared, lock)
}

fn transition_refs(
    repo: &TestRepository,
    target_name: &str,
) -> (
    ViewState,
    ViewState,
    WorkingCopyStateRef,
    WorkingCopyStateRef,
) {
    let record = repo.working_copy_record(repo.working_copy()).unwrap();
    let previous = view_state(repo, "dev");
    let target = view_state(repo, target_name);
    let before = working_copy_state_ref(record);
    let mut after = before.clone();
    after.desired_view = target.id;
    after.desired_state = target.state;
    after.materialized_state = None;
    after.materialized_manifest = None;
    (previous, target, before, after)
}

#[test]
fn stale_same_view_registration_reconciles_and_is_idempotent() {
    let (_dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();
    let stale = Hash::of(b"stale-desired-state");
    force_stale_desired_state(&repo, stale);

    let diagnosis = repo.inspect_working_copy_registration(wc, "dev").unwrap();
    assert!(diagnosis.location_matches);
    assert!(diagnosis.desired_view_is_stale);
    assert!(!diagnosis.already_reconciled);
    assert_eq!(diagnosis.desired_state, stale);

    let outcome = repo.reconcile_working_copy_registration(wc, "dev").unwrap();
    let operation = outcome.operation().expect("a mutation must be journaled");

    let record = repo.working_copy_record(wc).unwrap();
    assert_eq!(record.desired_view, view_state(&repo, "dev").id);
    assert_eq!(record.desired_state, view_state(&repo, "dev").state);
    assert!(record.materialized_state.is_none());
    assert!(record.materialized_manifest.is_none());

    let details = repo.operation_details(operation).unwrap();
    assert_eq!(
        details.operation.payload().kind,
        atomic_core::operation::OperationKind::ReconcileWorkingCopy
    );
    assert_eq!(details.operation.encoding_version(), 2);
    assert_eq!(
        details.verification,
        super::super::operation::OperationVerificationState::Verified
    );
    assert_eq!(
        details.head_of,
        vec![working_copy_scope(wc)],
        "the reconciliation is only a working-copy-scope head"
    );

    // Repeat: a second call is an idempotent no-op that writes nothing.
    let before_ops = operation_count(&repo);
    let again = repo.reconcile_working_copy_registration(wc, "dev").unwrap();
    match again {
        WorkingCopyReconcileOutcome::AlreadyReconciled { diagnosis } => {
            assert!(diagnosis.already_reconciled);
        }
        other => panic!("expected idempotent no-op, got {other:?}"),
    }
    assert_eq!(operation_count(&repo), before_ops);
}

#[test]
fn reconcile_rebinds_to_a_different_existing_view_and_preserves_membership() {
    let (_dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();
    repo.create_view("recon-target").unwrap();
    force_stale_desired_state(&repo, Hash::of(b"stale-before-rebind"));

    let dev_before = view_state(&repo, "dev");
    let target_before = view_state(&repo, "recon-target");
    let working_copies_before = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.list_working_copies().unwrap()
    };
    let repository_head_before = repository_head(&repo).ok();

    let outcome = repo
        .reconcile_working_copy_registration(wc, "recon-target")
        .unwrap();
    let operation = outcome.operation().unwrap();

    let record = repo.working_copy_record(wc).unwrap();
    assert_eq!(record.desired_view, target_before.id);
    assert_eq!(record.desired_state, target_before.state);
    assert!(record.materialized_state.is_none());

    // No view membership or state changed, and no other working copy changed.
    assert_eq!(view_state(&repo, "dev"), dev_before);
    assert_eq!(view_state(&repo, "recon-target"), target_before);
    let working_copies_after = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.list_working_copies().unwrap()
    };
    assert_eq!(working_copies_before.len(), working_copies_after.len());
    assert_eq!(working_copies_before[0].id, working_copies_after[0].id);
    assert_eq!(
        working_copies_before[0].location_fingerprint,
        working_copies_after[0].location_fingerprint
    );
    assert_eq!(repository_head(&repo).ok(), repository_head_before);

    // The only effect is the working-copy record transition; no filesystem,
    // Git, shelf, or view-membership effect is present.
    let details = repo.operation_details(operation).unwrap();
    let effects = &details.operation.payload().delta.effects;
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects[0].target,
        EffectTarget::WorkingCopy { .. }
    ));
    assert!(details.operation.payload().delta.metadata.is_empty());
}

#[test]
fn reconcile_refuses_missing_target_and_unknown_working_copy() {
    let (_dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();

    let operations_before = operation_count(&repo);
    let missing = repo
        .reconcile_working_copy_registration(wc, "no-such-view")
        .unwrap_err();
    assert!(matches!(missing, RepositoryError::ViewNotFound { .. }));
    // Failure before the prepare step wrote no operation.
    assert_eq!(operation_count(&repo), operations_before);

    let unknown = repo
        .reconcile_working_copy_registration(WorkingCopyId::new(), "dev")
        .unwrap_err();
    assert!(
        matches!(
            unknown,
            RepositoryError::WorkingCopyIdentityMismatch { .. }
                | RepositoryError::WorkingCopyRecordNotFound { .. }
        ),
        "unexpected error: {unknown:?}"
    );
}

#[test]
fn reconcile_preserves_dirty_working_copy_bytes() {
    let (dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();
    repo.create_view("recon-target").unwrap();

    // A tracked file, recorded and then dirtied in place.
    std::fs::write(dir.path().join("tracked.txt"), b"base\n").unwrap();
    repo.add_batch(&["tracked.txt"]).unwrap();
    repo.record_all("base").unwrap();
    std::fs::write(dir.path().join("tracked.txt"), b"base\ndirty edit\n").unwrap();
    // An untracked scratch file.
    std::fs::write(dir.path().join("scratch.bin"), b"\x00\x01untracked").unwrap();
    force_stale_desired_state(&repo, Hash::of(b"stale-with-dirty-tree"));

    repo.reconcile_working_copy_registration(wc, "recon-target")
        .unwrap();

    assert_eq!(
        std::fs::read(dir.path().join("tracked.txt")).unwrap(),
        b"base\ndirty edit\n"
    );
    assert_eq!(
        std::fs::read(dir.path().join("scratch.bin")).unwrap(),
        b"\x00\x01untracked"
    );
}

#[test]
fn inspect_and_dry_run_write_nothing() {
    let (_dir, repo) = create_temp_repo();
    let wc = repo.working_copy();
    force_stale_desired_state(&repo, Hash::of(b"stale-inspect"));

    let record_before = repo.working_copy_record(wc).unwrap();
    let operations_before = operation_count(&repo);
    let heads_before = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_operation_heads(working_copy_scope(wc)).unwrap()
    };

    let diagnosis = repo.inspect_working_copy_registration(wc, "dev").unwrap();
    assert!(diagnosis.desired_view_is_stale);

    assert_eq!(repo.working_copy_record(wc).unwrap(), record_before);
    assert_eq!(operation_count(&repo), operations_before);
    let heads_after = {
        let txn = repo.pristine.read_txn().unwrap();
        txn.get_operation_heads(working_copy_scope(wc)).unwrap()
    };
    assert_eq!(heads_before, heads_after);
}

#[test]
fn moved_target_view_state_is_refused_at_finalize() {
    let (_dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();
    repo.create_view("recon-target").unwrap();
    force_stale_desired_state(&repo, Hash::of(b"stale-moved-target"));

    let (previous, target, before, after) = transition_refs(&repo, "recon-target");
    let (prepared, lock) = prepare_reconcile(&repo, &previous, &target, &before, &after);
    let operation = prepared.operation().id();

    repo.apply_operation_metadata_locked(&lock, operation)
        .unwrap();

    // A concurrent writer advances the target view after the operation was
    // prepared and applied; the operation-level view lease must refuse.
    let advanced = advance_view_state(&repo, "recon-target");
    assert_ne!(advanced, target.state);

    let refused = repo
        .finalize_operation_verified(&lock, operation)
        .unwrap_err();
    assert!(
        matches!(refused, RepositoryError::InvalidOperation { .. }),
        "unexpected error: {refused:?}"
    );

    // Recovery under fresh leases rolls the record back to its prior value.
    repo.recover_incomplete_operation(&lock).unwrap();
    let record = repo.working_copy_record(wc).unwrap();
    assert_eq!(record.desired_state, Hash::of(b"stale-moved-target"));
    assert_eq!(record.desired_view, view_state(&repo, "dev").id);
}

#[test]
fn prepared_reconcile_without_finalize_is_recovered_on_reopen() {
    let (dir, mut repo) = create_temp_repo();
    let wc = repo.working_copy();
    repo.create_view("recon-target").unwrap();
    let stale = Hash::of(b"stale-crash-before-finalize");
    force_stale_desired_state(&repo, stale);

    let (previous, target, before, after) = transition_refs(&repo, "recon-target");
    let (prepared, lock) = prepare_reconcile(&repo, &previous, &target, &before, &after);
    let operation = prepared.operation().id();
    // Simulate a crash after the metadata effect landed but before the
    // operation-level verified receipt.
    repo.apply_operation_metadata_locked(&lock, operation)
        .unwrap();
    assert_eq!(
        repo.working_copy_record(wc).unwrap().desired_view,
        target.id
    );
    drop(lock);
    drop(repo);

    // A fresh ordinary open performs the idempotent recovery and rolls the
    // record back to the pre-operation value.
    let reopened = Repository::open(dir.path()).unwrap();
    let record = reopened.working_copy_record(wc).unwrap();
    assert_eq!(record.desired_state, stale);
    assert_eq!(record.desired_view, view_state_repo(&reopened, "dev").id);

    let head = reopened
        .sole_operation_head(working_copy_scope(wc))
        .unwrap();
    let details = reopened.operation_details(head).unwrap();
    assert_eq!(
        details.verification,
        super::super::operation::OperationVerificationState::Verified
    );
}

fn view_state_repo(repo: &Repository, name: &str) -> ViewState {
    let txn = repo.pristine.read_txn().unwrap();
    txn.get_view(name).unwrap().unwrap()
}
