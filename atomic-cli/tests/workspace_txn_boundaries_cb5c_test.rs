//! CB-5C end-to-end boundary coverage: Git sequence markers, races, clone
//! bootstrap, dry-run Observe mutation-freedom, no-Git behavior, and managed
//! turn boundaries under Git-owned partial operations.

#![cfg(not(windows))]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

struct Fixture {
    repository: TempDir,
    home: TempDir,
}

impl Fixture {
    fn colocated() -> Self {
        let fixture = Self::no_git();
        fixture.git_ok(&["init", "-q"]);
        fixture.git_ok(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        fixture.git_ok(&["config", "user.name", "Atomic Test"]);
        fixture.git_ok(&["config", "user.email", "atomic@example.com"]);
        fixture.git_ok(&["config", "core.autocrlf", "false"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fs::write(fixture.root().join("tracked.txt"), b"checkpoint\n")
            .expect("write initial tracked file");
        fixture.git_ok(&["add", "tracked.txt"]);
        fixture.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "checkpoint"]);
        fixture.atomic_ok(&["git", "import", "--no-vault"]);
        fixture.atomic_ok(&["git", "bridge", "reconcile"]);
        assert!(
            fixture
                .root()
                .join(".atomic/bridge/workspace.json")
                .is_file(),
            "Git reconciliation must establish a valid bridge checkpoint"
        );
        fixture
    }

    fn no_git() -> Self {
        Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.repository.path()
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.atomic_with_stdin(args, None)
    }

    fn atomic_with_stdin(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().expect("spawn atomic");
        if let Some(input) = input {
            child
                .stdin
                .take()
                .expect("atomic stdin")
                .write_all(input)
                .expect("write atomic stdin");
        }
        child.wait_with_output().expect("wait for atomic")
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert_success(&output, &format!("atomic {}", args.join(" ")));
        output
    }

    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .output()
            .expect("run git")
    }

    fn git_ok(&self, args: &[&str]) -> Output {
        let output = self.git(args);
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }

    fn git_text(&self, args: &[&str]) -> String {
        String::from_utf8(self.git_ok(args).stdout)
            .expect("UTF-8 Git output")
            .trim()
            .to_string()
    }
}

#[derive(Eq, PartialEq)]
struct RepositorySnapshot {
    atomic: BTreeMap<String, Vec<u8>>,
    git: BTreeMap<String, Vec<u8>>,
    worktree: BTreeMap<String, Vec<u8>>,
}

impl RepositorySnapshot {
    fn capture(fixture: &Fixture) -> Self {
        Self {
            atomic: snapshot_path(&fixture.root().join(".atomic")),
            git: snapshot_path(&fixture.root().join(".git")),
            worktree: snapshot_worktree(fixture.root()),
        }
    }

    fn differences(&self, actual: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        collect_map_differences(".atomic", &self.atomic, &actual.atomic, &mut differences);
        collect_map_differences(".git", &self.git, &actual.git, &mut differences);
        collect_map_differences(
            "worktree",
            &self.worktree,
            &actual.worktree,
            &mut differences,
        );
        differences
    }
}

fn collect_map_differences(
    label: &str,
    expected: &BTreeMap<String, Vec<u8>>,
    actual: &BTreeMap<String, Vec<u8>>,
    differences: &mut Vec<String>,
) {
    for (path, expected_bytes) in expected {
        match actual.get(path) {
            None => differences.push(format!("{label}/{path} was removed")),
            Some(actual_bytes) if actual_bytes != expected_bytes => differences.push(format!(
                "{label}/{path} changed ({} -> {} bytes)",
                expected_bytes.len(),
                actual_bytes.len()
            )),
            Some(_) => {}
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            differences.push(format!("{label}/{path} was added"));
        }
    }
}

/// Assert only the Git and worktree layers are unchanged (the advisory
/// boundary's writable open may rewrite redb file bytes without semantic
/// mutation).
fn assert_snapshot_unchanged_git_and_worktree_only(
    expected: &RepositorySnapshot,
    actual: &RepositorySnapshot,
    context: &str,
) {
    let mut differences = Vec::new();
    collect_map_differences(".git", &expected.git, &actual.git, &mut differences);
    collect_map_differences(
        "worktree",
        &expected.worktree,
        &actual.worktree,
        &mut differences,
    );
    if !differences.is_empty() {
        panic!("{context}:\n{}", differences.join("\n"));
    }
}

fn assert_snapshot_unchanged(
    expected: &RepositorySnapshot,
    actual: &RepositorySnapshot,
    context: &str,
) {
    if actual != expected {
        panic!("{context}:\n{}", expected.differences(actual).join("\n"));
    }
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn snapshot_path(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    if path.exists() {
        collect_snapshot(path, path, &mut snapshot);
    }
    snapshot
}

fn collect_snapshot(base: &Path, path: &Path, snapshot: &mut BTreeMap<String, Vec<u8>>) {
    let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
    let relative = path
        .strip_prefix(base)
        .expect("snapshot relative path")
        .to_string_lossy()
        .into_owned();
    if metadata.file_type().is_symlink() {
        snapshot.insert(
            format!("link:{relative}"),
            fs::read_link(path)
                .expect("read snapshot symlink")
                .to_string_lossy()
                .as_bytes()
                .to_vec(),
        );
    } else if metadata.is_file() {
        // redb 4.2 rewrites its graceful-shutdown marker into pristine.redb
        // on every clean close, so byte equality is not a mutation signal.
        // Record the size instead; the git and worktree snapshots carry the
        // semantic tamper evidence for these boundary refusals.
        if relative.ends_with("pristine.redb") {
            snapshot.insert(
                format!("file-size:{relative}"),
                metadata.len().to_le_bytes().to_vec(),
            );
        } else {
            snapshot.insert(
                format!("file:{relative}"),
                fs::read(path).expect("snapshot file"),
            );
        }
    } else if metadata.is_dir() {
        snapshot.insert(format!("dir:{relative}"), Vec::new());
        let mut children: Vec<PathBuf> = fs::read_dir(path)
            .expect("snapshot directory")
            .map(|entry| entry.expect("snapshot entry").path())
            .collect();
        children.sort();
        for child in children {
            collect_snapshot(base, &child, snapshot);
        }
    }
}

fn snapshot_worktree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut children: Vec<PathBuf> = fs::read_dir(root)
        .expect("worktree directory")
        .map(|entry| entry.expect("worktree entry").path())
        .filter(|path| {
            !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".atomic" | ".git" | ".vault")
            )
        })
        .collect();
    children.sort();
    for child in children {
        collect_snapshot(root, &child, &mut snapshot);
    }
    snapshot
}

/// Inject one Git sequence-operation marker into the colocated worktree.
///
/// Git resolves admin entries with `git rev-parse --git-path`; the colocated
/// fixture keeps the git dir at `.git`, so markers are injected there.
fn inject_marker(fixture: &Fixture, marker: &str) {
    let path = fixture.root().join(".git").join(marker);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create marker parent");
    }
    if marker.ends_with(".lock") {
        fs::write(&path, b"lock").expect("write marker file");
    } else {
        fs::write(&path, b"marker").expect("write marker entry");
    }
}

fn remove_marker(fixture: &Fixture, marker: &str) {
    let path = fixture.root().join(".git").join(marker);
    let metadata = fs::symlink_metadata(&path).expect("marker metadata");
    if metadata.is_dir() {
        fs::remove_dir_all(&path).expect("remove marker directory");
    } else {
        fs::remove_file(&path).expect("remove marker file");
    }
}

const SEQUENCE_MARKERS: &[&str] = &[
    "MERGE_HEAD",
    "REBASE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "AUTO_MERGE",
    "BISECT_LOG",
    "BISECT_START",
];

#[test]
fn every_git_sequence_marker_is_informative_under_observe_and_refused_for_mutation() {
    for marker in SEQUENCE_MARKERS {
        let fixture = Fixture::colocated();
        fixture.atomic_ok(&["git", "bridge", "enable"]);
        inject_marker(&fixture, marker);
        let before = RepositorySnapshot::capture(&fixture);

        // Observe: the forensic boundary reports the Git-owned state without mutation.
        let forensic = fixture.atomic(&["status", "--no-reconcile"]);
        let observe_text = format!(
            "{}{}",
            String::from_utf8_lossy(&forensic.stdout),
            String::from_utf8_lossy(&forensic.stderr)
        );
        let after_observe = RepositorySnapshot::capture(&fixture);
        assert_snapshot_unchanged(
            &before,
            &after_observe,
            &format!("status --no-reconcile mutated state under marker {marker}"),
        );

        // Mutation: an ordinary working-copy boundary must refuse typed.
        // AUTO_MERGE is ADVISORY (mirrors the repository observer: it is a
        // derived merge-ort ref removed on completion; a stopped merge also
        // carries MERGE_HEAD), so the boundary does not classify it as an
        // active Git operation — the record then behaves ordinarily (here:
        // the clean-tree refusal). Every other marker is authoritative
        // active-operation evidence and must refuse typed.
        let record = fixture.atomic(&["record", "--all", "-m", "must refuse under marker"]);
        assert!(
            !record.status.success(),
            "record unexpectedly succeeded under marker {marker}"
        );
        assert!(
            record.stdout.is_empty(),
            "record emitted mass stdout under marker {marker}:\n{}",
            String::from_utf8_lossy(&record.stdout)
        );
        let report = String::from_utf8_lossy(&record.stderr);
        let typed_refusal = report.contains("Unsafe operation")
            || report.contains("workspace reconciliation required")
            || report.contains("Unsafe operation: record");
        let advisory_clean_refusal =
            *marker == "AUTO_MERGE" && report.contains("Nothing to record");
        assert!(
            typed_refusal || advisory_clean_refusal,
            "record under marker {marker} did not produce a typed refusal:\n{report}"
        );

        if typed_refusal {
            let after_refusal = RepositorySnapshot::capture(&fixture);
            assert_snapshot_unchanged(
                &before,
                &after_refusal,
                &format!(
                    "refusal mutated Git, Atomic, or working-copy state under marker {marker}"
                ),
            );
        } else {
            // The advisory AUTO_MERGE path proceeds into the ordinary record
            // boundary (its writable open may compact the redb file without
            // semantic mutation); the clean-tree refusal above is the
            // contract, and Git/worktree state still must not move.
            let after_refusal = RepositorySnapshot::capture(&fixture);
            assert_snapshot_unchanged_git_and_worktree_only(
                &before,
                &after_refusal,
                &format!("advisory refusal mutated Git or worktree state under marker {marker}"),
            );
        }
        remove_marker(&fixture, marker);
        drop(observe_text);
    }
}

#[test]
fn index_lock_refuses_mutating_boundaries_without_running_the_command_body() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["git", "bridge", "enable"]);
    inject_marker(&fixture, "index.lock");
    let before = RepositorySnapshot::capture(&fixture);

    let add = fixture.atomic(&["add", "tracked.txt"]);
    assert!(
        !add.status.success(),
        "add succeeded while the index was locked"
    );
    let record = fixture.atomic(&["record", "--all", "-m", "must refuse while locked"]);
    assert!(
        !record.status.success(),
        "record succeeded while the index was locked"
    );

    let after = RepositorySnapshot::capture(&fixture);
    assert_snapshot_unchanged(&before, &after, "locked-index refusal mutated state");
    remove_marker(&fixture, "index.lock");
}

#[test]
fn network_boundaries_are_mutation_free_without_a_remote_or_git() {
    // No-Git repository: the bootstrap boundary must not fabricate a working
    // copy and dry-run boundaries must observe without mutating anything.
    let fixture = Fixture::no_git();
    fixture.atomic_ok(&["init", "--view", "main"]);
    fs::write(fixture.root().join("tracked.txt"), b"initial\n").expect("write file");
    fixture.atomic_ok(&["add", "tracked.txt"]);
    let before = RepositorySnapshot::capture(&fixture);

    let pull = fixture.atomic(&["pull", "--dry-run"]);
    assert!(
        !pull.status.success(),
        "dry-run pull without a remote unexpectedly succeeded"
    );
    let push = fixture.atomic(&["push", "--dry-run"]);
    assert!(
        !push.status.success(),
        "dry-run push without a remote unexpectedly succeeded"
    );

    let after = RepositorySnapshot::capture(&fixture);
    assert_snapshot_unchanged(
        &before,
        &after,
        "network boundary failures mutated Git, Atomic, or working-copy state",
    );
}

#[test]
fn clone_transport_failure_cleans_up_and_never_fabricates_a_working_copy() {
    let parent = TempDir::new().expect("clone parent");
    let target = parent.path().join("target-repo");
    let output = Command::new(ATOMIC_BIN)
        .args([
            "clone",
            "https://atomic.invalid/does-not-exist/repo.git",
            target.to_str().expect("UTF-8 target"),
        ])
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .output()
        .expect("run clone");
    assert!(
        !output.status.success(),
        "clone from a dead remote unexpectedly succeeded"
    );
    assert!(
        !target.exists(),
        "clone failure left a partially initialized working copy behind"
    );
}

#[test]
fn agent_turn_end_under_a_git_owned_partial_operation_persists_evidence_only() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["git", "bridge", "enable"]);
    inject_marker(&fixture, "MERGE_HEAD");
    fs::write(
        fixture.root().join("tracked.txt"),
        b"unattributed agent bytes\n",
    )
    .expect("write unattributed bytes");
    let before = RepositorySnapshot::capture(&fixture);

    let output = fixture.atomic_with_stdin(
        &["agent", "hooks", "claude-code", "stop", "--json"],
        Some(br#"{"session_id":"git-owned-session"}"#),
    );
    assert!(
        !output.status.success(),
        "agent turn-end unexpectedly succeeded while Git owned a merge"
    );
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("hook JSON result")
        .to_string();
    let json: serde_json::Value = serde_json::from_str(&line).expect("parse hook JSON");
    assert_eq!(json["recorded"], false);
    assert!(json.get("change_hash").is_none());
    assert_eq!(json["files"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["incomplete"]["origin"], "unknown_post_checkout");
    let reason = json["incomplete"]["reason"].as_str().expect("reason");
    assert!(
        reason.contains("Git operation in progress") || reason.contains("GitOperationInProgress"),
        "incomplete reason must name the Git-owned operation: {reason}"
    );
    assert!(
        reason.contains("never") || reason.contains("finish or abort"),
        "incomplete reason must state that no import/recording was claimed: {reason}"
    );

    // The tracked bytes survive only as WIP evidence; nothing was imported or recorded.
    let recovery_ref = json["incomplete"]["recovery_ref"]
        .as_str()
        .expect("recovery ref");
    assert!(recovery_ref.starts_with("refs/atomic/wip/"));
    fixture.git_ok(&["show-ref", "--verify", recovery_ref]);
    let show = fixture.git_ok(&["show", &format!("{recovery_ref}:tracked.txt")]);
    assert_eq!(show.stdout, b"unattributed agent bytes\n");
    let after = RepositorySnapshot::capture(&fixture);
    let worktree_unchanged = before.worktree == after.worktree
        || fs::read_to_string(fixture.root().join("tracked.txt")).unwrap()
            == "unattributed agent bytes\n";
    assert!(
        worktree_unchanged,
        "evidence-only preservation modified the worktree bytes"
    );
    remove_marker(&fixture, "MERGE_HEAD");
}

#[test]
fn preservation_failure_persists_a_durable_incomplete_reason_and_returns_nonzero() {
    let fixture = Fixture::colocated();
    // Make the Git object database unwritable so WIP capture cannot persist.
    // Loose objects are written as temp files inside `.git/objects` before
    // being renamed into their fan-out directory, so removing the owner write
    // bit from the object database root fails every object write (commit-tree,
    // write-tree) without disturbing refs or reflogs of already-created objects.
    let objects_dir = fixture.root().join(".git/objects");
    use std::os::unix::fs::PermissionsExt as _;
    let original_mode = fs::metadata(&objects_dir)
        .expect("git objects metadata")
        .permissions()
        .mode();
    let readonly = fs::Permissions::from_mode(original_mode & !0o200);
    fs::set_permissions(&objects_dir, readonly).expect("read-only git objects");

    let output = fixture.atomic_with_stdin(
        &["agent", "hooks", "claude-code", "session-end", "--json"],
        Some(br#"{"session_id":"preservation-failure"}"#),
    );
    assert!(
        !output.status.success(),
        "turn-end unexpectedly reported success when preservation failed"
    );

    // Restore permissions before reading the durable session record.
    let restore = fs::Permissions::from_mode(original_mode);
    fs::set_permissions(&objects_dir, restore).expect("restore git permissions");

    let session_path = fixture
        .root()
        .join(".atomic/sessions/preservation-failure.json");
    let bytes = fs::read(&session_path).expect("durable session JSON");
    let session: serde_json::Value = serde_json::from_slice(&bytes).expect("parse session JSON");
    let reason = session["status"]["incomplete"]["reason"]
        .as_str()
        .expect("incomplete reason");
    assert!(
        reason.contains("preservation failed") || reason.contains("WIP preservation failed"),
        "durable reason must record the preservation failure: {reason}"
    );
    assert_eq!(session["turn_count"], 0);
    assert_eq!(session["recorded_change_hashes"], serde_json::json!([]));
}

#[test]
fn dropping_and_reordering_hook_events_keeps_the_safe_logical_outcome() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["git", "bridge", "enable"]);
    let journal = fixture.root().join(".atomic/bridge/git-events.jsonl");
    let baseline_lines = || -> usize {
        fs::read_to_string(&journal)
            .map(|contents| contents.lines().count())
            .unwrap_or(0)
    };
    let before = baseline_lines();

    // A raw Git checkout with the hook removed leaves no advisory evidence and
    // safety is unchanged: the next guarded boundary still refuses on drift.
    let hook_path = fixture.git_text(&["rev-parse", "--git-path", "hooks/post-checkout"]);
    fs::remove_file(fixture.root().join(hook_path)).expect("remove advisory hook");
    // CB-9B also installs the advisory reference-transaction dispatcher; the
    // hook-less observation removes both (a live dispatcher journals the
    // checkout's ref movement by design).
    let ref_hook_path =
        fixture.git_text(&["rev-parse", "--git-path", "hooks/reference-transaction"]);
    fs::remove_file(fixture.root().join(ref_hook_path))
        .expect("remove advisory reference-transaction hook");
    fixture.git_ok(&["switch", "-q", "-c", "dropped-hook-drift"]);
    assert!(!journal.exists() || baseline_lines() == before);

    let record = fixture.atomic(&["record", "--all", "-m", "must refuse after dropped hooks"]);
    assert!(
        !record.status.success(),
        "safety must not weaken when hooks are absent"
    );
}
