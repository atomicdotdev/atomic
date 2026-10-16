// Temporary Unix-only Git bridge support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: a sibling merge leg must never evict its own landed ancestry
//! from the target view, and a leg's foreign-modify semantic resolution must
//! respect the assembly exclusions.
//!
//! Original failure (`git import --all` on the closure walk; the same class
//! stopped atomicdotdev/atomic at commit 19 of 354): bridge sequencing lands
//! base → A → B (sibling of A) → C (child of A) → merge. Two defects made
//! that walk fail closed:
//!
//! 1. B's import superseded (evicted) A's landed change from the view — the
//!    eviction treated every assembly exclusion as "history B's Git ancestry
//!    replaced", but a sibling leg of the same closure is HEAD-reachable and
//!    still needed. C then assembled against the pre-A baseline; its
//!    positional anchors diverged and the per-hunk whole-file escalation
//!    stacked (three insert hunks each escalated, rendering ~3× the file)
//!    until the staged verification refused.
//! 2. B's foreign-modify bound the CRDT trunk's globally-alive branches —
//!    including A's excluded line — so B's old-line→branch mapping was off
//!    by one and the staged semantic closure rendered B's lines reordered
//!    ("render 19 byte(s) ... expects 19 byte(s)").
//!
//! The fixture below reproduces both in miniature. Pre-fix, `--all` fails at
//! B (semantic closure) or at C (stacked escalation); post-fix, all five
//! commits land, the final render is byte-exact, and the bridge verifies.

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
        .env("GIT_AUTHOR_NAME", "Sibling Closure Tests")
        .env("GIT_AUTHOR_EMAIL", "sibling-closure@example.com")
        .env("GIT_COMMITTER_NAME", "Sibling Closure Tests")
        .env("GIT_COMMITTER_EMAIL", "sibling-closure@example.com")
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

/// Commit with a controlled committer date (the deterministic bridge
/// sequencing's first tie-break), so the sibling lands between the parent
/// and its child.
fn commit_at(root: &Path, date: &str, message: &str) -> String {
    git(root, &["add", "."]);
    let output = Command::new("git")
        .args(["commit", "-q", "-m", message])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Sibling Closure Tests")
        .env("GIT_AUTHOR_EMAIL", "sibling-closure@example.com")
        .env("GIT_COMMITTER_NAME", "Sibling Closure Tests")
        .env("GIT_COMMITTER_EMAIL", "sibling-closure@example.com")
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

struct SiblingFixture {
    root: PathBuf,
    home: PathBuf,
    base: String,
    a: String,
    b: String,
    c: String,
    merge: String,
}

/// base → A (main) ; B (side, sibling of A) ; C (child of A, two
/// non-adjacent insertions) ; merge(side into main). One local branch.
fn sibling_fixture() -> SiblingFixture {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    fs::create_dir_all(root.join("f")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);

    fs::write(root.join("f/a.txt"), b"line1\nline9\n").unwrap();
    let base = commit_at(&root, "2026-09-11T09:00:00 +0000", "base");

    fs::write(root.join("f/a.txt"), b"line1\nA-edit\nline9\n").unwrap();
    let a = commit_at(&root, "2026-09-11T09:30:00 +0000", "A edit");

    git(&root, &["checkout", "-q", "-b", "side", "HEAD~1"]);
    fs::write(root.join("f/a.txt"), b"line1\nline9\nB-edit\n").unwrap();
    let b = commit_at(&root, "2026-09-11T10:00:00 +0000", "B edit");

    git(&root, &["checkout", "-q", "main"]);
    // Two non-adjacent insertions: two insert hunks, so a per-hunk
    // whole-file escalation cannot save a regressed baseline single-handedly
    // (it would stack).
    fs::write(root.join("f/a.txt"), b"C0\nline1\nA-edit\nC2\nline9\nC4\n").unwrap();
    let c = commit_at(&root, "2026-09-11T11:00:00 +0000", "C edit");

    let merge = if Command::new("git")
        .args(["merge", "--no-ff", "side", "-m", "merge side"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "Sibling Closure Tests")
        .env("GIT_AUTHOR_EMAIL", "sibling-closure@example.com")
        .env("GIT_COMMITTER_NAME", "Sibling Closure Tests")
        .env("GIT_COMMITTER_EMAIL", "sibling-closure@example.com")
        .output()
        .expect("git merge")
        .status
        .success()
    {
        git(&root, &["rev-parse", "HEAD"])
    } else {
        fs::write(
            root.join("f/a.txt"),
            b"C0\nline1\nA-edit\nC2\nline9\nC4\nB-edit\n",
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "merge side"]);
        git(&root, &["rev-parse", "HEAD"])
    };
    git(&root, &["branch", "-D", "side"]);

    SiblingFixture {
        root,
        home,
        base,
        a,
        b,
        c,
        merge,
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
fn all_import_sibling_leg_never_evicts_its_own_ancestry() {
    let fixture = sibling_fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    // Pre-fix, this failed at B ("staged semantic closure ... render 19
    // byte(s) ... expects 19 byte(s)") or at C (stacked whole-file
    // escalations rendering ~3x the file).
    let summary = atomic_ok(root, home, &["git", "import", "--all", "--no-vault"]);
    assert!(
        summary.contains("5 total changes"),
        "expected the complete closure imported: {summary}"
    );

    // Every closure commit — including the sibling leg B and the child C of
    // the leg A that B's import would previously have evicted — holds a
    // change in the view.
    let repo = atomic_repository::Repository::open(root).unwrap();
    for (sha, label) in [
        (&fixture.base, "base"),
        (&fixture.a, "A edit"),
        (&fixture.b, "B edit"),
        (&fixture.c, "C edit"),
        (&fixture.merge, "merge side"),
    ] {
        let change = imported_change_of(&repo, "main", sha);
        assert_eq!(
            change.hashed.header.message.lines().next(),
            Some(label),
            "commit {label} ({sha}) must be imported"
        );
    }
    drop(repo);

    // The materialized working copy is byte-exact against the merge tree.
    let expected = fs::read(root.join("f/a.txt")).unwrap();
    let git_tree = {
        let out = Command::new("git")
            .args(["show", "HEAD:f/a.txt"])
            .current_dir(root)
            .output()
            .expect("git show");
        out.stdout
    };
    assert_eq!(
        expected, git_tree,
        "materialized f/a.txt must equal the Git tree byte-for-byte"
    );

    // Both CLIs agree the workspace is clean, and the bridge verifies.
    let status = atomic_ok(root, home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "atomic status must be clean after import, got: {status}"
    );
    atomic_ok(root, home, &["git", "bridge", "verify"]);

    // Incremental re-import is idempotent.
    let again = atomic_ok(
        root,
        home,
        &["git", "import", "--all", "--incremental", "--no-vault"],
    );
    assert!(
        !again.contains("6 total changes"),
        "incremental import must not synthesize new changes: {again}"
    );
    assert_eq!(fs::read(root.join("f/a.txt")).unwrap(), git_tree);
}

#[test]
fn plain_import_stays_a_first_parent_walk() {
    // The same fixture through the plain (single-branch) import: the walk is
    // the documented linearized first-parent chain (base, A, C, merge — the
    // sibling leg's internal commits arrive through the merge's first-parent
    // diff), and the result must still be clean, verified, and byte-exact.
    let fixture = sibling_fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    atomic_ok(root, home, &["git", "import", "--no-vault"]);

    let status = atomic_ok(root, home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "atomic status must be clean after plain import, got: {status}"
    );
    atomic_ok(root, home, &["git", "bridge", "verify"]);
    let on_disk = fs::read(root.join("f/a.txt")).unwrap();
    let git_tree = {
        let out = Command::new("git")
            .args(["show", "HEAD:f/a.txt"])
            .current_dir(root)
            .output()
            .expect("git show");
        out.stdout
    };
    assert_eq!(on_disk, git_tree);
}
