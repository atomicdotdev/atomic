//! Tests for the change store.

use super::*;
use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::types::Base32;
use tempfile::TempDir;

// Test Helpers

/// Create a temporary directory for testing.
fn create_temp_dir() -> TempDir {
    TempDir::new().expect("Failed to create temp directory")
}

/// Create a test change store in a temporary directory.
fn create_test_store() -> (ChangeStore, TempDir) {
    let temp_dir = create_temp_dir();
    let changes_dir = temp_dir.path().join("changes");
    let store =
        ChangeStore::new(changes_dir, DEFAULT_CACHE_CAPACITY).expect("Failed to create store");
    (store, temp_dir)
}

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

// LRU Cache Tests

#[test]
fn test_lru_cache_basic_operations() {
    let mut cache: LruCache<u32, String> = LruCache::new(3);

    // Insert and retrieve
    cache.insert(1, "one".to_string());
    cache.insert(2, "two".to_string());

    assert_eq!(cache.get(&1), Some(&"one".to_string()));
    assert_eq!(cache.get(&2), Some(&"two".to_string()));
    assert_eq!(cache.get(&3), None);
}

#[test]
fn test_lru_cache_eviction() {
    let mut cache: LruCache<u32, String> = LruCache::new(2);

    cache.insert(1, "one".to_string());
    cache.insert(2, "two".to_string());
    cache.insert(3, "three".to_string()); // Should evict 1

    assert_eq!(cache.get(&1), None); // Evicted
    assert_eq!(cache.get(&2), Some(&"two".to_string()));
    assert_eq!(cache.get(&3), Some(&"three".to_string()));
}

#[test]
fn test_lru_cache_access_updates_order() {
    let mut cache: LruCache<u32, String> = LruCache::new(2);

    cache.insert(1, "one".to_string());
    cache.insert(2, "two".to_string());

    // Access 1 to make it more recent
    let _ = cache.get(&1);

    // Insert 3, should evict 2 (now the LRU)
    cache.insert(3, "three".to_string());

    assert_eq!(cache.get(&1), Some(&"one".to_string())); // Still present
    assert_eq!(cache.get(&2), None); // Evicted
    assert_eq!(cache.get(&3), Some(&"three".to_string()));
}

#[test]
fn test_lru_cache_remove() {
    let mut cache: LruCache<u32, String> = LruCache::new(3);

    cache.insert(1, "one".to_string());
    assert!(cache.contains_key(&1));

    let removed = cache.remove(&1);
    assert!(removed);
    assert!(!cache.contains_key(&1));

    let removed_again = cache.remove(&1);
    assert!(!removed_again);
}

#[test]
fn test_lru_cache_update_existing() {
    let mut cache: LruCache<u32, String> = LruCache::new(2);

    cache.insert(1, "one".to_string());
    cache.insert(1, "ONE".to_string()); // Update, not new entry

    assert_eq!(cache.len(), 1);
    assert_eq!(cache.get(&1), Some(&"ONE".to_string()));
}

// ChangeStore Path Tests

#[test]
fn test_change_path_format() {
    let (store, _temp) = create_test_store();

    let hash = Hash::of(b"test content");
    let path = store.change_path(&hash);

    // Path should be: {changes_dir}/{prefix}/{full_hash}.change
    let hash_str = hash.to_base32();
    let expected_prefix = &hash_str[..2];

    assert!(path.to_string_lossy().contains(expected_prefix));
    assert!(path.to_string_lossy().ends_with(".change"));
    assert!(path.to_string_lossy().contains(&hash_str));
}

#[test]
fn test_change_path_deterministic() {
    let (store, _temp) = create_test_store();

    let hash = Hash::of(b"test");

    // Same hash should always produce same path
    let path1 = store.change_path(&hash);
    let path2 = store.change_path(&hash);

    assert_eq!(path1, path2);
}

#[test]
fn test_change_path_different_hashes_different_paths() {
    let (store, _temp) = create_test_store();

    let hash1 = Hash::of(b"content 1");
    let hash2 = Hash::of(b"content 2");

    let path1 = store.change_path(&hash1);
    let path2 = store.change_path(&hash2);

    assert_ne!(path1, path2);
}

// ChangeStore Save/Load Tests

#[test]
fn test_save_change() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test save change");
    let result = store.save_change(&change);

    assert!(result.is_ok());

    let hash = result.unwrap();

    // Verify the file was created
    let path = store.change_path(&hash);
    assert!(path.exists(), "Change file should exist at {:?}", path);
}

#[test]
fn test_load_change() {
    let (store, _temp) = create_test_store();

    // Save a change first
    let original = create_test_change("Test load change");
    let hash = store.save_change(&original).expect("Failed to save change");

    // Clear cache to force disk read
    store.clear_cache();

    // Load the change
    let loaded = store.load_change(&hash).expect("Failed to load change");

    // Verify the data matches
    assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
}

#[test]
fn test_save_load_roundtrip() {
    let (store, _temp) = create_test_store();

    let original = create_test_change_with_content(
        "Test roundtrip",
        b"Hello, this is test content for the change!",
    );

    // Save
    let hash = store.save_change(&original).expect("Failed to save change");

    // Clear cache to ensure we read from disk
    store.clear_cache();

    // Load
    let loaded = store.load_change(&hash).expect("Failed to load change");

    // Verify all fields
    assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
    assert_eq!(original.contents, loaded.contents);
    assert_eq!(
        original.hashed.header.authors.len(),
        loaded.hashed.header.authors.len()
    );
}

#[test]
fn test_copy_content_span_avoids_full_change_load_on_cache_hit() {
    let (store, _temp) = create_test_store();

    let mut original = create_test_change_with_content("Test content span", b"0123456789abcdef");
    original.unhashed = Some(serde_json::json!({
        "git": {
            "diff_lines": [
                {
                    "path": "large.rs",
                    "lines": (0..1000).map(|idx| serde_json::json!({
                        "origin": "+",
                        "content": format!("line {idx}\n"),
                        "old_lineno": null,
                        "new_lineno": idx + 1,
                    })).collect::<Vec<_>>()
                }
            ]
        }
    }));

    let hash = store.save_change(&original).expect("Failed to save change");

    let mut buf = [0u8; 4];
    let copied = store
        .copy_content_span(&hash, 4, 8, &mut buf)
        .expect("Failed to copy content span");

    assert_eq!(copied, 4);
    assert_eq!(&buf, b"4567");
    assert_eq!(store.cache_size(), 1);
}

#[test]
fn test_load_nonexistent_change() {
    let (store, _temp) = create_test_store();

    let fake_hash = Hash::of(b"nonexistent");
    let result = store.load_change(&fake_hash);

    assert!(result.is_err());
    assert!(
        result.unwrap_err().is_not_found(),
        "Should return NotFound error"
    );
}

#[test]
fn test_has_change() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test has_change");
    let hash = store.save_change(&change).expect("Failed to save change");

    // Should exist
    assert!(store.has_change(&hash));

    // Should not exist
    let fake_hash = Hash::of(b"nonexistent");
    assert!(!store.has_change(&fake_hash));
}

#[test]
fn test_has_change_from_cache() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test cache check");
    let hash = store.save_change(&change).expect("Failed to save change");

    // Change should be in cache after save
    assert!(store.cache_size() > 0);
    assert!(store.has_change(&hash));
}

// ChangeStore Delete Tests

#[test]
fn test_delete_change() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test delete change");
    let hash = store.save_change(&change).expect("Failed to save change");

    // Verify it exists
    assert!(store.has_change(&hash));

    // Delete it
    let deleted = store.delete_change(&hash).expect("Failed to delete change");
    assert!(deleted, "delete_change should return true");

    // Verify it's gone
    assert!(!store.has_change(&hash));

    // Verify the file is gone
    let path = store.change_path(&hash);
    assert!(!path.exists());
}

#[test]
fn test_delete_nonexistent_change() {
    let (store, _temp) = create_test_store();

    let fake_hash = Hash::of(b"nonexistent");
    let deleted = store
        .delete_change(&fake_hash)
        .expect("delete_change should succeed");

    assert!(!deleted, "Should return false for nonexistent change");
}

#[test]
fn test_delete_removes_from_cache() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test cache removal");
    let hash = store.save_change(&change).expect("Failed to save change");

    // Verify in cache
    assert!(store.cache_size() > 0);

    // Delete
    store.delete_change(&hash).expect("Failed to delete");

    // Load should fail (not found)
    let result = store.load_change(&hash);
    assert!(result.is_err());
    assert!(result.unwrap_err().is_not_found());
}

// ChangeStore Iteration Tests

#[test]
fn test_iter_changes_empty() {
    let (store, _temp) = create_test_store();

    let changes: Vec<_> = store.iter_changes().collect();
    assert!(changes.is_empty());
}

#[test]
fn test_iter_changes() {
    let (store, _temp) = create_test_store();

    // Save multiple changes
    let mut saved_hashes = Vec::new();
    for i in 0..5 {
        let change = create_test_change(&format!("Change {}", i));
        let hash = store.save_change(&change).expect("Failed to save change");
        saved_hashes.push(hash);
    }

    // Iterate and collect
    let found_hashes: Vec<Hash> = store.iter_changes().filter_map(|r| r.ok()).collect();

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
fn test_count_changes() {
    let (store, _temp) = create_test_store();

    // Initially empty
    assert_eq!(store.count_changes().unwrap(), 0);

    // Add some changes
    for i in 0..3 {
        let change = create_test_change(&format!("Change {}", i));
        store.save_change(&change).expect("Failed to save change");
    }

    assert_eq!(store.count_changes().unwrap(), 3);
}

// Cache Behavior Tests

#[test]
fn test_cache_hit() {
    let (store, _temp) = create_test_store();

    let change = create_test_change("Test cache hit");
    let hash = store.save_change(&change).expect("Failed to save change");

    // First load adds to cache
    let _ = store.load_change(&hash).expect("First load failed");
    assert!(store.cache_size() > 0);

    // Second load should be from cache (we can't directly verify this,
    // but we can verify it still works)
    let loaded = store.load_change(&hash).expect("Second load failed");
    assert_eq!(change.hashed.header.message, loaded.hashed.header.message);
}

#[test]
fn test_cache_eviction() {
    // Create store with small cache
    let temp_dir = create_temp_dir();
    let changes_dir = temp_dir.path().join("changes");
    let store = ChangeStore::new(changes_dir, 2).expect("Failed to create store");

    // Save 3 changes (more than cache capacity)
    let mut hashes = Vec::new();
    for i in 0..3 {
        let change = create_test_change(&format!("Change {}", i));
        let hash = store.save_change(&change).expect("Failed to save change");
        hashes.push(hash);
    }

    // Cache should have at most 2 entries
    assert!(store.cache_size() <= 2);

    // All changes should still be loadable from disk
    for hash in &hashes {
        store.clear_cache();
        let result = store.load_change(hash);
        assert!(
            result.is_ok(),
            "Should load change {} from disk",
            hash.to_base32()
        );
    }
}

// Error Condition Tests

#[test]
fn test_error_is_not_found() {
    let err = ChangeStoreError::NotFound {
        hash: "ABCDEF".to_string(),
    };
    assert!(err.is_not_found());

    let err = ChangeStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test"));
    assert!(!err.is_not_found());
}

#[test]
fn test_error_is_corruption() {
    let err = ChangeStoreError::HashMismatch {
        expected: "ABCDEF".to_string(),
        computed: "123456".to_string(),
    };
    assert!(err.is_corruption());

    let err = ChangeStoreError::NotFound {
        hash: "ABCDEF".to_string(),
    };
    assert!(!err.is_corruption());
}

#[test]
fn test_error_display() {
    let err = ChangeStoreError::NotFound {
        hash: "ABCDEF".to_string(),
    };
    assert!(err.to_string().contains("ABCDEF"));

    let err = ChangeStoreError::HashMismatch {
        expected: "EXPECTED".to_string(),
        computed: "COMPUTED".to_string(),
    };
    assert!(err.to_string().contains("EXPECTED"));
    assert!(err.to_string().contains("COMPUTED"));
}

// From Root Tests

#[test]
fn test_from_root() {
    let temp_dir = create_temp_dir();
    let root = temp_dir.path();

    let store =
        ChangeStore::from_root(root, DEFAULT_CACHE_CAPACITY).expect("Failed to create store");

    // Verify the changes directory was created in the right place
    let expected_dir = root.join(atomic_core::DOT_DIR).join("changes");
    assert_eq!(store.changes_dir(), expected_dir);
    assert!(expected_dir.exists());
}

// Multiple Changes with Same Prefix Tests

#[test]
fn test_multiple_changes_same_directory() {
    let (store, _temp) = create_test_store();

    // Save many changes (some will share directory prefixes statistically)
    let mut hashes = Vec::new();
    for i in 0..20 {
        let change = create_test_change(&format!("Change number {}", i));
        let hash = store.save_change(&change).expect("Failed to save change");
        hashes.push(hash);
    }

    // All should be retrievable
    for hash in &hashes {
        store.clear_cache();
        let result = store.load_change(hash);
        assert!(
            result.is_ok(),
            "Failed to load change {}: {:?}",
            hash.to_base32(),
            result.err()
        );
    }

    // Count should match
    assert_eq!(store.count_changes().unwrap(), 20);
}

// Content Preservation Tests

#[test]
fn test_large_content_preservation() {
    let (store, _temp) = create_test_store();

    // Create a change with large content
    let large_content: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
    let change = create_test_change_with_content("Large content test", &large_content);

    let hash = store.save_change(&change).expect("Failed to save change");

    store.clear_cache();

    let loaded = store.load_change(&hash).expect("Failed to load change");

    assert_eq!(loaded.contents.len(), large_content.len());
    assert_eq!(loaded.contents, large_content);
}

#[test]
fn test_empty_content_preservation() {
    let (store, _temp) = create_test_store();

    let change = create_test_change_with_content("Empty content test", &[]);

    let hash = store.save_change(&change).expect("Failed to save change");

    store.clear_cache();

    let loaded = store.load_change(&hash).expect("Failed to load change");

    assert!(loaded.contents.is_empty());
}

#[test]
fn test_binary_content_preservation() {
    let (store, _temp) = create_test_store();

    // Create binary content with all byte values
    let binary_content: Vec<u8> = (0..=255).collect();
    let change = create_test_change_with_content("Binary content test", &binary_content);

    let hash = store.save_change(&change).expect("Failed to save change");

    store.clear_cache();

    let loaded = store.load_change(&hash).expect("Failed to load change");

    assert_eq!(loaded.contents, binary_content);
}
