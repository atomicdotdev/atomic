//! Rename / move recording tests (rubric A10/A11, Stage 1).
//!
//! Stage 1 (ATOM::34) makes `Repository::record` classify a git-style raw
//! rename — `fs::rename(old, new)` on disk with the new path left untracked —
//! as a move: it emits a single `GraphOp::FileMove` that reuses the original
//! inode, instead of a FileDel + FileAdd that would lose history.
//!
//! These assertions inspect graph, semantic, and advisory evidence so move
//! identity remains reviewable across recording and replay.

use super::*;
use crate::record::{LossNote, MoveAuthority, MoveBasis, PROBABLE_MOVE_THRESHOLD_BPS};
use crate::record::{RecordError, RecordOptions};
use crate::tracking::TrackingOptions;
use crate::InsertOptions;
use crate::UnrecordOptions;
use atomic_core::change::{ChangeHeader, GraphOp};
use atomic_core::crdt::TrunkOp;
use atomic_core::pristine::TreeTxnT;

fn record_all(repo: &Repository, message: &str) -> Result<RecordOutcome, RecordError> {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(repo.require_working_copy_id().unwrap(), header, options)
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
    let evidence = outcome.move_evidence().unwrap().unwrap();
    assert!(evidence.authoritative_moves.is_empty());
    assert!(evidence.probable_moves.iter().any(|probable| {
        probable.old_path == "old.txt"
            && probable.new_path == "new.txt"
            && probable.score == 10_000
            && probable.basis == MoveBasis::ByteIdentity
    }));
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

#[test]
fn test_rename_back_records_exact_filemove_without_deleting_content() {
    let (temp, mut repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("new.txt");
    let content = b"alpha\nbeta\ngamma\n";

    std::fs::write(&old, content).unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("old.txt").unwrap().unwrap();
    repo.create_view_from("switch-check", "dev").unwrap();

    std::fs::rename(&old, &new).unwrap();
    let first = record_all(&repo, "old to new").unwrap();
    assert!(first
        .change()
        .hunks()
        .iter()
        .any(|operation| matches!(operation, GraphOp::FileMove { path, .. } if path == "new.txt")));

    std::fs::rename(&new, &old).unwrap();
    let second = record_all(&repo, "new back to old").unwrap();
    assert!(second.errors().is_empty(), "{:?}", second.errors());
    assert!(second.was_applied());
    assert!(
        matches!(
            second.change().hunks(),
            [GraphOp::FileMove { path, .. }] if path == "old.txt"
        ),
        "rename-back hunks: {:?}",
        second.change().hunks()
    );
    let evidence = second.move_evidence().unwrap().unwrap();
    assert!(evidence.authoritative_moves.is_empty());
    assert!(evidence
        .probable_moves
        .iter()
        .any(|candidate| candidate.old_path == "new.txt" && candidate.new_path == "old.txt"));
    let GraphOp::FileMove { del, .. } = &second.change().hunks()[0] else {
        unreachable!()
    };
    assert_eq!(del.edges.len(), 1);
    assert_eq!(
        del.edges[0].to.change,
        Some(*first.hash()),
        "rename-back must delete the current name vertex introduced by the first move"
    );
    assert_eq!(del.edges[0].introduced_by, Some(*first.hash()));

    repo.materialize().unwrap();
    assert_eq!(repo.get_file_inode("old.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&old).unwrap(), content);
    assert!(!new.exists());
    assert!(repo
        .status(crate::status::StatusOptions::default())
        .unwrap()
        .is_clean());
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    repo.switch_view("switch-check").unwrap();
    repo.switch_view("dev").unwrap();
    repo.materialize().unwrap();
    assert_eq!(std::fs::read(&old).unwrap(), content);
    assert!(!new.exists());
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    drop(repo);
    let reopened = Repository::open(temp.path()).unwrap();
    let working_copy = reopened.require_working_copy_id().unwrap();
    reopened.materialize(working_copy).unwrap();
    assert_eq!(reopened.get_file_inode("old.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&old).unwrap(), content);
    assert!(!new.exists());
    assert!(reopened
        .status(working_copy, crate::status::StatusOptions::default())
        .unwrap()
        .is_clean());
    assert!(reopened
        .verify_working_copy(working_copy)
        .unwrap()
        .is_healthy());
}

#[test]
fn test_exact_rename_into_new_nested_directories_is_parent_first_and_stable() {
    let (temp, mut repo) = create_temp_repo();
    let content = b"stable content\n";
    let original = temp.path().join("f.txt");
    std::fs::write(&original, content).unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("f.txt").unwrap().unwrap();
    repo.create_view_from("switch-check", "dev").unwrap();

    let one_level = temp.path().join("sub/f.txt");
    std::fs::create_dir_all(one_level.parent().unwrap()).unwrap();
    std::fs::rename(&original, &one_level).unwrap();
    let first = record_all(&repo, "move into new directory").unwrap();
    assert!(
        matches!(
            first.change().hunks(),
            [
                GraphOp::DirAdd { path: parent, .. },
                GraphOp::FileMove { path: moved, .. }
            ] if parent == "sub" && moved == "sub/f.txt"
        ),
        "one-level move hunks: {:?}",
        first.change().hunks()
    );
    repo.materialize().unwrap();
    assert_eq!(repo.get_file_inode("sub/f.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&one_level).unwrap(), content);
    assert!(repo
        .status(crate::status::StatusOptions::default())
        .unwrap()
        .is_clean());
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    let nested = temp.path().join("deep/nested/f.txt");
    std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
    std::fs::rename(&one_level, &nested).unwrap();
    let second = record_all(&repo, "move into nested new directories").unwrap();
    assert!(
        matches!(
            second.change().hunks(),
            [
                GraphOp::DirAdd { path: first_parent, .. },
                GraphOp::DirAdd { path: second_parent, .. },
                GraphOp::FileMove { path: moved, .. }
            ] if first_parent == "deep"
                && second_parent == "deep/nested"
                && moved == "deep/nested/f.txt"
        ),
        "nested move hunks: {:?}",
        second.change().hunks()
    );
    repo.materialize().unwrap();
    assert_eq!(
        repo.get_file_inode("deep/nested/f.txt").unwrap(),
        Some(inode)
    );
    assert_eq!(std::fs::read(&nested).unwrap(), content);
    assert!(!one_level.exists());
    assert!(repo
        .status(crate::status::StatusOptions::default())
        .unwrap()
        .is_clean());
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    repo.switch_view("switch-check").unwrap();
    repo.switch_view("dev").unwrap();
    repo.materialize().unwrap();
    assert_eq!(std::fs::read(&nested).unwrap(), content);
    assert_eq!(
        repo.get_file_inode("deep/nested/f.txt").unwrap(),
        Some(inode)
    );
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    drop(repo);
    let reopened = Repository::open(temp.path()).unwrap();
    let working_copy = reopened.require_working_copy_id().unwrap();
    reopened.materialize(working_copy).unwrap();
    assert_eq!(std::fs::read(&nested).unwrap(), content);
    assert_eq!(
        reopened.get_file_inode("deep/nested/f.txt").unwrap(),
        Some(inode)
    );
    assert!(reopened
        .status(working_copy, crate::status::StatusOptions::default())
        .unwrap()
        .is_clean());
    assert!(reopened
        .verify_working_copy(working_copy)
        .unwrap()
        .is_healthy());
}

/// `atomic mv` stages the original inode at the destination after moving the
/// filesystem entry. That stable-inode relationship is authoritative and must
/// record as one FileMove without relying on content similarity.
#[test]
fn test_atomic_mv_equivalent_records_as_filemove() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");

    std::fs::write(&old, "a\nb\nc\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let orig_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    // Exactly what `atomic mv` now does: move on disk, then stage the same inode.
    std::fs::rename(&old, temp.path().join("renamed.txt")).unwrap();
    assert_eq!(
        repo.move_file("old.txt", "renamed.txt").unwrap(),
        orig_inode
    );

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
    let evidence = outcome.move_evidence().unwrap().unwrap();
    let authoritative = evidence.authoritative_moves.iter().next().unwrap();
    assert_eq!(authoritative.old_path, "old.txt");
    assert_eq!(authoritative.new_path, "renamed.txt");
    assert_eq!(authoritative.inode, orig_inode);
    assert_eq!(
        authoritative.authority,
        MoveAuthority::StableInodeProjection
    );
    assert!(evidence.probable_moves.is_empty());

    repo.materialize().unwrap();
    assert_eq!(
        repo.get_file_inode("renamed.txt").unwrap().unwrap(),
        orig_inode,
        "atomic mv must preserve the inode"
    );
    assert!(repo.get_file_inode("old.txt").unwrap().is_none());
}

#[test]
fn test_authoritative_move_plus_edit_preserves_graph_and_semantic_identity() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("nested/new.txt");
    std::fs::write(&old, "one\ntwo\nthree\nfour\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::create_dir_all(new.parent().unwrap()).unwrap();
    std::fs::rename(&old, &new).unwrap();
    assert_eq!(repo.move_file("old.txt", "nested/new.txt").unwrap(), inode);
    std::fs::write(&new, "one\ntwo edited\nthree\nfour\n").unwrap();

    let outcome = record_all(&repo, "authoritative move plus edit").unwrap();
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|op| matches!(op, GraphOp::FileMove { path, .. } if path == "nested/new.txt")));
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|op| { matches!(op, GraphOp::Edit { .. } | GraphOp::Replacement { .. }) }));
    assert!(!outcome.change().hunks().iter().any(|op| {
        matches!(op, GraphOp::FileDel { path, .. } if path == "old.txt")
            || matches!(op, GraphOp::FileAdd { path, .. } if path == "nested/new.txt")
    }));

    let semantic = outcome
        .change()
        .file_ops()
        .iter()
        .find(|ops| ops.path() == "nested/new.txt")
        .expect("move should carry semantic operations");
    assert!(matches!(
        semantic.trunk_op(),
        Some(TrunkOp::Move { new_path, .. }) if new_path == "nested/new.txt"
    ));
    assert!(!semantic.line_ops().is_empty());

    let evidence = outcome.move_evidence().unwrap().unwrap();
    assert_eq!(evidence.authoritative_moves.len(), 1);
    assert!(evidence.probable_moves.is_empty());

    repo.materialize().unwrap();
    assert_eq!(repo.get_file_inode("nested/new.txt").unwrap(), Some(inode));
    assert_eq!(
        std::fs::read(&new).unwrap(),
        b"one\ntwo edited\nthree\nfour\n"
    );
    assert!(!old.exists());
    assert!(repo.verify_working_copy().unwrap().is_healthy());
    let native = repo.verify_native_derived_indexes().unwrap();
    assert!(
        native.is_healthy(),
        "native problems: {:?}",
        native.problems
    );
}

#[test]
fn test_authoritative_move_preserves_inode_across_large_rewrite_and_reopen() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("before.txt");
    let new = temp.path().join("after.txt");
    let old_content = (0..200)
        .map(|line| format!("original line {line}\n"))
        .collect::<String>();
    let new_content = (0..240)
        .map(|line| format!("replacement payload {line}\n"))
        .collect::<String>();
    std::fs::write(&old, &old_content).unwrap();
    repo.add("before.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("before.txt").unwrap().unwrap();

    std::fs::rename(&old, &new).unwrap();
    repo.move_file("before.txt", "after.txt").unwrap();
    std::fs::write(&new, &new_content).unwrap();
    let outcome = record_all(&repo, "move and rewrite").unwrap();
    assert!(outcome
        .move_evidence()
        .unwrap()
        .unwrap()
        .authoritative_moves
        .iter()
        .any(|mv| mv.inode == inode));
    assert_eq!(repo.get_file_inode("after.txt").unwrap(), Some(inode));

    drop(repo);
    let reopened = Repository::open(temp.path()).unwrap();
    let working_copy = reopened.require_working_copy_id().unwrap();
    reopened.materialize(working_copy).unwrap();
    assert_eq!(reopened.get_file_inode("after.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&new).unwrap(), new_content.as_bytes());
    assert!(reopened
        .verify_working_copy(working_copy)
        .unwrap()
        .is_healthy());
}

#[test]
fn test_authoritative_move_plus_edit_survives_unrecord_and_reinsert() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("new.txt");
    let base = b"base one\nbase two\n";
    let edited = b"base one\nedited two\nnew three\n";
    std::fs::write(&old, base).unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::rename(&old, &new).unwrap();
    repo.move_file("old.txt", "new.txt").unwrap();
    std::fs::write(&new, edited).unwrap();
    let moved = record_all(&repo, "move plus edit").unwrap();
    let moved_hash = *moved.hash();

    repo.unrecord(&moved_hash, UnrecordOptions::default())
        .unwrap();
    repo.materialize().unwrap();
    assert_eq!(repo.get_file_inode("old.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&old).unwrap(), base);
    // Unrecord deliberately does not overwrite/remove user working-copy bytes.
    // Clear both materialized and retained paths before testing replay output.
    std::fs::remove_file(&old).unwrap();
    if new.exists() {
        std::fs::remove_file(&new).unwrap();
    }

    repo.reinsert_change(&moved_hash, None).unwrap();
    repo.materialize().unwrap();
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), Some(inode));
    assert_eq!(std::fs::read(&new).unwrap(), edited);
    assert!(!old.exists());
    assert!(repo.verify_working_copy().unwrap().is_healthy());
}

#[test]
fn test_unrelated_content_at_historical_path_is_not_authoritative_move() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("new.txt");
    std::fs::write(&old, "alpha beta gamma delta\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let original_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::rename(&old, &new).unwrap();
    record_all(&repo, "rename").unwrap();
    std::fs::remove_file(&new).unwrap();
    std::fs::write(&old, "completely unrelated replacement payload\n").unwrap();

    let outcome = record_all(&repo, "delete current and recreate historical path").unwrap();
    assert!(!outcome.change().hunks().iter().any(|operation| matches!(
        operation,
        GraphOp::FileMove { .. } | GraphOp::FileUndel { .. }
    )));
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|operation| matches!(operation, GraphOp::FileDel { path, .. } if path == "new.txt")));
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|operation| matches!(operation, GraphOp::FileAdd { path, .. } if path == "old.txt")));
    assert!(outcome
        .move_evidence()
        .unwrap()
        .is_none_or(|evidence| evidence.authoritative_moves.is_empty()));
    let recreated_inode = repo.get_file_inode("old.txt").unwrap().unwrap();
    assert_ne!(recreated_inode, original_inode);
}

#[test]
fn test_historical_path_is_fresh_add_when_rename_detection_is_disabled() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("new.txt");
    std::fs::write(&old, "original payload\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let original_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::rename(&old, &new).unwrap();
    record_all(&repo, "rename").unwrap();
    std::fs::remove_file(&new).unwrap();
    std::fs::write(&old, "unrelated recreated payload\n").unwrap();

    let outcome = repo
        .record(
            ChangeHeader::new("detection disabled"),
            RecordOptions::new()
                .with_all(true)
                .detect_raw_renames(false)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    assert!(!outcome.change().hunks().iter().any(|operation| matches!(
        operation,
        GraphOp::FileMove { .. } | GraphOp::FileUndel { .. }
    )));
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|operation| matches!(operation, GraphOp::FileAdd { path, .. } if path == "old.txt")));
    assert_ne!(
        repo.get_file_inode("old.txt").unwrap().unwrap(),
        original_inode
    );
}

#[test]
fn test_raw_rename_plus_edit_is_probable_not_authoritative() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let new = temp.path().join("new.txt");
    std::fs::write(&old, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::rename(&old, &new).unwrap();
    std::fs::write(&new, "alpha\nbeta changed\ngamma\ndelta\nepsilon\n").unwrap();
    let outcome = record_all(&repo, "probable move plus edit").unwrap();

    let evidence = outcome.move_evidence().unwrap().unwrap();
    assert!(evidence.authoritative_moves.is_empty());
    let probable = evidence.probable_moves.iter().next().unwrap();
    assert_eq!(probable.old_path, "old.txt");
    assert_eq!(probable.new_path, "new.txt");
    assert_eq!(probable.inode, inode);
    assert_eq!(probable.basis, MoveBasis::ContentSimilarity);
    assert!(probable.score >= PROBABLE_MOVE_THRESHOLD_BPS);
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), Some(inode));
}

#[test]
fn test_ambiguous_raw_rename_remains_delete_add_with_loss_evidence() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    let first = temp.path().join("first.txt");
    let second = temp.path().join("second.txt");
    let content = b"same content\n";
    std::fs::write(&old, content).unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let old_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    std::fs::write(&first, content).unwrap();
    std::fs::write(&second, content).unwrap();
    std::fs::remove_file(&old).unwrap();
    let outcome = record_all(&repo, "ambiguous rename candidates").unwrap();

    assert!(!outcome
        .change()
        .hunks()
        .iter()
        .any(|op| matches!(op, GraphOp::FileMove { .. })));
    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|op| matches!(op, GraphOp::FileDel { path, .. } if path == "old.txt")));
    for expected in ["first.txt", "second.txt"] {
        assert!(outcome
            .change()
            .hunks()
            .iter()
            .any(|op| matches!(op, GraphOp::FileAdd { path, .. } if path == expected)));
    }

    let evidence = outcome.move_evidence().unwrap().unwrap();
    assert!(evidence.authoritative_moves.is_empty());
    assert!(evidence.probable_moves.is_empty());
    let candidates = evidence
        .loss_notes
        .iter()
        .find_map(|loss| match loss {
            LossNote::RenameUnresolved { candidates } => Some(candidates),
            LossNote::EmptyDirectory { .. } => None,
        })
        .expect("ambiguous rename loss note");
    assert_eq!(candidates.len(), 2);

    assert_eq!(repo.get_file_inode("old.txt").unwrap(), None);
    let first_inode = repo.get_file_inode("first.txt").unwrap().unwrap();
    let second_inode = repo.get_file_inode("second.txt").unwrap().unwrap();
    assert_ne!(first_inode, old_inode);
    assert_ne!(second_inode, old_inode);
    assert_ne!(first_inode, second_inode);
    assert!(repo.verify_working_copy().unwrap().is_healthy());
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

#[test]
fn test_concurrent_renames_project_both_names_without_tree_winner() {
    let (temp, mut repo) = create_temp_repo();
    let original = temp.path().join("f.txt");
    std::fs::write(&original, "content\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();
    let inode = repo.get_file_inode("f.txt").unwrap().unwrap();

    for view in ["left", "right", "merge"] {
        repo.create_view_from(view, "dev").unwrap();
    }

    repo.switch_view("left").unwrap();
    std::fs::rename(&original, temp.path().join("left.txt")).unwrap();
    let left = record_all(&repo, "rename left").unwrap();

    repo.switch_view("right").unwrap();
    std::fs::rename(&original, temp.path().join("right.txt")).unwrap();
    let right = record_all(&repo, "rename right").unwrap();

    repo.switch_view("merge").unwrap();
    repo.insert_change(left.hash(), InsertOptions::default())
        .unwrap();
    repo.insert_change(right.hash(), InsertOptions::default())
        .unwrap();
    repo.materialize_sequential().unwrap();

    assert_eq!(
        std::fs::read(temp.path().join("left.txt")).unwrap(),
        b"content\n"
    );
    assert_eq!(
        std::fs::read(temp.path().join("right.txt")).unwrap(),
        b"content\n"
    );
    let conflicted: Vec<_> = repo
        .status(crate::status::StatusOptions::default())
        .unwrap()
        .entries()
        .iter()
        .filter(|entry| entry.status() == crate::status::FileStatus::Conflicted)
        .map(|entry| entry.path().to_string_lossy().to_string())
        .collect();
    assert!(conflicted.contains(&"left.txt".to_string()));
    assert!(conflicted.contains(&"right.txt".to_string()));
    assert!(repo.get_file_content("left.txt").is_err());
    let listed: Vec<_> = repo
        .list_conflicts()
        .unwrap()
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(listed.contains(&"left.txt".to_string()));
    assert!(listed.contains(&"right.txt".to_string()));
    assert!(repo.verify_working_copy().unwrap().is_healthy());

    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.get_path(inode).unwrap(), None);
    atomic_core::pristine::PathClaimTxnT::validate_tree_bijection(&txn).unwrap();
    drop(txn);

    repo.switch_view("left").unwrap();
    repo.materialize().unwrap();
    assert!(temp.path().join("left.txt").is_file());
    assert_eq!(
        repo.get_file_content("left.txt").unwrap().unwrap(),
        b"content\n"
    );
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

/// Internal callers can explicitly disable raw rename detection when they
/// already know every destination path. The tracked deletion must still be
/// recorded, while the unrelated untracked file remains outside the change.
#[test]
fn test_raw_rename_detection_can_be_disabled() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");

    std::fs::write(&old, "same content\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    std::fs::rename(&old, temp.path().join("new.txt")).unwrap();
    let outcome = repo
        .record(
            ChangeHeader::new("delete without rename detection"),
            RecordOptions::new()
                .with_all(true)
                .detect_raw_renames(false),
        )
        .unwrap();

    assert!(outcome
        .change()
        .hunks()
        .iter()
        .any(|op| matches!(op, GraphOp::FileDel { path, .. } if path == "old.txt")));
    assert!(!outcome
        .change()
        .hunks()
        .iter()
        .any(|op| matches!(op, GraphOp::FileMove { .. })));
    assert!(temp.path().join("new.txt").exists());
}
