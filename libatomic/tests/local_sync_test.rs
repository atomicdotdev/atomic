//! Real native protobuf push/pull against a stateful, authenticated sync/1 peer.
//! The peer verifies JWT signatures, stores objects, and enforces ref CAS; it
//! does not echo requests or invoke a CLI/daemon transport.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use atomic_core::types::{Base32, Hash};
use atomic_identity::{Identity, IdentityStore, KeyPair};
use atomic_objects::{ObjectFamily, ObjectRecord, RefRecord, SyncPack, SyncWants};
use atomic_repository::Repository;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use data_encoding::BASE64URL_NOPAD;
use libatomic::{proto, LocalService};
use prost::Message;

struct Environment(Vec<(&'static str, Option<OsString>)>);

impl Environment {
    fn isolated(config: &Path) -> Self {
        let names = ["ATOMIC_CONFIG_DIR", "ATOMIC_AGENT_IDENTITY", "ATOMIC_DELEGATION"];
        let saved = names.iter().map(|name| (*name, std::env::var_os(name))).collect();
        std::env::set_var("ATOMIC_CONFIG_DIR", config);
        std::env::remove_var("ATOMIC_AGENT_IDENTITY");
        std::env::remove_var("ATOMIC_DELEGATION");
        Self(saved)
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[derive(Default)]
struct PeerState {
    objects: HashMap<(ObjectFamily, String), ObjectRecord>,
    refs: HashMap<String, String>,
    requests: usize,
    authenticated: usize,
    pushes: usize,
    require_identity: Option<Identity>,
    audience: String,
}

fn authenticates(headers: &HeaderMap, identity: &Identity, audience: &str) -> bool {
    let Some(token) = headers.get("authorization").and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ")) else { return false };
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 { return false; }
    let decode = |part: &str| BASE64URL_NOPAD.decode(part.as_bytes()).ok();
    let Some(header) = decode(parts[0]).and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok()) else { return false };
    let Some(claims) = decode(parts[1]).and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok()) else { return false };
    let Some(signature) = decode(parts[2]).and_then(|v| <[u8; 64]>::try_from(v).ok()) else { return false };
    let now = chrono::Utc::now().timestamp();
    header["alg"] == "EdDSA"
        && header["kid"] == identity.public_key_base32()
        && claims["sub"] == identity.public_key_base32()
        && claims["aud"] == audience
        && claims["iat"].as_i64().is_some_and(|iat| iat <= now)
        && claims["exp"].as_i64().is_some_and(|exp| exp > now)
        && identity.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).is_ok()
}

async fn peer(
    State(state): State<Arc<Mutex<PeerState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    state.requests += 1;
    if let Some(identity) = &state.require_identity {
        if !authenticates(&headers, identity, &state.audience) {
            return (StatusCode::UNAUTHORIZED, "identity credential required").into_response();
        }
        state.authenticated += 1;
    }
    if uri.query() == Some("project-identity=1") {
        return ([("content-type", "application/json")], "{\"bound\":false}").into_response();
    }
    if headers.get("Atomic-Protocol").and_then(|v| v.to_str().ok()) != Some("sync/1") {
        return (StatusCode::BAD_REQUEST, "sync/1 required").into_response();
    }
    if method == Method::GET {
        let Ok(wants) = SyncWants::decode(&body) else {
            return (StatusCode::BAD_REQUEST, "invalid wants").into_response();
        };
        let mut selected: std::collections::HashSet<_> = wants.refs.iter().cloned().collect();
        let mut pending = wants.refs.clone();
        while let Some(name) = pending.pop() {
            let Some(key) = state.refs.get(&name) else { continue };
            let Some(object) = state.objects.get(&(ObjectFamily::View, key.clone())) else { continue };
            let Some(snapshot) = atomic_objects::ViewSnapshot::from_bytes(&object.bytes) else { continue };
            if let Some(parent) = snapshot.parent_view {
                if selected.insert(parent.clone()) { pending.push(parent); }
            }
        }
        let refs: Vec<_> = state.refs.iter()
            .filter(|(name, _)| wants.refs.is_empty() || selected.contains(*name))
            .map(|(name, key)| RefRecord { name: name.clone(), expect_old: None, new_target: key.clone() })
            .collect();
        let objects = state.objects.values()
            .filter(|object| !wants.haves.contains(&object.key))
            .filter(|object| !wants.refs_only || object.family == ObjectFamily::View)
            .cloned().collect();
        return ([("content-type", atomic_objects::SYNC_MEDIA_TYPE)], SyncPack { objects, refs }.encode().unwrap()).into_response();
    }
    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(pack) = SyncPack::decode(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid pack").into_response();
    };
    for reference in &pack.refs {
        if state.refs.get(&reference.name) != reference.expect_old.as_ref() {
            return (StatusCode::CONFLICT, "ref CAS refused").into_response();
        }
        if !pack.objects.iter().any(|object| object.family == ObjectFamily::View && object.key == reference.new_target)
            && !state.objects.contains_key(&(ObjectFamily::View, reference.new_target.clone())) {
            return (StatusCode::BAD_REQUEST, "missing view snapshot").into_response();
        }
    }
    let stored = pack.objects.len();
    let moved = pack.refs.len();
    for object in pack.objects {
        state.objects.insert((object.family, object.key.clone()), object);
    }
    for reference in pack.refs {
        state.refs.insert(reference.name, reference.new_target);
    }
    state.pushes += 1;
    ([("content-type", "application/json")], serde_json::json!({"stored":stored,"refs_moved":moved}).to_string()).into_response()
}

async fn invoke<Req: Message, Res: Message + Default>(service: &LocalService, method: &str, request: &Req) -> Res {
    let bytes = service.call(method, &request.encode_to_vec()).await.unwrap();
    Res::decode(bytes.as_slice()).unwrap()
}

async fn resolve(service: &LocalService, path: &Path) -> proto::RepositoryRef {
    let response: proto::ResolveRepositoryResponse = invoke(service, "/atomic.DaemonService/ResolveRepository", &proto::ResolveRepositoryRequest { path: path.display().to_string() }).await;
    response.repository.unwrap()
}

async fn record(service: &LocalService, repository: &proto::RepositoryRef, message: &str) -> Hash {
    let response: proto::RecordResponse = invoke(service, "/atomic.RepositoryMutationService/Record", &proto::RecordRequest {
        repository: Some(repository.clone()), message: message.into(), paths: vec!["hello.txt".into()], ..Default::default()
    }).await;
    atomic_core::types::Merkle(response.change.unwrap().hash.unwrap().value.try_into().unwrap())
}

// This binary has one test, so its temporary process-global identity/config
// environment cannot race another test in this binary.
#[tokio::test]
async fn native_sync_authenticates_transfers_graph_and_does_not_materialize_host_files() {
    let config = tempfile::tempdir().unwrap();
    let _environment = Environment::isolated(config.path());
    let keypair = KeyPair::generate();
    let identity = Identity::new("registered", &keypair);
    let wrong_key = KeyPair::generate();
    let wrong_identity = Identity::new("default-other-user", &wrong_key);
    let mut store = IdentityStore::open_default().unwrap();
    store.save_with_keypair(&identity, &keypair, None).unwrap();
    store.save_with_keypair(&wrong_identity, &wrong_key, None).unwrap();
    store.set_default(&wrong_identity.id).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let audience = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let url = format!("{audience}/workspaces/test/projects/native/code");
    let state = Arc::new(Mutex::new(PeerState { require_identity: Some(identity.clone()), audience, ..Default::default() }));
    let router = Router::new().route("/workspaces/test/projects/native/code", any(peer)).with_state(state.clone());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });

    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let source_repo = Repository::init(source.path()).unwrap();
    let view = source_repo.current_view().to_string();
    source_repo.add_remote_default("origin", &url).unwrap();
    drop(source_repo);
    let target_repo = Repository::init(target.path()).unwrap();
    target_repo.add_remote_default("origin", &url).unwrap();
    drop(target_repo);
    std::fs::write(source.path().join("hello.txt"), "first native revision\n").unwrap();
    std::fs::write(target.path().join("hello.txt"), "local unrecorded bytes\n").unwrap();

    let service = LocalService::new();
    let source_ref = resolve(&service, source.path()).await;
    let target_ref = resolve(&service, target.path()).await;
    let empty = tempfile::tempdir().unwrap();
    drop(Repository::init(empty.path()).unwrap());
    let empty_ref = resolve(&service, empty.path()).await;
    let error = service.call("/atomic.SyncService/PullChanges", &proto::PullChangesRequest {
        repository: Some(empty_ref), ..Default::default()
    }.encode_to_vec()).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
    assert!(error.message().contains("remote 'origin' not found"));
    let _: proto::AddFilesResponse = invoke(&service, "/atomic.RepositoryMutationService/AddFiles", &proto::AddFilesRequest {
        repository: Some(source_ref.clone()), paths: vec!["hello.txt".into()], ..Default::default()
    }).await;
    let first = record(&service, &source_ref, "first native change").await;

    let mut push = proto::PushChangesRequest { repository: Some(source_ref.clone()), ..Default::default() };
    // A missing explicit identity must not silently use the default or network.
    push.identity = Some("does-not-exist".into());
    let error = service.call("/atomic.SyncService/PushChanges", &push.encode_to_vec()).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert!(error.message().contains("does-not-exist"));
    assert_eq!(state.lock().unwrap().requests, 0);
    // A usable but unauthorized default key must be rejected by the peer.
    push.identity = None;
    service.call("/atomic.SyncService/PushChanges", &push.encode_to_vec()).await.unwrap_err();
    assert!(state.lock().unwrap().refs.is_empty());

    push.identity = Some(identity.name.clone());
    let pushed: proto::PushChangesResponse = invoke(&service, "/atomic.SyncService/PushChanges", &push).await;
    assert!(pushed.conflict.is_none());
    assert_eq!(pushed.pushed_changes, 1);
    assert_eq!(state.lock().unwrap().pushes, 1);
    assert!(state.lock().unwrap().objects.contains_key(&(ObjectFamily::Change, first.to_base32())));
    // A direct URL in the remote selector follows the same no-op path without
    // requiring a registry entry whose name is that URL.
    push.remote = url.clone();
    let repeated: proto::PushChangesResponse = invoke(&service, "/atomic.SyncService/PushChanges", &push).await;
    assert_eq!(repeated.pushed_changes, 0);
    assert_eq!(state.lock().unwrap().pushes, 1);
    push.remote.clear();

    let pull = proto::PullChangesRequest {
        repository: Some(target_ref.clone()), identity: Some(identity.name.clone()),
        from_view: Some(view.clone()), to_view: Some("received".into()), import_sidecars: true, ..Default::default()
    };
    let pulled: proto::PullChangesResponse = invoke(&service, "/atomic.SyncService/PullChanges", &pull).await;
    let report = pulled.report.unwrap();
    assert_eq!(report.changes_failed, 0);
    assert!(report.apply_errors.is_empty());
    assert!(!report.materialized);
    assert!(report.materialize_error.is_none());
    assert_eq!(std::fs::read(target.path().join("hello.txt")).unwrap(), b"local unrecorded bytes\n");
    {
        let repo = Repository::open_readonly(target.path()).unwrap();
        assert!(repo.view_manifest("received").unwrap().changes.contains(&first));
        assert_eq!(repo.get_file_content_on_view("hello.txt", "received").unwrap().unwrap(), b"first native revision\n");
    }

    std::fs::write(source.path().join("hello.txt"), "second native revision\n").unwrap();
    let second = record(&service, &source_ref, "second native change").await;
    let pushed: proto::PushChangesResponse = invoke(&service, "/atomic.SyncService/PushChanges", &push).await;
    assert_eq!(pushed.pushed_changes, 1);
    let pulled: proto::PullChangesResponse = invoke(&service, "/atomic.SyncService/PullChanges", &pull).await;
    assert_eq!(pulled.report.as_ref().unwrap().changes_downloaded, 1);
    assert!(pulled.report.as_ref().unwrap().apply_errors.is_empty());
    {
        let repo = Repository::open_readonly(target.path()).unwrap();
        let manifest = repo.view_manifest("received").unwrap();
        assert!(manifest.changes.contains(&first));
        assert!(manifest.changes.contains(&second));
        assert_eq!(repo.get_file_content_on_view("hello.txt", "received").unwrap().unwrap(), b"second native revision\n");
    }
    assert_eq!(std::fs::read(target.path().join("hello.txt")).unwrap(), b"local unrecorded bytes\n");
    assert!(state.lock().unwrap().authenticated >= 8);

    // Calling the transport handler with the same selector must NOT consult
    // ambient identity storage: only LocalService is a trusted credential client.
    use libatomic::proto::sync_service_server::SyncService;
    let transport_state = Arc::new(libatomic::daemon::state::DaemonState::new());
    let reference = transport_state.register(target.path().canonicalize().unwrap()).repository_ref();
    let transport = libatomic::daemon::services_sync::SyncImpl { state: transport_state };
    let error = transport.pull_changes(tonic::Request::new(proto::PullChangesRequest {
        repository: Some(reference), identity: Some(identity.name.clone()), from_view: Some(view), ..Default::default()
    })).await.unwrap_err();
    assert!(error.message().contains("authentication_failed"));

    // Public reads still work without a usable identity: explicit missing
    // identity must not become a mandatory local authentication requirement.
    state.lock().unwrap().require_identity = None;
    let _: proto::PullChangesResponse = invoke(&service, "/atomic.SyncService/PullChanges", &proto::PullChangesRequest {
        identity: Some("does-not-exist".into()), ..pull
    }).await;
    let _ = stop.send(());
    server.await.unwrap();
}
