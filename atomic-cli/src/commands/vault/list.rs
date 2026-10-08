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

/// One vault listing row (path, entry type, content size, updated-at) —
/// the shape both the local body (repo-read) and the routed hook
/// (wire-carried) render.
pub(crate) struct EntryRow {
    pub path: String,
    pub entry_type: String,
    pub size: u64,
    pub updated_at: String,
}

/// The shared render — the exact local body's table and JSON shapes.
pub(crate) fn render_rows(rows: &[EntryRow], json: bool) {
    if json {
        let json: Vec<serde_json::Value> = rows
            .iter()
            .map(|e| {
                serde_json::json!({
                    "path": e.path,
                    "type": e.entry_type,
                    "size": e.size,
                    "updated_at": e.updated_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        return;
    }
    if rows.is_empty() {
        println!("No vault entries found.");
        return;
    }
    for entry in rows {
        println!(
            "  {:12} {:>8}  {}",
            entry.entry_type,
            format_size(entry.size),
            entry.path,
        );
    }
    let count_label = if rows.len() == 1 { "entry" } else { "entries" };
    println!("\n{} {}", rows.len(), count_label);
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        // Every form routes (the whole-vault listing carries its
        // type/size/date columns on the wire; --prefix/--type ride the
        // request's filters).
        if crate::commands::rpc::vault_list(self)? {
            return Ok(());
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let type_filter = self.r#type.as_deref().and_then(VaultEntryType::parse);
        let prefix = self.prefix.as_deref().unwrap_or("");

        let entries = repo
            .vault_list(prefix, type_filter)
            .map_err(CliError::Repository)?;

        render_rows(
            &entries
                .iter()
                .map(|e| EntryRow {
                    path: e.path.clone(),
                    entry_type: e.entry_type.to_string(),
                    size: e.content_size as u64,
                    updated_at: e.updated_at.clone(),
                })
                .collect::<Vec<_>>(),
            self.json,
        );

        Ok(())
    }
}
