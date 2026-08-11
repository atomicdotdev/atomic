//! Tests for `Repository::verify_working_copy` (the `atomic doctor check`
//! engine).

use super::*;
use crate::record::{RecordError, RecordOptions};
use crate::repository::VerifyProblem;
use atomic_core::change::ChangeHeader;
use atomic_core::types::Hash;

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

/// A clean, freshly-recorded working copy verifies healthy.
#[test]
fn verify_clean_working_copy_is_healthy() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join("f.txt"), "line1\nline2\nline3\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    let report = repo.verify_working_copy().unwrap();
    assert!(
        report.is_healthy(),
        "expected healthy, got problems: {:?}",
        report.problems
    );
    assert!(report.clean_files_checked >= 1);
}

/// A genuine conflict is NOT a verification problem: markers on disk, status
/// Conflicted, and list_conflicts all agree (the honesty invariant holds).
#[test]
fn verify_conflict_is_honest_not_a_problem() {
    let (temp, mut repo) = create_temp_repo();
    let file = temp.path().join("f.txt");
    std::fs::write(&file, "line1\nline2\nline3\nline4\nline5\n").unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    std::fs::write(&file, "line1\nAAA\nline2\nline3\nline4\nline5\n").unwrap();
    let a = record_all(&repo, "a").unwrap();
    let hash = *a.hash();

    repo.switch_view("dev").unwrap();
    std::fs::write(&file, "line1\nBBB\nline2\nline3\nline4\nline5\n").unwrap();
    record_all(&repo, "b").unwrap();

    repo.insert_change_rec(
        &hash,
        crate::apply::InsertOptions::default().apply_deps(true),
    )
    .unwrap();
    repo.materialize().unwrap();

    // Precondition: this really is a conflict on disk.
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains(">>>>>>>"), "expected a real conflict");

    let report = repo.verify_working_copy().unwrap();
    assert!(
        report.is_healthy(),
        "a self-consistent conflict must not be a problem, got: {:?}",
        report.problems
    );
    assert_eq!(report.conflicted_files, 1);
}

/// Silent materialization drift — a file `status` considers clean whose
/// on-disk bytes differ from the graph — is caught.
///
/// We defeat the status fast-path by writing same-length different content
/// and pointing FILE_INDEX at the new (mtime, size) but the OLD content hash,
/// so `status` skips hashing and reports the file clean while the bytes lie.
#[test]
fn verify_detects_silent_materialization_drift() {
    let (temp, repo) = create_temp_repo();
    let file = temp.path().join("f.txt");
    let original = b"hello\nworld\n";
    std::fs::write(&file, original).unwrap();
    repo.add("f.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "base").unwrap();

    // Corrupt on disk with SAME-length content.
    let corrupted = b"HELLO\nWORLD\n";
    assert_eq!(original.len(), corrupted.len());
    std::fs::write(&file, corrupted).unwrap();

    // Make FILE_INDEX claim the file is clean: new stat, but OLD content hash.
    let meta = std::fs::metadata(&file).unwrap();
    let mtime = meta.modified().unwrap();
    let dur = mtime
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();
    repo.update_file_index(&[(
        "f.txt".to_string(),
        dur.as_secs() as i64,
        dur.subsec_nanos(),
        meta.len(),
        Hash::of(original),
    )])
    .unwrap();

    let report = repo.verify_working_copy().unwrap();
    assert!(
        report.problems.iter().any(|p| matches!(
            p,
            VerifyProblem::MaterializationDrift { path, .. } if path == "f.txt"
        )),
        "expected drift on f.txt to be caught, got: {:?}",
        report.problems
    );
    assert!(!report.is_healthy());
}
