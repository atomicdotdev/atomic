use atomic_core::operation::{OperationKind, OperationScope};
use atomic_core::pristine::TagKind;
use atomic_core::{Hash, OperationId};

use super::*;

fn head_kind(
    repository: &Repository,
    working_copy: WorkingCopyId,
) -> (OperationId, OperationKind, OperationVerificationState) {
    let scope = OperationScope::WorkingCopy(working_copy);
    let head = match repository
        .operation_log(scope, Some(1), false)
        .expect("load operation head")
        .head_state
    {
        OperationHeadState::Single(head) => head,
        other => panic!("expected one operation head, found {other:?}"),
    };
    let details = repository
        .operation_details(head)
        .expect("load operation details");
    (head, details.operation.payload().kind, details.verification)
}

#[cfg(unix)]
#[test]
fn journaled_materialize_preserves_existing_restrictive_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, repository) = create_temp_repo();
    let path = directory.path().join("private.txt");
    std::fs::write(&path, b"recorded private content\n").expect("write private content");
    repository
        .add("private.txt", TrackingOptions::default())
        .expect("track private content");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repository
        .record(ChangeHeader::new("record private content"), options)
        .expect("record private content");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("restrict private file");
    std::fs::write(&path, b"dirty content\n").expect("write dirty private content");

    repository
        .materialize()
        .expect("materialize private content");
    assert_eq!(
        std::fs::read(&path).expect("read restored private content"),
        b"recorded private content\n"
    );
    assert_eq!(
        std::fs::metadata(&path)
            .expect("stat private content")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "materialization must not broaden an existing file's permissions"
    );
}

#[test]
fn native_mutation_chain_emits_verified_operation_kinds() {
    let (directory, repository) = create_temp_repo();
    let working_copy = repository.working_copy();
    let path = directory.path().join("routing.txt");
    std::fs::write(&path, b"operation routing\n").expect("write routing content");
    repository
        .add("routing.txt", TrackingOptions::default())
        .expect("track routing content");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    let recorded = repository
        .record(ChangeHeader::new("record routing content"), options)
        .expect("record routing content");
    let hash = *recorded.hash();
    let (record_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Record);
    assert_eq!(verification, OperationVerificationState::Verified);

    repository
        .create_tag("routing-tag", Some("routing tag"), TagKind::Release)
        .expect("create routed tag");
    let (tag_create_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Tag);
    assert_eq!(verification, OperationVerificationState::Verified);
    assert_eq!(
        repository
            .operation_details(tag_create_head)
            .expect("load tag create operation")
            .operation
            .payload()
            .parents,
        vec![record_head]
    );

    assert!(repository
        .delete_tag("routing-tag")
        .expect("delete routed tag"));
    let (tag_delete_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Tag);
    assert_eq!(verification, OperationVerificationState::Verified);
    assert_eq!(
        repository
            .operation_details(tag_delete_head)
            .expect("load tag delete operation")
            .operation
            .payload()
            .parents,
        vec![tag_create_head]
    );

    repository
        .unrecord(&hash, UnrecordOptions::default())
        .expect("unrecord routed change");
    let (unrecord_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Unrecord);
    assert_eq!(verification, OperationVerificationState::Verified);
    assert_eq!(
        repository
            .operation_details(unrecord_head)
            .expect("load unrecord operation")
            .operation
            .payload()
            .parents,
        vec![tag_delete_head]
    );

    repository
        .insert_change(&hash, InsertOptions::default())
        .expect("reinsert routed change");
    let (insert_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Insert);
    assert_eq!(verification, OperationVerificationState::Verified);
    assert_eq!(
        repository
            .operation_details(insert_head)
            .expect("load insert operation")
            .operation
            .payload()
            .parents,
        vec![unrecord_head]
    );

    repository.materialize().expect("materialize routed change");
    let (materialize_head, kind, verification) = head_kind(&repository, working_copy);
    assert_eq!(kind, OperationKind::Materialize);
    assert_eq!(verification, OperationVerificationState::Verified);
    assert_eq!(
        repository
            .operation_details(materialize_head)
            .expect("load materialize operation")
            .operation
            .payload()
            .parents,
        vec![insert_head]
    );

    let push_head = repository
        .append_verified_remote_operation(
            working_copy,
            OperationKind::Push,
            "origin",
            Hash::of(b"push evidence"),
        )
        .expect("append verified push operation");
    let push = repository
        .operation_details(push_head)
        .expect("load push operation");
    assert_eq!(push.operation.payload().kind, OperationKind::Push);
    assert_eq!(push.verification, OperationVerificationState::Verified);
    let mut expected_push_parents = vec![insert_head, materialize_head];
    expected_push_parents.sort_unstable();
    assert_eq!(push.operation.payload().parents, expected_push_parents);

    let pull_head = repository
        .append_verified_remote_operation(
            working_copy,
            OperationKind::Pull,
            "origin",
            Hash::of(b"pull evidence"),
        )
        .expect("append verified pull operation");
    let pull = repository
        .operation_details(pull_head)
        .expect("load pull operation");
    assert_eq!(pull.operation.payload().kind, OperationKind::Pull);
    assert_eq!(pull.verification, OperationVerificationState::Verified);
    assert_eq!(pull.operation.payload().parents, vec![push_head]);
}
