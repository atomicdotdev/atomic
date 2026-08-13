//! Integration tests for `Repository::split_view`.
//!
//! A split forks a new Draft view off a source view and removes a chosen set
//! of changes from the source. It is a pure metadata operation guarded by a
//! reverse-dependency safety check: a change cannot be removed from the source
//! while something remaining there still depends on it (unless `--cascade`).

use std::fs;
use std::path::Path;

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::pristine::ViewScope;
use atomic_core::types::{Base32, Hash};
use atomic_repository::history::HistoryOptions;
use atomic_repository::{RecordOptions, Repository, SplitOptions};
use tempfile::TempDir;

fn add_file(repo: &Repository, repo_path: &Path, name: &str, content: &str) {
    fs::write(repo_path.join(name), content).expect("write file");
    repo.add(name, Default::default()).expect("add file");
}

fn write_only(repo_path: &Path, name: &str, content: &str) {
    fs::write(repo_path.join(name), content).expect("write file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(header, RecordOptions::default())
        .expect("record")
        .hash()
}

fn log_hashes(repo: &Repository, view: &str) -> Vec<Hash> {
    repo.log(HistoryOptions::default().view(view))
        .expect("log")
        .into_iter()
        .map(|e| e.hash)
        .collect()
}

/// Splitting independent changes out of the middle of a view is clean: the
/// source keeps the survivors, the draft owns the extracted changes.
#[test]
fn split_independent_middle_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");
    let source = repo.current_view().to_string();

    // Four independent changes (separate files).
    add_file(&repo, &repo_path, "a.txt", "A\n");
    let a = record(&repo, "add a");
    add_file(&repo, &repo_path, "b.txt", "B\n");
    let b = record(&repo, "add b");
    add_file(&repo, &repo_path, "c.txt", "C\n");
    let c = record(&repo, "add c");
    add_file(&repo, &repo_path, "d.txt", "D\n");
    let d = record(&repo, "add d");

    assert_eq!(log_hashes(&repo, &source), vec![a, b, c, d]);

    // Split out the middle two.
    let outcome = repo
        .split_view(SplitOptions::new("wip", vec![b, c]))
        .expect("split");

    assert!(!outcome.blocked);
    assert!(outcome.dependents.is_empty(), "no dependents expected");
    assert_eq!(outcome.moved.len(), 2);
    assert_eq!(outcome.source_change_count, 2);
    assert_eq!(outcome.target_change_count, 2);

    // Source keeps the survivors in order.
    assert_eq!(log_hashes(&repo, &source), vec![a, d]);

    // Draft owns exactly the extracted changes.
    assert_eq!(log_hashes(&repo, "wip"), vec![b, c]);

    // Draft is a Draft parented on the source, and sees the full pre-split
    // state through inheritance.
    let info = repo.get_view_info("wip").expect("view info");
    assert_eq!(info.scope, ViewScope::Draft);
    assert_eq!(info.parent_name.as_deref(), Some(source.as_str()));

    let effective: Vec<Hash> = repo
        .effective_history(Some("wip"))
        .expect("effective history")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    let mut all = effective.clone();
    all.sort();
    let mut expected = vec![a, b, c, d];
    expected.sort();
    assert_eq!(all, expected, "draft should see all four changes");
}

/// A change that a remaining change depends on cannot be split out without
/// `--cascade`; the operation is refused and nothing is mutated.
#[test]
fn split_blocked_by_dependent() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");
    let source = repo.current_view().to_string();

    // c2 edits the line c1 introduced, so c2 depends on c1.
    add_file(&repo, &repo_path, "f.txt", "hello\n");
    let c1 = record(&repo, "create f");
    write_only(&repo_path, "f.txt", "HELLO\n");
    let c2 = record(&repo, "edit f");

    assert_eq!(log_hashes(&repo, &source), vec![c1, c2]);

    // Dry run reports the block without mutating.
    let preview = repo
        .split_view(SplitOptions {
            target_view: "wip".to_string(),
            from_view: None,
            changes: vec![c1],
            cascade: false,
            dry_run: true,
            materialize: false,
        })
        .expect("dry run ok");
    assert!(preview.blocked, "dry run should report blocked");
    assert_eq!(preview.dependents.len(), 1);
    assert_eq!(preview.dependents[0].hash, c2);
    assert!(preview.moved.is_empty());

    // Real attempt errors.
    let err = repo
        .split_view(SplitOptions::new("wip", vec![c1]))
        .expect_err("should be blocked");
    match err {
        atomic_repository::RepositoryError::ViewSplitHasDependents { blocking, .. } => {
            assert_eq!(blocking, vec![c2.to_base32()]);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Nothing was created or removed.
    assert!(!repo.view_exists("wip").expect("view_exists"));
    assert_eq!(log_hashes(&repo, &source), vec![c1, c2]);
}

/// With `--cascade`, the dependent is moved along with the requested change.
#[test]
fn split_cascade_moves_dependents() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");
    let source = repo.current_view().to_string();

    add_file(&repo, &repo_path, "f.txt", "hello\n");
    let c1 = record(&repo, "create f");
    write_only(&repo_path, "f.txt", "HELLO\n");
    let c2 = record(&repo, "edit f");

    let outcome = repo
        .split_view(SplitOptions {
            target_view: "wip".to_string(),
            from_view: Some(source.clone()),
            changes: vec![c1],
            cascade: true,
            dry_run: false,
            materialize: false,
        })
        .expect("cascade split");

    assert!(!outcome.blocked);
    assert_eq!(outcome.requested.len(), 1);
    assert_eq!(outcome.dependents.len(), 1);
    assert_eq!(outcome.dependents[0].hash, c2);
    assert_eq!(outcome.moved.len(), 2);

    // Both changes moved into the draft; source is now empty.
    assert_eq!(log_hashes(&repo, &source), Vec::<Hash>::new());
    assert_eq!(log_hashes(&repo, "wip"), vec![c1, c2]);
}

/// With `materialize`, splitting out of the current view reconciles the working
/// copy: a file whose only change left is deleted, and a reverted file's
/// content is rewritten to the source's new state.
#[test]
fn split_materialize_reconciles_working_copy() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");
    let source = repo.current_view().to_string();

    // g.txt is created then edited: create g (v1) <- edit g (v2). Independent
    // file h.txt is created once.
    add_file(&repo, &repo_path, "g.txt", "v1\n");
    let _cg = record(&repo, "create g");
    write_only(&repo_path, "g.txt", "v2\n");
    let edit_g = record(&repo, "edit g");
    add_file(&repo, &repo_path, "h.txt", "H\n");
    let create_h = record(&repo, "create h");

    // Split out `edit g` (revert g.txt) and `create h` (delete h.txt).
    let outcome = repo
        .split_view(SplitOptions {
            target_view: "wip".to_string(),
            from_view: Some(source.clone()),
            changes: vec![edit_g, create_h],
            cascade: false,
            dry_run: false,
            materialize: true,
        })
        .expect("materialize split");

    assert!(outcome.working_copy_updated);
    assert_eq!(outcome.files_removed, 1, "h.txt removed");
    assert_eq!(outcome.files_written, 1, "g.txt reverted");

    // h.txt is gone; g.txt reverted to v1.
    assert!(!repo_path.join("h.txt").exists(), "h.txt should be deleted");
    assert_eq!(
        fs::read_to_string(repo_path.join("g.txt")).expect("read g"),
        "v1\n",
        "g.txt should be reverted to the pre-edit content"
    );
}

/// Requesting a change that isn't in the source view is a clear error.
#[test]
fn split_rejects_change_not_in_view() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "a.txt", "A\n");
    let _a = record(&repo, "add a");

    // A hash that was never recorded.
    let phantom = Hash::of(b"nope");
    let err = repo
        .split_view(SplitOptions::new("wip", vec![phantom]))
        .expect_err("should reject unknown change");
    assert!(matches!(
        err,
        atomic_repository::RepositoryError::ChangeNotFound { .. }
    ));
}
