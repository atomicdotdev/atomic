use std::collections::HashSet;
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use atomic_agent::{ProvenanceAccumulator, ProvenanceJournalEnvelope, ProvenanceJournalEvent};
use atomic_core::types::{Base32, Hash};
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{ChangeStore, Repository, DEFAULT_CACHE_CAPACITY};
use serde_json::Value;
use tempfile::TempDir;

// Bound child processes so a platform-specific IPC regression produces a
// useful failure instead of occupying a CI runner indefinitely. Drain output
// concurrently so a full pipe cannot prevent the child from exiting.
fn wait_for_output(mut child: Child, operation: &str) -> Output {
    fn drain<R: Read + Send + 'static>(stream: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                stream.read_to_end(&mut bytes).expect("read child output");
            }
            let _ = sender.send(bytes);
        });
        receiver
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(30);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("inspect child process") {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out child");
            break (child.wait().expect("reap timed-out child"), true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .recv_timeout(Duration::from_secs(2))
        .expect("child stdout stayed open");
    let stderr = stderr
        .recv_timeout(Duration::from_secs(2))
        .expect("child stderr stayed open");
    assert!(
        !timed_out,
        "{operation} timed out after 30s; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    Output {
        status,
        stdout,
        stderr,
    }
}

fn command_output(command: &mut Command) -> Output {
    let operation = format!("{command:?}");
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn command");
    wait_for_output(child, &operation)
}

fn owner_command(repository: &std::path::Path, operation: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .arg("agent")
        .arg("database-owner")
        .arg(operation)
        .arg("--repository")
        .arg(repository)
        .arg("--no-color");
    command
}

fn spawn_owner(repository: &std::path::Path) -> Child {
    owner_command(repository, "serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn foreground database owner")
}

fn spawn_owner_with_failpoint(
    repository: &std::path::Path,
    failpoint: &str,
    marker: &std::path::Path,
) -> Child {
    owner_command(repository, "serve")
        .env("ATOMIC_OWNER_FAILPOINT", failpoint)
        .env("ATOMIC_OWNER_FAILPOINT_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn failpoint database owner")
}

fn run_owner(repository: &std::path::Path, operation: &str) -> Output {
    command_output(owner_command(repository, operation).arg("--json"))
}

fn wait_for_ping(repository: &std::path::Path, child: &mut Child) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = run_owner(repository, "ping");
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).expect("parse ping response");
        }
        if let Some(status) = child.try_wait().expect("inspect owner process") {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .expect("owner stderr")
                .read_to_string(&mut stderr)
                .expect("read owner stderr");
            panic!("database owner exited with {status}: {stderr}");
        }
        assert!(
            Instant::now() < deadline,
            "database owner did not become healthy: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn reserve(repository: &std::path::Path) -> Value {
    let output = command_output(
        owner_command(repository, "reserve")
            .arg("--session-id")
            .arg("owner-service-test")
            .arg("--turn")
            .arg("7")
            .arg("--now")
            .arg("1700000000")
            .arg("--json"),
    );
    assert!(
        output.status.success(),
        "reservation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse reservation response")
}

fn run_lifecycle(repository: &std::path::Path, args: &[&str]) -> Output {
    command_output(
        Command::new(env!("CARGO_BIN_EXE_atomic"))
            .arg("agent")
            .arg("lifecycle")
            .args(args)
            .arg("--no-color")
            .current_dir(repository),
    )
}

fn run_hook(repository: &std::path::Path, verb: &str, payload: &[u8]) -> Output {
    run_agent_hook(repository, "claude-code", verb, payload)
}

fn run_agent_hook(repository: &std::path::Path, agent: &str, verb: &str, payload: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .arg("agent")
        .arg("hooks")
        .arg(agent)
        .arg(verb)
        .arg("--foreground")
        .arg("--no-color")
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook process");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(payload)
        .expect("write hook payload");
    wait_for_output(child, &format!("{agent} {verb}"))
}

#[test]
fn read_only_turn_does_not_reuse_its_goal_for_the_next_turn_across_agents() {
    for (agent, prompt_verb) in [
        ("codex", "user-prompt-submit"),
        ("claude-code", "user-prompt-submit"),
        ("opencode", "user-prompt"),
    ] {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repo");
        drop(Repository::init(&repository).unwrap());
        let hook = |verb: &str, payload: Value| {
            let output = run_agent_hook(
                &repository,
                agent,
                verb,
                &serde_json::to_vec(&payload).unwrap(),
            );
            assert!(
                output.status.success(),
                "{agent} {verb}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        hook(
            "session-start",
            serde_json::json!({"session_id":"read-then-write"}),
        );
        hook(
            prompt_verb,
            serde_json::json!({"session_id":"read-then-write","prompt":"Inspect without edits"}),
        );
        hook(
            "stop",
            serde_json::json!({"session_id":"read-then-write","last_assistant_message":"Inspected","response":"Inspected"}),
        );
        // Retry the same terminal hook before the next user prompt.
        hook(
            "stop",
            serde_json::json!({"session_id":"read-then-write","last_assistant_message":"Inspected","response":"Inspected"}),
        );
        hook(
            prompt_verb,
            serde_json::json!({"session_id":"read-then-write","prompt":"Implement the second request"}),
        );
        std::fs::write(repository.join("second.txt"), b"second request\n").unwrap();
        hook(
            "stop",
            serde_json::json!({"session_id":"read-then-write","last_assistant_message":"Implemented","response":"Implemented"}),
        );
        hook(
            "stop",
            serde_json::json!({"session_id":"read-then-write","last_assistant_message":"Implemented","response":"Implemented"}),
        );
        assert!(run_owner(&repository, "shutdown").status.success());
        wait_for_shutdown(&repository);

        let repo = Repository::open(&repository).unwrap();
        let (_, turns) = repo.get_session_ledger("read-then-write").unwrap().unwrap();
        assert_eq!(
            turns.last().unwrap().goal.as_deref(),
            Some("Implement the second request"),
            "{agent}: a read-only turn must not contaminate the next turn"
        );
        assert_eq!(
            turns.len(),
            2,
            "{agent}: read-only provenance must remain queryable"
        );
        assert!(turns[0].change_hashes.is_empty());
        assert_eq!(turns[0].goal.as_deref(), Some("Inspect without edits"));
        assert_eq!(turns[1].previous_provenance, Some(turns[0].provenance_hash));
        assert_eq!(turns[1].change_hashes.len(), 1);
    }
}

fn wait_for_shutdown(repository: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !run_owner(repository, "ping").status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "database owner did not shut down"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn concurrent_hooks_commit_lossless_envelopes_without_output_or_drops() {
    const TOOL_EVENTS: usize = 16;

    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());

    let session_start = run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"hook-concurrent"}"#,
    );
    assert!(
        session_start.status.success(),
        "session start failed: {}",
        String::from_utf8_lossy(&session_start.stderr)
    );

    let turn_start = run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"hook-concurrent","prompt":"test concurrent journal routing"}"#,
    );
    assert!(
        turn_start.status.success(),
        "turn start failed: {}",
        String::from_utf8_lossy(&turn_start.stderr)
    );

    let reservation = command_output(owner_command(&repository, "reserve").args([
        "--session-id",
        "hook-concurrent",
        "--turn",
        "1",
        "--now",
        "1700000000",
        "--json",
    ]));
    assert!(reservation.status.success());
    let running_generation = serde_json::from_slice::<Value>(&reservation.stdout).unwrap()["turn"]
        ["generation"]
        .as_u64()
        .unwrap();

    let append_started = Instant::now();
    let mut workers = Vec::new();
    for index in 0..TOOL_EVENTS {
        let repository = repository.clone();
        workers.push(thread::spawn(move || {
            let payload = format!(
                "{{\"session_id\":\"hook-concurrent\",\"tool_use_id\":\"tool-{index}\",\"tool_name\":\"Read\",\"tool_input\":{{\"path\":\"src/{index}.rs\"}},\"tool_output\":\"ok\",\"status\":\"completed\"}}"
            );
            run_hook(&repository, "post-tool", payload.as_bytes())
        }));
    }
    for worker in workers {
        let output = worker.join().expect("join hook worker");
        assert!(
            output.status.success(),
            "tool hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "successful tool hook wrote stdout"
        );
        assert!(
            output.stderr.is_empty(),
            "successful tool hook wrote stderr"
        );
    }
    let append_elapsed = append_started.elapsed();
    assert!(
        append_elapsed / (TOOL_EVENTS as u32) < Duration::from_secs(1),
        "average hook append latency exceeded 1s: {:?}",
        append_elapsed / (TOOL_EVENTS as u32)
    );

    let retry = run_hook(
        &repository,
        "post-tool",
        br#"{"session_id":"hook-concurrent","tool_use_id":"tool-0","tool_name":"Read","tool_input":{"path":"src/0.rs"},"tool_output":"ok","status":"completed"}"#,
    );
    assert!(retry.status.success());
    assert!(retry.stdout.is_empty());
    assert!(retry.stderr.is_empty());

    let stop = run_hook(
        &repository,
        "stop",
        br#"{"session_id":"hook-concurrent","reason":"end_turn","reasoning_blocks":[{"text":"checked all concurrent events","duration_ms":9,"signature":"sig"}],"response":"all events committed","todos":[{"id":"todo-1","content":"verify journal","status":"completed","priority":"high"}]}"#,
    );
    assert!(
        stop.status.success(),
        "stop hook failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stop.stdout.is_empty());
    assert!(stop.stderr.is_empty());

    let shutdown = run_owner(&repository, "shutdown");
    assert!(shutdown.status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));

    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let turn = store
        .get_provenance_turn_for("hook-concurrent", 1)
        .unwrap()
        .expect("reserved hook turn");
    let stored = store.load_provenance_envelopes(turn.provenance_id).unwrap();
    assert_eq!(stored.len(), TOOL_EVENTS + 5);
    assert_eq!(
        stored.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (0..(TOOL_EVENTS as u64 + 5)).collect::<Vec<_>>()
    );
    let ids: HashSet<_> = stored.iter().map(|event| &event.event_id).collect();
    assert_eq!(ids.len(), TOOL_EVENTS + 5);
    let mut saw_reasoning = false;
    let mut saw_response = false;
    let mut saw_todo = false;
    let mut saw_terminal = false;
    for event in stored {
        let envelope = ProvenanceJournalEnvelope::from_json_bytes(&event.envelope).unwrap();
        saw_reasoning |= matches!(&envelope.event, ProvenanceJournalEvent::Reasoning { .. });
        saw_response |= matches!(&envelope.event, ProvenanceJournalEvent::Response { .. });
        saw_todo |= matches!(&envelope.event, ProvenanceJournalEvent::Todo { .. });
        saw_terminal |= matches!(&envelope.event, ProvenanceJournalEvent::Terminal { .. });
        assert_eq!(envelope.session_id, "hook-concurrent");
        assert_eq!(envelope.turn_number, 1);
        // Finalization fences writers by advancing the stored generation;
        // immutable events retain the generation acknowledged during append.
        assert_eq!(envelope.generation, running_generation);
    }
    assert!(saw_reasoning && saw_response && saw_todo && saw_terminal);
    assert!(matches!(
        turn.state,
        atomic_repository::redb_change_store::ProvenanceTurnState::Completed
    ));
}

#[test]
fn lifecycle_stop_resume_zombie_abandon_and_lease_expiry() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    let workdir = repository.to_string_lossy().to_string();

    let begin = run_lifecycle(
        &repository,
        &[
            "begin",
            "--owner",
            "sherpa",
            "--session",
            "owner-session",
            "--executor",
            "claude-code",
            "--workdir",
            &workdir,
            "--ttl-seconds",
            "60",
            "--json",
        ],
    );
    assert!(
        begin.status.success(),
        "{}",
        String::from_utf8_lossy(&begin.stderr)
    );
    let lifecycle: Value = serde_json::from_slice(&begin.stdout).unwrap();
    let run_id = lifecycle["run_id"].as_str().unwrap().to_string();

    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"lifecycle-session"}"#,
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"lifecycle-session","prompt":"resume me"}"#,
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "post-tool",
        br#"{"session_id":"lifecycle-session","tool_use_id":"before-stop","tool_name":"Read","tool_input":{"path":"a.rs"},"tool_output":"ok"}"#,
    )
    .status
    .success());

    let stopped = run_lifecycle(
        &repository,
        &[
            "stop",
            "--run-id",
            &run_id,
            "--cause",
            "process-exited",
            "--json",
        ],
    );
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let stopped_json: Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(stopped_json["stop_state"]["cause"], "process-exited");
    assert_eq!(stopped_json["stop_state"]["resumable"], true);

    let status = run_lifecycle(&repository, &["status", "--json"]);
    assert!(status.status.success());
    let status_json: Value = serde_json::from_slice(&status.stdout).unwrap();
    let stopped_turn = status_json["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["session_id"] == "lifecycle-session")
        .unwrap();
    let provenance_id = stopped_turn["provenance_id"].as_u64().unwrap();
    assert_eq!(stopped_turn["state"], "stopped");
    assert_eq!(stopped_turn["generation"], 2);
    assert_eq!(stopped_turn["last_event_seq"], 1);
    assert_eq!(stopped_turn["resumable"], true);

    let zombie = run_hook(
        &repository,
        "post-tool",
        br#"{"session_id":"lifecycle-session","tool_use_id":"zombie","tool_name":"Read","tool_input":{"path":"zombie.rs"},"tool_output":"late"}"#,
    );
    assert!(
        !zombie.status.success(),
        "stopped writer unexpectedly appended"
    );

    let resumed = run_lifecycle(
        &repository,
        &[
            "resume",
            "--run-id",
            &run_id,
            "--ttl-seconds",
            "60",
            "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(run_hook(
        &repository,
        "post-tool",
        br#"{"session_id":"lifecycle-session","tool_use_id":"after-resume","tool_name":"Read","tool_input":{"path":"b.rs"},"tool_output":"ok"}"#,
    )
    .status
    .success());

    let abandoned = run_lifecycle(&repository, &["abandon", "--run-id", &run_id, "--json"]);
    assert!(abandoned.status.success());
    let abandoned_json: Value = serde_json::from_slice(&abandoned.stdout).unwrap();
    assert_eq!(abandoned_json["stop_state"]["cause"], "abandoned");
    assert_eq!(abandoned_json["stop_state"]["resumable"], false);
    assert!(
        !run_lifecycle(&repository, &["resume", "--run-id", &run_id, "--json"])
            .status
            .success()
    );

    let lease_begin = run_lifecycle(
        &repository,
        &[
            "begin",
            "--owner",
            "sherpa",
            "--session",
            "lease-owner",
            "--executor",
            "claude-code",
            "--workdir",
            &workdir,
            "--ttl-seconds",
            "60",
            "--json",
        ],
    );
    assert!(lease_begin.status.success());
    let lease_lifecycle: Value = serde_json::from_slice(&lease_begin.stdout).unwrap();
    let lease_run_id = lease_lifecycle["run_id"].as_str().unwrap();
    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"lease-session"}"#,
    )
    .status
    .success());
    let lease_turn_start = run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"lease-session","prompt":"expire me"}"#,
    );
    assert!(
        lease_turn_start.status.success(),
        "lease turn start failed: {}",
        String::from_utf8_lossy(&lease_turn_start.stderr)
    );
    assert!(run_lifecycle(
        &repository,
        &[
            "renew",
            "--run-id",
            lease_run_id,
            "--ttl-seconds",
            "1",
            "--json",
        ],
    )
    .status
    .success());
    thread::sleep(Duration::from_millis(1_100));
    let lease_status = run_lifecycle(&repository, &["status", "--json"]);
    assert!(lease_status.status.success());
    let lease_json: Value = serde_json::from_slice(&lease_status.stdout).unwrap();
    assert!(lease_json["stale_lifecycles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|lifecycle| lifecycle["stop_state"]["cause"] == "lease-expired"));

    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));
    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let abandoned_turn = store
        .get_provenance_turn_for("lifecycle-session", 1)
        .unwrap()
        .unwrap();
    assert_eq!(abandoned_turn.provenance_id.get(), provenance_id);
    assert_eq!(abandoned_turn.generation, 4);
    match abandoned_turn.state {
        atomic_repository::redb_change_store::ProvenanceTurnState::Abandoned(stop) => {
            assert_eq!(stop.last_event_seq, Some(2));
            assert!(!stop.resumable);
        }
        other => panic!("expected abandoned turn, got {other:?}"),
    }
    let lease_turn = store
        .get_provenance_turn_for("lease-session", 1)
        .unwrap()
        .unwrap();
    match lease_turn.state {
        atomic_repository::redb_change_store::ProvenanceTurnState::Stopped(stop) => {
            assert_eq!(
                stop.cause,
                atomic_repository::redb_change_store::StopCause::LeaseExpired
            );
            assert!(stop.resumable);
        }
        other => panic!("expected lease-expired stop, got {other:?}"),
    }
}

#[test]
fn owner_death_before_and_after_event_commit_retries_exactly_once() {
    for failpoint in ["before-envelope-commit", "after-envelope-commit"] {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repo");
        drop(Repository::init(&repository).unwrap());
        assert!(run_hook(
            &repository,
            "session-start",
            br#"{"session_id":"event-crash"}"#,
        )
        .status
        .success());

        let marker = temp.path().join(format!("{failpoint}.marker"));
        let mut owner = spawn_owner_with_failpoint(&repository, failpoint, &marker);
        wait_for_ping(&repository, &mut owner);
        let started = Instant::now();
        let hook = run_hook(
            &repository,
            "user-prompt-submit",
            br#"{"session_id":"event-crash","prompt":"survive owner crash"}"#,
        );
        assert!(
            hook.status.success(),
            "{failpoint} retry failed: {}",
            String::from_utf8_lossy(&hook.stderr)
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!owner.wait().unwrap().success());

        assert!(run_owner(&repository, "shutdown").status.success());
        wait_for_shutdown(&repository);
        thread::sleep(Duration::from_millis(100));
        let store =
            RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
                .unwrap();
        let turn = store
            .get_provenance_turn_for("event-crash", 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .load_provenance_envelopes(turn.provenance_id)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn owner_death_after_checkpoint_prepare_and_bind_recovers_in_hook_process() {
    for failpoint in ["after-checkpoint-prepare", "after-checkpoint-bind"] {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repo");
        drop(Repository::init(&repository).unwrap());
        assert!(run_hook(
            &repository,
            "session-start",
            br#"{"session_id":"checkpoint-crash"}"#,
        )
        .status
        .success());
        assert!(run_hook(
            &repository,
            "user-prompt-submit",
            br#"{"session_id":"checkpoint-crash","prompt":"checkpoint despite crash"}"#,
        )
        .status
        .success());
        assert!(run_owner(&repository, "shutdown").status.success());
        wait_for_shutdown(&repository);
        thread::sleep(Duration::from_millis(100));
        std::fs::write(repository.join("crash.txt"), b"recover\n").unwrap();

        let marker = temp.path().join(format!("{failpoint}.marker"));
        let mut owner = spawn_owner_with_failpoint(&repository, failpoint, &marker);
        wait_for_ping(&repository, &mut owner);
        let started = Instant::now();
        let stop = run_hook(
            &repository,
            "stop",
            br#"{"session_id":"checkpoint-crash","response":"recovered"}"#,
        );
        assert!(
            stop.status.success(),
            "{failpoint} retry failed: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(!owner.wait().unwrap().success());

        assert!(run_owner(&repository, "shutdown").status.success());
        wait_for_shutdown(&repository);
        thread::sleep(Duration::from_millis(100));
        let store =
            RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
                .unwrap();
        let turn = store
            .get_provenance_turn_for("checkpoint-crash", 1)
            .unwrap()
            .unwrap();
        assert!(matches!(
            turn.state,
            atomic_repository::redb_change_store::ProvenanceTurnState::Completed
        ));
        drop(store);
        let repo = Repository::open(&repository).unwrap();
        let (_, turns) = repo
            .get_session_ledger("checkpoint-crash")
            .unwrap()
            .unwrap();
        assert_eq!(turns.len(), 1);
    }
}

#[test]
fn corrupt_legacy_graph_is_retained_when_migration_cannot_verify_it() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    let repo = Repository::init(&repository).unwrap();
    let session_dir = repo.dot_dir().join("sessions/corrupt-legacy");
    std::fs::create_dir_all(&session_dir).unwrap();
    let graph_path = ProvenanceAccumulator::graph_path(&session_dir);
    std::fs::write(&graph_path, b"not valid graph json").unwrap();
    std::fs::write(session_dir.join("graph.lock"), b"legacy lock").unwrap();
    drop(repo);

    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"corrupt-legacy"}"#,
    )
    .status
    .success());
    let migration = run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"corrupt-legacy","prompt":"do not discard"}"#,
    );
    assert!(!migration.status.success());
    assert!(graph_path.exists());
    assert!(session_dir.join("graph.lock").exists());

    // TurnStart probes the owner before migration, so clean it up explicitly.
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
}

#[test]
fn legacy_graph_pending_delta_imports_once_then_json_authority_is_removed() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    let repo = Repository::init(&repository).unwrap();
    let session_dir = repo.dot_dir().join("sessions/legacy-session");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut legacy = ProvenanceAccumulator::new("legacy-session");
    legacy.append_goal("already finalized", 1_000);
    let historical_graph = legacy.to_provenance_graph(
        "claude-code",
        "Claude Code",
        "anthropic",
        &[Hash::of(b"historical-change")],
    );
    let change_store = ChangeStore::new(repo.changes_dir(), DEFAULT_CACHE_CAPACITY).unwrap();
    let historical_hash = change_store
        .save_provenance_graph(&historical_graph)
        .unwrap();
    legacy.set_last_provenance_hash(historical_hash.to_base32());
    legacy.append_tool_call(
        "read",
        Some("legacy-read"),
        Some(&serde_json::json!({"path": "src/legacy.rs"})),
        Some("contents"),
        Some("completed"),
        Some(4),
        2_000,
    );
    legacy.save(&session_dir).unwrap();
    std::fs::write(session_dir.join("graph.lock"), b"legacy lock").unwrap();
    drop(repo);

    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"legacy-session"}"#,
    )
    .status
    .success());
    let first = run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"legacy-session","prompt":"continue legacy work"}"#,
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!ProvenanceAccumulator::graph_path(&session_dir).exists());
    assert!(!session_dir.join("graph.lock").exists());

    // Simulate a crash after owner acknowledgement but before obsolete-file
    // deletion by restoring the exact legacy cache. Retry must not duplicate it.
    legacy.save(&session_dir).unwrap();
    std::fs::write(session_dir.join("graph.lock"), b"legacy lock").unwrap();
    let retry = run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"legacy-session","prompt":"continue legacy work"}"#,
    );
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(!ProvenanceAccumulator::graph_path(&session_dir).exists());
    assert!(!session_dir.join("graph.lock").exists());

    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));
    let redb = RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
        .unwrap();
    let turn = redb
        .get_provenance_turn_for("legacy-session", 1)
        .unwrap()
        .unwrap();
    let stored = redb.load_provenance_envelopes(turn.provenance_id).unwrap();
    assert_eq!(
        stored.len(),
        2,
        "migration marker and goal must each commit once"
    );
    let envelopes = stored
        .iter()
        .map(|stored| ProvenanceJournalEnvelope::from_json_bytes(&stored.envelope).unwrap())
        .collect::<Vec<_>>();
    let legacy_import = envelopes
        .iter()
        .find_map(|envelope| match &envelope.event {
            ProvenanceJournalEvent::LegacyGraphImport {
                nodes,
                previous_provenance,
                ..
            } => Some((nodes, previous_provenance)),
            _ => None,
        })
        .expect("legacy import marker");
    assert_eq!(
        legacy_import.0.len(),
        1,
        "only the unsaved suffix is imported"
    );
    assert_eq!(
        legacy_import.0[0].tool_call_id.as_deref(),
        Some("legacy-read")
    );
    let historical_base32 = historical_hash.to_base32();
    assert_eq!(legacy_import.1.as_deref(), Some(historical_base32.as_str()));
    assert_eq!(
        change_store
            .load_provenance_graph(&historical_hash)
            .unwrap()
            .session_id,
        "legacy-session",
        "immutable finalized history remains readable"
    );
}

#[test]
fn turn_end_publishes_one_checkpoint_turn_and_advances_head() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());

    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"checkpoint-e2e"}"#,
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"checkpoint-e2e","prompt":"record checkpoint"}"#,
    )
    .status
    .success());
    std::fs::write(repository.join("checkpoint.txt"), b"checkpointed\n").unwrap();
    let checkpoint_started = Instant::now();
    let stop = run_hook(
        &repository,
        "stop",
        br#"{"session_id":"checkpoint-e2e","reason":"end_turn","response":"checkpoint complete"}"#,
    );
    assert!(
        stop.status.success(),
        "turn end failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stop.stdout.is_empty());
    assert!(stop.stderr.is_empty());
    assert!(
        checkpoint_started.elapsed() < Duration::from_secs(10),
        "checkpoint latency exceeded 10s: {:?}",
        checkpoint_started.elapsed()
    );

    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));

    let redb = RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
        .unwrap();
    let completed = redb
        .get_provenance_turn_for("checkpoint-e2e", 1)
        .unwrap()
        .unwrap();
    assert!(matches!(
        completed.state,
        atomic_repository::redb_change_store::ProvenanceTurnState::Completed
    ));
    let attempt = completed.checkpoint_attempt.unwrap();
    assert_eq!(
        attempt.phase,
        atomic_repository::redb_change_store::ProvenanceCheckpointPhase::Published
    );
    let provenance_hash = attempt.provenance_hash.unwrap();
    let manifest_hash = attempt.manifest_hash.unwrap();
    drop(redb);

    let repo = Repository::open(&repository).unwrap();
    let (_, turns) = repo.get_session_ledger("checkpoint-e2e").unwrap().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].provenance_hash, provenance_hash);
    assert_eq!(
        repo.get_session_head("checkpoint-e2e").unwrap(),
        Some(manifest_hash)
    );
    let graph_dir = repo.dot_dir().join("sessions/checkpoint-e2e");
    assert!(!ProvenanceAccumulator::graph_path(&graph_dir).exists());
    assert!(!graph_dir.join("graph.lock").exists());
    assert_eq!(
        repo.load_provenance_graph(&provenance_hash)
            .unwrap()
            .session_id,
        "checkpoint-e2e"
    );
}

#[test]
fn pre_cutover_session_count_gap_publishes_at_next_ledger_ordinal() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"legacy-gap"}"#,
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"legacy-gap","prompt":"publish after legacy turns"}"#,
    )
    .status
    .success());

    let session_path = repository.join(".atomic/sessions/legacy-gap.json");
    let mut session: Value =
        serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
    session["turn_count"] = Value::from(20);
    std::fs::write(&session_path, serde_json::to_vec_pretty(&session).unwrap()).unwrap();
    std::fs::write(repository.join("legacy-gap.txt"), b"record me\n").unwrap();

    let stop = run_hook(
        &repository,
        "stop",
        br#"{"session_id":"legacy-gap","response":"published"}"#,
    );
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));

    let repo = Repository::open(&repository).unwrap();
    let (_, turns) = repo.get_session_ledger("legacy-gap").unwrap().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].turn_number, 0);
}

#[test]
fn crashing_second_checkpoint_keeps_first_turn_immutable() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"immutable-turns"}"#,
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"immutable-turns","prompt":"first"}"#,
    )
    .status
    .success());
    std::fs::write(repository.join("first.txt"), b"first\n").unwrap();
    assert!(run_hook(
        &repository,
        "stop",
        br#"{"session_id":"immutable-turns","response":"first done"}"#,
    )
    .status
    .success());
    let first_hash = Repository::open(&repository)
        .unwrap()
        .get_session_ledger("immutable-turns")
        .unwrap()
        .unwrap()
        .1[0]
        .provenance_hash;

    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"immutable-turns","prompt":"second"}"#,
    )
    .status
    .success());
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));
    std::fs::write(repository.join("second.txt"), b"second\n").unwrap();
    let marker = temp.path().join("second-bind.marker");
    let mut owner = spawn_owner_with_failpoint(&repository, "after-checkpoint-bind", &marker);
    wait_for_ping(&repository, &mut owner);
    let second = run_hook(
        &repository,
        "stop",
        br#"{"session_id":"immutable-turns","response":"second done"}"#,
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(!owner.wait().unwrap().success());
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    thread::sleep(Duration::from_millis(100));

    let repo = Repository::open(&repository).unwrap();
    let (_, turns) = repo.get_session_ledger("immutable-turns").unwrap().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].provenance_hash, first_hash);
    assert_ne!(turns[1].provenance_hash, first_hash);
}

#[test]
fn owner_election_commit_reconnect_and_crash_restart() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());

    let mut first_owner = spawn_owner(&repository);
    let first_health = wait_for_ping(&repository, &mut first_owner);
    let first_pid = first_health["pid"].as_u64().unwrap();
    assert_eq!(first_health["protocol_version"], 1);

    let duplicate = command_output(&mut owner_command(&repository, "serve"));
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("another database owner"));

    let first_reservation = reserve(&repository);
    assert_eq!(first_reservation["committed"], true);
    let provenance_id = first_reservation["turn"]["provenance_id"].clone();

    first_owner.kill().expect("kill first owner");
    first_owner.wait().expect("reap first owner");

    let first_bootstrap = owner_command(&repository, "start")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first bootstrap contender");
    let second_bootstrap = owner_command(&repository, "start")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second bootstrap contender");
    let first_bootstrap = wait_for_output(first_bootstrap, "first bootstrap contender");
    let second_bootstrap = wait_for_output(second_bootstrap, "second bootstrap contender");
    assert!(first_bootstrap.status.success());
    assert!(second_bootstrap.status.success());
    let restarted_health: Value = serde_json::from_slice(&first_bootstrap.stdout).unwrap();
    let concurrent_health: Value = serde_json::from_slice(&second_bootstrap.stdout).unwrap();
    assert_ne!(restarted_health["pid"].as_u64().unwrap(), first_pid);
    assert_eq!(concurrent_health["pid"], restarted_health["pid"]);

    let replayed = reserve(&repository);
    assert_eq!(replayed["committed"], true);
    assert_eq!(replayed["turn"]["provenance_id"], provenance_id);

    let reconnected = run_owner(&repository, "start");
    assert!(reconnected.status.success());
    let reconnected_health: Value = serde_json::from_slice(&reconnected.stdout).unwrap();
    assert_eq!(reconnected_health["pid"], restarted_health["pid"]);

    let shutdown = run_owner(&repository, "shutdown");
    assert!(
        shutdown.status.success(),
        "shutdown failed: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    wait_for_shutdown(&repository);
}
