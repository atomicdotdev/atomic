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
        HookHealth::read(self.repo.path()).expect("hook health was recorded")
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

    let event =
        serde_json::to_vec(&TurnEvent::new("typed-health", HookType::SessionStart)).unwrap();
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

    let failure = fixture.fire(&args, &event, true);
    assert!(!failure.status.success());
    let health = fixture.health();
    let verb = &health.agents["opencode"].verbs["receive"];
    assert_eq!(verb.outcome, FireOutcome::Error);
    assert_eq!(verb.last_ok, last_ok);
    assert!(verb.detail.as_deref().unwrap().contains("routing failed"));

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
    assert_eq!(row["failing_verbs"], serde_json::json!([]));
}
