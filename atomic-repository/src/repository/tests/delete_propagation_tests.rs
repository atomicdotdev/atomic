//! Whole-file-delete propagation: op-level diagnostics and regressions.
//!
//! Tracks docs/MERGE-CONFLICT-RUBRIC.md §6.5: a whole-file deletion recorded
//! on a draft view must, when inserted into the base view, actually remove
//! the file there. These tests inspect the recorded change's GraphOps
//! directly (ground truth — the CLI renderer proved unreliable) and then
//! verify the end-to-end materialized result.

use super::*;
use atomic_core::change::{Atom, ChangeHeader, GraphOp};
use atomic_core::types::Hash;

use crate::record::{RecordError, RecordOptions};
use std::collections::HashSet;

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    repo.record(
        repo.require_working_copy_id().unwrap(),
        header,
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
}

/// Describe a GraphOp for diagnostics.
fn describe_op(op: &GraphOp<Option<Hash>>) -> String {
    match op {
        GraphOp::Edit { change, local, .. } => match change {
            Atom::Insertion(ins) => format!(
                "Edit/Insertion {{ path: {}, start: {:?}, end: {:?}, flag: {:?} }}",
                local.path, ins.start, ins.end, ins.flag
            ),
            Atom::EdgeUpdate(em) => {
                let flags: Vec<String> = em
                    .edges
                    .iter()
                    .map(|e| format!("{:?}->{:?}", e.previous, e.flag))
                    .collect();
                format!(
                    "Edit/EdgeUpdate {{ path: {}, edges: {}, flags: [{}] }}",
                    local.path,
                    em.edges.len(),
                    flags.join(", ")
                )
            }
        },
        GraphOp::FileDel { del, path, .. } => {
            format!(
                "FileDel {{ path: {}, del_edges: {} }}",
                path,
                del.edges.len()
            )
        }
        GraphOp::FileAdd { path, .. } => format!("FileAdd {{ path: {} }}", path),
        GraphOp::DirAdd { path, .. } => format!("DirAdd {{ path: {} }}", path),
        GraphOp::DirDel { path, .. } => format!("DirDel {{ path: {} }}", path),
        other => format!("{:?}", std::mem::discriminant(other)),
    }
}

/// Build: base 5-line file on dev, whole-file delete recorded on feature.
/// Returns (tempdir, repo, delete-change hash, op descriptions).
fn record_whole_file_delete() -> (TempDir, TestRepository, Hash, Vec<String>) {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");

    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&file).unwrap();
    let outcome = record_all(&repo, "delete f.txt").unwrap();

    let ops: Vec<String> = outcome.change().hunks().iter().map(describe_op).collect();
    let hash = *outcome.hash();

    (temp, repo, hash, ops)
}

/// Stage 1: the recorded whole-file delete must carry deletion edges for
/// EVERY content vertex (5 lines → 5 deletion edges), not an insertion and
/// not a single edge.
#[test]
fn whole_file_delete_records_deletion_edges_for_all_lines() {
    let (_temp, _repo, _hash, ops) = record_whole_file_delete();

    let diag = ops.join("\n  ");
    // Exactly one delete-ish op expected for f.txt.
    let mut deletion_edge_count = 0usize;
    let mut insertion_count = 0usize;
    for op in &ops {
        if op.contains("EdgeUpdate") || op.contains("FileDel") {
            // extract "edges: N"
            if let Some(idx) = op.find("edges: ") {
                let rest = &op[idx + 7..];
                let n: usize = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                deletion_edge_count += n;
            }
        }
        if op.contains("Insertion") {
            insertion_count += 1;
        }
    }

    assert_eq!(
        insertion_count, 0,
        "a whole-file delete must not record insertions.\nOps:\n  {diag}"
    );
    assert!(
        deletion_edge_count >= 5,
        "a 5-line whole-file delete must mark all content vertices deleted \
         (expected >= 5 deletion edges, got {deletion_edge_count}).\nOps:\n  {diag}"
    );
}

/// Delete-vs-modify: feature deletes the file, dev modified one line.
/// Under patch theory each line's fate is independent: the modified line
/// (never touched by the delete) survives; the unmodified lines die. The
/// stale pre-merge bytes must be replaced — the file must contain exactly
/// the surviving line.
#[test]
fn delete_vs_modify_keeps_only_surviving_lines() {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");

    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&file).unwrap();
    let outcome = record_all(&repo, "delete f.txt").unwrap();
    let hash = *outcome.hash();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "alpha\nbeta-mod\ngamma\ndelta\nepsilon\n").unwrap();
    record_all(&repo, "modify beta").unwrap();

    repo.insert_change_rec(
        &hash,
        crate::apply::InsertOptions::default().apply_deps(true),
    )
    .unwrap();
    repo.materialize().unwrap();

    let on_disk = std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(
        on_disk, "beta-mod\n",
        "delete-vs-modify must keep exactly the surviving (modified) line"
    );
}

/// Delete-vs-delete: both views deleted the file; after inserting feature's
/// delete into dev the file must stay removed (no resurrection).
#[test]
fn delete_vs_delete_stays_removed() {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");

    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::remove_file(&file).unwrap();
    record_all(&repo, "delete on feature").unwrap();

    repo.switch_view("dev").unwrap();
    std::fs::remove_file(&file).unwrap();
    record_all(&repo, "delete on dev").unwrap();

    let outcome = repo
        .insert_from_view(crate::apply::CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    // Whether the change applies or is skipped, the file must stay gone.
    let _ = outcome;
    repo.materialize().unwrap();

    assert!(
        !file.exists(),
        "delete-vs-delete: file must stay removed after bulk insert, found:\n{}",
        std::fs::read_to_string(&file).unwrap_or_default()
    );
}

/// Stage 2 (end-to-end): inserting the delete into the unchanged base view
/// removes the file from its materialized working copy.
#[test]
fn whole_file_delete_insert_removes_file_on_target_view() {
    let (temp, mut repo, hash, ops) = record_whole_file_delete();

    repo.switch_view("dev").unwrap();
    repo.insert_change_rec(
        &hash,
        crate::apply::InsertOptions::default().apply_deps(true),
    )
    .unwrap();
    repo.materialize().unwrap();

    let on_disk = std::fs::read_to_string(temp.path().join("f.txt")).ok();
    assert!(
        on_disk.is_none(),
        "inserting a whole-file delete into dev must remove f.txt, but disk has:\n{}\nRecorded ops:\n  {}",
        on_disk.unwrap_or_default(),
        ops.join("\n  ")
    );
}

#[derive(Debug, Clone, Copy)]
enum MaterializeMode {
    Parallel,
    Sequential,
    SelectedParallel,
    SelectedSequential,
    Prefix,
}

fn materialize_path(repo: &TestRepository, path: &str, mode: MaterializeMode) -> MaterializeResult {
    match mode {
        MaterializeMode::Parallel => repo.materialize().unwrap(),
        MaterializeMode::Sequential => repo.materialize_sequential().unwrap(),
        MaterializeMode::SelectedParallel => repo
            .materialize_paths(HashSet::from([path.to_string()]))
            .unwrap(),
        MaterializeMode::SelectedSequential => repo
            .materialize_paths_sequential(HashSet::from([path.to_string()]))
            .unwrap(),
        MaterializeMode::Prefix => repo.materialize_prefix(path).unwrap(),
    }
}

#[test]
fn every_materializer_preserves_present_empty_file() {
    let (temp, repo) = create_temp_repo();
    let path = "nested/empty.txt";
    let file = temp.path().join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, []).unwrap();
    repo.add(path, TrackingOptions::default()).unwrap();
    record_all(&repo, "add empty file").unwrap();

    for mode in [
        MaterializeMode::Parallel,
        MaterializeMode::Sequential,
        MaterializeMode::SelectedParallel,
        MaterializeMode::SelectedSequential,
        MaterializeMode::Prefix,
    ] {
        std::fs::write(&file, b"stale bytes").unwrap();
        let result = materialize_path(&repo, path, mode);
        assert!(
            file.is_file(),
            "{mode:?} must create the tracked empty file"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            Vec::<u8>::new(),
            "{mode:?} must truncate stale bytes for a tracked empty file"
        );
        assert_eq!(result.files_deleted, 0, "{mode:?} deleted a present file");
        assert_eq!(repo.get_file_content(path).unwrap(), Some(Vec::new()));
        assert!(matches!(
            repo.get_materialized_entry_on_view(path, "dev").unwrap(),
            MaterializedEntry::Present { bytes, .. } if bytes.is_empty()
        ));
    }
}

#[test]
fn every_materializer_removes_explicitly_absent_file() {
    let (temp, mut repo, hash, _ops) = record_whole_file_delete();
    let path = "f.txt";
    let file = temp.path().join(path);

    repo.switch_view("dev").unwrap();
    repo.insert_change_rec(
        &hash,
        crate::apply::InsertOptions::default().apply_deps(true),
    )
    .unwrap();

    for mode in [
        MaterializeMode::Parallel,
        MaterializeMode::Sequential,
        MaterializeMode::SelectedParallel,
        MaterializeMode::SelectedSequential,
        MaterializeMode::Prefix,
    ] {
        std::fs::write(&file, b"stale bytes that must not survive").unwrap();
        let result = materialize_path(&repo, path, mode);
        assert!(!file.exists(), "{mode:?} must remove an absent file");
        assert_eq!(result.files_deleted, 1, "{mode:?} deletion count");
        assert_eq!(repo.get_file_content(path).unwrap(), None);
        assert!(matches!(
            repo.get_materialized_entry_on_view(path, "dev").unwrap(),
            MaterializedEntry::Absent { .. }
        ));
    }
}

#[test]
fn sole_view_deletion_remains_absent_across_every_materializer() {
    let (temp, repo) = create_temp_repo();
    let path = "sole.txt";
    let file = temp.path().join(path);
    std::fs::write(&file, b"sole-view content\n").unwrap();
    repo.add(path, TrackingOptions::default()).unwrap();
    record_all(&repo, "add sole-view file").unwrap();
    std::fs::remove_file(&file).unwrap();
    record_all(&repo, "delete sole-view file").unwrap();

    assert!(matches!(
        repo.get_materialized_entry_on_view(path, "dev").unwrap(),
        MaterializedEntry::Absent { .. }
    ));
    for mode in [
        MaterializeMode::Parallel,
        MaterializeMode::Sequential,
        MaterializeMode::SelectedParallel,
        MaterializeMode::SelectedSequential,
        MaterializeMode::Prefix,
    ] {
        std::fs::write(&file, b"recreated stale bytes").unwrap();
        let result = materialize_path(&repo, path, mode);
        assert!(!file.exists(), "{mode:?} must honor sole-view deletion");
        assert_eq!(result.files_deleted, 1, "{mode:?} deletion count");
    }
}

#[test]
fn flattened_view_replays_lifecycle_in_causal_order() {
    let (temp, mut repo) = create_temp_repo();
    let deleted = temp.path().join("deleted.txt");
    let inherited_empty = temp.path().join("inherited-empty.txt");
    std::fs::write(&deleted, b"base content\n").unwrap();
    std::fs::write(&inherited_empty, []).unwrap();
    repo.add("deleted.txt", TrackingOptions::default()).unwrap();
    repo.add("inherited-empty.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "base files").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    let feature_empty = temp.path().join("feature-empty.txt");
    std::fs::write(&feature_empty, []).unwrap();
    repo.add("feature-empty.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "feature empty file").unwrap();
    std::fs::remove_file(&deleted).unwrap();
    record_all(&repo, "delete base file").unwrap();

    let closure = repo.effective_history(Some("feature")).unwrap();
    repo.create_shared_view("flattened").unwrap();
    for entry in closure {
        repo.insert_change(
            &entry.hash,
            crate::apply::InsertOptions::with_dependencies().view("flattened"),
        )
        .unwrap();
    }
    repo.align_to_view("flattened").unwrap();
    repo.reindex_working_copy().unwrap();

    assert!(matches!(
        repo.get_materialized_entry_on_view("deleted.txt", "flattened")
            .unwrap(),
        MaterializedEntry::Absent { .. }
    ));
    for path in ["inherited-empty.txt", "feature-empty.txt"] {
        assert!(matches!(
            repo.get_materialized_entry_on_view(path, "flattened")
                .unwrap(),
            MaterializedEntry::Present { bytes, .. } if bytes.is_empty()
        ));
    }
    let status = repo.status(StatusOptions::default()).unwrap();
    assert!(status.is_clean(), "flattened closure status: {status:?}");
}
