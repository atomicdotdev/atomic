//! Integration tests for the RPC-based provenance journal sink
//! (`ProvenanceRpcSink`): the real daemon binary from the Reactor
//! checkout, the real `atomic` binary driving the real sink through the
//! full ProvenanceJournalSink trait flow over the real socket.
//!
//! The sink itself lives inside the (binary-only) CLI crate, so the suite
//! drives it through the hidden `agent journal-rpc-selftest` harness
//! command, which exercises exactly the trait surface the orchestrator
//! uses: reserve → append (chunked under the wire budget) → prepare →
//! load_frozen_envelopes (paged reassembly) → bind → acknowledge, plus
//! stop_turn/turn_status. A second test proves `agent hooks` journals over
//! the daemon's ProvenanceService when the daemon is already reachable
//! (the daemon's RPC log must show the journal calls), and a third proves
//! the D4 cutover: with NO daemon pre-started, a hook flow auto-starts
//! one and still journals over the socket.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use atomic_repository::Repository;
use serial_test::serial;
use tempfile::TempDir;

/// Kills the spawned daemon when dropped — a panic in the test must not
/// leak a lock-holding daemon for the next run.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The Reactor checkout that owns the atomicd daemon binary. This test
/// file lives in <atomic>/atomic-cli/tests, so ../../reactor resolves to
/// the sibling workspace checkout; `ATOMIC_REACTOR_ROOT` overrides that
/// for non-sibling checkouts. `None` means no Reactor checkout exists
/// (e.g. CI runners) and the RPC-sink suite skips — the daemon binary is
/// not buildable from this workspace alone.
fn reactor_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("ATOMIC_REACTOR_ROOT") {
        let root = PathBuf::from(root);
        return root.join("Cargo.toml").exists().then_some(root);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reactor");
    root.canonicalize()
        .ok()
        .filter(|root| root.join("Cargo.toml").exists())
}

/// `None` when the Reactor checkout (and thus the daemon binary) is
/// unavailable; every test in this suite skips in that case.
fn daemon_binary() -> Option<PathBuf> {
    Some(reactor_root()?.join("target/debug/atomicd"))
}

/// Skip-label for tests that require the Reactor checkout.
fn skip_without_reactor(what: &str) {
    // cargo prints skipped-test output on stderr; keep the skip visible
    // in the run log without failing the suite.
    eprintln!("skipping {what}: no Reactor checkout (set ATOMIC_REACTOR_ROOT to enable)");
}

/// Guard that restores the socket override when dropped.
struct SocketEnv(Option<std::ffi::OsString>);

impl SocketEnv {
    fn set(socket: &Path) -> Self {
        let previous = std::env::var_os("ATOMIC_DAEMON_SOCKET");
        std::env::set_var("ATOMIC_DAEMON_SOCKET", socket);
        SocketEnv(previous)
    }
}

/// Guard that restores one env override when dropped (panic-safe).
struct VarEnv(&'static str, Option<std::ffi::OsString>);

impl VarEnv {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        VarEnv(key, previous)
    }
}

impl Drop for VarEnv {
    fn drop(&mut self) {
        match self.1.take() {
            Some(previous) => std::env::set_var(self.0, previous),
            None => std::env::remove_var(self.0),
        }
    }
}

/// Kill the daemon serving `socket` (the D4 test's auto-started daemon is
/// detached; it must not outlive the test).
fn kill_daemon_on(socket: &Path) {
    let output = Command::new("lsof").arg("-t").arg(socket).output();
    if let Ok(out) = output {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).output();
            }
        }
    }
}

impl Drop for SocketEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(previous) => std::env::set_var("ATOMIC_DAEMON_SOCKET", previous),
            None => std::env::remove_var("ATOMIC_DAEMON_SOCKET"),
        }
    }
}

fn wait_for_daemon(socket: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("inspect daemon") {
            panic!("atomicd exited with {status} during bootstrap");
        }
        if socket.exists() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "atomicd never bound {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn spawn_daemon(socket: &Path, extra_envs: &[(&str, &str)]) -> Child {
    let _ = std::fs::remove_file(socket);
    let mut command = Command::new(
        daemon_binary()
            .expect("caller guards on daemon_binary(); the suite skips without a Reactor checkout"),
    );
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in extra_envs {
        command.env(key, value);
    }
    command.spawn().expect("spawn atomicd")
}

fn run_atomic(args: &[&str], current_dir: &Path, stdin: Option<&[u8]>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .args(args)
        .current_dir(current_dir)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn atomic");
    if let (Some(stdin), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        pipe.write_all(stdin).expect("write hook payload");
    }
    let output = child.wait_with_output().expect("wait for atomic");
    assert!(
        output.status.success(),
        "atomic {:?} failed: stdout: {}; stderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
#[serial]
fn rpc_sink_drives_the_full_trait_flow_over_the_socket() {
    // The selftest is the REACTOR transport's exercise — opt in explicitly
    // (local mode is the CLI default and would refuse the command).
    let _service = VarEnv::set("ATOMIC_SERVICE", "reactor");
    // Build the daemon from the Reactor checkout when it is missing (the
    // normal verification flow builds it first).
    let (Some(reactor), Some(daemon)) = (reactor_root(), daemon_binary()) else {
        skip_without_reactor("rpc_sink_drives_the_full_trait_flow_over_the_socket");
        return;
    };
    if !daemon.exists() {
        let output = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(reactor.join("Cargo.toml"))
            .output()
            .expect("cargo build reactor");
        assert!(
            output.status.success(),
            "building atomicd failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let socket = temp.path().join("atomicd.sock");
    let _env = SocketEnv::set(&socket);
    let mut daemon = KillOnDrop(spawn_daemon(&socket, &[]));
    wait_for_daemon(&socket, &mut daemon.0);

    // Six ~450 KiB wire envelopes (payload plus JSON framing) against
    // the sink's 1 MiB wire chunk budget: the batch must split into
    // three chunks of two and reassemble bit-exact.
    let output = run_atomic(
        &[
            "agent",
            "journal-rpc-selftest",
            "--repository",
            repo.display().to_string().as_str(),
            "--session-id",
            "rpc-sink-selftest",
            "--turn",
            "1",
            "--envelopes",
            "6",
            "--envelope-bytes",
            "450000",
            "--json",
        ],
        &repo,
        None,
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json report");

    assert_eq!(report["acks"], 6, "every envelope is acknowledged");
    assert_eq!(
        report["ack_sequences"],
        serde_json::json!([0, 1, 2, 3, 4, 5]),
        "sequences are contiguous from zero"
    );
    assert_eq!(
        report["chunk_count"], 3,
        "a ~2.7 MiB batch must split into three chunks under the 1 MiB wire budget"
    );
    assert_eq!(report["frozen_envelopes"], 6);
    assert!(
        report["reassembled_equal"].as_bool().unwrap(),
        "paged loads must reassemble to the exact committed bytes"
    );
    assert_eq!(report["attempt_generation"], 2);
    assert!(
        report["bound_hash"].as_bool().unwrap(),
        "bind must persist the graph hash"
    );
    assert!(report["stopped"].as_bool().unwrap(), "stop_turn ran");
    assert!(
        report["status_lifecycle"]
            .as_str()
            .unwrap()
            .contains("Stopped"),
        "turn_status must report the stopped lifecycle: {}",
        report["status_lifecycle"]
    );

    // The daemon really owns the journal: inspect the store the daemon
    // wrote (its per-request handles are closed between RPCs).
    drop(daemon);
    let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
        Repository::canonical_database_path(&repo).unwrap(),
    )
    .unwrap();
    let completed = store
        .get_provenance_turn_for("rpc-sink-selftest", 1)
        .unwrap()
        .expect("turn 1");
    assert!(matches!(
        completed.state,
        atomic_repository::redb_change_store::ProvenanceTurnState::Completed
    ));
    assert_eq!(completed.checkpoint_attempt.unwrap().frozen_event_count, 6);
    let stopped = store
        .get_provenance_turn_for("rpc-sink-selftest", 2)
        .unwrap()
        .expect("turn 2");
    match stopped.state {
        atomic_repository::redb_change_store::ProvenanceTurnState::Stopped(stop) => {
            assert_eq!(
                stop.cause,
                atomic_repository::redb_change_store::StopCause::UserRequested
            );
            assert!(stop.resumable);
        }
        other => panic!("expected stopped turn, got {other:?}"),
    }
}

#[test]
#[serial]
fn hooks_prefer_the_rpc_journal_sink_when_the_daemon_is_reachable() {
    if !daemon_binary().is_some_and(|daemon| daemon.exists()) {
        skip_without_reactor("hooks_prefer_the_rpc_journal_sink_when_the_daemon_is_reachable");
        return; // covered by the sibling test when the daemon is present
    }
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let socket = temp.path().join("atomicd.sock");
    let rpc_log = temp.path().join("daemon-rpc.jsonl");
    // This suite proves the REACTOR journal transport: the hook flow must
    // journal over the daemon's ProvenanceService, not in-process.
    let _service = VarEnv::set("ATOMIC_SERVICE", "reactor");
    let _env = SocketEnv::set(&socket);
    let mut daemon = KillOnDrop(spawn_daemon(
        &socket,
        &[("ATOMIC_DAEMON_LOG_REQUESTS", rpc_log.to_str().unwrap())],
    ));
    wait_for_daemon(&socket, &mut daemon.0);

    // A plain hook flow with the daemon up: the orchestrator's journal
    // sink is the RPC sink (no owner process is ever spawned).
    run_atomic(
        &[
            "agent",
            "hooks",
            "claude-code",
            "session-start",
            "--foreground",
            "--no-color",
        ],
        &repo,
        Some(br#"{"session_id":"rpc-hook-flow"}"#),
    );
    run_atomic(
        &[
            "agent",
            "hooks",
            "claude-code",
            "user-prompt-submit",
            "--foreground",
            "--no-color",
        ],
        &repo,
        Some(br#"{"session_id":"rpc-hook-flow","prompt":"journal over the daemon"}"#),
    );

    // The daemon's RPC log is the proof the journal went over the wire:
    // ReserveTurn and AppendEnvelopes must have been served.
    let log = std::fs::read_to_string(&rpc_log).unwrap();
    assert!(
        log.contains("\"method\":\"ReserveTurn\""),
        "the hook must have reserved its turn over RPC:\n{log}"
    );
    assert!(
        log.contains("\"method\":\"AppendEnvelopes\""),
        "the hook must have journaled over RPC:\n{log}"
    );
}

#[test]
#[serial]
fn hooks_start_the_daemon_when_none_is_running() {
    if !daemon_binary().is_some_and(|daemon| daemon.exists()) {
        skip_without_reactor("hooks_start_the_daemon_when_none_is_running");
        return; // covered by the sibling test when the daemon is present
    }
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let socket = temp.path().join("atomicd.sock");
    let rpc_log = temp.path().join("daemon-rpc.jsonl");
    // No daemon is spawned by the test: the socket path is empty. The
    // sink must start the daemon itself (D4) and journal over it.
    // This suite proves the REACTOR journal transport.
    let _service = VarEnv::set("ATOMIC_SERVICE", "reactor");
    let _env = SocketEnv::set(&socket);
    let _bin = VarEnv::set(
        "ATOMIC_DAEMON_BIN",
        daemon_binary().expect("guarded above").to_str().unwrap(),
    );
    let _log = VarEnv::set("ATOMIC_DAEMON_LOG_REQUESTS", rpc_log.to_str().unwrap());

    run_atomic(
        &[
            "agent",
            "hooks",
            "claude-code",
            "session-start",
            "--foreground",
            "--no-color",
        ],
        &repo,
        Some(br#"{"session_id":"d4-autostart"}"#),
    );
    run_atomic(
        &[
            "agent",
            "hooks",
            "claude-code",
            "user-prompt-submit",
            "--foreground",
            "--no-color",
        ],
        &repo,
        Some(br#"{"session_id":"d4-autostart","prompt":"journal via the auto-started daemon"}"#),
    );

    // The auto-started daemon served the journal RPCs: the D4 proof.
    let log = std::fs::read_to_string(&rpc_log).unwrap();
    assert!(
        log.contains("\"method\":\"ReserveTurn\""),
        "the hook must have auto-started the daemon and reserved over RPC:\n{log}"
    );
    assert!(
        log.contains("\"method\":\"AppendEnvelopes\""),
        "the hook must have auto-started the daemon and journaled over RPC:\n{log}"
    );

    // Leave no daemon behind on the private socket.
    kill_daemon_on(&socket);
}
