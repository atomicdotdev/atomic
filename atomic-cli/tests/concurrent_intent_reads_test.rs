use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_repository::{IntentCreateOptions, Repository};

// Ensure a failed assertion or deadline never leaves query processes running.
struct Queries(Vec<std::process::Child>);

impl Drop for Queries {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_query(root: &std::path::Path, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finish_queries(mut queries: Queries, timeout: Duration) -> Vec<std::process::Output> {
    let deadline = Instant::now() + timeout;
    while queries
        .0
        .iter_mut()
        .any(|child| child.try_wait().unwrap().is_none())
    {
        assert!(Instant::now() < deadline, "read commands exceeded deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::mem::take(&mut queries.0)
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect()
}

#[test]
fn ordinary_queries_wait_for_a_writer_including_sandbox_intent_reads() {
    let temp = tempfile::tempdir().unwrap();
    let writer = Repository::init(temp.path()).unwrap();
    writer.init_vault().unwrap();
    let intent = writer
        .vault_intent_create(IntentCreateOptions {
            title: "Read after writer closes".to_string(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    let sandbox = tempfile::tempdir().unwrap();
    writer
        .provision_sandbox(sandbox.path(), writer.current_view())
        .unwrap();
    let commands: &[&[&str]] = &[
        &["intent", "list", "--json"],
        &["intent", "show", &intent.id, "--json"],
        &["intent", "list", "--json"],
        &["intent", "show", &intent.id, "--json"],
        &["status"],
        &["log"],
        &["diff"],
        &["conflicts"],
    ];
    let mut queries = Queries(
        commands
            .iter()
            .enumerate()
            .map(|(i, args)| {
                spawn_query(
                    if i == 2 || i == 3 {
                        sandbox.path()
                    } else {
                        temp.path()
                    },
                    args,
                )
            })
            .collect(),
    );
    // A real writable handle prevents all these opens. No command-side retry
    // is performed by the test; each child must wait inside its CLI opener.
    std::thread::sleep(Duration::from_millis(250));
    for child in &mut queries.0 {
        assert!(
            child.try_wait().unwrap().is_none(),
            "query failed before writer closed"
        );
    }
    drop(writer);
    for (i, output) in finish_queries(queries, Duration::from_secs(10))
        .into_iter()
        .enumerate()
    {
        assert!(
            output.status.success(),
            "{:?}: {}",
            commands[i],
            String::from_utf8_lossy(&output.stderr)
        );
        if i < 4 {
            let _: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains(&intent.id));
        }
    }
}

#[test]
fn ordinary_read_timeout_returns_failure_without_waiting_forever() {
    let temp = tempfile::tempdir().unwrap();
    let _writer = Repository::init(temp.path()).unwrap();
    let start = Instant::now();
    let outputs = finish_queries(
        Queries(vec![spawn_query(
            temp.path(),
            &["intent", "list", "--json"],
        )]),
        Duration::from_secs(20),
    );
    assert!(start.elapsed() >= Duration::from_secs(10));
    assert!(!outputs[0].status.success());
    assert!(String::from_utf8_lossy(&outputs[0].stderr).contains("Database already open"));
}

#[test]
fn ordinary_read_corruption_fails_without_contention_wait() {
    let temp = tempfile::tempdir().unwrap();
    drop(Repository::init(temp.path()).unwrap());
    std::fs::write(
        temp.path().join(".atomic/pristine.redb"),
        b"corrupt database",
    )
    .unwrap();
    let outputs = finish_queries(
        Queries(vec![spawn_query(
            temp.path(),
            &["intent", "list", "--json"],
        )]),
        Duration::from_secs(3),
    );
    assert!(!outputs[0].status.success());
    assert!(!String::from_utf8_lossy(&outputs[0].stderr).contains("Database already open"));
    assert!(outputs[0].stdout.is_empty());
}

#[test]
fn intent_queries_share_a_read_only_database_across_processes() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    repo.init_vault().unwrap();
    let intent = repo
        .vault_intent_create(IntentCreateOptions {
            title: "Concurrent read fixture".to_string(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    let view = repo.current_view().to_string();
    drop(repo);

    let sandbox = tempfile::tempdir().unwrap();
    std::fs::write(
        sandbox.path().join(".atomic-sandbox"),
        serde_json::to_vec(&serde_json::json!({
            "canonical": temp.path(), "view": view,
        }))
        .unwrap(),
    )
    .unwrap();

    // Keep a reader alive throughout every child invocation, making lock
    // overlap deterministic instead of relying on scheduler timing.
    let _reader = Repository::open_readonly(temp.path()).unwrap();
    let mut children: Vec<_> = (0..8)
        .map(|index| {
            let args = if index % 2 == 0 {
                vec!["intent", "list", "--json"]
            } else {
                vec!["intent", "show", intent.id.as_str(), "--json"]
            };
            Command::new(env!("CARGO_BIN_EXE_atomic"))
                .args(args)
                .current_dir(if index < 4 {
                    temp.path()
                } else {
                    sandbox.path()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while children
        .iter_mut()
        .any(|child| child.try_wait().unwrap().is_none())
    {
        if Instant::now() > deadline {
            for child in &mut children {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("concurrent intent reads exceeded 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    for output in outputs {
        assert!(
            output.status.success(),
            "query failed with a concurrent reader: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    }
}
