//! Incoming span changes must be indexed atomically and invalid closures refused.

use atomic_core::change::span_patch::{SpanOp, SpanPatch};
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::operation::OperationScope;
use atomic_core::pristine::{
    CapabilityMutTxnT, MutTxnT, OperationTxnT, ViewTxnT, SPAN_RANGE_CAPABILITY,
};
use atomic_core::types::{Hash, Merkle, OperationId};
use atomic_repository::apply::{CrossViewInsertOptions, InsertOptions};
use atomic_repository::{RecordOptions, Repository, StatusOptions};

#[test]
fn incoming_span_root_and_edits_are_indexed_on_insert() {
    use atomic_core::pristine::{span_patch_storage::SpanPatchReadTxn, GraphTxnT};
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let source_wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = source_dir.path().join("file.txt");
    std::fs::write(&path, b"one\ntwo\n").unwrap();
    source
        .add(source_wc, "file.txt", Default::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = source
        .record(source_wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&path, b"ONE\ntwo\n").unwrap();
    let edit = source
        .record(source_wc, ChangeHeader::new("edit"), options())
        .unwrap();
    std::fs::write(&path, b"ONE!\ntwo\n").unwrap();
    let nested = source
        .record(source_wc, ChangeHeader::new("nested"), options())
        .unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    let target_wc = target.require_working_copy_id().unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        // Force different repository-local allocation without adding view history.
        let id = txn
            .register_change(&Hash::of(b"receiver-local unrelated change"))
            .unwrap();
        txn.put_change_deps(id, &[]).unwrap();
        txn.commit().unwrap();
    }
    let mut rejected_bytes = Vec::new();
    root.change().serialize(&mut rejected_bytes).unwrap();
    let before_invalid = snapshot(&target);
    assert!(target
        .import_change_bytes(&Hash::of(b"wrong hash"), &rejected_bytes)
        .is_err());
    assert_eq!(snapshot(&target), before_invalid);
    {
        use atomic_core::pristine::CapabilityTxnT;
        assert_eq!(
            target
                .pristine()
                .read_txn()
                .unwrap()
                .required_capability_version(SPAN_RANGE_CAPABILITY.id())
                .unwrap(),
            None
        );
    }
    for outcome in [&root, &edit, &nested] {
        let mut bytes = Vec::new();
        assert_eq!(
            outcome.change().serialize(&mut bytes).unwrap(),
            *outcome.hash()
        );
        assert_eq!(
            target.import_change_bytes(outcome.hash(), &bytes).unwrap(),
            *outcome.hash()
        );
    }
    {
        use atomic_core::pristine::CapabilityTxnT;
        let txn = target.pristine().read_txn().unwrap();
        assert_eq!(
            txn.required_capability_version(SPAN_RANGE_CAPABILITY.id())
                .unwrap(),
            Some(SPAN_RANGE_CAPABILITY.minimum_version())
        );
        assert!(txn.get_internal(root.hash()).unwrap().is_none());
    }
    let before = snapshot(&target);
    let error = target
        .insert_change(nested.hash(), InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error.to_string().contains("missing applied dependency"),
        "{error}"
    );
    assert_eq!(snapshot(&target), before);
    assert!(target
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(root.hash())
        .unwrap()
        .is_none());
    let result = target
        .insert_change_rec(nested.hash(), InsertOptions::default().view("target"))
        .unwrap();
    assert!(result.stats.affected_paths.contains("file.txt"));
    {
        let txn = target.pristine().read_txn().unwrap();
        let source_txn = source.pristine().read_txn().unwrap();
        assert_ne!(
            txn.get_internal(root.hash()).unwrap(),
            source_txn.get_internal(root.hash()).unwrap(),
            "test must exercise different repository-local IDs"
        );
        for outcome in [&root, &edit, &nested] {
            let id = txn.get_internal(outcome.hash()).unwrap().unwrap();
            assert!(txn.was_change_applied(id).unwrap());
            assert!(txn.span_change_indexed(id).unwrap());
        }
    }
    assert_eq!(
        target
            .get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"ONE!\ntwo\n"
    );
    target.switch_view(target_wc, "target").unwrap();
    assert_eq!(
        std::fs::read(target_dir.path().join("file.txt")).unwrap(),
        b"ONE!\ntwo\n"
    );
    assert!(target
        .status(target_wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    {
        let txn = target.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(
                &txn,
                target.change_store(),
                "file.txt"
            )
            .unwrap(),
            b"ONE!\ntwo\n"
        );
    }
    std::fs::write(target_dir.path().join("file.txt"), b"ONE!\nTWO\n").unwrap();
    target
        .record(
            target_wc,
            ChangeHeader::new("local edit after import"),
            options(),
        )
        .expect("record after importing semantic identities");
    assert_eq!(
        target.get_file_content("file.txt").unwrap().unwrap(),
        b"ONE!\nTWO\n"
    );
    {
        let txn = target.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(
                &txn,
                target.change_store(),
                "file.txt"
            )
            .unwrap(),
            b"ONE!\nTWO\n"
        );
    }
    drop(target);
    let target = Repository::open(target_dir.path()).unwrap();
    assert_eq!(
        target
            .get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"ONE!\nTWO\n"
    );
}

#[test]
fn legacy_incoming_semantic_ids_are_refused_before_any_dependency_is_published() {
    use atomic_core::pristine::GraphTxnT;
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::write(source_dir.path().join("file.txt"), b"base\n").unwrap();
    source.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = source
        .record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(source_dir.path().join("file.txt"), b"edited\n").unwrap();
    let edit = source
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    assert!(!edit
        .change()
        .hashed
        .span
        .as_ref()
        .unwrap()
        .semantic_owners
        .is_empty());
    let mut legacy = edit.change().clone();
    let patch = legacy.hashed.span.as_mut().unwrap();
    patch.version = 1;
    patch.semantic_owners.clear();
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(target.save_change(root.change()).unwrap(), *root.hash());
    let legacy_hash = target.save_change(&legacy).unwrap();
    let before = snapshot(&target);
    let error = target
        .insert_change_rec(&legacy_hash, InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("complete portable owner mapping"),
        "{error}"
    );
    assert_eq!(snapshot(&target), before);
    assert!(target
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(root.hash())
        .unwrap()
        .is_none());
    assert!(!target_dir.path().join("file.txt").exists());
}

#[test]
fn legacy_span_capability_remains_readable_but_recording_requires_migration() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let source_wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::write(source_dir.path().join("file.txt"), b"legacy\n").unwrap();
    source
        .add(source_wc, "file.txt", Default::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = source
        .record(source_wc, ChangeHeader::new("root"), options())
        .unwrap();
    let mut legacy = root.change().clone();
    legacy.hashed.span.as_mut().unwrap().version = 1;
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    let wc = target.require_working_copy_id().unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let hash = target.save_change(&legacy).unwrap();
    target
        .insert_change(&hash, InsertOptions::default().view("target"))
        .unwrap();
    target.switch_view(wc, "target").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability_exact(SPAN_RANGE_CAPABILITY.id(), 2)
            .unwrap();
        txn.commit().unwrap();
    }
    drop(target);
    let target = Repository::open(target_dir.path()).unwrap();
    assert_eq!(
        target.get_file_content("file.txt").unwrap().unwrap(),
        b"legacy\n"
    );
    let mut new_bytes = Vec::new();
    root.change().serialize(&mut new_bytes).unwrap();
    let before_import = snapshot(&target);
    let import_error = target
        .import_change_bytes(root.hash(), &new_bytes)
        .unwrap_err();
    assert!(
        import_error.to_string().contains("requires migration"),
        "{import_error}"
    );
    assert_eq!(snapshot(&target), before_import);
    assert!(!target.has_change(root.hash()));
    let path = target_dir.path().join("file.txt");
    std::fs::write(&path, b"user edit\n").unwrap();
    let before = snapshot(&target);
    let error = target
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap_err();
    assert!(error.to_string().contains("requires migration"), "{error}");
    assert_eq!(snapshot(&target), before);
    assert_eq!(std::fs::read(path).unwrap(), b"user edit\n");
}

#[test]
fn incoming_span_name_conflict_refuses_before_publication() {
    use atomic_core::pristine::GraphTxnT;
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let source_wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::write(source_dir.path().join("file.txt"), b"source\n").unwrap();
    source
        .add(source_wc, "file.txt", Default::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let incoming = source
        .record(source_wc, ChangeHeader::new("source root"), options())
        .unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    let target_wc = target.require_working_copy_id().unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    target.switch_view(target_wc, "target").unwrap();
    std::fs::write(target_dir.path().join("file.txt"), b"target\n").unwrap();
    target
        .add(target_wc, "file.txt", Default::default())
        .unwrap();
    target
        .record(target_wc, ChangeHeader::new("target root"), options())
        .unwrap();
    assert_eq!(
        target.save_change(incoming.change()).unwrap(),
        *incoming.hash()
    );
    let before = snapshot(&target);
    let error = target
        .insert_change(incoming.hash(), InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error.to_string().contains("span-backed name conflict"),
        "{error}"
    );
    assert_eq!(snapshot(&target), before);
    assert!(target
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(incoming.hash())
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read(target_dir.path().join("file.txt")).unwrap(),
        b"target\n"
    );
    assert_eq!(
        target.get_file_content("file.txt").unwrap().unwrap(),
        b"target\n"
    );
}

#[test]
fn importing_spans_into_legacy_repository_preserves_legacy_edits() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let source_wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::write(source_dir.path().join("remote.txt"), b"remote\n").unwrap();
    source
        .add(source_wc, "remote.txt", Default::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let incoming = source
        .record(source_wc, ChangeHeader::new("incoming"), options())
        .unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    let wc = target.require_working_copy_id().unwrap();
    std::fs::write(target_dir.path().join("legacy.txt"), b"before\n").unwrap();
    target.add(wc, "legacy.txt", Default::default()).unwrap();
    let legacy = target
        .record(wc, ChangeHeader::new("legacy root"), options())
        .unwrap();
    assert!(legacy.change().hashed.span.is_none());
    target.create_draft_view("target", "dev").unwrap();
    target.switch_view(wc, "target").unwrap();
    let mut bytes = Vec::new();
    incoming.change().serialize(&mut bytes).unwrap();
    target.import_change_bytes(incoming.hash(), &bytes).unwrap();
    target
        .insert_change(incoming.hash(), InsertOptions::default().view("target"))
        .unwrap();
    target.materialize(wc).unwrap();
    std::fs::write(target_dir.path().join("legacy.txt"), b"after\n").unwrap();
    std::fs::write(target_dir.path().join("new.txt"), b"new span\n").unwrap();
    target.add(wc, "new.txt", Default::default()).unwrap();
    let mixed = target
        .record(wc, ChangeHeader::new("legacy edit and span add"), options())
        .unwrap();
    assert!(mixed.change().hashed.span.is_some());
    assert_eq!(
        target.get_file_content("legacy.txt").unwrap().unwrap(),
        b"after\n"
    );
    assert_eq!(
        target.get_file_content("new.txt").unwrap().unwrap(),
        b"new span\n"
    );
    assert_eq!(
        target.get_file_content("remote.txt").unwrap().unwrap(),
        b"remote\n"
    );
    for (name, expected) in [
        ("legacy.txt", b"after\n".as_slice()),
        ("new.txt", b"new span\n"),
    ] {
        let txn = target.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, target.change_store(), name)
                .unwrap(),
            expected
        );
    }
    drop(target);
    let target = Repository::open(target_dir.path()).unwrap();
    assert_eq!(
        target.get_file_content("legacy.txt").unwrap().unwrap(),
        b"after\n"
    );
    assert_eq!(
        target.get_file_content("new.txt").unwrap().unwrap(),
        b"new span\n"
    );
}

#[test]
fn incoming_span_move_and_deletion_preserve_semantic_identity() {
    use atomic_core::pristine::{CrdtTxnT, TreeTxnT};
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let source_wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let old = source_dir.path().join("old.txt");
    let new = source_dir.path().join("new.txt");
    std::fs::write(&old, b"base\n").unwrap();
    source
        .add(source_wc, "old.txt", Default::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = source
        .record(source_wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::rename(&old, &new).unwrap();
    let moved = source
        .record(source_wc, ChangeHeader::new("move"), options())
        .unwrap();
    std::fs::write(&new, b"edited\n").unwrap();
    let edited = source
        .record(source_wc, ChangeHeader::new("edit"), options())
        .unwrap();
    std::fs::remove_file(&new).unwrap();
    let deleted = source
        .record(source_wc, ChangeHeader::new("delete"), options())
        .unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    let target_wc = target.require_working_copy_id().unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    for outcome in [&root, &moved, &edited, &deleted] {
        assert_eq!(
            target.save_change(outcome.change()).unwrap(),
            *outcome.hash()
        );
    }
    target
        .insert_change_rec(edited.hash(), InsertOptions::default().view("target"))
        .unwrap();
    assert!(target
        .get_file_content_on_view("old.txt", "target")
        .unwrap()
        .is_none());
    assert_eq!(
        target
            .get_file_content_on_view("new.txt", "target")
            .unwrap()
            .unwrap(),
        b"edited\n"
    );
    target.switch_view(target_wc, "target").unwrap();
    let trunk_key = {
        let txn = target.pristine().read_txn().unwrap();
        let inode = txn.get_inode("new.txt").unwrap().unwrap();
        txn.get_crdt_inode_trunk(inode.get()).unwrap().unwrap()
    };
    {
        let txn = target.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, target.change_store(), "new.txt")
                .unwrap(),
            b"edited\n"
        );
    }
    target
        .insert_change_rec(deleted.hash(), InsertOptions::default().view("target"))
        .unwrap();
    assert_eq!(
        target
            .pristine()
            .read_txn()
            .unwrap()
            .get_crdt_trunk(&trunk_key)
            .unwrap()
            .unwrap()
            .state,
        atomic_core::crdt::TrunkState::Deleted
    );
    assert!(target.get_file_content("new.txt").unwrap().is_none());
    target.materialize(target_wc).unwrap();
    assert!(!target_dir.path().join("new.txt").exists());
    drop(target);
    let target = Repository::open(target_dir.path()).unwrap();
    assert!(target
        .get_file_content_on_view("new.txt", "target")
        .unwrap()
        .is_none());
}

#[test]
fn incoming_span_edit_rejects_semantic_trunk_from_another_file() {
    use atomic_core::change::FileOps;
    use atomic_core::pristine::{CrdtTxnT, GraphTxnT, TreeTxnT};
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let wc = source.require_working_copy_id().unwrap();
    {
        let mut txn = source.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    for name in ["a.txt", "b.txt"] {
        std::fs::write(source_dir.path().join(name), b"base\n").unwrap();
        source.add(wc, name, Default::default()).unwrap();
    }
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = source
        .record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(source_dir.path().join("a.txt"), b"edited\n").unwrap();
    let edit = source
        .record(wc, ChangeHeader::new("edit a"), options())
        .unwrap();
    let b_trunk = {
        let txn = source.pristine().read_txn().unwrap();
        let inode = txn.get_inode("b.txt").unwrap().unwrap();
        atomic_core::crdt::TrunkId::from_bytes(
            &txn.get_crdt_inode_trunk(inode.get()).unwrap().unwrap(),
        )
    };
    let mut forged = edit.change().clone();
    let mut wrong = FileOps::edit(b_trunk, "a.txt".into());
    for line in edit.change().file_ops()[0].line_ops() {
        wrong.add_line_op(line.clone());
    }
    forged.hashed.file_ops = vec![wrong];
    let target_dir = tempfile::tempdir().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    target.create_draft_view("target", "dev").unwrap();
    {
        let mut txn = target.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(target.save_change(root.change()).unwrap(), *root.hash());
    let forged_hash = target.save_change(&forged).unwrap();
    let before = snapshot(&target);
    let error = target
        .insert_change_rec(&forged_hash, InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error.to_string().contains("no matching semantic trunk"),
        "{error}"
    );
    assert_eq!(snapshot(&target), before);
    assert!(target
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(root.hash())
        .unwrap()
        .is_none());
}

fn snapshot(
    repo: &Repository,
) -> (
    Merkle,
    u64,
    Vec<OperationId>,
    Vec<OperationId>,
    Vec<OperationId>,
    usize,
) {
    let txn = repo.pristine().read_txn().unwrap();
    let view = txn.get_view("target").unwrap().unwrap();
    let mut operations: Vec<_> = txn
        .list_operations()
        .unwrap()
        .iter()
        .map(|op| op.id())
        .collect();
    operations.sort();
    let receipts = operations
        .iter()
        .map(|id| txn.get_effect_receipts(*id).unwrap().len())
        .sum();
    let mut heads = txn
        .get_operation_heads(OperationScope::Repository)
        .unwrap()
        .as_slice()
        .to_vec();
    heads.sort();
    let mut wc_heads = txn
        .get_operation_heads(OperationScope::WorkingCopy(
            repo.require_working_copy_id().unwrap(),
        ))
        .unwrap()
        .as_slice()
        .to_vec();
    wc_heads.sort();
    (
        view.state,
        view.change_count,
        operations,
        heads,
        wc_heads,
        receipts,
    )
}

fn fixture() -> (tempfile::TempDir, Repository, Hash, Hash) {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    let normal = Change::new(ChangeHeader::new("ordinary"), vec![], vec![], vec![]);
    let normal = repo.save_change(&normal).unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let mut change = Change::new(ChangeHeader::new("span"), vec![], b"x\n".to_vec(), vec![]);
    change.hashed.span = Some(SpanPatch::new(
        vec![],
        vec![SpanOp::Root {
            path: "x".into(),
            ordinal: 0,
            start: 0,
            end: 2,
        }],
    ));
    let span = repo.save_change(&change).unwrap();
    (dir, repo, normal, span)
}

#[test]
fn direct_insert_does_not_publish_span_without_namespace_creation() {
    let (_dir, repo, _, span) = fixture();
    let before = snapshot(&repo);
    let error = repo
        .insert_change(&span, InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error.to_string().contains("no unique namespace creation"),
        "{error}"
    );
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn cross_view_batch_preflights_all_changes() {
    let (_dir, repo, normal, span) = fixture();
    let before = snapshot(&repo);
    let error = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![normal, span]),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("no unique namespace creation"),
        "{error}"
    );
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn recursive_insert_preflights_dependencies() {
    let (_dir, repo, normal, span) = fixture();
    let mut dependencies = vec![normal, span];
    dependencies.sort();
    let root = Change::new(ChangeHeader::new("root"), vec![], vec![], dependencies);
    let root = repo.save_change(&root).unwrap();
    let before = snapshot(&repo);
    let error = repo
        .insert_change_rec(&root, InsertOptions::default().view("target"))
        .unwrap_err();
    assert!(
        error.to_string().contains("no unique namespace creation"),
        "{error}"
    );
    assert_eq!(snapshot(&repo), before);
}
