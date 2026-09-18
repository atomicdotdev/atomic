use std::collections::HashSet;
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use atomic_agent::{ProvenanceAccumulator, ProvenanceJournalEnvelope, ProvenanceJournalEvent};
use atomic_core::types::{Base32, Hash};
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{ChangeStore, Repository, DEFAULT_CACHE_CAPACITY};
use fs2::FileExt;
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
        .unwrap_or_else(|error| {
            panic!("{operation}: child exited with {status}, but stdout stayed open: {error}")
        });
    let stderr = stderr
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|error| {
            panic!("{operation}: child exited with {status}, but stderr stayed open: {error}")
        });
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
fn concurrent_session_starts_wait_for_writer_and_persist_views_and_lifecycle() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    let writer = Repository::init(&repository).unwrap();
    let pending: Vec<_> = (0..8)
        .map(|index| {
            let root = repository.clone();
            thread::spawn(move || {
                let session = format!("start-contended-{index}");
                let payload = serde_json::to_vec(&serde_json::json!({
                    "session_id": session, "cwd": root,
                }))
                .unwrap();
                run_agent_hook(&root, "opencode", "session-start", &payload)
            })
        })
        .collect();
    // Hold a real incompatible handle across all eight independent processes.
    // The hook must wait internally, without a test-side retry or startup queue.
    thread::sleep(Duration::from_millis(300));
    let returned_early = pending.iter().any(thread::JoinHandle::is_finished);
    drop(writer);
    let outputs: Vec<_> = pending.into_iter().map(|p| p.join().unwrap()).collect();
    assert!(
        !returned_early,
        "session start skipped a contended database"
    );
    let repo = Repository::open_existing(&repository).unwrap();
    let mut views = HashSet::new();
    for (index, output) in outputs.iter().enumerate() {
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Cannot acquire lock"), "{stderr}");
        let sid = format!("start-contended-{index}");
        let session: Value = serde_json::from_slice(
            &std::fs::read(repository.join(format!(".atomic/sessions/{sid}.json"))).unwrap(),
        )
        .unwrap();
        let view = session["view_name"].as_str().unwrap();
        assert!(
            views.insert(view.to_string()),
            "sessions must have distinct views"
        );
        assert!(repo.get_view_info(view).is_ok(), "view {view} missing");
        assert!(
            repo.get_session_ledger(&sid).unwrap().is_some(),
            "lifecycle missing for {sid}"
        );
    }
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
    // A failed ping only proves the endpoint is unavailable. The runtime may
    // still be draining tasks and closing redb, especially on Windows. The
    // owner releases its election lock only after that cleanup is complete.
    let path = Repository::canonical_dot_dir(repository)
        .unwrap()
        .join("changes-owner.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&lock).unwrap();
                return;
            }
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => panic!("failed to inspect database-owner shutdown: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "database owner did not release its lock after shutdown"
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

#[cfg(unix)]
fn batch_owner_rpc(repository: &std::path::Path, request: &Value) -> std::io::Result<Value> {
    let dot = std::fs::canonicalize(Repository::canonical_dot_dir(repository).unwrap())?;
    let digest = blake3::hash(dot.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    let mut stream = std::os::unix::net::UnixStream::connect(format!(
        "/tmp/atomic-owner-{}.sock",
        &digest[..24]
    ))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let frame = serde_json::to_vec(
        &serde_json::json!({"version":1,"request_id":"batch-regression","request":request}),
    )?;
    stream.write_all(&(frame.len() as u32).to_be_bytes())?;
    stream.write_all(&frame)?;
    let mut len = [0; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    assert!(len <= 8 * 1024 * 1024);
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(unix)]
#[test]
fn owner_batch_crash_retries_the_whole_durable_batch_exactly_once() {
    for failpoint in ["before-envelope-commit", "after-envelope-commit"] {
        let temp = TempDir::new().unwrap();
        let repository = temp.path().join("repo");
        drop(Repository::init(&repository).unwrap());
        let marker = temp.path().join("batch-crash.marker");
        let mut owner = spawn_owner_with_failpoint(&repository, failpoint, &marker);
        wait_for_ping(&repository, &mut owner);
        let reserved = reserve(&repository);
        let id = reserved["turn"]["provenance_id"].as_u64().unwrap();
        let generation = reserved["turn"]["generation"].as_u64().unwrap();
        let envelopes: Vec<_> = (0..16)
            .map(|i| serde_json::json!({"event_id":format!("batch-{i}"),"bytes":[i]}))
            .collect();
        let request = serde_json::json!({"AppendProvenanceEnvelopes":{
            "provenance_id":id,"expected_generation":generation,"envelopes":envelopes,"now":1700000001
        }});
        assert!(batch_owner_rpc(&repository, &request).is_err());
        assert!(!wait_for_output(owner, "batch failpoint owner")
            .status
            .success());
        let path = Repository::canonical_change_store_path(&repository).unwrap();
        let store = RedbChangeStore::open(&path).unwrap();
        let events = store
            .load_provenance_envelopes(atomic_repository::redb_change_store::ProvenanceId::new(id))
            .unwrap();
        assert_eq!(
            events.len(),
            if failpoint == "before-envelope-commit" {
                0
            } else {
                16
            }
        );
        drop(store);

        let mut owner = spawn_owner(&repository);
        wait_for_ping(&repository, &mut owner);
        let retry = batch_owner_rpc(&repository, &request).unwrap();
        let acks = retry["response"]["ProvenanceEnvelopesCommitted"]["acknowledgements"]
            .as_array()
            .unwrap();
        assert_eq!(acks.len(), 16);
        for (i, ack) in acks.iter().enumerate() {
            assert_eq!(ack["event_id"], format!("batch-{i}"));
            assert_eq!(ack["sequence"], i as u64);
        }
        assert_eq!(batch_owner_rpc(&repository, &request).unwrap(), retry);
        assert!(run_owner(&repository, "shutdown").status.success());
        assert!(wait_for_output(owner, "batch retry owner").status.success());
        let store = RedbChangeStore::open(path).unwrap();
        let events = store
            .load_provenance_envelopes(atomic_repository::redb_change_store::ProvenanceId::new(id))
            .unwrap();
        assert_eq!(events.len(), 16);
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.envelope, vec![i as u8]);
        }
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
fn concurrent_stops_publish_ordered_ledgers_without_external_serialization() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    let begin = run_lifecycle(
        &repository,
        &[
            "begin",
            "--owner",
            "test",
            "--session",
            "coordinator",
            "--executor",
            "claude-code",
            "--view",
            "dev",
            "--json",
        ],
    );
    assert!(
        begin.status.success(),
        "{}",
        String::from_utf8_lossy(&begin.stderr)
    );
    let sessions: Vec<_> = (0..8).map(|i| format!("concurrent-stop-{i}")).collect();
    for session in &sessions {
        let output = run_hook(
            &repository,
            "session-start",
            &serde_json::to_vec(&serde_json::json!({"session_id":session})).unwrap(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for round in 0..2 {
        for session in &sessions {
            let output = run_hook(
                &repository,
                "user-prompt-submit",
                &serde_json::to_vec(
                    &serde_json::json!({"session_id":session,"prompt":format!("round {round}")}),
                )
                .unwrap(),
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // First round includes an actual change. The second is read-only.
        if round == 0 {
            std::fs::write(repository.join("shared.txt"), "one shared change\n").unwrap();
        }
        // Force a transient *database* conflict in addition to simultaneous
        // Stops. No test-side Stop lock or retry hides production failures.
        let held_repository = Repository::open_existing(&repository).unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(sessions.len() + 1));
        let mut children = Vec::new();
        for session in &sessions {
            let root = repository.clone();
            let session = session.clone();
            let gate = gate.clone();
            children.push(thread::spawn(move || {
                gate.wait();
                run_hook(
                    &root,
                    "stop",
                    &serde_json::to_vec(&serde_json::json!({
                        "session_id":session,"response":format!("finished {round}")
                    }))
                    .unwrap(),
                )
            }));
        }
        gate.wait();
        thread::sleep(Duration::from_millis(200));
        drop(held_repository);
        for child in children {
            let output = child.join().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    for session in &sessions {
        let output = run_hook(
            &repository,
            "stop",
            &serde_json::to_vec(&serde_json::json!({"session_id":session})).unwrap(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let repo = Repository::open_existing(&repository).unwrap();
    let mut recorded = HashSet::new();
    for session in &sessions {
        let (_, turns) = repo.get_session_ledger(session).unwrap().unwrap();
        assert_eq!(turns.len(), 2, "{session}: duplicate or missing checkpoint");
        assert_eq!(turns[1].previous_provenance, Some(turns[0].provenance_hash));
        for (round, turn) in turns.iter().enumerate() {
            assert_eq!(turn.turn_number, round as u32);
            assert_eq!(
                turn.goal.as_deref(),
                Some(format!("round {round}").as_str())
            );
            let graph = repo.load_provenance_graph(&turn.provenance_hash).unwrap();
            assert_eq!(graph.previous, turn.previous_provenance);
            assert_eq!(graph.session_id, *session);
            recorded.extend(turn.change_hashes.iter().copied());
        }
    }
    assert_eq!(recorded.len(), 1, "one file edit must be recorded once");
}

#[test]
fn stop_publication_timeout_preserves_the_active_turn_for_retry() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    let payload = br#"{"session_id":"publication-timeout","prompt":"keep this turn"}"#;
    assert!(run_hook(&repository, "session-start", payload)
        .status
        .success());
    assert!(run_hook(&repository, "user-prompt-submit", payload)
        .status
        .success());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(repository.join(".atomic/turn-publication.lock"))
        .unwrap();
    file.lock_exclusive().unwrap();
    let output = run_hook(&repository, "stop", payload);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out waiting"));
    drop(file);
    for _ in 0..2 {
        let output = run_hook(&repository, "stop", payload);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let repo = Repository::open_existing(&repository).unwrap();
    let (_, turns) = repo
        .get_session_ledger("publication-timeout")
        .unwrap()
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].goal.as_deref(), Some("keep this turn"));
}

#[test]
fn killed_stop_releases_publication_lock_and_can_be_retried() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    let payload = br#"{"session_id":"killed-stop","prompt":"recover my change"}"#;
    assert!(run_hook(&repository, "session-start", payload)
        .status
        .success());
    assert!(run_hook(&repository, "user-prompt-submit", payload)
        .status
        .success());
    std::fs::write(repository.join("recover.txt"), "durable after retry\n").unwrap();
    // Hold pristine so the real Stop has acquired publication coordination
    // but cannot record yet. Kill that process, not a synthetic lock helper.
    let held = Repository::open_existing(&repository).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(["agent", "hooks", "claude-code", "stop", "--foreground"])
        .current_dir(&repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .open(repository.join(".atomic/turn-publication.lock"))
        {
            if let Err(error) = file.try_lock_exclusive() {
                assert_eq!(
                    error.raw_os_error(),
                    fs2::lock_contended_error().raw_os_error()
                );
                break;
            }
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "Stop exited before acquiring its lock"
        );
        assert!(
            Instant::now() < deadline,
            "Stop never acquired publication lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    drop(held);
    for _ in 0..2 {
        let output = run_hook(&repository, "stop", payload);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let repo = Repository::open_existing(&repository).unwrap();
    let (_, turns) = repo.get_session_ledger("killed-stop").unwrap().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].change_hashes.len(), 1);
    assert_eq!(turns[0].goal.as_deref(), Some("recover my change"));
}

fn large_checkpoint_fixture(
    agent: &str,
    count: usize,
    output_bytes: usize,
    crash_between_pages: bool,
) {
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
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr.is_empty(),
            "{agent} {verb}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    hook(
        "session-start",
        serde_json::json!({"session_id":"large-checkpoint"}),
    );
    hook(
        if agent == "opencode" {
            "user-prompt"
        } else {
            "user-prompt-submit"
        },
        serde_json::json!({"session_id":"large-checkpoint","prompt":"Record a large journal"}),
    );
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);

    // Seed valid, durably committed envelopes using the same store API as the
    // owner. Avoid 1024 CLI bootstraps; the actual Stop still runs in a process.
    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let turn = store
        .get_provenance_turn_for("large-checkpoint", 1)
        .unwrap()
        .unwrap();
    let mut expected = Vec::new();
    for index in 0..count {
        let event_id = format!("large-{index}");
        let output = "x".repeat(output_bytes);
        let value = serde_json::json!({
            "schema_version":1,"event_id":event_id,"session_id":"large-checkpoint",
            "turn_number":1,"generation":turn.generation,"timestamp_ms":1700000000000_i64 + index as i64,
            "event":{"type":"tool","phase":"after","tool_name":"Read","tool_call_id":event_id,
                "input":{"path":format!("src/{index}.rs")},"output":output,"status":"completed",
                "raw":{"tool_output":output}}
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        ProvenanceJournalEnvelope::from_json_bytes(&bytes).unwrap();
        store
            .append_provenance_envelope(turn.provenance_id, turn.generation, &event_id, &bytes, 2)
            .unwrap();
        expected.push(bytes);
    }
    if count == 1024 {
        assert!(serde_json::to_vec(&expected).unwrap().len() > 8 * 1024 * 1024);
    } else {
        assert!(
            expected[0].len() > 1024 * 1024,
            "single envelope must span pages"
        );
    }
    drop(store);
    let marker = temp.path().join("page-retry.marker");
    let mut crashing_owner = if crash_between_pages {
        let mut owner =
            spawn_owner_with_failpoint(&repository, "before-frozen-page-continuation", &marker);
        wait_for_ping(&repository, &mut owner);
        Some(owner)
    } else {
        None
    };
    std::fs::write(repository.join("large.txt"), b"large checkpoint\n").unwrap();
    let stop = serde_json::json!({"session_id":"large-checkpoint","reason":"end_turn",
        "response":"Completed","last_assistant_message":"Completed"});
    hook("stop", stop.clone());
    if let Some(owner) = &mut crashing_owner {
        assert!(marker.exists(), "must crash after the first page was read");
        assert!(!owner.wait().unwrap().success());
    }
    let repo = Repository::open(&repository).unwrap();
    let (_, ledger) = repo
        .get_session_ledger("large-checkpoint")
        .unwrap()
        .unwrap();
    assert_eq!(ledger.len(), 1);
    let head = repo.get_session_head("large-checkpoint").unwrap();
    drop(repo);
    hook("stop", stop);
    let repo = Repository::open(&repository).unwrap();
    assert_eq!(
        repo.get_session_ledger("large-checkpoint")
            .unwrap()
            .unwrap()
            .1,
        ledger
    );
    assert_eq!(repo.get_session_head("large-checkpoint").unwrap(), head);
    let graph = repo
        .load_provenance_graph(&ledger[0].provenance_hash)
        .unwrap();
    assert_eq!(
        Hash::of(&graph.serialize().unwrap()),
        ledger[0].provenance_hash
    );
    let tools: HashSet<_> = graph
        .nodes
        .iter()
        .filter_map(|n| n.tool_call_id.as_deref())
        .filter(|id| id.starts_with("large-"))
        .collect();
    assert_eq!(
        tools.len(),
        count,
        "published graph must contain every tool event"
    );
    for index in 0..count {
        assert!(tools.contains(format!("large-{index}").as_str()));
    }
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let completed = store
        .get_provenance_turn_for("large-checkpoint", 1)
        .unwrap()
        .unwrap();
    assert!(matches!(
        completed.state,
        atomic_repository::redb_change_store::ProvenanceTurnState::Completed
    ));
    let frozen = store
        .load_frozen_provenance_envelopes(turn.provenance_id)
        .unwrap();
    let actual: Vec<_> = frozen
        .into_iter()
        .filter(|e| e.event_id.starts_with("large-"))
        .map(|e| e.envelope)
        .collect();
    assert_eq!(
        actual, expected,
        "stored bytes and ordering must remain unchanged"
    );
}

#[test]
fn large_frozen_checkpoint_completes_across_agents() {
    for agent in ["codex", "claude-code", "opencode"] {
        large_checkpoint_fixture(agent, 1024, 1024, false);
    }
}

#[test]
fn large_envelope_checkpoint_recovers_between_pages() {
    large_checkpoint_fixture("claude-code", 1, 600 * 1024, true);
}

#[test]
fn failed_frozen_read_resumes_recorded_changes_on_next_stop() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repo");
    drop(Repository::init(&repository).unwrap());
    assert!(run_hook(
        &repository,
        "session-start",
        br#"{"session_id":"retry-frozen"}"#
    )
    .status
    .success());
    assert!(run_hook(
        &repository,
        "user-prompt-submit",
        br#"{"session_id":"retry-frozen","prompt":"record before read failure"}"#
    )
    .status
    .success());
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let marker = temp.path().join("unused.marker");
    let mut owner = spawn_owner_with_failpoint(&repository, "frozen-page-unavailable", &marker);
    wait_for_ping(&repository, &mut owner);
    std::fs::write(repository.join("retry.txt"), b"preserve this change\n").unwrap();
    let payload = br#"{"session_id":"retry-frozen","response":"complete"}"#;
    let failed = run_hook(&repository, "stop", payload);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("injected frozen page read failure"));
    assert!(run_owner(&repository, "shutdown").status.success());
    owner.wait().unwrap();
    wait_for_shutdown(&repository);
    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let prepared = store
        .get_provenance_turn_for("retry-frozen", 1)
        .unwrap()
        .unwrap()
        .checkpoint_attempt
        .unwrap();
    assert_eq!(prepared.source.change_hashes.len(), 1);
    assert_eq!(
        prepared.phase,
        atomic_repository::redb_change_store::ProvenanceCheckpointPhase::Prepared
    );
    drop(store);
    let retry = run_hook(&repository, "stop", payload);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(retry.stderr.is_empty());
    assert!(run_owner(&repository, "shutdown").status.success());
    wait_for_shutdown(&repository);
    let store =
        RedbChangeStore::open(Repository::canonical_change_store_path(&repository).unwrap())
            .unwrap();
    let completed = store
        .get_provenance_turn_for("retry-frozen", 1)
        .unwrap()
        .unwrap()
        .checkpoint_attempt
        .unwrap();
    assert_eq!(
        completed.phase,
        atomic_repository::redb_change_store::ProvenanceCheckpointPhase::Published
    );
    assert_eq!(completed.source, prepared.source);
    assert_eq!(completed.frozen_event_count, prepared.frozen_event_count);
    assert_eq!(completed.attempt_generation, prepared.attempt_generation);
    let repo = Repository::open(&repository).unwrap();
    let (_, ledger) = repo.get_session_ledger("retry-frozen").unwrap().unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].change_hashes, prepared.source.change_hashes);
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

#[test]
fn scoped_concurrent_stops_and_session_end_do_not_record_sibling_files() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    drop(Repository::init(&root).unwrap());
    let hook = |sid: &str, verb: &str, extra: Value| {
        let mut value = extra;
        value["session_id"] = sid.into();
        let output = run_agent_hook(
            &root,
            "opencode",
            verb,
            &serde_json::to_vec(&value).unwrap(),
        );
        assert!(output.status.success(), "{output:?}");
    };
    hook(
        "workspace-owner",
        "session-start",
        serde_json::json!({"recording_scope":"explicit-files-v1"}),
    );
    for sid in ["child-a", "child-b"] {
        hook(
            sid,
            "session-start",
            serde_json::json!({"recording_scope":"explicit-files-v1", "workspace_session_id":"workspace-owner"}),
        );
        hook(
            sid,
            "user-prompt",
            serde_json::json!({"prompt":"Write only the assigned file"}),
        );
    }
    for name in ["child-a.txt", "child-b.txt", "human.txt"] {
        std::fs::write(root.join(name), name).unwrap();
    }
    let pending:Vec<_> = ["child-a", "child-b"].into_iter().map(|sid|{
        let root=root.clone();
        thread::spawn(move||{
            let path=format!("{sid}.txt");
            let digest=atomic_agent::record::scope::fingerprint(&root,&path).unwrap();
            run_agent_hook(&root,"opencode","stop",&serde_json::to_vec(&serde_json::json!({"session_id":sid,"record_files":{path:digest},"response":"Done"})).unwrap())
        })
    }).collect();
    for handle in pending {
        let result = handle.join().unwrap();
        assert!(result.status.success(), "{result:?}");
    }
    for sid in ["child-a", "child-b"] {
        let state: Value = serde_json::from_slice(
            &std::fs::read(root.join(format!(".atomic/sessions/{sid}.json"))).unwrap(),
        )
        .unwrap();
        let owner: Value = serde_json::from_slice(
            &std::fs::read(root.join(".atomic/sessions/workspace-owner.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["view_name"], owner["view_name"]);
        assert_eq!(
            state["files_touched"],
            serde_json::json!([format!("{sid}.txt")])
        );
        assert_eq!(state["recorded_change_hashes"].as_array().unwrap().len(), 1);
        hook(
            sid,
            "stop",
            serde_json::json!({"record_files":{},"response":"duplicate idle"}),
        );
        hook(
            sid,
            "session-end",
            serde_json::json!({"record_files":{},"reason":"deleted"}),
        );
        let after: Value = serde_json::from_slice(
            &std::fs::read(root.join(format!(".atomic/sessions/{sid}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            after["recorded_change_hashes"],
            state["recorded_change_hashes"]
        );
        assert_eq!(after["turn_count"], state["turn_count"]);
    }
    assert_eq!(
        std::fs::read_to_string(root.join("human.txt")).unwrap(),
        "human.txt"
    );
    assert!(run_owner(&root, "shutdown").status.success());
    wait_for_shutdown(&root);
}

fn check_missing_turn_start(write_files: bool) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    drop(Repository::init(&root).unwrap());
    let hook = |sid: &str, verb: &str, mut value: Value| {
        value["session_id"] = sid.into();
        run_agent_hook(
            &root,
            "opencode",
            verb,
            &serde_json::to_vec(&value).unwrap(),
        )
    };
    assert!(hook(
        "parent",
        "session-start",
        serde_json::json!({"recording_scope":"explicit-files-v1"})
    )
    .status
    .success());
    let sid = "programmatic-child";
    assert!(hook(
        sid,
        "session-start",
        serde_json::json!({"recording_scope":"explicit-files-v1","workspace_session_id":"parent"})
    )
    .status
    .success());
    assert!(hook(
        sid,
        "user-prompt",
        serde_json::json!({"prompt":"First turn only"})
    )
    .status
    .success());
    for turn in 0..3 {
        // After the first turn, simulate a resumed/programmatic session that
        // emits tools and Stop but no chat.message / user-prompt callback.
        let event = serde_json::json!({"tool_name":"read","tool_call_id":format!("tool-{turn}"),"tool_input":{"path":"sample.txt"},"tool_output":format!("result-{turn}")});
        assert!(hook(sid, "before-tool", event.clone()).status.success());
        let files = if write_files {
            std::fs::write(root.join("sample.txt"), format!("turn {turn}\n")).unwrap();
            serde_json::json!({"sample.txt":atomic_agent::record::scope::fingerprint(&root,"sample.txt").unwrap()})
        } else {
            serde_json::json!({})
        };
        assert!(hook(sid, "after-tool", event).status.success());
        let stop = serde_json::json!({"record_files":files,"response":format!("done-{turn}")});
        if turn == 1 && write_files {
            let mut stale = stop.clone();
            stale["record_files"]["sample.txt"] = "stale digest".into();
            let failed = hook(sid, "stop", stale);
            assert!(!failed.status.success(), "{failed:?}");
            // Recovery must persist activation before a failing record so the
            // corrected Stop retries the same journal turn rather than skipping.
            let state: Value = serde_json::from_slice(
                &std::fs::read(root.join(format!(".atomic/sessions/{sid}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(state["phase"], "active");
            assert_eq!(state["turn_count"], 1);
        }
        for _ in 0..2 {
            let result = hook(sid, "stop", stop.clone());
            assert!(result.status.success(), "{result:?}");
        }
    }
    assert!(run_owner(&root, "shutdown").status.success());
    wait_for_shutdown(&root);
    let repo = Repository::open(&root).unwrap();
    let (_, turns) = repo.get_session_ledger(sid).unwrap().unwrap();
    assert_eq!(turns.len(), 3);
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn.change_hashes.len(), usize::from(write_files));
        assert_eq!(
            turn.previous_provenance,
            index.checked_sub(1).map(|i| turns[i].provenance_hash)
        );
        let graph = repo.load_provenance_graph(&turn.provenance_hash).unwrap();
        assert_eq!(graph.session_id, sid);
        let json = serde_json::to_string(&graph).unwrap();
        assert!(json.contains(&format!("tool-{index}")), "{json}");
        if index > 0 {
            assert_ne!(turn.goal.as_deref(), Some("First turn only"));
            for hash in &turn.change_hashes {
                assert_ne!(
                    repo.load_change(hash).unwrap().hashed.header.message,
                    "First turn only"
                );
            }
        }
    }
    if write_files {
        assert_eq!(
            std::fs::read_to_string(root.join("sample.txt")).unwrap(),
            "turn 2\n"
        );
    }
}

#[test]
fn missing_turn_start_recovers_changes_and_provenance_without_duplicate_stops() {
    check_missing_turn_start(true);
}

#[test]
fn missing_turn_start_recovers_read_only_provenance() {
    check_missing_turn_start(false);
}

#[test]
fn scoped_stop_without_start_or_journal_fails_visibly_and_can_retry() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    drop(Repository::init(&root).unwrap());
    let sid = "missing-everything";
    let hook = |verb: &str, mut value: Value| {
        value["session_id"] = sid.into();
        run_agent_hook(
            &root,
            "opencode",
            verb,
            &serde_json::to_vec(&value).unwrap(),
        )
    };
    assert!(hook(
        "session-start",
        serde_json::json!({"recording_scope":"explicit-files-v1"})
    )
    .status
    .success());
    assert!(hook(
        "user-prompt",
        serde_json::json!({"prompt":"first read-only turn"})
    )
    .status
    .success());
    assert!(hook("stop", serde_json::json!({"record_files":{}}))
        .status
        .success());
    // No tool event either: there is no durable evidence defining a new turn.
    std::fs::write(root.join("new.txt"), "keep this work\n").unwrap();
    let stop = serde_json::json!({"record_files":{"new.txt":atomic_agent::record::scope::fingerprint(&root,"new.txt").unwrap()}});
    let failed = hook("stop", stop.clone());
    assert!(!failed.status.success(), "{failed:?}");
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("no active turn or pending journal"),
        "{failed:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "keep this work\n"
    );
    assert!(hook(
        "user-prompt",
        serde_json::json!({"prompt":"recover pending file"})
    )
    .status
    .success());
    assert!(hook("stop", stop.clone()).status.success());
    // Unrelated human edits must not be swept up by a duplicate scoped Stop.
    std::fs::write(root.join("human.txt"), "leave alone\n").unwrap();
    assert!(hook("stop", stop).status.success());
    let invalid = hook(
        "stop",
        serde_json::json!({"record_files":{"../escape":null}}),
    );
    assert!(!invalid.status.success());
    assert!(run_owner(&root, "shutdown").status.success());
    wait_for_shutdown(&root);
    let repo = Repository::open(&root).unwrap();
    let (_, turns) = repo.get_session_ledger(sid).unwrap().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].change_hashes.len(), 1);
    assert!(repo.get_file_content("human.txt").unwrap().is_none());
}
