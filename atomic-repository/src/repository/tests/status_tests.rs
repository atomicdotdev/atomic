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

#[test]
fn test_record_heals_file_index_for_phantom_modified_files() {
    // The dd1d59b status fix's "self-healing" claim was wrong for the
    // false-positive case. Concrete failure: a tracked file with no
    // FILE_INDEX entry whose on-disk content actually matches pristine
    // (e.g. landed in the working copy via insert/clone or via an old
    // binary's view-switch that didn't populate FILE_INDEX) shows as
    // Modified. record proves the content matches pristine, skips the
    // file (empty hunks), and — without this heal — leaves FILE_INDEX
    // empty for it. The next status() reports it Modified again.
    // Forever, across arbitrarily many records.
    //
    // Repro observed in the wild: 22 phantom-Modified files persisted
    // through an opencode agent record that touched 4 unrelated files.
    //
    // This test pins the heal: after a record where a phantom-Modified
    // file is the only "Modified" entry, status() must report Clean.
    use crate::record::RecordOptions;
    use crate::status::FileStatus;

    let (temp_dir, repo) = create_temp_repo();

    // Step 1: record a tracked file (populates FILE_INDEX).
    let path = temp_dir.path().join("phantom.txt");
    std::fs::write(&path, b"unchanged content").unwrap();
    repo.add("phantom.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("Add phantom.txt"),
        RecordOptions::new().with_all(true),
    )
    .expect("initial record failed");

    // Step 2: drop the FILE_INDEX entry to simulate the post-insert /
    // post-clone / post-old-binary-view-switch state.
    repo.del_file_index("phantom.txt")
        .expect("del_file_index failed");

    // Sanity check: status now flags the file as Modified (phantom).
    let pre_status = repo
        .status(StatusOptions::default())
        .expect("pre-record status failed");
    assert!(
        pre_status
            .entries()
            .iter()
            .any(|e| e.path().to_string_lossy() == "phantom.txt"
                && e.status() == FileStatus::Modified),
        "precondition: phantom.txt should be flagged Modified before record"
    );

    // Step 3: record(all=true) — content matches pristine, so this
    // returns NothingToRecord. The heal should still run.
    let record_result = repo.record(
        ChangeHeader::new("would-be record"),
        RecordOptions::new().with_all(true),
    );
    assert!(
        matches!(record_result, Err(crate::record::RecordError::NothingToRecord)),
        "record should report NothingToRecord when all 'modified' files are phantom; got {:?}",
        record_result.as_ref().map(|_| "Ok").map_err(|e| format!("{:?}", e))
    );

    // Step 4: status() must now be clean — FILE_INDEX should have an
    // entry for phantom.txt that matches the on-disk state.
    let post_status = repo
        .status(StatusOptions::default())
        .expect("post-record status failed");

    assert!(
        post_status.is_clean(),
        "after record, phantom-Modified file should be clean. \
         entries={:?}",
        post_status
            .entries()
            .iter()
            .map(|e| (
                e.path().to_string_lossy().to_string(),
                e.status(),
                e.details().map(|s| s.to_string())
            ))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_record_heals_file_index_alongside_real_changes() {
    // Variant of the heal test that mirrors the real-world scenario:
    // a record run that mixes one truly-modified file with several
    // phantom-Modified ones. The recorded change should contain only
    // the real edit, AND the phantom files must be healed so the
    // post-record status is clean for them.
    use crate::record::RecordOptions;
    use crate::status::FileStatus;

    let (temp_dir, repo) = create_temp_repo();

    // Two tracked files, both with FILE_INDEX populated.
    let phantom = temp_dir.path().join("phantom.txt");
    let real = temp_dir.path().join("real.txt");
    std::fs::write(&phantom, b"phantom original").unwrap();
    std::fs::write(&real, b"real original").unwrap();
    repo.add("phantom.txt", TrackingOptions::default()).unwrap();
    repo.add("real.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("Add files"),
        RecordOptions::new().with_all(true),
    )
    .expect("initial record failed");

    // Drop FILE_INDEX for phantom.txt only.
    repo.del_file_index("phantom.txt")
        .expect("del_file_index failed");

    // Real edit to real.txt.
    std::fs::write(&real, b"real edited").unwrap();

    // Record both. real.txt produces hunks; phantom.txt produces empty
    // hunks and lands in skipped_paths.
    let outcome = repo
        .record(
            ChangeHeader::new("Edit real, phantom is clean"),
            RecordOptions::new().with_all(true),
        )
        .expect("mixed record failed");

    let recorded: Vec<&str> = outcome.recorded_files().iter().map(|s| s.as_str()).collect();
    assert!(
        recorded.iter().any(|p| *p == "real.txt"),
        "real.txt should be in recorded_files, got {:?}",
        recorded
    );
    assert!(
        !recorded.iter().any(|p| *p == "phantom.txt"),
        "phantom.txt should NOT be in recorded_files (no real change), got {:?}",
        recorded
    );

    // Post-record status: phantom.txt must be clean, no Modified
    // entries should remain.
    let post_status = repo
        .status(StatusOptions::default())
        .expect("post-record status failed");

    let modified: Vec<String> = post_status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Modified)
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();

    assert!(
        modified.is_empty(),
        "no files should be Modified after record (phantom should be healed). \
         modified={:?}",
        modified
    );
    assert!(
        post_status.is_clean(),
        "status should be clean after record"
    );
}
