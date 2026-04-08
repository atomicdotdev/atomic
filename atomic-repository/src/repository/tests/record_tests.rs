use super::*;
use crate::record::RecordOptions;

#[test]
fn test_write_recorded_creates_tree_entries() {
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
        .apply_after_record(false); // Don't auto-apply, we'll test write_recorded

    let record_outcome = repo.record(header, options).unwrap();

    // Verify the change was recorded
    assert!(record_outcome.was_saved());
    assert!(!record_outcome.was_applied());

    // Now apply using write_recorded
    let apply_outcome = repo
        .write_recorded(&record_outcome, InsertOptions::default())
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
fn test_write_recorded_updates_view_state() {
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

    // Get initial view state
    let initial_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        view.state
    };
    assert_eq!(initial_state, Merkle::ZERO);

    // Apply the change
    let apply_outcome = repo
        .write_recorded(&record_outcome, InsertOptions::default())
        .unwrap();

    // Verify state was updated
    assert_ne!(apply_outcome.new_state, Merkle::ZERO);
    assert_eq!(apply_outcome.sequence, 1);

    // Verify view in database reflects the change
    let final_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        view.state
    };
    assert_eq!(final_state, apply_outcome.new_state);
}

#[test]
fn test_write_recorded_with_specific_view() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Create another view
    repo.create_view("feature").unwrap();

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

    // Apply to the "feature" view specifically
    let apply_options = InsertOptions::default().view("feature");
    let apply_outcome = repo.write_recorded(&record_outcome, apply_options).unwrap();

    // Verify "feature" view was updated
    let feature_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view("feature").unwrap().unwrap();
        view.state
    };
    assert_eq!(feature_state, apply_outcome.new_state);

    // Verify "dev" view is still at zero
    let dev_state = {
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view("dev").unwrap().unwrap();
        view.state
    };
    assert_eq!(dev_state, Merkle::ZERO);
}

#[test]
fn test_record_stats_vertices_and_edges() {
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

#[test]
fn test_write_recorded_hash_matches() {
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
        .write_recorded(&record_outcome, InsertOptions::default())
        .unwrap();

    // Verify the hash is in the applied hashes
    assert!(apply_outcome.stats.applied_hashes.contains(&expected_hash));
}
