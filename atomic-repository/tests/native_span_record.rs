//! Native span recording uses the ordinary record and materialization paths.

use atomic_core::change::{ChangeHeader, GraphOp};
use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
use atomic_repository::{RecordOptions, Repository, StatusOptions};

#[test]
fn cross_view_span_insert_includes_dependencies_and_composes_independent_edits() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(&path, b"ALPHA\nbeta\ngamma\n").unwrap();
    let first = repo
        .record(wc, ChangeHeader::new("first"), options())
        .unwrap();
    std::fs::write(&path, b"ALPHA!\nbeta\ngamma\n").unwrap();
    let second = repo
        .record(wc, ChangeHeader::new("second"), options())
        .unwrap();
    repo.switch_view(wc, "target").unwrap();
    std::fs::write(&path, b"alpha\nbeta\nGAMMA\n").unwrap();
    let target = repo
        .record(wc, ChangeHeader::new("target"), options())
        .unwrap();
    let outcome = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![*second.hash()]),
        )
        .unwrap();
    assert!(outcome.affected_paths.contains("file.txt"));
    let changes = repo.get_view_changes(Some("target")).unwrap();
    assert!(changes.iter().any(|(_, hash)| hash == first.hash()));
    assert!(changes.iter().any(|(_, hash)| hash == second.hash()));
    assert!(changes.iter().any(|(_, hash)| hash == target.hash()));
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"ALPHA!\nbeta\nGAMMA\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "source")
            .unwrap()
            .unwrap(),
        b"ALPHA!\nbeta\ngamma\n"
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            b"ALPHA!\nbeta\nGAMMA\n"
        );
    }
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"ALPHA!\nbeta\nGAMMA\n");
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"ALPHA!\nbeta\nGAMMA\n"
    );
}

#[test]
fn overlapping_cross_view_span_insert_refuses_before_publication() {
    use atomic_core::operation::OperationScope;
    use atomic_core::pristine::{OperationTxnT, ViewTxnT};
    use atomic_repository::apply::CrossViewInsertOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"base\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(dir.path().join("safe.txt"), b"safe\n").unwrap();
    repo.add(wc, "safe.txt", Default::default()).unwrap();
    let safe = repo
        .record(wc, ChangeHeader::new("safe source change"), options())
        .unwrap();
    std::fs::write(&path, b"source\n").unwrap();
    let source = repo
        .record(wc, ChangeHeader::new("source edit"), options())
        .unwrap();
    repo.switch_view(wc, "target").unwrap();
    std::fs::write(&path, b"target\n").unwrap();
    repo.record(wc, ChangeHeader::new("target edit"), options())
        .unwrap();
    let snapshot = || {
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view("target").unwrap().unwrap();
        (
            view.state,
            view.change_count,
            txn.list_operations().unwrap().len(),
            txn.get_operation_heads(OperationScope::WorkingCopy(wc))
                .unwrap(),
        )
    };
    let before = snapshot();
    let error = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target")
                .only_changes(vec![*safe.hash(), *source.hash()]),
        )
        .unwrap_err();
    assert!(error.to_string().contains("overlapping"), "{error}");
    assert_eq!(snapshot(), before);
    assert_eq!(std::fs::read(&path).unwrap(), b"target\n");
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"target\n"
    );
    assert!(repo
        .get_file_content_on_view("safe.txt", "target")
        .unwrap()
        .is_none());
}

#[test]
fn insert_span_root_and_nested_edits_into_empty_sibling_view() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"original\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = repo
        .record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&path, b"edited\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    repo.switch_view(wc, "target").unwrap();
    assert!(!path.exists());
    let result = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![*edit.hash()]),
        )
        .unwrap();
    assert!(result.affected_paths.contains("file.txt"));
    let changes = repo.get_view_changes(Some("target")).unwrap();
    let hashes: Vec<_> = changes.iter().map(|(_, hash)| *hash).collect();
    assert!(
        hashes.iter().position(|hash| hash == root.hash()).unwrap()
            < hashes.iter().position(|hash| hash == edit.hash()).unwrap()
    );
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"edited\n"
    );
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"edited\n");
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    let repeat = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![*edit.hash()]),
        )
        .unwrap();
    assert_eq!(repeat.changes_applied, 0);
    assert!(repeat.skipped_hashes.contains(edit.hash()));
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"edited\n"
    );
}

#[test]
fn inserting_span_edit_also_inserts_its_rename_dependency() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"original\n").unwrap();
    repo.add(wc, "old.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::rename(&old, &new).unwrap();
    let moved = repo
        .record(wc, ChangeHeader::new("move"), options())
        .unwrap();
    std::fs::write(&new, b"edited\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    assert!(edit.change().dependencies().contains(moved.hash()));
    repo.switch_view(wc, "target").unwrap();
    let result = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![*edit.hash()]),
        )
        .unwrap();
    assert!(result.affected_paths.contains("old.txt"));
    assert!(result.affected_paths.contains("new.txt"));
    assert!(repo
        .get_file_content_on_view("old.txt", "target")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("new.txt", "target")
            .unwrap()
            .unwrap(),
        b"edited\n"
    );
    repo.materialize(wc).unwrap();
    assert!(!old.exists());
    assert_eq!(std::fs::read(&new).unwrap(), b"edited\n");
}

#[test]
fn inserting_span_file_deletion_retains_other_views() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"base\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(&path, b"edited\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let deletion = repo
        .record(wc, ChangeHeader::new("delete"), options())
        .unwrap();
    assert!(deletion.change().dependencies().contains(edit.hash()));
    repo.switch_view(wc, "target").unwrap();
    let result = repo
        .insert_from_view(
            CrossViewInsertOptions::new("source", "target").only_changes(vec![*deletion.hash()]),
        )
        .unwrap();
    assert!(result.affected_paths.contains("file.txt"));
    assert!(repo
        .get_file_content_on_view("file.txt", "target")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "dev")
            .unwrap()
            .unwrap(),
        b"base\n"
    );
    repo.materialize(wc).unwrap();
    assert!(!path.exists());
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo
        .get_file_content_on_view("file.txt", "target")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "dev")
            .unwrap()
            .unwrap(),
        b"base\n"
    );
}

#[test]
fn cross_view_insert_refuses_missing_span_index_before_journal_changes() {
    use atomic_core::operation::OperationScope;
    use atomic_core::pristine::{GraphTxnT, OperationTxnT, ViewTxnT};
    use atomic_repository::apply::CrossViewInsertOptions;
    use redb::TableDefinition;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(dir.path().join("file.txt"), b"source\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let change = repo
        .record(
            wc,
            ChangeHeader::new("root"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    let hash = *change.hash();
    let id = repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(&hash)
        .unwrap()
        .unwrap();
    drop(repo);
    let db = redb::Database::open(dir.path().join(".atomic/atomic.redb")).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut applied = txn
            .open_table(TableDefinition::<u64, u8>::new("span_patch_applied_v1"))
            .unwrap();
        assert!(applied.remove(id.get()).unwrap().is_some());
    }
    txn.commit().unwrap();
    drop(db);
    let repo = Repository::open(dir.path()).unwrap();
    let snapshot = || {
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view("target").unwrap().unwrap();
        (
            view.state,
            view.change_count,
            txn.list_operations().unwrap().len(),
            txn.get_operation_heads(OperationScope::WorkingCopy(wc))
                .unwrap(),
            txn.get_operation_heads(OperationScope::Repository).unwrap(),
        )
    };
    let before = snapshot();
    let error = repo
        .insert_from_view(CrossViewInsertOptions::new("source", "target").only_changes(vec![hash]))
        .unwrap_err();
    assert!(
        error.to_string().contains("span-aware repository writer"),
        "{error}"
    );
    assert_eq!(snapshot(), before);
    assert_eq!(
        std::fs::read(dir.path().join("file.txt")).unwrap(),
        b"source\n"
    );
}

#[test]
fn cross_view_insert_refuses_missing_span_edit_row() {
    use atomic_core::operation::OperationScope;
    use atomic_core::pristine::span_patch_storage::SpanPatchReadTxn;
    use atomic_core::pristine::{OperationTxnT, TreeTxnT, ViewTxnT};
    use atomic_repository::apply::CrossViewInsertOptions;
    use redb::MultimapTableDefinition;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"base\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(&path, b"edited\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    let hash = *edit.hash();
    let (key, value) = {
        let txn = repo.pristine().read_txn().unwrap();
        let inode = txn.get_inode("file.txt").unwrap().unwrap();
        let rows = txn.span_edits_for_inode(inode).unwrap();
        assert_eq!(rows.len(), 1);
        rows[0]
    };
    drop(repo);
    let db = redb::Database::open(dir.path().join(".atomic/atomic.redb")).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut edits = txn
            .open_multimap_table(MultimapTableDefinition::<&[u8; 32], &[u8; 40]>::new(
                "span_patch_edits_v1",
            ))
            .unwrap();
        assert!(edits.remove(&key, &value).unwrap());
    }
    txn.commit().unwrap();
    drop(db);
    let repo = Repository::open(dir.path()).unwrap();
    let snapshot = || {
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view("target").unwrap().unwrap();
        (
            view.state,
            view.change_count,
            txn.list_operations().unwrap().len(),
            txn.get_operation_heads(OperationScope::WorkingCopy(wc))
                .unwrap(),
        )
    };
    let before = snapshot();
    let error = repo
        .insert_from_view(CrossViewInsertOptions::new("source", "target").only_changes(vec![hash]))
        .unwrap_err();
    assert!(error.to_string().contains("span index differs"), "{error}");
    assert_eq!(snapshot(), before);
    assert_eq!(std::fs::read(&path).unwrap(), b"edited\n");
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "target")
            .unwrap()
            .unwrap(),
        b"base\n"
    );
}

#[test]
fn span_insert_operation_can_be_undone_and_restored() {
    use atomic_core::operation::{OperationKind, OperationScope};
    use atomic_core::pristine::OperationTxnT;
    use atomic_repository::apply::CrossViewInsertOptions;
    use atomic_repository::OperationVerificationState;
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"base\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("source", "dev").unwrap();
    repo.create_draft_view("target", "dev").unwrap();
    repo.switch_view(wc, "source").unwrap();
    std::fs::write(&path, b"edited\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    repo.switch_view(wc, "target").unwrap();
    repo.insert_from_view(
        CrossViewInsertOptions::new("source", "target").only_changes(vec![*edit.hash()]),
    )
    .unwrap();
    let id = {
        let txn = repo.pristine().read_txn().unwrap();
        let heads = txn
            .get_operation_heads(OperationScope::WorkingCopy(wc))
            .unwrap();
        assert_eq!(heads.as_slice().len(), 1);
        heads.as_slice()[0]
    };
    let details = repo.operation_details(id).unwrap();
    assert_eq!(details.operation.payload().kind, OperationKind::Insert);
    assert_eq!(details.verification, OperationVerificationState::Verified);
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"edited\n"
    );
    repo.undo_operation(wc, None).unwrap();
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"base\n"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"base\n");
    repo.restore_operation(wc, id).unwrap();
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"edited\n"
    );
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"edited\n");
}

#[test]
fn native_span_file_deletion_preserves_sibling_view_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"original\nbytes\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&path, b"edited\nbytes\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    repo.create_draft_view("deletion", "dev").unwrap();
    repo.switch_view(wc, "deletion").unwrap();
    std::fs::remove_file(&path).unwrap();
    let deletion = repo
        .record(wc, ChangeHeader::new("delete"), options())
        .expect("record span deletion");
    assert!(deletion
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::FileDel { .. })));
    assert!(deletion.change().dependencies().contains(edit.hash()));
    assert!(repo
        .get_file_content_on_view("file.txt", "deletion")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "dev")
            .unwrap()
            .unwrap(),
        b"edited\nbytes\n"
    );
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    repo.materialize(wc).unwrap();
    assert!(!path.exists());
    repo.switch_view(wc, "dev").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"edited\nbytes\n");
    repo.switch_view(wc, "deletion").unwrap();
    assert!(!path.exists());
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo
        .get_file_content_on_view("file.txt", "deletion")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "dev")
            .unwrap()
            .unwrap(),
        b"edited\nbytes\n"
    );
}

#[test]
fn native_span_rename_retains_identity_content_and_causality() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"first\nsecond\n").unwrap();
    repo.add(wc, "old.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&old, b"edited\nsecond\n").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit"), options())
        .unwrap();
    let identity = repo.get_inode_and_position("old.txt").unwrap().unwrap();
    repo.create_draft_view("rename", "dev").unwrap();
    repo.switch_view(wc, "rename").unwrap();
    std::fs::rename(&old, &new).unwrap();
    let moved = repo
        .record(wc, ChangeHeader::new("rename"), options())
        .expect("record span rename");
    assert!(moved
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::FileMove { path, .. } if path == "new.txt")));
    assert!(moved.change().dependencies().contains(edit.hash()));
    assert_eq!(
        repo.get_inode_and_position("new.txt").unwrap(),
        Some(identity)
    );
    assert_eq!(
        repo.get_file_content("new.txt").unwrap().unwrap(),
        b"edited\nsecond\n"
    );
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    repo.switch_view(wc, "dev").unwrap();
    assert_eq!(std::fs::read(&old).unwrap(), b"edited\nsecond\n");
    assert!(!new.exists());
    repo.switch_view(wc, "rename").unwrap();
    assert_eq!(std::fs::read(&new).unwrap(), b"edited\nsecond\n");
    std::fs::remove_file(&new).unwrap();
    let deleted = repo
        .record(wc, ChangeHeader::new("delete renamed file"), options())
        .unwrap();
    assert!(deleted.change().dependencies().contains(moved.hash()));
    assert!(deleted.change().dependencies().contains(edit.hash()));
    assert!(repo.get_file_content("new.txt").unwrap().is_none());
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo
        .get_file_content_on_view("new.txt", "rename")
        .unwrap()
        .is_none());
    assert_eq!(
        repo.get_file_content_on_view("old.txt", "dev")
            .unwrap()
            .unwrap(),
        b"edited\nsecond\n"
    );
}

#[test]
fn span_move_with_content_change_refuses_before_publication() {
    use atomic_core::operation::OperationScope;
    use atomic_core::pristine::{OperationTxnT, ViewTxnT};
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"original\n").unwrap();
    repo.add(wc, "old.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::rename(&old, &new).unwrap();
    repo.move_file(wc, "old.txt", "new.txt").unwrap();
    std::fs::write(&new, b"user edits\n").unwrap();
    let observe = || {
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view(repo.current_view()).unwrap().unwrap();
        (
            view.state,
            view.change_count,
            txn.list_operations().unwrap().len(),
            txn.get_operation_heads(OperationScope::WorkingCopy(wc))
                .unwrap(),
        )
    };
    let before = observe();
    let error = repo
        .record(wc, ChangeHeader::new("move and edit"), options())
        .unwrap_err();
    assert!(
        error.to_string().contains("move with content changes"),
        "{error}"
    );
    assert_eq!(observe(), before);
    assert_eq!(std::fs::read(&new).unwrap(), b"user edits\n");
    assert_eq!(
        repo.get_file_content_on_view("old.txt", "dev")
            .unwrap()
            .unwrap(),
        b"original\n"
    );
}

#[test]
fn span_untrack_keeps_user_bytes_and_can_share_a_record_with_new_files() {
    use atomic_repository::TrackingOptions;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let old = dir.path().join("old.txt");
    std::fs::write(&old, b"recorded\n").unwrap();
    repo.add(wc, "old.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&old, b"keep user bytes\n").unwrap();
    repo.remove(
        wc,
        "old.txt",
        TrackingOptions::default().with_keep_recorded(true),
    )
    .unwrap();
    std::fs::write(dir.path().join("new.txt"), b"new file\n").unwrap();
    repo.add(wc, "new.txt", Default::default()).unwrap();
    let change = repo
        .record(wc, ChangeHeader::new("untrack and add"), options())
        .unwrap();
    assert!(change
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::FileDel { path, .. } if path == "old.txt")));
    assert!(change.change().hashed.span.is_some());
    assert!(repo.get_file_content("old.txt").unwrap().is_none());
    assert_eq!(std::fs::read(&old).unwrap(), b"keep user bytes\n");
    assert_eq!(
        repo.get_file_content("new.txt").unwrap().unwrap(),
        b"new file\n"
    );
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.get_file_content("old.txt").unwrap().is_none());
    assert_eq!(std::fs::read(&old).unwrap(), b"keep user bytes\n");
    assert_eq!(
        repo.get_file_content("new.txt").unwrap().unwrap(),
        b"new file\n"
    );
}

#[test]
fn native_recorded_file_renders_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    // The capability identifies the new representation. Initializing new
    // repositories with it will be enabled once native edits are supported.
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    let bytes = b"alpha\nbeta\nomega";
    std::fs::write(&path, bytes).unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let outcome = repo
        .record(
            wc,
            ChangeHeader::new("record file"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .expect("native span record");
    let patch = outcome
        .change()
        .hashed
        .span
        .as_ref()
        .expect("span operations");
    assert_eq!(patch.ops.len(), 1);
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::FileAdd { contents: None, .. })));
    assert!(!outcome
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::Edit { .. })));
    assert_eq!(repo.get_file_content("file.txt").unwrap().unwrap(), bytes);
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            bytes
        );
    }
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(
        reopened.get_file_content("file.txt").unwrap().unwrap(),
        bytes
    );
}

#[test]
fn native_recorded_line_edit_uses_an_authored_range() {
    use atomic_core::change::span_patch::SpanOp;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nbeta\nomega").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = repo
        .record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(&path, b"ALPHA\nbeta\nomega").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("edit first line"), options())
        .expect("record span edit");
    let patch = edit.change().hashed.span.as_ref().expect("span operations");
    assert!(
        matches!(&patch.ops[..], [SpanOp::Edit { target, start: 0, end: 6, .. }] if target.owner == *root.hash())
    );
    assert_eq!(edit.change().contents, b"ALPHA\n");
    assert!(!edit.change().file_ops().is_empty());
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"ALPHA\nbeta\nomega"
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            b"ALPHA\nbeta\nomega"
        );
    }
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    std::fs::write(&path, b"ALPHA\nBETA\nomega").unwrap();
    let second = repo
        .record(wc, ChangeHeader::new("edit another line"), options())
        .expect("record second span edit");
    assert!(second.change().dependencies().contains(edit.hash()));
    assert_eq!(second.change().contents, b"BETA\n");
    std::fs::write(&path, b"Alpha again\nBETA\nomega").unwrap();
    let third = repo
        .record(
            wc,
            ChangeHeader::new("edit previously changed line"),
            options(),
        )
        .expect("record nested span edit");
    assert!(third.change().dependencies().contains(second.hash()));
    assert!(
        matches!(&third.change().hashed.span.as_ref().unwrap().ops[..], [SpanOp::Edit { target, .. }] if target.owner == *edit.hash())
    );
    assert_eq!(third.change().contents, b"Alpha again\n");
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"Alpha again\nBETA\nomega"
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            b"Alpha again\nBETA\nomega"
        );
    }
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"Alpha again\nBETA\nomega");
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(
        reopened.get_file_content("file.txt").unwrap().unwrap(),
        b"Alpha again\nBETA\nomega"
    );
}

#[test]
fn one_record_preserves_distinct_span_and_semantic_offsets_for_two_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    for (path, bytes) in [("a.txt", b"alpha\nend".as_slice()), ("b.txt", b"beta\nend")] {
        std::fs::write(dir.path().join(path), bytes).unwrap();
        repo.add(wc, path, Default::default()).unwrap();
    }
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("roots"), options())
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), b"ALPHA\nend").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"BETA\nend").unwrap();
    let edit = repo
        .record(wc, ChangeHeader::new("two edits"), options())
        .unwrap();
    assert_eq!(edit.change().hashed.span.as_ref().unwrap().ops.len(), 2);
    assert_eq!(edit.change().contents.len(), b"ALPHA\nBETA\n".len());
    for (path, expected) in [("a.txt", b"ALPHA\nend".as_slice()), ("b.txt", b"BETA\nend")] {
        assert_eq!(repo.get_file_content(path).unwrap().unwrap(), expected);
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), path)
                .unwrap(),
            expected
        );
        drop(txn);
        std::fs::remove_file(dir.path().join(path)).unwrap();
    }
    repo.materialize(wc).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).unwrap(),
        b"ALPHA\nend"
    );
    assert_eq!(
        std::fs::read(dir.path().join("b.txt")).unwrap(),
        b"BETA\nend"
    );
}

#[cfg(unix)]
#[test]
fn native_span_mode_change_preserves_content_and_materializes_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nomega").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.record(wc, ChangeHeader::new("mode edit"), options())
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nomega");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"alpha\nomega"
    );
    std::fs::write(&path, b"ALPHA\nomega").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let combined = repo
        .record(wc, ChangeHeader::new("content and mode"), options())
        .unwrap();
    assert!(combined.change().hashed.span.is_some());
    assert!(combined
        .change()
        .hunks()
        .iter()
        .any(|hunk| matches!(hunk, GraphOp::SetAttr { .. })));
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"ALPHA\nomega");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn native_line_insertion_and_deletion_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nbeta\nomega").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    for (message, bytes) in [
        ("insert", b"alpha\nnew line\nbeta\nomega".as_slice()),
        ("delete", b"alpha\nnew line\nomega".as_slice()),
        ("append", b"alpha\nnew line\nomega\nlast".as_slice()),
    ] {
        std::fs::write(&path, bytes).unwrap();
        repo.record(wc, ChangeHeader::new(message), options())
            .unwrap_or_else(|error| panic!("{message}: {error}"));
        assert_eq!(
            repo.get_file_content("file.txt").unwrap().unwrap(),
            bytes,
            "{message}"
        );
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            bytes,
            "{message}"
        );
        drop(txn);
        assert!(
            repo.status(wc, StatusOptions::default())
                .unwrap()
                .is_clean(),
            "{message}"
        );
    }
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"alpha\nnew line\nomega\nlast"
    );
}

#[test]
fn native_text_edits_preserve_bytes_across_common_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nbeta\ngamma\nomega").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    for (message, bytes) in [
        ("prepend", b"head\nalpha\nbeta\ngamma\nomega".as_slice()),
        (
            "disjoint changes",
            b"HEAD\nalpha\nbeta\nGAMMA\nomega".as_slice(),
        ),
        ("delete middle", b"HEAD\nomega".as_slice()),
        ("append", b"HEAD\nomega\nlast".as_slice()),
        ("empty", b"".as_slice()),
        ("refill", b"refilled\nvalue".as_slice()),
    ] {
        std::fs::write(&path, bytes).unwrap();
        repo.record(wc, ChangeHeader::new(message), options())
            .unwrap_or_else(|error| panic!("{message}: {error}"));
        assert_eq!(
            repo.get_file_content("file.txt").unwrap().unwrap(),
            bytes,
            "{message}"
        );
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            bytes,
            "{message}"
        );
        drop(txn);
        assert!(
            repo.status(wc, StatusOptions::default())
                .unwrap()
                .is_clean(),
            "{message}"
        );
    }
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(
        reopened.get_file_content("file.txt").unwrap().unwrap(),
        b"refilled\nvalue"
    );
}

#[test]
fn native_record_mixes_existing_span_edits_with_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::create_dir(dir.path().join("dir")).unwrap();
    std::fs::write(dir.path().join("dir/old.txt"), b"old\nend").unwrap();
    repo.add(wc, "dir/old.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    std::fs::write(dir.path().join("dir/old.txt"), b"EDITED\nend").unwrap();
    std::fs::write(dir.path().join("dir/new.txt"), b"new\nfile").unwrap();
    repo.add(wc, "dir/new.txt", Default::default()).unwrap();
    let outcome = repo
        .record(wc, ChangeHeader::new("mixed"), options())
        .unwrap();
    assert_eq!(outcome.change().hashed.span.as_ref().unwrap().ops.len(), 2);
    for (path, expected) in [
        ("dir/old.txt", b"EDITED\nend".as_slice()),
        ("dir/new.txt", b"new\nfile"),
    ] {
        assert_eq!(repo.get_file_content(path).unwrap().unwrap(), expected);
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), path)
                .unwrap(),
            expected
        );
    }
}

#[test]
fn native_empty_and_binary_roots_materialize_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    for (path, bytes) in [
        ("empty.txt", b"".as_slice()),
        ("data.bin", b"\0\x01\xff\nbytes"),
    ] {
        std::fs::write(dir.path().join(path), bytes).unwrap();
        repo.add(wc, path, Default::default()).unwrap();
    }
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    let outcome = repo
        .record(wc, ChangeHeader::new("roots"), options)
        .unwrap();
    assert_eq!(outcome.change().hashed.span.as_ref().unwrap().ops.len(), 2);
    for (path, bytes) in [
        ("empty.txt", b"".as_slice()),
        ("data.bin", b"\0\x01\xff\nbytes"),
    ] {
        assert_eq!(
            repo.get_file_content(path).unwrap().unwrap(),
            bytes,
            "{path}"
        );
        std::fs::remove_file(dir.path().join(path)).unwrap();
    }
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(dir.path().join("empty.txt")).unwrap(), b"");
    assert_eq!(
        std::fs::read(dir.path().join("data.bin")).unwrap(),
        b"\0\x01\xff\nbytes"
    );
}

#[test]
fn native_binary_edit_records_and_materializes_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("data.bin");
    std::fs::write(&path, b"\0\x01\xff\nbytes").unwrap();
    repo.add(wc, "data.bin", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("binary root"), options())
        .unwrap();
    std::fs::write(&path, b"\0\x02\xff\nnew bytes").unwrap();
    let first = repo
        .record(wc, ChangeHeader::new("binary edit"), options())
        .expect("record binary edit");
    assert_eq!(
        repo.get_file_content("data.bin").unwrap().unwrap(),
        b"\0\x02\xff\nnew bytes"
    );
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
    std::fs::write(&path, b"\0\x03\xfe\nfinal").unwrap();
    let second = repo
        .record(wc, ChangeHeader::new("second binary edit"), options())
        .unwrap();
    assert!(second.change().dependencies().contains(first.hash()));
    assert_eq!(
        repo.get_file_content("data.bin").unwrap().unwrap(),
        b"\0\x03\xfe\nfinal"
    );
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"\0\x03\xfe\nfinal");
}

#[test]
fn native_text_binary_transitions_keep_content_and_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"first\nsecond").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("text root"), options())
        .unwrap();
    std::fs::write(&path, b"\0binary\xff").unwrap();
    repo.record(wc, ChangeHeader::new("to binary"), options())
        .unwrap();
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"\0binary\xff"
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            b""
        );
    }
    std::fs::write(&path, b"new\ntext").unwrap();
    repo.record(wc, ChangeHeader::new("back to text"), options())
        .unwrap();
    assert_eq!(
        repo.get_file_content("file.txt").unwrap().unwrap(),
        b"new\ntext"
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(
            atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "file.txt")
                .unwrap(),
            b"new\ntext"
        );
    }
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new\ntext");
}

#[test]
fn missing_span_root_refuses_without_overwriting_working_bytes() {
    use atomic_core::pristine::TreeTxnT;
    use redb::TableDefinition;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"recorded\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    repo.record(
        wc,
        ChangeHeader::new("root"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    let inode = repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_inode("file.txt")
        .unwrap()
        .unwrap();
    drop(repo);
    let db = redb::Database::open(dir.path().join(".atomic/atomic.redb")).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut roots = txn
            .open_table(TableDefinition::<u64, &[u8; 24]>::new(
                "span_patch_roots_v1",
            ))
            .unwrap();
        assert!(roots.remove(inode.get()).unwrap().is_some());
    }
    txn.commit().unwrap();
    drop(db);
    let repo = Repository::open(dir.path()).unwrap();
    let error = repo.get_file_content("file.txt").unwrap_err();
    assert!(error.to_string().contains("span root missing"), "{error}");
    std::fs::write(&path, b"user work\n").unwrap();
    let error = repo.materialize(wc).unwrap_err();
    assert!(error.to_string().contains("span root missing"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"user work\n");
}

#[test]
fn native_span_recording_keeps_sibling_views_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"alpha\nbeta\ngamma\nomega").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    let root = repo
        .record(wc, ChangeHeader::new("root"), options())
        .unwrap();
    repo.create_draft_view("left", "dev").unwrap();
    repo.create_draft_view("right", "dev").unwrap();
    repo.switch_view(wc, "left").unwrap();
    std::fs::write(&path, b"ALPHA\nbeta\ngamma\nomega").unwrap();
    let left = repo
        .record(wc, ChangeHeader::new("left"), options())
        .unwrap();
    repo.switch_view(wc, "right").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nbeta\ngamma\nomega");
    std::fs::write(&path, b"alpha\nbeta\nGAMMA\nomega").unwrap();
    let right = repo
        .record(wc, ChangeHeader::new("right"), options())
        .unwrap();
    assert!(left.change().dependencies().contains(root.hash()));
    assert!(right.change().dependencies().contains(root.hash()));
    assert!(!right.change().dependencies().contains(left.hash()));
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "left")
            .unwrap()
            .unwrap(),
        b"ALPHA\nbeta\ngamma\nomega"
    );
    assert_eq!(
        repo.get_file_content_on_view("file.txt", "right")
            .unwrap()
            .unwrap(),
        b"alpha\nbeta\nGAMMA\nomega"
    );
    repo.switch_view(wc, "left").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"ALPHA\nbeta\ngamma\nomega");
    assert!(repo
        .status(wc, StatusOptions::default())
        .unwrap()
        .is_clean());
}

#[cfg(unix)]
#[test]
fn native_span_recording_preserves_symlink_target() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let path = dir.path().join("link");
    symlink("missing-target", &path).unwrap();
    repo.add(wc, "link", Default::default()).unwrap();
    repo.record(
        wc,
        ChangeHeader::new("record link"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    assert_eq!(
        repo.get_file_content("link").unwrap().unwrap(),
        b"missing-target"
    );
    std::fs::remove_file(&path).unwrap();
    symlink("different-target", &path).unwrap();
    repo.record(
        wc,
        ChangeHeader::new("change link"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    assert_eq!(
        repo.get_file_content("link").unwrap().unwrap(),
        b"different-target"
    );
    std::fs::remove_file(&path).unwrap();
    repo.materialize(wc).unwrap();
    assert_eq!(
        std::fs::read_link(&path).unwrap(),
        std::path::Path::new("different-target")
    );
}
