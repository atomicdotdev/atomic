//! The CLI inside a remote sandbox, end to end: the real `atomic` binary in a
//! directory holding nothing but a pointer, talking to libatomic's services
//! over a Unix socket served by this test — the same transport a host
//! forwards to its own server.
//!
//! The host side runs the same binary in its repository over the same
//! socket (`ATOMIC_SERVICE=reactor`), so every check of what landed reads
//! the repository through the server, never beside it.

// The sandbox rides a Unix socket end to end — the same surface the
// forwarder serves in the VM. There is no Windows story to test here yet;
// the other socket-based integration tests gate the same way.
#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use libatomic::daemon::state::DaemonState;
use libatomic::daemon::{
    services, services_agent, services_maintenance, services_query, services_sandbox,
    services_sync, services_tag, services_triage,
};
use serde_json::Value;
use tempfile::TempDir;

// ── the server: libatomic over a temp socket ────────────────────────────

struct Server {
    _dir: TempDir,
    socket: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a host does in front of the handlers: a request carrying a sandbox
/// token reaches only the methods whose handlers enforce a grant's scope
/// (`is_sandbox_scoped`). Anything else is refused before dispatch — so
/// these tests also prove the CLI in a sandbox never needs anything else.
#[derive(Clone)]
struct SandboxRouting<S>(S);

impl<S, B> tower::Service<tonic::codegen::http::Request<B>> for SandboxRouting<S>
where
    S: tower::Service<
        tonic::codegen::http::Request<B>,
        Response = tonic::codegen::http::Response<tonic::body::BoxBody>,
    >,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: tonic::codegen::http::Request<B>) -> Self::Future {
        let sandbox = request
            .headers()
            .contains_key(libatomic::daemon::sandbox_grants::SANDBOX_TOKEN_METADATA);
        let path = request.uri().path().to_string();
        if sandbox && !libatomic::daemon::sandbox_grants::is_sandbox_scoped(&path) {
            let refused = tonic::Status::permission_denied(format!(
                "atomic:FORBIDDEN: a sandbox may not call {path}"
            ))
            .into_http();
            return Box::pin(async move { Ok(refused) });
        }
        Box::pin(self.0.call(request))
    }
}

#[derive(Clone)]
struct SandboxRoutingLayer;

impl<S> tower::Layer<S> for SandboxRoutingLayer {
    type Service = SandboxRouting<S>;
    fn layer(&self, inner: S) -> Self::Service {
        SandboxRouting(inner)
    }
}

/// Serve every libatomic service on a fresh socket, as the reactor's
/// transport does.
fn serve() -> Server {
    use libatomic::atomic::*;
    // macOS caps a Unix socket path near 104 bytes: keep it short.
    let dir = tempfile::Builder::new()
        .prefix("asb")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("s.sock");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (ready, is_ready) = std::sync::mpsc::channel();
    let bind = socket.clone();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::bind(&bind).unwrap();
            let state = Arc::new(DaemonState::new());
            let s = || state.clone();
            let provenance = provenance_service_server::ProvenanceServiceServer::new(
                services_agent::ProvenanceImpl { state: s() },
            )
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024);
            let sandbox =
                sandbox_service_server::SandboxServiceServer::new(services_sandbox::SandboxImpl {
                    state: s(),
                })
                .max_decoding_message_size(64 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024);
            ready.send(()).unwrap();
            tonic::transport::Server::builder()
                .layer(SandboxRoutingLayer)
                .add_service(daemon_service_server::DaemonServiceServer::new(
                    services::DaemonImpl { state: s() },
                ))
                .add_service(
                    repository_query_service_server::RepositoryQueryServiceServer::new(
                        services::QueryImpl { state: s() },
                    ),
                )
                .add_service(
                    repository_mutation_service_server::RepositoryMutationServiceServer::new(
                        services::MutationImpl { state: s() },
                    ),
                )
                .add_service(vault_service_server::VaultServiceServer::new(
                    services_agent::VaultImpl { state: s() },
                ))
                .add_service(attestation_service_server::AttestationServiceServer::new(
                    services_agent::AttestationImpl { state: s() },
                ))
                .add_service(knowledge_service_server::KnowledgeServiceServer::new(
                    services_agent::KnowledgeImpl { state: s() },
                ))
                .add_service(provenance)
                .add_service(view_service_server::ViewServiceServer::new(
                    services_query::ViewImpl { state: s() },
                ))
                .add_service(maintenance_service_server::MaintenanceServiceServer::new(
                    services_maintenance::MaintenanceImpl { state: s() },
                ))
                .add_service(tag_service_server::TagServiceServer::new(
                    services_tag::TagImpl { state: s() },
                ))
                .add_service(sync_service_server::SyncServiceServer::new(
                    services_sync::SyncImpl { state: s() },
                ))
                .add_service(sandbox)
                .add_service(triage_service_server::TriageServiceServer::new(
                    services_triage::TriageImpl { state: s() },
                ))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::UnixListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
                .unwrap();
        });
    });
    is_ready.recv().unwrap();
    Server {
        _dir: dir,
        socket,
        shutdown: Some(stop),
        thread: Some(thread),
    }
}

// ── the binary ──────────────────────────────────────────────────────────

struct Env {
    home: TempDir,
    socket: Option<PathBuf>,
}

impl Env {
    /// Run `atomic` in `cwd`. With a socket, over the reactor transport —
    /// and never starting a daemon of its own.
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.run_with_stdin(cwd, args, None)
    }

    fn run_with_stdin(&self, cwd: &Path, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.home.path())
            .env("ATOMIC_DAEMON_BIN", "/nonexistent/atomicd")
            .env_remove("ATOMIC_RPC")
            .env_remove("RUST_LOG")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match &self.socket {
            Some(socket) => {
                command
                    .env("ATOMIC_SERVICE", "reactor")
                    .env("ATOMIC_DAEMON_SOCKET", socket);
            }
            None => {
                command.env("ATOMIC_SERVICE", "local");
            }
        }
        let mut child = command.spawn().expect("run atomic");
        if let Some(input) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn ok(output: Output, what: &str) -> String {
    assert!(output.status.success(), "{what} failed:\n{}", text(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn failure(output: Output, what: &str) -> String {
    assert!(
        !output.status.success(),
        "{what} should have failed:\n{}",
        text(&output)
    );
    text(&output)
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Every redb file under `dir`.
fn redb_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "redb") {
                out.push(path);
            }
        }
    }
    out
}

/// Nothing in the sandbox but its pointer, its tree, and its cache: no
/// repository database outside the cache, ever.
fn assert_only_the_cache(vm: &Path) {
    // A `.atomic` directory may hold the harness's session transcripts
    // (`.atomic/sessions/...`) — it is not a repository. What must never
    // appear outside the cache is a database.
    let cache = vm.join(".atomic-sandbox.d");
    for file in redb_files(vm) {
        assert!(
            file.starts_with(&cache),
            "a database outside the cache: {}",
            file.display()
        );
    }
}

/// A host repository with two recorded files on `dev` and a draft `exp-1`
/// off it, set up before the server starts; then the server, and the host
/// side of the environment talking through it.
fn setup() -> (Server, Env, TempDir) {
    let local = Env {
        home: TempDir::new().unwrap(),
        socket: None,
    };
    let host = TempDir::new().unwrap();
    ok(local.run(host.path(), &["init"]), "init");
    write(host.path(), "README.md", "hello\n");
    write(host.path(), "src/lib.rs", "pub fn f() {}\n");
    ok(
        local.run(host.path(), &["add", "README.md", "src/lib.rs"]),
        "add",
    );
    ok(
        local.run(host.path(), &["record", "-a", "-m", "first"]),
        "record",
    );
    ok(
        local.run(host.path(), &["view", "create", "exp-1", "--from", "dev"]),
        "view create",
    );
    let server = serve();
    let env = Env {
        home: local.home,
        socket: Some(server.socket.clone()),
    };
    (server, env, host)
}

fn open(env: &Env, host: &Path, view: &str, dest: &Path) -> Value {
    let out = ok(
        env.run(
            host,
            &[
                "sandbox",
                "open",
                view,
                "--acting-as",
                "did:key:zAgent",
                "--dest",
                dest.to_str().unwrap(),
            ],
        ),
        "sandbox open",
    );
    assert!(out.contains("opened"), "{out}");
    let pointer = dest.join(".atomic-sandbox");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pointer).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the pointer holds a token: owner-only");
    }
    serde_json::from_slice(&std::fs::read(pointer).unwrap()).unwrap()
}

// ── the tests ───────────────────────────────────────────────────────────

#[test]
fn a_remote_sandbox_records_through_the_socket_and_nothing_else() {
    let (_server, env, host) = setup();
    let host = host.path();
    let vms = TempDir::new().unwrap();
    let vm = vms.path().join("work");

    // The pointer: repository, view, token — no address.
    let pointer = open(&env, host, "exp-1", &vm);
    assert_eq!(pointer["view"], "exp-1");
    assert!(pointer["token"].as_str().unwrap().starts_with("ast_"));
    assert!(pointer["repository"]["repository_id"].is_string());
    assert!(pointer.get("remote").is_none() && pointer.get("endpoint").is_none());
    assert_eq!(
        std::fs::read_dir(&vm).unwrap().count(),
        1,
        "only the pointer"
    );

    // Before materializing there is nothing to work on, and it says so.
    let refused = failure(env.run(&vm, &["status"]), "status before materialize");
    assert!(!refused.contains("panicked"), "{refused}");
    assert_only_the_cache(&vm);

    // Materialize: the view's tree, and the cache.
    let out = ok(env.run(&vm, &["sandbox", "materialize"]), "materialize");
    assert!(out.contains("Materialized"), "{out}");
    assert_eq!(
        std::fs::read_to_string(vm.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(vm.join("src/lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );
    assert_only_the_cache(&vm);

    // Edit, status, record: the change lands on the view in the host repo.
    write(&vm, "README.md", "hello\nfrom the sandbox\n");
    write(&vm, "src/new.rs", "pub fn n() {}\n");
    let status = ok(env.run(&vm, &["status"]), "status");
    assert!(status.contains("README.md"), "{status}");
    assert!(status.contains("src/new.rs"), "{status}");
    ok(
        env.run(&vm, &["record", "-a", "-m", "from the sandbox"]),
        "record in the sandbox",
    );
    let log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    assert!(log.contains("from the sandbox"), "{log}");
    let dev = ok(env.run(host, &["log"]), "host log of dev");
    assert!(
        !dev.contains("from the sandbox"),
        "only on the granted view: {dev}"
    );
    let status = ok(env.run(&vm, &["status"]), "status after record");
    assert!(
        !status.contains("README.md"),
        "clean after landing: {status}"
    );

    // A second record builds on it.
    write(&vm, "src/new.rs", "pub fn n() { 2 }\n");
    ok(
        env.run(&vm, &["record", "-a", "-m", "again from the sandbox"]),
        "second record",
    );
    let log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    assert!(log.contains("again from the sandbox"), "{log}");

    // Reading the recorded state works off the cache: diff, restore, log.
    write(&vm, "README.md", "hello\nfrom the sandbox\nuncommitted\n");
    let diff = ok(env.run(&vm, &["diff"]), "diff in the sandbox");
    assert!(diff.contains("uncommitted"), "{diff}");
    ok(
        env.run(&vm, &["restore", "README.md"]),
        "restore in the sandbox",
    );
    assert_eq!(
        std::fs::read_to_string(vm.join("README.md")).unwrap(),
        "hello\nfrom the sandbox\n"
    );
    let log = ok(env.run(&vm, &["log"]), "log in the sandbox");
    assert!(
        log.contains("again from the sandbox") && log.contains("from the sandbox"),
        "{log}"
    );
    let host_log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    eprintln!("SANDBOX LOG\n{log}\nHOST LOG\n{host_log}");

    // A command that needs the repository itself is refused, clearly, and
    // opens nothing.
    for args in [
        &["view", "list"][..],
        &["push"][..],
        &["insert", "from-view", "dev"][..],
        &["sandbox", "open", "dev"][..],
    ] {
        let refused = failure(env.run(&vm, args), &format!("{args:?} in a sandbox"));
        assert!(refused.contains("remote sandbox"), "{args:?}: {refused}");
    }
    assert_only_the_cache(&vm);

    // The view moves underneath: the next record is refused as stale, and
    // the sandbox records again after materializing again.
    // (`dev`, the draft's parent, moves; the draft's own log does not.)
    write(host, "OUT-OF-BAND.md", "elsewhere\n");
    ok(env.run(host, &["add", "OUT-OF-BAND.md"]), "host add");
    ok(
        env.run(host, &["record", "-a", "-m", "out of band"]),
        "host record",
    );
    write(&vm, "README.md", "hello\nfrom the sandbox\nlate\n");
    let stale = failure(
        env.run(&vm, &["record", "-a", "-m", "against the old view"]),
        "a record against a view that moved",
    );
    assert!(stale.contains("VIEW_STALE"), "{stale}");
    assert!(stale.contains("atomic sandbox materialize"), "{stale}");
    let behind = failure(
        env.run(&vm, &["record", "-a", "-m", "still behind"]),
        "a record while behind",
    );
    assert!(behind.contains("materialize it again"), "{behind}");
    ok(
        env.run(&vm, &["sandbox", "materialize"]),
        "materialize again",
    );
    assert_eq!(
        std::fs::read_to_string(vm.join("OUT-OF-BAND.md")).unwrap(),
        "elsewhere\n"
    );
    write(
        &vm,
        "README.md",
        "hello\nfrom the sandbox\nafter the view moved\n",
    );
    ok(
        env.run(&vm, &["record", "-a", "-m", "after the view moved"]),
        "record after materializing again",
    );
    let log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    assert!(log.contains("after the view moved"), "{log}");
    assert!(!log.contains("against the old view"), "{log}");
    assert_only_the_cache(&vm);
}

#[test]
fn a_token_reaches_its_view_and_no_other() {
    let (_server, env, host) = setup();
    let host = host.path();
    ok(
        env.run(host, &["view", "create", "exp-2", "--from", "dev"]),
        "view create exp-2",
    );
    let vms = TempDir::new().unwrap();
    let vm = vms.path().join("work");
    let mut pointer = open(&env, host, "exp-2", &vm);

    // The token is exp-2's; a pointer naming exp-1 with it is refused.
    pointer["view"] = Value::String("exp-1".to_string());
    pointer.as_object_mut().unwrap().remove("view_id");
    std::fs::write(vm.join(".atomic-sandbox"), pointer.to_string()).unwrap();
    let refused = failure(env.run(&vm, &["sandbox", "materialize"]), "another view");
    assert!(refused.contains("FORBIDDEN"), "{refused}");

    // A forged token reaches nothing.
    pointer["view"] = Value::String("exp-2".to_string());
    pointer["token"] = Value::String("ast_forged".to_string());
    std::fs::write(vm.join(".atomic-sandbox"), pointer.to_string()).unwrap();
    let refused = failure(env.run(&vm, &["sandbox", "materialize"]), "forged token");
    assert!(refused.contains("UNAUTHORIZED"), "{refused}");

    // Past the CLI, the host's routing holds: with a token, only the
    // methods that enforce a grant's scope are reachable.
    let token = open(&env, host, "exp-2", &vms.path().join("probe"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let socket = env.socket.clone().unwrap();
    let refused = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let channel = tonic::transport::Endpoint::from_static("http://localhost")
                .connect_with_connector(tower::service_fn(move |_| {
                    let socket = socket.clone();
                    async move {
                        Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                            tokio::net::UnixStream::connect(socket).await?,
                        ))
                    }
                }))
                .await
                .unwrap();
            let mut request =
                tonic::Request::new(libatomic::atomic::ListVaultEntriesRequest::default());
            request.metadata_mut().insert_bin(
                libatomic::daemon::sandbox_grants::SANDBOX_TOKEN_METADATA,
                tonic::metadata::MetadataValue::from_bytes(token.as_bytes()),
            );
            libatomic::atomic::vault_service_client::VaultServiceClient::new(channel)
                .list_vault_entries(request)
                .await
                .unwrap_err()
        });
    assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused:?}");

    // Closed on the host: refused too.
    let vm2 = vms.path().join("other");
    open(&env, host, "exp-2", &vm2);
    ok(env.run(&vm2, &["sandbox", "materialize"]), "materialize");
    let out = ok(env.run(host, &["sandbox", "close", "exp-2"]), "close");
    assert!(out.contains("revoked"), "{out}");
    let refused = failure(env.run(&vm2, &["sandbox", "materialize"]), "closed grant");
    assert!(refused.contains("UNAUTHORIZED"), "{refused}");
    assert_only_the_cache(&vm2);
}

fn hook(env: &Env, cwd: &Path, verb: &str, payload: Value) -> Output {
    env.run_with_stdin(
        cwd,
        &["agent", "hooks", "sherpa", verb],
        Some(&payload.to_string()),
    )
}

/// The path outpost's VMs depend on: an agent's turn in a remote sandbox
/// lands its change on the view through SubmitChange, and its provenance in
/// the repository through the provenance RPCs and PublishProvenance.
#[test]
fn an_agent_turn_in_a_remote_sandbox_lands_with_its_provenance() {
    let (_server, env, host) = setup();
    let host = host.path();
    let vms = TempDir::new().unwrap();
    let vm = vms.path().join("work");
    open(&env, host, "exp-1", &vm);
    ok(env.run(&vm, &["sandbox", "materialize"]), "materialize");

    let cwd = vm.to_str().unwrap();
    let now = "2026-01-01T00:00:00Z";
    let turn = |n: u32| {
        serde_json::json!({
            "session_id": "agent-session", "cwd": cwd, "model": "m", "provider": "p",
            "turn_number": n, "intent_title": "greet the reader", "timestamp": now,
        })
    };
    ok(
        hook(
            &env,
            &vm,
            "session-start",
            serde_json::json!({
                "session_id": "agent-session", "cwd": cwd, "model": "m", "provider": "p",
                "turn_number": 0, "timestamp": now,
            }),
        ),
        "session-start",
    );
    ok(hook(&env, &vm, "turn-start", turn(1)), "turn-start");
    write(&vm, "README.md", "hello\nreader\n");
    let ended = ok(hook(&env, &vm, "turn-end", turn(1)), "turn-end");
    ok(
        hook(
            &env,
            &vm,
            "session-end",
            serde_json::json!({
                "session_id": "agent-session", "cwd": cwd, "turn_number": 1, "timestamp": now,
            }),
        ),
        "session-end",
    );

    // The turn's change is on the view, and only there.
    let log = ok(
        env.run(host, &["log", "--view", "exp-1"]),
        "host log of the draft",
    );
    assert!(
        log.contains("greet the reader"),
        "turn-end: {ended}\nlog: {log}"
    );
    let dev = ok(env.run(host, &["log"]), "host log of dev");
    assert!(
        !dev.contains("greet the reader"),
        "only on the draft: {dev}"
    );

    // Its provenance is in the repository, not only in the sandbox.
    let session = ok(
        env.run(host, &["session", "show", "agent-session"]),
        "host session",
    );
    assert!(session.contains("Turns: 1"), "{session}");
    assert!(session.contains("greet the reader"), "{session}");
    assert_only_the_cache(&vm);
}

/// A sherpa turn's provenance is the full account of how the change was
/// made: the transcript the harness writes (assistant text, tool calls, tool
/// results — Claude Code's JSONL shape) rides the change as its unhashed
/// agent turn, and feeds the session graph its LlmResponse and Execution
/// nodes. This is what a person reads at the gate; before the trace flowed,
/// the change carried a vendor and nothing else.
#[test]
fn a_turns_transcript_becomes_the_changes_provenance() {
    let (_server, env, host) = setup();
    let host = host.path();
    let vms = TempDir::new().unwrap();
    let vm = vms.path().join("work");
    open(&env, host, "exp-1", &vm);
    ok(env.run(&vm, &["sandbox", "materialize"]), "materialize");

    let cwd = vm.to_str().unwrap();
    let now = "2026-01-01T00:00:00Z";
    let sid = "agent-session";
    let turn = |n: u32, trace: Option<&str>| {
        let mut payload = serde_json::json!({
            "session_id": sid, "cwd": cwd, "model": "m", "provider": "p",
            "turn_number": n, "intent_title": "greet the reader", "timestamp": now,
            "input_tokens": 7, "output_tokens": 9, "step_count": 2,
        });
        if let Some(trace) = trace {
            payload["trace_file"] = serde_json::Value::String(trace.to_string());
        }
        payload
    };
    ok(
        hook(
            &env,
            &vm,
            "session-start",
            serde_json::json!({
                "session_id": sid, "cwd": cwd, "model": "m", "provider": "p",
                "turn_number": 0, "timestamp": now,
            }),
        ),
        "session-start",
    );

    // The turn's transcript, exactly as sherpa's harness writes it: the
    // user's prompt, an assistant turn that calls bash, the tool's result,
    // and the closing assistant text — as a BARE STRING, which is how a
    // text-only turn serializes (atomic-llm's untagged MessageContent; the
    // same shape Claude Code's JSONL uses for plain replies).
    let sessions_dir = vm.join(".atomic").join("sessions").join(sid);
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let trace = sessions_dir.join("turn-1.jsonl");
    std::fs::write(
        &trace,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"write CHECKOUT_NOTES.md"}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll write the notes."},{"type":"tool_use","id":"tu1","name":"bash","input":{"command":"echo hello > CHECKOUT_NOTES.md"}}]}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"hello"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":"Wrote CHECKOUT_NOTES.md; that completes the task."}}"#, "\n",
        ),
    )
    .unwrap();
    write(&vm, "CHECKOUT_NOTES.md", "hello\n");
    let ended = ok(
        hook(
            &env,
            &vm,
            "turn-end",
            turn(1, Some(trace.to_str().unwrap())),
        ),
        "turn-end",
    );
    assert!(!ended.contains("Error"), "{ended}");
    ok(
        hook(
            &env,
            &vm,
            "session-end",
            serde_json::json!({
                "session_id": sid, "cwd": cwd, "turn_number": 1, "timestamp": now,
            }),
        ),
        "session-end",
    );

    // The change landed on the view.
    let log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    assert!(log.contains("greet the reader"), "{log}");
    let hash = log
        .lines()
        .find(|l| l.contains("=== "))
        .and_then(|l| l.split("===").nth(1))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no change hash in: {log}"))
        .to_string();

    // Its unhashed section carries the condensed transcript — the tool
    // call and the reply, readable host-side.
    let shown = ok(
        env.run(host, &["change", &hash, "--format", "json", "--no-color"]),
        "change json",
    );
    assert!(
        shown.contains("agent_turn"),
        "no agent turn on the change: {shown}"
    );
    assert!(
        shown.contains("bash"),
        "the tool call is in the transcript: {shown}"
    );
    assert!(
        shown.contains("Wrote CHECKOUT_NOTES.md"),
        "the closing reply is there: {shown}"
    );
    // And the turn's cost, as the harness reported it at turn-end.
    assert!(
        shown.contains("input_tokens"),
        "the input tokens are there: {shown}"
    );
    assert!(
        shown.contains("output_tokens"),
        "the output tokens are there: {shown}"
    );
    assert!(
        shown.contains("step_count"),
        "the step count is there: {shown}"
    );

    // And the turn sits in the session ledger host-side, its provenance
    // bound to the change.
    let session = ok(env.run(host, &["session", "show", sid]), "host session");
    assert!(session.contains("Turns: 1"), "{session}");
    let session_json = ok(
        env.run(host, &["session", "show", sid, "--json"]),
        "session json",
    );
    assert!(
        session_json.contains(&hash[..8]),
        "the turn names the change: {session_json}"
    );
    let trace_out = ok(
        env.run(host, &["provenance", "trace", &hash]),
        "provenance trace",
    );
    assert!(
        trace_out.contains("generated"),
        "the graph binds the change: {trace_out}"
    );
    assert_only_the_cache(&vm);
}

/// Vault work in a remote sandbox is the view's: an intent written there is
/// shown there, and lands in the repository with the record that carries it.
#[test]
fn an_intent_written_in_a_remote_sandbox_lands_with_its_record() {
    let (_server, env, host) = setup();
    let host = host.path();
    let vms = TempDir::new().unwrap();
    let vm = vms.path().join("work");
    open(&env, host, "exp-1", &vm);
    ok(env.run(&vm, &["sandbox", "materialize"]), "materialize");

    let created = ok(
        env.run(&vm, &["intent", "new", "Greet readers"]),
        "intent new",
    );
    let key = created
        .split_whitespace()
        .find(|w| w.starts_with("WORK::"))
        .unwrap_or_else(|| panic!("no key in: {created}"))
        .to_string();
    let listed = ok(env.run(&vm, &["intent", "list"]), "intent list");
    assert!(listed.contains(&key), "new: {created}\nlist: {listed}");
    let shown = ok(env.run(&vm, &["intent", "show", &key]), "intent show");
    assert!(shown.contains("Greet readers"), "{shown}");
    ok(
        env.run(&vm, &["record", "-a", "-m", "the intent"]),
        "record the intent",
    );
    let log = ok(env.run(host, &["log", "--view", "exp-1"]), "host log");
    assert!(log.contains("the intent"), "{log}");
    let intents = ok(env.run(host, &["intent", "list"]), "host intent list");
    assert!(intents.contains(&key), "{intents}");
    assert_only_the_cache(&vm);
}
