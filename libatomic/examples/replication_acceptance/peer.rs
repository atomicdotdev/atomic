//! Test-only loopback object/ref endpoint. Not the production storage server.
use atomic_objects::{
    ObjectFamily, ObjectRecord, RefRecord, SyncPack, SyncWants, ViewSnapshot, PROTOCOL_HEADER,
    PROTOCOL_SYNC_V1, SYNC_MEDIA_TYPE,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

#[derive(Default)]
struct Store {
    objects: BTreeMap<(String, String), ObjectRecord>,
    refs: BTreeMap<String, String>,
    posts: Vec<Value>,
}
#[derive(Clone)]
struct Endpoint {
    store: Arc<Mutex<Store>>,
    root: PathBuf,
}
pub struct Peer {
    address: String,
    store: Arc<Mutex<Store>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<thread::JoinHandle<()>>,
}
impl Peer {
    pub fn start(root: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?.to_string();
        let store = Arc::new(Mutex::new(Store::default()));
        let endpoint = Endpoint {
            store: store.clone(),
            root: root.to_path_buf(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = thread::spawn(move || {
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("loopback listener");
                let router = Router::new()
                    .route(
                        "/workspaces/test/projects/acceptance/code",
                        get(pull).post(push),
                    )
                    .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
                    .with_state(endpoint);
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await
                    .expect("acceptance endpoint");
            })
        });
        Ok(Self {
            address,
            store,
            stop: Some(tx),
            task: Some(task),
        })
    }
    pub fn url(&self) -> String {
        format!(
            "http://{}/workspaces/test/projects/acceptance/code",
            self.address
        )
    }
    pub fn contains(&self, family: ObjectFamily, key: &str) -> bool {
        self.store
            .lock()
            .unwrap()
            .objects
            .contains_key(&(family.as_str().into(), key.into()))
    }
    pub fn inventory(&self) -> Value {
        let store = self.store.lock().unwrap();
        let mut counts = BTreeMap::<String, usize>::new();
        for (family, _) in store.objects.keys() {
            *counts.entry(family.clone()).or_default() += 1;
        }
        json!({"family_counts":counts,"refs":store.refs,"post_batches":store.posts})
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
fn reply(status: StatusCode, body: Vec<u8>, kind: &str) -> Response {
    (
        status,
        [(PROTOCOL_HEADER, PROTOCOL_SYNC_V1), ("content-type", kind)],
        body,
    )
        .into_response()
}
fn supported(headers: &HeaderMap) -> bool {
    headers.get(PROTOCOL_HEADER).and_then(|h| h.to_str().ok()) == Some(PROTOCOL_SYNC_V1)
}
fn error(message: impl ToString) -> Response {
    reply(
        StatusCode::BAD_REQUEST,
        message.to_string().into_bytes(),
        "text/plain",
    )
}
async fn push(State(endpoint): State<Endpoint>, headers: HeaderMap, body: Bytes) -> Response {
    if !supported(&headers) {
        return error("acceptance endpoint requires actual binary sync protocol");
    }
    let pack = match SyncPack::decode(&body) {
        Ok(p) => p,
        Err(e) => return error(e),
    };
    let mut store = endpoint.store.lock().unwrap();
    for reference in &pack.refs {
        if store.refs.get(&reference.name) != reference.expect_old.as_ref() {
            return reply(
                StatusCode::CONFLICT,
                b"view ref changed".to_vec(),
                "text/plain",
            );
        }
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for object in &pack.objects {
        if !object.key.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return error("unsafe object key");
        }
        *counts.entry(object.family.as_str().into()).or_default() += 1;
        let key = (object.family.as_str().into(), object.key.clone());
        if store.objects.contains_key(&key) {
            continue;
        } // Explicit first-write policy.
        let path = endpoint.root.join(object.family.as_str()).join(&object.key);
        if let Err(e) = std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|()| std::fs::write(&path, &object.bytes))
        {
            return error(e);
        }
        store.objects.insert(key, object.clone());
    }
    for reference in &pack.refs {
        store
            .refs
            .insert(reference.name.clone(), reference.new_target.clone());
    }
    store.posts.push(
        json!({"objects":counts,"refs":pack.refs.iter().map(|r|&r.name).collect::<Vec<_>>()}),
    );
    if let Err(e) = std::fs::write(
        endpoint
            .root
            .join(format!("push-{}.pack", store.posts.len())),
        &body,
    ) {
        return error(e);
    }
    reply(
        StatusCode::OK,
        serde_json::to_vec(&json!({"stored":pack.objects.len(),"refs_moved":pack.refs.len()}))
            .unwrap(),
        "application/json",
    )
}
async fn pull(State(endpoint): State<Endpoint>, headers: HeaderMap, body: Bytes) -> Response {
    if !supported(&headers) {
        return error("acceptance endpoint requires actual binary sync protocol");
    }
    let wants = match SyncWants::decode(&body) {
        Ok(w) => w,
        Err(e) => return error(e),
    };
    let store = endpoint.store.lock().unwrap();
    let mut names: BTreeSet<String> = if wants.refs.is_empty() {
        store.refs.keys().cloned().collect()
    } else {
        wants
            .refs
            .iter()
            .filter(|n| store.refs.contains_key(*n))
            .cloned()
            .collect()
    };
    loop {
        let mut added = Vec::new();
        for name in &names {
            if let Some(key) = store.refs.get(name) {
                if let Some(object) = store.objects.get(&("views".into(), key.clone())) {
                    if let Some(parent) =
                        ViewSnapshot::from_bytes(&object.bytes).and_then(|s| s.parent_view)
                    {
                        if !names.contains(&parent) && store.refs.contains_key(&parent) {
                            added.push(parent);
                        }
                    }
                }
            }
        }
        if added.is_empty() {
            break;
        }
        names.extend(added);
    }
    // Generous fixture inventory: full pulls receive every previously uploaded
    // non-have object. This isn't a production view authorization policy.
    let objects = store
        .objects
        .values()
        .filter(|o| {
            if wants.refs_only {
                o.family == ObjectFamily::View
            } else {
                o.family == ObjectFamily::View || !wants.haves.contains(&o.key)
            }
        })
        .cloned()
        .collect();
    let refs = names
        .into_iter()
        .filter_map(|name| {
            store.refs.get(&name).map(|key| RefRecord {
                name: name.clone(),
                expect_old: None,
                new_target: key.clone(),
            })
        })
        .collect();
    match (SyncPack { objects, refs }).encode() {
        Ok(bytes) => reply(StatusCode::OK, bytes, SYNC_MEDIA_TYPE),
        Err(e) => error(e),
    }
}
