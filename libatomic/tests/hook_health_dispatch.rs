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
    fn duplicate(&self) -> Self {
        match self {
            Self::Local(service) => Self::Local(ProvenanceImpl {
                state: service.state.clone(),
            }),
            Self::Rpc(client) => Self::Rpc(client.clone()),
        }
    }

    async fn append(&mut self, request: libatomic::atomic::AppendHookLogRequest) -> u64 {
        match self {
            Self::Local(service) => {
                service
                    .append_hook_log(Request::new(request))
                    .await
                    .unwrap()
                    .into_inner()
                    .sequence
            }
            Self::Rpc(client) => {
                client
                    .append_hook_log(request)
                    .await
                    .unwrap()
                    .into_inner()
                    .sequence
            }
        }
    }

    async fn load(
        &mut self,
        reference: &RepositoryRef,
        session: Option<&str>,
        after: u64,
        limit: u32,
    ) -> Vec<libatomic::atomic::HookLogEntry> {
        let request = libatomic::atomic::LoadHookLogRequest {
            repository: Some(reference.clone()),
            session_id: session.map(str::to_owned),
            after_sequence: after,
            limit,
            ..Default::default()
        };
        match self {
            Self::Local(service) => {
                service
                    .load_hook_log(Request::new(request))
                    .await
                    .unwrap()
                    .into_inner()
                    .entries
            }
            Self::Rpc(client) => {
                client
                    .load_hook_log(request)
                    .await
                    .unwrap()
                    .into_inner()
                    .entries
            }
        }
    }
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

fn read_health(root: &std::path::Path) -> HookHealth {
    let store = atomic_repository::redb_change_store::RedbChangeStore::open_existing(
        root.join(".atomic/atomic.redb"),
    )
    .unwrap();
    HookHealth::from_events(
        store
            .load_session_log(None, 0, 512)
            .unwrap()
            .into_iter()
            .map(|entry| entry.event)
            .collect(),
    )
    .unwrap()
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
        let health = read_health(repo.path());
        let verb = &health.agents["opencode"].verbs["session_start"];
        assert_eq!(verb.outcome, FireOutcome::Ok);
        let last_ok = verb.last_ok.clone();

        let invalid = dispatch
            .fire(request(&reference, "invalid-event"))
            .await
            .unwrap_err();
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let health = read_health(repo.path());
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
        let health = read_health(repo.path());
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
        let health = read_health(repo.path());
        let verb = &health.agents["opencode"].verbs["session_start"];
        assert_eq!(verb.outcome, FireOutcome::Ok);
        assert!(verb.detail.is_none());

        // Overlapping callers use the same service gate and retain explicit
        // parent lineage on the wire, including events that fail validation.
        let before = dispatch.load(&reference, None, 0, 512).await.len();
        let mut callers = Vec::new();
        for index in 0..24 {
            let mut caller = dispatch.duplicate();
            let mut event = request(&reference, "concurrent-invalid");
            let body = event.event.as_mut().unwrap();
            body.session_id = format!("child-{}", index % 3);
            body.parent_session_id = Some("service-health".into());
            callers.push(tokio::spawn(async move {
                caller.fire(event).await.unwrap_err()
            }));
        }
        for caller in callers {
            assert_eq!(caller.await.unwrap().code(), tonic::Code::InvalidArgument);
        }
        assert_eq!(
            dispatch.load(&reference, None, 0, 512).await.len(),
            before + 24
        );
        for index in 0..3 {
            let session = format!("child-{index}");
            let page = dispatch.load(&reference, Some(&session), 0, 5).await;
            let rest = dispatch
                .load(&reference, Some(&session), page.last().unwrap().sequence, 5)
                .await;
            assert_eq!(page.len() + rest.len(), 8);
            assert!(page.iter().chain(&rest).all(|entry| entry
                .event
                .as_ref()
                .unwrap()
                .parent_session_id
                .as_deref()
                == Some("service-health")));
        }

        let append = libatomic::atomic::AppendHookLogRequest {
            repository: Some(reference.clone()),
            event: Some(libatomic::atomic::HookLogEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                session_id: None,
                parent_session_id: None,
                agent: "opencode".into(),
                display_name: "OpenCode".into(),
                verb: "receive".into(),
                outcome: "error".into(),
                detail: Some("invalid JSON".into()),
                at: chrono::Utc::now().to_rfc3339(),
            }),
            ..Default::default()
        };
        let sequence = dispatch.append(append.clone()).await;
        assert_eq!(dispatch.append(append).await, sequence);
        assert_eq!(
            dispatch.load(&reference, None, 0, 512).await.len(),
            before + 25
        );
        assert!(!repo.path().join(".atomic/hook-health.json").exists());
        for task in tasks {
            task.abort();
        }
    }
}
