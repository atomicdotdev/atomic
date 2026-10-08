//! Explicit database maintenance through the shared service layer.

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::service::Service;
use atomic_client::proto::{CompactDatabaseRequest, RequestMeta};
use clap::Args;
use std::path::PathBuf;

/// Reclaim unused database space without deleting repository data.
#[derive(Debug, Args)]
pub struct Compact {
    /// Path inside the repository or one of its sandboxes.
    #[arg(long, default_value = ".")]
    repository: PathBuf,
    /// Emit the database path and before/after byte counts as JSON.
    #[arg(long)]
    json: bool,
}

impl Command for Compact {
    fn run(&self) -> CliResult<()> {
        let service =
            Service::open_root(&self.repository)?.ok_or_else(|| CliError::RepositoryNotFound {
                searched_path: self.repository.clone(),
            })?;
        let report = service.compact_database(CompactDatabaseRequest {
            repository: Some(service.reference.clone()),
            meta: Some(RequestMeta {
                request_id: uuid::Uuid::new_v4().to_string(),
                observed_at: None,
            }),
        })?;
        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "database": report.database,
                    "before_bytes": report.before_bytes,
                    "after_bytes": report.after_bytes,
                    "reclaimed_bytes": report.reclaimed_bytes,
                })
            );
        } else {
            println!(
                "Compacted {}: {} -> {} bytes ({} bytes reclaimed)",
                report.database, report.before_bytes, report.after_bytes, report.reclaimed_bytes
            );
        }
        Ok(())
    }
}
