//! The TriageService's report op, driven in-process: the canonical triage
//! report through `GenerateTriageReview`, exactly as a host's transport
//! would carry it — but with the transport replaced by a direct call into
//! `TriageImpl`.
//!
//! What is asserted is the wire shape the callers render from: the
//! `atomic.triage.report.v1` bundle parses to the report model, carries a
//! verdict, and reaches the same view pair the request named (the same
//! resolution defaults the CLI's local body applies). The report's own
//! substance is covered by the repository layer's `triage::report` tests —
//! this is the service seam.

// tonic's `Status` is the handlers' error type, as in `daemon`.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use atomic_core::change::Author;
use atomic_repository::{RecordOptions, Repository, TrackingOptions};
use libatomic::daemon::services_triage::TriageImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tonic::{Code, Request};

use atomic_core::change::ChangeHeader;
use atomic_repository::intent_evidence;
use libatomic::atomic::triage_service_server::TriageService;
use libatomic::atomic::{GenerateTriageReviewRequest, RepositoryRef, VersionedBytes};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A repository with one change on `dev` and one on a draft `feat`.
fn host() -> (TempDir, std::path::PathBuf, Arc<DaemonState>, RepositoryRef) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    {
        let mut repo = Repository::init(&root).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        write(&root, "README.md", "hello\n");
        repo.add(working_copy, "README.md", TrackingOptions::default())
            .unwrap();
        let header = ChangeHeader::builder()
            .message("first")
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let outcome = repo
            .record(
                working_copy,
                header,
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        assert!(outcome.was_applied());

        repo.create_view_from("feat", "dev").unwrap();
        repo.switch_view(working_copy, "feat").unwrap();
        write(&root, "src/lib.rs", "pub fn f() {}\n");
        repo.add(working_copy, "src/lib.rs", TrackingOptions::default())
            .unwrap();
        let header = ChangeHeader::builder()
            .message("a feature")
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let outcome = repo
            .record(
                working_copy,
                header,
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();
        assert!(outcome.was_applied());
        repo.switch_view(working_copy, "dev").unwrap();
    }
    let state = Arc::new(DaemonState::new());
    let root = root.canonicalize().unwrap();
    let repository = state.register(root.clone()).repository_ref();
    (dir, root, state, repository)
}

#[tokio::test]
async fn the_review_arrives_as_the_wire_report_shape() {
    let (_dir, _root, state, repository) = host();
    let triage = TriageImpl { state };

    let response = triage
        .generate_triage_review(Request::new(GenerateTriageReviewRequest {
            repository: Some(repository),
            from_view: "feat".into(),
            to_view: "dev".into(),
            report: None,
            evidence_reports: Vec::new(),
        }))
        .await
        .expect("the handler serves the report");
    let response = response.into_inner();
    let bundle = response
        .report
        .expect("the response carries the report bundle");
    assert_eq!(bundle.schema, "atomic.triage.report.v1");
    let report: atomic_repository::triage::TriageReport =
        serde_json::from_slice(&bundle.payload).expect("the bundle parses as the model");
    assert_eq!(
        (
            report.inputs.feature.as_str(),
            report.inputs.target.as_str()
        ),
        ("feat", "dev"),
        "the named pair, resolved"
    );
    // The candidate set is the feat-only change: one change reached.
    assert_eq!(report.summary.changes, 1, "the feat-only change");
    assert!(
        report.inputs.candidate_changes.len() == 1,
        "{:?}",
        report.inputs.candidate_changes
    );
    // A verdict exists (blocked here: an orphan change, no intent join).
    assert_eq!(
        serde_json::to_value(report.verdict).unwrap(),
        serde_json::json!("blocked")
    );
    assert!(report.findings.iter().any(|f| f.code == "ORPHAN_CHANGE"));
}

/// An unknown view is the same refusal the local body gives — never a
/// report about some other view.
#[tokio::test]
async fn an_unknown_view_is_refused() {
    let (_dir, _root, state, repository) = host();
    let triage = TriageImpl { state };
    let refused = triage
        .generate_triage_review(Request::new(GenerateTriageReviewRequest {
            repository: Some(repository),
            from_view: "nope".into(),
            to_view: "dev".into(),
            report: None,
            evidence_reports: Vec::new(),
        }))
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");
}

/// Retained replay reports ride the wire, bind into the report's pinned inputs,
/// and surface an unmatched report instead of trusting it.
#[tokio::test]
async fn evidence_reports_are_carried_and_bound_into_the_wire_report() {
    let (_dir, _root, state, repository) = host();
    let triage = TriageImpl { state };
    let document = serde_json::json!({
        "evidence_context": {
            "intent_id": "urn:atomic:intent:01UNRELATED",
            "intent_substance_hash": "blake3:substance",
            "intent_source_hash": "blake3:source",
            "view_chain": [{"name": "feat", "merkle": "STALE", "scope": "draft"}]
        },
        "evidence_replay": {
            "schema_version": "atomic-evidence-replay-report/v1",
            "decisions": [{
                "schema_version": "atomic-evidence-replay-response/v1",
                "request_digest": "blake3:request",
                "criterion_id": "urn:atomic:ac:01UNRELATED-ac-1",
                "claim_type": "source_text_coverage/v1",
                "decision": "supported",
                "reason": "all_selected_spans_present",
                "details": null
            }]
        }
    });
    let response = triage
        .generate_triage_review(Request::new(GenerateTriageReviewRequest {
            repository: Some(repository.clone()),
            from_view: "feat".into(),
            to_view: "dev".into(),
            report: None,
            evidence_reports: vec![VersionedBytes {
                schema: intent_evidence::REPORT_DOCUMENT_SCHEMA.into(),
                payload: serde_json::to_vec(&document).unwrap(),
            }],
        }))
        .await
        .expect("the handler serves the report");
    let bundle = response.into_inner().report.expect("report bundle");
    let report: atomic_repository::triage::TriageReport =
        serde_json::from_slice(&bundle.payload).unwrap();
    assert_eq!(report.inputs.evidence_reports.len(), 1);
    assert!(report.inputs.evidence_reports[0].starts_with("blake3:"));
    assert!(report
        .findings
        .iter()
        .any(|finding| finding.code == "EVIDENCE_REPLAY_UNRELATED"));

    let refused = triage
        .generate_triage_review(Request::new(GenerateTriageReviewRequest {
            repository: Some(repository),
            from_view: "feat".into(),
            to_view: "dev".into(),
            report: None,
            evidence_reports: vec![VersionedBytes {
                schema: "wrong".into(),
                payload: serde_json::to_vec(&document).unwrap(),
            }],
        }))
        .await
        .expect_err("unsupported schema refused");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");
}
