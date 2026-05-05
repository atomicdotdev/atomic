//! Repository diagnostic and repair commands.

use clap::{Args, Subcommand};

use crate::commands::{require_repository, Command};
use crate::error::CliResult;
use crate::output::{print_hint, print_info, print_success, print_warning};

/// Diagnose and repair repository indexes.
#[derive(Debug, Args)]
pub struct Doctor {
    #[command(subcommand)]
    pub command: DoctorCommands,
}

/// Doctor subcommands.
#[derive(Debug, Subcommand)]
pub enum DoctorCommands {
    /// Rebuild the redb dependency index used by fast view filters.
    ///
    /// This scans stored `.change` files once and backfills `CHANGE_DEPS` so
    /// interactive commands such as `status` can build dependency closures
    /// without repeatedly loading change files.
    #[command(name = "repair-dependency-index")]
    RepairDependencyIndex(RepairDependencyIndex),
}

/// Rebuild the normal change dependency index from stored changes.
#[derive(Debug, Args, Default)]
pub struct RepairDependencyIndex {
    /// Re-index changes even if they already have dependency metadata.
    #[arg(long)]
    pub force: bool,
}

impl Command for Doctor {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            DoctorCommands::RepairDependencyIndex(cmd) => cmd.run(),
        }
    }
}

impl Command for RepairDependencyIndex {
    fn run(&self) -> CliResult<()> {
        let repo = require_repository(None)?;

        print_info("Repairing change dependency index...");
        if self.force {
            print_warning("--force enabled: existing dependency index rows will be replaced");
        }

        let (indexed, skipped, failed) = repo.repair_change_dependency_index(self.force)?;

        print_success(&format!(
            "Dependency index repair complete: {} indexed, {} skipped, {} failed",
            indexed, skipped, failed
        ));

        if failed > 0 {
            print_hint(
                "Some changes could not be loaded. Run with verbose logging to identify corrupted or missing change files.",
            );
        } else if indexed > 0 {
            print_hint("View filter setup for status/diff/content paths can now use pristine indexes instead of scanning .change files.");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_dependency_index_defaults_to_non_force() {
        let cmd = RepairDependencyIndex::default();
        assert!(!cmd.force);
    }
}
