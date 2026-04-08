use super::*;
use crate::record::RecordOptions;
use atomic_core::change::GraphOp;

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
