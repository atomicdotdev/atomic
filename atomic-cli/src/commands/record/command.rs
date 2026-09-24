use super::*;

use atomic_repository::WorkspaceTxnMode;

use crate::commands::workspace_txn::enter_workspace;

impl Command for Record {
    /// Execute the record command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Get the commit message (from argument, editor, or prompt)
    /// 3. Detect changes in the working copy
    /// 4. If --dry-run, display preview and exit
    /// 5. Create the change from modifications (including untracked files for --all)
    /// 6. Save the change to the store
    /// 7. Apply the change to the current view
    /// 8. Display the result
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;

        let mode = if self.dry_run {
            WorkspaceTxnMode::Observe
        } else {
            WorkspaceTxnMode::Reconcile
        };
        let mut repo = if self.dry_run {
            Repository::open_readonly(&repo_root)
        } else {
            Repository::open_for_workspace_transaction(&repo_root)
        }
        .map_err(CliError::Repository)?;

        // Narrow metadata-only route: an explicit, scoped
        // `record --allow-conflict-markers <paths>` whose preflight proves every
        // named path is already recorded (working bytes equal the canonical
        // render, no graph conflict) and only stale CONFLICTS metadata needs to
        // change. That administrative cleanup needs no anchored Git baseline and
        // must not execute the workspace boundary's recovery/import effects, so
        // it runs through the leased reconcile API directly. Anything not proven
        // metadata-only falls through to the ordinary boundary and record path,
        // which clears nothing when it refuses.
        if !self.dry_run && self.allow_conflict_markers && !self.all && !self.files.is_empty() {
            let working_copy = repo
                .require_working_copy_id()
                .map_err(CliError::Repository)?;
            if let Some(cleanup) = repo
                .record_metadata_only_conflict_cleanup(working_copy, &self.files)
                .map_err(CliError::Repository)?
            {
                print!(
                    "{}",
                    super::format::format_conflict_cleanup(
                        &cleanup.view,
                        &cleanup.cleared_paths,
                        cleanup.cleared_rows,
                        cleanup.operation,
                    )
                );
                return Ok(());
            }
        }

        let workspace = enter_workspace(&mut repo, mode)?;
        let working_copy = workspace.working_copy();
        let view_name = workspace.view().name.clone();

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&repo, working_copy);
        }

        // Get commit message
        let message = self.get_message()?;

        // Resolve author from identity or command-line
        let author = self.resolve_author()?;

        // Build change header
        let mut header_builder = ChangeHeader::builder().message(&message);

        if let Some(author) = author {
            header_builder = header_builder.author(author);
        }

        let header = header_builder.build();

        // Build record options
        let options = self.build_options()?;

        // Repository::record owns --all inclusion so rename classification runs
        // before any untracked destination could be staged as a fresh inode.

        // Record the changes.
        //
        // This match is deliberately exhaustive: no `_ =>` arm. The catch-all
        // it replaces routed everything unlisted into `CliError::Internal`,
        // which tells the user "this appears to be a bug, please report it"
        // and exits 128 — so every new `RecordError` variant silently
        // defaulted to accusing Atomic of a bug. `FileTooLarge` reached users
        // that way. Keeping the match exhaustive makes the compiler demand a
        // classification decision for each variant added from here on.
        let outcome = repo.record(working_copy, header, options).map_err(|e| {
            use atomic_repository::record::RecordError as RE;
            match e {
                RE::NothingToRecord | RE::NoFilesMatched => CliError::NothingToRecord,
                RE::FileNotFound { path } => CliError::FileNotFound {
                    path: PathBuf::from(path),
                },
                RE::FileNotTracked { path } => CliError::FileNotTracked {
                    path: PathBuf::from(path),
                },
                // Refusing to bake an unresolved merge into history is the
                // documented behavior, not a bug — keep it out of the
                // Internal bucket so it neither tells the user to file an
                // issue nor exits 128.
                RE::ConflictMarkersPresent { path, line } => {
                    CliError::ConflictMarkers { path, line }
                }
                RE::UnresolvedConflicts => CliError::Conflict {
                    description: "the working copy has unresolved conflicts".to_string(),
                },
                // Almost always a build artifact or dependency cache that
                // should have been ignored. The user can fix it three ways,
                // all named in the error's suggestion.
                RE::FileTooLarge { path, size, limit } => {
                    CliError::FileTooLarge { path, size, limit }
                }
                // A bad `--message`/`--author` is a usage error, not a bug.
                RE::InvalidHeader { reason } => CliError::InvalidArgument { message: reason },
                // Unreadable file, full disk, bad permissions: the
                // environment failed, not Atomic.
                RE::Io(err) => CliError::Io(err),
                RE::Repository(err) => CliError::Repository(err),
                // Genuine internal failures: the change graph or the store
                // itself misbehaved. These are the ones worth a bug report.
                other @ (RE::Globalize(_)
                | RE::Assembly(_)
                | RE::ChangeStore(_)
                | RE::Database(_)) => CliError::Internal(anyhow::anyhow!("{}", other)),
            }
        })?;

        // Display result
        let output = self.format_outcome(&view_name, &outcome);
        print!("{}", output);

        // Show any errors that occurred during recording
        if outcome.has_errors() {
            println!();
            print_warning("Some files had errors:");
            for (path, error) in outcome.errors() {
                println!("  {}: {}", path, error);
            }
        }

        // Show skipped files if any
        if !outcome.skipped_files().is_empty() {
            println!();
            print_hint(&format!(
                "{} skipped (unchanged, empty, binary, or too large)",
                format_count(outcome.skipped_files().len(), "file")
            ));
        }

        // CB-8A (RFC §7.6, §8.1): an Atomic-origin record is a bridge
        // transition. In an active Git shadow the recorded durable state is
        // projected immediately — an operation-specific commit on the view's
        // mapped ref with the scope-correct HEAD placement and the index
        // aligned to the projected tree — so `git status` and `atomic
        // status` are both clean afterwards. Partial user staging is
        // intentionally preserved (never staged over) and skips only the
        // projection, never the record itself.
        let recorded_something =
            !outcome.recorded_files().is_empty() || !outcome.deleted_files().is_empty();
        let recorded_hash = recorded_something.then(|| *outcome.hash());
        drop(workspace);
        drop(repo);
        if let Some(hash) = recorded_hash {
            crate::commands::git::shadow::sync_git_projection_after_record(
                &repo_root, &view_name, &hash,
            )?;
        }

        Ok(())
    }
}

// Helper Functions

/// Format a count with singular/plural suffix.
pub(crate) fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

/// Check if stdin is a terminal (for interactive prompts).
/// Convert an atomic_identity::Identity to atomic_core::change::Author.
///
/// This bridges the two Author types: atomic_identity has its own Author
/// for lightweight identity operations, while atomic_core::change::Author
/// is used in change headers. This function performs the conversion.
pub(crate) fn identity_to_author(identity: &Identity) -> Author {
    Author::with_identity(
        identity.name.clone(),
        identity.email.clone(),
        identity.public_key_base32(),
    )
}

pub(crate) fn is_terminal() -> bool {
    // Use a simple heuristic - check if we're in a CI environment
    // or if stdin is piped
    std::env::var("CI").is_err() && std::env::var("ATOMIC_NONINTERACTIVE").is_err()
}
