// Temporary Unix-only Git bridge support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: a sibling leg's landed change must never be evicted from the
//! target view mid-walk, because a later commit on the evicted leg assembles
//! against the missing baseline.
//!
//! Original failure (`git import --all` on atomic's own history, commit 22
//! of 356: 5146c474 "perf: eliminate filesystem I/O in git import Phase 2";
//! reproduced in miniature here): the deterministic sequencing interleaves
//! the legs of a yet-unmerged fork by committer time —
//!
//! ```text
//! base ── A ── C (child of A, modifies A's file) ── merge
//!      \── B (sibling of A, later committer time)
//! ```
//!
//! B's import assembly-excludes A (correct per-leg isolation), but the
//! synthesis ALSO superseded the exclusion: A left the target view. When C
//! (child of A, next in committer-time order) imported its Git diff — a
//! modify of a file A created — Atomic's projected baseline no longer
//! contained the file (its creating change was evicted), so the modify was
//! synthesized with a placeholder trunk. The staged semantic closure then
//! failed closed:
//!
//! ```text
//! branch BranchId(4, 0) claims a different trunk than the closure replayed
//! for 'f/new.txt'; branch ownership must match the closure
//! ```
//!
//! The fix: assembly exclusions whose Git origin is inside the import
//! target's full closure are sibling legs of the SAME walk — never
//! superseded from the view (the merge resolution re-references them via
//! `ensure_union_state`). Only origins outside the target closure
//! (genuinely replaced history, e.g. after a force-push rewrite) are
//! superseded.

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
        .env("GIT_AUTHOR_NAME", "Sibling Eviction Tests")
        .env("GIT_AUTHOR_EMAIL", "sibling-eviction@example.com")
        .env("GIT_COMMITTER_NAME", "Sibling Eviction Tests")
        .env("GIT_COMMITTER_EMAIL", "sibling-eviction@example.com")
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

fn commit_at(root: &Path, date: &str, message: &str) -> String {
    git(root, &["add", "."]);
    let output = Command::new("git")
        .args(["commit", "-q", "-m", message])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Sibling Eviction Tests")
        .env("GIT_AUTHOR_EMAIL", "sibling-eviction@example.com")
        .env("GIT_COMMITTER_NAME", "Sibling Eviction Tests")
        .env("GIT_COMMITTER_EMAIL", "sibling-eviction@example.com")
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git commit '{message}' failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(root, &["rev-parse", "HEAD"])
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    commits: Vec<(String, &'static str)>,
}

/// base → A (creates f/new.txt) ; B (sibling of A, edits f/a.txt) ;
/// C (child of A, modifies f/new.txt with context-preserving hunks) ;
/// merge (side into main). One local branch. The sequencing order is
/// base, A, B, C, merge (committer time), so B imports while A is landed
/// and C imports after B's leg walk.
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    fs::create_dir_all(root.join("f")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);

    fs::write(root.join("f/a.txt"), b"line1\nline2\nline3\n").unwrap();
    let base = commit_at(&root, "2026-09-11T09:00:00 +0000", "base");

    fs::write(
        root.join("f/new.txt"),
        b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n",
    )
    .unwrap();
    let a = commit_at(&root, "2026-09-11T09:30:00 +0000", "A add new");

    git(&root, &["checkout", "-q", "-b", "side", "HEAD~1"]);
    fs::write(root.join("f/a.txt"), b"line1\nB-edit\nline3\n").unwrap();
    let b = commit_at(&root, "2026-09-11T10:00:00 +0000", "B edit");

    git(&root, &["checkout", "-q", "main"]);
    // A context-preserving modify of A's file: line 2 changes and the last
    // line gains text, so the delta is a hunk edit — not a whole-file
    // rewrite that a sentinel fallback could mask.
    fs::write(
        root.join("f/new.txt"),
        b"l1\nl2 changed\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10 extra\n",
    )
    .unwrap();
    let c = commit_at(&root, "2026-09-11T11:00:00 +0000", "C modify new");

    git(&root, &["merge", "--no-ff", "side", "-m", "merge side"]);
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["branch", "-D", "side"]);

    Fixture {
        root,
        home,
        commits: vec![
            (base, "base"),
            (a, "A add new"),
            (b, "B edit"),
            (c, "C modify new"),
            (merge, "merge side"),
        ],
    }
}

fn imported_change_of(
    repo: &atomic_repository::Repository,
    view: &str,
    sha: &str,
) -> atomic_core::change::Change {
    let entries = repo.effective_history(Some(view)).unwrap();
    for entry in entries {
        let change = repo.load_change(&entry.hash).unwrap();
        let imported = change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .and_then(|git| git.get("sha"))
            .and_then(|value| value.as_str());
        if imported == Some(sha) {
            return change;
        }
    }
    panic!("no imported change for git commit {sha}")
}

#[test]
fn all_import_never_supersedes_sibling_legs_of_the_target_closure() {
    let fixture = fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    // Pre-fix signature: fail-closed at C — "branch BranchId(4, 0) claims a
    // different trunk than the closure replayed for 'f/new.txt'".
    let summary = atomic_ok(root, home, &["git", "import", "--all", "--no-vault"]);
    assert!(
        summary.contains("Imported "),
        "expected an import summary: {summary}"
    );

    // Every closure commit — including A (whose landed change B's leg walk
    // previously superseded) and C (A's child) — holds a change in the view.
    let repo = atomic_repository::Repository::open(root).unwrap();
    for (sha, label) in &fixture.commits {
        let change = imported_change_of(&repo, "main", sha);
        assert_eq!(
            change.hashed.header.message.lines().next(),
            Some(*label),
            "commit {label} ({sha}) must stay a view member"
        );
    }
    drop(repo);

    // The merged tree materializes byte-exactly for both files.
    for file in ["f/a.txt", "f/new.txt"] {
        let git_tree = {
            let out = Command::new("git")
                .args(["show", &format!("HEAD:{file}")])
                .current_dir(root)
                .output()
                .expect("git show");
            out.stdout
        };
        assert_eq!(
            fs::read(root.join(file)).unwrap(),
            git_tree,
            "materialized {file} must equal the Git tree byte-for-byte"
        );
    }

    // Both CLIs agree the workspace is clean, and the bridge verifies.
    let status = atomic_ok(root, home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "atomic status must be clean after import, got: {status}"
    );
    atomic_ok(root, home, &["git", "bridge", "verify"]);

    // Incremental re-import is idempotent — the retained sibling legs are
    // already-imported markers, not new history to re-derive.
    let before_count = atomic_ok(root, home, &["log", "-f", "json"])
        .matches("\"sequence\":")
        .count();
    atomic_ok(
        root,
        home,
        &["git", "import", "--all", "--incremental", "--no-vault"],
    );
    let after_count = atomic_ok(root, home, &["log", "-f", "json"])
        .matches("\"sequence\":")
        .count();
    assert_eq!(
        before_count, after_count,
        "incremental import must not change the view membership"
    );
    atomic_ok(root, home, &["git", "bridge", "verify"]);
}
