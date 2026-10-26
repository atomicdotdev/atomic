//! Knowledge queries over the actual protobuf dispatch boundary shared by
//! LocalService and the daemon, including explicit non-materialized views.

use atomic_core::change::ChangeHeader;
use atomic_repository::{RecordOptions, Repository, TrackingOptions};
use libatomic::{proto as pb, LocalService};
use prost::Message;

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

async fn fixture() -> (tempfile::TempDir, LocalService, pb::RepositoryRef) {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\n// inherited_marker\n",
    )
    .unwrap();
    repo.add(wc, "src/lib.rs", TrackingOptions::default())
        .unwrap();
    record(&repo, "inherited history");
    repo.kg_enrich_from_vcs(wc).unwrap();
    repo.create_draft_view("feature", "dev").unwrap();
    repo.switch_view(wc, "feature").unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn target(secret: SiblingSecret) {}\npub fn caller() {}\n// inherited_marker\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/hidden.rs"),
        "pub fn sibling_only() { target(); }\n// sibling_marker\n",
    )
    .unwrap();
    repo.add(wc, "src/hidden.rs", TrackingOptions::default())
        .unwrap();
    record(&repo, "sibling history");
    repo.kg_enrich_from_vcs(wc).unwrap();
    atomic_repository::build_content_index(dir.path()).unwrap();
    repo.switch_view(wc, "dev").unwrap();
    drop(repo);
    // Neither this derived marker nor unrecorded disk bytes are authoritative.
    std::fs::write(dir.path().join(".atomic/current_view"), "feature").unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn disk_only() {}\n").unwrap();
    let service = LocalService::new();
    let bytes = service
        .call(
            "/atomic.DaemonService/ResolveRepository",
            &pb::ResolveRepositoryRequest {
                path: dir.path().display().to_string(),
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
    let reference = pb::ResolveRepositoryResponse::decode(bytes.as_slice())
        .unwrap()
        .repository
        .unwrap();
    (dir, service, reference)
}

fn view(name: &str) -> pb::ViewRef {
    pb::ViewRef {
        name: Some(name.into()),
        ..Default::default()
    }
}

async fn query(
    service: &LocalService,
    repository: &pb::RepositoryRef,
    view: Option<pb::ViewRef>,
    query: pb::query_graph_request::Query,
) -> Result<pb::QueryGraphResponse, Box<tonic::Status>> {
    let bytes = service
        .call(
            "/atomic.KnowledgeService/QueryGraph",
            &pb::QueryGraphRequest {
                repository: Some(repository.clone()),
                view,
                query: Some(query),
                limit: 1,
            }
            .encode_to_vec(),
        )
        .await?;
    Ok(pb::QueryGraphResponse::decode(bytes.as_slice()).unwrap())
}

fn search(term: &str) -> pb::query_graph_request::Query {
    pb::query_graph_request::Query::KgSearch(pb::KgSearchQuery {
        query: term.into(),
        pool: Some(1),
        ..Default::default()
    })
}

fn code(term: &str) -> pb::query_graph_request::Query {
    pb::query_graph_request::Query::ContentSearch(pb::ContentSearchQuery {
        query: term.into(),
        ..Default::default()
    })
}

fn callers() -> pb::query_graph_request::Query {
    pb::query_graph_request::Query::Callers(pb::CallersQuery {
        node_id: "src/lib.rs#target".into(),
    })
}

#[tokio::test]
async fn all_three_query_kinds_use_authoritative_default_and_explicit_views() {
    let (dir, service, repository) = fixture().await;
    let before = std::fs::read(dir.path().join("src/lib.rs")).unwrap();
    assert!(query(&service, &repository, None, search("sibling"))
        .await
        .unwrap()
        .nodes
        .is_empty());
    assert!(!query(
        &service,
        &repository,
        Some(view("feature")),
        search("sibling")
    )
    .await
    .unwrap()
    .nodes
    .is_empty());
    let base = query(
        &service,
        &repository,
        None,
        code("SiblingSecret|sibling_marker|disk_only"),
    )
    .await
    .unwrap();
    assert!(base.hits.is_empty());
    let base = base.content_result.unwrap();
    assert_eq!(base.total_matches, 0);
    assert!(base.dir_facets.is_empty());
    let feature = query(
        &service,
        &repository,
        Some(view("feature")),
        code("SiblingSecret|sibling_marker"),
    )
    .await
    .unwrap();
    assert_eq!(feature.hits.len(), 1);
    assert_eq!(feature.content_result.unwrap().total_matches, 2);
    let base = query(&service, &repository, None, callers()).await.unwrap();
    assert_eq!(base.callers.len(), 1);
    assert_eq!(base.callers[0].path, "src/lib.rs");
    assert_eq!(base.callers[0].name, "caller");
    assert_eq!(base.callers[0].line, 2);
    let feature = query(&service, &repository, Some(view("feature")), callers())
        .await
        .unwrap();
    assert_eq!(feature.callers.len(), 1);
    assert_eq!(feature.callers[0].path, "src/hidden.rs");
    assert_eq!(feature.callers[0].name, "sibling_only");
    assert_eq!(
        std::fs::read(dir.path().join("src/lib.rs")).unwrap(),
        before
    );
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(
        repo.desired_view_name(repo.require_working_copy_id().unwrap())
            .unwrap(),
        "dev"
    );
}

#[tokio::test]
async fn content_rpc_carries_filters_scoped_totals_facets_and_offsets() {
    let (_dir, service, repository) = fixture().await;
    let response = query(
        &service,
        &repository,
        Some(view("dev")),
        pb::query_graph_request::Query::ContentSearch(pb::ContentSearchQuery {
            query: "TARGET|INHERITED_MARKER".into(),
            paths: vec!["src/".into()],
            path_filter: Some("**/*.rs".into()),
            file_type: Some("rs".into()),
            case_insensitive: true,
            max_results: Some(1),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let result = response.content_result.unwrap();
    assert_eq!(result.total_matches, 3);
    assert_eq!(result.matches.len(), 1);
    let m = &result.matches[0];
    assert_eq!(m.path, "src/lib.rs");
    assert_eq!(m.line_number, 1);
    assert_eq!(m.byte_offset, 7);
    assert_eq!((m.submatch_start, m.submatch_end), (7, 13));
    assert_eq!(result.dir_facets[0].count, 3);
    assert_eq!(result.dir_facets[0].path, "src/lib.rs");

    for (paths, exclude_type, max_results, expected_total) in [
        (vec!["src/lib".into()], None, Some(1), 0), // component boundary
        (vec![], Some("rs".into()), Some(1), 0),
        (vec![], None, Some(0), 3),
    ] {
        let response = query(
            &service,
            &repository,
            None,
            pb::query_graph_request::Query::ContentSearch(pb::ContentSearchQuery {
                query: "target|inherited_marker".into(),
                paths,
                exclude_type,
                max_results,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let result = response.content_result.unwrap();
        assert!(result.matches.is_empty());
        assert_eq!(result.total_matches, expected_total);
    }
}

#[tokio::test]
async fn invalid_views_and_patterns_never_fall_back_to_ambient_results() {
    let (_dir, service, repository) = fixture().await;
    for kind in [search("target"), code("target"), callers()] {
        assert!(
            query(&service, &repository, Some(view("missing")), kind.clone())
                .await
                .is_err()
        );
        let mismatch = pb::ViewRef {
            name: Some("dev".into()),
            view_id: vec![1; 32],
        };
        let error = query(&service, &repository, Some(mismatch), kind)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }
    assert!(query(&service, &repository, None, code("[")).await.is_err());
    // ID-only ViewRefs are accepted and resolve to the same explicit view.
    let id = pb::ViewRef {
        name: None,
        view_id: blake3::hash(b"dev").as_bytes().to_vec(),
    };
    assert!(!query(&service, &repository, Some(id), search("inherited"))
        .await
        .unwrap()
        .nodes
        .is_empty());
}

#[tokio::test]
async fn tonic_transport_serves_the_same_filtered_contract() {
    use libatomic::daemon::{services_agent::KnowledgeImpl, state::DaemonState};
    use std::sync::Arc;

    let (dir, _local, _) = fixture().await;
    let state = Arc::new(DaemonState::new());
    let repository = state
        .register(dir.path().canonicalize().unwrap())
        .repository_ref();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(pb::knowledge_service_server::KnowledgeServiceServer::new(
                KnowledgeImpl { state },
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let mut client =
        pb::knowledge_service_client::KnowledgeServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
    for kind in [
        search("sibling"),
        code("sibling_marker|SiblingSecret"),
        callers(),
    ] {
        let base = client
            .query_graph(pb::QueryGraphRequest {
                repository: Some(repository.clone()),
                query: Some(kind.clone()),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let feature = client
            .query_graph(pb::QueryGraphRequest {
                repository: Some(repository.clone()),
                query: Some(kind.clone()),
                view: Some(view("feature")),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        match kind {
            pb::query_graph_request::Query::KgSearch(_) => {
                assert!(base.nodes.is_empty());
                assert!(!feature.nodes.is_empty());
            }
            pb::query_graph_request::Query::ContentSearch(_) => {
                assert_eq!(base.content_result.unwrap().total_matches, 0);
                assert_eq!(feature.content_result.unwrap().total_matches, 2);
            }
            pb::query_graph_request::Query::Callers(_) => {
                assert_eq!(base.callers[0].path, "src/lib.rs");
                assert_eq!(base.callers[0].name, "caller");
                assert_eq!(feature.callers[0].path, "src/hidden.rs");
                assert_eq!(feature.callers[0].name, "sibling_only");
            }
            _ => unreachable!(),
        }
    }
    drop(client);
    stop.send(()).unwrap();
    server.await.unwrap();
}
