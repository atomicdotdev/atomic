//! CB-0D integration coverage for advisory Git checkout evidence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use git2::{Oid, Repository as GitRepository, Signature};
use serde_json::Value;
use tempfile::TempDir;

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

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_atomic(root: &Path, home: &Path, view: &str) {
    let output = atomic(root, home, &["init", "--view", view]);
    assert_success(&output, "atomic init");
}

fn commit_file(repository: &GitRepository, path: &str, content: &[u8], message: &str) -> Oid {
    let worktree = repository.workdir().expect("Git worktree");
    fs::write(worktree.join(path), content).expect("write committed file");
    let mut index = repository.index().expect("Git index");
    index.add_path(Path::new(path)).expect("stage file");
    index.write().expect("write index");
    let tree_oid = index.write_tree().expect("write tree");
    let tree = repository.find_tree(tree_oid).expect("find tree");
    let signature = Signature::now("Atomic Test", "atomic@example.com").expect("signature");
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|oid| repository.find_commit(oid).ok());
    match parents.as_ref() {
        Some(parent) => repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[parent],
            )
            .expect("commit with parent"),
        None => repository
            .commit(Some("HEAD"), &signature, &signature, message, &tree, &[])
            .expect("initial commit"),
    }
}

fn init_git(root: &Path) -> GitRepository {
    let repository = GitRepository::init(root).expect("init Git");
    repository
        .set_head("refs/heads/main")
        .expect("set main HEAD");
    repository
}

fn init_git_with_feature(root: &Path) -> (Oid, Oid) {
    let repository = init_git(root);
    let main = commit_file(&repository, "tracked.txt", b"main\n", "main");
    let main_commit = repository.find_commit(main).expect("main commit");
    repository
        .branch("feature", &main_commit, false)
        .expect("feature branch");
    drop(main_commit);
    repository
        .set_head("refs/heads/feature")
        .expect("attach feature");
    repository
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("checkout feature");
    let feature = commit_file(&repository, "tracked.txt", b"feature\n", "feature");
    repository.set_head("refs/heads/main").expect("attach main");
    repository
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("checkout main");
    (main, feature)
}

fn run_git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git")
}

fn journal_path(root: &Path) -> PathBuf {
    root.join(".atomic/bridge/git-events.jsonl")
}

fn journal_records(root: &Path) -> Vec<Value> {
    let content = match fs::read_to_string(journal_path(root)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read journal: {error}"),
    };
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL record"))
        .collect()
}

fn wait_for_deferred_receipt(root: &Path, minimum_checkout_events: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let records = journal_records(root);
        let checkout_events = records
            .iter()
            .filter(|record| record["record_type"] == "post-checkout")
            .count();
        let receipts = records
            .iter()
            .filter(|record| record["record_type"] == "deferred-observation")
            .count();
        let requests_empty = fs::read_dir(root.join(".atomic/bridge/deferred-observations"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if checkout_events >= minimum_checkout_events
            && receipts >= minimum_checkout_events
            && requests_empty
        {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for deferred receipt; records: {records:#?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable");
}

#[test]
fn bridge_enable_records_checkout_and_direct_observation_does_not_need_the_hook() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let (main, feature) = init_git_with_feature(repository.path());
    init_atomic(repository.path(), home.path(), "main");
    let current_view_before = fs::read(repository.path().join(".atomic/current_view"))
        .expect("current view before checkout");
    let legacy_post_commit = repository.path().join(".git/hooks/post-commit");
    fs::write(
        &legacy_post_commit,
        b"#!/bin/sh\n\n# atomic:git:begin\natomic git import --incremental 2>/dev/null || true\n# atomic:git:end\n",
    )
    .expect("legacy Atomic hook");

    let enabled = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&enabled, "bridge enable");

    let hook = repository.path().join(".git/hooks/post-checkout");
    let script = fs::read_to_string(&hook).expect("installed dispatcher");
    assert!(script.contains("# atomic:git-bridge-dispatcher:v1"));
    assert!(script.contains(
        fs::canonicalize(ATOMIC_BIN)
            .expect("canonical Atomic binary")
            .to_str()
            .expect("UTF-8 Atomic binary")
    ));
    assert!(!script.contains("atomic git import"));
    assert!(!script.contains("reconcile"));
    assert!(!legacy_post_commit.exists());

    let refreshed = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&refreshed, "bridge enable refresh");
    assert_eq!(
        script,
        fs::read_to_string(&hook).expect("refreshed dispatcher")
    );

    #[cfg(unix)]
    {
        let dispatch_dir = PathBuf::from(format!("{}.d", hook.display()));
        fs::create_dir_all(&dispatch_dir).expect("dispatcher participant directory");
        let participant = dispatch_dir.join("10-existing-system");
        fs::write(
            &participant,
            b"#!/bin/sh\nprintf 'ran\\n' > existing-hook-ran\n",
        )
        .expect("participant script");
        make_executable(&participant);
    }

    let switched = run_git(repository.path(), &["switch", "feature"]);
    assert_success(&switched, "git switch feature");
    let records = wait_for_deferred_receipt(repository.path(), 1);

    let event = records
        .iter()
        .find(|record| record["record_type"] == "post-checkout")
        .expect("checkout event");
    assert_eq!(event["version"], 1);
    assert_eq!(event["advisory"].as_bool(), Some(true));
    assert_eq!(event["old_head"], main.to_string());
    assert_eq!(event["new_head"], feature.to_string());
    assert_eq!(event["checkout_kind"], "branch");
    assert!(records.iter().any(|record| {
        record["record_type"] == "deferred-observation"
            && record["cause_event_id"] == event["event_id"]
            && record["observation"]["head_oid"] == feature.to_string()
    }));
    assert_eq!(
        current_view_before,
        fs::read(repository.path().join(".atomic/current_view")).expect("current view after hook")
    );
    assert!(!repository
        .path()
        .join(".atomic/bridge/workspace.json")
        .exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_to_string(repository.path().join("existing-hook-ran"))
            .expect("composed participant ran"),
        "ran\n"
    );

    fs::remove_file(&hook).expect("disable dispatcher");
    // CB-9B also installs an advisory reference-transaction dispatcher; a
    // hook-less observation must remove both (a live ref-transaction
    // dispatcher journals the switches' ref movement by design).
    fs::remove_file(repository.path().join(".git/hooks/reference-transaction"))
        .expect("disable reference-transaction dispatcher");
    fs::remove_file(journal_path(repository.path())).expect("remove advisory evidence");
    let deferred_dir = repository
        .path()
        .join(".atomic/bridge/deferred-observations");
    assert!(fs::read_dir(&deferred_dir)
        .expect("deferred directory")
        .next()
        .is_none());
    assert_success(
        &run_git(repository.path(), &["switch", "main"]),
        "git switch main without hook",
    );
    assert_success(
        &run_git(repository.path(), &["switch", "feature"]),
        "git switch feature without hook",
    );
    assert!(!journal_path(repository.path()).exists());

    let forensic = atomic(
        repository.path(),
        home.path(),
        &["status", "--no-reconcile"],
    );
    assert_success(&forensic, "forensic status without hooks");
    let stdout = String::from_utf8_lossy(&forensic.stdout);
    assert!(stdout.contains("Checkpoint classification: Unanchored"));
    assert!(stdout.contains("AtomicViewMismatch"));
}

#[test]
fn custom_hooks_path_is_reported_and_left_untouched() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let git = init_git(repository.path());
    commit_file(&git, "tracked.txt", b"tracked\n", "initial");
    let mut config = git.config().expect("Git config");
    config
        .set_str("core.hooksPath", ".githooks")
        .expect("set custom hooks path");
    drop(config);
    drop(git);
    init_atomic(repository.path(), home.path(), "main");

    let custom_dir = repository.path().join(".githooks");
    fs::create_dir(&custom_dir).expect("custom hooks directory");
    let custom_hook = custom_dir.join("post-checkout");
    let original = b"#!/bin/sh\necho custom-manager\n";
    fs::write(&custom_hook, original).expect("custom hook");

    let output = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&output, "bridge enable with custom hooks path");
    assert_eq!(
        fs::read(&custom_hook).expect("custom hook after enable"),
        original
    );
    assert!(!repository.path().join(".git/hooks/post-checkout").exists());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("core.hooksPath"));
    assert!(combined.contains("hook-post-checkout"));
    assert!(combined.contains(ATOMIC_BIN));
}

#[test]
fn unmanaged_binary_hook_is_left_byte_for_byte_untouched() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let git = init_git(repository.path());
    commit_file(&git, "tracked.txt", b"tracked\n", "initial");
    drop(git);
    init_atomic(repository.path(), home.path(), "main");

    let hook = repository.path().join(".git/hooks/post-checkout");
    let original = b"\x7fELF\x00\xff\xfeunmanaged";
    fs::write(&hook, original).expect("binary hook");

    let output = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&output, "bridge enable with binary hook");
    assert_eq!(fs::read(&hook).expect("binary hook after enable"), original);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("left untouched"));
    assert!(combined.contains("hook-post-checkout"));
}

#[cfg(unix)]
#[test]
fn unmanaged_symlink_hook_and_target_are_left_untouched() {
    use std::os::unix::fs::symlink;

    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let git = init_git(repository.path());
    commit_file(&git, "tracked.txt", b"tracked\n", "initial");
    drop(git);
    init_atomic(repository.path(), home.path(), "main");

    let hook = repository.path().join(".git/hooks/post-checkout");
    let target = repository.path().join("custom-post-checkout");
    let original = b"#!/bin/sh\necho symlink-target\n";
    fs::write(&target, original).expect("symlink target");
    symlink(&target, &hook).expect("hook symlink");

    let output = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&output, "bridge enable with symlink hook");
    assert!(fs::symlink_metadata(&hook)
        .expect("hook metadata")
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read(&target).expect("symlink target after enable"),
        original
    );
}

#[test]
fn linked_worktree_uses_the_shared_common_hooks_directory() {
    let repository = TempDir::new().expect("repository tempdir");
    let linked_parent = TempDir::new().expect("linked parent");
    let home = TempDir::new().expect("home tempdir");
    let git = init_git(repository.path());
    commit_file(&git, "tracked.txt", b"tracked\n", "initial");
    drop(git);

    let linked = linked_parent.path().join("linked");
    let output = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg("linked")
        .arg(&linked)
        .current_dir(repository.path())
        .output()
        .expect("add linked worktree");
    assert_success(&output, "git worktree add");
    init_atomic(&linked, home.path(), "linked");

    let enabled = atomic(&linked, home.path(), &["git", "bridge", "enable"]);
    assert_success(&enabled, "enable from linked worktree");

    assert!(linked.join(".git").is_file());
    assert!(repository.path().join(".git/hooks/post-checkout").is_file());
    let linked_git = GitRepository::open(&linked).expect("open linked worktree");
    assert!(!linked_git.path().join("hooks/post-checkout").exists());
}

#[cfg(unix)]
#[test]
fn dispatcher_reaches_absolute_atomic_binary_with_empty_path() {
    let repository = TempDir::new().expect("repository tempdir");
    let home = TempDir::new().expect("home tempdir");
    let (main, _) = init_git_with_feature(repository.path());
    init_atomic(repository.path(), home.path(), "main");
    let enabled = atomic(repository.path(), home.path(), &["git", "bridge", "enable"]);
    assert_success(&enabled, "bridge enable");

    let hook = repository.path().join(".git/hooks/post-checkout");
    let output = Command::new(&hook)
        .arg(main.to_string())
        .arg(main.to_string())
        .arg("0")
        .current_dir(repository.path())
        .env("PATH", "")
        .env("HOME", home.path())
        .env("ATOMIC_HOME", home.path().join(".atomic"))
        .output()
        .expect("invoke dispatcher");
    assert_success(&output, "dispatcher with empty PATH");

    let records = wait_for_deferred_receipt(repository.path(), 1);
    assert!(records.iter().any(|record| {
        record["record_type"] == "post-checkout" && record["checkout_kind"] == "file"
    }));
}
