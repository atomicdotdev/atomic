//! Rename / move recording tests (rubric A10/A11, Stage 1).
//!
//! Stage 1 (ATOM::34) makes `Repository::record` classify a git-style raw
//! rename — `fs::rename(old, new)` on disk with the new path left untracked —
//! as a move: it emits a single `GraphOp::FileMove` that reuses the original
//! inode, instead of a FileDel + FileAdd that would lose history.
//!
//! These assertions are op-level (`outcome.change().hunks()`) because the
//! `atomic change` CLI renderer mislabels FileMove/FileDel.

use super::*;
use crate::record::{RecordError, RecordOptions};
use crate::tracking::TrackingOptions;
use crate::InsertOptions;
use atomic_core::change::{ChangeHeader, GraphOp};

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options)
}

/// A raw disk rename (new path untracked) records as one FileMove, reusing the
/// original inode, with no FileDel/FileAdd for the involved paths.
#[test]
fn test_raw_rename_records_as_filemove() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");

    std::fs::write(&old, "line1\nline2\nline3\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    // Raw disk rename; the new path is left UNTRACKED (no `atomic add`).
    std::fs::rename(&old, temp.path().join("new.txt")).unwrap();

    let outcome = record_all(&repo, "rename old->new").expect("rename should record");

    // Op-level: exactly one FileMove(-> new.txt); no FileDel/FileAdd for these paths.
    let mut filemoves = 0;
    for op in outcome.change().hunks() {
        match op {
            GraphOp::FileMove { path, .. } => {
                filemoves += 1;
                assert_eq!(path, "new.txt", "FileMove should target the new path");
            }
            GraphOp::FileDel { path, .. } => {
                assert_ne!(
                    path, "old.txt",
                    "rename must not emit a FileDel for the old path"
                );
            }
            GraphOp::FileAdd { path, .. } => {
                assert_ne!(
                    path, "new.txt",
                    "rename must not emit a FileAdd for the new path"
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        filemoves,
        1,
        "expected exactly one FileMove, got {filemoves}. hunks: {:?}",
        outcome
            .change()
            .hunks()
            .iter()
            .map(|h| h.type_name())
            .collect::<Vec<_>>()
    );
}

/// After recording a raw rename and re-materializing: the new path holds the
/// original content byte-exact, the old path is gone from disk and tracking,
/// and the inode is preserved (the whole point of a move vs delete+add).
#[test]
fn test_raw_rename_roundtrip_preserves_inode_and_content() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");

    std::fs::write(&old, "alpha\nbeta\ngamma\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let orig_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::rename(&old, temp.path().join("new.txt")).unwrap();
    record_all(&repo, "rename old->new").unwrap();
    repo.materialize().unwrap();

    // New path: original content, byte-exact.
    assert_eq!(
        std::fs::read(temp.path().join("new.txt")).unwrap(),
        b"alpha\nbeta\ngamma\n"
    );
    // Old path: gone from disk.
    assert!(!temp.path().join("old.txt").exists());

    // Tracking: new tracked with the ORIGINAL inode; old untracked.
    assert_eq!(
        repo.get_file_inode("new.txt").unwrap().unwrap(),
        orig_inode,
        "rename must preserve the inode (history), not allocate a new one"
    );
    assert!(
        repo.get_file_inode("old.txt").unwrap().is_none(),
        "old path must no longer be tracked"
    );
}

/// Stage 2 equivalence (ATOM::35): the `atomic mv` command now performs the
/// on-disk rename WITHOUT eagerly updating tracking (no `move_file`), so its
/// effect on the working copy is identical to a raw rename. This test
/// reproduces exactly that shape — `fs::rename` with no tracking call — and
/// asserts it records as one FileMove reusing the original inode, locking the
/// equivalence the CLI relies on.
#[test]
fn test_atomic_mv_equivalent_records_as_filemove() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");

    std::fs::write(&old, "a\nb\nc\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let orig_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    // Exactly what `atomic mv` now does: disk rename only, tracking untouched.
    std::fs::rename(&old, temp.path().join("renamed.txt")).unwrap();

    let outcome = record_all(&repo, "mv old->renamed").unwrap();

    let filemoves = outcome
        .change()
        .hunks()
        .iter()
        .filter(|op| matches!(op, GraphOp::FileMove { .. }))
        .count();
    assert_eq!(
        filemoves, 1,
        "atomic mv equivalent should record one FileMove"
    );

    repo.materialize().unwrap();
    assert_eq!(
        repo.get_file_inode("renamed.txt").unwrap().unwrap(),
        orig_inode,
        "atomic mv must preserve the inode"
    );
    assert!(repo.get_file_inode("old.txt").unwrap().is_none());
}

/// Stage 3 (ATOM::36): inserting a rename (FileMove) change into another view
/// must actually apply it — the new path appears with the original content, the
/// old path is gone, and the inode is preserved. Previously this was a silent
/// no-op (TREE was only journaled for switch-replay, so materialize kept the
/// old path).
#[test]
fn test_cross_view_rename_applies_on_insert() {
    let (temp, mut repo) = create_temp_repo();
    let f = temp.path().join("f.txt");

    std::fs::write(&f, "line1\nline2\nline3\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let orig_inode = repo.get_file_inode("f.txt").unwrap().unwrap();

    // feature renames f -> g.
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::rename(&f, temp.path().join("g.txt")).unwrap();
    let mv = record_all(&repo, "rename f->g").unwrap();
    let mv_hash = *mv.hash();

    // Back on dev (still has f.txt), insert the rename.
    repo.switch_view("dev").unwrap();
    assert!(
        temp.path().join("f.txt").exists(),
        "precondition: dev has f.txt"
    );
    repo.insert_change(&mv_hash, InsertOptions::default())
        .unwrap();
    repo.materialize().unwrap();

    assert!(
        temp.path().join("g.txt").exists(),
        "new path must appear on dev"
    );
    assert!(
        !temp.path().join("f.txt").exists(),
        "old path must be gone on dev"
    );
    assert_eq!(
        std::fs::read(temp.path().join("g.txt")).unwrap(),
        b"line1\nline2\nline3\n"
    );
    assert_eq!(
        repo.get_file_inode("g.txt").unwrap().unwrap(),
        orig_inode,
        "cross-view rename must preserve the inode"
    );
    assert!(repo.get_file_inode("f.txt").unwrap().is_none());

    // Idempotency: switching away and back must not double-apply or resurrect
    // the old path (the deferred journal must agree with the eager update).
    repo.switch_view("feature").unwrap();
    repo.switch_view("dev").unwrap();
    assert!(
        temp.path().join("g.txt").exists(),
        "g.txt survives switch round-trip"
    );
    assert!(
        !temp.path().join("f.txt").exists(),
        "f.txt must not resurrect after switch round-trip"
    );
    assert_eq!(repo.get_file_inode("g.txt").unwrap().unwrap(), orig_inode);
}

/// A10 (rename vs edit): one view renames f->g, the other edits f's content.
/// Inserting the rename yields the file at the NEW path carrying the EDITED
/// content — the inode survives the rename so the concurrent edit is preserved,
/// with no conflict markers.
#[test]
fn test_cross_view_rename_vs_edit_preserves_edit() {
    let (temp, mut repo) = create_temp_repo();
    let f = temp.path().join("f.txt");

    std::fs::write(&f, "line1\nline2\nline3\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let orig_inode = repo.get_file_inode("f.txt").unwrap().unwrap();

    // feature renames f -> g.
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::rename(&f, temp.path().join("g.txt")).unwrap();
    let mv = record_all(&repo, "rename f->g").unwrap();
    let mv_hash = *mv.hash();

    // dev edits f's content (same inode).
    repo.switch_view("dev").unwrap();
    std::fs::write(&f, "line1\nEDITED\nline3\n").unwrap();
    record_all(&repo, "edit line2 on dev").unwrap();

    // Insert the rename: the edit must ride along to the new path.
    repo.insert_change(&mv_hash, InsertOptions::default())
        .unwrap();
    repo.materialize().unwrap();

    assert!(temp.path().join("g.txt").exists());
    assert!(!temp.path().join("f.txt").exists());
    let content = std::fs::read(temp.path().join("g.txt")).unwrap();
    assert_eq!(
        content, b"line1\nEDITED\nline3\n",
        "the concurrent edit must survive the rename (inode preserved)"
    );
    assert!(
        !content.windows(7).any(|w| w == b">>>>>>>"),
        "rename vs edit is not a conflict; no markers expected"
    );
    assert_eq!(repo.get_file_inode("g.txt").unwrap().unwrap(), orig_inode);
}

/// Regression guard: an ordinary delete (no matching untracked file) is NOT
/// misclassified as a move — it still records as a deletion.
#[test]
fn test_plain_delete_is_not_a_rename() {
    let (temp, repo) = create_temp_repo();
    let f = temp.path().join("f.txt");

    std::fs::write(&f, "content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    std::fs::remove_file(&f).unwrap();
    let outcome = record_all(&repo, "delete f").unwrap();

    let mut sawdel = false;
    for op in outcome.change().hunks() {
        if let GraphOp::FileMove { .. } = op {
            panic!("a plain delete must not record as a FileMove");
        }
        if let GraphOp::FileDel { path, .. } = op {
            if path == "f.txt" {
                sawdel = true;
            }
        }
    }
    assert!(sawdel, "plain delete should still record as a FileDel");
    assert!(repo.get_file_inode("f.txt").unwrap().is_none());
}
