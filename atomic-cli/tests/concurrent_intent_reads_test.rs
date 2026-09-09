use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomic_repository::{IntentCreateOptions, Repository};

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
