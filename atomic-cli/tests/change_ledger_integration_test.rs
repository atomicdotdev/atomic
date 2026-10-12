//! Stored provenance identities must survive the libatomic → CLI JSON boundary.
use std::path::Path;
use std::process::Command;

use atomic_core::change::ProvenanceGraph;
use atomic_core::types::Base32;
use atomic_repository::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn run(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .output()
        .expect("run atomic");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn change_json_preserves_stored_provenance_hashes_and_session_chain() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    drop(Repository::init(root.path()).unwrap());
    std::fs::write(root.path().join("hello.txt"), "hello\n").unwrap();
    run(
        root.path(),
        home.path(),
        &[
            "record",
            "-a",
            "-m",
            "hello",
            "--author",
            "Test <test@example.invalid>",
        ],
    );
    let history: Value =
        serde_json::from_str(&run(root.path(), home.path(), &["log", "--format", "json"])).unwrap();
    let change = history[0]["hash"].as_str().unwrap();
    let change_hash = atomic_core::types::Hash::from_base32(change.as_bytes()).unwrap();
    let args = ["change", change, "--format", "json"];

    // A change with no provenance must still render successfully.
    let empty: Value = serde_json::from_str(&run(root.path(), home.path(), &args)).unwrap();
    assert!(empty
        .get("ledger")
        .is_none_or(|ledger| ledger.as_array().unwrap().is_empty()));

    let (first_hash, second_hash) = {
        let repo = Repository::open(root.path()).unwrap();
        let first = ProvenanceGraph::builder("session-hash-regression", "opencode")
            .add_change_explained(change_hash)
            .build();
        let first_hash = repo.save_provenance_graph(&first).unwrap();
        let second = ProvenanceGraph::builder("session-hash-regression", "opencode")
            .add_change_explained(change_hash)
            .previous(first_hash)
            .build();
        let second_hash = repo.save_provenance_graph(&second).unwrap();
        (first_hash, second_hash)
    };
    assert_ne!(first_hash, second_hash);

    let json: Value = serde_json::from_str(&run(root.path(), home.path(), &args)).unwrap();
    let ledger = json["ledger"].as_array().unwrap();
    assert_eq!(ledger.len(), 2);
    let first = ledger
        .iter()
        .find(|entry| entry["graph_hash"] == first_hash.to_base32())
        .expect("JSON must contain the first stored graph's actual hash");
    let second = ledger
        .iter()
        .find(|entry| entry["graph_hash"] == second_hash.to_base32())
        .expect("JSON must contain the second stored graph's actual hash");
    assert!(first["previous"].is_null());
    assert_eq!(second["previous"], first["graph_hash"]);

    // Consumers can use every returned identity to load the matching graph.
    let repo = Repository::open(root.path()).unwrap();
    for entry in ledger {
        let hash =
            atomic_core::types::Hash::from_base32(entry["graph_hash"].as_str().unwrap().as_bytes())
                .unwrap();
        let graph = repo.load_provenance_graph(&hash).unwrap();
        assert_eq!(entry["session_id"], graph.session_id);
        assert_eq!(graph.changes_explained, vec![change_hash]);
        assert_eq!(entry["changes_explained"], serde_json::json!([change]));
    }
}
