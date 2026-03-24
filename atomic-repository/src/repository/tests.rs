use super::*;

mod tests {
    use super::*;
    use atomic_core::change::{Author, Change, ChangeHeader};
    use atomic_core::types::Base32;

    use tempfile::TempDir;

    fn create_temp_repo() -> (TempDir, Repository) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        (temp_dir, repo)
    }

    #[test]
    fn test_init_creates_structure() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();

        assert!(repo.dot_dir().exists());
        assert!(repo.pristine_path().exists());
        assert!(repo.changes_dir().exists());
        assert!(repo.config_path().exists());
    }

    #[test]
    fn test_init_fails_if_exists() {
        let (temp_dir, _repo) = create_temp_repo();

        let result = Repository::init(temp_dir.path());
        assert!(matches!(result, Err(RepositoryError::AlreadyExists { .. })));
    }

    #[test]
    fn test_open_existing() {
        let (temp_dir, repo) = create_temp_repo();
        let root = repo.root().to_path_buf();

        // Drop the original repository to release the database lock
        drop(repo);

        let opened = Repository::open(temp_dir.path()).unwrap();
        assert_eq!(opened.root(), root);
        assert_eq!(opened.current_stack(), DEFAULT_STACK);
    }

    #[test]
    fn test_open_from_subdirectory() {
        let (temp_dir, repo) = create_temp_repo();
        let root = repo.root().to_path_buf();

        // Drop the original repository to release the database lock
        drop(repo);

        // Create a subdirectory
        let subdir = temp_dir.path().join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        // Open from subdirectory should find the root
        let opened = Repository::open(&subdir).unwrap();
        assert_eq!(opened.root(), root);
    }

    #[test]
    fn test_open_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let result = Repository::open(temp_dir.path());
        assert!(matches!(result, Err(RepositoryError::NotFound { .. })));
    }

    #[test]
    fn test_is_repository() {
        let (temp_dir, _repo) = create_temp_repo();

        assert!(Repository::is_repository(temp_dir.path()));

        let non_repo = TempDir::new().unwrap();
        assert!(!Repository::is_repository(non_repo.path()));
    }

    #[test]
    fn test_change_path() {
        let (_temp_dir, repo) = create_temp_repo();

        let hash = "ABCDEF123456";
        let path = repo.change_path(hash);

        assert!(path.to_string_lossy().contains("AB"));
        assert!(path.to_string_lossy().contains(hash));
    }

    #[test]
    fn test_to_relative() {
        let (temp_dir, repo) = create_temp_repo();

        let abs_path = temp_dir.path().join("src").join("main.rs");
        let rel_path = repo.to_relative(&abs_path).unwrap();

        assert_eq!(rel_path, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_to_absolute() {
        let (temp_dir, repo) = create_temp_repo();

        let rel_path = PathBuf::from("src/main.rs");
        let abs_path = repo.to_absolute(&rel_path);

        assert_eq!(abs_path, temp_dir.path().join("src/main.rs"));
    }

    #[test]
    fn test_is_internal_path() {
        let (_temp_dir, repo) = create_temp_repo();

        assert!(repo.is_internal_path(repo.dot_dir()));
        assert!(repo.is_internal_path(repo.pristine_path()));
        assert!(repo.is_internal_path(repo.changes_dir()));
        assert!(!repo.is_internal_path(repo.root().join("src")));
    }

    #[test]
    fn test_set_current_stack() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // First create the stack
        repo.create_stack("feature-stack").unwrap();

        // Then switch to it
        repo.set_current_stack("feature-stack").unwrap();
        assert_eq!(repo.current_stack(), "feature-stack");

        // Verify it persists - drop repo first to release lock
        let root = repo.root().to_path_buf();
        drop(repo);

        let reopened = Repository::open(&root).unwrap();
        assert_eq!(reopened.current_stack(), "feature-stack");
    }

    #[test]
    fn test_set_current_stack_nonexistent() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to switch to a nonexistent stack should fail
        let result = repo.set_current_stack("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_create_stack() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create a new stack
        repo.create_stack("feature").unwrap();

        // Verify it exists
        assert!(repo.stack_exists("feature").unwrap());

        // Creating the same stack again should fail
        let result = repo.create_stack("feature");
        assert!(matches!(
            result,
            Err(RepositoryError::StackAlreadyExists { .. })
        ));
    }

    #[test]
    fn test_list_stacks() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Should have default "dev" stack
        let stacks = repo.list_stacks().unwrap();
        assert!(stacks.contains(&"dev".to_string()));

        // Create additional stacks
        repo.create_stack("feature-a").unwrap();
        repo.create_stack("feature-b").unwrap();

        let stacks = repo.list_stacks().unwrap();
        assert_eq!(stacks.len(), 3);
        assert!(stacks.contains(&"dev".to_string()));
        assert!(stacks.contains(&"feature-a".to_string()));
        assert!(stacks.contains(&"feature-b".to_string()));
    }

    #[test]
    fn test_default_stack_name() {
        let (_temp_dir, repo) = create_temp_repo();
        assert_eq!(repo.current_stack(), "dev");
        assert_eq!(DEFAULT_STACK, "dev");
    }

    #[test]
    fn test_delete_stack() {
        use atomic_core::pristine::{MutTxnT, StackKind, StackTxnT};
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create an local workspace (only local workspaces can be deleted)
        {
            let mut txn = repo.pristine.write_txn().unwrap();
            let dev = txn.get_stack("dev").unwrap().unwrap();
            txn.create_stack("to-delete", StackKind::Local, Some(dev.id))
                .unwrap();
            txn.commit().unwrap();
        }
        assert!(repo.stack_exists("to-delete").unwrap());

        // Delete the stack
        repo.delete_stack("to-delete").unwrap();

        // Verify it's gone
        assert!(!repo.stack_exists("to-delete").unwrap());
    }

    #[test]
    fn test_delete_stack_nonexistent() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to delete a nonexistent stack should fail
        let result = repo.delete_stack("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_delete_current_stack_fails() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to delete the current stack should fail
        let result = repo.delete_stack("dev");
        assert!(matches!(
            result,
            Err(RepositoryError::CannotDeleteCurrentStack { .. })
        ));
    }

    #[test]
    fn test_delete_stack_preserves_others() {
        use atomic_core::pristine::{MutTxnT, StackKind, StackTxnT};
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create two local workspaces (only local workspaces can be deleted)
        {
            let mut txn = repo.pristine.write_txn().unwrap();
            let dev = txn.get_stack("dev").unwrap().unwrap();
            txn.create_stack("keep-me", StackKind::Local, Some(dev.id))
                .unwrap();
            txn.create_stack("delete-me", StackKind::Local, Some(dev.id))
                .unwrap();
            txn.commit().unwrap();
        }

        // Delete one
        repo.delete_stack("delete-me").unwrap();

        // Verify the other still exists
        assert!(repo.stack_exists("keep-me").unwrap());
        assert!(!repo.stack_exists("delete-me").unwrap());
    }

    #[test]
    fn test_get_stack_info() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create a stack
        repo.create_stack("info-test").unwrap();

        // Get info
        let info = repo.get_stack_info("info-test").unwrap();
        assert_eq!(info.name, "info-test");
        assert_eq!(info.change_count, 0);
        assert!(info.is_empty());
    }

    #[test]
    fn test_get_stack_info_nonexistent() {
        let (_temp_dir, repo) = create_temp_repo();

        // Trying to get info for a nonexistent stack should fail
        let result = repo.get_stack_info("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_stack_info_state_methods() {
        let (_temp_dir, mut repo) = create_temp_repo();

        repo.create_stack("state-test").unwrap();
        let info = repo.get_stack_info("state-test").unwrap();

        // Test state methods
        let base32 = info.state_base32();
        assert!(!base32.is_empty());

        let short = info.state_short();
        assert!(short.len() <= 12);

        // For an empty stack
        assert!(info.is_empty());
    }

    // Change Storage Tests

    /// Create a simple test change with the given message.
    fn create_test_change(message: &str) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), Vec::new(), Vec::new())
    }

    /// Create a test change with some content.
    fn create_test_change_with_content(message: &str, content: &[u8]) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), content.to_vec(), Vec::new())
    }

    #[test]
    fn test_repo_save_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test save change via repository");
        let result = repo.save_change(&change);

        assert!(result.is_ok());
        let hash = result.unwrap();

        // Verify the change exists
        assert!(repo.has_change(&hash));
    }

    #[test]
    fn test_repo_load_change() {
        let (_temp_dir, repo) = create_temp_repo();

        // Save a change first
        let original = create_test_change("Test load change via repository");
        let hash = repo.save_change(&original).expect("Failed to save change");

        // Load the change
        let loaded = repo.load_change(&hash).expect("Failed to load change");

        // Verify the data matches
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
    }

    #[test]
    fn test_repo_save_load_roundtrip() {
        let (_temp_dir, repo) = create_temp_repo();

        let original = create_test_change_with_content(
            "Test roundtrip via repository",
            b"Repository content test!",
        );

        // Save
        let hash = repo.save_change(&original).expect("Failed to save change");

        // Load
        let loaded = repo.load_change(&hash).expect("Failed to load change");

        // Verify all fields
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
        assert_eq!(original.contents, loaded.contents);
    }

    #[test]
    fn test_repo_load_nonexistent_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let fake_hash = Hash::of(b"nonexistent change");
        let result = repo.load_change(&fake_hash);

        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(RepositoryError::ChangeNotFound { .. })
        ));
    }

    #[test]
    fn test_repo_has_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test has_change via repository");
        let hash = repo.save_change(&change).expect("Failed to save change");

        // Should exist
        assert!(repo.has_change(&hash));

        // Should not exist
        let fake_hash = Hash::of(b"nonexistent");
        assert!(!repo.has_change(&fake_hash));
    }

    #[test]
    fn test_repo_delete_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test delete change via repository");
        let hash = repo.save_change(&change).expect("Failed to save change");

        // Verify it exists
        assert!(repo.has_change(&hash));

        // Delete it
        let deleted = repo.delete_change(&hash).expect("Failed to delete change");
        assert!(deleted);

        // Verify it's gone
        assert!(!repo.has_change(&hash));
    }

    #[test]
    fn test_repo_count_changes() {
        let (_temp_dir, repo) = create_temp_repo();

        // Initially empty
        assert_eq!(repo.count_changes().unwrap(), 0);

        // Add some changes
        for i in 0..3 {
            let change = create_test_change(&format!("Change {}", i));
            repo.save_change(&change).expect("Failed to save change");
        }

        assert_eq!(repo.count_changes().unwrap(), 3);
    }

    #[test]
    fn test_repo_iter_changes() {
        let (_temp_dir, repo) = create_temp_repo();

        // Save multiple changes
        let mut saved_hashes = Vec::new();
        for i in 0..5 {
            let change = create_test_change(&format!("Repository change {}", i));
            let hash = repo.save_change(&change).expect("Failed to save change");
            saved_hashes.push(hash);
        }

        // Iterate and collect
        let found_hashes: Vec<Hash> = repo.iter_changes().filter_map(|r| r.ok()).collect();

        // All saved changes should be found
        assert_eq!(found_hashes.len(), saved_hashes.len());
        for hash in &saved_hashes {
            assert!(
                found_hashes.contains(hash),
                "Should find saved hash {}",
                hash.to_base32()
            );
        }
    }

    #[test]
    fn test_repo_change_store_accessor() {
        let (_temp_dir, repo) = create_temp_repo();

        // Access the underlying change store
        let store = repo.change_store();

        // Should be able to use it directly
        assert_eq!(store.changes_dir(), repo.changes_dir());
    }

    // Status Tests

    #[test]
    fn test_repo_status_empty_repo() {
        let (_temp_dir, repo) = create_temp_repo();

        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        assert_eq!(status.stack(), "dev");
        // Empty repo should be clean
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_status_with_untracked_files() {
        let (temp_dir, repo) = create_temp_repo();

        // Create some untracked files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/main.rs"), b"fn main() {}").unwrap();

        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Should have untracked files
        assert!(status.has_untracked());
        assert_eq!(status.untracked_count(), 2);

        // But should still be "clean" (no tracked modifications)
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_status_tracked_only() {
        let (temp_dir, repo) = create_temp_repo();

        // Create some untracked files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1").unwrap();

        let status = repo
            .status(StatusOptions::tracked_only())
            .expect("status failed");

        // Should not include untracked files
        assert!(!status.has_untracked());
        assert_eq!(status.untracked_count(), 0);
    }

    #[test]
    fn test_repo_status_quick() {
        let (_temp_dir, repo) = create_temp_repo();

        // Quick status should work
        let status = repo.status_quick().expect("status_quick failed");
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_is_working_copy_clean() {
        let (_temp_dir, repo) = create_temp_repo();

        // Empty repo should be clean
        assert!(repo.is_working_copy_clean().expect("is_clean failed"));
    }

    #[test]
    fn test_repo_untracked_files() {
        let (temp_dir, repo) = create_temp_repo();

        // Create untracked files
        std::fs::write(temp_dir.path().join("new_file.txt"), b"content").unwrap();

        let untracked = repo.untracked_files().expect("untracked_files failed");

        assert_eq!(untracked.len(), 1);
        assert!(untracked.iter().any(|p| p.ends_with("new_file.txt")));
    }

    #[test]
    fn test_repo_modified_files_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        // No modified files in empty repo
        let modified = repo.modified_files().expect("modified_files failed");
        assert!(modified.is_empty());
    }

    #[test]
    fn test_repo_deleted_files_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        // No deleted files in empty repo
        let deleted = repo.deleted_files().expect("deleted_files failed");
        assert!(deleted.is_empty());
    }

    #[test]
    fn test_repo_status_ignores_atomic_dir() {
        let (_temp_dir, repo) = create_temp_repo();

        // The .atomic directory should be ignored
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // None of the .atomic files should appear
        for entry in status.entries() {
            assert!(
                !entry.path().starts_with(".atomic"),
                "Should not include .atomic directory files"
            );
        }
    }

    #[test]
    fn test_repo_ignore_rules() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(
            temp_dir.path().join(".atomicignore"),
            "target/\n*.log\n!important.log\n",
        )
        .unwrap();

        let rules = repo.ignore_rules();

        // Should ignore target directory
        assert!(rules.is_ignored(Path::new("target"), true));
        assert!(rules.is_ignored(Path::new("target/debug/app"), false));

        // Should ignore .log files
        assert!(rules.is_ignored(Path::new("debug.log"), false));
        assert!(rules.is_ignored(Path::new("logs/error.log"), false));

        // Should NOT ignore important.log (whitelisted)
        assert!(!rules.is_ignored(Path::new("important.log"), false));

        // Should NOT ignore normal files
        assert!(!rules.is_ignored(Path::new("src/main.rs"), false));
        assert!(!rules.is_ignored(Path::new("Cargo.toml"), false));
    }

    #[test]
    fn test_repo_is_ignored() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "build/\n*.tmp\n").unwrap();

        // Should ignore patterns from .atomicignore
        assert!(repo.is_ignored(Path::new("build"), true));
        assert!(repo.is_ignored(Path::new("cache.tmp"), false));

        // Should NOT ignore normal files
        assert!(!repo.is_ignored(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn test_repo_status_respects_atomicignore() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "ignored/\n*.bak\n").unwrap();

        // Create files (some should be ignored, some not)
        std::fs::create_dir_all(temp_dir.path().join("ignored")).unwrap();
        std::fs::write(temp_dir.path().join("ignored/file.txt"), b"ignored").unwrap();
        std::fs::write(temp_dir.path().join("backup.bak"), b"backup").unwrap();
        std::fs::write(temp_dir.path().join("visible.txt"), b"visible").unwrap();

        // Get status with default options (respects ignore files)
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should NOT include ignored files
        assert!(
            !paths.iter().any(|p| p.starts_with("ignored")),
            "Should not include ignored/ directory"
        );
        assert!(
            !paths.iter().any(|p| p.to_string_lossy().ends_with(".bak")),
            "Should not include .bak files"
        );

        // Should include visible.txt
        assert!(
            paths.iter().any(|p| p == Path::new("visible.txt")),
            "Should include visible.txt"
        );
    }

    #[test]
    fn test_repo_status_include_ignored() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "*.log\n").unwrap();

        // Create files
        std::fs::write(temp_dir.path().join("debug.log"), b"log").unwrap();
        std::fs::write(temp_dir.path().join("main.rs"), b"code").unwrap();

        // Get status with include_ignored = true
        let status = repo.status(StatusOptions::all()).expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should include both ignored and non-ignored files
        assert!(
            paths.iter().any(|p| p == Path::new("debug.log")),
            "Should include debug.log when include_ignored=true"
        );
        assert!(
            paths.iter().any(|p| p == Path::new("main.rs")),
            "Should include main.rs"
        );
    }

    #[test]
    fn test_repo_add_respects_atomicignore() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "ignored/\n").unwrap();

        // Create directory structure
        std::fs::create_dir_all(temp_dir.path().join("ignored")).unwrap();
        std::fs::write(temp_dir.path().join("ignored/file.txt"), b"ignored").unwrap();

        // Trying to add an ignored path should fail
        let result = repo.add("ignored", TrackingOptions::default());
        assert!(result.is_err(), "Adding ignored directory should fail");
    }

    #[test]
    fn test_repo_status_ignores_node_modules() {
        // This test mimics the real-world scenario where node_modules should be ignored
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file with "node_modules" (no trailing slash or newline issues)
        std::fs::write(temp_dir.path().join(".atomicignore"), "node_modules\n").unwrap();

        // Create node_modules directory with nested files
        std::fs::create_dir_all(temp_dir.path().join("node_modules/typescript/lib")).unwrap();
        std::fs::create_dir_all(temp_dir.path().join("node_modules/@types/node")).unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
            b"// typescript",
        )
        .unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join("node_modules/@types/node/child_process.d.ts"),
            b"// node types",
        )
        .unwrap();

        // Create some non-ignored files
        std::fs::write(temp_dir.path().join("package.json"), b"{}").unwrap();
        std::fs::write(temp_dir.path().join("index.js"), b"console.log('hello')").unwrap();

        // Verify ignore rules are loaded correctly
        let rules = repo.ignore_rules();
        assert!(rules.has_local_rules(), "Should have local rules");
        assert!(
            rules.is_ignored(Path::new("node_modules"), true),
            "node_modules directory should be ignored"
        );
        assert!(
            rules.is_ignored(
                Path::new("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
                false
            ),
            "Files in node_modules should be ignored"
        );

        // Get status with default options (respects ignore files)
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Debug output
        eprintln!("Status entries:");
        for path in &paths {
            eprintln!("  {:?}", path);
        }

        // Should NOT include any node_modules files
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules")),
            "Should not include node_modules directory in status, but found: {:?}",
            paths
                .iter()
                .filter(|p| p.starts_with("node_modules"))
                .collect::<Vec<_>>()
        );

        // Should include non-ignored files
        assert!(
            paths
                .iter()
                .any(|p| p.as_path() == Path::new("package.json")),
            "Should include package.json"
        );
        assert!(
            paths.iter().any(|p| p.as_path() == Path::new("index.js")),
            "Should include index.js"
        );
    }

    #[test]
    fn test_repo_status_ignores_node_modules_no_trailing_newline() {
        // Test the specific case where .atomicignore has no trailing newline
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore WITHOUT trailing newline (common user mistake)
        std::fs::write(temp_dir.path().join(".atomicignore"), "node_modules").unwrap();

        // Create node_modules
        std::fs::create_dir_all(temp_dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(temp_dir.path().join("node_modules/pkg/index.js"), b"module").unwrap();

        // Create non-ignored file
        std::fs::write(temp_dir.path().join("app.js"), b"app").unwrap();

        // Get status
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should NOT include node_modules
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules")),
            "node_modules should be ignored even without trailing newline"
        );

        // Should include app.js
        assert!(
            paths.iter().any(|p| p.as_path() == Path::new("app.js")),
            "Should include app.js"
        );
    }

    // File Tracking Tests

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

    // Tag Method Tests

    #[test]
    fn test_repo_create_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        let tag = repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        assert_eq!(tag.name, "v1.0.0");
        assert_eq!(tag.stack, DEFAULT_STACK);
        assert!(!tag.is_annotated());
    }

    #[test]
    fn test_repo_create_annotated_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        let options = TagOptions::default()
            .message("Release 1.0")
            .author("Alice", Some("alice@example.com"));

        let tag = repo.create_tag("v1.0.0", options).unwrap();

        assert_eq!(tag.name, "v1.0.0");
        assert!(tag.is_annotated());
        assert_eq!(tag.message(), Some("Release 1.0"));
    }

    #[test]
    fn test_repo_get_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_some());
        assert_eq!(tag.unwrap().name, "v1.0.0");
    }

    #[test]
    fn test_repo_get_tag_not_found() {
        let (_temp_dir, repo) = create_temp_repo();

        let tag = repo.get_tag("nonexistent").unwrap();
        assert!(tag.is_none());
    }

    #[test]
    fn test_repo_list_tags() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        let tags = repo.list_tags().unwrap();
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn test_repo_list_tags_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        let tags = repo.list_tags().unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_repo_list_tags_filtered() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default().message("Annotated"))
            .unwrap();
        repo.create_tag("release-1", TagOptions::default()).unwrap();

        // Filter by pattern
        let filter = TagFilter::new().pattern("v*");
        let tags = repo.list_tags_filtered(&filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter annotated only
        let filter = TagFilter::new().annotated_only();
        let tags = repo.list_tags_filtered(&filter).unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn test_repo_delete_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        assert!(repo.delete_tag("v1.0.0").unwrap());
        assert!(repo.get_tag("v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_repo_delete_tag_not_found() {
        let (_temp_dir, repo) = create_temp_repo();

        assert!(!repo.delete_tag("nonexistent").unwrap());
    }

    #[test]
    fn test_repo_tag_count() {
        let (_temp_dir, repo) = create_temp_repo();

        assert_eq!(repo.tag_count().unwrap(), 0);

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        assert_eq!(repo.tag_count().unwrap(), 2);
    }

    #[test]
    fn test_repo_create_tag_invalid_name() {
        let (_temp_dir, repo) = create_temp_repo();

        let result = repo.create_tag("", TagOptions::default());
        assert!(matches!(
            result,
            Err(RepositoryError::InvalidTagName { .. })
        ));

        let result = repo.create_tag("bad/name", TagOptions::default());
        assert!(matches!(
            result,
            Err(RepositoryError::InvalidTagName { .. })
        ));
    }

    #[test]
    fn test_repo_create_tag_already_exists() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        let result = repo.create_tag("v1.0.0", TagOptions::default());

        // Should fail because tag exists
        assert!(result.is_err());
    }

    #[test]
    fn test_repo_create_tag_force_overwrite() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Force overwrite should succeed
        let tag = repo
            .create_tag("v1.0.0", TagOptions::default().force(true))
            .unwrap();
        assert_eq!(tag.name, "v1.0.0");
    }

    #[test]
    fn test_repo_get_tag_from_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tag in current stack
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Get from current stack (default behavior)
        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_some());

        // Get from specific stack
        let tag = repo.get_tag_from_stack("v1.0.0", DEFAULT_STACK).unwrap();
        assert!(tag.is_some());

        // Get from different stack (should not exist)
        let tag = repo.get_tag_from_stack("v1.0.0", "other").unwrap();
        assert!(tag.is_none());
    }

    #[test]
    fn test_repo_list_tags_for_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tags in current stack
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // list_tags returns current stack only
        let tags = repo.list_tags().unwrap();
        assert_eq!(tags.len(), 2);

        // list_tags_for_stack with current stack
        let tags = repo.list_tags_for_stack(DEFAULT_STACK).unwrap();
        assert_eq!(tags.len(), 2);

        // list_tags_for_stack with other stack (empty)
        let tags = repo.list_tags_for_stack("other").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_repo_list_all_tags() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tags (all go to current stack since we can't easily switch)
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // list_all_tags includes all stacks
        let all_tags = repo.list_all_tags().unwrap();
        assert_eq!(all_tags.len(), 2);
    }

    #[test]
    fn test_repo_tag_count_for_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // tag_count returns count for current stack
        assert_eq!(repo.tag_count().unwrap(), 2);

        // tag_count_for_stack with specific stack
        assert_eq!(repo.tag_count_for_stack(DEFAULT_STACK).unwrap(), 2);
        assert_eq!(repo.tag_count_for_stack("other").unwrap(), 0);

        // tag_count_all returns total across all stacks
        assert_eq!(repo.tag_count_all().unwrap(), 2);
    }

    #[test]
    fn test_repo_delete_tag_from_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Delete from wrong stack should return false
        assert!(!repo.delete_tag_from_stack("v1.0.0", "other").unwrap());

        // Tag should still exist
        assert!(repo.get_tag("v1.0.0").unwrap().is_some());

        // Delete from correct stack should succeed
        assert!(repo.delete_tag_from_stack("v1.0.0", DEFAULT_STACK).unwrap());
        assert!(repo.get_tag("v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_repo_list_tag_stacks() {
        let (_temp_dir, repo) = create_temp_repo();

        // Initially no stacks have tags
        let stacks = repo.list_tag_stacks().unwrap();
        assert!(stacks.is_empty());

        // Create a tag
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Now current stack should be listed
        let stacks = repo.list_tag_stacks().unwrap();
        assert_eq!(stacks.len(), 1);
        assert!(stacks.contains(&DEFAULT_STACK.to_string()));
    }

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

    // Apply Method Tests (basic tests - full integration needs changes)

    #[test]
    fn test_apply_options_default() {
        let options = ApplyOptions::default();
        assert!(options.stack.is_none());
        assert!(!options.apply_dependencies);
        assert!(options.allow_conflicts);
    }

    #[test]
    fn test_apply_options_with_stack() {
        let options = ApplyOptions::default().stack("feature");
        assert_eq!(options.stack, Some("feature".to_string()));
    }

    // Apply Recorded Tests

    #[test]
    fn test_apply_recorded_creates_tree_entries() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create a file in the working copy
        let file_path = temp_dir.path().join("new_file.txt");
        std::fs::write(&file_path, b"Hello, Atomic!").unwrap();

        // Track and record the file
        repo.add("new_file.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add new file");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false); // Don't auto-apply, we'll test apply_recorded

        let record_outcome = repo.record(header, options).unwrap();

        // Verify the change was recorded
        assert!(record_outcome.was_saved());
        assert!(!record_outcome.was_applied());

        // Now apply using apply_recorded
        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify the apply succeeded
        assert_eq!(apply_outcome.stats.changes_applied, 1);
        assert!(!apply_outcome.has_conflicts);

        // Verify the tree entries were created
        let txn = repo.pristine.read_txn().unwrap();

        // Check path → inode mapping exists
        let inode = txn.get_inode("new_file.txt").unwrap();
        assert!(inode.is_some(), "TREE entry should exist for new_file.txt");

        // Check inode → path reverse mapping
        let inode = inode.unwrap();
        let path = txn.get_path(inode).unwrap();
        assert_eq!(path, Some("new_file.txt".to_string()));

        // Check inode → position mapping
        let position = txn.inode_position(inode).unwrap();
        assert!(position.is_some(), "INODES entry should exist");
    }

    #[test]
    fn test_apply_recorded_updates_stack_state() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        // Record without applying
        let header = ChangeHeader::new("Test change");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();

        // Get initial stack state
        let initial_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(initial_state, Merkle::ZERO);

        // Apply the change
        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify state was updated
        assert_ne!(apply_outcome.new_state, Merkle::ZERO);
        assert_eq!(apply_outcome.sequence, 1);

        // Verify stack in database reflects the change
        let final_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(final_state, apply_outcome.new_state);
    }

    #[test]
    fn test_apply_recorded_with_specific_stack() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Create another stack
        repo.create_stack("feature").unwrap();

        // Create and track a file
        std::fs::write(temp_dir.path().join("feature.txt"), b"feature content").unwrap();
        repo.add("feature.txt", TrackingOptions::default()).unwrap();

        // Record
        let header = ChangeHeader::new("Feature change");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();

        // Apply to the "feature" stack specifically
        let apply_options = ApplyOptions::default().stack("feature");
        let apply_outcome = repo.apply_recorded(&record_outcome, apply_options).unwrap();

        // Verify "feature" stack was updated
        let feature_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("feature").unwrap().unwrap();
            stack.state
        };
        assert_eq!(feature_state, apply_outcome.new_state);

        // Verify "dev" stack is still at zero
        let dev_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(dev_state, Merkle::ZERO);
    }

    #[test]
    fn test_record_stats_vertices_and_edges() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create a file with some content
        let file_path = temp_dir.path().join("hello.txt");
        std::fs::write(&file_path, b"Hello, World!\nThis is a test.\n").unwrap();

        // Track the file
        repo.add("hello.txt", TrackingOptions::default()).unwrap();

        // Record the file - this should create vertices for the new content
        let header = ChangeHeader::new("Add hello.txt");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);

        let outcome = repo.record(header, options).unwrap();

        // Verify the stats show vertices added (FileAdd creates 3: name, inode, content)
        let stats = outcome.stats();
        assert!(
            stats.vertices_added > 0,
            "Should have vertices_added > 0, got {}",
            stats.vertices_added
        );
        assert_eq!(
            stats.vertices_added, 3,
            "FileAdd should create 3 vertices (name, inode, content)"
        );
        assert!(
            stats.content_bytes > 0,
            "Should have content_bytes > 0, got {}",
            stats.content_bytes
        );
        assert_eq!(stats.files_recorded, 1);
        assert_eq!(stats.hunks_created, 1); // One FileAdd graph_op

        // Verify the display format shows the new CRDT-style output
        let display = format!("{}", stats);
        assert!(
            display.contains("vertices"),
            "Display should contain 'vertices', got: {}",
            display
        );
        assert!(
            display.contains("edges"),
            "Display should contain 'edges', got: {}",
            display
        );
        assert!(
            display.contains("bytes"),
            "Display should contain 'bytes', got: {}",
            display
        );
        // Should NOT contain old line-based format
        assert!(
            !display.contains("insertions"),
            "Display should NOT contain 'insertions', got: {}",
            display
        );
        assert!(
            !display.contains("deletions"),
            "Display should NOT contain 'deletions', got: {}",
            display
        );
    }

    // Edit GraphOp Tests - TDD for proper modified file handling
    //
    // These tests verify that modified files generate proper Edit hunks
    // instead of full FileAdd replacements. Edit hunks are more efficient
    // because they only record the changed content, not the entire file.
    //
    // In Atomic's graph model:
    // - FileAdd creates 3 vertices: name, inode, content (full file)
    // - Edit creates 1 span per insertion (just the new content)
    // - Deletions create EdgeUpdate atoms to mark old content as deleted
    //
    // The expected behavior for a modified file:
    // 1. Retrieve old content from the graph
    // 2. Diff old vs new content
    // 3. Create Edit hunks for insertions (new vertices)
    // 4. Create Replacement hunks for deletions (edge modifications)

    /// Test that modifying a file creates Edit hunks, not FileAdd.
    ///
    /// This is the core test for proper edit support. When a tracked file
    /// is modified, we should:
    /// 1. Detect it as Modified (not Added)
    /// 2. Retrieve the old content from the graph
    /// 3. Diff the old and new content
    /// 4. Create Edit/Replacement hunks (not FileAdd)
    ///
    /// Edit hunks are more efficient because they only store the delta,
    /// not the entire file content.
    #[test]
    fn test_modified_file_creates_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, b"line1\nline2\nline3\n").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial commit");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let initial_outcome = repo.record(header, options).unwrap();

        // Verify initial change has FileAdd graph_op
        let initial_change = initial_outcome.change();
        assert_eq!(
            initial_change.hunks().len(),
            1,
            "Initial commit should have exactly 1 graph_op"
        );
        assert!(
            matches!(initial_change.hunks()[0], GraphOp::FileAdd { .. }),
            "Initial commit should have FileAdd graph_op, got {:?}",
            initial_change.hunks()[0].type_name()
        );

        // Step 2: Modify the file (change middle line)
        std::fs::write(&file_path, b"line1\nmodified_line2\nline3\n").unwrap();

        // Step 3: Record the modification
        let header2 = ChangeHeader::new("Edit middle line");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let edit_outcome = repo.record(header2, options2).unwrap();

        // Step 4: Verify the change contains Edit or Replacement hunks, NOT FileAdd
        let edit_change = edit_outcome.change();
        assert!(
            !edit_change.hunks().is_empty(),
            "Edit commit should have at least one graph_op"
        );

        // Check that we got Edit/Replacement hunks, not FileAdd
        for graph_op in edit_change.hunks() {
            let hunk_type = graph_op.type_name();
            assert!(
                hunk_type == "Edit" || hunk_type == "Replacement",
                "Modified file should create Edit or Replacement graph_op, got {}",
                hunk_type
            );
        }

        // Verify stats reflect edit operations (fewer vertices than FileAdd)
        let stats = edit_outcome.stats();
        assert!(
            stats.vertices_added < 3,
            "Edit should create fewer than 3 vertices (FileAdd creates 3), got {}",
            stats.vertices_added
        );
    }

    /// Test that adding lines to a file creates Edit hunks for the new content.
    ///
    /// When lines are added to an existing file, we should create Edit hunks
    /// that contain only the new content, not the entire file.
    #[test]
    fn test_adding_lines_creates_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file
        let file_path = temp_dir.path().join("growing.txt");
        std::fs::write(&file_path, b"first line\n").unwrap();
        repo.add("growing.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Add more lines to the file
        std::fs::write(&file_path, b"first line\nsecond line\nthird line\n").unwrap();

        // Record the addition
        let header2 = ChangeHeader::new("Add lines");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify we got Edit hunks
        let change = outcome.change();
        for graph_op in change.hunks() {
            assert!(
                matches!(graph_op, GraphOp::Edit { .. } | GraphOp::Replacement { .. }),
                "Adding lines should create Edit/Replacement graph_op, got {}",
                graph_op.type_name()
            );
        }

        // The new content should only include the added lines
        let stats = outcome.stats();
        assert!(
            stats.content_bytes > 0,
            "Should have recorded some content bytes"
        );
    }

    /// Test that deleting lines creates edge modifications (Replacement hunks).
    ///
    /// When lines are removed from a file, we mark the old content as deleted
    /// using EdgeUpdate atoms (wrapped in Replacement hunks).
    #[test]
    fn test_deleting_lines_creates_replacement_hunks() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file with multiple lines
        let file_path = temp_dir.path().join("shrinking.txt");
        std::fs::write(&file_path, b"keep this\ndelete this\nalso keep\n").unwrap();
        repo.add("shrinking.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial with 3 lines");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Delete the middle line
        std::fs::write(&file_path, b"keep this\nalso keep\n").unwrap();

        // Record the deletion
        let header2 = ChangeHeader::new("Delete middle line");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify we got Replacement or Edit hunks (deletions use EdgeUpdate)
        let change = outcome.change();
        assert!(
            !change.hunks().is_empty(),
            "Deletion should create at least one graph_op"
        );

        // Stats should show edge modifications for deletions
        let stats = outcome.stats();
        assert!(
            stats.edges_modified > 0 || stats.vertices_added > 0,
            "Deletion should modify edges or add vertices, got edges={}, vertices={}",
            stats.edges_modified,
            stats.vertices_added
        );
    }

    /// Test that replacing content creates both deletion and insertion operations.
    ///
    /// When content is replaced (old text removed, new text added), we should
    /// see both edge modifications (for deleted content) and new vertices
    /// (for inserted content).
    #[test]
    fn test_replacing_content_creates_mixed_hunks() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file
        let file_path = temp_dir.path().join("replace.txt");
        std::fs::write(&file_path, b"old content here\n").unwrap();
        repo.add("replace.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Replace with completely different content
        std::fs::write(&file_path, b"new content here\n").unwrap();

        // Record the replacement
        let header2 = ChangeHeader::new("Replace content");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify the change was recorded
        assert_eq!(outcome.stats().files_recorded, 1);

        // For a replacement, we expect both vertices (new content) and edges (deletions)
        let stats = outcome.stats();
        assert!(
            stats.vertices_added > 0,
            "Replacement should add vertices for new content"
        );
    }

    /// Test that the old content is correctly retrieved from the graph.
    ///
    /// This tests the integration between record and get_file_content.
    /// The old content must be retrieved to compute the diff.
    #[test]
    fn test_old_content_retrieved_for_diff() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file with specific content
        let file_path = temp_dir.path().join("retrieve.txt");
        let original_content = b"This is the original content\nWith multiple lines\n";
        std::fs::write(&file_path, original_content).unwrap();
        repo.add("retrieve.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify we can retrieve the content from the graph
        let retrieved = repo.get_file_content(std::path::Path::new("retrieve.txt"));
        assert!(
            retrieved.is_ok(),
            "Should be able to retrieve file content: {:?}",
            retrieved.err()
        );
        let retrieved_content = retrieved.unwrap();
        assert!(
            retrieved_content.is_some(),
            "Retrieved content should not be None"
        );
        assert_eq!(
            retrieved_content.unwrap(),
            original_content.to_vec(),
            "Retrieved content should match original"
        );

        // Now modify and record - this should use the retrieved content for diff
        std::fs::write(
            &file_path,
            b"This is MODIFIED content\nWith multiple lines\n",
        )
        .unwrap();

        let header2 = ChangeHeader::new("Modify first line");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // The modification should have been recorded
        assert_eq!(outcome.stats().files_recorded, 1);
    }

    /// Test recording multiple modified files in one change.
    ///
    /// When multiple files are modified, each should get proper Edit hunks.
    #[test]
    fn test_multiple_modified_files_get_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record multiple files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1\n").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), b"content2\n").unwrap();
        repo.add("file1.txt", TrackingOptions::default()).unwrap();
        repo.add("file2.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial two files");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Modify both files
        std::fs::write(temp_dir.path().join("file1.txt"), b"modified1\n").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), b"modified2\n").unwrap();

        // Record both modifications
        let header2 = ChangeHeader::new("Modify both files");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Both files should be recorded
        assert_eq!(
            outcome.stats().files_recorded,
            2,
            "Should record 2 modified files"
        );

        // All hunks should be Edit or Replacement (not FileAdd)
        let change = outcome.change();
        for graph_op in change.hunks() {
            assert!(
                matches!(graph_op, GraphOp::Edit { .. } | GraphOp::Replacement { .. }),
                "Modified files should use Edit/Replacement hunks, got {}",
                graph_op.type_name()
            );
        }
    }

    /// Test that stats correctly reflect Edit operations vs FileAdd.
    ///
    /// Edit operations should show:
    /// - vertices_added: 1 per insertion (not 3 like FileAdd)
    /// - edges_modified: count of deletion operations
    #[test]
    fn test_edit_stats_are_accurate() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record a file
        let file_path = temp_dir.path().join("stats.txt");
        std::fs::write(&file_path, b"original\n").unwrap();
        repo.add("stats.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let initial = repo.record(header, options).unwrap();

        // Initial FileAdd should have 3 vertices
        assert_eq!(
            initial.stats().vertices_added,
            3,
            "FileAdd should create 3 vertices (name, inode, content)"
        );

        // Modify the file
        std::fs::write(&file_path, b"modified\n").unwrap();

        let header2 = ChangeHeader::new("Edit");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let edit = repo.record(header2, options2).unwrap();

        // Edit should have fewer vertices (just the new content, not name/inode)
        let edit_stats = edit.stats();
        assert!(
            edit_stats.vertices_added <= 2,
            "Edit should create at most 2 vertices (1 for new content, possibly 1 for context), got {}",
            edit_stats.vertices_added
        );

        // Edit that replaces content should also modify edges
        assert!(
            edit_stats.vertices_added > 0 || edit_stats.edges_modified > 0,
            "Edit should either add vertices or modify edges"
        );
    }

    /// Test that status shows files as Clean after recording.
    ///
    /// This is a regression test for the issue where files still showed
    /// as Modified after being recorded because content retrieval wasn't
    /// working correctly.
    #[test]
    fn test_status_clean_after_record() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record a new file
        let file_path = temp_dir.path().join("status_test.txt");
        let content = b"Initial content for status test\n";
        std::fs::write(&file_path, content).unwrap();

        repo.add("status_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add status test file");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Check status - file should be Clean (not Modified)
        let status = repo.status(StatusOptions::default()).unwrap();

        // The file should NOT appear as modified
        let modified_files: Vec<_> = status.modified().collect();
        assert!(
            modified_files.is_empty(),
            "No files should be modified after recording, but got: {:?}",
            modified_files.iter().map(|e| e.path()).collect::<Vec<_>>()
        );

        // The file should be Clean
        let clean_files: Vec<_> = status.clean().collect();
        assert!(
            clean_files
                .iter()
                .any(|e| e.path().to_string_lossy().contains("status_test.txt")),
            "status_test.txt should be Clean after recording"
        );

        // Step 3: Verify the recorded content matches the file
        let retrieved = repo.get_file_content("status_test.txt").unwrap();
        assert!(
            retrieved.is_some(),
            "Should be able to retrieve recorded content"
        );
        assert_eq!(
            retrieved.unwrap(),
            content.to_vec(),
            "Retrieved content should match original file"
        );
    }

    /// Test that status correctly detects modifications after initial record.
    #[test]
    fn test_status_modified_after_change() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("modify_test.txt");
        std::fs::write(&file_path, b"Initial content\n").unwrap();

        repo.add("modify_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Modify the file
        std::fs::write(&file_path, b"Modified content\n").unwrap();

        // Step 3: Check status - file should be Modified now
        let status = repo.status(StatusOptions::default()).unwrap();

        let modified_files: Vec<_> = status.modified().collect();
        assert_eq!(modified_files.len(), 1, "One file should be modified");
        assert!(
            modified_files[0]
                .path()
                .to_string_lossy()
                .contains("modify_test.txt"),
            "modify_test.txt should be Modified"
        );
    }

    /// Test modifying the FIRST line of a file.
    ///
    /// This is a regression test for a bug where modifying the first line of a
    /// file caused the unchanged lines to be lost. The bug was in `globalize_hunk`
    /// which used `content` (graph_op content) instead of `full_content` (full file)
    /// for Replace hunks.
    ///
    /// See: https://github.com/atomic-vcs/atomic/issues/XXX
    #[test]
    fn test_modify_first_line_content_retrieval() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create a file with 2 lines and record it
        let file_path = temp_dir.path().join("first_line_test.txt");
        let initial_content = b"Line 1 - original\nLine 2 - unchanged\n";
        std::fs::write(&file_path, initial_content).unwrap();

        repo.add("first_line_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file with two lines");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify initial content can be retrieved
        let retrieved1 = repo.get_file_content("first_line_test.txt").unwrap();
        assert!(
            retrieved1.is_some(),
            "Initial content should be retrievable"
        );
        assert_eq!(retrieved1.unwrap(), initial_content.to_vec());

        // Step 2: Modify ONLY the first line
        let modified_content = b"Line 1 - MODIFIED\nLine 2 - unchanged\n";
        std::fs::write(&file_path, modified_content).unwrap();

        // Step 3: Check status - should show as Modified
        let status1 = repo.status(StatusOptions::default()).unwrap();
        let modified_files: Vec<_> = status1.modified().collect();
        assert_eq!(modified_files.len(), 1, "File should show as modified");

        // Step 4: Record the modification (this creates a Replacement graph_op)
        let header2 = ChangeHeader::new("Modify first line only");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header2, options2).unwrap();

        // Step 5: Verify content retrieval returns the FULL modified file
        // (This was the bug - it only returned the first line, losing line 2)
        let retrieved2 = repo.get_file_content("first_line_test.txt").unwrap();
        assert!(
            retrieved2.is_some(),
            "Content should be retrievable after modifying first line"
        );
        assert_eq!(
            retrieved2.unwrap(),
            modified_content.to_vec(),
            "Retrieved content should match the full modified file (including unchanged line 2)"
        );

        // Step 6: Check status - should be Clean now
        let status2 = repo.status(StatusOptions::default()).unwrap();
        let modified_after: Vec<_> = status2.modified().collect();
        assert!(
            modified_after.is_empty(),
            "File should be Clean after recording the edit, but got Modified"
        );
    }

    /// Test full workflow: record → modify → record → status should be clean.
    #[test]
    fn test_status_clean_after_modify_and_record() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("workflow_test.txt");
        std::fs::write(&file_path, b"Version 1\n").unwrap();

        repo.add("workflow_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial version");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Modify the file
        let modified_content = b"Version 2 - modified\n";
        std::fs::write(&file_path, modified_content).unwrap();

        // Verify it shows as modified
        let status = repo.status(StatusOptions::default()).unwrap();
        assert!(
            status
                .modified()
                .any(|e| e.path().to_string_lossy().contains("workflow_test.txt")),
            "File should be Modified after modification"
        );

        // Step 3: Record the modification
        let header2 = ChangeHeader::new("Modified version");
        let options2 = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify the modification was recorded
        assert_eq!(
            outcome.stats().files_recorded,
            1,
            "Should have recorded 1 file"
        );

        // Step 4: Check status - should be clean now
        let status = repo.status(StatusOptions::default()).unwrap();

        let modified_files: Vec<_> = status.modified().collect();
        assert!(
            modified_files.is_empty(),
            "No files should be modified after recording the modification, but got: {:?}",
            modified_files.iter().map(|e| e.path()).collect::<Vec<_>>()
        );

        // Step 5: Verify the recorded content is the modified version
        let retrieved = repo.get_file_content("workflow_test.txt").unwrap();
        assert!(retrieved.is_some(), "Should be able to retrieve content");
        assert_eq!(
            retrieved.unwrap(),
            modified_content.to_vec(),
            "Retrieved content should be the modified version"
        );
    }

    #[test]
    fn test_apply_recorded_hash_matches() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("hash_test.txt"), b"hash test content").unwrap();
        repo.add("hash_test.txt", TrackingOptions::default())
            .unwrap();

        // Record and apply
        let header = ChangeHeader::new("Hash test");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();
        let expected_hash = *record_outcome.hash();

        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify the hash is in the applied hashes
        assert!(apply_outcome.stats.applied_hashes.contains(&expected_hash));
    }

    /// Test that switching stacks correctly outputs file content.
    ///
    /// This test verifies that when switching between stacks that share
    /// the same changes, the file content is preserved. A stack created
    /// with create_stack_from inherits the source stack's changes.
    #[test]
    fn test_switch_stack_outputs_content() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Step 1: Create and record a file on the default stack
        let file_path = temp_dir.path().join("switch_test.txt");
        let content = b"Content for stack switch test\n";
        std::fs::write(&file_path, content).unwrap();

        repo.add("switch_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file on dev stack");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Create a new stack FROM dev (inherits dev's changes)
        repo.create_stack_from("feature", "dev").unwrap();

        // Step 3: Switch to the new stack
        let _switch_result = repo.switch_stack("feature").unwrap();

        // The switch should succeed
        assert_eq!(repo.current_stack(), "feature");

        // Step 4: Verify the file content is still present in working copy
        let file_content = std::fs::read(&file_path).unwrap();
        assert_eq!(
            file_content, content,
            "File content should be preserved after stack switch"
        );

        // Step 5: Switch back to dev and verify content again
        let _switch_back_result = repo.switch_stack("dev").unwrap();
        assert_eq!(repo.current_stack(), "dev");

        let file_content_after = std::fs::read(&file_path).unwrap();
        assert_eq!(
            file_content_after, content,
            "File content should be present after switching back to dev"
        );
    }

    /// Test correct stack switching behavior with content isolation.
    ///
    /// This is the TDD test for how stack switching SHOULD work:
    /// 1. Record content on dev stack
    /// 2. Create feature stack FROM dev (inherits dev's changes)
    /// 3. Record different content on feature
    /// 4. Switching between stacks shows each stack's content
    ///
    /// Key insight: When creating a new stack, it should inherit the current
    /// stack's changes so that switching to it preserves the working copy state.
    #[test]
    fn test_switch_stack_shows_stack_content() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Step 1: Create and record a file on dev stack
        let file_path = temp_dir.path().join("stack_test.txt");
        let dev_content = b"Content on dev stack\n";
        std::fs::write(&file_path, dev_content).unwrap();

        repo.add("stack_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file on dev");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify dev has 1 change
        let dev_info = repo.get_stack_info("dev").unwrap();
        assert_eq!(dev_info.change_count, 1, "Dev should have 1 change");

        // Step 2: Create feature stack FROM dev (should inherit dev's changes)
        repo.create_stack_from("feature", "dev").unwrap();

        // Feature should now have the same changes as dev
        let feature_info = repo.get_stack_info("feature").unwrap();
        assert_eq!(
            feature_info.change_count, 1,
            "Feature should inherit dev's 1 change"
        );

        // Step 3: Switch to feature - content should still be present
        repo.switch_stack("feature").unwrap();

        let content_on_feature = std::fs::read(&file_path).unwrap();
        assert_eq!(
            content_on_feature, dev_content,
            "Content should be preserved when switching to feature (inherited from dev)"
        );

        // Step 4: Modify the file on feature stack
        let feature_content = b"Modified content on feature stack\n";
        std::fs::write(&file_path, feature_content).unwrap();

        let header = ChangeHeader::new("Modify file on feature");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Feature now has 2 changes (inherited + its own)
        let feature_info = repo.get_stack_info("feature").unwrap();
        assert_eq!(
            feature_info.change_count, 2,
            "Feature should have 2 changes (inherited + modification)"
        );

        // Verify feature content in working copy
        let current_content = std::fs::read(&file_path).unwrap();
        assert_eq!(current_content, feature_content);

        // Step 5: Switch back to dev - content should revert to dev version
        repo.switch_stack("dev").unwrap();

        let content_after_switch = std::fs::read(&file_path).unwrap();
        assert_eq!(
            content_after_switch, dev_content,
            "Content should revert to dev version after switching back"
        );

        // Dev still has only 1 change
        let dev_info = repo.get_stack_info("dev").unwrap();
        assert_eq!(dev_info.change_count, 1, "Dev should still have 1 change");

        // Step 6: Switch to feature again - content should be feature version
        repo.switch_stack("feature").unwrap();

        let feature_content_after_switch = std::fs::read(&file_path).unwrap();
        assert_eq!(
            feature_content_after_switch, feature_content,
            "Content should be feature version after switching to feature"
        );
    }
}
