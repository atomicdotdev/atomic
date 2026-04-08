use super::*;
use crate::record::RecordOptions;
use crate::status::StatusOptions;

/// Test that status shows files as Clean after recording.
///
/// This is a regression test for the issue where files still showed
/// as Modified after being recorded because content retrieval wasn't
/// working correctly.
#[test]
fn test_status_clean_after_record() {
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

/// Test that switching views correctly outputs file content.
///
/// This test verifies that when switching between views that share
/// the same changes, the file content is preserved. A view created
/// with create_view_from inherits the source view's changes.
#[test]
fn test_switch_view_outputs_content() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Step 1: Create and record a file on the default view
    let file_path = temp_dir.path().join("switch_test.txt");
    let content = b"Content for view switch test\n";
    std::fs::write(&file_path, content).unwrap();

    repo.add("switch_test.txt", TrackingOptions::default())
        .unwrap();

    let header = ChangeHeader::new("Add file on dev view");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();

    // Step 2: Create a new view FROM dev (inherits dev's changes)
    repo.create_view_from("feature", "dev").unwrap();

    // Step 3: Switch to the new view
    let _switch_result = repo.switch_view("feature").unwrap();

    // The switch should succeed
    assert_eq!(repo.current_view(), "feature");

    // Step 4: Verify the file content is still present in working copy
    let file_content = std::fs::read(&file_path).unwrap();
    assert_eq!(
        file_content, content,
        "File content should be preserved after view switch"
    );

    // Step 5: Switch back to dev and verify content again
    let _switch_back_result = repo.switch_view("dev").unwrap();
    assert_eq!(repo.current_view(), "dev");

    let file_content_after = std::fs::read(&file_path).unwrap();
    assert_eq!(
        file_content_after, content,
        "File content should be present after switching back to dev"
    );
}

/// Test correct view switching behavior with content isolation.
///
/// This is the TDD test for how view switching SHOULD work:
/// 1. Record content on dev view
/// 2. Create feature view FROM dev (inherits dev's changes)
/// 3. Record different content on feature
/// 4. Switching between views shows each view's content
///
/// Key insight: When creating a new view, it should inherit the current
/// view's changes so that switching to it preserves the working copy state.
#[test]
fn test_switch_view_shows_view_content() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Step 1: Create and record a file on dev view
    let file_path = temp_dir.path().join("view_test.txt");
    let dev_content = b"Content on dev view\n";
    std::fs::write(&file_path, dev_content).unwrap();

    repo.add("view_test.txt", TrackingOptions::default())
        .unwrap();

    let header = ChangeHeader::new("Add file on dev");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();

    // Verify dev has 1 change
    let dev_info = repo.get_view_info("dev").unwrap();
    assert_eq!(dev_info.change_count, 1, "Dev should have 1 change");

    // Step 2: Create feature view FROM dev (should inherit dev's changes)
    repo.create_view_from("feature", "dev").unwrap();

    // Feature should now have the same changes as dev
    let feature_info = repo.get_view_info("feature").unwrap();
    assert_eq!(
        feature_info.change_count, 1,
        "Feature should inherit dev's 1 change"
    );

    // Step 3: Switch to feature - content should still be present
    repo.switch_view("feature").unwrap();

    let content_on_feature = std::fs::read(&file_path).unwrap();
    assert_eq!(
        content_on_feature, dev_content,
        "Content should be preserved when switching to feature (inherited from dev)"
    );

    // Step 4: Modify the file on feature view
    let feature_content = b"Modified content on feature view\n";
    std::fs::write(&file_path, feature_content).unwrap();

    let header = ChangeHeader::new("Modify file on feature");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();

    // Feature now has 2 changes (inherited + its own)
    let feature_info = repo.get_view_info("feature").unwrap();
    assert_eq!(
        feature_info.change_count, 2,
        "Feature should have 2 changes (inherited + modification)"
    );

    // Verify feature content in working copy
    let current_content = std::fs::read(&file_path).unwrap();
    assert_eq!(current_content, feature_content);

    // Step 5: Switch back to dev - content should revert to dev version
    repo.switch_view("dev").unwrap();

    let content_after_switch = std::fs::read(&file_path).unwrap();
    assert_eq!(
        content_after_switch, dev_content,
        "Content should revert to dev version after switching back"
    );

    // Dev still has only 1 change
    let dev_info = repo.get_view_info("dev").unwrap();
    assert_eq!(dev_info.change_count, 1, "Dev should still have 1 change");

    // Step 6: Switch to feature again - content should be feature version
    repo.switch_view("feature").unwrap();

    let feature_content_after_switch = std::fs::read(&file_path).unwrap();
    assert_eq!(
        feature_content_after_switch, feature_content,
        "Content should be feature version after switching to feature"
    );
}
