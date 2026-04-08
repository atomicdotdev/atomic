use super::*;

// History Method Tests

#[test]
fn test_repo_history_summary_empty() {
    let (_temp_dir, repo) = create_temp_repo();

    let summary = repo.history_summary().unwrap();
    assert_eq!(summary.change_count, 0);
    assert!(summary.is_empty());
}

#[test]
fn test_repo_log_empty() {
    let (_temp_dir, repo) = create_temp_repo();

    let entries = repo.log(HistoryOptions::default()).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_repo_reverse_log_empty() {
    let (_temp_dir, repo) = create_temp_repo();

    let entries = repo.reverse_log(HistoryOptions::default()).unwrap();
    assert!(entries.is_empty());
}

// Archive Method Tests

#[test]
fn test_repo_archive_empty_fails() {
    let (_temp_dir, repo) = create_temp_repo();

    let dest = _temp_dir.path().join("archive");
    let result = repo.archive(&dest, ArchiveOptions::directory());

    // Should fail because no tracked files
    assert!(matches!(result, Err(RepositoryError::Archive(_))));
}

#[test]
fn test_repo_archive_to_directory() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and track a file
    std::fs::write(temp_dir.path().join("test.txt"), b"Hello World").unwrap();
    repo.add("test.txt", TrackingOptions::default()).unwrap();

    let dest = temp_dir.path().join("archive");
    let outcome = repo.archive(&dest, ArchiveOptions::directory()).unwrap();

    assert!(dest.exists());
    assert_eq!(outcome.manifest.file_count, 1);
    assert!(dest.join("test.txt").exists());
}

#[test]
fn test_repo_archive_with_prefix() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and track a file
    std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
    repo.add("test.txt", TrackingOptions::default()).unwrap();

    let dest = temp_dir.path().join("archive");
    let options = ArchiveOptions::directory().with_prefix("project-1.0/");
    let _outcome = repo.archive(&dest, options).unwrap();

    assert!(dest.exists());
    // The file should be at archive/project-1.0/test.txt
    assert!(dest.join("project-1.0/test.txt").exists());
}

#[test]
fn test_repo_archive_with_include_filter() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and track files
    std::fs::write(temp_dir.path().join("include.txt"), b"include").unwrap();
    std::fs::write(temp_dir.path().join("exclude.log"), b"exclude").unwrap();
    repo.add("include.txt", TrackingOptions::default()).unwrap();
    repo.add("exclude.log", TrackingOptions::default()).unwrap();

    let dest = temp_dir.path().join("archive");
    let options = ArchiveOptions::directory().include(&["*.txt"]);
    let outcome = repo.archive(&dest, options).unwrap();

    assert_eq!(outcome.manifest.file_count, 1);
    assert!(dest.join("include.txt").exists());
    assert!(!dest.join("exclude.log").exists());
}

#[test]
fn test_repo_archive_with_exclude_filter() {
    let (temp_dir, repo) = create_temp_repo();

    // Create and track files
    std::fs::write(temp_dir.path().join("keep.txt"), b"keep").unwrap();
    std::fs::write(temp_dir.path().join("remove.log"), b"remove").unwrap();
    repo.add("keep.txt", TrackingOptions::default()).unwrap();
    repo.add("remove.log", TrackingOptions::default()).unwrap();

    let dest = temp_dir.path().join("archive");
    let options = ArchiveOptions::directory().exclude(&["*.log"]);
    let outcome = repo.archive(&dest, options).unwrap();

    assert_eq!(outcome.manifest.file_count, 1);
    assert!(dest.join("keep.txt").exists());
    assert!(!dest.join("remove.log").exists());
}

#[test]
fn test_repo_archive_tag_not_found() {
    let (_temp_dir, repo) = create_temp_repo();

    let dest = _temp_dir.path().join("archive");
    let result = repo.archive_tag("nonexistent", &dest, ArchiveOptions::directory());

    assert!(matches!(result, Err(RepositoryError::TagNotFound { .. })));
}

// Insert Method Tests (basic tests - full integration needs changes)

#[test]
fn test_insert_options_default() {
    let options = InsertOptions::default();
    assert!(options.view.is_none());
    assert!(!options.apply_dependencies);
    assert!(options.allow_conflicts);
}

#[test]
fn test_insert_options_with_view() {
    let options = InsertOptions::default().view("feature");
    assert_eq!(options.view, Some("feature".to_string()));
}
