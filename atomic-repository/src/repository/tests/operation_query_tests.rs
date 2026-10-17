use atomic_core::operation::{
    ActorRef, EffectReceipt, EffectReceiptKind, EffectReceiptPayload, Operation, OperationKind,
    OperationPayload, OperationScope, RepoStateDelta,
};
use atomic_core::pristine::{OperationMutTxnT, OperationTxnT};

use super::*;

fn switched_repository() -> (TempDir, Repository, WorkingCopyId) {
    let directory = TempDir::new().expect("create temporary repository");
    let mut repository = Repository::init(directory.path()).expect("initialize repository");
    let working_copy = repository
        .require_working_copy_id()
        .expect("working-copy identity");
    repository
        .create_view("operation-query-feature")
        .expect("create feature view");
    repository
        .switch_view(working_copy, "operation-query-feature")
        .expect("switch view");
    (directory, repository, working_copy)
}

fn child_operation(parent: &Operation, timestamp_ms: i64, actor: &str) -> Operation {
    let state = parent.payload().delta.after.clone();
    Operation::new(OperationPayload {
        parents: vec![parent.id()],
        kind: OperationKind::Materialize,
        relation: None,
        working_copy: parent.payload().working_copy,
        before: state.clone(),
        delta: RepoStateDelta {
            after: state,
            metadata: Vec::new(),
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
    .expect("construct child operation")
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

#[test]
fn operation_log_orders_a_diverged_dag_and_resolves_case_insensitive_prefixes() {
    let (_directory, repository, working_copy) = switched_repository();
    let scope = OperationScope::WorkingCopy(working_copy);
    let initial = repository
        .operation_log(scope, None, false)
        .expect("load initial operation log");
    let parent_id = match initial.head_state {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let parent = repository
        .operation_details(parent_id)
        .expect("load parent operation")
        .operation;
    let first = child_operation(&parent, 100, "query-first");
    let second = child_operation(&parent, 100, "query-second");

    {
        let mut txn = repository
            .pristine
            .write_txn_immediate()
            .expect("open immediate operation transaction");
        txn.put_operation(&second).expect("append second child");
        txn.put_operation(&first).expect("append first child");
        txn.append_effect_receipt(&verified_receipt(&first))
            .expect("verify first child");
        txn.append_effect_receipt(&verified_receipt(&second))
            .expect("verify second child");
        txn.compare_and_set_operation_heads(scope, &[parent_id], &[second.id(), first.id()])
            .expect("publish concurrent heads");
        txn.commit().expect("commit concurrent heads");
    }

    let log = repository
        .operation_log(scope, None, false)
        .expect("load diverged operation log");
    assert_eq!(
        log.head_state,
        OperationHeadState::Diverged({
            let mut heads = vec![first.id(), second.id()];
            heads.sort_unstable();
            heads
        })
    );
    assert_eq!(log.entries.len(), 4, "two children, switch, and anchor");

    let mut expected_children = vec![first.id(), second.id()];
    expected_children.sort_unstable_by(|left, right| right.cmp(left));
    assert_eq!(
        log.entries[..2]
            .iter()
            .map(|entry| entry.operation.id())
            .collect::<Vec<_>>(),
        expected_children,
        "causally independent ready nodes use descending operation ID as the tie-breaker"
    );
    assert!(log.entries[..2].iter().all(|entry| {
        entry.is_head && entry.verification == OperationVerificationState::Verified
    }));
    assert_eq!(
        log.entries[2].operation.id(),
        parent_id,
        "children must precede their parent"
    );

    let selected = first.id().to_string();
    let prefix = selected[..8].to_ascii_lowercase();
    assert_eq!(
        repository
            .resolve_operation_id(&prefix)
            .expect("resolve lowercase prefix"),
        first.id()
    );
    assert!(matches!(
        repository.resolve_operation_id("ABC"),
        Err(RepositoryError::InvalidOperationSelector { .. })
    ));
    assert!(matches!(
        repository.resolve_operation_id("ZZZZ"),
        Err(RepositoryError::OperationNotFound { .. })
    ));

    let details = repository
        .operation_details(first.id())
        .expect("load operation details");
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(details.head_of, vec![scope]);
    assert_eq!(details.receipts.len(), 1);
}

#[test]
fn operation_inspection_open_exposes_an_incomplete_head_without_recovery() {
    let (directory, repository, working_copy) = switched_repository();
    let scope = OperationScope::WorkingCopy(working_copy);
    let parent_id = repository
        .pristine
        .read_txn()
        .expect("open read transaction")
        .get_operation_heads(scope)
        .expect("load operation heads")
        .as_slice()[0];
    let parent = repository
        .operation_details(parent_id)
        .expect("load parent operation")
        .operation;
    let prepared = child_operation(&parent, 200, "prepared-only");

    {
        let mut txn = repository
            .pristine
            .write_txn_immediate()
            .expect("open immediate operation transaction");
        txn.put_operation(&prepared)
            .expect("append prepared operation");
        txn.compare_and_set_operation_heads(scope, &[parent_id], &[prepared.id()])
            .expect("publish prepared head");
        txn.commit().expect("commit prepared head");
    }
    drop(repository);

    assert!(matches!(
        Repository::open_readonly(directory.path()),
        Err(RepositoryError::InvalidOperation { .. })
    ));

    let inspection = Repository::open_readonly_for_operation_inspection(directory.path())
        .expect("open operation inspection mode");
    let log = inspection
        .operation_log(scope, Some(1), false)
        .expect("inspect prepared operation");
    assert_eq!(log.head_state, OperationHeadState::Single(prepared.id()));
    assert_eq!(log.entries.len(), 1);
    assert_eq!(
        log.entries[0].verification,
        OperationVerificationState::Prepared
    );
    assert_eq!(log.entries[0].operation.id(), prepared.id());
}
