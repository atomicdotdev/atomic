//! Workflow inspection and maintenance use the shared handlers in both modes.
use std::path::Path;
use std::process::{Command, Output};

use atomic_core::kg::project_id::MintReason;
use atomic_repository::Repository;
use serde_json::Value;

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .args(args)
        .current_dir(root)
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .env("ATOMIC_DB_LOCK_WAIT_MS", "100")
        .env("ATOMIC_DAEMON_BIN", root.join("no-daemon-binary"))
        .env("ATOMIC_DAEMON_SOCKET", root.join("no-daemon.socket"));
    command
}

fn json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fixture(root: &Path) {
    let repo = Repository::init(root).unwrap();
    repo.ensure_project_identity(MintReason::Created).unwrap();
    repo.rebuild_kg(true).unwrap();
}

#[test]
fn local_workflow_reads_are_readonly_and_maintenance_routes_through_rpc() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fixture(root);
    let database = root.join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let log = root.join("requests.jsonl");
    let identity = json(
        command(root, &["repo", "identity", "show", "--json"])
            .env("ATOMIC_DAEMON_LOG_REQUESTS", &log)
            .output()
            .unwrap(),
    );
    assert_eq!(identity["minted"], "created");
    let report = json(
        command(root, &["query", "validate", "--format", "json"])
            .env("ATOMIC_DAEMON_LOG_REQUESTS", &log)
            .output()
            .unwrap(),
    );
    assert_eq!(report["conforms"], true);
    assert_eq!(
        std::fs::read(&database).unwrap(),
        before,
        "inspection must not migrate/write the database"
    );
    let output = command(root, &["query", "reindex", "--full"])
        .env("ATOMIC_DAEMON_LOG_REQUESTS", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = std::fs::read_to_string(log).unwrap();
    for method in [
        "GetProjectIdentity",
        "GetWorkflowSnapshot",
        "ListRemotes",
        "MaintainKnowledgeGraph",
    ] {
        assert!(
            requests.contains(method),
            "missing RPC {method}: {requests}"
        );
    }
}

#[test]
fn identity_inspection_does_not_silently_mint_a_missing_identity() {
    let directory = tempfile::tempdir().unwrap();
    drop(Repository::init(directory.path()).unwrap());
    let database = directory.path().join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let output = command(directory.path(), &["repo", "identity", "show", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MissingProjectIdentity"));
    assert_eq!(std::fs::read(database).unwrap(), before);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reactor_workflow_commands_use_served_handlers_without_local_fallback() {
    use libatomic::atomic::{
        daemon_service_server::DaemonServiceServer,
        knowledge_service_server::KnowledgeServiceServer,
        maintenance_service_server::MaintenanceServiceServer,
        sync_service_server::SyncServiceServer,
    };
    use libatomic::daemon::{
        services::DaemonImpl, services_agent::KnowledgeImpl, services_maintenance::MaintenanceImpl,
        services_sync::SyncImpl, state::DaemonState,
    };
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    fixture(&root);
    let local_identity = json(
        command(&root, &["repo", "identity", "show", "--json"])
            .output()
            .unwrap(),
    );
    let local_report = json(
        command(&root, &["query", "validate", "--format", "json"])
            .output()
            .unwrap(),
    );
    let socket_directory = tempfile::Builder::new()
        .prefix("kg-rpc-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = socket_directory.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state = Arc::new(DaemonState::new());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(DaemonServiceServer::new(DaemonImpl {
                state: state.clone(),
            }))
            .add_service(KnowledgeServiceServer::new(KnowledgeImpl {
                state: state.clone(),
            }))
            .add_service(MaintenanceServiceServer::new(MaintenanceImpl {
                state: state.clone(),
            }))
            .add_service(SyncServiceServer::new(SyncImpl { state }))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
    );
    for (args, expected) in [
        (
            vec!["repo", "identity", "show", "--json"],
            Some(local_identity),
        ),
        (
            vec!["query", "validate", "--format", "json"],
            Some(local_report),
        ),
        (vec!["query", "reindex", "--full"], None),
        (vec!["doctor", "repair-native-indexes"], None),
    ] {
        let mut cli = command(&root, &args);
        cli.env("ATOMIC_SERVICE", "reactor")
            .env("ATOMIC_DAEMON_SOCKET", &socket);
        let output = tokio::task::spawn_blocking(move || cli.output().unwrap())
            .await
            .unwrap();
        if let Some(expected) = expected {
            assert_eq!(json(output), expected);
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    for args in [
        vec!["repo", "identity", "show", "--json"],
        vec!["query", "validate", "--format", "json"],
        vec!["query", "reindex", "--full"],
        vec!["doctor", "repair-native-indexes"],
    ] {
        let mut cli = command(&root, &args);
        cli.env("ATOMIC_SERVICE", "reactor")
            .env("ATOMIC_DAEMON_SOCKET", &socket);
        let output = tokio::task::spawn_blocking(move || cli.output().unwrap())
            .await
            .unwrap();
        assert!(
            !output.status.success(),
            "unavailable reactor must not fall back to direct database access"
        );
    }
}
