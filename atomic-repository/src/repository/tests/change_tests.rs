use super::*;

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
