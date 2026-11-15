//! Identity/record integration must preserve the compact span graph, not merely
//! render correct bytes while expanding it back to linewise graph operations.
use atomic_core::change::{ChangeHeader, GraphOp};
use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
use atomic_repository::{RecordOptions, Repository};
use redb::{ReadableDatabase, ReadableTableMetadata};
use std::path::Path;

fn record(repo: &Repository, message: &str) -> atomic_core::Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new(message),
            RecordOptions::new()
                .paths(vec!["large.txt"])
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap()
        .hash()
}

fn graph_rows(repo: &Repository) -> (u64, u64) {
    let db = repo.pristine().shared_database().unwrap();
    let txn = db.begin_read().unwrap();
    (
        txn.open_multimap_table(atomic_core::pristine::tables::GRAPH)
            .unwrap()
            .len()
            .unwrap(),
        txn.open_multimap_table(atomic_core::pristine::tables::INODE_GRAPH)
            .unwrap()
            .len()
            .unwrap(),
    )
}

fn semantic_rows(repo: &Repository) -> (u64, u64, u64) {
    let db = repo.pristine().shared_database().unwrap();
    let txn = db.begin_read().unwrap();
    (
        txn.open_table(atomic_core::crdt::tables::TRUNKS)
            .unwrap()
            .len()
            .unwrap(),
        txn.open_table(atomic_core::crdt::tables::BRANCHES)
            .unwrap()
            .len()
            .unwrap(),
        txn.open_table(atomic_core::crdt::tables::LEAVES)
            .unwrap()
            .len()
            .unwrap(),
    )
}

// Raw compaction is confined to this disposable fixture, after the supported
// repository handle and all transactions have closed. Never a live migration.
fn compacted_bytes(root: &Path) -> u64 {
    let path = root.join(".atomic/atomic.redb");
    let mut db = redb::Database::open(&path).unwrap();
    db.compact().unwrap();
    drop(db);
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn small_native_edits_keep_span_graph_rows_constant_and_database_growth_bounded() {
    storage_guard(false);
}

#[test]
fn legacy_forward_edits_keep_authored_ranges_and_bounded_storage() {
    storage_guard(true);
}

fn storage_guard(legacy: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    if !legacy {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let mut lines: Vec<_> = (0..2048)
        .map(|i| format!("line {i:04}: stable payload\n"))
        .collect();
    let initial_rows = graph_rows(&repo);
    let original = lines.concat().into_bytes();
    std::fs::write(dir.path().join("large.txt"), &original).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "large.txt",
        Default::default(),
    )
    .unwrap();
    let root = record(&repo, "span root");
    let root_change = repo.load_change(&root).unwrap();
    if legacy {
        assert!(root_change.hashed.span.is_none());
    } else {
        assert_eq!(root_change.hashed.span.as_ref().unwrap().ops.len(), 1);
    }
    let before_rows = graph_rows(&repo);
    let before_semantic_rows = semantic_rows(&repo);
    assert!(
        legacy || (before_rows.0 - initial_rows.0 <= 8 && before_rows.1 - initial_rows.1 <= 8),
        "the span root expanded into per-line graph rows"
    );
    assert!(
        legacy
            || !root_change.hunks().iter().any(|op| matches!(
                op,
                GraphOp::Edit { .. }
                    | GraphOp::Replacement { .. }
                    | GraphOp::FileAdd {
                        contents: Some(_),
                        ..
                    }
            )),
        "a span root must not keep a duplicate linewise content graph"
    );
    drop(repo);
    let before_bytes = compacted_bytes(dir.path());

    let repo = Repository::open(dir.path()).unwrap();
    if legacy {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    let mut recorded = vec![root];
    for step in 0..8 {
        let line = 128 + step * 200;
        lines[line] = format!("line {line:04}: changed {step}\n");
        std::fs::write(dir.path().join("large.txt"), lines.concat()).unwrap();
        let hash = record(&repo, "small span edit");
        let change = repo.load_change(&hash).unwrap();
        recorded.push(hash);
        let span = change
            .hashed
            .span
            .as_ref()
            .expect("must not fall back to a linewise change");
        assert!(
            (1..=if legacy { 2 } else { 1 }).contains(&span.ops.len()),
            "one edited line must address only its original authored ranges"
        );
        assert!(
            change.contents.len() < 256,
            "record copied the full file into an edit"
        );
        assert!(
            !change
                .hunks()
                .iter()
                .any(|op| matches!(op, GraphOp::Edit { .. } | GraphOp::Replacement { .. })),
            "content edits must stay in the span algebra, not linewise graph hunks"
        );
        assert_eq!(
            graph_rows(&repo),
            before_rows,
            "span edits expanded GRAPH/INODE_GRAPH"
        );
        if legacy {
            assert_eq!(
                span.version,
                atomic_core::change::span_patch::LEGACY_SOURCE_VERSION
            );
            assert!(change.file_ops().iter().any(|ops| matches!(
                ops.trunk_op(),
                Some(atomic_core::crdt::TrunkOp::ProjectedSpanEdit { .. })
            )));
            assert_eq!(
                semantic_rows(&repo),
                before_semantic_rows,
                "forward projection must not fabricate historical CRDT cache rows"
            );
        }
    }
    let expected = lines.concat().into_bytes();
    // Replay only original immutable encodings, not the source's cache rows.
    let destination = tempfile::tempdir().unwrap();
    let receiver = Repository::init(destination.path()).unwrap();
    for hash in &recorded {
        let bytes = std::fs::read(repo.change_store().change_path(hash)).unwrap();
        receiver.import_change_bytes(hash, &bytes).unwrap();
        assert_eq!(
            std::fs::read(receiver.change_store().change_path(hash)).unwrap(),
            bytes
        );
    }
    receiver
        .insert_change_rec(recorded.last().unwrap(), Default::default())
        .unwrap();
    assert_eq!(
        receiver.get_file_content("large.txt").unwrap().unwrap(),
        expected
    );
    assert_eq!(graph_rows(&receiver), before_rows);
    if legacy {
        use atomic_core::pristine::span_patch_storage::SpanRenderContext;
        use atomic_core::pristine::{CachedGraphTxn, ViewTxnT};
        let (inode, _) = repo.get_inode_and_position("large.txt").unwrap().unwrap();
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view(repo.current_view()).unwrap().unwrap();
        let closure =
            atomic_repository::repository::effective_projection_closure(&txn, &view).unwrap();
        let cached = CachedGraphTxn::new(&txn).unwrap();
        let context = SpanRenderContext::new(&cached, repo.change_store(), &closure).unwrap();
        let state = context.record_state(inode).unwrap().unwrap();
        assert_eq!(state.spans.first().unwrap().target.owner, root);
        assert!(matches!(context.render_bounded(inode, 31).unwrap(),
            atomic_core::pristine::span_patch_storage::BoundedSpanContent::Oversized { retained_bytes } if retained_bytes <= 31));
    }
    assert_eq!(
        repo.get_file_content_on_view("large.txt", repo.current_view())
            .unwrap()
            .unwrap(),
        expected
    );
    {
        let db = repo.pristine().shared_database().unwrap();
        let txn = db.begin_read().unwrap();
        match txn.open_table(atomic_core::pristine::tables::CONTENT_CHUNKS) {
            Ok(chunks) => assert_eq!(
                chunks.len().unwrap(),
                0,
                "ordinary span recording must keep content in change objects"
            ),
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => panic!("could not inspect optional content cache: {error}"),
        }
        if legacy {
            let files = txn
                .open_table(redb::TableDefinition::<&[u8; 36], u64>::new(
                    "span_patch_files_v1",
                ))
                .unwrap()
                .len()
                .unwrap();
            let nodes = txn
                .open_table(redb::TableDefinition::<&[u8; 32], u8>::new(
                    "span_patch_nodes_v1",
                ))
                .unwrap()
                .len()
                .unwrap();
            let edits = txn
                .open_multimap_table(redb::MultimapTableDefinition::<&[u8; 32], &[u8; 40]>::new(
                    "span_patch_edits_v1",
                ))
                .unwrap()
                .len()
                .unwrap();
            assert_eq!(files, 1, "legacy context needs one portable file binding");
            assert!(
                nodes <= 16 && edits <= 16,
                "small edits expanded span indexes: {nodes} nodes / {edits} edits"
            );
            eprintln!("legacy span indexes: {files} files / {nodes} nodes / {edits} edits; retained CRDT rows {before_semantic_rows:?}");
        }
    }
    drop(repo);
    let after_bytes = compacted_bytes(dir.path());
    eprintln!("span storage: compacted db {before_bytes} -> {after_bytes} bytes; GRAPH/INODE_GRAPH rows {before_rows:?}");
    assert!(
        after_bytes <= before_bytes + 512 * 1024,
        "eight small edits lost the bounded storage benefit"
    );
    let reopened = Repository::open(dir.path()).unwrap();
    assert_eq!(graph_rows(&reopened), before_rows);
    assert_eq!(
        reopened
            .get_file_content_on_view("large.txt", reopened.current_view())
            .unwrap()
            .unwrap(),
        expected
    );
}
