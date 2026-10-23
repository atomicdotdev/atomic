use super::*;

#[test]
fn test_repo_add_file() {
    let (temp_dir, repo) = create_temp_repo();

    // Create a file
    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();

    // Add it to tracking
    let stats = repo.add("test.txt", TrackingOptions::default()).unwrap();

    assert_eq!(stats.files_added, 1);
    assert!(repo.is_tracked("test.txt").unwrap());
}

#[test]
fn test_repo_add_directory() {
    let (temp_dir, repo) = create_temp_repo();

    // Create directory with files
    std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    std::fs::write(temp_dir.path().join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(temp_dir.path().join("src/lib.rs"), b"// lib").unwrap();

    // Add directory recursively
    let stats = repo.add("src", TrackingOptions::default()).unwrap();

    assert!(stats.files_added >= 2);
    assert!(repo.is_tracked("src/main.rs").unwrap());
    assert!(repo.is_tracked("src/lib.rs").unwrap());
}

#[test]
fn test_repo_add_already_tracked() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and add a file
    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
    repo.add("test.txt", TrackingOptions::default()).unwrap();

    // Adding again should succeed but skip
    let stats = repo.add("test.txt", TrackingOptions::default()).unwrap();
    assert_eq!(stats.files_added, 0);
    assert_eq!(stats.skipped, 1);
}

#[test]
fn test_repo_add_dry_run() {
    let (temp_dir, repo) = create_temp_repo();

    // Create a file
    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();

    // Dry run should not actually add
    let stats = repo.add("test.txt", TrackingOptions::dry_run()).unwrap();

    assert_eq!(stats.files_added, 1);
    assert!(!repo.is_tracked("test.txt").unwrap()); // Not actually tracked
}

#[test]
fn test_repo_remove_file() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and add a file
    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
    repo.add("test.txt", TrackingOptions::default()).unwrap();
    assert!(repo.is_tracked("test.txt").unwrap());

    // Remove from tracking
    let stats = repo.remove("test.txt", TrackingOptions::default()).unwrap();

    assert_eq!(stats.files_removed, 1);
    assert!(!repo.is_tracked("test.txt").unwrap());
}

#[test]
fn test_repo_remove_not_tracked() {
    let (_temp_dir, repo) = create_temp_repo();

    // Removing non-tracked file should error
    let result = repo.remove("nonexistent.txt", TrackingOptions::default());
    assert!(result.is_err());

    // With force, it should succeed
    let stats = repo
        .remove("nonexistent.txt", TrackingOptions::forced())
        .unwrap();
    assert_eq!(stats.files_removed, 0);
}

#[test]
fn test_repo_move_file() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and add a file
    std::fs::write(temp_dir.path().join("old.txt"), b"content").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    let original_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

    // Move the file on disk
    std::fs::rename(
        temp_dir.path().join("old.txt"),
        temp_dir.path().join("new.txt"),
    )
    .unwrap();

    // Update tracking
    let inode = repo.move_file("old.txt", "new.txt").unwrap();

    // Inode should be preserved
    assert_eq!(inode, original_inode);
    assert!(!repo.is_tracked("old.txt").unwrap());
    assert!(repo.is_tracked("new.txt").unwrap());
}

#[test]
fn test_repo_list_tracked_files() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and add files
    std::fs::write(temp_dir.path().join("file1.txt"), b"1").unwrap();
    std::fs::write(temp_dir.path().join("file2.txt"), b"2").unwrap();
    repo.add("file1.txt", TrackingOptions::default()).unwrap();
    repo.add("file2.txt", TrackingOptions::default()).unwrap();

    let tracked = repo.list_tracked_files().unwrap();

    assert_eq!(tracked.len(), 2);
}

#[test]
fn test_repo_tracked_file_count() {
    let (temp_dir, repo) = create_temp_repo();

    assert_eq!(repo.tracked_file_count().unwrap(), 0);

    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
    repo.add("test.txt", TrackingOptions::default()).unwrap();

    assert_eq!(repo.tracked_file_count().unwrap(), 1);
}

#[test]
fn test_repo_tracked_files_under() {
    let (temp_dir, repo) = create_temp_repo();

    // Create files in different directories
    std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    std::fs::create_dir_all(temp_dir.path().join("tests")).unwrap();
    std::fs::write(temp_dir.path().join("src/main.rs"), b"main").unwrap();
    std::fs::write(temp_dir.path().join("src/lib.rs"), b"lib").unwrap();
    std::fs::write(temp_dir.path().join("tests/test.rs"), b"test").unwrap();

    repo.add("src", TrackingOptions::default()).unwrap();
    repo.add("tests", TrackingOptions::default()).unwrap();

    let src_files = repo.tracked_files_under("src").unwrap();

    // Should only have files under src/
    assert!(src_files.len() >= 2);
    for (path, _) in &src_files {
        assert!(
            path.starts_with("src/"),
            "Expected src/ prefix, got: {}",
            path
        );
    }
}

/// Recording a path moves its name binding out of tracking's reach.
///
/// Regression for the defect where `remove` retired the `INODES`/`REV_INODES`
/// binding `record` had created while the recorded change's `PATH_CLAIMS` event
/// stayed visible. The next name projection could not resolve its own claim and
/// failed with `path claim '<path>' has no inode for graph position <pos>`,
/// which errored `status` and refused every later `record`.
#[test]
fn remove_defers_recorded_paths_without_touching_their_binding() {
    let (temp_dir, repo) = create_temp_repo();
    std::fs::write(temp_dir.path().join("Cargo.lock"), b"a\nb\nc\n").unwrap();
    repo.add("Cargo.lock", TrackingOptions::default()).unwrap();
    repo.record_all("record the lockfile").unwrap();
    let inode = repo
        .get_file_inode("Cargo.lock")
        .unwrap()
        .expect("recorded path is tracked");

    let stats = repo
        .remove("Cargo.lock", TrackingOptions::default())
        .unwrap();

    // Nothing was untracked; the caller learns the path needs a recorded delete.
    assert_eq!(
        stats.files_removed, 0,
        "a recorded path is not tracking's to untrack"
    );
    assert_eq!(
        stats.pending_deletions,
        vec![std::path::PathBuf::from("Cargo.lock")]
    );

    // The binding and the claim are intact, so name projection still resolves.
    // Before the fix this call errored: "path claim 'Cargo.lock' has no inode
    // for graph position 3:5".
    assert!(repo.is_tracked("Cargo.lock").unwrap());
    assert_eq!(repo.get_file_inode("Cargo.lock").unwrap(), Some(inode));
    let status = repo.status_tracked().unwrap();
    assert!(
        status
            .entries()
            .iter()
            .all(|entry| entry.path() != std::path::Path::new("Cargo.lock")),
        "an untouched recorded path must not read as changed: {:?}",
        status.entries()
    );
    let report = repo.verify_working_copy().unwrap();
    assert!(report.is_healthy(), "derived indexes drifted: {report:?}");
}

/// Deleting the working-copy file and recording the delete is the untrack.
#[test]
fn recorded_delete_is_what_untracks_a_recorded_path() {
    let (temp_dir, repo) = create_temp_repo();
    std::fs::write(temp_dir.path().join("Cargo.lock"), b"a\nb\nc\n").unwrap();
    repo.add("Cargo.lock", TrackingOptions::default()).unwrap();
    repo.record_all("record the lockfile").unwrap();

    let stats = repo
        .remove("Cargo.lock", TrackingOptions::default())
        .unwrap();
    assert!(stats.has_pending_deletions());

    std::fs::remove_file(temp_dir.path().join("Cargo.lock")).unwrap();
    // The deletion is what status can see, which is how `git rm` behaves too.
    let status = repo.status_tracked().unwrap();
    assert!(
        status
            .get(std::path::Path::new("Cargo.lock"))
            .is_some_and(|entry| entry.status() == FileStatus::Deleted),
        "the removed path should read as a pending deletion: {:?}",
        status.entries()
    );

    repo.record_all("stop tracking the lockfile").unwrap();

    assert!(
        !repo.is_tracked("Cargo.lock").unwrap(),
        "the recorded delete should have untracked the path"
    );
    let report = repo.verify_working_copy().unwrap();
    assert!(report.is_healthy(), "{:?}", report);
}

/// A staged path is exactly what tracking owns: `remove` untracks it outright and
/// reports nothing pending.
#[test]
fn remove_untracks_staged_paths_with_no_pending_deletion() {
    let (temp_dir, repo) = create_temp_repo();
    std::fs::write(temp_dir.path().join("draft.txt"), b"notes\n").unwrap();
    repo.add("draft.txt", TrackingOptions::default()).unwrap();

    let stats = repo
        .remove("draft.txt", TrackingOptions::default())
        .unwrap();

    assert_eq!(stats.files_removed, 1);
    assert!(
        stats.pending_deletions.is_empty(),
        "a never-recorded path needs no recorded delete"
    );
    assert!(!repo.is_tracked("draft.txt").unwrap());
    // The bytes are the caller's concern, not the repository's.
    assert!(temp_dir.path().join("draft.txt").exists());
}

/// `remove_batch` must skip recorded bindings for the same reason `remove` does.
#[test]
fn remove_batch_skips_recorded_paths() {
    let (temp_dir, repo) = create_temp_repo();
    std::fs::write(temp_dir.path().join("Cargo.lock"), b"a\nb\nc\n").unwrap();
    repo.add("Cargo.lock", TrackingOptions::default()).unwrap();
    repo.record_all("record the lockfile").unwrap();

    // Added after the record, so it stays staged: the only path the batch owns.
    std::fs::write(temp_dir.path().join("draft.txt"), b"notes\n").unwrap();
    repo.add("draft.txt", TrackingOptions::default()).unwrap();

    let removed = repo
        .remove_batch(&["draft.txt", "Cargo.lock"])
        .expect("batch remove");

    assert_eq!(
        removed, 1,
        "only the staged path is tracking's to untrack in a batch"
    );
    assert!(!repo.is_tracked("draft.txt").unwrap());
    assert!(repo.is_tracked("Cargo.lock").unwrap());
    let report = repo.verify_working_copy().unwrap();
    assert!(report.is_healthy(), "{:?}", report);
}
