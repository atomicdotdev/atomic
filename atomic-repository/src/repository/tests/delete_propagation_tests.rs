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

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    repo.record(
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
fn record_whole_file_delete() -> (TempDir, Repository, Hash, Vec<String>) {
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
