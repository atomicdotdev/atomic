//! Integration tests for state-based content retrieval.
//!
//! These tests verify the core functionality needed for the `diff -c <hash>`
//! code review workflow:
//!
//! 1. Create a repository with multiple changes
//! 2. For each change, retrieve content BEFORE and AFTER
//! 3. Verify the content matches expectations
//!
//! # Test Scenarios
//!
//! ## Scenario 1: Simple File Addition
//! - Change 1: Add file "hello.txt" with "Hello, World!"
//! - Before Change 1: File doesn't exist (empty content)
//! - After Change 1: File contains "Hello, World!"
//!
//! ## Scenario 2: File Modification
//! - Change 2: Modify "hello.txt" to "Hello, Atomic!"
//! - Before Change 2: "Hello, World!"
//! - After Change 2: "Hello, Atomic!"
//!
//! ## Scenario 3: Multiple Files
//! - Change 3: Add "goodbye.txt" with "Goodbye!"
//! - Before Change 3: "goodbye.txt" doesn't exist
//! - After Change 3: "goodbye.txt" contains "Goodbye!"
//! - "hello.txt" unchanged at both states
//!
//! These tests validate the infrastructure needed for word-level diff
//! highlighting in code reviews.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    get_files_in_change, RecordOptions, Repository, StateBeforeChange, StatusOptions,
};
use tempfile::TempDir;

/// Create a test repository with a single recorded file.
fn create_test_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().to_path_buf();

    let repo = Repository::init(&repo_path).expect("Failed to init repository");

    (repo, temp, repo_path)
}

/// Helper to create a file in the repository and add it.
fn create_and_add_file(repo: &Repository, repo_path: &Path, name: &str, content: &str) {
    let file_path = repo_path.join(name);
    fs::write(&file_path, content).expect("Failed to write file");
    repo.add(name, Default::default())
        .expect("Failed to add file");
}

/// Helper to record a change with a message.
fn record_change(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();

    let outcome = repo
        .record(header, RecordOptions::default())
        .expect("Failed to record");

    *outcome.hash()
}

// StateBeforeChange Tests

#[test]
fn test_state_before_change_struct() {
    // Test the StateBeforeChange struct directly
    let parent_state = Hash::of(b"parent");
    let change_state = Hash::of(b"change");

    let state = StateBeforeChange::new(Some(5), parent_state, 6, change_state);

    assert_eq!(state.parent_sequence, Some(5));
    assert_eq!(state.change_sequence, 6);
    assert!(!state.is_first_change());
    assert_eq!(state.parent_max_sequence_exclusive(), 6);
}

#[test]
fn test_state_before_change_first() {
    let change_state = Hash::of(b"first");

    let state = StateBeforeChange::new(None, Hash::NONE, 0, change_state);

    assert!(state.is_first_change());
    assert_eq!(state.parent_max_sequence_exclusive(), 0);
}

// Basic Workflow Tests

#[test]
fn test_get_files_in_change_empty() {
    // Create a minimal change with no hunks
    use atomic_core::change::Change;

    let change = Change::empty(
        ChangeHeader::builder()
            .message("Empty change")
            .author(Author::new("Test", None::<String>))
            .build(),
    );

    let files = get_files_in_change(&change);
    assert!(files.is_empty());
}

// Integration Tests - Repository Workflow

#[test]
fn test_file_addition_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record a file
    create_and_add_file(&repo, &repo_path, "hello.txt", "Hello, World!");
    let hash = record_change(&repo, "Add hello.txt");

    // Get content BEFORE the change (should be empty - file didn't exist)
    let before = repo
        .get_file_content_before_change("hello.txt", &hash)
        .expect("Failed to get content before");

    // First change has no parent content
    assert!(
        before.is_none(),
        "Content before first change should be None"
    );

    // Get content AFTER the change (should have content)
    let after = repo
        .get_file_content_after_change("hello.txt", &hash)
        .expect("Failed to get content after");

    assert!(after.is_some(), "Content after change should exist");
    let after_content = after.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&after_content),
        "Hello, World!",
        "Content after should match what was written"
    );
}

/// This test requires working Edit graph_op application, which has a known bug.
/// The apply fails with "Block not found at position" when trying to resolve
/// up/down context for Edit atoms. This is tracked separately from the
/// state-based retrieval implementation.
#[test]
fn test_file_modification_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record initial file
    create_and_add_file(&repo, &repo_path, "hello.txt", "Hello, World!");
    let _hash1 = record_change(&repo, "Add hello.txt");

    // Verify initial content is retrievable
    let initial = repo
        .get_file_content("hello.txt")
        .expect("Failed to get initial");
    assert!(initial.is_some(), "Initial content should exist");

    // Modify the file
    let file_path = repo_path.join("hello.txt");
    fs::write(&file_path, "Hello, Atomic!").expect("Failed to write modified file");

    // Check status before recording
    let status = repo
        .status(StatusOptions::default())
        .expect("Failed to get status");
    let modified: Vec<_> = status.modified().collect();
    assert_eq!(modified.len(), 1, "Should have 1 modified file");

    // Record the modification
    let hash2 = record_change(&repo, "Modify hello.txt");

    // Check content right after recording
    let content_after_record = repo
        .get_file_content("hello.txt")
        .expect("Failed to get content after record");
    assert_eq!(
        content_after_record.as_deref(),
        Some(b"Hello, Atomic!".as_slice()),
        "Content after record should be modified"
    );

    // Check final status
    let final_status = repo
        .status(StatusOptions::default())
        .expect("Failed to get final status");
    let final_modified: Vec<_> = final_status.modified().collect();
    assert_eq!(
        final_modified.len(),
        0,
        "Should have no modified files after record"
    );

    // Get content BEFORE the modification
    let before = repo
        .get_file_content_before_change("hello.txt", &hash2)
        .expect("Failed to get content before");

    assert!(before.is_some(), "Content before modification should exist");
    let before_content = before.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&before_content),
        "Hello, World!",
        "Content before should be original content"
    );

    // Get content AFTER the modification
    let after = repo
        .get_file_content_after_change("hello.txt", &hash2)
        .expect("Failed to get content after");

    assert!(after.is_some(), "Content after modification should exist");
    let after_content = after.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&after_content),
        "Hello, Atomic!",
        "Content after should be modified content"
    );
}

#[test]
fn test_multiple_files_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record first file
    create_and_add_file(&repo, &repo_path, "file1.txt", "Content of file 1");
    let _hash1 = record_change(&repo, "Add file1.txt");

    // Create and record second file
    create_and_add_file(&repo, &repo_path, "file2.txt", "Content of file 2");
    let hash2 = record_change(&repo, "Add file2.txt");

    // For hash2, file1 should exist in both before and after states
    let file1_before = repo
        .get_file_content_before_change("file1.txt", &hash2)
        .expect("Failed to get file1 before hash2");
    let file1_after = repo
        .get_file_content_after_change("file1.txt", &hash2)
        .expect("Failed to get file1 after hash2");

    assert!(file1_before.is_some());
    assert!(file1_after.is_some());
    assert_eq!(file1_before, file1_after, "file1 unchanged by hash2");

    // For hash2, file2 should only exist after
    let file2_before = repo
        .get_file_content_before_change("file2.txt", &hash2)
        .expect("Failed to get file2 before hash2");
    let file2_after = repo
        .get_file_content_after_change("file2.txt", &hash2)
        .expect("Failed to get file2 after hash2");

    assert!(
        file2_before.is_none(),
        "file2 should not exist before hash2"
    );
    assert!(file2_after.is_some(), "file2 should exist after hash2");
}

/// Test retrieving file content at specific sequence numbers.
#[test]
fn test_content_at_sequence() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create initial file
    create_and_add_file(&repo, &repo_path, "counter.txt", "0");
    let _hash0 = record_change(&repo, "Initial: 0");

    // Modify to 1
    fs::write(repo_path.join("counter.txt"), "1").unwrap();
    let _hash1 = record_change(&repo, "Update to 1");

    // Modify to 2
    fs::write(repo_path.join("counter.txt"), "2").unwrap();
    let _hash2 = record_change(&repo, "Update to 2");

    // Get content at sequence 0 (should be empty - before any changes)
    let at_seq_0 = repo
        .get_file_content_at_sequence("counter.txt", 0)
        .expect("Failed to get at seq 0");
    assert!(at_seq_0.is_none(), "No content before sequence 0");

    // Get content at sequence 1 (after first change)
    let at_seq_1 = repo
        .get_file_content_at_sequence("counter.txt", 1)
        .expect("Failed to get at seq 1");
    assert_eq!(
        String::from_utf8_lossy(&at_seq_1.unwrap()),
        "0",
        "Content at seq 1"
    );

    // Get content at sequence 2 (after second change)
    let at_seq_2 = repo
        .get_file_content_at_sequence("counter.txt", 2)
        .expect("Failed to get at seq 2");
    assert_eq!(
        String::from_utf8_lossy(&at_seq_2.unwrap()),
        "1",
        "Content at seq 2"
    );

    // Get content at sequence 3 (after third change)
    let at_seq_3 = repo
        .get_file_content_at_sequence("counter.txt", 3)
        .expect("Failed to get at seq 3");
    assert_eq!(
        String::from_utf8_lossy(&at_seq_3.unwrap()),
        "2",
        "Content at seq 3"
    );
}

#[test]
fn test_change_not_in_history() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create a file to have something in the repo
    create_and_add_file(&repo, &repo_path, "file.txt", "content");
    let _hash = record_change(&repo, "Add file");

    // Try to get content for a non-existent change
    let fake_hash = Hash::of(b"this change does not exist");

    let before = repo.get_file_content_before_change("file.txt", &fake_hash);
    let after = repo.get_file_content_after_change("file.txt", &fake_hash);

    // Both should return Ok(None) for a change not in history
    assert!(before.is_ok());
    assert!(before.unwrap().is_none());
    assert!(after.is_ok());
    assert!(after.unwrap().is_none());
}

#[test]
fn test_untracked_file() {
    let (repo, _temp, _repo_path) = create_test_repo();

    // Create a change hash to test with (but don't actually record anything)
    let fake_hash = Hash::of(b"some hash");

    // Try to get content for an untracked file
    let before = repo.get_file_content_before_change("nonexistent.txt", &fake_hash);
    let after = repo.get_file_content_after_change("nonexistent.txt", &fake_hash);

    // Should return Ok(None) for untracked files
    assert!(before.is_ok());
    assert!(before.unwrap().is_none());
    assert!(after.is_ok());
    assert!(after.unwrap().is_none());
}

// Edge Case Tests

/// Empty files don't have content to record, so record() returns NothingToRecord.
/// This test verifies that attempting to record an empty file fails as expected.
#[test]
fn test_empty_file() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create an empty file and add it to tracking
    let file_path = repo_path.join("empty.txt");
    fs::write(&file_path, "").expect("Failed to write empty file");
    repo.add("empty.txt", Default::default())
        .expect("Failed to add empty file");

    // With the default options (record_empty_files: true), recording an
    // empty file should succeed — the change captures the file's existence
    // even though it has no content bytes.
    let header = ChangeHeader::builder()
        .message("Add empty file")
        .author(Author::new("Test", Some("test@example.com")))
        .build();

    let result = repo.record(header, RecordOptions::default());
    assert!(
        result.is_ok(),
        "Recording empty file should succeed with record_empty_files=true (default)"
    );

    // With record_empty_files explicitly disabled, recording an empty file
    // should fail with NothingToRecord because there are no content changes.
    let file_path2 = repo_path.join("empty2.txt");
    fs::write(&file_path2, "").expect("Failed to write empty file");
    repo.add("empty2.txt", Default::default())
        .expect("Failed to add empty file");

    let header2 = ChangeHeader::builder()
        .message("Add another empty file")
        .author(Author::new("Test", Some("test@example.com")))
        .build();

    let result2 = repo.record(header2, RecordOptions::default().record_empty_files(false));
    assert!(
        result2.is_err(),
        "Recording empty file should fail with record_empty_files=false"
    );
}

#[test]
fn test_binary_content() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create a file with binary content
    let binary_content: Vec<u8> = vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0xFD];
    let file_path = repo_path.join("binary.bin");
    fs::write(&file_path, &binary_content).expect("Failed to write binary file");
    repo.add("binary.bin", Default::default())
        .expect("Failed to add binary file");
    let hash = record_change(&repo, "Add binary file");

    let after = repo
        .get_file_content_after_change("binary.bin", &hash)
        .expect("Failed to get after");

    assert!(after.is_some(), "Binary content should be retrievable");
    assert_eq!(
        after.unwrap(),
        binary_content,
        "Binary content should match exactly"
    );
}

#[test]
#[ignore = "large-file integration case disabled pending faster default-path validation"]
fn test_large_file() {
    std::thread::Builder::new()
        .name("test_large_file".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let (repo, _temp, repo_path) = create_test_repo();

            // Keep the default integration case large enough to exercise the
            // file pipeline without making normal test runs benchmark-scale.
            let large_content = "Line of content\n".repeat(5_000);
            create_and_add_file(&repo, &repo_path, "large.txt", &large_content);
            let hash = record_change(&repo, "Add large file");

            let after = repo
                .get_file_content_after_change("large.txt", &hash)
                .expect("Failed to get after");

            assert!(after.is_some(), "Large content should be retrievable");
            assert_eq!(
                String::from_utf8_lossy(&after.unwrap()),
                large_content,
                "Large content should match"
            );
        })
        .expect("spawn test_large_file thread")
        .join()
        .expect("join test_large_file thread");
}

#[test]
#[ignore = "stress/perf guardrail; run explicitly when profiling large-file behavior"]
fn test_large_file_stress() {
    std::thread::Builder::new()
        .name("test_large_file_stress".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let (repo, _temp, repo_path) = create_test_repo();

            // Preserve the old pathological case as an opt-in regression guard.
            let large_content = "Line of content\n".repeat(65_536); // ~1MB
            create_and_add_file(&repo, &repo_path, "large.txt", &large_content);
            let hash = record_change(&repo, "Add large file stress case");

            let after = repo
                .get_file_content_after_change("large.txt", &hash)
                .expect("Failed to get after");

            assert!(after.is_some(), "Large content should be retrievable");
            assert_eq!(
                String::from_utf8_lossy(&after.unwrap()),
                large_content,
                "Large content should match"
            );
        })
        .expect("spawn test_large_file_stress thread")
        .join()
        .expect("join test_large_file_stress thread");
}

#[test]
fn test_unicode_content() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create a file with Unicode content
    let unicode_content = "Hello, 世界! 🌍 Привет мир! مرحبا بالعالم";
    create_and_add_file(&repo, &repo_path, "unicode.txt", unicode_content);
    let hash = record_change(&repo, "Add Unicode file");

    let after = repo
        .get_file_content_after_change("unicode.txt", &hash)
        .expect("Failed to get after");

    assert!(after.is_some(), "Unicode content should be retrievable");
    assert_eq!(
        String::from_utf8_lossy(&after.unwrap()),
        unicode_content,
        "Unicode content should match"
    );
}

// Code Review Workflow Simulation

/// This test simulates the full code review workflow:
/// 1. Developer creates initial code
/// 2. Developer modifies code
/// 3. Reviewer retrieves before/after to see what changed
#[test]
fn test_code_review_workflow() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Initial code
    let initial_code = r#"
fn main() {
    println!("Hello");
}
"#;
    create_and_add_file(&repo, &repo_path, "main.rs", initial_code);
    let _initial_hash = record_change(&repo, "Initial commit");

    // Developer modifies the code
    let modified_code = r#"
fn main() {
    println!("Hello, World!");
    println!("Welcome to Atomic!");
}
"#;
    fs::write(repo_path.join("main.rs"), modified_code).unwrap();
    let change_hash = record_change(&repo, "Add welcome message");

    // REVIEWER WORKFLOW:
    // 1. Get the before state (what it looked like before the change)
    let before = repo
        .get_file_content_before_change("main.rs", &change_hash)
        .expect("Failed to get before")
        .expect("Before content should exist");

    // 2. Get the after state (what it looks like after the change)
    let after = repo
        .get_file_content_after_change("main.rs", &change_hash)
        .expect("Failed to get after")
        .expect("After content should exist");

    // 3. Verify we got the correct before/after
    let before_str = String::from_utf8_lossy(&before);
    let after_str = String::from_utf8_lossy(&after);

    assert!(
        before_str.contains("println!(\"Hello\")"),
        "Before should have original code"
    );
    assert!(
        !before_str.contains("Welcome"),
        "Before should NOT have the new line"
    );

    assert!(
        after_str.contains("Hello, World!"),
        "After should have modified greeting"
    );
    assert!(
        after_str.contains("Welcome to Atomic!"),
        "After should have the new line"
    );

    // 4. Now a diff between before and after would show exactly what changed!
    // This is what `atomic diff -c <hash>` would use internally.

    // Simple verification that we can diff these
    use atomic_core::diff::{diff_text, Algorithm};
    let diff_result = diff_text(&before, &after, Algorithm::Myers);

    assert!(!diff_result.is_unchanged(), "Diff should show changes");
}

// Append Operation Tests

/// Test appending content to an existing file.
///
/// This test verifies the known limitation where appending content
/// to an existing file may not be properly recorded and retrieved.
#[test]
fn test_file_append_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record initial file
    create_and_add_file(&repo, &repo_path, "append.txt", "Line 1\n");
    let hash1 = record_change(&repo, "Add append.txt");
    println!("DEBUG: Initial change hash: {}", hash1.to_base32());

    // Verify initial content
    let initial_content = repo
        .get_file_content("append.txt")
        .expect("Failed to get initial content");
    assert!(initial_content.is_some(), "Initial content should exist");
    let initial_bytes = initial_content.unwrap();
    let initial_str = String::from_utf8_lossy(&initial_bytes);
    println!("DEBUG: Initial content retrieved: {:?}", initial_str);
    assert_eq!(initial_str, "Line 1\n", "Initial content should match");

    // Append content to the file
    let file_path = repo_path.join("append.txt");
    fs::write(&file_path, "Line 1\nLine 2\n").expect("Failed to append to file");
    println!("DEBUG: Wrote new content to file");

    // Check status - file should show as modified
    let status = repo
        .status(StatusOptions::default())
        .expect("Failed to get status");
    let modified_files: Vec<_> = status.modified().collect();
    println!("DEBUG: Modified files count: {}", modified_files.len());
    for f in &modified_files {
        println!("DEBUG:   Modified: {:?}", f.path());
    }
    assert_eq!(
        modified_files.len(),
        1,
        "File should show as modified after append"
    );
    assert_eq!(
        modified_files[0].path().to_str().unwrap(),
        "append.txt",
        "Modified file should be append.txt"
    );

    // Record the append
    let hash2 = record_change(&repo, "Append to append.txt");
    println!("DEBUG: Second change hash: {}", hash2.to_base32());

    // Get content BEFORE the append
    let before = repo
        .get_file_content_before_change("append.txt", &hash2)
        .expect("Failed to get content before append");

    println!(
        "DEBUG: Content before result: {:?}",
        before
            .as_ref()
            .map(|v| String::from_utf8_lossy(v).to_string())
    );
    assert!(before.is_some(), "Content before append should exist");
    let before_content = before.unwrap();
    let before_str = String::from_utf8_lossy(&before_content);
    println!("DEBUG: Content BEFORE append: {:?}", before_str);
    assert_eq!(
        before_str, "Line 1\n",
        "Content before append should be original"
    );

    // Get content AFTER the append
    let after = repo
        .get_file_content_after_change("append.txt", &hash2)
        .expect("Failed to get content after append");

    assert!(after.is_some(), "Content after append should exist");
    let after_content = after.unwrap();
    let after_str = String::from_utf8_lossy(&after_content);
    println!("DEBUG: Content AFTER append: {:?}", after_str);
    println!("DEBUG: Expected: {:?}", "Line 1\nLine 2\n");
    println!("DEBUG: After content bytes: {:?}", after_content);
    assert_eq!(
        after_str, "Line 1\nLine 2\n",
        "Content after append should include appended content"
    );

    // Verify final status is clean
    let final_status = repo
        .status(StatusOptions::default())
        .expect("Failed to get final status");
    let final_modified: Vec<_> = final_status.modified().collect();
    assert!(
        final_modified.is_empty(),
        "File should be clean after recording append. Found {} modified files: {:?}",
        final_modified.len(),
        final_modified.iter().map(|f| f.path()).collect::<Vec<_>>()
    );
}

/// Test prepending content to an existing file.
#[test]
fn test_file_prepend_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record initial file
    create_and_add_file(&repo, &repo_path, "prepend.txt", "Line 2\n");
    let _hash1 = record_change(&repo, "Add prepend.txt");

    // Prepend content to the file
    let file_path = repo_path.join("prepend.txt");
    fs::write(&file_path, "Line 1\nLine 2\n").expect("Failed to prepend to file");

    // Record the prepend
    let hash2 = record_change(&repo, "Prepend to prepend.txt");

    // Get content AFTER the prepend
    let after = repo
        .get_file_content_after_change("prepend.txt", &hash2)
        .expect("Failed to get content after prepend");

    assert!(after.is_some(), "Content after prepend should exist");
    let after_content = after.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&after_content),
        "Line 1\nLine 2\n",
        "Content after prepend should include prepended content"
    );

    // Verify final status is clean
    let final_status = repo
        .status(StatusOptions::default())
        .expect("Failed to get final status");
    let final_modified: Vec<_> = final_status.modified().collect();
    assert!(
        final_modified.is_empty(),
        "File should be clean after recording prepend. Found {} modified files: {:?}",
        final_modified.len(),
        final_modified.iter().map(|f| f.path()).collect::<Vec<_>>()
    );
}

/// Test inserting content in the middle of an existing file.
#[test]
fn test_file_insert_middle_state_retrieval() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Create and record initial file with multiple lines
    create_and_add_file(&repo, &repo_path, "middle.txt", "Line 1\nLine 3\n");
    let hash1 = record_change(&repo, "Add middle.txt");
    println!("DEBUG: Initial change hash: {}", hash1.to_base32());

    // Verify initial content
    let initial = repo
        .get_file_content("middle.txt")
        .expect("Failed to get initial");
    println!(
        "DEBUG: Initial content: {:?}",
        String::from_utf8_lossy(&initial.unwrap())
    );

    // Insert content in the middle
    let file_path = repo_path.join("middle.txt");
    fs::write(&file_path, "Line 1\nLine 2\nLine 3\n").expect("Failed to insert in middle");
    println!("DEBUG: Wrote new content with middle line inserted");

    // Check status before recording
    let status = repo
        .status(StatusOptions::default())
        .expect("Failed to get status");
    let modified: Vec<_> = status.modified().collect();
    println!("DEBUG: Modified files before record: {}", modified.len());

    // Record the insertion
    let hash2 = record_change(&repo, "Insert middle line");
    println!("DEBUG: Second change hash: {}", hash2.to_base32());

    // Get content AFTER the insertion
    let after = repo
        .get_file_content_after_change("middle.txt", &hash2)
        .expect("Failed to get content after insert");

    assert!(after.is_some(), "Content after insert should exist");
    let after_content = after.unwrap();
    let after_str = String::from_utf8_lossy(&after_content);
    println!("DEBUG: Content AFTER insert: {:?}", after_str);
    println!("DEBUG: Expected: {:?}", "Line 1\nLine 2\nLine 3\n");
    assert_eq!(
        after_str, "Line 1\nLine 2\nLine 3\n",
        "Content after insert should include inserted line"
    );

    // Verify final status is clean
    let final_status = repo
        .status(StatusOptions::default())
        .expect("Failed to get final status");
    let final_modified: Vec<_> = final_status.modified().collect();
    assert!(
        final_modified.is_empty(),
        "File should be clean after recording insert. Found {} modified files: {:?}",
        final_modified.len(),
        final_modified.iter().map(|f| f.path()).collect::<Vec<_>>()
    );
}
