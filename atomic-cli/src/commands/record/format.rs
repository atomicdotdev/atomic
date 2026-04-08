use super::*;

impl Record {
    /// Format the outcome for display.
    pub(super) fn format_outcome(&self, repo: &Repository, outcome: &RecordOutcome) -> String {
        let mut output = String::new();

        // Get the actual current view name from the repository
        let view_name = repo.current_view();

        // Get hash (shortened)
        let hash_short = &outcome.hash().to_base32()[..DEFAULT_HASH_LENGTH.min(8)];

        // Get message (first line only)
        let message = outcome
            .change()
            .hashed
            .header
            .message
            .lines()
            .next()
            .unwrap_or("No message");

        // Header line: [view seq/hash] message
        let sequence = outcome.new_state().map(|_| "1").unwrap_or("?");
        output.push_str(&format!(
            "[{} {}/{}] {}\n",
            view_name, sequence, hash_short, message
        ));

        // Stats line - show graph-based stats
        let stats = outcome.stats();
        if stats.has_changes() {
            output.push_str(&format!(
                " {} changed, +{} vertices, ~{} edges, {} bytes\n",
                format_count(stats.files_recorded, "file"),
                stats.vertices_added,
                stats.edges_modified,
                stats.content_bytes
            ));

            // CRDT token-level statistics (for fine-grained diff tracking)
            if stats.has_crdt_stats() {
                // Line-level changes
                let line_changes = stats.total_line_changes();
                if line_changes > 0 {
                    output.push_str(&format!(
                        " {} (+{} -{} ~{})\n",
                        format_count(line_changes, "line"),
                        stats.lines_added,
                        stats.lines_deleted,
                        stats.lines_modified
                    ));
                }

                // Token-level changes
                let token_ops = stats.total_token_ops();
                if token_ops > 0 {
                    output.push_str(&format!(
                        " {} (+{} -{} ~{})\n",
                        format_count(token_ops, "token"),
                        stats.tokens_added,
                        stats.tokens_deleted,
                        stats.tokens_replaced
                    ));
                }
            }
        }

        // File list
        for path in outcome.recorded_files() {
            output.push_str(&format!(" {}\n", path));
        }

        output
    }

    /// Display dry run preview.
    pub(super) fn display_dry_run(&self, repo: &Repository) -> CliResult<()> {
        let status = repo
            .status(StatusOptions::default())
            .map_err(CliError::Repository)?;

        let mut has_changes = false;

        println!("Would record:");

        for entry in status.entries() {
            // Skip untracked unless --all
            if matches!(entry.status(), FileStatus::Untracked) && !self.all {
                continue;
            }

            // Skip clean files
            if matches!(entry.status(), FileStatus::Clean) {
                continue;
            }

            // Filter by specified files if any
            if !self.files.is_empty() {
                let path_str = entry.path().to_string_lossy();
                if !self.files.iter().any(|f| path_str.contains(f)) {
                    continue;
                }
            }

            has_changes = true;
            let status_desc = match entry.status() {
                FileStatus::Added => "new file:",
                FileStatus::Modified => "modified:",
                FileStatus::Deleted => "deleted: ",
                FileStatus::Untracked => "new file:",
                FileStatus::TypeChanged => "typechange:",
                FileStatus::PermissionsChanged => "permissions:",
                FileStatus::Conflicted => "conflicted:",
                FileStatus::Clean => continue,
            };

            println!("  {}  {}", status_desc, entry.path().to_string_lossy());
        }

        if !has_changes {
            println!("  (no changes to record)");
        }

        Ok(())
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}
