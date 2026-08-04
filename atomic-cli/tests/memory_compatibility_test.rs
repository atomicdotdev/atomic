//! Compatibility coverage for freeform memories.
#![cfg(not(windows))]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .output()
        .expect("run atomic")
}

#[test]
fn memory_show_reads_written_and_default_freeform_memories() {
    let repo_tmp = TempDir::new().unwrap();
    let home_tmp = TempDir::new().unwrap();
    let repo_dir = repo_tmp.path();
    let home_dir = home_tmp.path();

    let init = atomic(repo_dir, home_dir, &["init"]);
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let mut child = Command::new(ATOMIC_BIN)
        .args(["memory", "write", "notes", "--type", "reference"])
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn memory write");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"# Notes\nKeep this context.\n")
        .unwrap();
    let write = child.wait_with_output().unwrap();
    assert!(
        write.status.success(),
        "memory write failed: {}",
        String::from_utf8_lossy(&write.stderr)
    );

    let show = atomic(repo_dir, home_dir, &["memory", "show", "notes"]);
    assert!(
        show.status.success(),
        "memory show failed: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert_eq!(
        String::from_utf8(show.stdout).unwrap(),
        "# Notes\nKeep this context.\n"
    );

    let show_json = atomic(repo_dir, home_dir, &["memory", "show", "notes", "--json"]);
    assert!(show_json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&show_json.stdout).unwrap();
    assert_eq!(value["path"], "memory/notes.md");
    assert_eq!(value["content"], "# Notes\nKeep this context.\n");
    assert_eq!(value["frontmatter"]["name"], "notes");
    assert_eq!(value["frontmatter"]["type"], "reference");

    let default = atomic(repo_dir, home_dir, &["memory", "show", "MEMORY"]);
    assert!(
        default.status.success(),
        "default memory show failed: {}",
        String::from_utf8_lossy(&default.stderr)
    );
    assert!(String::from_utf8(default.stdout)
        .unwrap()
        .contains("# Project Memory"));
}
