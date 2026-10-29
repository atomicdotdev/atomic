//! Physical maintenance. Only this private module opens redb for compaction.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_repository::repository::maintenance::CleanupPlan;
use atomic_repository::Repository;
use prost::Message;
use redb::ReadableDatabase;
use tonic::{Request, Status};

use crate::atomic::{
    CompactCleanupItem, CompactDatabaseRequest, CompactDatabaseResponse, ErrorCode, ErrorInfo,
    ResponseMeta,
};
use crate::daemon::state::DaemonState;

pub(super) async fn compact(
    state: &Arc<DaemonState>,
    request: Request<CompactDatabaseRequest>,
    wait: Duration,
) -> Result<CompactDatabaseResponse, Status> {
    // Serving transports must enforce LOCAL + maintenance.admin before calling
    // this handler. Restricted credentials must never fall through to the local
    // administrator path; invoking from a sandbox directory is a separate case.
    if request
        .metadata()
        .contains_key("x-atomic-sandbox-token-bin")
        || request.metadata().contains_key("authorization")
    {
        return Err(Status::permission_denied(
            "database compaction requires a local administrator",
        ));
    }
    let request = request.into_inner();
    let request_id = request
        .meta
        .as_ref()
        .map(|meta| meta.request_id.as_str())
        .unwrap_or("");
    if uuid::Uuid::parse_str(request_id).is_err() {
        return Err(Status::invalid_argument(
            "compaction requires a UUID request_id",
        ));
    }
    if (request.full || request.dry_run) && !request.cleanup {
        return Err(with_request_id(
            Status::invalid_argument("full/dry-run maintenance requires cleanup mode"),
            request_id,
        ));
    }
    compact_inner(state, &request, wait)
        .await
        .map_err(|status| with_request_id(status, request_id))
}

async fn compact_inner(
    state: &Arc<DaemonState>,
    request: &CompactDatabaseRequest,
    wait: Duration,
) -> Result<CompactDatabaseResponse, Status> {
    let started = Instant::now();
    let reference = request
        .repository
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("repository is required"))?;
    let resolved = state.resolve(reference)?;
    // Resolve sandbox pointers without opening, migrating or recovering a repo.
    let dot_dir = Repository::canonical_dot_dir(&resolved.root)
        .map_err(|error| Status::failed_precondition(error.to_string()))?
        .canonicalize()
        .map_err(|error| Status::failed_precondition(error.to_string()))?;
    let database_path = dot_dir.join(atomic_repository::DATABASE_FILE);
    let metadata = std::fs::symlink_metadata(&database_path).map_err(|error| {
        Status::failed_precondition(format!("existing atomic.redb required: {error}"))
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Status::failed_precondition(
            "atomic.redb must be a physical regular file",
        ));
    }
    let path = database_path.canonicalize().map_err(|error| {
        Status::failed_precondition(format!("existing atomic.redb required: {error}"))
    })?;
    // The sandbox and canonical repository must share maintenance exclusion.
    let handle = state.register(
        dot_dir
            .parent()
            .ok_or_else(|| Status::failed_precondition("canonical .atomic has no parent"))?
            .to_path_buf(),
    );
    let guard = tokio::time::timeout(
        wait.saturating_sub(started.elapsed()),
        handle.exclusive_owned(),
    )
    .await
    .map_err(|_| {
        Status::unavailable("database compaction timed out waiting for active requests")
    })?;
    state.log_rpc("CompactDatabase", Some(&handle));
    let options = CompactOptions {
        cleanup: request.cleanup,
        dry_run: request.dry_run,
        full: request.full,
        local_dot_dir: resolved.root.join(".atomic"),
    };
    let report = tokio::task::spawn_blocking(move || {
        // The blocking task, not the RPC future, owns this until redb closes.
        let _guard = guard;
        compact_waiting(&path, started, wait, &options)
    })
    .await
    .map_err(|error| Status::internal(format!("compaction task failed: {error}")))??;
    Ok(CompactDatabaseResponse {
        meta: Some(ResponseMeta {
            request_id: request
                .meta
                .as_ref()
                .expect("validated metadata")
                .request_id
                .clone(),
            replayed: false,
            snapshot: None,
        }),
        ..report
    })
}

fn compact_waiting(
    path: &Path,
    started: Instant,
    wait: Duration,
    options: &CompactOptions,
) -> Result<CompactDatabaseResponse, Status> {
    loop {
        match compact_file_with_options(path, options) {
            Ok(report) => return Ok(report),
            Err(error)
                if matches!(
                    error.downcast_ref::<redb::DatabaseError>(),
                    Some(redb::DatabaseError::DatabaseAlreadyOpen)
                ) || error
                    .downcast_ref::<atomic_repository::RepositoryError>()
                    .is_some_and(|error| error.is_lock_contended()) =>
            {
                let remaining = wait.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(Status::unavailable(format!(
                        "database compaction timed out waiting for {}; close other database readers/writers and retry",
                        path.display()
                    )));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(25)));
            }
            Err(error) => {
                return Err(Status::failed_precondition(format!(
                    "cannot compact {}: {error:#}",
                    path.display()
                )))
            }
        }
    }
}

#[derive(Default)]
struct CompactOptions {
    cleanup: bool,
    dry_run: bool,
    full: bool,
    local_dot_dir: std::path::PathBuf,
}

#[cfg(test)]
fn compact_file(path: &Path) -> anyhow::Result<CompactDatabaseResponse> {
    compact_file_with_options(path, &CompactOptions::default())
}

fn compact_file_with_options(
    path: &Path,
    options: &CompactOptions,
) -> anyhow::Result<CompactDatabaseResponse> {
    let dot_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("database has no parent"))?;
    let common = Repository::try_lock_maintenance(dot_dir)?;
    // Even opening a writable redb handle can persist allocator/header data on
    // close. A real preview MUST use the read-only file owner, not merely skip
    // Database::compact().
    let mut database = if options.dry_run {
        None
    } else {
        Some(redb::Builder::new().open(path)?)
    };
    let reader = if options.dry_run {
        Some(redb::Builder::new().open_read_only(path)?)
    } else {
        None
    };
    let read = if let Some(database) = &database {
        database.begin_read()?
    } else {
        reader.as_ref().expect("preview reader").begin_read()?
    };
    atomic_core::pristine::schema::check_schema_version(&read)?;
    let before_bytes = std::fs::metadata(path)?.len();
    let plan = if options.cleanup {
        let txn = atomic_core::pristine::ReadTxn::from_redb(read);
        Some(CleanupPlan::build(
            &txn,
            &common,
            &options.local_dot_dir,
            options.full,
        )?)
    } else {
        drop(read);
        None
    };
    // Discover/validate every candidate before touching the allocator. A failed
    // compact (e.g. a persistent savepoint) must not delete recovery data.
    if let Some(database) = &mut database {
        database.compact()?;
    }
    let items = match plan {
        Some(plan) if !options.dry_run => plan.execute(&common)?,
        Some(plan) => plan.into_items(),
        None => Vec::new(),
    };
    let mut candidate_bytes = 0u64;
    let mut removed_bytes = 0u64;
    let mut cleanup_items = Vec::new();
    for item in items {
        if item.eligible {
            candidate_bytes = candidate_bytes.saturating_add(item.bytes);
        }
        if item.removed {
            removed_bytes = removed_bytes.saturating_add(item.bytes);
        }
        cleanup_items.push(CompactCleanupItem {
            path: item.path.to_string_lossy().into_owned(),
            kind: item.kind.to_string(),
            bytes: item.bytes,
            eligible: item.eligible,
            removed: item.removed,
            reason: item.reason,
        });
    }
    // Closing persists allocator metadata; include that in the final size.
    drop(database);
    drop(reader);
    let after_bytes = std::fs::metadata(path)?.len();
    Ok(CompactDatabaseResponse {
        database: path.to_string_lossy().into_owned(),
        before_bytes,
        after_bytes,
        reclaimed_bytes: before_bytes.saturating_sub(after_bytes),
        cleanup_items,
        cleanup_candidate_bytes: candidate_bytes,
        cleanup_removed_bytes: removed_bytes,
        dry_run: options.dry_run,
        cleanup: options.cleanup,
        full: options.full,
        meta: None,
    })
}

fn with_request_id(status: Status, request_id: &str) -> Status {
    let code = match status.code() {
        tonic::Code::Unavailable => ErrorCode::ResourceExhausted,
        tonic::Code::InvalidArgument => ErrorCode::InvalidArgument,
        tonic::Code::NotFound => ErrorCode::RepositoryNotFound,
        tonic::Code::FailedPrecondition => ErrorCode::PreconditionFailed,
        _ => ErrorCode::Internal,
    };
    let info = ErrorInfo {
        code: code as i32,
        message: status.message().to_string(),
        request_id: Some(request_id.to_string()),
        ..Default::default()
    };
    Status::with_details(status.code(), status.message(), info.encode_to_vec().into())
}

#[cfg(test)]
mod tests;
