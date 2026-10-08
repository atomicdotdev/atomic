//! Physical maintenance. Only this private module opens redb for compaction.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_repository::Repository;
use prost::Message;
use redb::ReadableDatabase;
use tonic::{Request, Status};

use crate::atomic::{
    CompactDatabaseRequest, CompactDatabaseResponse, ErrorCode, ErrorInfo, ResponseMeta,
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
    let path = dot_dir
        .join(atomic_repository::DATABASE_FILE)
        .canonicalize()
        .map_err(|error| {
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
    let report = tokio::task::spawn_blocking(move || {
        // The blocking task, not the RPC future, owns this until redb closes.
        let _guard = guard;
        compact_waiting(&path, started, wait)
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
) -> Result<CompactDatabaseResponse, Status> {
    loop {
        match compact_file(path) {
            Ok(report) => return Ok(report),
            Err(error)
                if matches!(
                    error.downcast_ref::<redb::DatabaseError>(),
                    Some(redb::DatabaseError::DatabaseAlreadyOpen)
                ) =>
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

fn compact_file(path: &Path) -> anyhow::Result<CompactDatabaseResponse> {
    let mut database = redb::Builder::new().open(path)?;
    atomic_core::pristine::schema::check_schema_version(&database.begin_read()?)?;
    let before_bytes = std::fs::metadata(path)?.len();
    database.compact()?;
    // Closing persists allocator metadata; include that in the final size.
    drop(database);
    let after_bytes = std::fs::metadata(path)?.len();
    Ok(CompactDatabaseResponse {
        database: path.to_string_lossy().into_owned(),
        before_bytes,
        after_bytes,
        reclaimed_bytes: before_bytes.saturating_sub(after_bytes),
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
