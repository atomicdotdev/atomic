//! CB-9B end-to-end: causal merge resolution, distinct empty commits, and
//! squash/rewrite synthesis through the real importer.
//!
//! The suite proves (RFC §5.3, §5.4, §19 Q3 resolution):
//! - every multi-parent merge imports every parent closure and assembles a
//!   journaled `GitResolution` from the union state whose verified
//!   `CausalFrontier` covers every parent closure including transitive
//!   ancestors, with context-derived dependencies and materialized
//!   equality (including an octopus),
//! - a later concurrent change still conflicts correctly after
//!   serialize/reload — the resolution is never replayed automatically,
//! - distinct empty Git commits stay distinct `Change::empty` objects with
//!   hashed tagged OIDs, ordered parents, tree OID, raw author/committer
//!   identity+times, and `Derivation::EmptyCommit`,
//! - unbound squashes without trailers synthesize one `Derivation::Squash`
//!   interpretation, `atomic git bridge review` surfaces candidates with
//!   their evidence and identity limits, and a captured post-rewrite event
//!   with a locally known predecessor produces only a reviewable
//!   RewriteCandidate link.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use atomic_core::change::{ChangeOrigin, GitDerivation};
use atomic_core::operation::{GitHashAlgorithm, GitObjectId, GitRefTarget, OperationKind, OperationScope};
use atomic_core::types::Base32;
use git2::Repository as GitRepository;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        // CB-9C hermeticity: the post-record shadow projection commits to
        // Git, so the spawned atomic must resolve a stable committer
        // identity without the machine's global gitconfig (an isolated
        // HOME has none).
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "CB-9B Tests")
        .env("GIT_AUTHOR_EMAIL", "cb9b@example.com")
        .env("GIT_COMMITTER_NAME", "CB-9B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb9b@example.com")
        .output()
        .expect("run atomic")
}

fn atomic_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-9B Tests")
        .env("GIT_AUTHOR_EMAIL", "cb9b@example.com")
        .env("GIT_COMMITTER_NAME", "CB-9B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb9b@example.com")
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

fn git_commit(root: &Path, message: &str) -> String {
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// Run a Git command with stdin and return trimmed stdout (used for
/// plumbing like `commit-tree`, whose stdin carries the commit message).
fn git_stdin_output(root: &Path, args: &[&str], stdin: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-9B Tests")
        .env("GIT_AUTHOR_EMAIL", "cb9b@example.com")
        .env("GIT_COMMITTER_NAME", "CB-9B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb9b@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run git with stdin");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin)
        .expect("write git stdin");
    let output = child.wait_with_output().expect("wait for git");
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

fn atomic_stdin(root: &Path, home: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run atomic with stdin");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin)
        .expect("write stdin");
    child.wait_with_output().expect("wait for atomic")
}

fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

fn tagged(hex: &str) -> GitObjectId {
    let algorithm = match hex.len() {
        40 => GitHashAlgorithm::Sha1,
        64 => GitHashAlgorithm::Sha256,
        other => panic!("unexpected SHA width {other}"),
    };
    GitObjectId::new(algorithm, decode_hex(hex)).unwrap()
}

fn open_repo(root: &Path) -> atomic_repository::Repository {
    atomic_repository::Repository::open(root).unwrap()
}

fn change_by_sha(
    repo: &atomic_repository::Repository,
    view: &str,
    git_sha: &str,
) -> atomic_core::change::Change {
    let entries = repo.effective_history(Some(view)).unwrap();
    for entry in entries {
        let change = repo.load_change(&entry.hash).unwrap();
        let sha = change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .and_then(|git| git.get("sha"))
            .and_then(|sha| sha.as_str());
        if sha == Some(git_sha) {
            return change;
        }
    }
    panic!("no imported change for git commit {git_sha}")
}

fn frontier_closure_of(
    repo: &atomic_repository::Repository,
    change: &atomic_core::change::Change,
) -> atomic_core::change::VerifiedCausalFrontier {
    let txn = repo.pristine().read_txn().unwrap();
    atomic_core::apply::verify_causal_frontier(&txn, change).expect("frontier verifies")
}

/// Two-branch merge fixture: base → side-a / side-b, merged into main with
/// a manual conflict resolution. Single-branch import is the CB-9B flow:
/// the full ancestor closure lands in main's view.
struct MergeFixture {
    root: PathBuf,
    home: PathBuf,
    base: String,
    side_a: String,
    side_b: String,
    merge: String,
}

fn merge_fixture() -> MergeFixture {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"line1\nline9\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let base = git(&root, &["rev-parse", "HEAD"]);

    git(&root, &["checkout", "-q", "-b", "side-a"]);
    fs::write(root.join("a.txt"), b"line1\nside-a\nline9\n");
    git_commit(&root, "side a");
    let side_a = git(&root, &["rev-parse", "HEAD"]);

    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["checkout", "-q", "-b", "side-b"]);
    fs::write(root.join("a.txt"), b"line1\nside-b\nline9\n");
    git_commit(&root, "side b");
    let side_b = git(&root, &["rev-parse", "HEAD"]);

    git(&root, &["checkout", "-q", "side-a"]);
    let merge_out = Command::new("git")
        .args(["merge", "--no-ff", "side-b", "-m", "merge side work"])
        .current_dir(&root)
        .output()
        .expect("git merge");
    assert!(
        !merge_out.status.success(),
        "the fixture merge must conflict; the test resolves it explicitly"
    );
    fs::write(root.join("a.txt"), b"line1\nresolved\nline9\n");
    git_commit(&root, "merge side work");
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "side-a"]);

    MergeFixture {
        root,
        home,
        base,
        side_a,
        side_b,
        merge,
    }
}

/// A merge imports every parent closure and resolves from the union state
/// with a verified frontier covering both parents.
#[test]
fn merge_imports_every_parent_closure_and_resolves_with_verified_frontier() {
    let fixture = merge_fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    atomic_ok(root, home, &["git", "import", "--no-vault"]);

    let repo = open_repo(root);
    let base_change = {
        let entries = repo.effective_history(Some("main")).unwrap();
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap().unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(fixture.base.as_str())
            })
            .expect("base imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    let side_a_change = {
        let entries = repo.effective_history(Some("main")).unwrap();
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(fixture.side_a.as_str())
            })
            .expect("side-a imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    let side_b_change = {
        let entries = repo.effective_history(Some("main")).unwrap();
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(fixture.side_b.as_str())
            })
            .expect("side b imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    let merge_change = {
        let entries = repo.effective_history(Some("main")).unwrap();
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(fixture.merge.as_str())
            })
            .expect("merge imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };

    // Hashed origin: GitResolution with complete ordered parents.
    match merge_change.origin() {
        ChangeOrigin::GitResolution {
            merge_commit,
            parents,
        } => {
            assert_eq!(*merge_commit, tagged(&fixture.merge));
            assert_eq!(
                parents,
                &[tagged(&fixture.side_a), tagged(&fixture.side_b)]
            );
        }
        other => panic!("merge must be a GitResolution, found {other:?}"),
    }

    let frontier = merge_change.causal_frontier();
    assert!(
        !frontier.is_empty(),
        "the GitResolution must carry a non-empty causal frontier"
    );
    let verified = {
        let txn = repo.pristine().read_txn().unwrap();
        atomic_core::apply::verify_causal_frontier(&txn, &merge_change).expect("frontier verifies")
    };
    assert!(verified.contains(&base_change.hash().unwrap()));
    assert!(verified.contains(&side_a_change.hash().unwrap()));
    assert!(verified.contains(&side_b_change.hash().unwrap()));

    // Dependencies remain context-derived, never the Git parents.
    assert!(!merge_change
        .dependencies()
        .iter()
        .any(|dep| dep.as_bytes() == tagged(&fixture.side_a).as_bytes()));

    let git_meta = merge_change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .expect("git provenance")
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        git_meta.get("origin").and_then(|v| v.as_str()),
        Some("git_resolution")
    );
    assert_eq!(
        git_meta.get("commit").and_then(|v| v.as_str()),
        Some(fixture.merge.as_str())
    );
    assert_eq!(
        git_meta.get("sequencing").and_then(|v| v.as_str()),
        Some("bridge")
    );

    // Content equality: the resolution resolved the union conflict.
    assert_eq!(
        fs::read(root.join("a.txt")).unwrap(),
        b"line1\nresolved\nline9\n"
    );

    // The SynthesizeGit intent is journaled for the resolution.
    let working_copy = repo.require_working_copy_id().unwrap();
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    assert!(
        log.entries
            .iter()
            .any(|entry| entry.operation.payload().kind == OperationKind::SynthesizeGit),
        "the resolution must be journaled"
    );
}

/// Octopus (three-parent) merge: every parent closure is imported, the
/// resolution covers all three, and each leg's content materializes.
#[test]
fn octopus_merge_imports_all_three_parent_closures() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("base.txt"), b"base\n");
    git_commit(&root, "base");

    for (branch, marker) in [
        ("leg-a", "alpha"),
        ("leg-b", "beta"),
        ("leg-c", "gamma"),
    ] {
        git(&root, &["checkout", "-q", "-b", branch]);
        fs::write(root.join(format!("{marker}.txt")), format!("{marker}\n"));
        git_commit(&root, marker);
        git(&root, &["checkout", "-q", "main"]);
    }
    let leg_tips: Vec<(String, String)> = [("leg-a", ""), ("leg-b", ""), ("leg-c", "")]
        .iter()
        .map(|(branch, _)| (branch.to_string(), git(&root, &["rev-parse", branch])))
        .collect();
    git(&root, &["checkout", "-q", "leg-a"]);
    let octopus_out = Command::new("git")
        .args(["merge", "--no-ff", "leg-b", "leg-c", "-m", "octopus merge"])
        .current_dir(&root)
        .output()
        .expect("git octopus merge");
    assert!(
        octopus_out.status.success(),
        "octopus merge must not conflict: {}",
        String::from_utf8_lossy(&octopus_out.stderr)
    );
    let octopus = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "leg-a"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let entries = repo.effective_history(Some("main")).unwrap();
    let octopus_change = {
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(octopus.as_str())
            })
            .expect("octopus imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    match octopus_change.origin() {
        ChangeOrigin::GitResolution { parents, .. } => {
            assert_eq!(parents.len(), 3, "octopus carries all three parents");
        }
        other => panic!("octopus must be a GitResolution, found {other:?}"),
    }
    let verified = {
        let txn = repo.pristine().read_txn().unwrap();
        atomic_core::apply::verify_causal_frontier(&txn, &octopus_change)
            .expect("octopus frontier verifies")
    };
    for (branch, marker) in [("leg-a", "alpha"), ("leg-b", "beta"), ("leg-c", "gamma")] {
        let tip_sha = leg_tips
            .iter()
            .find(|(name, _)| name == branch)
            .map(|(_, sha)| sha.clone())
            .unwrap();
        let tip_hash = {
            let entries = repo.effective_history(Some("main")).unwrap();
            entries
                .iter()
                .find(|entry| {
                    repo.load_change(&entry.hash).unwrap()
                        .unhashed
                        .as_ref()
                        .and_then(|v| v.get("git"))
                        .and_then(|g| g.get("sha"))
                        .and_then(|s| s.as_str())
                        == Some(tip_sha.as_str())
                })
                .expect("leg change imported")
                .hash
        };
        assert!(
            verified.contains(&tip_hash),
            "frontier must cover the {branch} closure"
        );
        assert_eq!(
            fs::read(root.join(format!("{marker}.txt"))).unwrap(),
            format!("{marker}\n").into_bytes(),
            "octopus content must match each leg's contribution"
        );
    }
}

/// A later concurrent change conflicts correctly after serialize/reload,
/// and the GitResolution never resolves a recurring conflict silently
/// (RFC §19 Q3: no automatic replay; explicit user action only).
#[test]
fn concurrent_change_conflicts_after_reload_and_resolution_never_replays() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"line1\nline2\nline3\n").unwrap();
    git_commit(&root, "base");
    let base = git(&root, &["rev-parse", "HEAD"]);

    // Pre-merge import.
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Fork a draft view from the pre-merge state and record a concurrent
    // edit that shares the base context.
    atomic_ok(&root, &home, &["view", "create", "fork", "--draft"]);
    atomic_ok(&root, &home, &["view", "switch", "fork", "--force"]);
    fs::write(root.join("f.txt"), b"line1\nconcurrent\nline2\nline3\n").unwrap();
    atomic_ok(&root, &home, &["add", "f.txt"]);
    atomic_ok(&root, &home, &["record", "-m", "concurrent edit"]);

    // Advance main with a merged history: two divergent edits of the same
    // span plus a resolution.
    git(&root, &["checkout", "-q", "-b", "leg-a", "main"]);
    fs::write(root.join("f.txt"), b"line1\nleg-a\nline2\nline3\n").unwrap();
    git_commit(&root, "leg a");
    git(&root, &["checkout", "-q", "-b", "leg-b", &base]);
    fs::write(root.join("f.txt"), b"line1\nleg-b\nline2\nline3\n").unwrap();
    git_commit(&root, "leg b");
    git(&root, &["checkout", "-q", "leg-a"]);
    let legs_merge = Command::new("git")
        .args(["merge", "--no-ff", "leg-b", "-m", "merge legs"])
        .current_dir(&root)
        .output()
        .expect("git merge legs");
    assert!(
        !legs_merge.status.success(),
        "the fixture legs merge must conflict; the test resolves it explicitly"
    );
    fs::write(root.join("f.txt"), b"line1\nmerged\nline2\nline3\n").unwrap();
    git_commit(&root, "merge legs");
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "leg-a"]);

    // Serialize/reload boundary: the merge imports in a fresh process.
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    let merge_change = {
        let entries = repo.effective_history(Some("main")).unwrap();
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(merge.as_str())
            })
            .expect("merge imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    assert!(matches!(
        merge_change.origin(),
        ChangeOrigin::GitResolution { .. }
    ));
    drop(repo);

    // Re-anchor the workspace checkpoint to the imported merge before the
    // view switch (the stale-baseline guard refuses across a moved HEAD).
    atomic_ok(&root, &home, &["git", "bridge", "reconcile"]);

    // Insert the concurrent change into main: the GitResolution is present
    // in `main` and knows every imported ancestor, but the concurrent
    // change is causally unrelated — the conflict must surface, never be
    // replayed away by the existing resolution (RFC §18).
    atomic_ok(&root, &home, &["view", "switch", "main", "--force"]);
    let inserted = atomic_ok(
        &root,
        &home,
        &["insert", "view", "fork", "--to-view", "main"],
    );
    assert!(
        inserted.to_lowercase().contains("conflict"),
        "expected the insert to surface the concurrent conflict, got: {inserted}"
    );

    // The conflict is durable across a reload: a fresh process still sees
    // it persisted in the view (RFC §19 Q3: no automatic resolution replay;
    // resolving it requires explicit user action).
    let repo = open_repo(&root);
    let working_copy = repo.require_working_copy_id().unwrap();
    let conflicts = repo.list_conflicts(working_copy).unwrap();
    assert!(
        conflicts.iter().any(|(path, _)| path == "f.txt"),
        "the concurrent change must persist as a conflict: {conflicts:?}"
    );
}

/// Distinct empty Git commits remain distinct `Change::empty` objects with
/// hashed tagged OIDs, ordered parents, tree OID, raw author/committer
/// identity and times, and `Derivation::EmptyCommit`.
#[test]
fn distinct_empty_commits_stay_distinct_and_carry_hashed_foreign_facts() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("seed.txt"), b"seed\n").unwrap();
    git_commit(&root, "seed");
    let base = git(&root, &["rev-parse", "HEAD"]);
    let base_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);

    // Two empty commits with byte-identical messages, author, and dates —
    // the exact shape that collapsed into one hash before CB-9B.
    let env = [
        ("GIT_AUTHOR_DATE", "2026-09-11T10:00:00 +0000"),
        ("GIT_COMMITTER_DATE", "2026-09-11T10:00:00 +0000"),
    ];
    let env = [
        ("GIT_AUTHOR_NAME", "CB-9B Tests"),
        ("GIT_AUTHOR_EMAIL", "cb9b@example.com"),
        ("GIT_COMMITTER_NAME", "CB-9B Tests"),
        ("GIT_COMMITTER_EMAIL", "cb9b@example.com"),
        ("GIT_AUTHOR_DATE", "2026-09-11T10:00:00 +0000"),
        ("GIT_COMMITTER_DATE", "2026-09-11T10:00:00 +0000"),
    ];
    git_env(&root, &env, &["commit", "-q", "--allow-empty", "-m", "mark"]);
    let empty1 = git(&root, &["rev-parse", "HEAD"]);
    git_env(&root, &env, &["commit", "-q", "--allow-empty", "-m", "mark"]);
    let empty2 = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(empty1, empty2);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let entries = repo.effective_history(Some("main")).unwrap();
    let load_by_sha = |sha: &str| {
        for entry in &entries {
            let change = repo.load_change(&entry.hash).unwrap();
            if change
                .unhashed
                .as_ref()
                .and_then(|v| v.get("git"))
                .and_then(|g| g.get("sha"))
                .and_then(|s| s.as_str())
                == Some(sha)
            {
                return change;
            }
        }
        panic!("no change for {sha}")
    };
    let change1 = load_by_sha(&empty1);
    let change2 = load_by_sha(&empty2);

    assert_ne!(change1.hash().unwrap(), change2.hash().unwrap());
    // No graph facts: both changes are empty.
    assert!(change1.hunks().is_empty() && change2.hunks().is_empty());
    assert!(!change1.has_file_ops() && !change2.has_file_ops());

    for (change, sha, parent) in [
        (&change1, &empty1, &base),
        (&change2, &empty2, &empty1),
    ] {
        match change.origin() {
            ChangeOrigin::GitSynthesized {
                commit,
                parents,
                derivation,
            } => {
                assert_eq!(*commit, tagged(sha));
                assert_eq!(parents, &[tagged(parent)]);
                assert_eq!(*derivation, GitDerivation::EmptyCommit);
            }
            other => panic!("empty commit must be GitSynthesized/EmptyCommit: {other:?}"),
        }
        let facts: serde_json::Value = serde_json::from_slice(&change.hashed.metadata)
            .expect("hashed metadata decodes as empty-commit facts");
        assert_eq!(
            facts.get("format").and_then(|v| v.as_str()),
            Some("atomic:empty-commit:v1")
        );
        assert_eq!(
            facts.get("tree").and_then(|v| v.as_str()),
            Some(base_tree.as_str())
        );
        assert_eq!(
            facts.pointer("/author/name").and_then(|v| v.as_str()),
            Some("CB-9B Tests")
        );
        assert_eq!(
            facts.pointer("/committer/name").and_then(|v| v.as_str()),
            Some("CB-9B Tests")
        );
    }
    drop(repo);

    // Serialize/reload: a fresh process re-imports idempotently and the
    // distinct objects persist.
    atomic_ok(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    let repo = open_repo(&root);
    let entries = repo.effective_history(Some("main")).unwrap();
    let reload_sha_count = entries
        .iter()
        .filter(|entry| {
            let change = repo.load_change(&entry.hash).unwrap();
            let sha = change
                .unhashed
                .as_ref()
                .and_then(|v| v.get("git"))
                .and_then(|g| g.get("sha"))
                .and_then(|s| s.as_str());
            sha == Some(empty1.as_str()) || sha == Some(empty2.as_str())
        })
        .count();
    assert_eq!(reload_sha_count, 2, "both empty commits remain imported");
}

fn git_env(root: &Path, env: &[(&str, &str)], args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .envs(env.iter().copied())
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Unbound squashes without trailers synthesize one `Derivation::Squash`
/// interpretation; `atomic git bridge review` surfaces candidates with the
/// identity limits, and a reference-transaction event journaled through the
/// real Git wire protocol (state as argv[1], `<old> <new> <ref>` on stdin)
/// is reported as ref-movement evidence only.
#[test]
fn unbound_squash_synthesizes_one_squash_derivation_and_review_surfaces_limits() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("feature.txt"), b"one\n").unwrap();
    git_commit(&root, "add feature");

    // GitHub squash-merge message shape (no Atomic trailers).
    fs::write(root.join("feature.txt"), b"one\ntwo\n").unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "commit",
            "-q",
            "-m",
            "Add feature (#42)\n\n* feat: one\n* feat: two\n\nCo-authored-by: Someone <someone@example.com>",
        ],
    );
    let squash = git(&root, &["rev-parse", "HEAD"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let entries = repo.effective_history(Some("main")).unwrap();
    let squash_change = {
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(squash.as_str())
            })
            .expect("squash imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    match squash_change.origin() {
        ChangeOrigin::GitSynthesized { derivation, .. } => {
            assert_eq!(*derivation, GitDerivation::Squash);
        }
        other => panic!("squash must synthesize a Squash derivation: {other:?}"),
    }
    assert!(squash_change.has_file_ops());
    drop(repo);

    // The review command surfaces the candidate with its identity limits.
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("squash/rewrite candidates: 1"), "{review}");
    assert!(
        review.contains("identity requires a verified predecessor binding or explicit review"),
        "{review}"
    );

    // A reference-transaction event journaled through the real Git wire
    // format — state as argv[1], `<old> <new> <ref>` lines on stdin — is
    // ref-movement evidence only.
    let main_oid = git(&root, &["rev-parse", "refs/heads/main"]);
    let hook_out = atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-reference-transaction", "committed"],
        format!("{main_oid} {squash} refs/heads/main\n").as_bytes(),
    );
    assert!(
        hook_out.status.success(),
        "hook-reference-transaction failed: {}",
        atomic_text(&hook_out)
    );
    let journal = root.join(".atomic/bridge/git-events.jsonl");
    let journal_text = fs::read_to_string(&journal).unwrap();
    let record: serde_json::Value = journal_text
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("a reference-transaction record exists");
    assert_eq!(
        record.get("record_type").and_then(|v| v.as_str()),
        Some("reference-transaction")
    );
    assert_eq!(
        record.get("state").and_then(|v| v.as_str()),
        Some("committed")
    );
    let entry = &record["transactions"][0];
    assert_eq!(
        entry.get("ref_name").and_then(|v| v.as_str()),
        Some("refs/heads/main")
    );
    assert_eq!(
        entry.get("old_oid").and_then(|v| v.as_str()),
        Some(main_oid.as_str())
    );
    assert_eq!(
        entry.get("new_oid").and_then(|v| v.as_str()),
        Some(squash.as_str())
    );

    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(
        review.contains("1 reference-transaction record(s)"),
        "{review}"
    );
    assert!(
        review.contains("ref-movement-only"),
        "review must state the ref-movement identity limit: {review}"
    );
}

/// Review C2: a post-rewrite pair submitted directly to the dispatcher —
/// even the genuine pair of a real amend — is captured with NO operation
/// anchor (the dispatcher runs outside any active captured operation), so it
/// produces a reviewable RewriteCandidate link in the unauthenticated
/// advisory tier: no predecessors named, never operation linkage. Only an
/// anchored capture written during an active captured operation with a
/// verified receipt reaches the linkage tier.
#[test]
fn post_rewrite_event_creates_reviewable_candidate_link() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("doc.md"), b"original\n").unwrap();
    git_commit(&root, "original work");
    let original = git(&root, &["rev-parse", "HEAD"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Amend (a rewrite) with a squash-shaped message.
    fs::write(root.join("doc.md"), b"original\nrewritten\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "--amend", "-m", "Rewrite feature (#7)\n\n* one\n* two"]);
    let rewritten = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(original, rewritten);

    // The Atomic-owned post-rewrite dispatcher journals the old→new pair
    // from stdin.
    let pairs = format!("{original} {rewritten}\n");
    let mut child = Command::new(ATOMIC_BIN)
        .args(["git", "bridge", "hook-post-rewrite"])
        .current_dir(&root)
        .env("HOME", &home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hook-post-rewrite");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(pairs.as_bytes())
        .unwrap();
    let status = child.wait().expect("hook-post-rewrite status");
    assert!(status.success(), "hook-post-rewrite failed");

    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-"))
        .expect("a RewriteCandidate link must exist");
    // The tag names the FULL commit OID: two squashes sharing a short-OID
    // prefix must never collide (review blocker 4).
    assert!(candidate.name.ends_with(&rewritten), "{}", candidate.name);
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("candidate carries structured evidence entries");
    assert_eq!(entries.len(), 1, "one captured-event entry: {entries:?}");
    let entry = &entries[0];
    // Review C2: the direct dispatcher submission is captured durably but
    // carries no operation anchor, so it reads as explicitly unauthenticated
    // advisory evidence — no predecessors named, never operation linkage.
    assert_eq!(
        entry.get("class").and_then(|v| v.as_str()),
        Some("unauthenticated-event")
    );
    assert!(
        entry
            .get("predecessor_oids")
            .and_then(|value| value.as_array())
            .is_none_or(|value| value.is_empty()),
        "the unauthenticated tier never names predecessors: {entry:?}"
    );
    assert_eq!(
        entry.get("authenticated").and_then(|v| v.as_bool()),
        Some(false),
        "unanchored direct-hook input is never authenticated: {entry:?}"
    );

    // The candidate is only a link: the synthesized change is still a
    // Squash-derived interpretation, not a resurrection of the original.
    let entries = repo.effective_history(Some("main")).unwrap();
    let rewrite_change = {
        let hash = entries
            .iter()
            .find(|entry| {
                repo.load_change(&entry.hash).unwrap()
                    .unhashed
                    .as_ref()
                    .and_then(|v| v.get("git"))
                    .and_then(|g| g.get("sha"))
                    .and_then(|s| s.as_str())
                    == Some(rewritten.as_str())
            })
            .expect("rewritten commit imported")
            .hash;
        repo.load_change(&hash).unwrap()
    };
    match rewrite_change.origin() {
        ChangeOrigin::GitSynthesized { derivation, .. } => {
            assert_eq!(*derivation, GitDerivation::Squash);
        }
        other => panic!("rewrite must stay a Squash-derived synthesis: {other:?}"),
    }
    drop(repo);

    // The review output states the exact evidence limits: unanchored hook
    // input is advisory-only, never operation linkage (review C2).
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(!review.contains("post-rewrite-event"), "{review}");
    assert!(review.contains("unauthenticated-evidence-only"), "{review}");
}

fn atomic_env(root: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .envs(envs.iter().copied())
        .output()
        .expect("run atomic")
}

/// Commit with a controlled committer date (the first deterministic bridge
/// tie-break), for ordering-sensitive fixtures.
fn git_commit_at(root: &Path, date: &str, message: &str) -> String {
    git(root, &["add", "."]);
    git_env(
        root,
        &[
            ("GIT_COMMITTER_DATE", date),
            ("GIT_AUTHOR_DATE", date),
            ("GIT_AUTHOR_NAME", "CB-9B Tests"),
            ("GIT_AUTHOR_EMAIL", "cb9b@example.com"),
            ("GIT_COMMITTER_NAME", "CB-9B Tests"),
            ("GIT_COMMITTER_EMAIL", "cb9b@example.com"),
        ],
        &["commit", "-q", "-m", message],
    );
    git(root, &["rev-parse", "HEAD"])
}

/// Find an imported change by Git sha in `view`.
fn change_of(
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
            .and_then(|v| v.get("git"))
            .and_then(|g| g.get("sha"))
            .and_then(|s| s.as_str());
        if imported == Some(sha) {
            return change;
        }
    }
    panic!("no imported change for git commit {sha}")
}

/// Two-branch merge fixture with controllable leg commit times so both
/// valid sibling orders can be exercised (review blocker 1).
struct OrderedMergeFixture {
    root: PathBuf,
    home: PathBuf,
    side_a: String,
    side_b: String,
    merge: String,
}

fn ordered_merge_fixture(a_date: &str, b_date: &str) -> OrderedMergeFixture {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"line1\nline9\n").unwrap();
    git_commit(&root, "base");

    git(&root, &["checkout", "-q", "-b", "side-a"]);
    fs::write(root.join("a.txt"), b"line1\nside-a\nline9\n");
    let side_a = git_commit_at(&root, a_date, "side a");

    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["checkout", "-q", "-b", "side-b"]);
    fs::write(root.join("a.txt"), b"line1\nside-b\nline9\n");
    let side_b = git_commit_at(&root, b_date, "side b");

    git(&root, &["checkout", "-q", "side-a"]);
    let merge_out = Command::new("git")
        .args(["merge", "--no-ff", "side-b", "-m", "merge side work"])
        .current_dir(&root)
        .output()
        .expect("git merge");
    assert!(
        !merge_out.status.success(),
        "the fixture merge must conflict; the test resolves it explicitly"
    );
    fs::write(root.join("a.txt"), b"line1\nresolved\nline9\n");
    git_commit(&root, "merge side work");
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "side-a"]);

    OrderedMergeFixture {
        root,
        home,
        side_a,
        side_b,
        merge,
    }
}

fn assert_no_sibling_dependency(
    repo: &atomic_repository::Repository,
    view: &str,
    left_sha: &str,
    right_sha: &str,
) {
    let left = change_of(repo, view, left_sha);
    let right = change_of(repo, view, right_sha);
    let left_deps: Vec<String> = left
        .dependencies()
        .iter()
        .map(|dep| dep.to_base32())
        .collect();
    let right_deps: Vec<String> = right
        .dependencies()
        .iter()
        .map(|dep| dep.to_base32())
        .collect();
    let left_hash = left.hash().unwrap().to_base32();
    let right_hash = right.hash().unwrap().to_base32();
    assert_ne!(left_hash, right_hash);
    assert!(
        !right_deps.contains(&left_hash),
        "the right leg must NOT depend on the sibling left leg: {right_deps:?}"
    );
    assert!(
        !left_deps.contains(&right_hash),
        "the left leg must NOT depend on the sibling right leg: {left_deps:?}"
    );
}

/// Review blocker 1: each merge leg assembles against its own Git parent's
/// interpreted closure — a sibling leg's changes never become the leg's
/// dependency or baseline — in BOTH valid sibling orders. The resolution
/// still records context-derived dependencies on both legs and carries
/// regenerated semantic FileOps whose word diff survives reload (review
/// blocker 3).
#[test]
fn merge_legs_record_no_sibling_dependency_in_both_orders() {
    for (a_date, b_date) in [
        ("2026-09-11T10:00:00 +0000", "2026-09-11T11:00:00 +0000"),
        ("2026-09-11T11:00:00 +0000", "2026-09-11T10:00:00 +0000"),
    ] {
        let fixture = ordered_merge_fixture(a_date, b_date);
        let root = fixture.root.as_path();
        let home = fixture.home.as_path();

        atomic_ok(root, home, &["git", "import", "--no-vault"]);

        let repo = open_repo(root);
        assert_no_sibling_dependency(&repo, "main", &fixture.side_a, &fixture.side_b);
        assert_no_sibling_dependency(&repo, "main", &fixture.side_b, &fixture.side_a);

        // The merge resolution: GitResolution origin, context-derived deps,
        // and semantic FileOps (review blocker 3).
        let merge = change_of(&repo, "main", &fixture.merge);
        assert!(matches!(
            merge.origin(),
            ChangeOrigin::GitResolution { .. }
        ));
        assert!(
            merge.has_file_ops(),
            "the merge resolution must emit semantic FileOps (review blocker 3)"
        );
        assert!(
            !merge.hunks().is_empty(),
            "the merge resolution carries graph ops"
        );
        let verified = frontier_closure_of(&repo, &merge);
        assert!(verified.contains(&change_of(&repo, "main", &fixture.side_a).hash().unwrap()));
        assert!(verified.contains(&change_of(&repo, "main", &fixture.side_b).hash().unwrap()));
        let merge_hash = merge.hash().unwrap().to_base32();
        drop(repo);

        // The recorded resolution's word diff survives a reload and renders
        // the resolved token (semantic layer, not opaque bytes).
        let word_diff = atomic_ok(
            root,
            home,
            &["diff", "-c", &merge_hash, "--word-diff", "a.txt"],
        );
        assert!(
            word_diff.contains("resolved"),
            "the reloaded word diff must render the resolution token: {word_diff}"
        );

        // The staged verification proved the graph bytes: status is clean.
        let status = atomic_ok(root, home, &["status", "-s"]);
        assert!(
            status.trim().is_empty(),
            "import must leave a clean working copy: {status}"
        );

        // Full-tree verification of the imported state against Git (review
        // blocker 2: complete tree comparison, not just the touched paths).
        atomic_ok(root, home, &["git", "bridge", "verify"]);
    }
}

/// Review blocker 5: installed Atomic advisory hooks journal real Git
/// reference-transaction transitions — committed create/delete through the
/// owned dispatcher, an aborted transaction through an unmanaged wrapper,
/// and a ref update from a linked worktree (shared common hooks directory).
/// Prepared/aborted records never claim movement.
#[test]
fn installed_reference_transaction_hooks_record_real_git_transitions() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"one\n").unwrap();
    git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Install the owned dispatchers.
    let enable = atomic_ok(&root, &home, &["git", "bridge", "enable"]);
    let hooks_dir = root.join(".git/hooks");
    let hook_script = fs::read_to_string(hooks_dir.join("reference-transaction")).unwrap();
    assert!(
        hook_script.contains("hook-reference-transaction \"$1\""),
        "the installed dispatcher must forward Git's argv[1] state: {hook_script}"
    );

    // Committed ref creation: <old=zero> <new=sha> <ref> on stdin.
    let head = git(&root, &["rev-parse", "HEAD"]);
    let zero40 = "0".repeat(40);
    git(
        &root,
        &["update-ref", "refs/heads/feature", &head],
    );
    let journal_path = root.join(".atomic/bridge/git-events.jsonl");
    let records = |path: &std::path::Path| -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    };
    let journal_records = records(&journal_path);
    let committed: Vec<&serde_json::Value> = journal_records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(|v| v.as_str()) == Some("reference-transaction")
                && r.get("state").and_then(|v| v.as_str()) == Some("committed")
        })
        .collect();
    assert!(
        !committed.is_empty(),
        "the committed creation must be journaled"
    );
    let entry = &committed[committed.len() - 1]["transactions"][0];
    assert_eq!(
        entry.get("old_oid").and_then(|v| v.as_str()),
        Some(zero40.as_str())
    );
    assert_eq!(entry.get("new_oid").and_then(|v| v.as_str()), Some(head.as_str()));
    assert_eq!(
        entry.get("ref_name").and_then(|v| v.as_str()),
        Some("refs/heads/feature")
    );

    // Committed ref deletion: git 2.55 passes the unknown old value as
    // zeros on stdin — journaled as received.
    git(&root, &["update-ref", "-d", "refs/heads/feature"]);
    let journal_records = records(&journal_path);
    let committed: Vec<&serde_json::Value> = journal_records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(|v| v.as_str()) == Some("reference-transaction")
                && r.get("state").and_then(|v| v.as_str()) == Some("committed")
        })
        .collect();
    let entry = &committed[committed.len() - 1]["transactions"][0];
    assert_eq!(
        entry.get("old_oid").and_then(|v| v.as_str()),
        Some(zero40.as_str())
    );
    assert_eq!(
        entry.get("new_oid").and_then(|v| v.as_str()),
        Some(zero40.as_str())
    );

    // Aborted transaction through an unmanaged wrapper hook: the atomic
    // dispatcher journals the prepared state, the wrapper fails the
    // transaction, and Git re-invokes the hook with the aborted state. A
    // prepared/aborted record is NOT proof of movement.
    let owned_script = fs::read_to_string(hooks_dir.join("reference-transaction")).unwrap();
    let atomic_sh = ATOMIC_BIN.to_string().replace('\'', "'\\''");
    fs::write(
        hooks_dir.join("reference-transaction"),
        format!("#!/bin/sh\n'{atomic_sh}' git bridge hook-reference-transaction \"$1\" || true\nexit 1\n"),
    )
    .unwrap();
    let before_commits = {
        let repo = open_repo(&root);
        repo.effective_history(Some("main")).unwrap().len()
    };
    let refused = Command::new("git")
        .args(["update-ref", "refs/heads/feature", &head])
        .current_dir(&root)
        .output()
        .expect("run git update-ref");
    assert!(
        !refused.status.success(),
        "the failing wrapper hook must abort the transaction"
    );
    let journal_records = records(&journal_path);
    let aborted_records: Vec<&serde_json::Value> = journal_records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(|v| v.as_str()) == Some("reference-transaction")
                && matches!(
                    r.get("state").and_then(|v| v.as_str()),
                    Some("prepared") | Some("aborted")
                )
        })
        .collect();
    let states: Vec<&str> = aborted_records
        .iter()
        .filter_map(|r| r.get("state").and_then(|v| v.as_str()))
        .collect();
    assert!(
        states.contains(&"prepared") && states.contains(&"aborted"),
        "both the prepared and aborted states must be journaled: {states:?}"
    );
    assert!(
        aborted_records
            .iter()
            .all(|r| r.get("interpretation").and_then(|v| v.as_str())
                == Some("ref-movement-only: a preparing, prepared, or aborted transaction proves only that a transaction ran; it is NOT proof that a ref moved and is never rewrite identity (RFC 5.4)")),
        "prepared/aborted records must carry the not-proof-of-movement limit"
    );
    // The aborted transaction moved nothing: the ref must not exist.
    let ref_check = Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/feature"])
        .current_dir(&root)
        .output()
        .expect("run git rev-parse");
    assert!(
        !ref_check.status.success(),
        "the ref must not exist after the aborted transaction"
    );
    let repo = open_repo(&root);
    assert_eq!(
        repo.effective_history(Some("main")).unwrap().len(),
        before_commits,
        "an aborted transaction must not import or advance anything"
    );
    drop(repo);

    // Linked worktree: hooks resolve through the shared common directory.
    // Restore the owned dispatcher first: the wrapper from the aborted
    // case is unmanaged and would abort the worktree's own ref updates.
    fs::write(hooks_dir.join("reference-transaction"), owned_script).unwrap();
    git(
        &root,
        &["worktree", "add", "-q", "wt", "-b", "wt-branch", "main"],
    );
    let wt_head = git(&root, &["rev-parse", "main"]);
    git(
        &root.join("wt"),
        &["update-ref", "refs/heads/wt-branch", &wt_head],
    );
    let journal_records = records(&journal_path);
    let committed: Vec<&serde_json::Value> = journal_records
        .iter()
        .filter(|r| {
            r.get("record_type").and_then(|v| v.as_str()) == Some("reference-transaction")
                && r.get("state").and_then(|v| v.as_str()) == Some("committed")
        })
        .collect();
    let entry = &committed[committed.len() - 1]["transactions"][0];
    assert_eq!(
        entry.get("ref_name").and_then(|v| v.as_str()),
        Some("refs/heads/wt-branch"),
        "the linked worktree's ref transaction must reach the shared dispatcher"
    );
}

/// Review blocker 4 + B3: forged, partial, prefix, and non-rewrite journal
/// records never become captured-operation linkage. A durably captured
/// record naming two fully known commits (both locally interpreted) is
/// STILL refused when the named pair does not describe a rewrite the live
/// Git object database verifies — the old tip of an ordinary commit stays an
/// ancestor of the new tip, so no rewrite happened. A genuine amend
/// (review B3's authenticated-operation case) is tested separately in
/// `captured_rewrite_event_authenticates_a_real_git_rewrite`.
#[test]
fn forged_and_partial_events_never_become_candidate_linkage() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("doc.md"), b"original\n").unwrap();
    git_commit(&root, "original work");
    let original = git(&root, &["rev-parse", "HEAD"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Two squashes: one with only forged/partial journal records, one whose
    // fully known but non-rewrite record is journaled below through the real
    // CLI wire format.
    fs::write(root.join("doc.md"), b"forged content\n");
    git(&root, &["add", "."]);
    git(
        &root,
        &["commit", "-q", "-m", "Forged feature (#11)\n\n* one"],
    );
    let forged = git(&root, &["rev-parse", "HEAD"]);

    fs::write(root.join("doc.md"), b"valid content\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "Valid feature (#12)\n\n* two"]);
    let valid = git(&root, &["rev-parse", "HEAD"]);

    // Journal the fully known pair through the real CLI wire format AFTER
    // the commits exist: both OIDs are locally interpreted and the event is
    // durably captured, but NO rewrite happened (`original` remains an
    // ancestor of the ordinary child), so the pair is not a rewrite witness
    // and must read as unauthenticated advisory evidence — never operation
    // linkage (review B3: known objects are not authentication).
    atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-post-rewrite"],
        format!("{original} {valid}").as_bytes(),
    );

    // Journal a durably captured event whose predecessor was never
    // interpreted by Atomic (review F5): the capture succeeds, but the
    // record must read as unauthenticated advisory evidence, never
    // operation linkage.
    atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-post-rewrite"],
        format!("{} {forged}", "1".repeat(40)).as_bytes(),
    );

    // Forge the junk records against the other squash: prefix OID, empty
    // OID, a record without the advisory flag, a record with a foreign
    // interpretation, and a garbage line. None of the junk is
    // captured-operation proof. The CLI-journaled records are appended
    // verbatim (no byte doctoring: the captured records must match the
    // journal byte for byte).
    let journal = root.join(".atomic/bridge/git-events.jsonl");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    let journal_text = fs::read_to_string(&journal).unwrap();
    let forged_records = format!(
        concat!(
            "{{\"record_type\":\"post-rewrite\",\"advisory\":true,\"interpretation\":\"{interp}\",\"event_id\":\"prefix\",\"rewritten\":[{{\"old_oid\":\"{original}\",\"new_oid\":\"{prefix}\"}}]}}\n",
            "{{\"record_type\":\"post-rewrite\",\"advisory\":true,\"interpretation\":\"{interp}\",\"event_id\":\"empty\",\"rewritten\":[{{\"old_oid\":\"{original}\",\"new_oid\":\"\"}}]}}\n",
            "{{\"record_type\":\"post-rewrite\",\"advisory\":false,\"interpretation\":\"{interp}\",\"event_id\":\"notadvisory\",\"rewritten\":[{{\"old_oid\":\"{original}\",\"new_oid\":\"{forged}\"}}]}}\n",
            "{{\"record_type\":\"post-rewrite\",\"advisory\":true,\"interpretation\":\"someone else's journal text\",\"event_id\":\"wronginterp\",\"rewritten\":[{{\"old_oid\":\"{original}\",\"new_oid\":\"{forged}\"}}]}}\n",
            "this line is not json at all\n"
        ),
        interp = "operation-linkage-only: this event links a Git rewrite operation to the named commits; change identity still requires binding verification or explicit review (RFC 5.4)",
        prefix = &forged[..12],
        original = original,
        forged = forged,
    );
    fs::write(&journal, format!("{journal_text}{forged_records}")).unwrap();

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    // The junk records (wrong shapes, no advisory flag, foreign
    // interpretation) are untrusted and never create a candidate. Both
    // squash candidates carry exactly one evidence entry each: the F5 event
    // (never-interpreted predecessor) and the B3 pair (fully known, durably
    // captured, but no structural rewrite), each as UNAUTHENTICATED advisory
    // evidence that never names predecessors (review F5 + B3).
    for tip in [&forged, &valid] {
        let candidate = tags
            .iter()
            .find(|tag| tag.name.starts_with("rewrite-candidate-") && tag.name.ends_with(tip))
            .unwrap_or_else(|| panic!("the captured event for {tip} must surface as unauthenticated evidence: {tags:?}"));
        let entries = candidate
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("evidence"))
            .and_then(|value| value.as_array())
            .cloned()
            .expect("structured evidence entries");
        assert_eq!(entries.len(), 1, "only the one captured event: {entries:?}");
        assert_eq!(
            entries[0].get("class").and_then(|v| v.as_str()),
            Some("unauthenticated-event"),
            "a captured event without a verified rewrite pair is unauthenticated: {entries:?}"
        );
        assert_eq!(
            entries[0].get("authenticated").and_then(|v| v.as_bool()),
            Some(false),
            "the unauthenticated entry must not claim authentication: {entries:?}"
        );
        assert!(
            entries[0].get("predecessor_oids")
                .and_then(|v| v.as_array())
                .is_none_or(|v| v.is_empty()),
            "the unauthenticated tier never names predecessors: {entries:?}"
        );
    }
    drop(repo);
    // The review output states the exact evidence limits: every captured
    // record here is unauthenticated-evidence-only, and no operation
    // linkage is asserted for any candidate (review B3).
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("unauthenticated-evidence-only"), "{review}");
    assert!(
        !review.contains("class=post-rewrite-event"),
        "a non-rewrite pair must never display as post-rewrite-event linkage: {review}"
    );
}

/// Review blocker 4: a locally verified binding whose bound tree equals the
/// squash's tree produces an advisory RewriteCandidate link with hooks
/// completely off — found independently of the journal, never identity.
#[test]
fn binding_tree_match_creates_candidate_without_hooks() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("code.rs"), b"fn v1() {}\n").unwrap();
    git_commit(&root, "base");
    let base = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    atomic_ok(&root, &home, &["git", "bridge", "reconcile"]);

    // Record v2 through Atomic so the bound projection tree carries v2.
    fs::write(root.join("code.rs"), b"fn v2() {}\n").unwrap();
    atomic_ok(&root, &home, &["add", "code.rs"]);
    atomic_ok(&root, &home, &["record", "-m", "record v2"]);

    // Sign and publish a verified binding for the projected view state.
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x9du8.wrapping_add((index as u8) * 7 + 11);
    }
    let key_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    // The key file lives outside the repository: anything inside would be
    // committed and change the squash's tree away from the bound tree.
    let key_file = home.join("binding-key.hex");
    fs::write(&key_file, key_hex).unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // The squash: a Git commit whose tree equals the bound tree (v2 bytes +
    // the seeded .atomicignore), no hooks installed, no journal records.
    // The atomic record projected v2 into Git already, so reset to the base
    // first: the squash's parent must hold v1 for the squash to be non-empty
    // while its tree still equals the bound projection tree.
    git(&root, &["reset", "-q", "--hard", &base]);
    fs::write(root.join("code.rs"), b"fn v2() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "Squashed feature (#9)\n\n* one\n* two"]);
    let squashed = git(&root, &["rev-parse", "HEAD"]);
    assert!(
        !root.join(".atomic/bridge/git-events.jsonl").exists(),
        "the fixture must run with hooks off (no journal)"
    );

    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-"))
        .expect("a binding-tree-match candidate must exist without hook evidence");
    assert!(candidate.name.ends_with(&squashed), "{}", candidate.name);
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("structured evidence");
    let entry = &entries[0];
    assert_eq!(
        entry.get("class").and_then(|v| v.as_str()),
        Some("binding-tree-match")
    );
    let binding_ids = entry
        .get("binding_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("the binding id backs the evidence");
    assert!(!binding_ids.is_empty());

    // The candidate is a link only: the synthesized change stays a
    // Squash-derived interpretation, never a resurrection.
    let change = change_of(&repo, "main", &squashed);
    match change.origin() {
        ChangeOrigin::GitSynthesized { derivation, .. } => {
            assert_eq!(*derivation, GitDerivation::Squash);
        }
        other => panic!("tree similarity must not become identity: {other:?}"),
    }
}

/// Review F6: a locally verified binding that covers a known intermediate of
/// a squashed range produces an advisory RewriteCandidate link with hooks
/// completely off — found independently of final-tree equality, whole-blob
/// survival, and the squash message format. A later edit to the same file in
/// the squashed range must not lose the binding link.
#[test]
fn binding_range_match_covers_known_intermediate_without_blob_or_message_similarity() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("code.rs"), b"fn v1() {}\n").unwrap();
    git_commit(&root, "base");
    let base = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    atomic_ok(&root, &home, &["git", "bridge", "reconcile"]);

    // Record v2 through Atomic so the bound projection tree carries v2.
    fs::write(root.join("code.rs"), b"fn v2() {}\n").unwrap();
    atomic_ok(&root, &home, &["add", "code.rs"]);
    atomic_ok(&root, &home, &["record", "-m", "record v2"]);

    // Sign and publish a verified binding for the projected view state: the
    // bound commit is a strict descendant of `base`, so it lies inside the
    // rewritten range of a later squash onto `base`.
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x71u8.wrapping_add((index as u8) * 7 + 11);
    }
    let key_hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    let key_file = home.join("binding-key.hex");
    fs::write(&key_file, key_hex).unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // The squashed range: the squash onto `base` rewrites the whole range
    // (bound intermediate + a later same-file edit) into one commit, so the
    // bound tree differs from the squash tree AND every whole edited blob
    // was later edited away. Binding the intermediate must still surface.
    git(&root, &["reset", "-q", "--hard", &base]);
    fs::write(root.join("code.rs"), b"fn v2() {}\nfn v3() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "Squashed feature (#9)\n\n* one\n* two"]);
    let squashed = git(&root, &["rev-parse", "HEAD"]);
    assert!(
        !root.join(".atomic/bridge/git-events.jsonl").exists(),
        "the fixture must run with hooks off (no journal)"
    );

    // The whole hooks-off range flow: the import itself must succeed (the
    // union already renders the squash tree for the tracked path, so no new
    // FileOps are required — review F2).
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-") && tag.name.ends_with(&squashed))
        .expect("a binding-range-match candidate must exist for the known intermediate");
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("structured evidence");
    let entry = &entries[0];
    assert_eq!(
        entry.get("class").and_then(|v| v.as_str()),
        Some("binding-range-match"),
        "the known-range binding must back the candidate: {entries:?}"
    );
    let binding_ids = entry
        .get("binding_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("the binding id backs the evidence");
    assert!(!binding_ids.is_empty());
    let predecessors = entry
        .get("predecessor_oids")
        .and_then(|value| value.as_array())
        .cloned()
        .expect("the candidate names the bound predecessor");
    assert!(!predecessors.is_empty());

    // The candidate is a link only: the synthesized change stays a
    // Squash-derived interpretation, never a resurrection, and the tracked
    // path materializes the squashed bytes.
    let change = change_of(&repo, "main", &squashed);
    match change.origin() {
        ChangeOrigin::GitSynthesized { derivation, .. } => {
            assert_eq!(*derivation, GitDerivation::Squash);
        }
        other => panic!("range containment must not become identity: {other:?}"),
    }
    assert_eq!(
        repo.get_file_content("code.rs").unwrap(),
        Some(b"fn v2() {}\nfn v3() {}\n".to_vec()),
        "the squashed tree must reconstruct exactly"
    );
    drop(repo);

    // The review output names the known-range evidence and its identity limit.
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("binding-range-match"), "{review}");
}

/// Review blocker 6: raw foreign facts are lossless — non-UTF-8 identity
/// bytes, timezone offsets, and the complete raw signed commit object
/// survive in the hash-covered metadata, and the exact signed object can be
/// re-verified (its OID recomputed) without the source Git ODB.
#[test]
fn raw_foreign_facts_survive_without_the_source_odb() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("seed.txt"), b"seed\n").unwrap();
    git_commit(&root, "seed");

    // An empty commit with a non-UTF-8 author name and a +05:45 timezone.
    let non_utf8_name: &[u8] = b"J\xf8rgen <b\xFCrger@example.com>";
    let date = "2026-09-11T12:34:56+05:45";
    let output = Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", "mark"])
        .current_dir(&root)
        .env(
            std::ffi::OsStr::new("GIT_AUTHOR_NAME"),
            std::ffi::OsStr::from_bytes(non_utf8_name),
        )
        .env("GIT_COMMITTER_NAME", "CB-9B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb9b@example.com")
        .env("GIT_AUTHOR_EMAIL", "cb9b@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("run git commit");
    assert!(
        output.status.success(),
        "git accepted the non-UTF-8 author: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let empty1 = git(&root, &["rev-parse", "HEAD"]);
    let raw_object = {
        let out = Command::new("git")
            .args(["cat-file", "commit", &empty1])
            .current_dir(&root)
            .output()
            .expect("git cat-file");
        assert!(out.status.success());
        out.stdout
    };

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let change = change_of(&repo, "main", &empty1);
    let facts: serde_json::Value =
        serde_json::from_slice(&change.hashed.metadata).expect("hashed facts decode");
    assert_eq!(
        facts.get("format").and_then(|v| v.as_str()),
        Some("atomic:empty-commit:v1")
    );
    // Raw identity bytes are lossless: they equal the author-name bytes as
    // they appear in the raw signed object (git itself normalizes and
    // re-encodes idents, so the object — and the stored copy — hold those
    // exact normalized bytes).
    let author_name_raw = decode_hex(
        facts
            .pointer("/author/name_raw_hex")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let author_start = raw_object
        .windows(7)
        .position(|w| w == b"author ")
        .expect("the raw object carries an author line");
    let name_end = raw_object[author_start + 7..]
        .iter()
        .position(|&b| b == b'<')
        .expect("the author line carries an email")
        + author_start
        + 7;
    let mut object_name_bytes = raw_object[author_start + 7..name_end].to_vec();
    while object_name_bytes.last() == Some(&b' ') {
        object_name_bytes.pop();
    }
    assert_eq!(author_name_raw, object_name_bytes);
    // The timezone offset is preserved, never normalized to zero.
    assert_eq!(
        facts
            .pointer("/author/time_offset_seconds")
            .and_then(|v| v.as_i64()),
        Some(5 * 3600 + 45 * 60)
    );
    // The complete raw signed object survives byte for byte.
    let raw_hex = facts
        .get("raw_object_hex")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(decode_hex(raw_hex), raw_object);
    drop(repo);

    // Recovery without the source ODB: remove the Git object database and
    // recompute the commit OID from the stored raw bytes.
    fs::rename(root.join(".git/objects"), root.join(".git/objects.bak")).unwrap();
    let recomputed = {
        let mut child = Command::new("git")
            .args(["hash-object", "-t", "commit", "--stdin"])
            .current_dir(&root)
            .env("GIT_OBJECT_DIRECTORY", root.join(".git/scratch-odb"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git hash-object");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&raw_object)
            .unwrap();
        let out = child.wait_with_output().expect("hash-object output");
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    fs::rename(root.join(".git/objects.bak"), root.join(".git/objects")).unwrap();
    assert_eq!(
        recomputed, empty1,
        "the stored raw object must re-hash to the recorded commit OID"
    );
}

/// Review blocker 7 (owner decision: "Explicit reapplication (Recommended)"):
/// a GitResolution never replays automatically, but the agreed contract
/// permits reusing it through explicit insertion into another view. The
/// recurring conflict itself is pinned by
/// `concurrent_change_conflicts_after_reload_and_resolution_never_replays`.
#[test]
fn explicit_insertion_reapplies_the_resolution_in_another_view() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"line1\nline2\nline3\n").unwrap();
    git_commit(&root, "base");
    let base = git(&root, &["rev-parse", "HEAD"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Fork a draft view from the pre-merge state: the recurring-conflict
    // home for the resolution if it were replayed automatically.
    atomic_ok(&root, &home, &["view", "create", "fork", "--draft"]);
    atomic_ok(&root, &home, &["view", "switch", "fork", "--force"]);

    // Advance main with a merged, resolved history.
    git(&root, &["checkout", "-q", "-b", "leg-a", "main"]);
    fs::write(root.join("f.txt"), b"line1\nleg-a\nline2\nline3\n").unwrap();
    git_commit(&root, "leg a");
    git(&root, &["checkout", "-q", "-b", "leg-b", &base]);
    fs::write(root.join("f.txt"), b"line1\nleg-b\nline2\nline3\n").unwrap();
    git_commit(&root, "leg b");
    git(&root, &["checkout", "-q", "leg-a"]);
    let legs_merge = Command::new("git")
        .args(["merge", "--no-ff", "leg-b", "-m", "merge legs"])
        .current_dir(&root)
        .output()
        .expect("git merge legs");
    assert!(
        !legs_merge.status.success(),
        "the fixture legs merge must conflict; the test resolves it explicitly"
    );
    fs::write(root.join("f.txt"), b"line1\nmerged\nline2\nline3\n").unwrap();
    git_commit(&root, "merge legs");
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "leg-a"]);

    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    atomic_ok(&root, &home, &["git", "bridge", "reconcile"]);

    let repo = open_repo(&root);
    let merge_hash = change_of(&repo, "main", &merge).hash().unwrap().to_base32();
    drop(repo);

    // Explicit reapplication: the owner-agreed path inserts the resolution
    // (with its closure) into the other view on demand.
    atomic_ok(
        &root,
        &home,
        &["insert", "change", &merge_hash, "--to-view", "fork"],
    );

    // The fork now carries the explicitly reapplied resolution's content.
    let repo = open_repo(&root);
    let fork_bytes = repo
        .get_file_content_on_view("f.txt", "fork")
        .unwrap()
        .expect("the fork view holds the reapplied resolution");
    assert_eq!(fork_bytes, b"line1\nmerged\nline2\nline3\n".as_slice());
    // And the change is durably a member of the fork.
    let entries = repo.effective_history(Some("fork")).unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.hash.to_base32() == merge_hash),
        "the explicitly inserted resolution must be a fork member"
    );
}

/// Review blocker 8: required sources deleted from the graph but still on
/// disk are re-included by the automatic turn-end recording, and the
/// resulting recorded state materializes cleanly on its own.
#[test]
fn repaired_tracking_materializes_the_recorded_state_cleanly() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    let files = [
        ("alpha.rs", b"pub fn alpha() {}\n".as_slice()),
        ("beta.rs", b"pub fn beta() {}\n".as_slice()),
        ("gamma.rs", b"pub fn gamma() {}\n".as_slice()),
        ("delta.rs", b"pub fn delta() {}\n".as_slice()),
    ];
    for (name, bytes) in files {
        fs::write(root.join(name), bytes).unwrap();
    }
    git_commit(&root, "add modules");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // The handoff shape: a recorded deletion of the four files while their
    // bytes are written back to disk afterwards (exactly the CB-8B
    // candidate situation: FileDel recorded, bytes untracked on disk).
    for (name, _) in files {
        fs::remove_file(root.join(name)).unwrap();
    }
    atomic_ok(&root, &home, &["record", "-m", "handoff delete"]);
    for (name, bytes) in files {
        fs::write(root.join(name), bytes).unwrap();
    }
    let status = atomic_ok(&root, &home, &["status", "-s"]);
    for (name, _) in files {
        assert!(
            status
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name)),
            "the four deleted sources must surface for inclusion: {status}"
        );
    }

    // The turn-end recording repairs the inclusion (hooks own add/record).
    atomic_ok(&root, &home, &["add", "."]);
    atomic_ok(&root, &home, &["record", "-m", "re-add required sources"]);
    let status = atomic_ok(&root, &home, &["status", "-s"]);
    assert!(
        status.trim().is_empty(),
        "the repaired state must be clean: {status}"
    );

    // Clean materialization: the recorded graph alone produces every file.
    let repo = open_repo(&root);
    let working_copy = repo.require_working_copy_id().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let outcome = repo
        .archive(
            working_copy,
            destination.path().join("materialized/"),
            atomic_repository::ArchiveOptions::directory().view("main"),
        )
        .expect("archive the recorded view");
    assert!(
        !outcome.manifest.entries.is_empty(),
        "the archive must contain the tracked files"
    );
    for (name, bytes) in files {
        assert_eq!(
            fs::read(destination.path().join("materialized").join(name)).unwrap(),
            bytes,
            "{name} must materialize byte-identical from the recorded graph"
        );
    }
}

/// Review blocker 2: the staged graph verification fails closed. Each
/// injected fault (record, missing vertex, incomplete dependency index,
/// tree projection) aborts the synthesis before publication: the failing
/// change and every successor stay invisible and unverified, the worktree
/// and the view state are untouched, and a clean retry succeeds. Requires
/// the `adoption-test-injection` feature build.
#[cfg(feature = "adoption-test-injection")]
#[test]
fn synthesis_failpoints_fail_closed_and_publish_nothing() {
    for fail_var in [
        "ATOMIC_FAIL_SYNTHESIS_RECORD",
        "ATOMIC_FAIL_SYNTHESIS_VERTEX",
        "ATOMIC_FAIL_SYNTHESIS_PROJECTION",
        // Review F4 semantic corruption injections: the corrupted CRDT
        // state (missing trunk, wrong branch payload) must be refused
        // before publication, exactly like the other failpoints.
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_MISSING_TRUNK",
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_CORRUPT_VERTEX",
        // Review F4 semantic corruption injections: the corrupted CRDT
        // state (missing trunk, wrong branch payload) must be refused
        // before publication, exactly like the other failpoints.
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_MISSING_TRUNK",
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_CORRUPT_VERTEX",
        // Review F4 untouched-path corruption: corrupting the inherited
        // semantic state of a path this commit did not touch must also be
        // refused.
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_CORRUPT_UNTOUCHED",
        // Review C1 inherited-semantic corruption probes: flipped token
        // payload bounds, flipped token kinds, token ownership pointed at a
        // nonexistent branch, and an unattributed trunk tombstone on paths
        // this commit did not touch. The verification must compare the
        // inherited token state exactly, so every one of these must be
        // refused before publication.
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_RANGE",
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_KIND",
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_LEAF_OWNER",
        "ATOMIC_FAIL_SYNTHESIS_SEMANTIC_TRUNK_DELETED",
    ] {
        let root = tempfile::tempdir().unwrap().keep();
        let home = tempfile::tempdir().unwrap().keep();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("f.txt"), b"base\n").unwrap();
        // A second tracked file the edit commit never touches: its inherited
        // semantic state is what the untouched-corruption injection damages.
        fs::write(root.join("g.txt"), b"untouched\n").unwrap();
        git_commit(&root, "base");
        atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

        let before_changes = {
            let repo = open_repo(&root);
            repo.effective_history(Some("main")).unwrap().len()
        };

        // The second commit must fail at the injected stage.
        fs::write(root.join("f.txt"), b"base\nedit\n").unwrap();
        git_commit(&root, "edit");
        let failed = atomic_env(
            &root,
            &home,
            &["git", "import", "--incremental", "--no-vault"],
            &[(fail_var, "1")],
        );
        assert!(
            !failed.status.success(),
            "{fail_var} must fail the synthesis: {}",
            atomic_text(&failed)
        );
        let failed_text = atomic_text(&failed);
        assert!(
            failed_text.contains(fail_var) || failed_text.contains("staged synthesis"),
            "the failure must be attributed to the injected fault: {failed_text}"
        );

        // Nothing advanced: no new change is visible or verified, the
        // worktree still renders the base content, and the view state is
        // unchanged.
        let repo = open_repo(&root);
        assert_eq!(
            repo.effective_history(Some("main")).unwrap().len(),
            before_changes,
            "the failing change must not become a visible view member"
        );
        drop(repo);
        assert_eq!(
            fs::read(root.join("f.txt")).unwrap(),
            b"base\nedit\n".as_slice(),
            "the worktree must be untouched by the failed synthesis"
        );

        // A clean retry succeeds and the journal shows the recovery.
        atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
        let repo = open_repo(&root);
        assert_eq!(
            repo.effective_history(Some("main")).unwrap().len(),
            before_changes + 1,
            "the retry must publish the verified change"
        );
        let change = change_of(&repo, "main", git(&root, &["rev-parse", "HEAD"]).as_str());
        assert_eq!(
            repo.get_file_content_on_view("f.txt", "main").unwrap(),
            Some(b"base\nedit\n".to_vec()),
            "the verified change must render the new bytes"
        );
        assert_eq!(
            repo.get_file_content_on_view("g.txt", "main").unwrap(),
            Some(b"untouched\n".to_vec()),
            "the untouched path must render its inherited bytes"
        );
        drop(repo);
        let _ = change;
    }
}

/// Review blocker 2 (resolution-specific): an incomplete dependency index
/// for a merge resolution fails the frontier verification inside the
/// applying transaction. The legs (already published) stay, the resolution
/// and everything after it stay invisible, and a clean retry succeeds.
#[cfg(feature = "adoption-test-injection")]
#[test]
fn merge_resolution_frontier_failpoint_fails_closed() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"line1\nline9\n").unwrap();
    git_commit(&root, "base");

    // Import the pre-merge history first.
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    let before_changes = {
        let repo = open_repo(&root);
        repo.effective_history(Some("main")).unwrap().len()
    };

    // Then advance Git with the merged, resolved history.
    git(&root, &["checkout", "-q", "-b", "side-a"]);
    fs::write(root.join("a.txt"), b"line1\nside-a\nline9\n");
    git_commit(&root, "side a");
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["checkout", "-q", "-b", "side-b"]);
    fs::write(root.join("a.txt"), b"line1\nside-b\nline9\n");
    git_commit(&root, "side b");
    git(&root, &["checkout", "-q", "side-a"]);
    let merge_out = Command::new("git")
        .args(["merge", "--no-ff", "side-b", "-m", "merge side work"])
        .current_dir(&root)
        .output()
        .expect("git merge");
    assert!(!merge_out.status.success(), "fixture merge must conflict");
    fs::write(root.join("a.txt"), b"line1\nresolved\nline9\n");
    git_commit(&root, "merge side work");
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "side-a"]);

    // The resolution's frontier verification fails at the injected fault.
    let failed = atomic_env(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
        &[("ATOMIC_FAIL_SYNTHESIS_DEPS_INDEX", "1")],
    );
    assert!(
        !failed.status.success(),
        "the injected dependency-index fault must fail the resolution: {}",
        atomic_text(&failed)
    );
    let failed_text = atomic_text(&failed);
    assert!(
        failed_text.contains("staged synthesis verification failed"),
        "the failure must come from the staged verification: {failed_text}"
    );

    // The resolution itself never published. The two legs published
    // individually (each verified in its own transaction before the
    // resolution's failure), so exactly two new members exist and none of
    // them is a GitResolution.
    let repo = open_repo(&root);
    let history = repo.effective_history(Some("main")).unwrap();
    assert_eq!(
        history.len(),
        before_changes + 2,
        "the failed resolution must not become a visible view member (legs may publish)"
    );
    for entry in &history {
        let change = repo.load_change(&entry.hash).unwrap();
        assert!(
            !matches!(change.origin(), ChangeOrigin::GitResolution { .. }),
            "no resolution may be visible after the failed verification"
        );
    }
    drop(repo);
    assert_eq!(
        fs::read(root.join("a.txt")).unwrap(),
        b"line1\nresolved\nline9\n".as_slice(),
        "the worktree must be untouched by the failed resolution"
    );

    // A clean retry publishes the verified resolution.
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    let repo = open_repo(&root);
    let merge_change = change_of(&repo, "main", &merge);
    assert!(matches!(
        merge_change.origin(),
        ChangeOrigin::GitResolution { .. }
    ));
    assert!(frontier_closure_of(&repo, &merge_change).len() > 0);
}

/// Review C4 (AC-1/publication safety): a superseded assembly exclusion that
/// survives in an ANCESTOR view of the target must refuse the synthesis
/// BEFORE publication — the staged check must prove the actual
/// post-alignment effective view, not a filtered projection that hides
/// ancestor survivors, and ancestor views are never flattened or deleted to
/// make alignment pass. The refusal is clean (no partial publication, the
/// ancestor's membership is preserved) and the repository stays usable.
#[test]
fn ancestor_surviving_exclusion_refuses_before_publication() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // A Git-origin change lands in the ANCESTOR view `main`.
    fs::write(root.join("s.txt"), b"shared lineage\n").unwrap();
    git_commit(&root, "sibling lineage");
    let sibling = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    let main_state = {
        let repo = open_repo(&root);
        let _ = change_of(&repo, "main", &sibling);
        repo.effective_history(Some("main")).unwrap().len()
    };

    // A draft child view of `main` will be the import target: the rewrite
    // lands on a Git branch of the same name, so the import targets `work`
    // while `main` (its ancestor) keeps the pre-rewrite sibling change.
    atomic_ok(&root, &home, &["view", "create", "work", "--draft", "--parent", "main"]);

    // Rewrite the sibling commit on the `work` branch (amend): the old tip
    // leaves the new history, so a single-parent synthesis into the `work`
    // view excludes the old sibling change — which lives in the ANCESTOR
    // view `main` and can never be removed by target-local alignment.
    git(&root, &["checkout", "-q", "-b", "work"]);
    fs::write(root.join("s.txt"), b"rewritten lineage\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "--amend", "-m", "sibling lineage rewritten"]);
    let rewritten = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(sibling, rewritten);

    let failed = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        !failed.status.success(),
        "the ancestor-surviving exclusion must refuse the synthesis: {}",
        atomic_text(&failed)
    );
    let failed_text = atomic_text(&failed);
    assert!(
        failed_text.contains("survive in ancestor views"),
        "the refusal must name the surviving ancestor exclusion: {failed_text}"
    );

    // No partial publication: the child view's own membership did not change
    // (it stays empty), the ancestor view's membership is preserved
    // untouched (never flattened to pass), and the worktree is untouched.
    let repo = open_repo(&root);
    assert_eq!(
        repo.effective_history(Some("main")).unwrap().len(),
        main_state,
        "the ancestor view's membership must be preserved untouched"
    );
    {
        // `work` stays an empty child view: its OWN log (not the inherited
        // ancestor membership) gains nothing from the refused synthesis.
        use atomic_core::pristine::ViewTxnT;
        let txn = repo.pristine().read_txn().unwrap();
        let view = txn.get_view("work").unwrap().unwrap();
        assert_eq!(
            txn.iter_changes(&view, 0).unwrap().count(),
            0,
            "the refused synthesis must not publish any view-local member"
        );
    }
    drop(repo);
    assert_eq!(
        fs::read(root.join("s.txt")).unwrap(),
        b"rewritten lineage\n",
        "the worktree must be untouched by the failed synthesis"
    );

    // Recovery: the refusal left no partial publication behind — the
    // ancestor view's own boundary still verifies against its Git state.
    // The worktree first returns to the branch the checkpoint anchors
    // before the view switch is accepted.
    git(&root, &["checkout", "-q", "main"]);
    atomic_ok(&root, &home, &["view", "switch", "main", "--force"]);
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);
}

/// Review blocker 1 (empty-tip parents): a merge whose side parent's tip is
/// an empty commit still imports every parent closure, keeps the empty
/// change distinct, and records no sibling causality.
#[test]
fn merge_with_empty_tip_parent_imports_both_closures() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"line1\nline9\n").unwrap();
    git_commit(&root, "base");

    git(&root, &["checkout", "-q", "-b", "side-a"]);
    fs::write(root.join("a.txt"), b"line1\nside-a\nline9\n");
    git_commit(&root, "side a");
    let side_a = git(&root, &["rev-parse", "HEAD"]);

    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["checkout", "-q", "-b", "side-b"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "side b empty"]);
    let side_b = git(&root, &["rev-parse", "HEAD"]);

    git(&root, &["checkout", "-q", "side-a"]);
    let merge_out = Command::new("git")
        .args(["merge", "--no-ff", "side-b", "-m", "merge empty tip"])
        .current_dir(&root)
        .output()
        .expect("git merge");
    assert!(
        merge_out.status.success(),
        "the empty-tip merge must not conflict: {}",
        String::from_utf8_lossy(&merge_out.stderr)
    );
    let merge = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--ff-only", "side-a"]);

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    // The empty leg stays a distinct Change::empty with the EmptyCommit
    // derivation.
    let side_b_change = change_of(&repo, "main", &side_b);
    match side_b_change.origin() {
        ChangeOrigin::GitSynthesized { derivation, .. } => {
            assert_eq!(*derivation, GitDerivation::EmptyCommit);
        }
        other => panic!("the empty tip must stay an empty-commit interpretation: {other:?}"),
    }
    assert!(side_b_change.hunks().is_empty());

    // No sibling causality between the legs.
    assert_no_sibling_dependency(&repo, "main", &side_a, &side_b);

    // The resolution covers both closures.
    let merge_change = change_of(&repo, "main", &merge);
    assert!(matches!(
        merge_change.origin(),
        ChangeOrigin::GitResolution { .. }
    ));
    let verified = frontier_closure_of(&repo, &merge_change);
    assert!(verified.contains(&side_a_change_hash(&repo, side_a)));
    assert!(verified.contains(&side_b_change.hash().unwrap()));
}

fn side_a_change_hash(
    repo: &atomic_repository::Repository,
    sha: String,
) -> atomic_core::types::Hash {
    change_of(repo, "main", &sha).hash().unwrap()
}

/// Review B4/C3 (AC-2/AC-1): the hooks-off Git-native bound-intermediate flow
/// — base -> bound intermediate -> later same-file append, squashed onto base
/// with a locally verified binding covering the intermediate — imports to
/// exactly the Git boundary: import exit 0, the binding-range-match candidate
/// present, and no diverged published boundary or successor. Equality is
/// never weakened to get there.
#[test]
fn hooks_off_bound_range_squash_imports_to_the_git_boundary() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"line one\nbase\n").unwrap();
    let base = git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let intermediate = {
        fs::write(root.join("f.txt"), b"line one\nintermediate\n").unwrap();
        git_commit(&root, "intermediate")
    };
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    // Publish a locally verified binding for the intermediate's state.
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x19u8;
    }
    let key_file = home.join("binding-key.hex");
    fs::write(&key_file, secret.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // The final append and the squash: main is rewritten to one commit whose
    // parent is the base, hooks completely off.
    fs::write(root.join("f.txt"), b"line one\nintermediate\nfinal addition\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "final"]);
    let squash = git_stdin_output(
        &root,
        &["commit-tree", &git(&root, &["rev-parse", "HEAD^{tree}"]), "-p", &base],
        b"Squashed feature (#9)\n\n* intermediate\n* final\n",
    );
    git(&root, &["update-ref", "refs/heads/main", &squash]);
    assert!(
        !root.join(".atomic/bridge/git-events.jsonl").exists(),
        "the fixture must run with hooks off (no journal)"
    );

    // The rewritten range imports cleanly to the Git boundary.
    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the canceled-intermediate range must import: {}",
        atomic_text(&imported)
    );

    // The reviewable binding-range-match link exists, naming the bound
    // intermediate's full OID and the binding id — advisory, never identity.
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("binding-range-match"), "{review}");
    assert!(review.contains(&intermediate), "{review}");

    // The published target's projection is exactly the Git HEAD tree, and a
    // successor after the rewrite imports without a failing boundary.
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);
    fs::write(root.join("g.txt"), b"successor\n").unwrap();
    git_commit(&root, "successor");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);
}

/// Review B4/C3: the canceled-intermediate range — the squash cancels the
/// bound intermediate's f.txt edit and changes another file — imports
/// cleanly AND still surfaces the bound intermediate as a known-range
/// candidate (the canceled contribution leaves no surviving path or blob
/// intersection, so the candidate comes from the known range alone).
#[test]
fn hooks_off_canceled_intermediate_range_imports_cleanly() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), ".atomic/\n").unwrap();
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    fs::write(root.join("g.txt"), b"old\n").unwrap();
    let base = git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let intermediate = {
        fs::write(root.join("f.txt"), b"intermediate\n").unwrap();
        git_commit(&root, "intermediate")
    };
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let mut secret = [0u8; 32];
    for byte in secret.iter_mut() {
        *byte = 0x19;
    }
    let key_file = home.join("binding-key.hex");
    fs::write(&key_file, secret.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // Revert the intermediate's edit and change the other file, then squash
    // both onto base with a recognized squash message, hooks disabled.
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    fs::write(root.join("g.txt"), b"new\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "revert intermediate and edit another file"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let squash = git_stdin_output(
        &root,
        &["commit-tree", &tree, "-p", &base],
        b"Squashed feature (#99)\n\n* intermediate\n* revert and edit another file\n",
    );
    git(&root, &["update-ref", "refs/heads/main", &squash]);

    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the canceled-intermediate range must import: {}",
        atomic_text(&imported)
    );
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);

    // Review C3: the canceled intermediate is still a known bound
    // intermediate of the rewritten range — the candidate exists without any
    // surviving path or blob intersection.
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("binding-range-match"), "{review}");
    assert!(review.contains(&intermediate), "{review}");

    // The canceled intermediate's contribution is gone from the view.
    let content = {
        let repo = open_repo(&root);
        repo.get_file_content_on_view("f.txt", "main")
            .unwrap()
            .unwrap_or_default()
    };
    assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"base\n");
    let _ = content;
    let _ = base;
}

/// Review C3 (AC-2): the complete locally known range — several bound
/// intermediates whose contributions were later canceled (net-zero) or
/// renamed away, with the squash touching a disjoint file — produces a
/// reviewable binding-range-match candidate naming EVERY bound intermediate,
/// without requiring surviving blobs, surviving paths, or a recognized
/// message format. The candidates are explicitly advisory: range membership
/// is uncertain, never identity.
#[test]
fn known_range_candidates_cover_canceled_and_multiple_intermediates() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".gitignore"), ".atomic/\n").unwrap();
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    fs::write(root.join("g.txt"), b"old\n").unwrap();
    let base = git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Two bound intermediates: the first edits f.txt, the second renames
    // that edit away entirely (net-zero contribution to the final tree).
    // Each intermediate gets its own locally verified binding, so the
    // rewritten range has several known bound intermediates.
    let mut secret = [0u8; 32];
    for byte in secret.iter_mut() {
        *byte = 0x19;
    }
    let key_file = home.join("binding-key.hex");
    fs::write(&key_file, secret.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap();
    let first = {
        fs::write(root.join("f.txt"), b"first\n").unwrap();
        git_commit(&root, "first intermediate")
    };
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );
    let second = {
        fs::write(root.join("f.txt"), b"base\nsecond\n").unwrap();
        git_commit(&root, "second intermediate")
    };
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // The squash restores f.txt to the base bytes (canceling BOTH bound
    // intermediates' contributions entirely), changes the disjoint g.txt,
    // and uses an unrecognized message: no surviving path, blob, or message
    // format connects the intermediates to the squash.
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    fs::write(root.join("g.txt"), b"new\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "cancel both intermediates, edit another file"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let squash = git_stdin_output(
        &root,
        &["commit-tree", &tree, "-p", &base],
        b"unrecognized rewrite message\n",
    );
    git(&root, &["update-ref", "refs/heads/main", &squash]);
    assert!(
        !root.join(".atomic/bridge/git-events.jsonl").exists(),
        "the fixture must run with hooks off (no journal)"
    );

    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the canceled multi-intermediate range must import: {}",
        atomic_text(&imported)
    );
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-") && tag.name.ends_with(&squash))
        .expect("the known-range candidates must create a reviewable link");
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("structured evidence entries");
    let range_named: Vec<String> = entries
        .iter()
        .filter(|entry| entry.get("class").and_then(|v| v.as_str()) == Some("binding-range-match"))
        .flat_map(|entry| {
            entry
                .get("predecessor_oids")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
        })
        .collect();
    assert!(
        range_named.contains(&first),
        "the canceled first intermediate must be named as a known-range candidate: {entries:?}"
    );
    assert!(
        range_named.contains(&second),
        "the canceled second intermediate must be named as a known-range candidate: {entries:?}"
    );
    for entry in &entries {
        assert_eq!(
            entry.get("authenticated").and_then(|v| v.as_bool()),
            Some(false),
            "range candidates are explicitly uncertain, never authenticated identity: {entries:?}"
        );
    }
    drop(repo);
}

/// Review C2 (AC-3): a post-rewrite pair submitted directly to the hook —
/// whether it describes a REAL amend or a forged pair — is captured with NO
/// operation anchor, because the hook runs outside any active captured
/// operation. Git ancestry shape and object existence never prove a captured
/// operation, so both records read as unauthenticated advisory evidence that
/// never names predecessors. Only an anchored capture written during an
/// active captured operation with a verified receipt is operation linkage
/// (tested separately).
#[test]
fn hook_captured_pairs_without_an_active_operation_stay_unauthenticated() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("doc.md"), b"original\n").unwrap();
    git_commit(&root, "original work");
    let original = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // An ordinary commit that is later amended: the amend rewrites it.
    fs::write(root.join("doc.md"), b"original\nvictim\n").unwrap();
    git_commit(&root, "victim");
    let victim = git(&root, &["rev-parse", "HEAD"]);
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    fs::write(root.join("doc.md"), b"original\namended\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "--amend", "-m", "amended"]);
    let amended = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(victim, amended);

    // Journal the ACTUAL rewritten pair (the amend removed the victim tip
    // from the new history) — but submitted directly AFTER the amend, with
    // no active captured operation. Even this genuinely rewritten pair must
    // read as unauthenticated advisory evidence: the direct hook submission
    // is not an event during an active captured operation (review C2).
    atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-post-rewrite"],
        format!("{victim} {amended}\n").as_bytes(),
    );
    // Journal a fully known forged pair: both commits are locally
    // interpreted and durably captured, no rewrite happened. It must never
    // read as linkage either.
    atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-post-rewrite"],
        format!("{original} {amended}\n").as_bytes(),
    );

    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the amended boundary must import to the Git tree: {}",
        atomic_text(&imported)
    );

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("main").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-") && tag.name.ends_with(&amended))
        .expect("the captured pair must surface as explicit advisory evidence");
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("structured evidence entries");
    let classes: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry.get("class").and_then(|v| v.as_str()))
        .collect();
    assert!(
        classes.contains(&"unauthenticated-event"),
        "the unanchored direct-hook submissions stay unauthenticated: {entries:?}"
    );
    assert!(
        !classes.contains(&"post-rewrite-event"),
        "no direct hook submission may reach the operation-linkage tier without an \
         anchored capture: {entries:?}"
    );
    for entry in &entries {
        assert!(
            entry
                .get("predecessor_oids")
                .and_then(|v| v.as_array())
                .is_none_or(|v| v.is_empty()),
            "the unauthenticated tier never names predecessors: {entries:?}"
        );
        assert_eq!(
            entry.get("authenticated").and_then(|v| v.as_bool()),
            Some(false),
            "{entries:?}"
        );
    }
    drop(repo);
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "main"]);
    assert!(review.contains("unauthenticated-evidence-only"), "{review}");
}

/// Review C2 (AC-3): two ordinary sibling commits satisfy every
/// ancestry-shape predicate — the old tip is not an ancestor of the new tip
/// — yet no rewrite happened and no captured operation exists. Direct hook
/// stdin for such a pair must read unauthenticated-event, never
/// operation-linkage-only.
#[test]
fn ordinary_sibling_pairs_submitted_directly_stay_unauthenticated() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    git_commit(&root, "base");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Two ordinary siblings of base: left is imported first; right is a
    // second child of the same base (imported into its own branch view).
    let base = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["checkout", "-q", "-b", "left"]);
    fs::write(root.join("left.txt"), b"left\n").unwrap();
    git_commit(&root, "left sibling");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    git(&root, &["checkout", "-q", "-b", "right", &base]);
    fs::write(root.join("right.txt"), b"right\n").unwrap();
    git_commit(&root, "right sibling");
    let right = git(&root, &["rev-parse", "HEAD"]);

    // No hook events, no rewrite operation: the pair is submitted directly
    // to the advisory dispatcher, exactly like a forgery would be.
    atomic_stdin(
        &root,
        &home,
        &["git", "bridge", "hook-post-rewrite"],
        format!("{base} {right}\n").as_bytes(),
    );

    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the ordinary sibling must import: {}",
        atomic_text(&imported)
    );

    let repo = open_repo(&root);
    let tags = repo.list_tags_for_view("right").unwrap();
    let candidate = tags
        .iter()
        .find(|tag| tag.name.starts_with("rewrite-candidate-") && tag.name.ends_with(&right))
        .expect("the plausible sibling pair must surface as explicit advisory evidence");
    let entries = candidate
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("evidence"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("structured evidence entries");
    assert_eq!(entries.len(), 1, "only the sibling-pair event: {entries:?}");
    assert_eq!(
        entries[0].get("class").and_then(|v| v.as_str()),
        Some("unauthenticated-event"),
        "an ancestry-shaped sibling pair without an anchored capture is never operation \
         linkage: {entries:?}"
    );
    assert!(
        entries[0]
            .get("predecessor_oids")
            .and_then(|v| v.as_array())
            .is_none_or(|v| v.is_empty()),
        "the unauthenticated tier never names the sibling as a predecessor: {entries:?}"
    );
    drop(repo);
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "right"]);
    assert!(review.contains("unauthenticated-evidence-only"), "{review}");
    assert!(
        !review.contains("post-rewrite-event"),
        "no operation-linkage wording may appear for the sibling forgery: {review}"
    );
}

/// Review B1 (AC-1): an ordinary import of dozens of one-line files must not
/// overflow the per-change placeholder namespace. The pre-fix implementation
/// doubled the running branch base per entry and panicked on the 34th
/// one-line file; the namespace now advances by each entry's own span under
/// checked arithmetic in BOTH debug and release builds.
#[test]
fn many_one_line_files_import_without_placeholder_overflow() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    let files: Vec<(String, String)> = (0..34)
        .map(|index| (format!("file-{index}.txt"), format!("line {index}\n")))
        .collect();
    for (name, bytes) in &files {
        fs::write(root.join(name), bytes).unwrap();
    }
    git_commit(&root, "34 files");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    // The semantic layer renders every file and the status is clean: branch
    // and leaf placeholders stay unique per entry (referential integrity).
    atomic_ok(&root, &home, &["status"]);
    atomic_ok(&root, &home, &["git", "bridge", "verify"]);
    let repo = open_repo(&root);
    for (name, bytes) in &files {
        assert_eq!(
            repo.get_file_content(name).unwrap().as_deref(),
            Some(bytes.as_bytes()),
            "{name} must render byte-identical through the CRDT layer"
        );
    }
}

/// Review D4: a ROOT-SPANNING squash — both commits collapsed into one
/// parentless commit with an unrecognized message, hooks off — must still
/// surface a locally published binding for the ROOT as an advisory
/// candidate. The historical discovery required a first parent (for the
/// descent probe) or tree equality, so the root binding produced zero
/// candidates and the known-range coverage structurally excluded roots.
#[test]
fn root_spanning_squash_names_the_locally_published_root_binding() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"root contribution\n").unwrap();
    let old_root = git_commit(&root, "root contribution");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Publish a locally verified binding for the root commit's state.
    let mut secret = [0u8; 32];
    for byte in secret.iter_mut() {
        *byte = 0x19;
    }
    let key_file = home.join("binding-key.hex");
    fs::write(
        &key_file,
        secret.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    )
    .unwrap();
    atomic_ok(
        &root,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "publish",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );

    // The second commit replaces the root's content; commit-tree squashes
    // BOTH into a parentless commit with an unrecognized message. The old
    // tip stays locally reachable on main; hooks are off (no journal).
    fs::write(root.join("f.txt"), b"final replacement\n").unwrap();
    git_commit(&root, "replace contribution");
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let squash = git_stdin_output(&root, &["commit-tree", &tree], b"Consolidate root range\n");
    git(&root, &["update-ref", "refs/heads/squashed", &squash]);
    git(&root, &["checkout", "-q", "squashed"]);
    assert!(
        !root.join(".atomic/bridge/git-events.jsonl").exists(),
        "the fixture must run with hooks off (no journal)"
    );

    // The root-spanning squash imports cleanly into the squashed boundary.
    let imported = atomic(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    assert!(
        imported.status.success(),
        "the root-spanning squash must import: {}",
        atomic_text(&imported)
    );

    // The reviewable root-range candidate exists, naming the locally
    // published root binding's commit — advisory and explicitly uncertain,
    // never identity (review D4).
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "squashed"]);
    assert!(
        review.contains("binding-root-range-match"),
        "the root-spanning range candidate must be discoverable: {review}"
    );
    assert!(review.contains(&old_root), "{review}");
    assert!(
        review.contains("maximally uncertain"),
        "the root-range hint must preserve its uncertainty: {review}"
    );
}

/// CB-9B F1 — requested-closure semantic acceptance and the sibling
/// `git import --all` defect.
///
/// Two sibling branches share a base and edit disjoint files. `git import
/// --all` must complete (before the fix the branch whose base was already
/// imported into another view re-synthesized the base commit into a second
/// incarnation with new inodes, and the staged projection refused with an
/// unresolved `f.txt`/`g.txt` name conflict). After the reload, the requested
/// closure's semantic view must return that closure's own bytes: the view
/// that edited `f` renders its `f` and the base `g`; the view that edited `g`
/// renders its `g` and the base `f` — never the sibling's concurrent bytes.
fn sibling_branches_sharing_a_base_import_all_both_orders(edit_f: &str, edit_g: &str) {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"base f\n").unwrap();
    fs::write(root.join("g.txt"), b"base g\n").unwrap();
    let base = git_commit(&root, "base");

    git(&root, &["checkout", "-q", "-b", edit_f, &base]);
    let f_bytes = format!("{edit_f} f\n");
    fs::write(root.join("f.txt"), f_bytes.as_bytes()).unwrap();
    git_commit(&root, edit_f);

    git(&root, &["checkout", "-q", "-b", edit_g, &base]);
    let g_bytes = format!("{edit_g} g\n");
    fs::write(root.join("g.txt"), g_bytes.as_bytes()).unwrap();
    git_commit(&root, edit_g);

    // Before the fix this import failed closed on the second branch with an
    // unresolved name conflict; the sibling-shared-base corpus was unrunnable.
    atomic_ok(&root, &home, &["git", "import", "--all", "--no-vault"]);

    // Reload from disk (a fresh repository handle) before reading semantics,
    // so the assertion covers the persisted graph, not an in-memory cache.
    let repo = open_repo(&root);
    assert_eq!(
        repo.get_file_content_via_crdt_on_view("f.txt", edit_f).unwrap(),
        Some(f_bytes.as_bytes().to_vec()),
        "the f-editing closure renders its own f bytes"
    );
    assert_eq!(
        repo.get_file_content_via_crdt_on_view("g.txt", edit_f).unwrap(),
        Some(b"base g\n".to_vec()),
        "the f-editing closure renders the base g bytes, not the sibling's"
    );
    assert_eq!(
        repo.get_file_content_via_crdt_on_view("g.txt", edit_g).unwrap(),
        Some(g_bytes.as_bytes().to_vec()),
        "the g-editing closure renders its own g bytes"
    );
    assert_eq!(
        repo.get_file_content_via_crdt_on_view("f.txt", edit_g).unwrap(),
        Some(b"base f\n".to_vec()),
        "the g-editing closure renders the base f bytes, not the sibling's"
    );
    // The canonical CRDT compatibility entry point agrees (current view is
    // whatever `--all` materialized last; the explicit-view reads above are
    // the requested-closure contract).
    assert_eq!(
        repo.get_file_content_via_crdt("f.txt").unwrap(),
        repo.get_file_content("f.txt").unwrap()
    );
}

#[test]
fn sibling_branches_sharing_a_base_import_all_orders() {
    // `alpha` sorts before `main`, so its edit imports before the base-only
    // view (the branch-first order); `zeta` sorts after, so its edit imports
    // after base was already interpreted (the failing order). The reversed
    // labels exercise both sibling label orders.
    sibling_branches_sharing_a_base_import_all_both_orders("alpha", "zeta");
    sibling_branches_sharing_a_base_import_all_both_orders("zeta", "alpha");
}

/// CB-9B F2 — an ordinary ref movement is not rewrite authority, and a
/// genuine rewrite execution still authenticates its captured post-rewrite
/// event. This exercises the public API boundary end to end: prepare,
/// capture, anchor, verify, import, review — negative (ordinary ref-only)
/// and positive (rewrite) controls against the same local object database.
#[test]
fn ordinary_ref_movement_is_not_rewrite_authority_e2e() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f.txt"), b"base\n").unwrap();
    let base = git_commit(&root, "base");
    fs::write(root.join("f.txt"), b"ordinary left\n").unwrap();
    let old = git_commit(&root, "ordinary left");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    git(&root, &["checkout", "-q", "-b", "right", &base]);
    fs::write(root.join("f.txt"), b"ordinary right\n").unwrap();
    let next = git_commit(&root, "ordinary right");

    let journal = root.join(".atomic/bridge/git-events.jsonl");

    // ── Negative: the ordinary ref-only API mints no rewrite context ──────
    let repo = open_repo(&root);
    let wc = repo.require_working_copy_id().unwrap();
    let ordinary = repo
        .prepare_bridge_git_ref_write(
            wc,
            "refs/heads/ref-only-review",
            Some(GitRefTarget::Direct(tagged(&old))),
            GitRefTarget::Direct(tagged(&next)),
            atomic_core::Hash::of(b"ordinary ref movement, no rewrite"),
        )
        .expect("journal the ordinary ref movement");
    assert_eq!(
        repo.bridge_ref_capture_token(ordinary.operation_id).unwrap(),
        None,
        "an ordinary ref movement must not mint rewrite authority"
    );
    let forged = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "record_type": "post-rewrite",
        "event_id": "forged-ref-only-movement",
        "recorded_at": "2026-09-14T00:00:00+00:00",
        "advisory": true,
        "rewritten": [{"old_oid": old, "new_oid": next}],
        "capture_token": "11".repeat(32),
    }))
    .unwrap();
    let error = repo
        .capture_bridge_event_anchored(&forged, ordinary.operation_id)
        .expect_err("a ref-only operation must not accept a rewrite anchor");
    assert!(
        error.to_string().contains("no minted capture context"),
        "the refusal must name the missing rewrite context: {error}"
    );
    repo.record_bridge_git_ref_receipt(&ordinary, Some(GitRefTarget::Direct(tagged(&next))))
        .unwrap();
    let ordinary_id = ordinary.operation_id;
    repo.finalize_bridge_git_write(ordinary, Some(GitRefTarget::Direct(tagged(&next))))
        .unwrap();
    assert!(repo.operation_has_verified_receipt(ordinary_id).unwrap());

    // ── Positive control: a genuine rewrite execution authenticates ───────
    let rewrite = repo
        .prepare_bridge_git_rewrite(
            wc,
            "refs/heads/rewrite-review",
            Some(GitRefTarget::Direct(tagged(&old))),
            GitRefTarget::Direct(tagged(&next)),
            atomic_core::Hash::of(b"rewrite execution evidence"),
        )
        .expect("journal the rewrite execution");
    let token = repo
        .bridge_ref_capture_token(rewrite.operation_id)
        .expect("read capture context")
        .expect("a rewrite execution mints a capture token");
    let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
    // Journal the captured rewrite pair through the real hook wire format,
    // exporting the operation's minted capture token for the hook's duration.
    let mut child = Command::new(ATOMIC_BIN)
        .args(["git", "bridge", "hook-post-rewrite"])
        .current_dir(&root)
        .env("HOME", &home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_BRIDGE_CAPTURE_TOKEN", &token_hex)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hook-post-rewrite");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{old} {next}\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    let event_bytes: Vec<u8> = fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .last()
        .map(|line| line.as_bytes().to_vec())
        .expect("the rewrite capture is journaled");
    repo.capture_bridge_event_anchored(&event_bytes, rewrite.operation_id)
        .expect("the rewrite execution accepts its captured event");
    repo.record_bridge_git_ref_receipt(&rewrite, Some(GitRefTarget::Direct(tagged(&next))))
        .unwrap();
    let rewrite_id = rewrite.operation_id;
    repo.finalize_bridge_git_write(rewrite, Some(GitRefTarget::Direct(tagged(&next))))
        .unwrap();
    assert!(repo.operation_has_verified_receipt(rewrite_id).unwrap());
    assert!(
        repo.bridge_anchor_binds_operation(&event_bytes, rewrite_id).unwrap(),
        "the rewrite-execution capture binds its operation"
    );
    drop(repo);

    // The import reviews the right branch; the ordinary movement never reads
    // as rewrite authority, while the genuine rewrite execution does.
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    let review = atomic_ok(&root, &home, &["git", "bridge", "review", "--view", "right"]);
    assert!(
        review.contains("class=post-rewrite-event"),
        "the genuine rewrite execution must authenticate its captured event: {review}"
    );
    assert!(review.contains(&old), "the authenticated event names its predecessor: {review}");
    assert!(
        !review.contains("forged-ref-only-movement"),
        "the ordinary ref movement is never rewrite authority: {review}"
    );
}

/// CB-9B ::15 AC-3 / review ::26 R1: TRACKED Git paths are never stripped by
/// import ignore rules (RFC tree-fidelity table and the colocated status
/// contract). A Rust workspace whose Git tree tracks `Cargo.lock` records
/// EVERY tracked path — lockfile included — with exact bytes, and the
/// prospective gate compares the projection against the Git tree minus ONLY
/// the versioned bridge-private exclusions.
///
/// Failing-before (review ::26 R1 evidence): the import silently omitted
/// `Cargo.lock` from atomic content and the gate was aligned to that loss.
/// Passing-after: the lockfile is tracked content with exact bytes after
/// reload and after worktree deletion, and a lockfile-only modification
/// imports with the exact new bytes.
#[test]
fn tracked_cargo_lock_imports_with_exact_bytes() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);

    let manifest =
        b"[package]\nname = \"lockfile-import-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    let lib = b"pub fn recorded() -> u32 { 7 * 6 }\n";
    let lock_v1 =
        b"# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = \"lockfile-import-fixture\"\nversion = \"0.1.0\"\n";
    fs::write(root.join("Cargo.toml"), manifest).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), lib).unwrap();
    fs::write(root.join("Cargo.lock"), lock_v1).unwrap();
    git_commit(&root, "rust workspace with a tracked lockfile");

    let report = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(
        !report.contains("import-ignore policy excludes"),
        "no tracked path is excluded any more: {report}"
    );

    let repo = open_repo(&root);
    for (path, wanted) in [
        ("Cargo.toml", &manifest[..]),
        ("src/lib.rs", &lib[..]),
        ("Cargo.lock", &lock_v1[..]),
    ] {
        let bytes = repo
            .get_file_content_on_view(path, "main")
            .unwrap_or_else(|e| panic!("{path} readable: {e}"))
            .unwrap_or_else(|| panic!("{path} must be tracked content"));
        assert_eq!(bytes, wanted, "{path} exact bytes");
    }
    drop(repo);

    // An incremental import preserving every tracked path (the new file is
    // non-generated: the lockfile-MODIFY path has a discovered pre-existing
    // assembly defect, recorded in the tracker as an open ::26 finding).
    fs::write(root.join("src/second.rs"), b"pub fn second() -> u32 { 2 }\n").unwrap();
    git_commit(&root, "second file");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    for (path, wanted) in [
        ("Cargo.toml", &manifest[..]),
        ("src/lib.rs", &lib[..]),
        ("Cargo.lock", &lock_v1[..]),
        ("src/second.rs", &b"pub fn second() -> u32 { 2 }\n"[..]),
    ] {
        let bytes = repo
            .get_file_content_on_view(path, "main")
            .expect("path readable after incremental import")
            .unwrap_or_else(|| panic!("{path} tracked after incremental import"));
        assert_eq!(bytes, wanted, "{path} exact bytes after incremental import");
    }
    drop(repo);

    // Delete the physical worktree: only the recorded graph remains.
    for path in ["Cargo.toml", "Cargo.lock", "src"] {
        let target = root.join(path);
        if target.is_dir() {
            fs::remove_dir_all(&target).unwrap();
        } else {
            fs::remove_file(&target).unwrap();
        }
    }

    let repo = open_repo(&root);
    for (path, wanted) in [
        ("Cargo.toml", &manifest[..]),
        ("src/lib.rs", &lib[..]),
        ("Cargo.lock", &lock_v1[..]),
        ("src/second.rs", &b"pub fn second() -> u32 { 2 }\n"[..]),
    ] {
        let bytes = repo
            .get_file_content_on_view(path, "main")
            .expect("graph-only read succeeds with the worktree deleted")
            .unwrap_or_else(|| panic!("{path} tracked after worktree deletion"));
        assert_eq!(bytes, wanted, "{path} graph-only exact bytes");
    }
}

/// Review ::26 R1: tracked entries under an ignored DIRECTORY name survive
/// import (ignore rules never exclude tracked entries, including children of
/// `node_modules/`-style directories).
#[test]
fn tracked_children_of_ignored_directory_are_preserved() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    let child = b"console.log(\"tracked vendored source\");\n";
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), child).unwrap();
    git_commit(&root, "tracked children under an ignored directory name");

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let bytes = repo
        .get_file_content_on_view("node_modules/pkg/index.js", "main")
        .expect("tracked child readable")
        .expect("tracked child of an ignored directory name is preserved");
    assert_eq!(bytes, child.to_vec());
}

/// Review ::26 R1: the parent-tree seed path (retry / incremental import in
/// a fresh process, parent manifest not cached) preserves every tracked path
/// — the historical defect seeded the parent Git tree with import-ignore
/// omissions and refused the fold.
#[test]
fn incremental_import_after_a_fresh_process_preserves_tracked_paths() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"incremental-lock-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), b"pub fn first() -> u32 { 1 }\n").unwrap();
    fs::write(
        root.join("Cargo.lock"),
        b"# This file is automatically @generated by Cargo.\nversion = 4\n",
    )
    .unwrap();
    git_commit(&root, "base with a tracked lockfile");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    fs::write(root.join("src/second.rs"), b"pub fn second() -> u32 { 2 }\n").unwrap();
    git_commit(&root, "second file");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    let repo = open_repo(&root);
    let second = repo
        .get_file_content_on_view("src/second.rs", "main")
        .expect("second.rs readable")
        .expect("second.rs tracked");
    assert_eq!(second, b"pub fn second() -> u32 { 2 }\n".to_vec());
    let lock = repo
        .get_file_content_on_view("Cargo.lock", "main")
        .expect("lockfile reads cleanly")
        .expect("the seed path preserves the tracked lockfile");
    assert_eq!(
        lock,
        b"# This file is automatically @generated by Cargo.\nversion = 4\n".to_vec()
    );
}

/// Review ::26 R3: an all-excluded root folds to the CANONICAL empty tree of
/// the object format (sha1 `4b825dc6...`), never a zero sentinel. A root
/// commit whose only tracked entries are bridge-private (`.atomicignore`,
/// `.vault/`) imports: the projection of an empty state is the canonical
/// empty tree, and the bridge-private files stay tracked-alive in the view.
#[test]
fn all_private_root_imports_with_the_canonical_empty_tree() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join(".atomicignore"), b"target/\n").unwrap();
    fs::create_dir_all(root.join(".vault")).unwrap();
    fs::write(root.join(".vault/state.md"), b"private state\n").unwrap();
    git_commit(&root, "only bridge-private entries");
    let report = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(
        !report.contains("prospective import tree mismatch"),
        "the all-private root must fold to the canonical empty tree: {report}"
    );
    let repo = open_repo(&root);
    let vault = repo
        .get_file_content_on_view(".vault/state.md", "main")
        .expect("the vault path reads cleanly")
        .expect("the bridge-private file is tracked alive");
    assert_eq!(vault, b"private state\n".to_vec());
}

/// Review ::26 R3: deleting the LAST tracked file folds to the canonical
/// empty tree and imports; a transition back to non-empty imports cleanly.
#[test]
fn delete_last_file_folds_to_the_canonical_empty_tree_and_back() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("only.txt"), b"the only file\n").unwrap();
    git_commit(&root, "one file");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    // Delete the last tracked file: the commit tree is the canonical empty
    // tree; the projected (all-excluded) state folds to the same identity.
    fs::remove_file(root.join("only.txt")).unwrap();
    git_commit(&root, "delete the last file");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    let repo = open_repo(&root);
    assert_eq!(
        repo.get_file_content_on_view("only.txt", "main").unwrap(),
        None,
        "the deleted file is absent on the view"
    );
    drop(repo);

    // Transition back to non-empty.
    fs::write(root.join("back.txt"), b"back again\n").unwrap();
    git_commit(&root, "back to nonempty");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);
    let repo = open_repo(&root);
    assert_eq!(
        repo.get_file_content_on_view("back.txt", "main").unwrap(),
        Some(b"back again\n".to_vec()),
        "the view transitions back to non-empty cleanly"
    );
}

/// Review ::26 R1 follow-up (2026-09-16): a lockfile-only modification
/// imports with the EXACT new bytes — the targeted per-line surgery replaces
/// only the modified lines' vertices and the graph-only read (worktree
/// deleted) recovers the exact committed content.
///
/// Failing-before: the opaque-classified Replace hunks were routed to
/// whole-file replacement carrying ONLY the hunk's sliced line — a
/// two-line lockfile modification rendered 12 of 67 bytes (real data
/// loss). Passing-after: the render is byte-exact, and the CRDT rows show
/// the untouched lines keeping their base vertices.
#[test]
fn lockfile_only_modification_renders_exact_bytes() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"lf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        b"# generated\nversion = 4\n\n[[package]]\nname = \"lf\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git_commit(&root, "base with a tracked lockfile");
    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let lock_v2 = b"# generated\nversion = 5\n\n[[package]]\nname = \"lf\"\nversion = \"0.2.0\"\n";
    fs::write(root.join("Cargo.lock"), lock_v2).unwrap();
    git_commit(&root, "lockfile only bump");
    atomic_ok(&root, &home, &["git", "import", "--incremental", "--no-vault"]);

    // The recorded graph holds the exact new bytes.
    let repo = open_repo(&root);
    let bytes = repo
        .get_file_content_on_view("Cargo.lock", "main")
        .expect("lockfile readable after the modify import")
        .expect("lockfile tracked");
    assert_eq!(bytes, lock_v2.to_vec(), "lockfile-only modify exact bytes");

    // Graph-only oracle: delete the worktree, read from the graph alone.
    drop(repo);
    fs::remove_file(root.join("Cargo.lock")).unwrap();
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    let repo = open_repo(&root);
    let graph_only = repo
        .get_file_content_on_view("Cargo.lock", "main")
        .expect("graph-only read succeeds with the worktree deleted")
        .expect("lockfile tracked after worktree deletion");
    assert_eq!(graph_only, lock_v2.to_vec(), "graph-only exact bytes");
}

/// Review ::26 R1: `.vault/` files are versioned bridge-private policy
/// (`ExclusionPolicy::BridgePrivate`) — tracked-alive in the view, absent
/// from project manifests. The staged full-tree check must exempt the whole
/// bridge-private set (`.atomic*` AND `.vault/`), which it does via
/// `bridge_private_path`.
///
/// Failing-before: `staged path '.vault/seed.md' is present but the commit
/// tree does not hold it`. Passing-after: the import succeeds and the
/// recorded graph tracks the `.vault/` file with its exact committed bytes.
#[test]
fn tracked_vault_files_import_as_bridge_private_state() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("file.md"), b"visible\n").unwrap();
    fs::create_dir_all(root.join(".vault/skills")).unwrap();
    fs::write(root.join(".vault/seed.md"), b"vault private seed\n").unwrap();
    git_commit(&root, "repo tracking a vault file");

    atomic_ok(&root, &home, &["git", "import", "--no-vault"]);

    let repo = open_repo(&root);
    let visible = repo
        .get_file_content_on_view("file.md", "main")
        .expect("file.md readable")
        .expect("file.md tracked");
    assert_eq!(visible, b"visible\n".to_vec());
    let vault = repo
        .get_file_content_on_view(".vault/seed.md", "main")
        .expect("the vault path reads cleanly")
        .expect("the vault file is tracked alive in the view");
    assert_eq!(vault, b"vault private seed\n".to_vec());
}
