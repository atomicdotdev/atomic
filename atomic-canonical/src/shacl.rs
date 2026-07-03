//! Tier-2 formal validation: real W3C SHACL over the JSON-LD projection.
//!
//! The Rust gate (`gate.rs`) is tier 1 — fast, in-process, always on, the
//! agent inner loop's forcing function. This module is tier 2: it feeds the
//! node (with the `@context` inlined so the document is self-contained) and
//! `shapes/atomic-shapes.ttl` to **pyshacl**, a conformant SHACL engine over
//! rdflib. Per the team decision, we do not hand-roll a SHACL evaluator and
//! we do not bet on immature Rust SHACL crates — the formal check runs where
//! a real engine exists (status transitions, CI, circuit-breaker stages),
//! not in the interactive inner loop.
//!
//! Because pyshacl must first parse the document as JSON-LD to RDF, every
//! tier-2 run doubles as a JSON-LD conformance check of our projection: a
//! broken context or an unmapped key surfaces as missing triples and a
//! failed shape, not a silent pass.
//!
//! Engine resolution: `ATOMIC_PYSHACL` env var, else `pyshacl` on PATH.
//! Callers use [`is_available`] to decide whether tier 2 can run here.

use std::process::{Command, Stdio};

use serde_json::Value;

use crate::context;
use crate::error::{CanonicalError, Result};

/// The SHACL shapes graph (Turtle), embedded at compile time.
pub const SHAPES_TURTLE: &str = include_str!("../shapes/atomic-shapes.ttl");

/// The result of a tier-2 SHACL run.
#[derive(Debug, Clone)]
pub struct ShaclReport {
    /// `sh:conforms` — the document satisfies every shape.
    pub conforms: bool,
    /// The engine's human-readable validation report (violations, messages,
    /// source shapes). Empty violations section when conforming.
    pub report: String,
}

/// Resolve the pyshacl executable: `ATOMIC_PYSHACL`, else `pyshacl` on PATH.
fn pyshacl_cmd() -> String {
    std::env::var("ATOMIC_PYSHACL").unwrap_or_else(|_| "pyshacl".to_string())
}

/// Whether a SHACL engine is available in this environment.
pub fn is_available() -> bool {
    Command::new(pyshacl_cmd())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Validate a canonical JSON-LD value against the embedded shapes with a real
/// SHACL engine. The value's `@context` URL is replaced by the embedded term
/// map first, so the engine never needs the network.
pub fn validate_value(value: &Value) -> Result<ShaclReport> {
    let data = context::inline_context(value);
    let data_json = serde_json::to_string(&data)
        .map_err(|e| CanonicalError::Shacl(format!("failed to serialize data graph: {e}")))?;

    let dir = tempfile::tempdir()
        .map_err(|e| CanonicalError::Shacl(format!("failed to create temp dir: {e}")))?;
    let shapes_path = dir.path().join("atomic-shapes.ttl");
    let data_path = dir.path().join("data.jsonld");
    std::fs::write(&shapes_path, SHAPES_TURTLE)
        .map_err(|e| CanonicalError::Shacl(format!("failed to write shapes: {e}")))?;
    std::fs::write(&data_path, data_json)
        .map_err(|e| CanonicalError::Shacl(format!("failed to write data graph: {e}")))?;

    let output = Command::new(pyshacl_cmd())
        .arg("--shacl")
        .arg(&shapes_path)
        .args(["--shacl-file-format", "turtle"])
        .args(["--data-file-format", "json-ld"])
        .args(["--format", "human"])
        .arg(&data_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| CanonicalError::Shacl(format!("failed to run '{}': {e}", pyshacl_cmd())))?;

    let mut report = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // pyshacl prints "Conforms: True/False" in the human report and exits
    // non-zero on violations. Anything without a conforms verdict is an
    // engine/parse error (e.g. the document failed to parse as JSON-LD) and
    // must surface as an error, never as a pass.
    let conforms = if report.contains("Conforms: True") {
        true
    } else if report.contains("Conforms: False") {
        false
    } else {
        return Err(CanonicalError::Shacl(format!(
            "pyshacl produced no conformance verdict (exit: {:?})\nstdout: {}\nstderr: {}",
            output.status.code(),
            report.trim(),
            stderr.trim(),
        )));
    };

    if !stderr.trim().is_empty() {
        report.push_str("\n[stderr] ");
        report.push_str(stderr.trim());
    }

    Ok(ShaclReport { conforms, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;

    /// Skip tier-2 tests (with a note) where no engine is installed, so plain
    /// `cargo test` passes everywhere; CI/dev run them with
    /// `ATOMIC_PYSHACL=<venv>/bin/pyshacl`.
    fn engine() -> bool {
        if is_available() {
            true
        } else {
            eprintln!("shacl tests SKIPPED — no pyshacl (set ATOMIC_PYSHACL or install pyshacl)");
            false
        }
    }

    fn attested_intent(ac_attrs: &str) -> Value {
        let md = format!(
            "---\nid: WORD-5\ntitle: Add name prompt modal\nstatus: done\ncreated_at: 2026-06-25T11:24:25Z\n---\n\n:::why\nLocal-only modal over a persisted profile.\n:::\n\n:::acceptance-criterion{{#WORD-5-ac-1 {ac_attrs}}}\nOn first load, the app presents a modal.\n:::\n"
        );
        let (fm, body) = crate::lift::parse_markdown(&md).unwrap();
        let node = crate::lift::lift_intent(&fm, &body).unwrap();
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        crate::proof::attest(node, &id, &kp).to_value()
    }

    #[test]
    fn conforming_intent_passes_real_shacl() {
        if !engine() {
            return;
        }
        let value = attested_intent(
            "status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8ZE",
        );
        let report = validate_value(&value).unwrap();
        assert!(
            report.conforms,
            "expected conformance, got:\n{}",
            report.report
        );
    }

    #[test]
    fn met_criterion_without_evidence_fails_the_sparql_constraint() {
        if !engine() {
            return;
        }
        // The load-bearing rule, enforced by a real SHACL-SPARQL constraint:
        // a bare checked box cannot pass the gate.
        let value = attested_intent("status=met");
        let report = validate_value(&value).unwrap();
        assert!(
            !report.conforms,
            "a met AC without evidence must not conform"
        );
        assert!(
            report.report.contains("verifiedBy and evidence"),
            "violation must come from the met-needs-evidence constraint:\n{}",
            report.report
        );
    }

    #[test]
    fn unknown_status_fails_sh_in() {
        if !engine() {
            return;
        }
        let mut value = attested_intent("status=open");
        value["status"] = serde_json::json!("shipped");
        let report = validate_value(&value).unwrap();
        assert!(!report.conforms);
        assert!(report.report.contains("known states"), "{}", report.report);
    }

    #[test]
    fn task_without_status_fails_task_shape() {
        if !engine() {
            return;
        }
        let md = "---\nid: WORD-5\ntitle: t\nstatus: done\ncreated_at: 2026-06-25T11:24:25Z\n---\n\n:::why\na reason\n:::\n\n:::task{#WORD-5-1 status=done}\nAdd modal.\n:::\n";
        let (fm, body) = crate::lift::parse_markdown(md).unwrap();
        let node = crate::lift::lift_intent(&fm, &body).unwrap();
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        let mut value = crate::proof::attest(node, &id, &kp).to_value();

        let report = validate_value(&value).unwrap();
        assert!(
            report.conforms,
            "task with status conforms:\n{}",
            report.report
        );

        // Strip the task's status: TaskShape's cardinality must reject it.
        value["hasTask"][0]
            .as_object_mut()
            .unwrap()
            .remove("taskStatus");
        let report = validate_value(&value).unwrap();
        assert!(!report.conforms, "a task without a status must not conform");
        assert!(
            report.report.contains("exactly one status"),
            "violation must come from TaskShape:\n{}",
            report.report
        );
    }

    #[test]
    fn memory_with_unknown_kind_fails() {
        if !engine() {
            return;
        }
        let md = "---\nid: mem-1\nkind: constraint\nstatus: active\ncreated_at: 2026-05-02T09:14:00Z\n---\n\n:::memory\nNo single ordering authority.\n:::\n";
        let (fm, body) = crate::lift::parse_markdown(md).unwrap();
        let node = crate::memory::lift_memory(&fm, &body).unwrap();
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        let mut value = crate::proof::attest_memory(node, &id, &kp).to_value();

        let report = validate_value(&value).unwrap();
        assert!(
            report.conforms,
            "valid memory must conform:\n{}",
            report.report
        );

        value["memoryKind"] = serde_json::json!("vibe");
        let report = validate_value(&value).unwrap();
        assert!(!report.conforms, "unknown memory kind must not conform");
    }

    #[test]
    fn prov_graph_conforms_and_is_real_rdf() {
        if !engine() {
            return;
        }
        // The PROV projection parses as JSON-LD (rdflib) and satisfies the
        // Activity/SoftwareAgent shapes — the delegation chain included.
        let g = crate::prov::provenance_graph(&crate::prov::ProvenanceInput {
            session_id: "inner-1".into(),
            agent_name: "claude-code".into(),
            agent_display_name: "Claude Code".into(),
            agent_vendor: "anthropic".into(),
            model: "claude-fable-5".into(),
            started_at: "2026-07-01T22:11:00Z".into(),
            ended_at: Some("2026-07-01T22:14:00Z".into()),
            view: Some("sherpa-run-x".into()),
            change_hashes: vec!["W5GSLAVO".into()],
            used: vec!["urn:atomic:intent:019efe85".into()],
            managed_run: Some(crate::prov::ManagedRunInput {
                run_id: "run-1".into(),
                owner_agent: "sherpa".into(),
                owner_session_id: "sherpa-s1".into(),
                work_item_id: Some("NONA-7".into()),
            }),
            person: Some("did:atomic:lee".into()),
        });
        let report = validate_value(&g).unwrap();
        assert!(
            report.conforms,
            "PROV graph must conform:\n{}",
            report.report
        );
    }

    #[test]
    fn prov_activity_missing_agent_fails() {
        if !engine() {
            return;
        }
        let g = serde_json::json!({
            "@context": crate::node::CONTEXT_URL,
            "@id": "urn:atomic:provgraph:session:x",
            "@graph": [{
                "@type": "Activity",
                "@id": "urn:atomic:activity:session:x",
                "startedAtTime": "2026-07-01T22:11:00Z"
            }]
        });
        let report = validate_value(&g).unwrap();
        assert!(
            !report.conforms,
            "an activity with no agent must not conform"
        );
        assert!(
            report.report.contains("associated with an agent"),
            "{}",
            report.report
        );
    }
}
