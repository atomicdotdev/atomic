// Temporary Unix-only Git shim support to unblock Windows native CI.
// Revisit after Windows support is defined; see the RFC temporary platform policy.
#![cfg(unix)]

//! Regression: the alive-walk's bypass-child attachment must not defer a
//! bypass child *under* a direct child it precedes.
//!
//! Original failure (atomic's own history, first-parent ordinals
//! 1, 17, 22, 33, 72 — Git commits 3af95c83, a27c25de, 2d95a174, 961df86d,
//! bae5d00c): after a prepend (`[features]\nremote = []\n\n` before
//! `[package]`), a later insertion placed `ast`/`teams` lines *between*
//! `remote` and the prepended blank line, and the next commit deleted the
//! `ast` line. The deletion surfaced `teams` as a *bypass* child of
//! `remote`, while the pre-insertion edge `remote → blank` kept the blank
//! line as `remote`'s direct child. The old deferral heuristic attached the
//! bypass child under the last direct child — but `teams` is *upstream* of
//! `blank` (live edge `teams → blank`), so the alive graph closed a cycle
//! `blank → teams → blank`. Tarjan collapsed the pair into one SCC and the
//! cyclic-conflict resolution emitted only one side: exactly the blank
//! line's byte was dropped (`renders 1934 but the commit tree expects
//! 1935`), failing the fail-closed staged verification at commit 72.
//!
//! The four states below reproduce the same mechanism in miniature
//! (pre-fix signature: `renders 88 but the commit tree expects 89`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

const PATH: &str = "atomic-cli/Cargo.toml";

const STATE_1: &[u8] = b"[package]\nname = \"x\"\nversion = \"1\"\n";
const STATE_2: &[u8] = b"[features]\nremote = []\n\n[package]\nname = \"x\"\nversion = \"1\"\n";
const STATE_3: &[u8] = b"[features]\ndefault = [\"a\", \"b\"]\nremote = []\nast = [\"p\", \"q\"]\nteams = [\"t\"]\n\n[package]\nname = \"x\"\nversion = \"1\"\n";
const STATE_4: &[u8] =
    b"[features]\ndefault = [\"b\"]\nremote = []\nteams = [\"t\"]\n\n[package]\nname = \"x\"\nversion = \"1\"\n";

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
        .env("GIT_AUTHOR_NAME", "Bypass Cycle Tests")
        .env("GIT_AUTHOR_EMAIL", "bypass-cycle@example.com")
        .env("GIT_COMMITTER_NAME", "Bypass Cycle Tests")
        .env("GIT_COMMITTER_EMAIL", "bypass-cycle@example.com")
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

/// Build the four-state Git repository that reproduces the bypass-cycle
/// byte loss: base file, prepend a section with a blank separator, insert
/// two lines between the section and the blank, then delete one of them.
fn fixture() -> (PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("atomic-cli")).unwrap();
    for (i, state) in [STATE_1, STATE_2, STATE_3, STATE_4].iter().enumerate() {
        fs::write(root.join(PATH), state).unwrap();
        git(&root, &["add", "."]);
        if i == 0 {
            git(&root, &["commit", "-q", "-m", "base file"]);
        } else {
            git(&root, &["commit", "-q", "-am", &format!("state {}", i + 1)]);
        }
    }
    (root, home)
}

/// The graph-rendered bytes for `PATH` on `main`, read in-process exactly
/// as another reader would.
fn rendered_on_main(root: &Path) -> Vec<u8> {
    let repo = atomic_repository::Repository::open(root).unwrap();
    let content = repo
        .get_file_content_on_view(PATH, "main")
        .expect("content retrieval must not fail");
    content.unwrap_or_else(|| panic!("'{PATH}' must be present on view main"))
}

#[test]
fn import_of_prepend_insert_delete_history_renders_every_state_byte_exact() {
    let (root, home) = fixture();

    // The importer's per-commit staged verification is fail-closed: a
    // successful import proves EVERY intermediate state rendered
    // byte-exactly against its commit tree. Pre-fix, this failed at the
    // last commit with "renders 88 byte(s) but the commit tree expects 89".
    let summary = atomic_ok(&root, &home, &["git", "import", "--no-vault"]);
    assert!(summary.contains("4 changes"), "expected 4 imported changes");

    // The final state's graph render is byte-exact (not only the worktree).
    let rendered = rendered_on_main(&root);
    assert_eq!(
        rendered, STATE_4,
        "graph render must equal the final Git tree byte-for-byte"
    );

    // The blank line between `teams` and `[package]` is present: this is
    // the exact byte the bypass-cycle dropped.
    assert!(
        rendered.windows(3).any(|w| w == b"\n\n["),
        "blank separator line must survive the deletion import"
    );

    // Both CLIs agree the workspace is clean, and the shim layers verify.
    let status = atomic_ok(&root, &home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "atomic status must be clean after import, got: {status}"
    );
    atomic_ok(&root, &home, &["git", "shim", "verify"]);

    // Incremental re-import is idempotent (no new changes, still verified).
    let again = atomic_ok(
        &root,
        &home,
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(
        !again.contains("5 changes"),
        "incremental import must not synthesize new changes: {again}"
    );
    assert_eq!(rendered_on_main(&root), STATE_4);
}

#[test]
fn native_record_of_prepend_insert_delete_history_renders_byte_exact() {
    // The same walk serves the native record path (status / materialization
    // read through the identical retrieval), so the bypass-cycle fix must
    // hold there too: record the four states with `atomic record` and prove
    // the final materialized bytes and a clean status.
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();
    fs::create_dir_all(root.join("atomic-cli")).unwrap();

    atomic_ok(&root, &home, &["init", "--view", "main", "--no-vault"]);
    for (i, state) in [STATE_1, STATE_2, STATE_3, STATE_4].iter().enumerate() {
        fs::write(root.join(PATH), state).unwrap();
        atomic_ok(&root, &home, &["add", PATH]);
        let message = if i == 0 {
            "base file".to_string()
        } else {
            format!("state {}", i + 1)
        };
        atomic_ok(&root, &home, &["record", "-m", &message]);
    }

    let on_disk = fs::read(root.join(PATH)).unwrap();
    assert_eq!(
        on_disk.as_slice(),
        STATE_4,
        "native record must materialize the final state byte-exactly"
    );
    assert_eq!(rendered_on_main(&root), STATE_4);

    let status = atomic_ok(&root, &home, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "atomic status must be clean after native records, got: {status}"
    );

    // View switch away and back re-materializes from the graph alone: the
    // blank separator byte must still survive a cold render.
    atomic_ok(
        &root,
        &home,
        &["view", "create", "check", "--parent", "main"],
    );
    atomic_ok(&root, &home, &["view", "switch", "check"]);
    atomic_ok(&root, &home, &["view", "switch", "main"]);
    assert_eq!(fs::read(root.join(PATH)).unwrap().as_slice(), STATE_4);
}
