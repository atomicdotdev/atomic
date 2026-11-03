//! Authoring → work start → attestation → real hook recording. No fixture
//! supplies plan_id or manufactures a provenance graph/SessionTurn.
#![cfg(unix)]

use atomic_agent::turn::session::{ManagedRunStamp, SessionStore};
use atomic_repository::Repository;
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

struct Fixture {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        fixture.run(&[
            "identity",
            "new",
            "tester",
            "--email",
            "tester@example.invalid",
            "--set-default",
        ]);
        fixture.run(&["init"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env_remove("ATOMIC_RPC")
            .env_remove("ATOMIC_SESSION_ID")
            .env_remove("ATOMIC_AGENT_IDENTITY");
        command
    }

    fn run(&self, args: &[&str]) -> String {
        success(self.command().args(args).output().unwrap())
    }

    fn hook(&self, session: &str, verb: &str) -> serde_json::Value {
        let mut child = self
            .command()
            .args(["agent", "hooks", "opencode", verb, "--foreground", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({"session_id": session, "cwd": self.root(),
            "prompt": "Implement a greeting", "model": "test", "provider": "test",
            "response": "Implemented and tested a greeting"});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let text = success(child.wait_with_output().unwrap());
        serde_json::from_str(text.lines().last().unwrap()).unwrap()
    }

    fn new_intent(&self) -> (String, String) {
        let output = self.run(&["intent", "new", "Implement a greeting"]);
        let id = output
            .lines()
            .find_map(|line| line.strip_prefix("Created intent: "))
            .unwrap()
            .to_string();
        let repo = Repository::open_readonly(self.root()).unwrap();
        let entry = repo.vault_intent_show(&id).unwrap();
        let frontmatter: serde_json::Value = serde_json::from_str(&entry.frontmatter_json).unwrap();
        (id, frontmatter["uid"].as_str().unwrap().to_string())
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn ordinary_opencode_authoring_records_explicit_governing_intent() {
    for via_environment in [false, true] {
        let f = Fixture::new();
        let session = "ordinary-intent-session";
        f.hook(session, "session-start");
        f.hook(session, "user-prompt");
        let (id, uid) = f.new_intent();
        let path = f.root().join(format!(".vault/intents/{uid}/intent.md"));
        let scaffold = fs::read_to_string(&path).unwrap();
        let spine = scaffold.split(":::why").next().unwrap();
        fs::write(&path, format!("{spine}:::why\nMake the greeting usable.\n:::\n\n:::acceptance-criterion{{#{uid}-ac-1 status=unmet}}\nA greeting exists.\n:::\n\n:::task{{#{uid}-1 status=open criteria={uid}-ac-1}}\nImplement the greeting.\n::file-ref{{path=greeting.txt}}\n:::\n")).unwrap();
        f.run(&["vault", "sync"]);

        let mut start = f.command();
        start.args([
            "intent",
            "update",
            &id,
            "--status",
            "in_progress",
            "--turn",
            "1",
        ]);
        if via_environment {
            start.env("ATOMIC_SESSION_ID", session);
        } else {
            start.args(["--session-id", session]);
        }
        success(start.output().unwrap());

        let store = SessionStore::for_repo(f.root()).unwrap();
        assert!(store.load(session).unwrap().unwrap().managed_run.is_none());
        assert_eq!(
            store
                .turn_intent_binding(session, 1)
                .unwrap()
                .unwrap()
                .intent_uid,
            uid
        );
        // Work starts only after the durable declaration is observable.
        fs::write(f.root().join("greeting.txt"), "Hello, world!\n").unwrap();
        assert_eq!(
            fs::read_to_string(f.root().join("greeting.txt")).unwrap(),
            "Hello, world!\n"
        );
        let intent = fs::read_to_string(&path).unwrap()
            .replace("status=unmet", "status=met verifiedBy=\"greeting-test\" evidence=\"greeting.txt content asserted\"")
            .replace("status=open", "status=done");
        fs::write(&path, intent).unwrap();
        f.run(&["vault", "sync"]);
        f.run(&["intent", "attest", &id]);
        f.run(&["intent", "validate", &id]);
        f.run(&["intent", "update", &id, "--status", "needs-review"]);
        let stop = f.hook(session, "stop");
        assert_eq!(stop["recorded"], true);
        {
            let repo = Repository::open_readonly(f.root()).unwrap();
            let (_, turns) = repo.get_session_ledger(session).unwrap().unwrap();
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].plan_id.as_deref(), Some(uid.as_str()));
            assert!(!turns[0].change_hashes.is_empty());
            let graph = repo
                .load_provenance_graph(&turns[0].provenance_hash)
                .unwrap();
            assert_eq!(graph.plan_id.as_deref(), Some(uid.as_str()));
            assert_eq!(graph.changes_explained, turns[0].change_hashes);
            let report = repo.intent_graph(&id, 1000).unwrap();
            assert!(
                !report.implementing_changes.is_empty(),
                "the projection must see the real binding"
            );
        }
        // A late declaration may not repair an already recorded turn.
        assert!(store.bind_turn_intent(session, Some(1), &uid).is_err());
        f.hook(session, "user-prompt");
        assert!(store.turn_intent_binding(session, 2).unwrap().is_none());
        fs::write(f.root().join("greeting.txt"), "Hello again!\n").unwrap();
        f.hook(session, "stop");
        let repo = Repository::open_readonly(f.root()).unwrap();
        let (_, turns) = repo.get_session_ledger(session).unwrap().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(
            turns[1].plan_id, None,
            "never inherit the prior turn's intent"
        );
    }
}

#[test]
fn binding_rejects_conflicts_stale_turns_and_concurrent_recording() {
    let f = Fixture::new();
    let session = "binding-guards";
    f.hook(session, "session-start");
    f.hook(session, "user-prompt");
    let (id, uid) = f.new_intent();
    let start = [
        "intent",
        "update",
        &id,
        "--status",
        "in_progress",
        "--session-id",
        session,
        "--turn",
        "1",
    ];
    f.run(&start);
    f.run(&start); // Idempotent retry.
    let store = SessionStore::for_repo(f.root()).unwrap();
    assert!(store.bind_turn_intent(session, Some(2), &uid).is_err());
    assert!(store
        .bind_turn_intent(session, Some(1), "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .is_err());
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(
            f.root()
                .join(format!(".atomic/sessions/{session}/turn-end.lock")),
        )
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(store.bind_turn_intent(session, Some(1), &uid).is_err());
    drop(lock);
    assert_eq!(
        store
            .turn_intent_binding(session, 1)
            .unwrap()
            .unwrap()
            .intent_uid,
        uid
    );
    assert!(store.bind_turn_intent("../escape", Some(1), &uid).is_err());
}

#[test]
fn status_and_corecorded_intent_file_do_not_invent_governance() {
    let f = Fixture::new();
    let session = "status-only-session";
    f.hook(session, "session-start");
    f.hook(session, "user-prompt");
    let (id, _) = f.new_intent();
    f.run(&["intent", "update", &id, "--status", "in_progress"]);
    fs::write(f.root().join("greeting.txt"), "Hello!\n").unwrap();
    assert_eq!(f.hook(session, "stop")["recorded"], true);
    let repo = Repository::open_readonly(f.root()).unwrap();
    let (_, turns) = repo.get_session_ledger(session).unwrap().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].plan_id, None);
    assert_eq!(
        repo.load_provenance_graph(&turns[0].provenance_hash)
            .unwrap()
            .plan_id,
        None
    );
}

#[test]
fn managed_work_item_capture_remains_available_without_a_declaration() {
    let f = Fixture::new();
    let session = "managed-intent-session";
    f.hook(session, "session-start");
    f.hook(session, "user-prompt");
    let (_, uid) = f.new_intent();
    let store = SessionStore::for_repo(f.root()).unwrap();
    let mut state = store.load(session).unwrap().unwrap();
    state.managed_run = Some(ManagedRunStamp {
        run_id: "test-managed-run".into(),
        owner_agent: "test".into(),
        owner_session_id: "owner".into(),
        work_item_id: Some(uid.clone()),
    });
    store.save(&state).unwrap();
    assert!(store
        .bind_turn_intent(session, Some(1), "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .is_err());
    fs::write(f.root().join("greeting.txt"), "Hello!\n").unwrap();
    f.hook(session, "stop");
    let repo = Repository::open_readonly(f.root()).unwrap();
    let (_, turns) = repo.get_session_ledger(session).unwrap().unwrap();
    assert_eq!(turns[0].plan_id.as_deref(), Some(uid.as_str()));
    assert!(store.turn_intent_binding(session, 1).unwrap().is_none());
}
