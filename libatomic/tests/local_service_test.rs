use atomic_repository::{status::StatusOptions, Repository};
use libatomic::{proto, LocalService, FILE_DESCRIPTOR_SET};
use prost::Message;

async fn resolve(service: &LocalService, path: &std::path::Path) -> proto::RepositoryRef {
    let request = proto::ResolveRepositoryRequest {
        path: path.display().to_string(),
    };
    let bytes = service
        .call(
            "/atomic.DaemonService/ResolveRepository",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    proto::ResolveRepositoryResponse::decode(bytes.as_slice())
        .unwrap()
        .repository
        .unwrap()
}

#[tokio::test]
async fn local_status_matches_repository_without_retaining_a_database_handle() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    std::fs::write(dir.path().join("hello.txt"), b"hello\n").unwrap();
    std::fs::write(
        dir.path().join(".atomicignore"),
        b"node_modules/\nignored/\n",
    )
    .unwrap();
    for directory in ["node_modules/package", "ignored/nested"] {
        let path = dir.path().join(directory);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("artifact.txt"), b"must be ignored\n").unwrap();
    }
    let repo = Repository::open_readonly(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let expected = repo.status(working_copy, StatusOptions::default()).unwrap();
    let expected_view = expected.view().to_string();
    let expected_untracked = expected.untracked_count();
    drop(repo);

    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let request = proto::StatusRequest {
        repository: Some(reference),
        ..Default::default()
    };
    let bytes = service
        .clone()
        .call(
            "/atomic.RepositoryQueryService/Status",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    let status = proto::StatusResponse::decode(bytes.as_slice()).unwrap();
    assert_eq!(status.view, expected_view);
    assert_eq!(status.untracked_count as usize, expected_untracked);
    assert!(status.files.iter().any(|file| file.path == "hello.txt"));
    assert!(!status.files.iter().any(|file| {
        file.path.starts_with("node_modules/") || file.path.starts_with("ignored/")
    }));
    // A writable open must succeed even while the service is still alive.
    drop(Repository::open_existing(dir.path()).unwrap());
}

#[tokio::test]
async fn rejects_invalid_requests_and_unknown_methods() {
    let service = LocalService::new();
    let error = service
        .call("/atomic.DaemonService/ResolveRepository", &[0xff])
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    let error = service
        .call("/atomic.RepositoryQueryService/Status", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    for method in [
        "/atomic.RepositoryMutationService/AddFiles",
        "/atomic.RepositoryMutationService/Record",
        "/atomic.RepositoryQueryService/Log",
        "/atomic.RepositoryQueryService/GetChange",
        "/atomic.ViewService/ListViews",
        "/atomic.ViewService/CreateView",
        "/atomic.ViewService/SwitchView",
        "/atomic.ViewService/DeleteView",
        "/atomic.KnowledgeService/QueryGraph",
    ] {
        let error = service.call(method, &[]).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{method}");
        assert!(
            error.message().contains("repository reference is required"),
            "{method}"
        );
    }
    let error = service
        .call("/atomic.Unknown/Method", &[])
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn refuses_references_from_another_service_instance() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    let reference = resolve(&LocalService::new(), dir.path()).await;
    let request = proto::StatusRequest {
        repository: Some(reference),
        ..Default::default()
    };
    let error = LocalService::new()
        .call(
            "/atomic.RepositoryQueryService/Status",
            &request.encode_to_vec(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
}

// Exercise the same protobuf bytes the Node addon passes to LocalService.
async fn invoke<Req: Message, Res: Message + Default>(
    service: &LocalService,
    method: &str,
    request: &Req,
) -> Res {
    let bytes = service
        .call(method, &request.encode_to_vec())
        .await
        .unwrap();
    Res::decode(bytes.as_slice()).unwrap()
}

#[tokio::test]
async fn local_add_record_log_and_change_use_existing_handlers() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    let service = LocalService::new();
    let repository = resolve(&service, dir.path()).await;

    let added: proto::AddFilesResponse = invoke(
        &service,
        "/atomic.RepositoryMutationService/AddFiles",
        &proto::AddFilesRequest {
            repository: Some(repository.clone()),
            paths: vec!["hello.txt".into()],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(added.tracked, 1);

    let status: proto::StatusResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/Status",
        &proto::StatusRequest {
            repository: Some(repository.clone()),
            ..Default::default()
        },
    )
    .await;
    assert!(status.files.iter().any(|file| file.path == "hello.txt"));

    let recorded: proto::RecordResponse = invoke(
        &service,
        "/atomic.RepositoryMutationService/Record",
        &proto::RecordRequest {
            repository: Some(repository.clone()),
            message: "from node".into(),
            paths: vec!["hello.txt".into()],
            ..Default::default()
        },
    )
    .await;
    let hash = recorded.change.unwrap().hash.unwrap();
    assert_eq!(hash.value.len(), 32);

    let log: proto::LogResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/Log",
        &proto::LogRequest {
            repository: Some(repository.clone()),
            view: Some(status.view),
            ..Default::default()
        },
    )
    .await;
    assert!(log
        .entries
        .iter()
        .any(|entry| entry.message.as_deref() == Some("from node")));

    let change: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            repository: Some(repository.clone()),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(hash.clone())),
            }),
            includes: Some(proto::ChangeIncludes {
                metadata: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(change.change.unwrap().message.as_deref(), Some("from node"));
    let metadata = change.metadata.unwrap();
    assert_eq!(metadata.schema_version, 1);
    assert_eq!(metadata.change_hash, Some(hash.clone()));
    assert_eq!(metadata.affected_paths, vec!["hello.txt"]);
    assert!(metadata.usage.is_empty());
    assert_eq!(change.change_bundle.unwrap().schema, "atomic.change.v3");
    let without: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            repository: Some(repository),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(hash)),
            }),
            ..Default::default()
        },
    )
    .await;
    assert!(without.metadata.is_none());
    assert!(without.change_bundle.is_none());
}

#[tokio::test]
async fn local_view_lifecycle_uses_existing_handlers() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    let service = LocalService::new();
    let repository = resolve(&service, dir.path()).await;
    let list = |repository: proto::RepositoryRef| proto::ListViewsRequest {
        repository: Some(repository),
        ..Default::default()
    };
    let before: proto::ListViewsResponse = invoke(
        &service,
        "/atomic.ViewService/ListViews",
        &list(repository.clone()),
    )
    .await;
    assert!(!before.views.is_empty());
    let created: proto::CreateViewResponse = invoke(
        &service,
        "/atomic.ViewService/CreateView",
        &proto::CreateViewRequest {
            repository: Some(repository.clone()),
            name: "node-feature".into(),
            base: Some(proto::create_view_request::Base::Empty(true)),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(created.view.unwrap().name, "node-feature");
    let switched: proto::SwitchViewResponse = invoke(
        &service,
        "/atomic.ViewService/SwitchView",
        &proto::SwitchViewRequest {
            repository: Some(repository.clone()),
            view: "node-feature".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(switched.files_written, 0);
    let after: proto::ListViewsResponse = invoke(
        &service,
        "/atomic.ViewService/ListViews",
        &list(repository.clone()),
    )
    .await;
    assert!(after
        .views
        .iter()
        .any(|view| view.name == "node-feature" && view.current));
    let original = before
        .views
        .into_iter()
        .find(|view| view.current)
        .unwrap()
        .name;
    let _: proto::SwitchViewResponse = invoke(
        &service,
        "/atomic.ViewService/SwitchView",
        &proto::SwitchViewRequest {
            repository: Some(repository.clone()),
            view: original,
            ..Default::default()
        },
    )
    .await;
    let _: proto::DeleteViewResponse = invoke(
        &service,
        "/atomic.ViewService/DeleteView",
        &proto::DeleteViewRequest {
            repository: Some(repository.clone()),
            view: "node-feature".into(),
            ..Default::default()
        },
    )
    .await;
    let after: proto::ListViewsResponse =
        invoke(&service, "/atomic.ViewService/ListViews", &list(repository)).await;
    assert!(!after.views.iter().any(|view| view.name == "node-feature"));
}

#[tokio::test]
async fn get_change_metadata_returns_stored_cost_tokens_and_omits_private_provenance() {
    use atomic_core::change::{
        AITool, AIVendor, Change, ChangeHeader, Cost, Provenance, TokenUsage,
    };
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut change = Change::empty(ChangeHeader::new("stored AI metadata"));
    let mut usage = Provenance::new(AIVendor::OpenAI, "recorded-model", AITool::cli("OpenCode"));
    usage.tokens = TokenUsage::full(1200, 300, 80, 10000, 50);
    usage.cost = Cost::from_micro_usd(123456);
    usage.request_id = Some("shared-request".into());
    usage.session_id = Some("recorded-session".into());
    usage.reasoning_text = Some("PRIVATE_REASONING".into());
    usage.prompt = atomic_core::change::PromptContent::full("PRIVATE_PROMPT");
    change.hashed.provenance = vec![usage.clone(), usage];
    let hash = repo.save_change(&change).unwrap();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let response: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            repository: Some(reference),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(proto::Hash {
                    value: hash.0.to_vec(),
                    ..Default::default()
                })),
            }),
            includes: Some(proto::ChangeIncludes {
                metadata: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    let metadata = response.metadata.unwrap();
    assert_eq!(metadata.usage.len(), 2); // preserve occurrences; the consumer decides aggregation scope
    assert_eq!(metadata.usage[0].record_index, 0);
    assert_eq!(metadata.usage[1].record_index, 1);
    assert_eq!(metadata.usage[0].cost_micro_usd, Some(123456));
    assert_eq!(
        metadata.usage[0].tokens.as_ref().unwrap().cache_read_tokens,
        10000
    );
    assert_eq!(
        metadata.usage[0].request_id.as_deref(),
        Some("shared-request")
    );
    assert!(!format!("{metadata:?}").contains("PRIVATE_"));
    let graph = metadata.graph_summary.unwrap();
    assert_eq!(
        (
            graph.vertices_added,
            graph.edges_changed,
            graph.content_bytes,
            graph.files_changed
        ),
        (0, 0, 0, 0)
    );
    assert!(graph.files.is_empty());
    assert_eq!(response.change_bundle.unwrap().schema, "atomic.change.v3");
}

#[tokio::test]
async fn get_change_graph_summary_matches_the_stored_change_and_native_display() {
    use atomic_core::change::{Change, ChangeHeader};
    use atomic_repository::RecordOptions;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/hello.ts"), b"console.log('hello');\n").unwrap();
    repo.add(working_copy, "src/hello.ts", Default::default())
        .unwrap();
    let outcome = repo
        .record(
            working_copy,
            ChangeHeader::new("native graph summary"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    let hash = *outcome.hash();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let request = proto::GetChangeRequest {
        repository: Some(reference),
        r#ref: Some(proto::ChangeRef {
            kind: Some(proto::change_ref::Kind::Hash(proto::Hash {
                value: hash.0.to_vec(),
                ..Default::default()
            })),
        }),
        includes: Some(proto::ChangeIncludes {
            metadata: true,
            ..Default::default()
        }),
        include_file_contents: true,
        ..Default::default()
    };
    let response: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &request,
    )
    .await;
    let bundle = response.change_bundle.unwrap();
    let (change, decoded_hash) = Change::deserialize(&mut bundle.payload.as_slice()).unwrap();
    assert_eq!(decoded_hash, hash);
    let content = response
        .file_contents
        .iter()
        .find(|file| file.path == "src/hello.ts")
        .unwrap();
    assert_eq!(content.before_exists, Some(false));
    assert_eq!(content.after_exists, Some(true));
    assert!(!content.is_directory);
    assert_eq!(
        content.after.as_deref(),
        Some(&b"console.log('hello');\n"[..])
    );
    let graph = response.metadata.unwrap().graph_summary.unwrap();
    let (vertices, edges) = atomic_repository::triage::count_hunk_atoms(change.hunks());
    assert_eq!(
        (graph.vertices_added, graph.edges_changed),
        (vertices as u64, edges as u64)
    );
    assert!(vertices > 0);
    assert_eq!(graph.content_bytes, change.contents.len() as u64);
    assert_eq!(
        graph.files_changed,
        atomic_repository::get_files_in_change(&change).len() as u64
    );
    let rows = atomic_repository::triage::hunk_display_summaries(change.hunks());
    assert_eq!(
        graph
            .files
            .iter()
            .map(|row| (&*row.symbol, &*row.path, &*row.info))
            .collect::<Vec<_>>(),
        rows.iter()
            .map(|row| (row.symbol, &*row.path, &*row.info))
            .collect::<Vec<_>>()
    );
    let without: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            includes: None,
            ..request
        },
    )
    .await;
    assert!(without.metadata.is_none());
}

#[test]
fn embedded_descriptor_contains_the_status_contract() {
    let descriptor = prost_types::FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).unwrap();
    assert!(descriptor
        .file
        .iter()
        .any(|file| file.service.iter().any(|service| {
            service.name.as_deref() == Some("RepositoryQueryService")
                && service
                    .method
                    .iter()
                    .any(|method| method.name.as_deref() == Some("Status"))
        })));
}

#[tokio::test]
async fn change_blast_radius_is_readonly_change_local_and_uses_incoming_structural_edges() {
    use atomic_core::change::ChangeHeader;
    use atomic_core::pristine::{
        vault::{KgEdge, KgNode},
        KgMutTxnT, MutTxnT,
    };
    use atomic_repository::RecordOptions;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let file = dir.path().join("src/api.ts");
    std::fs::write(
        &file,
        "export function changed() { return 1; }\nfunction kept() { return 1; }\n",
    )
    .unwrap();
    repo.add(wc, "src/api.ts", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("baseline"), options())
        .unwrap();
    std::fs::write(
        &file,
        "export function changed() { return 2; }\nfunction kept() { return 1; }\n",
    )
    .unwrap();
    let hash = *repo
        .record(wc, ChangeHeader::new("public change"), options())
        .unwrap()
        .hash();
    let target = "entity:src/api.ts:changed:1";
    let caller = "entity:tests/api.ts:test:1";
    let mut txn = repo.pristine().write_txn().unwrap();
    for (id, kind, label) in [
        ("file:src/api.ts", "file", "api.ts"),
        ("file:tests/api.ts", "file", "api.ts"),
        ("file:src/ui.ts", "file", "ui.ts"),
        (target, "entity", "changed"),
        (caller, "entity", "test"),
    ] {
        txn.upsert_kg_node(&KgNode::new(id, kind, label, "ast"))
            .unwrap();
    }
    for (from, to, kind) in [
        ("file:src/api.ts", target, "DEFINES"),
        ("file:tests/api.ts", caller, "DEFINES"),
        (caller, target, "CALLS"),
        ("file:src/ui.ts", "file:src/api.ts", "INCLUDES"),
        (target, caller, "CALLS"),
        ("file:unrelated.ts", "file:src/api.ts", "CO_CHANGED"),
    ] {
        txn.upsert_kg_edge(&KgEdge::new(from, to, kind)).unwrap();
    }
    txn.commit().unwrap();
    // The working copy is now unrelated to the recorded change. Analysis must
    // still use the exact before/after change sources rather than this buffer.
    std::fs::write(&file, "function totallyDifferent() {}\n").unwrap();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let database = dir.path().join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    let response: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            repository: Some(reference),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(proto::Hash {
                    value: hash.0.to_vec(),
                    ..Default::default()
                })),
            }),
            includes: Some(proto::ChangeIncludes {
                blast_radius: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    let impact = response.blast_radius.unwrap();
    assert_eq!(impact.change_hash.unwrap().value, hash.0);
    assert_eq!(impact.mode, "structural", "{:?}", impact.warnings);
    assert!(impact
        .files
        .iter()
        .any(|file| file.path == "src/api.ts" && file.hop == 1));
    assert!(impact
        .files
        .iter()
        .any(|file| file.path == "tests/api.ts" && file.hop == 2));
    assert!(impact
        .files
        .iter()
        .any(|file| file.path == "src/ui.ts" && file.hop == 2));
    assert!(!impact.files.iter().any(|file| file.path == "unrelated.ts"));
    assert_eq!(impact.edges.len(), 2);
    assert_eq!(
        before,
        std::fs::read(database).unwrap(),
        "analysis must not write or enrich the KG"
    );
}

#[tokio::test]
async fn private_blast_radius_requires_successfully_analyzed_recorded_sources() {
    use atomic_core::change::ChangeHeader;
    use atomic_repository::RecordOptions;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let file = dir.path().join("private.ts");
    std::fs::write(&file, "function hidden() { return 1; }\n").unwrap();
    repo.add(wc, "private.ts", Default::default()).unwrap();
    let options = || {
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
    };
    repo.record(wc, ChangeHeader::new("baseline"), options())
        .unwrap();
    std::fs::write(&file, "function hidden() { return 2; }\n").unwrap();
    let hash = *repo
        .record(wc, ChangeHeader::new("private body change"), options())
        .unwrap()
        .hash();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let response: proto::GetChangeResponse = invoke(
        &service,
        "/atomic.RepositoryQueryService/GetChange",
        &proto::GetChangeRequest {
            repository: Some(reference),
            r#ref: Some(proto::ChangeRef {
                kind: Some(proto::change_ref::Kind::Hash(proto::Hash {
                    value: hash.0.to_vec(),
                    ..Default::default()
                })),
            }),
            includes: Some(proto::ChangeIncludes {
                blast_radius: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    let impact = response.blast_radius.unwrap();
    assert_eq!(impact.mode, "private", "{:?}", impact.warnings);
    assert!(impact.warnings.is_empty());
    assert!(impact.edges.is_empty());
    assert_eq!(impact.files.len(), 1);
    assert_eq!(impact.files[0].hop, 1);
}

#[tokio::test]
async fn recorded_file_preview_returns_graph_bytes_and_view_without_restoring_or_writing() {
    use atomic_core::change::ChangeHeader;
    use atomic_repository::RecordOptions;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let view = repo.current_view().to_string();
    for name in ["modified.txt", "deleted.txt", "empty.txt"] {
        std::fs::write(
            dir.path().join(name),
            if name == "empty.txt" {
                &b""[..]
            } else {
                &b"recorded\n"[..]
            },
        )
        .unwrap();
        repo.add(wc, name, Default::default()).unwrap();
    }
    repo.record(
        wc,
        ChangeHeader::new("baseline"),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    std::fs::write(dir.path().join("modified.txt"), b"working\n").unwrap();
    std::fs::remove_file(dir.path().join("deleted.txt")).unwrap();
    std::fs::write(dir.path().join("added.txt"), b"added\n").unwrap();
    repo.add(wc, "added.txt", Default::default()).unwrap();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let database = dir.path().join(".atomic/atomic.redb");
    let before = std::fs::read(&database).unwrap();
    for (name, expected) in [
        ("modified.txt", Some(&b"recorded\n"[..])),
        ("deleted.txt", Some(&b"recorded\n"[..])),
        ("empty.txt", Some(&b""[..])),
        ("added.txt", None),
    ] {
        let response: proto::PreviewMutationResponse = invoke(
            &service,
            "/atomic.RepositoryQueryService/PreviewRestore",
            &proto::PreviewRestoreRequest {
                repository: Some(reference.clone()),
                paths: vec![name.into()],
                content_path: Some(name.into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(response.view.as_deref(), Some(view.as_str()));
        assert_eq!(response.pristine_content.as_deref(), expected);
    }
    let error = service
        .call(
            "/atomic.RepositoryQueryService/PreviewRestore",
            &proto::PreviewRestoreRequest {
                repository: Some(reference),
                content_path: Some("../escape".into()),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .await
        .unwrap_err();
    assert!(error.message().contains("repository-relative"));
    assert_eq!(before, std::fs::read(database).unwrap());
    assert_eq!(
        std::fs::read(dir.path().join("modified.txt")).unwrap(),
        b"working\n"
    );
    assert!(!dir.path().join("deleted.txt").exists());
}
