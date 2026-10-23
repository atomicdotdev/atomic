// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: Git tree shapes where one path changes kind between commits.
//!
//! Issue #279 (`partial import failed ... incomplete tree projection
//! metadata: projected parent '.agents/skills/<directory>' is not a
//! directory`): a path that is a regular file or symlink in one commit and
//! a directory in a later one. The importer pre-registers every added path
//! in the tracking tree before the change is written, and the tracking tree
//! still binds the old file inode at the parent path, so the pre-registration
//! of `<parent>/<child>` was refused.
//!
//! Two sibling shapes failed the prospective worktree check with
//! `deleted Git path '<p>' is still present in the worktree`:
//! - a directory replaced by a symlink to a directory (the
//!   `.agents/skills/<name> -> ../../.claude/skills/<name>` layout), because
//!   the deleted children still resolve through the new link;
//! - a case-only rename on a case-insensitive filesystem, because the old
//!   spelling still resolves to the renamed file.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Tree Shape Tests")
        .env("GIT_AUTHOR_EMAIL", "tree-shape@example.com")
        .env("GIT_COMMITTER_NAME", "Tree Shape Tests")
        .env("GIT_COMMITTER_EMAIL", "tree-shape@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_string()
}

fn git_repo() -> (PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    (root, home)
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

fn rendered_on_main(root: &Path, path: &str) -> Vec<u8> {
    let repo = atomic_repository::Repository::open(root).unwrap();
    repo.get_file_content_on_view(path, "main")
        .expect("content retrieval must not fail")
        .unwrap_or_else(|| panic!("'{path}' must be present on view main"))
}

/// Import, then prove the workspace is clean and the shim layer verifies.
fn import_and_verify(root: &Path, home: &Path, expected_changes: &str) {
    let summary = atomic_ok(root, home, &["git", "import", "--no-vault"]);
    assert!(
        summary.contains(expected_changes),
        "expected '{expected_changes}', got:\n{summary}"
    );
    let status = atomic_ok(root, home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "status must report a clean workspace, got:\n{status}"
    );
    atomic_ok(root, home, &["git", "shim", "verify"]);
}

#[test]
fn import_of_regular_file_replaced_by_directory_lands() {
    let (root, home) = git_repo();
    fs::write(root.join("x"), b"a file\n").unwrap();
    commit_all(&root, "x is a file");
    fs::remove_file(root.join("x")).unwrap();
    fs::create_dir(root.join("x")).unwrap();
    fs::write(root.join("x/y"), b"inside a directory\n").unwrap();
    commit_all(&root, "x is a directory");

    import_and_verify(&root, &home, "2 changes");
    assert_eq!(rendered_on_main(&root, "x/y"), b"inside a directory\n");
}

#[test]
fn import_of_symlink_replaced_by_directory_lands() {
    // The exact issue #279 layout: `.agents/skills/<name>` starts as a
    // symlink and later becomes a real directory holding files.
    let (root, home) = git_repo();
    fs::create_dir_all(root.join(".claude/skills/review")).unwrap();
    fs::create_dir_all(root.join(".agents/skills")).unwrap();
    fs::write(root.join(".claude/skills/review/SKILL.md"), b"linked\n").unwrap();
    symlink(
        "../../.claude/skills/review",
        root.join(".agents/skills/review"),
    )
    .unwrap();
    commit_all(&root, "skill as symlink");
    fs::remove_file(root.join(".agents/skills/review")).unwrap();
    fs::create_dir(root.join(".agents/skills/review")).unwrap();
    fs::write(root.join(".agents/skills/review/SKILL.md"), b"copied\n").unwrap();
    commit_all(&root, "skill as directory");

    import_and_verify(&root, &home, "2 changes");
    assert_eq!(
        rendered_on_main(&root, ".agents/skills/review/SKILL.md"),
        b"copied\n"
    );
}

#[test]
fn import_of_directory_replaced_by_symlink_to_directory_lands() {
    // The reverse layout: a real skill directory is replaced by a symlink
    // to a directory elsewhere, so the deleted children still resolve on
    // disk through the new link.
    let (root, home) = git_repo();
    fs::create_dir_all(root.join(".agents/skills/review")).unwrap();
    fs::write(root.join(".agents/skills/review/SKILL.md"), b"copied\n").unwrap();
    commit_all(&root, "skill as directory");
    fs::remove_dir_all(root.join(".agents/skills/review")).unwrap();
    fs::create_dir_all(root.join(".claude/skills/review")).unwrap();
    fs::write(root.join(".claude/skills/review/SKILL.md"), b"linked\n").unwrap();
    symlink(
        "../../.claude/skills/review",
        root.join(".agents/skills/review"),
    )
    .unwrap();
    commit_all(&root, "skill as symlink");

    import_and_verify(&root, &home, "2 changes");
    assert_eq!(
        rendered_on_main(&root, ".claude/skills/review/SKILL.md"),
        b"linked\n"
    );
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert!(
        repo.get_file_content_on_view(".agents/skills/review/SKILL.md", "main")
            .unwrap()
            .is_none(),
        "the file under the old directory must be gone"
    );
}

#[test]
fn import_of_case_only_rename_lands() {
    // On a case-insensitive filesystem the old spelling still resolves to
    // the renamed file; the import must not mistake it for a lingering copy.
    let (root, home) = git_repo();
    fs::write(root.join("Foo.md"), b"same bytes\n").unwrap();
    commit_all(&root, "upper");
    git(&root, &["mv", "Foo.md", "foo.md"]);
    commit_all(&root, "lower");

    import_and_verify(&root, &home, "2 changes");
    assert_eq!(rendered_on_main(&root, "foo.md"), b"same bytes\n");
}
