//! `atomic intent delete <ID>` — discard an unstarted backlog intent.
//!
//! The canonical-family analogue of `atomic vault intent delete`. It delegates
//! to the exact same [`Repository::vault_intent_delete`] path (and the same
//! "unstarted backlog, no linked goals" guard it enforces), so its behavior is
//! byte-for-byte identical to the vault verb it supersedes.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Delete an unstarted backlog intent.
///
/// Removes an intent that has not left backlog and has no linked goals. Use this
/// for accidental drafts only; started work should be closed or superseded
/// instead of deleted. Without `--force`, prompts for confirmation.
#[derive(Parser, Debug)]
#[command(name = "delete")]
pub struct IntentDelete {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentDelete {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        if !self.force {
            // Check that the intent exists before prompting.
            repo.vault_intent_show(&self.id)
                .map_err(CliError::Repository)?;

            let prompt = format!(
                "Discard intent '{}'? This removes the unstarted draft from the vault.",
                self.id
            );
            let confirmed = dialoguer::Confirm::new()
                .with_prompt(&prompt)
                .default(false)
                .interact()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;

            if !confirmed {
                return Err(CliError::Cancelled);
            }
        }

        let result = repo
            .vault_intent_delete(&self.id)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": result.id,
                "intent_file": result.intent_file,
                "deleted": true,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Deleted intent: {}", result.id);
            println!("  file: .vault/{}", result.intent_file);
        }

        Ok(())
    }
}
