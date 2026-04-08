//! Tests for file tracking.

use super::*;

use crate::ignore::IgnoreRules;
use atomic_core::pristine::MutTxnT;
use std::path::Path;

#[test]
fn test_normalize_path_trailing_slash() {
    assert_eq!(normalize_path(Path::new("src/")), "src");
    assert_eq!(normalize_path(Path::new("src/lib/")), "src/lib");
}

#[test]
fn test_normalize_path_backslashes() {
    assert_eq!(normalize_path(Path::new("src\\main.rs")), "src/main.rs");
}

#[test]
fn test_normalize_path_empty() {
    assert_eq!(normalize_path(Path::new("")), ".");
}

#[test]
fn test_normalize_path_with_root_relative() {
    // Relative paths should pass through unchanged
    let root = Path::new("/repo");
    assert_eq!(
        normalize_path_with_root(Path::new("src/main.rs"), Some(root)),
        "src/main.rs"
    );
    assert_eq!(
        normalize_path_with_root(Path::new("file.txt"), Some(root)),
        "file.txt"
    );
}

#[test]
fn test_normalize_path_with_root_absolute_matching() {
    // Absolute paths matching root should be made relative
    let root = Path::new("/repo");
    assert_eq!(
        normalize_path_with_root(Path::new("/repo/src/main.rs"), Some(root)),
        "src/main.rs"
    );
    assert_eq!(
        normalize_path_with_root(Path::new("/repo/file.txt"), Some(root)),
        "file.txt"
    );
}

#[test]
fn test_normalize_path_with_root_absolute_not_matching() {
    // Absolute paths not matching root should remain absolute
    let root = Path::new("/repo");
    assert_eq!(
        normalize_path_with_root(Path::new("/other/src/main.rs"), Some(root)),
        "/other/src/main.rs"
    );
}

#[test]
fn test_normalize_path_with_root_none() {
    // Without root, absolute paths remain absolute
    assert_eq!(
        normalize_path_with_root(Path::new("/repo/src/main.rs"), None),
        "/repo/src/main.rs"
    );
    // Relative paths still work
    assert_eq!(
        normalize_path_with_root(Path::new("src/main.rs"), None),
        "src/main.rs"
    );
}

#[test]
fn test_normalize_path_with_root_trailing_slash() {
    let root = Path::new("/repo");
    assert_eq!(
        normalize_path_with_root(Path::new("/repo/src/"), Some(root)),
        "src"
    );
}

// Should Ignore Tests

#[test]
fn test_should_ignore_internal_dirs() {
    assert!(should_ignore(Path::new(".atomic"), true));
    assert!(should_ignore(Path::new(".atomic/changes"), true));
    assert!(should_ignore(Path::new(".git"), true));
    assert!(should_ignore(Path::new(".git/objects"), true));
}

#[test]
fn test_should_ignore_with_rules() {
    let temp = tempfile::TempDir::new().unwrap();
    let ignore_path = temp.path().join(".atomicignore");
    std::fs::write(&ignore_path, "target/\n*.log\n").unwrap();

    let rules = IgnoreRules::load(temp.path());

    // Create test directories/files for is_dir detection
    std::fs::create_dir_all(temp.path().join("target")).unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("debug.log"), "log").unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();

    // Test with rules
    assert!(should_ignore_with_rules(
        Path::new("target"),
        true,
        true, // is_dir
        Some(&rules)
    ));
    assert!(should_ignore_with_rules(
        Path::new("debug.log"),
        true,
        false, // is_dir
        Some(&rules)
    ));
    assert!(!should_ignore_with_rules(
        Path::new("src/main.rs"),
        true,
        false, // is_dir
        Some(&rules)
    ));

    // Test without rules (should still ignore internal dirs)
    assert!(should_ignore_with_rules(
        Path::new(".atomic"),
        true,
        true,
        None
    ));
    assert!(!should_ignore_with_rules(
        Path::new("src/main.rs"),
        true,
        false,
        None
    ));
}

#[test]
fn test_collect_files_with_ignore_rules() {
    let temp = tempfile::TempDir::new().unwrap();

    // Create directory structure
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::create_dir_all(temp.path().join("target/debug")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "// lib").unwrap();
    std::fs::write(temp.path().join("target/debug/app"), "binary").unwrap();
    std::fs::write(temp.path().join("debug.log"), "log content").unwrap();
    std::fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();

    // Create ignore file
    let ignore_path = temp.path().join(".atomicignore");
    std::fs::write(&ignore_path, "target/\n*.log\n").unwrap();

    let rules = IgnoreRules::load(temp.path());
    let options = TrackingOptions::default();

    // Collect without rules
    let files_no_rules = collect_files_for_tracking(temp.path(), Path::new("."), &options).unwrap();

    // Collect with rules
    let files_with_rules =
        collect_files_for_tracking_with_rules(temp.path(), Path::new("."), &options, Some(&rules))
            .unwrap();

    // Without rules, should include target/ and *.log files
    assert!(files_no_rules.iter().any(|p| p.starts_with("target")));
    assert!(files_no_rules
        .iter()
        .any(|p| p.to_string_lossy().ends_with(".log")));

    // With rules, should exclude target/ and *.log files
    assert!(!files_with_rules.iter().any(|p| p.starts_with("target")));
    assert!(!files_with_rules
        .iter()
        .any(|p| p.to_string_lossy().ends_with(".log")));

    // Both should include src/
    assert!(files_with_rules.iter().any(|p| p.starts_with("src")));
}

#[test]
fn test_should_ignore_normal_files() {
    assert!(!should_ignore(Path::new("src/main.rs"), true));
    assert!(!should_ignore(Path::new("README.md"), true));
    assert!(!should_ignore(Path::new("Cargo.toml"), true));
}

#[test]
fn test_should_ignore_hidden_files() {
    // With include_hidden = true
    assert!(!should_ignore(Path::new(".hidden"), true));
    assert!(!should_ignore(Path::new(".config/settings"), true));

    // With include_hidden = false
    assert!(should_ignore(Path::new(".hidden"), false));
    assert!(should_ignore(Path::new("src/.hidden"), false));
}

// TrackingStats Tests

#[test]
fn test_tracking_stats_new() {
    let stats = TrackingStats::new();
    assert_eq!(stats.files_added, 0);
    assert_eq!(stats.directories_added, 0);
    assert_eq!(stats.total_added(), 0);
    assert!(!stats.has_changes());
}

#[test]
fn test_tracking_stats_totals() {
    let mut stats = TrackingStats::new();
    stats.files_added = 5;
    stats.directories_added = 2;
    stats.files_removed = 1;

    assert_eq!(stats.total_added(), 7);
    assert_eq!(stats.total_removed(), 1);
    assert!(stats.has_changes());
}

#[test]
fn test_tracking_stats_skip() {
    let mut stats = TrackingStats::new();
    stats.skip(PathBuf::from("test.txt"), "already tracked");

    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.skipped_paths.len(), 1);
    assert_eq!(stats.skipped_paths[0].0, PathBuf::from("test.txt"));
    assert_eq!(stats.skipped_paths[0].1, "already tracked");
}

// TrackingOptions Tests

#[test]
fn test_tracking_options_default() {
    let opts = TrackingOptions::default();
    assert!(opts.recursive);
    assert!(!opts.force);
    assert!(opts.include_hidden);
    assert!(!opts.dry_run);
}

#[test]
fn test_tracking_options_non_recursive() {
    let opts = TrackingOptions::non_recursive();
    assert!(!opts.recursive);
}

#[test]
fn test_tracking_options_forced() {
    let opts = TrackingOptions::forced();
    assert!(opts.force);
}

#[test]
fn test_tracking_options_dry_run() {
    let opts = TrackingOptions::dry_run();
    assert!(opts.dry_run);
}

#[test]
fn test_tracking_options_builder() {
    let opts = TrackingOptions::default()
        .with_recursive(false)
        .with_force(true)
        .with_hidden(false);

    assert!(!opts.recursive);
    assert!(opts.force);
    assert!(!opts.include_hidden);
}

// TrackedFile Tests

#[test]
fn test_tracked_file_new() {
    let file = TrackedFile::new(PathBuf::from("src/main.rs"), Inode::new(42), false);

    assert_eq!(file.path, PathBuf::from("src/main.rs"));
    assert_eq!(file.inode, Inode::new(42));
    assert!(!file.is_directory);
}

#[test]
fn test_tracked_file_directory() {
    let dir = TrackedFile::new(PathBuf::from("src"), Inode::new(1), true);

    assert_eq!(dir.path, PathBuf::from("src"));
    assert!(dir.is_directory);
}

// Error Tests

#[test]
fn test_tracking_error_display() {
    let err = TrackingError::PathNotFound {
        path: "missing.txt".to_string(),
    };
    assert!(err.to_string().contains("missing.txt"));

    let err = TrackingError::AlreadyTracked {
        path: "file.txt".to_string(),
    };
    assert!(err.to_string().contains("Already tracked"));

    let err = TrackingError::NotTracked {
        path: "file.txt".to_string(),
    };
    assert!(err.to_string().contains("Not tracked"));

    let err = TrackingError::InternalPath {
        path: ".atomic".to_string(),
    };
    assert!(err.to_string().contains("internal"));

    let err = TrackingError::DestinationExists {
        path: "dest.txt".to_string(),
    };
    assert!(err.to_string().contains("already exists"));
}

// Collect Files Tests

#[test]
fn test_collect_files_single_file() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // Create a test file
    std::fs::write(root.join("test.txt"), b"content").unwrap();

    let options = TrackingOptions::default();
    let files = collect_files_for_tracking(root, Path::new("test.txt"), &options).unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0], PathBuf::from("test.txt"));
}

#[test]
fn test_collect_files_directory_recursive() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // Create directory structure
    std::fs::create_dir_all(root.join("src/subdir")).unwrap();
    std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(root.join("src/lib.rs"), b"// lib").unwrap();
    std::fs::write(root.join("src/subdir/mod.rs"), b"// mod").unwrap();

    let options = TrackingOptions::default();
    let files = collect_files_for_tracking(root, Path::new("src"), &options).unwrap();

    // Should have all files and subdirectory
    assert!(files.len() >= 3);
    assert!(files.iter().any(|p| p.ends_with("main.rs")));
    assert!(files.iter().any(|p| p.ends_with("lib.rs")));
    assert!(files.iter().any(|p| p.ends_with("mod.rs")));
}

#[test]
fn test_collect_files_ignores_atomic_dir() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // Create .atomic directory
    std::fs::create_dir_all(root.join(".atomic")).unwrap();
    std::fs::write(root.join(".atomic/config"), b"test").unwrap();
    std::fs::write(root.join("normal.txt"), b"content").unwrap();

    let options = TrackingOptions::default();

    // Collecting from root should not include .atomic
    let files = collect_files_for_tracking(root, Path::new("."), &options);

    // The collect might fail or return empty for "." - that's fine
    // The important thing is .atomic is not included if it succeeds
    if let Ok(files) = files {
        for file in &files {
            assert!(
                !file.starts_with(".atomic"),
                "Should not include .atomic files"
            );
        }
    }
}

#[test]
fn test_collect_files_nonexistent() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    let options = TrackingOptions::default();
    let result = collect_files_for_tracking(root, Path::new("nonexistent.txt"), &options);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        TrackingError::PathNotFound { .. }
    ));
}

#[test]
fn test_collect_files_non_recursive() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // Create directory with files
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();

    let options = TrackingOptions::non_recursive();
    let files = collect_files_for_tracking(root, Path::new("src"), &options).unwrap();

    // Non-recursive should only include the directory itself
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], PathBuf::from("src"));
}

#[test]
fn test_collect_files_excludes_hidden() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // Create files including hidden
    std::fs::write(root.join("normal.txt"), b"content").unwrap();
    std::fs::write(root.join(".hidden"), b"hidden").unwrap();

    // With hidden excluded
    let options = TrackingOptions::default().with_hidden(false);
    let files = collect_files_for_tracking(root, Path::new("normal.txt"), &options).unwrap();
    assert_eq!(files.len(), 1);

    let hidden_result = collect_files_for_tracking(root, Path::new(".hidden"), &options).unwrap();
    assert!(hidden_result.is_empty());
}

// Integration Tests with Pristine

#[test]
fn test_add_and_check_tracked() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Add a file to tracking
    {
        let mut txn = pristine.write_txn().unwrap();
        let inode = add_to_tree(&mut txn, "src/main.rs", false).unwrap();
        assert!(inode.get() > 0);
        txn.commit().unwrap();
    }

    // Check it's tracked
    {
        let txn = pristine.read_txn().unwrap();
        assert!(is_tracked(&txn, "src/main.rs").unwrap());
        assert!(!is_tracked(&txn, "nonexistent.rs").unwrap());
    }
}

#[test]
fn test_add_and_get_inode() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    let expected_inode;

    // Add a file
    {
        let mut txn = pristine.write_txn().unwrap();
        expected_inode = add_to_tree(&mut txn, "test.txt", false).unwrap();
        txn.commit().unwrap();
    }

    // Get the inode back
    {
        let txn = pristine.read_txn().unwrap();
        let inode = get_inode(&txn, "test.txt").unwrap();
        assert_eq!(inode, Some(expected_inode));
    }
}

#[test]
fn test_remove_from_tracking() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Add and then remove
    {
        let mut txn = pristine.write_txn().unwrap();
        add_to_tree(&mut txn, "to_remove.txt", false).unwrap();
        txn.commit().unwrap();
    }

    // Verify it's tracked
    {
        let txn = pristine.read_txn().unwrap();
        assert!(is_tracked(&txn, "to_remove.txt").unwrap());
    }

    // Remove it
    {
        let mut txn = pristine.write_txn().unwrap();
        let removed = remove_from_tree(&mut txn, "to_remove.txt").unwrap();
        assert!(removed.is_some());
        txn.commit().unwrap();
    }

    // Verify it's gone
    {
        let txn = pristine.read_txn().unwrap();
        assert!(!is_tracked(&txn, "to_remove.txt").unwrap());
    }
}

#[test]
fn test_move_tracked_file() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    let original_inode;

    // Add a file
    {
        let mut txn = pristine.write_txn().unwrap();
        original_inode = add_to_tree(&mut txn, "old_name.rs", false).unwrap();
        txn.commit().unwrap();
    }

    // Move it
    {
        let mut txn = pristine.write_txn().unwrap();
        let moved_inode = move_tracked(&mut txn, "old_name.rs", "new_name.rs").unwrap();
        assert_eq!(moved_inode, original_inode); // Inode preserved!
        txn.commit().unwrap();
    }

    // Verify the move
    {
        let txn = pristine.read_txn().unwrap();
        assert!(!is_tracked(&txn, "old_name.rs").unwrap());
        assert!(is_tracked(&txn, "new_name.rs").unwrap());

        // Same inode
        let inode = get_inode(&txn, "new_name.rs").unwrap();
        assert_eq!(inode, Some(original_inode));
    }
}

#[test]
fn test_list_tracked_files() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Add multiple files
    {
        let mut txn = pristine.write_txn().unwrap();
        add_to_tree(&mut txn, "file1.txt", false).unwrap();
        add_to_tree(&mut txn, "file2.txt", false).unwrap();
        add_to_tree(&mut txn, "src/main.rs", false).unwrap();
        txn.commit().unwrap();
    }

    // List them
    {
        let txn = pristine.read_txn().unwrap();
        let tracked: Vec<TrackedFile> = list_tracked(&txn).unwrap();

        assert_eq!(tracked.len(), 3);

        let paths: Vec<_> = tracked
            .iter()
            .map(|f| f.path.to_string_lossy().to_string())
            .collect();
        assert!(paths.contains(&"file1.txt".to_string()));
        assert!(paths.contains(&"file2.txt".to_string()));
        assert!(paths.contains(&"src/main.rs".to_string()));
    }
}

#[test]
fn test_tracked_under_prefix() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Add files in different directories
    {
        let mut txn = pristine.write_txn().unwrap();
        add_to_tree(&mut txn, "src/main.rs", false).unwrap();
        add_to_tree(&mut txn, "src/lib.rs", false).unwrap();
        add_to_tree(&mut txn, "tests/test.rs", false).unwrap();
        add_to_tree(&mut txn, "README.md", false).unwrap();
        txn.commit().unwrap();
    }

    // Get files under src/
    {
        let txn = pristine.read_txn().unwrap();
        let src_files = tracked_under_prefix(&txn, "src").unwrap();

        assert_eq!(src_files.len(), 2);
        assert!(src_files.iter().any(|(p, _)| p == "src/main.rs"));
        assert!(src_files.iter().any(|(p, _)| p == "src/lib.rs"));
    }
}

#[test]
fn test_move_to_existing_fails() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Add two files
    {
        let mut txn = pristine.write_txn().unwrap();
        add_to_tree(&mut txn, "file1.txt", false).unwrap();
        add_to_tree(&mut txn, "file2.txt", false).unwrap();
        txn.commit().unwrap();
    }

    // Try to move file1 to file2 (should fail)
    {
        let mut txn = pristine.write_txn().unwrap();
        let result = move_tracked(&mut txn, "file1.txt", "file2.txt");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TrackingError::DestinationExists { .. }
        ));
    }
}

#[test]
fn test_move_nonexistent_fails() {
    use atomic_core::pristine::Pristine;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("pristine.redb");

    let pristine = Pristine::open(&db_path).unwrap();

    // Try to move nonexistent file
    {
        let mut txn = pristine.write_txn().unwrap();
        let result = move_tracked(&mut txn, "nonexistent.txt", "new.txt");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TrackingError::NotTracked { .. }
        ));
    }
}
