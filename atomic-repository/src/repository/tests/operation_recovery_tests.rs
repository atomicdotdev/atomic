use super::super::operation::{
    classify_effect_lease, deterministic_effect_receipt, has_operation_verified_receipt,
    LeaseClassification, RecoveryOutcome,
};
use super::*;
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectReceiptKind, EffectTarget, EffectValue, FileKind, FileState,
    Operation, OperationKind, OperationPayload, RepoStateDelta, RepoStateRef, WorkingCopyStateRef,
};
use atomic_core::pristine::OperationTxnT;
use atomic_core::{Hash, OperationId};

fn operation_state(repo: &TestRepository) -> RepoStateRef {
    let record = repo.working_copy_record(repo.working_copy()).unwrap();
    RepoStateRef {
        view: None,
        working_copy: Some(WorkingCopyStateRef {
            id: record.id,
            location_fingerprint: record.location_fingerprint,
            desired_view: record.desired_view,
            desired_state: record.desired_state,
            materialized_state: record.materialized_state,
            materialized_manifest: record.materialized_manifest,
        }),
        git: None,
    }
}

fn actor() -> ActorRef {
    ActorRef::System {
        name: "operation-recovery-test".to_string(),
    }
}

fn regular_file_plan(repo: &TestRepository, path: &str, new_bytes: &[u8]) -> EffectPlan {
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let old = repo
        .observe_filesystem_effect(repo.working_copy(), &target)
        .unwrap();
    let EffectValue::File(old_file) = &old else {
        panic!("test fixture must begin with a regular file");
    };
    assert_eq!(old_file.kind, FileKind::Regular);
    let mode = old_file.mode;
    EffectPlan {
        ordinal: 0,
        target,
        expected_old: old,
        expected_new: EffectValue::File(FileState {
            kind: FileKind::Regular,
            mode,
            content: Hash::of(new_bytes),
        }),
    }
}

fn prepare_file_switch(
    repo: &TestRepository,
    path: &str,
    new_bytes: &[u8],
) -> (super::super::operation::PreparedSwitchOperation, EffectPlan) {
    let plan = regular_file_plan(repo, path, new_bytes);
    let state = operation_state(repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, vec![plan.clone()], actor(), 42)
        .unwrap();
    drop(lock);
    (prepared, plan)
}

fn prepare_working_copy_chain(
    repo: &TestRepository,
) -> (
    super::super::operation::PreparedSwitchOperation,
    WorkingCopyStateRef,
    WorkingCopyStateRef,
    WorkingCopyStateRef,
) {
    let state = operation_state(repo);
    let initial = state.working_copy.clone().unwrap();
    let mut intermediate = initial.clone();
    intermediate.desired_state = Hash::of(b"intermediate working-copy state");
    intermediate.materialized_state = None;
    intermediate.materialized_manifest = None;
    let mut final_state = intermediate.clone();
    final_state.desired_state = Hash::of(b"final working-copy state");
    final_state.materialized_state = Some(final_state.desired_state);
    let target = EffectTarget::WorkingCopy {
        working_copy: repo.working_copy(),
    };
    let effects = vec![
        EffectPlan {
            ordinal: 0,
            target: target.clone(),
            expected_old: EffectValue::WorkingCopy(initial.clone()),
            expected_new: EffectValue::WorkingCopy(intermediate.clone()),
        },
        EffectPlan {
            ordinal: 1,
            target,
            expected_old: EffectValue::WorkingCopy(intermediate.clone()),
            expected_new: EffectValue::WorkingCopy(final_state.clone()),
        },
    ];
    let after = RepoStateRef {
        view: None,
        working_copy: Some(final_state.clone()),
        git: None,
    };
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state, after, effects, actor(), 91)
        .unwrap();
    drop(lock);
    (prepared, initial, intermediate, final_state)
}

#[test]
fn classifier_distinguishes_old_new_and_third_values() {
    let old = EffectValue::Absent;
    let new = EffectValue::File(FileState {
        kind: FileKind::Regular,
        mode: 0o644,
        content: Hash::of(b"new"),
    });
    let third = EffectValue::File(FileState {
        kind: FileKind::Regular,
        mode: 0o644,
        content: Hash::of(b"third"),
    });

    assert_eq!(
        classify_effect_lease(&old, &old, &new),
        LeaseClassification::Apply
    );
    assert_eq!(
        classify_effect_lease(&new, &old, &new),
        LeaseClassification::AlreadyApplied
    );
    assert_eq!(
        classify_effect_lease(&third, &old, &new),
        LeaseClassification::Diverged
    );
}

#[test]
fn deterministic_receipts_reuse_identity_and_verified_is_detected() {
    let operation = Operation::new(OperationPayload {
        parents: vec![OperationId::from_bytes([7; 32])],
        kind: OperationKind::SwitchView,
        relation: None,
        working_copy: None,
        before: RepoStateRef::EMPTY,
        delta: RepoStateDelta {
            after: RepoStateRef::EMPTY,
            metadata: Vec::new(),
            effects: vec![EffectPlan {
                ordinal: 0,
                target: EffectTarget::FilesystemPath {
                    path: "file.txt".to_string(),
                },
                expected_old: EffectValue::Absent,
                expected_new: EffectValue::File(FileState {
                    kind: FileKind::Regular,
                    mode: 0o644,
                    content: Hash::of(b"new"),
                }),
            }],
        },
        git_observed: Vec::new(),
        evidence: Vec::new(),
        actor: actor(),
        timestamp_ms: 1234,
        lossy: Vec::new(),
    })
    .unwrap();

    let first = deterministic_effect_receipt(
        &operation,
        Some(0),
        EffectReceiptKind::Recovered,
        Some(operation.payload().delta.effects[0].expected_new.clone()),
        Some(operation.payload().delta.effects[0].expected_new.clone()),
    )
    .unwrap();
    let second = deterministic_effect_receipt(
        &operation,
        Some(0),
        EffectReceiptKind::Recovered,
        Some(operation.payload().delta.effects[0].expected_new.clone()),
        Some(operation.payload().delta.effects[0].expected_new.clone()),
    )
    .unwrap();
    assert_eq!(first.id(), second.id());
    assert_eq!(first, second);

    let verified =
        deterministic_effect_receipt(&operation, None, EffectReceiptKind::Verified, None, None)
            .unwrap();
    assert!(has_operation_verified_receipt(&[first, verified]));
}

#[test]
fn execution_rejects_third_value_before_mutation() {
    let (temp, repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let target_bytes = b"target contents\n";
    let third_bytes = b"newer external contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, _) = prepare_file_switch(&repo, path, target_bytes);
    std::fs::write(temp.path().join(path), third_bytes).unwrap();
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let error = repo
        .execute_filesystem_effect(&lock, prepared.operation().id(), 0, Some(target_bytes))
        .unwrap_err();
    assert!(error.to_string().contains("diverged"));
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), third_bytes);
}

#[cfg(unix)]
#[test]
fn atomic_file_effect_replaces_symlink_without_following_it() {
    use std::os::unix::fs::symlink;

    let (temp, mut repo) = create_temp_repo();
    let outside = temp.path().join("outside.txt");
    let path = "tracked.txt";
    std::fs::write(&outside, b"outside remains unchanged\n").unwrap();
    symlink(&outside, temp.path().join(path)).unwrap();
    let target_bytes = b"materialized target\n";
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let expected_old = repo
        .observe_filesystem_effect(repo.working_copy(), &target)
        .unwrap();
    let plan = EffectPlan {
        ordinal: 0,
        target,
        expected_old,
        expected_new: EffectValue::File(FileState {
            kind: FileKind::Regular,
            mode: 0o644,
            content: Hash::of(target_bytes),
        }),
    };
    let state = operation_state(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, vec![plan], actor(), 88)
        .unwrap();
    let pending = repo
        .execute_filesystem_effect(&lock, prepared.operation().id(), 0, Some(target_bytes))
        .unwrap();
    repo.record_pending_filesystem_effect(&lock, prepared.operation().id(), pending)
        .unwrap();
    assert!(!std::fs::symlink_metadata(temp.path().join(path))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), target_bytes);
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"outside remains unchanged\n"
    );

    repo.recover_incomplete_operation(&lock).unwrap();
    assert!(std::fs::symlink_metadata(temp.path().join(path))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"outside remains unchanged\n"
    );
}

#[test]
fn directory_effect_gap_recovers_to_absence() {
    let (temp, mut repo) = create_temp_repo();
    let path = "empty-dir";
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let state = operation_state(&repo);
    let plan = EffectPlan {
        ordinal: 0,
        target,
        expected_old: EffectValue::Absent,
        expected_new: super::super::operation::filesystem_directory_value(0o755),
    };
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, vec![plan], actor(), 89)
        .unwrap();
    std::fs::create_dir(temp.path().join(path)).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    assert!(!temp.path().join(path).exists());
    let txn = repo.pristine().read_txn().unwrap();
    let heads = txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            repo.working_copy(),
        ))
        .unwrap();
    assert_ne!(heads.as_slice()[0], prepared.operation().id());
}

#[test]
fn file_to_directory_transition_uses_two_receipted_stages() {
    let (temp, repo) = create_temp_repo();
    let path = "kind-change";
    std::fs::write(temp.path().join(path), b"file bytes").unwrap();
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let old = repo
        .observe_filesystem_effect(repo.working_copy(), &target)
        .unwrap();
    let effects = vec![
        EffectPlan {
            ordinal: 0,
            target: target.clone(),
            expected_old: old,
            expected_new: EffectValue::Absent,
        },
        EffectPlan {
            ordinal: 1,
            target,
            expected_old: EffectValue::Absent,
            expected_new: super::super::operation::filesystem_directory_value(0o755),
        },
    ];
    let state = operation_state(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, effects, actor(), 92)
        .unwrap();
    for ordinal in 0..=1 {
        let pending = repo
            .execute_filesystem_effect(&lock, prepared.operation().id(), ordinal, None)
            .unwrap();
        repo.record_pending_filesystem_effect(&lock, prepared.operation().id(), pending)
            .unwrap();
    }
    repo.finalize_operation_verified(&lock, prepared.operation().id())
        .unwrap();
    assert!(temp.path().join(path).is_dir());
}

#[test]
fn directory_to_file_transition_uses_two_receipted_stages() {
    let (temp, repo) = create_temp_repo();
    let path = "kind-change";
    std::fs::create_dir(temp.path().join(path)).unwrap();
    let bytes = b"file bytes";
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let old = repo
        .observe_filesystem_effect(repo.working_copy(), &target)
        .unwrap();
    let effects = vec![
        EffectPlan {
            ordinal: 0,
            target: target.clone(),
            expected_old: old,
            expected_new: EffectValue::Absent,
        },
        EffectPlan {
            ordinal: 1,
            target,
            expected_old: EffectValue::Absent,
            expected_new: EffectValue::File(FileState {
                kind: FileKind::Regular,
                mode: 0o644,
                content: Hash::of(bytes),
            }),
        },
    ];
    let state = operation_state(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, effects, actor(), 93)
        .unwrap();
    let remove = repo
        .execute_filesystem_effect(&lock, prepared.operation().id(), 0, None)
        .unwrap();
    repo.record_pending_filesystem_effect(&lock, prepared.operation().id(), remove)
        .unwrap();
    let write = repo
        .execute_filesystem_effect(&lock, prepared.operation().id(), 1, Some(bytes))
        .unwrap();
    repo.record_pending_filesystem_effect(&lock, prepared.operation().id(), write)
        .unwrap();
    repo.finalize_operation_verified(&lock, prepared.operation().id())
        .unwrap();
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), bytes);
}

#[test]
fn recovery_before_first_chained_effect_is_a_noop() {
    let (temp, repo) = create_temp_repo();
    let (_prepared, initial, _, _) = prepare_working_copy_chain(&repo);
    drop(repo);

    let reopened = Repository::open(temp.path()).unwrap();
    let record = reopened
        .working_copy_record(reopened.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(
        WorkingCopyStateRef {
            id: record.id,
            location_fingerprint: record.location_fingerprint,
            desired_view: record.desired_view,
            desired_state: record.desired_state,
            materialized_state: record.materialized_state,
            materialized_manifest: record.materialized_manifest,
        },
        initial
    );
}

#[test]
fn recovery_between_chained_effects_reverses_only_the_reached_stage() {
    let (temp, mut repo) = create_temp_repo();
    let (_prepared, initial, intermediate, _) = prepare_working_copy_chain(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    repo.apply_working_copy_state_locked(&lock, &intermediate)
        .unwrap();
    drop(lock);
    drop(repo);

    let reopened = Repository::open(temp.path()).unwrap();
    let record = reopened
        .working_copy_record(reopened.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(
        WorkingCopyStateRef {
            id: record.id,
            location_fingerprint: record.location_fingerprint,
            desired_view: record.desired_view,
            desired_state: record.desired_state,
            materialized_state: record.materialized_state,
            materialized_manifest: record.materialized_manifest,
        },
        initial
    );
}

/// CB-13C observability: an executed recovery is recorded in the bridge
/// event journal with the original/recovery operation IDs and whether a
/// new `Recover` operation was created. The automatic sink is
/// consent-gated (review R1/R4), so the fixture opts in first.
#[test]
fn executed_recovery_is_recorded_in_the_event_journal() {
    let (temp, mut repo) = create_temp_repo();
    opt_in_bridge_telemetry(temp.path());
    let (_prepared, initial, intermediate, _) = prepare_working_copy_chain(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    repo.apply_working_copy_state_locked(&lock, &intermediate)
        .unwrap();
    drop(lock);
    drop(repo);

    // Reopening performs the idempotent recovery before accepting new work.
    let reopened = Repository::open(temp.path()).unwrap();
    let record = reopened
        .working_copy_record(reopened.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(
        record.materialized_state, initial.materialized_state,
        "the interrupted stage was reversed"
    );

    let journal = temp
        .path()
        .join(super::super::DOT_DIR)
        .join("bridge/events.jsonl");
    let text = std::fs::read_to_string(&journal).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("\"recovery\""))
        .expect("the recovery outcome is recorded");
    let event: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(event["event"], "recovery");
    assert_eq!(
        event["created"], true,
        "a new Recover operation was appended"
    );
    assert!(
        event["original"].as_str().is_some_and(|id| !id.is_empty()),
        "the original operation id is recorded"
    );
    assert!(
        event["recovery"].as_str().is_some_and(|id| !id.is_empty()),
        "the recovery operation id is recorded"
    );
}

/// Append the explicit bridge opt-in to the repository config: the
/// automatic recovery journal sink is consent-gated (review R1/R4).
fn opt_in_bridge_telemetry(root: &std::path::Path) {
    use std::io::Write as _;
    let config = root.join(super::super::DOT_DIR).join("config.toml");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&config)
        .unwrap();
    writeln!(file, "\n[git.bridge]\nenabled = true\n").unwrap();
}

/// Review R5: a recovery that fails closed — here a replay lease
/// rejection because the observed file content matches neither the old
/// nor the new value — records a `recovery_failure` event with the
/// original operation and a stable reason, and the writable open fails
/// closed.
#[test]
fn failed_recovery_records_a_failure_event() {
    let (temp, repo) = create_temp_repo();
    opt_in_bridge_telemetry(temp.path());
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let target_bytes = b"target contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, _) = prepare_file_switch(&repo, path, target_bytes);
    // Execute the operation's file effect (receipt Applied, operation not
    // yet finalized), then corrupt the file to a third lease value: the
    // recovery inverse must fail closed on reopen.
    {
        let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
        repo.execute_filesystem_effect(
            &lock,
            prepared.operation().id(),
            0,
            Some(target_bytes.as_slice()),
        )
        .unwrap();
    }
    std::fs::write(temp.path().join(path), b"newer external contents\n").unwrap();
    drop(repo);

    // Reopening performs the idempotent recovery before accepting new
    // work; the diverged lease fails closed.
    Repository::open(temp.path()).unwrap_err();

    let journal = temp
        .path()
        .join(super::super::DOT_DIR)
        .join("bridge/events.jsonl");
    let text = std::fs::read_to_string(&journal).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("recovery_failure"))
        .expect("the recovery failure is recorded");
    let event: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(event["event"], "recovery_failure");
    assert_eq!(event["reason"], "inverse_construction");
    assert!(
        event["original"].as_str().is_some_and(|id| !id.is_empty()),
        "the original operation id is recorded"
    );
}

#[test]
fn recovery_handles_current_new_without_original_receipt() {
    let (temp, mut repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, _) = prepare_file_switch(&repo, path, new_bytes);
    // repo.root() is canonical (e.g. macOS /var -> /private/var); the
    // expected backup root must be derived from the canonical root.
    let canonical_root = std::fs::canonicalize(temp.path()).unwrap();
    assert_eq!(
        prepared.backup_root(),
        canonical_root
            .join(".atomic/working-copies")
            .join(repo.working_copy().to_string())
            .join("operation-recovery")
            .join(prepared.operation().id().to_string())
    );
    assert!(prepared.backup_root().join("complete").is_file());

    std::fs::write(temp.path().join(path), new_bytes).unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    assert!(txn
        .get_effect_receipts(prepared.operation().id())
        .unwrap()
        .is_empty());
    drop(txn);

    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let outcome = repo.recover_incomplete_operation(&lock).unwrap();
    let recovery = match outcome {
        RecoveryOutcome::Recovered {
            original,
            recovery,
            created,
        } => {
            assert_eq!(original, prepared.operation().id());
            assert!(created);
            recovery
        }
        other => panic!("expected recovery, got {other:?}"),
    };
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), old_bytes);
    assert!(repo.operation_is_verified(recovery).unwrap());
}

#[test]
fn open_existing_does_not_create_an_anchor_without_recovery_work() {
    let (temp, repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    drop(repo);

    let reopened = Repository::open_existing(temp.path()).unwrap();
    let txn = reopened.pristine().read_txn().unwrap();
    assert!(txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            working_copy,
        ))
        .unwrap()
        .is_empty());
}

#[test]
fn readonly_reopen_refuses_incomplete_operation() {
    let (temp, repo) = create_temp_repo();
    let path = "tracked.txt";
    std::fs::write(temp.path().join(path), b"old contents\n").unwrap();
    let (_prepared, _) = prepare_file_switch(&repo, path, b"new contents\n");
    drop(repo);

    let error = Repository::open_readonly(temp.path()).unwrap_err();
    assert!(matches!(error, RepositoryError::InvalidOperation { .. }));
    assert!(error.to_string().contains("still completing"));
}

#[test]
fn writable_reopen_recovers_effect_without_receipt() {
    let (temp, repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, _) = prepare_file_switch(&repo, path, new_bytes);
    std::fs::write(temp.path().join(path), new_bytes).unwrap();
    let original = prepared.operation().id();
    drop(prepared);
    drop(repo);

    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), old_bytes);
    let txn = reopened.pristine().read_txn().unwrap();
    let heads = txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            reopened.require_working_copy_id().unwrap(),
        ))
        .unwrap();
    let recovery = txn.get_operation(heads.as_slice()[0]).unwrap().unwrap();
    assert_eq!(recovery.payload().kind, OperationKind::Recover);
    assert_eq!(recovery.payload().parents, vec![original]);
    assert!(has_operation_verified_receipt(
        &txn.get_effect_receipts(recovery.id()).unwrap()
    ));
}

#[test]
fn writable_reopen_recovers_before_first_effect_as_noop() {
    let (temp, repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (_prepared, _) = prepare_file_switch(&repo, path, new_bytes);
    drop(repo);

    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), old_bytes);
    let txn = reopened.pristine().read_txn().unwrap();
    let heads = txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            reopened.require_working_copy_id().unwrap(),
        ))
        .unwrap();
    let recovery = txn.get_operation(heads.as_slice()[0]).unwrap().unwrap();
    assert_eq!(recovery.payload().kind, OperationKind::Recover);
    assert!(has_operation_verified_receipt(
        &txn.get_effect_receipts(recovery.id()).unwrap()
    ));
}

#[test]
fn shelf_recovery_restores_under_the_ordered_final_lock() {
    let (_temp, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let relative = "cache/state.bin";
    let shelf_path = repo
        .dot_dir()
        .join("working-copies")
        .join(working_copy.to_string())
        .join("workspaces/feature")
        .join(relative);
    std::fs::create_dir_all(shelf_path.parent().unwrap()).unwrap();
    std::fs::write(&shelf_path, b"shelved bytes").unwrap();
    let target = EffectTarget::ShelfPath {
        working_copy,
        view: "feature".to_string(),
        path: relative.to_string(),
    };
    let expected_old = repo
        .observe_filesystem_effect(working_copy, &target)
        .unwrap();
    let plan = EffectPlan {
        ordinal: 0,
        target,
        expected_old,
        expected_new: EffectValue::Absent,
    };
    let state = operation_state(&repo);
    let operation_lock = repo.try_lock_operation(working_copy).unwrap();
    let prepared = repo
        .prepare_switch_operation(
            &operation_lock,
            state.clone(),
            state,
            vec![plan],
            actor(),
            77,
        )
        .unwrap();
    std::fs::remove_file(&shelf_path).unwrap();
    let outcome = repo.recover_incomplete_operation(&operation_lock).unwrap();
    let recovery = match outcome {
        RecoveryOutcome::Recovered { recovery, .. } => recovery,
        other => panic!("expected shelf recovery, got {other:?}"),
    };
    assert_eq!(std::fs::read(&shelf_path).unwrap(), b"shelved bytes");
    assert!(repo.operation_is_verified(recovery).unwrap());
    assert!(prepared.backup_root().join("complete").is_file());
}

#[test]
fn cyclic_target_before_first_receipt_is_classified_as_not_started() {
    let (temp, mut repo) = create_temp_repo();
    let path = "cycle.txt";
    std::fs::write(temp.path().join(path), b"same bytes").unwrap();
    let target = EffectTarget::FilesystemPath {
        path: path.to_string(),
    };
    let old = repo
        .observe_filesystem_effect(repo.working_copy(), &target)
        .unwrap();
    let effects = vec![
        EffectPlan {
            ordinal: 0,
            target: target.clone(),
            expected_old: old.clone(),
            expected_new: EffectValue::Absent,
        },
        EffectPlan {
            ordinal: 1,
            target,
            expected_old: EffectValue::Absent,
            expected_new: old,
        },
    ];
    let state = operation_state(&repo);
    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    repo.prepare_switch_operation(&lock, state.clone(), state, effects, actor(), 78)
        .unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    assert_eq!(
        std::fs::read(temp.path().join(path)).unwrap(),
        b"same bytes"
    );
}

#[test]
fn shelf_transfer_crash_recovers_both_sides_of_a_chained_rename() {
    let (_temp, mut repo) = create_temp_repo();
    let working_copy = repo.working_copy();
    let relative = "cache/state.bin";
    let workspace_path = repo.root().join(relative);
    let shelf_path = repo
        .dot_dir()
        .join("working-copies")
        .join(working_copy.to_string())
        .join("workspaces/dev")
        .join(relative);
    std::fs::create_dir_all(workspace_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(shelf_path.parent().unwrap()).unwrap();
    std::fs::write(&workspace_path, b"same bytes").unwrap();
    std::fs::write(&shelf_path, b"same bytes").unwrap();
    let workspace = EffectTarget::WorkspacePath {
        working_copy,
        path: relative.to_string(),
    };
    let shelf = EffectTarget::ShelfPath {
        working_copy,
        view: "dev".to_string(),
        path: relative.to_string(),
    };
    let workspace_old = repo
        .observe_filesystem_effect(working_copy, &workspace)
        .unwrap();
    let shelf_old = repo
        .observe_filesystem_effect(working_copy, &shelf)
        .unwrap();
    assert_eq!(workspace_old, shelf_old);
    let effects = vec![
        EffectPlan {
            ordinal: 0,
            target: shelf.clone(),
            expected_old: shelf_old.clone(),
            expected_new: EffectValue::Absent,
        },
        EffectPlan {
            ordinal: 1,
            target: workspace,
            expected_old: workspace_old.clone(),
            expected_new: EffectValue::Absent,
        },
        EffectPlan {
            ordinal: 2,
            target: shelf.clone(),
            expected_old: EffectValue::Absent,
            expected_new: workspace_old,
        },
    ];
    let state = operation_state(&repo);
    let lock = repo.try_lock_operation(working_copy).unwrap();
    let prepared = repo
        .prepare_switch_operation(&lock, state.clone(), state, effects, actor(), 79)
        .unwrap();

    let first = &prepared.operation().payload().delta.effects[0];
    let before = repo
        .observe_filesystem_effect(working_copy, &first.target)
        .unwrap();
    std::fs::remove_file(&shelf_path).unwrap();
    let after = repo
        .observe_filesystem_effect(working_copy, &first.target)
        .unwrap();
    let write = lock.begin_write_immediate().unwrap();
    let mut txn = write.try_lock_shelf().unwrap();
    repo.append_successful_effect_outcome(
        &mut txn,
        prepared.operation(),
        first.ordinal,
        before,
        after,
    )
    .unwrap();
    txn.commit().unwrap();

    std::fs::rename(&workspace_path, &shelf_path).unwrap();
    repo.recover_incomplete_operation(&lock).unwrap();
    assert_eq!(std::fs::read(&workspace_path).unwrap(), b"same bytes");
    assert_eq!(std::fs::read(&shelf_path).unwrap(), b"same bytes");
}

#[test]
fn rollback_recovery_is_idempotent_after_an_applied_receipt() {
    let (temp, mut repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, plan) = prepare_file_switch(&repo, path, new_bytes);

    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    std::fs::write(temp.path().join(path), new_bytes).unwrap();
    let observed_new = repo
        .observe_filesystem_effect(repo.working_copy(), &plan.target)
        .unwrap();
    repo.record_effect_outcome(
        &lock,
        prepared.operation().id(),
        0,
        plan.expected_old.clone(),
        observed_new,
    )
    .unwrap();

    let first = repo.recover_incomplete_operation(&lock).unwrap();
    let recovery = match first {
        RecoveryOutcome::Recovered { recovery, .. } => recovery,
        other => panic!("expected recovery, got {other:?}"),
    };
    let receipt_count = {
        let txn = repo.pristine().read_txn().unwrap();
        txn.get_effect_receipts(recovery).unwrap().len()
    };

    let second = repo.recover_incomplete_operation(&lock).unwrap();
    assert_eq!(
        second,
        RecoveryOutcome::AlreadyComplete {
            operation: recovery
        }
    );
    let second_receipt_count = {
        let txn = repo.pristine().read_txn().unwrap();
        txn.get_effect_receipts(recovery).unwrap().len()
    };
    assert_eq!(receipt_count, second_receipt_count);
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), old_bytes);
}

#[test]
fn recovery_rejects_third_value_without_mutating_it() {
    let (temp, mut repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    let third_bytes = b"newer external contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (_prepared, _) = prepare_file_switch(&repo, path, new_bytes);
    std::fs::write(temp.path().join(path), third_bytes).unwrap();

    let lock = repo.try_lock_operation(repo.working_copy()).unwrap();
    let first_error = repo.recover_incomplete_operation(&lock).unwrap_err();
    assert!(matches!(
        &first_error,
        RepositoryError::InvalidOperation { .. }
    ));
    assert!(first_error.to_string().contains("diverged"));
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), third_bytes);

    let (head, first_receipt_count) = {
        let txn = repo.pristine().read_txn().unwrap();
        let heads = txn
            .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
                repo.working_copy(),
            ))
            .unwrap();
        let head = heads.as_slice()[0];
        let operation = txn.get_operation(head).unwrap().unwrap();
        assert_eq!(operation.payload().kind, OperationKind::SwitchView);
        let count = txn.get_effect_receipts(head).unwrap().len();
        (head, count)
    };

    let second_error = repo.recover_incomplete_operation(&lock).unwrap_err();
    assert!(matches!(
        &second_error,
        RepositoryError::InvalidOperation { .. }
    ));
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), third_bytes);
    let second_receipt_count = {
        let txn = repo.pristine().read_txn().unwrap();
        txn.get_effect_receipts(head).unwrap().len()
    };
    assert_eq!(first_receipt_count, second_receipt_count);
}

/// CB-13D review R1: the metadata-only budget fences writable-open
/// recovery — an open under `MetadataOnly` refuses pending recovery work
/// with a typed deferral *before* any effect-bearing plan replays, while
/// the ordinary command open still recovers.
#[test]
fn metadata_only_open_refuses_pending_recovery_instead_of_replaying() {
    let (temp, repo) = create_temp_repo();
    let path = "tracked.txt";
    let old_bytes = b"old contents\n";
    let new_bytes = b"new contents\n";
    std::fs::write(temp.path().join(path), old_bytes).unwrap();
    let (prepared, _) = prepare_file_switch(&repo, path, new_bytes);
    std::fs::write(temp.path().join(path), new_bytes).unwrap();
    let original = prepared.operation().id();
    drop(prepared);
    drop(repo);

    // The metadata-only open refuses: no recovery ran, so the user's bytes
    // are exactly what the interrupted operation left on disk.
    let error =
        Repository::open_with_budget(temp.path(), ReconcileEffectBudget::MetadataOnly).unwrap_err();
    assert!(
        matches!(&error, RepositoryError::ReactiveDeferred { .. }),
        "the metadata-only open must defer pending recovery, got {error:?}"
    );
    assert!(
        error.to_string().contains("explicit command boundary"),
        "the deferral must name the command-boundary remediation: {error}"
    );
    assert_eq!(
        std::fs::read(temp.path().join(path)).unwrap(),
        new_bytes,
        "the metadata-only open must not replay the interrupted effect"
    );

    // The command budget open recovers exactly as before.
    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(std::fs::read(temp.path().join(path)).unwrap(), old_bytes);
    let txn = reopened.pristine().read_txn().unwrap();
    let heads = txn
        .get_operation_heads(atomic_core::operation::OperationScope::WorkingCopy(
            reopened.require_working_copy_id().unwrap(),
        ))
        .unwrap();
    let recovery = txn.get_operation(heads.as_slice()[0]).unwrap().unwrap();
    assert_eq!(recovery.payload().parents, vec![original]);
    assert!(has_operation_verified_receipt(
        &txn.get_effect_receipts(recovery.id()).unwrap()
    ));
}
