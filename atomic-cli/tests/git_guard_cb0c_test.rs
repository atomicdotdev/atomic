//! CB-0C end-to-end coverage for the shared stale-baseline guard.

#![cfg(not(windows))]
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");
const EXACT_STALE_REMEDIATION: &str = "Remediation: Reconcile the observed Git baseline before retrying the refused operation.\n  atomic status --no-reconcile\n  atomic git bridge reconcile\n";

struct Fixture {
    repository: TempDir,
    home: TempDir,
}

impl Fixture {
    fn no_git() -> Self {
        Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        }
    }

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

    fn root(&self) -> &Path {
        self.repository.path()
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.atomic_with_stdin(args, None)
    }

    fn atomic_with_input(&self, args: &[&str], input: &[u8]) -> Output {
        self.atomic_with_stdin(args, Some(input))
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

    fn raw_git_drift(&self, branch: &str) {
        self.git_ok(&["switch", "-q", "-c", branch]);
        fs::write(self.root().join("tracked.txt"), b"raw Git checkout\n")
            .expect("write Git-side drift");
        self.git_ok(&["add", "tracked.txt"]);
        self.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "raw Git checkout"]);
    }

    fn log_json(&self) -> Vec<u8> {
        self.atomic_ok(&["log", "-f", "json", "--full-hash"]).stdout
    }
}

#[derive(Eq, PartialEq)]
struct RepositorySnapshot {
    atomic: BTreeMap<String, Vec<u8>>,
    git: BTreeMap<String, Vec<u8>>,
    worktree: BTreeMap<String, Vec<u8>>,
    log: Vec<u8>,
}

impl RepositorySnapshot {
    fn capture(fixture: &Fixture) -> Self {
        Self {
            atomic: snapshot_path(&fixture.root().join(".atomic")),
            git: snapshot_path(&fixture.root().join(".git")),
            worktree: snapshot_worktree(fixture.root()),
            log: fixture.log_json(),
        }
    }

    fn differences(&self, actual: &Self) -> Vec<String> {
        const MAX_DIFFERENCES: usize = 32;

        let mut differences = Vec::new();
        collect_map_differences(".atomic", &self.atomic, &actual.atomic, &mut differences);
        collect_map_differences(".git", &self.git, &actual.git, &mut differences);
        collect_map_differences(
            "worktree",
            &self.worktree,
            &actual.worktree,
            &mut differences,
        );
        if self.log != actual.log {
            differences.push(format!(
                "Atomic log changed ({} -> {} bytes)",
                self.log.len(),
                actual.log.len()
            ));
        }

        if differences.len() > MAX_DIFFERENCES {
            let omitted = differences.len() - MAX_DIFFERENCES;
            differences.truncate(MAX_DIFFERENCES);
            differences.push(format!("... and {omitted} more differences"));
        }
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

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
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
        // redb 4.2 writes a graceful-shutdown marker into pristine.redb on
        // every clean close, so the file's bytes differ between opens even
        // when no state changed. That internal marker is not a mutation of
        // repository state; record its size so growth is still caught and
        // rely on the change store, bridge journal, and op-log snapshots
        // for semantic tamper evidence.
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

fn latest_change(fixture: &Fixture) -> String {
    let log: Value = serde_json::from_slice(&fixture.log_json()).expect("Atomic JSON log");
    log.as_array()
        .and_then(|changes| changes.first())
        .and_then(|change| change.get("hash"))
        .and_then(Value::as_str)
        .expect("latest Atomic change hash")
        .to_string()
}

fn assert_guard_refusal(fixture: &Fixture, operation: &str, args: &[&str]) {
    let before = RepositorySnapshot::capture(fixture);
    let output = fixture.atomic(args);
    assert!(
        !output.status.success(),
        "atomic {} unexpectedly succeeded",
        args.join(" ")
    );
    assert!(
        output.stdout.is_empty(),
        "{} emitted mass stdout before refusal:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout)
    );
    let report = String::from_utf8_lossy(&output.stderr);
    if report.contains("workspace reconciliation required") {
        assert!(report.contains("HeadSymrefChanged"), "{report}");
        assert!(report.contains("refs/heads/main"), "{report}");
        assert!(report.contains("refs/heads/raw-drift"), "{report}");
        assert!(!report.contains("mass-output-000.txt"), "{report}");
        let after = RepositorySnapshot::capture(fixture);
        assert_snapshot_unchanged(
            &before,
            &after,
            &format!(
                "atomic {} mutated Git, Atomic, or working-copy state before refusal",
                args.join(" ")
            ),
        );
        return;
    }
    for expected in [
        format!("Unsafe operation: {operation}"),
        "Refusal: Git moved away from the bridge checkpoint".to_string(),
        "Old checkpoint Atomic view: main".to_string(),
        "Old checkpoint Atomic state:".to_string(),
        "Old checkpoint Atomic manifest root: atomic-visible-regular-files-blake3-v1:".to_string(),
        "Old checkpoint Git HEAD: attached refs/heads/main at ".to_string(),
        "Old checkpoint Git HEAD tree:".to_string(),
        "Old checkpoint Git index tree:".to_string(),
        "Old checkpoint Git index digest:".to_string(),
        "Old checkpoint Git refs digest:".to_string(),
        "Old checkpoint Git refs: digest-only checkpoint evidence".to_string(),
        "Old checkpoint Git manifest root: git-tree-oid:".to_string(),
        "Current Atomic view: main".to_string(),
        "Current Atomic state:".to_string(),
        "Current Atomic manifest root: atomic-visible-regular-files-blake3-v1:".to_string(),
        "Current Git HEAD: attached refs/heads/raw-drift at ".to_string(),
        "Current Git HEAD tree:".to_string(),
        "Current Git index tree:".to_string(),
        "Current Git index digest:".to_string(),
        "Current Git refs digest:".to_string(),
        "Current Git refs:".to_string(),
        "refs/heads/main ->".to_string(),
        "refs/heads/raw-drift ->".to_string(),
        "Current Git manifest root: git-tree-oid:".to_string(),
    ] {
        assert!(
            report.contains(&expected),
            "{} report omitted {expected:?}:\n{report}",
            args.join(" ")
        );
    }
    assert!(
        report.contains(EXACT_STALE_REMEDIATION),
        "{} did not print the exact remediation:\n{report}",
        args.join(" ")
    );
    assert!(
        !report.contains("mass-output-000.txt"),
        "{} interpreted the untracked tree before refusing:\n{report}",
        args.join(" ")
    );
    let after = RepositorySnapshot::capture(fixture);
    assert_snapshot_unchanged(
        &before,
        &after,
        &format!(
            "atomic {} mutated Git, Atomic, or working-copy state before refusal",
            args.join(" ")
        ),
    );
}

fn parse_hook_json(output: &Output) -> Value {
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("hook JSON result")
        .to_string();
    serde_json::from_str(&line).expect("parse hook JSON result")
}

fn assert_incomplete_hook_result(
    fixture: &Fixture,
    output: &Output,
    session_id: &str,
    expected_operation: &str,
    expected_ref: Option<&str>,
) -> String {
    assert!(
        !output.status.success(),
        "{expected_operation} hook unexpectedly succeeded:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_hook_json(output);
    assert_eq!(json["session_id"], session_id);
    assert_eq!(json["recorded"], false);
    assert!(json.get("change_hash").is_none());
    assert_eq!(json["files"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["incomplete"]["origin"], "unknown_post_checkout");
    assert_eq!(
        json["incomplete"]["paths"],
        serde_json::json!(["tracked.txt"])
    );
    let reason = json["incomplete"]["reason"]
        .as_str()
        .expect("incomplete reason");
    assert!(
        reason.contains(&format!("Unsafe operation: {expected_operation}"))
            || reason.contains("workspace reconciliation required")
    );
    assert!(
        reason.contains(EXACT_STALE_REMEDIATION.trim_end()) || reason.contains("HeadSymrefChanged"),
        "unexpected incomplete-session reason: {reason}"
    );
    let recovery_ref = json["incomplete"]["recovery_ref"]
        .as_str()
        .expect("recovery ref")
        .to_string();
    assert!(recovery_ref.starts_with("refs/atomic/wip/"));
    if let Some(expected_ref) = expected_ref {
        assert_eq!(
            recovery_ref, expected_ref,
            "boundary retry moved the WIP ref"
        );
    }
    fixture.git_ok(&["show-ref", "--verify", &recovery_ref]);
    assert_eq!(
        fixture
            .git_ok(&["show", &format!("{recovery_ref}:tracked.txt")])
            .stdout,
        b"unattributed agent bytes\n"
    );
    recovery_ref
}

fn assert_durable_incomplete_session(fixture: &Fixture, session_id: &str, recovery_ref: &str) {
    let session_path = fixture
        .root()
        .join(".atomic/sessions")
        .join(format!("{session_id}.json"));
    let session: Value = serde_json::from_slice(&fs::read(&session_path).expect("session JSON"))
        .expect("parse session JSON");
    assert_eq!(
        session["status"]["incomplete"]["recovery_ref"],
        recovery_ref
    );
    assert_eq!(session["turn_count"], 0);
    assert_eq!(session["recorded_change_hashes"], serde_json::json!([]));
    assert_eq!(session["files_touched"], serde_json::json!([]));

    let indexed = fixture.atomic_ok(&["session", "show", session_id]);
    let indexed = String::from_utf8_lossy(&indexed.stdout);
    assert!(indexed.contains("Status: incomplete"), "{indexed}");
    assert!(indexed.contains(recovery_ref), "{indexed}");
}

#[test]
fn stale_git_branch_and_head_refuse_every_guarded_process_before_output_or_mutation() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["view", "create", "feature", "--draft", "--parent", "main"]);
    let change = latest_change(&fixture);
    let historical_before =
        fixture.atomic_ok(&["diff", "--change", &change, "--name-only", "--no-color"]);

    fixture.atomic_ok(&["git", "bridge", "enable"]);
    let hook_path = fixture.git_text(&["rev-parse", "--git-path", "hooks/post-checkout"]);
    fs::remove_file(fixture.root().join(hook_path)).expect("remove advisory post-checkout hook");
    // CB-9B also installs the advisory reference-transaction dispatcher; a
    // hook-less observation removes both (a live dispatcher journals the
    // checkout's ref movement by design).
    let ref_hook_path =
        fixture.git_text(&["rev-parse", "--git-path", "hooks/reference-transaction"]);
    fs::remove_file(fixture.root().join(ref_hook_path))
        .expect("remove advisory reference-transaction hook");

    fixture.raw_git_drift("raw-drift");
    assert!(
        !fixture
            .root()
            .join(".atomic/bridge/git-events.jsonl")
            .exists(),
        "raw Git checkout unexpectedly left advisory evidence after hook removal"
    );
    for index in 0..64 {
        fs::write(
            fixture.root().join(format!("mass-output-{index:03}.txt")),
            b"must not be scanned\n",
        )
        .expect("write mass-output sentinel");
    }
    fs::write(
        fixture.root().join("candidate.txt"),
        b"must remain untracked\n",
    )
    .expect("write add candidate");

    for (operation, args) in [
        ("status", vec!["status", "--short"]),
        ("diff", vec!["diff", "--name-only", "--no-color"]),
        ("diff", vec!["diff", "--snapshot", "--no-color"]),
        ("record", vec!["record", "--all", "-m", "must refuse"]),
        ("add", vec!["add", "candidate.txt"]),
        ("view switch", vec!["view", "switch", "feature", "--force"]),
        ("materialize", vec!["restore", "tracked.txt"]),
    ] {
        assert_guard_refusal(&fixture, operation, &args);
    }

    let before_forensic = RepositorySnapshot::capture(&fixture);
    let forensic = fixture.atomic_ok(&["status", "--no-reconcile"]);
    let forensic_text = String::from_utf8_lossy(&forensic.stdout);
    assert!(forensic_text.contains("Forensic status (read-only; reconciliation disabled)"));
    assert!(forensic_text.contains("Bridge checkpoint: present"));
    assert!(forensic_text.contains("refs/heads/raw-drift"));
    let after_forensic = RepositorySnapshot::capture(&fixture);
    assert_snapshot_unchanged(
        &before_forensic,
        &after_forensic,
        "forensic status mutated Git, Atomic, or working-copy state",
    );

    let historical_after =
        fixture.atomic_ok(&["diff", "--change", &change, "--name-only", "--no-color"]);
    assert_eq!(historical_after.stdout, historical_before.stdout);
    assert_eq!(historical_after.stderr, historical_before.stderr);
}

#[test]
fn ordinary_edits_and_atomic_only_advancement_pass_the_shared_guard() {
    let fixture = Fixture::colocated();
    let checkpoint: Value = serde_json::from_slice(
        &fs::read(fixture.root().join(".atomic/bridge/workspace.json")).expect("read checkpoint"),
    )
    .expect("parse checkpoint");
    let checkpoint_state = checkpoint["atomic_state"]
        .as_str()
        .expect("checkpoint Atomic state")
        .to_string();
    let git_head = fixture.git_text(&["rev-parse", "HEAD"]);

    fs::write(fixture.root().join("tracked.txt"), b"ordinary edit\n").expect("write ordinary edit");
    let status = fixture.atomic_ok(&["status", "--short"]);
    assert!(output_text(&status).contains("tracked.txt"));
    let diff = fixture.atomic_ok(&["diff", "--name-only", "--no-color"]);
    assert!(output_text(&diff).contains("tracked.txt"));
    fixture.atomic_ok(&["record", "-m", "Atomic-only advancement"]);

    let forensic = fixture.atomic_ok(&["status", "--no-reconcile"]);
    let forensic = String::from_utf8_lossy(&forensic.stdout);
    let current_state = forensic
        .lines()
        .find_map(|line| line.strip_prefix("Atomic state: "))
        .expect("current Atomic state");
    assert_ne!(current_state, checkpoint_state);
    // CB-8A: an Atomic-origin record is a bridge transition — the recorded
    // state projects immediately, so Git HEAD advances via the operation-
    // specific projection commit and both statuses stay clean (RFC §8.2,
    // Phase 8 acceptance). The checkpoint tracks the projected HEAD.
    let projected_head = fixture.git_text(&["rev-parse", "HEAD"]);
    assert_ne!(projected_head, git_head);
    assert!(
        fixture.git_text(&["status", "--porcelain"]).is_empty(),
        "git status clean after the record projection"
    );

    fixture.atomic_ok(&["status", "--short"]);
    fixture.atomic_ok(&["diff", "--name-only", "--no-color"]);
    fs::write(fixture.root().join("atomic-only.txt"), b"new Atomic work\n")
        .expect("write Atomic-only file");
    fixture.atomic_ok(&["add", "atomic-only.txt"]);
    fixture.atomic_ok(&["record", "-m", "Further Atomic-only advancement"]);
}

#[test]
fn repositories_without_git_keep_normal_working_copy_behavior() {
    let fixture = Fixture::no_git();
    fixture.atomic_ok(&["init", "--view", "main"]);
    fs::write(fixture.root().join("tracked.txt"), b"initial\n").expect("write no-Git file");
    fixture.atomic_ok(&["add", "tracked.txt"]);
    fixture.atomic_ok(&["record", "-m", "Initial no-Git change"]);

    fs::write(fixture.root().join("tracked.txt"), b"edited\n").expect("edit no-Git file");
    let status = fixture.atomic_ok(&["status", "--short"]);
    assert!(output_text(&status).contains("tracked.txt"));
    let diff = fixture.atomic_ok(&["diff", "--name-only", "--no-color"]);
    assert!(output_text(&diff).contains("tracked.txt"));
    fixture.atomic_ok(&["restore", "tracked.txt"]);
    assert_eq!(
        fs::read(fixture.root().join("tracked.txt")).unwrap(),
        b"initial\n"
    );

    fixture.atomic_ok(&["view", "create", "feature", "--draft", "--parent", "main"]);
    fixture.atomic_ok(&["view", "switch", "feature", "--force"]);
    assert_eq!(
        fs::read_to_string(fixture.root().join(".atomic/current_view"))
            .expect("current no-Git view")
            .trim(),
        "feature"
    );
}

#[test]
fn managed_and_unmanaged_agent_endings_persist_incomplete_wip_without_false_changes() {
    let unmanaged = Fixture::colocated();
    unmanaged.raw_git_drift("unmanaged-drift");
    fs::write(
        unmanaged.root().join("tracked.txt"),
        b"unattributed agent bytes\n",
    )
    .expect("write unmanaged agent bytes");
    let unmanaged_log = unmanaged.log_json();
    let first = unmanaged.atomic_with_input(
        &["agent", "hooks", "claude-code", "stop", "--json"],
        br#"{"session_id":"unmanaged-session"}"#,
    );
    let unmanaged_ref = assert_incomplete_hook_result(
        &unmanaged,
        &first,
        "unmanaged-session",
        "agent turn-end",
        None,
    );
    assert_eq!(unmanaged.log_json(), unmanaged_log);
    assert_durable_incomplete_session(&unmanaged, "unmanaged-session", &unmanaged_ref);

    let retry = unmanaged.atomic_with_input(
        &["agent", "hooks", "claude-code", "session-end", "--json"],
        br#"{"session_id":"unmanaged-session"}"#,
    );
    assert_incomplete_hook_result(
        &unmanaged,
        &retry,
        "unmanaged-session",
        "agent turn-end",
        Some(&unmanaged_ref),
    );
    assert_eq!(unmanaged.log_json(), unmanaged_log);

    let managed = Fixture::colocated();
    let canonical_root = managed
        .root()
        .canonicalize()
        .expect("canonical managed root");
    let lifecycle = managed.atomic_ok(&[
        "agent",
        "lifecycle",
        "begin",
        "--owner",
        "claude-code",
        "--session",
        "managed-owner",
        "--executor",
        "claude-code",
        "--view",
        "main",
        "--workdir",
        canonical_root.to_str().expect("UTF-8 managed root"),
        "--json",
    ]);
    let lifecycle: Value = serde_json::from_slice(&lifecycle.stdout).expect("lifecycle JSON");
    let run_id = lifecycle["run_id"]
        .as_str()
        .expect("managed run id")
        .to_string();

    managed.raw_git_drift("managed-drift");
    fs::write(
        managed.root().join("tracked.txt"),
        b"unattributed agent bytes\n",
    )
    .expect("write managed agent bytes");
    let managed_log = managed.log_json();
    let first = managed.atomic_with_input(
        &["agent", "hooks", "claude-code", "session-end", "--json"],
        br#"{"session_id":"managed-session"}"#,
    );
    let managed_ref = assert_incomplete_hook_result(
        &managed,
        &first,
        "managed-session",
        "agent session-end",
        None,
    );
    assert_eq!(managed.log_json(), managed_log);
    assert_durable_incomplete_session(&managed, "managed-session", &managed_ref);

    let retry = managed.atomic_with_input(
        &["agent", "hooks", "claude-code", "stop", "--json"],
        br#"{"session_id":"managed-session"}"#,
    );
    assert_incomplete_hook_result(
        &managed,
        &retry,
        "managed-session",
        "agent session-end",
        Some(&managed_ref),
    );
    assert_eq!(managed.log_json(), managed_log);

    let refusal_path = managed
        .root()
        .join(".atomic/agent-lifecycle")
        .join(format!("{run_id}.refusal"));
    let refusal: Value = serde_json::from_slice(&fs::read(refusal_path).expect("managed refusal"))
        .expect("managed refusal JSON");
    assert_eq!(refusal["session_id"], "managed-session");
    assert_eq!(refusal["outcome"]["recovery_ref"], managed_ref);
    assert_eq!(
        refusal["outcome"]["paths"],
        serde_json::json!(["tracked.txt"])
    );

    let lifecycle_status = managed.atomic_ok(&["agent", "lifecycle", "status", "--json"]);
    let lifecycle_status: Value =
        serde_json::from_slice(&lifecycle_status.stdout).expect("lifecycle status JSON");
    let persisted = lifecycle_status["lifecycles"]
        .as_array()
        .expect("managed lifecycles")
        .iter()
        .find(|lifecycle| lifecycle["run_id"] == run_id)
        .expect("persisted managed lifecycle");
    assert_eq!(persisted["refusal"]["outcome"]["recovery_ref"], managed_ref);
}
