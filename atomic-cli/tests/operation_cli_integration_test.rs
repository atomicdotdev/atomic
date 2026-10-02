use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .output()
        .expect("run atomic binary")
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "`{command}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn operation_log_and_show_inspect_a_real_switch_operation() {
    let repository = TempDir::new().expect("create temporary repository");
    let root = repository.path();

    let init = atomic(root, &["init"]);
    assert_success(&init, "atomic init");
    let create = atomic(
        root,
        &["view", "create", "feature", "--draft", "--parent", "dev"],
    );
    assert_success(&create, "atomic view create feature --draft --parent dev");
    let switch = atomic(root, &["view", "switch", "feature"]);
    assert_success(&switch, "atomic view switch feature");

    let log = atomic(root, &["op", "log", "--json"]);
    assert_success(&log, "atomic op log --json");
    let log_json: Value = serde_json::from_slice(&log.stdout).expect("valid log JSON");
    assert_eq!(log_json["scope"]["kind"], "working_copy");
    assert_eq!(log_json["head_state"]["state"], "single");
    let head = log_json["head_state"]["head"]
        .as_str()
        .expect("full operation head")
        .to_string();
    assert_eq!(head.len(), 52, "operation IDs must not be abbreviated");
    let entries = log_json["entries"].as_array().expect("operation entries");
    assert!(
        !entries.is_empty(),
        "switch should create operation history"
    );
    assert!(entries.iter().any(|entry| {
        entry["id"].as_str() == Some(head.as_str()) && entry["is_head"] == Value::Bool(true)
    }));

    let human_log = atomic(root, &["op", "log", "-n", "1"]);
    assert_success(&human_log, "atomic op log -n 1");
    let human_log = String::from_utf8_lossy(&human_log.stdout);
    assert!(human_log.contains(&format!("HEAD {head}")));

    let prefix = head[..8].to_ascii_lowercase();
    let show = atomic(root, &["op", "show", &prefix, "--json"]);
    assert_success(&show, "atomic op show <lowercase-prefix> --json");
    let show_json: Value = serde_json::from_slice(&show.stdout).expect("valid show JSON");
    assert_eq!(show_json["id"], head);
    assert!(show_json["parents"].is_array());
    assert!(show_json["kind"].is_string());
    assert!(show_json["scope"].is_object());
    assert!(show_json["actor"].is_object());
    assert!(show_json["timestamp_ms"].is_i64());
    assert!(show_json["verification"].is_string());
    assert!(show_json["before"].is_object());
    assert!(show_json["delta"]["after"].is_object());
    assert!(show_json["delta"]["effects"].is_array());
    assert!(show_json["receipts"].is_array());
    assert!(show_json["git_observed"].is_array());
    assert!(show_json["evidence"].is_array());
    assert!(show_json["loss_notes"].is_array());
    assert!(show_json["head_scopes"].is_array());

    let human_show = atomic(root, &["op", "show", &prefix]);
    assert_success(&human_show, "atomic op show <lowercase-prefix>");
    let human_show = String::from_utf8_lossy(&human_show.stdout);
    for section in [
        "Parents:",
        "Kind:",
        "Scope:",
        "Actor:",
        "Verification:",
        "Head scopes:",
        "Before:",
        "Delta:",
        "effects:",
        "Evidence:",
        "Loss notes:",
        "Receipts:",
    ] {
        assert!(
            human_show.contains(section),
            "human show omitted {section:?}:\n{human_show}"
        );
    }
}

#[test]
fn operation_undo_and_restore_preserve_switch_content() {
    let repository = TempDir::new().expect("create temporary repository");
    let root = repository.path();
    assert_success(&atomic(root, &["init"]), "atomic init");

    std::fs::write(root.join("state.txt"), b"dev content\n").expect("write dev content");
    assert_success(&atomic(root, &["add", "state.txt"]), "atomic add state.txt");
    assert_success(
        &atomic(root, &["record", "-m", "record dev content"]),
        "atomic record dev content",
    );
    assert_success(
        &atomic(
            root,
            &["view", "create", "feature", "--draft", "--parent", "dev"],
        ),
        "atomic view create feature",
    );
    assert_success(
        &atomic(root, &["view", "switch", "feature"]),
        "atomic view switch feature",
    );
    std::fs::write(root.join("state.txt"), b"feature content\n").expect("write feature content");
    assert_success(
        &atomic(root, &["record", "-m", "record feature content"]),
        "atomic record feature content",
    );
    assert_success(
        &atomic(root, &["view", "switch", "dev"]),
        "atomic view switch dev",
    );
    assert_eq!(
        std::fs::read(root.join("state.txt")).expect("read dev content"),
        b"dev content\n"
    );

    let log = atomic(root, &["op", "log", "-n", "1", "--json"]);
    assert_success(&log, "atomic op log -n 1 --json");
    let log_json: Value = serde_json::from_slice(&log.stdout).expect("valid operation log JSON");
    let switch_id = log_json["entries"][0]["id"]
        .as_str()
        .expect("switch operation ID")
        .to_string();
    assert_eq!(log_json["entries"][0]["kind"], "switch_view");

    let undo = atomic(root, &["op", "undo", "--json"]);
    assert_success(&undo, "atomic op undo --json");
    let undo_json: Value = serde_json::from_slice(&undo.stdout).expect("valid undo JSON");
    assert_eq!(undo_json["kind"], "undo");
    assert_eq!(undo_json["encoding_version"], 2);
    assert_eq!(undo_json["relation"]["kind"], "undo");
    assert_eq!(undo_json["relation"]["target"], switch_id);
    assert_eq!(
        std::fs::read(root.join("state.txt")).expect("read feature content after undo"),
        b"feature content\n"
    );

    let restore_prefix = switch_id[..8].to_ascii_lowercase();
    let restore = atomic(root, &["op", "restore", &restore_prefix, "--json"]);
    assert_success(&restore, "atomic op restore <switch> --json");
    let restore_json: Value = serde_json::from_slice(&restore.stdout).expect("valid restore JSON");
    assert_eq!(restore_json["kind"], "restore");
    assert_eq!(restore_json["encoding_version"], 2);
    assert_eq!(restore_json["relation"]["kind"], "restore");
    assert_eq!(restore_json["relation"]["target"], switch_id);
    assert_eq!(
        std::fs::read(root.join("state.txt")).expect("read dev content after restore"),
        b"dev content\n"
    );
}
