//! Explicit repository database compaction through the local owner.

use std::path::PathBuf;

use clap::Args;

use crate::commands::{agent::compact_repository, Command};
use crate::error::CliResult;

/// Reclaim unused space in the repository database.
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
        let report = compact_repository(&self.repository)?;
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&report).map_err(anyhow::Error::from)?
            );
        } else {
            println!(
                "Compacted {}: {} -> {} bytes ({} bytes reclaimed)",
                report.database.display(),
                report.before_bytes,
                report.after_bytes,
                report.reclaimed_bytes,
            );
        }
        Ok(())
    }
}
