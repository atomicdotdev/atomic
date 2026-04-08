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
