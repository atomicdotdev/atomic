//! A rejected memory creation must not initialise the vault through a
//! pre-existing `.vault` or `.vault/memory` symlink.
//!
//! Vault handlers auto-initialise the vault on first use. The memory handler
//! also refuses symlinked vault directories. The refusal has to run before the
//! initialisation, otherwise the rejected request still creates directories
//! and files inside the symlink's external target.
#![cfg(unix)]

use std::sync::Arc;

use atomic_repository::Repository;
use libatomic::atomic::daemon_service_server::DaemonService;
use libatomic::atomic::vault_service_server::VaultService;
use libatomic::daemon::{services::DaemonImpl, services_agent::VaultImpl, state::DaemonState};
use libatomic::proto;
use tonic::Request;

#[tokio::test]
async fn rejected_memory_create_does_not_initialize_through_symlink() {
    for memory_only in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        assert!(!repo.has_vault().unwrap(), "fixture starts without a vault");
        drop(repo);
        let link = if memory_only {
            std::fs::create_dir(root.path().join(".vault")).unwrap();
            root.path().join(".vault/memory")
        } else {
            root.path().join(".vault")
        };
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let state = Arc::new(DaemonState::new());
        let daemon = DaemonImpl {
            state: state.clone(),
        };
        let reference = daemon
            .resolve_repository(Request::new(proto::ResolveRepositoryRequest {
                path: root.path().display().to_string(),
            }))
            .await
            .unwrap()
            .into_inner()
            .repository
            .unwrap();
        let service = VaultImpl { state };
        let error = service
            .create_vault_entity(Request::new(proto::CreateVaultEntityRequest {
                repository: Some(reference),
                kind: proto::VaultEntityKind::Memory as i32,
                memory_id: Some("guard-note".into()),
                body: Some("guard test".into()),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            tonic::Code::FailedPrecondition,
            "memory_only={memory_only}: symlinked vault directory must be refused"
        );
        let external: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            external.is_empty(),
            "memory_only={memory_only}: rejected request wrote outside the repository: {external:?}"
        );
    }
}
