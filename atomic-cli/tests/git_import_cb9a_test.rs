//! CB-9A end-to-end: `atomic git import` synthesizes foreign Git history
//! through normal recorded-file assembly with hashed Git origin, tagged
//! commit/tree OIDs, graph-context dependencies, bridge sequencing, and an
//! explicit `SynthesizeGit` operation — and repeated incremental imports
//! stay idempotent.
//!
//! The importer runs as the real binary (like the CB-4B/6A/6B suites); the
//! assertions reopen the pristine in-process so the serialized origins are
//! inspected exactly as another process would see them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use atomic_core::change::ChangeOrigin;
use atomic_core::operation::{GitHashAlgorithm, GitObjectId, OperationKind, OperationScope};
use git2::Repository as GitRepository;

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

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-9A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb9a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-9A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb9a@example.com")
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

/// Fixture: a Git repository with a root commit (nested paths) and a
/// single-parent follow-up (modification), plus the SHAs and trees of both.
struct Fixture {
    root: PathBuf,
    home: PathBuf,
    head: String,
    head_tree: String,
    parent: String,
    parent_tree: String,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    let git_dir = root.join(".git");
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src/domain")).unwrap();
    fs::write(root.join("src/domain/model.rs"), b"pub struct Model;\n").unwrap();
    fs::write(root.join("README.md"), b"readme\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial model"]);
    let parent = git(&root, &["rev-parse", "HEAD"]);
    let parent_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);

    fs::write(
        root.join("src/domain/model.rs"),
        b"pub struct Model;\npub enum Kind;\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("docs/deep")).unwrap();
    fs::write(root.join("docs/deep/guide.md"), b"# guide\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "extend model and add docs"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let head_tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let _ = git_dir;
    Fixture {
        root,
        home,
        head,
        head_tree,
        parent,
        parent_tree,
    }
}

fn change_by_sha(
    repo: &atomic_repository::Repository,
    git_sha: &str,
) -> atomic_core::change::Change {
    // The importer records the Git SHA in the acceleration index; the
    // authoritative lookup here walks the view history and matches the
    // unhashed provenance, which is exactly what a foreign reader sees.
    let entries = repo.effective_history(Some("main")).unwrap();
    let mut found = None;
    for entry in entries {
        let change = repo.load_change(&entry.hash).unwrap();
        let sha = change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .and_then(|git| git.get("sha"))
            .and_then(|sha| sha.as_str());
        if sha == Some(git_sha) {
            found = Some(change);
            break;
        }
    }
    found.unwrap_or_else(|| panic!("no imported change for git commit {git_sha}"))
}

/// Lowercase-hex decode (test helper; the hex crate is not a dependency).
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

#[test]
fn git_import_synthesizes_normal_assembly_with_hashed_origin_and_stays_idempotent() {
    let fixture = fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    atomic_ok(root, home, &["git", "import", "--all", "--no-vault"]);

    let repo = atomic_repository::Repository::open(root).unwrap();
    let root_change = change_by_sha(&repo, &fixture.parent);
    let head_change = change_by_sha(&repo, &fixture.head);

    // Hashed origin: tagged commit OID, complete ordered parents, derivation.
    match root_change.origin() {
        ChangeOrigin::GitSynthesized {
            commit,
            parents,
            derivation,
        } => {
            assert_eq!(*commit, tagged(&fixture.parent));
            assert!(parents.is_empty(), "root commit carries no parents");
            assert_eq!(*derivation, atomic_core::change::GitDerivation::Root);
        }
        other => panic!("root import origin must be GitSynthesized, found {other:?}"),
    }
    match head_change.origin() {
        ChangeOrigin::GitSynthesized {
            commit,
            parents,
            derivation,
        } => {
            assert_eq!(*commit, tagged(&fixture.head));
            assert_eq!(parents, &[tagged(&fixture.parent)]);
            assert_eq!(*derivation, atomic_core::change::GitDerivation::FirstParent);
        }
        other => panic!("head import origin must be GitSynthesized, found {other:?}"),
    }

    // Dependencies come only from graph context: the follow-up change
    // depends on the root synthesis it was assembled on — never on a Git
    // parent OID.
    assert!(root_change.dependencies().is_empty());
    assert_eq!(head_change.dependencies(), &[root_change.hash().unwrap()]);
    assert!(!head_change
        .dependencies()
        .iter()
        .any(|dep| dep.as_bytes() == tagged(&fixture.parent).as_bytes()));

    // The semantic layer (Trunk → Branch → Leaf) is present.
    assert!(root_change.has_file_ops());
    assert!(head_change.has_file_ops());

    // Tagged commit/tree OIDs and the bridge sequencing label travel in the
    // unhashed provenance.
    for (change, sha, tree) in [
        (&root_change, &fixture.parent, &fixture.parent_tree),
        (&head_change, &fixture.head, &fixture.head_tree),
    ] {
        let git_meta = change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .expect("synthesized git provenance")
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            git_meta.get("origin").and_then(|v| v.as_str()),
            Some("git_synthesized")
        );
        assert_eq!(
            git_meta.get("commit").and_then(|v| v.as_str()),
            Some(sha.as_str())
        );
        assert_eq!(
            git_meta.get("commit_algorithm").and_then(|v| v.as_str()),
            Some("sha1")
        );
        assert_eq!(
            git_meta.get("tree").and_then(|v| v.as_str()),
            Some(tree.as_str())
        );
        assert_eq!(
            git_meta.get("tree_algorithm").and_then(|v| v.as_str()),
            Some("sha1")
        );
        assert_eq!(
            git_meta.get("sequencing").and_then(|v| v.as_str()),
            Some("bridge")
        );
    }

    // The SynthesizeGit intent is journaled with a verified receipt.
    let working_copy = repo.require_working_copy_id().unwrap();
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    let synthesize_count = log
        .entries
        .iter()
        .filter(|entry| entry.operation.payload().kind == OperationKind::SynthesizeGit)
        .count();
    assert_eq!(
        synthesize_count, 2,
        "one journaled synthesis per imported commit"
    );
    for entry in &log.entries {
        assert_eq!(
            entry.verification,
            atomic_repository::OperationVerificationState::Verified,
            "imported operations must complete with verified receipts"
        );
    }

    // Content materialized through the normal path.
    let materialized = fs::read(root.join("src/domain/model.rs")).unwrap();
    assert_eq!(materialized, b"pub struct Model;\npub enum Kind;\n");
    drop(repo);

    // Repeated incremental import is a no-op: the view state, change count,
    // and operation log must not move.
    let log_len_before = {
        let repo = atomic_repository::Repository::open(root).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.operation_log(OperationScope::WorkingCopy(working_copy), None, false)
            .unwrap()
            .entries
            .len()
    };
    atomic_ok(
        root,
        home,
        &["git", "import", "--incremental", "--all", "--no-vault"],
    );
    let repo = atomic_repository::Repository::open(root).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let log_after = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    assert_eq!(
        log_after.entries.len(),
        log_len_before,
        "an idempotent re-import journals nothing new"
    );
    assert_eq!(repo.current_view(), "main");
}

/// The Git object database still resolves after the import (nothing moved).
#[test]
fn git_import_preserves_the_foreign_object_database() {
    let fixture = fixture();
    let root = fixture.root.as_path();
    let home = fixture.home.as_path();

    atomic_ok(root, home, &["git", "import", "--all", "--no-vault"]);

    let opened = GitRepository::open(root).unwrap();
    assert!(opened
        .find_commit(git2::Oid::from_str(&fixture.head).unwrap())
        .is_ok());
    assert_eq!(
        git(root, &["rev-parse", "HEAD^{tree}"]),
        fixture.head_tree,
        "the Git tree is untouched by synthesis"
    );
}

/// A shallow clone imports with an explicit boundary label; deepening the
/// clone afterwards refuses instead of silently reinterpreting the prior
/// bridge sequencing.
#[test]
fn shallow_import_labels_the_boundary_and_deepening_is_refused() {
    // Source repository: three commits.
    let source = tempfile::tempdir().unwrap().keep();
    git(&source, &["init", "-q", "-b", "main"]);
    for (name, content) in [
        ("one.txt", "one\n"),
        ("two.txt", "two\n"),
        ("three.txt", "three\n"),
    ] {
        fs::write(source.join(name), content).unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", name]);
    }

    // Shallow clone: only the tip is present; the shallow boundary is
    // explicit in the clone.
    let clone = tempfile::tempdir().unwrap().keep();
    let clone_home = tempfile::tempdir().unwrap().keep();
    let clone_out = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--quiet",
            &format!("file://{}", source.display()),
            clone.to_str().unwrap(),
        ])
        .output()
        .expect("shallow clone");
    assert!(
        clone_out.status.success(),
        "shallow clone failed: {}",
        String::from_utf8_lossy(&clone_out.stderr)
    );
    assert!(clone.join(".git/shallow").exists());

    atomic_ok(
        clone.as_path(),
        clone_home.as_path(),
        &["git", "import", "--all", "--no-vault"],
    );

    // Every synthesized change carries the explicit shallow boundary label.
    let repo = atomic_repository::Repository::open(clone.as_path()).unwrap();
    let entries = repo.effective_history(Some("main")).unwrap();
    let synthesized: Vec<atomic_core::change::Change> = entries
        .iter()
        .map(|entry| repo.load_change(&entry.hash).unwrap())
        .filter(|change| !change.origin().is_native())
        .collect();
    assert_eq!(
        synthesized.len(),
        1,
        "only the shallow tip is synthesized; the scaffold record is native"
    );
    let change = &synthesized[0];
    let boundaries = change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .and_then(|git| git.get("boundaries"))
        .and_then(|value| value.as_array())
        .cloned()
        .expect("shallow import stamps explicit boundaries");
    assert_eq!(boundaries, vec![serde_json::json!("shallow")]);
    let tip_sha = git(clone.as_path(), &["rev-parse", "HEAD"]);
    match change.origin() {
        ChangeOrigin::GitSynthesized { commit, .. } => {
            assert_eq!(*commit, tagged(&tip_sha));
        }
        other => panic!("expected GitSynthesized origin, found {other:?}"),
    }
    let state_before = repo.get_view_info("main").unwrap().state;
    drop(repo);

    // Deepen the clone: the missing history below the imported tip appears.
    let unshallow = Command::new("git")
        .args(["fetch", "--unshallow", "--quiet", "origin"])
        .current_dir(clone.as_path())
        .output()
        .expect("git fetch --unshallow");
    assert!(
        unshallow.status.success(),
        "unshallow failed: {}",
        String::from_utf8_lossy(&unshallow.stderr)
    );
    assert!(!clone.join(".git/shallow").exists());
    assert_eq!(
        git(clone.as_path(), &["rev-list", "--all"]).lines().count(),
        3
    );

    // A full re-import refuses: importing the deepened commits would
    // fabricate a second history below the already-imported change.
    let output = atomic(
        clone.as_path(),
        clone_home.as_path(),
        &["git", "import", "--all", "--no-vault"],
    );
    assert!(
        !output.status.success(),
        "deepened history must be refused, stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("deepened"),
        "the refusal must be an explicit deepening diagnostic, found: {combined}"
    );

    // The refusal advanced nothing.
    let repo = atomic_repository::Repository::open(clone.as_path()).unwrap();
    assert_eq!(
        repo.get_view_info("main").unwrap().state,
        state_before,
        "a refused deepened import must not move the view"
    );
}
