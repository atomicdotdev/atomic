//! Strong end-to-end evidence for the CB-N6 merge-rubric requirements.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use atomic_core::change::{ChangeHeader, GraphOp};
use atomic_core::WorkingCopyId;
use atomic_repository::{
    FileStatus, InsertOptions, RecordOptions, RecordOutcome, Repository, StatusOptions,
};
use tempfile::TempDir;

fn working_copy(repo: &Repository) -> WorkingCopyId {
    repo.require_working_copy_id().expect("working copy id")
}

fn record_all(repo: &Repository, message: &str) -> RecordOutcome {
    repo.record(
        working_copy(repo),
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap_or_else(|error| panic!("record '{message}': {error}"))
}

fn conflicted_paths(repo: &Repository) -> BTreeSet<String> {
    repo.status(working_copy(repo), StatusOptions::default())
        .expect("status")
        .entries()
        .iter()
        .filter(|entry| entry.status() == FileStatus::Conflicted)
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect()
}

fn listed_conflict_paths(repo: &Repository) -> BTreeSet<String> {
    repo.list_conflicts(working_copy(repo))
        .expect("list conflicts")
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

fn remove_if_present(path: &Path) {
    if path.exists() {
        fs::remove_file(path).expect("remove materialized file");
    }
}

fn assert_same_path_conflict(repo: &Repository, path: &Path) -> Vec<u8> {
    let body = fs::read(path).expect("read same-path conflict");
    let text = String::from_utf8(body.clone()).expect("text conflict fixture");
    assert!(text.contains(">>>>>>>"), "missing conflict marker:\n{text}");
    assert_eq!(text.matches("from-left\n").count(), 1, "{text}");
    assert_eq!(text.matches("from-right\n").count(), 1, "{text}");
    assert!(
        repo.get_file_content("same.txt").is_err(),
        "content lookup must not choose a path claimant"
    );
    assert!(conflicted_paths(repo).contains("same.txt"));
    assert!(listed_conflict_paths(repo).contains("same.txt"));
    assert!(
        !repo
            .visible_file_paths(repo.current_view())
            .expect("checkpoint-visible regular files")
            .contains("same.txt"),
        "provisional manifest enumeration must omit typed conflicts rather than choose a side"
    );
    body
}

fn rename_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    ["left.txt", "right.txt"]
        .into_iter()
        .map(|name| {
            let body = fs::read(root.join(name))
                .unwrap_or_else(|error| panic!("read retained rename path '{name}': {error}"));
            (name.to_string(), body)
        })
        .collect()
}

fn assert_rename_conflict(repo: &Repository, root: &Path) -> BTreeMap<String, Vec<u8>> {
    let snapshot = rename_snapshot(root);
    assert_eq!(snapshot["left.txt"], b"shared\n");
    assert_eq!(snapshot["right.txt"], b"shared\n");

    let status = conflicted_paths(repo);
    assert!(status.contains("left.txt"), "status paths: {status:?}");
    assert!(status.contains("right.txt"), "status paths: {status:?}");
    let listed = listed_conflict_paths(repo);
    assert!(listed.contains("left.txt"), "listed paths: {listed:?}");
    assert!(listed.contains("right.txt"), "listed paths: {listed:?}");
    assert!(repo.get_file_content("left.txt").is_err());
    assert!(repo.get_file_content("right.txt").is_err());
    snapshot
}

#[test]
fn a12_resolution_is_explicit_order_independent_and_reopen_stable() {
    let temp = TempDir::new().expect("temp repo");
    let root = temp.path();
    let same = root.join("same.txt");
    let mut repo = Repository::init(root).expect("init");

    fs::write(root.join("seed.txt"), b"seed\n").expect("write seed");
    repo.add(working_copy(&repo), "seed.txt", Default::default())
        .expect("add seed");
    record_all(&repo, "base");
    for view in ["left", "right", "merge-lr", "merge-rl"] {
        repo.create_view_from(view, "dev")
            .unwrap_or_else(|error| panic!("create view '{view}': {error}"));
    }

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch left");
    fs::write(&same, b"from-left\n").expect("write left claim");
    repo.add(working_copy(&repo), "same.txt", Default::default())
        .expect("add left claim");
    let left = record_all(&repo, "left creates same path");

    repo.switch_view(working_copy(&repo), "right")
        .expect("switch right");
    fs::write(&same, b"from-right\n").expect("write right claim");
    repo.add(working_copy(&repo), "same.txt", Default::default())
        .expect("add right claim");
    let right = record_all(&repo, "right creates same path");

    repo.switch_view(working_copy(&repo), "merge-lr")
        .expect("switch merge-lr");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right");
    repo.materialize_sequential(working_copy(&repo))
        .expect("sequential materialize");
    let sequential = assert_same_path_conflict(&repo, &same);
    remove_if_present(&same);
    repo.materialize(working_copy(&repo))
        .expect("parallel materialize");
    assert_eq!(
        fs::read(&same).expect("read parallel conflict"),
        sequential,
        "sequential and parallel conflict rendering must be byte-identical"
    );

    repo.switch_view(working_copy(&repo), "merge-rl")
        .expect("switch merge-rl");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right first");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left second");
    repo.materialize(working_copy(&repo))
        .expect("materialize reverse order");
    assert_same_path_conflict(&repo, &same);

    repo.switch_view(working_copy(&repo), "merge-lr")
        .expect("return to merge-lr");
    repo.create_view_from("unresolved-sibling", "merge-lr")
        .expect("fork unresolved sibling");

    fs::write(&same, b"from-left\n").expect("choose exact left claimant");
    assert!(
        conflicted_paths(&repo).contains("same.txt"),
        "public status must remain typed-conflicted until SolveNameConflict commits"
    );
    let resolution = record_all(&repo, "resolve same-path claim");
    let solves: Vec<_> = resolution
        .change()
        .hunks()
        .iter()
        .filter_map(|operation| match operation {
            GraphOp::SolveNameConflict { name, path } => Some((name, path)),
            _ => None,
        })
        .collect();
    assert_eq!(
        solves.len(),
        1,
        "resolution hunks: {:?}",
        resolution.change().hunks()
    );
    assert_eq!(solves[0].1, "same.txt");
    assert!(
        !solves[0].0.edges.is_empty(),
        "SolveNameConflict must carry real loser transitions"
    );
    assert!(resolution.change().dependencies().contains(left.hash()));
    assert!(resolution.change().dependencies().contains(right.hash()));

    repo.materialize_sequential(working_copy(&repo))
        .expect("materialize resolution");
    assert_eq!(fs::read(&same).expect("read resolution"), b"from-left\n");
    assert!(!conflicted_paths(&repo).contains("same.txt"));
    assert!(!listed_conflict_paths(&repo).contains("same.txt"));
    remove_if_present(&same);
    repo.materialize(working_copy(&repo))
        .expect("rematerialize resolution");
    assert_eq!(
        fs::read(&same).expect("read rematerialized resolution"),
        b"from-left\n"
    );

    drop(repo);

    let reopened = Repository::open(root).expect("reopen repository");
    reopened
        .materialize(working_copy(&reopened))
        .expect("materialize after reopen");
    assert_eq!(fs::read(&same).expect("read after reopen"), b"from-left\n");
    assert!(!conflicted_paths(&reopened).contains("same.txt"));
    assert!(!listed_conflict_paths(&reopened).contains("same.txt"));
}

#[test]
fn a11_retains_both_names_independent_of_insert_order_and_reopen() {
    let temp = TempDir::new().expect("temp repo");
    let root = temp.path();
    let original = root.join("original.txt");
    let mut repo = Repository::init(root).expect("init");

    fs::write(&original, b"shared\n").expect("write base");
    repo.add(working_copy(&repo), "original.txt", Default::default())
        .expect("add base");
    record_all(&repo, "base");
    for view in ["left", "right", "merge-lr", "merge-rl"] {
        repo.create_view_from(view, "dev")
            .unwrap_or_else(|error| panic!("create view '{view}': {error}"));
    }

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch left");
    fs::rename(&original, root.join("left.txt")).expect("rename left");
    let left = record_all(&repo, "rename left");

    repo.switch_view(working_copy(&repo), "right")
        .expect("switch right");
    fs::rename(&original, root.join("right.txt")).expect("rename right");
    let right = record_all(&repo, "rename right");

    repo.switch_view(working_copy(&repo), "merge-lr")
        .expect("switch merge-lr");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right");
    repo.materialize_sequential(working_copy(&repo))
        .expect("sequential materialize");
    let expected = assert_rename_conflict(&repo, root);
    remove_if_present(&root.join("left.txt"));
    remove_if_present(&root.join("right.txt"));
    repo.materialize(working_copy(&repo))
        .expect("parallel materialize");
    assert_eq!(assert_rename_conflict(&repo, root), expected);

    repo.switch_view(working_copy(&repo), "merge-rl")
        .expect("switch merge-rl");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right first");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left second");
    repo.materialize(working_copy(&repo))
        .expect("materialize reverse order");
    assert_eq!(
        assert_rename_conflict(&repo, root),
        expected,
        "neither insertion order may choose or discard a name"
    );

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch source view");
    assert!(root.join("left.txt").is_file());
    repo.switch_view(working_copy(&repo), "merge-lr")
        .expect("switch merged view");
    assert_eq!(assert_rename_conflict(&repo, root), expected);
    drop(repo);

    let reopened = Repository::open(root).expect("reopen repository");
    reopened
        .materialize(working_copy(&reopened))
        .expect("materialize after reopen");
    assert_eq!(assert_rename_conflict(&reopened, root), expected);
}

#[test]
fn a12_unresolved_sibling_recovers_both_claimants_after_other_view_resolves() {
    let temp = TempDir::new().expect("temp repo");
    let root = temp.path();
    let same = root.join("same.txt");
    let mut repo = Repository::init(root).expect("init");

    fs::write(root.join("seed.txt"), b"seed\n").expect("write seed");
    repo.add(working_copy(&repo), "seed.txt", Default::default())
        .expect("add seed");
    record_all(&repo, "base");
    for view in ["left", "right", "merge"] {
        repo.create_view_from(view, "dev")
            .unwrap_or_else(|error| panic!("create view '{view}': {error}"));
    }

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch left");
    fs::write(&same, b"from-left\n").expect("write left claim");
    repo.add(working_copy(&repo), "same.txt", Default::default())
        .expect("add left claim");
    let left = record_all(&repo, "left creates same path");

    repo.switch_view(working_copy(&repo), "right")
        .expect("switch right");
    fs::write(&same, b"from-right\n").expect("write right claim");
    repo.add(working_copy(&repo), "same.txt", Default::default())
        .expect("add right claim");
    let right = record_all(&repo, "right creates same path");

    repo.switch_view(working_copy(&repo), "merge")
        .expect("switch merge");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right");
    repo.materialize(working_copy(&repo))
        .expect("materialize conflict");
    assert_same_path_conflict(&repo, &same);
    repo.create_view_from("unresolved-sibling", "merge")
        .expect("fork unresolved sibling");

    fs::write(&same, b"from-left\n").expect("choose left claimant");
    let resolution = record_all(&repo, "resolve on merge only");
    assert!(resolution
        .change()
        .hunks()
        .iter()
        .any(|operation| matches!(operation, GraphOp::SolveNameConflict { .. })));

    repo.switch_view(working_copy(&repo), "unresolved-sibling")
        .expect("switch unresolved sibling");
    repo.materialize(working_copy(&repo))
        .expect("materialize unresolved sibling");
    assert_same_path_conflict(&repo, &same);
}

#[test]
fn a11_source_view_removes_foreign_rename_after_visiting_merged_view() {
    let temp = TempDir::new().expect("temp repo");
    let root = temp.path();
    let original = root.join("original.txt");
    let mut repo = Repository::init(root).expect("init");

    fs::write(&original, b"shared\n").expect("write base");
    repo.add(working_copy(&repo), "original.txt", Default::default())
        .expect("add base");
    record_all(&repo, "base");
    for view in ["left", "right", "merge"] {
        repo.create_view_from(view, "dev")
            .unwrap_or_else(|error| panic!("create view '{view}': {error}"));
    }

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch left");
    fs::rename(&original, root.join("left.txt")).expect("rename left");
    let left = record_all(&repo, "rename left");
    repo.switch_view(working_copy(&repo), "right")
        .expect("switch right");
    fs::rename(&original, root.join("right.txt")).expect("rename right");
    let right = record_all(&repo, "rename right");

    repo.switch_view(working_copy(&repo), "merge")
        .expect("switch merge");
    repo.insert_change(left.hash(), InsertOptions::default())
        .expect("insert left");
    repo.insert_change(right.hash(), InsertOptions::default())
        .expect("insert right");
    repo.materialize(working_copy(&repo))
        .expect("materialize merge");
    assert_rename_conflict(&repo, root);

    repo.switch_view(working_copy(&repo), "left")
        .expect("switch source view");
    repo.materialize(working_copy(&repo))
        .expect("rematerialize source view");
    assert!(root.join("left.txt").is_file());
    assert!(
        !root.join("right.txt").exists(),
        "right.txt from the merged view must not remain in the left source view"
    );
}
