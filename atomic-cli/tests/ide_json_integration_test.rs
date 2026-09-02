//! Process-level coverage for the JSON contract consumed by IDE integrations.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run atomic")
}

fn initialized_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let output = atomic(dir.path(), &["init"]);
    assert!(output.status.success(), "init failed: {output:?}");
    dir
}

#[test]
fn status_json_exposes_versioned_repository_state() {
    let dir = initialized_repo();
    std::fs::write(dir.path().join("file with spaces.txt"), b"hello\n").unwrap();

    let output = atomic(dir.path(), &["status", "--json"]);
    assert!(output.status.success(), "status failed: {output:?}");
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid status JSON");
    assert_eq!(json["schema_version"], 1);
    let canonical_root = dir.path().canonicalize().unwrap();
    assert_eq!(
        json["repository_root"],
        canonical_root.to_string_lossy().as_ref()
    );
    assert_eq!(json["view"], "dev");
    assert!(json["state"].is_string());
    assert_eq!(json["clean"], false);
    assert_eq!(json["entries"][0]["path"], "file with spaces.txt");
    assert_eq!(json["entries"][0]["status"], "untracked");
}

#[test]
fn view_list_json_marks_the_current_view() {
    let dir = initialized_repo();

    let output = atomic(dir.path(), &["view", "list", "--json"]);
    assert!(output.status.success(), "view list failed: {output:?}");
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid view JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["source"], "local");
    assert_eq!(json["current_view"], "dev");
    assert_eq!(json["views"][0]["name"], "dev");
    assert_eq!(json["views"][0]["current"], true);
    assert_eq!(json["views"][0]["scope"], "shared");
}

#[test]
fn json_and_short_output_cannot_be_combined() {
    let dir = initialized_repo();

    for args in [
        &["status", "--short", "--json"][..],
        &["view", "list", "--short", "--json"][..],
    ] {
        let output = atomic(dir.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("error: conflicting-args"),
            "{args:?}: {output:?}"
        );
    }
}
