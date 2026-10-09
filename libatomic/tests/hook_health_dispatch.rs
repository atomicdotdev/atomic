use std::sync::Arc;

use atomic_agent::hook_health::{FireOutcome, HookHealth};
use atomic_repository::Repository;
use libatomic::atomic::provenance_service_client::ProvenanceServiceClient;
use libatomic::atomic::provenance_service_server::{ProvenanceService, ProvenanceServiceServer};
use libatomic::atomic::{DispatchTurnEventRequest, RepositoryRef, TurnEventBody};
use libatomic::daemon::services_agent::ProvenanceImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tonic::{Request, Status};

enum Dispatch {
    Local(ProvenanceImpl),
    Rpc(ProvenanceServiceClient<tonic::transport::Channel>),
}

impl Dispatch {
    async fn fire(&mut self, request: DispatchTurnEventRequest) -> Result<(), Box<Status>> {
        let result = match self {
            Self::Local(service) => service
                .dispatch_turn_event(Request::new(request))
                .await
                .map(|_| ()),
            Self::Rpc(client) => client.dispatch_turn_event(request).await.map(|_| ()),
        };
        result.map_err(Box::new)
    }
}

fn request(reference: &RepositoryRef, event_type: &str) -> DispatchTurnEventRequest {
    DispatchTurnEventRequest {
        repository: Some(reference.clone()),
        agent_id: "opencode".to_string(),
        agent_display_name: Some("OpenCode".to_string()),
        event: Some(TurnEventBody {
            session_id: "service-health".to_string(),
            event_type: event_type.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn local_and_rpc_dispatch_record_success_validation_failure_and_recovery() {
    for rpc in [false, true] {
        let repo = TempDir::new().unwrap();
        drop(Repository::init(repo.path()).unwrap());
        let state = Arc::new(DaemonState::new());
        let reference = state.register(repo.path().to_path_buf()).repository_ref();
        let service = ProvenanceImpl { state };
        let mut tasks = Vec::new();
        let mut dispatch = if rpc {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (send, receive) = tokio::sync::mpsc::channel(8);
            tasks.push(tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    if send.send(Ok::<_, std::io::Error>(stream)).await.is_err() {
                        break;
                    }
                }
            }));
            tasks.push(tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(ProvenanceServiceServer::new(service))
                    .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(receive))
                    .await
                    .unwrap();
            }));
            Dispatch::Rpc(
                ProvenanceServiceClient::connect(format!("http://{address}"))
                    .await
                    .unwrap(),
            )
        } else {
            Dispatch::Local(service)
        };

        dispatch
            .fire(request(&reference, "session_start"))
            .await
            .unwrap();
        let health = HookHealth::read(repo.path()).unwrap();
        let verb = &health.agents["opencode"].verbs["session_start"];
        assert_eq!(verb.outcome, FireOutcome::Ok);
        let last_ok = verb.last_ok.clone();

        let invalid = dispatch
            .fire(request(&reference, "invalid-event"))
            .await
            .unwrap_err();
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let health = HookHealth::read(repo.path()).unwrap();
        let invalid = &health.agents["opencode"].verbs["invalid-event"];
        assert_eq!(invalid.outcome, FireOutcome::Error);
        assert!(invalid
            .detail
            .as_deref()
            .unwrap()
            .contains("unknown event type"));

        let sessions = repo.path().join(".atomic/sessions");
        let backup = repo.path().join(".atomic/sessions-backup");
        std::fs::rename(&sessions, &backup).unwrap();
        std::fs::write(&sessions, "blocked").unwrap();
        let failure = dispatch
            .fire(request(&reference, "session_start"))
            .await
            .unwrap_err();
        assert_eq!(failure.code(), tonic::Code::Internal);
        let health = HookHealth::read(repo.path()).unwrap();
        let verb = &health.agents["opencode"].verbs["session_start"];
        assert_eq!(verb.outcome, FireOutcome::Error);
        assert_eq!(verb.last_ok, last_ok);
        assert!(verb.detail.as_deref().unwrap().contains("orchestrator"));

        std::fs::remove_file(&sessions).unwrap();
        std::fs::rename(&backup, &sessions).unwrap();
        dispatch
            .fire(request(&reference, "session_start"))
            .await
            .unwrap();
        let health = HookHealth::read(repo.path()).unwrap();
        let verb = &health.agents["opencode"].verbs["session_start"];
        assert_eq!(verb.outcome, FireOutcome::Ok);
        assert!(verb.detail.is_none());
        for task in tasks {
            task.abort();
        }
    }
}
