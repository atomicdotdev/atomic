//! Abrupt death between record publication and checkpoint publication must not
//! lose the change-to-turn link or absorb working-copy edits made after death.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use atomic_repository::Repository;

fn hook(root: &Path, verb: &str, failpoint: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    command
        .args([
            "agent",
            "hooks",
            "codex",
            verb,
            "--foreground",
            "--no-color",
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(point) = failpoint {
        command
            .env("ATOMIC_PUBLICATION_FAILPOINT", point)
            .env("ATOMIC_PUBLICATION_FAILPOINT_ACTION", "exit");
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&serde_json::json!({
                "session_id": "publication-recovery",
                "cwd": root,
                "prompt": "Record the requested source change",
                "last_assistant_message": "Implemented the source change"
            }))
            .unwrap(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("hook {verb} timed out: {:?}", child.wait_with_output());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn committed_record_is_recovered_before_checkpoint_or_next_prompt() {
    for point in [
        "record-after-commit",
        "checkpoint-after-object",
        "checkpoint-after-ledger",
        "checkpoint-before-commit",
        "checkpoint-after-commit",
    ] {
        for (newer_edits, retry) in [
            (false, "stop"),
            (true, "stop"),
            (true, "user-prompt-submit"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            drop(Repository::init(dir.path()).unwrap());
            for verb in ["session-start", "user-prompt-submit"] {
                let result = hook(dir.path(), verb, None);
                assert!(result.status.success(), "{result:?}");
            }
            let source = dir.path().join("source.rs");
            std::fs::write(&source, "fn committed() {}\n").unwrap();
            let crashed = hook(dir.path(), "stop", Some(point));
            assert_eq!(crashed.status.code(), Some(86), "{point}: {crashed:?}");

            // Read canonical state using a new database handle, even when the
            // process exited before exporting the change or saving session JSON.
            let repo = Repository::open_existing(dir.path()).unwrap();
            let history = repo.log(Default::default()).unwrap();
            assert_eq!(history.len(), 1, "{point}");
            let original = history[0].hash;
            drop(repo);
            if newer_edits {
                std::fs::write(&source, "fn later_edit() {}\n").unwrap();
            }
            let recovered = hook(dir.path(), retry, None);
            assert!(recovered.status.success(), "{point} {retry}: {recovered:?}");
            let repo = Repository::open_existing(dir.path()).unwrap();
            let (_, turns) = repo
                .get_session_ledger("publication-recovery")
                .unwrap()
                .unwrap();
            assert_eq!(turns.len(), 1, "{point} {retry}");
            assert_eq!(turns[0].change_hashes, vec![original], "{point} {retry}");
            let graph = repo
                .load_provenance_graph(&turns[0].provenance_hash)
                .unwrap();
            assert_eq!(graph.changes_explained, vec![original]);
            assert_eq!(repo.log(Default::default()).unwrap().len(), 1);
            assert_eq!(
                repo.get_file_content("source.rs").unwrap().unwrap(),
                b"fn committed() {}\n"
            );
            if newer_edits {
                assert_eq!(
                    std::fs::read_to_string(&source).unwrap(),
                    "fn later_edit() {}\n"
                );
                assert!(!repo.status(Default::default()).unwrap().is_clean());
            } else {
                assert!(repo.status(Default::default()).unwrap().is_clean());
            }
            drop(repo);
            if retry == "stop" {
                for _ in 0..2 {
                    let result = hook(dir.path(), "stop", None);
                    assert!(result.status.success(), "{result:?}");
                }
                let repo = Repository::open_existing(dir.path()).unwrap();
                assert_eq!(repo.log(Default::default()).unwrap().len(), 1);
                assert_eq!(
                    repo.get_session_ledger("publication-recovery")
                        .unwrap()
                        .unwrap()
                        .1
                        .len(),
                    1
                );
            }
        }
    }
}
