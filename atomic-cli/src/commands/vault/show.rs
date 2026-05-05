//! `atomic vault show <path>` — print a vault entry's content.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Print a vault entry's content.
///
/// Retrieves and displays the content of a single vault entry by its
/// vault-relative path. By default, outputs the raw markdown content.
/// Use `--json` for structured output including frontmatter and metadata.
///
/// # Examples
///
/// ```text
/// # Show a memory file
/// atomic vault show memory/architecture.md
///
/// # Show a goal file as JSON
/// atomic vault show goals/swift-meadow-a3f2/_goal.md --json
/// ```
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct Show {
    /// Vault-relative path to the entry (e.g., "memory/architecture.md").
    pub path: String,

    /// Output as JSON instead of markdown.
    #[arg(long)]
    pub json: bool,
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let entry = repo
            .vault_retrieve(&self.path)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("Vault entry not found: {}", self.path),
            })?;

        if self.json {
            let json = serde_json::json!({
                "path": self.path,
                "entry_type": entry.entry_type.to_string(),
                "content": String::from_utf8_lossy(&entry.content_bytes),
                "frontmatter": serde_json::from_str::<serde_json::Value>(
                    &entry.frontmatter_json
                ).unwrap_or_default(),
                "created_at": entry.created_at,
                "updated_at": entry.updated_at,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            print!("{}", String::from_utf8_lossy(&entry.content_bytes));
        }

        Ok(())
    }
}
