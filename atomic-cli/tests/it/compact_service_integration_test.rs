//! Exercise the real CLI through the shared in-process and served handlers.
use std::path::Path;
use std::process::{Command, Output};

use atomic_repository::{ChangeStore, Repository, DEFAULT_CACHE_CAPACITY};
use serde_json::Value;

fn command(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .args(["compact", "--repository"])
        .arg(path)
        .arg("--json")
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .env("ATOMIC_DB_LOCK_WAIT_MS", "100")
        .env("ATOMIC_DAEMON_BIN", path.join("no-daemon-binary"))
        .env("ATOMIC_DAEMON_SOCKET", path.join("no-daemon.socket"));
    command
}

fn report(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn local_compaction_preserves_views_journal_changes_and_dirty_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let sandbox = dir.path().join("sandbox");
    let mut repo = Repository::init(&root).unwrap();
    let contents = b"recorded contents\n";
    std::fs::write(root.join("file.txt"), contents).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "file.txt",
        Default::default(),
    )
    .unwrap();
    let hash = *repo
        .record_all(
            repo.require_working_copy_id().unwrap(),
            "compaction fixture",
        )
        .unwrap()
        .hash();
    let change_path = ChangeStore::new(repo.changes_dir(), DEFAULT_CACHE_CAPACITY)
        .unwrap()
        .change_path(&hash);
    let change_bytes = std::fs::read(&change_path).unwrap();
    let view = repo.current_view().to_owned();
    repo.create_view("compact-child").unwrap();
    let views = repo.list_views().unwrap();
    repo.provision_sandbox(repo.require_working_copy_id().unwrap(), &sandbox, &view)
        .unwrap();
    let store = repo.redb_change_store().unwrap();
    let turn = store.reserve_provenance_turn("compact", 1, 1).unwrap();
    let envelope = serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"event_id":"pending","session_id":"compact",
        "turn_number":1,"generation":turn.generation,"timestamp_ms":1700000000000_i64,
        "event":{"type":"tool","phase":"after","tool_name":"Read",
            "tool_call_id":"pending","input":{"path":"file.txt"},
            "output":"recorded contents","status":"completed"}
    }))
    .unwrap();
    store
        .append_provenance_envelope(turn.provenance_id, turn.generation, "pending", &envelope, 2)
        .unwrap();
    let pending = store.load_provenance_envelopes(turn.provenance_id).unwrap();
    let turn_before = store.get_provenance_turn(turn.provenance_id).unwrap();
    drop(store);
    drop(repo);
    std::fs::write(root.join("file.txt"), b"dirty main\n").unwrap();
    std::fs::write(sandbox.join("file.txt"), b"dirty sandbox\n").unwrap();
    let database = root.join(".atomic/atomic.redb").canonicalize().unwrap();
    let before = database.metadata().unwrap().len();
    let log = dir.path().join("service.jsonl");
    let result = report(
        command(&sandbox)
            .env("ATOMIC_DAEMON_LOG_REQUESTS", &log)
            .output()
            .unwrap(),
    );
    assert_eq!(result["database"], database.to_str().unwrap());
    assert_eq!(result["before_bytes"], before);
    let after = database.metadata().unwrap().len();
    assert_eq!(result["after_bytes"], after);
    assert_eq!(result["reclaimed_bytes"], before.saturating_sub(after));
    assert!(std::fs::read_to_string(log)
        .unwrap()
        .contains("CompactDatabase"));
    report(command(&root).output().unwrap()); // Repeat maintenance is valid.
    let repo = Repository::open_existing(&root).unwrap();
    assert_eq!(repo.list_views().unwrap(), views);
    assert_eq!(repo.log(Default::default()).unwrap()[0].hash, hash);
    for view in [&view, "compact-child"] {
        assert_eq!(
            repo.get_file_content_on_view("file.txt", view)
                .unwrap()
                .unwrap(),
            contents
        );
    }
    assert_eq!(std::fs::read(change_path).unwrap(), change_bytes);
    assert_eq!(
        std::fs::read(root.join("file.txt")).unwrap(),
        b"dirty main\n"
    );
    assert_eq!(
        std::fs::read(sandbox.join("file.txt")).unwrap(),
        b"dirty sandbox\n"
    );
    let store = repo.redb_change_store().unwrap();
    assert_eq!(
        store.load_provenance_envelopes(turn.provenance_id).unwrap(),
        pending
    );
    assert_eq!(
        store.get_provenance_turn(turn.provenance_id).unwrap(),
        turn_before
    );
    assert!(!sandbox.join(".atomic/atomic.redb").exists());
}

#[test]
fn busy_database_reports_actionable_error_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let output = command(dir.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    drop(repo);
    report(command(dir.path()).output().unwrap());
}

#[test]
#[cfg(unix)]
fn preview_and_full_cleanup_are_explicit_and_preserve_live_data() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let mut repo = Repository::init(&root).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let original_view = repo.current_view().to_owned();
    repo.create_view("discarded").unwrap();
    repo.switch_view(wc, "discarded").unwrap();
    let old = repo
        .operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(wc),
            None,
            false,
        )
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.is_head)
        .unwrap()
        .operation
        .id();
    repo.switch_view(wc, &original_view).unwrap();
    repo.delete_view("discarded").unwrap();
    let metadata = root.join(".atomic");
    let wc_dir = metadata.join("working-copies").join(wc.to_string());
    let discarded = wc_dir.join("workspaces/discarded");
    let live = wc_dir.join("workspaces").join(&original_view);
    std::fs::create_dir_all(&discarded).unwrap();
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(discarded.join("ignored.txt"), "expired ignored artifact").unwrap();
    std::fs::write(live.join("ignored.txt"), "live ignored artifact").unwrap();
    let recovery = wc_dir.join("operation-recovery").join(old.to_string());
    assert!(recovery.exists());
    drop(repo);
    let before = blake3::hash(&std::fs::read(metadata.join("atomic.redb")).unwrap());
    let preview = report(
        command(&root)
            .args(["--full", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(preview["dry_run"], true);
    assert!(preview["cleanup_candidate_bytes"].as_u64().unwrap() > 0);
    assert_eq!(preview["cleanup_removed_bytes"], 0);
    assert_eq!(
        blake3::hash(&std::fs::read(metadata.join("atomic.redb")).unwrap()),
        before
    );
    assert!(discarded.exists() && recovery.exists());

    let physical_only = report(command(&root).arg("--database-only").output().unwrap());
    assert_eq!(physical_only["cleanup"], false);
    assert!(discarded.exists() && recovery.exists());
    let safe = report(command(&root).output().unwrap());
    assert_eq!(safe["cleanup"], true);
    assert!(
        !recovery.exists(),
        "older terminal recovery data should be reclaimed"
    );
    assert!(
        discarded.exists(),
        "ordinary compact must not expire a deleted-view shelf"
    );
    let full = report(command(&root).arg("--full").output().unwrap());
    assert!(full["cleanup_removed_bytes"].as_u64().unwrap() >= 24);
    assert!(!discarded.exists());
    assert_eq!(
        std::fs::read_to_string(live.join("ignored.txt")).unwrap(),
        "live ignored artifact"
    );
    let repeated = report(command(&root).arg("--full").output().unwrap());
    assert_eq!(repeated["cleanup_removed_bytes"], 0);
    assert!(!command(&root)
        .args(["--database-only", "--dry-run"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn cleanup_keeps_current_undo_and_historical_tracked_restore_working() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let mut repo = Repository::init(&root).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::write(root.join("file.txt"), "recorded fixture\n").unwrap();
    repo.add(wc, "file.txt", Default::default()).unwrap();
    repo.record_all(wc, "fixture baseline").unwrap();
    let original = repo.current_view().to_owned();
    repo.create_view("other").unwrap();
    repo.switch_view(wc, "other").unwrap();
    let selected = repo
        .operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(wc),
            None,
            false,
        )
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.is_head)
        .unwrap()
        .operation
        .id();
    repo.switch_view(wc, &original).unwrap();
    drop(repo);
    report(command(&root).output().unwrap());
    let mut repo = Repository::open_existing(&root).unwrap();
    repo.undo_operation(wc, None).unwrap();
    assert_eq!(repo.current_view(), "other");
    repo.restore_operation(wc, selected).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "recorded fixture\n"
    );
}

#[test]
fn missing_repository_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!command(dir.path()).output().unwrap().status.success());
    assert!(!dir.path().join(".atomic").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reactor_cli_uses_same_handler_over_socket_without_fallback() {
    use libatomic::atomic::daemon_service_server::DaemonServiceServer;
    use libatomic::atomic::maintenance_service_server::MaintenanceServiceServer;
    use libatomic::daemon::{
        services::DaemonImpl, services_maintenance::MaintenanceImpl, state::DaemonState,
    };
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    drop(Repository::init(&root).unwrap());
    // Short socket path for macOS. The test server serves the real library;
    // this does not require or modify the private Reactor checkout.
    let socket_dir = tempfile::Builder::new()
        .prefix("compact-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = socket_dir.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state = Arc::new(DaemonState::new());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(DaemonServiceServer::new(DaemonImpl {
                state: state.clone(),
            }))
            .add_service(MaintenanceServiceServer::new(MaintenanceImpl { state }))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
    );
    let mut cli = command(&root);
    cli.env("ATOMIC_SERVICE", "reactor")
        .env("ATOMIC_DAEMON_SOCKET", &socket);
    let output = tokio::task::spawn_blocking(move || cli.output().unwrap())
        .await
        .unwrap();
    let result = report(output);
    for args in [
        vec!["compact", "--json"],
        vec!["compact", "--repository", ".", "--json"],
    ] {
        let mut relative = Command::new(env!("CARGO_BIN_EXE_atomic"));
        relative
            .args(args)
            .current_dir(&root)
            .env("ATOMIC_SERVICE", "reactor")
            .env("ATOMIC_DAEMON_SOCKET", &socket)
            .env("ATOMIC_DAEMON_BIN", root.join("no-daemon-binary"));
        let output = tokio::task::spawn_blocking(move || relative.output().unwrap())
            .await
            .unwrap();
        assert_eq!(report(output)["database"], result["database"]);
    }
    assert_eq!(
        result["database"],
        root.join(".atomic/atomic.redb")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let mut cli = command(&root);
    cli.env("ATOMIC_SERVICE", "reactor")
        .env("ATOMIC_DAEMON_SOCKET", &socket);
    let output = tokio::task::spawn_blocking(move || cli.output().unwrap())
        .await
        .unwrap();
    assert!(
        !output.status.success(),
        "reactor mode must never fall back to local compaction"
    );
}
