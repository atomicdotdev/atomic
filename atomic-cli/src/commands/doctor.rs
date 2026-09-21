//! Repository diagnostic and repair commands.

use atomic_repository::CrdtMaterializeOptions;
use atomic_core::WorkingCopyId;
use clap::{Args, Subcommand};

use crate::commands::{
    require_repository, require_repository_for_native_check, require_repository_for_native_repair,
    require_repository_readonly, Command,
};
use crate::error::{CliError, CliResult};
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

    /// Atomically rebuild native tree, inode, directory, claim, and conflict caches.
    #[command(name = "repair-native-indexes")]
    RepairNativeIndexes(RepairNativeIndexes),

    /// Repair missing PATH_CLAIMS rows from graph authority (insert-only).
    ///
    /// Re-derives the complete graph-authoritative path-claim set (closure
    /// union across every view, dependency-ordered change replay) and writes
    /// ONLY the rows the live index is missing. Existing rows must match the
    /// derivation byte-for-byte or the repair refuses; stale rows the
    /// derivation does not produce also refuse (fail closed). Structural
    /// claims for files added by changes whose apply predates the path-claim
    /// index — leaving tracked paths invisible to the operation-aware
    /// projection — are restored here (review E4). No graph rows, change
    /// bytes, or views are touched.
    #[command(name = "repair-path-claims")]
    RepairPathClaims(RepairPathClaims),

    /// Rebuild REV_TREE as the exact inverse of TREE (journaled repair).
    ///
    /// 0.17.x tree writes could re-bind a path to a new inode without
    /// removing the previous inode's reverse row, leaving stale REV_TREE rows
    /// that fail the TREE/REV_TREE bijection validation every later tree
    /// write performs. This repair removes those stale reverse rows and
    /// inserts missing inverses; TREE is authoritative for the binding and
    /// forward rows are never touched. The repair runs in one immediately
    /// durable transaction and journals a Repair operation with a verified
    /// receipt; an interrupted process leaves either nothing or a complete
    /// journal entry.
    #[command(name = "repair-tree-bijection")]
    RepairTreeBijection(RepairTreeBijection),

    /// Undo the latest journaled path-claims repair (CB-13A follow-up R3).
    ///
    /// Restores the exact before-state captured in the repair's stored
    /// inverse under fresh leases (the live table must equal the repair's
    /// post-state; any third value refuses without overwriting). Journals
    /// an immutable Undo operation with a verified receipt; a repeated undo
    /// refuses as a third value (nothing left to undo).
    #[command(name = "undo-path-claims-repair")]
    UndoPathClaimsRepair(UndoPathClaimsRepair),

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

    /// Reconcile a working copy's persistent registration with an existing view.
    ///
    /// This is the supported recovery for the "working-copy expects view X at
    /// state S, but the view is at T" refusal: a journaled metadata-only
    /// operation rebinds the record's desired view/state and clears a stale
    /// materialized claim. It never materializes, deletes, or rewrites
    /// working-tree bytes, Git refs/indexes, shelves, or view membership.
    /// Use `--dry-run` to diagnose without writing anything.
    #[command(name = "reconcile-working-copy")]
    ReconcileWorkingCopy(ReconcileWorkingCopy),
}

/// Read-only working-copy consistency check.
#[derive(Debug, Args, Default)]
pub struct Check {}

/// Journaled working-copy registration reconciliation.
#[derive(Debug, Args)]
pub struct ReconcileWorkingCopy {
    /// Stable identity (ULID) of the physical working copy to reconcile.
    #[arg(long)]
    pub working_copy: String,

    /// Existing view the working copy should be registered against.
    #[arg(long = "target-view")]
    pub target_view: String,

    /// Diagnose only; write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// Atomically rebuild graph-derived native indexes.
#[derive(Debug, Args, Default)]
pub struct RepairNativeIndexes {}

/// Repair missing PATH_CLAIMS rows from graph authority.
///
/// Insert-only: missing rows are added atomically from the dependency-ordered
/// change replay; existing rows must match the derivation byte-exact or the
/// repair refuses, and stale rows always refuse. Structural bytes, change
/// files, graph rows, and views are untouched (review E4).
#[derive(Debug, Args, Default)]
pub struct RepairPathClaims {}

/// Rebuild REV_TREE as the exact inverse of TREE (journaled).
#[derive(Debug, Args, Default)]
pub struct RepairTreeBijection {}

/// Undo the latest journaled path-claims repair (journaled Undo).
#[derive(Debug, Args, Default)]
pub struct UndoPathClaimsRepair {}

impl Command for UndoPathClaimsRepair {
    fn run(&self) -> CliResult<()> {
        use crate::output::{print_info, print_success, print_warning};
        let repo = match require_repository_for_native_repair(None) {
            Ok(repo) => repo,
            Err(error) => {
                print_warning(&format!(
                    "The path-claims undo is unavailable because repository authority cannot be opened: {error}"
                ));
                return Err(error);
            }
        };
        print_info("Undoing the latest journaled path-claims repair under fresh leases...");
        match repo.undo_last_path_claims_repair() {
            Ok(Some(outcome)) => {
                print_success(&format!(
                    "Path-claims repair undone ({} row(s) restored); journaled as Undo operation {:?}",
                    outcome.rows_written,
                    outcome.operation
                ));
            }
            Ok(None) => {
                print_info("No journaled path-claims repair with a stored inverse exists; nothing to undo.");
            }
            Err(error) => {
                print_warning(&format!(
                    "The path-claims undo refused (third-value rejection or failure): {error}"
                ));
                print_hint("The live table was not overwritten. Inspect with 'atomic doctor check' first.");
                return Err(crate::error::CliError::Repository(error));
            }
        }
        Ok(())
    }
}

impl Command for RepairTreeBijection {
    fn run(&self) -> CliResult<()> {
        use crate::output::{print_hint, print_info, print_success, print_warning};
        let repo = match require_repository_for_native_repair(None) {
            Ok(repo) => repo,
            Err(error) => {
                print_warning(&format!(
                    "TREE/REV_TREE bijection is unrepairable because repository authority cannot be opened: {error}"
                ));
                return Err(error);
            }
        };
        print_info("Rebuilding REV_TREE as the exact inverse of TREE...");
        let outcome = match repo.repair_tree_bijection() {
            Ok(outcome) => outcome,
            Err(error) => {
                print_warning(&format!(
                    "TREE/REV_TREE bijection is unrepairable from available authority: {error}"
                ));
                print_hint("No repair transaction was committed. Nothing was modified.");
                return Err(crate::error::CliError::Repository(error));
            }
        };
        if outcome.already_healthy {
            print_success("TREE/REV_TREE bijection is already exact (0 rows written).");
        } else {
            print_success(&format!(
                "TREE/REV_TREE bijection repaired ({} rows written); journaled as operation {:?}",
                outcome.rows_written,
                outcome.operation
            ));
        }
        Ok(())
    }
}

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
            DoctorCommands::RepairNativeIndexes(cmd) => cmd.run(),
            DoctorCommands::RepairPathClaims(cmd) => cmd.run(),
            DoctorCommands::RepairTreeBijection(cmd) => cmd.run(),
            DoctorCommands::UndoPathClaimsRepair(cmd) => cmd.run(),
            DoctorCommands::MaterializeCrdt(cmd) => cmd.run(),
            DoctorCommands::Check(cmd) => cmd.run(),
            DoctorCommands::ReconcileWorkingCopy(cmd) => cmd.run(),
        }
    }
}

impl Command for Check {
    fn run(&self) -> CliResult<()> {
        let repo = match require_repository_for_native_check(None) {
            Ok(repo) => repo,
            Err(error) => {
                print_warning(&format!(
                    "Native derived indexes are unrepairable because repository authority cannot be opened: {}",
                    error
                ));
                return Err(error);
            }
        };

        print_info("Verifying native derived indexes against graph authority...");
        let native = repo
            .verify_native_derived_indexes()
            .map_err(|e| crate::error::CliError::Internal(e.into()))?;
        if !native.is_healthy() {
            print_warning(&format!(
                "{} native derived-index problem(s) found:",
                native.problems.len()
            ));
            for problem in &native.problems {
                println!("  ✗ {}", problem);
            }
            print_hint("Run `atomic doctor repair-native-indexes` to rebuild derived caches atomically from graph and change facts.");
            return Err(crate::error::CliError::Internal(anyhow::anyhow!(
                "native derived-index verification found {} problem(s)",
                native.problems.len()
            )));
        }
        print_success(&format!(
            "Native derived indexes are consistent ({} rows).",
            native.expected_rows
        ));

        print_info("Verifying working-copy consistency against the graph...");
        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| crate::error::CliError::Internal(e.into()))?;
        let report = repo
            .verify_working_copy(working_copy)
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

impl Command for ReconcileWorkingCopy {
    fn run(&self) -> CliResult<()> {
        let working_copy: WorkingCopyId =
            self.working_copy.trim().parse().map_err(|error| {
                CliError::Internal(anyhow::anyhow!(
                    "invalid --working-copy '{}': {error}",
                    self.working_copy
                ))
            })?;

        if self.dry_run {
            let repo = require_repository_readonly(None)?;
            let diagnosis = repo
                .inspect_working_copy_registration(working_copy, &self.target_view)
                .map_err(CliError::Repository)?;
            print_diagnosis(&diagnosis);
            if !diagnosis.location_matches {
                return Err(CliError::Repository(
                    atomic_repository::RepositoryError::WorkingCopyLocationMismatch {
                        id: working_copy,
                    },
                ));
            }
            if diagnosis.already_reconciled {
                print_success("Registration already matches the target view; nothing to do.");
            } else {
                print_info("Dry run: no operation was prepared and nothing was written.");
            }
            return Ok(());
        }

        let mut repo = require_repository(None)?;
        let diagnosis = repo
            .inspect_working_copy_registration(working_copy, &self.target_view)
            .map_err(CliError::Repository)?;
        print_diagnosis(&diagnosis);
        if !diagnosis.location_matches {
            return Err(CliError::Repository(
                atomic_repository::RepositoryError::WorkingCopyLocationMismatch {
                    id: working_copy,
                },
            ));
        }

        match repo
            .reconcile_working_copy_registration(working_copy, &self.target_view)
            .map_err(CliError::Repository)?
        {
            atomic_repository::WorkingCopyReconcileOutcome::AlreadyReconciled { .. } => {
                print_success(
                    "Registration already matches the target view; nothing was written.",
                );
            }
            atomic_repository::WorkingCopyReconcileOutcome::Reconciled {
                operation,
                previous_view,
                target_view,
            } => {
                print_success(&format!(
                    "Reconciled working-copy {working_copy} from '{previous_view}' to \
                     '{target_view}' as verified operation {operation}."
                ));
                print_hint(
                    "The record now desires the target view with no verified materialized claim; \
                     working-tree bytes, refs, shelves, and view membership were not touched.",
                );
            }
        }
        Ok(())
    }
}

fn print_diagnosis(diagnosis: &atomic_repository::WorkingCopyRegistrationDiagnosis) {
    print_info(&format!(
        "Working copy {}: location {}; desired view {} ({}) at {}; observed {}; \
         materialized {}; target '{}' at {}",
        diagnosis.working_copy,
        if diagnosis.location_matches {
            "matches"
        } else {
            "MISMATCH"
        },
        diagnosis.desired_view_id,
        diagnosis
            .desired_view_name
            .as_deref()
            .unwrap_or("<missing>"),
        diagnosis.desired_state,
        diagnosis
            .observed_desired_view_state
            .map(|state| state.to_string())
            .unwrap_or_else(|| "<missing>".to_string()),
        diagnosis
            .materialized_state
            .map(|state| state.to_string())
            .unwrap_or_else(|| "none".to_string()),
        diagnosis.target_view_name,
        diagnosis.target_view_state,
    ));
    if diagnosis.desired_view_is_stale {
        print_warning("The recorded desired state no longer matches its view (stale registration).");
    }
}

impl Command for RepairNativeIndexes {
    fn run(&self) -> CliResult<()> {
        let repo = match require_repository_for_native_repair(None) {
            Ok(repo) => repo,
            Err(error) => {
                print_warning(&format!(
                    "Native derived indexes are unrepairable because repository authority cannot be opened: {}",
                    error
                ));
                return Err(error);
            }
        };
        print_info("Rebuilding native derived indexes from graph authority...");
        let outcome = match repo.repair_native_derived_indexes() {
            Ok(outcome) => outcome,
            Err(error) => {
                print_warning(&format!(
                    "Native derived indexes are unrepairable from available authority: {}",
                    error
                ));
                print_hint("No repair transaction was committed. Restore missing/corrupt change or graph authority and retry.");
                return Err(crate::error::CliError::Repository(error));
            }
        };
        if outcome.already_healthy {
            print_success(&format!(
                "Native derived indexes are already consistent ({} rows).",
                outcome.rows_written
            ));
        } else {
            print_success(&format!(
                "Repaired {} native derived-index problem(s); wrote {} rows atomically{}.",
                outcome.problems_repaired,
                outcome.rows_written,
                outcome
                    .operation
                    .map(|operation| format!("; journaled as operation {operation}"))
                    .unwrap_or_default()
            ));
        }
        Ok(())
    }
}

impl Command for RepairPathClaims {
    fn run(&self) -> CliResult<()> {
        let repo = match require_repository_for_native_repair(None) {
            Ok(repo) => repo,
            Err(error) => {
                print_warning(&format!(
                    "Path-claim indexes are unrepairable because repository authority cannot be opened: {}",
                    error
                ));
                return Err(error);
            }
        };
        print_info("Repairing PATH_CLAIMS from graph authority (insert-only)...");
        let outcome = match repo.repair_path_claims_index() {
            Ok(outcome) => outcome,
            Err(error) => {
                print_warning(&format!(
                    "Path-claim indexes are unrepairable from available authority: {}",
                    error
                ));
                print_hint("No repair transaction was committed. Nothing was modified.");
                return Err(crate::error::CliError::Repository(error));
            }
        };
        if outcome.already_healthy {
            print_success(&format!(
                "Path-claim index is already consistent (0 rows written; derived rows verified)."
            ));
        } else {
            print_success(&format!(
                "Repaired the path-claim index; wrote {} missing rows atomically (existing rows verified byte-exact){}.",
                outcome.rows_written,
                outcome
                    .operation
                    .map(|operation| format!("; journaled as operation {operation}"))
                    .unwrap_or_default()
            ));
        }
        Ok(())
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
    fn repair_native_indexes_has_no_unsafe_override() {
        let _ = RepairNativeIndexes::default();
    }

    #[test]
    fn materialize_crdt_defaults_to_current_view_non_force() {
        let cmd = MaterializeCrdt::default();
        assert!(cmd.view.is_none());
        assert!(!cmd.force);
    }
}
