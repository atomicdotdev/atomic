use super::*;
use crate::Repository;
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectReceipt, EffectReceiptPayload, EffectTarget, EffectValue,
    MetadataTarget, MetadataTransition, MetadataValue, OperationPayload, OperationScope,
    RepoStateDelta, RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{MutTxnT, OperationMutTxnT};
use atomic_core::Hash;

fn anchor(owner: WorkingCopyId) -> Operation {
    Operation::new(OperationPayload {
        parents: vec![],
        kind: OperationKind::Anchor,
        relation: None,
        working_copy: Some(owner),
        before: RepoStateRef::EMPTY,
        delta: RepoStateDelta {
            after: RepoStateRef::EMPTY,
            metadata: vec![],
            effects: vec![],
        },
        git_observed: vec![],
        evidence: vec![],
        actor: ActorRef::System {
            name: "cleanup-test".into(),
        },
        timestamp_ms: 0,
        lossy: vec![],
    })
    .unwrap()
}

fn operation(owner: WorkingCopyId, timestamp: i64) -> Operation {
    let mut payload = anchor(owner).payload().clone();
    payload.parents = vec![anchor(owner).id()];
    payload.kind = OperationKind::SwitchView;
    payload.timestamp_ms = timestamp;
    Operation::new(payload).unwrap()
}

fn persist(repo: &Repository, operation: &Operation, verified: bool, head: Option<OperationScope>) {
    let mut txn = repo.pristine.write_txn_immediate().unwrap();
    let initial = anchor(operation.payload().working_copy.unwrap());
    if txn.get_operation(initial.id()).unwrap().is_none() {
        txn.put_operation(&initial).unwrap();
        txn.append_effect_receipt(
            &EffectReceipt::new(EffectReceiptPayload {
                operation: initial.id(),
                effect_ordinal: None,
                attempt: 0,
                kind: EffectReceiptKind::Verified,
                observed_old: None,
                observed_new: None,
                timestamp_ms: 0,
            })
            .unwrap(),
        )
        .unwrap();
    }
    txn.put_operation(operation).unwrap();
    if verified {
        for effect in &operation.payload().delta.effects {
            txn.append_effect_receipt(
                &EffectReceipt::new(EffectReceiptPayload {
                    operation: operation.id(),
                    effect_ordinal: Some(effect.ordinal),
                    attempt: 0,
                    kind: EffectReceiptKind::Applied,
                    observed_old: Some(effect.expected_old.clone()),
                    observed_new: Some(effect.expected_new.clone()),
                    timestamp_ms: operation.payload().timestamp_ms,
                })
                .unwrap(),
            )
            .unwrap();
        }
        txn.append_effect_receipt(
            &EffectReceipt::new(EffectReceiptPayload {
                operation: operation.id(),
                effect_ordinal: None,
                attempt: 0,
                kind: EffectReceiptKind::Verified,
                observed_old: None,
                observed_new: None,
                timestamp_ms: operation.payload().timestamp_ms,
            })
            .unwrap(),
        )
        .unwrap();
    }
    if let Some(scope) = head {
        let before = txn.get_operation_heads(scope).unwrap();
        txn.compare_and_set_operation_heads(scope, before.as_slice(), &[operation.id()])
            .unwrap();
    }
    txn.commit().unwrap();
}

fn backup(repo: &Repository, owner: WorkingCopyId, operation: OperationId) -> PathBuf {
    let path = repo
        .dot_dir
        .join("working-copies")
        .join(owner.to_string())
        .join("operation-recovery")
        .join(operation.to_string());
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("retained-bytes"), b"recovery bytes").unwrap();
    path
}

fn plan<'a>(
    repo: &Repository,
    guard: &'a RepositoryMaintenanceLockGuard,
    full: bool,
) -> anyhow::Result<CleanupPlan<'a>> {
    CleanupPlan::build(&repo.pristine.read_txn()?, guard, &repo.dot_dir, full)
}

#[test]
fn preview_is_read_only_apply_is_repeatable_and_all_heads_are_retained() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let old = operation(owner, 1);
    let wc_head = operation(owner, 2);
    let shared_head = operation(owner, 3);
    persist(&repo, &old, true, None);
    persist(
        &repo,
        &wc_head,
        true,
        Some(OperationScope::WorkingCopy(owner)),
    );
    persist(&repo, &shared_head, true, Some(OperationScope::Repository));
    let old_path = backup(&repo, owner, old.id());
    let wc_path = backup(&repo, owner, wc_head.id());
    let shared_path = backup(&repo, owner, shared_head.id());
    let unknown_path = backup(&repo, owner, OperationId::from_bytes([99; 32]));
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let preview = plan(&repo, &guard, false).unwrap();
    assert_eq!(preview.items.iter().filter(|item| item.eligible).count(), 1);
    assert!(old_path.exists());
    let results = preview.execute(&guard).unwrap();
    assert_eq!(results.iter().filter(|item| item.removed).count(), 1);
    assert!(!old_path.exists());
    assert!(wc_path.exists() && shared_path.exists() && unknown_path.exists());
    assert!(!plan(&repo, &guard, true)
        .unwrap()
        .items
        .iter()
        .any(|item| item.eligible));
    // Journal history/receipts themselves are unchanged by disposal.
    assert_eq!(
        repo.pristine
            .read_txn()
            .unwrap()
            .list_operations()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn verified_inverse_recovery_releases_failed_operation_backup() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let original = operation(owner, 1);
    persist(&repo, &original, false, None);
    let mut payload = operation(owner, 2).payload().clone();
    payload.parents = vec![original.id()];
    payload.kind = OperationKind::Recover;
    payload.before = original.payload().delta.after.clone();
    payload.delta.after = original.payload().before.clone();
    let recovery = Operation::new(payload).unwrap();
    persist(
        &repo,
        &recovery,
        true,
        Some(OperationScope::WorkingCopy(owner)),
    );
    let path = backup(&repo, owner, original.id());
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    plan(&repo, &guard, false).unwrap().execute(&guard).unwrap();
    assert!(!path.exists());
}

#[test]
fn effect_bearing_metadata_keeps_original_bytes_for_undo_even_off_head() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let mut payload = operation(owner, 1).payload().clone();
    payload.delta.metadata.push(MetadataTransition {
        target: MetadataTarget::Tag {
            view: "dev".into(),
            name: "fixture".into(),
        },
        expected_old: MetadataValue::Absent,
        expected_new: MetadataValue::Bytes(vec![1]),
    });
    payload.delta.effects.push(EffectPlan {
        ordinal: 0,
        target: EffectTarget::FilesystemPath {
            path: "hook".into(),
        },
        expected_old: EffectValue::Absent,
        expected_new: EffectValue::Verification(Hash::of(b"fixture")),
    });
    let protected = Operation::new(payload).unwrap();
    persist(&repo, &protected, true, None);
    let path = backup(&repo, owner, protected.id());
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let result = plan(&repo, &guard, true).unwrap().execute(&guard).unwrap();
    assert!(path.exists());
    assert!(result
        .iter()
        .any(|item| item.path == path && !item.eligible && item.reason.contains("undo")));
}

#[test]
fn incomplete_and_divergent_heads_fail_before_any_removal() {
    for divergent in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let owner = repo.require_working_copy_id().unwrap();
        let old = operation(owner, 1);
        let head = operation(owner, 2);
        persist(&repo, &old, true, None);
        persist(
            &repo,
            &head,
            divergent,
            Some(OperationScope::WorkingCopy(owner)),
        );
        if divergent {
            let mut txn = repo.pristine.write_txn_immediate().unwrap();
            txn.compare_and_set_operation_heads(
                OperationScope::WorkingCopy(owner),
                &[head.id()],
                &[old.id(), head.id()],
            )
            .unwrap();
            txn.commit().unwrap();
        }
        let path = backup(&repo, owner, old.id());
        let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
        let error = plan(&repo, &guard, true)
            .err()
            .expect("unsafe journal must fail")
            .to_string();
        assert!(
            error.contains(if divergent { "divergent" } else { "incomplete" }),
            "{error}"
        );
        assert!(path.exists());
    }
}

#[test]
fn corrupted_journal_fails_closed() {
    use atomic_core::pristine::tables::OPERATIONS;
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let database = repo.pristine.shared_database().unwrap();
    let write = database.begin_write().unwrap();
    write
        .open_table(OPERATIONS)
        .unwrap()
        .insert(&[1u8; 32], b"invalid".as_slice())
        .unwrap();
    write.commit().unwrap();
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    assert!(plan(&repo, &guard, true).is_err());
}

#[test]
fn full_expires_only_known_deleted_shelves_not_live_nested_or_unknown_views() {
    let temp = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    repo.create_view("group/live").unwrap();
    let mut payload = operation(owner, 1).payload().clone();
    payload.before.view = Some(ViewStateRef {
        name: "deleted".into(),
        state: Hash::of(b"old"),
        set_id: None,
    });
    persist(&repo, &Operation::new(payload).unwrap(), true, None);
    let shelves = repo
        .dot_dir
        .join("working-copies")
        .join(owner.to_string())
        .join("workspaces");
    for name in ["deleted", "dev", "group", "unknown"] {
        fs::create_dir_all(shelves.join(name)).unwrap();
        fs::write(shelves.join(name).join("ignored-work"), name).unwrap();
    }
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    assert!(!plan(&repo, &guard, false)
        .unwrap()
        .items
        .iter()
        .any(|item| item.eligible));
    let results = plan(&repo, &guard, true).unwrap().execute(&guard).unwrap();
    assert_eq!(results.iter().filter(|item| item.removed).count(), 1);
    assert!(!shelves.join("deleted").exists());
    for name in ["dev", "group", "unknown"] {
        assert!(shelves.join(name).exists());
    }
}

#[test]
fn worktree_local_staging_is_disposed_only_for_its_registered_owner() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let op = operation(owner, 1);
    persist(&repo, &op, true, None);
    let staging = repo
        .dot_dir
        .join("operation-recovery")
        .join(op.id().to_string());
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("index.backup"), "original index").unwrap();
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    plan(&repo, &guard, false).unwrap().execute(&guard).unwrap();
    assert!(!staging.exists());
}

#[test]
fn changed_candidate_aborts_the_whole_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let a = operation(owner, 1);
    let b = operation(owner, 2);
    persist(&repo, &a, true, None);
    persist(&repo, &b, true, None);
    let a_path = backup(&repo, owner, a.id());
    let b_path = backup(&repo, owner, b.id());
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let pending = plan(&repo, &guard, false).unwrap();
    fs::write(b_path.join("unexpected-new-file"), "external edit").unwrap();
    assert!(pending.execute(&guard).is_err());
    assert!(a_path.exists() && b_path.exists());
}

#[cfg(unix)]
#[test]
fn symlink_roots_are_kept_and_nested_symlinks_never_delete_targets() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("precious"), "keep").unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let a = operation(owner, 1);
    let b = operation(owner, 2);
    persist(&repo, &a, true, None);
    persist(&repo, &b, true, None);
    let a_path = backup(&repo, owner, a.id());
    let b_path = a_path.parent().unwrap().join(b.id().to_string());
    symlink(outside.path(), &b_path).unwrap();
    symlink(outside.path(), a_path.join("external-link")).unwrap();
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let results = plan(&repo, &guard, true).unwrap().execute(&guard).unwrap();
    assert!(!a_path.exists());
    assert!(b_path.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(outside.path().join("precious")).unwrap(),
        "keep"
    );
    assert!(results
        .iter()
        .any(|item| item.path == b_path && !item.eligible));
}

#[test]
fn maintenance_exclusion_contends_with_common_operation_writers() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let operation = repo.try_lock_common_operation().unwrap();
    assert!(Repository::try_lock_maintenance(&repo.dot_dir)
        .err()
        .unwrap()
        .is_lock_contended());
    drop(operation);
    let maintenance = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    assert!(repo
        .try_lock_common_operation()
        .err()
        .unwrap()
        .is_lock_contended());
    drop(maintenance);
    assert!(repo.try_lock_common_operation().is_ok());
}

#[cfg(unix)]
#[test]
fn deletion_does_not_follow_a_parent_symlink_installed_after_preflight() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().join("metadata");
    let candidate = root.join("parent/operation");
    fs::create_dir_all(&candidate).unwrap();
    fs::write(candidate.join("disposable"), "old bytes").unwrap();
    let expected = fs::symlink_metadata(&candidate).unwrap();
    fs::create_dir_all(outside.path().join("operation")).unwrap();
    fs::write(outside.path().join("operation/precious"), "must survive").unwrap();
    fs::rename(root.join("parent"), root.join("original-parent")).unwrap();
    symlink(outside.path(), root.join("parent")).unwrap();
    assert!(super::secure_remove::remove(&root, &candidate, &expected).is_err());
    assert_eq!(
        fs::read_to_string(outside.path().join("operation/precious")).unwrap(),
        "must survive"
    );
    assert!(root.join("original-parent/operation/disposable").exists());
}

#[test]
fn copied_worktree_identity_does_not_authorize_cleaning_an_unregistered_location() {
    let temp = tempfile::tempdir().unwrap();
    let copied = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let op = operation(owner, 1);
    persist(&repo, &op, true, None);
    let local = copied.path().join(".atomic");
    let staging = local.join("operation-recovery").join(op.id().to_string());
    fs::create_dir_all(&staging).unwrap();
    fs::write(local.join("working_copy_id"), owner.to_string()).unwrap();
    fs::write(staging.join("unowned"), "keep this copied work").unwrap();
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let result = CleanupPlan::build(&repo.pristine.read_txn().unwrap(), &guard, &local, true)
        .unwrap()
        .execute(&guard)
        .unwrap();
    assert!(staging.join("unowned").exists());
    assert!(result
        .iter()
        .any(|item| item.reason.contains("unknown physical working-copy")));
}

#[test]
fn full_cleanup_preserves_shelves_referenced_by_a_retained_head() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let owner = repo.require_working_copy_id().unwrap();
    let mut payload = operation(owner, 1).payload().clone();
    payload.before.view = Some(ViewStateRef {
        name: "deleted".into(),
        state: Hash::of(b"old"),
        set_id: None,
    });
    payload.delta.effects.push(EffectPlan {
        ordinal: 0,
        target: EffectTarget::ShelfPath {
            working_copy: owner,
            view: "deleted".into(),
            path: "ignored".into(),
        },
        expected_old: EffectValue::Absent,
        expected_new: EffectValue::Verification(Hash::of(b"fixture")),
    });
    let op = Operation::new(payload).unwrap();
    persist(&repo, &op, true, Some(OperationScope::WorkingCopy(owner)));
    let shelf = repo
        .dot_dir
        .join("working-copies")
        .join(owner.to_string())
        .join("workspaces/deleted");
    fs::create_dir_all(&shelf).unwrap();
    fs::write(shelf.join("ignored"), "undo needs this").unwrap();
    let guard = Repository::try_lock_maintenance(&repo.dot_dir).unwrap();
    let result = plan(&repo, &guard, true).unwrap().execute(&guard).unwrap();
    assert!(shelf.exists());
    assert!(result
        .iter()
        .any(|item| item.path == shelf && !item.eligible && item.reason.contains("retained head")));
}

#[test]
fn linked_worktree_local_staging_routes_to_registered_owner_and_common_database() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    let sandbox = temp.path().join("sandbox");
    let repo = Repository::init(&root).unwrap();
    let primary = repo.require_working_copy_id().unwrap();
    repo.provision_sandbox(
        repo.require_working_copy_id().unwrap(),
        &sandbox,
        repo.current_view(),
    )
    .unwrap();
    drop(repo);
    let linked = Repository::open_existing(&sandbox).unwrap();
    let owner = linked.require_working_copy_id().unwrap();
    assert_ne!(owner, primary);
    let op = operation(owner, 1);
    persist(&linked, &op, true, None);
    let local = sandbox.join(".atomic");
    let staging = local.join("operation-recovery").join(op.id().to_string());
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("old-index"), "disposable staging").unwrap();
    let guard = Repository::try_lock_maintenance(&linked.dot_dir).unwrap();
    CleanupPlan::build(&linked.pristine.read_txn().unwrap(), &guard, &local, false)
        .unwrap()
        .execute(&guard)
        .unwrap();
    assert!(!staging.exists());
    assert!(!local.join("atomic.redb").exists());
    assert!(root.join(".atomic/atomic.redb").exists());
}
