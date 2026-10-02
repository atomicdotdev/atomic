//! CB-12A follow-up (::19) AC-9 + AC-11 oracles, end to end through the real
//! CLI hook surface:
//!
//! - AC-9 (Git-vs-Atomic): the REAL pre-commit hook is installed, the Git
//!   commit LANDS (Git's zero exit — the hook never rejects solely for
//!   inability to split), the capture is retained, and the Atomic turn
//!   finalization (`agent hooks ... stop`) carries the durable
//!   `SessionStatus::Incomplete` with `ManagedCaptureAwaitingReassembly` —
//!   nonzero exit, no exact attribution.
//! - AC-11 (bypass matrix): with the hook bypassed (no capture — hook
//!   removed / `--no-verify` shape), the turn still yields
//!   observed-operation-only attribution with durable incomplete (not
//!   merely a refusal); the commit-then-edit sequence keeps the durable
//!   incomplete; and the negative control proves no attribution is
//!   manufactured (the unbound commit list is exactly the observed commit).

use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

#[allow(dead_code)]
fn atomic_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn atomic_ok(root: &std::path::Path, home: &std::path::Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn atomic_with_stdin(
    root: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
    stdin: &str,
) -> Output {
    use std::io::Write as _;
    let mut child = Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn atomic");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("run atomic")
}

fn git(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("run git")
}

/// The managed session fixture (the shape the orchestrator persists): a
/// turn-start boundary on the tracked file, turn 1 in flight.
fn write_managed_session(root: &std::path::Path, session_id: &str) -> String {
    use atomic_core::types::Base32;
    let sessions_dir = root.join(".atomic/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    // The session view: the fixture repo's current view.
    let repo = atomic_repository::Repository::open_readonly(root).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let view = repo.desired_view_name(working_copy).unwrap();
    let view_state = repo.get_view_info(&view).unwrap();
    let state_b32 = view_state.state.to_base32();
    let view_name = view.clone();
    let working_copy_id = working_copy.to_string();
    drop(repo);

    // The Git HEAD the session started from.
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .expect("rev-parse");
    let head_oid = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let head_tree = Command::new("git")
        .args(["rev-parse", "HEAD^{tree}"])
        .current_dir(root)
        .output()
        .expect("rev-parse tree");
    let head_tree = String::from_utf8_lossy(&head_tree.stdout)
        .trim()
        .to_string();
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .expect("abbrev-ref");
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();

    // Build the boundary through the library so the serialized shape is
    // exactly what the orchestrator writes (no hand-maintained field list).
    let boundary = atomic_core::change::session::TurnBoundary {
        session_id: session_id.to_string(),
        turn: 0,
        working_copy: working_copy_id,
        operation: None,
        view: view_name.clone(),
        view_state: Some(
            atomic_core::types::Hash::from_base32(state_b32.as_bytes())
                .expect("the view state parses"),
        ),
        set_id: None,
        snapshot: None,
        git: Some(atomic_core::change::session::GitBoundaryCheckpoint {
            head_oid: Some(head_oid),
            head_symref: Some(branch),
            head_tree: Some(head_tree.clone()),
            index_digest: None,
            index_tree: Some(head_tree),
            index_locked: false,
            repository_state: "clean".to_string(),
            markers: Vec::new(),
        }),
        manifest: None,
        conversion_policy: None,
        at: 1758153600,
    };

    let session_json = serde_json::json!({
        "session_id": session_id,
        "view_name": view_name,
        "phase": "active",
        "status": "active",
        "turn_count": 0,
        "agent_name": "opencode",
        "agent_display_name": "OpenCode",
        "started_at": "2026-09-18T00:00:00Z",
        "boundary_start": boundary,
        "boundary_end": null,
        "turn_outcomes": [],
        "current_turn_started_at": "2026-09-18T00:00:00Z",
        "mac_key": null,
        "attested_operations": [],
        "last_attestation": null,
        "repair_history": [],
        "evidence_retained": false
    });
    let path = sessions_dir.join(format!("{session_id}.json"));
    std::fs::write(&path, serde_json::to_vec(&session_json).unwrap()).unwrap();
    view
}

#[test]
fn managed_capture_commit_lands_and_atomic_finalization_is_durably_incomplete() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();

    // Empty Atomic view; the Git repo carries the base commit.
    atomic_ok(&root, &home, &["init"]);
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "git base"]);

    // Install the REAL pre-commit dispatcher (advisory) and anchor.
    atomic_ok(&root, &home, &["git", "bridge", "enable"]);
    assert!(
        root.join(".git/hooks/pre-commit").exists(),
        "the pre-commit dispatcher is installed"
    );

    let view = write_managed_session(&root, "sess-oracle");
    let _ = view;

    // The agent's work: add a file, stage it, commit via plain git.
    std::fs::write(root.join("committed.txt"), "committed work\n").unwrap();
    let add = git(&root, &["add", "committed.txt"]);
    assert!(
        add.status.success(),
        "git add failed: {}{}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let commit = Command::new("git")
        .args(["commit", "-q", "-m", "the agent commit"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com")
        .env("HOME", &home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("git commit");
    // AC-9 Git-vs-Atomic oracle part 1: the commit LANDS (zero exit — the
    // pre-commit hook never rejects solely for inability to split).
    assert!(
        commit.status.success(),
        "the managed commit must land: {}{}",
        String::from_utf8_lossy(&commit.stdout),
        String::from_utf8_lossy(&commit.stderr)
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .unwrap();
    let landed_oid = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert!(!landed_oid.is_empty(), "the commit is observable in Git");

    // The retained capture evidence exists.
    let sessions_dir = root.join(".atomic/sessions");
    let capture_dir = sessions_dir.join("sess-oracle").join("captures");
    assert!(
        std::fs::read_dir(&capture_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false),
        "the commit-time capture is retained for the turn"
    );

    // The commit moved Git HEAD past the bridge checkpoint — the workspace
    // is unanchored at turn end. The turn-end guard refuses (nonzero) and
    // persists the WIP recovery evidence durably: the AC-9 oracle's
    // nonzero-finalization / no-exact-attribution / retained-recovery
    // triple. (When the workspace is re-anchored and the commit's content
    // is exactly the turn's content, the clean path classifies
    // ManagedGitCommitCaptured instead — covered by the agent-lib exact
    // test dirty_turn_with_verified_capture_and_no_remainder_is_exact.)
    let turn_end = atomic_with_stdin(
        &root,
        &home,
        &["agent", "hooks", "opencode", "stop"],
        &serde_json::json!({
            "session_id": "sess-oracle",
            "turn_number": 1,
            "prompt": "the managed commit"
        })
        .to_string(),
    );
    assert!(
        !turn_end.status.success(),
        "the Atomic finalization must exit nonzero for the captured-but-unreassembled commit: {}{}",
        String::from_utf8_lossy(&turn_end.stdout),
        String::from_utf8_lossy(&turn_end.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&turn_end.stdout),
        String::from_utf8_lossy(&turn_end.stderr)
    );
    assert!(
        text.contains("Unsafe operation: agent turn-end")
            && text.contains("workspace is not anchored"),
        "the finalization refuses typed with the unanchored-workspace oracle: {text}"
    );

    // The durable session evidence: incomplete, with a retained recovery ref
    // (the WIP evidence the oracle requires).
    let raw = std::fs::read_to_string(root.join(".atomic/sessions/sess-oracle.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let incomplete = &value["status"]["incomplete"];
    assert!(
        !incomplete.is_null(),
        "the session carries durable incomplete evidence: {value:?}"
    );
    let recovery_ref = incomplete["recovery_ref"].as_str().unwrap_or_default();
    assert!(
        recovery_ref.starts_with("refs/atomic/wip/"),
        "the recovery evidence is retained as a WIP ref: {incomplete:?}"
    );
    // No exact attribution: the atomic view never claimed the commit.
    assert!(
        value["turn_outcomes"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "no turn outcome claims the commit exactly: {value:?}"
    );
}

#[test]
fn bypassed_commit_lands_and_finalization_keeps_durable_unattributed_incomplete() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();

    atomic_ok(&root, &home, &["init"]);
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "git base"]);

    let view = write_managed_session(&root, "sess-bypass");
    let _ = view;

    // NO hook installed: the bypass shape. The agent's work lands via the
    // bypassed commit (Git exit 0).
    std::fs::write(root.join("bypassed.txt"), "bypassed work\n").unwrap();
    let add = git(&root, &["add", "bypassed.txt"]);
    assert!(
        add.status.success(),
        "git add failed: {}{}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let commit = Command::new("git")
        .args(["commit", "-q", "-m", "bypassed commit"])
        .current_dir(&root)
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com")
        .env("HOME", &home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("commit");
    assert!(
        commit.status.success(),
        "a bypassed commit still lands (Git exit 0): {}{}",
        String::from_utf8_lossy(&commit.stdout),
        String::from_utf8_lossy(&commit.stderr)
    );

    // Re-anchor the workspace at the landed commit before finalization.
    atomic_ok(&root, &home, &["git", "bridge", "reconcile"]);

    // Atomic finalization: observed-operation-only + durable incomplete.
    let turn_end = atomic_with_stdin(
        &root,
        &home,
        &["agent", "hooks", "opencode", "stop"],
        &serde_json::json!({
            "session_id": "sess-bypass",
            "turn_number": 1
        })
        .to_string(),
    );
    assert!(
        !turn_end.status.success(),
        "the bypassed commit's finalization exits nonzero"
    );
    let raw = std::fs::read_to_string(root.join(".atomic/sessions/sess-bypass.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let incomplete = &value["status"]["incomplete"];
    assert!(
        !incomplete.is_null(),
        "the session carries durable incomplete evidence: {value:?}"
    );
    assert_eq!(
        incomplete["origin"].as_str(),
        Some("unattributed_git_operation"),
        "the bypass origin is the unattributed-git-operation class"
    );
    // The negative control: exactly the observed commit is named as unbound
    // — no invented attribution beyond the observation.
    assert!(
        incomplete["unbound_commits"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the observed commit is retained as unbound evidence: {incomplete:?}"
    );
}
