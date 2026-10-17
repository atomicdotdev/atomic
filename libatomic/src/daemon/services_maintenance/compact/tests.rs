use super::*;
use redb::{Database, ReadableTableMetadata, TableDefinition};

const DATA: TableDefinition<u64, &[u8]> = TableDefinition::new("compaction_data");

fn request(handle: &crate::daemon::state::RepoHandle) -> Request<CompactDatabaseRequest> {
    Request::new(CompactDatabaseRequest {
        repository: Some(handle.repository_ref()),
        meta: Some(crate::atomic::RequestMeta {
            request_id: uuid::Uuid::new_v4().to_string(),
            observed_at: None,
        }),
    })
}

#[tokio::test]
async fn busy_reader_and_writer_time_out_then_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let state = Arc::new(DaemonState::new());
    let handle = state.register(dir.path().to_path_buf());
    let call = request(&handle);
    let id = call.get_ref().meta.as_ref().unwrap().request_id.clone();
    let error = compact(&state, call, Duration::from_millis(20))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(
        ErrorInfo::decode(error.details()).unwrap().request_id,
        Some(id)
    );
    drop(repo);
    let reader = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(
        compact(&state, request(&handle), Duration::from_millis(20))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    drop(reader);
    let call = request(&handle);
    let id = call.get_ref().meta.as_ref().unwrap().request_id.clone();
    let report = compact(&state, call, Duration::from_secs(1)).await.unwrap();
    assert_eq!(report.meta.unwrap().request_id, id);
    assert!(Repository::open_existing(dir.path()).is_ok());
}

#[tokio::test]
async fn refuses_invalid_requests_before_opening_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new());
    let handle = state.register(dir.path().to_path_buf());
    let mut call = request(&handle);
    call.get_mut().meta = None;
    assert_eq!(
        compact(&state, call, Duration::ZERO)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    for header in ["authorization", "x-atomic-sandbox-token-bin"] {
        let mut call = request(&handle);
        if header.ends_with("-bin") {
            call.metadata_mut()
                .insert_bin(header, tonic::metadata::MetadataValue::from_bytes(b"token"));
        } else {
            call.metadata_mut()
                .insert(header, "Bearer token".parse().unwrap());
        }
        assert_eq!(
            compact(&state, call, Duration::ZERO)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
    }
    assert!(compact(&state, request(&handle), Duration::ZERO)
        .await
        .is_err());
    assert!(!dir.path().join(".atomic").exists());
    let mut unknown = request(&handle);
    unknown.get_mut().repository.as_mut().unwrap().repository_id = vec![7; 32];
    assert_eq!(
        compact(&state, unknown, Duration::ZERO)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_keeps_gate_until_blocking_compaction_finishes() {
    use std::future::Future;
    use std::task::Poll;
    let dir = tempfile::tempdir().unwrap();
    let held = Repository::init(dir.path()).unwrap();
    let state = Arc::new(DaemonState::new());
    let handle = state.register(dir.path().to_path_buf());
    let held_gate = handle.exclusive().await;
    let mut call = Box::pin(compact(&state, request(&handle), Duration::from_secs(3)));
    // Poll to queue behind the held gate, without depending on scheduling sleeps.
    assert!(std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx).is_pending())).await);
    drop(held_gate);
    // This poll obtains the gate and starts blocking work; the independent
    // repository handle ensures compaction cannot have finished yet.
    assert!(std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx).is_pending())).await);
    drop(call); // RPC cancellation/disconnection drops its future.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), handle.exclusive())
            .await
            .is_err()
    );
    drop(held);
    let _gate = tokio::time::timeout(Duration::from_secs(2), handle.exclusive())
        .await
        .unwrap();
    assert!(Repository::open_existing(dir.path()).is_ok());
}

#[tokio::test]
async fn concurrent_compactions_finish_and_release_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    let state = Arc::new(DaemonState::new());
    let handle = state.register(dir.path().to_path_buf());
    let (first, second) = tokio::join!(
        compact(&state, request(&handle), Duration::from_secs(2)),
        compact(&state, request(&handle), Duration::from_secs(2)),
    );
    assert_eq!(first.unwrap().database, second.unwrap().database);
    let _gate = tokio::time::timeout(Duration::from_secs(1), handle.exclusive())
        .await
        .unwrap();
    assert!(Repository::open_existing(dir.path()).is_ok());
}

#[tokio::test]
async fn sandbox_uses_canonical_database_and_gate() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().join("repo");
    let sandbox = dir.path().join("sandbox");
    let repo = Repository::init(&canonical).unwrap();
    repo.provision_sandbox(
        repo.require_working_copy_id().unwrap(),
        &sandbox,
        repo.current_view(),
    )
    .unwrap();
    drop(repo);
    let state = Arc::new(DaemonState::new());
    let main_handle = state.register(canonical.clone());
    let sandbox_handle = state.register(sandbox.clone());
    let gate = main_handle.exclusive().await;
    assert_eq!(
        compact(&state, request(&sandbox_handle), Duration::from_millis(20))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    drop(gate);
    let report = compact(&state, request(&sandbox_handle), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        Path::new(&report.database),
        canonical
            .join(".atomic/atomic.redb")
            .canonicalize()
            .unwrap()
    );
    assert!(!sandbox.join(".atomic/atomic.redb").exists());
}

#[tokio::test]
async fn capability_advertises_local_administrative_write() {
    use crate::atomic::daemon_service_server::DaemonService;
    use crate::atomic::{CallerClass, ExecutionScope, GetCapabilitiesRequest, RpcEffect};
    let service = crate::daemon::services::DaemonImpl {
        state: Arc::new(DaemonState::new()),
    };
    let capabilities = service
        .get_capabilities(Request::new(GetCapabilitiesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let method = capabilities
        .methods
        .iter()
        .find(|m| m.method == "CompactDatabase")
        .unwrap();
    assert_eq!(method.scope, ExecutionScope::Local as i32);
    assert_eq!(method.required_capabilities, ["maintenance.admin"]);
    assert_eq!(method.allowed_callers, [CallerClass::Local as i32]);
    assert_eq!(method.effects, [RpcEffect::RepositoryWrite as i32]);
}

#[test]
fn compaction_reclaims_space_and_preserves_surviving_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    let db = Database::create(&path).unwrap();
    let value = vec![0x5a; 4096];
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(DATA).unwrap();
        for key in 0..1024 {
            table.insert(key, value.as_slice()).unwrap();
        }
    }
    write.commit().unwrap();
    // Keep the old pages pinned through deletion so the fixture has
    // reclaimable space without depending on the allocator's layout.
    let reader = db.begin_read().unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(DATA).unwrap();
        for key in 1..1024 {
            table.remove(key).unwrap();
        }
    }
    write.commit().unwrap();
    drop(reader);
    drop(db);

    let before = std::fs::metadata(&path).unwrap().len();
    let report = compact_file(&path).unwrap();
    assert_eq!(report.before_bytes, before);
    assert!(report.after_bytes < report.before_bytes, "{report:?}");
    assert_eq!(report.after_bytes, std::fs::metadata(&path).unwrap().len());
    assert_eq!(report.reclaimed_bytes, before - report.after_bytes);
    let db = redb::Builder::new().open_read_only(&path).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.list_tables().unwrap().count(), 1);
    let table = read.open_table(DATA).unwrap();
    assert_eq!(table.len().unwrap(), 1);
    assert_eq!(table.get(0).unwrap().unwrap().value(), value);
    drop(table);
    drop(read);
    drop(db);
    // A repeated maintenance request remains valid.
    compact_file(&path).unwrap();
}

#[test]
fn compaction_does_not_create_a_missing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    assert!(compact_file(&path).is_err());
    assert!(!path.exists());
}

#[test]
fn compaction_respects_existing_writer_and_reader_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    let db = Database::create(&path).unwrap();
    assert!(matches!(
        compact_file(&path)
            .unwrap_err()
            .downcast_ref::<redb::DatabaseError>(),
        Some(redb::DatabaseError::DatabaseAlreadyOpen)
    ));
    drop(db);
    let db = redb::Builder::new().open_read_only(&path).unwrap();
    assert!(matches!(
        compact_file(&path)
            .unwrap_err()
            .downcast_ref::<redb::DatabaseError>(),
        Some(redb::DatabaseError::DatabaseAlreadyOpen)
    ));
    drop(db);
    compact_file(&path).unwrap();
}

#[test]
fn compaction_rejects_a_future_repository_schema() {
    use atomic_core::pristine::{schema::SCHEMA_VERSION, tables::ATOMIC_META};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    let db = Database::create(&path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut meta = write.open_table(ATOMIC_META).unwrap();
        meta.insert(
            atomic_core::pristine::schema::SCHEMA_VERSION_KEY,
            (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
        )
        .unwrap();
    }
    write.commit().unwrap();
    drop(db);
    let error = compact_file(&path).unwrap_err().to_string();
    assert!(error.contains("schema"), "{error}");
}

#[test]
fn compaction_preserves_persistent_savepoints_and_reports_the_blocker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    let db = Database::create(&path).unwrap();
    let write = db.begin_write().unwrap();
    let savepoint = write.persistent_savepoint().unwrap();
    write.commit().unwrap();
    drop(db);

    assert!(matches!(
        compact_file(&path)
            .unwrap_err()
            .downcast::<redb::CompactionError>()
            .unwrap(),
        redb::CompactionError::PersistentSavepointExists
    ));
    let db = redb::Builder::new().open(&path).unwrap();
    let write = db.begin_write().unwrap();
    assert_eq!(
        write
            .list_persistent_savepoints()
            .unwrap()
            .collect::<Vec<_>>(),
        vec![savepoint]
    );
}
