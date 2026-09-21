//! CB-12A AC3 e2e: `atomic agent repair <session>` resumes an incomplete
//! session under the workspace lease while RETAINING every piece of durable
//! evidence — the incomplete refusal is snapshotted verbatim into an
//! append-only repair note, the retention lease is set, and repair never
//! erases or manufactures attribution.

use std::fs;
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .output()
        .expect("run atomic")
}

fn atomic_ok(root: &std::path::Path, home: &std::path::Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn repair_resumes_an_incomplete_session_and_retains_evidence() {
    let root = tempfile::tempdir().unwrap().keep();
    let home = tempfile::tempdir().unwrap().keep();

    atomic_ok(&root, &home, &["init"]);

    // Forge a durably incomplete session (the shape the orchestrator writes
    // when a Git transition lacks authenticated capture).
    let sessions_dir = root.join(".atomic").join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let session_path = sessions_dir.join("sess-repair-e2e.json");
    fs::write(
        &session_path,
        serde_json::json!({
            "session_id": "sess-repair-e2e",
            "view_name": "agent-sess-repair-e2e",
            "phase": "idle",
            "status": {
                "incomplete": {
                    "reason": "unexplained Git transition (e2e fixture)",
                    "paths": [],
                    "recovery_ref": "",
                    "origin": "unattributed_git_operation",
                    "unbound_commits": ["0123456789abcdef0123456789abcdef01234567"]
                }
            },
            "turn_count": 1,
            "agent_name": "claude-code",
            "agent_display_name": "Claude Code",
            "boundary_start": null,
            "boundary_end": null,
            "turn_outcomes": [],
            "first_prompt": "",
            "started_at": "2026-09-18T00:00:00Z",
            "current_turn_started_at": null,
            "mac_key": null,
            "attested_operations": [],
            "last_attestation": null
        })
        .to_string(),
    )
    .unwrap();

    let out = atomic_ok(&root, &home, &["agent", "repair", "sess-repair-e2e"]);
    assert!(
        out.contains("evidence retained"),
        "the repair reports retained evidence: {out}"
    );

    // The durable session JSON: resumed + lease + append-only note with the
    // verbatim refusal.
    let raw = fs::read_to_string(&session_path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["status"].as_str(), Some("active"));
    assert_eq!(value["evidence_retained"].as_bool(), Some(true));
    let note = &value["repair_history"][0];
    assert_eq!(note["action"].as_str(), Some("resume"));
    assert_eq!(note["prior_status"].as_str(), Some("incomplete"));
    let retained = &note["retained_incomplete"];
    assert_eq!(
        retained["reason"].as_str(),
        Some("unexplained Git transition (e2e fixture)"),
        "the retained evidence is byte-for-byte the prior refusal"
    );
    assert_eq!(
        retained["unbound_commits"][0].as_str(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );

    // Verify-only: no state changes.
    let before = fs::read_to_string(&session_path).unwrap();
    atomic_ok(
        &root,
        &home,
        &["agent", "repair", "sess-repair-e2e", "--verify-only"],
    );
    let after = fs::read_to_string(&session_path).unwrap();
    assert_eq!(before, after, "verify-only never rewrites the session");

    // A missing session refuses typed, never invents one.
    let missing = atomic(&root, &home, &["agent", "repair", "no-such-session"]);
    assert!(
        !missing.status.success(),
        "repair of a missing session must fail"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(
        text.contains("not found"),
        "the refusal names the missing session: {text}"
    );
}
