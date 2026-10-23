//! The semantic closure memo must answer a foreign deletion exactly like a
//! fresh fold of the closure, fold each visible change once per process,
//! and extend by exactly the newly visible changes.

use super::*;
use crate::record::RecordOptions;

fn record_all(repo: &TestRepository, message: &str) -> crate::record::RecordOutcome {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
}

fn folded_changes(repo: &TestRepository) -> usize {
    repo.semantic_closure_memo.lock().unwrap().folded_len()
}

#[test]
fn foreign_deletion_domain_is_stable_across_memo_reuse() {
    let (temp, repo) = create_temp_repo();
    let file = temp.path().join("f.txt");
    std::fs::write(&file, b"a\nb\nc\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "one");
    std::fs::write(&file, b"a\nB\nc\nd\n").unwrap();
    record_all(&repo, "two");

    // Cold: both changes are folded exactly once.
    let cold = repo.record_foreign_deleted_file("f.txt").unwrap();
    let cold_ops = cold.crdt_ops().cloned().expect("semantic deletion");
    assert!(
        cold_ops.line_ops().len() >= 4,
        "every observed line is tombstoned: {cold_ops:?}"
    );
    let folded = folded_changes(&repo);
    assert!(folded >= 2, "folded {folded} changes");

    // Warm: the same deletion domain, with nothing re-folded.
    let warm = repo.record_foreign_deleted_file("f.txt").unwrap();
    assert_eq!(warm.crdt_ops(), Some(&cold_ops));
    assert_eq!(folded_changes(&repo), folded);

    // A later change extends the memo by exactly that change and leaves
    // the earlier file's domain unchanged.
    std::fs::write(temp.path().join("g.txt"), b"other\n").unwrap();
    repo.add("g.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "three");
    let extended = repo.record_foreign_deleted_file("f.txt").unwrap();
    assert_eq!(extended.crdt_ops(), Some(&cold_ops));
    assert_eq!(folded_changes(&repo), folded + 1);
}

#[test]
fn memo_restarts_when_the_closure_key_changes() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), b"a\nb\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "one");
    std::fs::write(temp.path().join("g.txt"), b"other\n").unwrap();
    repo.add("g.txt", TrackingOptions::default()).unwrap();
    let two = record_all(&repo, "two");

    let cold = repo.record_foreign_deleted_file("f.txt").unwrap();
    let cold_ops = cold.crdt_ops().cloned().expect("semantic deletion");
    let full = folded_changes(&repo);
    assert!(full >= 2, "folded {full} changes");

    // Excluding the unrelated head change is a different closure key: the
    // memo starts over, folds only what stays visible, and f.txt's domain
    // is unchanged.
    let view = repo.current_view.clone();
    let excluded = repo
        .record_foreign_deleted_file_on_view_excluding("f.txt", &view, &[*two.hash()])
        .unwrap();
    assert_eq!(excluded.crdt_ops(), Some(&cold_ops));
    let partial = folded_changes(&repo);
    assert!(
        partial < full,
        "memo restarted on the exclusion key: {partial} < {full}"
    );

    // Back on the plain key the memo starts over again and folds the full
    // closure.
    let again = repo.record_foreign_deleted_file("f.txt").unwrap();
    assert_eq!(again.crdt_ops(), Some(&cold_ops));
    assert_eq!(folded_changes(&repo), full);
}
