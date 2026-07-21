//! Tests for the turn orchestrator.

use super::*;
use atomic_repository::Repository;

use crate::event::{HookType, TurnEvent};
use crate::turn::phase::Phase;
use crate::turn::session::{AgentSession, SessionStore};
use crate::watcher::fallback::FallbackWatcher;
use crate::watcher::WatcherConfig;
use std::fs;
use tempfile::TempDir;

/// Create a test orchestrator with a real FallbackWatcher.
fn make_orchestrator(dir: &TempDir) -> TurnOrchestrator {
    // Create .atomic directory structure
    fs::create_dir_all(dir.path().join(".atomic/sessions")).unwrap();

    let session_store = SessionStore::for_repo(dir.path()).unwrap();
    let watcher_config = WatcherConfig::new(dir.path());
    let watcher = FallbackWatcher::new(watcher_config);

    TurnOrchestrator::with_watcher(dir.path(), session_store, Box::new(watcher))
}

fn session_start_event(session_id: &str) -> TurnEvent {
    TurnEvent::new(session_id, HookType::SessionStart)
}

fn turn_start_event(session_id: &str, prompt: &str) -> TurnEvent {
    TurnEvent::new(session_id, HookType::TurnStart).with_prompt(prompt)
}

fn turn_end_event(session_id: &str) -> TurnEvent {
    TurnEvent::new(session_id, HookType::TurnEnd)
}

fn session_end_event(session_id: &str) -> TurnEvent {
    TurnEvent::new(session_id, HookType::SessionEnd)
}

// DispatchResult tests

#[test]
fn test_dispatch_result_new() {
    let result = DispatchResult::new("sess-1", Phase::Idle);
    assert_eq!(result.session_id, "sess-1");
    assert_eq!(result.new_phase, Phase::Idle);
    assert!(!result.was_recorded());
    assert!(result.message.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_dispatch_result_with_message() {
    let result = DispatchResult::new("sess-1", Phase::Idle).with_message("Tracking started");
    assert_eq!(result.message.as_deref(), Some("Tracking started"));
}

#[test]
fn test_dispatch_result_with_warning() {
    let result = DispatchResult::new("sess-1", Phase::Active).with_warning("Stale session");
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(result.warnings[0], "Stale session");
}

#[test]
fn test_dispatch_result_display() {
    let result = DispatchResult::new("sess-1", Phase::Idle);
    let s = result.to_string();
    assert!(s.contains("sess-1"));
    assert!(s.contains("idle"));
}

#[test]
fn test_dispatch_result_display_with_warning() {
    let result = DispatchResult::new("sess-1", Phase::Active).with_warning("test warning");
    let s = result.to_string();
    assert!(s.contains("⚠"));
    assert!(s.contains("test warning"));
}

// SessionStart tests

#[tokio::test]
async fn test_session_start_creates_session() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    let result = orch
        .dispatch(session_start_event("sess-new"))
        .await
        .unwrap();

    assert_eq!(result.session_id, "sess-new");
    assert_eq!(result.new_phase, Phase::Idle);
    assert!(result.message.is_some());
    assert!(result.message.unwrap().contains("Atomic is tracking"));

    // Session should be persisted
    let session = orch.session_store.load("sess-new").unwrap();
    assert!(session.is_some());
    assert_eq!(session.unwrap().phase, Phase::Idle);
}

#[tokio::test]
async fn test_session_start_resumes_ended_session() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Create and end a session
    let mut session = AgentSession::new("sess-resume", "claude-code", "Claude Code");
    session.phase = Phase::Ended;
    session.turn_count = 3;
    session.ended_at = Some(chrono::Utc::now());
    orch.session_store.save(&session).unwrap();

    // Resume it
    let result = orch
        .dispatch(session_start_event("sess-resume"))
        .await
        .unwrap();

    assert_eq!(result.new_phase, Phase::Idle);
    let msg = result.message.as_deref().unwrap_or("");
    assert!(msg.contains("resumed"));

    // Verify session state
    let session = orch.session_store.load("sess-resume").unwrap().unwrap();
    assert_eq!(session.phase, Phase::Idle);
    assert!(session.ended_at.is_none()); // cleared
    assert_eq!(session.turn_count, 3); // preserved
}

#[tokio::test]
async fn test_session_start_in_sandbox_adopts_view_without_forking() {
    // Canonical repository with a distinct view the sandbox operates on.
    let canonical = TempDir::new().unwrap();
    let mut repo = Repository::init(canonical.path()).unwrap();
    let user_view = repo.current_view().to_string(); // "dev"
    repo.create_view_from("agent-sbx", &user_view).unwrap();

    // Provision a sandbox working tree bound to the agent view.
    let sandbox_dir = TempDir::new().unwrap();
    repo.provision_sandbox(sandbox_dir.path(), "agent-sbx")
        .unwrap();
    drop(repo); // release the canonical pristine lock before reopening

    // Sanity: the sandbox opens on its provisioned view.
    let sbx = Repository::open_existing(sandbox_dir.path()).unwrap();
    assert!(sbx.is_sandbox());
    assert_eq!(sbx.current_view(), "agent-sbx");
    drop(sbx);

    // Orchestrator rooted at the sandbox working tree (mirrors the hook path).
    let session_store = SessionStore::for_repo(sandbox_dir.path()).unwrap();
    let watcher = FallbackWatcher::new(WatcherConfig::new(sandbox_dir.path()));
    let mut orch =
        TurnOrchestrator::with_watcher(sandbox_dir.path(), session_store, Box::new(watcher));

    orch.dispatch(session_start_event("sess-sbx"))
        .await
        .unwrap();

    // The session records on the sandbox's own view — not a freshly forked one.
    let session = orch.session_store.load("sess-sbx").unwrap().unwrap();
    assert_eq!(session.view_name, "agent-sbx");
    assert!(
        session.parent_view.is_none(),
        "sandbox session must not fork a child view"
    );

    // The canonical graph is untouched: no spurious agent view was forked and
    // the current view was not repointed.
    let canonical_after = Repository::open_existing(canonical.path()).unwrap();
    let mut views = canonical_after.list_views().unwrap();
    views.sort();
    assert_eq!(
        views,
        vec!["agent-sbx".to_string(), user_view.clone()],
        "sandbox session must not fork a new view into the canonical graph"
    );
    assert_eq!(canonical_after.current_view(), user_view);
}

#[tokio::test]
async fn test_full_turn_in_sandbox_records_provenance_into_canonical_graph() {
    // Canonical repo with a distinct view the sandbox operates on.
    let canonical = TempDir::new().unwrap();
    let repo = Repository::init(canonical.path()).unwrap();
    // Default `atomic sandbox create` (no --from/--view) binds to the current
    // shared view. Exercise that exact case.
    let user_view = repo.current_view().to_string();

    let sandbox_dir = TempDir::new().unwrap();
    repo.provision_sandbox(sandbox_dir.path(), &user_view)
        .unwrap();
    drop(repo);

    // Orchestrator rooted at the sandbox working tree (mirrors the hook path).
    let session_store = SessionStore::for_repo(sandbox_dir.path()).unwrap();
    let watcher = FallbackWatcher::new(WatcherConfig::new(sandbox_dir.path()));
    let mut orch =
        TurnOrchestrator::with_watcher(sandbox_dir.path(), session_store, Box::new(watcher));
    orch.set_agent("claude-code", "Claude Code");

    orch.dispatch(session_start_event("sess-sbx-prov"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event("sess-sbx-prov", "Create agent.txt"))
        .await
        .unwrap();

    // Agent writes a file into its sandbox working tree.
    fs::write(sandbox_dir.path().join("agent.txt"), "agent work\n").unwrap();

    let result = orch
        .dispatch(turn_end_event("sess-sbx-prov"))
        .await
        .unwrap();

    assert!(
        result.was_recorded(),
        "a turn that created a file in the sandbox must record a change"
    );
    let change_hash = result.change_recorded.as_ref().unwrap().hash;

    // The provenance graph file must be written into the CANONICAL change
    // store (not a throwaway `.atomic/changes` inside the sandbox). Phase 1 of
    // the save is the lock-free durability guarantee; if it targets the
    // sandbox's local dir, provenance is lost whenever the best-effort Phase 2
    // (which takes the redb write lock) loses a race with a concurrent agent.
    let canonical_changes = atomic_repository::ChangeStore::new(
        canonical.path().join(".atomic").join("changes"),
        atomic_repository::DEFAULT_CACHE_CAPACITY,
    )
    .unwrap();
    assert!(
        canonical_changes.count_provenance_graphs().unwrap() >= 1,
        "provenance graph must be written to the canonical change store"
    );

    // Nothing should be written into a sandbox-local change store.
    let sandbox_changes = sandbox_dir.path().join(".atomic").join("changes");
    if sandbox_changes.is_dir() {
        let local = atomic_repository::ChangeStore::new(
            sandbox_changes,
            atomic_repository::DEFAULT_CACHE_CAPACITY,
        )
        .unwrap();
        assert_eq!(
            local.count_provenance_graphs().unwrap(),
            0,
            "provenance must not be written to a throwaway change store inside the sandbox"
        );
    }

    // End-to-end: the change and its provenance are both registered in the
    // canonical graph.
    let canonical_repo = Repository::open(canonical.path()).unwrap();
    let provenance = canonical_repo
        .find_provenance_for_change(&change_hash)
        .unwrap();
    assert!(
        !provenance.is_empty(),
        "provenance graph for the sandbox turn must be registered in the canonical graph"
    );
}

/// Recording a sandbox turn must not persist the session view into the
/// shared canonical current_view (#99 fixed session-start; this covers the
/// record path). The sandbox is bound to a DIFFERENT view than the user's,
/// so a persisted alignment would flip the pointer and fail the assert.
#[tokio::test]
async fn test_sandbox_turn_end_leaves_canonical_current_view_untouched() {
    let canonical = TempDir::new().unwrap();
    let mut repo = Repository::init(canonical.path()).unwrap();
    let user_view = repo.current_view().to_string();
    repo.create_view_from("agent-draft", &user_view).unwrap();

    let sandbox_dir = TempDir::new().unwrap();
    repo.provision_sandbox(sandbox_dir.path(), "agent-draft")
        .unwrap();
    drop(repo);

    let session_store = SessionStore::for_repo(sandbox_dir.path()).unwrap();
    let watcher = FallbackWatcher::new(WatcherConfig::new(sandbox_dir.path()));
    let mut orch =
        TurnOrchestrator::with_watcher(sandbox_dir.path(), session_store, Box::new(watcher));
    orch.set_agent("claude-code", "Claude Code");

    orch.dispatch(session_start_event("sess-sbx-view"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event("sess-sbx-view", "Create agent.txt"))
        .await
        .unwrap();
    fs::write(sandbox_dir.path().join("agent.txt"), "agent work\n").unwrap();
    let result = orch
        .dispatch(turn_end_event("sess-sbx-view"))
        .await
        .unwrap();
    assert!(result.was_recorded(), "sandbox turn must record");

    let canonical_repo = Repository::open_existing(canonical.path()).unwrap();
    assert_eq!(
        canonical_repo.current_view(),
        user_view,
        "recording a sandbox turn must not switch the user's current view"
    );
}

// TurnStart tests

#[tokio::test]
async fn test_turn_start_transitions_to_active() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Create a session first
    orch.dispatch(session_start_event("sess-1")).await.unwrap();

    // Start a turn
    let result = orch
        .dispatch(turn_start_event("sess-1", "Fix the bug"))
        .await
        .unwrap();

    assert_eq!(result.new_phase, Phase::Active);

    // Watcher should be active
    assert!(orch.watcher.is_active());

    // Session should have the prompt
    let session = orch.session_store.load("sess-1").unwrap().unwrap();
    assert_eq!(session.first_prompt.as_deref(), Some("Fix the bug"));
    assert!(session.current_turn_started_at.is_some());
}

#[tokio::test]
async fn test_turn_start_creates_session_if_missing() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Directly start a turn without SessionStart
    let result = orch
        .dispatch(turn_start_event("sess-orphan", "Do something"))
        .await
        .unwrap();

    assert_eq!(result.new_phase, Phase::Active);

    // Session should be auto-created
    let session = orch.session_store.load("sess-orphan").unwrap();
    assert!(session.is_some());
}

// TurnEnd tests

#[tokio::test]
async fn test_turn_end_with_no_changes() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Start session and turn
    orch.dispatch(session_start_event("sess-1")).await.unwrap();
    orch.dispatch(turn_start_event("sess-1", "Do nothing"))
        .await
        .unwrap();

    // End turn without modifying any files
    let result = orch.dispatch(turn_end_event("sess-1")).await.unwrap();

    assert_eq!(result.new_phase, Phase::Idle);
    assert!(!result.was_recorded()); // No changes → no recording

    // Turn count should be incremented
    let session = orch.session_store.load("sess-1").unwrap().unwrap();
    assert_eq!(session.turn_count, 1);
}

#[test]
fn test_has_working_copy_changes_detects_untracked_files() {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();
    let orch = make_orchestrator(&dir);

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/index.ts"), "console.log('hello');\n").unwrap();

    assert!(
        orch.has_working_copy_changes(),
        "untracked-only turns must reach record_turn so files can be auto-added with provenance"
    );
}

#[tokio::test]
async fn test_turn_end_records_untracked_only_files() {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();
    let mut orch = make_orchestrator(&dir);
    orch.set_agent("codex", "Codex");

    orch.dispatch(session_start_event("sess-untracked"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event(
        "sess-untracked",
        "Create a TypeScript hello world project",
    ))
    .await
    .unwrap();

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("package.json"), "{\"type\":\"module\"}\n").unwrap();
    fs::write(
        dir.path().join("tsconfig.json"),
        "{\"compilerOptions\":{}}\n",
    )
    .unwrap();
    fs::write(dir.path().join("src/index.ts"), "console.log('hello');\n").unwrap();

    let result = orch
        .dispatch(turn_end_event("sess-untracked"))
        .await
        .unwrap();

    assert_eq!(result.new_phase, Phase::Idle);
    assert!(
        result.was_recorded(),
        "turn with only new files should be auto-added and recorded"
    );

    let repo = Repository::open(dir.path()).unwrap();
    let status = repo
        .status(atomic_repository::status::StatusOptions::default())
        .unwrap();
    assert_eq!(status.untracked_count(), 0);
    assert!(
        status.is_clean(),
        "status after auto-record should be clean"
    );

    let recorded = result.change_recorded.as_ref().unwrap();
    assert!(recorded
        .recorded_file_list()
        .contains(&"src/index.ts".to_string()));
    assert!(recorded
        .recorded_file_list()
        .contains(&"package.json".to_string()));
}

// PR0 — Attestation coverage correctness
//
// Regression guard: the session-end attestation must cover only the changes
// the orchestrator actually recorded for this session — NOT the inherited
// baseline (init) changes that the agent view picked up from its parent view.
//
// Before this fix, `create_session_attestation` scanned the full agent-view
// history and reported every change as "covered by the agent". On a fresh
// repo with 2 init changes + 1 agent file, it incorrectly reported 3 changes.
// See: notes/2026-05-26-standup.md §3 + development/atomic-attest-fixes.md (B0).
#[tokio::test]
async fn test_session_attestation_covers_only_agent_recorded_changes() {
    use atomic_core::change::ChangeHeader;

    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();

    // Pre-existing baseline: record a "human" change BEFORE the agent
    // session starts. This change will be inherited by the agent view
    // when the session forks. The bug we're guarding against is that
    // this baseline used to get attributed to the agent.
    let repo_for_baseline = Repository::open(dir.path()).unwrap();
    fs::write(dir.path().join("baseline.txt"), "baseline content\n").unwrap();
    repo_for_baseline
        .add(
            "baseline.txt",
            atomic_repository::tracking::TrackingOptions::default(),
        )
        .expect("track baseline.txt");
    let baseline_outcome = repo_for_baseline
        .record(
            ChangeHeader::new("baseline change"),
            atomic_repository::record::RecordOptions::new().with_all(true),
        )
        .expect("baseline record should succeed");
    let baseline_hash = *baseline_outcome.hash();
    drop(repo_for_baseline);

    let mut orch = make_orchestrator(&dir);
    orch.set_agent("claude-code", "Claude Code");

    orch.dispatch(session_start_event("sess-cov-1"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event("sess-cov-1", "Create agent-p0.txt"))
        .await
        .unwrap();

    fs::write(dir.path().join("agent-p0.txt"), "agent p0 test\n").unwrap();

    let turn_result = orch.dispatch(turn_end_event("sess-cov-1")).await.unwrap();

    let recorded = turn_result
        .change_recorded
        .as_ref()
        .expect("turn with new file must record a change");
    let agent_hash = recorded.hash;
    assert_ne!(agent_hash, baseline_hash);

    // The session must remember the exact hash(es) it just recorded —
    // and NOT the baseline that the agent view inherited.
    let session_after_turn = orch.session_store.load("sess-cov-1").unwrap().unwrap();
    assert_eq!(
        session_after_turn.recorded_change_hashes,
        vec![agent_hash],
        "session must track exactly the agent's recorded changes, not inherited baseline",
    );

    // Confirm the agent view DID inherit the baseline (otherwise the test
    // isn't exercising the regression scenario). We must drop this repo
    // handle before dispatching session_end — `create_session_attestation`
    // opens its own handle and the lock is exclusive.
    {
        let repo = Repository::open(dir.path()).unwrap();
        let agent_view_history = repo
            .log(
                atomic_repository::history::HistoryOptions::default()
                    .view(&session_after_turn.view_name)
                    .include_inherited(true),
            )
            .unwrap();
        let inherited_hashes: Vec<_> = agent_view_history.iter().map(|e| e.hash).collect();
        assert!(
            inherited_hashes.contains(&baseline_hash),
            "agent view should inherit the baseline change from parent",
        );
        assert!(
            inherited_hashes.contains(&agent_hash),
            "agent view should also contain the agent's own change",
        );
    }

    // Session end triggers create_session_attestation.
    orch.dispatch(session_end_event("sess-cov-1"))
        .await
        .unwrap();

    let repo = Repository::open(dir.path()).unwrap();

    let attestation_hashes: Vec<_> = repo
        .change_store()
        .iter_attestations()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        attestation_hashes.len(),
        1,
        "expected exactly one attestation after session end",
    );
    let attest = repo.load_attestation(&attestation_hashes[0]).unwrap();
    assert_eq!(
        attest.changes_covered,
        vec![agent_hash],
        "attestation must cover only what the agent recorded ({:?}) — not the inherited baseline ({:?})",
        agent_hash,
        baseline_hash,
    );
    assert!(
        !attest.changes_covered.contains(&baseline_hash),
        "regression: attestation should never include the pre-session baseline change",
    );
}

// PR0 — early-return guard
//
// A session that never recorded any changes (e.g. agent opened then exited
// without doing work) must NOT create a virtual attestation by falling back
// to the full view history.
#[tokio::test]
async fn test_session_end_skips_attestation_when_no_recorded_changes() {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();
    let mut orch = make_orchestrator(&dir);
    orch.set_agent("claude-code", "Claude Code");

    orch.dispatch(session_start_event("sess-empty"))
        .await
        .unwrap();
    // No turn_end → no record_turn → recorded_change_hashes stays empty.
    orch.dispatch(session_end_event("sess-empty"))
        .await
        .unwrap();

    let repo = Repository::open(dir.path()).unwrap();
    let attestations: Vec<_> = repo
        .change_store()
        .iter_attestations()
        .filter_map(|r| r.ok())
        .collect();
    assert!(
        attestations.is_empty(),
        "session with zero recorded turns must not produce an attestation",
    );
}

// Antigravity CLI has no SessionEnd hook — its Stop payload's
// `fullyIdle: true` is the terminal signal that drives attestation.
#[tokio::test]
async fn test_fully_idle_turn_end_creates_attestation() {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();
    let mut orch = make_orchestrator(&dir);
    orch.set_agent("agy", "Antigravity CLI");

    orch.dispatch(session_start_event("sess-agy-1"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event("sess-agy-1", "Create agy-file.txt"))
        .await
        .unwrap();

    fs::write(dir.path().join("agy-file.txt"), "written by agy\n").unwrap();

    let agy_stop = turn_end_event("sess-agy-1").with_raw_json(serde_json::json!({
        "conversationId": "sess-agy-1",
        "terminationReason": "model_stop",
        "fullyIdle": true
    }));
    let turn_result = orch.dispatch(agy_stop).await.unwrap();
    assert!(turn_result.change_recorded.is_some());

    let repo = Repository::open(dir.path()).unwrap();
    let attestations: Vec<_> = repo
        .change_store()
        .iter_attestations()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        attestations.len(),
        1,
        "Stop with fullyIdle=true must create an attestation",
    );
    let attest = repo.load_attestation(&attestations[0]).unwrap();
    assert_eq!(attest.session_id, "sess-agy-1");
    assert_eq!(attest.changes_covered.len(), 1);

    // A second idle Stop with no new changes must NOT create a duplicate.
    orch.dispatch(turn_start_event("sess-agy-1", "no-op turn"))
        .await
        .unwrap();
    let agy_stop_again = turn_end_event("sess-agy-1").with_raw_json(serde_json::json!({
        "conversationId": "sess-agy-1",
        "terminationReason": "model_stop",
        "fullyIdle": true
    }));
    orch.dispatch(agy_stop_again).await.unwrap();

    let attestations_after: Vec<_> = repo
        .change_store()
        .iter_attestations()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        attestations_after.len(),
        1,
        "fullyIdle Stop with nothing new to attest must be a no-op",
    );
}

#[tokio::test]
async fn test_non_idle_turn_end_skips_attestation() {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();
    let mut orch = make_orchestrator(&dir);
    orch.set_agent("agy", "Antigravity CLI");

    orch.dispatch(session_start_event("sess-agy-2"))
        .await
        .unwrap();
    orch.dispatch(turn_start_event("sess-agy-2", "Create agy-file.txt"))
        .await
        .unwrap();

    fs::write(dir.path().join("agy-file.txt"), "written by agy\n").unwrap();

    // fullyIdle: false — background tasks still running, agent not done.
    let agy_stop_busy = turn_end_event("sess-agy-2").with_raw_json(serde_json::json!({
        "conversationId": "sess-agy-2",
        "terminationReason": "model_stop",
        "fullyIdle": false
    }));
    orch.dispatch(agy_stop_busy).await.unwrap();

    let repo = Repository::open(dir.path()).unwrap();
    let attestations: Vec<_> = repo
        .change_store()
        .iter_attestations()
        .filter_map(|r| r.ok())
        .collect();
    assert!(
        attestations.is_empty(),
        "Stop with fullyIdle=false must not create an attestation",
    );
}

#[tokio::test]
async fn test_turn_end_with_file_changes() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Start session and turn
    orch.dispatch(session_start_event("sess-1")).await.unwrap();
    orch.dispatch(turn_start_event("sess-1", "Add a file"))
        .await
        .unwrap();

    // Create a file during the turn
    fs::write(dir.path().join("new_file.rs"), "fn hello() {}").unwrap();

    // End the turn — recording will fail because there's no initialized
    // Atomic repository (no pristine database), but the session state
    // transitions should still work correctly.
    let result = orch.dispatch(turn_end_event("sess-1")).await.unwrap();

    assert_eq!(result.new_phase, Phase::Idle);

    // Turn count should be incremented even though recording failed
    let session = orch.session_store.load("sess-1").unwrap().unwrap();
    assert_eq!(session.turn_count, 1);

    // Recording failure is reported as a warning, not a fatal error
    // (files_touched is only populated on successful recording)
    assert!(
        !result.warnings.is_empty() || !result.was_recorded(),
        "Expected either warnings about recording failure or no recording"
    );
}

// SessionEnd tests

#[tokio::test]
async fn test_session_end_transitions_to_ended() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    orch.dispatch(session_start_event("sess-1")).await.unwrap();

    let result = orch.dispatch(session_end_event("sess-1")).await.unwrap();

    assert_eq!(result.new_phase, Phase::Ended);

    let session = orch.session_store.load("sess-1").unwrap().unwrap();
    assert!(session.is_ended());
    assert!(session.ended_at.is_some());
}

#[tokio::test]
async fn test_session_end_unknown_session_is_ok() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // End a session that doesn't exist — should not error
    let result = orch
        .dispatch(session_end_event("nonexistent"))
        .await
        .unwrap();

    assert_eq!(result.new_phase, Phase::Ended);
}

#[tokio::test]
async fn test_session_end_cancels_active_turn() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    orch.dispatch(session_start_event("sess-1")).await.unwrap();
    orch.dispatch(turn_start_event("sess-1", "Working..."))
        .await
        .unwrap();

    // Watcher should be active
    assert!(orch.watcher.is_active());

    // End the session while turn is active
    let result = orch.dispatch(session_end_event("sess-1")).await.unwrap();

    assert_eq!(result.new_phase, Phase::Ended);
    // Watcher should be cancelled
    assert!(!orch.watcher.is_active());
}

// ToolUse tests

#[tokio::test]
async fn test_tool_use_event_handled() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    orch.dispatch(session_start_event("sess-1")).await.unwrap();
    orch.dispatch(turn_start_event("sess-1", "Working..."))
        .await
        .unwrap();

    let event = TurnEvent::new("sess-1", HookType::PostToolUse)
        .with_tool_name("Edit")
        .with_tool_use_id("tu-001");

    let result = orch.dispatch(event).await.unwrap();
    assert_eq!(result.new_phase, Phase::Active);
}

#[tokio::test]
async fn test_tool_use_unknown_session_is_ok() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    let event = TurnEvent::new("nonexistent", HookType::PreToolUse).with_tool_name("Task");

    let result = orch.dispatch(event).await.unwrap();
    // Should not error — just returns Idle
    assert_eq!(result.new_phase, Phase::Idle);
}

// Full lifecycle integration test

#[tokio::test]
async fn test_full_lifecycle_multi_turn() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    // Session starts
    let r = orch.dispatch(session_start_event("sess-lc")).await.unwrap();
    assert_eq!(r.new_phase, Phase::Idle);

    // Turn 1
    let r = orch
        .dispatch(turn_start_event("sess-lc", "First prompt"))
        .await
        .unwrap();
    assert_eq!(r.new_phase, Phase::Active);

    fs::write(dir.path().join("file1.rs"), "fn one() {}").unwrap();

    let r = orch.dispatch(turn_end_event("sess-lc")).await.unwrap();
    assert_eq!(r.new_phase, Phase::Idle);

    // Turn 2
    let r = orch
        .dispatch(turn_start_event("sess-lc", "Second prompt"))
        .await
        .unwrap();
    assert_eq!(r.new_phase, Phase::Active);

    fs::write(dir.path().join("file2.rs"), "fn two() {}").unwrap();

    let r = orch.dispatch(turn_end_event("sess-lc")).await.unwrap();
    assert_eq!(r.new_phase, Phase::Idle);

    // Session ends
    let r = orch.dispatch(session_end_event("sess-lc")).await.unwrap();
    assert_eq!(r.new_phase, Phase::Ended);

    // Verify final session state — phase transitions and turn counting
    // work correctly even without a real Atomic repository for recording.
    // files_touched is only populated on successful recordings, which
    // require a real initialized repo (tested in integration tests).
    let session = orch.session_store.load("sess-lc").unwrap().unwrap();
    assert_eq!(session.turn_count, 2);
    assert!(session.is_ended());
    assert_eq!(session.first_prompt.as_deref(), Some("First prompt"));
}

#[tokio::test]
async fn test_lifecycle_ctrl_c_recovery() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_orchestrator(&dir);

    orch.dispatch(session_start_event("sess-cr")).await.unwrap();

    // Start a turn
    orch.dispatch(turn_start_event("sess-cr", "First attempt"))
        .await
        .unwrap();

    // Agent crashes — user restarts with a new prompt
    // This sends another TurnStart without a TurnEnd
    let r = orch
        .dispatch(turn_start_event("sess-cr", "Second attempt"))
        .await
        .unwrap();

    // Should still be Active (Ctrl-C recovery)
    assert_eq!(r.new_phase, Phase::Active);

    // End the second turn normally
    let r = orch.dispatch(turn_end_event("sess-cr")).await.unwrap();
    assert_eq!(r.new_phase, Phase::Idle);

    let session = orch.session_store.load("sess-cr").unwrap().unwrap();
    // First prompt is preserved (not overwritten by second)
    assert_eq!(session.first_prompt.as_deref(), Some("First attempt"));
}

// Debug trait

#[test]
fn test_orchestrator_debug() {
    let dir = TempDir::new().unwrap();
    let orch = make_orchestrator(&dir);
    let debug = format!("{:?}", orch);
    assert!(debug.contains("TurnOrchestrator"));
    assert!(debug.contains("repo_root"));
}

// Managed-run adoption tests

fn managed_context(view: Option<&str>) -> ManagedRunContext {
    ManagedRunContext {
        stamp: crate::turn::session::ManagedRunStamp {
            run_id: "run-mng-1".to_string(),
            owner_agent: "sherpa".to_string(),
            owner_session_id: "sherpa-sess-1".to_string(),
            work_item_id: Some("NONA-42".to_string()),
        },
        view: view.map(|v| v.to_string()),
    }
}

/// Orchestrator rooted at a real initialized repository (recording works).
fn make_repo_orchestrator(dir: &TempDir) -> TurnOrchestrator {
    Repository::init(dir.path()).unwrap();
    let session_store = SessionStore::for_repo(dir.path()).unwrap();
    let watcher = FallbackWatcher::new(WatcherConfig::new(dir.path()));
    TurnOrchestrator::with_watcher(dir.path(), session_store, Box::new(watcher))
}

#[tokio::test]
async fn test_session_start_adopts_declared_managed_view() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_repo_orchestrator(&dir);
    orch.set_agent("codex", "Codex");
    orch.set_managed_run(managed_context(Some("sherpa-run-view")));

    let result = orch
        .dispatch(session_start_event("sess-managed"))
        .await
        .unwrap();
    assert!(result
        .message
        .as_deref()
        .unwrap_or("")
        .contains("sherpa-run-view"));

    let session = orch.session_store.load("sess-managed").unwrap().unwrap();
    assert_eq!(
        session.view_name, "sherpa-run-view",
        "session must adopt the declared managed-run view instead of forking"
    );
    assert_eq!(
        session.parent_view.as_deref(),
        Some("dev"),
        "parent view is remembered so session-end can restore it"
    );
    let stamp = session
        .managed_run
        .expect("session must carry the run stamp");
    assert_eq!(stamp.run_id, "run-mng-1");
    assert_eq!(stamp.owner_agent, "sherpa");
    assert_eq!(stamp.work_item_id.as_deref(), Some("NONA-42"));

    // The working copy switched onto the declared view.
    let repo = Repository::open_existing(dir.path()).unwrap();
    assert_eq!(repo.current_view(), "sherpa-run-view");
    drop(repo);

    // A full turn records onto the declared view.
    orch.dispatch(turn_start_event("sess-managed", "do work"))
        .await
        .unwrap();
    fs::write(dir.path().join("work.rs"), "fn work() {}\n").unwrap();
    let result = orch.dispatch(turn_end_event("sess-managed")).await.unwrap();
    assert!(
        result.was_recorded(),
        "turn under a managed run must record normally — nothing is suppressed"
    );
    assert_eq!(result.view.as_deref(), Some("sherpa-run-view"));

    // Session end restores the parent view.
    orch.dispatch(session_end_event("sess-managed"))
        .await
        .unwrap();
    let repo = Repository::open_existing(dir.path()).unwrap();
    assert_eq!(
        repo.current_view(),
        "dev",
        "session-end restores the user's view"
    );
}

#[tokio::test]
async fn test_managed_run_without_view_stamps_but_forks_normally() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_repo_orchestrator(&dir);
    orch.set_agent("codex", "Codex");
    orch.set_managed_run(managed_context(None));

    orch.dispatch(session_start_event("sess-stamp-only"))
        .await
        .unwrap();

    let session = orch.session_store.load("sess-stamp-only").unwrap().unwrap();
    assert!(
        session.managed_run.is_some(),
        "session must be stamped even when the run declares no view"
    );
    assert_ne!(
        session.view_name, "dev",
        "without a declared view the session forks its usual per-session view"
    );
    assert_eq!(session.parent_view.as_deref(), Some("dev"));
}

#[tokio::test]
async fn test_sandbox_session_keeps_provisioned_view_under_managed_run() {
    // Canonical repository with a distinct view the sandbox operates on.
    let canonical = TempDir::new().unwrap();
    let mut repo = Repository::init(canonical.path()).unwrap();
    let user_view = repo.current_view().to_string();
    repo.create_view_from("agent-sbx", &user_view).unwrap();

    let sandbox_dir = TempDir::new().unwrap();
    repo.provision_sandbox(sandbox_dir.path(), "agent-sbx")
        .unwrap();
    drop(repo);

    let session_store = SessionStore::for_repo(sandbox_dir.path()).unwrap();
    let watcher = FallbackWatcher::new(WatcherConfig::new(sandbox_dir.path()));
    let mut orch =
        TurnOrchestrator::with_watcher(sandbox_dir.path(), session_store, Box::new(watcher));
    orch.set_agent("codex", "Codex");
    // The run declares a DIFFERENT view — the sandbox pointer must win.
    orch.set_managed_run(managed_context(Some("some-other-view")));

    orch.dispatch(session_start_event("sess-sbx-managed"))
        .await
        .unwrap();

    let session = orch
        .session_store
        .load("sess-sbx-managed")
        .unwrap()
        .unwrap();
    assert_eq!(
        session.view_name, "agent-sbx",
        "inside a sandbox the provisioned view always wins over the declared one"
    );
    assert!(session.managed_run.is_some(), "the stamp still applies");

    // The canonical current view is untouched.
    let repo = Repository::open_existing(canonical.path()).unwrap();
    assert_eq!(repo.current_view(), user_view);
}

#[tokio::test]
async fn test_preexisting_session_is_not_restamped() {
    let dir = TempDir::new().unwrap();
    let mut orch = make_repo_orchestrator(&dir);
    orch.set_agent("claude-code", "Claude Code");

    // Session born OUTSIDE any managed run.
    orch.dispatch(session_start_event("sess-direct"))
        .await
        .unwrap();
    assert!(orch
        .session_store
        .load("sess-direct")
        .unwrap()
        .unwrap()
        .managed_run
        .is_none());

    // A lifecycle begins afterwards; re-entering the session must NOT
    // attribute the pre-existing session to the run.
    orch.set_managed_run(managed_context(Some("sherpa-run-view")));
    orch.dispatch(session_start_event("sess-direct"))
        .await
        .unwrap();

    let session = orch.session_store.load("sess-direct").unwrap().unwrap();
    assert!(
        session.managed_run.is_none(),
        "pre-existing sessions are never re-attributed to a managed run"
    );
    assert_ne!(
        session.view_name, "sherpa-run-view",
        "pre-existing sessions keep their own view"
    );
}

#[tokio::test]
async fn test_sandbox_session_files_land_in_canonical_store() {
    // Regression: sessions written by the REAL constructor (the hooks path)
    // must land in the canonical .atomic/sessions — not in a throwaway
    // .atomic/ inside the sandbox working tree — or `lifecycle end`'s
    // stamp harvest never sees sandbox sessions.
    let canonical = TempDir::new().unwrap();
    let mut repo = Repository::init(canonical.path()).unwrap();
    let user_view = repo.current_view().to_string();
    repo.create_view_from("agent-sbx2", &user_view).unwrap();

    let sandbox_dir = TempDir::new().unwrap();
    repo.provision_sandbox(sandbox_dir.path(), "agent-sbx2")
        .unwrap();
    drop(repo);

    let mut orch = TurnOrchestrator::new(sandbox_dir.path()).await.unwrap();
    orch.set_agent("codex", "Codex");
    orch.dispatch(session_start_event("sess-canon"))
        .await
        .unwrap();

    assert!(
        canonical
            .path()
            .join(".atomic/sessions/sess-canon.json")
            .is_file(),
        "sandbox session must be stored in the canonical session store"
    );
    assert!(
        !sandbox_dir
            .path()
            .join(".atomic/sessions/sess-canon.json")
            .exists(),
        "sandbox session must not be stranded in a sandbox-local .atomic"
    );
}
