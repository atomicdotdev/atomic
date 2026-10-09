//! The mutation service's record op — the write path every host runs —
//! driven in-process: what a record carries (its AI authorship), and the
//! fence that keeps two writers off the same stale read.
//!
//! The change's provenance substance is covered by atomic-core's
//! provenance tests; this is the service seam: the wire field lands on
//! the change, and `expected` compares against the recorded state.

// tonic's `Status` is the handlers' error type, as in `daemon`.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use atomic_core::change::ChangeHeader;
use atomic_repository::{InsertOptions, RecordOptions, Repository, TrackingOptions};
use libatomic::daemon::services::MutationImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tonic::{Code, Request};

use libatomic::atomic::repository_mutation_service_server::RepositoryMutationService;
use libatomic::atomic::{
    expected_state, Author, ExpectedState, RecordAiAuthorship, RecordRequest, RepositoryRef,
};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A repository with one change on `dev`.
fn host() -> (TempDir, std::path::PathBuf, Arc<DaemonState>, RepositoryRef) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    {
        let repo = Repository::init(&root).unwrap();
        write(&root, "README.md", "hello\n");
        repo.add("README.md", TrackingOptions::default()).unwrap();
        let header = ChangeHeader::builder().message("first").build();
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
    }
    let state = Arc::new(DaemonState::new());
    let root = root.canonicalize().unwrap();
    let repository = state.register(root.clone()).repository_ref();
    (dir, root, state, repository)
}

fn record_request(repository: &RepositoryRef) -> RecordRequest {
    RecordRequest {
        repository: Some(repository.clone()),
        meta: None,
        message: "a change".into(),
        paths: Vec::new(),
        expected: None,
        author: None,
        identity_name: None,
        ai_authorship: None,
    }
}

#[tokio::test]
async fn an_ai_assisted_record_carries_its_provenance() {
    let (_dir, root, state, repository) = host();
    let mutation = MutationImpl {
        state: state.clone(),
    };

    write(&root, "README.md", "hello v2\n");
    let mut request = record_request(&repository);
    request.message = "the agent's work".into();
    request.paths = vec!["README.md".into()];
    request.ai_authorship = Some(RecordAiAuthorship {
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        tool: Some("cli:opencode".into()),
        suggestion_type: Some("collaborative".into()),
        input_tokens: Some(1200),
        output_tokens: Some(80),
        request_id: Some("req-1".into()),
        session_id: Some("exp-42".into()),
    });
    let response = mutation
        .record(Request::new(request))
        .await
        .expect("the handler serves the record")
        .into_inner();
    assert!(
        response.change.as_ref().is_some_and(|c| c.has_provenance),
        "the change says it was AI-assisted"
    );
    let merkle = atomic_core::types::Merkle(
        response
            .change
            .unwrap()
            .hash
            .unwrap()
            .value
            .try_into()
            .unwrap(),
    );

    // The stored change carries the provenance the wire named (read
    // through the state's handle, scoped so the read drops before the
    // next record's write opens).
    {
        let handle = state.register(root.clone());
        let repo = handle.repository_readonly().unwrap();
        let change = repo.load_change(&merkle).unwrap();
        let provenance = &change.provenance()[0];
        assert_eq!(provenance.model, "claude-sonnet-4-6");
        assert!(
            matches!(&provenance.tool, atomic_core::change::AITool::Cli(name) if name == "opencode")
        );
        assert_eq!(provenance.session_id.as_deref(), Some("exp-42"));
        assert_eq!(provenance.tokens.input_tokens, 1200);
    }

    // Without authorship: a human record, provenance-free.
    write(&root, "README.md", "hello v3\n");
    let response = mutation
        .record(Request::new(record_request(&repository)))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !response.change.as_ref().unwrap().has_provenance,
        "a plain record carries no provenance"
    );
}

/// The record fence: `expected` names the view's merkle root, and the
/// response's `view_merkle` is the next call's expectation — a stale one
/// refuses VIEW_STALE, never silently over a moved-on view.
#[tokio::test]
async fn a_record_fences_on_the_view_it_saw() {
    let (_dir, root, state, repository) = host();
    let mutation = MutationImpl {
        state: state.clone(),
    };

    write(&root, "README.md", "hello v2\n");
    let mut first_request = record_request(&repository);
    first_request.paths = vec!["README.md".into()];
    let first = mutation
        .record(Request::new(first_request))
        .await
        .unwrap()
        .into_inner();
    let at = first.view_merkle.expect("the post-record state");

    // Fencing at the state the last record returned: records.
    write(&root, "README.md", "hello v3\n");
    let mut fenced = record_request(&repository);
    fenced.paths = vec!["README.md".into()];
    fenced.expected = Some(ExpectedState {
        kind: Some(expected_state::Kind::MerkleRoot(at.clone())),
    });
    fenced.message = "on the current state".into();
    let ok = mutation
        .record(Request::new(fenced))
        .await
        .unwrap()
        .into_inner();
    let moved_on = ok.view_merkle.unwrap();

    // The stale expectation (the pre-record state) is refused.
    let mut stale = record_request(&repository);
    stale.expected = Some(ExpectedState {
        kind: Some(expected_state::Kind::MerkleRoot(at)),
    });
    stale.paths = vec!["README.md".into()];
    let refused = mutation
        .record(Request::new(stale))
        .await
        .expect_err("the view has moved on");
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused}");
    assert!(refused.message().contains("VIEW_STALE"), "{refused}");

    // The unsupported fence forms are refused as unreadable, never ignored.
    let mut generation = record_request(&repository);
    generation.paths = vec!["README.md".into()];
    generation.expected = Some(ExpectedState {
        kind: Some(expected_state::Kind::Generation(7)),
    });
    let refused = mutation
        .record(Request::new(generation))
        .await
        .expect_err("unsupported for record");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");

    // The author is injectable per call: an inline author lands on the
    // change (the host's default identity is only the absent case).
    write(&root, "README.md", "hello v5\n");
    let mut authored = record_request(&repository);
    authored.paths = vec!["README.md".into()];
    authored.author = Some(Author {
        name: "an agent".into(),
        email: Some("agent@outpost".into()),
    });
    let response = mutation
        .record(Request::new(authored))
        .await
        .unwrap()
        .into_inner();
    let authors = &response.change.unwrap().authors;
    assert!(authors.iter().any(|a| a.name == "an agent"), "{authors:?}");
    let _ = moved_on;
}
