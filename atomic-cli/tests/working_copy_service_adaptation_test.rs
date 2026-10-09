//! The unchanged RPC contract must carry working-copy operations through the
//! shared handlers with the same identities and rendering as native recording.
#![cfg(unix)]

use atomic_core::change::GraphOp;
use atomic_repository::{HistoryOptions, Repository};
use libatomic::atomic::{
    daemon_service_server::DaemonServiceServer,
    repository_mutation_service_server::RepositoryMutationServiceServer,
    repository_query_service_server::RepositoryQueryServiceServer,
    view_service_server::ViewServiceServer,
};
use libatomic::daemon::{
    services::{DaemonImpl, MutationImpl, QueryImpl},
    services_query::ViewImpl,
    state::DaemonState,
};
use std::{fs, path::Path, process::Command, sync::Arc};

fn run(root: &Path, home: &Path, socket: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .env("ATOMIC_HOME", home)
        .env("ATOMIC_SERVICE", "reactor")
        .env_remove("ATOMIC_RPC")
        .env("ATOMIC_DAEMON_SOCKET", socket)
        .env("ATOMIC_DAEMON_BIN", home.join("no-daemon"))
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_record_all_keeps_rename_identity_and_view_switch_renders_live_parent() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    drop(Repository::init(root.path()).unwrap());
    let socket_dir = tempfile::Builder::new()
        .prefix("adapt-")
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
            .add_service(RepositoryQueryServiceServer::new(QueryImpl {
                state: state.clone(),
            }))
            .add_service(RepositoryMutationServiceServer::new(MutationImpl {
                state: state.clone(),
            }))
            .add_service(ViewServiceServer::new(ViewImpl { state }))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
    );
    let result = tokio::task::spawn_blocking(move || {
        let cli = |args: &[&str]| run(root.path(), home.path(), &socket, args);
        let record = |message: &str| { cli(&["record", "-am", message, "--author", "Service Regression"]); };
        fs::write(root.path().join("revise-first.txt"), b"original\n").unwrap();
        record("revise first");
        fs::write(root.path().join("revise-second.txt"), b"independent\n").unwrap();
        record("revise second");
        let pending = Repository::open(root.path()).unwrap().get_view_changes(None).unwrap()[1].1;
        fs::write(root.path().join("revise-first.txt"), b"edited through reactor socket\n").unwrap();
        fs::write(root.path().join("revise-scratch.txt"), b"private\n").unwrap();
        cli(&["revise", "@~1", "-m", "revised through reactor socket"]);
        {
            let repo = Repository::open(root.path()).unwrap();
            let history = repo.get_view_changes(None).unwrap();
            assert_eq!(history.len(), 2);
            assert_eq!(history[1].1, pending);
            let mut files = std::collections::BTreeMap::new();
            repo.materialize_view_entries::<()>(repo.current_view(), |entry| {
                files.insert(entry.path, entry.content); Ok(())
            }).unwrap().unwrap();
            assert_eq!(files["revise-first.txt"], b"edited through reactor socket\n");
            assert_eq!(files["revise-second.txt"], b"independent\n");
            assert!(!files.contains_key("revise-scratch.txt"));
        }
        assert_eq!(fs::read(root.path().join("revise-scratch.txt")).unwrap(), b"private\n");
        fs::remove_file(root.path().join("revise-scratch.txt")).unwrap();
        let stack_base = "first\nseparator one\nsecond\nseparator two\nthird\n";
        fs::write(root.path().join("stack.txt"), stack_base).unwrap();
        record("stack target");
        let stack_second = stack_base.replace("second", "SECOND");
        fs::write(root.path().join("stack.txt"), &stack_second).unwrap();
        record("stack dependent");
        let stack_tip = stack_second.replace("third", "THIRD");
        fs::write(root.path().join("stack.txt"), &stack_tip).unwrap();
        record("stack transitive");
        let old_stack = Repository::open(root.path()).unwrap().get_view_changes(None).unwrap();
        let expected_stack = stack_tip.replace("first", "FIRST");
        fs::write(root.path().join("stack.txt"), &expected_stack).unwrap();
        cli(&["revise", "@~2", "-m", "revised dependent stack through reactor"]);
        {
            let repo = Repository::open(root.path()).unwrap();
            let history = repo.get_view_changes(None).unwrap();
            assert_eq!(history.len(), old_stack.len());
            for (old, new) in old_stack.iter().rev().take(3).zip(history.iter().rev()) {
                assert_ne!(old.1, new.1);
            }
            let mut files = std::collections::BTreeMap::new();
            repo.materialize_view_entries::<()>(repo.current_view(), |entry| {
                files.insert(entry.path, entry.content); Ok(())
            }).unwrap().unwrap();
            assert_eq!(files["stack.txt"], expected_stack.as_bytes());
        }
        assert_eq!(fs::read(root.path().join("stack.txt")).unwrap(), expected_stack.as_bytes());
        assert!(cli(&["status", "--short"]).trim().is_empty());
        fs::write(root.path().join("old.txt"), b"one\ntwo\nthree\nfour\n").unwrap();
        record("base");
        let inode = Repository::open(root.path()).unwrap().get_file_inode("old.txt").unwrap().unwrap();
        fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
        record("rename via status add record");
        {
            let repo = Repository::open(root.path()).unwrap();
            assert_eq!(repo.get_file_inode("new.txt").unwrap(), Some(inode));
            let latest = repo.log(HistoryOptions::default()).unwrap().into_iter().max_by_key(|e| e.sequence).unwrap();
            let change = repo.load_change(&latest.hash).unwrap();
            assert!(change.hunks().iter().any(|op| matches!(op, GraphOp::FileMove { .. })));
            assert!(!change.hunks().iter().any(|op| matches!(op, GraphOp::FileAdd { .. } | GraphOp::FileDel { .. })));
        }
        let base = "[server]\nhost = localhost\nport = 8080\nworkers = 4\n\n[database]\nurl = localhost\npool_size = 10\ntimeout = 30\n";
        fs::write(root.path().join("server.conf"), base).unwrap();
        record("server base");
        cli(&["view", "create", "feature", "--draft", "--parent", "dev"]);
        cli(&["view", "switch", "feature"]);
        let child = base.replace("port = 8080", "port = 9090").replace("pool_size = 10", "pool_size = 20");
        fs::write(root.path().join("server.conf"), &child).unwrap(); record("child");
        cli(&["view", "switch", "dev"]);
        let parent = base.replace("workers = 4", "workers = 8").replace("timeout = 30", "timeout = 60");
        fs::write(root.path().join("server.conf"), &parent).unwrap(); record("parent");
        cli(&["view", "switch", "feature"]);
        assert_eq!(fs::read_to_string(root.path().join("server.conf")).unwrap(), child.replace("workers = 4", "workers = 8").replace("timeout = 30", "timeout = 60"));
        assert!(cli(&["status", "--short"]).trim().is_empty());

        // --stash must save edited bytes before switching through the RPC.
        fs::write(root.path().join("new.txt"), b"unrecorded stash edit\n").unwrap();
        cli(&["view", "switch", "dev", "--stash"]);
        cli(&["view", "switch", "feature"]);
        cli(&["stash", "pop"]);
        assert_eq!(fs::read(root.path().join("new.txt")).unwrap(), b"unrecorded stash edit\n");
        cli(&["record", "-am", "record recovered stash", "--author", "Service Regression"]);

        // The same service must resolve the sandbox's own stable identity and
        // view while using the canonical database, leaving its parent untouched.
        let sandbox = home.path().join("sandbox");
        let (main_working_copy, main_change_count) = {
            let mut repo = Repository::open(root.path()).unwrap();
            let parent = repo.current_view().to_string();
            repo.create_view_from("sandbox-work", &parent).unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            repo.provision_sandbox(wc, &sandbox, "sandbox-work").unwrap();
            (wc, repo.get_view_info(&parent).unwrap().change_count)
        };
        fs::rename(sandbox.join("new.txt"), sandbox.join("sandbox.txt")).unwrap();
        run(&sandbox, home.path(), &socket, &["record", "-am", "sandbox rename", "--author", "Service Regression"]);
        {
            let repo = Repository::open(&sandbox).unwrap();
            assert_ne!(repo.require_working_copy_id().unwrap(), main_working_copy);
            assert_eq!(repo.current_view(), "sandbox-work");
            assert_eq!(repo.get_file_inode("sandbox.txt").unwrap(), Some(inode));
        }
        let repo = Repository::open(root.path()).unwrap();
        assert_eq!(repo.get_view_info("feature").unwrap().change_count, main_change_count);
        assert!(root.path().join("new.txt").is_file());
        assert!(!root.path().join("sandbox.txt").exists());
        assert!(!sandbox.join(".atomic/atomic.redb").exists());
    }).await;
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    result.unwrap();
}
