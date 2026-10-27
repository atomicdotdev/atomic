use std::io::Write;
use std::process::{Command, Output, Stdio};

use atomic_agent::event::{HookType, TurnEvent};
use atomic_agent::hook_health::{FireOutcome, HookHealth};
use atomic_repository::Repository;
use tempfile::TempDir;

struct Fixture {
    repo: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let repo = TempDir::new().unwrap();
        drop(Repository::init(repo.path()).unwrap());
        Self {
            repo,
            home: TempDir::new().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .args(args)
            .current_dir(self.repo.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("APPDATA", self.home.path().join("config"))
            .env("LOCALAPPDATA", self.home.path().join("local"))
            .env("ATOMIC_SERVICE", "local")
            .env_remove("ATOMIC_AGENT_IDENTITY")
            .env_remove("ATOMIC_SESSION_ID")
            .env_remove("ATOMIC_PARENT_SESSION_ID")
            .env(
                "ATOMIC_DAEMON_SOCKET",
                self.home.path().join("missing.sock"),
            )
            .env(
                "ATOMIC_DAEMON_BIN",
                self.home.path().join("missing-atomicd"),
            );
        command
    }

    fn fire(&self, args: &[&str], input: &[u8], reactor: bool) -> Output {
        let mut command = self.command(args);
        if reactor {
            command.env("ATOMIC_SERVICE", "reactor");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn health(&self) -> HookHealth {
        let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
            self.repo.path().join(".atomic/atomic.redb"),
        )
        .unwrap();
        let mut events: Vec<_> = store
            .load_session_log(None, 0, 512)
            .unwrap()
            .into_iter()
            .map(|entry| entry.event)
            .collect();
        events.extend(
            atomic_agent::hook_log::pending(self.repo.path())
                .unwrap()
                .into_iter()
                .map(|(_, event)| event),
        );
        HookHealth::from_events(events).expect("hook log was recorded")
    }
}

#[test]
fn native_hook_reports_parse_and_initialization_failures() {
    let fixture = Fixture::new();
    let args = [
        "agent",
        "hooks",
        "claude-code",
        "session-start",
        "--foreground",
    ];
    let malformed = fixture.fire(&args, b"{", false);
    assert!(!malformed.status.success());
    let health = fixture.health();
    let verb = &health.agents["claude-code"].verbs["session-start"];
    assert_eq!(verb.outcome, FireOutcome::Error);
    assert!(verb.last_ok.is_none());

    // Valid parsing, but the orchestrator cannot create its session store.
    let sessions = fixture.repo.path().join(".atomic/sessions");
    if sessions.exists() {
        std::fs::remove_dir_all(&sessions).unwrap();
    }
    std::fs::write(sessions, "blocked").unwrap();
    let output = fixture.fire(&args, br#"{"session_id":"native-health"}"#, false);
    assert!(!output.status.success());
    let health = fixture.health();
    let verb = &health.agents["claude-code"].verbs["session-start"];
    assert_eq!(verb.outcome, FireOutcome::Error);
    assert!(verb
        .detail
        .as_deref()
        .unwrap()
        .contains("Failed to create orchestrator"));
}

#[test]
fn typed_receive_reports_parsing_transport_and_service_outcomes() {
    let fixture = Fixture::new();
    let args = ["agent", "receive", "--agent", "opencode", "--json"];
    let malformed = fixture.fire(&args, b"{", false);
    assert!(!malformed.status.success());
    let health = fixture.health();
    let verb = &health.agents["opencode"].verbs["receive"];
    assert_eq!(verb.outcome, FireOutcome::Error);
    assert!(verb.last_ok.is_none());

    let mut event =
        serde_json::to_value(TurnEvent::new("typed-health", HookType::SessionStart)).unwrap();
    event["parent_session_id"] = serde_json::json!("typed-parent");
    let event = serde_json::to_vec(&event).unwrap();
    let success = fixture.fire(&args, &event, false);
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    let health = fixture.health();
    let verbs = &health.agents["opencode"].verbs;
    assert_eq!(verbs["receive"].outcome, FireOutcome::Ok);
    // This row is written inside libatomic, not by the CLI receiver.
    assert_eq!(verbs["session_start"].outcome, FireOutcome::Ok);
    let last_ok = verbs["receive"].last_ok.clone();
    {
        let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
            fixture.repo.path().join(".atomic/atomic.redb"),
        )
        .unwrap();
        let events = store
            .load_session_log(Some("typed-health"), 0, 512)
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|entry| entry.event.parent_session_id.as_deref() == Some("typed-parent")));
    }

    let failure = fixture.fire(&args, &event, true);
    assert!(!failure.status.success());
    let health = fixture.health();
    let verb = &health.agents["opencode"].verbs["receive"];
    assert_eq!(verb.outcome, FireOutcome::Error);
    assert_eq!(verb.last_ok, last_ok);
    assert!(verb.detail.as_deref().unwrap().contains("routing failed"));

    let offline_status = fixture
        .command(&["agent", "status", "--json"])
        .env("ATOMIC_SERVICE", "reactor")
        .output()
        .unwrap();
    assert!(
        offline_status.status.success(),
        "{}",
        String::from_utf8_lossy(&offline_status.stderr)
    );
    let offline_status: serde_json::Value = serde_json::from_slice(&offline_status.stdout).unwrap();
    assert!(offline_status["hook_health_error"].is_string());
    assert!(offline_status.get("silent_agents").is_none());
    assert_eq!(
        offline_status["hook_sessions"][0]["session_id"],
        "typed-health"
    );

    let recovered = fixture.fire(&args, &event, false);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let health = fixture.health();
    assert_eq!(
        health.agents["opencode"].verbs["receive"].outcome,
        FireOutcome::Ok
    );

    let status = fixture
        .command(&["agent", "status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let row = status["hook_health"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "opencode")
        .unwrap();
    assert_eq!(row["verbs_recorded"], 2);
    // A later attributable success must not hide an unassigned parse failure.
    assert_eq!(row["failing_verbs"], serde_json::json!(["receive"]));
    assert_eq!(status["hook_sessions"].as_array().unwrap().len(), 2);
    assert!(!fixture
        .repo
        .path()
        .join(".atomic/hook-health.json")
        .exists());
}

#[test]
fn concurrent_hook_processes_preserve_parent_child_and_unassigned_failures() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        for index in 0..16 {
            let fixture = &fixture;
            scope.spawn(move || {
                let input = if index == 0 {
                    b"{".to_vec()
                } else {
                    serde_json::to_vec(&serde_json::json!({
                        "session_id": format!("child-{index}"), "parent_session_id": "parent",
                        "event_type": "invalid-event",
                    }))
                    .unwrap()
                };
                let output = fixture.fire(
                    &["agent", "receive", "--agent", "opencode", "--json"],
                    &input,
                    false,
                );
                assert!(!output.status.success());
            });
        }
    });
    let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
        fixture.repo.path().join(".atomic/atomic.redb"),
    )
    .unwrap();
    let mut events: Vec<_> = store
        .load_session_log(None, 0, 512)
        .unwrap()
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    events.extend(
        atomic_agent::hook_log::pending(fixture.repo.path())
            .unwrap()
            .into_iter()
            .map(|(_, event)| event),
    );
    let ids: std::collections::HashSet<_> = events.iter().map(|event| &event.event_id).collect();
    assert_eq!(ids.len(), 16);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.session_id.is_none())
            .count(),
        1
    );
    assert!(events
        .iter()
        .filter(|event| event.session_id.is_some())
        .all(|event| event.parent_session_id.as_deref() == Some("parent")));
    assert!(!fixture
        .repo
        .path()
        .join(".atomic/hook-health.json")
        .exists());
}
