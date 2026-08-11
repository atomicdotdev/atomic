//! Integration tests for the bare `atomic insert` promotion path, driving the
//! real CLI binary end-to-end against a temporary repository.
//!
//! Bare `atomic insert` (no change hash, no subcommand) inserts the current
//! view's changes into its parent view. These tests exercise the happy path,
//! the dry-run preview, the idempotent "already even" no-op, the root-view
//! guard, and the shared -> shared confirmation gate.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Path to the compiled `atomic` binary (provided by Cargo to integration tests).
const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Run `atomic <args>` inside `dir`.
fn atomic(dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run atomic")
}

/// Combined stdout + stderr as a lossy string, for substring assertions.
fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

/// Initialize a repo (default view `dev`) with a base file recorded on `dev`.
fn repo_with_base() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(atomic(root, &["init"]).status.success(), "init");
    std::fs::write(root.join("base.txt"), b"base\n").unwrap();
    assert!(
        atomic(root, &["add", "base.txt"]).status.success(),
        "add base"
    );
    assert!(
        atomic(root, &["record", "-m", "base"]).status.success(),
        "record base"
    );
    dir
}

/// Create a draft view `feature` (parent = dev), switch to it, and record a
/// change unique to that view.
fn add_draft_change(root: &Path) {
    assert!(
        atomic(root, &["view", "create", "feature", "--draft", "--switch"])
            .status
            .success(),
        "create+switch draft"
    );
    std::fs::write(root.join("feature.txt"), b"feature work\n").unwrap();
    assert!(
        atomic(root, &["add", "feature.txt"]).status.success(),
        "add feature file"
    );
    assert!(
        atomic(root, &["record", "-m", "feature change"])
            .status
            .success(),
        "record feature change"
    );
}

#[test]
fn bare_insert_promotes_draft_into_parent() {
    let dir = repo_with_base();
    let root = dir.path();
    add_draft_change(root);

    let out = atomic(root, &["insert"]);
    let text = combined(&out);
    assert!(out.status.success(), "bare insert should succeed:\n{text}");
    // Direction is shown explicitly, source -> parent.
    assert!(
        text.contains("feature"),
        "should name the source view:\n{text}"
    );
    assert!(text.contains("dev"), "should name the parent view:\n{text}");
    assert!(
        text.contains("Inserted") && text.contains("change"),
        "should report the insert:\n{text}"
    );

    // Running it again is a friendly no-op: nothing left to promote.
    let again = atomic(root, &["insert"]);
    let again_text = combined(&again);
    assert!(
        again.status.success(),
        "second insert should succeed:\n{again_text}"
    );
    assert!(
        again_text.to_lowercase().contains("already even")
            || again_text.to_lowercase().contains("nothing"),
        "second insert should be a no-op:\n{again_text}"
    );
}

#[test]
fn bare_insert_dry_run_does_not_mutate() {
    let dir = repo_with_base();
    let root = dir.path();
    add_draft_change(root);

    let out = atomic(root, &["insert", "-n"]);
    let text = combined(&out);
    assert!(out.status.success(), "dry-run should succeed:\n{text}");
    assert!(
        text.to_lowercase().contains("dry run"),
        "should announce a dry run:\n{text}"
    );

    // Nothing was inserted, so a real insert still has work to do.
    let real = atomic(root, &["insert"]);
    let real_text = combined(&real);
    assert!(
        real.status.success(),
        "real insert after dry-run should succeed:\n{real_text}"
    );
    assert!(
        real_text.contains("Inserted") && real_text.contains("change"),
        "dry-run must not have inserted anything:\n{real_text}"
    );
}

#[test]
fn bare_insert_on_root_view_errors() {
    // Fresh repo sits on `dev`, a root view with no parent.
    let dir = repo_with_base();
    let root = dir.path();

    let out = atomic(root, &["insert"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "insert on a root view should fail:\n{text}"
    );
    assert!(
        text.contains("root view"),
        "error should explain there is no parent:\n{text}"
    );
}

#[test]
fn insert_view_subcommand_and_alias_are_equivalent() {
    // `insert view <src>` and its `from-view` alias both insert the source
    // view's changes into the current view. Set up a draft with one change,
    // then drive the insert from `dev` (the parent/current view).
    for subcmd in ["view", "from-view"] {
        let dir = repo_with_base();
        let root = dir.path();
        add_draft_change(root); // creates+switches to `feature` with one change
        assert!(
            atomic(root, &["view", "switch", "dev"]).status.success(),
            "switch back to dev"
        );

        let out = atomic(root, &["insert", subcmd, "feature"]);
        let text = combined(&out);
        assert!(
            out.status.success(),
            "insert {subcmd} feature should succeed:\n{text}"
        );
        assert!(
            text.contains("Inserted") && text.contains("change"),
            "insert {subcmd} should report the insert:\n{text}"
        );
        // The change's file is now materialized on dev.
        assert_eq!(
            std::fs::read(root.join("feature.txt")).unwrap(),
            b"feature work\n",
            "insert {subcmd} should materialize the inserted change on dev"
        );
    }
}

#[test]
fn insert_change_alias_pick_still_parses() {
    // The `change` subcommand accepts its `pick` alias. A bogus hash should
    // fail at hash resolution (not at argument parsing), proving the alias and
    // positional wiring are intact.
    let dir = repo_with_base();
    let root = dir.path();

    let out = atomic(root, &["insert", "pick", "deadbeef"]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a nonexistent change should fail:\n{text}"
    );
    // Should be a change-resolution error, not an "unrecognized subcommand".
    assert!(
        !text.to_lowercase().contains("unrecognized")
            && !text.to_lowercase().contains("unexpected argument"),
        "'pick' alias must be accepted as a subcommand:\n{text}"
    );
}

#[test]
fn bare_insert_shared_to_shared_requires_confirmation() {
    let dir = repo_with_base();
    let root = dir.path();

    // Create a *shared* view parented on dev and switch to it.
    assert!(
        atomic(
            root,
            &["view", "create", "staging", "--parent", "dev", "--switch"]
        )
        .status
        .success(),
        "create shared staging"
    );
    std::fs::write(root.join("staging.txt"), b"staging work\n").unwrap();
    assert!(
        atomic(root, &["add", "staging.txt"]).status.success(),
        "add"
    );
    assert!(
        atomic(root, &["record", "-m", "staging change"])
            .status
            .success(),
        "record staging change"
    );

    // Non-interactive (piped stdin): without --confirm this must refuse.
    let refused = atomic(root, &["insert"]);
    let refused_text = combined(&refused);
    assert!(
        !refused.status.success(),
        "shared->shared insert without --confirm should fail:\n{refused_text}"
    );
    assert!(
        refused_text.to_lowercase().contains("confirm"),
        "error should point at --confirm:\n{refused_text}"
    );

    // With --confirm it proceeds in one line.
    let ok = atomic(root, &["insert", "--confirm"]);
    let ok_text = combined(&ok);
    assert!(
        ok.status.success(),
        "insert --confirm should succeed:\n{ok_text}"
    );
    assert!(
        ok_text.contains("Inserted") && ok_text.contains("change"),
        "should report the insert:\n{ok_text}"
    );
}
