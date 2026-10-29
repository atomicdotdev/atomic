//! Explicit database maintenance through the shared service layer.

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::service::Service;
use atomic_client::proto::{CompactDatabaseRequest, RequestMeta};
use clap::Args;
use std::path::PathBuf;

/// Reclaim database space and disposable journal recovery data.
#[derive(Debug, Args)]
pub struct Compact {
    /// Path inside the repository or one of its sandboxes.
    #[arg(long, default_value = ".")]
    repository: PathBuf,
    /// Emit the database path and before/after byte counts as JSON.
    #[arg(long)]
    json: bool,
    /// Inventory cleanup candidates without deleting files or compacting redb.
    #[arg(long, conflicts_with = "database_only")]
    dry_run: bool,
    /// Also remove journal-known deleted-view shelves (may contain ignored work).
    #[arg(long, conflicts_with = "database_only")]
    full: bool,
    /// Only compact redb pages; do not inspect or remove recovery/shelf data.
    #[arg(long)]
    database_only: bool,
}

impl Command for Compact {
    fn run(&self) -> CliResult<()> {
        // Resolve relative paths in the caller's working directory, before
        // sending them to a service process with its own working directory.
        let repository = std::path::absolute(&self.repository)?;
        let service =
            Service::open_root(&repository)?.ok_or_else(|| CliError::RepositoryNotFound {
                searched_path: self.repository.clone(),
            })?;
        let report = service.compact_database(CompactDatabaseRequest {
            repository: Some(service.reference.clone()),
            meta: Some(RequestMeta {
                request_id: uuid::Uuid::new_v4().to_string(),
                observed_at: None,
            }),
            cleanup: !self.database_only,
            dry_run: self.dry_run,
            full: self.full,
        })?;
        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "database": report.database,
                    "before_bytes": report.before_bytes,
                    "after_bytes": report.after_bytes,
                    "reclaimed_bytes": report.reclaimed_bytes,
                    "cleanup_candidate_bytes": report.cleanup_candidate_bytes,
                    "cleanup_removed_bytes": report.cleanup_removed_bytes,
                    "dry_run": report.dry_run,
                    "cleanup": report.cleanup,
                    "full": report.full,
                    "cleanup_items": report.cleanup_items.iter().map(|item| serde_json::json!({
                        "path": item.path, "kind": item.kind, "bytes": item.bytes,
                        "eligible": item.eligible, "removed": item.removed, "reason": item.reason,
                    })).collect::<Vec<_>>(),
                })
            );
        } else {
            println!(
                "{} {}: {} -> {} database bytes ({} bytes reclaimed)",
                if report.dry_run {
                    "Previewed"
                } else {
                    "Compacted"
                },
                report.database,
                report.before_bytes,
                report.after_bytes,
                report.reclaimed_bytes,
            );
            for item in &report.cleanup_items {
                println!(
                    "{} {} ({} logical bytes): {}",
                    if item.removed {
                        "removed"
                    } else if item.eligible {
                        "would remove"
                    } else {
                        "kept"
                    },
                    item.path,
                    item.bytes,
                    item.reason
                );
            }
            if report.cleanup {
                println!("Filesystem cleanup: {} candidate bytes, {} removed logical bytes; shared/reflink blocks may remain allocated",
                    report.cleanup_candidate_bytes, report.cleanup_removed_bytes);
            }
        }
        Ok(())
    }
}
