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
use atomic_repository::{InsertOptions, RecordOptions, Repository, TrackingOptions};
use libatomic::daemon::services_triage::TriageImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tonic::{Code, Request};

use atomic_core::change::ChangeHeader;
use libatomic::atomic::triage_service_server::TriageService;
use libatomic::atomic::{GenerateTriageReviewRequest, RepositoryRef};

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
        write(&root, "README.md", "hello\n");
        repo.add("README.md", TrackingOptions::default()).unwrap();
        let header = ChangeHeader::builder()
            .message("first")
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let outcome = repo
            .record(
                header,
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(false),
            )
            .unwrap();
        repo.write_recorded(&outcome, InsertOptions::default())
            .unwrap();

        repo.create_view_from("feat", "dev").unwrap();
        repo.switch_view("feat").unwrap();
        write(&root, "src/lib.rs", "pub fn f() {}\n");
        repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
        let header = ChangeHeader::builder()
            .message("a feature")
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let outcome = repo
            .record(
                header,
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(false),
            )
            .unwrap();
        repo.write_recorded(&outcome, InsertOptions::default())
            .unwrap();
        repo.switch_view("dev").unwrap();
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
        serde_json::to_value(&report.verdict).unwrap(),
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
        }))
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");
}
