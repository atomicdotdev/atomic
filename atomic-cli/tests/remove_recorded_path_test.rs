//! `atomic remove` of recorded paths: untracking stays a recorded delete.
//!
//! Regressions for the defect where untracking a *recorded* path wiped the
//! `INODES`/`REV_INODES` binding that `record` had created while leaving that
//! change's `PATH_CLAIMS` event visible. Every later name projection then failed
//! to resolve its own claim with "path claim '<path>' has no inode for graph
//! position <pos>", which made `atomic status` itself error and refused all
//! further `atomic record` calls — until `atomic doctor repair-native-indexes`
//! rebuilt the wiped rows.
//!
//! `remove` now mirrors `git rm`: a staged (never recorded) path is untracked
//! outright, while a recorded path is removed from the working copy and left for
//! `atomic record` to finalize.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(root)
        .args(args)
        .env("ATOMIC_HOME", root.join(".atomic/test-home"))
        .env_remove("ATOMIC_RPC")
        .env("ATOMIC_SERVICE", "local")
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

/// Combined stdout+stderr, for assertions that target either stream.
fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn cli(root: &Path, args: &[&str]) -> String {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        output_text(&output)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    cli(root.path(), &["init", "--no-vault"]);
    root
}

/// Track and record a file, so its content — not merely its name — is in the graph.
fn record_file(root: &Path, name: &str, bytes: &[u8]) {
    fs::write(root.join(name), bytes).unwrap();
    cli(root, &["add", name]);
    cli(root, &["record", "-m", &format!("record {name}"), name]);
}

fn change_count(root: &Path) -> usize {
    cli(root, &["log", "-f", "oneline"])
        .lines()
        .filter(|line| line.starts_with('#'))
        .count()
}

/// The reported wedge: after untracking a recorded path the repository must stay
/// usable. `status`, `diff`, and `record` all run the name projection that used
/// to fail closed on the orphaned claim.
#[test]
fn untracking_a_recorded_path_leaves_the_repository_usable() {
    let root = init();
    let dir = root.path();
    record_file(dir, "Cargo.lock", b"a\nb\nc\n");

    let removed = run(dir, &["remove", "Cargo.lock"]);
    assert!(
        removed.status.success(),
        "remove failed: {}",
        output_text(&removed)
    );

    for args in [
        vec!["status"],
        vec!["diff"],
        vec!["log"],
        vec!["doctor", "check"],
        vec!["view", "list"],
        vec!["conflicts"],
    ] {
        let probe = run(dir, &args);
        let text = output_text(&probe);
        assert!(probe.status.success(), "{args:?} after remove: {text}");
        assert!(
            !text.contains("has no inode"),
            "{args:?} still hits the orphaned-claim wedge: {text}"
        );
        assert!(
            !text.contains("This appears to be a bug"),
            "{args:?} reports an internal error after remove: {text}"
        );
    }

    // The deletion is visible and pending, exactly as `git rm` leaves it.
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("deleted:") && status.contains("Cargo.lock"),
        "the removal should read as a pending deletion: {status}"
    );

    // Recording the delete is what stops tracking the path.
    cli(dir, &["record", "-m", "stop tracking Cargo.lock"]);
    let after = cli(dir, &["status"]);
    assert!(
        after.contains("nothing to record"),
        "working tree should be clean once the delete is recorded: {after}"
    );
    assert!(
        !dir.join("Cargo.lock").exists(),
        "a recorded delete keeps the path out of the working copy"
    );
    cli(dir, &["doctor", "check"]);
}

/// Untracking is not complete until the delete is recorded: the untrack itself
/// adds no change, and recording the deletion adds exactly one.
#[test]
fn recorded_path_stays_in_history_until_the_delete_is_recorded() {
    let root = init();
    let dir = root.path();
    record_file(dir, "package-lock.json", b"{\n  \"version\": 1\n}\n");
    let before = change_count(dir);

    cli(dir, &["remove", "package-lock.json"]);
    assert_eq!(
        change_count(dir),
        before,
        "removing a recorded path must not invent a change"
    );

    // The recorded content is still reachable: the change that holds it is intact.
    let log = cli(dir, &["log", "-f", "oneline"]);
    assert!(
        log.contains("record package-lock.json"),
        "the original record should still be in history: {log}"
    );

    cli(dir, &["record", "-m", "untrack the lockfile"]);
    assert_eq!(change_count(dir), before + 1);
    cli(dir, &["doctor", "check"]);
}

/// `--keep` is the `git rm --cached` shape, and a recorded untrack cannot be
/// expressed that way, so it must refuse loudly rather than report success.
#[test]
fn keep_refuses_recorded_paths_and_preserves_everything() {
    let root = init();
    let dir = root.path();
    record_file(dir, "yarn.lock", b"one\ntwo\nthree\n");

    let refused = run(dir, &["remove", "--keep", "yarn.lock"]);
    assert!(
        !refused.status.success(),
        "--keep on a recorded path must not report success: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("cannot untrack recorded path(s)"),
        "the refusal should explain why: {}",
        output_text(&refused)
    );

    // Refusing leaves the file and the repository untouched.
    assert_eq!(
        fs::read(dir.join("yarn.lock")).unwrap(),
        b"one\ntwo\nthree\n",
        "a refusal must not delete the file"
    );
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "a refusal must not create a pending change: {status}"
    );
    cli(dir, &["doctor", "check"]);
}

/// Like `git rm` without `-f`, deleting a recorded path with uncommitted edits is
/// refused so the edits survive. `--force` discards them.
#[test]
fn uncommitted_edits_are_not_deleted_without_force() {
    let root = init();
    let dir = root.path();
    record_file(dir, "Cargo.lock", b"a\nb\nc\n");
    fs::write(dir.join("Cargo.lock"), b"a\nB-changed\nc\n").unwrap();

    let refused = run(dir, &["remove", "Cargo.lock"]);
    assert!(
        !refused.status.success(),
        "removing a dirty recorded path must refuse without --force: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("uncommitted edits"),
        "the refusal should name the reason: {}",
        output_text(&refused)
    );
    assert_eq!(
        fs::read(dir.join("Cargo.lock")).unwrap(),
        b"a\nB-changed\nc\n",
        "the uncommitted edit must survive the refusal"
    );
    cli(dir, &["doctor", "check"]);

    // An explicit force discards the edit, as `git rm -f` does.
    cli(dir, &["remove", "--force", "Cargo.lock"]);
    assert!(!dir.join("Cargo.lock").exists());
    cli(dir, &["record", "-m", "stop tracking Cargo.lock"]);
    cli(dir, &["doctor", "check"]);
}

/// The bytes are recoverable until the delete is recorded: `atomic restore`
/// brings the path back from the graph, which is the undo for a removal the user
/// did not mean to keep.
#[test]
fn an_unrecorded_removal_can_be_restored() {
    let root = init();
    let dir = root.path();
    record_file(dir, "Cargo.lock", b"a\nb\nc\n");
    cli(dir, &["remove", "Cargo.lock"]);
    assert!(!dir.join("Cargo.lock").exists());

    cli(dir, &["restore", "Cargo.lock"]);
    assert_eq!(
        fs::read(dir.join("Cargo.lock")).unwrap(),
        b"a\nb\nc\n",
        "restoring an unrecorded removal should bring the recorded bytes back"
    );

    // Nothing was recorded, so the path is still tracked and the tree is clean.
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "a restored path should leave no pending change: {status}"
    );
    cli(dir, &["doctor", "check"]);
}

/// A never-recorded path is still tracking's to undo: untracked, bytes intact.
#[test]
fn staged_path_is_untracked_and_kept() {
    let root = init();
    let dir = root.path();
    fs::write(dir.join("draft.txt"), b"notes\n").unwrap();
    cli(dir, &["add", "draft.txt"]);

    let out = cli(dir, &["remove", "draft.txt"]);
    assert!(out.contains("Untracked"), "{out}");
    assert_eq!(
        fs::read(dir.join("draft.txt")).unwrap(),
        b"notes\n",
        "untracking a staged path must keep the bytes"
    );
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("draft.txt"),
        "the kept file should now read as untracked: {status}"
    );
    cli(dir, &["doctor", "check"]);
}

/// The same rule holds recursively: every recorded candidate in a directory
/// becomes a pending delete, and the tree records cleanly.
#[test]
fn recorded_directory_untracks_recursively() {
    let root = init();
    let dir = root.path();
    fs::create_dir_all(dir.join("gen/sub")).unwrap();
    fs::write(dir.join("gen/a.lock"), b"a\n").unwrap();
    fs::write(dir.join("gen/sub/b.lock"), b"b\n").unwrap();
    cli(dir, &["add", "gen"]);
    cli(dir, &["record", "-m", "record generated tree", "gen"]);

    cli(dir, &["remove", "gen"]);
    assert!(!dir.join("gen/a.lock").exists(), "recursive remove failed");
    assert!(!dir.join("gen/sub/b.lock").exists(), "nested remove failed");

    cli(dir, &["record", "-m", "stop tracking generated tree"]);
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "a recursive untrack should record cleanly: {status}"
    );
    cli(dir, &["doctor", "check"]);
}

/// Switching views re-materializes from the projection, which is the other route
/// that used to walk into the orphaned-claim failure.
#[test]
fn view_switch_after_removing_a_recorded_path_works() {
    let root = init();
    let dir = root.path();
    record_file(dir, "Cargo.lock", b"a\nb\nc\n");
    cli(dir, &["remove", "Cargo.lock"]);
    cli(dir, &["view", "create", "scratch", "--draft"]);

    // With the delete pending: the switch itself re-projects the tree. Switching
    // is refused without --force while a change is unrecorded, which is the
    // correct signal that the untrack has not happened yet.
    let blocked = run(dir, &["view", "switch", "scratch"]);
    assert!(
        !blocked.status.success(),
        "a pending deletion should block a plain switch: {}",
        output_text(&blocked)
    );

    for args in [
        vec!["view", "switch", "scratch", "--force"],
        vec!["view", "switch", "dev", "--force"],
    ] {
        let probe = run(dir, &args);
        let text = output_text(&probe);
        assert!(probe.status.success(), "{args:?}: {text}");
        assert!(
            !text.contains("has no inode"),
            "{args:?} still hits the orphaned-claim wedge: {text}"
        );
    }

    // The forced switch re-materialized the file from the graph - the untrack was
    // never recorded - so removing and recording it completes the untrack.
    assert!(dir.join("Cargo.lock").exists());
    cli(dir, &["remove", "Cargo.lock"]);
    cli(dir, &["record", "-m", "stop tracking Cargo.lock"]);
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "the working tree should be clean after the recorded delete: {status}"
    );
    cli(dir, &["doctor", "check"]);
}

/// A path already missing from the working copy needs no deletion: it is pending
/// a recorded delete either way, and remove must not fail on the absent bytes.
#[test]
fn already_deleted_recorded_path_reports_the_pending_delete() {
    let root = init();
    let dir = root.path();
    record_file(dir, "Gemfile.lock", b"one\ntwo\n");
    fs::remove_file(dir.join("Gemfile.lock")).unwrap();

    let out = cli(dir, &["remove", "Gemfile.lock"]);
    assert!(
        out.contains("Removed 1 recorded file(s)"),
        "an already-deleted recorded path should still report its pending delete: {out}"
    );
    cli(dir, &["doctor", "check"]);

    cli(dir, &["record", "-m", "stop tracking Gemfile.lock"]);
    let status = cli(dir, &["status"]);
    assert!(
        status.contains("nothing to record"),
        "the pending delete should record: {status}"
    );
}
