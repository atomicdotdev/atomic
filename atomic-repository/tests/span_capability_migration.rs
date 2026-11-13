//! `doctor migrate-spans`: raising an existing repository to native spans.

use atomic_core::change::ChangeHeader;
use atomic_core::operation::{EffectReceiptKind, OperationKind, OperationScope};
use atomic_core::pristine::{
    CapabilityMutTxnT, CapabilityTxnT, GraphTxnT, MutTxnT, OperationTxnT, SPAN_RANGE_CAPABILITY,
};
use atomic_repository::{RecordOptions, Repository, StatusOptions};

fn record_options() -> RecordOptions {
    RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true)
}

fn span_capability_version(repo: &Repository) -> Option<u32> {
    repo.pristine()
        .read_txn()
        .unwrap()
        .required_capability_version(SPAN_RANGE_CAPABILITY.id())
        .unwrap()
}

#[test]
fn migrate_enables_span_recording_on_a_graph_only_repository() {
    use atomic_core::pristine::span_patch_storage::SpanPatchReadTxn;

    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let path = temp.path().join("file.txt");

    // Graph-origin history recorded before the migration.
    std::fs::write(&path, b"one\ntwo\n").unwrap();
    repo.add(working_copy, "file.txt", Default::default())
        .unwrap();
    repo.record(
        working_copy,
        ChangeHeader::new("graph origin"),
        record_options(),
    )
    .unwrap();
    assert_eq!(span_capability_version(&repo), None);

    let outcome = repo.migrate_span_capability().unwrap();
    assert_eq!(outcome.previous, None);
    assert!(!outcome.already_migrated);
    let operation_id = outcome.operation.expect("migration journals an operation");
    assert_eq!(span_capability_version(&repo), Some(3));

    // The journal carries the capability lease, a verified receipt, and the
    // repository head.
    {
        let txn = repo.pristine().read_txn().unwrap();
        let operation = txn.get_operation(operation_id).unwrap().unwrap();
        assert_eq!(operation.payload().kind, OperationKind::Repair);
        assert!(operation.payload().working_copy.is_none());
        assert_eq!(operation.payload().delta.metadata.len(), 1);
        assert!(operation
            .payload()
            .delta
            .metadata
            .iter()
            .any(|transition| transition.expected_new
                == atomic_core::operation::MetadataValue::Sequence(3)));
        let receipts = txn.get_effect_receipts(operation_id).unwrap();
        assert!(receipts
            .iter()
            .any(|receipt| receipt.payload().kind == EffectReceiptKind::Verified));
        assert!(txn
            .get_operation_heads(OperationScope::Repository)
            .unwrap()
            .as_slice()
            .contains(&operation_id));
    }

    // Graph-origin content stays readable and the working copy is clean.
    assert!(repo
        .status(working_copy, StatusOptions::default())
        .unwrap()
        .is_clean());

    // New records after the migration take the span path.
    std::fs::write(&path, b"ONE\ntwo\n").unwrap();
    let edit = repo
        .record(
            working_copy,
            ChangeHeader::new("span edit"),
            record_options(),
        )
        .unwrap();
    {
        let txn = repo.pristine().read_txn().unwrap();
        let id = txn.get_internal(edit.hash()).unwrap().unwrap();
        assert!(txn.span_change_indexed(id).unwrap());
    }
    assert!(repo
        .status(working_copy, StatusOptions::default())
        .unwrap()
        .is_clean());
    assert_eq!(std::fs::read(&path).unwrap(), b"ONE\ntwo\n");

    // Idempotent: a migrated repository journals nothing.
    let again = repo.migrate_span_capability().unwrap();
    assert!(again.already_migrated);
    assert_eq!(again.operation, None);
    assert_eq!(span_capability_version(&repo), Some(3));
}

#[test]
fn migrate_raises_a_legacy_span_capability_version_two_repository() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability_exact(SPAN_RANGE_CAPABILITY.id(), 2)
            .unwrap();
        txn.commit().unwrap();
    }
    let path = temp.path().join("file.txt");
    std::fs::write(&path, b"one\n").unwrap();
    repo.add(working_copy, "file.txt", Default::default())
        .unwrap();

    // Version 2 refuses recording until it is migrated.
    let error = repo
        .record(
            working_copy,
            ChangeHeader::new("refused on v2"),
            record_options(),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("migration"),
        "unexpected refusal: {error}"
    );

    let outcome = repo.migrate_span_capability().unwrap();
    assert_eq!(outcome.previous, Some(2));
    assert!(!outcome.already_migrated);
    assert_eq!(span_capability_version(&repo), Some(3));

    // Recording works after the raise.
    repo.record(
        working_copy,
        ChangeHeader::new("record on v3"),
        record_options(),
    )
    .unwrap();
    assert!(repo
        .status(working_copy, StatusOptions::default())
        .unwrap()
        .is_clean());
}
