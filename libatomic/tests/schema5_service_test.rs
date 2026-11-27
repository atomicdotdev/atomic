//! The protobuf-byte boundary used by Rust and the Node addon must preserve
//! schema-5 payloads, not just compile against a schema-5-aware core.
use atomic_core::change::{
    format_v3::{
        graph_fields::GraphEncoding, semantic::SemanticEncoding, FileHeader,
        FIELD_CODEC_FORMAT_VERSION,
    },
    signing::verify_change_signature,
    Change, ChangeHeader,
};
use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
use atomic_identity::KeyPair;
use atomic_repository::{RecordOptions, Repository};
use libatomic::{daemon::sandbox_wire as wire, proto, LocalService};
use prost::Message;

async fn invoke<Req: Message, Res: Message + Default>(
    service: &LocalService,
    method: &str,
    request: Req,
) -> Res {
    let bytes = service
        .call(method, &request.encode_to_vec())
        .await
        .unwrap();
    Res::decode(bytes.as_slice()).unwrap()
}

async fn resolve(service: &LocalService, dir: &std::path::Path) -> proto::RepositoryRef {
    let response: proto::ResolveRepositoryResponse = invoke(
        service,
        "/atomic.DaemonService/ResolveRepository",
        proto::ResolveRepositoryRequest {
            path: dir.display().to_string(),
        },
    )
    .await;
    response.repository.unwrap()
}

async fn query(
    service: &LocalService,
    repository: &proto::RepositoryRef,
    hash: &atomic_core::Hash,
) -> proto::GetChangeResponse {
    invoke(
        service,
        "/atomic.RepositoryQueryService/GetChange",
        proto::GetChangeRequest {
            repository: Some(repository.clone()),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(
                    libatomic::daemon::convert::hash_proto(hash),
                )),
            }),
            includes: Some(proto::ChangeIncludes {
                metadata: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
}

fn text() -> String {
    (0..2048)
        .map(|i| format!("let variable_{i} = calculate({i}, input);\n"))
        .collect()
}

fn fixture(span: bool) -> Change {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    if span {
        let mut txn = repo.pristine().write_txn().unwrap();
        txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
        txn.commit().unwrap();
    }
    std::fs::write(dir.path().join("code.rs"), text()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    repo.add(wc, "code.rs", Default::default()).unwrap();
    let outcome = repo
        .record(
            wc,
            ChangeHeader::new("codec fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    assert!(outcome.was_applied());
    // Preserve real edit/delete snapshots and non-ASCII token classifications,
    // not just a newly added file's final text.
    let revised = text()
        .lines()
        .enumerate()
        .filter(|(i, _)| i % 7 != 0)
        .map(|(i, line)| {
            if i % 5 == 0 {
                format!("// α {line}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect::<String>();
    std::fs::write(dir.path().join("code.rs"), revised).unwrap();
    let outcome = repo
        .record(
            wc,
            ChangeHeader::new("edited codec fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    assert!(outcome.was_applied());
    repo.load_change(outcome.hash()).unwrap()
}

#[tokio::test]
async fn native_record_and_query_advertise_the_actual_schema_five_payload() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    std::fs::write(dir.path().join("code.rs"), text()).unwrap();
    let service = LocalService::new();
    let repository = resolve(&service, dir.path()).await;
    let _: proto::AddFilesResponse = invoke(
        &service,
        "/atomic.RepositoryMutationService/AddFiles",
        proto::AddFilesRequest {
            repository: Some(repository.clone()),
            paths: vec!["code.rs".into()],
            ..Default::default()
        },
    )
    .await;
    let recorded: proto::RecordResponse = invoke(
        &service,
        "/atomic.RepositoryMutationService/Record",
        proto::RecordRequest {
            repository: Some(repository.clone()),
            message: "schema-5 native record".into(),
            paths: vec!["code.rs".into()],
            ..Default::default()
        },
    )
    .await;
    let hash = wire::hash_from_proto(recorded.change.unwrap().hash.as_ref()).unwrap();
    let response = query(&service, &repository, &hash).await;
    let bundle = response.change_bundle.unwrap();
    assert_eq!(bundle.schema, wire::CHANGE_SCHEMA);
    let header = FileHeader::from_bytes(bundle.payload[..64].try_into().unwrap()).unwrap();
    assert_eq!(header.version, FIELD_CODEC_FORMAT_VERSION);
    assert!(header.has_graph_fields());
    let (change, actual) = Change::deserialize(&mut bundle.payload.as_slice()).unwrap();
    assert_eq!(actual, hash);
    assert!(!change.hashed.hunks.is_empty());
    assert!(!change.file_ops().is_empty());
    assert_eq!(response.metadata.unwrap().affected_paths, ["code.rs"]);
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(
        std::fs::read(repo.change_store().change_path(&hash)).unwrap(),
        bundle.payload
    );
}

#[tokio::test]
async fn query_preserves_signed_codec_combinations_and_historical_modes() {
    let key = KeyPair::generate();
    let service = LocalService::new();
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    let repository = resolve(&service, dir.path()).await;
    for span in [false, true] {
        let original = fixture(span);
        for graph in [GraphEncoding::Raw, GraphEncoding::FieldsV1] {
            for semantic in [
                SemanticEncoding::Raw,
                SemanticEncoding::Compact,
                SemanticEncoding::CompactMixed,
                SemanticEncoding::PredictiveV2,
            ] {
                let mut change = original.clone();
                change.graph_encoding = graph;
                change.semantic_encoding = semantic;
                let hash = change
                    .sign_with("did:atomic:schema5-test", key.secret.as_bytes(), 123)
                    .unwrap();
                let mut bytes = vec![];
                assert_eq!(change.serialize(&mut bytes).unwrap(), hash);
                let header = FileHeader::from_bytes(bytes[..64].try_into().unwrap()).unwrap();
                let expected_schema = if header.version == FIELD_CODEC_FORMAT_VERSION {
                    wire::CHANGE_SCHEMA
                } else {
                    wire::LEGACY_CHANGE_SCHEMA
                };
                {
                    let repo = Repository::open_existing(dir.path()).unwrap();
                    repo.import_change_bytes(&hash, &bytes).unwrap();
                }
                let response = query(&service, &repository, &hash).await;
                let bundle = response.change_bundle.unwrap();
                assert_eq!(bundle.schema, expected_schema);
                assert_eq!(bundle.payload, bytes);
                let (decoded, actual) =
                    Change::deserialize(&mut bundle.payload.as_slice()).unwrap();
                assert_eq!(actual, hash);
                assert_eq!(decoded.hashed, change.hashed);
                assert_eq!(decoded.contents, change.contents);
                // Span-only edits have no GRAPH section to encode. Their
                // nonexistent graph codec correctly decodes as Raw, even if a
                // FieldsV1 write policy was requested on the in-memory change.
                let encoded_graph = if change.hashed.hunks.is_empty() {
                    GraphEncoding::Raw
                } else {
                    graph
                };
                assert_eq!(decoded.graph_encoding, encoded_graph);
                assert_eq!(decoded.semantic_encoding, semantic);
                verify_change_signature(
                    decoded.signature.as_ref().unwrap(),
                    key.public.as_bytes(),
                    &hash,
                )
                .unwrap();
                let envelope = wire::change_bundle(&hash, bytes.clone());
                assert_eq!(envelope.schema, expected_schema);
                assert_eq!(
                    wire::change_from_bundle(&envelope).unwrap(),
                    (hash, bytes.clone())
                );
                for legacy in [wire::LEGACY_CHANGE_SCHEMA, ""] {
                    let mut envelope = envelope.clone();
                    envelope.schema = legacy.into();
                    assert_eq!(
                        wire::change_from_bundle(&envelope).unwrap(),
                        (hash, bytes.clone())
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn capabilities_advertise_schema_five_without_changing_rpc_protocol_version() {
    let service = LocalService::new();
    let response: proto::GetCapabilitiesResponse = invoke(
        &service,
        "/atomic.DaemonService/GetCapabilities",
        proto::GetCapabilitiesRequest {},
    )
    .await;
    assert_eq!(response.protocol_version, 2);
    assert!(response
        .features
        .iter()
        .any(|feature| feature == wire::CHANGE_SCHEMA5_FEATURE));
}

#[test]
fn envelopes_reject_unknown_schemas_and_v5_claims_on_old_or_truncated_payloads() {
    let mut change = fixture(false);
    change.graph_encoding = GraphEncoding::Raw;
    change.semantic_encoding = SemanticEncoding::Raw;
    let mut bytes = vec![];
    let hash = change.serialize(&mut bytes).unwrap();
    let mut envelope = wire::change_bundle(&hash, bytes);
    assert_eq!(envelope.schema, wire::LEGACY_CHANGE_SCHEMA);
    envelope.schema = "atomic.change.v6".into();
    assert!(wire::change_from_bundle(&envelope).is_err());
    envelope.schema = wire::CHANGE_SCHEMA.into();
    assert!(wire::change_from_bundle(&envelope)
        .unwrap_err()
        .contains("schema-5"));
    envelope.payload.truncate(12);
    assert!(wire::change_from_bundle(&envelope).is_err());
}
