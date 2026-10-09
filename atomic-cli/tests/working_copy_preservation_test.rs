//! Exercise working-copy protection through the actual CLI/libatomic boundary.
use std::path::Path;
use std::process::{Command, Output};

use atomic_repository::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn atomic(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .output()
        .unwrap()
}

fn run(root: &Path, args: &[&str]) -> String {
    let out = atomic(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn record(root: &Path, message: &str) {
    run(
        root,
        &[
            "record",
            "-a",
            "-m",
            message,
            "--author",
            "Test <test@example.invalid>",
        ],
    );
}

fn history(root: &Path, view: &str) -> Value {
    serde_json::from_str(&run(root, &["log", "--view", view, "--format", "json"])).unwrap()
}

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    std::fs::write(dir.path().join("local.txt"), "recorded\n").unwrap();
    std::fs::write(dir.path().join("obsolete.txt"), "old content\n").unwrap();
    record(dir.path(), "base");
    dir
}

fn incoming(root: &Path) -> String {
    run(root, &["view", "create", "feature", "--switch"]);
    std::fs::write(root.join("incoming.txt"), "incoming\n").unwrap();
    record(root, "incoming");
    let hash = history(root, "feature")[0]["hash"]
        .as_str()
        .unwrap()
        .to_string();
    run(root, &["view", "switch", "dev"]);
    hash
}

#[test]
fn dirty_insert_refuses_before_publication_then_stash_insert_pop_preserves_work() {
    let dir = fixture();
    let root = dir.path();
    let hash = incoming(root);
    std::fs::write(root.join("local.txt"), "unrecorded work\n").unwrap();
    let before = history(root, "dev");
    let out = atomic(root, &["insert", &hash]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unrecorded changes"));
    assert_eq!(
        std::fs::read(root.join("local.txt")).unwrap(),
        b"unrecorded work\n"
    );
    assert!(!root.join("incoming.txt").exists());
    assert_eq!(history(root, "dev"), before);

    run(root, &["stash", "push", "-m", "local work"]);
    run(root, &["insert", &hash]);
    run(root, &["stash", "pop"]);
    assert_eq!(
        std::fs::read(root.join("local.txt")).unwrap(),
        b"unrecorded work\n"
    );
    assert_eq!(
        std::fs::read(root.join("incoming.txt")).unwrap(),
        b"incoming\n"
    );
    assert_eq!(history(root, "dev").as_array().unwrap().len(), 2);
}

#[test]
fn view_and_multi_pick_inserts_preserve_deleted_and_untracked_work() {
    for deleted in [false, true] {
        let dir = fixture();
        let root = dir.path();
        let hash = incoming(root);
        if deleted {
            std::fs::remove_file(root.join("local.txt")).unwrap();
        } else {
            // The new graph path collides with an untracked user file.
            std::fs::write(root.join("incoming.txt"), "local untracked\n").unwrap();
        }
        let before = history(root, "dev");
        for args in [
            vec!["insert", "view", "feature"],
            vec!["insert", "change", hash.as_str()],
        ] {
            let out = atomic(root, &args);
            assert!(!out.status.success(), "must refuse {args:?}");
            assert_eq!(history(root, "dev"), before);
            if deleted {
                assert!(!root.join("local.txt").exists());
            } else {
                assert_eq!(
                    std::fs::read(root.join("incoming.txt")).unwrap(),
                    b"local untracked\n"
                );
            }
        }
    }
}

#[test]
fn preview_and_non_current_insert_do_not_touch_dirty_working_copy() {
    let dir = fixture();
    let root = dir.path();
    incoming(root);
    run(root, &["view", "create", "target"]);
    std::fs::write(root.join("local.txt"), "unrecorded\n").unwrap();
    let before = history(root, "dev");
    run(root, &["insert", "view", "feature", "--dry-run"]);
    assert_eq!(history(root, "dev"), before);
    run(root, &["insert", "view", "feature", "--to", "target"]);
    assert_eq!(history(root, "dev"), before);
    assert_eq!(
        std::fs::read(root.join("local.txt")).unwrap(),
        b"unrecorded\n"
    );
    assert!(!root.join("incoming.txt").exists());
    assert!(history(root, "target")
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["message"] == "incoming"));
}

#[test]
fn clean_insert_preserves_both_same_name_claimants_in_opposite_orders() {
    let dir = fixture();
    let root = dir.path();
    for (view, content) in [("left", "left\n"), ("right", "right\n")] {
        run(root, &["view", "create", view, "--switch"]);
        std::fs::write(root.join("same.txt"), content).unwrap();
        record(root, view);
        run(root, &["view", "switch", "dev"]);
    }
    let mut projections = Vec::new();
    let mut hashes = Vec::new();
    for (view, sources) in [("ab", ["left", "right"]), ("ba", ["right", "left"])] {
        run(root, &["view", "create", view, "--switch"]);
        for source in sources {
            run(root, &["insert", "view", source, "--allow-conflicts"]);
        }
        let bytes = std::fs::read_to_string(root.join("same.txt")).unwrap();
        assert!(bytes.contains("left\n") && bytes.contains("right\n"));
        let conflicts = run(root, &["conflicts", "--short"]);
        assert!(conflicts.contains("same.txt"));
        projections.push((bytes, conflicts));
        let mut membership: Vec<_> = history(root, view)
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["hash"].as_str().unwrap().to_string())
            .collect();
        membership.sort();
        hashes.push(membership);
        run(root, &["view", "switch", "dev"]);
    }
    assert_eq!(hashes[0], hashes[1]);
    assert_eq!(projections[0], projections[1]);
}

#[test]
fn stash_pop_restores_a_deletion_after_reopening() {
    let dir = fixture();
    let root = dir.path();
    let before = history(root, "dev");
    std::fs::remove_file(root.join("obsolete.txt")).unwrap();
    run(root, &["stash", "push", "-m", "delete obsolete"]);
    assert_eq!(
        std::fs::read(root.join("obsolete.txt")).unwrap(),
        b"old content\n"
    );
    // Each CLI invocation reopens the repository and reads the saved manifest.
    run(root, &["stash", "pop"]);
    assert!(!root.join("obsolete.txt").exists());
    let status = run(root, &["status"]);
    assert!(status.contains("deleted:") && status.contains("obsolete.txt"));
    assert!(run(root, &["stash", "list"]).contains("No stashes"));
    assert_eq!(history(root, "dev"), before);
}

#[test]
fn stash_mixed_edits_and_deletions_restore_together_and_apply_is_repeatable() {
    let dir = fixture();
    let root = dir.path();
    std::fs::write(root.join("local.txt"), "edited\n").unwrap();
    std::fs::remove_file(root.join("obsolete.txt")).unwrap();
    run(root, &["stash", "push"]);
    assert_eq!(
        std::fs::read(root.join("local.txt")).unwrap(),
        b"recorded\n"
    );
    assert!(root.join("obsolete.txt").exists());
    for _ in 0..2 {
        run(root, &["stash", "apply"]);
        assert_eq!(std::fs::read(root.join("local.txt")).unwrap(), b"edited\n");
        assert!(!root.join("obsolete.txt").exists());
    }
    run(root, &["stash", "pop"]);
    assert!(!root.join("obsolete.txt").exists());
}

#[test]
fn failed_stash_deletion_keeps_new_work_and_stash_for_recovery() {
    let dir = fixture();
    let root = dir.path();
    std::fs::remove_file(root.join("obsolete.txt")).unwrap();
    run(root, &["stash", "push"]);
    std::fs::write(root.join("obsolete.txt"), "new work after stash\n").unwrap();
    let out = atomic(root, &["stash", "pop"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed since it was stashed"));
    assert_eq!(
        std::fs::read(root.join("obsolete.txt")).unwrap(),
        b"new work after stash\n"
    );
    assert!(!run(root, &["stash", "list"]).contains("No stashes"));
}

#[test]
fn missing_legacy_stash_payload_fails_without_dropping_the_stash() {
    let dir = fixture();
    let root = dir.path();
    std::fs::write(root.join("local.txt"), "stashed work\n").unwrap();
    run(root, &["stash", "push"]);
    let repo = Repository::open(root).unwrap();
    let stash = repo.stash_list().unwrap().remove(0);
    let sidecar = repo.dot_dir().join("stashes").join(stash.view_name);
    drop(repo);
    assert_eq!(
        std::fs::read_to_string(sidecar.join("MANIFEST")).unwrap(),
        "local.txt"
    );
    std::fs::remove_file(sidecar.join("local.txt")).unwrap();
    let out = atomic(root, &["stash", "pop"]);
    assert!(!out.status.success());
    assert!(sidecar.join("MANIFEST").exists());
    assert!(!run(root, &["stash", "list"]).contains("No stashes"));
    assert_eq!(
        std::fs::read(root.join("local.txt")).unwrap(),
        b"recorded\n"
    );
}

#[cfg(unix)]
#[test]
fn stash_deletion_cannot_follow_a_replaced_parent_symlink() {
    let dir = fixture();
    let root = dir.path();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/file.txt"), "recorded\n").unwrap();
    record(root, "nested file");
    std::fs::remove_file(root.join("nested/file.txt")).unwrap();
    run(root, &["stash", "push"]);
    std::fs::remove_file(root.join("nested/file.txt")).unwrap();
    std::fs::remove_dir(root.join("nested")).unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("file.txt"), "recorded\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("nested")).unwrap();
    let out = atomic(root, &["stash", "pop"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlink"));
    assert_eq!(
        std::fs::read(outside.path().join("file.txt")).unwrap(),
        b"recorded\n"
    );
    assert!(!run(root, &["stash", "list"]).contains("No stashes"));
}
