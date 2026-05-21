//! Integration tests for `atomic reset`, driving the real CLI binary
//! end-to-end (`Reset::run()`), against a temporary repository.
//!
//! These complement the unit tests in `commands/reset.rs` by exercising the
//! full command path: argument parsing, the safety guard, `--view` rejection,
//! and the actual filesystem effects.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Path to the compiled `atomic` binary (provided by Cargo to integration tests).
const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Run `atomic <args>` inside `dir`.
fn atomic(dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run atomic")
}

/// Initialize a repo with a single recorded file `file.txt` = "v1\n".
fn repo_with_recorded_file() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(atomic(root, &["init"]).status.success());
    std::fs::write(root.join("file.txt"), b"v1\n").unwrap();
    assert!(atomic(root, &["add", "file.txt"]).status.success());
    assert!(atomic(root, &["record", "-m", "rec"]).status.success());
    dir
}

#[test]
fn reset_named_modified_file_restores_without_force() {
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("file.txt"), b"local edit\n").unwrap();

    let out = atomic(root, &["reset", "file.txt"]);
    assert!(out.status.success(), "reset <file> should succeed");
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"v1\n");
}

#[test]
fn reset_named_file_not_blocked_by_unrelated_dirty_file() {
    let dir = repo_with_recorded_file();
    let root = dir.path();
    std::fs::write(root.join("other.txt"), b"o1\n").unwrap();
    assert!(atomic(root, &["add", "other.txt"]).status.success());
    assert!(atomic(root, &["record", "-m", "two"]).status.success());

    // Both files dirty; reset only file.txt.
    std::fs::write(root.join("file.txt"), b"edit-a\n").unwrap();
    std::fs::write(root.join("other.txt"), b"edit-b\n").unwrap();

    let out = atomic(root, &["reset", "file.txt"]);
    assert!(out.status.success());
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"v1\n");
    // The unrelated dirty file is untouched.
    assert_eq!(std::fs::read(root.join("other.txt")).unwrap(), b"edit-b\n");
}

#[test]
fn whole_tree_reset_without_force_is_user_error_not_internal() {
    let dir = repo_with_recorded_file();
    let root = dir.path();
    std::fs::write(root.join("file.txt"), b"dirty\n").unwrap();

    let out = atomic(root, &["reset"]);
    assert!(!out.status.success(), "whole-tree reset needs --force");
    assert_eq!(out.status.code(), Some(1), "user-level exit code");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr);
    assert!(
        combined.contains("Cannot reset"),
        "should explain the guard, got: {combined}"
    );
    assert!(
        !combined.contains("Internal error"),
        "must not surface as an internal bug, got: {combined}"
    );
    // The file is left untouched when the guard fires.
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"dirty\n");
}

#[test]
fn reset_added_file_untracks_and_keeps_it_on_disk() {
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("new.txt"), b"brand new\n").unwrap();
    assert!(atomic(root, &["add", "new.txt"]).status.success());

    let out = atomic(root, &["reset", "new.txt"]);
    assert!(out.status.success());
    // Kept on disk...
    assert!(root.join("new.txt").exists());
    assert_eq!(std::fs::read(root.join("new.txt")).unwrap(), b"brand new\n");
    // ...and no longer a pending change.
    let status = atomic(root, &["status"]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        !text.contains("new file:"),
        "added file should no longer be reported as a pending change: {text}"
    );
}

#[test]
fn dry_run_added_file_reports_untrack_without_erroring() {
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("new.txt"), b"brand new\n").unwrap();
    assert!(atomic(root, &["add", "new.txt"]).status.success());

    let out = atomic(root, &["reset", "--dry-run", "new.txt"]);
    assert!(
        out.status.success(),
        "dry-run on an added file must not error"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("Would untrack"),
        "dry-run should preview an untrack, got: {combined}"
    );
    assert!(
        !combined.contains("File not found"),
        "dry-run must not try to read pristine content for an added file: {combined}"
    );
    // Dry run leaves everything as-is.
    assert!(root.join("new.txt").exists());
}

#[test]
fn reset_view_fails_fast_and_points_to_view_switch() {
    // `reset --view` must not silently half-switch views (it used to leave
    // stale working-copy content while printing "working copy already clean").
    // It now fails fast and directs the user to `atomic view switch`.
    let dir = repo_with_recorded_file();
    let root = dir.path();
    assert!(atomic(root, &["view", "create", "feature"])
        .status
        .success());

    std::fs::write(root.join("file.txt"), b"DIRTY-EDIT\n").unwrap();

    let out = atomic(root, &["reset", "--force", "--view", "feature"]);
    assert!(!out.status.success(), "reset --view should be rejected");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("view switch"),
        "should point users to 'atomic view switch', got: {combined}"
    );
    // It must not have touched the working copy or switched the view.
    assert_eq!(
        std::fs::read(root.join("file.txt")).unwrap(),
        b"DIRTY-EDIT\n"
    );
    let views = atomic(root, &["view", "list"]);
    let views_text = String::from_utf8_lossy(&views.stdout);
    assert!(
        views_text.contains("* dev"),
        "current view must be unchanged (still dev), got: {views_text}"
    );
}
