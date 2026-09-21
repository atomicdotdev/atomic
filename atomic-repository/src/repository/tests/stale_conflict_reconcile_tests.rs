//! Tests for scoped, journaled cleanup of stale persisted conflict rows.
//!
//! Every test runs on a disposable `tempfile` repository. The scenario is a
//! path whose persisted `CONFLICTS` row no longer describes a real graph
//! conflict: the canonical render emits no conflict and equals the working-tree
//! bytes, but no materialize/record has refreshed the row, so `record` is a
//! permanent no-op for it.

use super::*;
use crate::record::{RecordError, RecordOptions};
use atomic_core::change::ChangeHeader;
use atomic_core::pristine::{MutTxnT, StoredConflict, StoredConflictKind, TreeTxnT, ViewTxnT};

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(repo.require_working_copy_id().unwrap(), header, options)
}

fn persisted_conflict_count(repo: &TestRepository, view: &str) -> usize {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(view).unwrap().unwrap();
    txn.iter_conflicts(view.id).unwrap().len()
}

fn inject_order_conflict(repo: &TestRepository, path: &str, line: u32) {
    let view_name = repo.desired_view_name(repo.working_copy()).unwrap();
    let (view_id, inode) = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view(&view_name).unwrap().unwrap();
        let inode = txn.get_inode(path).unwrap().unwrap();
        (view.id, inode)
    };
    let mut txn = repo.pristine.write_txn().unwrap();
    txn.put_conflicts(
        view_id,
        inode.get(),
        &[StoredConflict {
            kind: StoredConflictKind::Order,
            path: path.to_string(),
            line: Some(line),
            sides: Vec::new(),
        }],
    )
    .unwrap();
    txn.commit().unwrap();
}

/// A marker-shaped fixture is legitimate content; recording it with the
/// explicit allow must not persist a conflict. Then a stale order row is
/// injected to reproduce the live state.
fn marker_fixture_repo() -> (TempDir, TestRepository, std::path::PathBuf, String) {
    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("fixture.md");
    let fixture = [
        "Conflict marker example:",
        ">>>>>>> 1",
        "left side",
        "======= 1",
        "right side",
        "<<<<<<< 1",
        "",
    ]
    .join("\n");
    std::fs::write(&file, fixture.as_bytes()).unwrap();
    repo.add("fixture.md", TrackingOptions::default()).unwrap();
    let header = ChangeHeader::new("add marker-shaped fixture");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true)
        .allow_conflict_markers(true);
    repo.record(header, options).unwrap();
    repo.materialize().unwrap();
    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        0,
        "precondition: marker-shaped content is not a persisted conflict"
    );
    (temp_dir, repo, file, fixture)
}

#[test]
fn stale_order_conflict_is_cleared_by_record() {
    let (_temp_dir, repo, file, fixture) = marker_fixture_repo();

    inject_order_conflict(&repo, "fixture.md", 1);
    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        1,
        "precondition: the injected stale row is present"
    );

    // The scoped explicit allow must clear the stale metadata as a successful
    // record call, not return NothingToRecord.
    let header = ChangeHeader::new("cleanup stale conflict");
    let options = RecordOptions::new()
        .paths(vec!["fixture.md"])
        .save_to_store(true)
        .apply_after_record(true)
        .allow_conflict_markers(true);
    let outcome = repo
        .record(header, options)
        .expect("scoped cleanup record must succeed");

    let cleanup = outcome
        .conflict_cleanup()
        .expect("outcome reports the conflict cleanup");
    assert_eq!(cleanup.paths, vec!["fixture.md".to_string()]);
    assert_eq!(cleanup.rows_cleared, 1);
    assert!(cleanup.operation.is_some(), "cleanup is journaled");
    assert!(
        outcome.recorded_files().is_empty(),
        "a metadata cleanup records no content change"
    );

    assert_eq!(persisted_conflict_count(&repo, "dev"), 0);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        fixture.as_bytes(),
        "the fixture bytes must be preserved verbatim"
    );
}

#[test]
fn stale_cleanup_is_idempotent() {
    let (_temp_dir, repo, _file, _fixture) = marker_fixture_repo();
    inject_order_conflict(&repo, "fixture.md", 1);

    let paths = vec!["fixture.md".to_string()];
    let first = repo
        .reconcile_stale_conflicts(repo.working_copy(), &paths)
        .unwrap();
    assert!(!first.is_noop());
    assert_eq!(first.cleared_rows, 1);
    assert!(first.operation.is_some());
    assert_eq!(persisted_conflict_count(&repo, "dev"), 0);

    // Repeating after success is a read-only no-op with no new operation.
    let second = repo
        .reconcile_stale_conflicts(repo.working_copy(), &paths)
        .unwrap();
    assert!(second.is_noop());
    assert!(second.operation.is_none());
    assert!(second.cleared_paths.is_empty());

    let report = repo
        .inspect_stale_conflicts(repo.working_copy(), &paths)
        .unwrap();
    assert!(!report.has_stale());
    assert_eq!(
        report.paths[0].disposition,
        StaleConflictDisposition::AlreadyClean
    );
}

#[test]
fn genuine_graph_conflict_is_refused_and_preserved() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file = temp_dir.path().join("f.txt");
    std::fs::write(&file, "line1\nline2\nline3\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "line1\nAAA\nline2\nline3\n").unwrap();
    record_all(&repo, "edit A").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "line1\nBBB\nline2\nline3\n").unwrap();
    record_all(&repo, "edit B").unwrap();

    repo.insert_from_view(crate::apply::CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let before = persisted_conflict_count(&repo, "dev");
    assert!(before >= 1, "precondition: a real conflict is persisted");

    let err = repo
        .reconcile_stale_conflicts(repo.working_copy(), &["f.txt".to_string()])
        .expect_err("a genuine graph conflict must be refused");
    match err {
        RepositoryError::InvalidOperation { message } => {
            assert!(message.contains("genuine graph conflict"), "got: {message}");
        }
        other => panic!("expected InvalidOperation, got {other:?}"),
    }
    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        before,
        "the genuine conflict rows are untouched"
    );

    // `record --allow-conflict-markers` for that path refuses too, so
    // unresolved markers are never baked into history.
    let header = ChangeHeader::new("attempt to bake conflict");
    let options = RecordOptions::new()
        .paths(vec!["f.txt"])
        .save_to_store(true)
        .apply_after_record(true)
        .allow_conflict_markers(true);
    let err = repo.record(header, options).unwrap_err();
    assert!(matches!(
        err,
        RecordError::Repository(RepositoryError::InvalidOperation { .. })
    ));
    assert_eq!(persisted_conflict_count(&repo, "dev"), before);
}

#[test]
fn diverged_content_is_reported_and_skipped() {
    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("a.txt");
    std::fs::write(&file, "alpha\n").unwrap();
    repo.add("a.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    repo.materialize().unwrap();

    // A pending edit: the render still says "alpha", the disk says "beta".
    std::fs::write(&file, "beta\n").unwrap();
    inject_order_conflict(&repo, "a.txt", 1);

    let report = repo
        .inspect_stale_conflicts(repo.working_copy(), &["a.txt".to_string()])
        .unwrap();
    assert_eq!(
        report.paths[0].disposition,
        StaleConflictDisposition::ContentDiverged
    );
    assert!(!report.has_stale());

    // The cleanup never fires for a pending resolution the record path owns.
    let outcome = repo
        .reconcile_stale_conflicts(repo.working_copy(), &["a.txt".to_string()])
        .unwrap();
    assert!(outcome.is_noop());
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(persisted_conflict_count(&repo, "dev"), 1);
}

#[test]
fn cleanup_touches_only_explicit_paths() {
    let (temp_dir, repo) = create_temp_repo();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(temp_dir.path().join(name), format!("{name}\n")).unwrap();
        repo.add(name, TrackingOptions::default()).unwrap();
    }
    record_all(&repo, "base").unwrap();
    repo.materialize().unwrap();

    inject_order_conflict(&repo, "a.txt", 1);
    inject_order_conflict(&repo, "b.txt", 1);
    assert_eq!(persisted_conflict_count(&repo, "dev"), 2);

    let outcome = repo
        .reconcile_stale_conflicts(repo.working_copy(), &["a.txt".to_string()])
        .unwrap();
    assert_eq!(outcome.cleared_paths, vec!["a.txt".to_string()]);
    assert_eq!(outcome.cleared_rows, 1);

    // b.txt is untouched: still exactly its one row.
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view("dev").unwrap().unwrap();
    let b_inode = txn.get_inode("b.txt").unwrap().unwrap();
    assert_eq!(
        txn.get_conflicts(view.id, b_inode.get()).unwrap().len(),
        1,
        "an unrelated explicit path is not cleared"
    );
}

#[test]
fn cleanup_operation_is_journaled_as_repair_with_evidence() {
    let (_temp_dir, repo, _file, _fixture) = marker_fixture_repo();
    inject_order_conflict(&repo, "fixture.md", 1);

    let outcome = repo
        .reconcile_stale_conflicts(repo.working_copy(), &["fixture.md".to_string()])
        .unwrap();
    let operation_id = outcome.operation.expect("journaled operation");

    let details = repo.operation_details(operation_id).unwrap();
    assert_eq!(
        details.operation.payload().kind,
        atomic_core::operation::OperationKind::Repair
    );
    assert_eq!(
        details.operation.payload().evidence.len(),
        2,
        "before/after snapshot digests are recorded"
    );
    assert_eq!(
        details.operation.payload().actor,
        atomic_core::operation::ActorRef::System {
            name: "repository-stale-conflict-reconcile".to_string()
        }
    );
}

/// The narrow metadata-only route clears a verified-stale row without any
/// content record and is a no-op once the row is gone.
#[test]
fn metadata_only_cleanup_clears_stale_and_then_noops() {
    let (_temp_dir, repo, file, fixture) = marker_fixture_repo();
    inject_order_conflict(&repo, "fixture.md", 1);

    let outcome = repo
        .record_metadata_only_conflict_cleanup(repo.working_copy(), &["fixture.md".to_string()])
        .unwrap()
        .expect("a proven metadata-only plan clears the stale row");
    assert_eq!(outcome.cleared_paths, vec!["fixture.md".to_string()]);
    assert_eq!(outcome.cleared_rows, 1);
    assert!(outcome.operation.is_some(), "cleanup is journaled");
    assert_eq!(persisted_conflict_count(&repo, "dev"), 0);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        fixture.as_bytes(),
        "the fixture bytes must be preserved verbatim"
    );

    // Once cleared there is no metadata-only effect left; the ordinary guard
    // owns the request from here.
    let again = repo
        .record_metadata_only_conflict_cleanup(repo.working_copy(), &["fixture.md".to_string()])
        .unwrap();
    assert!(again.is_none());
}

/// A mixed scope (one stale path plus one path with a pending content delta)
/// is not a metadata-only plan: the route clears nothing.
#[test]
fn metadata_only_cleanup_refuses_mixed_content_scope() {
    let (temp_dir, repo) = create_temp_repo();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(temp_dir.path().join(name), format!("{name}\n")).unwrap();
        repo.add(name, TrackingOptions::default()).unwrap();
    }
    record_all(&repo, "base").unwrap();
    repo.materialize().unwrap();

    inject_order_conflict(&repo, "a.txt", 1);
    // b.txt has a pending edit the ordinary record path owns.
    std::fs::write(temp_dir.path().join("b.txt"), "beta\n").unwrap();

    let result = repo
        .record_metadata_only_conflict_cleanup(
            repo.working_copy(),
            &["a.txt".to_string(), "b.txt".to_string()],
        )
        .unwrap();
    assert!(result.is_none(), "a mixed scope is not metadata-only");
    assert_eq!(
        persisted_conflict_count(&repo, "dev"),
        1,
        "the stale row is untouched when the scope is not proven metadata-only"
    );
}

/// An untracked named path is not a metadata-only target: the route clears
/// nothing.
#[test]
fn metadata_only_cleanup_refuses_untracked_path() {
    let (temp_dir, repo) = create_temp_repo();
    std::fs::write(temp_dir.path().join("a.txt"), "alpha\n").unwrap();
    repo.add("a.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    repo.materialize().unwrap();

    inject_order_conflict(&repo, "a.txt", 1);
    std::fs::write(temp_dir.path().join("untracked.txt"), "new\n").unwrap();

    let result = repo
        .record_metadata_only_conflict_cleanup(
            repo.working_copy(),
            &["a.txt".to_string(), "untracked.txt".to_string()],
        )
        .unwrap();
    assert!(result.is_none(), "an untracked path is not metadata-only");
    assert_eq!(persisted_conflict_count(&repo, "dev"), 1);
}
