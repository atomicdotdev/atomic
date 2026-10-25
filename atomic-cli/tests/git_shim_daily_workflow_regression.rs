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
//!   symlink (`unsupported Git entry ... with mode 120000`);
//! - `atomic git shim disable` and `atomic git hooks uninstall` left the
//!   pre-push dispatcher on disk, so a plain `git push` was still refused
//!   after the team opted out;
//! - inside a linked `git worktree` the pre-push dispatcher refused every
//!   push (`working-copy identity ... requires writable migration`), the
//!   other dispatchers journaled into a stray `.atomic/bridge` there, and
//!   `atomic git shim reconcile` tried to initialize a second repository
//!   (`Repository already exists`);
//! - a commit that truncated one file and extended another left the next
//!   edit of the extended file attached to the truncation's empty vertex,
//!   so the import refused the following commit (objx: `go.sum`);
//! - an empty lockfile or checksum file (`package-lock.json`, `go.sum`)
//!   that later gained content imported without semantic ops, so the staged
//!   semantic closure rendered zero bytes and the import refused;
//! - `atomic record` of a moved directory whose first entry is an empty
//!   file next to a non-empty sibling (a Python package's `__init__.py`)
//!   failed to apply (`ambiguous source ... for prior FOLDER edge`);
//! - deleting a truncated file whose neighbour was rewritten wholesale in
//!   the truncating commit, while editing that neighbour again, refused the
//!   import: the deletion collected the neighbour's first line as the
//!   truncated file's content (objx: `go.sum` beside `map_test.go`);
//! - moving a file into a subdirectory while a symlink to it takes the old
//!   path (emanote: `README.md` -> `emanote/README.md`) refused the import
//!   (`prospective import tree mismatch`): the rename's source removal wiped
//!   the symlink the same commit added there, and the mirror shape (a
//!   symlink replaced by the file it pointed to) lost the moved file;
//! - a repository whose committed tree already holds conflict markers (a
//!   merged-by-hand `Cargo.lock`) refused every `atomic record` through the
//!   shim after the record had landed, leaving Git behind Atomic for good;
//! - `git mv` of the last file out of a directory leaves the emptied
//!   directory on disk (Git cannot represent it); `atomic status` reported
//!   it as a reappeared `new dir`, so every reconcile and verify refused
//!   with `Atomic working copy is not clean` until it was removed by hand;
//! - a CRLF file committed before the repository adopted `* text=auto
//!   eol=lf` (Git keeps the blob as committed until the next add) refused
//!   the import (`changed worktree path ... does not match the verified Git
//!   tree`): the worktree bytes were judged through the clean filter
//!   although they are exactly the committed blob.

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

/// `atomic_ok` with extra environment variables for this one invocation.
fn atomic_ok_env(root: &Path, home: &Path, envs: &[(&str, &str)], args: &[&str]) -> String {
    let output = Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_NONINTERACTIVE", "1")
        .envs(envs.iter().copied())
        .output()
        .expect("run atomic");
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

/// Every Git hook `atomic git shim enable` installs as an Atomic-owned
/// dispatcher.
const ATOMIC_HOOKS: [&str; 5] = [
    "post-checkout",
    "pre-commit",
    "pre-push",
    "post-rewrite",
    "reference-transaction",
];

fn add_bare_origin(root: &Path) -> PathBuf {
    let remote = tempfile::tempdir().unwrap().keep();
    git(&remote, &["init", "-q", "--bare"]);
    git(root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    remote
}

/// A linked `git worktree` of `root` on a fresh branch, created with the
/// hook environment pinned so the post-checkout dispatcher runs as it would
/// for the team.
fn linked_worktree(root: &Path, home: &Path, branch: &str) -> PathBuf {
    let worktree = tempfile::tempdir().unwrap().keep().join("linked");
    let output = git_with_home(
        root,
        home,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            worktree.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "git worktree add failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    worktree
}

#[test]
fn shim_disable_removes_every_dispatcher_and_unblocks_push() {
    let (root, home) = shim_repo();
    add_bare_origin(&root);
    let hooks_dir = root.join(".git/hooks");
    for hook in ATOMIC_HOOKS {
        assert!(hooks_dir.join(hook).is_file(), "enable installs {hook}");
    }
    // A branch no view has interpreted: the pre-push gate refuses it.
    git(&root, &["switch", "-qc", "never"]);
    fs::write(root.join("n.txt"), b"n\n").unwrap();
    commit_all(&root, "n");
    let refused = git_with_home(&root, &home, &["push", "-q", "origin", "never"]);
    assert!(!refused.status.success(), "precondition: the gate refuses");

    let disabled = atomic_ok(&root, &home, &["git", "shim", "disable"]);
    for hook in ATOMIC_HOOKS {
        assert!(
            !hooks_dir.join(hook).exists(),
            "disable must remove the {hook} dispatcher; output:\n{disabled}"
        );
    }
    let pushed = git_with_home(&root, &home, &["push", "-q", "origin", "never"]);
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(
        pushed.status.success() && !stderr.contains("pre-push refused"),
        "a plain git push must not be refused after disable:\n{stderr}"
    );
}

#[test]
fn hooks_uninstall_removes_atomic_dispatchers_but_keeps_foreign_hooks() {
    let (root, home) = shim_repo();
    let hooks_dir = root.join(".git/hooks");
    let custom = b"#!/bin/sh\necho custom pre-push\nexit 0\n";
    fs::write(hooks_dir.join("pre-push"), custom).unwrap();
    fs::write(hooks_dir.join("commit-msg"), custom).unwrap();

    let run = atomic(&root, &home, &["git", "hooks", "uninstall"]);
    assert!(run.status.success(), "uninstall failed");
    // Removals are reported on stdout, kept foreign hooks as warnings on stderr.
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    for hook in [
        "post-checkout",
        "pre-commit",
        "post-rewrite",
        "reference-transaction",
    ] {
        assert!(
            !hooks_dir.join(hook).exists(),
            "uninstall must remove the {hook} dispatcher; output:\n{output}"
        );
        assert!(output.contains(hook), "each removal is reported:\n{output}");
    }
    assert_eq!(fs::read(hooks_dir.join("pre-push")).unwrap(), custom);
    assert_eq!(fs::read(hooks_dir.join("commit-msg")).unwrap(), custom);
    assert!(
        output.contains("pre-push") && output.contains("not Atomic-owned"),
        "the kept foreign hook is reported:\n{output}"
    );
    let status = atomic_ok(&root, &home, &["git", "hooks", "status"]);
    for hook in ATOMIC_HOOKS {
        assert!(status.contains(&format!("{hook}:")), "got:\n{status}");
    }
}

#[test]
fn linked_worktree_commit_and_push_pass_through_the_hooks() {
    let (root, home) = shim_repo();
    add_bare_origin(&root);
    let worktree = linked_worktree(&root, &home, "wt");
    fs::write(worktree.join("w.txt"), b"w\n").unwrap();
    git(&worktree, &["add", "-A"]);

    let commit = git_with_home(&worktree, &home, &["commit", "-qm", "w"]);
    let commit_stderr = String::from_utf8_lossy(&commit.stderr);
    assert!(commit.status.success(), "commit failed:\n{commit_stderr}");
    assert!(
        commit_stderr.contains("linked worktree") && commit_stderr.contains("hooks pass through"),
        "the pre-commit dispatcher must advise once:\n{commit_stderr}"
    );

    let push = git_with_home(&worktree, &home, &["push", "-q", "origin", "wt"]);
    let push_stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        push.status.success() && !push_stderr.contains("pre-push refused"),
        "push from a linked worktree must pass through:\n{push_stderr}"
    );
    assert!(
        push_stderr.contains("hooks pass through"),
        "got:\n{push_stderr}"
    );
    assert!(
        !worktree.join(".atomic").exists(),
        "hooks must not create a repository"
    );
    assert_clean(&root, &home);
}

#[test]
fn linked_worktree_status_and_reconcile_refuse_without_creating_a_repository() {
    let (root, home) = shim_repo();
    let worktree = linked_worktree(&root, &home, "wt");
    let expected = format!(
        "linked git worktrees are not supported by the shim yet; run from the main worktree '{}'",
        root.canonicalize().unwrap().display()
    );
    for args in [&["status"][..], &["git", "shim", "reconcile"][..]] {
        let error = atomic_err(&worktree, &home, args);
        assert!(error.contains(&expected), "atomic {args:?} got:\n{error}");
    }
    assert!(
        !worktree.join(".atomic").exists(),
        "no second repository may be created in the linked worktree"
    );
    assert_clean(&root, &home);
}

#[test]
fn renaming_a_file_without_a_trailing_newline_keeps_later_git_commits_reconcilable() {
    let (root, home) = shim_repo();
    // Git already holds the file; its last line has no newline.
    fs::write(root.join("notes.md"), b"# Notes\n\nfirst\nlast").unwrap();
    commit_all(&root, "notes");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    // A native record of another file first (the team's usual sequence),
    // then a plain mv plus a native record: the record detects the rename.
    fs::write(root.join("tracked.txt"), b"base\nedited\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "edit tracked"]);
    assert_clean(&root, &home);
    fs::rename(root.join("notes.md"), root.join("notes.renamed.md")).unwrap();
    atomic_ok(&root, &home, &["record", "-am", "rename notes"]);
    assert_clean(&root, &home);
    assert_eq!(
        git(
            &root,
            &["ls-tree", "--name-only", "HEAD", "notes.renamed.md"]
        ),
        "notes.renamed.md"
    );

    // A Git commit elsewhere, verified against the WHOLE tree (what a merge
    // or a fresh import does): the renamed file's semantic closure must
    // render its bytes once, not twice.
    fs::write(root.join("tracked.txt"), b"base\nedited\nagain\n").unwrap();
    commit_all(&root, "edit elsewhere");
    atomic_ok_env(
        &root,
        &home,
        &[("ATOMIC_FULL_SEMANTIC_VERIFY", "1")],
        &["git", "shim", "reconcile"],
    );
    assert_clean(&root, &home);

    // And a commit touching the renamed file still reconciles.
    fs::write(
        root.join("notes.renamed.md"),
        b"# Notes\n\nfirst\nlast\nmore\n",
    )
    .unwrap();
    commit_all(&root, "edit renamed");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("notes.renamed.md")).unwrap(),
        b"# Notes\n\nfirst\nlast\nmore\n"
    );
}

#[test]
fn renaming_and_editing_a_file_in_one_record_keeps_later_git_commits_reconcilable() {
    let (root, home) = shim_repo();
    fs::write(root.join("notes.md"), b"# Notes\n\nfirst\nlast").unwrap();
    commit_all(&root, "notes");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    fs::write(root.join("tracked.txt"), b"base\nedited\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "edit tracked"]);

    // Rename AND change the content in the same native record: the moved
    // file takes the whole-file replace, which must tombstone the lines the
    // view holds, not the ones the shared rows happen to show alive.
    fs::rename(root.join("notes.md"), root.join("notes.renamed.md")).unwrap();
    fs::write(
        root.join("notes.renamed.md"),
        b"# Notes\n\nfirst\nlast\nappended",
    )
    .unwrap();
    atomic_ok(&root, &home, &["record", "-am", "rename and edit notes"]);
    assert_clean(&root, &home);

    fs::write(root.join("tracked.txt"), b"base\nedited\nagain\n").unwrap();
    commit_all(&root, "edit elsewhere");
    atomic_ok_env(
        &root,
        &home,
        &[("ATOMIC_FULL_SEMANTIC_VERIFY", "1")],
        &["git", "shim", "reconcile"],
    );
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("notes.renamed.md")).unwrap(),
        b"# Notes\n\nfirst\nlast\nappended"
    );
}

#[cfg(unix)]
#[test]
fn shim_switch_handles_a_symlinked_directory_in_the_tree() {
    use std::os::unix::fs::symlink;
    let (root, home) = shim_repo();
    // `.claude/commands` starts as a real directory with files ...
    fs::create_dir_all(root.join(".claude/commands")).unwrap();
    fs::write(root.join(".claude/commands/add.md"), b"add\n").unwrap();
    fs::write(root.join(".claude/commands/fix.md"), b"fix\n").unwrap();
    commit_all(&root, "real commands directory");
    // ... and later becomes a symlink to a tracked directory elsewhere (the
    // agents layout): Git stores the link, the files live under `.agents`.
    git(&root, &["rm", "-rq", ".claude/commands"]);
    fs::create_dir_all(root.join(".agents/commands")).unwrap();
    fs::write(root.join(".agents/commands/add.md"), b"add\n").unwrap();
    fs::write(root.join(".agents/commands/fix.md"), b"fix\n").unwrap();
    fs::create_dir_all(root.join(".claude")).unwrap();
    symlink("../.agents/commands", root.join(".claude/commands")).unwrap();
    commit_all(&root, "symlinked directory");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // A native record must not sweep the files behind the symlink into the
    // view under the link's path.
    fs::write(root.join("tracked.txt"), b"base\nrecorded\n").unwrap();
    let recorded = atomic_ok(&root, &home, &["record", "-am", "native edit"]);
    assert!(
        !recorded.contains(".claude/commands/"),
        "the record must not list files beneath the symlink:\n{recorded}"
    );
    assert_clean(&root, &home);
    assert_eq!(
        git(
            &root,
            &["ls-tree", "--name-only", "HEAD", ".claude/commands/"]
        ),
        "",
        "the projection must not hold files beneath the symlink"
    );

    git(&root, &["switch", "-qc", "topic"]);
    fs::write(root.join("tracked.txt"), b"base\ntopic\n").unwrap();
    commit_all(&root, "topic edit");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    atomic_ok(&root, &home, &["git", "shim", "switch", "main"]);
    assert_eq!(git(&root, &["branch", "--show-current"]), "main");
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\nrecorded\n"
    );
    assert!(root
        .join(".claude/commands")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    atomic_ok(&root, &home, &["git", "shim", "switch", "topic"]);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\ntopic\n"
    );
    assert_clean(&root, &home);
}

#[test]
fn a_branch_created_after_an_amend_and_a_revert_reconciles() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\namend base\n").unwrap();
    commit_all(&root, "amend base");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // Amend the tip (history rewritten), then revert the amended commit.
    fs::write(root.join("tracked.txt"), b"base\namend base\namended\n").unwrap();
    git(&root, &["commit", "-q", "--amend", "-am", "amended"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    git(&root, &["revert", "--no-edit", "HEAD"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\n");

    // A branch off that tip must reconcile with the tree Git has.
    git(&root, &["switch", "-qc", "topic"]);
    fs::write(root.join("topic.txt"), b"topic\n").unwrap();
    commit_all(&root, "topic 1");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\n");
}

#[test]
fn a_branch_created_after_merges_reconciles_with_the_same_tree() {
    let (root, home) = shim_repo();
    fs::write(root.join("tracked.txt"), b"base\ngit edit\n").unwrap();
    commit_all(&root, "git edit");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);

    // A fast-forwarded branch, then a no-ff merged branch, each appending a
    // line: the history main carries into the next branch.
    git(&root, &["switch", "-qc", "feature/real"]);
    fs::write(root.join("tracked.txt"), b"base\ngit edit\nbranch edit\n").unwrap();
    commit_all(&root, "branch edit");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["merge", "-q", "--ff-only", "feature/real"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["switch", "-qc", "feature/noff"]);
    fs::write(
        root.join("tracked.txt"),
        b"base\ngit edit\nbranch edit\nnoff edit\n",
    )
    .unwrap();
    commit_all(&root, "noff edit");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(&root, &["switch", "-q", "main"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    git(
        &root,
        &["merge", "-q", "--no-ff", "-m", "merge noff", "feature/noff"],
    );
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // The next branch is seeded from main's closure: its tree must render
    // exactly what Git holds (the noff line once, at the end).
    git(&root, &["switch", "-qc", "topic"]);
    fs::write(root.join("topic.txt"), b"topic\n").unwrap();
    commit_all(&root, "topic 1");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"base\ngit edit\nbranch edit\nnoff edit\n"
    );
}

#[test]
fn deleting_a_branch_whose_view_was_adopted_does_not_block_reconcile() {
    let (root, home) = shim_repo();
    // `git switch -c topic` at the tip adopts a view for the branch; the user
    // then goes back to main and deletes the branch.
    git(&root, &["switch", "-qc", "topic"]);
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    git(&root, &["switch", "-q", "main"]);
    git(&root, &["branch", "-qD", "topic"]);

    // The deleted branch's mapping is retired, not reported as a ref that
    // moved independently of HEAD; work on main continues.
    let out = atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert!(
        !out.contains("advanced beyond the last observed mapping"),
        "got:\n{out}"
    );
    assert_clean(&root, &home);
    fs::write(root.join("tracked.txt"), b"base\nafter delete\n").unwrap();
    commit_all(&root, "after delete");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
}

#[test]
fn importing_a_truncation_recorded_with_an_edit_keeps_later_edits() {
    let (root, home) = git_repo();
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    fs::write(root.join("notes.txt"), b"l1\nl2\nl3\n").unwrap();
    commit_all(&root, "base");
    // notes.txt serializes after a.txt, so the truncation's empty vertex
    // sits exactly where a.txt's new last line ends.
    fs::write(root.join("notes.txt"), b"").unwrap();
    fs::write(root.join("a.txt"), b"a\nb\n").unwrap();
    commit_all(&root, "truncate notes, extend a");
    fs::write(root.join("a.txt"), b"a\nb\nc\n").unwrap();
    commit_all(&root, "extend a again");
    // Deleting the appended line resolves the deletion edge's source at the
    // same boundary.
    fs::write(root.join("a.txt"), b"a\nb\n").unwrap();
    commit_all(&root, "drop the appended line");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("4 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("a.txt", "main").unwrap(),
        Some(b"a\nb\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("notes.txt", "main").unwrap(),
        Some(Vec::new())
    );
}

#[test]
fn reconciling_a_truncation_recorded_with_an_edit_keeps_later_edits() {
    let (root, home) = shim_repo();
    fs::write(root.join("zz-notes.txt"), b"l1\nl2\nl3\n").unwrap();
    commit_all(&root, "notes");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    fs::write(root.join("zz-notes.txt"), b"").unwrap();
    fs::write(root.join("tracked.txt"), b"base\nsecond\n").unwrap();
    commit_all(&root, "truncate notes, extend tracked");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    fs::write(root.join("tracked.txt"), b"base\nsecond\nthird\n").unwrap();
    commit_all(&root, "extend tracked again");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("tracked.txt", "main")
            .unwrap(),
        Some(b"base\nsecond\nthird\n".to_vec())
    );
}

#[test]
fn an_empty_lockfile_that_gains_content_imports() {
    let (root, home) = git_repo();
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    fs::write(root.join("package-lock.json"), b"").unwrap();
    fs::write(root.join("go.sum"), b"h1\nh2\n").unwrap();
    commit_all(&root, "base");
    fs::write(root.join("go.sum"), b"").unwrap();
    commit_all(&root, "truncate go.sum");
    fs::write(root.join("package-lock.json"), b"{}\n").unwrap();
    fs::write(root.join("go.sum"), b"h3\n").unwrap();
    commit_all(&root, "regenerate both");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("package-lock.json", "main")
            .unwrap(),
        Some(b"{}\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("go.sum", "main").unwrap(),
        Some(b"h3\n".to_vec())
    );
}

#[test]
fn an_empty_lockfile_that_gains_content_reconciles() {
    let (root, home) = shim_repo();
    fs::write(root.join("Cargo.lock"), b"").unwrap();
    commit_all(&root, "empty lockfile");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    fs::write(root.join("Cargo.lock"), b"# generated\nversion = 4\n").unwrap();
    commit_all(&root, "generate lockfile");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    fs::write(root.join("Cargo.lock"), b"").unwrap();
    commit_all(&root, "truncate lockfile");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    fs::write(root.join("Cargo.lock"), b"version = 5\n").unwrap();
    commit_all(&root, "regenerate lockfile");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
}

#[test]
fn moving_a_package_directory_with_an_empty_init_records_through_the_shim() {
    let (root, home) = shim_repo();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/__init__.py"), b"").unwrap();
    fs::write(root.join("pkg/mod.py"), b"x = 1\n").unwrap();
    commit_all(&root, "add package");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    // The whole package is renamed on disk and recorded natively.
    fs::rename(root.join("pkg"), root.join("pkg2")).unwrap();
    atomic_ok(&root, &home, &["record", "-a", "-m", "move the package"]);
    assert_clean(&root, &home);
    assert_eq!(
        git(&root, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "pkg2/__init__.py\npkg2/mod.py\ntracked.txt",
        "the move projects to Git"
    );

    // A later Git commit inside the moved package still reconciles.
    fs::write(root.join("pkg2/mod.py"), b"x = 2\n").unwrap();
    commit_all(&root, "edit the moved module");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
}

#[test]
fn deleting_a_truncated_file_beside_a_rewritten_neighbour_imports() {
    let (root, home) = git_repo();
    let neighbour =
        "package x\n\nimport (\n\t\"testing\"\n\t\"assert\"\n\t\"require\"\n)\n\nvar m = 1\n";
    let rewritten = "package x\n\nimport (\n\t\"testing\"\n)\n\nvar m = 1\n";
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    fs::write(root.join("go.sum"), b"h1\nh2\nh3\nh4\n").unwrap();
    fs::write(root.join("map_test.go"), neighbour).unwrap();
    commit_all(&root, "base");
    // Truncating go.sum records a zero-length vertex at the change's first
    // content position; dropping two contiguous import lines rewrites the
    // neighbour wholesale, so its first line starts at that same position.
    fs::write(root.join("go.sum"), b"").unwrap();
    fs::write(root.join("map_test.go"), rewritten).unwrap();
    commit_all(&root, "drop testify");
    let edited = format!("{rewritten}// more\n");
    fs::write(root.join("go.sum"), b"").unwrap();
    git(&root, &["rm", "-q", "go.sum"]);
    fs::write(root.join("map_test.go"), &edited).unwrap();
    commit_all(&root, "delete go.sum, edit the neighbour");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("map_test.go", "main")
            .unwrap(),
        Some(edited.into_bytes())
    );
    assert_eq!(
        repo.get_file_content_on_view("go.sum", "main").unwrap(),
        None
    );
}

#[test]
fn importing_a_file_moved_into_a_package_and_symlinked_from_its_old_path() {
    let (root, home) = git_repo();
    fs::write(root.join("README.md"), b"readme\n").unwrap();
    fs::write(root.join("LICENSE"), b"MIT\n").unwrap();
    fs::create_dir_all(root.join("default")).unwrap();
    fs::write(root.join("default/index.yaml"), b"x: 1\n").unwrap();
    fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
    commit_all(&root, "base");
    // emanote dc1fb69: the package moves into a subdirectory and the
    // top-level docs become symlinks into it.
    fs::create_dir_all(root.join("pkg")).unwrap();
    git(&root, &["mv", "default", "pkg/default"]);
    git(&root, &["mv", "README.md", "pkg/README.md"]);
    git(&root, &["mv", "LICENSE", "pkg/LICENSE"]);
    std::os::unix::fs::symlink("pkg/README.md", root.join("README.md")).unwrap();
    std::os::unix::fs::symlink("pkg/LICENSE", root.join("LICENSE")).unwrap();
    commit_all(&root, "move the package, symlink the docs");
    fs::write(root.join("main.rs"), b"fn main() {}\n// after\n").unwrap();
    commit_all(&root, "after");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("README.md", "main").unwrap(),
        Some(b"pkg/README.md".to_vec()),
        "the old path holds the symlink target"
    );
    assert_eq!(
        repo.get_file_content_on_view("pkg/README.md", "main")
            .unwrap(),
        Some(b"readme\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("default/index.yaml", "main")
            .unwrap(),
        None
    );
}

#[test]
fn importing_a_symlink_replaced_by_the_file_it_pointed_to() {
    let (root, home) = git_repo();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/README.md"), b"readme\n").unwrap();
    std::os::unix::fs::symlink("pkg/README.md", root.join("README.md")).unwrap();
    fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
    commit_all(&root, "base");
    fs::remove_file(root.join("README.md")).unwrap();
    git(&root, &["mv", "pkg/README.md", "README.md"]);
    commit_all(&root, "move the file over its symlink");
    fs::write(root.join("main.rs"), b"fn main() {}\n// after\n").unwrap();
    commit_all(&root, "after");

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("README.md", "main").unwrap(),
        Some(b"readme\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("pkg/README.md", "main")
            .unwrap(),
        None
    );
}

#[test]
fn recording_beside_a_committed_file_with_conflict_markers_projects_to_git() {
    let (root, home) = git_repo();
    fs::write(root.join("tracked.txt"), b"base\n").unwrap();
    // llmlang: a dependabot merge committed Cargo.lock with its markers.
    fs::write(
        root.join("Cargo.lock"),
        b"[[package]]\n<<<<<<< HEAD\nversion = \"1.0.0\"\n=======\nversion = \"1.0.1\"\n>>>>>>> origin/dep\n",
    )
    .unwrap();
    commit_all(&root, "baseline with a half-merged lockfile");
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
    assert_clean(&root, &home);

    // An unrelated record must land in Atomic AND in Git: the markers were
    // already Git's own content, so nothing unresolved is being introduced.
    fs::write(root.join("tracked.txt"), b"base\nedit\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "edit beside the lockfile"]);
    assert_eq!(git(&root, &["show", "HEAD:tracked.txt"]), "base\nedit");
    assert!(
        git(&root, &["log", "-1", "--format=%B"]).contains("Atomic record projection"),
        "the record projected a Git commit"
    );
    assert_clean(&root, &home);

    // A record that itself writes markers is still refused before anything
    // lands.
    fs::write(
        root.join("tracked.txt"),
        b"base\n<<<<<<< ours\nedit\n=======\nother\n>>>>>>> theirs\n",
    )
    .unwrap();
    let error = atomic_err(&root, &home, &["record", "-am", "half-merged edit"]);
    assert!(error.contains("conflict marker"), "got:\n{error}");
    fs::write(root.join("tracked.txt"), b"base\nedit\n").unwrap();
    assert_clean(&root, &home);
}

#[test]
fn moving_the_last_file_out_of_a_directory_reconciles() {
    let (root, home) = shim_repo();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/README.md"), b"readme\n").unwrap();
    commit_all(&root, "package");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);

    git(&root, &["mv", "pkg/README.md", "README.md"]);
    commit_all(&root, "move out of the package");
    assert!(
        root.join("pkg").is_dir(),
        "git leaves the emptied directory on disk"
    );
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
    {
        // Scoped: the handle holds the database lock the CLI needs below.
        let repo = atomic_repository::Repository::open(&root).unwrap();
        assert_eq!(
            repo.get_file_content_on_view("README.md", "main").unwrap(),
            Some(b"readme\n".to_vec())
        );
        assert_eq!(
            repo.get_file_content_on_view("pkg/README.md", "main")
                .unwrap(),
            None
        );
    }

    // The leftover directory keeps not blocking later work on either side.
    fs::write(root.join("README.md"), b"readme\nmore\n").unwrap();
    atomic_ok(&root, &home, &["record", "-am", "edit after the move"]);
    assert_eq!(git(&root, &["show", "HEAD:README.md"]), "readme\nmore");
    fs::write(root.join("tracked.txt"), b"base\ngit\n").unwrap();
    commit_all(&root, "git edit after the move");
    atomic_ok(&root, &home, &["git", "shim", "reconcile"]);
    assert_clean(&root, &home);
}

#[test]
fn importing_a_move_that_empties_a_directory_leaves_status_clean() {
    let (root, home) = git_repo();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/README.md"), b"readme\n").unwrap();
    fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
    commit_all(&root, "base");
    git(&root, &["mv", "pkg/README.md", "README.md"]);
    commit_all(&root, "move out of the package");
    fs::write(root.join("main.rs"), b"fn main() {}\n// after\n").unwrap();
    commit_all(&root, "after");
    assert!(root.join("pkg").is_dir());

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
}

#[test]
fn importing_a_crlf_file_committed_before_eol_normalization() {
    let (root, home) = git_repo();
    fs::write(root.join("RULES.md"), b"rule one\r\nrule two\r\n").unwrap();
    fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
    commit_all(&root, "crlf file");
    // The attribute arrives later; the committed blob keeps its CRLF and Git
    // reports the worktree clean by its index stat.
    fs::write(root.join(".gitattributes"), b"* text=auto eol=lf\n").unwrap();
    commit_all(&root, "normalize line endings from now on");
    fs::write(root.join("main.rs"), b"fn main() {}\n// after\n").unwrap();
    commit_all(&root, "after");
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
    assert!(fs::read(root.join("RULES.md")).unwrap().contains(&b'\r'));

    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("3 changes"), "got:\n{summary}");
    assert_clean(&root, &home);
    let repo = atomic_repository::Repository::open(&root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("RULES.md", "main").unwrap(),
        Some(b"rule one\r\nrule two\r\n".to_vec()),
        "the recorded bytes are the committed blob"
    );
}
