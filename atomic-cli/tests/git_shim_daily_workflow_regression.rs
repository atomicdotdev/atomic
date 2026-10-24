// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: ordinary day-one Git usage against an imported, shim-enabled
//! repository.
//!
//! Each test is a scenario a small team hit in review before the fix:
//! - `atomic git import` materializing a branch that is not checked out
//!   wrote the imported tree over uncommitted edits and untracked files;
//! - a tracked path with a space or a non-ASCII byte was registered under
//!   its `%XX`-escaped identity, so `atomic status` was dirty right after
//!   the import and the next incremental import could not verify the view;
//! - truncating a file to zero bytes aborted the mainline import;
//! - `atomic git shim switch` projected the view over a branch whose tip
//!   Git had moved without a reconcile, silently reverting those commits;
//! - `git checkout <imported ancestor>` plus a reconcile created an
//!   ephemeral Draft under the checkpoint view that could never match the
//!   checkout, stranding the workspace;
//! - a `.git/REBASE_HEAD` left behind by a completed interactive rebase
//!   refused every Atomic command as an in-progress Git operation;
//! - any untracked file, gitignored or not, made reconcile and verify
//!   refuse with `Worktree/ExtraPath`, and the next Git commit then left
//!   `atomic status` refusing too;
//! - `git reset --hard` onto an imported ancestor imported nothing, kept the
//!   dropped change in the view and stranded every later command;
//! - the pre-push hook bound every proposed branch to the current view, so
//!   pushing any other branch was refused;
//! - `atomic git shim switch` refused any repository whose tree holds a
//!   symlink (`unsupported Git entry ... with mode 120000`).

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
        .env("ATOMIC_NONINTERACTIVE", "1")
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

fn atomic_err(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        !output.status.success(),
        "atomic {args:?} unexpectedly succeeded:\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Daily Workflow Tests")
        .env("GIT_AUTHOR_EMAIL", "daily@example.com")
        .env("GIT_COMMITTER_NAME", "Daily Workflow Tests")
        .env("GIT_COMMITTER_EMAIL", "daily@example.com")
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

/// `git` with the hook environment pinned to the test home.
fn git_with_home(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_AUTHOR_NAME", "Daily Workflow Tests")
        .env("GIT_AUTHOR_EMAIL", "daily@example.com")
        .env("GIT_COMMITTER_NAME", "Daily Workflow Tests")
        .env("GIT_COMMITTER_EMAIL", "daily@example.com")
        .output()
        .expect("run git")
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

fn assert_clean(root: &Path, home: &Path) {
    let status = atomic_ok(root, home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "status must report a clean workspace, got:\n{status}"
    );
    atomic_ok(root, home, &["git", "shim", "verify"]);
}

/// A Git repository with one baseline commit, imported and shim-enabled.
fn shim_repo() -> (PathBuf, PathBuf) {
    let (root, home) = git_repo();
    fs::write(root.join("tracked.txt"), b"base\n").unwrap();
    commit_all(&root, "baseline");
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

#[test]
fn import_refuses_to_materialize_over_unrecorded_work() {
    let (root, home) = git_repo();
    fs::write(root.join("a.txt"), b"v1\n").unwrap();
    commit_all(&root, "c1");
    fs::write(root.join("a.txt"), b"v2\n").unwrap();
    fs::write(root.join("b.txt"), b"b\n").unwrap();
    commit_all(&root, "c2");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Detached on the older commit with an edit and an untracked file that
    // the imported tree would overwrite.
    git(&root, &["checkout", "-q", "HEAD~1"]);
    fs::write(root.join("a.txt"), b"MY UNCOMMITTED WORK\n").unwrap();
    fs::write(root.join("b.txt"), b"my untracked notes\n").unwrap();

    let message = atomic_err(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(
        message.contains("cannot materialize"),
        "the import must refuse before writing the worktree, got:\n{message}"
    );
    assert_eq!(
        fs::read(root.join("a.txt")).unwrap(),
        b"MY UNCOMMITTED WORK\n"
    );
    assert_eq!(
        fs::read(root.join("b.txt")).unwrap(),
        b"my untracked notes\n"
    );

    // Only the untracked collision left: still refused, still untouched.
    git(&root, &["checkout", "-q", "--", "a.txt"]);
    let message = atomic_err(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(
        message.contains("b.txt"),
        "the refusal must name the untracked collision, got:\n{message}"
    );
    assert_eq!(
        fs::read(root.join("b.txt")).unwrap(),
        b"my untracked notes\n"
    );

    // A clean worktree materializes.
    fs::remove_file(root.join("b.txt")).unwrap();
    atomic_ok(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"v2\n");
}

#[test]
fn paths_with_spaces_and_unicode_import_clean() {
    let (root, home) = git_repo();
    fs::create_dir(root.join("dir with space")).unwrap();
    fs::write(root.join("has space.txt"), b"s\n").unwrap();
    fs::write(root.join("caf\u{e9}.txt"), b"c\n").unwrap();
    fs::write(root.join("dir with space/inner file.md"), b"i\n").unwrap();
    commit_all(&root, "odd names");
    let key = home.join("binding.key");
    fs::write(&key, "ab".repeat(32)).unwrap();

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert_clean(&root, &home);
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
    assert_clean(&root, &home);

    // Edit, rename and delete those paths through Git, then import again.
    fs::write(root.join("has space.txt"), b"s2\n").unwrap();
    git(
        &root,
        &["mv", "caf\u{e9}.txt", "dir with space/caf\u{e9} moved.txt"],
    );
    git(&root, &["rm", "-q", "dir with space/inner file.md"]);
    commit_all(&root, "edit odd names");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // The repository knows the paths under the names on disk.
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("has space.txt", "main")
            .unwrap(),
        Some(b"s2\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("dir with space/caf\u{e9} moved.txt", "main")
            .unwrap(),
        Some(b"c\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("dir with space/inner file.md", "main")
            .unwrap(),
        None
    );
    drop(repo);

    // A native record of such a path round-trips through the shim too.
    fs::write(root.join("has space.txt"), b"s3\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "atomic edit"]);
    assert_eq!(git(&root, &["show", "HEAD:has space.txt"]), "s3");
    assert_clean(&root, &home);
}

#[test]
fn truncating_a_file_to_empty_imports_on_the_mainline() {
    let (root, home) = git_repo();
    fs::write(root.join("t.txt"), b"full\n").unwrap();
    fs::write(root.join("keep.txt"), b"k\n").unwrap();
    commit_all(&root, "c1");
    fs::write(root.join("t.txt"), b"").unwrap();
    commit_all(&root, "truncate");
    fs::write(root.join("t.txt"), b"refilled\n").unwrap();
    commit_all(&root, "refill");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    {
        let repo = atomic_repository::Repository::open(&root).unwrap();
        assert_eq!(
            repo.get_file_content_on_view("t.txt", "main").unwrap(),
            Some(b"refilled\n".to_vec())
        );
    }

    // The same truncation arriving incrementally.
    fs::write(root.join("t.txt"), b"").unwrap();
    commit_all(&root, "truncate again");
    atomic_ok(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert_clean(&root, &home);
}

#[test]
fn shim_switch_refuses_a_branch_with_unimported_commits() {
    let (root, home) = shim_repo();
    git(&root, &["switch", "-qc", "feature"]);
    fs::write(root.join("f.txt"), b"F1\n").unwrap();
    commit_all(&root, "F1");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    // A second commit the shim never saw: hooks do not reconcile a branch
    // that is being left.
    fs::write(root.join("f.txt"), b"F2\n").unwrap();
    commit_all(&root, "F2");
    let f2 = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    let message = atomic_err(&root, &home, &["git", "shim", "switch", "feature"]);
    assert!(
        message.contains("has not imported"),
        "switch must refuse the un-imported tip, got:\n{message}"
    );
    assert_eq!(git(&root, &["rev-parse", "feature"]), f2);
    assert_eq!(git(&root, &["show", "feature:f.txt"]), "F2");
    assert_eq!(git(&root, &["branch", "--show-current"]), "main");

    // Reconciling the branch makes the switch legal and keeps F2.
    git(&root, &["switch", "-q", "feature"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    atomic_ok(&root, &home, &["git", "shim", "switch", "feature"]);
    assert_eq!(git(&root, &["branch", "--show-current"]), "feature");
    assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"F2\n");
    assert_eq!(git(&root, &["rev-parse", "feature"]), f2);
    assert_clean(&root, &home);
}

#[test]
fn checkout_of_an_imported_ancestor_reconciles_and_returns() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\nc1\n").unwrap();
    commit_all(&root, "c1");
    fs::write(root.join("tracked.txt"), b"base\nc1\nc2\n").unwrap();
    commit_all(&root, "c2");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    // `git bisect` territory: a detached checkout of an imported ancestor.
    git(&root, &["checkout", "-q", "HEAD~1"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\nc1\n");

    git(&root, &["checkout", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\nc1\nc2\n"
    );
    atomic_ok(&root, &home, &["record", "--help"]);
}

#[test]
fn stale_rebase_head_does_not_block_reconcile() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\nc1\n").unwrap();
    commit_all(&root, "c1");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    // What Git 2.50 leaves behind after `rebase -i` with a squash: the
    // marker file alone, no rebase-merge/rebase-apply directory.
    let head = git(&root, &["rev-parse", "HEAD"]);
    fs::write(root.join(".git/REBASE_HEAD"), format!("{head}\n")).unwrap();
    assert!(!root.join(".git/rebase-merge").exists());
    assert!(!root.join(".git/rebase-apply").exists());

    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    fs::write(root.join("tracked.txt"), b"base\nc1\nedit\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "after stale marker"]);
    assert_eq!(git(&root, &["show", "HEAD:tracked.txt"]), "base\nc1\nedit");
}

#[test]
fn untracked_and_ignored_files_do_not_block_the_shim() {
    let (root, home) = shim_repo();
    fs::write(root.join(".gitignore"), b"node_modules/\n*.log\n").unwrap();
    commit_all(&root, "ignore rules");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), b"x\n").unwrap();
    fs::write(root.join("debug.log"), b"l\n").unwrap();
    fs::write(root.join("scratch.txt"), b"notes\n").unwrap();
    assert_eq!(
        git(
            &root,
            &["status", "--porcelain", "--", "node_modules", "debug.log"]
        ),
        ""
    );

    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    atomic_ok(&root, &home, &["git", "shim", "verify"]);
    fs::write(root.join("tracked.txt"), b"base\nc1\n").unwrap();
    commit_all(&root, "c1");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    atomic_ok(&root, &home, &["git", "shim", "verify"]);
    let status = atomic_ok(&root, &home, &["status"]);
    assert!(
        !status.contains("reconciliation required"),
        "status must not be refused, got:\n{status}"
    );
    assert!(!status.contains("node_modules"), "got:\n{status}");
    assert!(!status.contains("modified"), "got:\n{status}");

    // Atomic-side work keeps projecting while the scratch files stay.
    fs::write(root.join("tracked.txt"), b"base\nc1\natomic\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "atomic edit"]);
    assert_eq!(
        git(&root, &["show", "HEAD:tracked.txt"]),
        "base\nc1\natomic"
    );
    atomic_ok(&root, &home, &["git", "shim", "verify"]);
    assert!(root.join("scratch.txt").exists());
    assert!(root.join("node_modules/pkg/index.js").exists());
}

#[test]
fn reset_to_an_imported_ancestor_rewinds_the_view() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\nc1\n").unwrap();
    commit_all(&root, "c1");
    fs::write(root.join("b.txt"), b"new\n").unwrap();
    fs::write(root.join("tracked.txt"), b"base\nc1\nc2\n").unwrap();
    commit_all(&root, "c2");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert!(atomic_ok(&root, &home, &["log", "--short"]).contains("c2"));

    git(&root, &["reset", "-q", "--hard", "HEAD~1"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    let after = atomic_ok(&root, &home, &["log", "--short"]);
    assert!(
        !after.contains("c2"),
        "the dropped commit must leave the view:\n{after}"
    );
    assert!(after.contains("c1"), "got:\n{after}");
    assert!(!root.join("b.txt").exists());
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\nc1\n");

    // Work continues on the rewound branch.
    fs::write(root.join("tracked.txt"), b"base\nc1\nc3\n").unwrap();
    commit_all(&root, "c3");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert!(atomic_ok(&root, &home, &["log", "--short"]).contains("c3"));
}

#[test]
fn pushing_a_reconciled_non_current_branch_passes_the_pre_push_gate() {
    let (root, home) = shim_repo();
    let remote = tempfile::tempdir().unwrap().keep();
    git(&remote, &["init", "-q", "--bare"]);
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&root, &["switch", "-qc", "other"]);
    fs::write(root.join("o.txt"), b"o\n").unwrap();
    commit_all(&root, "o");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    // A branch no view has ever interpreted.
    git(&root, &["switch", "-qc", "never"]);
    fs::write(root.join("n.txt"), b"n\n").unwrap();
    commit_all(&root, "n");
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    for branch in ["other", "main"] {
        let output = git_with_home(&root, &home, &["push", "-q", "origin", branch]);
        assert!(
            output.status.success(),
            "push of reconciled branch '{branch}' must pass the gate:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = git_with_home(&root, &home, &["push", "-q", "origin", "never"]);
    assert!(
        !output.status.success(),
        "an un-reconciled branch must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pre-push refused"),
        "got:
{stderr}"
    );
    assert!(git(&remote, &["branch", "--list"]).contains("other"));
}

#[test]
fn shim_switch_handles_symlinks_in_the_tree() {
    let (root, home) = shim_repo();
    // The agent-skills layout: a directory of symlinks to skill folders.
    fs::create_dir_all(root.join(".claude/skills/review")).unwrap();
    fs::write(root.join(".claude/skills/review/SKILL.md"), b"review\n").unwrap();
    fs::create_dir_all(root.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(
        "../../.claude/skills/review",
        root.join(".agents/skills/review"),
    )
    .unwrap();
    commit_all(&root, "skills layout");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    git(&root, &["switch", "-qc", "feature"]);
    fs::write(root.join("tracked.txt"), b"base\nfeature\n").unwrap();
    commit_all(&root, "feature edit");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    atomic_ok(&root, &home, &["git", "shim", "switch", "feature"]);
    assert_eq!(git(&root, &["branch", "--show-current"]), "feature");
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\nfeature\n"
    );
    assert!(root
        .join(".agents/skills/review")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    atomic_ok(&root, &home, &["git", "shim", "switch", "main"]);
    assert_eq!(git(&root, &["branch", "--show-current"]), "main");
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\n");
    assert_clean(&root, &home);
}

#[test]
fn native_record_deleting_lines_keeps_later_git_commits_reconcilable() {
    let (root, home) = shim_repo();
    // A multi-line file lands through Git first.
    fs::write(root.join("tracked.txt"), b"base\nsecond\nthird\n").unwrap();
    commit_all(&root, "three lines");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // A native record deletes the middle line.
    fs::write(root.join("tracked.txt"), b"base\nthird\n").unwrap();
    atomic_ok(&root, &home, &["record", "-a", "-m", "drop second"]);
    assert_clean(&root, &home);
    assert_eq!(
        git(&root, &["show", "HEAD:tracked.txt"]),
        "base\nthird",
        "the record projects to Git"
    );

    // The next Git commit on the same file must still reconcile.
    fs::write(root.join("tracked.txt"), b"base\nthird\nfourth\n").unwrap();
    commit_all(&root, "fourth line");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\nthird\nfourth\n"
    );
    assert!(atomic_ok(&root, &home, &["log", "--short"]).contains("fourth line"));
}

#[test]
fn native_record_rewriting_a_line_keeps_later_git_commits_reconcilable() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\nsecond\nthird\n").unwrap();
    commit_all(&root, "three lines");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    // A native record rewrites the middle line in place.
    fs::write(root.join("tracked.txt"), b"base\nSECOND\nthird\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "rewrite second"]);
    assert_clean(&root, &home);

    // A later Git commit on another file still reconciles, and so does one
    // on the rewritten file.
    fs::write(root.join("other.txt"), b"other\n").unwrap();
    commit_all(&root, "other file");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    fs::write(root.join("tracked.txt"), b"base\nSECOND\n").unwrap();
    commit_all(&root, "drop third");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\nSECOND\n"
    );
}

#[test]
fn a_new_branch_reconciles_only_its_own_commits() {
    let (root, home) = shim_repo();
    for n in 1..=3 {
        fs::write(root.join("tracked.txt"), format!("base\n{n}\n")).unwrap();
        commit_all(&root, &format!("main {n}"));
    }
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // Branch off the reconciled tip, commit once, reconcile: the new view is
    // seeded from the tip's interpreted closure and imports one commit, not
    // the whole history again.
    git(&root, &["switch", "-qc", "topic"]);
    fs::write(root.join("topic.txt"), b"topic\n").unwrap();
    commit_all(&root, "topic 1");
    let out = atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert!(
        out.contains("Imported 1 change"),
        "the new branch must import only its own commit:\n{out}"
    );
    assert_clean(&root, &home);
    let log = atomic_ok(&root, &home, &["log", "--short"]);
    for message in ["baseline", "main 1", "main 2", "main 3", "topic 1"] {
        assert!(
            log.contains(message),
            "{message} missing from the topic view:\n{log}"
        );
    }
}
