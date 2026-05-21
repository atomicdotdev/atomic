//! Integration tests for `atomic restore`, driving the real CLI binary
//! end-to-end (`Restore::run()`), against a temporary repository.
//!
//! These complement the unit tests in `commands/restore.rs` by exercising the
//! full command path: argument parsing, the safety guard, the `reset` alias,
//! `--view` removal, and the actual filesystem effects.

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
fn restore_named_modified_file_without_force() {
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("file.txt"), b"local edit\n").unwrap();

    let out = atomic(root, &["restore", "file.txt"]);
    assert!(out.status.success(), "restore <file> should succeed");
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"v1\n");
}

#[test]
fn reset_alias_still_works() {
    // The legacy `reset` name is kept as an alias for `restore`.
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("file.txt"), b"local edit\n").unwrap();

    let out = atomic(root, &["reset", "file.txt"]);
    assert!(out.status.success(), "reset alias should still work");
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"v1\n");
}

#[test]
fn restore_named_file_not_blocked_by_unrelated_dirty_file() {
    let dir = repo_with_recorded_file();
    let root = dir.path();
    std::fs::write(root.join("other.txt"), b"o1\n").unwrap();
    assert!(atomic(root, &["add", "other.txt"]).status.success());
    assert!(atomic(root, &["record", "-m", "two"]).status.success());

    // Both files dirty; restore only file.txt.
    std::fs::write(root.join("file.txt"), b"edit-a\n").unwrap();
    std::fs::write(root.join("other.txt"), b"edit-b\n").unwrap();

    let out = atomic(root, &["restore", "file.txt"]);
    assert!(out.status.success());
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"v1\n");
    // The unrelated dirty file is untouched.
    assert_eq!(std::fs::read(root.join("other.txt")).unwrap(), b"edit-b\n");
}

#[test]
fn whole_tree_restore_without_force_is_user_error_not_internal() {
    let dir = repo_with_recorded_file();
    let root = dir.path();
    std::fs::write(root.join("file.txt"), b"dirty\n").unwrap();

    let out = atomic(root, &["restore"]);
    assert!(!out.status.success(), "whole-tree restore needs --force");
    assert_eq!(out.status.code(), Some(1), "user-level exit code");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("Cannot restore"),
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
fn restore_added_file_untracks_and_keeps_it_on_disk() {
    let dir = repo_with_recorded_file();
    let root = dir.path();

    std::fs::write(root.join("new.txt"), b"brand new\n").unwrap();
    assert!(atomic(root, &["add", "new.txt"]).status.success());

    let out = atomic(root, &["restore", "new.txt"]);
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

    let out = atomic(root, &["restore", "--dry-run", "new.txt"]);
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
fn view_flag_is_removed() {
    // `--view` was removed; switching views is `atomic view switch`. clap
    // should reject the unknown flag before any work happens.
    let dir = repo_with_recorded_file();
    let root = dir.path();

    let out = atomic(root, &["restore", "--view", "feature", "--force"]);
    assert!(!out.status.success(), "--view should no longer be accepted");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--view"),
        "clap should reject --view, got: {stderr}"
    );
}
