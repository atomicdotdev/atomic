//! Repository diagnostic and repair commands.

use atomic_repository::CrdtMaterializeOptions;
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

    /// Materialize stored FileOps into CRDT semantic tables.
    ///
    /// This is the second phase of graph-first Git import: the graph is already
    /// written, and this command builds CRDT tables from graph-linked FileOps
    /// stored in the imported changes.
    #[command(name = "materialize-crdt")]
    MaterializeCrdt(MaterializeCrdt),

    /// Verify working-copy consistency against the graph (read-only).
    ///
    /// Recomputes each file's content from the graph and reports two classes
    /// of problem:
    ///   * materialization drift — a clean file whose on-disk bytes differ
    ///     from what the graph would materialize (silent corruption);
    ///   * conflict-state disagreement — on-disk markers, `atomic status`,
    ///     and `atomic conflicts` must all agree.
    ///
    /// Mutates nothing. Exits non-zero when problems are found.
    Check(Check),
}

/// Read-only working-copy consistency check.
#[derive(Debug, Args, Default)]
pub struct Check {}

/// Rebuild the normal change dependency index from stored changes.
#[derive(Debug, Args, Default)]
pub struct RepairDependencyIndex {
    /// Re-index changes even if they already have dependency metadata.
    #[arg(long)]
    pub force: bool,
}

/// Build CRDT tables from stored change FileOps.
#[derive(Debug, Args, Default)]
pub struct MaterializeCrdt {
    /// View to materialize. Defaults to the current view.
    #[arg(long)]
    pub view: Option<String>,

    /// Re-apply even when a trunk row already exists.
    #[arg(long)]
    pub force: bool,
}

impl Command for Doctor {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            DoctorCommands::RepairDependencyIndex(cmd) => cmd.run(),
            DoctorCommands::MaterializeCrdt(cmd) => cmd.run(),
            DoctorCommands::Check(cmd) => cmd.run(),
        }
    }
}

impl Command for Check {
    fn run(&self) -> CliResult<()> {
        let repo = require_repository(None)?;

        print_info("Verifying working-copy consistency against the graph...");
        let report = repo
            .verify_working_copy()
            .map_err(|e| crate::error::CliError::Internal(e.into()))?;

        print_info(&format!(
            "Checked {} clean file(s); {} with uncommitted edits skipped; {} conflicted.",
            report.clean_files_checked, report.uncommitted_skipped, report.conflicted_files
        ));

        if report.is_healthy() {
            print_success("Working copy is consistent with the graph.");
            return Ok(());
        }

        print_warning(&format!("{} problem(s) found:", report.problems.len()));
        for p in &report.problems {
            println!("  ✗ {}", p);
        }
        print_hint(
            "Materialization drift can often be repaired by re-materializing \
             (e.g. `atomic view switch <current-view>`); conflict-state \
             disagreements indicate a bug worth reporting.",
        );
        Err(crate::error::CliError::Internal(anyhow::anyhow!(
            "working-copy verification found {} problem(s)",
            report.problems.len()
        )))
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

impl Command for MaterializeCrdt {
    fn run(&self) -> CliResult<()> {
        let repo = require_repository(None)?;
        let view = self
            .view
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string());

        print_info(&format!("Materializing CRDT tables for view '{}'...", view));
        if self.force {
            print_warning("--force enabled: existing CRDT trunk rows may be overwritten");
        }

        let outcome = repo.materialize_crdt_from_changes(CrdtMaterializeOptions {
            view: Some(view),
            force: self.force,
        })?;

        print_success(&format!(
            "CRDT materialization complete in {:.1}s: {} changes scanned, {} changes applied, {} FileOps applied, {} already materialized, {} skipped",
            outcome.elapsed_ms as f64 / 1000.0,
            outcome.changes_scanned,
            outcome.changes_applied,
            outcome.file_ops_applied,
            outcome.file_ops_already_materialized,
            outcome.file_ops_skipped
        ));
        print_hint(&format!(
            "CRDT rows: trunks +{}, branches +{}, leaves +{}",
            outcome.stats.trunks_created,
            outcome.stats.branches_created,
            outcome.stats.leaves_created
        ));
        if outcome.file_ops_skipped > 0 {
            print_hint(&format!(
                "Skipped FileOps: non_create={}, unresolved_path={}, unresolved_line={}, missing_range={}, non_insert_branch={}, non_insert_leaf={}",
                outcome.skip_stats.non_create_trunk,
                outcome.skip_stats.unresolved_path,
                outcome.skip_stats.unresolved_line,
                outcome.skip_stats.missing_content_range,
                outcome.skip_stats.non_insert_branch,
                outcome.skip_stats.non_insert_leaf
            ));
            if !outcome.skip_samples.is_empty() {
                print_hint(&format!(
                    "Skip samples: {}",
                    outcome.skip_samples.join(", ")
                ));
            }
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

    #[test]
    fn materialize_crdt_defaults_to_current_view_non_force() {
        let cmd = MaterializeCrdt::default();
        assert!(cmd.view.is_none());
        assert!(!cmd.force);
    }
}
