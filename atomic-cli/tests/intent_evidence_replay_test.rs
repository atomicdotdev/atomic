//! Native CLI integration controls. The checker here is a synthetic trusted
//! protocol peer; the optional example has separate real semantic-adapter tests.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use atomic_canonical::lift_and_attest;
use atomic_identity::{Identity, KeyPair};
use atomic_repository::{IntentCreateOptions, IntentUpdateOptions, Repository};
use serde_json::{json, Value};

fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn checker(body: &str) -> tempfile::TempPath {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "#!/bin/sh\nset -e\n{body}\n").unwrap();
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .unwrap();
    file.into_temp_path()
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn signed_fixture(root: &Path) -> (String, String) {
    let repo = Repository::init(root).unwrap();
    repo.init_vault().unwrap();
    let created = repo
        .vault_intent_create(IntentCreateOptions {
            title: "Retain selected source passages".into(),
            priority: None,
            assignee: None,
            labels: Vec::new(),
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    repo.vault_intent_update(&created.id, IntentUpdateOptions {
        content: Some(format!(":::why\nKeep a reviewable retained report.\n:::\n\n:::acceptance-criterion{{#{}-ac-1 status=unmet}}\nSelected passages remain in the retained report.\n:::\n", created.uid)),
        force: true, ..Default::default()
    }).unwrap();
    let entry = repo.vault_intent_show(&created.id).unwrap();
    let fm: serde_json::Map<String, Value> = serde_json::from_str(&entry.frontmatter_json).unwrap();
    let body = String::from_utf8(entry.content_bytes).unwrap();
    let key = KeyPair::generate();
    let identity = Identity::new("synthetic-reviewer", &key);
    let node = lift_and_attest(&fm, &body, &identity, &key).unwrap();
    // Native legacy sidecar format, with the same source anchor as the tracked
    // path. No globally installed identity/configuration is needed by the test.
    let mut hash = blake3::Hasher::new();
    hash.update(serde_json::to_string(&fm).unwrap().as_bytes());
    hash.update(b"\0");
    hash.update(body.as_bytes());
    let safe: String = created
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = repo.dot_dir().join("canonical/intents").join(safe);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("attested.jsonld"), serde_json::to_vec(&json!({
        "node":node, "source":{"sourceContentHash":format!("blake3:{}",hash.finalize().to_hex())}
    })).unwrap()).unwrap();
    (created.id, node.has_acceptance_criterion[0].id.clone())
}

#[test]
fn replay_is_opt_in_bound_to_native_state_and_does_not_mark_a_criterion_met() {
    let temp = tempfile::tempdir().unwrap();
    let (id, criterion_id) = signed_fixture(temp.path());
    let ordinary = cli(temp.path(), &["intent", "validate", &id, "--json"]);
    assert!(
        ordinary.status.success(),
        "{}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
    let ordinary_report: Value = serde_json::from_slice(&ordinary.stdout).unwrap();
    assert!(ordinary_report.get("evidence_replay").is_none());
    let captured = cli(
        temp.path(),
        &["intent", "validate", &id, "--json", "--evidence-context"],
    );
    assert!(captured.status.success());
    let context =
        serde_json::from_slice::<Value>(&captured.stdout).unwrap()["evidence_context"].clone();
    let claim = json!({"criterion_id":criterion_id,"claim_type":"source_text_coverage/v1",
        "artifact_root":".","case":{},"policy":{}});
    let path = temp.path().join("manifest.json");
    let mut manifest = json!({"schema_version":"atomic-evidence-replay-manifest/v1",
        "context":context,"claims":[claim]});
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let mut resolved = claim;
    resolved["artifact_root"] = json!(temp.path().canonicalize().unwrap());
    let digest = atomic_canonical::hash::content_hash(&json!({
        "schema_version":"atomic-evidence-replay-request/v1","context":context,"claim":resolved
    }));
    let response = json!({"schema_version":"atomic-evidence-replay-response/v1", "request_digest":digest,
        "criterion_id":criterion_id, "claim_type":"source_text_coverage/v1", "decision":"supported",
        "reason":"synthetic_protocol_control", "details":{"scope":"synthetic checker; not a semantic result"}});
    let body = format!(
        "cat <<'RESPONSE'\n{}\nRESPONSE",
        serde_json::to_string(&response).unwrap()
    );
    let program = checker(&body);
    let args = [
        "intent",
        "validate",
        &id,
        "--json",
        "--replay-evidence",
        path.to_str().unwrap(),
        "--evidence-checker",
        program.to_str().unwrap(),
    ];
    let output = cli(temp.path(), &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["results"], ordinary_report["results"]);
    assert_eq!(
        report["evidence_replay"]["decisions"][0]["decision"],
        "supported"
    );
    let shown = cli(temp.path(), &["intent", "show", &id, "--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&shown.stdout).unwrap()["hasAcceptanceCriterion"][0]
            ["acStatus"],
        "unmet"
    );

    manifest["context"]["view_chain"][0]["merkle"] = json!("stale");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let stale = cli(temp.path(), &args);
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale intent/view pins"));
    manifest["context"] = context;
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    // The checker successfully returns an originally bound supported result,
    // but edits the native intent first. The result must lose its fresh status.
    let mutator = checker(&format!(
        "{} intent update {} --title 'Changed during replay' >/dev/null\n{body}",
        quote(env!("CARGO_BIN_EXE_atomic")),
        quote(&id)
    ));
    let changed = cli(
        temp.path(),
        &[
            "intent",
            "validate",
            &id,
            "--json",
            "--replay-evidence",
            path.to_str().unwrap(),
            "--evidence-checker",
            mutator.to_str().unwrap(),
        ],
    );
    assert_eq!(
        changed.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let report: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(report["conforms"], false);
    assert!(report["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["shape"] == "EvidenceReplayFreshness"));
}
