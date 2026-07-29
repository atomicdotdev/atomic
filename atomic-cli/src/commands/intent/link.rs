//! `atomic intent link <ID> --goal <GOAL>` — link a goal to a stored intent.
//!
//! The canonical-family analogue of `atomic vault intent link`. It delegates to
//! the exact same [`Repository::vault_intent_link`] path, so its behavior is
//! byte-for-byte identical to the vault verb it supersedes.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Link a goal to an intent.
///
/// Associates a goal with an intent so that the work done in the goal is tracked
/// against the intent.
#[derive(Parser, Debug)]
#[command(name = "link")]
pub struct IntentLink {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Goal name to link.
    #[arg(long)]
    pub goal: String,
}

impl Command for IntentLink {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        repo.vault_intent_link(&self.id, &self.goal)
            .map_err(CliError::Repository)?;
        println!("Linked goal '{}' to intent '{}'", self.goal, self.id);

        Ok(())
    }
}
