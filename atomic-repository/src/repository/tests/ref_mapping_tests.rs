//! CB-10A ref-mapping persistence, journaling, and lifecycle.

use super::*;
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind,
};
use atomic_core::pristine::{RefSyncStatus, REF_MAPPING_VERSION};
use atomic_core::Hash;
use crate::repository::ref_mapping::{
    classify_three_way, Containment, ThreeWayAction, ThreeWayObservation,
};

fn mapping(view_id: u64) -> atomic_core::pristine::RefMapping {
    atomic_core::pristine::RefMapping {
        version: REF_MAPPING_VERSION,
        view_id,
        view_name: "dev".to_string(),
        scope: atomic_core::pristine::ViewScope::Shared as u8,
        local_ref: Some("refs/heads/dev".to_string()),
        remote: Some(("origin".to_string(), "refs/heads/dev".to_string())),
        last_observed_local: Some("1111111111111111111111111111111111111111".to_string()),
        last_observed_remote: None,
        last_exported: Some("1111111111111111111111111111111111111111".to_string()),
        last_exported_state: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()),
        last_observed_atomic: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()),
        status: RefSyncStatus::Synchronized,
    }
}

fn view_id_of(repository: &Repository, name: &str) -> u64 {
    let txn = repository.pristine().read_txn().expect("read txn");
    txn.get_view(name)
        .expect("view lookup")
        .expect("view row")
        .id
}

#[test]
fn ref_mapping_persists_across_reopen_with_all_fields() {
    let (directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    let view_id = view_id_of(repository, "dev");
    let mut mapping = mapping(view_id);
    repository
        .set_ref_mapping(working_copy, "dev", Some(mapping.clone()))
        .expect("write mapping");
    drop(test_repo);

    // A fresh handle (fresh process semantics): every field — including the
    // remote pair and the observed/exported tips — survives the reopen, and
    // the journaled write is a verified RefMapping operation.
    let reopened = Repository::open(directory.path()).expect("reopen repository");
    let stored = reopened
        .get_ref_mapping("dev")
        .expect("read mapping")
        .expect("mapping row");
    assert_eq!(stored, mapping);

    let scope = atomic_core::operation::OperationScope::WorkingCopy(working_copy);
    let head = match reopened
        .operation_log(scope, Some(1), false)
        .expect("operation log")
        .head_state
    {
        crate::OperationHeadState::Single(head) => head,
        other => panic!("expected one head, found {other:?}"),
    };
    let details = reopened.operation_details(head).expect("operation details");
    assert_eq!(
        details.operation.payload().kind,
        OperationKind::RefMapping
    );
    assert_eq!(
        details.verification,
        super::operation::OperationVerificationState::Verified
    );
}

#[test]
fn identical_mapping_write_is_an_idempotent_noop() {
    let (_directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    let view_id = view_id_of(repository, "dev");
    let mapping = mapping(view_id);
    let first = repository
        .set_ref_mapping(working_copy, "dev", Some(mapping.clone()))
        .expect("first write");
    assert!(first.is_some(), "the initial transition journals an op");
    let second = repository
        .set_ref_mapping(working_copy, "dev", Some(mapping))
        .expect("second write");
    assert!(second.is_none(), "an identical write must not journal an op");
}

#[test]
fn mapping_write_refuses_a_stale_lease_without_overwriting() {
    // The metadata lease compares complete canonical bytes: a prepared
    // transition whose expected_old no longer matches the stored row is
    // refused at prepare time and nothing is written.
    let (_directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    let view_id = view_id_of(repository, "dev");
    let mut mapping = mapping(view_id);
    repository
        .set_ref_mapping(working_copy, "dev", Some(mapping.clone()))
        .expect("baseline write");

    // An out-of-band writer moves the row (bypassing the journal).
    {
        use atomic_core::pristine::{MutTxnT, RefMappingMutTxnT, ViewTxnT};
        let mut txn = repository.pristine().write_txn().expect("write txn");
        let view = txn.get_view("dev").expect("view").expect("dev");
        let mut moved = mapping.clone();
        moved.last_observed_local = Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string());
        txn.put_ref_mapping_bytes(view.id, &moved.encode().expect("encode"))
            .expect("concurrent put");
        txn.commit().expect("commit");
    }

    // A stale lease (prepared against the pre-move row) must be refused.
    let stale = mapping;
    let lock = repository
        .try_lock_operation(working_copy)
        .expect("operation lock");
    let state = repository
        .current_working_copy_state(working_copy)
        .expect("state");
    let error = repository
        .prepare_metadata_operation(
            &lock,
            OperationKind::RefMapping,
            None,
            state.clone(),
            state,
            vec![MetadataTransition {
                target: MetadataTarget::RefMapping {
                    view: "dev".to_string(),
                },
                expected_old: MetadataValue::Bytes(mapping_encoding(&stale)),
                expected_new: MetadataValue::Bytes(Vec::new()),
            }],
            vec![Hash::of(b"stale lease evidence")],
            ActorRef::System {
                name: "ref-mapping-test".to_string(),
            },
            0,
        )
        .expect_err("the stale lease must be refused");
    assert!(
        error.to_string().contains("diverged"),
        "the stale lease must be reported as divergence: {error}"
    );
    drop(lock);

    // The concurrent move was never silently reverted.
    let stored = repository
        .get_ref_mapping("dev")
        .expect("read")
        .expect("row");
    assert_eq!(stored.last_observed_local.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
}

fn mapping_encoding(mapping: &atomic_core::pristine::RefMapping) -> Vec<u8> {
    mapping.encode().expect("encode mapping")
}

#[test]
fn deleted_view_lifecycle_reconciles_the_mapping_and_keeps_the_branch() {
    let (directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    let dev_id = view_id_of(repository, "dev");

    // A draft view with its own mapping row.
    {
        use atomic_core::pristine::{MutTxnT, ViewScope};
        let mut txn = repository.pristine().write_txn().expect("write txn");
        let dev = txn.get_view("dev").expect("view").expect("dev");
        txn.create_view("to-delete", ViewScope::Draft, Some(dev.id))
            .expect("create draft");
        txn.commit().expect("commit");
    }
    let draft_id = view_id_of(repository, "to-delete");
    let mut draft_mapping = mapping(draft_id);
    draft_mapping.view_name = "to-delete".to_string();
    draft_mapping.scope = atomic_core::pristine::ViewScope::Draft as u8;
    draft_mapping.local_ref = Some("refs/atomic/views/to-delete".to_string());
    repository
        .set_ref_mapping(working_copy, "to-delete", Some(draft_mapping))
        .expect("write draft mapping");
    assert!(repository
        .get_ref_mapping("to-delete")
        .expect("read")
        .is_some());

    // Deleting the draft view removes its mapping row (view GC keeps the
    // mapping from outliving a deletable view).
    {
        use atomic_core::pristine::MutTxnT;
        let mut txn = repository.pristine().write_txn().expect("write txn");
        let view = txn.get_view("to-delete").expect("view").expect("to-delete");
        txn.del_view(&view).expect("delete draft view");
        txn.commit().expect("commit");
    }
    repository
        .reconcile_mapping_after_view_delete(
            working_copy,
            "to-delete",
            atomic_core::pristine::ViewScope::Draft,
        )
        .expect("reconcile mapping after draft delete");
    assert!(
        repository
            .get_ref_mapping("to-delete")
            .expect("read")
            .is_none(),
        "a deleted draft's mapping row must be removed"
    );

    // The Shared-unrepresentable path (a Shared view cannot be deleted —
    // enforced upstream — so recovery/undo flows that remove one reconcile
    // the mapping to Unrepresentable and never touch its branch). CB-10A
    // review R5: the tombstone keeps the persisted ref name — it is the only
    // durable association a diagnostic can show.
    let mut shared = mapping(dev_id);
    repository
        .set_ref_mapping(working_copy, "dev", Some(shared.clone()))
        .expect("write shared mapping");
    shared.status = RefSyncStatus::Unrepresentable;
    repository
        .reconcile_mapping_after_view_delete(
            working_copy,
            "dev",
            atomic_core::pristine::ViewScope::Shared,
        )
        .expect("reconcile mapping after shared delete");
    let stored = repository
        .get_ref_mapping("dev")
        .expect("read")
        .expect("stale row survives");
    assert_eq!(stored.status, RefSyncStatus::Unrepresentable);
    assert_eq!(
        stored.local_ref.as_deref(),
        Some("refs/heads/dev"),
        "the tombstone must preserve the persisted ref name"
    );

    // Ref-mapping reconciliation never deletes Git refs: the mapping code
    // contains no deletion path at all (structural guarantee, enforced by
    // the CLI e2e suites).
    drop(directory);
}

#[test]
fn refused_shared_delete_leaves_every_mapping_byte_untouched() {
    // CB-10A review R5: a refused delete (Shared views are permanent) must
    // not mutate the mapping as a side effect: the eligibility gate runs
    // before any effect, so the refused attempt leaves the row, the
    // journaled-operation count and the view state exactly as they were.
    let (directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    {
        use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
        let mut txn = repository.pristine().write_txn().expect("write txn");
        let dev = txn.get_view("dev").expect("view").expect("dev");
        txn.create_view("release", ViewScope::Shared, Some(dev.id))
            .expect("create shared view");
        txn.commit().expect("commit");
    }
    let view_id = view_id_of(repository, "release");
    let mut shared_mapping = mapping(view_id);
    shared_mapping.view_name = "release".to_string();
    shared_mapping.local_ref = Some("refs/heads/release".to_string());
    repository
        .set_ref_mapping(working_copy, "release", Some(shared_mapping.clone()))
        .expect("write shared mapping");

    let before = repository
        .get_ref_mapping("release")
        .expect("read")
        .expect("row");
    let scope = atomic_core::operation::OperationScope::WorkingCopy(working_copy);
    let operations_before = repository
        .operation_log(scope, None, false)
        .expect("operation log")
        .entries
        .len();

    // Shared views refuse deletion.
    let error = repository
        .delete_view("release")
        .expect_err("shared views are permanent");
    assert!(
        error.to_string().contains("cannot delete shared view"),
        "the typed refusal must survive: {error}"
    );

    let after = repository
        .get_ref_mapping("release")
        .expect("read")
        .expect("row");
    assert_eq!(before, after, "a refused delete must not mutate the mapping");
    assert!(
        repository.view_exists("release").expect("view exists"),
        "the refused view must still exist"
    );
    let operations_after = repository
        .operation_log(scope, None, false)
        .expect("operation log")
        .entries
        .len();
    assert_eq!(
        operations_before, operations_after,
        "a refused delete must not journal any operation"
    );
    drop(directory);
}

#[test]
fn a_stale_observation_cannot_clobber_a_newer_mapping_row() {
    // CB-10A review R6: a replacement built from an observed row is pinned
    // to it. A second writer that moves the row in between makes the pinned
    // write fail closed with a typed error — the newer row is never
    // clobbered under an invented lease.
    let (directory, mut test_repo) = create_temp_repo();
    let working_copy = test_repo.working_copy();
    let repository = test_repo.deref_mut();
    let view_id = view_id_of(repository, "dev");
    let observed = mapping(view_id);
    repository
        .set_ref_mapping(working_copy, "dev", Some(observed.clone()))
        .expect("baseline write");

    // An interleaved writer moves the row after the first caller observed it.
    let mut moved = observed.clone();
    moved.last_observed_local = Some("2222222222222222222222222222222222222222".to_string());
    {
        use atomic_core::pristine::{MutTxnT, RefMappingMutTxnT, ViewTxnT};
        let mut txn = repository.pristine().write_txn().expect("write txn");
        let view = txn.get_view("dev").expect("view").expect("dev");
        txn.put_ref_mapping_bytes(view.id, &moved.encode().expect("encode"))
            .expect("concurrent put");
        txn.commit().expect("commit");
    }

    // The first caller now writes the replacement it built from the old row.
    let mut replacement = observed.clone();
    replacement.last_observed_local = Some("3333333333333333333333333333333333333333".to_string());
    let error = repository
        .set_ref_mapping_from_observation(working_copy, "dev", Some(&observed), Some(replacement))
        .expect_err("the stale-built replacement must be refused");
    assert!(
        error.to_string().contains("moved since the caller observed"),
        "the typed observation-moved refusal must surface: {error}"
    );
    let stored = repository
        .get_ref_mapping("dev")
        .expect("read")
        .expect("row");
    assert_eq!(
        stored,
        moved,
        "the interleaved newer row must survive untouched"
    );
    drop(directory);
}

#[test]
fn incomplete_walks_and_stale_exports_never_prove_containment() {
    use crate::repository::ref_mapping::{commits_added_since, git_contains_atomic_export, AddedCommits};
    use git2::Repository as GitRepository;

    // Real Git repository with a two-commit history.
    let directory = tempfile::TempDir::new().expect("tempdir");
    let git = GitRepository::init(directory.path()).expect("init git repo");
    let signature = git2::Signature::now("CB-10A Tests", "cb10a@example.com").expect("signature");
    let empty_tree = {
        let mut builder = git.treebuilder(None).expect("treebuilder");
        let tree_oid = builder.write().expect("empty tree");
        tree_oid
    };
    let tree = git.find_tree(empty_tree).expect("empty tree");
    let base = git
        .commit(Some("refs/heads/main"), &signature, &signature, "base", &tree, &[])
        .expect("base commit");
    let child = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "child",
            &tree,
            &[&git.find_commit(base).expect("base")],
        )
        .expect("child commit");
    drop(tree);
    drop(git);
    let git = GitRepository::open(directory.path()).expect("open git repo");

    // A complete walk between two real commits yields the exact set.
    assert_eq!(
        commits_added_since(&git, Some(&base.to_string()), Some(&child.to_string())),
        AddedCommits::Added(vec![child.to_string()]),
    );
    // An unreachable (garbage) baseline is unprovable — never an empty set
    // read as positive containment (CB-10A review R2).
    let zero = git2::Oid::zero().to_string();
    assert!(matches!(
        commits_added_since(&git, Some(&zero), Some(&child.to_string())),
        AddedCommits::Unprovable(_)
    ));
    // A missing tip (deleted/unborn ref) with an observed baseline is
    // unprovable, not "no movement".
    assert!(matches!(
        commits_added_since(&git, Some(&base.to_string()), None),
        AddedCommits::Unprovable(_)
    ));
    // Neither side observable: genuinely nothing moved.
    assert_eq!(commits_added_since(&git, None, None), AddedCommits::NoMovement);

    // A stale export (bound to an older state) cannot prove containment.
    assert_eq!(
        git_contains_atomic_export(
            &[child.to_string()],
            Some(&base.to_string()),
            Some("older-state"),
            "current-state"
        ),
        None,
        "an obsolete export must be rejected"
    );
    // An unbound export proves nothing.
    assert_eq!(
        git_contains_atomic_export(
            &[child.to_string()],
            Some(&base.to_string()),
            None,
            "current-state"
        ),
        None
    );
    // The positive arm: an export bound to exactly the current state proves
    // containment when the exported commit is in the added set.
    assert_eq!(
        git_contains_atomic_export(
            &[base.to_string(), child.to_string()],
            Some(&base.to_string()),
            Some("current-state"),
            "current-state"
        ),
        Some(true)
    );
}

#[test]
fn three_way_classification_matrix_over_persisted_mapping() {
    let (_directory, mut test_repo) = create_temp_repo();
    let repository = test_repo.deref_mut();
    let view_id = view_id_of(repository, "dev");
    let mut mapping = mapping(view_id);
    mapping.local_ref = Some("refs/heads/dev".to_string());
    let observe = |git: Option<&str>, atomic: &str| ThreeWayObservation {
        current_git: git.map(str::to_string),
        current_atomic: atomic.to_string(),
    };
    let proven = |atomic: bool, git: bool| Containment {
        atomic_contains_git: Some(atomic),
        git_contains_atomic: Some(git),
    };

    assert_eq!(
        classify_three_way(&mapping, &observe(Some("1111111111111111111111111111111111111111"), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), proven(false, false))
            .action,
        ThreeWayAction::Noop
    );
    assert_eq!(
        classify_three_way(&mapping, &observe(Some("2222222222222222222222222222222222222222"), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), proven(false, false))
            .action,
        ThreeWayAction::Import
    );
    assert_eq!(
        classify_three_way(&mapping, &observe(Some("1111111111111111111111111111111111111111"), "moved-state"), proven(false, false))
            .action,
        ThreeWayAction::Export
    );
    assert_eq!(
        classify_three_way(&mapping, &observe(Some("2222222222222222222222222222222222222222"), "moved-state"), proven(true, false))
            .action,
        ThreeWayAction::Export
    );
    assert_eq!(
        classify_three_way(&mapping, &observe(Some("2222222222222222222222222222222222222222"), "moved-state"), proven(false, true))
            .action,
        ThreeWayAction::Import
    );
    assert_eq!(
        classify_three_way(&mapping, &observe(Some("2222222222222222222222222222222222222222"), "moved-state"), proven(false, false))
            .action,
        ThreeWayAction::Diverged
    );
    // Unprovable containment (an added commit without verified closure)
    // fails closed: neither side may move.
    assert_eq!(
        classify_three_way(
            &mapping,
            &observe(Some("2222"), "moved-state"),
            Containment::default()
        )
        .action,
        ThreeWayAction::Diverged
    );

    // Ephemeral/deleted-view mappings carry no ref: never reconciled.
    let mut no_ref = mapping;
    no_ref.local_ref = None;
    assert_eq!(
        classify_three_way(&no_ref, &observe(Some("2222222222222222222222222222222222222222"), "moved-state"), proven(true, true))
            .action,
        ThreeWayAction::Unrepresentable
    );
}