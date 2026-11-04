use atomic_core::pristine::VaultEntryType;
use atomic_identity::{Identity, KeyPair};
use atomic_repository::{IntentCreateOptions, Repository};
use libatomic::{proto, LocalService};
use prost::Message;
use serde_json::{json, Map, Value};

async fn resolve(service: &LocalService, root: &std::path::Path) -> proto::RepositoryRef {
    let bytes = service
        .call(
            "/atomic.DaemonService/ResolveRepository",
            &proto::ResolveRepositoryRequest {
                path: root.display().to_string(),
            }
            .encode_to_vec(),
        )
        .await
        .unwrap();
    proto::ResolveRepositoryResponse::decode(bytes.as_slice())
        .unwrap()
        .repository
        .unwrap()
}

#[tokio::test]
async fn memory_create_honors_explicit_identity_and_status_without_overwriting() {
    use libatomic::atomic::daemon_service_server::DaemonService;
    use libatomic::atomic::vault_service_server::VaultService;
    use libatomic::daemon::{services::DaemonImpl, services_agent::VaultImpl, state::DaemonState};
    use std::sync::Arc;
    use tonic::Request;
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    drop(repo);
    let state = Arc::new(DaemonState::new());
    let daemon = DaemonImpl {
        state: state.clone(),
    };
    let capabilities = daemon
        .get_capabilities(Request::new(proto::GetCapabilitiesRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(
        capabilities
            .features
            .iter()
            .any(|feature| feature
                == libatomic::daemon::services::VAULT_MEMORY_CREATE_OPTIONS_FEATURE)
    );
    let reference = daemon
        .resolve_repository(Request::new(proto::ResolveRepositoryRequest {
            path: dir.path().display().to_string(),
        }))
        .await
        .unwrap()
        .into_inner()
        .repository
        .unwrap();
    let service = VaultImpl { state };
    let request = proto::CreateVaultEntityRequest {
        repository: Some(reference.clone()),
        kind: proto::VaultEntityKind::Memory as i32,
        memory_kind: Some("lesson".into()),
        memory_id: Some("inherited-note".into()),
        memory_status: Some("superseded".into()),
        body: Some("preserve this memory".into()),
        ..Default::default()
    };
    let entry = service
        .create_vault_entity(Request::new(request.clone()))
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(entry.id, "inherited-note");
    assert_eq!(
        entry.status,
        Some(proto::VaultEntityStatus::Superseded as i32)
    );
    assert_eq!(
        entry.vault_path.as_deref(),
        Some(".vault/memory/inherited-note.md")
    );
    let path = dir.path().join(".vault/memory/inherited-note.md");
    let before = std::fs::read(&path).unwrap();
    let duplicate = service
        .create_vault_entity(Request::new(request.clone()))
        .await
        .unwrap_err();
    assert_eq!(duplicate.code(), tonic::Code::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let invalid = proto::CreateVaultEntityRequest {
        repository: Some(reference.clone()),
        memory_id: Some("../escape".into()),
        ..request.clone()
    };
    assert_eq!(
        service
            .create_vault_entity(Request::new(invalid))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    assert!(!dir.path().join(".vault/escape.md").exists());
    let untracked_path = dir.path().join(".vault/memory/untracked.md");
    std::fs::write(&untracked_path, b"untracked user bytes").unwrap();
    let untracked = proto::CreateVaultEntityRequest {
        repository: Some(reference.clone()),
        memory_id: Some("untracked".into()),
        ..request.clone()
    };
    assert_eq!(
        service
            .create_vault_entity(Request::new(untracked))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::AlreadyExists
    );
    assert_eq!(
        std::fs::read(&untracked_path).unwrap(),
        b"untracked user bytes"
    );
    let invalid_status = proto::CreateVaultEntityRequest {
        repository: Some(reference),
        memory_id: Some("bad-status".into()),
        memory_status: Some("not-a-status".into()),
        ..request.clone()
    };
    assert_eq!(
        service
            .create_vault_entity(Request::new(invalid_status))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    assert!(!dir.path().join(".vault/memory/bad-status.md").exists());
    let repo = Repository::open(dir.path()).unwrap();
    let stored = repo
        .vault_retrieve("memory/inherited-note.md")
        .unwrap()
        .unwrap();
    let frontmatter: serde_json::Value = serde_json::from_str(&stored.frontmatter_json).unwrap();
    assert_eq!(frontmatter["uid"], "inherited-note");
    assert_eq!(frontmatter["status"], "superseded");
    assert_eq!(stored.content_bytes, b"preserve this memory\n");
}

fn create() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    let created = repo
        .vault_intent_create(IntentCreateOptions {
            title: "Node vault fixture".into(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap();
    repo.vault_store(
        "memory/fixture.md",
        VaultEntryType::Memory,
        b"Remember native bindings".to_vec(),
        "{}".into(),
    )
    .unwrap();
    (dir, created.id, created.intent_file)
}

#[tokio::test]
async fn get_list_and_context_are_read_only_and_reject_unsupported_views() {
    let (dir, id, _) = create();
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let db = dir.path().join(".atomic/atomic.redb");
    let before = blake3::hash(&std::fs::read(&db).unwrap());
    let request = proto::GetVaultEntryRequest {
        repository: Some(reference.clone()),
        kind: proto::VaultEntityKind::Intent as i32,
        id: id.clone(),
        include_bundle: true,
        ..Default::default()
    };
    let bytes = service
        .call(
            "/atomic.VaultService/GetVaultEntry",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    let response = proto::GetVaultEntryResponse::decode(bytes.as_slice()).unwrap();
    assert_eq!(response.entry.unwrap().title, "Node vault fixture");
    assert_eq!(
        response.entry_bundle.unwrap().schema,
        "atomic.vault.entry.bundle.v1"
    );
    let list = proto::ListVaultEntriesRequest {
        repository: Some(reference.clone()),
        kind: Some(proto::VaultEntityKind::Intent as i32),
        ..Default::default()
    };
    let bytes = service
        .call(
            "/atomic.VaultService/ListVaultEntries",
            &list.encode_to_vec(),
        )
        .await
        .unwrap();
    assert!(proto::ListVaultEntriesResponse::decode(bytes.as_slice())
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.id == id));
    let context = proto::GetVaultContextRequest {
        repository: Some(reference.clone()),
        max_entries: Some(1),
        ..Default::default()
    };
    service
        .call(
            "/atomic.VaultService/GetVaultContext",
            &context.encode_to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(before, blake3::hash(&std::fs::read(&db).unwrap()));
    let scoped = proto::GetVaultEntryRequest {
        view: Some(proto::ViewRef {
            name: Some("other".into()),
            ..Default::default()
        }),
        ..request
    };
    assert_eq!(
        service
            .call(
                "/atomic.VaultService/GetVaultEntry",
                &scoped.encode_to_vec()
            )
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
    for method in ["GetVaultEntry", "ListVaultEntries", "GetVaultContext"] {
        assert_eq!(
            service
                .call(&format!("/atomic.VaultService/{method}"), &[])
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
    }
    assert_eq!(
        service
            .call("/atomic.VaultService/CreateVaultEntity", &[])
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
}

fn source_hash(frontmatter: &Map<String, Value>, body: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(serde_json::to_string(frontmatter).unwrap().as_bytes());
    hash.update(b"\0");
    hash.update(body.as_bytes());
    format!("blake3:{}", hash.finalize().to_hex())
}

#[tokio::test]
async fn verification_uses_only_a_public_key_and_preserves_the_signed_revision() {
    let (dir, id, path) = create();
    let repo = Repository::open_existing(dir.path()).unwrap();
    let entry = repo.vault_intent_show(&id).unwrap();
    let fm: Map<String, Value> = serde_json::from_str(&entry.frontmatter_json).unwrap();
    let body = String::from_utf8(entry.content_bytes.clone()).unwrap();
    let key = KeyPair::generate();
    let identity = Identity::from_public_key("public-only fixture", key.public.clone());
    let node = atomic_canonical::lift::lift_intent(&fm, &body).unwrap();
    let signed = atomic_canonical::proof::attest(node, &identity, &key);
    let document = serde_json::to_string_pretty(&signed).unwrap();
    let attested_path = format!(
        "attestations/{}/attested.md",
        repo.resolve_intent_key(&id)
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    );
    repo.vault_store(
        &attested_path,
        VaultEntryType::Attestation,
        document.as_bytes().to_vec(),
        json!({"sourceContentHash": source_hash(&fm, &body)}).to_string(),
    )
    .unwrap();
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let request = proto::VerifyAttestationRequest {
        repository: Some(reference.clone()),
        target: Some(proto::verify_attestation_request::Target::Subject(
            proto::AttestationTarget {
                kind: Some(proto::attestation_target::Kind::IntentId(id.clone())),
            },
        )),
        public_key: Some(key.public.as_bytes().to_vec()),
        ..Default::default()
    };
    let db = dir.path().join(".atomic/atomic.redb");
    let before = blake3::hash(&std::fs::read(&db).unwrap());
    let bytes = service
        .call(
            "/atomic.AttestationService/VerifyAttestation",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    let fresh = proto::VerifyAttestationResponse::decode(bytes.as_slice()).unwrap();
    assert!(fresh.valid);
    assert_eq!(fresh.signature_valid, Some(true));
    assert_eq!(fresh.source_fresh, Some(true));
    assert_eq!(fresh.signed_document.as_deref(), Some(document.as_str()));
    assert_eq!(
        fresh.signed_revision.unwrap().value,
        blake3::hash(document.as_bytes()).as_bytes()
    );
    assert_eq!(before, blake3::hash(&std::fs::read(&db).unwrap()));

    // No private key was ever saved; verification above only received public bytes.
    let repo = Repository::open_existing(dir.path()).unwrap();
    repo.vault_store(
        &path,
        VaultEntryType::Intent,
        b"Changed source".to_vec(),
        entry.frontmatter_json.clone(),
    )
    .unwrap();
    drop(repo);
    let bytes = service
        .call(
            "/atomic.AttestationService/VerifyAttestation",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    let stale = proto::VerifyAttestationResponse::decode(bytes.as_slice()).unwrap();
    assert!(!stale.valid);
    assert_eq!(stale.signature_valid, Some(true));
    assert_eq!(stale.source_fresh, Some(false));
    assert_eq!(stale.signed_document.as_deref(), Some(document.as_str()));

    let repo = Repository::open_existing(dir.path()).unwrap();
    let mut tampered = signed.to_value();
    tampered["title"] = json!("tampered title");
    repo.vault_store(
        &attested_path,
        VaultEntryType::Attestation,
        serde_json::to_vec(&tampered).unwrap(),
        json!({"sourceContentHash": source_hash(&fm, &body)}).to_string(),
    )
    .unwrap();
    drop(repo);
    let bytes = service
        .call(
            "/atomic.AttestationService/VerifyAttestation",
            &request.encode_to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        proto::VerifyAttestationResponse::decode(bytes.as_slice())
            .unwrap()
            .signature_valid,
        Some(false)
    );
    let repo = Repository::open_existing(dir.path()).unwrap();
    let mut other = signed.clone();
    other.id = "urn:atomic:intent:another-subject".into();
    let other = atomic_canonical::proof::attest(other, &identity, &key);
    repo.vault_store(
        &attested_path,
        VaultEntryType::Attestation,
        serde_json::to_vec(&other).unwrap(),
        json!({"sourceContentHash": source_hash(&fm, &body)}).to_string(),
    )
    .unwrap();
    drop(repo);
    let error = service
        .call(
            "/atomic.AttestationService/VerifyAttestation",
            &request.encode_to_vec(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::DataLoss);
    assert!(error.message().contains("different intent"));

    let invalid = proto::VerifyAttestationRequest {
        public_key: Some(vec![1]),
        ..request
    };
    assert_eq!(
        service
            .call(
                "/atomic.AttestationService/VerifyAttestation",
                &invalid.encode_to_vec()
            )
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
}

#[tokio::test]
async fn memory_listing_sorts_before_the_budget_and_rejects_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.init_vault().unwrap();
    for id in ["b", "c", "a"] {
        repo.vault_store(
            &format!("memory/{id}.md"),
            VaultEntryType::Memory,
            b"A fact.".to_vec(),
            "{}".into(),
        )
        .unwrap();
    }
    drop(repo);
    let service = LocalService::new();
    let reference = resolve(&service, dir.path()).await;
    let list = |sort: proto::VaultListSort, reverse: bool, limit: Option<u32>| {
        proto::ListVaultEntriesRequest {
            repository: Some(reference.clone()),
            kind: Some(proto::VaultEntityKind::Memory as i32),
            sort: Some(sort as i32),
            reverse,
            budget: limit.map(|max| proto::PageBudget {
                max_items: Some(max),
                max_bytes: None,
            }),
            ..Default::default()
        }
    };
    let ids = |bytes: Vec<u8>| -> Vec<String> {
        proto::ListVaultEntriesResponse::decode(bytes.as_slice())
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.id)
            .collect()
    };
    let call = |request: proto::ListVaultEntriesRequest| {
        let service = service.clone();
        async move {
            service
                .call(
                    "/atomic.VaultService/ListVaultEntries",
                    &request.encode_to_vec(),
                )
                .await
        }
    };

    let sorted = ids(call(list(proto::VaultListSort::Id, false, None))
        .await
        .unwrap());
    assert_eq!(sorted, ["a", "b", "c"]);
    // The budget keeps the first N of the requested order, not of the default.
    let last = ids(call(list(proto::VaultListSort::Id, true, Some(2)))
        .await
        .unwrap());
    assert_eq!(last, ["c", "b"]);
    // Memories carry no title column.
    assert_eq!(
        call(list(proto::VaultListSort::Title, false, None))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
}
