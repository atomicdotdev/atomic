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
