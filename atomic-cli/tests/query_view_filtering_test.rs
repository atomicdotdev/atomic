//! CLI presentation and routing for the three view-filtered query verbs.

use std::process::{Command, Output};

use atomic_core::change::ChangeHeader;
use atomic_repository::{RecordOptions, Repository, TrackingOptions};
use serde_json::Value;

fn run(dir: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(dir)
        .env("ATOMIC_SERVICE", "local")
        .env("HOME", home)
        .output()
        .unwrap()
}

fn json(dir: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Value {
    let output = run(dir, home, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn record(repo: &Repository, message: &str) {
    repo.record(
        repo.require_working_copy_id().unwrap(),
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
}

#[test]
fn query_verbs_render_filtered_json_and_human_output_over_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\n",
    )
    .unwrap();
    repo.add(wc, "src/lib.rs", TrackingOptions::default())
        .unwrap();
    record(&repo, "base");
    repo.kg_enrich_from_vcs(wc).unwrap();
    repo.create_draft_view("feature", "dev").unwrap();
    repo.switch_view(wc, "feature").unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn target(secret: SiblingSecret) {}\npub fn caller() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/sibling.rs"),
        "pub fn sibling_only() { target(); }\n",
    )
    .unwrap();
    repo.add(wc, "src/sibling.rs", TrackingOptions::default())
        .unwrap();
    record(&repo, "sibling");
    repo.kg_enrich_from_vcs(wc).unwrap();
    // No content index is required by the view-scoped code/search paths.
    repo.switch_view(wc, "dev").unwrap();
    drop(repo);
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn disk_only() {}\n").unwrap();

    for verb in ["search", "code"] {
        let result = json(
            dir.path(),
            home.path(),
            &["query", verb, "SiblingSecret", "--json"],
        );
        if verb == "search" {
            assert_eq!(result, serde_json::json!([]));
        } else {
            assert_eq!(result["matches"], serde_json::json!([]));
            assert_eq!(result["total_matches"], 0);
            assert_eq!(result["dir_facets"], serde_json::json!([]));
        }
    }
    let result = json(
        dir.path(),
        home.path(),
        &[
            "query",
            "search",
            "target",
            "--kind",
            "https://atomic.dev/ns/workflow#File",
            "--json",
        ],
    );
    assert_eq!(result.as_array().unwrap().len(), 1);
    assert_eq!(result[0]["metadata"]["path"], "src/lib.rs");
    let result = json(
        dir.path(),
        home.path(),
        &["query", "callers", "src/lib.rs#target", "--json"],
    );
    assert_eq!(result.as_array().unwrap().len(), 1);
    assert_eq!(result[0]["path"], "src/lib.rs");
    assert_eq!(result[0]["name"], "caller");
    assert_eq!(result[0]["line"], 2);
    let result = json(
        dir.path(),
        home.path(),
        &[
            "query", "code", "TARGET", "-i", "-g", "src/", "-t", "rs", "-n", "1", "--json",
        ],
    );
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert_eq!(result["matches"][0]["path"], "src/lib.rs");
    assert_eq!(result["matches"][0]["line_number"], 1);
    assert_eq!(result["matches"][0]["byte_offset"], 7);
    assert_eq!(result["total_matches"], 2);
    let output = run(dir.path(), home.path(), &["query", "code", "target"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("src/lib.rs:1: pub fn target() {}"), "{text}");
    assert!(text.contains("2 match(es) in 1 file(s)."), "{text}");
    let output = run(dir.path(), home.path(), &["query", "code", "["]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid search pattern"));
    assert!(!dir
        .path()
        .join(".atomic/content-index/manifest.json")
        .exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "pub fn disk_only() {}\n"
    );
}
