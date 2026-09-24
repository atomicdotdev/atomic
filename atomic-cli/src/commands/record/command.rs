use super::*;

impl Command for Record {
    /// Execute the record command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Get the commit message (from argument, editor, or prompt)
    /// 3. Detect changes in the working copy
    /// 4. If --all, add untracked files
    /// 5. If --dry-run, display preview and exit
    /// 6. Create the change from modifications
    /// 7. Save the change to the store
    /// 8. Apply the change to the current view
    /// 9. Display the result
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&repo);
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

        // If --all, first add all untracked files
        if self.all {
            let status = repo
                .status(StatusOptions::default())
                .map_err(CliError::Repository)?;

            for entry in status.untracked() {
                let path = entry.path();
                if let Err(e) = repo.add(path, Default::default()) {
                    print_warning(&format!("Failed to add '{}': {}", path.display(), e));
                }
            }
        }

        // Record the changes.
        //
        // This match is deliberately exhaustive: no `_ =>` arm. The catch-all
        // it replaces routed everything unlisted into `CliError::Internal`,
        // which tells the user "this appears to be a bug, please report it"
        // and exits 128 — so every new `RecordError` variant silently
        // defaulted to accusing Atomic of a bug. `FileTooLarge` reached users
        // that way. Keeping the match exhaustive makes the compiler demand a
        // classification decision for each variant added from here on.
        let outcome = repo.record(header, options).map_err(|e| {
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
        let output = self.format_outcome(&repo, &outcome);
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
