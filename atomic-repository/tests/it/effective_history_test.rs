//! Integration tests for `Repository::effective_history`.
//!
//! `effective_history` returns the *full* change set a view depends on, in
//! dependency order — the draft's own changes plus everything inherited from
//! its shared base. This is what `push` uploads so a flattened (shared)
//! remote view receives a complete graph, unlike `log`, which for a draft
//! shows only that view's own new changes.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewScope, ViewTxnT};
use atomic_core::types::Hash;
use atomic_core::WorkingCopyId;
use atomic_repository::history::HistoryOptions;
use atomic_repository::{
    graph_visibility_closure, MaterializedEntry, RecordOptions, Repository, StatusOptions,
};
use tempfile::TempDir;

fn working_copy(repo: &Repository) -> WorkingCopyId {
    repo.require_working_copy_id().expect("working copy id")
}

fn add_file(repo: &Repository, repo_path: &Path, name: &str, content: &str) {
    let path = repo_path.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create file parent");
    }
    fs::write(path, content).expect("write file");
    repo.add(working_copy(repo), name, Default::default())
        .expect("add file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(working_copy(repo), header, RecordOptions::default())
        .expect("record")
        .hash()
}

/// A draft view's `effective_history` must include its shared base's changes,
/// ordered base-first, while `log` on the draft shows only its own changes.
#[test]
fn effective_history_includes_inherited_base_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    // Change A on the shared base view (dev).
    add_file(&repo, &repo_path, "a.txt", "A\n");
    let a = record(&repo, "add a");

    // Create a draft view parented on dev and switch to it.
    repo.create_view("feature").expect("create draft view");
    repo.switch_view(working_copy(&repo), "feature")
        .expect("switch to draft");

    // Change B recorded on the draft.
    add_file(&repo, &repo_path, "b.txt", "B\n");
    let b = record(&repo, "add b");

    // `log` on the draft shows only its own new change.
    let own: Vec<Hash> = repo
        .log(HistoryOptions::default().view("feature"))
        .expect("log draft")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    assert_eq!(own, vec![b], "draft log should show only its own change");

    // `effective_history` includes the inherited base change first.
    let effective: Vec<Hash> = repo
        .effective_history(Some("feature"))
        .expect("effective history")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    assert_eq!(
        effective,
        vec![a, b],
        "effective history should be base-first: inherited change then own change"
    );
}

/// For a shared view, `effective_history` equals `log` (shared views are
/// self-contained) — this guards against changing push behavior for shared
/// views.
#[test]
fn effective_history_matches_log_for_shared_view() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "a.txt", "A\n");
    let a = record(&repo, "add a");
    add_file(&repo, &repo_path, "b.txt", "B\n");
    let b = record(&repo, "add b");

    let log_hashes: Vec<Hash> = repo
        .log(HistoryOptions::default())
        .expect("log")
        .into_iter()
        .map(|e| e.hash)
        .collect();
    let effective: Vec<Hash> = repo
        .effective_history(None)
        .expect("effective history")
        .into_iter()
        .map(|e| e.hash)
        .collect();

    assert_eq!(log_hashes, vec![a, b]);
    assert_eq!(effective, log_hashes);
}

/// A nested draft inherits both history and content through every parent,
/// without duplicating changes copied into a draft's own log.
#[test]
fn nested_draft_effective_history_and_content_include_all_parents() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "base.txt", "base\n");
    let base = record(&repo, "base");

    let shared_view = repo.current_view().to_string();
    repo.create_view_from("parent", &shared_view)
        .expect("create parent draft");
    repo.switch_view(working_copy(&repo), "parent")
        .expect("switch to parent draft");
    add_file(&repo, &repo_path, "parent.txt", "parent\n");
    let parent = record(&repo, "parent");

    repo.create_view_from("child", "parent")
        .expect("create nested child draft");
    repo.switch_view(working_copy(&repo), "child")
        .expect("switch to child draft");
    add_file(&repo, &repo_path, "child.txt", "child\n");
    let child = record(&repo, "child");

    let effective: Vec<Hash> = repo
        .effective_history(Some("child"))
        .expect("nested effective history")
        .into_iter()
        .map(|entry| entry.hash)
        .collect();
    assert_eq!(effective, vec![base, parent, child]);

    assert_eq!(
        repo.get_file_content_on_view("base.txt", "child")
            .expect("read inherited base content"),
        Some(b"base\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("parent.txt", "child")
            .expect("read inherited parent content"),
        Some(b"parent\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("child.txt", "child")
            .expect("read child content"),
        Some(b"child\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_before_change("parent.txt", &child)
            .expect("read inherited content before first child change"),
        Some(b"parent\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_after_change("parent.txt", &child)
            .expect("read inherited content after first child change"),
        Some(b"parent\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_at_sequence("parent.txt", 0)
            .expect("read ancestors before child sequence zero"),
        Some(b"parent\n".to_vec())
    );
}

/// A tracked empty file is distinct from an untracked path: its recorded
/// content is present and has zero bytes.
#[test]
fn tracked_empty_file_content_is_some_empty_vec() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "empty.txt", "");
    record(&repo, "add empty file");

    assert_eq!(
        repo.get_file_content("empty.txt")
            .expect("read tracked empty file"),
        Some(Vec::new())
    );
}

#[test]
fn omitted_direct_dependencies_are_visible_across_every_entry_point() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let mut repo = Repository::init(&repo_path).expect("init");

    // Build A → B → C on a sibling draft so the legacy view can directly
    // reference only C while retaining complete dependency metadata.
    repo.create_view("source").expect("create source view");
    repo.switch_view(working_copy(&repo), "source")
        .expect("switch source view");
    add_file(&repo, &repo_path, "src/value.txt", "one\n");
    add_file(&repo, &repo_path, "outside.txt", "outside\n");
    let a = record(&repo, "A: add files");
    fs::write(repo_path.join("src/value.txt"), "two\n").unwrap();
    let b = record(&repo, "B: edit value");
    fs::write(repo_path.join("src/value.txt"), "three\n").unwrap();
    let c = record(&repo, "C: edit value again");

    let (a_id, b_id, c_id) = {
        let txn = repo.pristine().read_txn().unwrap();
        (
            txn.get_internal(&a).unwrap().unwrap(),
            txn.get_internal(&b).unwrap().unwrap(),
            txn.get_internal(&c).unwrap().unwrap(),
        )
    };
    {
        let mut txn = repo.pristine().write_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        let mut legacy = txn
            .create_view("legacy", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.put_change(&mut legacy, c_id, &c).unwrap();
        txn.update_view(&legacy).unwrap();
        txn.commit().unwrap();
    }

    // Direct membership and Merkle identity remain untouched; traversal derives
    // A and B in memory from C's validated dependency closure.
    {
        let txn = repo.pristine().read_txn().unwrap();
        let legacy = txn.get_view("legacy").unwrap().unwrap();
        let membership = txn.view_membership_set(&legacy).unwrap();
        assert_eq!(membership.iter().copied().collect::<Vec<_>>(), vec![c_id]);
        let visibility = graph_visibility_closure(&txn, &legacy).unwrap();
        assert!(visibility.contains(a_id));
        assert!(visibility.contains(b_id));
        assert!(visibility.contains(c_id));
        assert_eq!(visibility.len(), 3);
    }

    repo.set_current_view(working_copy(&repo), "legacy")
        .unwrap();
    assert_eq!(
        repo.visible_file_paths("legacy").unwrap(),
        HashSet::from(["src/value.txt".to_string(), "outside.txt".to_string()])
    );
    assert_eq!(
        repo.get_file_content("src/value.txt").unwrap(),
        Some(b"three\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("src/value.txt", "legacy")
            .unwrap(),
        Some(b"three\n".to_vec())
    );
    assert!(matches!(
        repo.get_materialized_entry_on_view("src/value.txt", "legacy")
            .unwrap(),
        MaterializedEntry::Present { bytes, .. } if bytes == b"three\n"
    ));

    // Every materializer independently repairs stale bytes from the same closure.
    for sequential in [false, true] {
        fs::write(repo_path.join("src/value.txt"), "stale\n").unwrap();
        fs::write(repo_path.join("outside.txt"), "stale outside\n").unwrap();
        if sequential {
            repo.materialize_sequential(working_copy(&repo)).unwrap();
        } else {
            repo.materialize(working_copy(&repo)).unwrap();
        }
        assert_eq!(
            fs::read(repo_path.join("src/value.txt")).unwrap(),
            b"three\n"
        );
        assert_eq!(
            fs::read(repo_path.join("outside.txt")).unwrap(),
            b"outside\n"
        );
    }

    for selected in [false, true] {
        fs::write(repo_path.join("src/value.txt"), "stale selected\n").unwrap();
        fs::write(repo_path.join("outside.txt"), "selected sentinel\n").unwrap();
        let paths = HashSet::from(["src/value.txt".to_string()]);
        if selected {
            repo.materialize_paths_sequential(working_copy(&repo), paths)
                .unwrap();
        } else {
            repo.materialize_paths(working_copy(&repo), paths).unwrap();
        }
        assert_eq!(
            fs::read(repo_path.join("src/value.txt")).unwrap(),
            b"three\n"
        );
        assert_eq!(
            fs::read(repo_path.join("outside.txt")).unwrap(),
            b"selected sentinel\n"
        );
    }

    fs::write(repo_path.join("src/value.txt"), "stale prefix\n").unwrap();
    fs::write(repo_path.join("outside.txt"), "prefix sentinel\n").unwrap();
    repo.materialize_prefix(working_copy(&repo), "src/")
        .unwrap();
    assert_eq!(
        fs::read(repo_path.join("src/value.txt")).unwrap(),
        b"three\n"
    );
    assert_eq!(
        fs::read(repo_path.join("outside.txt")).unwrap(),
        b"prefix sentinel\n"
    );

    // Restore the full expected tree, then prove status and record old-content
    // retrieval share the same closure.
    repo.materialize(working_copy(&repo)).unwrap();
    assert!(repo
        .status(working_copy(&repo), StatusOptions::default())
        .unwrap()
        .is_clean());
    fs::write(repo_path.join("src/value.txt"), "four\n").unwrap();
    let d = record(&repo, "D: record from dependency-repaired baseline");
    assert_eq!(
        repo.get_file_content_before_change("src/value.txt", &d)
            .unwrap(),
        Some(b"three\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_after_change("src/value.txt", &d)
            .unwrap(),
        Some(b"four\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_excluding("src/value.txt", &c)
            .unwrap(),
        Some(b"two\n".to_vec()),
        "excluding a non-tip change must return its causal prefix, not re-add it"
    );
    repo.materialize(working_copy(&repo)).unwrap();
    assert_eq!(
        fs::read(repo_path.join("src/value.txt")).unwrap(),
        b"four\n"
    );
    assert!(repo
        .status(working_copy(&repo), StatusOptions::default())
        .unwrap()
        .is_clean());

    let effective: Vec<Hash> = repo
        .effective_history(Some("legacy"))
        .unwrap()
        .into_iter()
        .map(|entry| entry.hash)
        .collect();
    assert_eq!(
        effective,
        vec![c, d],
        "closure repair must not rewrite membership"
    );
}

#[test]
fn incomplete_dependency_index_fails_closed_before_traversal() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    let base_change = Change::new(
        ChangeHeader::new("legacy base"),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let base = repo.save_change(&base_change).unwrap();
    let tip_change = Change::new(
        ChangeHeader::new("indexed tip"),
        Vec::new(),
        Vec::new(),
        vec![base],
    );
    let tip = repo.save_change(&tip_change).unwrap();

    let (base_id, tip_id) = {
        let mut txn = repo.pristine().write_txn().unwrap();
        let base_id = txn.register_change(&base).unwrap();
        let tip_id = txn.register_change(&tip).unwrap();
        txn.put_change_deps(tip_id, &[base]).unwrap();
        let mut dev = txn.get_view("dev").unwrap().unwrap();
        txn.put_change(&mut dev, tip_id, &tip).unwrap();
        txn.update_view(&dev).unwrap();
        txn.commit().unwrap();
        (base_id, tip_id)
    };

    let guard = repo_path.join("guard.txt");
    fs::write(&guard, "must survive\n").unwrap();
    for error in [
        repo.status(working_copy(&repo), StatusOptions::default())
            .unwrap_err()
            .to_string(),
        repo.materialize(working_copy(&repo))
            .unwrap_err()
            .to_string(),
        repo.get_file_content("guard.txt").unwrap_err().to_string(),
        repo.get_file_content_via_crdt("guard.txt")
            .unwrap_err()
            .to_string(),
    ] {
        assert!(
            error.contains("no indexed dependency metadata"),
            "unexpected fail-closed diagnostic: {error}"
        );
    }
    assert_eq!(fs::read(&guard).unwrap(), b"must survive\n");

    {
        let txn = repo.pristine().read_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        let membership = txn.view_membership_set(&dev).unwrap();
        assert!(membership.contains(tip_id));
        assert!(!membership.contains(base_id));
        assert!(txn.is_change_deps_indexed(tip_id).unwrap());
        assert!(!txn.is_change_deps_indexed(base_id).unwrap());
    }

    let (indexed, _skipped, failed) = repo.repair_change_dependency_index(false).unwrap();
    assert!(indexed >= 1);
    assert_eq!(failed, 0);
    let txn = repo.pristine().read_txn().unwrap();
    let dev = txn.get_view("dev").unwrap().unwrap();
    let visibility = graph_visibility_closure(&txn, &dev).unwrap();
    assert!(visibility.contains(base_id));
    assert!(visibility.contains(tip_id));
}
