//! Regression: default `atomic git import` on a Git repository that already
//! tracks `.vault/` content (the colocated vault case).
//!
//! Before the fix, the import's default vault initialization overwrote the
//! Git-tracked user files with template bytes (data loss, dirty `git
//! status`), recorded the templates as an Atomic change, and the standing
//! `git bridge verify` failed the manifest/index layers by construction:
//! the projection excluded `.vault/` while the Git HEAD tree tracked it.
//!
//! The conforming contract (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §6.2):
//! - a pre-existing `.vault/` working copy is user work; the vault tables
//!   hydrate from disk and nothing is overwritten;
//! - a Git-tracked `.vault/` runs under the *tracked* vault policy, so the
//!   manifest projection covers the vault exactly like any other tracked
//!   path and the raw-tree verification is symmetric;
//! - a fresh repository without `.vault/` keeps the default private vault.

#![cfg(unix)]

use git2::{Repository as GitRepository, Signature};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const CUSTOM_MEMORY: &str = "# CUSTOM MEMORY - user work marker 6f4c1a\n";
const CUSTOM_PROMPT: &str = "CUSTOM system prompt marker 6f4c1a\n";
const CUSTOM_SKILL: &str = "CUSTOM atomic-vault skill marker 6f4c1a\n";
const CUSTOM_CODE_INTEL: &str = "CUSTOM code-intelligence skill marker 6f4c1a\n";
const INTENT_BODY: &str = "fixture user intent body\n";

fn tracked_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (".vault/memory/MEMORY.md", CUSTOM_MEMORY),
        (".vault/prompts/system_prompt.md", CUSTOM_PROMPT),
        (".vault/skills/atomic-vault.md", CUSTOM_SKILL),
        (".vault/skills/code-intelligence.md", CUSTOM_CODE_INTEL),
        (".vault/intents/fixture-intent/abc/1/intent.md", INTENT_BODY),
    ]
}

fn run_atomic(root: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("ATOMIC_")) {
        command.env_remove(key);
    }
    let output = command
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_CONFIG_DIR", home.join(".atomic"))
        .env("ATOMIC_SERVICE", "local")
        .env("ATOMIC_NONINTERACTIVE", "1")
        .arg("--no-color")
        .args(args)
        .output()
        .expect("failed to spawn the atomic binary");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn write_tree(git: &GitRepository, files: &[(&str, &str)]) -> git2::Oid {
    fn build(git: &GitRepository, files: &[(&str, &str)]) -> git2::Oid {
        let mut builder = git.treebuilder(None).unwrap();
        let mut children: BTreeMap<String, Vec<(&str, &str)>> = BTreeMap::new();
        for (path, content) in files {
            if let Some((head, rest)) = path.split_once('/') {
                children
                    .entry(head.to_string())
                    .or_default()
                    .push((rest, content));
            } else {
                builder
                    .insert(path, git.blob(content.as_bytes()).unwrap(), 0o100644)
                    .unwrap();
            }
        }
        for (name, nested) in children {
            builder
                .insert(&name, build(git, &nested), 0o040000)
                .unwrap();
        }
        builder.write().unwrap()
    }
    build(git, files)
}

fn commit(
    git: &GitRepository,
    files: &[(&str, &str)],
    message: &str,
    parent: Option<git2::Oid>,
) -> git2::Oid {
    let tree = git.find_tree(write_tree(git, files)).unwrap();
    let signature = Signature::now("Vault Fixture", "vault@example.com").unwrap();
    let parents: Vec<_> = parent
        .map(|oid| git.find_commit(oid).unwrap())
        .into_iter()
        .collect();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        message,
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

/// Build a two-commit repository; `with_vault` controls whether the history
/// tracks custom `.vault/` content, and return its root and isolated HOME.
fn fixture(with_vault: bool) -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let mut git_config = git.config().unwrap();
    git_config.set_str("user.name", "Vault Fixture").unwrap();
    git_config
        .set_str("user.email", "vault@example.com")
        .unwrap();
    drop(git_config);

    let mut first: Vec<(&str, &str)> = vec![("src/main.rs", "fn main() { println!(\"v1\"); }\n")];
    if with_vault {
        first.extend(tracked_vault_files());
    }
    let first_commit = commit(&git, &first, "c1: project + vault", None);

    let mut second: Vec<(&str, &str)> = vec![("src/main.rs", "fn main() { println!(\"v2\"); }\n")];
    if with_vault {
        second.extend(tracked_vault_files());
    }
    commit(&git, &second, "c2: bump main.rs", Some(first_commit));

    git.set_head("refs/heads/main").unwrap();
    git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    (root, home)
}

/// Import must preserve the tracked vault byte-for-byte, keep `git status`
/// clean, hydrate the vault database with the user's content, and leave
/// `git bridge verify` green — with the default import (vault enabled).
#[test]
fn tracked_vault_default_import_preserves_user_content() {
    let (root, home) = fixture(true);

    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "import"]);
    assert!(ok, "import failed:\n{output}");

    // The user's bytes survive untouched.
    for (path, content) in tracked_vault_files() {
        let disk = std::fs::read(root.path().join(path)).unwrap();
        assert_eq!(
            disk,
            content.as_bytes(),
            "user content at {path} was overwritten by the import"
        );
    }

    // No modification to the Git-tracked state: nothing was staged,
    // overwritten, or committed behind the user's back.
    let status = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "git status must stay clean after the import"
    );

    // The bridge recorded the tracked-vault policy.
    let config = std::fs::read_to_string(root.path().join(".atomic/config.toml")).unwrap();
    assert!(
        config.contains("vault = \"tracked\""),
        "the tracked-vault policy must be recorded; config:\n{config}"
    );

    // The vault database holds the user's bytes, not the templates.
    let (ok, output) = run_atomic(
        root.path(),
        home.path(),
        &["vault", "show", "memory/MEMORY.md"],
    );
    assert!(ok, "vault show failed:\n{output}");
    assert!(
        output.contains("user work marker 6f4c1a"),
        "vault database must hold the custom memory content:\n{output}"
    );
    let (ok, output) = run_atomic(root.path(), home.path(), &["vault", "list"]);
    assert!(ok, "vault list failed:\n{output}");
    assert!(
        output.contains("fixture-intent"),
        "vault list must include the imported intent:\n{output}"
    );

    // Atomic's own status is clean.
    let (ok, output) = run_atomic(root.path(), home.path(), &["status"]);
    assert!(ok, "atomic status failed:\n{output}");

    // The standing verifier is green: manifest, index, worktree, census.
    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "bridge", "verify"]);
    assert!(
        ok,
        "bridge verify must pass after a tracked-vault import:\n{output}"
    );
}

/// `--no-vault` only skips vault *subsystem* initialization; the tracked
/// `.vault/` files are still part of the Git tree, so the import and the
/// verifier stay symmetric without any vault database.
#[test]
fn tracked_vault_no_vault_import_still_verifies_clean() {
    let (root, home) = fixture(true);

    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "import", "--no-vault"]);
    assert!(ok, "import --no-vault failed:\n{output}");

    for (path, content) in tracked_vault_files() {
        let disk = std::fs::read(root.path().join(path)).unwrap();
        assert_eq!(disk, content.as_bytes(), "{path} must be untouched");
    }

    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "bridge", "verify"]);
    assert!(ok, "bridge verify must pass:\n{output}");
}

#[test]
fn enabling_bridge_before_import_does_not_force_a_private_vault_policy() {
    let (root, home) = fixture(true);
    // Local Atomic metadata is not part of this fixture's Git history.
    // Keep it ignored before native initialization, as import itself does
    // when it initializes a new Atomic repository.
    let exclude = root.path().join(".git/info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(exclude, "/.atomic/\n/.atomicignore\n/.vault/\n").unwrap();
    for args in [
        &["init", "--no-vault", "--view", "main"][..],
        &["git", "bridge", "enable"][..],
        &["git", "import", "--incremental"][..],
    ] {
        let (ok, output) = run_atomic(root.path(), home.path(), args);
        assert!(ok, "atomic {args:?} failed:\n{output}");
    }

    for (path, content) in tracked_vault_files() {
        assert_eq!(
            std::fs::read(root.path().join(path)).unwrap(),
            content.as_bytes()
        );
    }
    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "bridge", "verify"]);
    assert!(
        ok,
        "tracked-vault import after enabling the bridge must verify:\n{output}"
    );
    let git = GitRepository::open(root.path()).unwrap();
    let mut status_options = git2::StatusOptions::new();
    status_options.include_ignored(false);
    let statuses = git.statuses(Some(&mut status_options)).unwrap();
    let changed: Vec<_> = statuses
        .iter()
        .map(|entry| (entry.path().map(str::to_owned), entry.status()))
        .collect();
    assert!(changed.is_empty(), "Git must remain clean: {changed:?}");
}

/// The fresh-vault experience is unchanged: a repository whose Git tree has
/// no `.vault/` gets the default vault (private policy), a clean Git
/// status, KG enrichment, a content index, and a green verifier — and a
/// later `vault init` is a no-op that resets nothing.
#[test]
fn fresh_repository_default_import_creates_private_vault() {
    let (root, home) = fixture(false);

    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "import"]);
    assert!(ok, "import failed:\n{output}");
    assert!(
        output.contains("KG enriched"),
        "the knowledge graph must be enriched after the fresh vault init:\n{output}"
    );
    assert!(
        output.contains("Content index built"),
        "the content search index must be built:\n{output}"
    );

    for path in [
        ".vault/prompts/system_prompt.md",
        ".vault/skills/atomic-vault.md",
        ".vault/skills/code-intelligence.md",
        ".vault/memory/MEMORY.md",
    ] {
        let disk = root.path().join(path);
        assert!(disk.is_file(), "default {path} must be created");
        assert!(
            !std::fs::read_to_string(&disk).unwrap().is_empty(),
            "default {path} must be non-empty"
        );
    }

    // The vault stays bridge-private: Git ignores it and nothing diverges.
    let status = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "git status must stay clean"
    );

    let (ok, output) = run_atomic(root.path(), home.path(), &["git", "bridge", "verify"]);
    assert!(ok, "bridge verify must pass:\n{output}");

    // Re-running vault init on the initialized vault resets nothing.
    let memory_before = std::fs::read(root.path().join(".vault/memory/MEMORY.md")).unwrap();
    let (ok, output) = run_atomic(root.path(), home.path(), &["vault", "init"]);
    assert!(ok, "vault init must succeed:\n{output}");
    assert!(
        output.contains("already initialized"),
        "an initialized vault must be reported as such:\n{output}"
    );
    assert_eq!(
        std::fs::read(root.path().join(".vault/memory/MEMORY.md")).unwrap(),
        memory_before,
        "vault init must not reset templates or memories"
    );
}
