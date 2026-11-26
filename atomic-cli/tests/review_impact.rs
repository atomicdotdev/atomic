//! Exercise the shipped command against a real, tiny recorded repository.

use std::process::Command;

use atomic_core::{change::ChangeHeader, types::Base32};
use atomic_repository::{record::RecordOptions, tracking::TrackingOptions, Repository};

#[test]
fn recorded_contract_breakage_is_assessed_without_supplied_findings_or_pipe_animation() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.init_vault().unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    let path = directory.path().join("src/lib.rs");
    std::fs::write(&path, "fn work(x:i32) {} fn consumer() { work(1); }\n").unwrap();
    repo.add(wc, "src/lib.rs", TrackingOptions::default())
        .unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .enrich_kg(false)
    };
    repo.record(wc, ChangeHeader::new("initial"), options())
        .unwrap();
    std::fs::write(
        &path,
        "fn work(x:i32,y:i32) {} fn consumer() { work(1); }\n",
    )
    .unwrap();
    let hash = repo
        .record(wc, ChangeHeader::new("change arity"), options())
        .unwrap()
        .hash()
        .to_base32();
    let database = repo.dot_dir().join("atomic.redb");
    drop(repo);
    let database_before = std::fs::read(&database).unwrap();
    for json in [true, false] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .current_dir(directory.path())
            .env_remove("ATOMIC_SESSION_ID")
            .args(["review", "--change", &hash]);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            !text.contains("Thinking")
                && !String::from_utf8_lossy(&output.stderr).contains("Thinking")
        );
        assert!(
            !text.contains('\u{1b}'),
            "captured output cannot contain spinner/ANSI frames"
        );
        if json {
            let report: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(report["graph"]["risk"]["assessment"]["rating"], "high");
            assert_eq!(
                report["graph"]["risk"]["impact"][0]["checked_call_contracts"],
                1
            );
            assert_eq!(
                report["graph"]["risk"]["impact"][0]["issues"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                report["graph"]["findings"][0]["finding"]["requirement_or_rule"]["id"],
                "compatibility.resolved-direct-call-contract"
            );
        } else {
            assert!(text.contains("Risk: High") && text.contains("1 current bound caller(s)"));
            assert!(text.contains("F1") && text.contains("1 incompatible after-contract(s)"));
        }
    }
    assert_eq!(
        database_before,
        std::fs::read(database).unwrap(),
        "review remains read-only"
    );
}
