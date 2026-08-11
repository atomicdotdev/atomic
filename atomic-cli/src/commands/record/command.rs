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

        // Record the changes
        let outcome = repo.record(header, options).map_err(|e| match e {
            atomic_repository::record::RecordError::NothingToRecord => CliError::NothingToRecord,
            atomic_repository::record::RecordError::NoFilesMatched => CliError::NothingToRecord,
            atomic_repository::record::RecordError::FileNotFound { path } => {
                CliError::FileNotFound {
                    path: PathBuf::from(path),
                }
            }
            atomic_repository::record::RecordError::FileNotTracked { path } => {
                CliError::FileNotTracked {
                    path: PathBuf::from(path),
                }
            }
            // Refusing to bake an unresolved merge into history is the
            // documented behavior, not a bug — keep it out of the
            // Internal bucket so it neither tells the user to file an
            // issue nor exits 128.
            atomic_repository::record::RecordError::ConflictMarkersPresent { path, line } => {
                CliError::ConflictMarkers { path, line }
            }
            atomic_repository::record::RecordError::UnresolvedConflicts => CliError::Conflict {
                description: "the working copy has unresolved conflicts".to_string(),
            },
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
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
