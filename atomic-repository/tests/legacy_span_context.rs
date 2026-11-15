//! Forward legacy edits use verified original ranges, never old numeric CRDT
//! identities. All corruption and capability experiments are disposable.
use atomic_core::change::span_patch::SpanOp;
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::{CapabilityMutTxnT, CapabilityTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
use atomic_repository::{RecordOptions, Repository};

fn record(repo: &Repository) -> atomic_core::Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new("fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap()
        .hash()
}

fn fixture(initial: &[u8]) -> (tempfile::TempDir, Repository, atomic_core::Hash) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    std::fs::write(dir.path().join("f"), initial).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "f",
        Default::default(),
    )
    .unwrap();
    let root = record(&repo);
    assert!(repo.load_change(&root).unwrap().hashed.span.is_none());
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
    txn.commit().unwrap();
    (dir, repo, root)
}

#[test]
fn empty_legacy_file_and_repeated_projected_edits_replay_after_reopen() {
    let (dir, repo, root) = fixture(b"");
    let mut hashes = vec![root];
    for content in [b"first\n".as_slice(), b"second\n", b"", b"again\n"] {
        std::fs::write(dir.path().join("f"), content).unwrap();
        hashes.push(record(&repo));
        assert_eq!(repo.get_file_content("f").unwrap().unwrap(), content);
    }
    let receiver_dir = tempfile::tempdir().unwrap();
    let receiver = Repository::init(receiver_dir.path()).unwrap();
    for hash in &hashes {
        let bytes = std::fs::read(repo.change_store().change_path(hash)).unwrap();
        receiver.import_change_bytes(hash, &bytes).unwrap();
    }
    receiver
        .insert_change_rec(hashes.last().unwrap(), Default::default())
        .unwrap();
    drop(receiver);
    assert_eq!(
        Repository::open(receiver_dir.path())
            .unwrap()
            .get_file_content("f")
            .unwrap()
            .unwrap(),
        b"again\n"
    );
}

#[test]
fn legacy_projected_binary_edits_and_text_transitions_replay() {
    let (dir, source, root) = fixture(b"before\0\xff");
    let mut changes = vec![root];
    for bytes in [
        b"after\0\xfe".as_slice(),
        b"plain text\n",
        b"binary again\0\xff",
    ] {
        std::fs::write(dir.path().join("f"), bytes).unwrap();
        let edit = record(&source);
        let change = source.load_change(&edit).unwrap();
        assert!(change.file_ops().iter().any(|ops| matches!(
            ops.trunk_op(),
            Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { .. })
        )));
        assert_eq!(source.get_file_content("f").unwrap().unwrap(), bytes);
        changes.push(edit);

        // Check each transition independently from immutable objects, without
        // inheriting the author's CRDT caches or filesystem state.
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver = Repository::init(receiver_dir.path()).unwrap();
        for hash in &changes {
            let original = std::fs::read(source.change_store().change_path(hash)).unwrap();
            receiver.import_change_bytes(hash, &original).unwrap();
        }
        receiver
            .insert_change_rec(&edit, Default::default())
            .unwrap();
        assert_eq!(receiver.get_file_content("f").unwrap().unwrap(), bytes);
        receiver
            .materialize(receiver.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(std::fs::read(receiver_dir.path().join("f")).unwrap(), bytes);
    }
}

#[test]
fn materialize_legacy_span_edits_without_a_typed_root() {
    let (dir, source, root) = fixture(b"before\n");
    std::fs::write(dir.path().join("f"), b"after\n").unwrap();
    let edit = record(&source);
    for sequential in [false, true] {
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver = Repository::init(receiver_dir.path()).unwrap();
        for hash in [root, edit] {
            let bytes = std::fs::read(source.change_store().change_path(&hash)).unwrap();
            receiver.import_change_bytes(&hash, &bytes).unwrap();
        }
        receiver
            .insert_change_rec(&edit, Default::default())
            .unwrap();
        assert_eq!(receiver.get_file_content("f").unwrap().unwrap(), b"after\n");
        let wc = receiver.require_working_copy_id().unwrap();
        if sequential {
            receiver.materialize_sequential(wc).unwrap();
        } else {
            receiver.materialize(wc).unwrap();
        }
        assert_eq!(
            std::fs::read(receiver_dir.path().join("f")).unwrap(),
            b"after\n"
        );
        assert_eq!(receiver.materialize(wc).unwrap().files_written, 0);
    }
}

#[test]
fn malformed_legacy_context_refuses_before_publishing_dependencies() {
    let (dir, source, root) = fixture(b"one\ntwo\n");
    std::fs::write(dir.path().join("f"), b"ONE\ntwo\n").unwrap();
    let candidate = source.load_change(&record(&source)).unwrap();
    for mode in 0..5 {
        let mut bad: Change = candidate.clone();
        bad.hashed.header = ChangeHeader::new(format!("invalid context {mode}"));
        if mode == 0 {
            for op in &mut bad.hashed.span.as_mut().unwrap().ops {
                if let SpanOp::Edit { file, .. } = op {
                    file.ordinal = 999;
                }
            }
        } else if mode == 1 {
            if let SpanOp::Edit { target, .. } = &mut bad.hashed.span.as_mut().unwrap().ops[0] {
                target.owner = atomic_core::Hash::of(b"not an authored provider");
            }
        } else if mode == 2 {
            bad.hashed.span.as_mut().unwrap().version = 2;
        } else if mode == 3 {
            bad.hashed.file_ops.clear();
        } else {
            let ops = &bad.hashed.file_ops[0];
            let Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { file, .. }) = ops.trunk_op()
            else {
                panic!("fixture must use projected semantics");
            };
            bad.hashed.file_ops[0] = atomic_core::change::FileOps::projected_span_edit(
                ops.path().to_owned(),
                file.clone(),
                atomic_core::change::span_patch::SpanProjectionId(atomic_core::Hash::of(
                    b"wrong dependency baseline",
                )),
            );
        }
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver = Repository::init(receiver_dir.path()).unwrap();
        let bytes = std::fs::read(source.change_store().change_path(&root)).unwrap();
        receiver.import_change_bytes(&root, &bytes).unwrap();
        let before = receiver.view_set_id(receiver.current_view()).unwrap();
        match receiver.save_change(&bad) {
            Ok(hash) => assert!(
                receiver
                    .insert_change_rec(&hash, Default::default())
                    .is_err(),
                "accepted mode {mode}"
            ),
            Err(error) => assert!(
                error.to_string().contains("capability"),
                "unexpected refusal: {error}"
            ),
        }
        assert_eq!(
            receiver.view_set_id(receiver.current_view()).unwrap(),
            before
        );
    }
}

#[test]
fn legacy_reader_fence_is_present_after_projected_record_and_reopen() {
    let (dir, repo, _) = fixture(b"before\n");
    std::fs::write(dir.path().join("f"), b"after\n").unwrap();
    let hash = record(&repo);
    let patch = repo.load_change(&hash).unwrap();
    assert_eq!(
        patch.hashed.span.unwrap().version,
        atomic_core::change::span_patch::LEGACY_SOURCE_VERSION
    );
    drop(repo);
    let mut repo = Repository::open(dir.path()).unwrap();
    let txn = repo.pristine().read_txn().unwrap();
    for capability in [
        atomic_core::pristine::SPAN_LEGACY_CONTEXT_CAPABILITY,
        atomic_core::pristine::SPAN_PROJECTED_SEMANTICS_CAPABILITY,
    ] {
        assert_eq!(
            txn.required_capability_version(capability.id()).unwrap(),
            Some(1)
        );
    }
    assert!(matches!(
        atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), "f"),
        Err(atomic_core::output::crdt::CrdtOutputError::SpanContextRequired(_))
    ));
    drop(txn);
    repo.undo_operation(repo.require_working_copy_id().unwrap(), None)
        .unwrap();
    assert_eq!(repo.get_file_content("f").unwrap().unwrap(), b"before\n");
    assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"after\n");
    let txn = repo.pristine().read_txn().unwrap();
    assert_eq!(
        txn.required_capability_version(atomic_core::pristine::SPAN_LEGACY_CONTEXT_CAPABILITY.id())
            .unwrap(),
        Some(1)
    );
}

#[test]
fn legacy_projection_is_view_scoped_and_disjoint_edits_commute() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let (dir, mut repo, _) = fixture(b"alpha\nbeta\ngamma\n");
    let wc = repo.require_working_copy_id().unwrap();
    repo.create_draft_view("left", "dev").unwrap();
    repo.create_draft_view("right", "dev").unwrap();
    repo.switch_view(wc, "left").unwrap();
    std::fs::write(dir.path().join("f"), b"ALPHA\nbeta\ngamma\n").unwrap();
    let left = record(&repo);
    repo.switch_view(wc, "right").unwrap();
    assert_eq!(
        repo.get_file_content("f").unwrap().unwrap(),
        b"alpha\nbeta\ngamma\n"
    );
    std::fs::write(dir.path().join("f"), b"alpha\nbeta\nGAMMA\n").unwrap();
    let right = record(&repo);
    repo.insert_from_view(CrossViewInsertOptions::new("left", "right").only_changes(vec![left]))
        .unwrap();
    assert_eq!(
        repo.get_file_content_on_view("f", "left").unwrap().unwrap(),
        b"ALPHA\nbeta\ngamma\n"
    );
    assert_eq!(
        repo.get_file_content_on_view("f", "dev").unwrap().unwrap(),
        b"alpha\nbeta\ngamma\n"
    );
    repo.insert_from_view(CrossViewInsertOptions::new("right", "left").only_changes(vec![right]))
        .unwrap();
    assert_eq!(
        repo.view_set_id("left").unwrap(),
        repo.view_set_id("right").unwrap()
    );
    for view in ["left", "right"] {
        assert_eq!(
            repo.get_file_content_on_view("f", view).unwrap().unwrap(),
            b"ALPHA\nbeta\nGAMMA\n"
        );
    }
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("f", "dev").unwrap().unwrap(),
        b"alpha\nbeta\ngamma\n"
    );
}

#[test]
fn missing_legacy_indexes_or_capabilities_never_return_stale_graph_bytes() {
    use redb::ReadableMultimapTable;
    for mode in 0..3 {
        let (dir, repo, _) = fixture(b"before\n");
        std::fs::write(dir.path().join("f"), b"after\n").unwrap();
        record(&repo);
        // Deliberate corruption of this isolated fixture only. Supported
        // operations are not allowed to remove these immutable-object fences.
        let db = repo.pristine().shared_database().unwrap();
        let txn = db.begin_write().unwrap();
        if mode == 0 {
            let mut edits = txn
                .open_multimap_table(redb::MultimapTableDefinition::<&[u8; 32], &[u8; 40]>::new(
                    "span_patch_edits_v1",
                ))
                .unwrap();
            let keys = edits
                .iter()
                .unwrap()
                .map(|row| *row.unwrap().0.value())
                .collect::<Vec<_>>();
            for key in keys {
                drop(edits.remove_all(&key).unwrap());
            }
        } else {
            let capability = if mode == 1 {
                atomic_core::pristine::SPAN_LEGACY_CONTEXT_CAPABILITY
            } else {
                atomic_core::pristine::SPAN_PROJECTED_SEMANTICS_CAPABILITY
            };
            txn.open_table(atomic_core::pristine::tables::PRISTINE_META)
                .unwrap()
                .remove(format!("required-capability/{}", capability.id()).as_str())
                .unwrap();
        }
        txn.commit().unwrap();
        assert!(
            repo.get_file_content("f").is_err(),
            "returned stale bytes after corruption {mode}"
        );
        assert!(
            repo.materialize(repo.require_working_copy_id().unwrap())
                .is_err(),
            "materialized stale bytes after corruption {mode}"
        );
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"after\n");
    }
}

#[test]
fn overlapping_legacy_span_edits_refuse_without_publishing_membership() {
    use atomic_repository::apply::CrossViewInsertOptions;
    let (dir, mut repo, _) = fixture(b"before\n");
    let wc = repo.require_working_copy_id().unwrap();
    repo.create_draft_view("left", "dev").unwrap();
    repo.create_draft_view("right", "dev").unwrap();
    repo.switch_view(wc, "left").unwrap();
    std::fs::write(dir.path().join("f"), b"LEFT\n").unwrap();
    let left = record(&repo);
    repo.switch_view(wc, "right").unwrap();
    std::fs::write(dir.path().join("f"), b"RIGHT\n").unwrap();
    record(&repo);
    let before = repo.view_set_id("right").unwrap();
    assert!(repo
        .insert_from_view(CrossViewInsertOptions::new("left", "right").only_changes(vec![left]))
        .is_err());
    assert_eq!(repo.view_set_id("right").unwrap(), before);
    assert_eq!(
        repo.get_file_content_on_view("f", "right")
            .unwrap()
            .unwrap(),
        b"RIGHT\n"
    );
    assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"RIGHT\n");
}
