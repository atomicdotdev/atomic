//! `atomic vault list` — list vault entries.
//!
//! Lists all vault entries in the repository, with optional filtering by
//! entry type and path prefix.
//!
//! # Usage
//!
//! ```text
//! atomic vault list [OPTIONS]
//!
//! Options:
//!   -t, --type <TYPE>      Filter by entry type (session, memory, intent, skill, scratch, tool_result)
//!   -p, --prefix <PREFIX>  Filter by path prefix
//!       --json             Output as JSON
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all vault entries
//! $ atomic vault list
//!   session     1.2 KiB  goals/swift-meadow-a3f2/_goal.md
//!   memory       804 B   memory/architecture.md
//!   intent       512 B   intents/PIMO-1.md
//!
//! 3 entries
//!
//! # Filter by type
//! $ atomic vault list --type memory
//!   memory   804 B  memory/architecture.md
//!   memory   1.1 KiB  memory/conventions.md
//!
//! 2 entries
//!
//! # Filter by prefix
//! $ atomic vault list --prefix goals/
//!
//! # Output as JSON
//! $ atomic vault list --json
//! ```

use clap::Parser;

use atomic_core::pristine::vault::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, format_size, Command};
use crate::error::{CliError, CliResult};

/// List vault entries.
///
/// Shows all entries stored in the vault, optionally filtered by type or
/// path prefix. Each entry displays its type, size, and vault-relative path.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct List {
    /// Filter by entry type (session, memory, intent, skill, scratch, tool_result).
    #[arg(long, short = 't')]
    pub r#type: Option<String>,

    /// Filter by path prefix.
    #[arg(long, short = 'p')]
    pub prefix: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let type_filter = self.r#type.as_deref().and_then(VaultEntryType::parse);
        let prefix = self.prefix.as_deref().unwrap_or("");

        let entries = repo
            .vault_list(prefix, type_filter)
            .map_err(CliError::Repository)?;

        if self.json {
            let json: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "path": e.path,
                        "type": e.entry_type,
                        "size": e.content_size,
                        "updated_at": e.updated_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            if entries.is_empty() {
                println!("No vault entries found.");
                return Ok(());
            }

            for entry in &entries {
                println!(
                    "  {:12} {:>8}  {}",
                    entry.entry_type,
                    format_size(entry.content_size as u64),
                    entry.path,
                );
            }

            let count_label = if entries.len() == 1 {
                "entry"
            } else {
                "entries"
            };
            println!("\n{} {}", entries.len(), count_label);
        }

        Ok(())
    }
}
