//! The SandboxService data ops, driven in-process: a remote sandbox's cache
//! records against the repository through the handlers, exactly as a host's
//! transport would carry it — but with the transport replaced by a direct
//! call into `SandboxImpl`.
//!
//! The cache side reaches the handlers through a test `RemoteSandboxLink`
//! built from `daemon::sandbox_wire`, the same conversions a host's link uses.

// tonic's `Status` is the handlers' error type, as in `daemon`.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use atomic_core::change::session::{SessionCheckpointPublication, SessionTurn};
use atomic_core::change::{Author, ChangeHeader, ProvenanceGraph};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    ChangeFile, InsertOptions, RecordOptions, RemoteSandboxLink, Repository, SandboxSkeleton,
    SandboxSlice, SubmitRejection, Submitted, SubmittedOutcome, TrackingOptions, ViewEntryKind,
};
use chrono::{DateTime, Utc};
use libatomic::atomic::sandbox_service_server::SandboxService;
use libatomic::atomic::*;
use libatomic::daemon::sandbox_grants::{
    InMemorySandboxGrants, SandboxGrant, SANDBOX_TOKEN_METADATA,
};
use libatomic::daemon::sandbox_wire as wire;
use libatomic::daemon::services_provenance as provenance;
use libatomic::daemon::services_sandbox::SandboxImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tokio_stream::StreamExt;
use tonic::{Code, Request, Status};

const ALL: &[&str] = &[
    "sandbox.read",
    "sandbox.submit",
    "provenance.read",
    "provenance.write",
    "agent.checkpoint",
    "vault.read",
    "vault.write",
];

// ── the host: a repository and the handlers over it ─────────────────────

struct Host {
    _dir: TempDir,
    root: PathBuf,
    state: Arc<DaemonState>,
    repository: RepositoryRef,
    rt: tokio::runtime::Runtime,
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn header(message: &str) -> ChangeHeader {
    ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build()
}

fn record_options() -> RecordOptions {
    RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(false)
}

/// Record everything in `repo`'s working tree and land it: on the host,
/// applied locally; in a sandbox's cache, submitted through the service.
fn record(repo: &Repository, message: &str) -> Result<Hash, String> {
    let outcome = repo
        .record(header(message), record_options())
        .map_err(|e| e.to_string())?;
    repo.write_recorded(&outcome, InsertOptions::default())
        .map_err(|e| e.to_string())?;
    Ok(*outcome.hash())
}

fn host() -> Host {
    host_with(DaemonState::new())
}

/// A repository with two recorded files on `dev`, and a state serving it.
fn host_with(state: DaemonState) -> Host {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    {
        let repo = Repository::init(&root).unwrap();
        write(&root, "README.md", "hello\n");
        write(&root, "src/lib.rs", "pub fn f() {}\n");
        repo.add("README.md", TrackingOptions::default()).unwrap();
        repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
        record(&repo, "first").unwrap();
    }
    let state = Arc::new(state);
    let repository = state.register(root.clone()).repository_ref();
    let root = root.canonicalize().unwrap();
    Host {
        _dir: dir,
        root,
        state,
        repository,
        rt: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap(),
    }
}

impl Host {
    /// The repository, opened for the closure only: the handlers open it per
    /// request, and redb allows one handle.
    fn with_repo<T>(&self, f: impl FnOnce(&mut Repository) -> T) -> T {
        let mut repo = Repository::open_existing(&self.root).unwrap();
        f(&mut repo)
    }

    fn draft(&self, name: &str) {
        self.with_repo(|repo| repo.create_view_from(name, "dev").unwrap());
    }

    fn service(&self) -> SandboxImpl {
        SandboxImpl {
            state: Arc::clone(&self.state),
        }
    }

    fn open_request(&self, view: &str, capabilities: &[&str], ttl: u64) -> OpenSandboxRequest {
        OpenSandboxRequest {
            repository: Some(self.repository.clone()),
            meta: None,
            view: String::new(),
            acting_as_did: Some("did:key:zAgent".to_string()),
            ttl_secs: Some(ttl),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            target: Some(wire::view_ref(view)),
        }
    }

    /// Mint a token for `view` (as the local admin).
    fn open(&self, view: &str, capabilities: &[&str]) -> SandboxOpened {
        self.rt
            .block_on(self.service().open_sandbox(Request::new(self.open_request(
                view,
                capabilities,
                600,
            ))))
            .unwrap()
            .into_inner()
            .opened
            .unwrap()
    }

    fn tree(&self, view: &str) -> BTreeMap<String, String> {
        self.with_repo(|repo| {
            let mut files = BTreeMap::new();
            repo.materialize_view_entries::<()>(view, |e| {
                if e.kind == ViewEntryKind::File {
                    files.insert(e.path, String::from_utf8_lossy(&e.content).into_owned());
                }
                Ok(())
            })
            .unwrap()
            .unwrap();
            files
        })
    }

    fn on_view(&self, view: &str, hash: &Hash) -> bool {
        self.with_repo(|repo| repo.first_foreign_change(view, &[*hash]).unwrap().is_none())
    }
}

fn with_token<T>(message: T, token: &[u8]) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert_bin(
        SANDBOX_TOKEN_METADATA,
        tonic::metadata::MetadataValue::from_bytes(token),
    );
    request
}

fn assert_refused<T>(result: Result<T, Status>, code: Code, contract: &str) {
    match result {
        Ok(_) => panic!("expected {contract}, got a success"),
        Err(status) => {
            assert_eq!(status.code(), code, "{status:?}");
            assert!(
                status.message().contains(contract),
                "expected {contract}: {status:?}"
            );
        }
    }
}

// ── the link: the cache's way to the handlers ───────────────────────────

struct Route {
    state: Arc<DaemonState>,
    repository: RepositoryRef,
    token: Vec<u8>,
    rt: tokio::runtime::Handle,
}

fn routes() -> &'static Mutex<HashMap<PathBuf, Arc<Route>>> {
    static ROUTES: OnceLock<Mutex<HashMap<PathBuf, Arc<Route>>>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        atomic_repository::set_remote_sandbox_link(Box::new(TestLink));
        Mutex::default()
    })
}

struct TestLink;

impl TestLink {
    fn route(root: &Path) -> Result<Arc<Route>, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        routes()
            .lock()
            .unwrap()
            .get(&root)
            .cloned()
            .ok_or_else(|| format!("no route for {}", root.display()))
    }
}

fn status_text(status: Status) -> String {
    status.message().to_string()
}

impl RemoteSandboxLink for TestLink {
    fn file_states(&self, root: &Path, inodes: Vec<u64>) -> Result<SandboxSlice, String> {
        let route = Self::route(root)?;
        let service = SandboxImpl {
            state: Arc::clone(&route.state),
        };
        let response = route
            .rt
            .block_on(service.get_file_states(with_token(
                GetFileStatesRequest {
                    repository: Some(route.repository.clone()),
                    inodes,
                    target: None,
                    expected_snapshot: None,
                },
                &route.token,
            )))
            .map_err(status_text)?
            .into_inner();
        wire::slice_from_proto(&response.slice.ok_or("no slice")?)
    }

    fn submit(
        &self,
        root: &Path,
        base_state: String,
        hash: Hash,
        bytes: Vec<u8>,
    ) -> Result<SubmittedOutcome, String> {
        let route = Self::route(root)?;
        let service = SandboxImpl {
            state: Arc::clone(&route.state),
        };
        let fence = Hash::from_base32(base_state.as_bytes()).ok_or("bad base state")?;
        let response = route
            .rt
            .block_on(service.submit_change(with_token(
                SubmitChangeRequest {
                    repository: Some(route.repository.clone()),
                    meta: None,
                    change: Some(wire::change_bundle(&hash, bytes)),
                    expected_snapshot: Some(wire::view_snapshot("", &fence, None)),
                    target: None,
                },
                &route.token,
            )))
            .map_err(status_text)?
            .into_inner();
        match response.outcome.ok_or("no outcome")? {
            submit_change_response::Outcome::Submitted(submitted) => {
                let skeleton =
                    wire::skeleton_from_proto(&submitted.skeleton.ok_or("no skeleton")?)?;
                let effective = response
                    .meta
                    .and_then(|m| m.snapshot)
                    .map(|s| wire::snapshot_fence(&s))
                    .transpose()?
                    .unwrap_or_default();
                Ok(Ok((
                    Submitted {
                        hash,
                        state: skeleton.view.state.to_base32(),
                        effective,
                    },
                    skeleton,
                )))
            }
            submit_change_response::Outcome::Refused(refused) => {
                let info = refused.rejection.ok_or("no rejection")?;
                let rejection =
                    wire::rejection_from_error_info(&info).ok_or_else(|| info.message.clone())?;
                let skeleton = refused
                    .skeleton
                    .map(|s| wire::skeleton_from_proto(&s))
                    .transpose()?;
                Ok(Err((rejection, skeleton)))
            }
        }
    }

    fn changes(&self, root: &Path, hashes: Vec<Hash>) -> Result<Vec<ChangeFile>, String> {
        let route = Self::route(root)?;
        let service = SandboxImpl {
            state: Arc::clone(&route.state),
        };
        let response = route
            .rt
            .block_on(
                service.get_changes(with_token(
                    GetChangesRequest {
                        repository: Some(route.repository.clone()),
                        hashes: hashes
                            .iter()
                            .map(libatomic::daemon::convert::hash_proto)
                            .collect(),
                        target: None,
                        expected_snapshot: None,
                    },
                    &route.token,
                )),
            )
            .map_err(status_text)?
            .into_inner();
        match response.outcome.ok_or("no outcome")? {
            get_changes_response::Outcome::Changes(payload) => payload
                .changes
                .iter()
                .map(wire::change_from_bundle)
                .collect(),
            get_changes_response::Outcome::Refused(info) => Err(info.message),
        }
    }

    fn publish_provenance(
        &self,
        root: &Path,
        graph: Vec<u8>,
        turn: SessionTurn,
    ) -> Result<Result<SessionCheckpointPublication, SubmitRejection>, String> {
        let route = Self::route(root)?;
        let service = SandboxImpl {
            state: Arc::clone(&route.state),
        };
        let response = route
            .rt
            .block_on(service.publish_provenance(with_token(
                PublishProvenanceRequest {
                    repository: Some(route.repository.clone()),
                    meta: None,
                    graph: Some(wire::provenance_graph_bytes(graph)),
                    turn: Some(libatomic::atomic::SessionTurn {
                        session_id: turn.session_id.clone(),
                        turn_number: turn.turn_number,
                    }),
                    expected_generation: 0,
                    target: None,
                    session_turn: Some(wire::session_turn_bytes(&turn)?),
                },
                &route.token,
            )))
            .map_err(status_text)?
            .into_inner();
        match response.outcome.ok_or("no outcome")? {
            publish_provenance_response::Outcome::Publication(published) => {
                Ok(Ok(wire::publication_from(&published)?))
            }
            publish_provenance_response::Outcome::Refused(info) => Ok(Err(
                wire::rejection_from_error_info(&info).ok_or_else(|| info.message.clone())?,
            )),
        }
    }
}

// ── the sandbox: a directory with a pointer and a cache ─────────────────

struct Vm {
    _dir: TempDir,
    root: PathBuf,
    cache: Repository,
    entries: u64,
}

/// Materialize `opened`'s view into a fresh directory, as a sandbox does:
/// every entry through the kernel's path-checked writer, then the cache.
fn materialize(host: &Host, opened: &SandboxOpened) -> Vm {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let (entries, skeleton) = materialize_into(host, &opened.token, &root);
    let cache = adopt(host, opened, &root, &skeleton);
    Vm {
        _dir: dir,
        root,
        cache,
        entries,
    }
}

fn materialize_into(host: &Host, token: &[u8], root: &Path) -> (u64, SandboxSkeleton) {
    let frames: Vec<MaterializeFrame> = host.rt.block_on(async {
        host.service()
            .materialize(with_token(
                MaterializeRequest {
                    repository: Some(host.repository.clone()),
                    target: None,
                },
                token,
            ))
            .await
            .unwrap()
            .into_inner()
            .map(|frame| frame.unwrap())
            .collect()
            .await
    });
    let mut written = 0;
    for frame in frames {
        match frame.frame.unwrap() {
            materialize_frame::Frame::Entry(entry) => {
                atomic_repository::write_sandbox_entry(
                    root,
                    &entry.path,
                    wire::tree_entry_kind(&entry).unwrap(),
                    entry.mode.unwrap_or(0o644),
                    entry.inline.as_deref().unwrap_or_default(),
                )
                .unwrap();
                written += 1;
            }
            materialize_frame::Frame::Done(done) => {
                assert_eq!(done.entries, written, "every entry arrived");
                let skeleton = done.skeleton.unwrap();
                assert_eq!(
                    skeleton.snapshot.as_ref().unwrap().effective_state,
                    done.snapshot,
                    "one coherent state"
                );
                return (written, wire::skeleton_from_proto(&skeleton).unwrap());
            }
        }
    }
    panic!("materialize ended without its summary");
}

/// Make the cache at `root` and route its link to the handlers with
/// `opened`'s token.
fn adopt(
    host: &Host,
    opened: &SandboxOpened,
    root: &Path,
    skeleton: &SandboxSkeleton,
) -> Repository {
    let repository = opened.repository.as_ref().unwrap();
    atomic_repository::write_remote_sandbox_pointer(
        root,
        &atomic_repository::RemoteSandboxPointer {
            repository: atomic_repository::RemoteRepositoryRef {
                authority: repository.authority.clone(),
                repository_id: data_encoding::HEXLOWER.encode(&repository.repository_id),
            },
            view: opened.view.clone(),
            view_id: None,
            token: String::from_utf8(opened.token.clone()).unwrap(),
        },
    )
    .unwrap();
    routes().lock().unwrap().insert(
        root.to_path_buf(),
        Arc::new(Route {
            state: Arc::clone(&host.state),
            repository: opened.repository.clone().unwrap(),
            token: opened.token.clone(),
            rt: host.rt.handle().clone(),
        }),
    );
    let cache = Repository::create_remote_sandbox_cache(root, skeleton).unwrap();
    assert!(cache.is_remote_sandbox());
    cache
}

// ── the tests ───────────────────────────────────────────────────────────

#[test]
fn a_sandbox_materializes_records_and_lands_on_its_view() {
    let host = host();
    host.draft("exp-1");
    let opened = host.open("exp-1", ALL);
    assert!(opened.token.starts_with(b"ast_"));
    assert_eq!(opened.view, "exp-1");
    assert!(opened.repository.as_ref().unwrap().workspace_id.is_none());

    let vm = materialize(&host, &opened);
    assert!(
        vm.entries >= 3,
        "README, src/ and src/lib.rs: {}",
        vm.entries
    );
    assert_eq!(
        std::fs::read_to_string(vm.root.join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(
        !vm.root.join(".atomic").exists(),
        "nothing of the repository"
    );

    // Record in the cache: the change is computed locally and lands through
    // SubmitChange.
    write(&vm.root, "README.md", "hello\nfrom the sandbox\n");
    write(&vm.root, "src/new.rs", "pub fn n() {}\n");
    vm.cache
        .add("src/new.rs", TrackingOptions::default())
        .unwrap();
    let first = record(&vm.cache, "from the sandbox").unwrap();
    let tree = host.tree("exp-1");
    assert_eq!(tree["README.md"], "hello\nfrom the sandbox\n");
    assert_eq!(tree["src/new.rs"], "pub fn n() {}\n");
    assert!(host.on_view("exp-1", &first));
    assert!(!host.on_view("dev", &first), "only on the granted view");
    assert!(!host.tree("dev").contains_key("src/new.rs"));

    // The next record builds on the first.
    write(&vm.root, "src/new.rs", "pub fn n() { 2 }\n");
    let second = record(&vm.cache, "again from the sandbox").unwrap();
    assert_eq!(host.tree("exp-1")["src/new.rs"], "pub fn n() { 2 }\n");
    assert!(host.on_view("exp-1", &second));

    // The cache holds the draft as itself — scope, parent, own log — so
    // what it computes about the view's own work is what the repository
    // computes (it once held the view flattened, and triage counted
    // inherited history as the sandbox's).
    let theirs = vm.cache.triage_candidate_set("exp-1", "dev").unwrap();
    let ours = host.with_repo(|repo| repo.triage_candidate_set("exp-1", "dev").unwrap());
    assert_eq!(theirs.only_in_feature, ours.only_in_feature);
    assert_eq!(theirs.closure_additions, ours.closure_additions);
    let mut own = vec![first.to_base32(), second.to_base32()];
    own.sort();
    assert_eq!(
        ours.only_in_feature, own,
        "the sandbox's two changes, nothing inherited"
    );

    // GetChanges: the cache fetches the change files its view has.
    let fetched = vm.cache.fetch_remote_sandbox_changes().unwrap();
    assert!(fetched >= 1, "the base change at least: {fetched}");
    assert_eq!(
        vm.cache.missing_sandbox_changes().unwrap(),
        Vec::<Hash>::new()
    );

    // A second sandbox of the view sees the first one's work.
    let other = materialize(&host, &host.open("exp-1", ALL));
    assert_eq!(
        std::fs::read_to_string(other.root.join("src/new.rs")).unwrap(),
        "pub fn n() { 2 }\n"
    );
}

#[test]
fn a_stale_submit_is_refused_with_the_live_skeleton() {
    let host = host();
    host.draft("exp-1");
    let opened = host.open("exp-1", ALL);
    let vm = materialize(&host, &opened);

    write(&vm.root, "README.md", "hello\nsandbox\n");
    let landed = record(&vm.cache, "from the sandbox").unwrap();
    assert!(host.on_view("exp-1", &landed));

    // `dev` moves underneath the draft, with no sandbox involved. The
    // draft's own log does not move; its effective perspective does.
    host.with_repo(|repo| {
        write(repo.root(), "src/lib.rs", "pub fn f() { 1 }\n");
        write(repo.root(), "OUT-OF-BAND.md", "elsewhere\n");
        repo.add("OUT-OF-BAND.md", TrackingOptions::default())
            .unwrap();
        record(repo, "out of band").unwrap();
    });

    // A change recorded against the old perspective is refused as stale.
    write(&vm.root, "README.md", "hello\nsandbox\nout of date\n");
    let refused = record(&vm.cache, "against the old view").unwrap_err();
    assert!(refused.contains("moved on"), "{refused}");
    assert_eq!(
        host.tree("exp-1")["README.md"],
        "hello\nsandbox\n",
        "nothing of the refused change landed"
    );
    // ...and the refusal carried the live skeleton: the cache's fence is the
    // repository's again.
    let live = host.with_repo(|repo| repo.sandbox_effective_state("exp-1").unwrap());
    assert_eq!(
        vm.cache.remote_sandbox_effective_state().unwrap(),
        live.to_base32()
    );
    // Its files are not current, though: recording over them would read what
    // landed underneath as this sandbox's edits. It refuses until it is
    // materialized again.
    assert!(vm.cache.remote_sandbox_is_behind());
    let behind = record(&vm.cache, "over stale files").unwrap_err();
    assert!(behind.contains("materialize it again"), "{behind}");

    // A sandbox that fell behind materializes again, and records on the
    // view as it is now.
    let fresh = materialize(&host, &opened);
    assert_eq!(
        std::fs::read_to_string(fresh.root.join("src/lib.rs")).unwrap(),
        "pub fn f() { 1 }\n"
    );
    write(
        &fresh.root,
        "README.md",
        "hello\nsandbox\nafter the view moved\n",
    );

    // But first, the stale refusal over the wire, directly: VIEW_STALE with
    // the live skeleton.
    let outcome = fresh.cache.record(header("raw"), record_options()).unwrap();
    let stale = Hash::of(b"a perspective nobody has");
    let response = host
        .rt
        .block_on(host.service().submit_change(with_token(
            SubmitChangeRequest {
                repository: Some(host.repository.clone()),
                meta: None,
                change: Some(wire::change_bundle(
                    outcome.hash(),
                    outcome.v3_bytes().unwrap().to_vec(),
                )),
                expected_snapshot: Some(wire::view_snapshot("exp-1", &stale, None)),
                target: None,
            },
            &opened.token,
        )))
        .unwrap()
        .into_inner();
    let Some(submit_change_response::Outcome::Refused(refused)) = response.outcome else {
        panic!("expected a refusal: {response:?}");
    };
    assert_eq!(
        refused.rejection.as_ref().unwrap().code,
        ErrorCode::ViewStale as i32
    );
    let skeleton = wire::skeleton_from_proto(refused.skeleton.as_ref().unwrap()).unwrap();
    assert_eq!(skeleton.view.name, "exp-1");
    assert_eq!(
        refused.skeleton.unwrap().snapshot.unwrap().effective_state,
        Some(libatomic::daemon::convert::hash_proto(&live))
    );

    // Now for real — keeping what landed underneath it.
    let retried = record(&fresh.cache, "after the view moved").unwrap();
    assert!(host.on_view("exp-1", &retried));
    let tree = host.tree("exp-1");
    assert_eq!(tree["README.md"], "hello\nsandbox\nafter the view moved\n");
    assert_eq!(tree["src/lib.rs"], "pub fn f() { 1 }\n");
    assert_eq!(
        tree["OUT-OF-BAND.md"], "elsewhere\n",
        "what landed underneath survives"
    );
    assert!(!fresh.cache.remote_sandbox_is_behind());
}

#[test]
fn a_token_reaches_its_view_and_nothing_else() {
    let host = host();
    host.draft("exp-1");
    host.draft("exp-2");
    let opened = host.open("exp-1", &["sandbox.read"]);
    let service = host.service();
    let materialize = |token: &[u8], target: Option<ViewRef>| {
        host.rt.block_on(service.materialize(with_token(
            MaterializeRequest {
                repository: Some(host.repository.clone()),
                target,
            },
            token,
        )))
    };

    assert!(materialize(&opened.token, None).is_ok());
    assert!(materialize(&opened.token, Some(wire::view_ref("exp-1"))).is_ok());
    // Another view, by name or by id.
    assert_refused(
        materialize(&opened.token, Some(wire::view_ref("exp-2"))),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    assert_refused(
        materialize(
            &opened.token,
            Some(ViewRef {
                view_id: wire::view_ref("dev").view_id,
                name: None,
            }),
        ),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    // A host workspace, never.
    let mut workspace = host.repository.clone();
    workspace.workspace_id = Some("host".to_string());
    assert_refused(
        host.rt.block_on(service.materialize(with_token(
            MaterializeRequest {
                repository: Some(workspace),
                target: None,
            },
            &opened.token,
        ))),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    // A forged token reaches nothing.
    assert_refused(
        materialize(b"ast_forged", None),
        Code::Unauthenticated,
        "UNAUTHORIZED",
    );
    // A grant without sandbox.submit cannot submit.
    assert_refused(
        host.rt.block_on(service.submit_change(with_token(
            SubmitChangeRequest {
                repository: Some(host.repository.clone()),
                meta: None,
                change: Some(wire::change_bundle(&Hash::of(b"x"), b"x".to_vec())),
                expected_snapshot: Some(wire::view_snapshot("exp-1", &Hash::of(b"s"), None)),
                target: None,
            },
            &opened.token,
        ))),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    // Grant administration is local only.
    assert_refused(
        host.rt.block_on(service.open_sandbox(with_token(
            host.open_request("exp-2", ALL, 600),
            &opened.token,
        ))),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    // Undelegable capabilities and absurd lifetimes are refused, and the
    // registry keeps serving.
    for (caps, ttl) in [
        (&["repository.write"][..], 600),
        (&[][..], 600),
        (&["sandbox.read"][..], 0),
        (&["sandbox.read"][..], 10_000_000_000_000_000),
    ] {
        assert_refused(
            host.rt.block_on(
                service.open_sandbox(Request::new(host.open_request("exp-2", caps, ttl))),
            ),
            Code::InvalidArgument,
            "INVALID_ARGUMENT",
        );
    }
    assert_refused(
        host.rt
            .block_on(service.open_sandbox(Request::new(host.open_request(
                "no-such-view",
                ALL,
                600,
            )))),
        Code::NotFound,
        "NOT_FOUND",
    );
    assert!(materialize(&opened.token, None).is_ok());

    // A token for another repository reaches nothing here.
    let elsewhere = self::host();
    let foreign = elsewhere.open("dev", ALL);
    // (Registered in another state: unknown here.)
    assert_refused(
        materialize(&foreign.token, None),
        Code::Unauthenticated,
        "UNAUTHORIZED",
    );
    // A grant the host attached itself, for another repository: refused.
    let mut request = Request::new(MaterializeRequest {
        repository: Some(host.repository.clone()),
        target: None,
    });
    request.extensions_mut().insert(SandboxGrant {
        repository_id: elsewhere.repository.repository_id.clone(),
        view: "dev".to_string(),
        view_id: None,
        acting_as: None,
        capabilities: ["sandbox.read".to_string()].into(),
        expires: Utc::now() + chrono::Duration::hours(1),
    });
    assert_refused(
        host.rt.block_on(service.materialize(request)),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    // ...and one for this repository's view works without any token.
    let mut request = Request::new(MaterializeRequest {
        repository: Some(host.repository.clone()),
        target: None,
    });
    request.extensions_mut().insert(SandboxGrant {
        repository_id: host.repository.repository_id.clone(),
        view: "exp-2".to_string(),
        view_id: None,
        acting_as: None,
        capabilities: ["sandbox.read".to_string()].into(),
        expires: Utc::now() + chrono::Duration::hours(1),
    });
    assert!(host.rt.block_on(service.materialize(request)).is_ok());
}

#[test]
fn renew_keeps_the_token_close_ends_it_and_reopening_replaces_it() {
    let now = Arc::new(Mutex::new(Utc::now()));
    let clock = Arc::clone(&now);
    let host = host_with(DaemonState::new().with_sandbox_grants(Arc::new(
        InMemorySandboxGrants::with_clock(Arc::new(move || -> DateTime<Utc> {
            *clock.lock().unwrap()
        })),
    )));
    host.draft("exp-1");
    let service = host.service();
    let materialize = |token: &[u8]| {
        host.rt.block_on(service.materialize(with_token(
            MaterializeRequest {
                repository: Some(host.repository.clone()),
                target: None,
            },
            token,
        )))
    };
    let renew = |ttl: u64| {
        host.rt
            .block_on(service.renew_sandbox(Request::new(RenewSandboxRequest {
                repository: Some(host.repository.clone()),
                meta: None,
                view: "exp-1".to_string(),
                ttl_secs: ttl,
                target: None,
            })))
    };

    let opened = host.open("exp-1", ALL);
    assert!(materialize(&opened.token).is_ok());

    // Expired: refused, and nothing renews a dead token.
    *now.lock().unwrap() += chrono::Duration::seconds(601);
    assert_refused(
        materialize(&opened.token),
        Code::Unauthenticated,
        "UNAUTHORIZED",
    );
    assert_refused(renew(600), Code::PermissionDenied, "SANDBOX_TOKEN");

    // A live token renewed keeps working past its old expiry.
    let opened = host.open("exp-1", ALL);
    *now.lock().unwrap() += chrono::Duration::seconds(500);
    let renewed = renew(3600).unwrap().into_inner();
    assert!(
        renewed.expires_at.unwrap().seconds > opened.expires_at.unwrap().seconds,
        "the expiry moved"
    );
    *now.lock().unwrap() += chrono::Duration::seconds(500);
    assert!(
        materialize(&opened.token).is_ok(),
        "past the original expiry"
    );
    assert_refused(renew(0), Code::InvalidArgument, "INVALID_ARGUMENT");

    // Reopening replaces the view's token.
    let replacement = host.open("exp-1", ALL);
    assert_refused(
        materialize(&opened.token),
        Code::Unauthenticated,
        "UNAUTHORIZED",
    );
    assert!(materialize(&replacement.token).is_ok());

    // Close revokes it, once.
    let close = || {
        host.rt
            .block_on(service.close_sandbox(Request::new(CloseSandboxRequest {
                repository: Some(host.repository.clone()),
                meta: None,
                view: String::new(),
                target: Some(wire::view_ref("exp-1")),
            })))
            .unwrap()
            .into_inner()
            .revoked
    };
    assert!(close());
    assert!(!close());
    assert_refused(
        materialize(&replacement.token),
        Code::Unauthenticated,
        "UNAUTHORIZED",
    );
}

#[test]
fn a_view_deleted_and_recreated_does_not_revive_its_grant() {
    let host = host();
    host.draft("exp-1");
    let opened = host.open("exp-1", ALL);
    host.with_repo(|repo| {
        repo.delete_view("exp-1").unwrap();
        repo.create_view_from("exp-1", "dev").unwrap();
    });
    assert_refused(
        host.rt.block_on(host.service().materialize(with_token(
            MaterializeRequest {
                repository: Some(host.repository.clone()),
                target: None,
            },
            &opened.token,
        ))),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
}

/// Several sandboxes at once, each on its own draft, each recording twice:
/// every change lands on its own draft and on no other. The second record is
/// the one that once found a slice's CRDT rows crossing views.
#[test]
fn several_sandboxes_record_at_once_and_stay_apart() {
    const SANDBOXES: usize = 6;
    let host = Arc::new(host());
    let names: Vec<String> = (0..SANDBOXES).map(|i| format!("sb-{i:03}")).collect();
    for name in &names {
        host.draft(name);
    }
    let gate = Arc::new(std::sync::Barrier::new(SANDBOXES));
    let threads: Vec<_> = names
        .iter()
        .map(|name| {
            let (host, gate, name) = (Arc::clone(&host), Arc::clone(&gate), name.clone());
            std::thread::spawn(move || {
                let opened = host.open(&name, ALL);
                gate.wait();
                let vm = materialize(&host, &opened);
                write(&vm.root, "README.md", &format!("hello\nfrom {name}\n"));
                write(&vm.root, &format!("{name}.txt"), &format!("only {name}\n"));
                vm.cache
                    .add(format!("{name}.txt"), TrackingOptions::default())
                    .unwrap();
                let first = record(&vm.cache, &format!("first by {name}")).unwrap();
                write(
                    &vm.root,
                    "README.md",
                    &format!("hello\nfrom {name}\nmore\n"),
                );
                let second = record(&vm.cache, &format!("second by {name}")).unwrap();
                (name, first, second)
            })
        })
        .collect();
    let landed: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();

    for (name, first, second) in &landed {
        let tree = host.tree(name);
        assert_eq!(tree["README.md"], format!("hello\nfrom {name}\nmore\n"));
        assert_eq!(tree[&format!("{name}.txt")], format!("only {name}\n"));
        assert!(host.on_view(name, first) && host.on_view(name, second));
        for (other, other_first, other_second) in &landed {
            if other != name {
                assert!(
                    !host.on_view(name, other_first),
                    "{name} has {other}'s change"
                );
                assert!(
                    !host.on_view(name, other_second),
                    "{name} has {other}'s change"
                );
                assert!(!tree.contains_key(&format!("{other}.txt")));
            }
        }
        assert!(!host.on_view("dev", first), "a draft's work reached dev");
    }
    assert_eq!(host.tree("dev")["README.md"], "hello\n");
}

/// A sandbox on the checked-out view itself: what lands is written into the
/// host's working copy too, so the host's status never offers to delete the
/// sandbox's file (and its next `record -a` never commits that deletion).
#[test]
fn a_sandbox_on_the_checked_out_view_lands_in_the_working_copy() {
    let host = host();
    let opened = host.open("dev", ALL);
    let vm = materialize(&host, &opened);
    write(&vm.root, "from-sandbox.txt", "sandbox\n");
    vm.cache
        .add("from-sandbox.txt", TrackingOptions::default())
        .unwrap();
    record(&vm.cache, "sandbox on dev").unwrap();
    assert_eq!(
        std::fs::read_to_string(host.root.join("from-sandbox.txt")).unwrap(),
        "sandbox\n"
    );
    let status = host.with_repo(|repo| {
        repo.status(atomic_repository::status::StatusOptions::default())
            .unwrap()
    });
    assert!(
        status
            .entries()
            .iter()
            .all(|e| e.status() != atomic_repository::status::FileStatus::Deleted),
        "the host offers to delete the sandbox's file"
    );
}

fn reserve(
    host: &Host,
    token: Option<&[u8]>,
    session: &str,
    turn: u32,
) -> Result<StoredProvenanceTurn, Status> {
    let message = ReserveTurnRequest {
        repository: Some(host.repository.clone()),
        meta: None,
        session_id: session.to_string(),
        turn_number: turn,
        view: None,
    };
    let request = match token {
        Some(token) => with_token(message, token),
        None => Request::new(message),
    };
    host.rt
        .block_on(provenance::reserve_turn_impl(&host.state, request))
        .map(|r| r.into_inner().turn.unwrap())
}

#[test]
fn provenance_a_sandbox_starts_is_its_own() {
    let host = host();
    host.draft("exp-1");
    host.draft("exp-2");
    let one = host.open("exp-1", ALL);
    let two = host.open("exp-2", ALL);

    // A session a sandbox starts is its own; another's is not, nor is one
    // the host already wrote to.
    let turn = reserve(&host, Some(&one.token), "vm-session", 1).unwrap();
    reserve(&host, None, "host-session", 1).unwrap();
    assert_refused(
        reserve(&host, Some(&one.token), "host-session", 2),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    assert_refused(
        reserve(&host, Some(&two.token), "vm-session", 2),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
    reserve(&host, Some(&one.token), "vm-session", 2).unwrap();

    // Its turn is its own: another sandbox cannot append to it.
    let append = |token: &[u8]| {
        host.rt.block_on(provenance::append_envelopes_impl(
            &host.state,
            with_token(
                AppendEnvelopesRequest {
                    repository: Some(host.repository.clone()),
                    meta: None,
                    provenance_id: turn.provenance_id,
                    expected_generation: turn.generation,
                    envelopes: Vec::new(),
                },
                token,
            ),
        ))
    };
    assert_refused(append(&two.token), Code::PermissionDenied, "FORBIDDEN");
    assert!(append(&one.token).is_ok());

    // A grant with no provenance capabilities reserves nothing.
    host.draft("exp-3");
    let read_only = host.open("exp-3", &["sandbox.read"]);
    assert_refused(
        reserve(&host, Some(&read_only.token), "fresh", 1),
        Code::PermissionDenied,
        "FORBIDDEN",
    );
}

/// The whole turn: the sandbox records, then publishes the checkpoint that
/// explains its change through PublishProvenance — which lands in the
/// repository, and only for a session the sandbox owns.
#[test]
fn a_sandbox_publishes_provenance_for_its_own_work() {
    let host = host();
    host.draft("exp-1");
    let opened = host.open("exp-1", ALL);
    let vm = materialize(&host, &opened);
    write(&vm.root, "README.md", "hello\nreader\n");
    let change = record(&vm.cache, "greet the reader").unwrap();

    let graph = |session: &str, explained: Vec<Hash>| {
        ProvenanceGraph::builder(session, "opencode")
            .agent_display_name("OpenCode")
            .agent_vendor("openai")
            .changes_explained(explained)
            .timestamp(12_000)
            .build()
    };
    let turn_for = |graph: &ProvenanceGraph| SessionTurn {
        session_id: graph.session_id.clone(),
        turn_number: 0,
        goal: Some("greet the reader".to_string()),
        provenance_hash: Hash::of(&graph.serialize().unwrap()),
        change_hashes: graph.changes_explained.clone(),
        previous_provenance: None,
        timestamp: graph.timestamp,
        plan_id: None,
        todos: Vec::new(),
    };

    // Not its session yet: refused.
    let unclaimed = graph("agent-session", vec![change]);
    let refused = vm
        .cache
        .publish_provenance_checkpoint(&unclaimed, turn_for(&unclaimed))
        .unwrap_err();
    assert!(refused.to_string().contains("FORBIDDEN"), "{refused}");

    // Claimed by reserving a turn, it publishes — in the repository.
    reserve(&host, Some(&opened.token), "agent-session", 1).unwrap();
    let publication = vm
        .cache
        .publish_provenance_checkpoint(&unclaimed, turn_for(&unclaimed))
        .unwrap();
    assert_eq!(publication.turn.session_id, "agent-session");
    let (_, turns) = host
        .with_repo(|repo| repo.get_session_ledger("agent-session").unwrap())
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].goal.as_deref(), Some("greet the reader"));

    // Provenance explaining a change the view cannot see is refused whole.
    let foreign = graph("agent-session", vec![Hash::of(b"a change on no view")]);
    let refused = vm
        .cache
        .publish_provenance_checkpoint(&foreign, turn_for(&foreign))
        .unwrap_err();
    assert!(
        refused.to_string().contains("not on this view"),
        "{refused}"
    );
}
