use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataValue, OperationKind, OperationRelation, OperationScope,
};
use atomic_core::pristine::{GraphTxnT, ViewTxnT};

use super::*;

fn record_all(repository: &TestRepository, message: &str) {
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repository
        .record(ChangeHeader::new(message), options)
        .expect("record test content");
}

#[test]
fn switch_undo_and_restore_append_related_operations_and_preserve_content() {
    let (directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let base_view = repository.current_view().to_string();
    let path = directory.path().join("operation-state.txt");

    std::fs::write(&path, b"base view\n").expect("write base content");
    repository
        .add("operation-state.txt", TrackingOptions::default())
        .expect("track test file");
    record_all(&repository, "record base operation state");

    repository
        .create_view("operation-feature")
        .expect("create feature view");
    repository
        .switch_view("operation-feature")
        .expect("switch to feature");
    std::fs::write(&path, b"feature view\n").expect("write feature content");
    record_all(&repository, "record feature operation state");

    repository
        .switch_view(&base_view)
        .expect("switch back to base");
    assert_eq!(
        std::fs::read(&path).expect("read base content"),
        b"base view\n"
    );
    let scope = OperationScope::WorkingCopy(working_copy);
    let switch_id = match repository
        .operation_log(scope, Some(1), false)
        .expect("load switch operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one switch head, found {other:?}"),
    };
    assert_eq!(
        repository
            .operation_details(switch_id)
            .expect("load switch details")
            .operation
            .payload()
            .kind,
        OperationKind::SwitchView
    );

    let undo_id = repository
        .undo_operation(working_copy, None)
        .expect("undo current switch");
    assert_eq!(repository.current_view(), "operation-feature");
    assert_eq!(
        std::fs::read(&path).expect("read restored feature content"),
        b"feature view\n"
    );
    let undo = repository
        .operation_details(undo_id)
        .expect("load undo operation");
    assert_eq!(undo.operation.payload().kind, OperationKind::Undo);
    assert_eq!(undo.operation.encoding_version(), 2);
    assert_eq!(
        undo.operation.payload().relation,
        Some(OperationRelation::Undo { target: switch_id })
    );
    assert_eq!(undo.verification, OperationVerificationState::Verified);

    let restore_id = repository
        .restore_operation(working_copy, switch_id)
        .expect("restore original switch after-state");
    assert_eq!(repository.current_view(), base_view);
    assert_eq!(
        std::fs::read(&path).expect("read re-restored base content"),
        b"base view\n"
    );
    let restore = repository
        .operation_details(restore_id)
        .expect("load restore operation");
    assert_eq!(restore.operation.payload().kind, OperationKind::Restore);
    assert_eq!(restore.operation.encoding_version(), 2);
    assert_eq!(
        restore.operation.payload().relation,
        Some(OperationRelation::Restore { target: switch_id })
    );
    assert_eq!(restore.verification, OperationVerificationState::Verified);
    assert_eq!(restore.operation.payload().parents, vec![undo_id]);
}

#[test]
fn record_undo_preserves_working_content_and_the_change_object() {
    let (directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let view_name = repository.current_view().to_string();
    let path = directory.path().join("record-undo.txt");
    std::fs::write(&path, b"recorded content\n").expect("write record content");
    repository
        .add("record-undo.txt", TrackingOptions::default())
        .expect("track record content");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    let outcome = repository
        .record(ChangeHeader::new("record undo content"), options)
        .expect("record content");
    let hash = *outcome.hash();
    let scope = OperationScope::WorkingCopy(working_copy);
    let record_id = match repository
        .operation_log(scope, Some(1), false)
        .expect("load record operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one record head, found {other:?}"),
    };
    let record = repository
        .operation_details(record_id)
        .expect("load record details");
    assert_eq!(record.operation.payload().kind, OperationKind::Record);
    assert_eq!(record.operation.encoding_version(), 2);
    assert_eq!(record.operation.payload().delta.metadata.len(), 1);
    assert!(matches!(
        &record.operation.payload().delta.metadata[0].target,
        MetadataTarget::ViewChange { view, change }
            if view == &view_name && change == &hash
    ));
    assert_eq!(
        record.operation.payload().delta.metadata[0].expected_old,
        MetadataValue::Absent
    );

    let undo_id = repository
        .undo_operation(working_copy, None)
        .expect("undo record operation");
    assert_eq!(
        std::fs::read(&path).expect("read retained working content"),
        b"recorded content\n",
        "record undo must not rewrite working-copy bytes"
    );
    assert!(
        repository.has_change(&hash),
        "change object must remain loadable"
    );
    {
        let txn = repository
            .pristine
            .read_txn()
            .expect("open membership transaction");
        let view = txn
            .get_view(&view_name)
            .expect("read view")
            .expect("view exists");
        let change_id = txn
            .get_internal(&hash)
            .expect("read registered change")
            .expect("change remains registered");
        assert_eq!(
            txn.get_change_seq(&view, change_id)
                .expect("read change membership"),
            None,
            "undo removes only the view reference"
        );
    }
    let undo = repository
        .operation_details(undo_id)
        .expect("load record undo");
    assert_eq!(undo.operation.payload().kind, OperationKind::Undo);
    assert_eq!(
        undo.operation.payload().relation,
        Some(OperationRelation::Undo { target: record_id })
    );

    let restore_id = repository
        .restore_operation(working_copy, record_id)
        .expect("restore record operation");
    let restore = repository
        .operation_details(restore_id)
        .expect("load record restore");
    assert_eq!(restore.operation.payload().kind, OperationKind::Restore);
    assert_eq!(
        restore.operation.payload().relation,
        Some(OperationRelation::Restore { target: record_id })
    );
    let txn = repository
        .pristine
        .read_txn()
        .expect("open restored membership transaction");
    let view = txn
        .get_view(&view_name)
        .expect("read restored view")
        .expect("restored view exists");
    let change_id = txn
        .get_internal(&hash)
        .expect("read restored change")
        .expect("restored change remains registered");
    assert!(txn
        .get_change_seq(&view, change_id)
        .expect("read restored membership")
        .is_some());
}

#[test]
fn historical_restore_reinserts_non_tail_membership_without_duplicating_shifted_changes() {
    let (directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };

    std::fs::write(directory.path().join("a.txt"), b"A\n").expect("write A");
    repository
        .add("a.txt", TrackingOptions::default())
        .expect("track A");
    let first = repository
        .record(ChangeHeader::new("record A"), options())
        .expect("record A");
    let first_hash = *first.hash();

    std::fs::write(directory.path().join("b.txt"), b"B\n").expect("write B");
    repository
        .add("b.txt", TrackingOptions::default())
        .expect("track B");
    let second = repository
        .record(ChangeHeader::new("record B"), options())
        .expect("record B");
    let second_hash = *second.hash();
    let scope = OperationScope::WorkingCopy(working_copy);
    let second_operation = match repository
        .operation_log(scope, Some(1), false)
        .expect("load record B operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one record B head, found {other:?}"),
    };

    repository
        .unrecord(&first_hash, UnrecordOptions::default())
        .expect("unrecord non-tail A");
    {
        let txn = repository
            .pristine
            .read_txn()
            .expect("open shifted membership transaction");
        let view = txn
            .get_view(repository.current_view())
            .expect("read shifted view")
            .expect("shifted view exists");
        let second_id = txn
            .get_internal(&second_hash)
            .expect("read B")
            .expect("B remains registered");
        assert_eq!(
            txn.get_change_seq(&view, second_id)
                .expect("read shifted B sequence"),
            Some(0)
        );
    }

    repository
        .restore_operation(working_copy, second_operation)
        .expect("restore state before non-tail unrecord");
    let txn = repository
        .pristine
        .read_txn()
        .expect("open restored membership transaction");
    let view = txn
        .get_view(repository.current_view())
        .expect("read restored view")
        .expect("restored view exists");
    let first_id = txn
        .get_internal(&first_hash)
        .expect("read A")
        .expect("A remains registered");
    let second_id = txn
        .get_internal(&second_hash)
        .expect("read B")
        .expect("B remains registered");
    assert_eq!(
        txn.get_change_seq(&view, first_id)
            .expect("read restored A sequence"),
        Some(0)
    );
    assert_eq!(
        txn.get_change_seq(&view, second_id)
            .expect("read restored B sequence"),
        Some(1)
    );
    assert_eq!(
        view.change_count, 2,
        "B must not be duplicated during restore"
    );
}

#[test]
fn historical_record_restore_removes_intervening_changes_and_reprojects_content() {
    let (directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let path = directory.path().join("historical-restore.txt");
    std::fs::write(&path, b"state A\n").expect("write state A");
    repository
        .add("historical-restore.txt", TrackingOptions::default())
        .expect("track historical restore file");
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let first = repository
        .record(ChangeHeader::new("historical state A"), options())
        .expect("record state A");
    let first_hash = *first.hash();
    let scope = OperationScope::WorkingCopy(working_copy);
    let first_operation = match repository
        .operation_log(scope, Some(1), false)
        .expect("load first record operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one first record head, found {other:?}"),
    };

    std::fs::write(&path, b"state B\n").expect("write state B");
    let second = repository
        .record(ChangeHeader::new("historical state B"), options())
        .expect("record state B");
    let second_hash = *second.hash();
    assert_eq!(std::fs::read(&path).expect("read state B"), b"state B\n");

    let restore_id = repository
        .restore_operation(working_copy, first_operation)
        .expect("restore historical state A");
    assert_eq!(
        std::fs::read(&path).expect("read restored state A"),
        b"state A\n",
        "historical restore must re-project the selected operation state"
    );
    let view_name = repository.current_view().to_string();
    let txn = repository
        .pristine
        .read_txn()
        .expect("open restored view transaction");
    let view = txn
        .get_view(&view_name)
        .expect("read restored view")
        .expect("restored view exists");
    let first_id = txn
        .get_internal(&first_hash)
        .expect("read first change")
        .expect("first change remains registered");
    let second_id = txn
        .get_internal(&second_hash)
        .expect("read second change")
        .expect("second change remains registered");
    assert!(txn
        .get_change_seq(&view, first_id)
        .expect("read first membership")
        .is_some());
    assert_eq!(
        txn.get_change_seq(&view, second_id)
            .expect("read second membership"),
        None
    );
    drop(txn);
    let restore = repository
        .operation_details(restore_id)
        .expect("load historical restore operation");
    assert_eq!(restore.operation.payload().kind, OperationKind::Restore);
    assert_eq!(restore.verification, OperationVerificationState::Verified);
    let current_head = match repository
        .operation_log(scope, Some(1), false)
        .expect("load post-restore head")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one materialize head, found {other:?}"),
    };
    let materialize = repository
        .operation_details(current_head)
        .expect("load restore materialization");
    assert_eq!(
        materialize.operation.payload().kind,
        OperationKind::Materialize
    );
    assert!(materialize
        .operation
        .payload()
        .parents
        .contains(&restore_id));
}

#[test]
fn incomplete_record_metadata_is_rolled_back_by_startup_recovery() {
    let (directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let view_name = repository.current_view().to_string();
    let path = directory.path().join("record-recovery.txt");
    std::fs::write(&path, b"recoverable content\n").expect("write recoverable content");
    repository
        .add("record-recovery.txt", TrackingOptions::default())
        .expect("track recoverable content");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repository
        .record(ChangeHeader::new("record recovery content"), options)
        .expect("record recoverable content");
    let scope = OperationScope::WorkingCopy(working_copy);
    let record_id = match repository
        .operation_log(scope, Some(1), false)
        .expect("load record operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one record head, found {other:?}"),
    };
    let record = repository
        .operation_details(record_id)
        .expect("load record details")
        .operation;
    repository
        .undo_operation(working_copy, None)
        .expect("establish absent metadata state");
    let current_id = match repository
        .operation_log(scope, Some(1), false)
        .expect("load undo head")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one undo head, found {other:?}"),
    };
    let current = repository
        .operation_details(current_id)
        .expect("load undo details")
        .operation;

    let operation_lock = repository
        .try_lock_operation(working_copy)
        .expect("lock working-copy operation");
    let prepared = repository
        .prepare_metadata_operation(
            &operation_lock,
            OperationKind::Record,
            None,
            current.payload().delta.after.clone(),
            record.payload().delta.after.clone(),
            record.payload().delta.metadata.clone(),
            record.payload().evidence.clone(),
            ActorRef::System {
                name: "record-recovery-test".to_string(),
            },
            super::super::operation::current_operation_timestamp_ms(),
        )
        .expect("prepare interrupted record metadata");
    repository
        .apply_operation_metadata_locked(&operation_lock, prepared.id())
        .expect("land metadata before interruption");
    drop(operation_lock);
    drop(repository);

    let recovered = Repository::open(directory.path()).expect("recover incomplete metadata");
    let txn = recovered
        .pristine
        .read_txn()
        .expect("open recovered membership transaction");
    let view = txn
        .get_view(&view_name)
        .expect("read recovered view")
        .expect("recovered view exists");
    let hash = match &record.payload().delta.metadata[0].target {
        MetadataTarget::ViewChange { change, .. } => *change,
        target => panic!("expected view-change metadata, found {target:?}"),
    };
    let change_id = txn
        .get_internal(&hash)
        .expect("read recovered change")
        .expect("change remains registered");
    assert_eq!(
        txn.get_change_seq(&view, change_id)
            .expect("read recovered membership"),
        None,
        "startup recovery must invert landed unverified metadata"
    );
    drop(txn);
    let head = match recovered
        .operation_log(scope, Some(1), false)
        .expect("load recovery operation")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one recovery head, found {other:?}"),
    };
    let recovery = recovered
        .operation_details(head)
        .expect("load recovery details");
    assert_eq!(recovery.operation.payload().kind, OperationKind::Recover);
    assert_eq!(recovery.operation.payload().parents, vec![prepared.id()]);
    assert_eq!(recovery.verification, OperationVerificationState::Verified);
    assert_eq!(
        std::fs::read(&path).expect("read preserved recovery content"),
        b"recoverable content\n"
    );
}

#[test]
fn undo_refuses_a_non_head_operation_without_mutating_history() {
    let (_directory, mut repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let base_view = repository.current_view().to_string();
    repository
        .create_view("operation-feature")
        .expect("create feature view");
    repository
        .switch_view("operation-feature")
        .expect("switch to feature");
    let first = match repository
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .expect("load first switch")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one head, found {other:?}"),
    };
    repository
        .switch_view(&base_view)
        .expect("switch back to base");
    let before = repository
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .expect("load current head");

    assert!(matches!(
        repository.undo_operation(working_copy, Some(first)),
        Err(RepositoryError::OperationNotReversible { .. })
    ));
    assert_eq!(
        repository
            .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
            .expect("reload current head")
            .head_state,
        before.head_state
    );
    assert_eq!(repository.current_view(), base_view);
}
