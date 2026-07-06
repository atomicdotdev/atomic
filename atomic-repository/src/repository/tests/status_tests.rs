use super::*;

#[test]
fn test_repo_status_empty_repo() {
    let (_temp_dir, repo) = create_temp_repo();

    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");

    assert_eq!(status.view(), "dev");
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

#[test]
fn test_status_no_untracked_for_files_tracked_on_other_view() {
    // Reproduces the customer bug report:
    //   "I run atomic status and it shows me tons of things that aren't
    //    tracked, then I run atomic add and it says there's nothing to add."
    //
    // Root cause: status() filters TREE entries by the view's change filter
    // (view-aware), excluding files whose introducing change isn't visible.
    // Those files then appear as "Untracked" during the filesystem walk.
    // But is_tracked() (used by add) checks the global TREE table without
    // any view filter — so those same files are "already tracked."
    //
    // Scenario: agent hook creates a draft view, adds+records files on it,
    // then the user checks status on the parent (dev) view while the files
    // still exist on disk.
    use crate::record::RecordOptions;
    use crate::status::FileStatus;
    use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};

    let (temp_dir, mut repo) = create_temp_repo();

    // Step 1: Record a base file on dev so the view isn't empty
    std::fs::write(temp_dir.path().join("base.txt"), b"base content").unwrap();
    repo.add("base.txt", TrackingOptions::default()).unwrap();
    let header = ChangeHeader::new("Add base.txt");
    repo.record(header, RecordOptions::new().with_all(true))
        .expect("base record failed");

    // Step 2: Create a draft child view (simulates agent session)
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        txn.create_view("agent-draft", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();
    }

    // Step 3: Switch to draft, add+record files, switch back
    repo.set_current_view("agent-draft").unwrap();

    std::fs::write(temp_dir.path().join("agent_file.txt"), b"created by agent").unwrap();
    std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    std::fs::write(temp_dir.path().join("src/module.rs"), b"pub fn agent() {}").unwrap();

    repo.add("agent_file.txt", TrackingOptions::default())
        .unwrap();
    repo.add("src/module.rs", TrackingOptions::default())
        .unwrap();

    let header = ChangeHeader::new("Agent adds files");
    repo.record(header, RecordOptions::new().with_all(true))
        .expect("agent record failed");

    // Step 4: Switch back to dev (but leave files on disk — simulates
    // agent creating files in the working copy without cleaning up)
    repo.set_current_view("dev").unwrap();

    // Files still exist on disk (agent created them, switch_view wasn't
    // called so no materialization cleanup happened)
    assert!(temp_dir.path().join("agent_file.txt").exists());
    assert!(temp_dir.path().join("src/module.rs").exists());

    // Step 5: Check status on dev — THE BUG
    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");

    // These files should NOT appear as Untracked
    let untracked_paths: Vec<String> = status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Untracked)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();

    assert!(
        !untracked_paths.iter().any(|p| p == "agent_file.txt"),
        "agent_file.txt must NOT appear as Untracked on dev. \
         It is in TREE (from draft view) and should be Added or hidden. \
         Untracked entries: {:?}",
        untracked_paths
    );
    assert!(
        !untracked_paths.iter().any(|p| p == "src/module.rs"),
        "src/module.rs must NOT appear as Untracked on dev. \
         Untracked entries: {:?}",
        untracked_paths
    );

    // Step 6: Verify the contradictory state doesn't exist
    // is_tracked should return true (file is in global TREE)
    assert!(
        repo.is_tracked("agent_file.txt").unwrap(),
        "agent_file.txt should be tracked (in global TREE)"
    );

    // If status says Untracked AND is_tracked says true, that's the bug
    let is_untracked_in_status = untracked_paths.iter().any(|p| p == "agent_file.txt");
    let is_tracked_globally = repo.is_tracked("agent_file.txt").unwrap();
    assert!(
        !(is_untracked_in_status && is_tracked_globally),
        "CONTRADICTION: status says Untracked but is_tracked says true. \
         This is the exact bug the customer reported."
    );

    // Step 7: The correct behavior — these files should show as Added
    // (tracked in TREE, but not yet recorded on THIS view)
    let added_paths: Vec<String> = status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Added)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();

    assert!(
        added_paths.iter().any(|p| p == "agent_file.txt"),
        "agent_file.txt should appear as Added on dev (tracked but not \
         recorded on this view). Added entries: {:?}, All entries: {:?}",
        added_paths,
        status
            .entries()
            .iter()
            .map(|e| (e.path().to_string_lossy().to_string(), e.status()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_status_foreign_file_not_on_disk_is_invisible() {
    // Complement to the test above: when a file is tracked on another view
    // but does NOT exist on disk, it should not appear in status at all.
    // It should not be Deleted (it was never on this view) or Untracked.
    use crate::record::RecordOptions;
    use crate::status::FileStatus;
    use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};

    let (temp_dir, mut repo) = create_temp_repo();

    // Record base on dev
    std::fs::write(temp_dir.path().join("base.txt"), b"base").unwrap();
    repo.add("base.txt", TrackingOptions::default()).unwrap();
    let header = ChangeHeader::new("Base commit");
    repo.record(header, RecordOptions::new().with_all(true))
        .unwrap();

    // Create draft, record a file on it
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        let dev = txn.get_view("dev").unwrap().unwrap();
        txn.create_view("draft", ViewScope::Draft, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();
    }
    repo.set_current_view("draft").unwrap();

    std::fs::write(temp_dir.path().join("draft_only.txt"), b"draft content").unwrap();
    repo.add("draft_only.txt", TrackingOptions::default())
        .unwrap();
    let header = ChangeHeader::new("Draft file");
    repo.record(header, RecordOptions::new().with_all(true))
        .unwrap();

    // Switch back to dev AND remove the file from disk
    repo.set_current_view("dev").unwrap();
    std::fs::remove_file(temp_dir.path().join("draft_only.txt")).unwrap();

    // Status on dev should NOT mention draft_only.txt at all
    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");
    let all_paths: Vec<(String, FileStatus)> = status
        .entries()
        .iter()
        .map(|e| (e.path().to_string_lossy().to_string(), e.status()))
        .collect();

    assert!(
        !all_paths.iter().any(|(p, _)| p == "draft_only.txt"),
        "draft_only.txt should not appear in status on dev at all \
         (not on disk, belongs to another view). Entries: {:?}",
        all_paths
    );
}

#[test]
fn test_status_detects_modification_when_file_index_missing() {
    // Repro for the silent-data-loss bug: when a tracked file has no
    // FILE_INDEX entry (e.g. after `atomic insert` / `atomic clone` /
    // `atomic view switch` materialized it without going through `record`),
    // `status()` falls through with the "Assume clean" comment and the
    // file becomes invisible to `status`, `diff`, and `record -a`. Any
    // edits the user (or an agent) makes to that file are silently
    // dropped on the next record.
    //
    // This test pins the correct behavior: status() must detect the
    // modification regardless of whether the index has been populated.
    use crate::record::RecordOptions;
    use crate::status::FileStatus;

    let (temp_dir, repo) = create_temp_repo();

    // Step 1: create + record a tracked file. Recording with
    // apply_after_record (the default) populates FILE_INDEX for this file.
    let file_path = temp_dir.path().join("tracked.txt");
    std::fs::write(&file_path, b"original content").unwrap();
    repo.add("tracked.txt", TrackingOptions::default()).unwrap();
    let header = ChangeHeader::new("Add tracked.txt");
    repo.record(header, RecordOptions::new().with_all(true))
        .expect("initial record failed");

    // Step 2: drop the FILE_INDEX entry to simulate the post-insert /
    // post-clone / post-switch state. In production, those code paths
    // materialize files into the working copy without writing FILE_INDEX
    // entries — only `record()` and `materialize_view()` populate it.
    repo.del_file_index("tracked.txt")
        .expect("del_file_index failed");

    // Step 3: modify the file on disk. mtime, size, and content all change.
    std::fs::write(&file_path, b"modified content (different size)").unwrap();

    // Step 4: status() must report it as Modified.
    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");

    let modified_paths: Vec<String> = status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Modified)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();

    assert!(
        modified_paths.iter().any(|p| p == "tracked.txt"),
        "status() must detect modifications to tracked files even when \
         FILE_INDEX has no entry for them. \
         entries={:?}",
        status
            .entries()
            .iter()
            .map(|e| (e.path().to_string_lossy().to_string(), e.status()))
            .collect::<Vec<_>>()
    );
    assert!(
        !status.is_clean(),
        "status() must not be clean when a tracked file is modified"
    );
}
