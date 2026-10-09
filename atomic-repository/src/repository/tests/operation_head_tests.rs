use atomic_core::operation::{
    ActorRef, EffectReceipt, EffectReceiptKind, EffectReceiptPayload, MetadataTarget,
    MetadataTransition, MetadataValue, Operation, OperationKind, OperationPayload, OperationScope,
    RepoStateDelta, RepoStateRef,
};
use atomic_core::pristine::{OperationMutTxnT, OperationTxnT};
use atomic_core::{Hash, OperationId};

use super::*;

fn run_git(root: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn operation(
    parents: Vec<OperationId>,
    kind: OperationKind,
    metadata: Vec<MetadataTransition>,
    timestamp_ms: i64,
    actor: &str,
) -> Operation {
    Operation::new(OperationPayload {
        parents,
        kind,
        relation: None,
        working_copy: None,
        before: RepoStateRef::EMPTY,
        delta: RepoStateDelta {
            after: RepoStateRef::EMPTY,
            metadata,
            effects: Vec::new(),
        },
        git_observed: Vec::new(),
        evidence: Vec::new(),
        actor: ActorRef::System {
            name: actor.to_string(),
        },
        timestamp_ms,
        lossy: Vec::new(),
    })
    .expect("construct operation")
}

fn verified_receipt(operation: &Operation) -> EffectReceipt {
    EffectReceipt::new(EffectReceiptPayload {
        operation: operation.id(),
        effect_ordinal: None,
        attempt: 0,
        kind: EffectReceiptKind::Verified,
        observed_old: None,
        observed_new: None,
        timestamp_ms: operation.payload().timestamp_ms,
    })
    .expect("construct verified receipt")
}

fn tag_transition(name: &str, byte: u8) -> MetadataTransition {
    MetadataTransition {
        target: MetadataTarget::Tag {
            view: "dev".to_string(),
            name: name.to_string(),
        },
        expected_old: MetadataValue::Absent,
        expected_new: MetadataValue::Digest(Hash::from_bytes([byte; 32])),
    }
}

fn install_repository_heads(
    repository: &Repository,
    first_transition: MetadataTransition,
    second_transition: MetadataTransition,
) -> (Operation, Operation, Operation) {
    let anchor = operation(Vec::new(), OperationKind::Anchor, Vec::new(), 0, "anchor");
    let first = operation(
        vec![anchor.id()],
        OperationKind::Tag,
        vec![first_transition],
        10,
        "first",
    );
    let second = operation(
        vec![anchor.id()],
        OperationKind::Tag,
        vec![second_transition],
        20,
        "second",
    );
    let mut txn = repository
        .pristine
        .write_txn_immediate()
        .expect("open operation transaction");
    txn.put_operation(&anchor).expect("append anchor");
    txn.put_operation(&second).expect("append second");
    txn.put_operation(&first).expect("append first");
    txn.append_effect_receipt(&verified_receipt(&anchor))
        .expect("verify anchor");
    txn.append_effect_receipt(&verified_receipt(&first))
        .expect("verify first");
    txn.append_effect_receipt(&verified_receipt(&second))
        .expect("verify second");
    txn.compare_and_set_operation_heads(
        OperationScope::Repository,
        &[],
        &[second.id(), first.id()],
    )
    .expect("publish repository heads");
    txn.commit().expect("commit repository heads");
    (anchor, first, second)
}

#[test]
fn disjoint_verified_metadata_heads_consolidate_canonically_and_idempotently() {
    let directory = TempDir::new().expect("create temporary repository");
    let repository = Repository::init(directory.path()).expect("initialize repository");
    let (_anchor, first, second) = install_repository_heads(
        &repository,
        tag_transition("release-a", 1),
        tag_transition("release-b", 2),
    );

    drop(repository);
    let repository = Repository::open(directory.path())
        .expect("writable open consolidates disjoint repository heads");
    let state = repository
        .operation_log(OperationScope::Repository, Some(1), false)
        .expect("load consolidated repository head")
        .head_state;
    let consolidation_id = match state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one consolidated head, found {other:?}"),
    };
    let details = repository
        .operation_details(consolidation_id)
        .expect("load consolidation operation");
    assert_eq!(details.operation.payload().kind, OperationKind::Consolidate);
    assert_eq!(details.operation.encoding_version(), 2);
    let mut expected_parents = vec![first.id(), second.id()];
    expected_parents.sort_unstable();
    assert_eq!(details.operation.payload().parents, expected_parents);
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert!(details.operation.payload().delta.metadata.is_empty());
    assert!(details.operation.payload().delta.effects.is_empty());

    assert_eq!(
        repository
            .consolidate_operation_heads(OperationScope::Repository)
            .expect("repeat consolidation"),
        OperationHeadState::Single(consolidation_id),
        "repeated consolidation must be idempotent"
    );
}

#[test]
fn writable_open_consolidates_commuting_working_copy_heads_before_recovery() {
    let directory = TempDir::new().expect("create temporary repository");
    let mut repository = Repository::init(directory.path()).expect("initialize repository");
    let working_copy = repository
        .require_working_copy_id()
        .expect("working-copy identity");
    repository
        .create_view("head-feature")
        .expect("create feature view");
    repository
        .switch_view(working_copy, "head-feature")
        .expect("create operation anchor and switch");
    let scope = OperationScope::WorkingCopy(working_copy);
    let parent_id = repository
        .pristine
        .read_txn()
        .expect("open head transaction")
        .get_operation_heads(scope)
        .expect("load working-copy head")
        .as_slice()[0];
    let parent = repository
        .operation_details(parent_id)
        .expect("load parent operation")
        .operation;
    let make_child = |transition: MetadataTransition, actor: &str| {
        Operation::new(OperationPayload {
            parents: vec![parent_id],
            kind: OperationKind::Tag,
            relation: None,
            working_copy: Some(working_copy),
            before: parent.payload().delta.after.clone(),
            delta: RepoStateDelta {
                after: parent.payload().delta.after.clone(),
                metadata: vec![transition],
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: actor.to_string(),
            },
            timestamp_ms: 30,
            lossy: Vec::new(),
        })
        .expect("construct working-copy child")
    };
    let first = make_child(tag_transition("wc-a", 5), "wc-first");
    let second = make_child(tag_transition("wc-b", 6), "wc-second");
    {
        let mut txn = repository
            .pristine
            .write_txn_immediate()
            .expect("open operation transaction");
        txn.put_operation(&first).expect("append first child");
        txn.put_operation(&second).expect("append second child");
        txn.append_effect_receipt(&verified_receipt(&first))
            .expect("verify first child");
        txn.append_effect_receipt(&verified_receipt(&second))
            .expect("verify second child");
        txn.compare_and_set_operation_heads(scope, &[parent_id], &[first.id(), second.id()])
            .expect("publish concurrent working-copy heads");
        txn.commit().expect("commit working-copy heads");
    }
    drop(repository);

    let reopened = Repository::open(directory.path()).expect("open and consolidate heads");
    let head = match reopened
        .operation_log(scope, Some(1), false)
        .expect("load consolidated log")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one consolidated head, found {other:?}"),
    };
    let details = reopened
        .operation_details(head)
        .expect("load startup consolidation");
    assert_eq!(details.operation.payload().kind, OperationKind::Consolidate);
    let mut expected_parents = vec![first.id(), second.id()];
    expected_parents.sort_unstable();
    assert_eq!(details.operation.payload().parents, expected_parents);
    assert_eq!(details.verification, OperationVerificationState::Verified);
}

#[test]
fn linked_working_copies_share_one_repository_metadata_head() {
    use atomic_core::pristine::TagKind;

    let directory = TempDir::new().expect("create linked-worktree root");
    let primary = directory.path().join("primary");
    let linked = directory.path().join("linked");
    std::fs::create_dir_all(&primary).expect("create primary worktree");
    run_git(&primary, &["init"]);
    run_git(
        &primary,
        &[
            "-c",
            "user.name=Atomic Test",
            "-c",
            "user.email=atomic@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );

    let primary_repository = Repository::init(&primary).expect("initialize primary Atomic repo");
    let primary_working_copy = primary_repository
        .require_working_copy_id()
        .expect("primary working-copy identity");
    primary_repository
        .create_tag("shared-tag", Some("primary value"), TagKind::Release)
        .expect("create primary tag operation");
    let primary_operation = primary_repository
        .pristine
        .read_txn()
        .expect("open primary head transaction")
        .get_operation_heads(OperationScope::WorkingCopy(primary_working_copy))
        .expect("load primary head")
        .as_slice()[0];
    assert_eq!(
        primary_repository
            .pristine
            .read_txn()
            .expect("open repository head transaction")
            .get_operation_heads(OperationScope::Repository)
            .expect("load repository head")
            .as_slice(),
        &[primary_operation]
    );
    drop(primary_repository);

    run_git(
        &primary,
        &[
            "worktree",
            "add",
            "-b",
            "operation-linked",
            linked.to_str().expect("linked path is UTF-8"),
        ],
    );
    let linked_repository = Repository::open(&linked).expect("open linked Atomic worktree");
    let linked_working_copy = linked_repository
        .require_working_copy_id()
        .expect("linked working-copy identity");
    assert_ne!(linked_working_copy, primary_working_copy);
    linked_repository
        .create_tag("shared-tag", Some("linked replacement"), TagKind::Release)
        .expect("create linked tag operation");
    let linked_operation = linked_repository
        .pristine
        .read_txn()
        .expect("open linked head transaction")
        .get_operation_heads(OperationScope::WorkingCopy(linked_working_copy))
        .expect("load linked head")
        .as_slice()[0];
    let linked_details = linked_repository
        .operation_details(linked_operation)
        .expect("load linked operation");
    assert!(linked_details
        .operation
        .payload()
        .parents
        .contains(&primary_operation));
    assert_eq!(
        linked_repository
            .pristine
            .read_txn()
            .expect("open shared repository head transaction")
            .get_operation_heads(OperationScope::Repository)
            .expect("load shared repository head")
            .as_slice(),
        &[linked_operation]
    );
    assert_eq!(
        linked_repository
            .pristine
            .read_txn()
            .expect("open retained primary head transaction")
            .get_operation_heads(OperationScope::WorkingCopy(primary_working_copy))
            .expect("load retained primary head")
            .as_slice(),
        &[primary_operation],
        "the linked operation advances the shared repository chain without rewriting the other working-copy head"
    );
    drop(linked_repository);

    let mut primary_repository =
        Repository::open(&primary).expect("reopen primary Atomic worktree");
    primary_repository
        .create_tag("primary-second-tag", None, TagKind::Release)
        .expect("create second primary tag operation");
    let primary_second = primary_repository
        .pristine
        .read_txn()
        .expect("open second primary head transaction")
        .get_operation_heads(OperationScope::WorkingCopy(primary_working_copy))
        .expect("load second primary head")
        .as_slice()[0];
    let details = primary_repository
        .operation_details(primary_second)
        .expect("load second primary operation");
    assert!(details
        .operation
        .payload()
        .parents
        .contains(&primary_operation));
    assert!(details
        .operation
        .payload()
        .parents
        .contains(&linked_operation));
    assert_eq!(
        primary_repository
            .pristine
            .read_txn()
            .expect("open final repository head transaction")
            .get_operation_heads(OperationScope::Repository)
            .expect("load final repository head")
            .as_slice(),
        &[primary_second]
    );
    primary_repository
        .restore_operation(primary_working_copy, primary_second)
        .expect("fold redundant ancestor and descendant parents with a replaced tag");

    let prepared_push = primary_repository
        .prepare_remote_operation(
            primary_working_copy,
            OperationKind::Push,
            "origin",
            Hash::of(b"prepared linked push"),
        )
        .expect("prepare remote operation while retaining locks");
    assert!(matches!(
        primary_repository.try_lock_common_operation(),
        Err(RepositoryError::LockContended { .. })
    ));
    let prepared_push_id = prepared_push.id();
    drop(prepared_push);
    drop(primary_repository);

    assert!(matches!(
        Repository::open(&linked),
        Err(RepositoryError::OperationNotVerified { operation })
            if operation == prepared_push_id.to_string()
    ));
    let recovered_primary = Repository::open(&primary)
        .expect("owning working copy recovers its incomplete shared operation");
    let recovered_head = recovered_primary
        .pristine
        .read_txn()
        .expect("open recovered repository head transaction")
        .get_operation_heads(OperationScope::Repository)
        .expect("load recovered repository head")
        .as_slice()[0];
    assert_eq!(
        recovered_primary
            .operation_details(recovered_head)
            .expect("load shared recovery operation")
            .operation
            .payload()
            .kind,
        OperationKind::Recover
    );
    drop(recovered_primary);
    Repository::open(&linked).expect("linked worktree opens after shared recovery");
}

#[test]
fn overlapping_metadata_heads_remain_diverged_without_an_invented_after_state() {
    let directory = TempDir::new().expect("create temporary repository");
    let repository = Repository::init(directory.path()).expect("initialize repository");
    let target = MetadataTarget::Tag {
        view: "dev".to_string(),
        name: "release".to_string(),
    };
    let (_anchor, first, second) = install_repository_heads(
        &repository,
        MetadataTransition {
            target: target.clone(),
            expected_old: MetadataValue::Absent,
            expected_new: MetadataValue::Digest(Hash::from_bytes([3; 32])),
        },
        MetadataTransition {
            target,
            expected_old: MetadataValue::Absent,
            expected_new: MetadataValue::Digest(Hash::from_bytes([4; 32])),
        },
    );
    let mut expected_heads = vec![first.id(), second.id()];
    expected_heads.sort_unstable();

    assert_eq!(
        repository
            .consolidate_operation_heads(OperationScope::Repository)
            .expect("classify overlapping heads"),
        OperationHeadState::Diverged(expected_heads.clone())
    );
    let txn = repository
        .pristine
        .read_txn()
        .expect("open read transaction");
    assert_eq!(
        txn.get_operation_heads(OperationScope::Repository)
            .expect("load unchanged heads")
            .as_slice(),
        expected_heads.as_slice()
    );
    assert!(txn
        .list_operations()
        .expect("list operations")
        .iter()
        .all(|operation| operation.payload().kind != OperationKind::Consolidate));
}
