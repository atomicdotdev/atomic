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
    let expected = repo.status(StatusOptions::default()).unwrap();
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
