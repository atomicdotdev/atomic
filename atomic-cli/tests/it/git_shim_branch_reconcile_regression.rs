// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: Git branches created from an imported tip reconcile.
//!
//! docs/shim-review-testing.md §9 documented this as a known failure:
//! after Atomic records on `main`, `git switch -c <branch>` plus a Git
//! commit made `atomic git shim reconcile` fail with
//! `repository projection failed: view '<branch>' not found`, and once that
//! guard was relaxed with `forged Atomic change closure: prospective import
//! tree mismatch`. Record projection commits never parse as push trailers
//! (their last paragraph carries `atomic-*` lines), so they were verified
//! by the dependency closure of their `Atomic-Changes` roots, which is not
//! the view state once a rename commutes with an earlier edit. They are
//! now verified by the view state they name.
//!
//! A fast-forward merge of such a branch back into `main` then imported
//! nothing because the incremental marker set was the repository-wide Git
//! SHA index rather than the target view's membership.

use std::fs;
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
        .env("GIT_AUTHOR_NAME", "Branch Reconcile Tests")
        .env("GIT_AUTHOR_EMAIL", "branch-reconcile@example.com")
        .env("GIT_COMMITTER_NAME", "Branch Reconcile Tests")
        .env("GIT_COMMITTER_EMAIL", "branch-reconcile@example.com")
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

/// A Git repository with one baseline commit, imported and shim-enabled.
fn shim_repo() -> (PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("tracked.txt"), b"base\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-q", "-m", "baseline"]);
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    let key = home.join("binding.key");
    fs::write(&key, "ab".repeat(32)).unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "shim",
            "enable",
            "--no-refspecs",
            "--binding-key-file",
            key.to_str().unwrap(),
        ],
    );
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    (root, home)
}

fn assert_clean_and_verified(root: &Path, home: &Path) {
    atomic_ok(root, home, &["git", "shim", "verify"]);
    let status = atomic_ok(root, home, &["status", "--short"]);
    assert!(status.trim().is_empty(), "atomic status dirty:\n{status}");
    assert!(git(root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn literal_percent_path_reconciles_without_stale_checkpoint() {
    let (root, home) = shim_repo();
    fs::write(root.join("100%.txt"), b"literal percent\n").unwrap();
    git(&root, &["add", "100%.txt"]);
    git(&root, &["commit", "-qm", "add literal-percent path"]);
    let head = git(&root, &["rev-parse", "HEAD"]);

    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(
        fs::read(root.join("100%.txt")).unwrap(),
        b"literal percent\n"
    );
    assert!(!root.join("100%25.txt").exists());
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
}

#[test]
fn literal_percent_lookalikes_reconcile_through_edit_rename_delete() {
    let (root, home) = shim_repo();
    for (path, bytes) in [
        ("100%.txt", "literal percent\n"),
        ("a%41.txt", "literal escape\n"),
        ("aA.txt", "decoded lookalike\n"),
        ("a%2541.txt", "double escape lookalike\n"),
    ] {
        fs::write(root.join(path), bytes).unwrap();
    }
    git(&root, &["add", "."]);
    git(
        &root,
        &["commit", "-qm", "add distinct literal-percent paths"],
    );
    let head = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert!(!root.join("100%25.txt").exists());

    fs::write(root.join("a%41.txt"), b"Git edit of literal escape\n").unwrap();
    git(&root, &["add", "a%41.txt"]);
    git(&root, &["commit", "-qm", "edit literal escape"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(
        fs::read(root.join("aA.txt")).unwrap(),
        b"decoded lookalike\n"
    );
    assert_eq!(
        fs::read(root.join("a%2541.txt")).unwrap(),
        b"double escape lookalike\n"
    );

    git(&root, &["mv", "100%.txt", "renamed%25.txt"]);
    git(&root, &["commit", "-qm", "rename literal-percent path"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(
        fs::read(root.join("renamed%25.txt")).unwrap(),
        b"literal percent\n"
    );
    assert!(!root.join("100%.txt").exists());

    git(&root, &["rm", "renamed%25.txt"]);
    git(&root, &["commit", "-qm", "delete literal-percent path"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
}

#[test]
fn git_branch_created_after_atomic_records_reconciles() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"atomic edit\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "atomic edit"]);
    fs::rename(root.join("tracked.txt"), root.join("renamed.txt")).unwrap();
    atomic_ok(&root, &home, &["record", "-am", "atomic rename"]);
    assert_eq!(git(&root, &["show", "HEAD:renamed.txt"]), "atomic edit");

    git(&root, &["switch", "-q", "-c", "foreign-edit"]);
    fs::write(root.join("renamed.txt"), b"Git edit\n").unwrap();
    git(&root, &["add", "renamed.txt"]);
    git(&root, &["commit", "-q", "-m", "foreign Git edit"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);

    // Switching back and forth keeps each view's content.
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_eq!(
        fs::read(root.join("renamed.txt")).unwrap(),
        b"atomic edit\n"
    );
    git(&root, &["switch", "-q", "foreign-edit"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_eq!(fs::read(root.join("renamed.txt")).unwrap(), b"Git edit\n");
    assert_clean_and_verified(&root, &home);
}

#[test]
fn fast_forward_merge_of_a_reconciled_branch_reconciles_main() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"atomic edit\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "atomic edit"]);

    git(&root, &["switch", "-q", "-c", "feature"]);
    fs::write(root.join("feature.txt"), b"feature work\n").unwrap();
    git(&root, &["add", "feature.txt"]);
    git(&root, &["commit", "-q", "-m", "feature: add file"]);
    fs::write(root.join("tracked.txt"), b"atomic edit\nfeature line\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-q", "-m", "feature: edit tracked"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);

    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["merge", "-q", "--ff-only", "feature"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean_and_verified(&root, &home);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"atomic edit\nfeature line\n"
    );
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("feature.txt", "main")
            .unwrap(),
        Some(b"feature work\n".to_vec()),
        "main's view must hold the fast-forwarded branch content"
    );
}
