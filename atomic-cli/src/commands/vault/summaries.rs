//! `atomic vault summaries` — get tool result summaries.
//!
//! Retrieves summaries of tool results stored in the vault, optionally
//! scoped to a specific goal. Output is always JSON for machine
//! consumption by AI agents and tooling.
//!
//! # Usage
//!
//! ```text
//! atomic vault summaries [OPTIONS]
//!
//! Options:
//!       --goal <NAME>     Goal name to get summaries for
//!   -h, --help            Print help information
//! ```
//!
//! Output is always JSON.
//!
//! # Examples
//!
//! ```text
//! # Get all tool result summaries
//! $ atomic vault summaries
//! {
//!   "toolu_abc123": "Read file src/main.rs ...",
//!   "toolu_def456": "Executed cargo test ..."
//! }
//!
//! # Get summaries for a specific goal
//! $ atomic vault summaries --goal swift-meadow-a3f2
//! ```

use clap::Parser;

use atomic_core::pristine::vault::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Get tool result summaries for a goal.
///
/// Retrieves preview snippets of tool results stored in the vault.
/// This is primarily used by AI agents to recall previous tool
/// outputs without re-reading full content.
#[derive(Parser, Debug)]
#[command(name = "summaries")]
pub struct Summaries {
    /// Goal name to get summaries for.
    ///
    /// When provided, only tool results from the specified goal
    /// are included. When omitted, tool results from all goals
    /// are returned.
    #[arg(long)]
    pub goal: Option<String>,
}

impl Command for Summaries {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let prefix = match &self.goal {
            Some(name) => format!("goals/{}/", name),
            None => "goals/".to_string(),
        };

        let entries = repo
            .vault_list(&prefix, Some(VaultEntryType::ToolResult))
            .map_err(CliError::Repository)?;

        let mut summaries = serde_json::Map::new();
        for meta in &entries {
            // Extract tool_use_id from filename
            // e.g., "goals/abc/toolu_xyz.md" -> "toolu_xyz"
            let filename = meta.path.rsplit('/').next().unwrap_or(&meta.path);
            let id = filename.strip_suffix(".md").unwrap_or(filename);

            // Get a preview (first 200 chars of content)
            if let Ok(Some(entry)) = repo.vault_retrieve(&meta.path) {
                let content = String::from_utf8_lossy(&entry.content_bytes);
                let preview: String = content.chars().take(200).collect();
                summaries.insert(id.to_string(), serde_json::Value::String(preview));
            }
        }

        let output = serde_json::Value::Object(summaries);
        println!("{}", serde_json::to_string_pretty(&output).unwrap());

        Ok(())
    }
}
