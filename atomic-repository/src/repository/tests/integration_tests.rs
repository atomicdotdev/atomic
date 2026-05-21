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

    // The file should not appear in any non-clean category.
    // Note: status() is an exception-reporter — clean files are omitted
    // for performance, so their absence from all non-clean lists is the
    // correct way to verify cleanliness.
    let added_files: Vec<_> = status.added().collect();
    assert!(
        !added_files
            .iter()
            .any(|e| e.path().to_string_lossy().contains("status_test.txt")),
        "status_test.txt should not be Added after recording"
    );
    let deleted_files: Vec<_> = status.deleted().collect();
    assert!(
        !deleted_files
            .iter()
            .any(|e| e.path().to_string_lossy().contains("status_test.txt")),
        "status_test.txt should not be Deleted after recording"
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

#[test]
fn test_import_line_index_seed_reads_current_graph_lines() {
    let (temp_dir, repo) = create_temp_repo();

    let file_path = temp_dir.path().join("seed_test.txt");
    std::fs::write(&file_path, b"one\ntwo\nthree\n").unwrap();
    repo.add("seed_test.txt", TrackingOptions::default())
        .unwrap();

    repo.record(
        ChangeHeader::new("seed base"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();

    let seed = repo
        .import_line_index_seed("seed_test.txt")
        .unwrap()
        .expect("tracked file should seed from graph");
    assert_eq!(seed.lines.len(), 3);
    assert!(seed.lines.iter().all(|line| line.start < line.end));

    std::fs::write(&file_path, b"one\nTWO\nthree\nfour\n").unwrap();
    repo.record(
        ChangeHeader::new("seed edit"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();

    let seed = repo
        .import_line_index_seed("seed_test.txt")
        .unwrap()
        .expect("edited file should seed from graph");
    assert_eq!(seed.lines.len(), 4);
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

#[test]
fn test_switch_view_preserves_harness_08_sequence_through_v8() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file_path = temp_dir.path().join("src/app.ts");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

    let versions = harness_08_versions();

    repo.create_view("agent-h08").unwrap();
    repo.switch_view("agent-h08").unwrap();

    for (idx, content) in versions.iter().take(8).enumerate() {
        std::fs::write(&file_path, content.as_bytes()).unwrap();

        if idx == 0 {
            repo.add("src/app.ts", TrackingOptions::default()).unwrap();
        }

        repo.record(
            ChangeHeader::new(&format!("v{}", idx + 1)),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    }

    let expected_v8 = versions[7].as_bytes();
    assert_eq!(std::fs::read(&file_path).unwrap(), expected_v8);

    repo.switch_view("dev").unwrap();
    repo.switch_view("agent-h08").unwrap();

    let after_switch = std::fs::read(&file_path).unwrap();
    assert_eq!(
        after_switch, expected_v8,
        "agent view content should survive a dev round-trip without truncation"
    );
}

#[test]
fn test_switch_view_preserves_harness_08_sequence_through_v9() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file_path = temp_dir.path().join("src/app.ts");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

    let versions = harness_08_versions();

    repo.create_view("agent-h08-v9").unwrap();
    repo.switch_view("agent-h08-v9").unwrap();

    for (idx, content) in versions.iter().take(9).enumerate() {
        std::fs::write(&file_path, content.as_bytes()).unwrap();

        if idx == 0 {
            repo.add("src/app.ts", TrackingOptions::default()).unwrap();
        }

        repo.record(
            ChangeHeader::new(&format!("v{}", idx + 1)),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    }

    let expected_v9 = versions[8].as_bytes();
    assert_eq!(std::fs::read(&file_path).unwrap(), expected_v9);

    repo.switch_view("dev").unwrap();
    repo.switch_view("agent-h08-v9").unwrap();

    let after_switch = std::fs::read(&file_path).unwrap();
    assert_eq!(
        after_switch, expected_v9,
        "agent view content should survive a dev round-trip through v9"
    );
}

#[test]
fn test_switch_view_preserves_harness_08_sequence_through_v10() {
    let (temp_dir, mut repo) = create_temp_repo();
    let file_path = temp_dir.path().join("src/app.ts");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

    let versions = harness_08_versions();

    repo.create_view("agent-h08-v10").unwrap();
    repo.switch_view("agent-h08-v10").unwrap();

    for (idx, content) in versions.iter().enumerate() {
        std::fs::write(&file_path, content.as_bytes()).unwrap();

        if idx == 0 {
            repo.add("src/app.ts", TrackingOptions::default()).unwrap();
        }

        repo.record(
            ChangeHeader::new(&format!("v{}", idx + 1)),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    }

    let expected_v10 = versions[9].as_bytes();
    assert_eq!(std::fs::read(&file_path).unwrap(), expected_v10);

    repo.switch_view("dev").unwrap();
    repo.switch_view("agent-h08-v10").unwrap();

    let after_switch = std::fs::read(&file_path).unwrap();
    if after_switch != expected_v10 {
        dump_filtered_alive_graph_for_test(&repo, "src/app.ts");
        eprintln!("actual v10:\n{}", String::from_utf8_lossy(&after_switch));
        eprintln!("expected v10:\n{}", String::from_utf8_lossy(expected_v10));
    }
    assert_eq!(
        after_switch, expected_v10,
        "agent view content should survive a dev round-trip through v10"
    );
}

#[test]
fn test_insert_change_preserves_harness_08_sequence_through_v9() {
    let versions = harness_08_versions();

    let (client_dir, mut client) = create_temp_repo();
    let client_src = client_dir.path().join("src");
    std::fs::create_dir_all(&client_src).unwrap();
    let client_file = client_src.join("app.ts");

    client.create_view("agent-h08-insert").unwrap();
    client.switch_view("agent-h08-insert").unwrap();

    let mut server_hashes = Vec::new();
    let (server_dir, mut server) = create_temp_repo();
    server.create_view("agent-h08-insert").unwrap();
    let server_file = server_dir.path().join("src/app.ts");

    for (idx, content) in versions.iter().enumerate() {
        std::fs::write(&client_file, content.as_bytes()).unwrap();
        if idx == 0 {
            client
                .add("src/app.ts", TrackingOptions::default())
                .unwrap();
        }

        let outcome = client
            .record(
                ChangeHeader::new(&format!("v{}", idx + 1)),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();

        let change = client.load_change(outcome.hash()).unwrap();
        let server_hash = server.save_change(&change).unwrap();
        server_hashes.push(server_hash);
    }

    for (idx, expected) in versions.iter().take(9).enumerate() {
        server
            .insert_change(
                &server_hashes[idx],
                InsertOptions::default().view("agent-h08-insert"),
            )
            .unwrap();
        server.switch_view("agent-h08-insert").unwrap();

        let actual = std::fs::read(&server_file).unwrap_or_default();
        if actual != expected.as_bytes() {
            dump_filtered_alive_graph_for_test(&server, "src/app.ts");
        }
        assert_eq!(
            actual,
            expected.as_bytes(),
            "server insert path diverged at v{}",
            idx + 1
        );
    }
}

#[test]
fn test_insert_change_preserves_harness_08_sequence_through_v10() {
    let versions = harness_08_versions();

    let (client_dir, mut client) = create_temp_repo();
    let client_src = client_dir.path().join("src");
    std::fs::create_dir_all(&client_src).unwrap();
    let client_file = client_src.join("app.ts");

    client.create_view("agent-h08-insert-v10").unwrap();
    client.switch_view("agent-h08-insert-v10").unwrap();

    let mut server_hashes = Vec::new();
    let (server_dir, mut server) = create_temp_repo();
    server.create_view("agent-h08-insert-v10").unwrap();
    let server_file = server_dir.path().join("src/app.ts");

    for (idx, content) in versions.iter().enumerate() {
        std::fs::write(&client_file, content.as_bytes()).unwrap();
        if idx == 0 {
            client
                .add("src/app.ts", TrackingOptions::default())
                .unwrap();
        }

        let outcome = client
            .record(
                ChangeHeader::new(&format!("v{}", idx + 1)),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();

        let change = client.load_change(outcome.hash()).unwrap();
        let server_hash = server.save_change(&change).unwrap();
        server_hashes.push(server_hash);
    }

    for (idx, expected) in versions.iter().enumerate() {
        server
            .insert_change(
                &server_hashes[idx],
                InsertOptions::default().view("agent-h08-insert-v10"),
            )
            .unwrap();
        server.switch_view("agent-h08-insert-v10").unwrap();

        let actual = std::fs::read(&server_file).unwrap_or_default();
        if actual != expected.as_bytes() {
            dump_filtered_alive_graph_for_test(&server, "src/app.ts");
        }
        assert_eq!(
            actual,
            expected.as_bytes(),
            "inserted server content should match client snapshot at v{}",
            idx + 1
        );
    }
}

fn harness_08_versions() -> Vec<&'static str> {
    vec![
        r#"// App v1 — initial
const VERSION = "1";

function greet(name: string): string {
  return `Hello, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
"#,
        r#"// App v2 — add color + helper
const VERSION = "2";

function formatName(name: string): string {
  return name.toUpperCase();
}

function greet(name: string): string {
  return `Hello, ${formatName(name)}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
"#,
        r#"// App v3 — inline formatting
const VERSION = "3";

function greet(name: string): string {
  return `Hello, ${name.toUpperCase()}!`;
}

function main(): void {
  const result = greet("World");
  console.log(result);
}

main();
"#,
        r#"// App v4 — add config
const VERSION = "4";

const config = {
  greeting: "Hello",
  loud: true,
};

function greet(name: string): string {
  const g = config.loud ? config.greeting.toUpperCase() : config.greeting;
  return `${g}, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
"#,
        r#"const VERSION = "5";

const config = {
  greeting: "Hey",
  loud: false,
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(): void {
  console.log(greet("World"));
}

main();
"#,
        r#"const VERSION = "6";

const config = {
  greeting: "Hey",
  loud: false,
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(args: string[]): void {
  try {
    const name = args[0] || "World";
    console.log(greet(name));
  } catch (e) {
    console.error("Failed:", e);
  }
}

main(process.argv.slice(2));
"#,
        r#"const VERSION = "7";

const logger = {
  info: (msg: string) => console.log(`[INFO] ${msg}`),
  error: (msg: string) => console.error(`[ERROR] ${msg}`),
};

const config = {
  greeting: "Hey",
  emoji: true,
};

function greet(name: string): string {
  const suffix = config.emoji ? " 👋" : "";
  return `${config.greeting}, ${name}!${suffix}`;
}

function main(): void {
  logger.info(greet("World"));
}

main();
"#,
        r#"const VERSION = "8";

let callCount = 0;

const logger = {
  info: (msg: string) => console.log(`[${new Date().toISOString()}] ${msg}`),
};

const config = {
  greeting: "Hey",
};

function greet(name: string): string {
  callCount++;
  return `${config.greeting}, ${name}!`;
}

function main(): void {
  logger.info(greet("World"));
  logger.info(`Calls: ${callCount}`);
}

main();
"#,
        r#"const VERSION = "9";

const config = {
  greeting: "Hello",
};

function greet(name: string): string {
  return `${config.greeting}, ${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
// End of app
"#,
        r#"const VERSION = "10";

type Config = { greeting: string; formal: boolean };

const config: Config = {
  greeting: "Greetings",
  formal: true,
};

function greet(name: string): string {
  const title = config.formal ? "esteemed " : "";
  return `${config.greeting}, ${title}${name}!`;
}

function main(): void {
  console.log(greet("World"));
}

main();
"#,
    ]
}

fn dump_filtered_alive_graph_for_test(repo: &Repository, path: &str) {
    use crate::repository::filter::collect_visible_change_ids;
    use atomic_core::change::ChangeStore as _;
    use atomic_core::output::alive::{compute_order, retrieve_graph, RetrieveOptions, VertexId};
    use atomic_core::pristine::{GraphTxnT, ViewTxnT};

    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(&repo.current_view).unwrap().unwrap();
    let filter = collect_visible_change_ids(&txn, &view).unwrap();
    let (_, position) = repo.get_inode_and_position(path).unwrap().unwrap();
    let retrieve = retrieve_graph(
        &txn,
        position,
        RetrieveOptions::new().with_change_filter(filter),
    )
    .unwrap();
    let mut graph = retrieve.graph;

    eprintln!(
        "--- alive graph dump for view={} path={} ---",
        repo.current_view, path
    );
    for idx in 1..graph.len_vertices() {
        let vid = VertexId::new(idx);
        let node = graph.get_vertex(vid).node;
        let len = (node.end.get() - node.start.get()) as usize;
        let mut buf = vec![0; len];
        repo.change_store()
            .get_contents(|id| txn.get_external(id).unwrap(), node, &mut buf)
            .unwrap();
        let text = String::from_utf8_lossy(&buf).replace('\n', "\\n");
        let children: Vec<String> = graph
            .children(vid)
            .filter(|(_, child)| !child.is_dummy())
            .map(|(edge, child)| {
                let flags = edge
                    .map(|e| format!("{:?}", e.flag()))
                    .unwrap_or_else(|| "Bypass".to_string());
                format!("{flags}->{:?}", graph.get_vertex(*child).node)
            })
            .collect();
        let raw_forward: Vec<String> = txn
            .iter_forward(node, true)
            .unwrap()
            .into_iter()
            .map(|edge| format!("{:?}->{:?}", edge.kind, txn.find_block(edge.dest).ok()))
            .collect();
        let raw_parents: Vec<String> = txn
            .iter_parents(node, true)
            .unwrap()
            .into_iter()
            .map(|edge| format!("{:?}->{:?}", edge.kind, txn.find_block_end(edge.dest).ok()))
            .collect();
        eprintln!(
            "V{idx} {:?} text={:?} children={children:?} raw_forward={raw_forward:?} raw_parents={raw_parents:?}",
            node, text
        );
    }

    let order = compute_order(&mut graph);
    for (idx, scc) in order.sccs.iter().enumerate() {
        eprintln!("SCC{idx}: {:?}", scc);
    }
    eprintln!("--- end alive graph dump ---");
}

/// Repro for the post-switch phantom-Deleted / false-Modified bug.
///
/// Scenario:
///   1. Record alpha.txt + bravo.txt on dev.
///   2. Split feature off dev, switch to feature.
///   3. On feature: edit alpha.txt and add delta.txt; record.
///   4. Switch back to dev.
///
/// At step 4 the working copy is correct (alpha.txt has dev's original
/// content, delta.txt is gone), and dev's recorded state is also correct.
/// Yet `status()` reports `alpha.txt: Modified` and `delta.txt: Deleted`.
///
/// Cause: `switch_view` updates the disk via `materialize` and removes
/// stale files from the working copy, but it does not reconcile TREE
/// or FILE_INDEX with the destination view's recorded state.
///   - delta.txt remains in TREE (it was added by feature's record on a
///     globally-shared TREE), and dev's status filter is "universal" for
///     a no-parent shared view, so iter_tree surfaces delta.txt → since
///     it's not on disk, it's reported Deleted.
///   - alpha.txt's FILE_INDEX entry still holds feature's hash; status
///     hashes the disk content, sees it differs from the cached hash,
///     and reports Modified.
///
/// The correct post-switch invariant is: if no working-copy edits have
/// happened since the switch, `status().is_clean()` must hold and there
/// must be no phantom Deleted or false Modified entries.
#[test]
fn test_status_clean_after_view_switch_with_sibling_changes() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Step 1: record alpha.txt + bravo.txt on dev.
    std::fs::write(temp_dir.path().join("alpha.txt"), b"alpha-original\n").unwrap();
    std::fs::write(temp_dir.path().join("bravo.txt"), b"bravo-original\n").unwrap();
    repo.add("alpha.txt", TrackingOptions::default()).unwrap();
    repo.add("bravo.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("Add alpha + bravo on dev"),
        RecordOptions::new().with_all(true),
    )
    .unwrap();

    // Step 2: split feature off dev and switch to it.
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();

    // Step 3: on feature, modify alpha.txt and add delta.txt; record.
    std::fs::write(
        temp_dir.path().join("alpha.txt"),
        b"alpha-modified-on-feature\n",
    )
    .unwrap();
    std::fs::write(temp_dir.path().join("delta.txt"), b"delta-feature\n").unwrap();
    repo.add("delta.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("Edit alpha + add delta on feature"),
        RecordOptions::new().with_all(true),
    )
    .unwrap();

    // Step 4: switch back to dev.
    repo.switch_view("dev").unwrap();

    // Sanity-check the disk before checking status. The switch should
    // have restored alpha.txt to dev's content and removed delta.txt.
    let alpha_on_disk = std::fs::read(temp_dir.path().join("alpha.txt")).unwrap();
    assert_eq!(
        alpha_on_disk, b"alpha-original\n",
        "switch_view must restore alpha.txt to dev's content"
    );
    assert!(
        !temp_dir.path().join("delta.txt").exists(),
        "switch_view must remove delta.txt (not in dev) from the working copy"
    );

    // The actual assertion: status must reflect the disk truth.
    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");

    // Collect any non-clean entries for diagnostics.
    let dirty: Vec<(String, crate::status::FileStatus)> = status
        .entries()
        .iter()
        .filter(|e| e.status().is_dirty())
        .map(|e| (e.path().to_string_lossy().to_string(), e.status()))
        .collect();

    assert!(
        dirty.is_empty(),
        "status() after view switch must be clean — disk and view state agree, \
         but status reported phantom dirty entries: {:?}",
        dirty
    );
    assert!(
        status.is_clean(),
        "status().is_clean() must hold immediately after a view switch with no \
         working-copy edits"
    );
}
